# Attributed Codex segment fixtures (#1856)

`src/codex_reader/segment/tests.rs` drives the actual binding validator and
reader using local private files, attributed test-process writer identities,
owned shell children and supplied clocks. No model, operator profile, sleep or
retry is used. Admission verifies the candidate incrementally and calls
`Binding::finish` only after successful EOF; failure rejects the pending claim while preserving its
launch-generation trust history. A stale callback cannot reject a newer claim.

The launch controller quiesces the previous writer, captures the resume
snapshot, then starts the new writer. Snapshot capture works when the previous
writer is already gone; the snapshot itself cannot prove writer identity. Capture verifies complete envelopes/ordinals and keeps
both the read offset and inherited partial suffix. Destination admission checks
the exact prefix hash and thread, and requires matching device/inode identity unless a known
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

Worker supervision and writer health are provided by
[the #1857 worker](../codex-worker/README.md); runtime authority/consumer
integration remains #1664. #1860 replaces snapshot and admission full-file buffers
with bounded steps through the shared streaming verifier. `Capture` requires the
old writer to remain quiescent until it returns a snapshot. `Admission` keeps
binding trust pending across steps and rechecks its capability before each step;
late malformed records or a second child delimiter cannot commit early trust.
Resume/compaction seed the privately captured checkpoint, verify its full prefix
in bounded units, then process only appended records. Resume skips completion
of an inherited partial line; compaction retains the owned reducer's partial
state. Handoff captures the last verified cursor without a synchronous reread;
admission proves that cursor's continuity before delivering the transferred state.
Dropping capture/admission cancels local work without a join or notification wait;
the controller must reject or replace a cancelled pending binding claim.

Tests capture, resume, explicitly copy, fork and compact long histories with
11-byte work units. They require bounded progress to reach EOF, verify pending
trust until completion, and reject stale claims and late ambiguous delimiters.
Initial metadata validation is still bounded by its own metadata limit, and
record/turn-ID limits remain explicit failures. This slice does not enable the
reader on the server or treat typed in-TUI commands as retirement confirmation.
