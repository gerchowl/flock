# Attributed Codex segment fixtures (#1856)

`src/codex_reader/segment/tests.rs` drives the actual binding validator and
reader using local private files, attributed test-process writer identities,
owned shell children and supplied clocks. No model, operator profile, sleep or
retry is used. Admission first prepares the entire candidate, then calls
`Binding::finish`; failure rejects the pending claim while preserving its
launch-generation trust history. A stale callback cannot reject a newer claim.

The launch controller quiesces the previous writer, captures the resume
snapshot, then starts the new writer. Snapshot capture works when the previous
writer is already gone; the snapshot itself cannot prove writer identity. Capture verifies complete envelopes/ordinals and keeps
both the read offset and inherited partial suffix. Destination admission checks
the exact prefix hash and thread, and requires the same inode unless a known
local copy is explicitly selected. An incomplete inherited record is skipped
through its eventual newline; neither it nor an old completion can bootstrap a
new turn. Only a complete new start after the snapshot supplies current evidence.
The launch controller, not hook text, selects the mode and snapshot transaction.

Fork scenarios include copied parent start/completion/settings and referenced
history with a declared ordinal base. Exactly one settings delimiter owned by
the bound child must precede its start. Missing or ambiguous delimiters reject
initially with diagnosed binding failure; after current-launch trust they remain
unknown. Imported parent boundaries never establish the child's turn.

Compaction consumes an owned handoff captured while the old capability is still
accepted. Transfer proves the same local node/pane/launch, writer PID/start,
thread, file identity, prefix and read cursor. The existing reducer carries any
partial record across the handoff. Replacement, prefix changes, another local
writer or a destination route cannot inherit it; the named diagnostic is
`codex_compaction_continuity_unproven`. No cloneable reducer or callback can
manufacture a handoff. Supplied time does not expire quiet evidence.

The API test `codex_session_report_retains_fork_path_without_changing_resume_id`
starts with the real JSON report command and ends at terminal state. It checks
startup-to-fork identity replacement, retained source/path, unchanged ID-based
resume semantics and stale-sequence refusal. The transcript path remains
unvalidated data, separate from the session ID; it does not select a file or
supply authority. Claude's source set does not acquire fork.

Worker supervision and writer health remain #1857; runtime authority/consumer
integration remains #1664. The O(file)-per-poll cost and file-size cliff recorded
in #1860 remain explicitly assigned to #1857. This slice does not enable the
reader on the server or treat typed in-TUI commands as retirement confirmation.
