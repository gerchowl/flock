import json
import subprocess
import tempfile
import unittest
from pathlib import Path

import scripts.conventional_commits as conventional_commits
import scripts.preview as preview
from scripts.test_support import commit, make_repo, tag


def build_manifest_in(tmp: str, output_name: str = "preview.json", **overrides) -> str:
    """`preview.build_manifest` with the arguments a workflow would pass."""
    arguments = {
        "output": Path(tmp) / output_name,
        "repo": "gerchowl/flock",
        "tag": "preview-2026-06-02-abcdef123456",
        "build_id": "2026-06-02-abcdef123456",
        "commit": "abcdef1234567890",
        "built_at": "2026-06-02T03:00:00Z",
        "base_version": "0.6.6",
        "protocol": 25,
        "notes": "Preview notes\n",
        "shas": {},
        "retain": 30,
    }
    arguments.update(overrides)
    return preview.build_manifest(**arguments)


class PreviewNotesTests(unittest.TestCase):
    def test_humanize_groups_conventional_subjects(self):
        self.assertEqual(
            preview.humanize_subject("feat(update): add preview channel"),
            ("Added", "Add preview channel"),
        )
        self.assertEqual(
            preview.humanize_subject("fix: handle preview manifest"),
            ("Fixed", "Handle preview manifest"),
        )
        self.assertEqual(
            preview.humanize_subject("not conventional"),
            ("Other", "Not conventional"),
        )

    def test_build_manifest_archives_current_assets(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "preview.json"
            notes = "Preview notes\n"
            content = preview.build_manifest(
                output=output,
                repo="gerchowl/flock",
                tag="preview-2026-06-02-abcdef123456",
                build_id="2026-06-02-abcdef123456",
                commit="abcdef1234567890",
                built_at="2026-06-02T03:00:00Z",
                base_version="0.6.6",
                protocol=12,
                notes=notes,
                shas={"linux-x86_64": "deadbeef"},
                retain=30,
            )
            data = json.loads(content)
            self.assertEqual(data["channel"], "preview")
            self.assertEqual(data["build_id"], "2026-06-02-abcdef123456")
            self.assertEqual(
                data["assets"]["linux-x86_64"]["sha256"],
                "deadbeef",
            )
            self.assertIn("2026-06-02-abcdef123456", data["builds"])

    def test_hidden_subjects_include_preview_manifest_commits(self):
        self.assertTrue(preview.hidden_subject("docs: update preview manifest"))
        self.assertFalse(preview.hidden_subject("fix: repair preview manifest"))

    def test_preview_docs_rewrite_links_to_preview_namespace(self):
        source = """---
title: Install Flock
---

[Install](/docs/install/)
file: ../../../public/assets/logo.svg
"""
        output = subprocess.check_output(
            ["node", "website/scripts/prepare-docs.mjs", "--rewrite-preview-doc-fixture"],
            input=source,
            text=True,
        )
        self.assertIn("[Install](/docs/preview/install/)", output)
        self.assertIn("file: ../../../../public/assets/logo.svg", output)
        self.assertIn("Preview docs describe unreleased preview builds", output)


class ConventionalCommitTests(unittest.TestCase):
    def test_valid_subjects_allow_scopes_and_bang(self):
        self.assertTrue(conventional_commits.valid_subject("fix(update): handle preview"))
        self.assertTrue(conventional_commits.valid_subject("feat!: change config"))
        self.assertFalse(conventional_commits.valid_subject("update preview channel"))

    def test_commit_message_subject_skips_comments(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "COMMIT_EDITMSG"
            path.write_text(
                "\n# Please enter the commit message\n\nfix(update): switch channel\n",
                encoding="utf-8",
            )
            self.assertEqual(
                conventional_commits.commit_message_subject(path),
                "fix(update): switch channel",
            )

    def test_classify_commit_reads_type_scope_and_breaking(self):
        self.assertEqual(
            conventional_commits.classify_commit("feat(update): offer a channel"),
            ("feat", False),
        )
        self.assertEqual(
            conventional_commits.classify_commit("feat(update)!: offer a channel"),
            ("feat", True),
        )
        self.assertEqual(
            conventional_commits.classify_commit("refactor: rewrite the reader", "BREAKING CHANGE: gone"),
            ("refactor", True),
        )

    def test_classify_commit_rejects_what_ci_rejects(self):
        self.assertEqual(conventional_commits.classify_commit("update preview channel"), (None, False))
        # A well-formed subject of a type outside the allowed set is not a
        # conventional commit here either, so it plans like an unclassified one.
        self.assertEqual(conventional_commits.classify_commit("wip: try again"), (None, False))

    def test_recommended_bump_takes_the_strongest_signal(self):
        def commits(*subjects: str) -> list[conventional_commits.Commit]:
            return [
                conventional_commits.Commit(sha=f"{index:040x}", subject=subject, body="")
                for index, subject in enumerate(subjects)
            ]

        self.assertIsNone(conventional_commits.recommended_bump(commits()))
        self.assertEqual(
            conventional_commits.recommended_bump(commits("fix: a", "docs: b")), "patch"
        )
        self.assertEqual(
            conventional_commits.recommended_bump(commits("fix: a", "feat: b")), "minor"
        )
        self.assertEqual(
            conventional_commits.recommended_bump(commits("feat: b", "fix!: c")), "major"
        )


class PreviewManifestOwnershipTests(unittest.TestCase):
    """The preview twin of the stable archive's foreign-asset guard."""

    def archived_build(self, build_id: str, repo: str, tag_name: str) -> dict:
        return {
            "base_version": "0.6.7",
            "commit": "0123456789abcdef0123456789abcdef01234567",
            "built_at": "2026-06-02T03:00:00Z",
            "protocol": 12,
            "tag": tag_name,
            "assets": {
                target: {"url": f"https://github.com/{repo}/releases/download/{tag_name}/flock-{target}"}
                for target in ("linux-x86_64", "linux-aarch64", "macos-x86_64", "macos-aarch64")
            },
        }

    def test_builds_from_another_repository_are_named(self):
        builds = {
            "2026-06-02-abcdef123456": self.archived_build(
                "2026-06-02-abcdef123456", "gerchowl/flock", "preview-2026-06-02-abcdef123456"
            ),
            "2026-06-03-854fc3c1aff5": self.archived_build(
                "2026-06-03-854fc3c1aff5", "example.invalid/another-project", "preview-2026-06-03-854fc3c1aff5"
            ),
        }

        self.assertEqual(
            preview.foreign_build_entries(builds, "gerchowl/flock"),
            ["2026-06-03-854fc3c1aff5"],
        )

    def test_build_manifest_drops_archived_builds_from_another_repository(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "preview.json"
            output.write_text(
                json.dumps(
                    {
                        "builds": {
                            "2026-06-03-854fc3c1aff5": self.archived_build(
                                "2026-06-03-854fc3c1aff5",
                                "example.invalid/another-project",
                                "preview-2026-06-03-854fc3c1aff5",
                            )
                        }
                    }
                ),
                encoding="utf-8",
            )

            data = json.loads(
                build_manifest_in(
                    tmp,
                    tag="preview-2026-07-01-0123456789ab",
                    build_id="2026-07-01-0123456789ab",
                    commit="0123456789abcdef0123456789abcdef01234567",
                    built_at="2026-07-01T03:00:00Z",
                )
            )

        self.assertEqual(list(data["builds"]), ["2026-07-01-0123456789ab"])

    def test_build_manifest_keeps_this_repositorys_own_builds(self):
        with tempfile.TemporaryDirectory() as tmp:
            first = json.loads(build_manifest_in(tmp, built_at="2026-06-02T03:00:00Z"))
            (Path(tmp) / "preview.json").write_text(json.dumps(first), encoding="utf-8")

            data = json.loads(
                build_manifest_in(
                    tmp,
                    tag="preview-2026-07-01-0123456789ab",
                    build_id="2026-07-01-0123456789ab",
                    commit="0123456789abcdef0123456789abcdef01234567",
                    built_at="2026-07-01T03:00:00Z",
                )
            )

        self.assertEqual(
            sorted(data["builds"]),
            ["2026-06-02-abcdef123456", "2026-07-01-0123456789ab"],
        )


class PreviewHistoryBaseTests(unittest.TestCase):
    """The preview notes are diffed from the previous build — which must exist here."""

    def write_manifest(self, tmp: str, commit: str | None) -> Path:
        path = Path(tmp) / "preview.json"
        path.write_text(json.dumps({"commit": commit}), encoding="utf-8")
        return path

    def test_a_commit_this_repository_does_not_have_is_not_a_previous_preview(self):
        # The manifest's recorded sha is what the next preview's notes are diffed
        # from, and `git log <sha>..<commit>` fails outright on a sha this
        # repository cannot resolve — a preview run that dies before it publishes.
        absent = "0" * 40
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            manifest = self.write_manifest(tmp, absent)

            self.assertIsNone(preview.previous_preview_commit(manifest, repo))
            with self.assertRaises(subprocess.CalledProcessError):
                preview.commit_subjects(absent, absent, repo)

    def test_the_previous_preview_commit_is_used_when_it_exists(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            marker = commit(repo, "feat: a feature")
            manifest = self.write_manifest(tmp, marker)

            self.assertEqual(preview.previous_preview_commit(manifest, repo), marker)
            self.assertEqual(preview.preview_base_ref(manifest, repo), marker)

    def test_the_base_ref_falls_back_to_the_first_commit_without_a_tag(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            commit(repo, "feat: a feature")
            manifest = self.write_manifest(tmp, None)

            self.assertIsNone(preview.latest_stable_tag(repo))
            self.assertEqual(preview.preview_base_ref(manifest, repo), preview.first_commit(repo))

    def test_the_base_ref_prefers_the_last_stable_tag_over_the_first_commit(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            commit(repo, "chore: the very first commit")
            tag(repo, "v1.0.0")
            commit(repo, "feat: a feature")
            manifest = self.write_manifest(tmp, None)

            self.assertEqual(preview.preview_base_ref(manifest, repo), "v1.0.0")

    def test_notes_for_a_first_preview_cover_the_whole_history(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            first = preview.first_commit(repo)
            head = commit(repo, "feat: a feature")

            notes = preview.build_notes(None, head, "2026-07-01-0123456789ab", "1.0.0", "gerchowl/flock", repo)

        self.assertIn("### Added", notes)
        self.assertIn("A feature", notes)
        # A compare link naming `None` 404s for every reader.
        self.assertIn(f"{first}...{head}", notes)
        self.assertNotIn("None", notes)


if __name__ == "__main__":
    unittest.main()
