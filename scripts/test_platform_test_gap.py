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

        #269 closed the blanket macOS gates. What is left is ONE test that is
        # Linux-only because it reads `/proc/<pid>/cwd`, which no test-side shim
        can paper over. This is the test that notices if a gate comes back, and
        it is deliberately an exact count rather than a "greater than zero" — a
        ratchet that tolerates growth is not a ratchet.
        """
        root = Path(__file__).resolve().parent.parent
        self.assertEqual(gated_tests(root), {Path("tests/api_ping.rs"): 1})


if __name__ == "__main__":
    unittest.main()
