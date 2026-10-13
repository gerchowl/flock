"""Real devShell regression probe. Run with `nix develop --command just test-sccache`."""

import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import unittest


def assert_private_endpoint(env, root, port):
    """Refuse every endpoint except the private socket or allocated port."""
    if port is None:
        assert env.get('SCCACHE_SERVER_UDS') == str(root / 'server.sock')
        assert 'SCCACHE_SERVER_PORT' not in env
    else:
        assert port != 4226
        assert env.get('SCCACHE_SERVER_PORT') == str(port)
        assert 'SCCACHE_SERVER_UDS' not in env


class SccacheProbe(unittest.TestCase):
    def probe(self, private_port):
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
            command = ((os.environ.get('FLOCK_TEST_SCCACHE')
                        or os.environ.get('RUSTC_WRAPPER') or 'sccache')
                       if private_port else 'sccache')

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
    unittest.main()
