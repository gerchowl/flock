# Claude session fixtures

Redacted, reconstructed JSONL excerpts of the fields published in ADR-0036
Appendix A.2, A.3, A.5, A.6 and A.11–A.15 (Claude Code 2.1.296).
The raw captures were not committed by the probe authors. These are reduced
examples of the documented shapes, not copies of unavailable raw captures.
Identifiers and text are fictional, paths and account metadata are omitted.
Token counts in the base result and compaction are the published A.3 figures.
SSE body layout follows A.2's raw Anthropic event catalogue.

`variants.txt` lists the expected variant for each JSONL line, in order.
Includes all catalogue envelopes, four lifecycle states, nullable status,
four delta types, three block types, API failure with success subtype,
and streaming/tool interruption. A.11 peer holds and A.14–A.15 control
success/error replies supplement the earlier A.2 catalogue.

Replacement with sanitized real S0 captures is tracked in [#1385](https://github.com/gerchowl/flock/issues/1385).

The `control_cancel_request` fixture is reconstructed from the CLI/SDK
cancellation protocol, not an S0 capture: the committed S0 catalogue does not
contain that shape and its raw captures were unavailable for this fix.
Claude Code 2.1.296 contains the cancellation wire tag and structured-I/O
cancellation handling. The official SDK receives this message from the CLI and
cancels the incoming request identified by `request_id`:
https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/_internal/query.py
