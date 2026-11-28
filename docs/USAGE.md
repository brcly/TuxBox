# Running games

Options for `python tuxbox.py run`. `python tuxbox.py run --help` lists all of them.

## Shader preparation

On Linux, the launcher prepares shaders before opening Frosty, from a profile of its game files. ProjectScarlett records the shaders the game requests, but a normal launch doesn't wait for them: it translates shaders as the game asks. After an update that changes the shader translator, run `python tuxbox.py run --prepare-only` first. Preparing the recorded requests together is much faster than translating them during play.

The launcher shows compilation progress and reuses the cache on later runs. After a TuxBox update, shaders that translate to the same source reuse their compiled output. While ProjectScarlett runs, a small panel over the game shows shader and pipeline progress. Pass `--prepare-only` to prepare the cache without opening the game, or `--no-shader-prep` to skip preparation for one launch. Place either option before the game path.

On Linux, each prefix keeps its GPU caches in its `shadercache` folder: vkd3d-proton's pipeline cache and, on NVIDIA, the driver cache. TuxBox asks the NVIDIA driver not to trim its cache, with a 4 GiB limit. A larger cache file stopped being indexed, so every launch compiled the same pipelines again. A launch reports a cache file that has outgrown its index and leaves the file in place. [Environment variables](ENVIRONMENT.md) lists these and the other settings.

## Project Scarlett graphics trials

On Linux, put `--graphics-menu` before the game path to choose the packaged settings or experimental Series S mode. Use `--graphics-profile series-s` to select it from a script. Series S mode uses Bubblewrap and a temporary copy of the Wine prefix. The copy is discarded at exit.

The Series S trial requested a 1920×1080 render resource and stopped on a texture allocation before presenting. Use the packaged settings for the current playable startup path.

## Host profile

TuxBox chooses shader translations and fallbacks by what the graphics driver can do, not by who made the card. The provider logs the profile it found when it first imports game memory:

```text
host profile: adapter=10de:2705 wave=32-32 section-import=aliased descriptor-bytes=32/32 mesh=yes rays=yes
```

| Field | Meaning |
| --- | --- |
| `wave` | The fewest and most lanes a shader wave can have |
| `section-import` | Whether the GPU reads the game's own memory after importing it: `aliased`, `substituted` (the driver reported success but the GPU read something else), `refused` or `unknown` |
| `descriptor-bytes` | Descriptor sizes for views and samplers |
| `mesh` | Whether the card runs mesh shaders: `yes` or `none`. Without them, mesh dispatch signatures are refused with `E_NOTIMPL` |
| `rays` | Whether the card traces rays: `yes` or `none`. Without it, ray dispatch signatures are refused with `E_NOTIMPL` |

`TUXBOX_HOST_PROFILE` replaces what the driver reports, so one machine can try a path meant for another. For example, `wave=32` makes a 64-lane card take the 32-lane translations. Settings are separated by `;`, and one unusable setting rejects them all. Overridden values are marked `(set)` in the log. Forcing `section-import=substituted` takes the [memory copy](#memory-copy) on a card that doesn't need it. `mesh=none` and `rays=none` take a capability away; an override can't add one the driver lacks. `cpu-view-heap=1048576` refuses any CPU-only view heap above that many descriptors, as some drivers do without saying so, so a large guest heap is kept in several smaller CPU heaps. An override cannot reproduce another driver's own behaviour.

### Memory copy

Mesa's RADV driver on Linux does not import the memory a game's pages live in. vkd3d-proton still reports success, and the GPU reads other memory. When the profile shows `substituted` or `refused`, TuxBox imports a private copy of those pages instead and copies the CPU's changes into it before each submission. Hosts that report `aliased` never take this path.

The copy has known limits:

- **GPU writes stay in the copy.** The CPU, and other GPU addresses of the same pages, don't see them.
- **It costs time.** Changed memory is found and copied before every submission.
- **Retail titles aren't proven.** In a contributor's tests of the change this comes from ([#200](https://github.com/brcly/TuxBox/pull/200), RX 6700 XT), ProjectScarlett reached the account picker far more slowly than with direct imports, and crashed in two of three runs.

A way for RADV to read the game's pages directly would replace it ([#191](https://github.com/brcly/TuxBox/issues/191)).

## Performance overlay

Add `--overlay` before the game path to draw TuxBox's own panel over the game. It works through TuxBox's graphics provider, so it needs no other software.

| Shows | Notes |
| --- | --- |
| Frame rate, 1% low, frame times and their graph | Measured between presented frames. The panel freezes when the game stops presenting. |
| Processor use, the whole system and the game | |
| Graphics card load, temperature, clock and power | From NVIDIA's management library, or the Linux kernel's sensor files. AMD and Intel cards on Windows show `-`. |
| Game memory, system memory, swap and video memory | |
| Shaders being translated, pipelines being built and empty stand-ins | |

```sh
python tuxbox.py run --overlay --app-id Lanes build/lanes
```

`TUXBOX_OVERLAY_POS` sets the corner (top left by default), and `TUXBOX_FRAME_LOG` writes every frame time to a CSV file. [Environment variables](ENVIRONMENT.md) has both. The overlay's own cost and its behaviour on Windows haven't been measured.

## Game logs

`tuxbox.py run` shows game output in the terminal as it arrives. It saves no copy unless you pass `--log-file` before the game path. Each run appends to that file.

```sh
python tuxbox.py run --log-file build/lanes.log --app-id Lanes build/lanes
```

On Linux, add `--mangohud` before the game path to see FPS, frame times and hardware load while playing. TuxBox saves a CSV of presented frames and a `summary.json` with average FPS and median, p95 and p99 frame times in a capture folder beside the log file. Without `--log-file`, the folder goes in `~/.local/state/project-tuxbox/logs/` (or under `XDG_STATE_HOME`). The Frosty launcher enables this automatically when MangoHud is installed.

On Linux, add `--offline` before the game path to launch without network access. The game runs in a new network namespace, so its crash handler cannot upload reports. ProjectScarlett's handler sends minidumps to an online crash service, so TuxBox always launches it this way. Offline launches need unprivileged user namespaces.

```sh
python tuxbox.py run --offline /path/to/game
```

Run `python tuxbox.py inspect /path/to/game` to see the executable TuxBox will launch and what it found in the package’s `.bin` files. For Frosty, its shipping symbols can also turn addresses from the log into function names. [Symbol lookup example](MATRIX_AWAKENS.md#what-carries-over-to-projectscarlett)
