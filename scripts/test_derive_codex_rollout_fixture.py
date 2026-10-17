"""Fixture derivation preserves storage shape while excluding private content."""
import json
import unittest
from scripts.derive_codex_rollout_fixture import derive


class DerivationTests(unittest.TestCase):
    def source(self):
        return [
            {"type": "session_meta", "ordinal": 0, "payload": {
                "cli_version": "0.160.1", "id": "private-session", "cwd": "private-path",
                "history_mode": "paginated", "source": "cli"}},
            {"type": "event_msg", "ordinal": 1, "payload": {
                "type": "task_started", "turn_id": "private-turn"}},
            {"type": "turn_context", "ordinal": 2, "payload": {
                "cwd": "private-path", "nested": [None, True, 42, {"text": "private-text"}]}},
            {"type": "event_msg", "ordinal": 3, "payload": {
                "type": "task_complete", "turn_id": "private-turn"}},
        ]

    def test_preserves_every_shape_and_nonstring_value(self):
        source = self.source()
        actual = derive(map(json.dumps, source))

        def compare(before, after):
            self.assertIs(type(before), type(after))
            if isinstance(before, dict):
                self.assertEqual(list(before), list(after))
                for key in before:
                    compare(before[key], after[key])
            elif isinstance(before, list):
                self.assertEqual(len(before), len(after))
                for left, right in zip(before, after):
                    compare(left, right)
            elif not isinstance(before, str):
                self.assertEqual(before, after)
        compare(source, actual)
        self.assertNotIn("private-", json.dumps(actual))
        self.assertEqual(actual[1]["payload"]["turn_id"], actual[3]["payload"]["turn_id"])
        self.assertEqual(actual[0]["payload"]["cwd"], actual[2]["payload"]["cwd"])
        self.assertEqual(actual[0]["payload"]["cli_version"], "0.160.1")
        self.assertEqual(actual[1]["payload"]["type"], "task_started")
        self.assertEqual(actual[0]["payload"]["history_mode"], "paginated")
        self.assertEqual(actual[0]["payload"]["source"], "cli")
        self.assertEqual(derive(map(json.dumps, source)), actual)

    def test_stops_only_at_matching_completion_and_keeps_all_intervening_records(self):
        source = self.source()
        other = {"type": "event_msg", "ordinal": 3, "payload": {
            "type": "task_complete", "turn_id": "other-turn"}}
        source.insert(3, other)
        actual = derive(map(json.dumps, source + [{"secret": "must-not-be-read"}]))
        self.assertEqual(len(actual), len(source))
        self.assertEqual(actual[3]["ordinal"], 3)

    def test_rejects_wrong_version_unknown_discriminants_and_incomplete_turn(self):
        source = self.source()
        with self.assertRaisesRegex(ValueError, "no matching"):
            derive(map(json.dumps, source[:-1]))
        source[0]["payload"]["cli_version"] = "0.159.0"
        with self.assertRaisesRegex(ValueError, "0.160.1"):
            derive(map(json.dumps, source))
        for key in ("type", "source", "role"):
            source = self.source()
            source[2]["payload"][key] = "unreviewed-private-value"
            with self.assertRaisesRegex(ValueError, "explicit public review"):
                derive(map(json.dumps, source))
