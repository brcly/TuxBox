#!/usr/bin/env python3
"""Measure each stage from a recorded console shader to a ready pipeline.

`run` reproduces the per-stage table for the fixed sample in
tools/reference/stage-baseline/sample.json: for each request, time and peak
memory in the translator, DXC, DXV, vkd3d-proton's DXIL to SPIR-V conversion
and the driver's compile, on each card in the configuration. The other
commands run one part: `sample` chooses a new sample, `cpu` the host stages,
`gpu` one card, `report` the table. Paths come from the shader harness
configuration (--config, default build/shader-harness.json); see
docs/SHADER_HARNESS.md. Output goes to a run folder under build/stage-baseline/,
and only the run folder holds translated programs.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import glob
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
from threading import Lock
from time import monotonic, sleep, strftime

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from project_tuxbox import stage_baseline as stages  # noqa: E402
from project_tuxbox import shader_survey as survey  # noqa: E402
from project_tuxbox.console_cbv import configure_console_cbv, prepend  # noqa: E402
from project_tuxbox.pipeline_prep import winpath  # noqa: E402
from tools.shader_harness import build_translator, git, load_config, need, say  # noqa: E402

WARMUP = """RWByteAddressBuffer output : register(u0);
[numthreads(64, 1, 1)] void main(uint lane : SV_DispatchThreadID) { output.Store(lane * 4, lane); }
"""
# The driver's own caches, off, so each compile is cold. Each driver reads its own.
COLD = {"__GL_SHADER_DISK_CACHE": "0", "MESA_SHADER_CACHE_DISABLE": "true", "VKD3D_SHADER_CACHE_PATH": "0"}


def load_rows(path):
    return survey.load(path) if path.is_file() else []


def sample_requests(sample, requests):
    entries = json.loads(Path(sample).read_text())["requests"]
    present = [entry for entry in entries if (Path(requests) / entry["request"]).exists()]
    missing = [entry["request"] for entry in entries if entry not in present]
    return present, missing


def cpu_stage(args, config, run):
    need(config, "requests", "dxc")
    run.mkdir(parents=True, exist_ok=True)
    xshader = args.xshader.resolve() if args.xshader else build_translator(ROOT, run / "xshader")
    dxc = Path(config["dxc"])
    tools = (xshader, dxc, dxc.with_name("dxv"))
    entries, missing = sample_requests(args.sample, config["requests"])
    output = run / "cpu.jsonl"
    done = {(row["request"], row["host"]) for row in load_rows(output)}
    work = [(entry, host) for entry in entries
            for host in stages.hosts_for(Path(config["requests"]) / entry["request"])
            if (entry["request"], host) not in done]
    meta = run / "meta.json"
    if not meta.is_file():
        version = subprocess.run([dxc, "--version"], capture_output=True, text=True).stdout.strip()
        sample = Path(args.sample).resolve()
        meta.write_text(json.dumps({"sample": str(sample.relative_to(ROOT) if sample.is_relative_to(ROOT) else sample),
                                    "head": git("rev-parse", "--short", "HEAD"), "dirty": bool(git("status", "--porcelain")),
                                    "dxc_version": version, "jobs": args.jobs, "requests": len(entries),
                                    "missing": missing, "load": round(os.getloadavg()[0], 1)}, indent=1) + "\n")
    say(f"host stages: {len(work)} translations of {len(entries)} requests, {args.jobs} at a time"
        + (f"; {len(missing)} sample requests are not recorded here" if missing else ""))
    scratch = run / "scratch"
    scratch.mkdir(exist_ok=True)
    lock = Lock()

    def one(item):
        entry, host = item
        row = stages.measure(Path(config["requests"]) / entry["request"], host, tools, scratch, run / "dxil",
                             sys.executable)
        row["kind"] = entry["kind"]
        with lock, open(output, "a") as stream:
            stream.write(json.dumps(row, sort_keys=True) + "\n")
        say(f"  {entry['request'][:12]} {host}: {row['status']} in {row['wall_seconds']:.1f}s")

    with ThreadPoolExecutor(args.jobs) as pool:
        list(pool.map(one, work))
    shutil.rmtree(scratch, ignore_errors=True)


def build_guest(folder):
    from build import Toolchain
    exe = folder / "stage-baseline.exe"
    source = stages.REFERENCE / "guest.cpp"
    if not exe.is_file() or exe.stat().st_mtime < source.stat().st_mtime:
        tools = Toolchain()
        obj = tools.compile(source, folder, "/W4", "/WX")
        tools.link(exe, obj, "d3d12.lib", "dxgi.lib", "kernel32.lib")
    return exe


def build_layer(folder):
    folder.mkdir(parents=True, exist_ok=True)
    library = folder / "pipeline_timing.so"
    source = stages.REFERENCE / "layer.c"
    if not library.is_file() or library.stat().st_mtime < source.stat().st_mtime:
        subprocess.run([os.environ.get("CC", "cc"), "-std=c11", "-Wall", "-Wextra", "-Werror", "-O2", "-fPIC",
                        "-shared", "-pthread", str(source), "-o", str(library)], check=True)
    shutil.copyfile(stages.REFERENCE / "layer.json", folder / "pipeline_timing.json")
    return folder


def driver_of(adapter):
    """driverName and driverInfo of the Vulkan device with this PCI id."""
    try:
        text = subprocess.run(["vulkaninfo", "--summary"], capture_output=True, text=True, timeout=30).stdout
    except (OSError, subprocess.SubprocessError):
        return None
    vendor, device = (int(part, 16) for part in adapter.split(":"))
    for block in re.split(r"\nGPU\d+:", text):
        fields = dict(re.findall(r"^\s*(\w+)\s*=\s*(.+)$", block, re.M))
        if fields.get("vendorID") == f"0x{vendor:04x}" and fields.get("deviceID") == f"0x{device:04x}":
            return f"{fields.get('driverName', '?')} {fields.get('driverInfo', '?')}"
    return None


def gpu_stage(run, card, config, timeout):
    """Create each compiled compute program's pipeline on one card, with the driver cache off."""
    need(config, "proton", "dxc")
    proton = Path(config["proton"]).expanduser().resolve()
    folder = (run / "gpu" / card["name"]).resolve()
    folder.mkdir(parents=True, exist_ok=True)
    rows = load_rows(run / "cpu.jsonl")
    dxil = (run / "dxil").resolve()
    lines, hosts = stages.gpu_plan(rows, dxil, winpath)
    (folder / "hosts.json").write_text(json.dumps(hosts) + "\n")
    results, times = folder / "results.txt", folder / "driver.txt"
    previous = stages.guest_results(results.read_text()) if results.is_file() else None
    finished = set(previous["done"]) | set(previous["unfinished"]) | set(previous["skipped"]) if previous else set()
    pending = [line for line in lines if line.split()[0] not in finished]
    if previous and previous["unfinished"]:
        say(f"{card['name']}: {len(previous['unfinished'])} pipelines did not finish last time; not retried")
    if not pending:
        say(f"{card['name']}: every pipeline is done")
        return
    (folder / "card.json").write_text(json.dumps({"name": card["name"], "id": card["id"],
                                                  "driver": driver_of(card["id"])}) + "\n")
    warm = folder / "warmup"
    if not (warm.with_suffix(".cso")).is_file():
        warm.with_suffix(".hlsl").write_text(WARMUP)
        subprocess.run([config["dxc"], "-T", "cs_6_0", "-E", "main", "-Fo", str(warm.with_suffix(".cso")),
                        str(warm.with_suffix(".hlsl"))], check=True)
        warm.with_suffix(".root").write_text("uav 0 0 1\n")
    plan = folder / "plan.txt"
    plan.write_text(f"warmup {winpath(warm.with_suffix('.cso'))} 0 {winpath(warm.with_suffix('.root'))}\n"
                    + "".join(pending))
    exe = build_guest(run.resolve() / "gpu")
    layer = build_layer(run.resolve() / "gpu" / "layer")
    prefix = folder / "prefix"
    if not prefix.is_dir():
        # A disposable copy, without its GPU caches, as the other guest checks use.
        shutil.copytree(Path(card["prefix"]).expanduser().resolve(), prefix, symlinks=True,
                        ignore=shutil.ignore_patterns("shadercache", "tuxbox_shader_cache"))
    files = proton / "files"
    wine_libs = files / "lib/wine"
    for name in ("d3d12.dll", "d3d12core.dll"):
        shutil.copyfile(wine_libs / "vkd3d-proton/x86_64-windows" / name, folder / name)
    shutil.copyfile(wine_libs / "dxvk/x86_64-windows/dxgi.dll", folder / "dxgi.dll")
    shutil.copyfile(exe, folder / exe.name)
    for name in ("tmp", "runtime", "cache"):
        (folder / name).mkdir(mode=0o700, exist_ok=True)
    env = dict(os.environ, WINEPREFIX=str(prefix), WINESERVER=str(files / "bin/wineserver"), WINEDEBUG="-all",
               VKD3D_DEBUG="warn", WINEDLLOVERRIDES="d3d12,d3d12core,dxgi=n,b", XDG_CACHE_HOME=str(folder / "cache"),
               XDG_RUNTIME_DIR=str(folder / "runtime"), TMPDIR=str(folder / "tmp"), PROTONPATH=str(proton),
               TUXBOX_REFERENCE_CBV="1", TUXBOX_PIPELINE_TIMES=str(times), **COLD, **card.get("env", {}))
    env["RADV_DEBUG"] = ",".join(filter(None, (os.environ.get("RADV_DEBUG"), "nocache")))
    # The title's own constant-buffer settings, which decide vkd3d-proton's binding path.
    configure_console_cbv("proton", env, None, lambda message: say(f"{card['name']}: {message}"))
    del env["TUXBOX_REFERENCE_CBV"]
    prepend(env, "VK_ADD_LAYER_PATH", str(layer), ":")
    prepend(env, "VK_INSTANCE_LAYERS", "VK_LAYER_TUXBOX_pipeline_timing", ":")
    command = ["bwrap", "--die-with-parent", "--unshare-pid", "--unshare-net", "--ro-bind", "/", "/",
               "--dev-bind", "/dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--bind", str(folder), str(folder),
               "--unsetenv", "WAYLAND_DISPLAY", "--chdir", str(folder)]
    # bwrap passes on this environment; name only what differs from it.
    for key, value in env.items():
        if os.environ.get(key) != value:
            command += ["--setenv", key, value]
    command += ["--", "xvfb-run", "-a", "timeout", "-k", "10s", f"{timeout}s", str(files / "bin/wine"),
                str(folder / exe.name), "--adapter", card["id"], winpath(plan), winpath(results)]
    settings = {key: value for key, value in env.items()
                if key.startswith(("VK", "__GL", "MESA", "RADV", "VKD3D", "DXVK"))}
    (folder / "settings.json").write_text(json.dumps(settings, indent=1) + "\n")
    say(f"{card['name']}: creating {len(pending)} pipelines on {card['id']} (driver cache off)")
    with open(folder / "guest.log", "a") as log:
        status = watch_guest(command, log, results, card["name"], len(pending))
    say(f"{card['name']}: guest exited {status}")
    if results.is_file() and results.read_text().splitlines()[-1:] == ["finished"]:
        # Kept until then so an interrupted card can resume in the same prefix.
        shutil.rmtree(prefix, ignore_errors=True)


