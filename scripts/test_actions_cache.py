"""Cache policy contracts: writers are dev, readers include pull requests."""
from pathlib import Path
import json
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / '.github/workflows'
DEV = "${{ github.ref == 'refs/heads/dev' }}"


def workflow(name):
    # Ruby's YAML parser is also used by the existing hook configuration test.
    result = subprocess.run(
        ['ruby', '-ryaml', '-rjson', '-e',
         'd = YAML.load_file(ARGV[0]); d["on"] = d.delete(true) if d.key?(true); puts JSON.generate(d)', str(WORKFLOWS / name)],
        check=True, capture_output=True, text=True,
    )
    return json.loads(result.stdout)


def steps(document):
    return [step for job in document['jobs'].values() for step in job.get('steps', [])]


class ActionsCachePolicy(unittest.TestCase):
    def test_zig_build_caches_are_disabled_in_every_build_workflow(self):
        count = 0
        for name in ('ci-leg.yml', 'preview.yml', 'release.yml', 'build-artifacts-manual.yml'):
            for step in steps(workflow(name)):
                if step.get('uses', '').startswith('mlugg/setup-zig@'):
                    count += 1
                    with self.subTest(workflow=name):
                        self.assertIs(step['with'].get('use-cache'), False)
        self.assertEqual(count, 7)

    def test_rust_caches_restore_on_prs_and_only_save_on_dev(self):
        count = 0
        for name in ('ci-leg.yml', 'preview.yml', 'release.yml'):
            for step in steps(workflow(name)):
                if step.get('uses', '').startswith('Swatinem/rust-cache@'):
                    count += 1
                    with self.subTest(workflow=name):
                        self.assertRegex(step['uses'], r'@[a-f0-9]{40}$')
                        self.assertEqual(step['with'].get('save-if'), DEV)
                        self.assertNotIn('github.ref', step.get('if', ''))
        self.assertEqual(count, 5)

    def test_nix_cache_is_bounded_pinned_and_written_only_by_dev(self):
        document = workflow('nix.yml')
        cache, = [s for s in steps(document)
                  if s.get('uses', '').startswith('nix-community/cache-nix-action@')]
        self.assertRegex(cache['uses'], r'@[a-f0-9]{40}$')
        inputs = cache['with']
        self.assertEqual(inputs['save'], DEV)
        self.assertEqual(inputs['purge'], DEV)
        self.assertEqual(inputs['gc-max-store-size-linux'], '2G')
        self.assertEqual(inputs['purge-created'], 0)
        self.assertEqual(inputs['purge-primary-key'], 'never')
        self.assertEqual(inputs['purge-prefixes'], inputs['restore-prefixes-first-match'])
        self.assertIn("hashFiles('flake.lock'", inputs['primary-key'])
        self.assertNotIn('github.run_id', inputs['primary-key'])
        self.assertNotIn('if', cache)
        self.assertEqual(document['jobs']['flake-check']['permissions']['actions'], 'write')

    def test_closed_pr_cleanup_never_checks_out_or_executes_pr_code(self):
        document = workflow('cache-cleanup.yml')
        self.assertEqual(document['on'], {'pull_request_target': {'types': ['closed']}})
        self.assertEqual(document['permissions'], {'actions': 'write'})
        self.assertEqual(len(document['jobs']), 1)
        commands = steps(document)
        self.assertEqual(len(commands), 1)
        step, = commands
        self.assertNotIn('uses', step)
        self.assertEqual(step['env']['GH_TOKEN'], '${{ github.token }}')
        self.assertEqual(step['env']['PR_NUMBER'], '${{ github.event.pull_request.number }}')
        self.assertEqual(step['env']['GH_REPO'], '${{ github.repository }}')
        self.assertEqual(step['run'],
                         'gh cache delete --all --ref "refs/pull/${PR_NUMBER}/merge" --succeed-on-no-caches\n')

    def test_cleanup_scopes_the_cli_request_and_propagates_api_failure(self):
        step, = steps(workflow('cache-cleanup.yml'))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            capture = root / 'args'
            gh = root / 'gh'
            gh.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$CAPTURE"\nexit "$RESULT"\n')
            gh.chmod(0o755)
            for status in (0, 1, 4):
                with self.subTest(status=status):
                    result = subprocess.run(
                        ['/bin/sh', '-eu', '-c', step['run']],
                        env={'PATH': directory, 'CAPTURE': str(capture),
                             'PR_NUMBER': '123', 'GH_REPO': 'example/flock',
                             'GH_TOKEN': 'fictional-token', 'RESULT': str(status)},
                        capture_output=True, text=True,
                    )
                    self.assertEqual(result.returncode, status)
                    self.assertEqual(capture.read_text().splitlines(),
                                     ['cache', 'delete', '--all', '--ref',
                                      'refs/pull/123/merge', '--succeed-on-no-caches'])


if __name__ == '__main__':
    unittest.main()
