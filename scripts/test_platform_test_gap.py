from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts.platform_test_gap import gated_tests


class PlatformTestGap(unittest.TestCase):
    def _tree(self, files: dict[str, str]) -> Path:
        root = Path(tempfile.mkdtemp())
        (root / "tests").mkdir()
        for name, body in files.items():
            (root / "tests" / name).write_text(body, encoding="utf-8")
        return root

    def test_file_level_gate_withholds_every_test_in_the_file(self):
        # `#![cfg(...)]` is an inner attribute gating the WHOLE file — this is
        # where most of the gap comes from, and an item-level regex misses it.
        body = """
        #![cfg(not(target_os = "macos"))]
        #[test]
        fn a() {}
        #[tokio::test]
        async fn b() {}
        #[test]
        fn c() {}
        """
        root = self._tree({"cli_wrapper.rs": body})
        self.assertEqual(gated_tests(root), {Path("tests/cli_wrapper.rs"): 3})

    def test_file_level_gate_counts_every_test_for_both_spellings(self):
        # The bug this pins: `GATE_FILE` used to match only `not(macos)` while
        # the item-level pattern matched both spellings. A file gated wholesale
        # with `target_os = "linux"` was therefore reported as withholding ONE
        # gate rather than every test in it — so the exact-count pin could be
        # satisfied while a whole file was compiled out. Whichever spelling is
        # used, file scope must report the same number as item scope would.
        body = """
        #![cfg(target_os = "linux")]
        #[test]
        fn a() {}
        #[tokio::test]
        async fn b() {}
        #[test]
        fn c() {}
        """
        root = self._tree({"linux_only.rs": body})
        self.assertEqual(gated_tests(root), {Path("tests/linux_only.rs"): 3})

    def test_item_level_gates_count_individually(self):
        body = """
        #[test]
        fn runs_everywhere() {}

        #[cfg(not(target_os = "macos"))]
        #[test]
        fn linux_only() {}

        #[cfg(not(target_os = "macos"))]
        #[test]
        fn also_linux_only() {}
        """
        root = self._tree({"api_ping.rs": body})
        self.assertEqual(gated_tests(root), {Path("tests/api_ping.rs"): 2})

    def test_linux_only_gates_are_counted_too(self):
        # `#[cfg(target_os = "linux")]` withholds a test from a macOS build just
        # as `not(macos)` does. Matching only the second under-reported the gap.
        body = """
        #[cfg(target_os = "linux")]
        #[test]
        fn reads_proc() {}
        """
        root = self._tree({"api_ping.rs": body})
        self.assertEqual(gated_tests(root), {Path("tests/api_ping.rs"): 1})

    def test_a_gate_on_a_helper_costs_no_coverage(self):
        # A Linux-only helper function withholds nothing by itself. Counting it
        # reported "6 tests" when the real gap was one test.
        body = """
        #[cfg(target_os = "linux")]
        fn helper_reading_proc() -> usize { 0 }

        #[test]
        fn runs_everywhere() {}
        """
        root = self._tree({"live_handoff.rs": body})
        self.assertEqual(gated_tests(root), {})

    def test_macos_only_gates_are_not_a_gap_here(self):
        # Those tests DO exist in a macOS build. Counting them would be backwards.
        body = """
        #[cfg(target_os = "macos")]
        #[test]
        fn darwin_only() {}
        """
        root = self._tree({"live_handoff.rs": body})
        self.assertEqual(gated_tests(root), {})

    def test_a_gate_named_in_a_comment_is_not_a_gate(self):
        # The headers of tests/cli_wrapper.rs and tests/auto_detect.rs explain in
        # prose which gate they used to carry. Counting that prose would report
        # every test in the file as withheld, which is exactly the false positive
        # that let a stale count survive.
        body = """
        // This file used to open with `#![cfg(not(target_os = "macos"))]`, which
        // compiled all 47 tests out of every Mac build.
        /* also #[cfg(not(target_os = "macos"))] in a block comment */
        #[test]
        fn runs_everywhere() {}
        """
        root = self._tree({"cli_wrapper.rs": body})
        self.assertEqual(gated_tests(root), {})

    def test_ungated_files_are_absent(self):
        root = self._tree({"plain.rs": "#[test]\nfn a() {}\n"})
        self.assertEqual(gated_tests(root), {})

def test_reports_the_real_tree(self):
        """The notice must describe this repository, not a fixture.

        The answer is `{}`, and that is the whole point of the ratchet: #269
        triaged every gate in `tests/` and the last one went too, so a macOS
        build now contains every test in the suite. Any gate at all fails this
        assertion, which is the enforcement — a "greater than zero" pin would
        have let the gap creep back one test at a time unnoticed.

        The last gate to fall was
        `pane_info_reports_foreground_cwd_without_changing_pane_cwd`, which had
        been Linux-only for a `/proc/<pid>/cwd` read. Asking the foreground shell
        for its own cwd with `pwd` proves the same thing through a portable
        interface, and it means `platform::macos::process_cwd` is now covered
        rather than sitting untested behind a gate.
        """
        root = Path(__file__).resolve().parent.parent
        self.assertEqual(gated_tests(root), {})


if __name__ == "__main__":
    unittest.main()
