import json
from pathlib import Path
import tempfile
import tomllib
import unittest

from scripts.nextest_timings import p99, report, samples


class Timings(unittest.TestCase):
    def test_nearest_rank_uses_test_durations(self):
        self.assertEqual(p99(list(range(1, 101))), 99)
        self.assertEqual(p99([31]), 31)

    def test_partial_runs_skips_and_platforms(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runs = [{"databaseId": 1, "createdAt": "2026-10-14", "conclusion": "cancelled"}]
            (root / "runs.json").write_text(json.dumps(runs))
            for leg, duration in [("macos-15", 19), ("ubuntu-latest", 25)]:
                path = root / "1" / f"junit-{leg}-unit-1" / "junit.xml"
                path.parent.mkdir(parents=True)
                path.write_text(f'<testsuites><testsuite name="flock-ai::fixture">'
                                f'<testcase time="{duration}"/><testcase><skipped/></testcase>'
                                '</testsuite></testsuites>')
            data = samples(root, runs)
            self.assertEqual(data["fixture", "macos-15"], [19])
            self.assertIn("| `fixture` | 1 / 19.000 / 19.000 | 1 / 25.000 / 25.000 | 300 |", report(root))
            self.assertIn("cancelled | 2 |", report(root))

    def test_missing_reports_fail_instead_of_changing_the_sample(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "no reports for run 42"):
                samples(Path(directory), [{"databaseId": 42}])

    def test_recorded_measurements_fit_every_configured_ceiling(self):
        root = Path(__file__).resolve().parents[1]
        config = tomllib.loads((root / ".config/nextest.toml").read_text())
        default = config["profile"]["default"]
        def seconds(slow):
            return int(slow["period"].removesuffix("s")) * slow["terminate-after"]
        checked = 0
        for row in (root / "docs/next/nextest-timings.md").read_text().splitlines():
            if not row.startswith("| `"):
                continue
            cells = row.split("|")
            binary = cells[1].strip().strip("`")
            ceiling = seconds(default["slow-timeout"])
            for override in default["overrides"]:
                if f"binary(={binary})" in override["filter"] and "slow-timeout" in override:
                    ceiling = seconds(override["slow-timeout"])
                    break
            self.assertGreaterEqual(ceiling, int(cells[-2]), binary)
            checked += 1
        self.assertGreater(checked, 50)


if __name__ == "__main__":
    unittest.main()
