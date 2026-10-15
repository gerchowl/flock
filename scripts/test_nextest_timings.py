"""Check ADR-0037 ceilings and provable first-match shadowing.

Run from any directory with `python3 -I /path/to/scripts/test_nextest_timings.py`,
or use repository-root unittest discovery (`just script-tests`).
"""
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest

# Resolve the sibling explicitly: isolated Python omits the script directory.
_spec = importlib.util.spec_from_file_location(
    "nextest_timings", Path(__file__).resolve().with_name("nextest_timings.py"),
)
_timings = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_timings)
p99, report, samples = _timings.p99, _timings.report, _timings.samples


def filter_clauses(expression):
    """Return DNF for binary/test literals, &, | and parentheses, or None.

    Binary selectors default to globs, so supported wildcard-free names are
    exact, with or without '='. Test selectors default to substring matching.
    Wildcards, negation, regexes and other predicates are deliberately unproved.
    """
    if expression is None:
        return None
    token = re.compile(r"\s*(?:(binary|test)\((=?[A-Za-z0-9_:.-]+)\)|([&|()]))")
    tokens = []
    position = 0
    while position < len(expression.rstrip()):
        match = token.match(expression, position)
        if match is None:
            return None
        kind, selector, operator = match.groups()
        tokens.append((kind, selector) if kind else operator)
        position = match.end()
    index = 0

    def primary():
        nonlocal index
        item = tokens[index]
        index += 1
        if item == "(":
            clauses = union()
            if tokens[index] != ")":
                raise ValueError("unclosed filter group")
            index += 1
            return clauses
        if isinstance(item, tuple):
            return [frozenset([item])]
        raise ValueError("expected filter literal")

    def intersection():
        nonlocal index
        clauses = primary()
        while index < len(tokens) and tokens[index] == "&":
            index += 1
            right = primary()
            clauses = [left | other for left in clauses for other in right]
        return clauses

    def union():
        nonlocal index
        clauses = intersection()
        while index < len(tokens) and tokens[index] == "|":
            index += 1
            clauses += intersection()
        return clauses

    try:
        clauses = union()
        return clauses if index == len(tokens) else None
    except (IndexError, ValueError):
        return None


def clause_implies(later, earlier):
    """Prove each earlier literal using a more restrictive later literal."""
    def implies(current, prior):
        kind, selector = current
        prior_kind, prior_selector = prior
        if kind != prior_kind:
            return False
        if kind == "binary":
            return selector.removeprefix("=") == prior_selector.removeprefix("=")
        if prior_selector.startswith("="):
            return selector == prior_selector
        return prior_selector in selector.removeprefix("=")

    return all(any(implies(current, prior) for current in later) for prior in earlier)



