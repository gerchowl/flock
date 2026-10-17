"""Compile-guard the token capability: only its auth module can construct it."""
import pathlib
import subprocess
import tempfile
import unittest


class PermissionPrincipalPrivacy(unittest.TestCase):
    def test_token_cannot_be_forged_from_a_subject(self):
        root = pathlib.Path(__file__).resolve().parents[1]
        module = (root / "src/structured/permissions/auth.rs").read_text()
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
