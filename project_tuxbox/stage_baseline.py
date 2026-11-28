"""The per-stage baseline: what each step from a console shader to a ready pipeline costs.

A fixed sample of recorded requests (`tools/reference/stage-baseline/sample.json`,
request keys only) is measured stage by stage:

- the translator, DXC and DXV, on the host, exactly as shader preparation runs
  them (`runtime_shaders.compile_request` in a fresh cache), with each tool
  timed and its peak resident memory read by `tools/reference/stage-baseline/probe.py`;
- vkd3d-proton's DXIL to SPIR-V conversion and the driver's pipeline compile,
  on each card, by a guest that creates one compute pipeline per program
  (`tools/reference/stage-baseline/guest.cpp`) with a Vulkan layer that times
  the driver's part (`layer.c`).

Nothing here changes how a shader is compiled. Results hold request keys,
sizes, times and memory; no shader bytes or HLSL.
"""
from collections import defaultdict
import hashlib
import json
from pathlib import Path
import re
import shutil
import statistics
import struct
import subprocess
from tempfile import TemporaryDirectory
from time import monotonic

from .runtime_shaders import ShaderRefused, compile_request
from .shader_diagnostics import normalize_refusal
from .shader_prep import ToolFailure
from . import shader_survey as survey

ROOT = Path(__file__).resolve().parents[1]
REFERENCE = ROOT / "tools/reference/stage-baseline"
SAMPLE = REFERENCE / "sample.json"
PROBE = REFERENCE / "probe.py"
TOOLS = ("xshader", "dxc", "dxv")
# HLSL size strata, in bytes. The last is the tail of programs over 1.8 MB.
BUCKETS = ((0, "<32K"), (32 << 10, "32K-128K"), (128 << 10, "128K-512K"), (512 << 10, "512K-1.8M"),
           (1_800_000, ">1.8M"))
SEED = "t14-stage-baseline-v1"
# The provider names each slow compute compile with the guest program's short
# hash, and since 67508417 with the exact request it compiled.
LOGGED = re.compile(r"CreatePipelineState #\d+ CS ([0-9a-f]{8})(?: request=([0-9a-f_]+|original))?"
                    r"(?: bytes=\d+)? finished in (\d+)s")


def bucket(size):
    return [label for floor, label in BUCKETS if size >= floor][-1]


def rank(request):
    """A fixed order for picking requests, independent of how they were listed."""
    return hashlib.sha256(f"{SEED}:{request}".encode()).hexdigest()


def request_kind(request, profile):
    stage = profile.split("_", 1)[0]
    return f"link-{stage}" if request.endswith(".link") else stage


def survey_requests(rows):
    """One `(kind, hlsl_bytes)` per translated request, from its 64-lane translation
    when it has one, so a compute program counts once."""
    found = {}
    for row in sorted(rows, key=lambda r: r["host"] != "wave64"):
        if row["status"] == "translated" and row["request"] not in found:
            found[row["request"]] = (request_kind(row["request"], row["profile"]), row["hlsl_bytes"])
    return found


def logged_compiles(lines):
    """The longest logged driver compile for each guest program: short hash ->
    (seconds, exact request or None)."""
    found = {}
    for line in lines:
        match = LOGGED.search(line)
        if not match:
            continue
        short, request, seconds = match[1], match[2], int(match[3])
        old_seconds, old_request = found.get(short, (0, None))
        exact = request if request and request != "original" else None
        found[short] = (max(seconds, old_seconds), old_request or exact)
    return found


def resolve_logged(short, exact, names):
    if exact and f"{exact}.dxbc" in names:
        return f"{exact}.dxbc"
    matches = [name for name in names if name.startswith(short) and name.endswith(".dxbc")]
    return matches[0] if len(matches) == 1 else None


