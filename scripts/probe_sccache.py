"""Real devShell regression probe. Run with `nix develop --command just test-sccache`."""

import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import unittest


class SccacheProbe(unittest.TestCase):
    def probe(self, private_port):
        # Short roots also keep the default Unix socket below macOS sun_path limits.
        with tempfile.TemporaryDirectory(prefix='sc-', dir='/tmp') as directory:
            root = Path(directory)
            startup = root / 't1'
            startup.mkdir()
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith(('SCCACHE_', 'FLOCK_'))}
            env.update(HOME=directory, TMPDIR=str(startup))
            if private_port:
                with socket.socket() as listener:
                    listener.bind(('127.0.0.1', 0))
                    port = listener.getsockname()[1]
                self.assertNotEqual(port, 4226)
                env['SCCACHE_SERVER_PORT'] = str(port)
            command = (os.environ.get('FLOCK_TEST_SCCACHE', os.environ['RUSTC_WRAPPER'])
                       if private_port else 'sccache')

            def run(*args):
                result = subprocess.run(
                    [command, *args], env=env, text=True,
                    capture_output=True, timeout=60,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                return result.stdout

            run('--start-server')
            try:
                shutil.rmtree(startup)
                env['TMPDIR'] = directory
                source = root / 'tiny.rs'
                source.write_text('pub fn answer() -> u32 { 42 }\n')
                output = root / 'out'
                output.mkdir()

                def compile_crate():
                    run(shutil.which('rustc'), '--crate-name', 'tiny',
                        '--crate-type', 'rlib', '--emit', 'link', str(source),
                        '--out-dir', str(output))

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
            finally:
                run('--stop-server')

    def test_private_port_survives_deleted_startup_temp_and_retains_hits(self):
        self.probe(private_port=True)

    def test_default_socket_survives_deleted_startup_temp_and_retains_hits(self):
        self.probe(private_port=False)


if __name__ == '__main__':
    unittest.main()
