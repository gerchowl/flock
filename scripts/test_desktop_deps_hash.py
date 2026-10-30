"""A failed desktop dependency refresh must preserve the installable pin."""

import contextlib
import io
import json
import os
from pathlib import Path
import select
import shlex
import signal
import subprocess
import sys
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

from scripts import desktop_deps_hash


class RefreshTests(unittest.TestCase):
    def refresh(self, results):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "nix").mkdir()
            (root / "desktop").mkdir()
            (root / "desktop/bun.lock").write_text("fixture dependency lock\n")
            (root / "desktop/package.json").write_text('{"name":"fixture-desktop"}\n')
            pin = root / "nix/desktop-deps.json"
            original = b'{"hash":"existing-installable-pin"}\n'
            pin.write_bytes(original)
            builds = iter(results)

            def run(command, **_kwargs):
                return next(builds)

            with (
                patch.object(desktop_deps_hash, "Path", side_effect=lambda name: root / name),
                patch.object(desktop_deps_hash.subprocess, "run", side_effect=run),
                patch.object(desktop_deps_hash.subprocess, "check_output", return_value="fixture-bun"),
                patch.object(desktop_deps_hash.signal, "signal"),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                try:
                    desktop_deps_hash.main()
                except SystemExit as error:
                    self.assertEqual(pin.read_bytes(), original)
                    return error.code, None
                return 0, json.loads(pin.read_text())

    def test_fetch_error_restores_pin(self):
        code, _ = self.refresh([SimpleNamespace(returncode=9, stdout="fetch failed\n")])
        self.assertNotEqual(code, 0)

    def test_verification_failure_restores_pin(self):
        code, _ = self.refresh([
            SimpleNamespace(returncode=1, stdout="  got: sha256-fixture\n"),
            SimpleNamespace(returncode=9, stdout="could not reproduce dependency fetch\n"),
        ])
        self.assertEqual(code, 9)

    def test_only_verified_hash_replaces_pin(self):
        code, pin = self.refresh([
            SimpleNamespace(returncode=1, stdout="  got: sha256-fixture\n"),
            SimpleNamespace(returncode=0, stdout=""),
        ])
        self.assertEqual(code, 0)
        self.assertEqual(pin["hash"], "sha256-fixture")
        self.assertEqual(pin["bun"], "fixture-bun")

    def test_repeated_term_during_restore_preserves_pin(self):
        for phase in ("probe", "verify"):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "nix").mkdir()
                (root / "desktop").mkdir()
                (root / "desktop/bun.lock").write_text("fixture lock\n")
                (root / "desktop/package.json").write_text('{"name":"fixture-desktop"}\n')
                original = b'{"hash":"existing-installable-pin"}\n'
                pin = root / "nix/desktop-deps.json"
                pin.write_bytes(original)
                tools = root / "tools"
                tools.mkdir()
                fake = tools / "nix"
                fake_code = root / "fake_nix.py"
                fake_code.write_text('''
import json, os, signal, sys
from pathlib import Path
if sys.argv[1] == 'eval':
    print('fixture-bun')
    raise SystemExit(0)
pin = json.loads(Path('nix/desktop-deps.json').read_text())
phase = 'verify' if pin['hash'] else 'probe'
if phase == os.environ['BLOCK_PHASE']:
    with open(os.environ['READY_FILE'], 'wb', buffering=0) as ready:
        ready.write(b'R')
    signal.pause()
print('  got: sha256-fixture')
raise SystemExit(1)
''')
                fake.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable) + " "
                                + shlex.quote(str(fake_code)) + ' "$@"\n')
                fake.chmod(0o755)
                ready_file = root / "fetch-ready"
                os.mkfifo(ready_file)
                ready_r = os.open(ready_file, os.O_RDWR | os.O_NONBLOCK)
                restore_r, restore_w = os.pipe()
                release_r, release_w = os.pipe()
                # Gate the actual restoration write: a second TERM arrives
                # precisely inside recovery, rather than relying on timing.
                shim = '''
import os, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from scripts import desktop_deps_hash
original = Path('nix/desktop-deps.json').read_bytes()
write = Path.write_bytes
gated = False
def restore_write(path, data):
    global gated
    if not gated and data == original:
        gated = True
        os.write(int(os.environ['RESTORE_FD']), b'R')
        assert os.read(int(os.environ['RELEASE_FD']), 1) == b'G'
    return write(path, data)
Path.write_bytes = restore_write
desktop_deps_hash.main()
'''
                env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                           BLOCK_PHASE=phase, READY_FILE=str(ready_file),
                           RESTORE_FD=str(restore_w), RELEASE_FD=str(release_r))
                process = subprocess.Popen(
                    [sys.executable, "-c", shim, str(Path(__file__).resolve().parents[1])],
                    cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                    pass_fds=(restore_w, release_r), start_new_session=True,
                )
                try:
                    self.assertTrue(select.select([ready_r], [], [], 10)[0], "fetch did not reach barrier")
                    self.assertEqual(os.read(ready_r, 1), b"R")
                    os.killpg(process.pid, signal.SIGTERM)
                    self.assertTrue(select.select([restore_r], [], [], 10)[0], "restore did not reach barrier")
                    self.assertEqual(os.read(restore_r, 1), b"R")
                    os.killpg(process.pid, signal.SIGTERM)
                    os.write(release_w, b"G")
                    output, _ = process.communicate(timeout=10)
                    self.assertEqual(process.returncode, 143, output.decode())
                    self.assertEqual(pin.read_bytes(), original)
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate(timeout=10)
                    process.stdout.close()
                    for descriptor in (ready_r, restore_r, restore_w, release_r, release_w):
                        os.close(descriptor)


if __name__ == "__main__":
    unittest.main()
