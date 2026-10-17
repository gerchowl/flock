#!/usr/bin/env python3
"""Derive a public fixture from a real 0.160.1 rollout's first completed turn.

Read only existing data, never launch Codex. Preserve all records through the
first matching completion, dictionary keys/order, container types, numbers,
booleans, nulls, ordinals and lifecycle discriminants. Neutralize string content
with stable replacements, retaining identity equality across references.
"""
import argparse
import json
from pathlib import Path

DISCRIMINANTS = {
    "session_meta", "event_msg", "response_item", "turn_context", "world_state",
    "token_usage_record", "compacted", "task_started", "turn_started",
    "task_complete", "turn_complete", "turn_aborted", "token_count",
    "item_started", "item_completed", "agent_message", "agent_reasoning",
    "user_message", "message", "reasoning", "function_call",
    "function_call_output", "custom_tool_call", "custom_tool_call_output",
    "input_text", "output_text", "summary_text", "reasoning_text",
    "thread_settings_applied", "context_compacted", "error", "warning",
    "AgentMessage", "CommandExecution", "Extension", "Reasoning", "Text",
    "UserMessage", "danger-full-access", "disabled", "list_files", "model",
    "read", "search", "text", "unknown",
    "web_search_call", "image_generation_call", "local_shell_call",
}
ROLES = {"user", "assistant", "system", "developer", "tool"}
REVIEWED_ENUMS = {
    "approval_policy": {"never"}, "approvals_reviewer": {"user"},
    "collaboration_mode_kind": {"default"}, "effort": {"medium"},
    "history_mode": {"paginated"}, "mode": {"default", "explicitRequestOnly"},
    "model_provider": {"openai"}, "originator": {"codex-tui"},
    "personality": {"pragmatic"}, "phase": {"commentary"},
    "reasoning_effort": {"medium"}, "source": {"cli", "unified_exec_startup"},
    "status": {"available", "completed", "failed", "none"}, "thread_source": {"user"},
}



def derive(lines):
    """Return neutral records through the first attributable completed turn."""
    replacements = {}

    def neutral(value, key=None):
        if isinstance(value, dict):
            return {k: neutral(v, k) for k, v in value.items()}
        if isinstance(value, list):
            return [neutral(v) for v in value]
        if not isinstance(value, str):
            return value
        if key == "type":
            if value not in DISCRIMINANTS:
                raise ValueError("unknown discriminator requires explicit public review")
            return value
        if key == "role":
            if value not in ROLES:
                raise ValueError("unknown enum requires explicit public review")
            return value
        if key in REVIEWED_ENUMS:
            if value not in REVIEWED_ENUMS[key]:
                raise ValueError("unknown enum requires explicit public review")
            return value
        if key == "cli_version":
            if value != "0.160.1":
                raise ValueError("fixture requires Codex 0.160.1")
            return value
        if key == "timestamp":
            return "2000-01-01T00:00:00.000Z"
        if value not in replacements:
            replacements[value] = f"neutral-{len(replacements):04d}"
        return replacements[value]

    records = []
    active = None
    for line in lines:
        record = json.loads(line)
        if not records and (record.get("type") != "session_meta"
                            or record.get("payload", {}).get("cli_version") != "0.160.1"):
            raise ValueError("first record must be 0.160.1 session metadata")
        records.append(neutral(record))
        payload = record.get("payload")
        if record.get("type") == "event_msg" and isinstance(payload, dict):
            kind = payload.get("type")
            if kind in {"task_started", "turn_started"}:
                active = payload.get("turn_id")
            if (active and kind in {"task_complete", "turn_complete"}
                    and payload.get("turn_id") == active):
                return records
    raise ValueError("no matching completed first turn in existing rollout")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    with args.source.open() as source:
        records = derive(source)
    args.output.write_text("".join(json.dumps(r, separators=(",", ":")) + "\n" for r in records))
    print(f"Derived {len(records)} records from Codex 0.160.1, through first matching completion")


if __name__ == "__main__":
    main()
