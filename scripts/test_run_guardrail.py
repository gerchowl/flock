"""Hook tooling diagnostics and transparent execution without a devShell."""

from pathlib import Path
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
        self.assertIn("nix develop --command git commit", result.stderr)
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

    def test_every_guardrails_hook_uses_wrapper_and_diagnoses_missing_tool(self):
        entries = [
            shlex.split(line.split("entry:", 1)[1])
            for line in (ROOT / ".pre-commit-config.yaml").read_text().splitlines()
            if "entry:" in line and "guardrails-" in line
        ]
        self.assertGreater(len(entries), 0)
        with tempfile.TemporaryDirectory() as directory:
            for entry in entries:
                with self.subTest(entry=entry):
                    index = entry.index("sh")
                    self.assertEqual(entry[index + 1], "scripts/run_guardrail.sh")
                    tool = entry[index + 2]
                    env = {"PATH": directory}
                    if entry[0] == "env":
                        key, value = entry[1].split("=", 1)
                        env[key] = value
                    result = subprocess.run(
                        ["/bin/sh", str(WRAPPER), tool], env=env,
                        capture_output=True, text=True,
                    )
                    self.assertEqual(result.returncode, 127)
                    self.assertIn(f"{tool} not on PATH", result.stderr)
