"""Behavioral gates for the project-owned guest fixtures."""
from contextlib import contextmanager, nullcontext
import ctypes
import ctypes.util
import copy
import hashlib
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import select
import statistics
import re
import shutil
import struct
import sys
import subprocess
import tempfile

from .jobs import check_jobs
from .prefix_pool import PrefixPool
from .inspect import digest, export_table, image, launch_target

ROOT = Path(__file__).resolve().parents[1]
BUILD = ROOT / "build"
BASE_SUITES = ("cpu", "xmem", "xapu", "xgameruntime", "xcrdapi", "graphics", "graphics-abi", "guest-frame", "present", "lanes", "lanes-selftest", "lanes-shader-cache", "lanes-raster-tables", "lanes-compute-root", "lanes-compute-root-recovery", "lanes-compute-root-srv", "lanes-large-dispatch", "lanes-late-pipeline", "lanes-bindless-compute", "lanes-compute-heap-stride", "lanes-benchmark",
               "cpu-benchmark", "d3d12-exports", "d3d12-features", "d3d12-indirect", "d3d12-queries", "d3d12-queues", "d3d12-tables",
               "d3d12-heaps", "d3d12-placement", "d3d12-copy-resource", "d3d12-draw", "d3d12-stream", "d3d12-frame-events",
               "d3d12-guids", "d3d12-signatures", "xg-provider")
OPTIONAL_PROVIDERS = {"dxc-utils": "dxc-utils/dxcompiler.dll", "xg": "xg/xg_xs.dll"}
SUITES = (*BASE_SUITES, "d3d12-mesh", *OPTIONAL_PROVIDERS)
# The scenarios of `lanes.exe --selftest`, one gate each.
LANES_SCENARIOS = ("labels", "cross-queue", "compute-frame", "skipped-command", "copy-memory", "shared-texture",
                   "indirect-dispatch", "gpu-indirect-dispatch", "gpu-indirect-order", "mesh-signature",
                   "dispatch-wait", "direct-copy-wait", "texture-readback", "texture-publication",
                   "ray-tracing-builder", "ray-tracing-guest-memory", "ray-tracing-rehydrate",
                   "thread-dimensions",
                   "descriptor-reclaim", "unwritten-descriptors")


class LanesShader:
    """The Lanes console shader in a prefix's translation cache."""

    def __init__(self, host, data=None):
        if data is None:
            from examples.lanes.shader_fixture import shader
            data = shader()
        from .shader_prep import SOURCE_CACHE

        self.data = data
        self.key = hashlib.sha256(self.data).hexdigest()
        self.sources = Path(host.env["WINEPREFIX"]) / SOURCE_CACHE
        self.root = self.sources.parent

    def forget(self):
        # Only this synthetic shader's entries; a prefix may hold a title cache.
        for key in (self.key, f"{self.key}_w32"):
            for path in [*self.sources.glob(f"{key}.*"), *self.root.glob(f"runtime/*/{key}.*")]:
                path.unlink()

    def prepare(self, work):
        """Translate it into the prepared cache, as shader preparation does."""
        from .runtime_shaders import compile_request
        from .shader_prep import compiler_tools

        self.forget()
        self.sources.mkdir(parents=True, exist_ok=True)
        request = work / f"{self.key}.dxbc"
        request.write_bytes(self.data)
        compile_request(request, self.key, compiler_tools(), self.sources, None)


