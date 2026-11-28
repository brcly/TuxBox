#!/usr/bin/env python3
"""Test a change to the shader translator against recorded shaders and real GPUs.

`run` builds a baseline and the working tree, then:

  1. runs the code checks;
  2. translates every recorded request for 64-lane and 32-lane hosts with both
     builds, and sorts the differences;
  3. translates a sample twice, to catch output that changes between runs;
  4. compiles changed and recovered programs through DXC and DXV;
  5. runs the translator's GPU checks and the shader Lanes suites on each card;
  6. runs a sample of recorded compute shaders, as translated by both builds,
     over the same generated resources on each card and compares their outputs;
  7. reports instructions the recorded shaders use that no GPU check runs.

The other commands run one stage. Paths come from a JSON file (`--config`,
default build/shader-harness.json); see docs/SHADER_HARNESS.md. Everything is
written under the run folder, and no shader bytes or HLSL are kept.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import random
import shutil
import subprocess
import sys
import tarfile
from tempfile import TemporaryDirectory
from time import monotonic, strftime

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from project_tuxbox import instruction_inventory as inventory  # noqa: E402
from project_tuxbox import shader_survey as survey  # noqa: E402
from tools.compare_scarlett_shaders import compile_one  # noqa: E402
from tools import execute_recorded_shaders as executed  # noqa: E402

TARGET = "x86_64-unknown-linux-gnu"
LANES = ("lanes-shader-cache", "lanes-raster-tables", "lanes-compute-root",
         "lanes-compute-root-recovery", "lanes-bindless-compute")
VENDORS = {"10de": "NVIDIA", "1002": "AMD", "8086": "Intel"}


def say(text):
    print(f"[{strftime('%H:%M:%S')}] {text}", flush=True)


def load_config(path):
    config = json.loads(Path(path).read_text()) if Path(path).is_file() else {}
    for key in ("requests", "dxc", "proton", "windows_dxc"):
        if key in config:
            config[key] = str(Path(config[key]).expanduser())
    return config


def need(config, *keys):
    missing = [key for key in keys if not config.get(key)]
    if missing:
        raise SystemExit(f"set {', '.join(missing)} in the config file; see docs/SHADER_HARNESS.md")


def build_translator(source, output):
    """Build xshader from the tree at `source` and copy it to `output`."""
    target = ROOT / "build/shader-harness/cargo"
    subprocess.run(["cargo", "build", "--release", "--locked", "--quiet", "--target", TARGET,
                    "--manifest-path", str(source / "tools/shader-compiler/Cargo.toml"),
                    "--target-dir", str(target)], check=True)
    shutil.copy2(target / TARGET / "release/xshader", output)
    return output


def build_ref(ref, output):
    """Build the translator as it was at a git ref, from an exported copy of the tree."""
    with TemporaryDirectory(prefix="tuxbox-baseline-") as tmp:
        archive = Path(tmp) / "tree.tar"
        subprocess.run(["git", "-C", str(ROOT), "archive", "-o", str(archive), ref], check=True)
        tree = Path(tmp) / "tree"
        with tarfile.open(archive) as stream:
            stream.extractall(tree, filter="data")
        return build_translator(tree, output)


def run_survey(xshader, config, output, jobs):
    pairs = survey.plan(config["requests"])
    if not pairs:
        raise SystemExit("no recorded requests found")
    started = monotonic()
    rows = survey.survey(xshader, pairs, jobs, progress=say)
    survey.save(rows, output)
    say(f"translated {len(rows)} in {monotonic() - started:.0f}s: {json.dumps(survey.totals(rows))}")
    return rows


def determinism(xshader, rows, config, count, jobs):
    """Translate a sample again; return requests whose outcome moved."""
    pairs = survey.plan(config["requests"])
    chosen = random.Random(0).sample(pairs, min(count, len(pairs)))
    first = {(row["request"], row["host"]): row for row in rows}
    moved = []
    for row in survey.survey(xshader, chosen, jobs):
        before = first[row["request"], row["host"]]
        keys = ("status", "reason", "hlsl_sha256", "profile")
        if any(before.get(key) != row.get(key) for key in keys):
            moved.append(f"{row['request']} ({row['host']})")
    return moved


def select_compiles(comparison, limit):
    changed = [(name, host, True) for host, kinds in comparison["hosts"].items()
               for name in kinds["changed_output"]]
    recovered = [(name, host, False) for host, kinds in comparison["hosts"].items()
                 for name in kinds["recovered"]]
    # Every recovered program is new to DXC; changed ones are sampled evenly.
    pick = random.Random(0)
    pick.shuffle(recovered)
    pick.shuffle(changed)
    if limit:
        recovered = recovered[:limit]
        changed = changed[:limit]
    return recovered + changed


def compile_pair(item, config, tools):
    name, host, paired = item
    source = Path(config["requests"]) / name
    result = {"request": name, "host": host}
    with TemporaryDirectory(prefix="tuxbox-harness-input-") as tmp:
        if host == "wave32" and source.suffix == ".dxbc":
            asked = Path(tmp) / f"{source.stem}_w32.wave32"
            shutil.copyfile(source, asked)
            source = asked
        for side in (("before", "after") if paired else ("after",)):
            started = monotonic()
            result[side] = compile_one(source, tools[side])
            result[side]["seconds"] = round(monotonic() - started, 2)
    return result


def run_compiles(comparison, builds, config, limit, jobs):
    dxc = Path(config["dxc"])
    tools = {side: (build, dxc, dxc.with_name("dxv")) for side, build in builds.items()}
    chosen = select_compiles(comparison, limit)
    say(f"compiling {len(chosen)} programs through DXC and DXV")
    with ThreadPoolExecutor(jobs or max(1, (os.cpu_count() or 2) // 2)) as pool:
        rows = list(pool.map(lambda item: compile_pair(item, config, tools), chosen))
    broken = [row for row in rows if row["after"]["status"] != "compiled"
              and row.get("before", {"status": "compiled"})["status"] == "compiled"]
    both = [row for row in rows if "before" in row
            and row["before"]["status"] == row["after"]["status"] == "compiled"]
    return {"compiled": sum(row["after"]["status"] == "compiled" for row in rows), "selected": len(rows),
            "broken": broken,
            "seconds_before": round(sum(row["before"]["seconds"] for row in both), 1),
            "seconds_after": round(sum(row["after"]["seconds"] for row in both), 1),
            "timed_pairs": len(both)}


def run_logged(command, log, env):
    with open(log, "w") as stream:
        return subprocess.run(command, cwd=ROOT, stdout=stream, stderr=subprocess.STDOUT, env=env).returncode


def run_gpu(adapter, config, folder, *, lanes=True):
    """The translator checks and shader Lanes suites on one card. Expects a built tree."""
    folder.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, **adapter.get("env", {}), "DXC": config["dxc"],
           "TUXBOX_XSHADER_COVERAGE": str(folder / "coverage.txt")}
    host = ["--proton", config["proton"], "--prefix", str(Path(adapter["prefix"]).expanduser()),
            "--adapter", adapter["id"]]
    work = folder / "translator-work"
    shutil.rmtree(work, ignore_errors=True)
    say(f"{adapter['name']}: translator GPU checks")
    run_logged([sys.executable, "tools/check_translator_gpu.py", "--provider", "build/d3d12-smoke/d3d12_xs.dll",
                "--dxc", config["dxc"], "--compiler-dir", config["windows_dxc"], "--work", str(work), *host],
               folder / "translator.log", env)
    summary = work / "summary.json"
    checks = json.loads(summary.read_text())["checks"] if summary.is_file() else []
    result = {"name": adapter["name"], "id": adapter["id"], "checks": len(checks),
              "failed": [row["check"] for row in checks if not row["passed"]]}
    if not checks:
        result["failed"] = ["translator checks did not run (see translator.log)"]
    if lanes:
        say(f"{adapter['name']}: shader Lanes suites")
        command = [sys.executable, "tuxbox.py", "verify", "--backend", "proton", *host,
                   "--output", str(folder / "lanes.json")]
        for suite in LANES:
            command += ["--only", suite]
        (folder / "lanes.json").unlink(missing_ok=True)
        if run_logged(command, folder / "lanes.log", env) != 0:
            result["failed"].append("Lanes suites (see lanes.log)")
        result["lanes"] = True
    return result


def run_execute(builds, config, adapter, folder, args):
    """The execute stage on one card."""
    say(f"{adapter['name']}: running {args.execute_sample} recorded compute shaders with both builds")
    env = {**adapter.get("env", {})}
    saved = {key: os.environ.get(key) for key in env}
    os.environ.update(env)
    try:
        host = {"proton": Path(config["proton"]).resolve(), "prefix": Path(adapter["prefix"]).expanduser().resolve(),
                "compiler_dir": Path(config["windows_dxc"]).resolve()}
        report = executed.execute_stage(builds, config["requests"], config["dxc"], host, folder,
                                        sample=args.execute_sample, seed=args.execute_seed, jobs=args.jobs)
    finally:
        for key, value in saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
    report["name"] = adapter["name"]
    kinds = report["kinds"]
    say(f"{adapter['name']}: {report['ran']['after']} of {report['sample']} ran; {kinds['same']} same, "
        f"{kinds['equivalent']} equivalent, "
        f"{kinds['changed']} changed, {kinds['unstable']} unstable, {kinds['lost']} lost, {kinds['gained']} gained")
    return report


def exercised(folders):
    found = survey.exercised_mnemonics(folders)
    for folder in folders:
        for record in Path(folder).rglob("coverage.txt"):
            found.update(record.read_text().split())
    return found


def render(report):
    """The report a person reads. Request names stay in the JSON files."""
    lines = [f"# Shader harness: {'FAIL' if report['problems'] else 'PASS'}", "",
             f"Baseline `{report['baseline']}`, candidate: the working tree at `{report['head']}`"
             f"{' with uncommitted changes' if report['dirty'] else ''}.", ""]
    lines += ["## Blocking problems", ""] + ([f"- {p}" for p in report["problems"]] or ["None."]) + [""]
    comparison = report.get("comparison")
    if comparison:
        lines += ["## Recorded shaders", "",
                  "| Host | Translated before | after | Recovered | Regressed | New failures | Output changed | Reason changed |",
                  "|---|---:|---:|---:|---:|---:|---:|---:|"]
        for host, kinds in comparison["hosts"].items():
            cells = [host, comparison["before"][host].get("translated", 0), comparison["after"][host].get("translated", 0),
                     *(len(kinds[k]) for k in ("recovered", "regressed", "new_failure", "changed_output", "changed_reason"))]
            lines.append("| " + " | ".join(map(str, cells)) + " |")
        for kind, title in (("regressed", "Refusals now hit"), ("new_failure", "Failures"),
                            ("recovered", "Refusals no longer hit")):
            if comparison["reasons"][kind]:
                lines += ["", f"{title}:", ""] + [f"- {n} × {reason}" for reason, n in comparison["reasons"][kind][:8]]
        lines.append("")
    if "sizes" in report:
        before, after = report["sizes"]["before"], report["sizes"]["after"]
        lines += ["Size and time of the translations, run in parallel (time is indicative only):", "",
                  "| Host | HLSL MiB before | after | Largest KiB before | after | Translation s before | after "
                  "| Slowest s before | after |", "|---|---:|---:|---:|---:|---:|---:|---:|---:|"]
        for host in before:
            a, b = before[host], after[host]
            cells = [host, f"{a['hlsl_bytes'] / (1 << 20):.1f}", f"{b['hlsl_bytes'] / (1 << 20):.1f}",
                     f"{a['largest_bytes'] >> 10}", f"{b['largest_bytes'] >> 10}", a["seconds"], b["seconds"],
                     f"{a['slowest_seconds']:.2f}", f"{b['slowest_seconds']:.2f}"]
            lines.append("| " + " | ".join(map(str, cells)) + " |")
        lines.append("")
    if "compile" in report:
        c = report["compile"]
        lines += ["## DXC and DXV", "", f"{c['compiled']} of {c['selected']} changed or recovered programs compiled; "
                  f"{len(c['broken'])} broke."]
        if c["timed_pairs"]:
            lines.append(f"Compile time over {c['timed_pairs']} pairs run under parallel load (indicative only): "
                         f"{c['seconds_before']} s before, {c['seconds_after']} s after.")
        lines.append("")
    lines += ["## GPU checks", ""]
    for card in report.get("gpu", []):
        state = f"failed: {', '.join(card['failed'])}" if card["failed"] else "passed"
        lines.append(f"- {card['name']} (`{card['id']}`): {card['checks']} translator checks"
                     f"{' and Lanes suites' if card.get('lanes') else ''}, {state}")
    if not report.get("gpu"):
        lines.append("- Not run.")
    ran = {VENDORS.get(card["id"].split(":")[0], "other") for card in report.get("gpu", [])}
    ran |= {VENDORS.get(card["adapter"].split(":")[0], "other") for card in report.get("execute", [])}
    absent = [vendor for vendor in ("AMD", "NVIDIA") if vendor not in ran]
    if absent:
        lines.append(f"- **Still missing: {' and '.join(absent)}.**")
    if report.get("execute"):
        lines += ["", "## Recorded shaders executed", "",
                  "Each request's two translations ran over the same generated resources; equal outputs mean the "
                  "change didn't alter what it computes for those inputs. *Equivalent* translations changed and "
                  "wrote the same; *unstable* ones didn't repeat their own results when the baseline ran twice.", "",
                  "| Card | Sample | Ran before | after | Same | Equivalent | Changed | Unstable | Lost | Gained |",
                  "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
        for card in report["execute"]:
            k = card["kinds"]
            lines.append(f"| {card['name']} (`{card['adapter']}`, {card['host']}) | {card['sample']} | "
                         f"{card['ran']['before']} | {card['ran']['after']} | {k['same']} | {k['equivalent']} | {k['changed']} | "
                         f"{k['unstable']} | {k['lost']} | {k['gained']} |")
        card = report["execute"][0]
        lines += ["", f"On {card['name']}, {card['wrote_nothing']} of the {card['ran']['after']} programs that ran left "
                  "every writable resource as seeded, so they show little for these inputs."]
        if card["reasons"]:
            lines += ["", "Why requests did not run (candidate, first card):", "", "| Requests | Reason |", "|---:|---|"]
            lines += [f"| {count} | {reason} |" for reason, count in card["reasons"][:10]]
        if any(card["kinds"]["unstable"] for card in report["execute"]):
            lines += ["", "**Unstable** programs wrote different results, or none, in two runs of the baseline "
                      "translation over the same inputs: a race, an uninitialised read, a driver fault or a loaded "
                      "host. They don't block a change."]
        if report.get("cross_adapter") is not None:
            lines += ["", f"{len(report['cross_adapter'])} executed requests differ between the cards "
                      "(reported, not blocking)."]
    if "coverage" in report:
        c = report["coverage"]
        lines += ["", "## Instruction coverage", "",
                  f"Recorded shaders use {c['used']} instructions; GPU check programs hold {c['tested']} of them. "
                  "Holding one does not prove every operand form or lane state was tested.",
                  "", "Most used without a GPU check:", "", "| Instruction | Shaders |", "|---|---:|"]
        lines += [f"| `{name}` | {count} |" for name, count in c["untested"][:25]]
        lines += ["", "## Largest remaining refusals", "", "| Translations | Reason |", "|---:|---|"]
        lines += [f"| {count} | {reason} |" for reason, count in c["refusals"][:15]]
    lines += ["", "## Not covered", "",
              "- Executed shaders run over generated inputs on a small grid, not the title's own; their outputs are "
              "compared between builds and cards, not with a console.",
              "- Programs that index the descriptor heap directly or read a ray tracing scene are not executed.",
              "- Drivers' own pipeline builds are not run or timed here; `tools/stage_baseline.py` times them "
              "for a fixed sample.", ""]
    return "\n".join(lines)


def record_inventory(run, config):
    """Save what a finished run measured as evidence, and regenerate the inventory."""
    report = json.loads((run / "report.json").read_text())
    if report["problems"] or report["dirty"]:
        raise SystemExit("evidence comes from a passing run of a committed tree")
    # The evidence must describe the translator that is checked out now.
    source = "tools/shader-compiler"
    translator = git("rev-parse", f"HEAD:{source}")
    if git("rev-parse", f"{report['head']}:{source}") != translator or git("status", "--porcelain", "--", source):
        raise SystemExit("the translator has changed since that run; run the harness again")
    adapters = {}
    for card in report["gpu"]:
        names = set((run / "gpu" / card["name"] / "coverage.txt").read_text().split())
        vendor = VENDORS.get(card["id"].split(":")[0], card["name"])
        adapters[f"{vendor} {card['id']}"] = {"checks": card["checks"], "instructions": sorted(names)}
    if not adapters:
        raise SystemExit("the run has no GPU stage")
    need(config, "title")
    counts = {}
    rows = survey.load(run / "survey-after.jsonl")
    for row in rows:
        for name in row.get("mnemonics", ()):
            # `tb_` names are the translator's own lowered operations.
            if not name.startswith("tb_"):
                counts[name] = counts.get(name, 0) + 1
    scope = "Instruction names only; no shader bytes or translated text"
    inventory.EVIDENCE.mkdir(parents=True, exist_ok=True)
    (inventory.EVIDENCE / "gpu-checks.json").write_text(json.dumps(
        {"scope": f"Instructions in the programs the translator GPU checks ran. {scope}",
         "translator_tree": translator, "adapters": adapters}, indent=1) + "\n")
    (inventory.EVIDENCE / "recorded-use.json").write_text(json.dumps(
        {"scope": f"Translated shader requests that hold each instruction, 64-lane and 32-lane translations counted "
                  f"separately. {scope}", "title": config["title"], "translator_tree": translator,
         "translations": sum(row["status"] == "translated" for row in rows),
         "instructions": dict(sorted(counts.items()))}, indent=1) + "\n")
    write_inventory()


def compile_inventory(config):
    """Compile every instruction the translator emits alone through DXC and DXV, and save which do."""
    need(config, "dxc")
    source = "tools/shader-compiler"
    translator = git("rev-parse", f"HEAD:{source}")
    if git("status", "--porcelain", "--", source):
        raise SystemExit("commit the translator first, so the evidence names the tree it describes")
    dxc = Path(config["dxc"])
    with TemporaryDirectory(prefix="tuxbox-instructions-") as tmp:
        folder = Path(tmp)
        say("translating every instruction alone")
        subprocess.run(["cargo", "test", "--quiet", "--locked", "--target", TARGET, "--manifest-path",
                        str(ROOT / source / "Cargo.toml"), "--target-dir", str(ROOT / "build/shader-harness/cargo"),
                        "instruction_inventory"], check=True, capture_output=True,
                       env={**os.environ, "TUXBOX_INSTRUCTION_HLSL": str(folder)})
        programs = sorted(folder.glob("*.hlsl"))
        say(f"compiling {len(programs)} translations through DXC and DXV")

        def build(hlsl):
            blob = hlsl.with_suffix(".cso")
            for command in ([dxc, "-T", hlsl.with_suffix(".profile").read_text().strip(), "-E", "main", "-Fo", blob, hlsl],
                            [dxc.with_name("dxv"), blob]):
                run = subprocess.run(command, capture_output=True, text=True, errors="replace", timeout=120)
                if run.returncode:
                    lines = [line for line in (run.stderr or run.stdout).splitlines() if "error" in line]
                    return hlsl.stem, (lines or ["failed"])[0].split("error: ", 1)[-1][:200]
            return hlsl.stem, None

        with ThreadPoolExecutor(max(1, (os.cpu_count() or 2) - 2)) as pool:
            results = dict(pool.map(build, programs))
    version = subprocess.run([dxc, "--version"], capture_output=True, text=True).stdout.strip().splitlines()[0]
    key = lambda stem: " ".join(stem.split("-", 2))
    inventory.EVIDENCE.mkdir(parents=True, exist_ok=True)
    (inventory.EVIDENCE / "dxil.json").write_text(json.dumps(
        {"scope": "Each instruction the translator emits a statement for, translated alone and compiled through "
                  "DXC and DXV. Instruction names only; no shader bytes or translated text",
         "translator_tree": translator, "dxc": version,
         "compiled": sorted(key(stem) for stem, error in results.items() if error is None),
         "failed": {key(stem): error for stem, error in sorted(results.items()) if error}}, indent=1) + "\n")
    failed = sum(error is not None for error in results.values())
    say(f"{len(results) - failed} of {len(results)} compiled to DXIL")


def write_inventory():
    inventory.DOCUMENT.write_text(inventory.current())
    say(f"wrote {inventory.DOCUMENT.relative_to(ROOT)}")


def git(*args):
    return subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, text=True, check=True).stdout.strip()


def full_run(args, config):
    need(config, "requests", "dxc")
    folder = (args.output or ROOT / "build/shader-harness" / strftime("%Y%m%d-%H%M%S")).resolve()
    folder.mkdir(parents=True, exist_ok=False)
    report = {"baseline": git("rev-parse", "--short", args.baseline), "head": git("rev-parse", "--short", "HEAD"),
              "dirty": bool(git("status", "--porcelain")), "problems": []}
    problems = report["problems"]
    say("building the baseline and the working tree")
    builds = {"before": build_ref(args.baseline, folder / "xshader-baseline"),
              "after": build_translator(ROOT, folder / "xshader-candidate")}
    if not args.skip_code_checks:
        say("code checks")
        if run_logged([sys.executable, "tuxbox.py", "check", "--no-gpu"], folder / "code-checks.log", dict(os.environ)):
            problems.append("code checks failed (see code-checks.log)")
    surveys = {}
    for side, build in builds.items():
        say(f"survey: {side}")
        surveys[side] = run_survey(build, config, folder / f"survey-{side}.jsonl", args.jobs)
    comparison = report["comparison"] = survey.compare(surveys["before"], surveys["after"])
    report["sizes"] = {side: survey.size_and_time(rows) for side, rows in surveys.items()}
    problems += survey.blocking(comparison, identical=args.identical)
    moved = determinism(builds["after"], surveys["after"], config, args.repeat, args.jobs)
    if moved:
        problems.append(f"{len(moved)} translations differ between two runs, e.g. {moved[0]}")
    compiled = report["compile"] = run_compiles(comparison, builds, config, args.compile_limit, args.jobs)
    if compiled["broken"]:
        problems.append(f"{len(compiled['broken'])} programs no longer compile through DXC and DXV")
    if args.no_gpu:
        report["gpu"] = []
        if not args.allow_no_gpu:
            problems.append("GPU checks were skipped")
    else:
        need(config, "proton", "windows_dxc", "adapters")
        say("building providers and guests")
        if run_logged([sys.executable, "build.py"], folder / "build.log", {**os.environ, "DXC": config["dxc"]}):
            raise SystemExit("build.py failed; see build.log")
        report["gpu"] = [run_gpu(card, config, folder / "gpu" / card["name"], lanes=not args.no_lanes)
                         for card in config["adapters"]]
        problems += [f"{card['name']}: {name}" for card in report["gpu"] for name in card["failed"]]
        if args.execute_sample:
            report["execute"] = [run_execute(builds, config, card, folder / "execute" / card["name"], args)
                                 for card in config["adapters"]]
            problems += [f"{card['name']}: {p}" for card in report["execute"] for p in card["problems"]]
            if len(report["execute"]) > 1:
                report["cross_adapter"] = executed.cross_adapter(report["execute"])
        report["coverage"] = survey.coverage(surveys["after"], exercised([folder / "gpu"]))
    (folder / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    (folder / "report.md").write_text(render(report))
    say(f"report: {folder / 'report.md'}")
    for problem in problems:
        print(f"BLOCKING  {problem}")
    return int(bool(problems))


def execute_only(args, config):
    need(config, "requests", "dxc", "proton", "windows_dxc", "adapters")
    folder = (args.output or ROOT / "build/shader-harness" / strftime("execute-%Y%m%d-%H%M%S")).resolve()
    folder.mkdir(parents=True, exist_ok=False)
    say("building the baseline and the working tree")
    builds = {"before": args.baseline_xshader or build_ref(args.baseline, folder / "xshader-baseline"),
              "after": args.candidate_xshader or build_translator(ROOT, folder / "xshader-candidate")}
    cards = [card for card in config["adapters"] if not args.adapter or card["name"] == args.adapter]
    if not cards:
        raise SystemExit(f"no configured card is named {args.adapter}")
    report = {"baseline": git("rev-parse", "--short", args.baseline), "head": git("rev-parse", "--short", "HEAD"),
              "dirty": bool(git("status", "--porcelain")), "problems": []}
    report["execute"] = [run_execute(builds, config, card, folder / card["name"], args) for card in cards]
    if len(cards) > 1:
        report["cross_adapter"] = executed.cross_adapter(report["execute"])
    report["problems"] = [f"{card['name']}: {p}" for card in report["execute"] for p in card["problems"]]
    report["gpu"] = []
    (folder / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    (folder / "report.md").write_text(render(report))
    say(f"report: {folder / 'report.md'}")
    for problem in report["problems"]:
        print(f"BLOCKING  {problem}")
    return int(bool(report["problems"]))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", type=Path, default=ROOT / "build/shader-harness.json")
    parser.add_argument("--jobs", type=int, default=0, help="parallel translations (default: cores less two)")
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run", help="every stage, with a report")
    run.add_argument("--baseline", default="origin/main", help="git ref to compare with")
    run.add_argument("--output", type=Path, help="new run folder")
    run.add_argument("--identical", action="store_true", help="a refactor: any change of output blocks")
    run.add_argument("--compile-limit", type=int, default=200,
                     help="at most this many recovered and this many changed programs; 0 for all")
    run.add_argument("--repeat", type=int, default=500, help="translations repeated for determinism")
    run.add_argument("--skip-code-checks", action="store_true")
    run.add_argument("--no-gpu", action="store_true", help="CPU stages only; blocks unless --allow-no-gpu")
    run.add_argument("--allow-no-gpu", action="store_true")
    run.add_argument("--no-lanes", action="store_true", help="translator GPU checks without the Lanes suites")
    run.add_argument("--execute-sample", type=int, default=100,
                     help="recorded compute shaders to run with both builds on each card; 0 skips the stage")
    run.add_argument("--execute-seed", type=int, default=0, help="which sample to run")
    ran = commands.add_parser("execute", help="run a sample of recorded compute shaders with both builds")
    ran.add_argument("--baseline", default="origin/main", help="git ref to compare with")
    ran.add_argument("--output", type=Path, help="new run folder")
    ran.add_argument("--execute-sample", "--sample", type=int, default=100, dest="execute_sample")
    ran.add_argument("--execute-seed", "--seed", type=int, default=0, dest="execute_seed")
    ran.add_argument("--adapter", help="only the configured card with this name")
    ran.add_argument("--baseline-xshader", type=Path, help="a built baseline translator, instead of building --baseline")
    ran.add_argument("--candidate-xshader", type=Path, help="a built candidate translator, instead of the working tree")
    one = commands.add_parser("survey", help="translate every recorded request with one build")
    one.add_argument("--xshader", type=Path, required=True)
    one.add_argument("--output", type=Path, required=True)
    two = commands.add_parser("compare", help="sort the differences between two surveys")
    two.add_argument("before", type=Path)
    two.add_argument("after", type=Path)
    two.add_argument("--identical", action="store_true")
    two.add_argument("--json", type=Path, help="write the request names too")
    listed = commands.add_parser("inventory", help="regenerate docs/INSTRUCTIONS.md")
    listed.add_argument("--from-run", type=Path, help="first save a passing run's coverage and use as evidence")
    listed.add_argument("--compile", action="store_true", help="first compile each instruction's translation to DXIL")
    show = commands.add_parser("explain", help="one request on both hosts: outcome and instructions")
    show.add_argument("--xshader", type=Path, required=True)
    show.add_argument("request", help="request file name, or the start of it")
    args = parser.parse_args(argv)
    config = load_config(args.config)
    if args.command == "run":
        return full_run(args, config)
    if args.command == "execute":
        return execute_only(args, config)
    if args.command == "inventory":
        if args.compile:
            compile_inventory(config)
        if args.from_run:
            record_inventory(args.from_run.resolve(), config)
        else:
            write_inventory()
        return 0
    if args.command == "survey":
        need(config, "requests")
        run_survey(args.xshader, config, args.output, args.jobs)
        return 0
    if args.command == "compare":
        result = survey.compare(survey.load(args.before), survey.load(args.after))
        if args.json:
            args.json.write_text(json.dumps(result, indent=2) + "\n")
        for host, kinds in result["hosts"].items():
            print(host, " ".join(f"{kind}={len(names)}" for kind, names in kinds.items()))
        problems = survey.blocking(result, identical=args.identical)
        for problem in problems:
            print(f"BLOCKING  {problem}")
        return int(bool(problems))
    need(config, "requests")
    matches = [pair for pair in survey.plan(config["requests"]) if pair[0].name.startswith(args.request)]
    if not matches or len({source for source, _ in matches}) > 1:
        raise SystemExit(f"{len({s for s, _ in matches})} requests start with that; give one")
    for source, host in matches:
        row = survey.probe(Path(args.xshader).resolve(), source, host)
        print(json.dumps(row, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
