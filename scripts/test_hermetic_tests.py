from __future__ import annotations

import unittest
from pathlib import Path

from scripts.hermetic_tests import check_text

INTEGRATION = Path("tests/api_ping.rs")
SOURCE = Path("src/ui/sidebar.rs")


def names(findings: list[tuple[int, str, str]]) -> list[str]:
    return [name for _, name, _ in findings]


class HermeticTestsGate(unittest.TestCase):
    """Each case is one of the three real defects from #268, or the boundary
    that keeps the gate from firing on legitimate code."""

    def test_flags_cwd_derived_assertions(self):
        # The sidebar ordering test sorted on the basename of the process cwd,
        # so it passed in ~/Projects/flock and failed in a worktree named
        # `262-render-hot-path`.
        source = """
        #[cfg(test)]
        mod tests {
            fn fixture() {
                let identity = std::env::current_dir().unwrap();
            }
        }
        """
        self.assertEqual(names(check_text(source, SOURCE)), ["current_dir"])

    def test_flags_real_hostname_reads(self):
        source = """
        #[cfg(test)]
        mod tests {
            fn fixture() {
                let host = gethostname();
            }
        }
        """
        self.assertEqual(names(check_text(source, SOURCE)), ["hostname"])

    def test_flags_non_posix_bin_paths_but_allows_bin_sh(self):
        # /bin/bash made an integration test unrunnable on NixOS, which ships
        # /bin/sh and nothing else in /bin.
        bash = 'let child = spawn_with_shell("/bin/bash");'
        self.assertEqual(names(check_text(bash, INTEGRATION)), ["fhs-path"])

        posix = 'let child = spawn_with_shell("/bin/sh");'
        self.assertEqual(check_text(posix, INTEGRATION), [])

    def test_ignores_production_code_outside_test_modules(self):
        # Production code legitimately reads all three; only test code is in
        # scope. Without the cfg(test) gating this would be unusable.
        source = """
        pub fn resolve() -> PathBuf {
            std::env::current_dir().unwrap_or_else(|_| "/".into())
        }
        """
        self.assertEqual(check_text(source, SOURCE), [])

    def test_cfg_test_region_ends_at_its_closing_brace(self):
        # A read AFTER the test module closes is production code again.
        source = """
        #[cfg(test)]
        mod tests {
            fn fixture() { let a = 1; }
        }

        pub fn later() -> PathBuf {
            std::env::current_dir().unwrap()
        }
        """
        self.assertEqual(check_text(source, SOURCE), [])

    def test_escape_hatch_silences_a_deliberate_read(self):
        source = """
        #[cfg(test)]
        mod tests {
            fn fixture() {
                // guardrails-ok(hermetic): asserts the unreadable-cwd fallback
                let identity = std::env::current_dir();
            }
        }
        """
        self.assertEqual(check_text(source, SOURCE), [])

    def test_everything_under_tests_dir_is_in_scope(self):
        # Integration tests have no cfg(test) attribute — the whole file counts.
        source = 'fn main() { let cwd = std::env::current_dir().unwrap(); }'
        self.assertEqual(names(check_text(source, INTEGRATION)), ["current_dir"])


class GateRunsCleanOverTheTree(unittest.TestCase):
    def test_repository_is_currently_clean(self):
        """The gate ships enforcing, so the tree must already satisfy it.

        This is what stops the gate from being merged as decoration: if any of
        the #268 fixes regress, this fails alongside the hook.
        """
        root = Path(__file__).resolve().parent.parent
        offenders: list[str] = []
        for path in list(root.glob("tests/**/*.rs")) + list(root.glob("src/**/*.rs")):
            findings = check_text(path.read_text(encoding="utf-8"), path.relative_to(root))
            offenders.extend(
                f"{path.relative_to(root)}:{line}: [{name}]" for line, name, _ in findings
            )
        self.assertEqual(offenders, [], "\n".join(offenders))


if __name__ == "__main__":
    unittest.main()