def build_sample(rows, names, logged, *, per_stratum=3, tail=8, slowest=8, slow_band=8, band=(100, 200)):
    """The sample: `per_stratum` requests from each stage and HLSL size, `tail` more of
    the programs over 1.8 MB, and the logged slow driver compiles that the survey
    translated (the `slowest`, and `slow_band` more inside `band` seconds)."""
    names = set(names)
    translated = {name: info for name, info in survey_requests(rows).items() if name in names}
    strata = defaultdict(list)
    for name, (kind, size) in translated.items():
        strata[kind, bucket(size)].append(name)
    chosen = {}

    def take(name, why, **extra):
        kind, size = translated[name]
        entry = chosen.setdefault(name, {"request": name, "kind": kind, "bucket": bucket(size),
                                         "hlsl_bytes_at_sampling": size, "why": []})
        entry["why"].append(why)
        entry.update(extra)

    for (kind, label), members in sorted(strata.items()):
        for name in sorted(members, key=rank)[:per_stratum]:
            take(name, "stratum")
    big = sorted((name for name, (_, size) in translated.items() if size >= BUCKETS[-1][0]), key=rank)
    for name in [name for name in big if name not in chosen][:tail]:
        take(name, "tail")
    resolved = []
    for short, (seconds, exact) in logged.items():
        name = resolve_logged(short, exact, names)
        # A program this translator refuses has no pipeline to time.
        if name in translated:
            resolved.append((seconds, name))
    slow = sorted(resolved, key=lambda item: (-item[0], rank(item[1])))
    picked = slow[:slowest]
    within = sorted((item for item in slow[slowest:] if band[0] <= item[0] <= band[1]), key=lambda item: rank(item[1]))
    for seconds, name in picked + within[:slow_band]:
        take(name, "logged slow driver compile", logged_seconds=seconds)
    return sorted(chosen.values(), key=lambda entry: (entry["kind"], entry["bucket"] or "", entry["request"]))


# ---- host stages -------------------------------------------------------------------------------

def write_stub(path, log, name, tool, python):
    path.write_text(f'#!/bin/sh\nexec "{python}" -I "{PROBE}" "{log}" "{name}" "{tool}" "$@"\n')
    path.chmod(0o755)
    return path


def tool_runs(events, ended_at):
    """Each tool run from the probe's records. A start without an end was killed at
    the caller's timeout; it ran until the next tool started, or the request ended."""
    ends = {event["pid"]: event for event in events if event["event"] == "end"}
    starts = [event for event in events if event["event"] == "start"]
    runs = []
    for index, start in enumerate(starts):
        if "-help" in start["options"]:
            continue
        end = ends.get(start["pid"])
        run = {"tool": start["tool"], "options": start["options"]}
        if end:
            run.update({key: end[key] for key in ("seconds", "cpu_seconds", "exit", "output_bytes", "wave_size")
                        if key in end})
            run["peak_mib"] = round(end["peak_rss_kib"] / 1024, 1)
        else:
            until = starts[index + 1]["at"] if index + 1 < len(starts) else ended_at
            run.update(seconds=round(until - start["at"], 4), killed=True)
        runs.append(run)
    return runs


def stage_totals(runs):
    totals = {}
    for tool in TOOLS:
        mine = [run for run in runs if run["tool"] == tool]
        if mine:
            totals[tool] = {"seconds": round(sum(run["seconds"] for run in mine), 3),
                            "cpu_seconds": round(sum(run.get("cpu_seconds", 0) for run in mine), 3),
                            "peak_mib": max((run.get("peak_mib", 0) for run in mine), default=0),
                            "runs": len(mine), "killed": sum(bool(run.get("killed")) for run in mine)}
    return totals


def hosts_for(source):
    if source.suffix == ".wave32":
        return ("wave32",)
    if source.suffix == ".dxbc" and survey.is_compute(source):
        return ("wave64", "wave32")
    return ("wave64",)


def output_key(source, host):
    return f"{source.stem}_w32" if host == "wave32" and source.suffix == ".dxbc" else source.stem


