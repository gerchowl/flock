"""Real devShell regression probe. Run with `nix develop --command just test-sccache`."""

from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import os
from pathlib import Path
import select
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
import unittest


def assert_private_endpoint(env, root, port):
    """Refuse every endpoint except the private socket or allocated port."""
    if port is None:
        private = (env.get('SCCACHE_SERVER_UDS') == str(root / 'server.sock')
                   and 'SCCACHE_SERVER_PORT' not in env)
    else:
        private = (port != 4226 and env.get('SCCACHE_SERVER_PORT') == str(port)
                   and 'SCCACHE_SERVER_UDS' not in env)
    if not private:
        raise RuntimeError('refusing non-private sccache endpoint')


def require_wrapper():
    """Reject an inactive devShell before creating or contacting any server."""
    expected = os.environ.get('FLOCK_TEST_SCCACHE_WRAPPER')
    active = shutil.which('sccache')
    rust_wrapper = shutil.which(os.environ.get('RUSTC_WRAPPER', ''))
    raw = shutil.which(os.environ.get('FLOCK_TEST_SCCACHE_RAW', ''))
    if not expected or not raw or any(
        not path or Path(path).resolve() != Path(expected).resolve()
        for path in (active, rust_wrapper)
    ):
        raise SystemExit('run inside nix develop (the wrapper is not active)')
    return expected


@contextmanager
def startup_sandbox():
    """Own every endpoint and process group, including failed startup children."""
    wrapper = require_wrapper()
    with tempfile.TemporaryDirectory(prefix='sc-start-', dir='/tmp') as directory:
        root = Path(directory)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(('SCCACHE_', 'FLOCK_'))}
        env.update(HOME=directory, TMPDIR=directory,
                   XDG_CONFIG_HOME=str(root / 'config'),
                   SCCACHE_DIR=str(root / 'cache'),
                   SCCACHE_SERVER_UDS=str(root / 'server.sock'),
                   SCCACHE_NO_DAEMON='1', SCCACHE_IDLE_TIMEOUT='0',
                   SCCACHE_LOG='info')
        processes = []

        def start(*args, foreground=False):
            assert_private_endpoint(env, root, None)
            child_env = dict(env)
            if foreground:
                child_env['SCCACHE_START_SERVER'] = '1'
            log = tempfile.NamedTemporaryFile(
                mode='w', prefix='client-', suffix='.log', dir=root, delete=False)
            try:
                child = subprocess.Popen(
                    [wrapper, *args], env=child_env, start_new_session=True,
                    stdout=subprocess.PIPE if foreground else log,
                    stderr=subprocess.STDOUT if foreground else log,
                )
                processes.append(child)
            finally:
                log.close()
            return child

        try:
            yield root, start
        finally:
            for child in processes:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            for child in processes:
                child.wait(timeout=10)
                if child.stdout:
                    child.stdout.close()


class SccacheStartupProbe(unittest.TestCase):
    def test_twenty_rounds_of_four_automatic_starters_preserve_live_server(self):
        for trial in range(20):
            with self.subTest(trial=trial), startup_sandbox() as (root, start):
                barrier = threading.Barrier(4)

                def launch(_):
                    barrier.wait(timeout=10)
                    return start('--zero-stats').wait(timeout=60)

                with ThreadPoolExecutor(max_workers=4) as pool:
                    codes = list(pool.map(launch, range(4)))
                logs = ''.join(path.read_text() for path in root.glob('client-*.log'))
                self.assertEqual(codes, [0] * 4, logs)
                self.assertEqual(logs.count('server started, listening on'), 1, logs)
                endpoint = root / 'server.sock'
                inode = endpoint.stat().st_ino
                lock = root / 'server.sock.lock'
                self.assertEqual(lock.stat().st_mode & 0o777, 0o600)
                lock_inode = lock.stat().st_ino
                self.assertEqual(start('--start-server').wait(timeout=60), 0)
                self.assertEqual(endpoint.stat().st_ino, inode)
                self.assertEqual(lock.stat().st_ino, lock_inode)
                self.assertEqual(start('--zero-stats').wait(timeout=60), 0)
                print(f'Concurrent startup round {trial + 1}: 4/4, live socket preserved',
                      flush=True)

    def test_reclaims_socket_left_by_killed_server(self):
        with startup_sandbox() as (root, start):
            server = start(foreground=True)
            deadline = time.monotonic() + 60
            observed = b''
            while b'server started, listening on' not in observed:
                remaining = deadline - time.monotonic()
                self.assertGreater(remaining, 0, observed.decode(errors='replace'))
                ready, _, _ = select.select([server.stdout], [], [], remaining)
                self.assertTrue(ready, observed.decode(errors='replace'))
                chunk = os.read(server.stdout.fileno(), 4096)
                self.assertTrue(chunk, f'server exited before readiness: {observed!r}')
                observed += chunk
                self.assertNotIn(b' ERROR ', observed)
                self.assertNotIn(b'server already running', observed)
                self.assertIsNone(server.poll(), observed.decode(errors='replace'))
            endpoint = root / 'server.sock'
            lock_inode = (root / 'server.sock.lock').stat().st_ino
            server.kill()
            server.wait(timeout=10)
            self.assertTrue(endpoint.is_socket())
            with socket.socket(socket.AF_UNIX) as probe:
                with self.assertRaises(ConnectionRefusedError):
                    probe.connect(str(endpoint))
            self.assertEqual(start('--zero-stats').wait(timeout=60), 0)
            self.assertEqual((root / 'server.sock.lock').stat().st_ino, lock_inode)
            with socket.socket(socket.AF_UNIX) as probe:
                probe.connect(str(endpoint))

    def test_preserves_non_socket_paths_and_rejects_unsafe_locks(self):
        for kind in ('file', 'symlink', 'lock-mode', 'lock-symlink'):
            with self.subTest(kind=kind), startup_sandbox() as (root, start):
                endpoint = root / 'server.sock'
                lock = root / 'server.sock.lock'
                data = root / 'data'
                data.write_text('preserve this data')
                if kind == 'file':
                    endpoint.write_text('preserve this endpoint')
                elif kind == 'symlink':
                    endpoint.symlink_to(data)
                elif kind == 'lock-mode':
                    lock.touch(mode=0o644)
                    lock.chmod(0o644)
                else:
                    lock.symlink_to(data)
                self.assertNotEqual(start('--start-server').wait(timeout=60), 0)
                self.assertEqual(data.read_text(), 'preserve this data')
                if kind == 'file':
                    self.assertEqual(endpoint.read_text(), 'preserve this endpoint')
                elif kind == 'symlink':
                    self.assertTrue(endpoint.is_symlink())
                elif kind == 'lock-mode':
                    self.assertEqual(lock.stat().st_mode & 0o777, 0o644)
                else:
                    self.assertTrue(lock.is_symlink())


