"""The shader harness sorts differences correctly and blocks the ones that matter."""
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest

from project_tuxbox import shader_survey as survey
import tools.shader_harness as harness

# A stand-in translator: the request's text says what to answer, per host.
FAKE = """#!/usr/bin/env python3
import json, sys
args = sys.argv[1:]
spec = json.loads(open(args[0], "rb").read().split(b"\\n", 1)[1])
answer = spec["wave32" if "--wave32-host" in args else "wave64"]
if "--stride-table" in args:
    answer = spec.get("stride", answer)
out = args[args.index("--hlsl-out") + 1]
profile = args[args.index("--profile-out") + 1]
if answer.startswith("refuse "):
    print(answer[7:], file=sys.stderr)
    raise SystemExit(65)
if answer == "crash":
    raise SystemExit(101)
open(out, "w").write("[WaveSize(32)]\\n    // 0x0004 " + answer + "\\n")
open(profile, "w").write("cs_6_6")
"""


def row(request, status, host="wave64", **fields):
    if status == "translated":
        fields = {"hlsl_sha256": "a", "profile": "cs_6_6", **fields}
    return {"request": request, "host": host, "status": status, **fields}


class Compare(unittest.TestCase):
    def test_every_kind_of_difference_is_named(self):
        before = [row("kept", "translated"), row("lost", "translated"), row("won", "refused", reason="x"),
                  row("moved", "translated"), row("died", "refused", reason="x"),
                  row("reworded", "refused", reason="old")]
        after = [row("kept", "translated"), row("lost", "refused", reason="no s_load at 0x10"),
                 row("won", "translated"), row("moved", "translated", hlsl_sha256="b"),
                 row("died", "failed", reason="panic"), row("reworded", "refused", reason="new")]
        result = survey.compare(before, after)
        self.assertEqual(result["hosts"]["wave64"], {
            "recovered": ["won"], "regressed": ["lost"], "new_failure": ["died"],
            "changed_output": ["moved"], "changed_reason": ["reworded"]})
        self.assertEqual(survey.blocking(result), ["wave64: 1 regressed", "wave64: 1 new failure"])
        self.assertEqual(result["reasons"]["regressed"], [("no s_load at N", 1)])

    def test_a_refactor_blocks_any_new_output(self):
        result = survey.compare([row("moved", "translated")], [row("moved", "translated", hlsl_sha256="b")])
        self.assertEqual(survey.blocking(result), [])
        self.assertEqual(survey.blocking(result, identical=True), ["wave64: 1 changed output"])

    def test_one_host_can_regress_alone(self):
        before = [row("a", "translated"), row("a", "translated", "wave32")]
        after = [row("a", "translated"), row("a", "refused", "wave32", reason="r")]
        self.assertEqual(survey.blocking(survey.compare(before, after)), ["wave32: 1 regressed"])

    def test_different_request_sets_are_refused(self):
        with self.assertRaises(ValueError):
            survey.compare([row("a", "translated")], [row("b", "translated")])


@unittest.skipIf(sys.platform == "win32", "the stand-in translator is a script")
class Survey(unittest.TestCase):
    def setUp(self):
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.requests = self.root / "requests"
        self.requests.mkdir()
        self.translator = self.root / "xshader"
        self.translator.write_text(FAKE)
        self.translator.chmod(0o700)

    def request(self, name, **answers):
        (self.requests / name).write_bytes(b"x\n" + json.dumps(answers).encode())
        return self.requests / name

    def test_outcomes_hold_no_shader_text(self):
        good = self.request("a.dxbc", wave64="v_mov_b32", wave32="refuse no wave at 0x20")
        done = survey.probe(self.translator, good, "wave64")
        self.assertEqual((done["status"], done["wave_size"], done["mnemonics"]), ("translated", 32, ["v_mov_b32"]))
        self.assertNotIn("hlsl", done)
        refused = survey.probe(self.translator, good, "wave32")
        self.assertEqual(refused["status"], "refused")
        crash = self.request("b.dxbc", wave64="crash", wave32="crash")
        self.assertEqual(survey.probe(self.translator, crash, "wave64")["status"], "failed")

    def test_the_stride_table_retry_matches_the_runtime(self):
        source = self.request("c.dxbc", wave64="refuse raw buffer IDXEN needs a stride",
                              wave32="refuse raw buffer IDXEN needs a stride", stride="s_mov_b32")
        for host in survey.HOSTS:
            done = survey.probe(self.translator, source, host)
            self.assertEqual((done["status"], done["stride_table"]), ("translated", True), host)

    def test_a_recorded_wave32_request_is_not_translated_twice(self):
        self.request("d.dxbc", wave64="x", wave32="x")
        self.request("d_w32.wave32", wave64="x", wave32="x")
        self.request("e_w32.wave32", wave64="x", wave32="x")
        planned = [(source.name, host) for source, host in survey.plan(self.requests)]
        self.assertEqual(planned, [("d.dxbc", "wave64"), ("e_w32.wave32", "wave32")])


