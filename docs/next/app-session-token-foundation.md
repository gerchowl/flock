# App-session token foundation (#2442 PR1a)

ADR-0038 §6 and the ADR-0036 "App-session tokens and shell origins (#1601)"
amendment define the desktop shell's ephemeral credentials. This slice adds
crate-private foundations only. There is no socket mint verb, alternate
`TokenStore::authenticate` route, shell-origin admission or desktop change.
Those integration paths remain #2442 PR1b and PR2.

Each server owner constructs a fresh memory store and shares its handle. It
must not recreate a store for each lookup or carry it through a restart or
handoff. Successor owners reject predecessor credentials. Quiescing owners
call `revoke_all` before relinquishing ownership; dropping the final owner also
closes the retained revocation receivers. Nothing writes app-session bearers,
digests, process watches or records to disk or handoff snapshots. Existing
file-token admission locks and temporary revocation-intent files still apply.

An internal issuance requires the sealed LocalOperator proof and credentials
captured from the minting socket. It first registers a close-on-exec kernel
watch, using kqueue `EVFILT_PROC`/`NOTE_EXIT` or Linux `pidfd_open`, then compares
the accepted birth identity with a fresh observation. Unknown evidence, watch
failure, mismatch or an already exited process refuses with the registered
`peer_process_changed` code (`retry: never`, CLI exit 2). A fresh kernel-attested
connection is required; callers must not retry stale connection evidence.
This primitive does not itself grant LocalOperator authority or implement
Linux shell hardening policy, which belongs to the public mint integration.

App tokens grant exactly ClientOpen's `read`, `send`, `answer`, `operate` set.
They expire exactly 12 monotonic hours after issuance. Reading or admitting
never renews them; renewal issues an independent bearer and principal. The
store retains at most 128 entries. Capacity pressure is the existing `Busy`
refusal; issuance first reclaims known expired/exited entries.

Admission rechecks the token under #2455's shared filesystem lock and retains
that lock through the bounded callback. Creation is exclusive. Revocation,
expiry sweeping and owner quiescence publish the existing revocation intent
before waiting for exclusive ownership, then remove records and signal their
observers after all earlier admissions drain. New app and file admissions
cannot overtake the intent. Callbacks cannot recursively manage tokens or wait
for models. The existing two-second revocation-acquisition ceiling is unchanged.

Expiry and process exit refuse admission immediately, including for retained
session handles. Owners must schedule `sweep` from the exposed deadline and
kernel descriptor readiness, off the app executor, to publish the fenced
revocation notification. This foundation starts no polling loop or background
worker. Watch or lock errors fail closed and propagate; it never silently
turns an unknown observation into a live process. PR1b owns readiness scheduling
and the existing `auth_revoked` acknowledgement / 4403 live-session closure.