class SccacheProbe(unittest.TestCase):
    def probe(self, private_port):
        wrapper = require_wrapper()
        # Short roots also keep the default Unix socket below macOS sun_path limits.
        with tempfile.TemporaryDirectory(prefix='sc-', dir='/tmp') as directory:
            root = Path(directory)
            startup = root / 't1'
            startup.mkdir()
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith(('SCCACHE_', 'FLOCK_'))}
            env.update(HOME=directory, TMPDIR=str(startup),
                       XDG_CACHE_HOME=str(root / 'xdg-cache'),
                       XDG_CONFIG_HOME=str(root / 'config'))
            port = None
            if private_port:
                with socket.socket() as listener:
                    listener.bind(('127.0.0.1', 0))
                    port = listener.getsockname()[1]
                self.assertNotEqual(port, 4226)
                env['SCCACHE_SERVER_PORT'] = str(port)
            else:
                env['SCCACHE_SERVER_UDS'] = str(root / 'server.sock')
            command = wrapper if private_port else 'sccache'

            def run(*args, executable=None):
                assert_private_endpoint(env, root, port)
                result = subprocess.run(
                    [executable or command, *args], env=env, text=True,
                    capture_output=True, timeout=60,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                return result.stdout

            print('Owned endpoint:', env.get('SCCACHE_SERVER_UDS',
                                              env.get('SCCACHE_SERVER_PORT')), flush=True)
            run('--start-server')
            try:
                shutil.rmtree(startup)
                env['TMPDIR'] = directory
                source = root / 'tiny.rs'
                source.write_text('pub fn answer() -> u32 { 42 }\n')
                output = root / 'out'
                output.mkdir()

                def compile_crate(executable=None):
                    run(shutil.which('rustc'), '--crate-name', 'tiny',
                        '--crate-type', 'rlib', '--emit', 'link', str(source),
                        '--out-dir', str(output), executable=executable)

                compile_crate()
                shutil.rmtree(output)
                output.mkdir()
                compile_crate()
                stats = run('--show-stats')
                self.assertRegex(stats, r'Cache hits\s+[1-9]\d*')
                self.assertRegex(stats, r'Cache misses\s+[1-9]\d*')
                print(stats, flush=True)
                # Explicit restart covers the same path used after daemon idle exit.
                run('--stop-server')
                run('--start-server')
                compile_crate()
                self.assertRegex(run('--show-stats'), r'Cache hits\s+[1-9]\d*')
                run('--stop-server')
                compile_crate()
                self.assertRegex(run('--show-stats'), r'Cache hits\s+[1-9]\d*')
                # Warm the platform-default cache with an unwrapped daemon,
                # then require a hit from the wrapper using that same cache.
                run('--stop-server')
                source.write_text('pub fn answer() -> u32 { 43 }\n')
                raw = os.environ['FLOCK_TEST_SCCACHE_RAW']
                compile_crate(executable=raw)
                raw_stats = run('--show-stats', executable=raw)
                self.assertIn(directory, raw_stats)
                self.assertRegex(raw_stats, r'Cache misses\s+1\b')
                run('--stop-server', executable=raw)
                compile_crate()
                wrapped_stats = run('--show-stats')
                self.assertRegex(wrapped_stats, r'Cache hits\s+[1-9]\d*')
                raw_location = next(line for line in raw_stats.splitlines()
                                    if line.startswith('Cache location'))
                self.assertIn(raw_location, wrapped_stats)
                print('Raw and wrapped platform cache:', raw_location, flush=True)
            finally:
                run('--stop-server')

    def test_private_port_survives_deleted_startup_temp_and_retains_hits(self):
        self.probe(private_port=True)

    def test_private_socket_survives_deleted_startup_temp_and_retains_platform_hits(self):
        self.probe(private_port=False)


if __name__ == '__main__':
    require_wrapper()
    unittest.main()
