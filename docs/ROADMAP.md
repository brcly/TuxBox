# Roadmap

This is the plan for two pieces of work: making the shader translator complete and fast, and auditing the whole project part by part. Each task below has an ID. To take one, open an issue named after it (for example "T2: GPU checks for float instructions") so effort isn't doubled. [Where to improve](FOCUS.md) lists other open areas, and [CONTRIBUTING](../CONTRIBUTING.md) has the rules every change follows.

Figures are from `main` on 28 November 2026 and one title's recorded shaders (ProjectScarlett 1.0.2.1), unless noted.

## The goal

TuxBox should translate **any** shader a Series X|S title supplies, in the console's RDNA 2 instruction set, into what the host needs:

- DXIL on Windows;
- DXIL, or SPIR-V if that proves better, on Linux;
- with the same results as the console, on AMD, NVIDIA and Intel cards of any generation, whether their waves hold 32 or 64 lanes;
- fast enough that a first launch isn't spent compiling.

HLSL is how we get there today. It stays for as long as it is the best route, and no longer.

## Where we are

| Measure | Today |
| --- | --- |
| Recorded shaders that translate, 64-lane hosts | 38,781 of 42,859 |
| Recorded compute shaders that translate, 32-lane hosts | 6,157 of 7,683 |
| Instructions the decoder names | 1,085, in 1,398 encodings ([inventory](INSTRUCTIONS.md)) |
| Instructions the recorded shaders use | 372 |
| Of those, run by a GPU check | 372 |
| Encodings with no translation | 206, and 186 more unproven: no recorded shader uses them |
| Recorded shaders executed and compared with a reference | none; a sample of compute shaders is executed and compared between builds and cards |
| Share of translate-and-compile time spent in DXC | nearly all; the translator itself averages about 30 ms a shader |
| Translated HLSL | median 97 KB; one shader in a hundred is over 1.8 MB |

Three things follow. Nearly every instruction real shaders use is now run on both vendors' cards, but only in synthetic programs. Real compute shaders are only compared between builds and cards, never with a console. And the time goes to compiling very large generated HLSL, so the route and the size of what we generate matter more than the translator's own speed.

## Part 1: shader translation

Every change to the translator runs the [shader harness](SHADER_HARNESS.md). It compares a change with `main` over recorded shaders for both wave widths, runs the GPU checks on each card and blocks regressions.

### Stage A: know what is correct

Nothing later is safe without this.

