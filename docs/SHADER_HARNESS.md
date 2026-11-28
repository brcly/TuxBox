# Shader harness

[`tools/shader_harness.py`](../tools/shader_harness.py) tests a change to the [shader translator](../tools/shader-compiler) against a title's recorded shaders and against your graphics cards. It compares your working tree with a baseline, `origin/main` unless you name another, and says what blocks the change.

```sh
python tools/shader_harness.py run
```

It needs Linux, a folder of recorded shader requests from your own copy of a title, and the DXC used for [shader preparation](PROJECTSCARLETT.md). The GPU stages also need what [the full check](../CONTRIBUTING.md#the-full-check) needs.

## Configuration

Put the paths in `build/shader-harness.json`, which Git ignores:

```json
{
  "requests": "PREFIX/drive_c/users/steamuser/AppData/Local/Temp/tuxbox_shader_cache/projectscarlett/HASH/requests",
  "dxc": "DXC/bin/dxc",
  "proton": "PROTON",
  "windows_dxc": "DXC-WINDOWS/bin/x64",
  "adapters": [
    {"name": "nvidia", "id": "10de:XXXX", "prefix": "PREFIX-A"},
    {"name": "amd", "id": "1002:XXXX", "prefix": "PREFIX-B",
     "env": {"VKD3D_FILTER_DEVICE_NAME": "RADV", "DXVK_FILTER_DEVICE_NAME": "RADV"}}
  ]
}
```

## Stages

| Stage | What it does | Blocks when |
| --- | --- | --- |
| Code checks | `tuxbox.py check --no-gpu` | any step fails |
| Survey | Translates every request with both builds. Compute programs are translated twice, for hosts with 64-lane and with 32-lane waves | a program stops translating, or the translator crashes or hangs |
| Determinism | Translates a sample again | any outcome or output differs |
| Compile | Runs changed and recovered programs through DXC and DXV | a program that compiled no longer does |
| GPU | Builds, then runs the [translator checks](../tools/check_translator_gpu.py) and the shader [Lanes](../examples/lanes/README.md) suites on each card | any check fails |
| Execute | Runs a sample of recorded compute shaders, as both builds translate them, over the same generated resources on each card, and compares what they write ([below](#executing-recorded-shaders)) | a shader computes something different, or ran before and no longer runs |
| Coverage | Lists instructions the recorded shaders use that no GPU check program holds, and the largest refusals | never; it shows where to add tests |

The report is `report.md` in the run folder under `build/shader-harness/`, with request names and details in `report.json`. Neither holds shader bytes or HLSL. It gives the HLSL size and translation time of both builds for each host, and names any of AMD and NVIDIA that did not run.

Options for common cases:

| Option | Use |
| --- | --- |
| `--baseline REF` | Compare with another commit |
| `--identical` | A refactor: any change of translated output blocks |
| `--no-gpu` | CPU stages only. The run still fails unless you add `--allow-no-gpu` |
| `--compile-limit 0` | Compile every changed and recovered program, not a sample of 200 of each |
| `--execute-sample N`, `--execute-seed S` | Execute N recorded compute shaders (default 100) from sample S; `0` skips the stage |

Single stages: `survey --xshader BUILD --output FILE`, `compare BEFORE AFTER`, `explain --xshader BUILD REQUEST` for one program on both hosts, and `execute --baseline REF --sample N` for the execute stage alone (`--adapter NAME` picks one card; `--baseline-xshader` and `--candidate-xshader` take built translators).

## Executing recorded shaders

The execute stage runs real shaders, which the other stages only translate and compile. For each sampled recorded compute request it:

1. translates the request with both builds, for the card's wave width: the 32-lane translation on a card whose waves hold at most 32 lanes;
2. reads every resource the translation declares, and gives the program a root signature of one descriptor table for its views and one for its samplers;
3. compiles that with DXC and DXV, using the runtime's compute flags;
4. fills each resource with contents seeded from the request's name, so both builds get the same inputs;
5. dispatches 2×2×1 groups under Proton, with [a small D3D12 runner](../tools/reference/execute-shaders/guest.cpp), and reads every writable resource back.

| Outcome | Meaning | Blocks |
| --- | --- | --- |
| Same | Same translation, same output | no |
| Equivalent | The translation changed and wrote the same for these inputs | no |
| Changed | The translation changed and wrote something else | yes |
| Unstable | The baseline translation, run twice, wrote something else or didn't finish the second time: a race, an uninitialised read, a driver fault or a loaded host | no; it is listed |
| Lost | Ran with the baseline both times, and no longer translates, compiles or finishes | yes |

The sample is drawn from the request folder's contents, and a prefix that a running title uses keeps adding requests. Point `requests` at a copy that doesn't change, so two runs with the same seed execute the same programs.

Seeded values stay small so that a count or index read from them can't run away: floats are multiples of 1/64 below 4, integers are below 256, and words of constant, raw and structured buffers mix integers below 32 with those floats. A dispatch that doesn't finish within 15 seconds, or a request that takes over 90 seconds including its pipeline build, counts as a hang. The runner then restarts with the requests after it and tries the hung one once more. With two cards configured, requests whose output differs between them are counted, without blocking, until we know how much float variance is normal.

Not executed, with the reason in the report: programs that index the descriptor heap directly (about half of ProjectScarlett's compute programs), unbounded descriptor arrays, ray tracing scenes, multisampled textures, and translations over 1 MiB. Generated inputs reach only some paths of a program. Some programs leave every output as seeded, and the report counts those. A change can alter a path these inputs don't take and still pass. On an RTX 4070, a translator that emitted `min` for `v_max_f32` changed 28 of the 77 programs that ran from a sample of 200. Of those 28, 8 wrote different results and 20 did not.

## Per-stage baseline

[`tools/stage_baseline.py`](../tools/stage_baseline.py) measures what each step from a recorded console shader to a ready pipeline costs, for a fixed sample of requests. It uses the same configuration file, and the cards listed in `adapters`.

```sh
python tools/stage_baseline.py run
```

| Stage | How it is measured |
| --- | --- |
| Translator, DXC, DXV | Shader preparation's own code compiles each request in a fresh cache, so DXC's first-boot fallbacks count. Each tool runs behind [a probe](../tools/reference/stage-baseline/probe.py) that records its wall time, CPU time and peak resident memory. Compute programs are measured for 64-lane and 32-lane hosts |
| vkd3d-proton, the driver | [A guest](../tools/reference/stage-baseline/guest.cpp) creates one compute pipeline per program on vkd3d-proton, with the driver's caches off (`__GL_SHADER_DISK_CACHE=0`, `MESA_SHADER_CACHE_DISABLE=true`, `RADV_DEBUG=nocache`, `VKD3D_SHADER_CACHE_PATH=0`). [A Vulkan layer](../tools/reference/stage-baseline/layer.c) times each `vkCreateComputePipelines` inside the call; the rest of the call is vkd3d-proton's DXIL to SPIR-V conversion |

The sample, [`sample.json`](../tools/reference/stage-baseline/sample.json), holds request keys only: three requests from each stage and HLSL size, eight more of the programs over 1.8 MB, and sixteen of the slowest driver compiles logged in game runs. `sample --survey FILE --sized-by COMMIT --logs GAME-LOGS` chooses a new one; change it only on purpose, since results are compared against it. The report is `report.md` in the run folder under `build/stage-baseline/`, with every measurement in `report.json`. The run folder also holds the translated programs for the GPU stage; neither report holds shader bytes or HLSL.

Single parts: `cpu --output RUN`, then `gpu RUN --name NAME --adapter VVVV:DDDD --prefix PREFIX` for each card, then `report RUN`. A run folder that stopped part-way is resumed. The GPU stage does not reproduce the title's root signatures: each program gets one built from its own bindings, in descriptor tables. Graphics pipelines are not timed on the GPU.

## The instruction inventory

[INSTRUCTIONS.md](INSTRUCTIONS.md) lists every RDNA 2 instruction with what the translator does with it alone, which cards a GPU check ran it on, and how many recorded shaders use it. It is generated from three files in the repository, and the Rust and Python tests fail when any of them is out of date.

| You changed | Run |
| --- | --- |
| What the translator does with an instruction | `TUXBOX_BLESS_INSTRUCTIONS=1 cargo test --manifest-path tools/shader-compiler/Cargo.toml --target x86_64-unknown-linux-gnu instruction_inventory`, then `python tools/shader_harness.py inventory` |
| A GPU check, or the recorded requests | `python tools/shader_harness.py run`, then `python tools/shader_harness.py inventory --from-run RUN-FOLDER` |
| Anything the translator emits | `python tools/shader_harness.py inventory --compile`, which compiles each instruction's translation through DXC and DXV and saves which compile |

The [progress map](progress.svg) counts an instruction as converted when its translation compiled or a GPU check ran it; regenerate it with `python tuxbox.py status --svg docs/progress.svg`. The second command saves the run's instruction names and counts under `evidence/shader-instructions/`. It needs `"title"` in the configuration, such as `"ProjectScarlett 1.0.2.1"`, and a passing run of a committed tree on every card you list. Commit the record, the evidence and the document together.

The translator must refuse an instruction it can't translate. The inventory's test fails if any instruction makes it crash.

## What a pass does not show

Recorded shaders are translated and compiled, and a sample of compute shaders is executed over generated inputs. Their outputs are compared between builds and between cards, not with a console, so a program that was already wrong stays wrong. Graphics stages are not executed. The GPU checks run synthetic programs: when you add or fix an instruction, add it to one of them, and use the coverage table to find instructions that have none. A check program holding an instruction does not prove every operand form or lane state was tested. The harness does not run or time the drivers' own pipeline builds; the [per-stage baseline](#per-stage-baseline) does, for its sample.
