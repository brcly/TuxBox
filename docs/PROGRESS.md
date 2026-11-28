# Progress and compatibility

The overall bar gives equal weight to verified library behavior and RDNA 2 instruction coverage. The library percentage counts the reviewed library behaviors games need. The instruction percentage counts the [RDNA 2 instructions](INSTRUCTIONS.md) TuxBox converts to HLSL that compiles to DXIL: one translated alone compiled through DXC and DXV, or a GPU check ran it. Compiling doesn't prove a result matches the console; the GPU checks compare results. Each library behavior counts once, including paths that use packaged or host DLLs. Each verified behavior needs an observable result from the game or a focused check. The percentage describes this workload and moves as games expose new requirements. [Progress map](progress.svg) · [Library audit](LIBRARIES.md) · [Instructions](INSTRUCTIONS.md) · [Shader survey](SHADERS.md)

Project Scarlett shader preparation compiles 36,653 of 41,064 captured requests (89.3%), with 4,411 unsupported and no compiler failures. The pass took 23 minutes with 16 jobs after a translator update, reusing 24,129 compiled shaders. The captured requests come from recorded runs of this build. [Preparation results](../evidence/projectscarlett-shader-preparation.json)

With the packaged Xbox graphics settings on stock Proton, Frosty rendered its main menu and accepted Start Chase. The game played Wake Up and activated the first street corner of the chase intro. The NVIDIA driver reported a GPU memory fault before the shooting tutorial. Earlier runs with reduced settings reached the shooting tutorial and the city. [Original-settings menu](../evidence/matrix-awakens/packaged-settings-menu.webp) · [Wake Up transition](../evidence/matrix-awakens/packaged-settings-room.json) · [Earlier chase and city](MATRIX_AWAKENS.md)

## Games

| Title | Current result | What remains |
| --- | --- | --- |
| **Lanes** | Runs through TuxBox's Xbox graphics, memory and input providers. On an RTX 4070 Ti SUPER under stock Proton, the guest checks (Lanes' replay, self-test, shader cache and benchmark checks among them) and the D3D12 regression controls pass. A change must pass the [checks relevant to it](../CONTRIBUTING.md#test-what-your-change-touches) before it is merged. | Add shader forms the translator often refuses, and run the checks on more systems. [Lanes](../examples/lanes/README.md) · [Guest check results](../evidence/lanes-checks-rtx4070tis.json) |
| **The Matrix Awakens (Frosty)** | With the original Xbox graphics settings, a Linux run rendered the menu, played Wake Up and loaded the first street corner of the chase intro. Earlier reduced-settings runs reached the highway and city with missing and distorted surfaces. | Verify the shooting tutorial, image quality and frame times with the packaged settings. [Setup and results](MATRIX_AWAKENS.md) · [Recorded original-settings run](../evidence/matrix-awakens/packaged-settings-room.json) |
| **ProjectScarlett (GTA VI 1.0.2.1)** | On November 28 an offline Linux run rendered the opening world scene, with frames of 11 to 17 seconds, and ended on an Xid 13 GPU fault about five minutes after launch. Earlier runs ended before the world: on full video or system memory, on a GPU write fault, or on stalled fence waits. [Earlier memory run](../evidence/projectscarlett-memory-run.json) · [Run record](../evidence/projectscarlett-gameplay.json) · [Startup record](../evidence/projectscarlett-startup.json) | Fix the Xid 13 fault, cut frame times and memory use, support the remaining shaders, then check images, input and audio, and run on AMD. [Setup guide](PROJECTSCARLETT.md#what-to-expect) |
| **Fortnite** | One supplied build provided graphics API call-site evidence. | Test initialization and rendering in TuxBox for a specific build. [Device evidence](../evidence/fortnite-device.json) |

No other retail title is currently claimed playable. Record the build, settings, host, provider and observed faults for each run, and distinguish starting, rendering, input and sustained gameplay.

## Current priorities

### ProjectScarlett

1. Find which access in the distant lights draws raises Xid 13 in the world's opening scene. Reproduce the cause as a [Lanes self-test](../examples/lanes/README.md#self-test) scenario first.
2. Measure why a frame in that scene takes 11 to 17 seconds.
3. Account for the memory beyond the console's, and run the November 28 memory changes on AMD. [Where to improve](FOCUS.md#memory)
4. Make shaders compile quickly enough. A cold launch spends many minutes translating, and single large pipelines have taken the driver about three minutes (earlier runs on another Proton build saw most of an hour). [Shader coverage](SHADERS.md#projectscarlett-refusals)
5. Support the remaining refused shader forms, ranked with the [pipeline failure record](PROJECTSCARLETT.md#finding-why-pipelines-fail).

### The Matrix Awakens

1. Finish the five remaining non-ray-tracing pixel shaders. Two light hair strands and three cull distance-field objects. Check their output with the packaged bindings on both GPUs. [Shader survey](SHADERS.md#measured-on-the-matrix-awakens)
2. Identify the GPU command that faults while the first chase street corner loads, then verify the shooting tutorial with the packaged settings. [Retail result](../evidence/matrix-awakens/packaged-settings-room.json)
3. Check the menu, characters, hair, vehicles and city in the game. Test input and audio, then measure cold and warm frame times.
4. Repeat title and provider checks on additional hardware and games as their executables become available.

The default compiler still contains a TomBox-derived HLSL emitter. TuxBox's independent emitter is growing, but it does not yet replace the legacy path for all supported shaders. [Source attribution](REFERENCES.md#shader-compiler-attribution)
