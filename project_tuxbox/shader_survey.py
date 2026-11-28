"""Translate recorded shader requests for each host wave width, and compare two translator builds.

The results hold request digests, refusal messages, output hashes and instruction
names. No shader bytes or HLSL are kept.
"""
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
from tempfile import TemporaryDirectory
from time import monotonic

from .runtime_shaders import STAGE_IDS, XSHADER_UNSUPPORTED_HLSL, container_stage
from .shader_diagnostics import normalize_refusal

# `wave64` is the ordinary translation, which a host with 64-lane waves runs.
# `wave32` asks again for a host whose waves are too narrow for it.
HOSTS = ("wave64", "wave32")
# Every translated instruction is announced by a comment with its offset.
MNEMONIC = re.compile(r"^\s*// 0x[0-9a-f]+ ([a-z][a-z0-9_]*)", re.MULTILINE)


def jobs_for(requested=0):
    return requested or max(1, (os.cpu_count() or 2) - 2)


def plan(requests):
    """The `(request, host)` pairs to translate, in a stable order."""
    sources = sorted(path for path in Path(requests).iterdir()
                     if path.suffix in (".dxbc", ".link", ".stencil", ".wave32"))
    ordinary = {path.stem for path in sources if path.suffix == ".dxbc"}
    pairs = []
    for source in sources:
        if source.suffix == ".wave32":
            # Covered by the wave32 pass over its own program, when that was recorded.
            if source.stem.removesuffix("_w32") not in ordinary:
                pairs.append((source, "wave32"))
            continue
        pairs.append((source, "wave64"))
        if source.suffix == ".dxbc" and is_compute(source):
            pairs.append((source, "wave32"))
    return pairs


def is_compute(source):
    try:
        return container_stage(source.read_bytes(), portable=False) == STAGE_IDS["cs"]
    except (OSError, ValueError):
        return False


def emitter_flags(source, host):
    if source.suffix == ".link":
        return source / "producer.dxbc", ["--link-pixel", source / "pixel.dxbc", "--pixel-order", source / "pixel.cso"]
    if source.suffix == ".stencil":
        return source, ["--stencil-target", source.stem.rpartition("_rt")[2]]
    return source, ["--wave32-host"] if host == "wave32" else []


def translate(xshader, source, host, work, timeout):
    """One translation as the runtime asks for it, including its stride-table retry."""
    cooked, flags = emitter_flags(source, host)
    hlsl, profile = work / "shader.hlsl", work / "profile"
    command = [xshader, cooked, *flags, "--hlsl-out", hlsl, "--profile-out", profile]
    run = subprocess.run(command, capture_output=True, timeout=timeout)
    retried = False
    if (run.returncode == XSHADER_UNSUPPORTED_HLSL and source.suffix not in (".link", ".stencil")
            and b"raw buffer IDXEN" in run.stderr):
        # Both host widths retry, as preparation does.
        again = subprocess.run([*command, "--stride-table"], capture_output=True, timeout=timeout)
        if again.returncode == 0:
            run, retried = again, True
    return run, hlsl, profile, retried


def probe(xshader, source, host, timeout=120):
    row = {"request": source.name, "host": host}
    started = monotonic()
    with TemporaryDirectory(prefix="tuxbox-survey-") as tmp:
        work = Path(tmp)
        try:
            run, hlsl, profile, retried = translate(xshader, source, host, work, timeout)
        except subprocess.TimeoutExpired:
            return {**row, "status": "failed", "reason": f"timed out after {timeout}s"}
        except OSError as error:
            return {**row, "status": "failed", "reason": normalize_refusal(str(error))}
        row["seconds"] = round(monotonic() - started, 4)
        message = run.stderr.decode("utf-8", "replace").replace(tmp, "WORK")
        if run.returncode == XSHADER_UNSUPPORTED_HLSL:
            return {**row, "status": "refused", "reason": normalize_refusal(message)}
        if run.returncode != 0 or not hlsl.is_file() or not profile.is_file():
            # A panic or a crash: the translator must refuse, never die.
            last = message.strip().splitlines()[-1:] or [f"exit {run.returncode}"]
            return {**row, "status": "failed", "reason": normalize_refusal(last[0])}
        data = hlsl.read_bytes()
        text = data.decode("utf-8", "replace")
        wave = re.search(r"\[WaveSize\((\d+)\)\]", text)
        row.update(status="translated", profile=profile.read_text().strip(),
                   hlsl_sha256=hashlib.sha256(data).hexdigest(), hlsl_bytes=len(data),
                   wave_size=int(wave[1]) if wave else None, stride_table=retried,
                   mnemonics=sorted(set(MNEMONIC.findall(text))))
        return row


