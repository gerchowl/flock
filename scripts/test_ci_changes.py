import re
import unittest
from pathlib import Path

from scripts.ci_changes import allowed, may_skip


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

    def test_rename_generated_and_script_tested_paths_run_everything(self):
        # `--no-renames` lists both sides of a rename: the old path is outside the allow-list.
        self.assertFalse(self.skip(["src/x.rs", "docs/x.rs"]))
        self.assertFalse(self.skip(["web/client/src/gen/types.ts"]))
        self.assertFalse(self.skip(["website/scripts/prepare-docs.mjs"]))
        self.assertFalse(self.skip(["web/client/src/wire/connection.ts", "web/client/tests/wire/fleet-server.ts"]))
        # Safe only because script-tests run in a job that is never skipped.
        self.assertTrue(self.skip(["README.md"]))

    def test_never_on_batches_or_non_pr_events(self):
        self.assertFalse(self.skip(["docs/a.md"], head="land/b231"))
        self.assertFalse(self.skip(["docs/a.md"], head="LAND/b231"))
        for event in ("push", "merge_group", "workflow_dispatch", ""):
            self.assertFalse(self.skip(["docs/a.md"], event=event))


if __name__ == "__main__":
    unittest.main()


class RustReaders(unittest.TestCase):
    """Anything Rust reads or spawns under a skippable prefix, plus its TS import closure, must be denied."""

    ROOT = Path(__file__).resolve().parents[1]
    LITERAL = re.compile(r'"((?:web/client|website|docs)/[A-Za-z0-9_./-]*)"')
    # Bare root names ("README.md") are mostly temp-repo fixtures; count them only on lines that
    # read from the checkout itself.
    ROOT_MD = re.compile(r'"([A-Za-z0-9_-]+\.md)"')
    # include_str!("../../web/client/...") relative forms and join("web/client/...") both contain the literal.
    REL_LITERAL = re.compile(r'"(?:\.\./)+((?:web/client|website|docs)/[A-Za-z0-9_./-]*)"')
    IMPORT = re.compile(r'''(?:from\s+|import\s*\(\s*|import\s+)["'](\.[^"']*)["']''')

    def rust_sources(self):
        for base in ("src", "tests"):
            yield from (self.ROOT / base).rglob("*.rs")
        yield self.ROOT / "build.rs"

    def referenced_files(self):
        found = set()
        for source in self.rust_sources():
            if source.is_relative_to(self.ROOT / "tests" / "fixtures"):
                continue
            text = source.read_text(errors="replace")
            literals = self.LITERAL.findall(text) + self.REL_LITERAL.findall(text)
            for line in text.splitlines():
                if "CARGO_MANIFEST_DIR" in line or "include_str!" in line or "include_bytes!" in line:
                    literals += self.ROOT_MD.findall(line)
            for literal in literals:
                target = self.ROOT / literal
                if target.is_file():
                    found.add(target)
                elif target.is_dir():
                    found.update(p for p in target.rglob("*") if p.is_file())
        return found

    def resolve(self, importer, spec):
        base = (importer.parent / spec).resolve()
        for candidate in (base, *(base.with_suffix(s) for s in (".ts", ".tsx", ".js", ".mjs", ".json")),
                          *(base / f"index{s}" for s in (".ts", ".tsx", ".js"))):
            if candidate.is_file():
                return candidate
        return None

    def closure(self, roots):
        seen, todo = set(), [r for r in roots if r.suffix in (".ts", ".tsx", ".js", ".mjs")]
        while todo:
            current = todo.pop()
            if current in seen:
                continue
            seen.add(current)
            for spec in self.IMPORT.findall(current.read_text(errors="replace")):
                resolved = self.resolve(current, spec)
                self.assertIsNotNone(resolved, f"unresolvable import {spec!r} in {current}")
                todo.append(resolved)
        return seen

    def test_every_file_rust_reads_or_spawns_and_its_imports_are_not_skippable(self):
        referenced = self.referenced_files()
        self.assertTrue(any(p.name == "fleet-server.ts" for p in referenced), "scan lost the bun-spawned scripts")
        reachable = referenced | self.closure(referenced)
        bad = sorted(str(p.relative_to(self.ROOT)) for p in reachable if allowed(str(p.relative_to(self.ROOT))))
        self.assertEqual(bad, [], "Rust reads/spawns these (directly or via imports) but ci_changes.py "
                                  "would let a PR skip the Rust legs: extend DENY_*")