def measure(source, host, tools, scratch, keep, python):
    """Prepare one request on one host as shader preparation does, timing each tool."""
    row = {"request": source.name, "host": host}
    if source.is_dir():
        row["input_bytes"] = sum(path.stat().st_size for path in source.iterdir() if path.is_file())
    else:
        row["input_bytes"] = source.stat().st_size
    with TemporaryDirectory(prefix="tuxbox-stage-", dir=scratch) as tmp:
        work = Path(tmp)
        log = work / "probe.jsonl"
        stubs = tuple(write_stub(work / name, log, name, tool, python) for name, tool in zip(TOOLS, tools))
        asked = source
        if host == "wave32" and source.suffix == ".dxbc":
            asked = work / f"{source.stem}_w32.wave32"
            shutil.copyfile(source, asked)
        cache = work / "cache"
        cache.mkdir()
        started = monotonic()
        try:
            compile_request(asked, asked.stem, stubs, cache, None)
            row["status"] = "compiled"
        except ShaderRefused as error:
            row.update(status="refused", reason=normalize_refusal(str(error)))
        except (OSError, ValueError, ToolFailure, subprocess.SubprocessError) as error:
            row.update(status="failed", reason=normalize_refusal(str(error)))
        row["wall_seconds"] = round(monotonic() - started, 3)
        events = [json.loads(line) for line in log.read_text().splitlines()] if log.is_file() else []
        runs = tool_runs(events, max((event["at"] for event in events), default=0))
        row["runs"] = runs
        row["stages"] = stage_totals(runs)
        translated = [run for run in runs if run["tool"] == "xshader" and run.get("exit") == 0]
        if translated:
            row["hlsl_bytes"] = translated[-1].get("output_bytes")
            row["wave_size"] = translated[-1].get("wave_size")
        targets = [run["options"][run["options"].index("-T") + 1] for run in runs
                   if run["tool"] == "dxc" and "-T" in run["options"]]
        if targets:
            row["profile"] = targets[-1]
        outputs = sorted(cache.glob("*.cso"))
        if row["status"] == "compiled" and outputs:
            data = outputs[0].read_bytes()
            row.update(dxil_bytes=len(data), dxil_sha256=hashlib.sha256(data).hexdigest())
            keep.mkdir(parents=True, exist_ok=True)
            name = output_key(source, host)
            (keep / f"{name}.cso").write_bytes(data)
            (keep / f"{name}.root").write_text(root_text(resource_bindings(data)))
    return row


# ---- GPU stages --------------------------------------------------------------------------------

# PSV0 resource types (DxilPipelineStateValidation.h).
PSV_KINDS = {1: "sampler", 2: "cbv", 3: "srv", 4: "srv", 5: "srv", 6: "uav", 7: "uav", 8: "uav", 9: "uav"}


def container_part(data, name):
    count = struct.unpack_from("<I", data, 28)[0]
    for index in range(count):
        offset = struct.unpack_from("<I", data, 32 + 4 * index)[0]
        if data[offset:offset + 4] == name:
            size = struct.unpack_from("<I", data, offset + 4)[0]
            return data[offset + 8:offset + 8 + size]
    return None


def resource_bindings(dxil):
    """`(kind, space, first, count)` for each resource the program binds; count 0 is unbounded."""
    psv = container_part(dxil, b"PSV0")
    if psv is None:
        raise ValueError("DXIL container has no PSV0 part")
    offset = 4 + struct.unpack_from("<I", psv, 0)[0]
    count = struct.unpack_from("<I", psv, offset)[0]
    offset += 4
    if not count:
        return []
    stride = struct.unpack_from("<I", psv, offset)[0]
    offset += 4
    bindings = []
    for index in range(count):
        kind, space, lower, upper = struct.unpack_from("<4I", psv, offset + index * stride)
        if kind not in PSV_KINDS:
            continue
        bindings.append((PSV_KINDS[kind], space, lower, 0 if upper == 0xFFFFFFFF else upper - lower + 1))
    return bindings


def root_text(bindings):
    return "".join(f"{kind} {space} {first} {count}\n" for kind, space, first, count in bindings)


