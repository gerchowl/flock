"""Keep every CI nextest install connected to the devShell's shared pin."""

import copy
import json
import os
import re
import tempfile
from pathlib import Path
import subprocess
import unittest

from scripts.nextest_version import read_version

ROOT = Path(__file__).resolve().parents[1]
PIN = 'just,cargo-nextest@${{ steps.nextest-version.outputs.version }}'
NIX_READ = 'builtins.replaceStrings [ "\\n" ] [ "" ] (builtins.readFile ./nextest-version)'


def workflow(name):
    return json.loads(subprocess.check_output([
        "ruby", "-ryaml", "-rjson", "-e",
        "puts JSON.generate(YAML.load_file(ARGV[0]))",
        str(ROOT / ".github/workflows" / name),
    ], text=True))


class SharedPin(unittest.TestCase):
    def test_nix_reads_the_shared_pin(self):
        source = (ROOT / "nix/nextest.nix").read_text()
        assignments = re.findall(r'^\s*version\s*=\s*(.+);', source, re.MULTILINE)
        self.assertEqual(assignments, [NIX_READ])
        self.assertIn('(callPackage ./nix/nextest.nix { })', (ROOT / 'flake.nix').read_text())

    def test_nix_source_cache_identity_includes_version(self):
        source = (ROOT / "nix/nextest.nix").read_text()
        fetch = re.search(r"src = fetchFromGitHub \{(.*?)\n  \};", source, re.DOTALL).group(1)
        names = re.findall(r'^\s*name\s*=\s*"([^"]+)";', fetch, re.MULTILINE)
        self.assertEqual(names, ["cargo-nextest-${version}-source"])

    def assert_install_pin(self, steps, index):
        step = steps[index]
        self.assertEqual(step["with"]["tool"], PIN)
        readers = [s for s in steps[:index] if s.get("id") == "nextest-version"]
        self.assertEqual(len(readers), 1)
        reader = readers[0]
        self.assertEqual(reader.get("if"), step.get("if"))
        self.assertEqual(reader["shell"], "bash")
        self.assertIn('version="$(python3 scripts/nextest_version.py)"', reader["run"])
        self.assertIn('echo "version=$version" >> "$GITHUB_OUTPUT"', reader["run"])

    def test_every_ci_install_reads_the_shared_pin(self):
        found = []
        paths = sorted((ROOT / ".github/workflows").glob("*.y*ml"))
        for path in paths:
            for name, job in workflow(path.name)["jobs"].items():
                steps = job.get("steps", [])
                for index, step in enumerate(steps):
                    tools = step.get("with", {}).get("tool", "")
                    if "cargo-nextest" not in tools:
                        continue
                    found.append((path.name, name))
                    with self.subTest(workflow=path.name, job=name):
                        self.assert_install_pin(steps, index)
        self.assertEqual(found, [("ci-leg.yml", "build"), ("preview.yml", "preflight")])

    def test_archive_consumer_uses_the_nix_shared_pin(self):
        steps = workflow("ci-leg.yml")["jobs"]["test"]["steps"]
        run = next(s["run"] for s in steps if s.get("name") == "Run shard")
        self.assertTrue(run.startswith("nix develop -c just ci-shard "))
        self.test_nix_reads_the_shared_pin()

    def test_unpinned_literal_and_wrong_output_versions_fail(self):
        steps = workflow("ci-leg.yml")["jobs"]["build"]["steps"]
        index = next(i for i, step in enumerate(steps) if "cargo-nextest" in step.get("with", {}).get("tool", ""))
        for bad in ["just,cargo-nextest", "just,cargo-nextest@0.9.146", "just,cargo-nextest@${{ steps.other.outputs.version }}"]:
            with self.subTest(tool=bad):
                changed = copy.deepcopy(steps)
                changed[index]["with"]["tool"] = bad
                with self.assertRaises(AssertionError):
                    self.assert_install_pin(changed, index)

    def test_missing_late_or_differently_conditioned_reader_fails(self):
        steps = workflow("preview.yml")["jobs"]["preflight"]["steps"]
        index = next(i for i, step in enumerate(steps) if "cargo-nextest" in step.get("with", {}).get("tool", ""))
        reader_index = next(i for i, step in enumerate(steps) if step.get("id") == "nextest-version")
        for kind in ["missing", "late", "condition"]:
            with self.subTest(kind=kind):
                changed = copy.deepcopy(steps)
                if kind == "condition":
                    changed[reader_index].pop("if")
                else:
                    reader = changed.pop(reader_index)
                    if kind == "late":
                        changed.append(reader)
                install = next(i for i, step in enumerate(changed) if "cargo-nextest" in step.get("with", {}).get("tool", ""))
                with self.assertRaises(AssertionError):
                    self.assert_install_pin(changed, install)

    def test_workflow_output_matches_the_nix_pin(self):
        expected = read_version()
        for filename, job in [("ci-leg.yml", "build"), ("preview.yml", "preflight")]:
            reader = next(s for s in workflow(filename)["jobs"][job]["steps"] if s.get("id") == "nextest-version")
            with tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "output"
                subprocess.run(["bash", "-e", "-o", "pipefail", "-c", reader["run"]],
                               cwd=ROOT, env={**os.environ, "GITHUB_OUTPUT": str(output)}, check=True)
                self.assertEqual(output.read_text(), f"version={expected}\n")


class VersionFile(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "nix").mkdir()
        (self.root / ".config").mkdir()
        (self.root / ".config/nextest.toml").write_text('nextest-version = "0.9.145"\n')

    def test_release_pin_with_or_without_newline(self):
        for text in ["0.9.146", "0.9.146\n"]:
            (self.root / "nix/nextest-version").write_text(text)
            self.assertEqual(read_version(self.root), "0.9.146")

    def test_invalid_partial_or_multiline_pins_fail(self):
        for text in ["", "latest", "0.9", "v0.9.146", "0.9.146-beta", " 0.9.146", "0.9.146\n\n", "0.9.146\r\n", "0.9.146\n0.9.147", "0.09.146"]:
            with self.subTest(pin=text):
                (self.root / "nix/nextest-version").write_text(text)
                with self.assertRaisesRegex(ValueError, "invalid nextest release version"):
                    read_version(self.root)

    def test_pin_below_repo_minimum_fails(self):
        (self.root / "nix/nextest-version").write_text("0.9.144\n")
        with self.assertRaisesRegex(ValueError, "below required 0.9.145"):
            read_version(self.root)

    def test_version_order_is_numeric_and_equality_is_allowed(self):
        (self.root / ".config/nextest.toml").write_text('nextest-version = "0.9.9"\n')
        for text in ["0.9.9", "0.9.10"]:
            (self.root / "nix/nextest-version").write_text(text)
            self.assertEqual(read_version(self.root), text)

    def test_missing_pin_fails(self):
        with self.assertRaises(FileNotFoundError):
            read_version(self.root)


if __name__ == "__main__":
    unittest.main()
