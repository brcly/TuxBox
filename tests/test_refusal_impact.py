from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from unittest.mock import patch

from tools.refusal_impact import main


class RefusalImpact(unittest.TestCase):
    def write_cache(self, root, rows, reasons):
        runtime = root / "runtime" / "cache-a"
        runtime.mkdir(parents=True)
        for key, reason in reasons.items():
            (runtime / f"{key}.reason").write_text(reason)
        failures = root / "pipeline-failures"
        failures.mkdir(parents=True)
        (failures / "1-1.tsv").write_text("count\toutcome\tpath\tstages\n" + rows)

    def test_actionable_and_expected_rows_are_separated_and_ranked(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write_cache(
                root,
                "3\tstand-in\tslot62\tCS=rootless CS.refused=mesh root signature missing\n"
                "6\tstand-in\tslot62\tCS.refused=mixed lane Wave32 cannot lower ds_write2_b32 at 0x1\n"
                "4\tstand-in\tslot62\tCS.refused=mixed lane Wave32 cannot lower ds_write2_b32 at 0x2\n"
                "2\tstand-in\tslot62\tCS.refused=v_readlane_b32 at 0x3: inside non-uniform loop (0x3..=0x5); barrier/wave read cannot be proven convergent\n"
                "2\tstand-in\tslot62\tCS.refused=driver compile deadline\n",
                {"rootless": "mesh root signature missing"},
            )
            output = StringIO()
            with redirect_stdout(output):
                self.assertEqual(main([str(root)]), 0)
            text = output.getvalue()
            self.assertIn("17 pipelines not delivered", text)
            self.assertIn("mixed_lane_ds_write2_b32", text)
            self.assertIn("readlane_non_uniform_loop", text)
            # Expected at preparation stays out of the actionable list.
            self.assertNotIn("mesh_root_layout               mesh root signature missing", text.split("Not translator refusals")[0])
            self.assertIn("mesh_root_layout", text.split("Expected at preparation")[1].split("Not translator refusals")[0])
            # The deadline is not a translator refusal.
            not_refusal = text.split("Not translator refusals")[1]
            self.assertIn("driver compile deadline", not_refusal)
            # ds_write2_b32 (10 pipelines) outranks non-uniform-loop readlane (2).
            self.assertLess(text.index("mixed_lane_ds_write2_b32"),
                            text.index("readlane_non_uniform_loop"))

    def test_every_expected_bucket_is_printed(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write_cache(
                root,
                "3\tstand-in\tslot62\tCS=rootless CS.refused=mesh root signature missing\n"
                "4\tstand-in\tslot62\tCS=blank CS.refused=unsupported mesh launch or output configuration: MeshInfo { vertices: 128, primitives: 128, topology: 2, threads: [128, 1, 1], payload: 0, shared_bytes: 3072 }\n",
                {"rootless": "mesh root signature missing"},
            )
            with patch("tools.refusal_impact.expected_at_preparation",
                       lambda bucket: bucket in ("mesh_root_layout", "mesh_launch")):
                output = StringIO()
                with redirect_stdout(output):
                    self.assertEqual(main([str(root)]), 0)
            expected = output.getvalue().split("Expected at preparation")[1].split("Not translator refusals")[0]
            self.assertIn("mesh_root_layout", expected)
            self.assertIn("mesh_launch", expected)

    def test_non_refusal_reasons_are_excluded_from_actionable(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write_cache(
                root,
                "4\tstand-in\tslot62\tCS.refused=driver compile deadline\n"
                "2\t0x80004001\tslot86\tCS.refused=stand-in after translation (cause not recorded)\n",
                {},
            )
            output = StringIO()
            with redirect_stdout(output):
                self.assertEqual(main([str(root)]), 0)
            text = output.getvalue()
            actionable = text.split("Not translator refusals")[0]
            self.assertIn("(none)", actionable)
            self.assertIn("driver compile deadline", text.split("Not translator refusals")[1])

    def test_default_cache_is_not_a_path_leak_and_accepts_run_flag(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write_cache(
                root,
                "2\tstand-in\tslot62\tCS.refused=driver compile deadline\n",
                {},
            )
            run = root / "pipeline-failures" / "2-2.tsv"
            run.write_text("count\toutcome\tpath\tstages\n1\tstand-in\tslot62\tCS.refused=driver compile deadline\n")
            output = StringIO()
            with redirect_stdout(output):
                self.assertEqual(main([str(root), "--run", str(run)]), 0)
            self.assertIn("1 pipelines not delivered", output.getvalue())
            self.assertNotIn(str(root), output.getvalue())


if __name__ == "__main__":
    unittest.main()
