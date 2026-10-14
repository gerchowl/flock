"""Exercise the shim's release waiter without starting a fleet or a relay."""
import ast
import io
import signal
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import Mock


class PollReleaseWait(unittest.TestCase):
    def waiter(self, gate, *, expired=False, exited=False):
        source = Path(__file__).resolve().parents[1] / 'tests/support/fleet_ssh.py'
        # Only the waiter is loaded: the shim's top level owns real subprocesses.
        tree = ast.parse(source.read_text())
        function = next(n for n in tree.body if isinstance(n, ast.FunctionDef)
                        and n.name == 'wake_on_poll_release')
        clock = Mock(side_effect=[0, 31] if expired else [0, 0])
        failure = RuntimeError('shim terminated')
        namespace = dict(
            time=types.SimpleNamespace(monotonic=clock, sleep=Mock()),
            child=types.SimpleNamespace(poll=lambda: 1 if exited else None),
            emit=Mock(), json=__import__('json'), sys=types.SimpleNamespace(stderr=io.StringIO()),
            os=types.SimpleNamespace(getpid=lambda: 123, kill=Mock(side_effect=failure)),
            signal=signal,
        )
        exec(compile(ast.Module(body=[function], type_ignores=[]), str(source), 'exec'), namespace)
        return namespace

    def test_silent_live_producer_fails_at_ceiling(self):
        with tempfile.TemporaryDirectory() as directory:
            gate = Path(directory)
            ns = self.waiter(gate, expired=True)
            with self.assertRaisesRegex(RuntimeError, 'shim terminated'):
                ns['wake_on_poll_release'](gate)
            self.assertIn('poll gate was not released', ns['sys'].stderr.getvalue())
            self.assertIn('poll gate was not released', (gate / 'failed').read_text())
            ns['os'].kill.assert_called_once_with(123, signal.SIGTERM)
            ns['emit'].assert_not_called()

    def test_dead_producer_fails_before_release(self):
        with tempfile.TemporaryDirectory() as directory:
            gate = Path(directory)
            ns = self.waiter(gate, exited=True)
            with self.assertRaisesRegex(RuntimeError, 'shim terminated'):
                ns['wake_on_poll_release'](gate)
            self.assertIn('relay exited before poll release', ns['sys'].stderr.getvalue())
            ns['emit'].assert_not_called()

    def test_release_emits_one_wake(self):
        with tempfile.TemporaryDirectory() as directory:
            gate = Path(directory)
            (gate / 'release').touch()
            ns = self.waiter(gate)
            ns['wake_on_poll_release'](gate)
            ns['emit'].assert_called_once_with('{"push": "mesh.wake"}\n')
            ns['os'].kill.assert_not_called()

    def test_removed_gate_fails_instead_of_waking(self):
        with tempfile.TemporaryDirectory() as directory:
            gate = Path(directory) / 'removed'
            ns = self.waiter(gate)
            with self.assertRaisesRegex(RuntimeError, 'shim terminated'):
                ns['wake_on_poll_release'](gate)
            ns['emit'].assert_not_called()