def gpu_plan(rows, dxil, winpath):
    """Guest plan lines for every compute request that compiled: its 64-lane
    translation first, then its 32-lane one. Also each request's hosts, in that order."""
    by_request = defaultdict(dict)
    for row in rows:
        if row["status"] == "compiled" and row.get("profile", "").startswith("cs_"):
            by_request[row["request"]][row["host"]] = row
    lines, hosts = [], {}
    for request in sorted(by_request):
        variants = []
        for host in ("wave64", "wave32"):
            row = by_request[request].get(host)
            if row:
                hosts.setdefault(request, []).append(host)
                name = output_key(Path(request), host)
                variants.append(f"{winpath(dxil / f'{name}.cso')} {row.get('wave_size') or 0} "
                                f"{winpath(dxil / f'{name}.root')}")
        lines.append(f"{request} {' '.join(variants)}\n")
    return lines, hosts


def guest_results(text):
    device, done, begun, skipped, error = None, {}, {}, {}, None
    for line in text.splitlines():
        fields = line.split()
        if not fields:
            continue
        if fields[0] == "device":
            device = {"id": fields[1], "wave_min": int(fields[2]), "wave_max": int(fields[3])}
        elif fields[0] == "begin":
            begun[fields[1]] = int(fields[2])
        elif fields[0] == "done":
            done[fields[1]] = {"variant": int(fields[2]), "start": int(fields[3]), "end": int(fields[4]),
                               "hresult": fields[5], "dxil_bytes": int(fields[6])}
        elif fields[0] == "skipped":
            skipped[fields[1]] = " ".join(fields[2:])
        elif fields[0] == "error":
            error = " ".join(fields[1:])
    unfinished = [name for name in begun if name not in done]
    return {"device": device, "done": done, "skipped": skipped, "unfinished": unfinished, "error": error}


def driver_records(text):
    records = []
    for line in text.splitlines():
        fields = line.split()
        if fields[:1] == ["driver"] and len(fields) == 10:
            kind, start, end, result, count, hwm0, rss0, hwm1, rss1 = fields[1:]
            records.append({"kind": kind, "start": int(start), "end": int(end), "result": int(result),
                            "hwm0": int(hwm0), "rss0": int(rss0), "hwm1": int(hwm1), "rss1": int(rss1)})
        elif fields[:1] == ["destroyed"] and len(fields) == 3:
            records.append({"kind": "destroyed", "at": int(fields[1]), "rss": int(fields[2])})
    return records


def gpu_rows(results, records, hosts):
    """Split each pipeline call into the driver's compile and the rest, which is
    vkd3d-proton's DXIL to SPIR-V conversion. `hosts` maps a request to the hosts
    of its plan variants, in order."""
    rows = []
    calls = sorted(records, key=lambda r: r.get("start", r.get("at")))
    for request, done in results["done"].items():
        if request == "warmup":
            continue
        inside = [r for r in calls if r["kind"] != "destroyed" and done["start"] <= r["start"] and r["end"] <= done["end"]]
        before = [r for r in calls if r.get("end", r.get("at")) <= done["start"]]
        # The peak was reset when the last pipeline went away; the resident size then is the base.
        base = next((r.get("rss", r.get("rss1")) for r in reversed(before)), None)
        total = (done["end"] - done["start"]) / 1e9
        driver = sum(r["end"] - r["start"] for r in inside) / 1e9
        row = {"request": request, "host": hosts[request][done["variant"]], "hresult": done["hresult"],
               "pipeline_seconds": round(total, 4), "driver_calls": len(inside)}
        if inside:
            row["driver_seconds"] = round(driver, 4)
            row["vkd3d_seconds"] = round(total - driver, 4)
            row["driver_peak_mib"] = round(max(r["hwm1"] - r["rss0"] for r in inside) / 1024, 1)
            if base is not None:
                row["vkd3d_peak_mib"] = round((inside[0]["hwm0"] - base) / 1024, 1)
            row["driver_failed"] = any(r["result"] != 0 for r in inside)
        rows.append(row)
    for request in results["unfinished"]:
        rows.append({"request": request, "host": None, "hresult": "unfinished"})
    return rows


