"""Tests for `scripts/ssh_hosts_gate.py`.

Hermetic: a throwaway `$HOME` with a synthetic `~/.ssh` and synthetic command
output, so nothing here reads the developer's real machines. The one test that
touches the real module is the source-level one, which pins the properties the
gate depends on rather than any particular label.
"""

import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "ssh_hosts_gate.py"


def load_module():
    spec = importlib.util.spec_from_file_location("ssh_hosts_gate", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class DestinationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_module()

    def test_user_port_and_dotted_form(self) -> None:
        self.assertEqual(
            self.mod._split_destination("operator@kiln.tail1234.ts.net:22"),
            ["kiln.tail1234.ts.net", "kiln"],
        )

    def test_a_jump_chain_yields_one_entry_per_hop(self) -> None:
        self.assertEqual(
            sorted(self.mod._split_destination("a.local,b.local")),
            ["a", "a.local", "b", "b.local"],
        )

    def test_a_phrase_yields_nothing_host_shaped(self) -> None:
        # "via hub" is prose in a routing field; `hub` alone is not a destination.
        self.assertEqual(self.mod._split_destination("via hub"), ["via", "hub"])
        self.assertEqual(self.mod._split_destination("not a summary")[:1], ["not"])

    def test_an_ssh_option_is_never_a_destination(self) -> None:
        # #392 exists because OpenSSH reads a leading `-` as an option.
        self.assertEqual(self.mod._split_destination("-oProxyCommand=id"), [])

    def test_a_public_service_is_not_a_machine(self) -> None:
        # `github.com` is in everyone's known_hosts, and a hosted GitLab can sit
        # in a personal ssh config; expanding either to a bare label would flag
        # every git URL in the tree.
        for value in ("git@github.com", "github.com", "gitlab.psi.ch"):
            self.assertEqual(self.mod._split_destination(value), [], value)

    def test_a_tailnet_name_is_a_machine(self) -> None:
        self.assertEqual(
            self.mod._split_destination("atlas.tail1234.ts.net"),
            ["atlas.tail1234.ts.net", "atlas"],
        )


class CandidateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_module()
        self._tmp = tempfile.TemporaryDirectory()
        self.home = Path(self._tmp.name)
        (self.home / ".ssh").mkdir()

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_config_aliases_hostnames_and_jump_hops_are_candidates(self) -> None:
        (self.home / ".ssh" / "config").write_text(
            "Host atlas\n  HostName atlas.tail1234.ts.net\n"
            "Host spoke*\n  ProxyJump bastion\n"
            "Host !bad\n",
            encoding="utf-8",
        )
        found = self.mod.candidates(self.home)
        self.assertIn("atlas", found)
        self.assertIn("atlas.tail1234.ts.net", found)
        self.assertIn("bastion", found)
        # A wildcard alias names no literal host, so there is nothing to protect.
        self.assertFalse([label for label in found if label.startswith("spoke")])

    def test_plaintext_known_hosts_entries_are_candidates(self) -> None:
        (self.home / ".ssh" / "known_hosts").write_text(
            "kiln ssh-ed25519 AAAA\n", encoding="utf-8"
        )
        self.assertIn("kiln", self.mod.candidates(self.home))

    def test_hashed_entries_yield_no_label_and_are_not_an_error(self) -> None:
        (self.home / ".ssh" / "known_hosts").write_text(
            "|1|YWVzLXNhbHQ=|aGFzaGVk a1 ssh-ed25519 AAAA\n", encoding="utf-8"
        )
        self.assertNotIn("|1|", self.mod.candidates(self.home))

    def test_no_ssh_state_means_no_candidates(self) -> None:
        # Failing open is deliberate: this protects one person's fleet, and a
        # contributor has none of it.
        self.assertEqual(self.mod.candidates(self.home), {})


class IconSuppressionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_module()

    def test_the_registry_vocabulary_is_read(self) -> None:
        names = self.mod.icon_names(ROOT)
        self.assertIn("laptop", names)
        self.assertIn("cat", names)

    def test_an_icon_name_is_never_a_candidate(self) -> None:
        self.assertFalse(self.mod._usable("cat", self.mod.icon_names(ROOT)))
        self.assertFalse(self.mod._usable("laptop", self.mod.icon_names(ROOT)))


class CheckTests(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_module()
        self.labels = {"kiln": {"known_hosts"}, "atlas": {"tailscale peer"}}

    def _check(self, line: str, name: str = "src/peers.rs"):
        return self.mod.check_text(line + "\n", Path(name), self.labels)

    def test_ssh_destination_is_flagged(self) -> None:
        # Two rules legitimately match one destination (`ssh = "..."` and
        # `user@host`); what matters is that the label is named, once per rule.
        findings = self._check('let ssh = "operator@kiln";')
        self.assertEqual({f[2] for f in findings}, {"kiln"})
        self.assertEqual({f[1] for f in findings}, {"ssh-target", "user-at-host"})

    def test_reported_host_field_is_flagged(self) -> None:
        findings = self._check('peer.host = Some("atlas".into());')
        self.assertEqual([f[2] for f in findings], ["atlas"])

    def test_an_agent_id_host_component_is_flagged(self) -> None:
        findings = self._check('let id = "agent_kiln_1";')
        self.assertEqual([f[2] for f in findings], ["kiln"])

    def test_prose_without_a_host_position_is_not_flagged(self) -> None:
        self.assertEqual(self._check("// the kiln sorts first"), [])
        self.assertEqual(self._check("let pane_id = 3;"), [])

    def test_the_escape_hatch_covers_the_line_and_the_one_above(self) -> None:
        self.assertEqual(self._check('let ssh = "operator@kiln"; // guardrails-ok(ssh-hosts): x'), [])
        self.assertEqual(self._check("// guardrails-ok(ssh-hosts): x\nlet ssh = \"operator@kiln\";"), [])

    def test_no_labels_means_no_findings(self) -> None:
        self.assertEqual(self.mod.check_text('let ssh = "operator@kiln";\n', Path("a.rs"), {}), [])

    def test_an_unrelated_label_is_not_flagged(self) -> None:
        self.assertEqual(self._check('let ssh = "operator@elsewhere";'), [])

    def test_a_suffixed_alias_still_names_its_machine(self) -> None:
        findings = self._check('let ssh = "operator@kiln-dev";')
        self.assertIn("kiln", {f[2] for f in findings})


class ProcessTests(unittest.TestCase):
    def test_it_is_clean_when_there_is_no_local_ssh_state(self) -> None:
        # Fails open, so a contributor with no fleet is not blocked. PATH points
        # at an empty dir and HOME at a temp one, so `uname`, `scutil` and
        # `tailscale` are all unreachable and only the gate's own logic runs.
        import os
        import sys as _sys
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            empty = Path(tmp) / "bin"
            empty.mkdir()
            result = subprocess.run(
                [_sys.executable, str(SCRIPT), "--verbose", str(ROOT / "README.md")],
                capture_output=True,
                text=True,
                env={**os.environ, "PATH": str(empty), "HOME": tmp},
            )
        self.assertEqual(result.returncode, 0)
        self.assertIn("fails open", result.stdout)


class SourceTests(unittest.TestCase):
    """The properties the gate's trustworthiness rests on."""

    def setUp(self) -> None:
        self.source = SCRIPT.read_text(encoding="utf-8")

    def test_it_never_writes_a_file(self) -> None:
        self.assertNotIn("write_text", self.source)
        self.assertNotIn("open(", self.source)

    def test_it_states_that_it_cannot_bind_ci(self) -> None:
        self.assertIn("cannot and does not run in CI", self.source)


if __name__ == "__main__":
    unittest.main()