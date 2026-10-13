"""Persistent sccache environment and transparent wrapper failures."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
WRAPPER = ROOT / 'nix/sccache.sh'


class SccacheWrapperTests(unittest.TestCase):
    def invoke(self, root, **overrides):
        backend = root / 'backend'
        backend.write_text(
            '#!/usr/bin/env python3\nimport json, os, sys\n'
            'print(json.dumps(dict(tmp=os.environ["TMPDIR"], '
            'cache=os.environ["SCCACHE_DIR"], '
            'uds=os.environ.get("SCCACHE_SERVER_UDS"), '
            'port=os.environ.get("SCCACHE_SERVER_PORT"), args=sys.argv[1:])))\n'
            'sys.exit(23)\n'
        )
        backend.chmod(0o755)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith('SCCACHE_')}
        env.update(HOME=str(root), TMPDIR=str(root / 'deleted'),
                   SCCACHE_REAL=str(backend))
        env.update(overrides)
        return subprocess.run(
            ['/bin/sh', str(WRAPPER), 'argument with spaces', '--show-stats'],
            env=env, capture_output=True, text=True, timeout=10,
        )

    def test_default_paths_survive_caller_temp_and_forward_failure_and_arguments(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = self.invoke(root)
            self.assertEqual(result.returncode, 23)
            state = json.loads(result.stdout)
            self.assertEqual(state['tmp'], str(root / '.cache/sccache/tmp'))
            self.assertTrue(Path(state['tmp']).is_dir())
            self.assertEqual(state['cache'], str(root / '.cache/sccache'))
            self.assertEqual(state['uds'], str(root / '.cache/sccache/flock.sock'))
            self.assertEqual(state['args'], ['argument with spaces', '--show-stats'])

    def test_explicit_cache_and_private_port(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cache = str(root / 'cache with spaces')
            result = self.invoke(root, SCCACHE_DIR=cache, SCCACHE_SERVER_PORT='43210')
            self.assertEqual(result.returncode, 23)
            state = json.loads(result.stdout)
            self.assertEqual(state['tmp'], cache + '/tmp')
            self.assertEqual(state['port'], '43210')
            self.assertIsNone(state['uds'])

    def test_explicit_socket(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = self.invoke(root, SCCACHE_SERVER_UDS=str(root / 'chosen.sock'))
            self.assertEqual(result.returncode, 23)
            self.assertEqual(json.loads(result.stdout)['uds'], str(root / 'chosen.sock'))

    def test_unusable_cache_names_directory_and_does_not_run_backend(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cache = root / 'file'
            cache.touch()
            result = self.invoke(root, SCCACHE_DIR=str(cache))
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stdout, '')
            self.assertIn('cannot create persistent temp directory: ' + str(cache / 'tmp'),
                          result.stderr)
