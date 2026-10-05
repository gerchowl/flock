"""Tests for `scripts/exec_name_gate.py`.

Hermetic: the gate's token set is derived from `src/cli.rs`, `src/main.rs` and
`src/cli/help.rs`, and every assertion below feeds it hand-written text rather
than reaching into a network, a $HOME, or an ambient machine.

The properties worth pinning are the **negative** ones. A gate that cannot flag a
planted violation is worthless, but a gate that fires on correct prose is worse —
it gets `--no-verify`'d within a day, and that habit then covers the real leak.
So most of what follows is `flock` in a position that must stay silent, several of
them regressions this gate actually shipped with:

* `flock` as a product noun in a doc comment or a sentence ("the focused flock
  pane", "whose namespace stays `flock`") — the reason Rust is out of scope;
* `flock(1)`, `flock-ai`, `flock.dev`, `~/.config/flock`, `FLOCK_*`;
* `flock` as a Homebrew/mise **package** name, which is the Nix `pname` and stays
  the long name even though the binary does not;
* `flock_agent_list` and friends — the MCP tool-name prefix, which is a
  `flock_`-joined token and never a command;
* a repo slug in a `--repo gerchowl/flock` argument, which the gate rewrote to a
  repository that does not exist before the rule grew a left boundary.
"""

import importlib.util
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "exec_name_gate.py"


def load_module():
    spec = importlib.util.spec_from_file_location("exec_name_gate", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class GateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_module()
        self.rules = self.mod.build_rules(self.mod.invocable_tokens())

    def findings(self, text: str):
        return self.mod.check_text(text, self.rules)

    # --- the positive cases: a real command must be flagged ------------------

    def test_a_command_in_a_code_block_is_flagged(self) -> None:
        self.assertTrue(self.findings("```bash\nflock pane list\n```"))

    def test_a_command_behind_a_shell_prompt_is_flagged(self) -> None:
        self.assertTrue(self.findings("```\n$ flock server stop\n```"))

    def test_the_bare_launch_is_flagged(self) -> None:
        self.assertTrue(self.findings("```bash\nflock\n```"))

    def test_an_inline_command_span_is_flagged(self) -> None:
        self.assertTrue(self.findings("run `flock update` to retry"))

    def test_an_inline_flag_is_flagged(self) -> None:
        self.assertTrue(self.findings("the `flock --remote` workflow"))

    def test_a_group_with_no_verb_is_flagged(self) -> None:
        self.assertTrue(self.findings("`flock server` runs the headless server"))

    # --- the negative cases: correct prose must stay silent ------------------

    def test_a_bare_backticked_name_in_markdown_is_presumed_a_command(self) -> None:
        """The heuristic's actual boundary, pinned rather than hidden.

        Prose in a user- or agent-facing doc is written as instruction, and all 20
        bare backticked uses of the long name in this tree are commands ("run
        `flock`", "your shell cannot find `flock`"). So the gate presumes command
        there. The same construct in a Rust doc comment is description, which is
        why Rust is out of scope — see the module docstring.

        Where the presumption is wrong, the escape hatch is the release valve, and
        the one place it was needed is the skill's `name:` in SKILL.md's
        frontmatter.
        """
        self.assertTrue(self.findings("run `flock` again to reattach"))
        self.assertTrue(self.findings("your shell cannot find `flock` on PATH"))
        self.assertEqual(
            self.findings(
                "install that file as a skill named `flock`  "
                "# guardrails-ok(exec-name): that is the skill's name: field"
            ),
            [],
        )

    def test_the_product_noun_outside_backticks_is_not_a_command(self) -> None:
        # Verbatim from the tree and correct as written: the product noun.
        for sentence in (
            "you are not running inside a flock-managed pane and stop.",
            "do not inspect or control the focused flock pane from outside flock.",
            "Flock checks the remote platform, then checks ~/.local/bin",
        ):
            with self.subTest(sentence=sentence):
                self.assertEqual(self.findings(sentence), [])

    def test_a_package_name_is_not_a_command(self) -> None:
        # The Nix `pname` is still the long name; only the binary was renamed.
        for line in ("brew install flock", "mise use -g flock", "brew upgrade flock"):
            with self.subTest(line=line):
                self.assertEqual(self.findings(f"```bash\n{line}\n```"), [])

    def test_a_repo_slug_is_not_a_command(self) -> None:
        # Regression: the rule had no left boundary, so a flag after the slug
        # matched and `--repo gerchowl/flock` became a repository that is not real.
        self.assertEqual(
            self.findings(
                '```bash\ngh issue create --repo gerchowl/flock --title "x"\n```'
            ),
            [],
        )

    def test_a_build_artifact_path_is_out_of_scope_and_silent(self) -> None:
        # Stated in the module docstring as a deliberate gap rather than hidden:
        # the gate judges invocation position, not filesystem paths.
        self.assertEqual(self.findings("```bash\n./target/release/flock\n```"), [])

    def test_brand_and_namespace_forms_are_exempt(self) -> None:
        for token in ("flock(1)", "flock-ai", "flock.dev", "~/.config/flock"):
            with self.subTest(token=token):
                self.assertEqual(self.findings(f"see {token} for details"), [])

    def test_the_mcp_tool_prefix_is_not_a_command(self) -> None:
        self.assertEqual(self.findings("call `flock_agent_list` first"), [])

    def test_env_vars_are_exempt(self) -> None:
        self.assertEqual(self.findings("only when FLOCK_ENV=1 is set"), [])

    def test_prose_naming_a_group_is_not_a_command(self) -> None:
        # The measured false positive that scoped the whole design: in ordinary
        # prose "flock pane", "flock server" and "flock session" are product
        # noun phrases, and a whole-word rule flags every one of them.
        self.assertEqual(self.findings("the focused flock pane from outside"), [])
        self.assertEqual(self.findings("each flock session runs a server"), [])

    # --- the escape hatch and the installer assertion ------------------------

    def test_the_escape_hatch_silences_one_line(self) -> None:
        self.assertEqual(
            self.findings(
                "flock pane list  # guardrails-ok(exec-name): quoting a fixture"
            ),
            [],
        )

    def test_the_installer_must_install_the_built_binary_name(self) -> None:
        # The P0 itself: two literals, two files, no shared source.
        self.assertEqual(self.mod.installer_matches_bin_name(), [])

    def test_a_mismatched_installer_is_reported(self) -> None:
        built = self.mod.ROOT / "Cargo.toml"
        install = self.mod.ROOT / "website" / "install.sh"
        original = install.read_text(encoding="utf8")
        try:
            install.write_text(
                original.replace('BIN="flk"', 'BIN="flock"', 1), encoding="utf8"
            )
            self.assertTrue(self.mod.installer_matches_bin_name())
        finally:
            install.write_text(original, encoding="utf8")
        self.assertTrue(built.is_file())

    # --- the tree itself ----------------------------------------------------

    def test_the_repository_tree_is_clean(self) -> None:
        """The gate's own verdict on the committed tree, plus its exit code."""
        result = subprocess.run(
            [sys.executable, str(SCRIPT)], capture_output=True, text=True, cwd=ROOT
        )
        self.assertEqual(
            result.returncode,
            0,
            msg=f"exec-name gate is not clean:\n{result.stderr}",
        )


if __name__ == "__main__":
    unittest.main()