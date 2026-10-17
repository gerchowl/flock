"""Compile-guard the token capability: only its auth module can construct it."""
import pathlib
import re
import subprocess
import tempfile
import unittest


class PermissionPrincipalPrivacy(unittest.TestCase):
    def test_token_cannot_be_forged_from_a_subject(self):
        root = pathlib.Path(__file__).resolve().parents[1]
        # Compile the actual sealed declaration without its token-store dependencies.
        module = (root / "src/structured/permissions/auth.rs").read_text().split("\npub mod tokens;")[0]
        probes = [
            ("let _: Option<auth::VerifiedToken<'static>> = None;", None),
            ('let _ = auth::VerifiedToken { subject: "fixture", operate: true };', "E0451"),
            ('let _ = auth::VerifiedToken::new("fixture", true);', "E0624"),
            ('let _: auth::VerifiedToken = "fixture".into();', "E0277"),
        ]
        with tempfile.TemporaryDirectory(prefix="permission-privacy-") as name:
            base = pathlib.Path(name)
            for body, error in probes:
                source = base / "probe.rs"
                source.write_text("mod auth {\n" + module + "\n}\nfn main() {" + body + "}\n")
                result = subprocess.run(
                    ["rustc", "--edition=2021", "--emit=metadata", "-o", str(base / "probe.rmeta"), str(source)],
                    capture_output=True, text=True, timeout=30, check=False,
                )
                if error is None:
                    self.assertEqual(result.returncode, 0, result.stderr)
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(error, result.stderr)

    def test_local_operator_witness_has_no_public_field_constructor(self):
        root = pathlib.Path(__file__).resolve().parents[1]
        source = (root / "src/app/api.rs").read_text()
        declaration = re.search(r"pub\(crate\) struct LocalOperator<'a> \{.*?\n\}", source, re.S).group()
        module = "mod app { pub struct App; mod api { use super::App; " + declaration + " } pub(crate) use api::LocalOperator; }"
        with tempfile.TemporaryDirectory(prefix="operator-privacy-") as name:
            base = pathlib.Path(name)
            for body, error in [
                ("let _: Option<app::LocalOperator<'static>> = None;", None),
                ("let mut a = app::App; let _ = app::LocalOperator { _app: &mut a };", "E0451"),
            ]:
                path = base / "probe.rs"
                path.write_text(module + " fn main() {" + body + "}")
                result = subprocess.run(["rustc", "--edition=2021", "--emit=metadata", "-o", str(base / "probe.rmeta"), str(path)], capture_output=True, text=True, timeout=30, check=False)
                if error is None:
                    self.assertEqual(result.returncode, 0, result.stderr)
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(error, result.stderr)
