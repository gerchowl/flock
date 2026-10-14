import json
from pathlib import Path
import re
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

    def assert_ceilings_fit(self, config, minimums):
        default = config["profile"]["default"]

        def seconds(slow):
            return int(slow["period"].removesuffix("s")) * slow["terminate-after"]

        def matches(expression, binary):
            selectors = re.findall(r"\bbinary\(([^)]*)\)", expression)
            # Conservative for negation, regexes and filters without a binary:
            # they may reach any binary. Per-test ceilings use binary-wide p99.
            if not selectors or re.search(r"\bnot\b|!", expression):
                return True
            name = binary.removeprefix("bin/")
            for selector in selectors:
                if not re.fullmatch(r"=?[A-Za-z0-9_-]+", selector):
                    return True
                if (name == selector[1:] if selector.startswith("=") else selector in name):
                    return True
            return False

        binary_union = r"\s*binary\(=?[A-Za-z0-9_-]+\)(?:\s*\|\s*binary\(=?[A-Za-z0-9_-]+\))*\s*"
        for binary, minimum in minimums.items():
            ceiling = seconds(default["slow-timeout"])
            for override in default["overrides"]:
                if ("slow-timeout" in override
                        and re.fullmatch(binary_union, override["filter"])
                        and matches(override["filter"], binary)):
                    ceiling = seconds(override["slow-timeout"])
                    break
            self.assertGreaterEqual(ceiling, minimum, binary)
            # Check every override, even one masked by an earlier binary rule,
            # so a later filter reorder cannot expose an undersized ceiling.
            for profile in ["default", "ci"]:
                for override in config["profile"].get(profile, {}).get("overrides", []):
                    if "slow-timeout" in override and matches(override["filter"], binary):
                        self.assertGreaterEqual(
                            seconds(override["slow-timeout"]), minimum,
                            f"{binary}: {profile} {override['filter']}",
                        )

    def test_recorded_measurements_fit_every_configured_ceiling(self):
        root = Path(__file__).resolve().parents[1]
        config = tomllib.loads((root / ".config/nextest.toml").read_text())
        minimums = {}
        for row in (root / "docs/next/nextest-timings.md").read_text().splitlines():
            if row.startswith("| `"):
                cells = row.split("|")
                minimums[cells[1].strip().strip("`")] = int(cells[-2])
        self.assertGreater(len(minimums), 50)
        self.assert_ceilings_fit(config, minimums)

    def test_short_per_test_ceiling_is_rejected_even_after_a_binary_override(self):
        for expression in [
            "binary(delegate_codex) & test(slow_case)",
            "binary(=delegate_codex) & test(slow_case)",
            "test(slow_case)",
            "binary(/delegate_codex/) & test(slow_case)",
            "not binary(=unrelated) & test(slow_case)",
        ]:
            for profile in ["default", "ci"]:
                with self.subTest(expression=expression, profile=profile):
                    config = {"profile": {
                        "default": {
                            "slow-timeout": {"period": "60s", "terminate-after": 7},
                            "overrides": [{
                                "filter": "binary(=delegate_codex)",
                                "slow-timeout": {"period": "60s", "terminate-after": 7},
                            }],
                        },
                        "ci": {"overrides": []},
                    }}
                    config["profile"][profile]["overrides"].append({
                        "filter": expression,
                        "slow-timeout": {"period": "30s", "terminate-after": 3},
                    })
                    with self.assertRaises(AssertionError):
                        self.assert_ceilings_fit(config, {"delegate_codex": 420})


if __name__ == "__main__":
    unittest.main()