def watch_guest(command, log, results, name, total, *, poll=5, still=300, start_limit=600, grace=15):
    """Run the sandboxed guest, report each pipeline as it starts and ends, and stop the
    sandbox once the guest has exited: Wine's own processes keep it open otherwise."""
    process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    started = monotonic()
    offset, current, since, nagged, seen, gone_at, count = 0, None, None, 0, False, None, 0
    while process.poll() is None:
        sleep(poll)
        if results.is_file():
            with open(results) as stream:
                stream.seek(offset)
                chunk = stream.read()
            complete = chunk[:chunk.rfind("\n") + 1]
            offset += len(complete.encode())
            for line in complete.splitlines():
                fields = line.split()
                if fields[:1] == ["begin"] and fields[1] != "warmup":
                    count += 1
                    current, since, nagged = fields[1], monotonic(), 0
                    say(f"{name}: {count}/{total} {current[:12]} started")
                elif fields[:1] == ["done"] and fields[1] != "warmup":
                    say(f"{name}: {count}/{total} {fields[1][:12]} done in "
                        f"{(int(fields[4]) - int(fields[3])) / 1e9:.1f}s, {fields[5]}")
                    current = None
                elif fields[:1] in (["skipped"], ["error"]):
                    say(f"{name}: {line}")
        if current and monotonic() - since >= still * (nagged + 1):
            nagged += 1
            say(f"{name}: {current[:12]} still compiling after {(monotonic() - since) / 60:.0f} min")
        states = stages.process_states(process.pid, "stage-baseline")
        seen = seen or bool(states)
        if stages.guest_gone(states, seen) or (not seen and monotonic() - started > start_limit):
            gone_at = gone_at or monotonic()
            if monotonic() - gone_at >= grace:
                why = "the guest exited" if seen else "the guest never started"
                say(f"{name}: {why}{f' during {current[:12]}' if current else ''}; stopping the sandbox")
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                return "guest gone"
        else:
            gone_at = None
    return process.returncode


