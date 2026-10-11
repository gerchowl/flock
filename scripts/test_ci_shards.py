from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts.ci_shards import (
    Config,
    Shard,
    check_assignment,
    check_partition,
    load_config,
    matched_tests,
    parse_junit,
    render_summary,
    serial_group_binaries,
    shard_filter,
)

CONFIG = Config(
    legs={"linux": "all()", "mac": "not binary(c)"},
    shards=(
        Shard("u1", ("p",), serial=False),
        Shard("u2", (), serial=False, rest=True),
        Shard("s1", ("a",), serial=True),
        Shard("s2", ("b", "c"), serial=True),
    ),
)


class RealTree(unittest.TestCase):
    def test_every_serial_binary_has_exactly_one_shard(self):
        # The drift this guards: a binary added to the serial-pty group in
        # nextest.toml but to no shard would land in the unit job, serial
        # behind everything else there, and quietly rebuild the long chain.
        self.assertEqual(check_assignment(load_config(), serial_group_binaries()), [])

    def test_the_serial_group_is_read_from_nextest_toml(self):
        self.assertIn("mesh_step1", serial_group_binaries())


class Filters(unittest.TestCase):
    def test_a_shard_stays_inside_its_legs_base_filter(self):
        self.assertEqual(shard_filter(CONFIG, "mac", "s2"), "(not binary(c)) & (binary(b) | binary(c))")

    def test_the_rest_shard_is_the_complement_of_every_named_binary(self):
        self.assertEqual(
            shard_filter(CONFIG, "linux", "u2"),
            "(all()) & not (binary(p) | binary(a) | binary(b) | binary(c))",
        )

    def test_an_unknown_leg_or_shard_fails_loudly(self):
        with self.assertRaises(SystemExit):
            shard_filter(CONFIG, "windows", "s1")
        with self.assertRaises(SystemExit):
            shard_filter(CONFIG, "linux", "s9")


class Assignment(unittest.TestCase):
    def test_a_serial_binary_without_a_shard_is_reported(self):
        problems = check_assignment(CONFIG, {"a", "b", "c", "d"})
        self.assertEqual(len(problems), 1)
        self.assertIn("d is in the serial-pty group", problems[0])

    def test_a_binary_outside_the_group_in_a_serial_shard_is_reported(self):
        problems = check_assignment(CONFIG, {"a", "b"})
        self.assertEqual(len(problems), 1)
        self.assertIn("c is in serial shard s2", problems[0])

    def test_a_serial_binary_in_a_parallel_shard_is_reported(self):
        problems = check_assignment(CONFIG, {"a", "b", "c", "p"})
        self.assertEqual(len(problems), 1)
        self.assertIn("p is in the serial-pty group but in parallel shard u1", problems[0])

    def test_a_binary_in_two_shards_is_reported(self):
        config = Config(
            CONFIG.legs,
            (Shard("s1", ("a",), serial=True), Shard("s2", ("a",), serial=True)),
        )
        self.assertIn("a is in both s1 and s2", check_assignment(config, {"a"}))

    def test_the_config_needs_exactly_one_rest_shard(self):
        path = Path(tempfile.mkdtemp()) / "shards.toml"
        path.write_text(
            '[legs.l]\nfilter = "all()"\n'
            '[[shards]]\nname = "s"\nserial = true\nbinaries = ["a"]\n',
            encoding="utf-8",
        )
        with self.assertRaises(SystemExit):
            load_config(path)


class Partition(unittest.TestCase):
    def test_an_exact_partition_passes(self):
        self.assertEqual(check_partition({"x", "y", "z"}, {"u": {"x"}, "s": {"y", "z"}}), [])

    def test_a_dropped_test_fails(self):
        problems = check_partition({"x", "y"}, {"u": {"x"}})
        self.assertEqual(problems, ["y is in the base set but no shard runs it"])

    def test_a_test_in_two_shards_fails(self):
        problems = check_partition({"x"}, {"u": {"x"}, "s": {"x"}})
        self.assertEqual(problems, ["x runs in both u and s"])

    def test_a_test_outside_the_base_set_fails(self):
        problems = check_partition({"x"}, {"u": {"x", "w"}})
        self.assertEqual(problems, ["w runs in u but is not in the base set"])

    def test_only_matching_testcases_are_listed(self):
        listing = {
            "rust-suites": {
                "p::bin": {
                    "testcases": {
                        "kept": {"filter-match": {"status": "matches"}},
                        "ignored": {"filter-match": {"status": "mismatch", "reasons": ["ignored"]}},
                    }
                },
                "p::other": {"status": "skipped"},
            }
        }
        self.assertEqual(matched_tests(listing), {"p::bin kept"})


JUNIT = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="3" failures="1" errors="0" time="42.5">
  <testsuite name="flock-ai::mesh_step1" tests="2">
    <testcase name="slow" classname="flock-ai::mesh_step1" time="30.0"></testcase>
    <testcase name="flaky" classname="flock-ai::mesh_step1" time="2.0">
      <flakyFailure message="first attempt"/>
    </testcase>
  </testsuite>
  <testsuite name="flock-ai::bin/flk" tests="1">
    <testcase name="broken" classname="flock-ai::bin/flk" time="0.5">
      <failure message="boom"/>
    </testcase>
  </testsuite>
</testsuites>
"""


class Summary(unittest.TestCase):
    def test_a_report_renders_wall_clock_outcomes_and_totals(self):
        path = Path(tempfile.mkdtemp()) / "junit.xml"
        path.write_text(JUNIT, encoding="utf-8")
        report = parse_junit(path, "serial-2")
        self.assertEqual(report.seconds, 42.5)
        self.assertEqual(
            [(c.binary, c.outcome) for c in report.cases],
            [("mesh_step1", "passed"), ("mesh_step1", "flaky"), ("bin/flk", "failed")],
        )
        text = render_summary("check (linux)", [report])
        self.assertIn("| serial-2 | 42 s | 3 | 1 | 1 |", text)
        self.assertIn("| `mesh_step1::slow` | serial-2 | 30.0 s |", text)
        self.assertIn("| mesh_step1 | serial-2 | 2 | 32 s |", text)

    def test_no_reports_still_renders(self):
        self.assertIn("## nothing ran", render_summary("nothing ran", []))


if __name__ == "__main__":
    unittest.main()
