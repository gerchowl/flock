import unittest

from scripts.ci_changes import may_skip


class CiChanges(unittest.TestCase):
    def skip(self, paths, event="pull_request", head="issue/1-x"):
        return may_skip(event, head, paths)

    def test_docs_website_markdown_and_web_client_skip(self):
        for paths in (["docs/guide.md"], ["website/index.html", "README.md"],
                      ["web/client/src/app.ts", "web/client/bun.lock"],
                      ["docs/a.md", "web/client/src/a.ts"]):
            self.assertTrue(self.skip(paths), paths)

    def test_anything_else_runs_everything(self):
        for paths in (["src/main.rs"], ["docs/a.md", "Cargo.toml"], ["scripts/ci_shards.py"],
                      ["workflows/ci.yml"], ["docs/next/changes/1.fixed.md"],
                      ["web/client/test/fixtures/wire/x.jsonl"], ["website/latest.json"],
                      ["CHANGELOG.md"], ["nested/NOTES.md"], ["flake.nix"], [], [""]):
            self.assertFalse(self.skip(paths), paths)

    def test_never_on_batches_or_non_pr_events(self):
        self.assertFalse(self.skip(["docs/a.md"], head="land/b231"))
        self.assertFalse(self.skip(["docs/a.md"], head="LAND/b231"))
        for event in ("push", "merge_group", "workflow_dispatch", ""):
            self.assertFalse(self.skip(["docs/a.md"], event=event))


if __name__ == "__main__":
    unittest.main()
