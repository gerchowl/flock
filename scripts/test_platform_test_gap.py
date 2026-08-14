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

    def test_ungated_files_are_absent(self):
        root = self._tree({"plain.rs": "#[test]\nfn a() {}\n"})
        self.assertEqual(gated_tests(root), {})

    def test_reports_the_real_tree(self):
        """The notice must describe this repository, not a fixture.

        If the gap ever closes, this is the test that says so.
        """
        root = Path(__file__).resolve().parent.parent
        counts = gated_tests(root)
        self.assertIn(Path("tests/api_ping.rs"), counts)
        self.assertGreater(sum(counts.values()), 0)


if __name__ == "__main__":
    unittest.main()
