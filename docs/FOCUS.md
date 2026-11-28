# Where to improve

Open areas that need work, with what is known and a useful next step. [Progress](PROGRESS.md) records what works today. If you start on one, say so in an issue so effort isn't doubled. The [roadmap](ROADMAP.md) breaks shader translation and a project-wide audit into numbered tasks. Measurements below are from ProjectScarlett 1.0.2.1 on a Ryzen with 16 threads, 32 GB of memory and an RTX 4070 Ti SUPER, unless noted.

## Memory

ProjectScarlett now gets into the world on a 32 GB machine with a 16 GB card, but needs far more than the console. The console is one chip with a single pool of shared memory; a PC splits it between system memory and the graphics card, and adds the translation layer, driver and compilers on top.

What TuxBox does about it, as of November 28, 2026:

- Graphics pipelines past the first 4,096 are released after compiling and rebuilt when first drawn (`TUXBOX_RESIDENT_PIPELINES`). One launch had committed 8.8 GiB at the player screen, against 13.9 GiB without this.
- A sampled texture first used when the card is nearly full goes to system memory (`TUXBOX_VRAM_RESERVE_MIB`).
- A texture that the next one starts inside gets memory only for the small mips that end before it (`TUXBOX_PARTIAL_MIPS`). At the prologue about 7,900 textures were cut this way, saving about 2.4 GiB. The rule rests on a sample of one compressed format, and the picture hasn't been checked for missing detail.
- With `TUXBOX_LOG`, the log shows every five seconds what the card, system memory and placed textures hold.

Open work:

