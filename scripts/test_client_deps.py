"""Client dependency pin diagnostics, interrupted refresh and cross-system hashes."""
import json
import os
from pathlib import Path
import select
import shlex
import signal
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
HASH = 'sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='

FAKE_NIX = '''import json, os, signal, sys
from pathlib import Path
args = sys.argv[1:]
if args[0] == 'eval':
    print('1.3.13')
    raise SystemExit(0)
assert args[0] == 'build', args
pin = json.loads(Path('nix/client-deps.json').read_text())
phase = 'verify' if pin['hash'] else 'probe'
if os.environ.get('BLOCK_PHASE') == phase:
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    signal.signal(signal.SIGTERM, signal.SIG_DFL)
    os.write(int(os.environ['READY_FD']), f'ready {os.getpid()}\\n'.encode())
    signal.pause()
if os.environ.get('FAIL_PHASE') == phase:
    print('fixture fetch failure', file=sys.stderr)
    raise SystemExit(9)
if phase == 'probe':
    print('  got: ' + os.environ['FETCH_HASH'], file=sys.stderr)
    raise SystemExit(1)
raise SystemExit(0)
'''


class BunPinTests(unittest.TestCase):
    def test_bun_bump_names_recovery_and_same_version_cache_limit(self):
        expression = '''let
          source = builtins.toPath (builtins.getEnv "CLIENT_SOURCE");
          lock = builtins.hashFile "sha256" (builtins.toPath (builtins.getEnv "CLIENT_LOCK"));
        in (import source) {
          lib = {
            importJSON = _: { inherit lock; hash = "fixture-hash"; bun = "old-version"; };
            assertMsg = condition: message: if condition then true else throw message;
          };
          stdenv = { buildPlatform.system = "aarch64-darwin"; mkDerivation = _: "accepted"; };
          stdenvNoCC = null; bun.version = "new-version";
          nodejs = null; cacert = null; autoPatchelfHook = null;
        }'''
        env = dict(os.environ, CLIENT_SOURCE=str(ROOT / 'nix/client.nix'),
                   CLIENT_LOCK=str(ROOT / 'web/client/bun.lock'))
        result = subprocess.run(['nix-instantiate', '--eval', '--strict', '--expr', expression],
                                env=env, capture_output=True, text=True, timeout=30)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn('just client-deps-hash', result.stderr)
        self.assertIn('same-version nixpkgs Bun patch', result.stderr)
        self.assertIn('cached', result.stderr)


