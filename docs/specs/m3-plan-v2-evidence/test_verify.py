"""Regression checks for coordinated edits to the dated benchmark evidence."""

import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent


class BudgetVerificationTests(unittest.TestCase):
    """Exercise the verifier's CLI against original and altered evidence copies."""

    def test_original_evidence_passes(self):
        """The committed measurements remain valid under the frozen budget table."""
        result = subprocess.run(
            [sys.executable, "-B", str(ROOT / "verify.py")], capture_output=True, text=True
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def check_altered_budget(self, bucket, field, value, drivers=("drey", "naive")):
        """Alter both repeats and repair verdict summaries so self-consistency is insufficient."""
        with tempfile.TemporaryDirectory(prefix="drey-evidence-budget-") as directory:
            copied = Path(directory) / "evidence"
            shutil.copytree(ROOT, copied, ignore=shutil.ignore_patterns("__pycache__"))
            summary_path = copied / "analysis.json"
            summary = json.loads(summary_path.read_text())
            for driver in drivers:
                for attempt in (1, 2):
                    path = copied / f"representative-{driver}-{attempt}.json"
                    data = json.loads(path.read_text())
                    row = next(row for row in data["results"] if row["op"] == bucket)
                    row[field] = value
                    if driver == "drey":
                        # The raised latency cap makes similarity pass; relaxing or
                        # removing the throughput gate leaves the update row passing.
                        row["pass"] = True
                        summary["representative"]["budget_failures"][str(attempt)] = [
                            row for row in data["results"] if row["pass"] is False
                        ]
                    path.write_text(json.dumps(data) + "\n")
            summary_path.write_text(json.dumps(summary) + "\n")
            result = subprocess.run(
                [sys.executable, "-B", str(copied / "verify.py")],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(result.returncode, 0, "altered budgets were accepted")
            self.assertIn(f"budgets {bucket}", result.stderr)

    def test_coordinated_latency_budget_and_verdict_edits_are_rejected(self):
        """A 30 ms cap cannot hide the recorded similarity overrun in both repeats."""
        self.check_altered_budget("similar_nodes:cand=10000", "budget_us", 30000.0)

    def test_coordinated_throughput_budget_edits_are_rejected(self):
        """Relaxing or removing the update throughput requirement is refused."""
        for value in (1000.0, None):
            with self.subTest(budget=value):
                self.check_altered_budget(
                    "update_edge_weight", "budget_throughput_per_s", value
                )

    def test_naive_budget_drift_is_rejected(self):
        """Naive metadata must retain the fixed budgets even though verdicts are null."""
        self.check_altered_budget(
            "similar_nodes:cand=10000", "budget_us", 30000.0, drivers=("naive",)
        )


if __name__ == "__main__":
    unittest.main()
