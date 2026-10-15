"""Drive both issue automation steps through their real reference parsing."""
from pathlib import Path
import json
import os
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]

FAKE_GH = '''import json, os, sys
from pathlib import Path
args = sys.argv[1:]
fixture = json.loads(Path(os.environ['ISSUE_FIXTURE']).read_text())
if args[0] == 'api':
    endpoint = next(arg for arg in args[1:] if arg == 'user' or arg.startswith('repos/'))
    if endpoint == 'user':
        print('fixture-contributor')
    elif '/comments?' in endpoint:
        print('\\n'.join(fixture.get('comments', [])))
    elif '/labels/' in endpoint:
        print('{}')
    elif endpoint.endswith('/issues/123'):
        print(json.dumps(fixture))
    else:
        raise SystemExit('unexpected API endpoint: ' + endpoint)
elif args[:1] == ['issue'] and args[1] in ('edit', 'close', 'comment'):
    with open(os.environ['MUTATIONS'], 'a') as output:
        output.write(json.dumps(args) + '\\n')
else:
    raise SystemExit('unexpected gh command: ' + repr(args))
'''

FAKE_GIT = '''#!/bin/sh
case "$1" in
  log) printf '%s\\n' "$REF_TEXT" ;;
  rev-list) printf '%s\\n' '2222222222222222222222222222222222222222' ;;
  describe) printf '%s\\n' 'v1.0.0' ;;
  *) exit 42 ;;
esac
'''


def workflow_step(filename):
    result = subprocess.run(
        ['ruby', '-ryaml', '-rjson', '-e',
         'puts JSON.generate(YAML.load_file(ARGV[0]))',
         str(ROOT / '.github/workflows' / filename)],
        capture_output=True, text=True, check=True,
    )
    document = json.loads(result.stdout)
    return next(step['run'] for job in document['jobs'].values()
                for step in job.get('steps', [])
                if step.get('name', '').startswith('Close issues referenced'))


class ReleaseIssueExclusion(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflows = {name: workflow_step(name) for name in
                         ('label-next-release-issues.yml', 'release.yml')}

    def drive(self, workflow, *, labels=(), title='Repair a pane', state='open',
              refs='Refs #123', comments=(), omit_title=False, pull_request=False):
        fixture = {'title': title, 'state': state,
                   'labels': [{'name': label} for label in labels],
                   'comments': list(comments)}
        if omit_title:
            del fixture['title']
        if pull_request:
            fixture['pull_request'] = {}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'issue.json').write_text(json.dumps(fixture))
            (root / 'gh').write_text(f'#!{sys.executable}\n' + FAKE_GH)
            (root / 'git').write_text(FAKE_GIT)
            (root / 'gh').chmod(0o755)
            (root / 'git').chmod(0o755)
            mutations = root / 'mutations.jsonl'
            env = dict(PATH=directory + os.pathsep + os.environ['PATH'],
                       ISSUE_FIXTURE=str(root / 'issue.json'), MUTATIONS=str(mutations),
                       REF_TEXT=refs, GITHUB_REPOSITORY='example/flock',
                       GITHUB_REF_NAME='v1.1.0', GH_TOKEN='fictional-token',
                       NEXT_RELEASE_LABEL='pending-release',
                       LEGACY_NEXT_RELEASE_LABEL='included-in-next-release')
            script = self.workflows[workflow]
            script = script.replace('${{ github.event.before }}', '1' * 40)
            script = script.replace('${{ github.sha }}', '2' * 40)
            result = subprocess.run(['bash', '-c', script], cwd=ROOT, env=env,
                                    capture_output=True, text=True)
            changes = [json.loads(line) for line in mutations.read_text().splitlines()] \
                if mutations.exists() else []
            return result, changes

    def test_referenced_trackers_are_not_labelled_or_closed_on_dev(self):
        for labels, title in ((('tracking',), 'A roadmap'), (('epic',), 'An epic'),
                              ((), 'A roadmap (tracking)'), (('Tracking',), 'A roadmap'),
                              ((), 'A roadmap (Tracking)')):
            with self.subTest(labels=labels, title=title):
                result, changes = self.drive('label-next-release-issues.yml',
                                             labels=labels, title=title)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(changes, [])

    def test_already_pending_trackers_are_not_closed_at_release(self):
        for label in ('pending-release', 'included-in-next-release'):
            for labels, title in ((('tracking',), 'A roadmap'), (('epic',), 'An epic'),
                                  ((), 'A roadmap (tracking)')):
                with self.subTest(label=label, labels=labels, title=title):
                    result, changes = self.drive('release.yml',
                                                 labels=(*labels, label), title=title)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(changes, [])

    def test_normal_issue_is_still_labelled_and_closed_on_dev(self):
        result, changes = self.drive('label-next-release-issues.yml')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([change[1] for change in changes], ['edit', 'close'])
        self.assertIn('--add-label', changes[0])
        self.assertIn('pending-release', changes[0])
        self.assertIn('<!-- flock:pending-release -->', changes[1][-1])

    def test_normal_pending_issue_is_still_closed_at_release(self):
        result, changes = self.drive('release.yml', labels=('pending-release',))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([change[1] for change in changes], ['close', 'edit'])
        self.assertIn('<!-- flock:released:v1.1.0 -->', changes[0][-1])
        self.assertIn('--remove-label', changes[1])
        self.assertIn('pending-release', changes[1])

    def test_unrelated_labels_and_mid_title_tracking_text_remain_eligible(self):
        for workflow in self.workflows:
            for labels, title in ((('tracking-help',), 'Fix tracking controls'),
                                  (('epics',), 'Roadmap (tracking) follow-up')):
                with self.subTest(workflow=workflow, labels=labels, title=title):
                    result, changes = self.drive(workflow, labels=labels, title=title)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn('close', [change[1] for change in changes])

    def test_closed_pending_tracker_gets_no_release_comment_or_label_removal(self):
        result, changes = self.drive('release.yml', state='closed',
                                     labels=('tracking', 'pending-release'))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(changes, [])

    def test_existing_release_comment_does_not_bypass_tracker_exclusion(self):
        result, changes = self.drive('release.yml', labels=('epic', 'pending-release'),
                                     comments=('<!-- flock:released:v1.1.0 -->',))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(changes, [])

    def test_part_of_reference_does_not_select_an_issue(self):
        for workflow in self.workflows:
            with self.subTest(workflow=workflow):
                result, changes = self.drive(workflow, refs='Part of #123')
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(changes, [])

    def test_pull_request_reference_is_still_ignored(self):
        for workflow in self.workflows:
            with self.subTest(workflow=workflow):
                result, changes = self.drive(workflow, pull_request=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(changes, [])

    def test_malformed_issue_metadata_fails_before_any_issue_mutation(self):
        for workflow in self.workflows:
            with self.subTest(workflow=workflow):
                result, changes = self.drive(workflow, omit_title=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(changes, [])


if __name__ == '__main__':
    unittest.main()