def survey(xshader, pairs, jobs=0, progress=None):
    xshader = Path(xshader).resolve()
    rows = []
    with ThreadPoolExecutor(jobs_for(jobs)) as pool:
        for number, row in enumerate(pool.map(lambda pair: probe(xshader, *pair), pairs), 1):
            rows.append(row)
            if progress and number % 5000 == 0:
                progress(f"  {number}/{len(pairs)}")
    return rows


def save(rows, path):
    with open(path, "w", encoding="utf-8") as output:
        output.writelines(json.dumps(row, sort_keys=True) + "\n" for row in rows)


def load(path):
    with open(path, encoding="utf-8") as source:
        return [json.loads(line) for line in source]


def construct(reason):
    """Group refusals that differ only in where they happened."""
    return re.sub(r"0x[0-9a-fA-F_]+|\b\d+\b", "N", reason.split(" [", 1)[0])[:160]


def totals(rows):
    out = {host: Counter() for host in HOSTS}
    for row in rows:
        out[row["host"]][row["status"]] += 1
    return {host: dict(counts) for host, counts in out.items()}


def size_and_time(rows):
    """Per host: HLSL written and seconds spent by the translations that succeeded."""
    out = {}
    for host in HOSTS:
        mine = [row for row in rows if row["host"] == host and row["status"] == "translated"]
        sizes = [row.get("hlsl_bytes", 0) for row in mine]
        times = [row.get("seconds", 0) for row in mine]
        out[host] = {"hlsl_bytes": sum(sizes), "largest_bytes": max(sizes, default=0),
                     "seconds": round(sum(times), 1), "slowest_seconds": max(times, default=0)}
    return out


def compare(before, after):
    """What `after` does differently from `before`, over the same requests."""
    old = {(row["request"], row["host"]): row for row in before}
    new = {(row["request"], row["host"]): row for row in after}
    if old.keys() != new.keys():
        raise ValueError("the two surveys cover different requests; survey both against one folder")
    kinds = ("recovered", "regressed", "new_failure", "changed_output", "changed_reason")
    found = {host: {kind: [] for kind in kinds} for host in HOSTS}
    for key in sorted(old):
        a, b = old[key], new[key]
        name, host = key
        if b["status"] == "failed" and a["status"] != "failed":
            kind = "new_failure"
        elif a["status"] == "translated" and b["status"] != "translated":
            kind = "regressed"
        elif b["status"] == "translated" and a["status"] != "translated":
            kind = "recovered"
        elif b["status"] == "translated":
            same = (a["hlsl_sha256"], a["profile"]) == (b["hlsl_sha256"], b["profile"])
            kind = None if same else "changed_output"
        else:
            kind = None if a.get("reason") == b.get("reason") else "changed_reason"
        if kind:
            found[host][kind].append(name)
    reasons = {}
    for kind, source in (("regressed", new), ("new_failure", new), ("recovered", old)):
        counts = Counter(construct(source[name, host].get("reason", "")) for host in HOSTS
                         for name in found[host][kind])
        reasons[kind] = counts.most_common(20)
    return {"before": totals(before), "after": totals(after), "hosts": found, "reasons": reasons}


def blocking(comparison, *, identical=False):
    """Why a candidate must not pass, as short sentences."""
    problems = []
    for host, kinds in comparison["hosts"].items():
        for kind in ("regressed", "new_failure", *(("changed_output", "recovered") if identical else ())):
            if kinds[kind]:
                problems.append(f"{host}: {len(kinds[kind])} {kind.replace('_', ' ')}")
    return problems


def coverage(rows, exercised):
    """Instructions that translated shaders use and no GPU check runs, and the largest gaps."""
    users = defaultdict(set)
    for row in rows:
        for name in row.get("mnemonics", ()):
            users[name].add(row["request"])
    # `tb_` names are the translator's own lowered operations.
    untested = sorted(((name, len(found)) for name, found in users.items() if name not in exercised),
                      key=lambda item: (-item[1], item[0]))
    gaps = Counter(construct(row["reason"]) for row in rows if row["status"] == "refused")
    return {"used": len(users), "tested": len(set(users) & exercised), "untested": untested,
            "refusals": gaps.most_common()}


def exercised_mnemonics(directories):
    """Instructions in the HLSL that GPU checks left in their work folders."""
    found = set()
    for directory in directories:
        for path in Path(directory).rglob("*.hlsl"):
            found.update(MNEMONIC.findall(path.read_text(errors="replace")))
    return found
