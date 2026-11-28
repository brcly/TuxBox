"""The per-stage baseline picks a fixed sample, times each tool as preparation runs it, and splits GPU time."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory
import time
import unittest
from unittest.mock import patch

from project_tuxbox import shader_survey as survey
from project_tuxbox import stage_baseline as stages

KEY = re.compile(r"[0-9a-f]{64}(_[0-9a-f]{64}){0,2}\.(dxbc|link|wave32|stencil)")


def translated(request, profile, size, host="wave64"):
    return {"request": request, "host": host, "status": "translated", "profile": profile, "hlsl_bytes": size}


def container(parts):
    """A DXBC container holding `parts`, as {name: bytes}."""
    header = 32 + 4 * len(parts)
    offsets, body = [], b""
    for name, data in parts.items():
        offsets.append(header + len(body))
        body += name + struct.pack("<I", len(data)) + data
    size = header + len(body)
    return b"DXBC" + bytes(16) + struct.pack("<III", 1, size, len(parts)) + struct.pack(f"<{len(parts)}I", *offsets) + body


def psv(bindings):
    """PSV0 with a 52-byte runtime record and version-1 binding records."""
    records = b"".join(struct.pack("<6I", kind, space, lower, upper, 0, 0) for kind, space, lower, upper in bindings)
    data = struct.pack("<I", 52) + bytes(52) + struct.pack("<I", len(bindings))
    return data + (struct.pack("<I", 24) + records if bindings else b"")


class Sample(unittest.TestCase):
    def setUp(self):
        self.rows = []
        self.names = []
        for index in range(12):
            for kind, profile, size in (("a", "cs_6_6", 10 << 10), ("b", "ps_6_0", 200 << 10), ("c", "cs_6_6", 2 << 20)):
                name = hashlib.sha256(f"{kind}{index}".encode()).hexdigest() + ".dxbc"
                self.names.append(name)
                self.rows.append(translated(name, profile, size))
                if profile.startswith("cs"):
                    self.rows.append(translated(name, profile, size * 2, "wave32"))

    def test_each_stage_and_size_gives_the_same_requests_whatever_the_order(self):
        first = stages.build_sample(self.rows, self.names, {}, per_stratum=2, tail=3)
        again = stages.build_sample(list(reversed(self.rows)), list(reversed(self.names)), {}, per_stratum=2, tail=3)
        self.assertEqual(first, again)
        strata = sorted((e["kind"], e["bucket"]) for e in first if e["why"] == ["stratum"])
        self.assertEqual(strata, [("cs", "<32K")] * 2 + [("cs", ">1.8M")] * 2 + [("ps", "128K-512K")] * 2)
        # The 64-lane translation sizes a compute program.
        self.assertTrue(all(e["hlsl_bytes_at_sampling"] == 2 << 20 for e in first if e["bucket"] == ">1.8M"))
        self.assertEqual(sum(e["why"] == ["tail"] for e in first), 3)

    def test_logged_compiles_name_the_exact_request_when_the_log_has_it(self):
        exact = self.names[3]
        lines = [f'"TuxBox d3d12_xs: CreatePipelineState #5 CS {self.names[0][:8]} finished in 150s\\n"',
                 f'CreatePipelineState #6 CS {self.names[0][:8]} finished in 90s',
                 f'CreatePipelineState #7 CS 0123abcd request={exact[:-5]} bytes=1513440 finished in 300s',
                 'CreatePipelineState #8 PS 789a39df MS 8d4c5f73 finished in 5s',
                 f'CreatePipelineState #9 CS 0123abcd request=original bytes=10 still running 20s']
        logged = stages.logged_compiles(lines)
        self.assertEqual(logged, {self.names[0][:8]: (150, None), "0123abcd": (300, exact[:-5])})
        self.assertEqual(stages.resolve_logged("0123abcd", exact[:-5], set(self.names)), exact)
        sample = stages.build_sample(self.rows, self.names, logged, per_stratum=0, tail=0, slowest=1, slow_band=1)
        self.assertEqual([(e["request"], e["logged_seconds"]) for e in sample],
                         sorted([(exact, 300), (self.names[0], 150)]))

    def test_a_program_the_translator_refuses_is_not_sampled_for_its_compile(self):
        rows = [row for row in self.rows if row["request"] != self.names[0]]
        logged = {self.names[0][:8]: (500, None)}
        self.assertEqual(stages.build_sample(rows, self.names, logged, per_stratum=0, tail=0), [])

    def test_the_committed_sample_is_request_keys_only(self):
        sample = json.loads(stages.SAMPLE.read_text())
        names = [entry["request"] for entry in sample["requests"]]
        self.assertEqual(len(names), len(set(names)))
        self.assertTrue(all(KEY.fullmatch(name) for name in names), names[:3])
        whys = {why for entry in sample["requests"] for why in entry["why"]}
        self.assertEqual(whys, {"stratum", "tail", "logged slow driver compile"})
        self.assertTrue(any(entry["bucket"] == ">1.8M" for entry in sample["requests"]))
        allowed = {"request", "kind", "bucket", "hlsl_bytes_at_sampling", "why", "logged_seconds"}
        self.assertTrue(all(set(entry) <= allowed for entry in sample["requests"]))


FAKE_TOOL = """#!{python}
import os, sys, time
args = sys.argv[1:]
open({pids!r}, "a").write(f"{{os.getpid()}}\\n")
if "-O0" not in args and {sleep!r} and "-T" in args:
    time.sleep(30)
