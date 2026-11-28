# Launch options and environment variables

`python tuxbox.py run <game>` needs no variables for a normal launch. This page lists what you can change.

## Launch options

Place options before the game path. Anything after the path goes to the game.

| Option | Default | Effect |
| --- | --- | --- |
| `--app-id ID` | none | Select a manifest entry by its Id |
| `--backend` | `proton` on Linux, `native` on Windows | `native`, `proton` or `wine` |
| `--proton PATH` | the pinned GE-Proton11-14 | Use another Proton distribution |
| `--prefix PATH` | `~/.local/share/project-tuxbox/prefixes/<proton>` | Wine prefix |
| `--offline` | off, always on for ProjectScarlett | Launch without network access (Linux) |
| `--log-file PATH` | off | Also append the game log to this file |
| `--mangohud` | off | Show and record frame times (Linux) |
| `--overlay` | off | Show TuxBox's [performance panel](USAGE.md#performance-overlay) |
| `--require-current-provider` | off | Refuse to launch unless the staged D3D12 DLL matches the built one |
| `--profile-pipelines` | off | Record pipeline creation times |
| `--prepare-only` | off | Prepare shader caches and exit |
| `--no-shader-prep` | off | Skip launch-time shader preparation |
| `--graphics-menu`, `--graphics-profile` | off | ProjectScarlett graphics trials |

## Set by the launcher

The launcher sets these for a Proton or Wine launch. A value you set yourself is kept, except where noted.

| Variable | Value | Purpose |
| --- | --- | --- |
| `VKD3D_SHADER_CACHE_PATH` | `<prefix>/shadercache` | vkd3d-proton's pipeline cache; `0` disables it. The launcher merges the last launch's `.write` archive into it, which GE-Proton11-14 otherwise discards |
| `MALLOC_ARENA_MAX`, `MALLOC_MMAP_THRESHOLD_`, `MALLOC_TRIM_THRESHOLD_` | `8`, `1048576`, `8388608` | glibc limits for Proton and Wine launches, so freed compile memory is shared and returned instead of piling up in per-thread arenas |
| `__GL_SHADER_DISK_CACHE` | `1` | NVIDIA driver cache; `0` leaves it off |
| `__GL_SHADER_DISK_CACHE_PATH` | `<prefix>/shadercache` | Gives each prefix its own NVIDIA cache |
| `__GL_SHADER_DISK_CACHE_SKIP_CLEANUP` | `1` | Asks the NVIDIA driver not to trim the cache |
| `__GL_SHADER_DISK_CACHE_SIZE` | 4 GiB | Size limit; larger cache files stopped being indexed |
| `__GL_SHADER_DISK_CACHE_APP_NAME` | `tuxbox<n>` | Cache file this launch writes. A new number starts at 1 GiB, because the driver cannot index past 4 GiB |
| `__GL_SHADER_DISK_CACHE_READ_ONLY_APP_NAME` | every other file | Cache files the driver also reads. Setting either name yourself leaves both alone |
| `TUXBOX_SHADER_PROGRAMS` | the count in `shaders.rpf` | ProjectScarlett only: the total the progress panel counts compiled shader programs against |
| `TUXBOX_LOG` | `1` (`stderr` on Windows) | Sends provider messages to the game log |
| `WINEDEBUG`, `VKD3D_DEBUG`, `DXVK_LOG_LEVEL` | warnings and errors | Backend messages in the game log |
| `UMU_RUNTIME_UPDATE` | `0` for offline launches (always set) | Stops umu-run from needing the network |

## Behaviour

