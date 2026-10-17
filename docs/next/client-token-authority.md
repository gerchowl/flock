# Client-token authority (S7-min slice 1)

ADR-0036 section 5/5a requires a bearer token regardless of connection address.
This internal slice supplies its authority store; it enables no listener or CLI.
Slice 7b (#1827) owns the operator socket/CLI entry, and slice 2 (#1828) owns
WebSocket authentication and close-frame ordering. Public structured start stays
gated by #1734.

`TokenStore::open` uses an existing trusted data root and creates its private
`client-tokens` namespace. Open it at startup and share its handle; hello
verification operates on that already-open store. Each lowercase client name (1–64 ASCII letters, digits,
dash or underscore) has one `<name>.token` JSON record: a 64-character lowercase
hex bearer generated from 32 random bytes, a fresh opaque 128-bit principal,
and explicit read/send/answer/operate scopes. Records are capped at 4096 bytes.
Reissuing a revoked name creates a different principal. Operate does not imply
other scopes; S6 permission decisions require its operate-authorized capability.

Management issue/list/revoke operations require the borrowed LocalOperator
witness produced by the existing kernel UID/PID, pane, lease and relay policy.
The witness cannot be constructed from request JSON or outlive its caller.
Issue refuses any existing entry rather than overwriting it; list returns public
metadata or an explicit unsafe-entry status. Each entry has a lossless hexadecimal
filename identifier, so even a non-UTF-8 or otherwise invalid token filename can be
revoked without parsing or trusting its contents. Bearer diagnostics are redacted and owned bearer buffers are wiped
on drop. Only the issuing operator's explicit result serializes the bearer.

The namespace is pinned by a descriptor. Symlinks, nonregular files, unsafe owner
or modes, invalid names and malformed records authenticate nobody. Scans skip and
log these entries individually (ADR-0028 section 3a); they do not deny other valid
clients or prevent operator inspection. Directory I/O failures and namespace
replacement still fail the operation. Directories are 0700 and files 0600, including under restrictive
umasks. New records are written to private exclusive temporary files, synced,
then linked without replacement and the directory synced. Temporary entries are
removed by their writer. Opening the store also sweeps orphan `.tmp-*` files under
the writer lock, so an active issuance is never mistaken for an orphan. A failed publication/sync can leave a
complete token issued without returning its bearer; inspect/list and explicitly
revoke it before issuing again. There is no automatic mutation retry.

A stable `.lock` entry serializes admission with issue/revoke across separate
store handles and processes. Hello verification and listing never take this lock.
Issue and command admission return Busy on contention. Operator revocation instead
waits for the OS lock with a two-second failure bound; a deadline is an explicit
TimedOut I/O error, never a successful revocation. There is no sleep or mutation
retry. A contended revocation uses a worker blocked on the OS lock; if its caller
times out, the worker discards its late guard when the current holder releases it
and cannot delete a token. The caller alone performs the unlink under the guard. Store users must not delete or replace the lock/namespace. The admission
callback must perform bounded queue/control admission only, never wait for a model
or recursively call token management. It cannot return the borrowed sealed S6
capability. Previously admitted work is not cancelled by revocation.

Hello verification hashes the presented bearer and compares fixed-size SHA-256
digests without a data-dependent byte-comparison exit. Every command admission
reopens the current record, verifies the connection's principal and digest, and
checks current scopes while holding the revocation lock through dispatch.
Deleted/replaced credentials refuse as Revoked; scope shrink refuses as Scope.
An unsafe entry matches nobody and is reported by list; directory failures stay
explicit. Revoke can remove an unsafe entry without opening its contents.
Existing read subscriptions are not continuously reauthorized; listener/dispatch
code must implement ADR-0036's 4401/4403 policy without logging raw frames.

Changes to scopes in a valid record are observed at the next admission. Direct
filesystem changes are observed at verification/admission boundaries, not
proactively on idle sockets, and do not participate in API admission ordering.
This slice exposes only issue/list/revoke management, not a scope-editing API.

Production randomness uses the OS entropy provider. An injected source makes
bearer, principal and temporary-name fixtures deterministic without changing the
operator boundary. Tests do not change the process-wide umask.
