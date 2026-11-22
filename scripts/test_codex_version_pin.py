"""CI compatibility declaration checked against recorded/scripted Codex fixtures."""
import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]


class CodexVersionPin(unittest.TestCase):
    def test_declared_version_matches_recorded_metadata_and_writer_fixture(self):
        source = (ROOT / 'src/codex_version.rs').read_text()
        pin = re.search(r'const TESTED_VERSION: &str = "([^"]+)";', source)[1]
        self.assertEqual(pin, '0.160.1', 'a compatibility expansion needs reviewed fixtures/tests')
        for fixture in ('tests/fixtures/codex/finished.jsonl',
                        'tests/fixtures/codex-rollout/real-0.160.1.jsonl'):
            with self.subTest(fixture=fixture):
                record = json.loads((ROOT / fixture).read_text().splitlines()[0])
                self.assertEqual(record['payload']['cli_version'], pin)
        self.assertEqual((ROOT / 'tests/fixtures/codex-version/version.stdout').read_text(),
                         f'codex-cli {pin}\n')