| ID | Task | Done when |
| --- | --- | --- |
| T1 | **Done: [INSTRUCTIONS.md](INSTRUCTIONS.md).** Instruction inventory. Generate a table of every RDNA 2 instruction from AMD's published instruction set reference: decoded, translated, run by a GPU check, used by recorded shaders. Generate it from code and check it in CI, as `RESULTS.md` is generated from evidence | The table is in `docs/`, and CI fails when it is stale |
| T2 | **Done: 372 of 372.** GPU checks for used instructions. Every instruction the recorded shaders use is run by a GPU check on AMD and NVIDIA, the last three (`s_swappc_b64`, `s_setpc_b64` and `flat_load_ushort`) in programs shaped as the title's | Every instruction recorded shaders use is run on AMD and NVIDIA |
| T3 | **In progress.** Operand forms and lane states. A check that holds an instruction doesn't cover every form. Every modifier and memory field (VOP3, VOP3P, SDWA, DPP16, DPP8, buffer, typed buffer, image, LDS, flat and scalar loads) has been flipped one at a time to see whether the translation reads it. A field it can't honour now refuses instead of being ignored. Every field recorded shaders set is honoured, and `v_fma_mix` clamp, fused rounding and inline half constants were fixed. Left: GPU checks for each form, and showing forms in the inventory | The inventory shows forms, not only names |
| T4 | **In progress ([#379](https://github.com/brcly/TuxBox/issues/379)).** Run real shaders. The harness's [execute stage](SHADER_HARNESS.md#executing-recorded-shaders) runs a sample of recorded compute shaders over generated resources with both builds, blocks a change whose shaders compute something different, and compares cards without blocking. Still to do: programs that index the descriptor heap directly, which are half of ProjectScarlett's compute programs, inputs that reach more paths, and graphics stages | A change that alters a real shader's results fails the harness |
| T5 | **A console reference.** Where an RDNA 2 desktop card is available, run the same instructions natively through its Vulkan driver and compare. It is the closest thing to the console we can measure | One documented, repeatable comparison |

### Stage B: translate everything

| ID | Task | Done when |
| --- | --- | --- |
| T6 | **In progress.** Rank refusals by what a game loses. `tools/refusal_impact.py` ranks translator refusals by the pipelines a run lost; its list isn't in `SHADERS.md` yet. Join the harness's refusal list with the [pipeline failure record](PROJECTSCARLETT.md#finding-why-pipelines-fail). Recorded requests overstate some refusals, such as `mesh root signature missing` (1,567), which a live launch may never hit | A ranked list in `docs/SHADERS.md`, regenerated by one command |
| T7 | **In progress ([#358](https://github.com/brcly/TuxBox/issues/358)).** Bindings. Many of these refusals are discarded attempts against a root that isn't the program's own; one real group is fixed (#370). The largest refusal groups are about resources, not instructions: stores and atomics to a table the root signature doesn't describe as writable (about 1,660), sampler descriptors (about 460), and tables the root doesn't describe (about 500) | Each group has a cause, and a fix or a stated reason it can't be fixed |
| T8 | **One lane model for every host.** 53 compute shaders translate for 64-lane hosts and not for 32-lane ones, and wave operations in divergent control flow still refuse on both. Several mechanisms now overlap: guards, votes, ballots, lockstep, two lanes per thread, half waves. Describe one model they all fit, then fold them into it. The rule they share: a scalar value and every branch are the whole wave's, while each lane holds its own bit of a mask. Branches on EXEC and VCC now follow it wherever the ballots apply. Branches on SCC from a mask operation are still each lane's | The model is written down; no recorded shader translates on only one width |
| T9 | **Unused instructions.** Work through the inventory's untranslated rows: translate with a GPU check, or record why a title can't emit it | No row is blank |
| T10 | **Other titles.** The figures above are one title. Record requests from a second and run the harness on them | A second request set gives a report |

### Stage C: the right structure

The translator decides what a program means and writes HLSL text in the same step, and later passes edit that text. That makes each fix risky and ties us to HLSL. Two translators also exist side by side (`legacy` and `independent`).

| ID | Task | Done when |
| --- | --- | --- |
| T11 | **An intermediate form.** Split translation into: decode, analyse (control flow, lane masks, resources), a typed program in memory, then a backend that writes it out. HLSL is the first backend. Move one instruction family at a time, with the harness in `--identical` mode so output doesn't change | No pass edits HLSL text; `exec_group.rs` works on the typed program |
| T12 | **One list of strategies.** `driver/pipeline.rs` picks how to translate through nested retries and thread-local switches. Make it one ordered list of named strategies, each with the condition that selects it | Adding a strategy is one entry; output is identical |
| T13 | **One translator.** Decide what the `independent` translator is for. Merge it into the main one or remove it | One code path translates a shader |

### Stage D: the right route

Don't choose by argument. Measure, then decide.

What we know:

- D3D12 accepts shaders only as DXBC or DXIL containers. On Windows, and on Linux through vkd3d-proton, **DXIL is what we must hand over**. Writing DXIL ourselves would skip DXC on both.
- Writing SPIR-V would also skip vkd3d-proton's conversion, but we know of no way to give vkd3d-proton SPIR-V without patching it, which the [ground rules](FOCUS.md#keeping-the-ground-rules) exclude. It would need TuxBox to create Vulkan pipelines itself.
- The driver's own compile is a large cost that no route removes. Smaller generated code helps every route. Removing per-lane write guards from two large shaders cut driver time by about half, but gave wrong results; a safe version needs the lane analysis of T8 and T11.

| ID | Task | Done when |
| --- | --- | --- |
| T14 | **Done: [per-stage baseline](SHADER_HARNESS.md#per-stage-baseline).** For a fixed set of recorded shaders, `python tools/stage_baseline.py run` records time and memory in the translator, DXC, DXV, vkd3d-proton and the driver, on each card, and the harness report gives size and translation time | One command reproduces the table |
| T15 | **Trial: write DXIL directly.** For straight-line compute programs only, write DXIL from the intermediate form without DXC. Check that Windows and vkd3d-proton accept the container, compare results on the GPU with the HLSL route, and time it against T14 | A written result with the numbers |
| T16 | **Trial: SPIR-V on Linux.** Confirm whether any unpatched route exists. If one does, repeat T15 for it | A written answer |
| T17 | **Decide.** Adopt a direct backend if it gives identical results on both cards and a clear gain in time to a ready pipeline; we suggest at least three times faster as the bar. Otherwise keep HLSL and put the effort into T18 | The decision and its numbers are in this file |
| T18 | **Generate less code.** With lane analysis in place: drop write guards that can't be observed, simplify multiplies whose operands are known, and split very large programs | Measured against T14, results unchanged in T4 |

T11 comes before T15: a second backend is only affordable once the meaning of a program is separate from HLSL.

## Part 2: the audit

TuxBox has grown quickly. Each part gets one audit, in small pieces, to remove what is dead, merge what is duplicated and bring tests and documentation up to date.

### How an audit runs

1. **Open an issue** named for the part ("A3: audit runtime/d3d12 commands").
2. **List findings first, change nothing.** Post the list to the issue under the headings below.
3. **Fix in small pull requests**, one kind of change each. A cleanup doesn't change behaviour; a behaviour fix is its own pull request with a test.
4. **Prove removals.** Code goes when nothing references it and the checks for that part pass. For the translator, the harness must pass with `--identical`.
5. **Close** with what was removed, what was kept and why, and what was left for later.

### What every audit checks

- **Dead code:** functions, flags, environment variables, scripts and files nothing uses.
- **Two answers to one problem:** overlapping mechanisms, and workarounds for faults since fixed.
- **Title-specific code:** a patch or path for one title has the exact-build check and the comment [AGENTS.md](../AGENTS.md) requires, or it becomes general.
- **Vendor checks:** paths chosen by what the driver can do, not by vendor.
- **Tests:** each behaviour has one test that would fail without it; duplicates and tests that can't fail go.
- **Speed and memory:** hot paths measured, not assumed.
- **Documentation:** matches the code; history that no longer guides anyone is removed or moved out of the way.
- **Rules:** file length, test location, licence and attribution.

### The parts

Sizes are lines in tracked source and documentation files.

| ID | Part | Size | Look at first |
| --- | --- | ---: | --- |
| A1 | `tools/`: checks and developer tools | 105,000 in 695 files | 159 check scripts, 58 of them for one title, and 31 patch files in `tools/reference/`. Which still run, and which the check runners call |
| A2 | `tools/shader-compiler/`: the translator | 52,600 | Overlapping lane mechanisms (T8), two translators (T13), single-program special cases |
| A3 | `runtime/d3d12/`: the graphics provider | 49,300 | Diagnostics and trace switches, workarounds for fixed faults, title-specific paths |
| A4 | `runtime/dxbc/`: containers and the decoder | 7,200 | The decoder against AMD's reference (T1) |
| A5 | `runtime/xmem/`: game memory | 5,800 | After the physical-backing change: paths it replaced |
| A6 | `runtime/xg/`, `runtime/xg-layout/` | 7,100 | Tables against their sources; duplicated layout code |
| A7 | Small providers: `xgameruntime`, `xcrdapi`, `dxc`, `gameinput`, `xapu`, `acphal`, `bvh` | 8,200 | Stubs that claim success; unimplemented calls that should say so |
| A8 | `project_tuxbox/`: launcher, build and checks | 7,000 | Environment variables against [ENVIRONMENT.md](ENVIRONMENT.md); shader preparation and cache paths |
| A9 | `tests/` and CI | 9,100 | Tests that need no GPU but aren't in CI; slow or duplicate tests |
| A10 | `examples/lanes/`: the test game | 5,500 | Scenarios that overlap; scenarios CONTRIBUTING's table never names |
| A11 | `docs/` and `evidence/` | 36 documents | Per-fix notes that are history, not guidance; evidence nothing regenerates |

Findings are posted for A1 and A11 ([#359](https://github.com/brcly/TuxBox/issues/359)) and A9 ([#369](https://github.com/brcly/TuxBox/issues/369)); their fixes are open to take with the person auditing.

Suggested order: **A1 and A11 first**, since they are the largest, the least risky and make every later audit easier to read. Then A8 and A9, then the runtime parts. A2 runs alongside Part 1, and A4 with T1.

## Order of work

T1, T2 and T14 are done.

1. **Now:** T3, T4, T6, T7, and audits A1, A11 and A9, which have started. These need no design decisions and can run in parallel.
2. **Next:** T12, T13, and audits A8 and A2.
3. **Then:** T8 and T11, the two that change how the translator is built.
4. **After T11:** T15 to T18, and the decision on the route.

T5, T9 and T10 can be picked up at any time.
