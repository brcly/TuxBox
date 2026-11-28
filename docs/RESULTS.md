# Measured homebrew results

Each column represents one run on its named host, provider and hardware. Each receipt records the guest output, exit status and binary hashes.

| Gate | [arch-rtx5090-valve-amd](../evidence/arch-rtx5090-valve-amd.json) | [bazzite-rtx3080ti-valve-nvidia](../evidence/bazzite-rtx3080ti-valve-nvidia.json) | [cachyos-rx6900xt-valve-amd](../evidence/cachyos-rx6900xt-valve-amd.json) | [cachyos-rx6950xt-valve-amd](../evidence/cachyos-rx6950xt-valve-amd.json) | [ge-nvidia](../evidence/ge-nvidia.json) | [lanes-checks-rtx4070tis](../evidence/lanes-checks-rtx4070tis.json) | [nixos-rx9060xt-valve-amd](../evidence/nixos-rx9060xt-valve-amd.json) | [valdi-bazzite-rtx4080](../evidence/valdi-bazzite-rtx4080.json) | [valve-amd](../evidence/valve-amd.json) | [valve-nvidia](../evidence/valve-nvidia.json) | [windows11-rtx4060laptop-native](../evidence/windows11-rtx4060laptop-native.json) | [windows11-uhd-a78b-native](../evidence/windows11-uhd-a78b-native.json) | [wine-nvidia](../evidence/wine-nvidia.json) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Upper route and guest output | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| Lower route and collision | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| Invalid input exit status | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| Root signature 1.0 and 1.1 serialization, deserialization and errors | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| D3D12 and DXGI identifier data by name and ordinal | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| xg-dcc | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| xg-library | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| TuxBox XG texture layouts and element addresses | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| TuxBox XG layouts and addresses across 78 formats | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Game Runtime initialization, task delivery and cancellation | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Opus PCM, decoder pre-skip and command completion | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Xbox storage file reads, zlib and queue completion | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| CPU title and private heap creation | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| CPU heap attribute profiles | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| CPU virtual-memory allocation profiles | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| CPU scattered physical-page mapping | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Guest write-watch dirty-page query | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Guest allocator callback routing | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Memory allocation tag encoding | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| xmem-http | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| CPU virtual memory and image queries | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Xbox memory allocation and working set | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| xmem-partitions | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| CPU page allocation, remapping and release | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Desktop D3D12 pixel readback | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| Xbox root-signature import and compute readback | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| Blob, debug interface, four DXGIX entry points and Series CPU memory clear | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Xbox capability records compared with the selected adapter | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Placed resources and allocation offsets in a shared memory heap | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Virtual-address BC1 cubes and R8 textures across page changes | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Whole-buffer and multi-mip texture copies through Xbox command-list slot 84 | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Scissored triangle through Xbox graphics pipeline and draw slots | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Mesh and amplification shader dispatch on a mesh-tier adapter | Pass | — | Pass | — | — | — | — | Pass | — | — | — | — | — |
| Frame origins, timeout tokens, completed presentation events and display statistics | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Shader reads and writes through tables, including a late-copied texture view | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Direct, compute and copy queue synchronization | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| GPU timestamps, occlusion readback and descriptor heap handles | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Indirect dispatch and texture cache flushes between compute writes | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Xbox Device2 graphics pipeline stream, ownership and masked draw | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Xbox 0x20004 and default factories, compute queues and process debug flags | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Xbox command-list clear and ordered copy readback | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Fail |
| RGB10A2 display allocation and two guarded GPU readbacks | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Guest command rejection, fence isolation and list recovery | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Fail |
| Xbox PresentX and two screen-verified frames | Pass | Pass | Pass | Pass | Fail | Pass | Pass | Pass | Fail | Pass | Pass | Pass | Fail |
| Two RGB10A2 frames sampled from the desktop window | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| 4K RGB10 display scaled to a TuxBox window | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Fail | Fail | — |
| 4K RGB10 display fitted to the title window’s monitor | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| 4K RGB10 display scaled inside a smaller title window | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| Lanes Xbox imports, course, board readback and gameplay replay | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass | Pass | — |
| 8,388,608-move CPU replay | Pass | Pass | Pass | Pass | Pass | Pass | Pass | — | Pass | Pass | Pass | Pass | Pass |
| Xbox DXC utilities and binary blob lifetime | — | — | — | — | Pass | Pass | — | Pass | Pass | Pass | — | — | Pass |
| Native Scarlett XG texture layouts with Rust XMem | — | — | — | — | Pass | — | — | — | Pass | Pass | — | — | Pass |
| Lanes gameplay replay (earlier guest with desktop graphics) | — | — | — | — | Pass | — | — | — | Pass | Pass | — | — | Pass |
| Xbox DXC caller allocator and factory ordinals | — | — | — | — | — | Pass | — | Pass | — | — | — | — | — |
| Scarlett DXC compute shader and compiler diagnostics | — | — | — | — | — | Pass | — | Pass | — | — | — | — | — |
| xdxr-link | — | — | — | — | — | Pass | — | Pass | — | — | — | — | — |
| Bottom-of-pipe label writes with and without a flush | — | — | — | — | — | Pass | Pass | — | — | — | Pass | Pass | — |
| Compute and direct queues waiting on each other's labels | — | — | — | — | — | Pass | Pass | — | — | — | Pass | Pass | — |
| Compute queue frame submission | — | — | — | — | — | Pass | Pass | — | — | — | Pass | Pass | — |
| A skipped command leaving its list and fence write intact | — | — | — | — | — | Pass | Pass | — | — | — | Pass | Pass | — |
| CopyMemoryToMemoryX direction and contents | — | — | — | — | — | Pass | Pass | — | — | — | Pass | Pass | — |
| A waiting queue reading a texture another queue writes | — | — | — | — | — | Pass | Pass | — | — | — | Pass | Pass | — |
| ExecuteIndirect matching a direct dispatch | — | — | — | — | — | Pass | Pass | — | — | — | Pass | Pass | — |
| A guest texture copied back to title memory | — | — | — | — | — | Pass | Pass | — | — | — | Pass | Pass | — |
| Single, copied and batched builds through XboxBvhBuilder | — | — | — | — | — | Pass | Pass | — | — | — | Pass | Fail | — |
| Lanes frame, submission, fence handover, copy and pipeline timings | — | — | — | — | — | Pass | Pass | — | — | — | Pass | Pass | — |
| A console shader translated before launch, loaded from the prepared cache | — | — | — | — | — | Pass | Pass | — | — | — | — | — | — |
| A console shader translated by the host worker while the game runs | — | — | — | — | — | Pass | Pass | — | — | — | — | — | — |
| The worker's translation reused on the next launch without compiling | — | — | — | — | — | Pass | Pass | — | — | — | — | — | — |
| Slot 80 thread-dimension dispatch converted using the bound PSO group size | — | — | — | — | — | — | Pass | — | — | — | Pass | Pass | — |
| Shader-visible descriptor slot reclamation across completed, in-flight, ClearState, and OOM lists | — | — | — | — | — | — | Pass | — | — | — | Pass | Pass | — |
| Two frames presented as two planes, the second blended over the first | — | — | — | — | — | — | — | — | — | — | Pass | Pass | — |
| Package splash over a fitted 4K title window until frames come steadily | — | — | — | — | — | — | — | — | — | — | Fail | Fail | — |
| TuxBox's performance panel on screen over Lanes | — | — | — | — | — | — | — | — | — | — | Pass | Pass | — |
| A compute queue held for a label whose recorded write is submitted six seconds later | — | — | — | — | — | — | — | — | — | — | Pass | Pass | — |
| GPU-written arguments controlling an indirect dispatch | — | — | — | — | — | — | — | — | — | — | Pass | Pass | — |
| Indirect arguments and counts read after the dispatch that wrote them | — | — | — | — | — | — | — | — | — | — | Pass | Pass | — |
| A mesh dispatch signature made only when the device reports mesh shaders | — | — | — | — | — | — | — | — | — | — | Pass | Pass | — |
| A memory wait before and after a compute dispatch | — | — | — | — | — | — | — | — | — | — | Pass | Pass | — |
| A direct queue waiting before and after a buffer copy | — | — | — | — | — | — | — | — | — | — | Pass | Pass | — |
| GPU texture writes published to title memory, leaving padding, closed pages and memory past the texture alone | — | — | — | — | — | — | — | — | — | — | Pass | Pass | — |
| A structure, its copy and a scene built in title memory that no resource names | — | — | — | — | — | — | — | — | — | — | Fail | Fail | — |
| Console structures rehydrated in title memory as host builds of their coded geometry, traced through a scene | — | — | — | — | — | — | — | — | — | — | Pass | Fail | — |
| A descriptor table with slots the title never wrote, in a heap whose title memory held leftover bytes | — | — | — | — | — | — | — | — | — | — | Pass | Pass | — |
| Descriptor slot reclamation with the guest heap kept in several CPU heaps, as when a host refuses one heap that large | — | — | — | — | — | — | — | — | — | — | Pass | Pass | — |
| A mesh dispatch signature refused when the host profile takes mesh shaders away | — | — | — | — | — | — | — | — | — | — | Pass | Pass | — |

Rebuild this table from the repository root with `python tuxbox.py status > docs/RESULTS.md`. A dash means that the gate was omitted from that run. [Current Lanes replay](../examples/lanes/README.md#deterministic-replay) · [Xbox DLL checks](LIBRARIES.md#run-the-provider-checks) · [Retail results and priorities](PROGRESS.md)
