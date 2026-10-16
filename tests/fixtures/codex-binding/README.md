# Codex binding fixtures (#1662)

`src/codex_binding/tests.rs` constructs local scripted metadata files and
launch-home directories. It uses the test process's own PID/start identity as
an owned writer fixture, with supplied S1 version facts. It never launches a
model, reads an operator profile, chooses a file by name, or treats a fixture
label as a route. Node identifiers use reserved `.example` names.

The real neutralized 0.160.1 fixture from
[`../codex-rollout/README.md`](../codex-rollout/README.md) additionally verifies
first-metadata binding and interoperability with the S2 reducer: the same
opened handle retains the complete prefix and ordinal zero. This is a fixed
fixture read, not evidence of an incremental reader or runtime authority.

The tests cover pending validation, initial diagnosed fallback, trust retained
across failed rebinds, explicit launch retirement, reused thread/path values,
revoked and mismatched callbacks, missing/escaped/non-regular paths, launch-home
overrides, first metadata identity, partial/malformed/oversized metadata, writer
absence/PID reuse/ambiguity, and S1 version eligibility combined with actual
metadata. The CLI hook tests drive the real planner from SessionStart payloads
for startup/resume/clear/compact/fork, and reject rebind fields on other events.

Known duplicate thread claims poison both capabilities, including when the
owners have different node identities. Retiring a claim or verifying its writer exit does not
revive the other old capability: fresh validation is required. Retained rejected
claims prevent a later callback from winning over a still-known live owner.
Pending/rejected rebinds and file loss retain prior known live ownership. Only
verified writer exit, a validated replacement or known retirement releases it.
The runtime must revoke a pane's claim before dropping it. These are supplied
identity models, not an implementation of cross-node ownership transport.

## Boundaries and failure modes

This is the ADR-0028 §1a **binding foundation**. A validated binding establishes
path/metadata/local-writer attribution only, not an idle/working decision. The
launch-resolved absolute home and writer candidates must come from trusted local
runtime attribution, never hook JSON, aliases, ambient cwd or another profile.
The captured node/pane and launch generation must match the callback key before
any path is opened. Late SessionStart claims from a retired launch are rejected
before changing the current binding. S1 supplies the cached launch writer probe. Actual first metadata supplies its
own version, so a deferred hook does not require a second writer probe.

File validation runs on a worker with a caller-supplied metadata-byte budget.
Oversize, incomplete or malformed first metadata fails validation; it never
scans ahead for a better match. Directory-descriptor traversal rejects symlink
redirection after path resolution. The returned file is the same opened handle,
reset to byte zero for subsequent prefix/ordinal verification. Initial failure
returns a diagnosed screen-fallback marker, without manufacturing idle. Once a
launch has trusted a binding, failed claims or unexplained loss remain unknown.
Only known launch/resume retirement resets that trust. Foreign routes do not
start a claim on this pane.

The incremental reader, file identity/prefix/cursor continuity, live writer
monitoring, supervised reader health, resume/fork attribution and compaction
transfer are #1663. Runtime hook-to-reader wiring, authority selection, screen
classification/submission eligibility and consumer diagnostics are #1664.
The move transaction and its cross-node ownership facts are #1500. Immediate
in-TUI replacement confirmation has no current producer (#1479); typed commands,
Working chrome, labels, repeated EOF or time alone must not retire a binding.
No lifecycle authority is enabled by this slice.
