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
        found = set()
        for name in ('ci-leg.yml', 'preview.yml', 'release.yml', 'build-artifacts-manual.yml'):
            for job_name, job in workflow(name)['jobs'].items():
                for step in job.get('steps', []):
                    if step.get('uses', '').startswith('mlugg/setup-zig@'):
                        self.assertNotIn((name, job_name), found)
                        found.add((name, job_name))
                        with self.subTest(workflow=name, job=job_name):
                            self.assertIs(step['with'].get('use-cache'), False)
        self.assertEqual(found, {
            ('ci-leg.yml', 'build'), ('preview.yml', 'preflight'),
            ('preview.yml', 'build'), ('release.yml', 'build'),
            ('build-artifacts-manual.yml', 'build-linux'),
            ('build-artifacts-manual.yml', 'build-macos'),
        })

    def test_ci_zig_cache_has_stable_keys_and_dev_only_save(self):
        for name in ('build',):
            job = workflow('ci-leg.yml')['jobs'][name]
            restore, = [s for s in job['steps']
                        if s.get('uses', '').startswith('actions/cache/restore@')]
            save, = [s for s in job['steps']
                     if s.get('uses', '').startswith('actions/cache/save@')]
            with self.subTest(job=name):
                self.assertRegex(restore['uses'], r'@[a-f0-9]{40}$')
                self.assertRegex(save['uses'], r'@[a-f0-9]{40}$')
                self.assertNotIn('if', restore)
                inputs = restore['with']
                self.assertIn('github.job', inputs['key'])
                self.assertIn('runner.arch', inputs['key'])
                self.assertIn('runner.os', inputs['key'])
                self.assertIn("hashFiles('vendor/libghostty-vt/**', 'build.rs')", inputs['key'])
                self.assertNotIn('github.run', inputs['key'])
                self.assertNotIn('github.sha', inputs['key'])
                self.assertNotIn('github.ref', inputs['key'])
                self.assertEqual(inputs['path'], '${{ github.workspace }}/.zig-cache')
                self.assertEqual(save['with']['path'], inputs['path'])
                self.assertEqual(save['with']['key'], '${{ steps.zig-cache.outputs.cache-primary-key }}')
                self.assertEqual(save['if'],
                                 "github.ref == 'refs/heads/dev' && steps.zig-cache.outputs.cache-hit != 'true'")
                self.assertEqual(inputs['key'].split('--dev-')[0] + '--', inputs['restore-keys'])
                restore_index = job['steps'].index(restore)
                save_index = job['steps'].index(save)
                compile_index = next(i for i, step in enumerate(job['steps'])
                                     if step.get('run', '').startswith(('just lint', 'just ci-archive')))
                self.assertLess(restore_index, compile_index)
                self.assertLess(compile_index, save_index)

    def test_rust_caches_restore_on_prs_and_only_save_on_dev(self):
        found = set()
        for name in ('ci-leg.yml', 'preview.yml', 'release.yml'):
            for job_name, job in workflow(name)['jobs'].items():
                for step in job.get('steps', []):
                    if step.get('uses', '').startswith('Swatinem/rust-cache@'):
                        self.assertNotIn((name, job_name), found)
                        found.add((name, job_name))
                        with self.subTest(workflow=name, job=job_name):
                            self.assertRegex(step['uses'], r'@[a-f0-9]{40}$')
                            self.assertEqual(step['with'].get('save-if'), DEV)
                            self.assertNotIn('github.ref', step.get('if', ''))
        self.assertEqual(found, {
            ('ci-leg.yml', 'build'), ('preview.yml', 'preflight'),
            ('preview.yml', 'build'), ('release.yml', 'build'),
        })

    def assert_ci_dev_shell_cache(self, document, job, command):
        install, = [s for s in job['steps']
                    if s.get('uses', '').startswith('cachix/install-nix-action@')]
        cache, = [s for s in job['steps']
                  if s.get('uses', '').startswith('nix-community/cache-nix-action@')]
        for step in (install, cache):
            self.assertRegex(step['uses'], r'@[a-f0-9]{40}$')
            self.assertNotIn('if', step)
        self.assertEqual(job.get('permissions', document['permissions']), {'contents': 'read'})
        inputs = cache['with']
        self.assertEqual(inputs['save'], DEV)
        self.assertFalse(inputs.get('purge', False))
        for platform in ('linux', 'macos'):
            self.assertEqual(inputs[f'gc-max-store-size-{platform}'], '2G')
        self.assertEqual(cache['uses'],
                         'nix-community/cache-nix-action@7df957e333c1e5da7721f60227dbba6d06080569')
        self.assertEqual(inputs['primary-key'],
                         "nix-v1-${{ runner.os }}-${{ hashFiles('flake.lock', 'flake.nix', 'nix/**') }}")
        for unstable in ('github.run', 'github.sha', 'github.ref'):
            self.assertNotIn(unstable, inputs['primary-key'])
        self.assertEqual(inputs['restore-prefixes-first-match'], 'nix-v1-${{ runner.os }}-')
        run = next(step for step in job['steps'] if step.get('run') == command)
        self.assertLess(job['steps'].index(install), job['steps'].index(cache))
        self.assertLess(job['steps'].index(cache), job['steps'].index(run))

    def test_ci_lint_uses_the_dev_shell_and_bounded_dev_only_store_cache(self):
        document = workflow('ci-leg.yml')
        job = document['jobs']['lint']
        self.assert_ci_dev_shell_cache(document, job, 'nix develop -c just lint')
        commands = {step['run'] for step in job['steps'] if 'run' in step}
        self.assertEqual(commands, {
            'nix develop -c just lint', 'nix develop -c just doc-check',
            'nix develop -c just script-tests',
        })

    def test_archived_shards_run_in_the_dev_shell_on_every_platform(self):
        document = workflow('ci-leg.yml')
        job = document['jobs']['test']
        steps = job['steps']
        run, = [s for s in steps if s.get('name') == 'Run shard']
        self.assertEqual(run['run'],
                         "nix develop -c just ci-shard '${{ inputs.leg }}' "
                         "'${{ matrix.shard.name }}' nextest-archive.tar.zst")
        self.assertNotIn('if', run)
        # The Nix installer otherwise points TMPDIR under the runner's HOME.
        self.assertEqual(run.get('env', {}).get('TMPDIR'), '/tmp')
        self.assert_ci_dev_shell_cache(document, job, run['run'])

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
