#!/usr/bin/env python3
"""Rank the actionable translator refusals by the pipelines they block.

`rank_pipeline_failures.py` ranks every pipeline failure row. Most rows are
not translator refusals (driver deadlines, driver failures, stand-ins), so the
relative size of the remaining reasons is hard to read off. This tool separates
them: translator refusals first, ranked by pipelines blocked, then the
non-refusal remainder. The refusal reasons beside the runtime cache (kept one
line per unsupported shader) are joined to the pipeline file by request key.

The printed summary holds no paths, request keys or shader data.

Example:
    python tools/refusal_impact.py "$PREFIX/drive_c/users/steamuser/AppData/Local/Temp/tuxbox_shader_cache"
"""
import argparse
from collections import Counter, defaultdict
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from tools.rank_pipeline_failures import (
    HOST_REJECTED, MIXED, NO_ANSWER, NO_REASON, NOT_LISTED, STAND_IN, CONFLICT,
    load_reasons, newest_runtime_cache, rank,
)
from project_tuxbox.scarlett_report import expected_at_preparation, refusal_bucket

# Failure rows that are not translator refusals. Most rows here are driver or
# recording outcomes, not ways the emitter can be made to translate more.
NON_REFUSAL_REASONS = frozenset({
    "driver compile deadline",
    "stand-in after translation (cause not recorded)",
    "translated, then rejected by the driver or runtime",
    "unlisted pipelines (record limit reached)",
    "no translator refusal recorded",
    "translation missing with no recorded reason",
    "mixed translation lookups (causal refusal unknown)",
    "conflicting refusal reasons across runtime caches",
})


class Reason:
    """Minimal stand-in for the emitter refusal the report classifies."""

    reason_known = True

    def __init__(self, message):
        self.message = message

    def __str__(self):
        return self.message


def bucket_of(reason):
    if reason in NON_REFUSAL_REASONS or reason.startswith("driver compile failed"):
        return None
    return refusal_bucket(Reason(reason))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("cache", type=Path, help="the prefix's tuxbox_shader_cache folder")
    parser.add_argument("--run", type=Path, help="a failure file (default: the newest)")
    parser.add_argument("--top", type=int, default=15,
                        help="maximum number of actionable buckets to list (default: 15)")
    parser.add_argument("--runtime-cache", type=Path,
                        help="restrict refusal reasons to a known runtime version directory")
    parser.add_argument("--all-runtime-caches", action="store_true",
                        help="merge every runtime cache, including other compiler builds")
    args = parser.parse_args(argv)
    run = args.run or max((args.cache / "pipeline-failures").glob("*.tsv"),
                          key=lambda path: path.stat().st_mtime, default=None)
    if run is None or not run.is_file():
        parser.error("no pipeline failure file found; run the game first")
    if args.runtime_cache is not None and not args.runtime_cache.is_dir():
        parser.error("--runtime-cache must be a directory")
    if args.runtime_cache is not None and args.all_runtime_caches:
        parser.error("choose one of --runtime-cache and --all-runtime-caches")
    if args.runtime_cache is not None:
        caches = [args.runtime_cache]
    elif args.all_runtime_caches:
        caches = [path for path in (args.cache / "runtime").glob("*") if path.is_dir()]
    else:
        newest = newest_runtime_cache(args.cache)
        caches = [newest] if newest is not None else []

    reasons = load_reasons(caches)
    pipelines, shaders, outcomes = rank(run.read_text(encoding="utf-8").splitlines(), reasons)
    total = sum(pipelines.values())

    buckets = {}
    for reason, count in pipelines.items():
        bucket = bucket_of(reason)
        entry = buckets.setdefault(bucket, {"pipelines": 0, "shaders": set(), "reasons": Counter()})
        entry["pipelines"] += count
        entry["shaders"].add(shaders.get(reason, 0))
        entry["reasons"][reason] += count

    actionable = sorted(((b, e) for b, e in buckets.items()
                         if b is not None and not expected_at_preparation(b)),
                        key=lambda item: -item[1]["pipelines"])
    expected = sorted(((b, e) for b, e in buckets.items()
                       if b is not None and expected_at_preparation(b)),
                      key=lambda item: -item[1]["pipelines"])
    non_refusal = sorted(((b, e) for b, e in buckets.items() if b is None),
                         key=lambda item: -item[1]["pipelines"])

    print(f"{total} pipelines not delivered: "
          + ", ".join(f"{count} {outcome}" for outcome, count in outcomes.most_common()))
    print()
    print("Actionable translator refusals ranked by pipeline impact")
    if not actionable:
        print("  (none)")
    else:
        print("  pipelines  share  shaders   bucket                        reason")
        for bucket, entry in actionable[:args.top]:
            pipelines_blocked = entry["pipelines"]
            print(f"{pipelines_blocked:8d}  {100 * pipelines_blocked / total:5.1f}%  "
                  f"{max(entry['shaders']):7d}  {bucket:35s}  {entry['reasons'].most_common(1)[0][0]}")

    if expected:
        print()
        print("Expected at preparation (not actionable here)")
        for bucket, entry in expected:
            (reason, _), = entry["reasons"].most_common(1)
            print(f"{entry['pipelines']:8d}  {max(entry['shaders']):7d}  {bucket:32s}  {reason[:72]}")

    print()
    print("Not translator refusals")
    for _, entry in non_refusal:
        for reason, count in entry["reasons"].most_common():
            print(f"{count:8d}               {reason}")

    print()
    print("Request-count ranking: tools/triage_scarlett_shaders.py \"$REQUESTS\" "
          "--top N (offline, emitter only; needs the recorded archive)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
