# Lanes

Lanes is an original, turn-based homebrew game for Project TuxBox. Collect as many coins as possible over 16 turns and avoid the red barriers. The bottom row is the next turn. Left and Right move one lane and advance. Space advances while holding your lane. A click on a lane takes one step toward it, or holds position in the current lane. On an XInput controller, use the D-pad or left stick to move and A to hold. R or Start restarts the course, and Escape closes the window. You can also click the board to restart after a finish.

## Build requirements

Use an x86-64 PC with Python 3.10 or newer, Rust 1.95 or newer with Cargo, and the DirectX Shader Compiler (`dxc`) on your PATH. Add the Windows build target once with `rustup target add x86_64-pc-windows-msvc`. [DirectX Shader Compiler releases](https://github.com/microsoft/DirectXShaderCompiler/releases)

- **Linux** needs `clang-cl`, `lld-link` and the Windows SDK and C runtime staged with xwin. To stage them in the cargo-xwin cache that `build.py` reads, run `cargo install cargo-xwin` and then `cargo xwin cache xwin --xwin-arch x86_64` once. If the SDK is outside the cargo-xwin cache, set `XWIN_SDK_ROOT` to the directory containing `sdk/` and `crt/`. The launcher builds a small Vulkan layer on first run, so also install the Vulkan headers (`vulkan-headers` on Arch). Use Microsoft's Linux DXC release: Arch's `directx-shader-compiler` 1.10.2605.37 aborts on every shader, and `build.py` reports this before building. Install UMU (`umu-run`, the `umu-launcher` package in the AUR on Arch) and [GE-Proton11-14](https://github.com/GloriousEggroll/proton-ge-custom/releases/tag/GE-Proton11-14), extracted into Steam's `compatibilitytools.d`, to play. From the project folder, run `python3 -m venv .venv` and `source .venv/bin/activate` before installing the Python dependencies.
- **Nix** users can run `nix develop` (or `direnv allow`) for a shell with these Linux tools. The Windows SDK still needs `cargo xwin cache xwin --xwin-arch x86_64` once. The shell also provides GE-Proton11-14 and sets `PROTONPATH` to it, so the launcher uses it without a Steam install; pass `--proton "$PROTONPATH"` to `tuxbox.py check` for the GPU checks. On NixOS, prefix the GPU checks with `tuxbox-fhs` (`tuxbox-fhs python tuxbox.py check ...`), which supplies the system libraries that Proton's Wine expects.
- **Windows** needs Visual Studio Build Tools with the C++ tools and Windows SDK. Run the commands in an **x64 Native Tools Command Prompt**.

## Build and play

Run these commands from the project folder.

```sh
python -m pip install -r requirements.txt
python build.py lanes
python tuxbox.py run --app-id Lanes build/lanes
```

The package in `build/lanes` contains the game, three TuxBox providers and a local Scarlett manifest. The launcher defaults to Proton on x86-64 Linux and native execution on x64 Windows. Add `--backend`, `--prefix` and `--proton` to the launcher before the package path when selecting a different Linux setup. An optional guest argument `--adapter VVVV:DDDD` selects a hardware GPU by PCI IDs.

## What the game exercises

| Game operation | Xbox library |
| --- | --- |
| Allocate and release the game state and course | `xmem.dll` |
| Create the guest device, dispatch the course shader, upload the board and display it with `PresentX` | `d3d12_xs.dll` |
| Read keyboard, mouse and controller state | `GameInput.dll`, through the same factory ordinal Frosty imports |

The game also imports Windows core API sets for services such as files, timing and synchronization. The providers handle desktop graphics, the window, and host input. The game draws its mouse cursor from GameInput movement deltas. [GDK mouse readings](https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/input/gameinput/structs/gameinputmousestate)

## Self-test

`lanes.exe` also checks Xbox graphics behaviour that retail titles depend on, without opening a window. It creates the Series X|S device layout (`ID3D12Device11`) and runs each scenario on a fresh device. A scenario fails if the GPU has not finished within two seconds, so a deadlock fails the check instead of hanging it.

| Scenario | What it checks |
| --- | --- |
| `labels` | `Write32BitValueBottomOfPipeX` and `Write64BitValueBottomOfPipeX`, with and without a flush |
| `cross-queue` | The compute queue waits for a direct-queue label, writes a 64-bit payload and signals a reply. The direct queue waits for the reply and copies the payload for CPU readback. |
| `compute-frame` | `ExecuteCommandListsX` on a compute queue |
| `skipped-command` | A command that cannot be recorded leaves its list, and the list's fence write, intact |
| `copy-memory` | `CopyMemoryToMemoryX` copies from the source to the destination |
| `direct-copy-wait` | A direct queue waits before or after a buffer copy. The copy before the wait can finish while a second copy waits for the compute producer’s label and payload. |
| `shared-texture` | A queue waiting on a label reads a guest texture that the queue setting the label writes |
| `indirect-dispatch` | `ExecuteIndirect` on its Series X\|S slot matches a direct dispatch |
| `gpu-indirect-dispatch` | A compute shader writes indirect dispatch arguments for 0, 1 and 2 groups. Check all 24 output words, including untouched guards. |
| `gpu-indirect-order` | A compute shader rewrites dispatch arguments, and a command count, 48 times in one list, with no CPU wait before each `ExecuteIndirect` that reads them. Six cases: a transition to the indirect argument state, a transition to a combined read state, or only a UAV barrier between the write and the read, each with and without a count buffer. Every round must read the values just written. |
| `mesh-signature` | A mesh dispatch signature is made when the device reports mesh shaders, and refused with `E_NOTIMPL` when it doesn't |
| `mesh-signature-none` | Under `TUXBOX_HOST_PROFILE=mesh=none`, the device reports no mesh shaders and refuses the signature with `E_NOTIMPL`. `all` leaves it out |
| `dispatch-wait` | Copies a producer's word after a label wait, first at list start and then immediately after a course dispatch. Reports whether compute finished before producer submission and the word read by the CPU. |
| `texture-readback` | `CopyMemoryToMemoryX` reads a guest texture into title memory |
| `texture-publication` | The GPU writes a tiled guest texture several times while a compute queue waits for the last write's label. Title memory then holds each texel in its console position. The padding between texels, a page the title closed and write-watched memory past the texture are untouched. |
| `ray-tracing-builder` | `XboxBvhBuilder` builds a structure with a size report, copies it and builds it again in a batch (needs ray tracing) |
| `ray-tracing-guest-memory` | `XboxBvhBuilder` builds a structure and a scene in title memory that no resource names, with vertices, instance record and scratch there too, then copies the structure (needs ray tracing) |
| `ray-tracing-rehydrate` | `XboxBvhBuilder` rehydrates two console structures 256 bytes apart in title memory. Each becomes a host build of the geometry its call names, in the console's meshlet-coded index and quantized position formats, one of them written only after the call, all named by an address mapped for the GPU before the data was written. Rays through a scene of both report the expected instance, geometry and primitive (needs ray tracing) |
| `thread-dimensions` | Slot 80 `USE_THREAD_DIMENSIONS` (`0x20`) converts thread counts using the bound PSO's `[numthreads]` and fails closed without one |
| `descriptor-reclaim` | Shader-visible descriptor heap reclamation across completed batches, overlapping in-flight `Reset()`, `ClearState`, evicted-slot rebinding, and unsubmitted pin exhaustion |
| `unwritten-descriptors` | A heap whose title memory held leftover bytes, bound as a table whose root declares slots the title never writes. The dispatch must run and store its word. |
| `xbox-shader` | A console shader's pipeline and dispatch ([shader cache](#shader-cache)) |
| `bindless-compute`, `bindless-sampler` | Direct-indexed console compute programs through the paged resource index map ([compute roots](#compute-roots)); not part of `all` |
| `bindless-table-store` | A program under a direct-indexed root that stores through one descriptor table slot: that texture is published, and another UAV texture elsewhere in the heap keeps the title's memory ([compute roots](#compute-roots)); not part of `all` |
| `bindless-heap-read`, `bindless-texture-store` | A guest texture with a UAV in the heap of a direct-indexed program. A program with no image write must leave the texture's title memory alone, and a store to it must be published ([compute roots](#compute-roots)); not part of `all` |
| `wave64-shader` | A console shader that sums across 64-lane waves, translated while the game runs; not part of `all` |
| `mixed-lane` | A first-active-lane selector reads a value written to an EXEC-masked lane, then readback verifies that the VGPR still holds the other lanes’ values. Requires the live shader compiler and runs separately from `all`. |
| `wave-alloc` | A console shader's masked 32-lane exclusive prefix, translated to a wave prefix sum; every lane's word is checked. Needs the live shader compiler. |
| `raster-tables` | A console vertex and pixel pair drawn under a root with two descriptor tables, reading through the second ([raster tables](#raster-tables)) |
| `raster-samplers` | A console pixel program's literal linear sampler, under a root whose static sampler at that register is point-filtered ([raster tables](#raster-tables)) |
| `lane-spill-interp` | A console pixel program whose f16 interpolator's attribute selector matches the number of its lane-spill register ([raster tables](#raster-tables)) |
| `raster-heap-stride` | A console vertex program reading its positions by index from a structured buffer in the resource heap ([raster tables](#raster-tables)); not part of `all` |
| `large-dispatch` | A console compute program that stores its group ID, dispatched with 70,000 groups across and then 70,000 down ([large dispatches](#large-dispatches)); not part of `all` |
| `raster-sampler-waterfall` | A console vertex program sampling with a sampler from the sampler heap, read back through a waterfall loop after an if/else copied it ([raster tables](#raster-tables)); not part of `all` |
| `compute-root` | Two root-less console compute programs under a root with three descriptor tables that the provider cannot reflect ([compute roots](#compute-roots)) |
| `compute-root-srv` | A console compute program under three tables and a root SRV stores through a table slot that the title's root declares an SRV ([compute roots](#a-compute-root-with-a-root-srv-after-its-tables)) |
| `mesh-samplers` | The same literal sampler in a mesh pipeline, whose mesh root the provider builds ([raster tables](#raster-tables)); not part of `all` |
| `mesh-sampler-single` | The one-sampler pixel program in a mesh pipeline whose root's `s0` is point-filtered ([raster tables](#raster-tables)); not part of `all` |
| `mesh-sampler-rootless` | The literal sampler programs in mesh pipelines whose root has no static samplers ([raster tables](#raster-tables)); not part of `all` |

```sh
python tuxbox.py verify --only lanes-selftest --output build/lanes-selftest.json
```

To run one scenario, or all of them in one process:

```sh
python tuxbox.py run --app-id Lanes build/lanes --selftest all --report selftest.txt
```

## Benchmark

`lanes.exe --benchmark` times fixed operations and reports each in microseconds, as a median unless noted. Lower is better.

| Reading | Operation |
| --- | --- |
| `submit_us` | Record, submit and wait for a list holding one fence write |
| `roundtrip_us` | Hand a fence label from the direct queue to a waiting compute queue |
| `copy64k_us` | Copy 64 KiB of title memory with `CopyMemoryToMemoryX` |
| `frame_us`, `frame_p99_us` | Paint the game board, upload it through the Xbox command list and read it back (typical and 99th percentile, without a window) |
| `shader_pipeline_us` | Create a compute pipeline from the console shader, found in the prepared cache (Proton only) |
| `dxil_pipeline_us` | Create the same pipeline from its translation directly. The difference from `shader_pipeline_us` is the cache lookup (Proton only) |

`python tuxbox.py check` runs it. Compare with an earlier run on the same machine:

```sh
cp build/check/results.json build/baseline.json
python tuxbox.py check --baseline build/baseline.json
```

The comparison reports changes and does not fail the check. Timings vary with other load on the machine, so close other GPU work before drawing conclusions.

## Shader cache

Console shaders reach the GPU through TuxBox's translation cache. Lanes carries a small hand-written console compute shader (`shader_fixture.py`; it is not taken from any game) and the `xbox-shader` scenario creates a pipeline from it and dispatches it. The provider substitutes an empty pipeline when it has no translation, so `verify` also requires the provider to report loading the translated shader.

```sh
python tuxbox.py verify --only lanes-shader-cache --output build/lanes-cache.json
```

| Check | What it proves |
| --- | --- |
| Prepared | A translation made before launch is loaded from the prepared cache |
| Compiled | An uncached shader is translated by the host worker while the game runs |
| Reused | The next launch uses that translation without compiling it again |

Lanes also carries a console program that sums across 64-lane waves. The `wave64-shader` scenario runs it and compares every thread's word, twice: once with the provider told the host has 32-lane waves ([host profile](../../docs/USAGE.md#host-profile)), so any card takes the [32-lane translation](../../docs/SHADERS.md#64-lane-programs-on-32-lane-hosts), and once with the card as it is.

| Check | What it proves |
| --- | --- |
| Wave, narrow | The provider asks for the 32-lane translation while the game runs, loads it, and the results are the console's |
| Wave, host | The results are the console's with whatever translation this card needs |

These checks run on the Proton backend. They remove only these shaders' cache entries from the prefix.

## Raster tables

The `raster-tables` scenario draws a fullscreen triangle with two hand-written console programs (`raster_fixture.py`; not taken from any game). The pixel program reads one dword through slot 2 of the root's second descriptor table and writes it to a uint target. Its FSSM records and PSV binding name that descriptor `t3, space2`, while the root declares `t0` to `t3` in space 1 there. The provider has to build the root the program was compiled against, with the register at that slot, and translate both programs for it. Under the title's root the pipeline becomes a stand-in and the target keeps its clear value. A wrong table or slot reads a decoy that names its view.

The `raster-samplers` scenario draws with the same vertex program and a third hand-written pixel program. It samples a 2x1 texture of 1.0 and 3.0, at table 1 slot 0, with a linear clamped sampler it builds from literal words, as the console compiles a static sampler into a program. It declares that sampler as `s0`, where the title's root has a point-filtered static sampler. The provider has to give the literal state a static sampler of its own in the reflected root, so every texel reads 1.5. Bound to the root's `s0`, the texels read the point-filtered 1.0.

The `lane-spill-interp` scenario draws the same fullscreen triangle with a third hand-written pixel program under an empty root. It writes a lane-spill area in v128, then runs an f16 interpolator whose attribute selector is encoded like v128. The selector is not a register read, so every texel must hold the program's constant. Under a refused translation the pipeline becomes a stand-in and every texel keeps its clear value.

The `mesh-samplers` scenario samples the same way through a mesh pipeline. Its pixel program declares `s0` and `s1`, which the title's root has as point-filtered static samplers, so neither register can stand in for the literal state and the program does not translate against that root. Its synthetic console mesh program names the root constant `b501` where the title root declares `b500`, so the provider builds the mesh stage a root of its own. It emits guarded half-to-float positions for a 67-vertex NGG program. The provider has to add the literal state to the reflected root as a private static sampler and translate both stages against it, so every texel reads 1.5. The verification gate matches the loaded mesh and pixel digests against fresh shader-worker receipts, forbids rootless translations and stand-ins, and does not prepare a desktop mesh shader. The separate translated-mesh GPU control tests 3, 67 and 128 vertices, including clipped draws.

The `mesh-sampler-rootless` scenario draws through that mesh program under the same root without its static samplers, with the `raster-samplers` pixel program and with the two-sampler one carrying an empty feature part. The translation against a root with no static samplers reads a literal state from a private sampler, so the provider has to add each program's samplers to the mesh root: the declared `s0` for the program with one sampler, a private register for the program with two. Without them the host rejects both pipelines, which become stand-ins.

These roots are made while the game runs, so the pixel programs cannot be prepared before launch. `verify` runs these scenarios with the host worker, requires the provider to report the reflected root (for the vertex pipelines) or the mesh pipeline and its descriptor slots, and fails if a pixel or vertex program was translated without a root.

```sh
python tuxbox.py verify --only lanes-raster-tables --output build/lanes-raster-tables.json
```

The suite runs on the Proton backend and removes only these programs' cache entries from the prefix.

The `raster-heap-stride` scenario draws with its own vertex and pixel pair under a root with no parameters and a paged resource heap (resident limit 64). The vertex program takes the descriptor of a structured buffer with 32-byte records from heap slot 100 and reads its position at its vertex number. The program carries no stride, so the provider has to retry the translation against a root with its stride table and bind that table for the draw; every texel must then hold the pixel program's constant. A refused pipeline, a skipped draw or another stride leaves the target at its clear value. `verify` requires the paged stride table bind and a translation made against the pipeline's root.

The `raster-sampler-waterfall` scenario draws with its own vertex program and the same pixel program under a root with no parameters that indexes both heaps directly. The vertex program samples the 2x1 texture at resource heap slot 100 with the sampler at sampler heap slot 5, which it loads at a constant offset. As RAGE's VfxVBuffer programs do, both arms of an if/else copy that sampler into vector registers and a waterfall loop reads it back for the sample. The sampled value is the position's w: slot 5 holds a point sampler, which keeps the triangle over the whole target, and every other slot a linear one, which shrinks it. A program the translator refuses becomes a stand-in pipeline and leaves the target at its clear value.

## Compute roots

The `compute-root` scenario creates two root-less console compute programs (`shader_fixture.py`; not taken from any game) under a root with three descriptor tables. Without user-data records the provider cannot reflect that root, so it must translate each program against the root as given. Both programs store a word through table 1, and their FSSM records and PSV binding name that descriptor `u3, space2`, a register the root does not declare. A translation made without the root therefore cannot be bound.

- The first program stores through slot 2, which the root declares as `u2, space1`. Its translation for the root writes the word to that view; a wrong view, or a stand-in that leaves the decoy, fails the scenario.
- The second stores through slot 5, past the end of the table. The translator refuses it for this root, and the provider must give it a stand-in rather than fall back to the root-less translation.

`verify` runs the scenario with the host worker. It fails if either program was translated without its root, if the second was not refused, or if the provider substituted anything but that one stand-in.

```sh
python tuxbox.py verify --only lanes-compute-root --output build/lanes-compute-root.json
```

It runs on the Proton backend and removes only these programs' cache entries from the prefix.

## Large dispatches

A console dispatch can have more groups in a dimension than the 65,535 a desktop `Dispatch` allows. The provider records it as several dispatches of at most 65,535 groups in each dimension that is over, and divides no other. Before each one it sets the console group ID that dispatch starts at in three root constants (`b0, space32766`) appended to the pipeline's root. The program is translated again against that root, and the translation adds the constants to every group ID it reads, so each group runs with its console ID. Group counts a program reads come from the title's own constants, which keep the console values. The variant is made at the first such dispatch. A pipeline that cannot take the group base, such as an inline ray-query program, a program the provider runs untranslated or a stream-created pipeline, still fails the list, with the reason in the log. `ExecuteIndirect` arguments over the limit are written by the GPU and are not split, and neither is `DispatchMesh`.

The `large-dispatch` scenario runs a hand-written console program (`shader_fixture.py`; not taken from any game) that stores its group ID and a marker at record x + 2y of a raw UAV. It dispatches 70,000 x 1 x 1 groups, then 2 x 70,000 x 1, and checks every word of all 140,000 records, including those the first dispatch leaves unwritten. Without the split the list fails to close. `verify` runs it with the host worker and fails unless the provider made the variant, split the dispatch in two and substituted no stand-in.

```sh
python tuxbox.py verify --only lanes-large-dispatch --output build/lanes-large-dispatch.json
```

It runs on the Proton backend and removes only this program's cache entries from the prefix.

## Late pipelines

A large translated compute program's driver compile can outlast the pipeline deadline (`TUXBOX_PIPELINE_DEADLINE`, 20 seconds by default). The title then gets an empty stand-in so it can keep loading, and the compile carries on. When it finishes, the next bind uses the real pipeline. Dispatches recorded before then are skipped.

The `late-pipeline` scenario runs a hand-written console program (`shader_fixture.py`; not taken from any game) in which each of 64 threads runs its index through a 60,000-step chain of dependent integer operations and stores the result. The chain makes the DXIL larger than the size at which compiles are deferred. `verify` sets a 10 ms deadline, so the program gets a stand-in, and the scenario dispatches until every thread's word holds the expected value. It fails if the stand-in is never replaced within two minutes, or if a word holds anything else.

```sh
python tuxbox.py verify --only lanes-late-pipeline --output build/lanes-late-pipeline.json
```

It runs on the Proton backend and removes only this program's cache entries from the prefix.

## Deterministic replay

```sh
python tuxbox.py run --app-id Lanes build/lanes --replay --report replay.txt
```

With `--overlay` before the package path, add `--panel` after `--replay` to fail unless TuxBox's [performance panel](../../docs/USAGE.md#performance-overlay) is on screen. The `present` checks run this as `lanes-overlay`.

The replay checks all 16 GPU-generated rows, a win, a missed coin and a collision. It uploads two 520 × 640 boards through the Xbox command list, reads both textures back, and checks every pixel plus the lane and player colors. The replay runs without opening a window. During interactive play, the same textures are presented through the Xbox command queue.

Run `python tuxbox.py verify --only lanes --output build/lanes-results.json` to check the replay and audit both the regular and delay-load import tables. The verifier requires all three Xbox libraries and rejects direct desktop graphics, drawing or input imports. [Proton Experimental](../../evidence/providers/lanes-xbox-valve.json) · [GE-Proton](../../evidence/providers/lanes-xbox-ge.json) · [Radeon replay](../../evidence/providers/lanes-xbox-radeon.json)

An interactive run compared the displayed board after keyboard presses, mouse clicks, and input from a hot-plugged virtual Xbox controller. It also checked the three DLLs loaded from the package. [Input and loader results](../../evidence/lanes-xbox-input.json) · [Captured board](../../evidence/lanes-xbox-board.png)

To repeat the keyboard part on Linux, run `python tools/check_lanes_input.py --prefix PREFIX --output build/lanes-input.json`. It launches the package with `tuxbox.py run`, presses keys through XTest, and reads the presented board from the X root window. Left and Right must move the highlighted lane. Steering into the next barrier must show COLLISION, and a later move must be ignored. R must restart, and Escape must exit with status 0 and leave no process in the prefix. The checks read the board, so they hold for any course. On a Wayland desktop the game runs in a private rootful Xwayland; elsewhere, run the check under `xvfb-run`. Add `--adapter VVVV:DDDD` and `--proton` as for the launcher.


### Partial compute root recovery

`compute-root-recovery` uses a version 1.1 root with two tables, one
pre-existing UAV and an independently corroborated missing slot in the
second table. Only the latter has a user-data record. A logical 128-entry
heap limited to 64 resident descriptors forces paging. Both writes and
all other guard words must match; stand-ins are disabled.

```sh
python tuxbox.py verify --only lanes-compute-root-recovery --output build/lanes-compute-root-recovery.json
```

### A compute root with a root SRV after its tables

`compute-root-srv` runs a console program under a version 1.1 root of
three tables and a root SRV, the layout of some retail compute roots. The
program's records name that root SRV and a UAV at slot 2 of the second
table, where the title's root declares four SRVs; the title fills the
table as the program was compiled to use it. The program's word must
arrive in that view. Against the title's declaration the store has no UAV
and the program cannot be translated, so the pipeline is not created.
Stand-ins are disabled.

```sh
python tuxbox.py verify --only lanes-compute-root-srv --output build/lanes-compute-root-srv.json
```

### A root-less compute program through the heap stride table

`compute-heap-stride` runs a root-less console compute program (`shader_fixture.py`;
not taken from any game) under a version 1.1 root with no parameters and a paged
resource heap (resident limit 64). Each of its 64 threads takes the thread number,
reads the first dword of the 32-byte record it names in a structured buffer in
the heap (slot 100) and stores the word back into the same index's record of a
second structured buffer (slot 110). The load is an IDXEN raw access whose record
stride the console stores in the descriptor's word 1, which the program carries no
way to carry: the provider has to retry the translation against a root with its
stride table appended and bind that table for the dispatch. Every record must
then start with its expected marker and keep its decoy; a refused pipeline, a
skipped dispatch or another stride leaves the record at its decoy. Stand-ins are
disabled. `verify` requires the paged stride table bind and a translation made
against the pipeline's root.

```sh
python tuxbox.py verify --only lanes-compute-heap-stride --output build/lanes-compute-heap-stride.json
```

The `lanes-bindless-compute` verification suite runs three native console programs under direct-indexed roots, each with a roomy and a constrained paged resource heap (resident limit 64 in the constrained run). `bindless-compute` stores through the heap UAV at logical slot 80. `bindless-sampler` samples the heap texture at slot 90 with a linear sampler built from literal words, which the root does not declare, and stores the value through the UAV at slot 85. `bindless-table-compute` is `bindless-compute` under a direct-indexed root that also has a bound table of four UAVs, which the program does not read. `bindless-heap-read` runs the first program beside a guest texture whose UAV sits at slot 100, holds the dispatch on a label while the title rewrites the texture's memory, and fails if the provider then writes the texture back. `bindless-texture-store` stores one word to one texel of that texture and checks the GPU image and the title's memory. Each run requires a translation made against the pipeline's root, the paged map bind and disabled stand-ins.
