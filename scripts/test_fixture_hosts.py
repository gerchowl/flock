"""Tests for `scripts/fixture_hosts.py` and the declaration it reads.

Hermetic: temp dirs and inline fixtures only. The one thing deliberately NOT
faked is the declaration file, because its absence must be a hard error rather
than an empty allow-list that silently passes everything.
"""

import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "fixture_hosts.py"
DECLARATION = ROOT / "scripts" / "fixture-hosts.toml"


def load_module():
    spec = importlib.util.spec_from_file_location("fixture_hosts", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    sys_path_added = False
    try:
        spec.loader.exec_module(module)
    except ImportError:
        import sys

        sys.path.insert(0, str(ROOT / "scripts"))
        sys_path_added = True
        spec.loader.exec_module(module)
        if sys_path_added:
            sys.path.pop(0)
    return module


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


class DeclarationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_module()

    def test_the_repository_declares_its_own_fixtures(self) -> None:
        labels = self.mod.declared_labels(DECLARATION)
        self.assertTrue(labels, "the declaration must not be empty")
        self.assertIn("kiln", labels)
        # A declared host that resolves is a declaration that leaked.
        self.assertTrue(
            all(
                "." not in label
                or label.endswith((".invalid", ".test", ".example", ".ts.net"))
                for label in labels
            ),
            "every declared FQDN must be unresolvable or an invented MagicDNS name",
        )

    def test_a_missing_declaration_is_a_hard_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(SystemExit):
                self.mod.declared_labels(Path(tmp) / "absent.toml")

    def test_an_unparseable_declaration_is_a_hard_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bad = Path(tmp) / "bad.toml"
            write(bad, "[hosts\nbroken")
            with self.assertRaises(SystemExit):
                self.mod.declared_labels(bad)

    def test_an_empty_table_is_refused_by_the_rust_test_not_silently_passed(self) -> None:
        # The python side treats an empty table as "nothing declared"; the Rust
        # invariant test in src/server_icons.rs is what makes that a failure, so
        # this documents the division rather than duplicating it.
        with tempfile.TemporaryDirectory() as tmp:
            empty = Path(tmp) / "empty.toml"
            write(empty, "[hosts]\n")
            self.assertEqual(self.mod.declared_labels(empty), set())


class LabelTests(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_module()

    def test_bare_words_are_not_judged(self) -> None:
        # Host fields legitimately hold `panel`, `status`, `session`. Judging a
        # bare word is what turns a precise rule into noise.
        self.assertEqual(self.mod._labels_in("panel"), [])
        self.assertEqual(self.mod._labels_in("status"), [])
        self.assertEqual(self.mod._labels_in("not a summary"), [])

    def test_structured_names_are_judged(self) -> None:
        self.assertEqual(self.mod._labels_in("kiln-dev"), ["kiln-dev"])
        self.assertEqual(self.mod._labels_in("kiln.tail1234.ts.net"),
                         ["kiln.tail1234.ts.net", "kiln"])
        # A bare host is deliberately NOT judged here: `scripts/ssh_hosts_gate.py`
        # covers bare words, because it knows the real labels and so has no false
        # positives to suppress. Here it would only add noise.
        self.assertEqual(self.mod._labels_in("operator@kiln"), [])
        self.assertEqual(self.mod._labels_in("operator@kiln-dev"), ["kiln-dev"])

    def test_placeholders_and_paths_are_not_hosts(self) -> None:
        for value in ("", "<home>", "/usr/bin", "a/b"):
            self.assertEqual(self.mod._labels_in(value), [], value)


class CheckTests(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_module()
        self.allowed = {"kiln", "atlas", "kiln-dev", "node-b", "operator"}

    def _check(self, text: str, name: str = "tests/x.rs"):
        path = Path(name)
        ranges = self.mod._test_line_ranges(text, path)
        return self.mod.check_text(text, path, self.allowed, ranges)

    def test_an_undeclared_hyphenated_host_fails(self) -> None:
        findings = self._check('let p = PeerConfig { ssh: "operator@unlistedbox-dev".into(), ..Default::default() };')
        self.assertEqual([f[1] for f in findings], ["unlistedbox-dev"])

    def test_an_undeclared_fqdn_fails_including_its_short_form(self) -> None:
        findings = self._check('let ssh = "unlistedbox.tail1234.ts.net";\n')
        labels = {f[1] for f in findings}
        self.assertIn("unlistedbox.tail1234.ts.net", labels)
        self.assertIn("unlistedbox", labels)

    def test_a_declared_host_passes(self) -> None:
        self.assertEqual(self._check('let p = PeerConfig { ssh: "operator@kiln-dev".into(), ..Default::default() };'), [])

    def test_rfc2606_names_pass_without_declaration(self) -> None:
        self.assertEqual(self._check('let ssh = "operator@spoke9.invalid";'), [])

    def test_production_code_is_out_of_scope(self) -> None:
        text = 'fn real() { let ssh = "operator@unlistedbox-dev"; }\n'
        path = Path("src/peers.rs")
        ranges = self.mod._test_line_ranges(text, path)
        self.assertEqual(ranges, [])
        self.assertEqual(self.mod.check_text(text, path, self.allowed, ranges), [])

    def test_a_test_region_is_in_scope(self) -> None:
        text = '#[cfg(test)]\nmod tests {\n    fn t() { let ssh = "operator@unlistedbox-dev"; }\n}\n'
        self.assertTrue(self.mod._test_line_ranges(text, Path("src/peers.rs")))

    def test_the_escape_hatch_silences_the_line_and_the_one_above(self) -> None:
        inline = 'let ssh = "operator@unlistedbox-dev"; // guardrails-ok(fixture): asserting the rejection\n'
        self.assertEqual(self._check(inline), [])
        above = '// guardrails-ok(fixture): a negative fixture\nlet ssh = "operator@unlistedbox-dev";\n'
        self.assertEqual(self._check(above), [])

    def test_an_agent_id_is_judged_only_when_structured(self) -> None:
        structured = self._check('let id = "agent_unlistedbox-dev_beef";\n')
        self.assertIn("unlistedbox-dev", {f[1] for f in structured})
        # `agent_status_id` is an identifier, not a machine named "status".
        code = self._check('strip_prefix("agent_");\nlet x = agent_status_id();\n')
        self.assertEqual(code, [])


class FreeTextTests(unittest.TestCase):
    """Commit messages, PR/issue titles and bodies — published, and previously ungated."""

    def setUp(self) -> None:
        self.mod = load_module()
        self.allowed = {"kiln", "kiln-dev", "operator"}

    def _findings(self, text: str, labels=None):
        return self.mod.check_free_text(text, self.allowed, labels or {})

    def test_a_user_at_host_destination_is_caught(self) -> None:
        self.assertEqual([f[1] for f in self._findings("operator@undeclared-box\n")], ["undeclared-box"])

    def test_a_tailnet_fqdn_is_caught(self) -> None:
        findings = self._findings("undeclared-box.tail1234.ts.net is unreachable\n")
        self.assertIn("undeclared-box.tail1234.ts.net", {f[1] for f in findings})

    def test_an_agent_id_host_is_caught_when_structured(self) -> None:
        self.assertIn("undeclared-box-dev", {f[1] for f in self._findings('id "agent_undeclared-box-dev_1"\n')})

    def test_the_project_own_domain_is_not_a_machine(self) -> None:
        for text in ("see flock.dev for docs\n", "reconcile with github.com/org/repo\n"):
            self.assertEqual(self._findings(text), [], text)

    def test_a_filename_that_looks_like_a_tld_is_not_a_host(self) -> None:
        # `config.local` is a file this repository ships; `.local` is not a
        # tailnet shape and judging it would flag ordinary commit subjects.
        self.assertEqual(self._findings("config.local was not in the overlay\n"), [])

    def test_a_git_ref_is_not_a_login(self) -> None:
        self.assertEqual(self._findings("pins nothing (dtolnay/rust-toolchain@stable)\n"), [])

    def test_an_attribution_trailer_is_identity_not_a_machine(self) -> None:
        for text in ("Co-authored-by: someone <someone@their-corp.example>\n",
                     "Signed-off-by: bot <bot@github.com>\n"):
            self.assertEqual(self._findings(text), [], text)

    def test_documentation_placeholders_are_not_machines(self) -> None:
        for text in ("literals like `user@host:port` still dial\n", "Co-authored-by: T <t@t>\n"):
            self.assertEqual(self._findings(text), [], text)

    def test_an_ordinary_identifier_is_never_judged(self) -> None:
        # Prose is full of hyphenated words that are not hosts.
        for text in ("fix(sidebar): sidecar nextest no-such-host\n", "chore: bump cross-platform-pty\n"):
            self.assertEqual(self._findings(text), [], text)

    def test_a_declared_host_and_rfc2606_pass(self) -> None:
        self.assertEqual(self._findings("operator@kiln-dev and spoke9.invalid\n"), [])

    def test_a_bare_hostname_in_prose_is_caught_nowhere(self) -> None:
        """The honest limit, pinned so it cannot be quietly misremembered.

        Prose has no `host = "..."` key, and a bare word in a sentence is
        indistinguishable from an ordinary one — so neither half judges it, not
        even the private one with the exact label in hand. What both halves judge
        is the three structured shapes, which is why those are the ones to use in
        a commit subject or a bug report.
        """
        import importlib.util as _u

        spec = _u.spec_from_file_location("sshgate", ROOT / "scripts" / "ssh_hosts_gate.py")
        gate = _u.module_from_spec(spec); spec.loader.exec_module(gate)
        labels = {"localbox": {"known_hosts"}}
        self.assertEqual(gate.check_text("switch to localbox failed\n", ROOT / "x.md", labels), [])
        self.assertEqual(self._findings("switch to localbox failed\n", labels), [])

    def test_the_private_half_still_judges_prose_shaped_destinations(self) -> None:
        import importlib.util as _u

        spec = _u.spec_from_file_location("sshgate", ROOT / "scripts" / "ssh_hosts_gate.py")
        gate = _u.module_from_spec(spec); spec.loader.exec_module(gate)
        labels = {"localbox": {"known_hosts"}}
        self.assertTrue(
            gate.check_text("ssh = \"operator@localbox\"\n", ROOT / "x.rs", labels)
        )


class EndToEndTests(unittest.TestCase):
    """The gate as a process, because the exit code is the contract."""

    def _run(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["python3", str(SCRIPT), *args], capture_output=True, text=True
        )

    def test_a_leak_fails_and_an_escape_hatch_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            # Under `tests/`, so the file counts as test code: the gate's scope is
            # fixtures, not production code.
            bad = Path(tmp) / "tests" / "leak.rs"
            write(bad, 'fn t() { let ssh = "operator@unlistedbox-dev"; }\n')
            self.assertEqual(self._run(str(bad)).returncode, 1)
            write(bad, 'fn t() { let ssh = "operator@kiln-dev"; }\n')
            self.assertEqual(self._run(str(bad)).returncode, 0)

    def test_a_message_file_is_checked(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            good = Path(tmp) / "msg"
            write(good, "fix(remote): a switch now says why\n")
            self.assertEqual(self._run("--message-file", str(good)).returncode, 0)
            bad = Path(tmp) / "msg2"
            write(bad, "fix: operator@undeclared-box never dialled\n")
            self.assertEqual(self._run("--message-file", str(bad)).returncode, 1)

    def test_no_message_file_still_checks_rust_paths(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            rs = Path(tmp) / "tests" / "x.rs"
            write(rs, 'let ssh = "operator@kiln-dev";\n')
            self.assertEqual(self._run(str(rs)).returncode, 0)

    def test_non_rust_paths_are_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            doc = Path(tmp) / "notes.md"
            write(doc, "operator@unlistedbox-dev\n")
            self.assertEqual(self._run(str(doc)).returncode, 0)


if __name__ == "__main__":
    import sys

    unittest.main()