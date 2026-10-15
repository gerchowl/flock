"""Coverage and result-policy regressions from the CI shard review (#1008)."""

import fnmatch
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from scripts.ci_shards import serial_group_binaries, verify
from scripts.test_ci_shards import CONFIG

ROOT = Path(__file__).resolve().parents[1]


def named_steps(path, name):
    text = path.read_text()
    starts = list(re.finditer(r'^      - name: (.+)$', text, re.M))
    return [text[step.start():starts[i + 1].start() if i + 1 < len(starts) else len(text)]
            for i, step in enumerate(starts) if step[1] == name]


def job(text, name):
    match = re.search(rf'^  {re.escape(name)}:\n(.*?)(?=^  [\w-]+:|\Z)', text, re.M | re.S)
    if match is None:
        raise AssertionError(f'missing job {name}')
    return match[1]


class WorkflowFollowups(unittest.TestCase):
    def test_malformed_summary_is_advisory_in_every_summary_step(self):
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / 'junit.xml'
            report.write_text('<testsuites><broken>')
            result = subprocess.run(
                [sys.executable, str(ROOT / 'scripts/ci_shards.py'), 'summary',
                 '--title', 'fixture', f'shard={report}'],
                capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('ParseError', result.stderr)
        for workflow, name, count in [('ci.yml', 'Summarize shards', 2),
                                      ('ci-leg.yml', 'Summarize', 1)]:
            steps = named_steps(ROOT / '.github/workflows' / workflow, name)
            self.assertEqual(len(steps), count)
            for step in steps:
                with self.subTest(workflow=workflow, step=step):
                    self.assertIn('\n        continue-on-error: true\n', step)

    def test_every_aggregate_retains_its_result_gate_when_not_cancelled(self):
        text = (ROOT / '.github/workflows/ci.yml').read_text()
        for name in ['check-ubuntu', 'check-macos']:
            block = job(text, name)
            with self.subTest(job=name):
                self.assertIn('    if: ${{ !cancelled() }}\n', block)
                self.assertNotRegex(block, r'(?m)^    continue-on-error:')
                gate = block.split('      - name: Require every job of the leg\n', 1)[1]
                script = '\n'.join(line[10:] for line in gate.split('        run: |\n', 1)[1].splitlines()
                                   if line.startswith('          '))
                for state in ['success', 'failure', 'cancelled', 'skipped']:
                    result = subprocess.run(['/bin/sh', '-c', script],
                                            env={**os.environ, 'RESULT': state},
                                            capture_output=True, text=True, timeout=10)
                    self.assertEqual(result.returncode, 0 if state == 'success' else 1,
                                     (state, result.stderr))

    def test_nix_pushes_include_every_sharding_input(self):
        text = (ROOT / '.github/workflows/nix.yml').read_text()
        push = text.split('  push:\n', 1)[1].split('\npermissions:', 1)[0]
        patterns = re.findall(r'^      - "([^"]+)"', push, re.M)
        for path in ['.github/workflows/ci.yml', '.github/workflows/ci-leg.yml',
                     '.config/ci-shards.toml', '.config/nextest.toml', 'scripts/ci_shards.py']:
            with self.subTest(path=path):
                self.assertTrue(any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns), path)


class SerialFilters(unittest.TestCase):
    def read(self, expression, profile='default'):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'nextest.toml'
            path.write_text(f'[[profile.{profile}.overrides]]\n'
                            f'filter = {json.dumps(expression)}\n'
                            'test-group = "serial-pty"\n')
            return serial_group_binaries(path)

    def test_literal_union_accepts_whitespace_and_exact_prefix(self):
        self.assertEqual(self.read(' binary(a) | binary(=b) '), {'a', 'b'})

    def test_ci_profile_group_assignments_are_checked_too(self):
        self.assertEqual(self.read('binary(ci_only)', 'ci'), {'ci_only'})

    def test_platform_condition_is_rejected_with_context(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'nextest.toml'
            path.write_text('[[profile.ci.overrides]]\nfilter="binary(a)"\n'
                            'test-group="serial-pty"\nplatform="cfg(unix)"\n')
            with self.assertRaisesRegex(SystemExit, 'unsupported serial-pty platform condition') as caught:
                serial_group_binaries(path)
            self.assertIn('profile.ci.overrides[0]', str(caught.exception))
            self.assertIn('cfg(unix)', str(caught.exception))

    def test_filter_error_identifies_the_profile_override_and_expression(self):
        with self.assertRaises(SystemExit) as caught:
            self.read('package(flock-ai)', 'ci')
        message = str(caught.exception)
        self.assertIn('profile.ci.overrides[0]', message)
        self.assertIn('package(flock-ai)', message)

    def test_verify_rejects_unsupported_filter_before_querying_the_archive(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'scripts').mkdir()
            (root / '.config').mkdir()
            script = root / 'scripts/ci_shards.py'
            script.write_text((ROOT / 'scripts/ci_shards.py').read_text())
            (root / '.config/ci-shards.toml').write_text(
                '[legs.fixture]\nfilter="all()"\n[[shards]]\n'
                'name="rest"\nserial=false\nrest=true\n')
            (root / '.config/nextest.toml').write_text(
                '[[profile.default.overrides]]\nfilter="package(flock-ai)"\n'
                'test-group="serial-pty"\n')
            result = subprocess.run(
                [sys.executable, str(script), 'verify', '--leg', 'fixture',
                 '--archive-file', str(root / 'absent-archive')],
                env={'PATH': ''}, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 1)
            self.assertIn('unsupported serial-pty filter', result.stderr)
            self.assertIn('profile.default.overrides[0]', result.stderr)
            self.assertNotIn('FileNotFoundError', result.stderr)

    def test_unsupported_filters_fail_instead_of_extracting_partial_names(self):
        for expression in ['package(flock-ai)', 'test(slow)', 'binary(a) & test(slow)',
                           'binary(a) | package(flock-ai)', 'not binary(a)', 'binary(/a/)',
                           'binary(a*)', 'binary(~a)', 'binary(a) trailing',
                           'binary(a) || binary(b)', 'binary(a) |', '']:
            with self.subTest(expression=expression):
                with self.assertRaisesRegex(SystemExit, 'unsupported serial-pty filter'):
                    self.read(expression)


class MatrixLabels(unittest.TestCase):
    def test_labels_only_name_binaries_selected_for_the_leg(self):
        known = {'p', 'a', 'b', 'c', 'other'}
        selections = {'all()': known, 'not binary(c)': known - {'c'},
                      '(not binary(c)) & (binary(p))': {'p'},
                      '(not binary(c)) & not (binary(p) | binary(a) | binary(b) | binary(c))': {'other'},
                      '(not binary(c)) & (binary(a))': {'a'},
                      '(not binary(c)) & (binary(b) | binary(c))': {'b'}}

        def listing(_archive, expression):
            return {'rust-suites': {f'fixture::{name}': {
                'binary-name': name, 'testcases': {'test': {'filter-match': {
                    'status': 'matches' if name in selections[expression] else 'mismatch'}}}}
                for name in known}}

        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'output'
            with patch('scripts.ci_shards.nextest_list', side_effect=listing), \
                 patch('scripts.ci_shards.serial_group_binaries', return_value={'a', 'b', 'c'}), \
                 patch.dict(os.environ, {'GITHUB_OUTPUT': str(output)}):
                self.assertEqual(verify(CONFIG, 'mac', 'fixture-archive'), 0)
            matrix = json.loads(output.read_text().removeprefix('shards='))
            self.assertEqual(next(shard['label'] for shard in matrix if shard['name'] == 's2'), 'b')