def card_report(run, card):
    folder = run / "gpu" / card["name"]
    results = folder / "results.txt"
    if not results.is_file():
        return None
    parsed = stages.guest_results(results.read_text())
    times = folder / "driver.txt"
    records = stages.driver_records(times.read_text()) if times.is_file() else []
    hosts = json.loads((folder / "hosts.json").read_text())
    device = parsed["device"] or {}
    return {"name": card["name"], "id": card["id"], "driver": card.get("driver"),
            "waves": f"{device.get('wave_min')}-{device.get('wave_max')}" if device else "?",
            "rows": stages.gpu_rows(parsed, records, hosts), "skipped": parsed["skipped"], "error": parsed["error"],
            "settings": json.loads((folder / "settings.json").read_text())}


def report_stage(run):
    meta = json.loads((run / "meta.json").read_text())
    rows = load_rows(run / "cpu.jsonl")
    sample = ROOT / meta["sample"] if not Path(meta["sample"]).is_absolute() else Path(meta["sample"])
    logged = {entry["request"]: entry["logged_seconds"] for entry in json.loads(sample.read_text())["requests"]
              if "logged_seconds" in entry} if sample.is_file() else {}
    for row in rows:
        if row["request"] in logged:
            row["logged_seconds"] = logged[row["request"]]
    cards = [found for path in sorted((run / "gpu").glob("*/card.json"))
             if (found := card_report(run, json.loads(path.read_text())))]
    report = {**meta, "cpu": rows, "cards": cards}
    (run / "report.json").write_text(json.dumps(report, indent=1) + "\n")
    (run / "report.md").write_text(stages.render(report))
    say(f"report: {run / 'report.md'}")