class RefreshTests(unittest.TestCase):
    def fixture(self, root):
        (root / 'nix').mkdir()
        (root / 'web/client').mkdir(parents=True)
        (root / 'web/client/bun.lock').write_text('fixture dependency lock\n')
        original = b'{"lock":"old-lock","bun":"old-bun","hash":"old-hash"}\n'
        (root / 'nix/client-deps.json').write_bytes(original)
        tools = root / 'tools'
        tools.mkdir()
        fake = root / 'fake_nix.py'
        fake.write_text(FAKE_NIX)
        for name, body in {
            'nix': f'exec {shlex.quote(sys.executable)} {shlex.quote(str(fake))} "$@"',
            'nix-hash': "printf '%s\\n' '" + 'b' * 64 + "'",
        }.items():
            path = tools / name
            path.write_text('#!/bin/sh\n' + body + '\n')
            path.chmod(0o755)
        env = {key: value for key, value in os.environ.items() if not key.startswith('FLOCK_')}
        env.update(PATH=str(tools) + os.pathsep + os.environ['PATH'], FETCH_HASH=HASH)
        command = ['just', '--justfile', str(ROOT / 'justfile'),
                   '--working-directory', str(root), 'client-deps-hash']
        return command, env, original

    def test_interrupt_restores_original_manifest(self):
        self.interrupt_restores_original_manifest()

    def test_repeated_term_during_exit_restores_original_manifest(self):
        self.interrupt_restores_original_manifest(gate_restore=True)

    def interrupt_restores_original_manifest(self, gate_restore=False):
        for phase in ['probe', 'verify']:
            signals = [signal.SIGTERM] if gate_restore else [signal.SIGINT, signal.SIGTERM]
            for sig in signals:
                with self.subTest(phase=phase, signal=sig), tempfile.TemporaryDirectory() as name:
                    root = Path(name)
                    command, env, original = self.fixture(root)
                    read_fd, write_fd = os.pipe()
                    env.update(BLOCK_PHASE=phase, READY_FD=str(write_fd))
                    extra_fds = []
                    if gate_restore:
                        restore_read, restore_write = os.pipe()
                        release_read, release_write = os.pipe()
                        extra_fds = [restore_read, restore_write, release_read, release_write]
                        bash_env = root / 'bash-env'
                        # DEBUG gates entry to the EXIT callback, before its first command.
                        bash_env.write_text('''set -T
trap 'if [[ ${FUNCNAME[0]-} == restore && ${restore_gated-} != yes ]]; then
    restore_gated=yes
    printf "restore\\n" >&"$RESTORE_READY_FD"
    IFS= read -r -t 10 -u "$RESTORE_RELEASE_FD" release
fi' DEBUG
''')
                        env.update(BASH_ENV=str(bash_env), RESTORE_READY_FD=str(restore_write),
                                   RESTORE_RELEASE_FD=str(release_read))
                    # This private session contains only our recipe and fake tools.
                    child_fds = (write_fd, *(extra_fds[1:3] if gate_restore else []))
                    child = subprocess.Popen(command, cwd=root, env=env, start_new_session=True,
                                             pass_fds=child_fds, stdout=subprocess.PIPE,
                                             stderr=subprocess.PIPE, text=True)
                    os.close(write_fd)
                    if gate_restore:
                        os.close(restore_write)
                        os.close(release_read)
                        extra_fds = [restore_read, release_write]
                    try:
                        ready, _, _ = select.select([read_fd], [], [], 10)
                        self.assertTrue(ready, 'fake fetch failed to reach the requested stage')
                        notice = os.read(read_fd, 64)
                        self.assertRegex(notice, rb'\Aready [0-9]+\n\Z')
                        if gate_restore:
                            # Fail only the ready fetch so Bash enters EXIT naturally.
                            # Signalling the group here lets just's forwarded TERM
                            # re-enter restore before its DEBUG gate can announce it.
                            os.kill(int(notice.split()[1]), sig)
                        else:
                            os.killpg(child.pid, sig)
                        if gate_restore:
                            ready, _, _ = select.select([restore_read], [], [], 10)
                            self.assertTrue(ready, 'recipe never entered its restore callback')
                            self.assertEqual(os.read(restore_read, 64), b'restore\n',
                                             'restore gate must fire before the duplicate TERM')
                            # A group TERM also reaches just, which forwards it to Bash.
                            # Deliver the duplicate while EXIT is active, then proceed.
                            os.killpg(child.pid, signal.SIGTERM)
                            try:
                                os.write(release_write, b'continue\n')
                            except BrokenPipeError:
                                # Only gate release may race the duplicate TERM closing
                                # its reader after reentrant restoration. communicate below
                                # must still prove exit and the original manifest.
                                pass
                        stdout, stderr = child.communicate(timeout=10)
                        self.assertNotEqual(child.returncode, 0, stdout + stderr)
                        self.assertEqual((root / 'nix/client-deps.json').read_bytes(), original)
                    finally:
                        if child.poll() is None:
                            os.killpg(child.pid, signal.SIGKILL)
                        child.communicate(timeout=5)
                        os.close(read_fd)
                        for fd in extra_fds:
                            os.close(fd)

    def test_success_records_lock_bun_and_hash(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            command, env, _ = self.fixture(root)
            result = subprocess.run(command, cwd=root, env=env, capture_output=True,
                                    text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads((root / 'nix/client-deps.json').read_text()),
                             dict(lock='b' * 64, bun='1.3.13', hash=HASH))

    def test_failed_fetch_or_verification_restores_original(self):
        for phase in ['probe', 'verify']:
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as name:
                root = Path(name)
                command, env, original = self.fixture(root)
                env['FAIL_PHASE'] = phase
                result = subprocess.run(command, cwd=root, env=env, capture_output=True,
                                        text=True, timeout=10)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual((root / 'nix/client-deps.json').read_bytes(), original)


class ReportTests(unittest.TestCase):
    def reports(self, root, **second):
        paths = []
        for system in ['x86_64-linux', 'aarch64-darwin']:
            report = dict(system=system, hash=HASH, bun='1.3.13', lock='fixture-lock')
            if system == 'aarch64-darwin':
                report.update(second)
            path = root / (system + '.json')
            path.write_text(json.dumps(report))
            paths.append(path)
        return paths

    def test_equal_reports_pass(self):
        from scripts.client_deps import compare_reports
        with tempfile.TemporaryDirectory() as name:
            self.assertEqual(compare_reports(self.reports(Path(name))), HASH)

    def test_different_hash_lock_or_bun_is_refused(self):
        from scripts.client_deps import compare_reports
        for key in ['hash', 'lock', 'bun']:
            with self.subTest(key=key), tempfile.TemporaryDirectory() as name:
                paths = self.reports(Path(name), **{key: 'different'})
                with self.assertRaisesRegex(ValueError, key + ' differs across systems'):
                    compare_reports(paths)

    def test_missing_duplicate_and_empty_reports_are_refused(self):
        from scripts.client_deps import compare_reports
        with tempfile.TemporaryDirectory() as name:
            paths = self.reports(Path(name))
            for invalid in [[], paths[:1], [paths[0], paths[0]]]:
                with self.subTest(paths=invalid), self.assertRaisesRegex(ValueError, 'exactly one'):
                    compare_reports(invalid)
            paths = self.reports(Path(name), hash='')
            with self.assertRaisesRegex(ValueError, 'nonempty hash'):
                compare_reports(paths)

    def test_real_build_and_rebuild_precede_actual_hash_measurement(self):
        from unittest import mock
        from scripts import client_deps
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            pin = root / 'pin.json'
            pin.write_text(json.dumps(dict(hash=HASH, bun='1.3.13', lock='fixture-lock')))
            report = root / 'report.json'
            with mock.patch.object(client_deps, 'run', side_effect=[
                    'x86_64-linux', str(root), '', HASH]) as run:
                client_deps.build_report('x86_64-linux', report, pin)
            self.assertEqual(run.call_args_list, [
                mock.call(['nix', 'eval', '--raw', '.#packages.x86_64-linux.flock-client.deps.system']),
                mock.call(['nix', 'build', '.#packages.x86_64-linux.flock-client.deps',
                           '--no-link', '--print-out-paths']),
                mock.call(['nix', 'build', '.#packages.x86_64-linux.flock-client.deps',
                           '--no-link', '--rebuild']),
                mock.call(['nix', 'hash', 'path', '--type', 'sha256', '--sri', str(root)]),
            ])
            self.assertEqual(json.loads(report.read_text())['hash'], HASH)
            with mock.patch.object(client_deps, 'run', side_effect=[
                    'x86_64-linux', str(root), '', 'different']):
                with self.assertRaisesRegex(ValueError, 'produced different'):
                    client_deps.build_report('x86_64-linux', report, pin)

    def test_build_failure_names_refresh_command_and_patch_cache_limit(self):
        from unittest import mock
        from scripts import client_deps
        with mock.patch.object(client_deps, 'run', side_effect=subprocess.CalledProcessError(1, 'nix')):
            with mock.patch('sys.argv', ['client_deps.py', 'build', 'x86_64-linux', 'report.json']):
                import contextlib
                import io
                output = io.StringIO()
                with contextlib.redirect_stderr(output):
                    self.assertEqual(client_deps.main(), 1)
        self.assertIn('just client-deps-hash', output.getvalue())
        self.assertIn('same-version nixpkgs Bun patch', output.getvalue())
        self.assertIn('cached', output.getvalue())

    def test_workflow_builds_both_systems_then_compares_their_artifacts(self):
        from scripts.test_actions_cache import workflow
        document = workflow('client-deps.yml')
        build = document['jobs']['build-deps']
        self.assertEqual(build['strategy']['matrix']['include'], [
            dict(system='x86_64-linux', runner='ubuntu-latest'),
            dict(system='aarch64-darwin', runner='macos-15'),
        ])
        commands = [step['run'] for step in build['steps'] if 'run' in step]
        self.assertTrue(any("client_deps.py build '${{ matrix.system }}'" in step
                            for step in commands))
        compare = document['jobs']['compare']
        self.assertEqual(compare['needs'], 'build-deps')
        self.assertTrue(any('client_deps.py compare reports/*.json' in step.get('run', '')
                            for step in compare['steps']))