class Report(unittest.TestCase):
    def test_coverage_ranks_untested_instructions_by_use(self):
        rows = [row("a", "translated", mnemonics=["s_mov_b32", "v_rare"]),
                row("b", "translated", mnemonics=["s_mov_b32", "v_common"]),
                row("c", "translated", mnemonics=["v_common"]),
                row("d", "refused", reason="lane op at 0x40: x")]
        found = survey.coverage(rows, {"s_mov_b32", "v_unused"})
        self.assertEqual((found["used"], found["tested"]), (3, 1))
        self.assertEqual(found["untested"], [("v_common", 2), ("v_rare", 1)])
        self.assertEqual(found["refusals"], [("lane op at N: x", 1)])

    def test_a_missing_card_is_stated(self):
        text = harness.render({"baseline": "a", "head": "b", "dirty": False, "problems": [],
                               "gpu": [{"name": "n", "id": "10de:0001", "checks": 1, "failed": []}]})
        self.assertIn("Still missing: AMD.", text)
        self.assertIn("Shader harness: PASS", text)

    def test_executed_shaders_are_reported_per_card(self):
        kinds = {"same": 50, "equivalent": 3, "changed": 2, "unstable": 1, "lost": 0, "gained": 1, "neither": 40}
        card = {"name": "n", "adapter": "10de:0001", "host": "wave32", "sample": 97, "ran": {"before": 54, "after": 55},
                "wrote_nothing": 7, "kinds": kinds, "reasons": [("skipped: indexes the descriptor heap directly", 40)]}
        text = harness.render({"baseline": "a", "head": "b", "dirty": False, "problems": ["n: 2 changed"],
                               "gpu": [], "execute": [card, {**card, "name": "m"}], "cross_adapter": ["r"]})
        self.assertIn("| n (`10de:0001`, wave32) | 97 | 54 | 55 | 50 | 3 | 2 | 1 | 0 | 1 |", text)
        self.assertIn("7 of the 55 programs that ran", text)
        self.assertIn("| 40 | skipped: indexes the descriptor heap directly |", text)
        self.assertIn("**Unstable**", text)
        self.assertIn("1 executed requests differ between the cards", text)

    def test_the_report_gives_hlsl_size_and_translation_time_per_host(self):
        rows = [row("a", "translated", hlsl_bytes=1 << 20, seconds=1.5),
                row("b", "translated", profile="ps_6_0", hlsl_bytes=1 << 19, seconds=0.25),
                row("c", "refused", seconds=9.0),
                row("a", "translated", "wave32", hlsl_bytes=3 << 20, seconds=2.0)]
        sizes = survey.size_and_time(rows)
        self.assertEqual(sizes["wave64"], {"hlsl_bytes": 3 << 19, "largest_bytes": 1 << 20, "seconds": 1.8,
                                           "slowest_seconds": 1.5})
        text = harness.render({"baseline": "a", "head": "b", "dirty": False, "problems": [],
                               "sizes": {"before": sizes, "after": survey.size_and_time(rows[:1])}})
        self.assertIn("| wave64 | 1.5 | 1.0 | 1024 | 1024 | 1.8 | 1.5 | 1.50 | 1.50 |", text)
        self.assertIn("| wave32 | 3.0 | 0.0 | 3072 | 0 | 2.0 | 0 | 2.00 | 0.00 |", text)


if __name__ == "__main__":
    unittest.main()
