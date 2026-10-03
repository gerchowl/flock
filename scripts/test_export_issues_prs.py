"""Tests for the issue/PR archiver.

The defect these pin (#512): the archiver hardcoded `gerchowl/herdr`, so it read
and mirrored the upstream tracker instead of this repository's, and every
generated filename is derived from an issue title — so a title naming a host
became a committed path.

Hermetic by construction: no network, no `gh`, no ambient machine state. The repo
resolution is exercised against a throwaway git repo built in a temp dir.
"""

import importlib.util
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "export-issues-prs.py"


def load_module():
    spec = importlib.util.spec_from_file_location("export_issues_prs", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class SlugTests(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_module()

    def test_lowercases_and_hyphenates(self) -> None:
        self.assertEqual(self.mod.slug("Fix: the Thing!"), "fix-the-thing")

    def test_empty_title_becomes_untitled(self) -> None:
        self.assertEqual(self.mod.slug(""), "untitled")
        self.assertEqual(self.mod.slug(None), "untitled")
        self.assertEqual(self.mod.slug("---"), "untitled")

    def test_truncation_never_leaves_a_trailing_hyphen(self) -> None:
        # A title cut mid-word must not end on the separator, or the next
        # `.md` would be glued to a dangling dash.
        slug = self.mod.slug("alpha beta gamma delta epsilon", n=12)
        self.assertEqual(slug, "alpha-beta-g")
        self.assertFalse(slug.endswith("-"))
        # Cut exactly on the separator instead: still no dangling hyphen.
        self.assertEqual(self.mod.slug("alpha beta gamma", n=11), "alpha-beta")

    def test_slug_cannot_escape_its_directory(self) -> None:
        # Slugs are filenames; a traversal or separator must never survive.
        for title in ("../../etc/passwd", "a/b", "..", "x\\y"):
            out = self.mod.slug(title)
            self.assertNotIn("/", out)
            self.assertNotIn("\\", out)
            self.assertNotIn("..", out)


class ResolveRepoTests(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_module()
        self._tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _git(self, *args: str) -> None:
        subprocess.run(["git", "-C", str(self.repo), *args], check=True, capture_output=True)

    def _with_remote(self, url: str) -> None:
        self._git("init", "-q")
        self._git("remote", "add", "origin", url)

    def test_parses_https_and_ssh_remotes(self) -> None:
        for url, expected in (
            ("https://github.com/acme/tool.git", "acme/tool"),
            ("https://github.com/acme/tool", "acme/tool"),
            ("git@github.com:acme/tool.git", "acme/tool"),
            ("ssh://git@github.com/acme/tool.git", "acme/tool"),
        ):
            with self.subTest(url=url):
                self._with_remote(url)
                self.assertEqual(self.mod.resolve_repo(self.repo), expected)
                self._git("remote", "remove", "origin")

    def test_refuses_when_there_is_no_origin(self) -> None:
        # Refusing is the point: archiving a different repository silently is
        # worse than not archiving at all.
        self._git("init", "-q")
        with self.assertRaises(SystemExit):
            self.mod.resolve_repo(self.repo)

    def test_refuses_a_remote_that_is_not_owner_repo(self) -> None:
        self._with_remote("/some/local/path/with/no/owner")
        with self.assertRaises(SystemExit):
            self.mod.resolve_repo(self.repo)


class SourceTests(unittest.TestCase):
    """The regression itself: a hardcoded repository pointer."""

    def setUp(self) -> None:
        self.source = SCRIPT.read_text(encoding="utf-8")

    def test_no_repository_is_hardcoded(self) -> None:
        # A literal `owner/repo` is exactly what let the fork inherit the
        # upstream tracker (#512). The repository comes from the checkout.
        literals = re.findall(r'["\'][A-Za-z0-9._-]+/[A-Za-z0-9._-]+["\']', self.source)
        self.assertEqual(literals, [], f"hardcoded repo literal(s): {literals}")

    def test_no_bare_repo_constant_is_exported(self) -> None:
        self.assertIsNone(re.search(r"^REPO\s*=", self.source, re.M))

    def test_generated_index_header_names_the_resolved_repo(self) -> None:
        self.assertIn("{kind} archive ({repo})", self.source)


class WriteItemTests(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_module()
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _item(self, **over):
        item = {
            "number": 7,
            "title": "a title",
            "state": "OPEN",
            "author": {"login": "someone"},
            "labels": [],
            "createdAt": "2026-01-01T00:00:00Z",
            "closedAt": None,
            "url": "https://example.invalid/x",
            "body": "body text",
            "comments": [],
        }
        item.update(over)
        return item

    def test_writes_one_file_named_from_number_and_slug(self) -> None:
        name = self.mod.write_item("issue", self.dir, self._item(title="Fix: the Thing!"))
        self.assertEqual(name, "0007-fix-the-thing.md")
        written = (self.dir / name).read_text(encoding="utf-8")
        self.assertIn("number: 7", written)
        self.assertIn("body text", written)

    def test_pr_front_matter_records_base_and_head(self) -> None:
        name = self.mod.write_item(
            "pr",
            self.dir,
            self._item(mergedAt="2026-01-02T00:00:00Z", baseRefName="main", headRefName="topic"),
        )
        written = (self.dir / name).read_text(encoding="utf-8")
        self.assertIn("base: main", written)
        self.assertIn("head: topic", written)

    def test_comments_are_rendered_when_present(self) -> None:
        name = self.mod.write_item(
            "issue",
            self.dir,
            self._item(comments=[{"author": {"login": "a"}, "createdAt": "2026-01-03T00:00:00Z", "body": "hi"}]),
        )
        self.assertIn("## Comments", (self.dir / name).read_text(encoding="utf-8"))

    def test_written_path_stays_inside_the_target_directory(self) -> None:
        name = self.mod.write_item("issue", self.dir, self._item(title="../../escape"))
        self.assertEqual((self.dir / name).resolve().parent, self.dir.resolve())


if __name__ == "__main__":
    unittest.main()