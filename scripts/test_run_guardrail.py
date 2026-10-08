"""Hook tooling diagnostics and transparent execution without a devShell."""

from pathlib import Path
import json
import shlex
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
WRAPPER = ROOT / "scripts/run_guardrail.sh"


class RunGuardrailTests(unittest.TestCase):
    def test_missing_tool_names_recovery_commands(self):
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run(
                ["/bin/sh", str(WRAPPER), "guardrails-no-fake-impl"],
                env={"PATH": directory}, capture_output=True, text=True,
            )
        self.assertEqual(result.returncode, 127)
        self.assertEqual(result.stdout, "")
        self.assertIn("guardrails-no-fake-impl not on PATH", result.stderr)
        self.assertIn("nix develop", result.stderr)
        self.assertIn("hook tooling", result.stderr)
        self.assertNotIn("git commit", result.stderr)
        self.assertIn("direnv allow", result.stderr)
        self.assertIn("direnv-enabled shell", result.stderr)

    def test_forwards_arguments_environment_and_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            tool = Path(directory) / "guardrails-no-fake-impl"
            tool.write_text(
                '#!/bin/sh\nprintf "%s\\n" "$GUARDRAILS_TRACE_ALLOW_GLOBS" "$@"\nexit 23\n'
            )
            tool.chmod(0o755)
            result = subprocess.run(
                ["/bin/sh", str(WRAPPER), tool.name, "file with spaces.rs", "--flag"],
                env={"PATH": directory, "GUARDRAILS_TRACE_ALLOW_GLOBS": "*/logging.rs"},
                capture_output=True, text=True,
            )
        self.assertEqual(result.returncode, 23)
        self.assertEqual(result.stdout.splitlines(), ["*/logging.rs", "file with spaces.rs", "--flag"])
        self.assertEqual(result.stderr, "")

    def test_missing_tool_runs_nix_with_repo_arguments_and_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            nix = Path(directory) / "nix"
            nix.write_text(
                '#!/bin/sh\nprintf "%s\\n" "$FLOCK_GUARDRAIL_IN_DEVSHELL" '
                '"$GUARDRAILS_TRACE_ALLOW_GLOBS" "$@"\nexit 23\n'
            )
            nix.chmod(0o755)
            result = subprocess.run(
                ["/bin/sh", str(WRAPPER), "guardrails-no-fake-impl", "file with spaces.rs", "--flag"],
                cwd=directory,
                env={"PATH": directory + ":/usr/bin:/bin", "GUARDRAILS_TRACE_ALLOW_GLOBS": "*/logging.rs"},
                capture_output=True, text=True,
            )
        self.assertEqual(result.returncode, 23)
        self.assertEqual(result.stdout.splitlines(), [
            "1", "*/logging.rs", "develop", str(ROOT), "--command",
            "guardrails-no-fake-impl", "file with spaces.rs", "--flag",
        ])
        self.assertEqual(result.stderr, "")

    def test_old_python_uses_dev_shell(self):
        with tempfile.TemporaryDirectory() as directory:
            python = Path(directory) / "python3"
            python.write_text("#!/bin/sh\nexit 1\n")
            python.chmod(0o755)
            nix = Path(directory) / "nix"
            nix.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n')
            nix.chmod(0o755)
            result = subprocess.run(
                ["/bin/sh", str(WRAPPER), "python3", "scripts/fixture_hosts.py"],
                env={"PATH": directory + ":/usr/bin:/bin"},
                capture_output=True, text=True,
            )
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout.splitlines(), [
            "develop", str(ROOT), "--command", "python3", "scripts/fixture_hosts.py",
        ])

    def test_dev_shell_guard_prevents_recursive_fallback(self):
        with tempfile.TemporaryDirectory() as directory:
            nix = Path(directory) / "nix"
            nix.write_text('#!/bin/sh\nprintf "unexpected nix invocation\\n"\nexit 42\n')
            nix.chmod(0o755)
            result = subprocess.run(
                ["/bin/sh", str(WRAPPER), "guardrails-no-fake-impl"],
                env={"PATH": directory, "FLOCK_GUARDRAIL_IN_DEVSHELL": "1"},
                capture_output=True, text=True,
            )
        self.assertEqual(result.returncode, 127)
        self.assertEqual(result.stdout, "")
        self.assertIn("hook tooling", result.stderr)

    def test_every_dev_shell_only_hook_uses_wrapper(self):
        # Psych parses the YAML structure, including quoted and multiline entries.
        parsed = subprocess.run(
            ["ruby", "-ryaml", "-rjson", "-e",
             "puts JSON.generate(YAML.load_file(ARGV.fetch(0)))",
             str(ROOT / ".pre-commit-config.yaml")],
            capture_output=True, text=True, check=True,
        )
        config = json.loads(parsed.stdout)
        hooks = [hook for repo in config["repos"] for hook in repo["hooks"]]
        wrapped = []
        for hook in hooks:
            entry = shlex.split(hook["entry"])
            if entry[0] == "env":
                entry = entry[1:]
                while entry and "=" in entry[0]:
                    entry = entry[1:]
            with self.subTest(hook=hook["id"]):
                self.assertEqual(entry[:2], ["sh", "scripts/run_guardrail.sh"])
                self.assertTrue(entry[2].startswith("guardrails-") or entry[2] in {"cargo", "gitleaks", "python3"})
                wrapped.append(hook["id"])
        self.assertTrue({"gitleaks", "rustfmt", "clippy", "cargo-deny"}.issubset(wrapped))
        self.assertEqual(len(wrapped), 21)