- **Account for the rest.** The title claims about 7 GiB of physical memory at startup. Much of the remaining process memory, a jump of several GiB during the prologue load, and several GiB of video memory outside textures have no known owner. About 0.9 GiB of resident memory held only zeros in one launch.
- **Share overlapping textures.** After the change above, about 1 GiB of texture memory still repeats ranges another texture covers.
- **Give games the console's budget.** A Series X leaves a game 13,312 MiB. `XMemGetWorkingSetStatistics` still reports a budget based on the whole PC and counts the whole process as used. Report the budget with the game's own allocations as used, make `XMemGetFreePageStatistics` agree, and check what the game does with `GlobalMemoryStatusEx`. The title was not seen asking in earlier traced launches.
- **Overflow to disk.** A host whose only swap is compressed RAM has nowhere to spill. Creating Linux swap needs administrator rights, so TuxBox must not do it silently.
- **AMD.** None of the November 28 memory changes has run on an AMD card in the game ([#292](https://github.com/brcly/TuxBox/issues/292)).

## GPU faults and speed

- ProjectScarlett's run ends with Xid 13 in the world's opening scene. `TUXBOX_TRACE_GPU_FAULT=1` names the unfinished draws; in one launch they all used the distant lights shaders, which index textures and buffers with values read from game data.
- Frames in that scene took 11 to 17 seconds. Measure where the time goes before changing anything.

## Startup and shader compilation

A warm launch reached player selection in 69 s. Cold launches take many minutes, and it is too slow. The first launch, with nothing cached, matters most, and fixes should help any title.

- **Measure each stage.** A shader goes from console code to HLSL, DXC (DXIL), dxil-spirv in vkd3d-proton, then the driver. Compare time and memory per stage for a cold and a warm start.
- **Make DXC faster.** Try its settings, or our own build of it. Lifetime markers are now off for every shader. With the simpler branch and mesh code, 800 random ProjectScarlett programs compiled 1.26 times as fast offline: 3.9 times for `ms_6_6`, but only 1.05 times for `cs_6_6`, half of the total. A DXC that lists `-fnew-inlining-behavior` now gets it, which compiled 800 programs 1.31 times as fast. Splitting compute shaders into small functions made DXC about 3.4 times as fast on 12 large ones, but dxil-spirv could not convert the split shaders that otherwise built. No launch gain has been measured.
- **Benchmark a shorter route.** On Linux, translating straight to SPIR-V would drop DXC and dxil-spirv. Windows would keep DXIL. Compare both against the per-stage baseline before building either.
- **Measure speed in the checks.** A change that alters compile speed without changing coverage should show in `tuxbox.py check`, with outputs still verified.
- **Fewer empty stand-ins.** A large compile that passes its deadline is replaced by an empty pipeline for the whole session. Swapping in the finished result later is untested. Lowering the number of large compiles with available memory went with hundreds more stand-ins in one cold launch; the cause was not isolated.
- **Large compute shaders.** Single driver compiles took up to 158 s. Smaller or better structured translated code would help every stage.
- **Later:** translate the shaders seen in earlier launches on all cores before the game asks.

## Pipeline and driver caches

- The NVIDIA driver stopped updating its cache index once one file grew past about 5 GB, so every launch recompiled the same pipelines. TuxBox now limits the cache to 4 GiB. Check that long sessions stay warm, and find the real limit.
- A new translator version retranslates shaders on the next launch. Measure how much of that cost can be avoided.

## Audio start-up

ProjectScarlett's presenter shares a lock with its audio output. When audio start-up was slow, the game's own code left that lock held and froze presentation; TuxBox now patches that. The slow start came from a failing USB audio device on the host: while it was enabled, streams timed out and piled up.

- Find out why one failing device stalls streams on another output, and whether Wine can recover or report it.
- Check whether the black screen before player selection is the splash screen that should be drawn there.

## Frame pacing

At high frame rates ProjectScarlett shows pairs of frame time spikes between otherwise even frames. Capture them with `TUXBOX_FRAME_LOG` or `--mangohud` and find what runs at those moments.

## Shader translation

Compile time comes first (above), then unsupported shaders; a larger change with some risk is acceptable if it is verified. Preparation on November 28 found 165 unsupported programs among 19,017 it had requests for, of 25,889 in the shader pack ([refusals](SHADERS.md#projectscarlett-refusals)). Not every unsupported program is one a live run needs. In a launch the same day, the causes that blocked the most pipelines were group sums in divergent control flow (43), lane reads inside non-uniform loops (24), waves that must be complete (15) and unrecognized mesh launch calculations (15).

- Many pipelines still get empty stand-ins because a stage can't be translated. Rank them with the [pipeline failure record](PROJECTSCARLETT.md#finding-why-pipelines-fail), by pipelines blocked rather than by refused requests. A refusal for a missing buffer stride is often a first attempt that a retry translates.
- Check each instruction against AMD's RDNA 2 instruction set reference, which is also published as XML with its developer tools. AMD's Radeon GPU Analyzer shows what a pattern compiles to; the offline Vulkan compiler in version 2.15 does not target the console's generation. Neither proves a translation correct: compare its output on the GPU with a reference.
- Group sums in divergent control flow need a proof, or a vote, that every lane of a wave takes the same path.
- dxil-spirv reports `Failed to remap` errors for some translated shaders. Find which bindings cause them.
- The two-lanes-per-thread translation for 64-lane programs passes synthetic tests. Its output hasn't been verified in a game.

## AMD and NVIDIA

- RADV substitutes other memory for imported game pages, and TuxBox copies them instead, which is slow and loses GPU writes ([#191](https://github.com/brcly/TuxBox/issues/191)).
- Shared code needs a run on both vendors. See [CONTRIBUTING](../CONTRIBUTING.md#amd-and-nvidia).

## GPU synchronisation

A queue held on a fence label is released after five seconds only when no recorded or running list will write the value. A rejected batch still drops everything its queue sends afterwards, so one refusal can stall the whole title ([#215](https://github.com/brcly/TuxBox/issues/215)).

## Keeping the ground rules

- Linux comes first, and changes must keep Windows working.
- Don't patch Proton. If its behaviour must change, hook it while the game runs, or look for another route.
