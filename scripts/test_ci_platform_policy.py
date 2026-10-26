"""Owner-approved platform scheduling and required-gate contracts (#1613)."""
import os
import re
import subprocess
import unittest

from scripts.ci_shards import load_config
from scripts.test_actions_cache import workflow


class PlatformPolicy(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.ci = workflow('ci.yml')
        cls.leg = workflow('ci-leg.yml')
        cls.shards = [shard.name for shard in load_config().shards]

    def enabled(self, platform, event, head, ref):
        default = self.leg['on']['workflow_call']['inputs'].get('run-tests', {}).get('default', True)
        value = self.ci['jobs'][platform]['with'].get('run-tests', default)
        if isinstance(value, bool):
            return value
        match = re.fullmatch(
            r"\$\{\{ github.event_name != '([^']+)' \|\| startsWith\(github\.(head_ref|ref), '([^']+)'\) \}\}", value)
        self.assertIsNotNone(match, value)
        excluded_event, field, prefix = match.groups()
        context = {'head_ref': head, 'ref': ref}
        return event != excluded_event or context[field].casefold().startswith(prefix.casefold())

    def scheduled_shards(self, platform, event, head, ref):
        test = self.leg['jobs']['test']
        condition = test.get('if')
        if condition is not None:
            self.assertEqual(condition, '${{ inputs.run-tests }}')
            if not self.enabled(platform, event, head, ref):
                return []
        self.assertEqual(test['needs'], 'build')
        self.assertEqual(test['strategy']['matrix'],
                         {'shard': '${{ fromJSON(needs.build.outputs.shards) }}'})
        return self.shards

    def test_ordinary_pr_runs_linux_shards_and_no_macos_shards(self):
        for head in ['issue/17-feature', 'landing/b230']:
            with self.subTest(head=head):
                context = ('pull_request', head, 'refs/pull/17/merge')
                self.assertEqual(self.scheduled_shards('macos', *context), [])
                self.assertEqual(self.scheduled_shards('ubuntu', *context), self.shards)

    def test_batch_pr_pushes_and_merge_groups_run_every_shard(self):
        contexts = [('pull_request', 'land/b230', 'refs/pull/18/merge'),
                    ('push', '', 'refs/heads/dev'), ('push', '', 'refs/heads/main'),
                    ('merge_group', '', 'refs/heads/gh-readonly-queue/dev/fixture')]
        for context in contexts:
            for platform in ['macos', 'ubuntu']:
                with self.subTest(context=context, platform=platform):
                    self.assertEqual(self.scheduled_shards(platform, *context), self.shards)

    def test_gui_fixture_check_gates_linux_before_archive_upload(self):
        steps = self.leg['jobs']['build']['steps']
        matching = [s for s in steps if s.get('name') == 'Check GUI embedding and feature exclusion']
        self.assertEqual(len(matching), 1)
        check = matching[0]
        self.assertEqual(check['if'], "runner.os == 'Linux' && inputs.run-tests")
        self.assertEqual(check['run'], 'just gui-check')
        self.assertFalse(check.get('continue-on-error', False))
        archive = next(s for s in steps if s.get('id') == 'archive')
        upload = next(s for s in steps if s.get('name') == 'Upload test archive')
        self.assertLess(steps.index(archive), steps.index(check))
        self.assertLess(steps.index(check), steps.index(upload))

    def test_reusable_leg_defaults_to_full_tests_and_keeps_build_and_lint(self):
        self.assertEqual(self.leg['on']['workflow_call']['inputs']['run-tests']['type'], 'boolean')
        self.assertIs(self.leg['on']['workflow_call']['inputs']['run-tests']['default'], True)
        for name in ['build', 'lint']:
            self.assertNotIn('if', self.leg['jobs'][name])
        archive = next(step for step in self.leg['jobs']['build']['steps'] if step.get('id') == 'archive')
        self.assertIn('just ci-archive', archive['run'])
        self.assertNotIn('if', archive)

    def test_required_gates_fail_cancelled_failed_skipped_or_missing_legs_in_every_context(self):
        for event, head, ref in [('pull_request', 'issue/17-feature', 'refs/pull/17/merge'),
                                 ('pull_request', 'land/b230', 'refs/pull/18/merge'),
                                 ('push', '', 'refs/heads/dev'), ('push', '', 'refs/heads/main')]:
            for platform, gate_name in [('macos', 'check-macos'), ('ubuntu', 'check-ubuntu')]:
                gate = self.ci['jobs'][gate_name]
                self.assertEqual(gate['needs'], ['changes', platform] + (['script-tests'] if platform == 'ubuntu' else []))
                self.assertEqual(gate['if'], 'always()')
                self.assertFalse(gate.get('continue-on-error', False))
                step = next(step for step in gate['steps'] if step['name'] == 'Require every job of the leg')
                self.assertEqual(step.get('if'), 'always()')
                self.assertEqual(step['env']['RESULT'], '${{ needs.' + platform + '.result }}')
                self.assertFalse(step.get('continue-on-error', False))
                self.assertEqual(step['env']['SKIP_RUST'], '${{ needs.changes.outputs.skip_rust }}')
                for result in ['success', 'failure', 'cancelled', 'skipped', '']:
                    with self.subTest(event=event, head=head, platform=platform, result=result):
                        outcome = subprocess.run(['/bin/sh', '-c', step['run']],
                                                 env={**os.environ, 'RESULT': result, 'SKIP_RUST': 'false'},
                                                 capture_output=True, text=True, timeout=10)
                        self.assertEqual(outcome.returncode, 0 if result == 'success' else 1)
                        if result != 'success':
                            self.assertIn('leg ended as', outcome.stderr)

    def test_macos_gate_asserts_expected_shards_only_for_full_matrix_events(self):
        gate = self.ci['jobs']['check-macos']
        step = next(step for step in gate['steps'] if step['name'].startswith('Require the expected test shards'))
        self.assertEqual(step.get('if'), 'always()')
        self.assertFalse(step.get('continue-on-error', False))
        self.assertEqual(step['env']['FULL'], self.ci['jobs']['macos']['with']['run-tests'])
        self.assertEqual(step['env']['SHARDS'], '${{ needs.macos.outputs.shards }}')
        self.assertEqual(self.leg['on']['workflow_call']['outputs']['shards']['value'],
                         '${{ jobs.build.outputs.shards }}')

        def run(full, shards, reports):
            import tempfile
            with tempfile.TemporaryDirectory() as tmp:
                for i in range(reports):
                    os.makedirs(f'{tmp}/junit/s{i}')
                    open(f'{tmp}/junit/s{i}/junit.xml', 'w').close()
                return subprocess.run(['/bin/sh', '-c', step['run']], cwd=tmp, capture_output=True, text=True,
                                      env={**os.environ, 'FULL': full, 'SHARDS': shards}, timeout=10).returncode
        three = '[{"name":"a"},{"name":"b"},{"name":"c"}]'
        self.assertEqual(run('true', three, 3), 0)
        self.assertEqual(run('true', three, 2), 1)
        self.assertEqual(run('true', three, 0), 1)
        self.assertEqual(run('true', '', 0), 1)
        self.assertEqual(run('false', three, 0), 0)

    def test_rust_legs_skip_only_when_detection_says_so_and_gates_still_report(self):
        for platform in ['ubuntu', 'macos']:
            job = self.ci['jobs'][platform]
            self.assertEqual(job['needs'], 'changes')
            self.assertEqual(job['if'], "${{ !cancelled() && needs.changes.outputs.skip_rust != 'true' }}")
        gate = self.ci['jobs']['check-macos']
        step = next(step for step in gate['steps'] if step['name'] == 'Require every job of the leg')
        for result, skip, code in [('skipped', 'true', 0), ('skipped', 'false', 1), ('skipped', '', 1),
                                   ('failure', 'true', 1), ('cancelled', 'true', 1), ('success', 'true', 0),
                                   ('success', '', 0)]:
            with self.subTest(result=result, skip=skip):
                outcome = subprocess.run(['/bin/sh', '-c', step['run']], capture_output=True, text=True,
                                         env={**os.environ, 'RESULT': result, 'SKIP_RUST': skip}, timeout=10)
                self.assertEqual(outcome.returncode, code)

    def test_changes_job_never_skips_outside_ordinary_prs(self):
        job = self.ci['jobs']['changes']
        decide = next(step for step in job['steps'] if step.get('id') == 'decide')
        self.assertIn('scripts/ci_changes.py', decide['run'])
        self.assertIn('git diff --no-renames --name-only', decide['run'])
        self.assertEqual(decide['env']['HEAD_REF'], '${{ github.head_ref }}')
        self.assertEqual(job['steps'][0]['with']['fetch-depth'], 0)
        out = subprocess.run(['/bin/sh', '-c', decide['run'].split('fi\n')[0] + 'fi'], capture_output=True,
                             text=True, env={**os.environ, 'EVENT': 'push', 'GITHUB_OUTPUT': '/dev/stdout'})
        self.assertEqual(out.stdout.strip(), 'skip_rust=false')

    def test_script_tests_run_in_a_job_that_is_never_skipped_and_gate_the_ubuntu_check(self):
        job = self.ci['jobs']['script-tests']
        self.assertNotIn('if', job)
        self.assertIn('nix develop -c just script-tests', [s.get('run') for s in job['steps']])
        self.assertIn('script-tests', self.ci['jobs']['check-ubuntu']['needs'])
        lint_runs = [s.get('run', '') for s in self.leg['jobs']['lint']['steps']]
        self.assertFalse(any('script-tests' in r for r in lint_runs))

    def test_ubuntu_gate_fails_unless_script_tests_succeeded(self):
        gate = self.ci['jobs']['check-ubuntu']
        step = next(step for step in gate['steps'] if step['name'] == 'Require the maintenance script tests')
        self.assertEqual(step['if'], 'always()')
        self.assertEqual(step['env']['SCRIPT_TESTS'], '${{ needs.script-tests.result }}')
        for result, code in [('success', 0), ('failure', 1), ('cancelled', 1), ('skipped', 1), ('', 1)]:
            outcome = subprocess.run(['/bin/sh', '-c', step['run']], capture_output=True, text=True,
                                     env={**os.environ, 'SCRIPT_TESTS': result}, timeout=10)
            self.assertEqual(outcome.returncode, code, result)
