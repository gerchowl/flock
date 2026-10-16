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
                self.assertEqual(gate['needs'], platform)
                self.assertEqual(gate['if'], 'always()')
                self.assertFalse(gate.get('continue-on-error', False))
                step = next(step for step in gate['steps'] if step['name'] == 'Require every job of the leg')
                self.assertEqual(step.get('if'), 'always()')
                self.assertEqual(step['env']['RESULT'], '${{ needs.' + platform + '.result }}')
                self.assertFalse(step.get('continue-on-error', False))
                for result in ['success', 'failure', 'cancelled', 'skipped', '']:
                    with self.subTest(event=event, head=head, platform=platform, result=result):
                        outcome = subprocess.run(['/bin/sh', '-c', step['run']],
                                                 env={**os.environ, 'RESULT': result},
                                                 capture_output=True, text=True, timeout=10)
                        self.assertEqual(outcome.returncode, 0 if result == 'success' else 1)
                        if result != 'success':
                            self.assertIn('leg ended as', outcome.stderr)