def choose_sample(args, config):
    need(config, "requests")
    names = sorted(path.name for path in Path(config["requests"]).iterdir())
    rows = survey.load(args.survey)
    lines = []
    for pattern in args.logs:
        for path in sorted(glob.glob(os.path.expanduser(pattern))):
            lines += Path(path).read_text(errors="replace").splitlines()
    logged = stages.logged_compiles(lines)
    entries = stages.build_sample(rows, names, logged, per_stratum=args.per_stratum, tail=args.tail,
                                  slowest=args.slowest, slow_band=args.slow_band)
    sample = {"scope": "Request keys of a title's recorded shader requests, the stratum each was drawn from, and "
                       "the longest driver compile logged for it in game runs. No shader bytes.",
              "seed": stages.SEED, "sized_by": args.sized_by,
              "strata": [label for _, label in stages.BUCKETS], "requests": entries}
    args.output.write_text(json.dumps(sample, indent=1) + "\n")
    say(f"wrote {len(entries)} requests to {args.output}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", type=Path, default=ROOT / "build/shader-harness.json")
    commands = parser.add_subparsers(dest="command", required=True)
    every = commands.add_parser("run", help="host stages, every configured card, then the table")
    every.add_argument("--no-gpu", action="store_true", help="host stages only")
    host = commands.add_parser("cpu", help="the translator, DXC and DXV")
    for command in (every, host):
        command.add_argument("--output", type=Path, help="run folder; an unfinished one is resumed")
        command.add_argument("--sample", type=Path, default=stages.SAMPLE)
        command.add_argument("--xshader", type=Path, help="translator to measure (default: build this tree)")
        command.add_argument("--jobs", type=int, default=2, help="host translations at a time (default 2)")
        command.add_argument("--timeout", type=int, default=6 * 3600, help="seconds for each card's guest")
    card = commands.add_parser("gpu", help="vkd3d-proton and the driver on one card")
    card.add_argument("run", type=Path)
    card.add_argument("--name", required=True)
    card.add_argument("--adapter", required=True, help="PCI vendor:device")
    card.add_argument("--prefix", type=Path, required=True)
    card.add_argument("--proton", type=Path, help="Proton to use (default: the configuration's)")
    card.add_argument("--timeout", type=int, default=6 * 3600)
    card.add_argument("--env", action="append", default=[], help="KEY=VALUE for the guest")
    table = commands.add_parser("report", help="write report.md for a run folder")
    table.add_argument("run", type=Path)
    pick = commands.add_parser("sample", help="choose a new sample")
    pick.add_argument("--survey", type=Path, required=True, help="a `shader_harness.py survey` result")
    pick.add_argument("--sized-by", required=True, help="the translator commit the survey used")
    pick.add_argument("--logs", nargs="*", default=[], help="game logs with CreatePipelineState timings")
    pick.add_argument("--per-stratum", type=int, default=3)
    pick.add_argument("--tail", type=int, default=8)
    pick.add_argument("--slowest", type=int, default=8)
    pick.add_argument("--slow-band", type=int, default=8)
    pick.add_argument("--output", type=Path, default=stages.SAMPLE)
    args = parser.parse_args(argv)
    config = load_config(args.config)
    if args.command == "sample":
        choose_sample(args, config)
        return 0
    if args.command in ("run", "cpu"):
        if not 1 <= args.jobs <= 16:
            parser.error("--jobs must be 1..16")
        run = (args.output or ROOT / "build/stage-baseline" / strftime("%Y%m%d-%H%M%S")).resolve()
        cpu_stage(args, config, run)
        if args.command == "run":
            if not args.no_gpu:
                need(config, "adapters")
                for adapter in config["adapters"]:
                    gpu_stage(run, adapter, config, args.timeout)
            report_stage(run)
        return 0
    if args.command == "gpu":
        extra = dict(item.split("=", 1) for item in args.env)
        adapter = {"name": args.name, "id": args.adapter, "prefix": str(args.prefix), "env": extra}
        if args.proton:
            config["proton"] = str(args.proton.expanduser())
        gpu_stage(args.run.resolve(), adapter, config, args.timeout)
        report_stage(args.run.resolve())
        return 0
    if args.command == "report":
        report_stage(args.run.resolve())
        return 0
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