class Timings(unittest.TestCase):
    def assert_no_shadowed_overrides(self, config):
        """Check each setting independently, with CI overrides before defaults.

        An earlier unscoped rule covers every platform. A scoped rule proves
        coverage only for an identical scope. Unsupported filters remain
        unchecked by this proof, while the ceiling checks still inspect them.
        Partially shadowed overrides with a reachable OR arm, such as the
        original #1051 rule, are deliberately not flagged. Return diagnostics
        for unproved filters, including platform-only overrides with no filter.
        """
        unproved = []
        default = config["profile"]["default"].get("overrides", [])
        for profile, settings in config["profile"].items():
            overrides = settings.get("overrides", [])
            if profile != "default":
                overrides = overrides + default
            earlier = []
            for index, override in enumerate(overrides):
                expression = override.get("filter")
                clauses = filter_clauses(expression)
                if clauses is None:
                    reason = ("platform-only (no filter)" if expression is None
                              else f"unsupported filter {expression!r}")
                    unproved.append(f"{profile} override {index + 1} unproved: {reason}")
                else:
                    for key in override.keys() - {"filter", "platform"}:
                        covered = []
                        for previous, prior_clauses in earlier:
                            if (key in previous and prior_clauses is not None
                                    and ("platform" not in previous
                                         or previous["platform"] == override.get("platform"))):
                                covered += prior_clauses
                        self.assertFalse(
                            all(any(clause_implies(clause, prior) for prior in covered)
                                for clause in clauses),
                            f"{profile} override {index + 1} shadowed {key}: {override['filter']}",
                        )
                earlier.append((override, clauses))
        return unproved

    def test_isolated_invocation_from_an_unrelated_directory(self):
        script = Path(__file__).resolve()
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run(
                [sys.executable, "-I", str(script),
                 "Timings.test_nearest_rank_uses_test_durations"],
                cwd=directory, capture_output=True, text=True,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Ran 1 test", result.stderr)

    def test_fully_shadowed_ceiling_is_rejected(self):
        config = {"profile": {"default": {"overrides": [
            {"filter": "binary(=live_handoff)", "slow-timeout": {}},
            {"filter": "binary(=live_handoff) & test(=rollback_case)",
             "slow-timeout": {}},
        ]}}}
        with self.assertRaisesRegex(AssertionError, "shadowed.*slow-timeout"):
            self.assert_no_shadowed_overrides(config)

    def test_binary_literal_selectors_are_exact(self):
        for earlier in ["binary(live)", "binary(=live)"]:
            for later in ["binary(live_handoff)", "binary(=live_handoff)"]:
                with self.subTest(earlier=earlier, later=later):
                    self.assert_no_shadowed_overrides({"profile": {"default": {
                        "overrides": [
                            {"filter": earlier, "retries": 0},
                            {"filter": later + " & test(rollback)", "retries": 2},
                        ],
                    }}})
            for later in ["binary(live)", "binary(=live)"]:
                with self.subTest(earlier=earlier, later=later):
                    with self.assertRaisesRegex(AssertionError, "shadowed retries"):
                        self.assert_no_shadowed_overrides({"profile": {"default": {
                            "overrides": [
                                {"filter": earlier, "retries": 0},
                                {"filter": later, "retries": 2},
                            ],
                        }}})

    def test_platform_only_overrides_are_reported_as_unproved(self):
        scoped = {"platform": "cfg(unix)", "slow-timeout": {}}
        filtered = {"filter": "binary(=one)", "slow-timeout": {}}
        for overrides, index in [([scoped, filtered], 1), ([filtered, scoped], 2)]:
            with self.subTest(index=index):
                unproved = self.assert_no_shadowed_overrides({"profile": {
                    "default": {"overrides": overrides},
                }})
                self.assertIn(
                    f"default override {index} unproved: platform-only (no filter)",
                    unproved,
                )

    def test_binary_literal_ceiling_does_not_match_a_longer_name(self):
        config = {"profile": {"default": {
            "slow-timeout": {"period": "60s", "terminate-after": 7},
            "overrides": [{"filter": "binary(live)",
                           "slow-timeout": {"period": "60s", "terminate-after": 3}}],
        }}}
        self.assert_ceilings_fit(config, {"live_handoff": 420})

    def test_platform_only_ceilings_are_still_checked(self):
        config = {"profile": {"default": {
            "slow-timeout": {"period": "60s", "terminate-after": 7},
            "overrides": [{"platform": "cfg(unix)",
                           "slow-timeout": {"period": "30s", "terminate-after": 3}}],
        }}}
        with self.assertRaises(AssertionError):
            self.assert_ceilings_fit(config, {"one": 420})

    def test_shadow_proof_handles_unions_and_substrings(self):
        for earlier, later in [
            ("binary(live_handoff)", "binary(=live_handoff) & test(rollback)"),
            ("binary(=one) | binary(=two)",
             "(binary(=one) & test(=case)) | binary(=two)"),
            ("binary(=one) & test(rollback)",
             "binary(=one) & test(=rollback_case)"),
        ]:
            with self.subTest(earlier=earlier, later=later):
                config = {"profile": {"default": {"overrides": [
                    {"filter": earlier, "retries": 0},
                    {"filter": later, "retries": 2},
                ]}}}
                with self.assertRaisesRegex(AssertionError, "shadowed retries"):
                    self.assert_no_shadowed_overrides(config)

    def test_separate_rules_can_collectively_shadow_a_union(self):
        config = {"profile": {"default": {"overrides": [
            {"filter": "binary(=one)", "slow-timeout": {}},
            {"filter": "binary(=two)", "slow-timeout": {}},
            {"filter": "binary(=one) | binary(=two)", "slow-timeout": {}},
        ]}}}
        with self.assertRaisesRegex(AssertionError, "shadowed slow-timeout"):
            self.assert_no_shadowed_overrides(config)

    def test_partial_coverage_and_different_settings_are_allowed(self):
        for earlier, later in [
            ({"filter": "binary(=one)", "slow-timeout": {}},
             {"filter": "binary(=one)", "test-group": "serial"}),
            ({"filter": "binary(=one)", "slow-timeout": {}},
             {"filter": "binary(=one) | binary(=two)", "slow-timeout": {}}),
            ({"filter": "binary(=one) & test(=specific)", "retries": 0},
             {"filter": "binary(=one)", "retries": 2}),
            ({"filter": "binary(=one)", "retries": 0},
             {"filter": "binary(one_more)", "retries": 2}),
        ]:
            with self.subTest(earlier=earlier, later=later):
                self.assert_no_shadowed_overrides({"profile": {
                    "default": {"overrides": [earlier, later]},
                }})

    def test_each_setting_is_checked_even_if_another_is_reachable(self):
        config = {"profile": {"default": {"overrides": [
            {"filter": "binary(=one)", "slow-timeout": {}},
            {"filter": "binary(=one)", "slow-timeout": {}, "retries": 0},
        ]}}}
        with self.assertRaisesRegex(AssertionError, "shadowed slow-timeout"):
            self.assert_no_shadowed_overrides(config)

    def test_platform_scopes_and_profile_inheritance(self):
        for prior_scope, later_scope, shadowed in [
            (None, "cfg(unix)", True),
            ("cfg(unix)", "cfg(unix)", True),
            ("cfg(unix)", None, False),
            ("cfg(unix)", "cfg(windows)", False),
        ]:
            earlier = {"filter": "binary(=one)", "retries": 0}
            later = {"filter": "binary(=one)", "retries": 2}
            if prior_scope:
                earlier["platform"] = prior_scope
            if later_scope:
                later["platform"] = later_scope
            config = {"profile": {"default": {"overrides": [later]},
                                  "ci": {"overrides": [earlier]}}}
            with self.subTest(prior_scope=prior_scope, later_scope=later_scope):
                if shadowed:
                    with self.assertRaisesRegex(AssertionError, "ci.*shadowed retries"):
                        self.assert_no_shadowed_overrides(config)
                else:
                    self.assert_no_shadowed_overrides(config)

    def test_unknown_filter_syntax_is_not_guessed(self):
        for expression in [None, "binary(one*)", "binary(one?)", "binary([ab])",
                           "not binary(=one)", "binary(/one/)", "kind(test)",
                           "binary(=one) &", "(binary(=one)"]:
            with self.subTest(expression=expression):
                self.assertIsNone(filter_clauses(expression))
                self.assert_no_shadowed_overrides({"profile": {"default": {"overrides": [
                    {"filter": expression, "retries": 0},
                    {"filter": "binary(=one)", "retries": 2},
                ]}}})

    def test_real_overrides_have_no_fully_shadowed_settings(self):
        root = Path(__file__).resolve().parents[1]
        config = tomllib.loads((root / ".config/nextest.toml").read_text())
        for diagnostic in self.assert_no_shadowed_overrides(config):
            sys.stderr.write(diagnostic + "\n")

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
                if name == selector.removeprefix("="):
                    return True
            return False

        binary_union = r"\s*binary\(=?[A-Za-z0-9_-]+\)(?:\s*\|\s*binary\(=?[A-Za-z0-9_-]+\))*\s*"
        for binary, minimum in minimums.items():
            ceiling = seconds(default["slow-timeout"])
            for override in default["overrides"]:
                if ("slow-timeout" in override
                        and re.fullmatch(binary_union, override.get("filter", ""))
                        and matches(override.get("filter", ""), binary)):
                    ceiling = seconds(override["slow-timeout"])
                    break
            self.assertGreaterEqual(ceiling, minimum, binary)
            # Check every override, even one masked by an earlier binary rule,
            # so a later filter reorder cannot expose an undersized ceiling.
            for profile in ["default", "ci"]:
                for override in config["profile"].get(profile, {}).get("overrides", []):
                    if "slow-timeout" in override and matches(override.get("filter", ""), binary):
                        self.assertGreaterEqual(
                            seconds(override["slow-timeout"]), minimum,
                            f"{binary}: {profile} {override.get('filter', '<platform-only>')}",
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