| Variable | Default | Effect |
| --- | --- | --- |
| `TUXBOX_RUNTIME_SHADERS` | on | `0` stops translating shaders during play |
| `TUXBOX_RUNTIME_SHADER_JOBS` | half the CPU cores, at least 2 where there are two, at most 32 | Compiler jobs during play, 1 to 32 |
| `TUXBOX_SHADER_PREP_JOBS` | one per CPU core, at most 32 | Compiler jobs for ProjectScarlett preparation |
| `TUXBOX_SHADER_PREP` | `chase` | Frosty preparation: `0`, `1`, `menu` or `chase` |
| `TUXBOX_SHADER_PREP_UI` | on for ProjectScarlett | `1` shows a desktop progress window |
| `TUXBOX_SCARLETT_SEED_PACK` | off | `1` prepares every shader in `shaders.rpf` |
| `TUXBOX_LARGE_COMPILES` | `4` | Large compute pipelines compiling at once; `0` removes the limit |
| `TUXBOX_TRACE_PIPELINE_REQUESTS` | `0` | With diagnostic logging enabled, also log start and completion of short slot-62 compute pipeline attempts, including the selected shader cache request key and byte size |
| `TUXBOX_PIPELINE_DEADLINE` | `20` | Seconds, whole or fractional, before a large compile gets an empty stand-in, which the finished compile later replaces; `0` waits |
| `TUXBOX_RESIDENT_PIPELINES` | `4096` | Graphics pipeline streams kept resident. Later ones are compiled, so the driver caches them, then released and rebuilt at their first bind, which saves host memory. `all` keeps every one |
| `TUXBOX_LAZY_TEXTURES` | on | Placed textures get graphics memory when GPU work first uses them; `0` gives each its memory when it is created |
| `TUXBOX_VRAM_RESERVE_MIB` | `1536` | Video memory that sampled placed textures leave free; past it a texture gets system memory at first use. `off` keeps every texture on the card. Needs `TUXBOX_LAZY_TEXTURES` |
| `TUXBOX_NULL_PIPELINES` | on | `0` fails unsupported pipelines instead of giving empty stand-ins |
| `TUXBOX_PROGRESS_OVERLAY` | `auto` | `always` or `0` for the progress panel |
| `TUXBOX_PROGRESS_OVERLAY_POS` | `tr` | `tl`, `tr`, `bl` or `br` |
| `TUXBOX_CHECK_DISPLAY` | private | The window checks sample a private Xwayland on Wayland desktops; `desktop` uses the current display. The splash check is skipped, and reported as skipped, for a GPU that presents to the private server through software |
| `TUXBOX_CHECK_JOBS` | half the CPU threads, at most 8 | GPU checks run at once: the translator, the D3D12 controls and most Lanes and fixture gates |
| `TUXBOX_OVERLAY` | off | `1` shows the [performance panel](USAGE.md#performance-overlay) (`run --overlay`) |
| `TUXBOX_OVERLAY_POS` | `tl` | `tl`, `tr`, `bl` or `br` |
| `TUXBOX_FRAME_LOG` | off | File to write each frame's time to, as CSV; the game must be able to write it |
| `TUXBOX_KEEP_TITLE_WATCHDOG` | off | `1` leaves ProjectScarlett's hang watchdog running while TuxBox holds a title thread |
| `TUXBOX_KEYBOARD_GAMEPAD` | on | `0` leaves only physical XInput controllers |
| `TUXBOX_STORAGE_THREADS` | CPU cores, at most 8 | Storage request threads |
| `TUXBOX_MANGOHUD_PCI` | chosen automatically | PCI address of the GPU MangoHud reports |

## Testing another path

| Variable | Effect |
| --- | --- |
| `TUXBOX_HOST_PROFILE` | Replace what the [graphics driver reports](USAGE.md#host-profile) |
| `TUXBOX_PERSISTENT_DXC` | On Linux the launcher builds a [DXC helper](PROJECTSCARLETT.md#dxc-helper) that compiles shaders faster; `0` turns it off, a path uses that helper |
| `TUXBOX_REFERENCE_CBV` | `1` applies the constant-buffer layer to a provider TuxBox didn't build |
| `TUXBOX_RASTER_ROOT_FALLBACK` | Try an unsupported raster root signature unchanged |
| `TUXBOX_DESCRIPTOR_HEAP_LIMIT` | Cap shader-visible descriptor heaps to force paging |
| `TUXBOX_REQUIRE_BUFFER_IMPORT` | `1` fails instead of falling back when memory can't be imported |
| `TUXBOX_RUNTIME_SERVICES` | `0`, or a list such as `XUser,XGame`, limits the Xbox runtime services |
| `TUXBOX_RUNTIME_PROBE` | `1` also exposes the unidentified runtime class |
| `TUXBOX_PACKAGED` | `1` reports a packaged process |
| `TUXBOX_SURVEY` | `1` continues past recoverable failures to report several in one launch |

## Diagnostics

All are off by default. Capture variables write the game's data to disk, so don't share the output.

| Variable | Reports or saves |
| --- | --- |
| `PROTON_LOG`, `PROTON_LOG_DIR` | Proton's own log |
| `TUXBOX_TRACE_SHADERS` | Shader cache lookups |
| `TUXBOX_TRACE_PRESENT` | Presented frames and writes to display resources |
| `TUXBOX_TRACE_RASTER_DRAWS`, `TUXBOX_TRACE_RASTER_ROOTS`, `TUXBOX_TRACE_RASTER_INPUTS` | Draws, their roots and inputs |
| `TUXBOX_TRACE_RAYTRACING` | Acceleration structures and ray pipelines |
| `TUXBOX_TRACE_STENCIL` | Stencil export routing |
| `TUXBOX_TRACE_HEAP_IMPORTS`, `TUXBOX_TRACE_BUFFER_BACKING` | How guest memory reaches the GPU |
| `TUXBOX_TRACE_ALLOCATION_BYTES` | Allocations of the listed sizes |
| `TUXBOX_WATCH_ADDRESS` | Allocations and resources near `<hex>[,<hex radius>]` |
| `TUXBOX_XG_TRACE` | XG address calculations |
| `TUXBOX_XCRD_LOG` | DirectStorage requests counted by option word (zlib, BCPack mode, swizzle, page and memory flags) with their sizes, every five seconds and when a queue closes. Requests with options the provider can't handle are logged once per option word without it |
| `TUXBOX_WAVE_DEBUG` | Why the translator didn't mask a wave reduction |
| `TUXBOX_CAPTURE_SHADER_INPUTS`, `TUXBOX_CAPTURE_RASTER_ROOTS`, `TUXBOX_CAPTURE_RAY_ROOTS` | Folder for shader inputs and root data |
| `TUXBOX_CAPTURE_BLAS` | Folder for acceleration structure inputs |
| `TUXBOX_CAPTURE_RASTER_PAIR`, `TUXBOX_CAPTURE_DRAW_READBACK` | Selected draws and their output |

## Build and test tools

`TUXBOX_XSHADER` selects the shader compiler for launcher preparation and `DXC` selects the DirectX Shader Compiler. `TUXBOX_XSHADER_COVERAGE` names a file to which the compiler appends the instructions of every program it emits; the [shader harness](SHADER_HARNESS.md) sets it for its GPU checks. `TUXBOX_BLESS_PROFILES`, `TUXBOX_ORACLE_*`, `TUXBOX_TEST_*` and the probe and replay variables in `tools/` belong to individual checks. `TUXBOX_PROVIDER`, `TUXBOX_SHADER_SESSION` and `TUXBOX_WINE_*` are internal; don't set them.


Cold shader requests that translate to identical HLSL and the same target
profile share a single DXC compilation across launcher processes using the
same toolchain cache. Exact source cache hits still bypass compilation and
locking. Different HLSL or target profiles compile independently. A failed
compile does not publish an unsupported marker for another source; the next
request can retry. Content locks are retained as empty files to avoid races
with other processes. The wait is bounded by the maximum compiler fallback
budget plus validation and scheduling margin.