def lanes_shader_cache(host, check, gpu_args, pci, memory_symbols, work):
    """Load the Lanes console shader from a prepared cache, then from the runtime worker.

    The prepared gate translates it before launch, as shader preparation does.
    The runtime gates start with it uncached: the first launch must have the
    host worker compile it, and the second must reuse that result.
    """
    from .runtime_shaders import shader_session

    lanes = LanesShader(host)
    key, root, forget = lanes.key, lanes.root, lanes.forget

    saved = {name: host.env.get(name) for name in ("TUXBOX_LOG", "WINEDEBUG", "TUXBOX_SHADER_SESSION")}
    host.env.update(TUXBOX_LOG="1", WINEDEBUG="-all,+debugstr")
    host.env.pop("TUXBOX_SHADER_SESSION", None)
    loaded = f"prepared compute shader loaded, source SHA-256 {key}"

    def expect_loaded(output):
        return None if loaded in output else "the provider did not load the translated shader"

    def launch(name, log_check):
        return check(name, "lanes/lanes.exe", ["--selftest", "xbox-shader", *gpu_args, "--report"],
                     rf"lanes-selftest PASS pci={pci} scenario=xbox-shader passed=1",
                     providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                                "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                     config="lanes/MicrosoftGame.config", device_factory="xbox", log_check=log_check)

    try:
        lanes.prepare(work)
        launch("lanes-shader-cache-prepared", expect_loaded)

        forget()
        for name, compiles in (("lanes-shader-cache-compiled", 1), ("lanes-shader-cache-reused", 0)):
            messages = []
            with shader_session(root, host.env, messages.append):
                launch(name, lambda output, messages=messages, compiles=compiles: expect_loaded(output) or (
                    None if sum(f"Compiled Xbox shader {key}" in line for line in messages) == compiles
                    else f"expected {compiles} worker compiles, saw: {messages}"))
        forget()
    finally:
        for name, value in saved.items():
            if value is None:
                host.env.pop(name, None)
            else:
                host.env[name] = value


def lanes_wave_shader(host, check, gpu_args, pci, memory_symbols):
    """Translate a console program that needs 64-lane waves while Lanes runs it.

    The first launch reports a 32-lane host to the provider, so every card takes
    the two-lanes-per-thread translation. The second leaves the card as it is.
    Both must write the console's results.
    """
    from examples.lanes.shader_fixture import wave_shader
    from .runtime_shaders import shader_session

    lanes = LanesShader(host, wave_shader())
    saved = {name: host.env.get(name) for name in
             ("TUXBOX_LOG", "WINEDEBUG", "TUXBOX_SHADER_SESSION", "TUXBOX_HOST_PROFILE")}
    host.env.update(TUXBOX_LOG="1", WINEDEBUG="-all,+debugstr")
    host.env.pop("TUXBOX_SHADER_SESSION", None)
    narrow = f"narrow-wave compute shader loaded, source SHA-256 {lanes.key}"
    try:
        for name, profile in (("lanes-wave-shader-narrow", "wave=32"), ("lanes-wave-shader-host", None)):
            lanes.forget()
            if profile:
                host.env["TUXBOX_HOST_PROFILE"] = profile
            else:
                host.env.pop("TUXBOX_HOST_PROFILE", None)
            messages = []
            with shader_session(lanes.root, host.env, messages.append):
                check(name, "lanes/lanes.exe", ["--selftest", "wave64-shader", *gpu_args, "--report"],
                      rf"lanes-selftest PASS pci={pci} scenario=wave64-shader passed=1",
                      providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                                 "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                      config="lanes/MicrosoftGame.config", device_factory="xbox",
                      log_check=lambda output, forced=profile is not None, messages=messages:
                          None if not forced or narrow in output
                          else f"the provider did not load the 32-lane translation, worker said: {messages}")
        lanes.forget()
    finally:
        for name, value in saved.items():
            if value is None:
                host.env.pop(name, None)
            else:
                host.env[name] = value


def lanes_live_wave_shaders(host, check, gpu_args, pci, memory_symbols):
    """Translate Wave32 programs live and verify the provider loads each result."""
    from .runtime_shaders import shader_session
    from examples.lanes.shader_fixture import lane_shader
    from examples.lanes.wave_alloc_fixture import shader as wave_alloc_shader

    saved = {name: host.env.get(name) for name in ("TUXBOX_LOG", "WINEDEBUG", "TUXBOX_SHADER_SESSION")}
    host.env.update(TUXBOX_LOG="1", WINEDEBUG="-all,+debugstr")
    host.env.pop("TUXBOX_SHADER_SESSION", None)

    def expect_loaded(output, key):
        loaded = f"prepared compute shader loaded, source SHA-256 {key}"
        return None if loaded in output else "the provider did not load the translated shader"

    try:
        for scenario, data in (("wave-alloc", wave_alloc_shader()), ("mixed-lane", lane_shader())):
            shader = LanesShader(host, data)
            shader.forget()
            try:
                with shader_session(shader.root, host.env, lambda line: None):
                    check(f"lanes-shader-cache-{scenario}", "lanes/lanes.exe",
                          ["--selftest", scenario, *gpu_args, "--report"],
                          rf"lanes-selftest PASS pci={pci} scenario={scenario} passed=1",
                          providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                                     "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                          config="lanes/MicrosoftGame.config", device_factory="xbox",
                          log_check=lambda output, key=shader.key: expect_loaded(output, key))
            finally:
                shader.forget()
    finally:
        for name, value in saved.items():
            if value is None:
                host.env.pop(name, None)
            else:
                host.env[name] = value


def cpu_model():
    if sys.platform == "linux" and Path("/proc/cpuinfo").exists():
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name") and ":" in line:
                return line.partition(":")[2].strip()
    return platform.processor() or platform.machine()


def guid_receipt(imports, provider):
    """The receipt line for the identifiers the guest imports from `provider`, by name or by ordinal."""
    symbols = {symbol for module in imports if module["module"].lower() == provider.name.lower()
               for symbol in module["symbols"]}
    ordinals = {symbol for symbol in symbols if symbol.startswith("#") and symbol[1:].isdigit()}
    if ordinals:  # MSVC's link.exe imports exported data by ordinal
        slots, _ = export_table(provider)
        symbols -= ordinals
        for symbol in ordinals:
            symbols.update(slots.get(int(symbol[1:]), ()))
    count = sum(name.startswith(("IID_", "WKPDID_")) for name in symbols)
    return f"d3d12-guids PASS identifiers={count} bytes={16 * count}"


def lanes_imports(pe):
    """Check the title boundary, including imports that are resolved on first use."""
    modules = sorted({entry["module"].lower() for key in ("imports", "delay_imports") for entry in pe[key]})
    providers = {"d3d12_xs.dll", "gameinput.dll", "xmem.dll"}
    unexpected = [name for name in modules if name not in providers and not (
        name.startswith(("api-ms-win-core-", "api-ms-win-crt-")) and name.endswith(".dll"))]
    if unexpected:
        raise ValueError(f"Lanes imports a library outside its Xbox title profile: {', '.join(unexpected)}")
    return modules


ROOTLESS_X = ("the X server refuses to read its root window (rootless XWayland), so screen samples "
              "read 0; run the present suite under xvfb-run or a rootful Xwayland")


def xauthority_entry(cookie):
    """One MIT-MAGIC-COOKIE-1 Xauthority record that matches any host and display number."""
    fields = (b"", b"", b"MIT-MAGIC-COOKIE-1", cookie)  # FamilyWild: empty address and number
    return struct.pack(">H", 0xFFFF) + b"".join(struct.pack(">H", len(f)) + f for f in fields)


def _free_display_number(start=2, attempts=16):
    """Pick a display number free of lock files and sockets, or None."""
    for candidate in range(start, start + attempts):
        if Path(f"/tmp/.X{candidate}-lock").exists():
            continue
        if Path(f"/tmp/.X11-unix/X{candidate}").exists():
            continue
        return candidate
    return None


def _drm_pci_id(node):
    """PCI vendor:device and kernel driver of a /dev/dri node, or (None, None)."""
    device = Path("/sys/class/drm") / Path(node).name / "device"
    try:
        ids = [(device / key).read_text().strip().lower().removeprefix("0x") for key in ("vendor", "device")]
        driver = Path(os.readlink(device / "driver")).name
    except OSError:
        return None, None
    return ":".join(ids), driver


def _adapter_driver(adapter):
    """Kernel driver of the render node for a PCI vendor:device, or None."""
    for node in sorted(Path("/dev/dri").glob("renderD*")):
        pci, driver = _drm_pci_id(node)
        if pci == adapter.lower():
            return driver
    return None


def _server_render_device(pid):
    """PCI vendor:device of the GPU an X server renders with, or None if it holds no DRM node."""
    try:
        nodes = [os.readlink(fd) for fd in Path(f"/proc/{pid}/fd").iterdir()]
    except OSError:
        return None
    for node in sorted(nodes):
        if node.startswith("/dev/dri/renderD"):
            return _drm_pci_id(node)[0]
    return None


@contextmanager
def _xwayland(xwayland, display, auth, geometry, timeout, options=()):
    """Yield (DISPLAY, pid) of a started rootful Xwayland, or None; stop it when the block exits."""
    # A driver cache path exported for the guest is not the server's. NVIDIA's
    # driver in Xwayland would share it without the launcher's cleanup opt-out,
    # and once the folder passes the default cap it deletes every file there
    # that no running process holds: the guest's pipelines, at each launch.
    env = {key: value for key, value in os.environ.items() if key != "__GL_SHADER_DISK_CACHE_PATH"}
    read, write = os.pipe()
    try:
        server = subprocess.Popen([xwayland, f":{display}", "-displayfd", str(write), "-geometry", geometry,
                                   "-noreset", "-auth", str(auth), *options], pass_fds=(write,), env=env,
                                  stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    except OSError:
        os.close(read)
        os.close(write)
        yield None
        return
    os.close(write)
    try:
        number = b""
        while not number.endswith(b"\n") and select.select([read], [], [], timeout)[0]:
            chunk = os.read(read, 16)
            if not chunk:
                break
            number += chunk
        # A server that exits before being ready is not a display we can use.
        if number.strip().isdigit() and server.poll() is None:
            yield (f":{int(number)}", server.pid)
        else:
            yield None
    finally:
        os.close(read)
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()


@contextmanager
def private_x_server(directory, geometry="2560x1440", timeout=10, adapter=None):
    """Yield (DISPLAY, XAUTHORITY, environment) for a temporary rootful Xwayland, or None.

    The rootful server draws into its own Wayland window and keeps a readable root window, so
    Wine's screen samples work on a Wayland desktop. It accepts only its own random cookie,
    is started on a display number picked below the desktop's and is stopped when the block exits.

    Xwayland takes its display number as the positional argument (":N"); "-displayfd" only
    reports the number it took, so a free one is chosen here instead of trusting the default,
    which is the inherited DISPLAY and collides with the desktop server.

    Xwayland renders on the compositor's GPU. When `adapter` is another Mesa-driven GPU, Mesa's
    X11 swapchain must cross GPUs over DRI3, which can fail outright (RADV against an NVIDIA
    Xwayland returns VK_ERROR_INITIALIZATION_FAILED), leaving the window unpainted. The server
    is then restarted without glamor, and `environment` asks Mesa to present through plain X
    images, which every GPU can do.
    """
    xwayland = shutil.which("Xwayland")
    if not sys.platform.startswith("linux") or not xwayland or not os.environ.get("WAYLAND_DISPLAY"):
        yield None
        return
    display = _free_display_number()
    if display is None:
        yield None
        return
    auth = Path(directory) / "Xauthority"
    descriptor = os.open(auth, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(xauthority_entry(os.urandom(16)))
    with _xwayland(xwayland, display, auth, geometry, timeout) as server:
        foreign = (server and adapter and _adapter_driver(adapter) not in (None, "nvidia")
                   and _server_render_device(server[1]) != adapter.lower())
        if not foreign:
            yield server and (server[0], str(auth), {})
            return
    with _xwayland(xwayland, display, auth, geometry, timeout, ("-glamor", "off")) as server:
        yield server and (server[0], str(auth), {"MESA_VK_WSI_DEBUG": "sw"})


def splash_skip_reason(display_environment):
    """Why the package splash check cannot run on this test display, or None.

    A GPU other than the desktop's presents to the private server through
    software (`private_x_server`). There the first frame after the splash never
    completes; the cause is not known. An AMD iGPU beside an NVIDIA desktop
    reproduces it, while the other present checks pass.
    """
    if "MESA_VK_WSI_DEBUG" in display_environment:
        return "this GPU presents to the test display through software"
    return None


def screen_capture_problem(display=None, library=None, xauthority=None):
    """Why the present checks cannot sample the screen on this host, or None.

    Wine backs GetDC(nullptr)/GetPixel with the X root window. The rootless XWayland of a
    Plasma or GNOME Wayland session rejects GetImage on that window with BadMatch, so every
    sample reads 0 even though the frames are presented. Only that refusal is reported;
    anything else is left for the guest check to report.
    """
    display = os.environ.get("DISPLAY") if display is None else display
    if not sys.platform.startswith("linux") or not display:
        return None
    try:
        x11 = ctypes.CDLL(library or ctypes.util.find_library("X11") or "libX11.so.6")
    except OSError:
        return None
    x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
    x11.XDefaultRootWindow.restype = ctypes.c_ulong
    x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    x11.XGetImage.restype = ctypes.c_void_p
    x11.XGetImage.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                              ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_int]
    x11.XDestroyImage.argtypes = [ctypes.c_void_p]
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
    handler_type = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)
    x11.XSetErrorHandler.restype = ctypes.c_void_p
    x11.XSetErrorHandler.argtypes = [handler_type]
    errors = []
    # Xlib's default handler exits the process; record the error instead.
    handler = handler_type(lambda _display, _event: errors.append(1) or 0)
    saved = os.environ.get("XAUTHORITY")
    if xauthority is not None:
        os.environ["XAUTHORITY"] = xauthority  # Xlib reads the cookie file when it connects
    try:
        connection = x11.XOpenDisplay(display.encode())
    finally:
        if xauthority is not None:
            if saved is None:
                os.environ.pop("XAUTHORITY", None)
            else:
                os.environ["XAUTHORITY"] = saved
    if not connection:
        return None
    previous = x11.XSetErrorHandler(handler)
    try:
        image = x11.XGetImage(connection, x11.XDefaultRootWindow(connection), 0, 0, 1, 1,
                              0xFFFFFFFF, 2)  # ZPixmap
        if image:
            x11.XDestroyImage(image)
    finally:
        x11.XCloseDisplay(connection)
        x11.XSetErrorHandler(ctypes.cast(previous, handler_type) if previous else handler_type())
    return ROOTLESS_X if errors else None


def missing_provider_imports(modules, provider, expected):
    """Return missing expected static imports, resolving #ordinal imports via provider exports."""
    found = {symbol for mod in modules if mod["module"].lower() == provider.name.lower()
             for symbol in mod["symbols"]}
    ordinals = [int(symbol[1:]) for symbol in found if symbol.startswith("#") and symbol[1:].isdigit()]
    if ordinals and not set(expected).issubset(found):
        slots, _ = export_table(provider)
        for ordinal in ordinals:
            found.update(slots.get(ordinal, ()))
    return sorted(set(expected) - found)


def lanes_compute_root(host, check, gpu_args, pci, memory_symbols):
    """Dispatch the Lanes root-less console compute programs under a root with three tables.

    The provider cannot reflect that root, so each program must be translated
    against it while the game runs. The first program's translation writes the
    scenario's word. The second is refused for this root and must stay refused:
    neither may fall back to a translation without the root, whose register
    the root does not declare.
    """
    from examples.lanes.shader_fixture import root_shader
    from .runtime_shaders import shader_session
    from .shader_prep import SOURCE_CACHE

    sources = Path(host.env["WINEPREFIX"]) / SOURCE_CACHE
    rootless = {hashlib.sha256(root_shader(offset)).hexdigest() for offset in (0x40, 0xA0)}
    messages = []

    def requested(prefix):
        return {match[1] for line in messages if (match := re.match(prefix + r"([0-9a-f]{64})\b", line))}

    def forget(keys):
        # Only these programs' entries; a prefix may hold a title cache.
        for key in keys:
            for path in [*sources.glob(f"{key}.*"), *sources.parent.glob(f"runtime/*/{key}.*")]:
                path.unlink()

    def expect_root_translations(output):
        compiled, refused = requested("Compiled Xbox shader "), requested("Xbox shader ")
        if (compiled | refused) & rootless:
            return f"the worker translated a program without its root: {messages}"
        if not compiled:
            return f"the worker translated no program against the three-table root: {messages}"
        if not any("unmapped descriptor at s5 + 0xa0" in line for line in messages):
            return f"the worker did not refuse the program that reads past table 1: {messages}"
        # Exactly one stand-in: the refused program's.
        if "substituted null compute pipeline for an untranslated shader (1 so far)" not in output:
            return "the provider did not give the refused program a stand-in"
        if "substituted null compute pipeline for an untranslated shader (2 so far)" in output:
            return "the provider substituted a stand-in for the program its root can bind"
        return None

    # This gate deliberately checks one refused program's stand-in. Do not
    # inherit the caller's policy for the real game or another verification gate.
    names = ("TUXBOX_LOG", "WINEDEBUG", "TUXBOX_SHADER_SESSION", "TUXBOX_NULL_PIPELINES")
    saved = {name: host.env.get(name) for name in names}
    host.env.update(TUXBOX_LOG="1", WINEDEBUG="-all,+debugstr", TUXBOX_NULL_PIPELINES="1")
    host.env.pop("TUXBOX_SHADER_SESSION", None)
    try:
        forget(rootless)
        with shader_session(sources.parent, host.env, messages.append):
            check("lanes-compute-root", "lanes/lanes.exe", ["--selftest", "compute-root", *gpu_args, "--report"],
                  rf"lanes-selftest PASS pci={pci} scenario=compute-root passed=1",
                  providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                             "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                  config="lanes/MicrosoftGame.config", device_factory="xbox", log_check=expect_root_translations)
    finally:
        forget(rootless | requested("Compiled Xbox shader ") | requested("Xbox shader "))
        for name, value in saved.items():
            if value is None:
                host.env.pop(name, None)
            else:
                host.env[name] = value


def lanes_bindless_compute(host, check, gpu_args, pci, memory_symbols):
    """Dispatch the Lanes direct-indexed compute programs with a paged resource heap.

    One stores through a heap UAV; the other also samples a heap texture with a
    literal sampler. A third stores like the first from a root that also has a
    descriptor table. Each runs with a roomy and a constrained resident limit.
    """
    from functools import partial
    from examples.lanes.shader_fixture import (bindless_compute_shader, bindless_sampler_shader,
                                               bindless_table_store_shader, bindless_texture_store_shader)
    from .runtime_shaders import shader_session
    from .shader_prep import SOURCE_CACHE

    sources = Path(host.env["WINEPREFIX"]) / SOURCE_CACHE
    names = ("TUXBOX_LOG", "WINEDEBUG", "TUXBOX_SHADER_SESSION",
             "TUXBOX_DESCRIPTOR_HEAP_LIMIT", "TUXBOX_NULL_PIPELINES")
    saved = {name: host.env.get(name) for name in names}
    try:
        trials = [(scenario, hashlib.sha256(build()).hexdigest(), kind, limit)
                  for scenario, build in (("bindless-compute", bindless_compute_shader),
                                          ("bindless-sampler", bindless_sampler_shader),
                                          ("bindless-heap-read", bindless_compute_shader),
                                          ("bindless-texture-store", bindless_texture_store_shader),
                                          ("bindless-table-store", bindless_table_store_shader),
                                          ("bindless-table-compute", partial(bindless_compute_shader, table=True)))
                  for kind, limit in (("wide", "1000000"), ("constrained", "64"))]
        for scenario, original, kind, limit in trials:
            messages = []
            host.env.update(TUXBOX_LOG="1", WINEDEBUG="-all,+debugstr",
                            TUXBOX_DESCRIPTOR_HEAP_LIMIT=limit, TUXBOX_NULL_PIPELINES="0")
            host.env.pop("TUXBOX_SHADER_SESSION", None)
            def keys():
                return {m[1] for line in messages if (m := re.match(r"Compiled Xbox shader ([0-9a-f]{64})", line))}
            def clean(entries):
                for key in entries:
                    for path in [*sources.glob(f"{key}.*"), *sources.parent.glob(f"runtime/*/{key}.*")]:
                        path.unlink()
            def receipt(output):
                compiled = keys()
                loaded = set(re.findall(r"prepared compute shader loaded, source SHA-256 ([0-9a-f]{64})", output))
                if original in compiled or not compiled or loaded != compiled:
                    return "expected fresh root-aware worker translation matching loaded digest"
                # Scarlett device11_queue always uses an arena even when its
                # descriptor count fits. Both trials must bind the paged map.
                if "bound paged resource descriptor index map" not in output:
                    return "missing paged resource map bind"
                if "substituted null " in output:
                    return "bindless compute used a stand-in"
                return None
            try:
                clean({original})
                with shader_session(sources.parent, host.env, messages.append):
                    check(f"lanes-{scenario}-{kind}", "lanes/lanes.exe",
                          ["--selftest", scenario, *gpu_args, "--report"],
                          rf"lanes-selftest PASS pci={pci} scenario={scenario} passed=1",
                          providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                                     "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                          config="lanes/MicrosoftGame.config", device_factory="xbox", log_check=receipt)
            finally:
                clean({original} | keys())
    finally:
        for name, value in saved.items():
            if value is None:
                host.env.pop(name, None)
            else:
                host.env[name] = value


def lanes_large_dispatch(host, check, gpu_args, pci, memory_symbols):
    """Dispatch the Lanes group ID program with more than 65,535 groups in a dimension.

    The provider splits each dispatch and translates the program again, while
    the game runs, against its root with the group base appended.
    """
    from examples.lanes.shader_fixture import group_shader
    from .runtime_shaders import shader_session
    from .shader_prep import SOURCE_CACHE

    sources = Path(host.env["WINEPREFIX"]) / SOURCE_CACHE
    original = hashlib.sha256(group_shader()).hexdigest()
    messages = []

    def compiled():
        return {m[1] for line in messages if (m := re.match(r"Compiled Xbox shader ([0-9a-f]{64})", line))}

    def forget(keys):
        # Only this program's entries; a prefix may hold a title cache.
        for key in keys:
            for path in [*sources.glob(f"{key}.*"), *sources.parent.glob(f"runtime/*/{key}.*")]:
                path.unlink()

    def receipt(output):
        if not compiled():
            return f"the worker translated nothing: {messages}"
        if "substituted null " in output:
            return "the program received a stand-in"
        if "created the group base variant of CS " not in output:
            return "the provider made no group base variant"
        if "split compute dispatch" not in output or "into 2 dispatches" not in output:
            return "the provider did not split the dispatch in two"
        return None

    names = ("TUXBOX_LOG", "WINEDEBUG", "TUXBOX_SHADER_SESSION", "TUXBOX_NULL_PIPELINES")
    saved = {name: host.env.get(name) for name in names}
    host.env.update(TUXBOX_LOG="1", WINEDEBUG="-all,+debugstr", TUXBOX_NULL_PIPELINES="0")
    host.env.pop("TUXBOX_SHADER_SESSION", None)
    try:
        forget({original})
        with shader_session(sources.parent, host.env, messages.append):
            check("lanes-large-dispatch", "lanes/lanes.exe", ["--selftest", "large-dispatch", *gpu_args, "--report"],
                  rf"lanes-selftest PASS pci={pci} scenario=large-dispatch passed=1",
                  providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                             "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                  config="lanes/MicrosoftGame.config", device_factory="xbox", log_check=receipt)
    finally:
        forget({original} | compiled())
        for name, value in saved.items():
            if value is None:
                host.env.pop(name, None)
            else:
                host.env[name] = value


def lanes_late_pipeline(host, check, gpu_args, pci, memory_symbols):
    """Run a program whose driver compile misses a 10 ms pipeline deadline.

    The provider gives the title a stand-in, and the compile carries on. The
    scenario dispatches until the finished compile replaces the stand-in.
    """
    from examples.lanes.shader_fixture import late_shader
    from .runtime_shaders import shader_session
    from .shader_prep import SOURCE_CACHE

    sources = Path(host.env["WINEPREFIX"]) / SOURCE_CACHE
    original = hashlib.sha256(late_shader()[0]).hexdigest()
    messages = []

    def compiled():
        return {m[1] for line in messages if (m := re.match(r"Compiled Xbox shader ([0-9a-f]{64})", line))}

    def forget(keys):
        # Only this program's entries; a prefix may hold a title cache.
        for key in keys:
            for path in [*sources.glob(f"{key}.*"), *sources.parent.glob(f"runtime/*/{key}.*")]:
                path.unlink()

    def receipt(output):
        if not compiled():
            return f"the worker translated nothing: {messages}"
        if "return a null pipeline while the driver compile continues" not in output:
            return "the provider did not use the short deadline"
        if "a finished compile replaced its stand-in compute pipeline" not in output:
            return "no finished compile replaced its stand-in"
        return None

    # With a cached pipeline the compile would beat the deadline: keep vkd3d-proton's
    # and the drivers' disk caches out of this gate.
    caches = dict(VKD3D_SHADER_CACHE_PATH="0", __GL_SHADER_DISK_CACHE="0", MESA_SHADER_CACHE_DISABLE="true")
    names = ("TUXBOX_LOG", "WINEDEBUG", "TUXBOX_SHADER_SESSION", "TUXBOX_NULL_PIPELINES", "TUXBOX_PIPELINE_DEADLINE",
             *caches)
    saved = {name: host.env.get(name) for name in names}
    host.env.update(TUXBOX_LOG="1", WINEDEBUG="-all,+debugstr", TUXBOX_PIPELINE_DEADLINE="0.01", **caches)
    host.env.pop("TUXBOX_SHADER_SESSION", None)
    host.env.pop("TUXBOX_NULL_PIPELINES", None)
    try:
        forget({original})
        with shader_session(sources.parent, host.env, messages.append):
            check("lanes-late-pipeline", "lanes/lanes.exe", ["--selftest", "late-pipeline", *gpu_args, "--report"],
                  rf"lanes-selftest PASS pci={pci} scenario=late-pipeline passed=1",
                  providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                             "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                  config="lanes/MicrosoftGame.config", device_factory="xbox", log_check=receipt,
                  timeout=240)  # the translation, then up to two minutes for the deferred compile
    finally:
        forget({original} | compiled())
        for name, value in saved.items():
            if value is None:
                host.env.pop(name, None)
            else:
                host.env[name] = value


def lanes_compute_root_recovery(host, check, gpu_args, pci, memory_symbols):
    """Run the partially reflected second-table shader with paged descriptors."""
    from examples.lanes.shader_fixture import recovered_root_shader
    lanes_reflected_compute_root(host, check, gpu_args, pci, memory_symbols, "compute-root-recovery",
                                 recovered_root_shader(), slots=4, heap_limit="64")


def lanes_compute_root_srv(host, check, gpu_args, pci, memory_symbols):
    """Run the program under three tables and a root SRV that stores where the root declares an SRV."""
    from examples.lanes.shader_fixture import root_srv_shader
    lanes_reflected_compute_root(host, check, gpu_args, pci, memory_symbols, "compute-root-srv",
                                 root_srv_shader(), slots=3)


def lanes_compute_heap_stride(host, check, gpu_args, pci, memory_symbols):
    """Dispatch the root-less console compute program that reads a structured buffer by thread number.

    The load is an IDXEN raw access whose record stride the console stores in the
    descriptor's word 1; the program carries no stride. The provider must retry the
    translation against a root with the heap stride table appended and bind that
    table, with a paged resource heap. The program carries no embedded root, and
    no translation against the title's root (which does not carry the table).
    """
    from examples.lanes.shader_fixture import heap_stride_compute_shader
    from .runtime_shaders import shader_session
    from .shader_prep import SOURCE_CACHE

    sources = Path(host.env["WINEPREFIX"]) / SOURCE_CACHE
    key = hashlib.sha256(heap_stride_compute_shader()).hexdigest()
    messages = []

    def compiled():
        return {line.removeprefix("Compiled Xbox shader ") for line in messages
                if line.startswith("Compiled Xbox shader ")}

    def refused():
        return {match[1] for line in messages if (match := re.match(r"Xbox shader ([0-9a-f]{64}):", line))}

    def forget(keys):
        # Only this program's entries; a prefix may hold a title cache.
        for k in keys:
            for path in [*sources.glob(f"{k}.*"), *sources.parent.glob(f"runtime/*/{k}.*")]:
                path.unlink()

    def expect_strides(output):
        if "substituted null compute pipeline" in output:
            return f"the provider substituted a stand-in pipeline: {messages}"
        if not compiled():
            return f"the worker translated nothing: {messages}"
        if key in compiled() or key in refused():
            return f"the worker translated a program without its root: {messages}"
        if "bound paged resource descriptor stride table at compute root slot" not in output:
            return "the dispatch did not bind the paged stride table"
        return None

    names = ("TUXBOX_LOG", "WINEDEBUG", "TUXBOX_SHADER_SESSION", "TUXBOX_NULL_PIPELINES",
             "TUXBOX_DESCRIPTOR_HEAP_LIMIT")
    saved = {name: host.env.get(name) for name in names}
    host.env.update(TUXBOX_LOG="1", WINEDEBUG="-all,+debugstr",
                    TUXBOX_NULL_PIPELINES="0", TUXBOX_DESCRIPTOR_HEAP_LIMIT="64")
    host.env.pop("TUXBOX_SHADER_SESSION", None)
    try:
        forget({key})
        with shader_session(sources.parent, host.env, messages.append):
            check("lanes-compute-heap-stride", "lanes/lanes.exe",
                  ["--selftest", "compute-heap-stride", *gpu_args, "--report"],
                  rf"lanes-selftest PASS pci={pci} scenario=compute-heap-stride passed=1",
                  providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                             "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                  config="lanes/MicrosoftGame.config", device_factory="xbox",
                  log_check=expect_strides)
    finally:
        forget({key} | compiled() | refused())
        for name, value in saved.items():
            if value is None:
                host.env.pop(name, None)
            else:
                host.env[name] = value


def lanes_reflected_compute_root(host, check, gpu_args, pci, memory_symbols, scenario, program, *, slots,
                                 heap_limit=None):
    """Run a scenario whose program must be translated against a root the provider reflects."""
    from .runtime_shaders import shader_session
    from .shader_prep import SOURCE_CACHE

    sources = Path(host.env["WINEPREFIX"]) / SOURCE_CACHE
    rootless = {hashlib.sha256(program).hexdigest()}
    messages = []

    def requested(prefix):
        return {match[1] for line in messages if (match := re.match(prefix + r"([0-9a-f]{64})\b", line))}

    def forget(keys):
        # Only these programs' entries; a prefix may hold a title cache.
        for key in keys:
            for path in [*sources.glob(f"{key}.*"), *sources.parent.glob(f"runtime/*/{key}.*")]:
                path.unlink()

    def expect_root_translations(output):
        compiled, refused = requested("Compiled Xbox shader "), requested("Xbox shader ")
        if (compiled | refused) & rootless:
            return f"the worker translated a program without its root: {messages}"
        if not compiled:
            return f"the worker translated no program against the two-table root: {messages}"
        if "substituted null compute pipeline" in output:
            return "the program received a stand-in"
        if f"created compute pipeline with {slots} reflected FSSM descriptor slots" not in output:
            return f"the program's pipeline did not reflect {slots} descriptor slots"
        return None

    names = ("TUXBOX_LOG", "WINEDEBUG", "TUXBOX_SHADER_SESSION", "TUXBOX_DESCRIPTOR_HEAP_LIMIT", "TUXBOX_NULL_PIPELINES")
    saved = {name: host.env.get(name) for name in names}
    host.env.update(TUXBOX_LOG="1", WINEDEBUG="-all,+debugstr", TUXBOX_NULL_PIPELINES="0")
    if heap_limit:
        host.env["TUXBOX_DESCRIPTOR_HEAP_LIMIT"] = heap_limit
    host.env.pop("TUXBOX_SHADER_SESSION", None)
    try:
        forget(rootless)
        with shader_session(sources.parent, host.env, messages.append):
            check(f"lanes-{scenario}", "lanes/lanes.exe", ["--selftest", scenario, *gpu_args, "--report"],
                  rf"lanes-selftest PASS pci={pci} scenario={scenario} passed=1",
                  providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                             "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                  config="lanes/MicrosoftGame.config", device_factory="xbox", log_check=expect_root_translations)
    finally:
        forget(rootless | requested("Compiled Xbox shader ") | requested("Xbox shader "))
        for name, value in saved.items():
            if value is None:
                host.env.pop(name, None)
            else:
                host.env[name] = value


# Table slots the reflected `raster-samplers` root holds (see `lanes_raster_tables`).
RASTER_SAMPLER_SLOTS = 1
# Table slots the `mesh-samplers` mesh root holds: the pixel program's texture.
MESH_SAMPLER_SLOTS = 1


def missing_mesh_worker_receipt(output, compiled):
    """Require the exact mesh/pixel binaries loaded by the provider to be worker results."""
    for stage in ("mesh", "pixel"):
        loaded = set(re.findall(
            rf"prepared {stage} shader loaded, source SHA-256 ([0-9a-f]{{64}})(?![0-9a-f])",
            output))
        if not loaded or not loaded.issubset(compiled):
            return f"missing worker receipt for loaded {stage}: {loaded}"
    return None


def lanes_raster_tables(host, check, gpu_args, pci, memory_symbols):
    """Draw with the Lanes console raster programs under a root with two tables.

    `raster-tables`: the pixel program reads a register the title's root does
    not declare, so it draws only under the root the provider reflects from both
    programs. `raster-samplers`: the pixel program builds a linear sampler from
    literal words that the root's static sampler at its register does not have,
    so it samples correctly only with the private static sampler the provider
    adds to the reflected root. That root is serialized while the game runs, so
    nothing is prepared before launch: the host worker translates both programs
    against it. `mesh-samplers`: the same pixel program, declaring two samplers
    the root has in another state, in a mesh pipeline. Both stages are
    translated by the worker against the reflected root; the mesh program
    exports guarded FP16-to-FP32 positions. No desktop shader is prepared.
    `mesh-sampler-single`: the one-sampler pixel program in that mesh pipeline. `mesh-sampler-rootless`:
    two pixel programs in mesh pipelines under a root with no static samplers,
    whose literal samplers the provider must add to the mesh root whole.
    `raster-heap-stride`: the vertex program reads a heap buffer by index under
    a root with no parameters, so the provider must translate it against a root
    with its stride table and bind that table, with a paged resource heap.
    `raster-sampler-waterfall`: a vertex program that reads a sampler from the
    sampler heap back through a waterfall loop, after an if/else copied it into
    vector registers; the sampled value is its position's w.
    """
    from examples.lanes.raster_fixture import (heap_stride_pixel, heap_stride_vertex, mesh, mesh_sampler_pixel, pixel,
                                               rootless_sampler_pixel, sampler_pixel, sampler_waterfall_vertex, vertex)
    from examples.lanes.spill_fixture import pixel as spill_pixel
    from .runtime_shaders import shader_session
    from .shader_prep import SOURCE_CACHE

    sources = Path(host.env["WINEPREFIX"]) / SOURCE_CACHE
    rootless = {hashlib.sha256(data).hexdigest()
                for data in (vertex(), pixel(), sampler_pixel(), mesh_sampler_pixel(), rootless_sampler_pixel(),
                             spill_pixel(), heap_stride_vertex(), heap_stride_pixel(), sampler_waterfall_vertex())}
    mesh_key = hashlib.sha256(mesh()).hexdigest()
    messages = []

    def compiled():
        return {line.removeprefix("Compiled Xbox shader ") for line in messages
                if line.startswith("Compiled Xbox shader ")}

    def refused():
        return {match[1] for line in messages if (match := re.match(r"Xbox shader ([0-9a-f]{64}):", line))}

    def forget(keys):
        # Only these programs' entries; a prefix may hold a title cache.
        for key in keys:
            for path in [*sources.glob(f"{key}.*"), *sources.parent.glob(f"runtime/*/{key}.*")]:
                path.unlink()

    def expect_reflected(slots, mesh_pipeline=False):
        def problem(output):
            if "substituted null " in output:
                return "the provider substituted a stand-in pipeline"
            if mesh_pipeline:
                if "created Xbox mesh pipeline" not in output:
                    return "the provider did not create the mesh pipeline"
            # +debugstr escapes the quotes of the logged map.
            elif not re.search(r'multi-table raster roots after 1 pipelines: \{\\?"reflected\\?": 1\}', output):
                return "the provider did not reflect the two-table root"
            if f"created raster pipeline with {slots} reflected FSSM descriptor slots" not in output:
                return "the provider did not create the pipeline under the reflected root"
            if compiled() & (rootless | {mesh_key}):
                return f"the worker translated a program without its root: {messages}"
            if mesh_pipeline:
                # A draw alone could pass with a prebuilt desktop mesh shader.
                # Require the exact loaded binaries to have been requested from
                # the Xbox shader worker during this scenario.
                problem = missing_mesh_worker_receipt(output, compiled())
                if problem:
                    return f"{problem}; {messages}"
            return None
        return problem

    def expect_translated(output):
        # An empty root needs no reflection: only the translation is in question.
        if "substituted null " in output:
            return f"the provider substituted a stand-in pipeline: {messages}"
        if not compiled():
            return f"the worker translated nothing: {messages}"
        return None

    def expect_strides(output):
        problem = expect_translated(output)
        if problem:
            return problem
        if "bound paged resource descriptor stride table at graphics root slot" not in output:
            return "the draw did not bind the paged stride table"
        if compiled() & rootless:
            return f"the worker translated a program without its root: {messages}"
        return None

    names = ("TUXBOX_LOG", "WINEDEBUG", "TUXBOX_TRACE_RASTER_ROOTS", "TUXBOX_SHADER_SESSION",
             "TUXBOX_RASTER_ROOT_FALLBACK", "TUXBOX_NULL_PIPELINES", "TUXBOX_DESCRIPTOR_HEAP_LIMIT")
    saved = {name: host.env.get(name) for name in names}
    host.env.update(TUXBOX_LOG="1", WINEDEBUG="-all,+debugstr", TUXBOX_TRACE_RASTER_ROOTS="1",
                    TUXBOX_NULL_PIPELINES="0")
    # The fallback translates against the title's root and would hide a missing reflection.
    for name in ("TUXBOX_SHADER_SESSION", "TUXBOX_RASTER_ROOT_FALLBACK"):
        host.env.pop(name, None)
    try:
        for scenario, slots in (("raster-tables", 3), ("lane-spill-interp", None), ("raster-samplers", RASTER_SAMPLER_SLOTS),
                                ("mesh-samplers", MESH_SAMPLER_SLOTS), ("mesh-sampler-single", MESH_SAMPLER_SLOTS),
                                ("mesh-sampler-rootless", MESH_SAMPLER_SLOTS), ("raster-heap-stride", None),
                                ("raster-sampler-waterfall", None)):
            if scenario == "raster-heap-stride":
                # The stride table exists only for a paged heap.
                host.env["TUXBOX_DESCRIPTOR_HEAP_LIMIT"] = "64"
            forget(rootless | {mesh_key})
            messages.clear()
            with shader_session(sources.parent, host.env, messages.append):
                check(f"lanes-{scenario}", "lanes/lanes.exe", ["--selftest", scenario, *gpu_args, "--report"],
                      rf"lanes-selftest PASS pci={pci} scenario={scenario} passed=1",
                      providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                                 "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                      config="lanes/MicrosoftGame.config", device_factory="xbox",
                      log_check=expect_strides if scenario == "raster-heap-stride"
                      else expect_translated if slots is None
                      else expect_reflected(slots, scenario.startswith("mesh-")))
            forget(compiled() | refused())
    finally:
        forget(rootless | compiled() | refused() | {mesh_key})
        for name, value in saved.items():
            if value is None:
                host.env.pop(name, None)
            else:
                host.env[name] = value


def verify(host, output, adapter, selected=None, samples=1):
    if adapter and not re.fullmatch(r"[0-9a-fA-F]{4}:[0-9a-fA-F]{4}", adapter):
        raise ValueError("Adapter must be a PCI vendor:device pair in hexadecimal")
    if not selected:
        selected = (*BASE_SUITES, *(suite for suite, provider in OPTIONAL_PROVIDERS.items()
                                   if (BUILD / provider).is_file()))
    cases = []
    builds = {}
    present_display = None
    skipped = []
    BUILD.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="guest-check-", dir=BUILD) as tmp:
        work = Path(tmp)
        # Gates that neither share a cache nor measure time or the screen run
        # side by side, each worker in a copy of the prefix. Proton only: the
        # other backends were not measured.
        parallel = host.backend == "proton" and check_jobs() > 1
        prefixes = PrefixPool(work, check_jobs())
        waiting, side_by_side = [], [parallel]

        def stage(relative, folder):
            src = BUILD / relative
            if not src.is_file():
                raise ValueError(f"Build the missing fixture first: {src}")
            dest = folder / src.name
            if not dest.exists():
                shutil.copy2(src, dest)
                builds[relative] = digest(dest)
            return dest

        def check(name, binary, arguments, match, expected_code=0, providers=None, config=None, device_factory=None,
                  dependencies=(), log_check=None, timeout=None):
            row = {"gate": name, "passed": False}
            cases.append(row)
            folder = work / str(len(cases))
            folder.mkdir()
            receipt = folder / "result.txt"

            def attempt(runner, timeout):
                """Run the guest on `runner` and return what to print about it."""
                nonlocal match
                shown = []
                try:
                    exe = stage(binary, folder)
                    if providers:
                        pe = image(exe)
                        modules = pe["imports"]
                        if config:
                            row["guest_import_modules"] = lanes_imports(pe)
                        if callable(match):
                            match = match(modules)
                        if device_factory is not None:
                            desktop = next((entry["symbols"] for entry in modules
                                            if entry["module"].lower() == "d3d12.dll"), ())
                            if device_factory == "xbox" and desktop:
                                raise ValueError("Frame fixture imports desktop D3D12 directly")
                            # The Windows SDK import library selects this function by ordinal.
                            if device_factory == "desktop" and not {"D3D12CreateDevice", "#101"}.intersection(desktop):
                                raise ValueError("Expected device creation from desktop d3d12.dll")
                        for dll, symbols in providers.items():
                            provider = stage(dll, folder)
                            missing = missing_provider_imports(modules, provider, symbols)
                            if missing:
                                raise ValueError(f"Missing static guest imports from {provider.name}: {missing}")
                    for dependency in dependencies:
                        stage(dependency, folder)
                    if config:
                        stage(config, folder)
                        exe = launch_target(folder, "Lanes")
                    else:
                        exe = launch_target(exe, None)
                    proc = runner.run(exe, [*arguments, receipt.name], timeout=timeout, capture=True)
                    text = receipt.read_text(encoding="utf-8").rstrip("\r\n") if receipt.is_file() else None
                    row.update(exit_code=proc.returncode, receipt=text)
                    row["passed"] = (proc.returncode == expected_code and
                                     (text is None if match is None else text is not None and re.fullmatch(match, text) is not None))
                    if row["passed"] and log_check:
                        problem = log_check(proc.stdout.decode(errors="replace"))
                        if problem:
                            row.update(passed=False, error=problem)
                    if not row["passed"]:
                        shown.append(proc.stdout[-2000:].decode(errors="replace"))
                except (OSError, ValueError, subprocess.TimeoutExpired) as error:
                    row["error"] = str(error)
                shown.append(f"{'PASS' if row['passed'] else 'FAIL'}  {name}")
                return "\n".join(shown)

            if side_by_side[0]:
                # The settings are those in force now; the guest runs later.
                snapshot = copy.copy(host)
                snapshot.env = dict(host.env)
                # Guests starting together take longer to come up.
                waiting.append((snapshot, lambda runner: attempt(runner, timeout or 30)))
            else:
                print(attempt(host, timeout or 15), flush=True)
            return row

        def run_together(enable=False):
            """Run the gates waiting to run side by side; `enable` collects more."""
            if waiting:
                for shown in prefixes.run(waiting):
                    print(shown, flush=True)
                waiting.clear()
            side_by_side[0] = enable and parallel

        def unsampled(name, reason):
            row = {"gate": name, "passed": False, "error": f"screen capture unavailable: {reason}"}
            cases.append(row)
            print(f"FAIL  {name}  ({row['error']})", flush=True)
            return row

        if "cpu" in selected:
            for route, state in [("RRRRDD", "coin_mask=1 ticks=6 bumps=0"), ("LDDRRRR", "coin_mask=2 ticks=7 bumps=1")]:
                check(f"route-{route}", "route-game.exe", [route], f"route-game x=5 y=3 {state} exit=1")
            check("invalid-route", "route-game.exe", ["X"], None, 2)
        gpu_args = ["--adapter", adapter] if adapter else []
        pci = adapter.lower() if adapter else r"[0-9a-f]{4}:[0-9a-f]{4}"
        memory_symbols = ("XMemAllocDefault", "XMemFreeDefault")
        graphics_symbols = ("D3D12SerializeRootSignature",)
        if "dxc-utils" in selected:
            binary = "dxc-utils/dxc-utils.exe"
            provider = "dxc-utils/dxcompiler_xs.dll"
            dependencies = ("dxc-utils/dxcompiler.dll",)
            check("dxc-create-blob", binary, ["--report"],
                  r"dxc-utils PASS bytes=257 source-freed=1 writable=257 retained=1",
                  providers={provider: ("DxcCreateInstance",)}, dependencies=dependencies)
            check("dxc-allocator", binary, ["--allocator", "--report"],
                  "dxc-allocator PASS ordinals=3,4 custom=1 retained=1 balanced=1",
                  providers={provider: ("DxcCreateInstance2",)}, dependencies=dependencies)
            check("dxc-compile", binary, ["--compile", "--report"],
                  r"dxc-compiler PASS profile=cs_6_0 dxil=1 invalid-entry=1 diagnostics=1",
                  providers={provider: ("DxcCreateInstance",)}, dependencies=dependencies)
            check("xdxr-link", "dxc-utils/xdxr-link.exe", ["--report"],
                  r"xdxr-link PASS factories=2 flat=1 grouped=1 retained-shader-body=1 shader-tables=single,multiple",
                  providers={"dxc-utils/dxcompiler_xs.dll": ("CreateXdxrCompiler", "CreateXdxrCompiler2", "DxcCreateInstance")},
                  dependencies=("dxc-utils/dxcompiler.dll",))
        xg_addresses = r"xg-texture PASS layouts=18 traced-descriptors=8 element-addresses=872759"
        if "d3d12-signatures" in selected:
            check("d3d12-signatures", "d3d12-smoke/d3d12-signatures.exe", ["--report"],
                  r"d3d12-signatures PASS versions=1.0,1.1 ranges=1 invalid=2",
                  providers={"d3d12-smoke/d3d12_xs.dll": ("D3D12SerializeVersionedRootSignature",
                             "D3D12CreateRootSignatureDeserializer", "D3D12CreateVersionedRootSignatureDeserializer")})
        if "d3d12-guids" in selected:
            check("d3d12-guids", "d3d12-smoke/d3d12-guids.exe", ["--report"],
                  lambda modules: guid_receipt(modules, BUILD / "d3d12-smoke/d3d12_xs.dll"),
                  providers={"d3d12-smoke/d3d12_xs.dll": ("IID_ID3D12Device",)})
        if "xg-provider" in selected:
            check("xg-dcc", "xg-provider/xg-dcc.exe", ["--report"],
                  r"xg-dcc PASS xbox-blocks=2 encode-bytes=32 atomic-batch=1",
                  providers={"xg-provider/xg_xs.dll": ("XGEncodeDCC", "XGDecodeDCC")})
            check("xg-library", "xg-provider/xg-library.exe", ["--report"],
                  r"xg-library PASS depth=516 descriptors=375 lock=recursive\+factory copies=bidirectional depth_surfaces=16 typed_images=880 map_targets=1474 msaa_depth=384",
                  providers={"xg-provider/xg_xs.dll": ("XGCreateTextureComputerFromDescriptor",
                             "XGInitializeLibrary", "XGTerminateLibrary", "XGSetHardwareVersion",
                             "XGEnableExclusiveLocking", "XGEnterExclusiveLock", "XGLeaveExclusiveLock",
                             "XGComputeOptimalDepthStencilSwizzleMode")})
        if "xg" in selected:
            check("xg-native-layout", "xg/xg-texture.exe", ["--report"],
                  r"xg-texture PASS descriptors=4 layout-queries=8 plane-records=4 releases=4",
                  providers={"xg/xg_xs.dll": ("XGCreateTextureComputer",)},
                  dependencies=("xmem-smoke/xmem.dll",))
        for suite, addresses_gate, formats_gate, deps in (
            ("xg", "xg-native-addresses", "xg-native-formats", ("xmem-smoke/xmem.dll",)),
            ("xg-provider", "xg-addresses", "xg-formats", ()),
        ):
            if suite in selected:
                provider = {f"{suite}/xg_xs.dll": ("XGCreateTextureComputer",)}
                check(addresses_gate, f"{suite}/xg-texture.exe", ["--addresses", "--report"], xg_addresses,
                      providers=provider, dependencies=deps)
                check(formats_gate, f"{suite}/xg-texture.exe", ["--formats", "--report"],
                      r"xg-formats PASS formats=78 layouts=156 element-addresses=9231018",
                      providers=provider, dependencies=deps)
        if "xgameruntime" in selected:
            check("xgameruntime", "xgameruntime/runtime-control.exe", [],
                  r"xgameruntime PASS init=balanced interfaces=checked manual=ordered termination=canceled automatic=3 stale=rejected",
                  providers={"xgameruntime/xgameruntime.dll": ("InitializeApiImpl", "InitializeApiImplEx",
                                                               "UninitializeApiImpl", "QueryApiImpl")})
        if "xapu" in selected:
            check("xapu", "xapu/xapu-control.exe", [],
                  r"xapu PASS Opus=960 pre-skip=312 output=648 queue=ordered",
                  providers={"xapu/xapu.dll": ("XApuConnect", "XApuEnqueueCommand", "XApuDequeueResult")})
        if "xcrdapi" in selected:
            check("xcrdapi", "xcrdapi/xcrdapi-read.exe", [],
                  r"raw=16 zlib=16 guards=2 errors=2 cancellation=2 bc1=4 bc7=2 pages=2 autosubmit=64 PASS",
                  providers={"xcrdapi/xcrdapi.dll": ("DStorageGetFactory",)},
                  dependencies=("xmem-smoke/xmem.dll",))
        if "xmem" in selected:
            check("xmem-heaps", "xmem-smoke/xmem-smoke.exe", ["--heaps", "--report"],
                  r"xmem-heaps PASS title=1 private=4 invalid=2 pool-commits=2 pool-decommits=2 heap-callbacks=2 stats=1 dump=1 history=2 page-classes=3",
                  providers={"xmem-smoke/xmem.dll": ("XMemGetTitleHeap", "XMemCreateWin32Heap", "XMemAllocDefault", "XMemFreeDefault")})
            check("xmem-heap-profiles", "xmem-smoke/xmem-smoke.exe", ["--heap-profiles", "--report"],
                  r"xmem-heap-profiles PASS attributes=40 alloc=80 free=80 null-free=2",
                  providers={"xmem-smoke/xmem.dll": ("XMemAlloc", "XMemFree", "XMemAllocDefault", "XMemFreeDefault")})
            check("xmem-virtual-cpu", "xmem-smoke/xmem-smoke.exe", ["--virtual-cpu", "--report"],
                  r"xmem-virtual-cpu PASS protections=6 allocations=12 guards=5 invalid=1",
                  providers={"xmem-smoke/xmem.dll": ("XMemVirtualAlloc",)})
            check("xmem-scatter", "xmem-smoke/xmem-smoke.exe", ["--scatter", "--report"],
                  r"xmem-scatter PASS pages=3 remapped-bytes=196608 duplicates=2 guards=2 rollback=1",
                  providers={"xmem-smoke/xmem.dll": ("XMemMapPhysicalPagesScatter", "XMemAllocatePhysicalPages", "XMemFreePhysicalPages")})
            check("xmem-watch", "xmem-smoke/xmem-smoke.exe", ["--watch", "--report"],
                  r"xmem-watch PASS initial=2 reset=2 next=1 guards=2 invalid=1 pause=2 resume=3 allocations=2 direct-tags=2 retail-apertures=2",
                  providers={"xmem-smoke/xmem.dll": ("XMemVirtualAlloc", "XMemGetWriteWatch", "XMemResumeWriteWatch", "XMemSetOption")})
            check("xmem-hooks", "xmem-smoke/xmem-smoke.exe", ["--hooks", "--report"],
                  r"xmem-hooks PASS calls=2/2 zero-size=1 null-free=1 restore=1 reentrant=1",
                  providers={"xmem-smoke/xmem.dll": ("XMemAlloc", "XMemFree", "XMemSetAllocationHooks")})
            check("xmem-tags", "xmem-smoke/xmem-smoke.exe", ["--tags", "--report"],
                  r"xmem-tags PASS count=5120 hash=d4bf30c57509b176",
                  providers={"xmem-smoke/xmem.dll": ("XMemMakeTag",)})
            check("xmem-http", "xmem-smoke/xmem-http.exe", ["--report"],
                  r"xmem-http PASS callbacks=7 host-session=1",
                  providers={"xmem-smoke/xmem.dll": ("XMemSetWinHttpHooks",),
                             "xmem-smoke/tuxbox_xhttp.dll": (
                                 "WinHttpOpen", "WinHttpConnect", "WinHttpOpenRequest",
                                 "WinHttpWebSocketCompleteUpgrade", "WinHttpCloseHandle",
                                 "WinHttpReceiveResponse", "WinHttpSetOption")})
            check("xmem-query", "xmem-smoke/xmem-smoke.exe", ["--query", "--report"],
                  r"xmem-query PASS cpu=5 released=1 unsupported=3 guards=2 null=1",
                  providers={"xmem-smoke/xmem.dll": ("XMemVirtualQuery",)})
            check("xmem-default", "xmem-smoke/xmem-smoke.exe", ["--report"],
                  re.escape("xmem-default: PASS (allocator pairs 1/6 and 2/7, ordinary and 64 KiB allocations, "
                            "host working set v2, virtual reserve/commit)"),
                  providers={"xmem-smoke/xmem.dll": (*memory_symbols, "XMemAlloc", "XMemFree",
                                                     "XMemGetWorkingSetStatistics", "XMemVirtualAlloc")})
            check("xmem-partitions", "xmem-smoke/xmem-partitions.exe", ["--report"],
                  r"xmem-partitions PASS stable_cpu_pfns=2 budget_reduced=1 ids=2 native_handles=[0-2]",
                  providers={"xmem-smoke/xmem.dll": ("XMemGetPartitionHandleAndId",
                             "XMemGetPhysicalMemoryLayout", "XMemTransferMemory")})
            check("xmem-pages", "xmem-smoke/xmem-smoke.exe", ["--pages", "--report"],
                  r"xmem-pages PASS cpu-pages=2 remapped-bytes=131072 stale-and-foreign-rejected=1 shared-aliases=3 scatter-aliases=2 rollback=1 free-unmaps=2",
                  providers={"xmem-smoke/xmem.dll": ("XMemAllocatePhysicalPages", "XMemMapPhysicalPages",
                                                     "XMemFreePhysicalPages")})
        if "graphics" in selected:
            check("desktop-clear", "graphics/desktop-clear.exe", [*gpu_args, "--report"],
                  rf"desktop-clear PASS adapter=.+ pci={pci} pixels=481/481 row_bytes=148 row_pitch=[1-9][0-9]*")
        if "graphics-abi" in selected:
            check("root-signature", "d3d12-smoke/d3d12-root-smoke.exe", [*gpu_args, "--report"],
                  rf"d3d12-root-smoke PASS import=D3D12SerializeRootSignature pci={pci} "
                  r"constant=b5/space2 uav=u3/space4 dispatches=2 words=64/64 guards=16",
                  providers={"d3d12-smoke/d3d12_xs.dll": (*graphics_symbols, "D3D12CreateRootSignatureDeserializer")},
                  device_factory="desktop")
        frame_match = (rf"xbox-frame PASS pci={pci} submissions=2 snapshots=2 pixels=962/962 "
                       r"inset=208 guards=3 fence=2 compute_fence=7")
        frame_provider = {"d3d12-smoke/d3d12_xs.dll": ("D3D12XboxCreateDevice",)}
        if "d3d12-exports" in selected:
            dll = "d3d12-smoke/d3d12_xs.dll"

            def exports_receipt(_imports):
                slots, _ = export_table(BUILD / dll)
                cpu_clear = any("D3D12ZeroMemoryX" in names for names in slots.values())
                return "d3d12-exports PASS debug=1 blob=1 dxgix=4" + (" cpu_clear=1" if cpu_clear else "")

            check("d3d12-exports", "d3d12-smoke/d3d12-exports.exe", ["--report"],
                  exports_receipt, providers={dll: ("D3D10CreateBlob", "D3D12XboxGetProcessDebugFlags",
                                                   "D3D12XboxSetProcessDebugFlags")})
        for suite, expected in (
            ("features", r"queries=7 binding=[1-3] heap=[12] root=1 dxr=0 allocations=4 footprints=4"),
            ("heaps", r"heap=32MiB offsets=0,65536 words=256/256 invalid=4 display=3840x2160/RGB10A2 pixel=c00ffc00 volume=45/45 priorities=4/4 invalid_priority_calls=4"),
            ("placement", r"slot=44 VA-offsets=0,4096,0 passes=2 BC1-blocks=522 cube-blocks=16404 R8-texels=262144 " +
             ("rejected=4 configured_vm=1" if host.env.get("TUXBOX_TEST_CONFIGURED_VM") else "rejected=3 configured_vm=0")),
            ("copy-resource", r"slot=84 buffer_bytes=1028 texture_subresources=8 nonzero_option=E_NOTIMPL "
                              r"self_copy=E_INVALIDARG source_and_destination=verified placed_uploads=2 placed_dst=verified "
                              r"same_va_alias=verified cpu_writeback=verified partial_copy_texels=2 overlap_mips=4 pending_reader=verified"),
            ("draw", r"red=36 blue=220"),
            ("mesh", r"tier=1 stream=680 direct=6 amplified=12 indirect=2 zero=0 pixels=256/256 guards=32 rejected=5"),
            ("frame-events", r"slots=78,79,80,81 tokens=2 timeout=S_FALSE flip=1 detached=1 stats=2"),
            ("tables", r"words=16/16 guards=4 heaps=2 rejected=3 placed-srv=16/16 range-copies=4 descriptor-mode=1"),
            ("queues", r"queues=3 bytes=64 wait=1 signal-x=2"),
            ("queries", r"timestamps=2 occlusion=0 guards=2 heaps=6 rejected=3"),
            ("indirect", r"sdk=0x20004 device=41 list=59 words=64/64 counts=direct,one,zero flush=global,range,wake-up\(0x3000:0\+0xa30000\),zero-length,global-range budget=default,rejected2 mesh=0x[0-9a-f]{8} rays=0x[0-9a-f]{8}"),
        ):
            name = f"d3d12-{suite}"
            if name in selected:
                check(name, f"d3d12-smoke/{name}.exe", [*gpu_args, "--report"],
                      rf"{name} PASS pci={pci} {expected}", providers=frame_provider, device_factory="xbox")
        if "d3d12-stream" in selected:
            check("d3d12-stream", "d3d12-smoke/d3d12-draw.exe", ["--stream", *gpu_args, "--report"],
                  rf"d3d12-stream PASS pci={pci} slot=86 bytes=712 indices=16,32 half-red-blue=18 magenta=18 blue=220 rejected=4",
                  providers=frame_provider, device_factory="xbox")
        if "guest-frame" in selected:
            check("guest-factories", "d3d12-smoke/d3d12-factory.exe", [*gpu_args, "--report"],
                  rf"xbox-factory PASS pci={pci} adapter={'selected' if adapter else 'default'} "
                  r"profile=0x20004 queues=2 fences=2 flag-masks=4 dxgi=2",
                  providers={"d3d12-smoke/d3d12_xs.dll": ("D3D12XboxCreateDevice", "D3D12CreateDevice",
                             "D3D12XboxSetProcessDebugFlags", "D3D12XboxGetProcessDebugFlags")}, device_factory="xbox")
            check("guest-command-list", "d3d12-smoke/xbox-frame.exe", [*gpu_args, "--report"],
                  frame_match, providers=frame_provider, device_factory="xbox")
            check("guest-frame-rgb10", "d3d12-smoke/xbox-frame-rgb10.exe", [*gpu_args, "--report"],
                  frame_match + r" format=rgb10a2", providers=frame_provider, device_factory="xbox")
            check("guest-command-errors", "d3d12-smoke/xbox-frame-negative.exe", [*gpu_args, "--report"],
                  rf"xbox-frame-negative PASS pci={pci} foreign-parents=2 copy-errors=2 recovered=1 rejected-batches=3 compute-present-rejected=1",
                  providers=frame_provider, device_factory="xbox")
        run_together()
        if "present" in selected:
            # Sample a private rootful Xwayland, where nothing on the desktop can cover
            # the frames. `TUXBOX_CHECK_DISPLAY=desktop` uses the current display, which
            # is also the fallback when no private server can be sampled.
            desktop = os.environ.get("TUXBOX_CHECK_DISPLAY") == "desktop"
            server = nullcontext() if desktop else private_x_server(work, adapter=adapter)
            saved = {key: host.env.get(key) for key in ("DISPLAY", "XAUTHORITY", "MESA_VK_WSI_DEBUG",
                                                      "TUXBOX_OVERLAY")}
            with server as private:
                splash_skip = None
                if private and screen_capture_problem(display=private[0], xauthority=private[1]) is None:
                    capture = None
                    host.env.update(DISPLAY=private[0], XAUTHORITY=private[1], **private[2])
                    present_display = "private rootful Xwayland"
                    splash_skip = splash_skip_reason(private[2])
                    if private[2]:
                        present_display += " without glamor"
                    print(f"present: sampling a {present_display} on {private[0]}", flush=True)
                else:
                    capture = screen_capture_problem()
                try:
                    present = check if capture is None else (lambda name, *_args, **_kwargs: unsampled(name, capture))
                    present("guest-present", "d3d12-smoke/xbox-frame.exe", [*gpu_args, "--present", "--report"],
                            frame_match + r" displayed=2/2", providers=frame_provider, device_factory="xbox")
                    present("guest-present-rgb10", "d3d12-smoke/xbox-frame-rgb10.exe", [*gpu_args, "--present", "--report"],
                            frame_match + r" format=rgb10a2 displayed=2/2", providers=frame_provider, device_factory="xbox")
                    present("guest-present-planes", "d3d12-smoke/xbox-frame-planes.exe", [*gpu_args, "--present", "--report"],
                            frame_match + r" displayed=2/2 planes=2", providers=frame_provider, device_factory="xbox")
                    for mode in ("owned", "oversized", "fit", "splash"):
                        if mode == "splash" and splash_skip:
                            skipped.append({"gate": f"guest-present-4k-{mode}", "reason": splash_skip})
                            print(f"SKIP  guest-present-4k-{mode}: {splash_skip}", flush=True)
                            continue
                        present(f"guest-present-4k-{mode}", "d3d12-smoke/xbox-frame-4k.exe",
                                [mode, *gpu_args, "--present", "--report"],
                                rf"xbox-frame PASS pci={pci} submissions=2 snapshots=2 pixels=16588800/16588800 "
                                r"inset=921600 guards=3 fence=2 compute_fence=7 format=rgb10a2 displayed=2/2 "
                                r"client=\d+x\d+ origin=-?\d+,-?\d+ monitor=\d+x\d+"
                                + (" splash=held,ended" if mode == "splash" else ""),
                                providers=frame_provider, device_factory="xbox",
                                dependencies=("d3d12-smoke/splash/MicrosoftGame.Config", "d3d12-smoke/splash/splash.png")
                                if mode == "splash" else ())
                    # The same screen sampling shows the provider's panel over Lanes.
                    host.env["TUXBOX_OVERLAY"] = "1"
                    present("lanes-overlay", "lanes/lanes.exe", ["--replay", "--panel", *gpu_args, "--report"],
                            rf"lanes-replay PASS pci={pci} course=16/16 win=16/16 safe-miss=1 collision=1 frames=2 "
                            r"input=gameinput panel=1",
                            providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                                       "lanes/d3d12_xs.dll": (*graphics_symbols, "D3D12XboxCreateDevice")},
                            config="lanes/MicrosoftGame.config", device_factory="xbox")
                finally:
                    for key, value in saved.items():
                        if value is None:
                            host.env.pop(key, None)
                        else:
                            host.env[key] = value
        run_together(enable=True)
        if "lanes" in selected:
            check("lanes-xbox-replay", "lanes/lanes.exe", ["--replay", *gpu_args, "--report"],
                  rf"lanes-replay PASS pci={pci} course=16/16 win=16/16 safe-miss=1 collision=1 frames=2 input=gameinput",
                  providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                             "lanes/d3d12_xs.dll": (*graphics_symbols, "D3D12XboxCreateDevice")},
                  config="lanes/MicrosoftGame.config", device_factory="xbox")
        if "lanes-selftest" in selected:
            # The descriptor-reclaim selftest asserts the console's 1,000,000-descriptor
            # limit; force it so the OOM case is deterministic on GPUs that support larger
            # shader-visible heaps (the provider otherwise caps at the GPU's maximum).
            saved_limit = host.env.get("TUXBOX_DESCRIPTOR_HEAP_LIMIT")
            host.env["TUXBOX_DESCRIPTOR_HEAP_LIMIT"] = "1000000"
            for scenario in LANES_SCENARIOS:
                check(f"lanes-selftest-{scenario}", "lanes/lanes.exe",
                      ["--selftest", scenario, *gpu_args, "--report"],
                      rf"lanes-selftest PASS pci={pci} scenario={scenario} passed=1",
                      providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                                 "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                      config="lanes/MicrosoftGame.config", device_factory="xbox",
                      dependencies=("lanes/XboxBvhBuilder.dll",) if scenario.startswith("ray-tracing-") else ())
            # A card with mesh shaders also takes the refusal a card without them gets.
            saved_profile = host.env.get("TUXBOX_HOST_PROFILE")
            host.env["TUXBOX_HOST_PROFILE"] = "mesh=none"
            # The scenario itself requires no mesh shaders and a refused signature.
            check("lanes-selftest-mesh-signature-none", "lanes/lanes.exe",
                  ["--selftest", "mesh-signature-none", *gpu_args, "--report"],
                  rf"lanes-selftest PASS pci={pci} scenario=mesh-signature-none passed=1",
                  providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                             "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                  config="lanes/MicrosoftGame.config", device_factory="xbox")
            if saved_profile is None:
                host.env.pop("TUXBOX_HOST_PROFILE", None)
            else:
                host.env["TUXBOX_HOST_PROFILE"] = saved_profile
            if saved_limit is None:
                host.env.pop("TUXBOX_DESCRIPTOR_HEAP_LIMIT", None)
            else:
                host.env["TUXBOX_DESCRIPTOR_HEAP_LIMIT"] = saved_limit
        run_together()
        if "lanes-benchmark" in selected:
            # Console shaders are only translated on the Proton backend, so the
            # pipeline readings exist there alone.
            shader = LanesShader(host) if host.backend == "proton" else None
            readings = rf"lanes-benchmark PASS pci={pci}(?: [a-z0-9_]+=[0-9]+\.[0-9])+"
            if shader:
                shader.prepare(work)
                readings = (rf"lanes-benchmark PASS pci={pci}(?: [a-z0-9_]+=[0-9]+\.[0-9])*"
                            r" shader_pipeline_us=[0-9]+\.[0-9] dxil_pipeline_us=[0-9]+\.[0-9]")
            try:
                check("lanes-benchmark", "lanes/lanes.exe", ["--benchmark", *gpu_args, "--report"], readings,
                      providers={"lanes/xmem.dll": memory_symbols, "lanes/GameInput.dll": ("#1",),
                                 "lanes/d3d12_xs.dll": ("D3D12XboxCreateDevice",)},
                      config="lanes/MicrosoftGame.config", device_factory="xbox")
            finally:
                if shader:
                    shader.forget()
        if "lanes-shader-cache" in selected and host.backend == "proton":
            lanes_shader_cache(host, check, gpu_args, pci, memory_symbols, work)
            lanes_wave_shader(host, check, gpu_args, pci, memory_symbols)
            lanes_live_wave_shaders(host, check, gpu_args, pci, memory_symbols)
        if "lanes-raster-tables" in selected and host.backend == "proton":
            lanes_raster_tables(host, check, gpu_args, pci, memory_symbols)
        if "lanes-compute-root" in selected and host.backend == "proton":
            lanes_compute_root(host, check, gpu_args, pci, memory_symbols)
        if "lanes-bindless-compute" in selected and host.backend == "proton":
            lanes_bindless_compute(host, check, gpu_args, pci, memory_symbols)
        if "lanes-compute-root-recovery" in selected and host.backend == "proton":
            lanes_compute_root_recovery(host, check, gpu_args, pci, memory_symbols)
        if "lanes-compute-root-srv" in selected and host.backend == "proton":
            lanes_compute_root_srv(host, check, gpu_args, pci, memory_symbols)
        if "lanes-large-dispatch" in selected and host.backend == "proton":
            lanes_large_dispatch(host, check, gpu_args, pci, memory_symbols)
        if "lanes-late-pipeline" in selected and host.backend == "proton":
            lanes_late_pipeline(host, check, gpu_args, pci, memory_symbols)
        if "lanes-compute-heap-stride" in selected and host.backend == "proton":
            lanes_compute_heap_stride(host, check, gpu_args, pci, memory_symbols)
        if "cpu-benchmark" in selected:
            readings = []
            for _ in range(samples):
                sample = check("cpu-replay", "route-game.exe", ["--bench"],
                               r"route-bench routes=65536 moves=8388608 checksum=24316bf89788468d "
                               r"elapsed_ticks=[1-9][0-9]* frequency=[1-9][0-9]*")
                readings.append(sample)
            if samples > 1:
                del cases[-samples:]
                series = {"gate": "cpu-replay", "passed": all(row["passed"] for row in readings), "samples": readings}
                if series["passed"]:
                    seconds = []
                    for run in readings:
                        ticks, hz = (int(s.split("=")[1]) for s in run["receipt"].split()[-2:])
                        seconds.append(ticks / hz)
                    series["median_ms"] = round(statistics.median(seconds) * 1000, 4)
                cases.append(series)

    report = {"schema": 1, "type": "homebrew", "date_utc": datetime.now(timezone.utc).date().isoformat(),
              "host": platform.system(), "os_release": platform.release(), "cpu": cpu_model(),
              "machine": platform.machine(), "backend": host.backend,
              "provider": host.version, "binaries_sha256": builds, "checks": cases}
    if present_display:
        report["present_display"] = present_display
    if skipped:
        report["skipped"] = skipped
    output.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=output.parent, delete=False) as tmp:
            tmp_path = Path(tmp.name)
            json.dump(report, tmp, indent=2)
            tmp.write("\n")
        tmp_path.replace(output)
    finally:
        if tmp_path is not None and tmp_path.exists():
            try:
                tmp_path.unlink()
            except OSError:
                pass
    return int(any(not row["passed"] for row in cases))


GATE_LABELS = {
    "xapu": "Opus PCM, decoder pre-skip and command completion",
    "xgameruntime": "Game Runtime initialization, task delivery and cancellation",
    "xcrdapi": "Xbox storage file reads, zlib and queue completion",
    "guest-factories": "Xbox 0x20004 and default factories, compute queues and process debug flags",
    "route-RRRRDD": "Upper route and guest output", "route-LDDRRRR": "Lower route and collision",
    "xmem-pages": "CPU page allocation, remapping and release",
    "xmem-query": "CPU virtual memory and image queries",
    "xmem-tags": "Memory allocation tag encoding",
    "xmem-hooks": "Guest allocator callback routing",
    "xmem-watch": "Guest write-watch dirty-page query",
    "xmem-scatter": "CPU scattered physical-page mapping",
    "xmem-virtual-cpu": "CPU virtual-memory allocation profiles",
    "xmem-heap-profiles": "CPU heap attribute profiles",
    "xmem-heaps": "CPU title and private heap creation",
    "invalid-route": "Invalid input exit status", "xmem-default": "Xbox memory allocation and working set",
    "d3d12-signatures": "Root signature 1.0 and 1.1 serialization, deserialization and errors",
    "d3d12-exports": "Blob, debug interface, four DXGIX entry points and Series CPU memory clear",
    "d3d12-features": "Xbox capability records compared with the selected adapter",
    "d3d12-indirect": "Indirect dispatch and texture cache flushes between compute writes",
    "guest-frame-rgb10": "RGB10A2 display allocation and two guarded GPU readbacks",
    "guest-present-4k-owned": "4K RGB10 display scaled to a TuxBox window",
    "guest-present-4k-oversized": "4K RGB10 display fitted to the title window’s monitor",
    "guest-present-4k-fit": "4K RGB10 display scaled inside a smaller title window",
    "guest-present-4k-splash": "Package splash over a fitted 4K title window until frames come steadily",
    "guest-present-rgb10": "Two RGB10A2 frames sampled from the desktop window",
    "guest-present-planes": "Two frames presented as two planes, the second blended over the first",
    "d3d12-frame-events": "Frame origins, timeout tokens, completed presentation events and display statistics",
    "d3d12-heaps": "Placed resources and allocation offsets in a shared memory heap",
    "d3d12-placement": "Virtual-address BC1 cubes and R8 textures across page changes",
    "d3d12-copy-resource": "Whole-buffer and multi-mip texture copies through Xbox command-list slot 84",
    "d3d12-mesh": "Mesh and amplification shader dispatch on a mesh-tier adapter",
    "d3d12-draw": "Scissored triangle through Xbox graphics pipeline and draw slots",
    "d3d12-stream": "Xbox Device2 graphics pipeline stream, ownership and masked draw",
    "d3d12-tables": "Shader reads and writes through tables, including a late-copied texture view",
    "d3d12-queues": "Direct, compute and copy queue synchronization",
    "d3d12-queries": "GPU timestamps, occlusion readback and descriptor heap handles",
    "d3d12-guids": "D3D12 and DXGI identifier data by name and ordinal",
    "xg-native-layout": "Native Scarlett XG texture layouts with Rust XMem",
    "xg-native-addresses": "Supplied XG texture layouts and element addresses",
    "xg-native-formats": "Supplied XG layouts and addresses across 78 formats",
    "xg-formats": "TuxBox XG layouts and addresses across 78 formats",
    "xg-addresses": "TuxBox XG texture layouts and element addresses",
    "dxc-allocator": "Xbox DXC caller allocator and factory ordinals",
    "dxc-create-blob": "Xbox DXC utilities and binary blob lifetime",
    "dxc-compile": "Scarlett DXC compute shader and compiler diagnostics",
    "desktop-clear": "Desktop D3D12 pixel readback", "root-signature": "Xbox root-signature import and compute readback",
    "guest-command-list": "Xbox command-list clear and ordered copy readback",
    "guest-command-errors": "Guest command rejection, fence isolation and list recovery",
    "guest-present": "Xbox PresentX and two screen-verified frames",
    "lanes-xbox-replay": "Lanes Xbox imports, course, board readback and gameplay replay",
    "lanes-overlay": "TuxBox's performance panel on screen over Lanes",
    "lanes-selftest-labels": "Bottom-of-pipe label writes with and without a flush",
    "lanes-selftest-cross-queue": "Compute and direct queues waiting on each other's labels",
    "lanes-selftest-compute-frame": "Compute queue frame submission",
    "lanes-selftest-skipped-command": "A skipped command leaving its list and fence write intact",
    "lanes-selftest-copy-memory": "CopyMemoryToMemoryX direction and contents",
    "lanes-selftest-shared-texture": "A waiting queue reading a texture another queue writes",
    "lanes-selftest-indirect-dispatch": "ExecuteIndirect matching a direct dispatch",
    "lanes-selftest-direct-copy-wait": "A direct queue waiting before and after a buffer copy",
    "lanes-selftest-gpu-indirect-dispatch": "GPU-written arguments controlling an indirect dispatch",
    "lanes-selftest-gpu-indirect-order": "Indirect arguments and counts read after the dispatch that wrote them",
    "lanes-selftest-mesh-signature": "A mesh dispatch signature made only when the device reports mesh shaders",
    "lanes-selftest-mesh-signature-none": "A mesh dispatch signature refused when the host profile takes mesh shaders away",
    "lanes-selftest-dispatch-wait": "A memory wait before and after a compute dispatch",
    "lanes-selftest-texture-readback": "A guest texture copied back to title memory",
    "lanes-selftest-texture-publication": "GPU texture writes published to title memory, leaving padding, closed pages and memory past the texture alone",
    "lanes-benchmark": "Lanes frame, submission, fence handover, copy and pipeline timings",
    "lanes-selftest-ray-tracing-builder": "Single, copied and batched builds through XboxBvhBuilder",
    "lanes-selftest-ray-tracing-guest-memory": "A structure, its copy and a scene built in title memory that no resource names",
    "lanes-selftest-ray-tracing-rehydrate": "Console structures rehydrated in title memory as host builds of their coded geometry, traced through a scene",
    "lanes-selftest-thread-dimensions": "Slot 80 thread-dimension dispatch converted using the bound PSO group size",
    "lanes-selftest-descriptor-reclaim": "Shader-visible descriptor slot reclamation across completed, in-flight, ClearState, and OOM lists",
    "lanes-selftest-unwritten-descriptors": "A descriptor table with slots the title never wrote, in a heap whose title memory held leftover bytes",
    "lanes-shader-cache-prepared": "A console shader translated before launch, loaded from the prepared cache",
    "lanes-shader-cache-compiled": "A console shader translated by the host worker while the game runs",
    "lanes-shader-cache-reused": "The worker's translation reused on the next launch without compiling",
    "lanes-wave-shader-narrow": "A 64-lane console shader translated for 32-lane waves while the game runs",
    "lanes-wave-shader-host": "A 64-lane console shader with the translation this graphics card needs",
    "lanes-raster-tables": "Console raster programs drawn under the root reflected from their two descriptor tables",
    "lanes-shader-cache-wave-alloc": "A wave32 masked DPP exclusive prefix lowered to WavePrefixSum and loaded by the provider",
    "lanes-shader-cache-mixed-lane": "An EXEC-independent lane write and a dynamic lane read in a Wave32 compute shader",
    "lanes-lane-spill-interp": "A console pixel program with a lane-spill area and an f16 interpolator selector of the same number",
    "lanes-raster-samplers": "A console pixel program's literal linear sampler, which the title's root names in another state",
    "lanes-raster-heap-stride": "A console vertex program's indexed read of a heap buffer, through the paged stride table",
    "lanes-raster-sampler-waterfall": "A console vertex program's heap sampler, read back through a waterfall loop after an if/else",
    "lanes-compute-root": "Root-less console compute programs translated only against a three-table root",
    "lanes-mesh-samplers": "The same literal sampler in a mesh pipeline, added to the mesh root the provider builds",
    "lanes-mesh-sampler-rootless": "Literal samplers in mesh pipelines whose root has no static samplers",
    "lanes-compute-root-recovery": "Partially reflected compute root with an appended descriptor range and paging",
    "lanes-compute-root-srv": "A console compute program under three tables and a root SRV that stores where the title's root declares an SRV",
    "lanes-large-dispatch": "A console compute program dispatched with more than 65,535 groups in a dimension, split with its group IDs intact",
    "lanes-late-pipeline": "A console compute program whose driver compile misses the pipeline deadline runs once the finished compile replaces its stand-in",
    "lanes-compute-heap-stride": "A root-less console compute program reads a structured buffer by thread number, through the paged stride table",
    "lanes-bindless-compute-wide": "A direct-indexed compute program's heap UAV through the paged index map",
    "lanes-bindless-compute-constrained": "The same with fewer resident descriptors than the index it uses",
    "lanes-bindless-sampler-wide": "A direct-indexed compute program's literal sampler, added to its paged root",
    "lanes-bindless-sampler-constrained": "The same with fewer resident descriptors than the indices it uses",
    "lanes-bindless-table-compute-wide": "A direct-indexed compute program's heap UAV when its root also has a table",
    "lanes-bindless-table-compute-constrained": "The same with fewer resident descriptors than the index it uses",
    "lanes-bindless-heap-read-wide": "A direct-indexed program with no image write leaving a heap UAV texture's title memory alone",
    "lanes-bindless-heap-read-constrained": "The same with fewer resident descriptors than the indices in use",
    "lanes-bindless-texture-store-wide": "A direct-indexed store to a heap UAV texture, published to title memory",
    "lanes-bindless-texture-store-constrained": "The same with fewer resident descriptors than the index it uses",
    "lanes-bindless-table-store-wide": "A table store under a direct-indexed root published alone, leaving another heap UAV texture's title memory alone",
    "lanes-bindless-table-store-constrained": "The same with fewer resident descriptors than the indices in use",
    "lanes-package-replay": "Lanes gameplay replay (earlier guest with desktop graphics)", "cpu-replay": "8,388,608-move CPU replay",
}


def homebrew_matrix(evidence):
    rows = []
    for filename in sorted(evidence.glob("*.json")):
        data = json.loads(filename.read_text(encoding="utf-8"))
        if data.get("type") == "homebrew":
            rows.append((filename.name, data))

    gates = list(dict.fromkeys(r["gate"] for _, data in rows for r in data["checks"]))
    return rows, gates


def status():
    rows, gates = homebrew_matrix(ROOT / "evidence")
    lines = ["# Measured homebrew results", "",
             "Each column represents one run on its named host, provider and hardware. "
             "Each receipt records the guest output, exit status and binary hashes.", ""]
    names = [f"[{Path(name).stem}](../evidence/{name})" for name, _ in rows]
    pending_windows = not any(data["host"] == "Windows" for _, data in rows)
    if pending_windows:
        names.append("Physical Windows x64")
    lines.append("| Gate | " + " | ".join(names) + " |")
    lines.append("|---" * (1 + len(rows) + int(pending_windows)) + "|")
    for gate in gates:
        cells = []
        for _, d in rows:
            result = next((r for r in d["checks"] if r["gate"] == gate), None)
            cells.append("Pass" if result and result["passed"] else "Fail" if result else "—")
        if pending_windows:
            cells.append("Pending")
        lines.append("| " + " | ".join([GATE_LABELS.get(gate, gate), *cells]) + " |")
    lines += ["", "Rebuild this table from the repository root with "
              "`python tuxbox.py status > docs/RESULTS.md`. A dash means that the gate was omitted from that run. "
              "[Current Lanes replay](../examples/lanes/README.md#deterministic-replay) · [Xbox DLL checks](LIBRARIES.md#run-the-provider-checks) · [Retail results and priorities](PROGRESS.md)"]
    return "\n".join(lines)