def process_states(root, name):
    """The states ("R", "S", "Z", ...) of `root`'s descendants whose command name starts with `name`."""
    from .host import _descendant_pids
    states = []
    for pid in _descendant_pids(root):
        try:
            raw = Path(f"/proc/{pid}/stat").read_text()
        except OSError:
            continue
        comm = raw[raw.find("(") + 1:raw.rfind(")")]
        if comm.startswith(name):
            states.append(raw[raw.rfind(")") + 2:].split()[0])
    return states


def guest_gone(states, seen):
    """Whether a guest seen before has exited: no process left, or only zombies.
    Wine's other processes outlive it, so the sandbox does not end by itself."""
    return seen and all(state == "Z" for state in states)


# ---- the table ---------------------------------------------------------------------------------

def seconds(value):
    if value is None:
        return "–"
    return f"{value:.3f}" if value < 1 else f"{value:.2f}" if value < 10 else f"{value:.0f}"


def mib(value):
    return "–" if value is None else f"{value:.0f}"


def kib(value):
    return "–" if value is None else f"{value / 1024:.0f}"


def render(report):
    cards = report.get("cards", [])
    lines = ["# Per-stage baseline", "",
             f"Sample `{report['sample']}` ({report['requests']} requests, {len(report['cpu'])} translations), "
             f"translator at `{report['head']}`{' with uncommitted changes' if report.get('dirty') else ''}, "
             f"DXC {report.get('dxc_version', 'unknown')}.", "",
             f"Host stages ran {report['jobs']} at a time; the load average was {report.get('load', '?')} at the start.",
             "Times are wall seconds; memory is each tool's peak resident size in MiB. "
             "DXC counts every attempt: a timed-out optimised compile, then `-O0`.", ""]
    if report.get("missing"):
        lines += [f"{len(report['missing'])} sample requests are not in this request folder and were not measured.", ""]
    for card in cards:
        skipped = card.get("skipped", {})
        unfinished = [row["request"] for row in card["rows"] if row.get("hresult") == "unfinished"]
        failed = [row for row in card["rows"] if row.get("hresult", "0x00000000") not in ("unfinished", "0x00000000")]
        notes = [f"driver {card.get('driver') or 'unknown'}", f"waves {card.get('waves', '?')}",
                 f"{len(card['rows']) - len(unfinished)} pipelines"]
        if skipped:
            reasons = sorted({reason.split()[0] for reason in skipped.values()})
            notes.append(f"{len(skipped)} skipped ({', '.join(reasons)}: no translation this device can run)")
        if failed:
            notes.append(f"{len(failed)} failed")
        if unfinished:
            notes.append(f"{len(unfinished)} did not finish (crash or timeout)")
        if card.get("error"):
            notes.append(f"stopped: {card['error']}")
        lines.append(f"- **{card['name']}** `{card['id']}`: " + ", ".join(notes))
    missing = [vendor for vendor, prefix in (("AMD", "1002"), ("NVIDIA", "10de"))
               if not any(card["id"].startswith(prefix) for card in cards)]
    if missing:
        lines.append(f"- **GPU stages not run on {' and '.join(missing)}.**")
    lines.append("")
    lines += summary_table(report), ""
    head = ["Request", "Kind", "Host", "HLSL KiB", "DXIL KiB", "xshader s", "MiB", "DXC s", "MiB", "tries",
            "DXV s", "MiB"]
    for card in cards:
        head += [f"{card['name']} vkd3d s", "MiB", f"{card['name']} driver s", "MiB"]
    head.append("Game log s")
    lines += ["## Each translation", "", "| " + " | ".join(head) + " |", "|" + "---|" * 3 + "---:|" * (len(head) - 3)]
    gpu = {(card["name"], row["request"], row["host"]): row for card in cards for row in card["rows"]}
    for row in sorted(report["cpu"], key=lambda r: (r.get("kind", ""), r.get("hlsl_bytes") or 0, r["request"], r["host"])):
        stages = row.get("stages", {})
        cells = [f"`{row['request'][:12]}`", row.get("kind", "?"), row["host"], kib(row.get("hlsl_bytes")),
                 kib(row.get("dxil_bytes"))]
        for tool in TOOLS:
            stage = stages.get(tool, {})
            cells += [seconds(stage.get("seconds")), mib(stage.get("peak_mib"))]
            if tool == "dxc":
                cells.append(str(stage.get("runs", 0)) + (f" ({stage['killed']} timed out)" if stage.get("killed") else ""))
        if row["status"] != "compiled":
            cells[-1] += f" {row['status']}"
        for card in cards:
            g = gpu.get((card["name"], row["request"], row["host"]), {})
            cells += [seconds(g.get("vkd3d_seconds")), mib(g.get("vkd3d_peak_mib")),
                      seconds(g.get("driver_seconds")), mib(g.get("driver_peak_mib"))]
            if g.get("hresult") not in (None, "0x00000000"):
                cells[-1] += f" {g['hresult']}"
        cells.append(str(row["logged_seconds"]) if row.get("logged_seconds") else "")
        lines.append("| " + " | ".join(cells) + " |")
    lines += ["", "## How it was measured", "",
              "- **Measured:** xshader, DXC and DXV wall time, CPU time and peak resident memory (wait4), run "
              "through the preparation code itself in a fresh cache, so the first boot's DXC fallbacks are included.",
              "- **Measured:** each `CreateComputePipelineState` on vkd3d-proton, and inside it each "
              "`vkCreateComputePipelines` (the driver), with the driver's disk cache off. vkd3d s is the call less "
              "the driver; it includes vkd3d-proton's own bookkeeping as well as DXIL to SPIR-V.",
              "- **Measured, process-wide:** GPU memory columns are VmHWM rises: for vkd3d from the resident size "
              "after the last pipeline was released, for the driver from its entry.",
              "- **Not the game's path:** the root signature is built from the program's own bindings, in "
              "descriptor tables, not the title's root through the TuxBox provider; that can change the SPIR-V "
              "vkd3d-proton writes. Programs run one at a time on an idle device, not four large ones at once "
              "under a running title, so the logged in-game times are higher.",
              "- **Game log s** is the longest `CreatePipelineState` the provider logged for the program in NVIDIA "
              "game runs, with other compiles running beside it; it is a reason the program is in the sample, not a "
              "measurement of this run.",
              "- **Not covered:** graphics pipelines on the GPU (vertex, pixel and mesh programs have host stages only).",
              ""]
    return "\n".join(lines)