for flag in ("--hlsl-out", "-Fo"):
    if flag in args:
        open(args[args.index(flag) + 1], "w").write("[WaveSize(64)]\\n" + "x" * 1000)
raise SystemExit({status})
"""


class HostStages(unittest.TestCase):
    def setUp(self):
        self.tmp = TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.pids = self.root / "pids"

    def tearDown(self):
        self.tmp.cleanup()

    def tool(self, name, status=0, sleep=False):
        path = self.root / name
        path.write_text(FAKE_TOOL.format(python=sys.executable, pids=str(self.pids), sleep=sleep, status=status))
        path.chmod(0o755)
        return path

    def measure(self, tools, compile_request):
        source = self.root / ("ab" * 32 + ".dxbc")
        source.write_bytes(b"DXBC" + bytes(28))
        with patch.object(stages, "compile_request", compile_request):
            return stages.measure(source, "wave64", tools, self.root, self.root / "keep", sys.executable)

    def test_each_tool_is_timed_with_its_peak_memory_and_the_output_kept(self):
        dxil = container({b"PSV0": psv([(2, 0, 3, 3), (6, 1, 0, 0xFFFFFFFF)])})

        def fake(source, key, tools, cache, session):
            xshader, dxc, dxv = tools
            work = cache / "work"
            work.mkdir()
            subprocess.run([xshader, source, "--hlsl-out", work / "s.hlsl", "--profile-out", work / "p"], check=True)
            subprocess.run([dxc, "-help"], check=True)
            subprocess.run([dxc, "-T", "cs_6_6", "-E", "main", "-Fo", work / "o.cso", work / "s.hlsl"], check=True)
            subprocess.run([dxv, work / "o.cso"], check=True)
            (cache / f"{key}.cso").write_bytes(dxil)
            return True

        row = self.measure((self.tool("x"), self.tool("d"), self.tool("v")), fake)
        self.assertEqual(row["status"], "compiled")
        self.assertEqual([run["tool"] for run in row["runs"]], ["xshader", "dxc", "dxv"])
        self.assertEqual(row["runs"][1]["options"], ["-T", "cs_6_6", "-E", "main"])
        self.assertEqual((row["hlsl_bytes"], row["wave_size"], row["profile"]), (1015, 64, "cs_6_6"))
        for tool in stages.TOOLS:
            self.assertGreater(row["stages"][tool]["peak_mib"], 1)
            self.assertEqual(row["stages"][tool]["runs"], 1)
        name = "ab" * 32
        self.assertEqual((self.root / "keep" / f"{name}.cso").read_bytes(), dxil)
        self.assertEqual((self.root / "keep" / f"{name}.root").read_text(), "cbv 0 3 1\nuav 1 0 0\n")

    def test_a_compile_killed_at_its_timeout_is_counted_and_takes_the_tool_with_it(self):
        def fake(source, key, tools, cache, session):
            xshader, dxc, dxv = tools
            try:
                subprocess.run([dxc, "-T", "cs_6_6", "-Fo", cache / "o.cso", source], timeout=1)
            except subprocess.TimeoutExpired:
                subprocess.run([dxc, "-T", "cs_6_6", "-O0", "-Fo", cache / "o.cso", source], check=True)
            raise ValueError("compiler returned invalid DXIL")

        row = self.measure((self.tool("x"), self.tool("d", sleep=True), self.tool("v")), fake)
        self.assertEqual(row["status"], "failed")
        self.assertEqual(row["stages"]["dxc"]["runs"], 2)
        self.assertEqual(row["stages"]["dxc"]["killed"], 1)
        killed = row["runs"][0]
        self.assertTrue(killed["killed"])
        self.assertGreaterEqual(killed["seconds"], 0.9)
        self.assertLess(killed["seconds"], 10)
        first = int(self.pids.read_text().split()[0])
        for _ in range(50):
            try:
                os.kill(first, 0)
            except ProcessLookupError:
                break
            time.sleep(0.1)
        else:
            self.fail("the timed-out tool outlived its probe")

    def test_compute_programs_are_measured_for_both_hosts(self):
        self.assertEqual(stages.hosts_for(Path("x.wave32")), ("wave32",))
        self.assertEqual(stages.hosts_for(Path("x.link")), ("wave64",))
        self.assertEqual(stages.output_key(Path("ab.dxbc"), "wave32"), "ab_w32")


class GpuStages(unittest.TestCase):
    def test_bindings_come_from_the_program_and_unbounded_ranges_stay_unbounded(self):
        dxil = container({b"SFI0": bytes(8), b"PSV0": psv([(1, 1, 3, 3), (2, 0, 11, 11), (5, 0, 12, 75),
                                                              (7, 2, 0, 0xFFFFFFFF), (0, 0, 0, 0)])})
        self.assertEqual(stages.resource_bindings(dxil),
                         [("sampler", 1, 3, 1), ("cbv", 0, 11, 1), ("srv", 0, 12, 64), ("uav", 2, 0, 0)])
        self.assertEqual(stages.resource_bindings(container({b"PSV0": psv([])})), [])

    def test_a_plan_offers_the_64_lane_translation_before_the_32_lane_one(self):
        rows = [{"request": "a.dxbc", "host": "wave32", "status": "compiled", "profile": "cs_6_6", "wave_size": 32},
                {"request": "a.dxbc", "host": "wave64", "status": "compiled", "profile": "cs_6_6", "wave_size": 64},
                {"request": "b.dxbc", "host": "wave64", "status": "compiled", "profile": "ps_6_0"},
                {"request": "c.dxbc", "host": "wave64", "status": "refused", "profile": "cs_6_6"}]
        lines, hosts = stages.gpu_plan(rows, Path("/d"), lambda path: f"Z:{path}")
        self.assertEqual(lines, ["a.dxbc Z:/d/a.cso 64 Z:/d/a.root Z:/d/a_w32.cso 32 Z:/d/a_w32.root\n"])
        self.assertEqual(hosts, {"a.dxbc": ["wave64", "wave32"]})

    def test_the_driver_part_is_split_from_each_pipeline_call(self):
        results = stages.guest_results("\n".join([
            "device 10de:2b85 32 32",
            "begin warmup 0 1", "done warmup 0 1000 2000 0x00000000 100",
            "begin a.dxbc 1 10000", "done a.dxbc 1 10000 5010000 0x00000000 900",
            "skipped b.dxbc wave",
            "begin c.dxbc 0 6000000"]))
        self.assertEqual(results["device"], {"id": "10de:2b85", "wave_min": 32, "wave_max": 32})
        self.assertEqual((results["skipped"], results["unfinished"]), ({"b.dxbc": "wave"}, ["c.dxbc"]))
        records = stages.driver_records("\n".join([
            "driver compute 1200 1800 0 1 500 400 600 450",
            "destroyed 2500 300000",
            # vkd3d-proton reached 400 MiB above the base while converting; the driver then 1 GiB more.
            "driver compute 1010000 5000000 0 1 709600 400000 1448576 500000",
            "destroyed 5020000 310000",
            "garbage"]))
        rows = stages.gpu_rows(results, records, {"a.dxbc": ["wave64", "wave32"], "c.dxbc": ["wave64"]})
        self.assertEqual(rows[0], {"request": "a.dxbc", "host": "wave32", "hresult": "0x00000000",
                                   "pipeline_seconds": 0.005, "driver_calls": 1, "driver_seconds": 0.004,
                                   "vkd3d_seconds": 0.001, "driver_peak_mib": 1024.0, "vkd3d_peak_mib": 400.0,
                                   "driver_failed": False})
        self.assertEqual(rows[1], {"request": "c.dxbc", "host": None, "hresult": "unfinished"})

    def test_the_table_names_missing_cards_and_holds_no_shader_text(self):
        cpu = [{"request": "ab" * 32 + ".dxbc", "host": "wave64", "kind": "cs", "status": "compiled",
                "hlsl_bytes": 3 << 20, "dxil_bytes": 1 << 20, "stages": {
                    "xshader": {"seconds": 0.2, "peak_mib": 30}, "dxc": {"seconds": 80, "peak_mib": 900, "runs": 2,
                                                                         "killed": 1},
                    "dxv": {"seconds": 0.1, "peak_mib": 40}}}]
        card = {"name": "amd", "id": "1002:13c0", "rows": [{"request": cpu[0]["request"], "host": "wave64",
                                                            "vkd3d_seconds": 1.5, "driver_seconds": 120.0,
                                                            "vkd3d_peak_mib": 300, "driver_peak_mib": 4000}]}
        text = stages.render({"sample": "s", "requests": 1, "head": "h", "jobs": 2, "cpu": cpu, "cards": [card]})
        self.assertIn("GPU stages not run on NVIDIA.", text)
        self.assertIn("| cs | >1.8M | 1 | 0.200 (0.200) | 80 (80) | 0.100 (0.100) | 1.50 (1.50) | 120 (120) |", text)
        self.assertIn("| 2 (1 timed out) |", text)
        self.assertNotIn("WaveSize", text)

    @unittest.skipUnless(shutil.which("cc") and subprocess.run(
        ["cc", "-E", "-x", "c", "-"], input=b"#include <vulkan/vk_layer.h>\n", capture_output=True).returncode == 0,
        "needs a C compiler and the Vulkan headers")
    def test_the_timing_layer_builds_without_warnings(self):
        with TemporaryDirectory() as tmp:
            subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-O2", "-fPIC", "-shared", "-pthread",
                            str(stages.REFERENCE / "layer.c"), "-o", str(Path(tmp) / "layer.so")], check=True)
        manifest = json.loads((stages.REFERENCE / "layer.json").read_text())
        self.assertEqual(manifest["layer"]["library_path"], "./pipeline_timing.so")


class GuestWatch(unittest.TestCase):
    """Wine's own processes outlive a guest that crashed, so the sandbox never exits by itself."""

    def test_a_guest_that_died_stops_the_sandbox_instead_of_waiting_out_the_timeout(self):
        import tools.stage_baseline as runner
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            guest = root / "stage-baseline.exe"
            shutil.copyfile(os.path.realpath(sys.executable), guest)
            guest.chmod(0o755)
            results = root / "results.txt"
            # The guest starts a pipeline and dies; the rest of "Wine" lingers for a minute.
            script = (f"echo 'begin a.dxbc 0 1' >> {results}; {guest} -I -S -c 'import time; time.sleep(0.5)'; sleep 60")
            started = time.monotonic()
            with open(root / "log", "w") as log, patch.object(runner, "say") as said:
                status = runner.watch_guest(["sh", "-c", script], log, results, "card", 1, poll=0.1, grace=0.3)
            self.assertEqual(status, "guest gone")
            self.assertLess(time.monotonic() - started, 20)
            lines = [call.args[0] for call in said.call_args_list]
            self.assertIn("card: 1/1 a.dxbc started", lines)
            self.assertIn("card: the guest exited during a.dxbc; stopping the sandbox", lines)

    def test_only_zombies_left_means_the_guest_is_gone(self):
        self.assertFalse(stages.guest_gone([], seen=False))
        self.assertFalse(stages.guest_gone(["S", "Z"], seen=True))
        self.assertTrue(stages.guest_gone(["Z"], seen=True))
        self.assertTrue(stages.guest_gone([], seen=True))
        child = subprocess.Popen(["sh", "-c", "sleep 30"])
        try:
            for _ in range(50):
                if stages.process_states(child.pid, "sleep"):
                    break
                time.sleep(0.05)
            self.assertEqual(len(stages.process_states(child.pid, "sleep")), 1)
        finally:
            child.kill()
            child.wait()


if __name__ == "__main__":
    unittest.main()