def summary_table(report):
    """Medians and maxima per kind and size, the shape of the whole sample."""
    cards = report.get("cards", [])
    gpu = defaultdict(dict)
    for card in cards:
        for row in card["rows"]:
            gpu[row["request"], row["host"]][card["name"]] = row
    groups = defaultdict(list)
    for row in report["cpu"]:
        if row["status"] == "compiled":
            groups[row.get("kind", "?"), bucket(row.get("hlsl_bytes") or 0)].append(row)
    head = ["Kind", "HLSL", "n", "xshader s", "DXC s", "DXV s"]
    for card in cards:
        head += [f"{card['name']} vkd3d s", f"{card['name']} driver s"]
    lines = ["## Median (max) per kind and HLSL size", "", "| " + " | ".join(head) + " |",
             "|" + "---|" * 2 + "---:|" * (len(head) - 2)]
    order = [label for _, label in BUCKETS]
    for (kind, label), rows in sorted(groups.items(), key=lambda item: (item[0][0], order.index(item[0][1]))):
        cells = [kind, label, str(len(rows))]
        for tool in TOOLS:
            cells.append(spread([r["stages"].get(tool, {}).get("seconds") for r in rows]))
        for card in cards:
            mine = [gpu[r["request"], r["host"]].get(card["name"], {}) for r in rows]
            cells += [spread([g.get("vkd3d_seconds") for g in mine]), spread([g.get("driver_seconds") for g in mine])]
        lines.append("| " + " | ".join(cells) + " |")
    return "\n".join(lines)


def spread(values):
    values = [value for value in values if value is not None]
    if not values:
        return "–"
    return f"{seconds(statistics.median(values))} ({seconds(max(values))})"
