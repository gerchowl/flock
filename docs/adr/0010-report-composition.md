# ADR 0010 — Bug reports compose locally and are submitted by a human, never by the binary

- Status: Proposed
- Date: 2026-08-06
- Issues: #233 (the spike). Constrained by ADR-0002 (config layers — the report
  destination is deliberately *not* one of them), ADR-0003 (the executable is
  `flk`), and ADR-0005 (the durable event log, which a later phase will use for
  report correlation).
- Decision owner: operator; design from five independent expert reviews on
  #233 (security/privacy, build/distribution, GitHub platform, developer
  experience, architecture), plus empirical probes against github.com.

## Context

Issue #232 is the motivating artifact: a bug report containing a `flk status`
block, the resolved binary path, OS/kernel/arch, the systemd unit description,
four reproduction invocations with their exact JSON error envelopes, a JSONL
excerpt from `flock-server.log`, and a `/proc/<pid>/environ` PATH dump — with
usernames hand-redacted to `<user>`.

Every fact in it except the last two was already known to the binary at the
moment of failure. It took a maintainer to know they were worth including.

The question was whether `flk` could assemble that itself, and how far it
should go toward actually filing.

## What decided it

**Three mechanisms died on evidence, not preference.**

*Build-time git-origin detection.* `Cargo.toml` already declares `repository`,
readable as `CARGO_PKG_REPOSITORY` on every build path with no build script.
A `git remote get-url origin` probe in `build.rs` is strictly worse: the
`lib.fileset` in `nix/package.nix` excludes `.git`, the crates.io `include`
list omits it, and `cargo install --git <fork-url>` would bake whichever fork
the user typed — silently filing reports to the wrong tracker.

*Build-time template parsing.* The same two manifests exclude `.github`
entirely, so parsing the issue forms at build time yields an empty table on the
two paths most users install through, while adding a YAML parser to
`[build-dependencies]` that the lock file does not otherwise carry.

*An environment-variable destination override.* Not a convenience — an exfil
channel. A `.envrc`, direnv config, or shell profile exporting a destination
silently reroutes every subsequent report, and its log tail, to a repository
the reporter never chose. Print-and-confirm is not mitigation: someone who
typed the command hits enter through anything that looks like chrome.

**The URL is itself a disclosure.** GitHub records full request URLs
server-side, so anything in the query string is exposed the moment the browser
opens it — before the reporter decides whether to submit. That retired
"diagnostics in the URL" and, with it, the entire 8KB truncation-fallback
problem. Security and developer-experience reviews reached this shape
independently from opposite directions.

**"Structured events only" was false safety.** The identifying material lives
in field *values*: `remote_bridge_started` emits `target`/`ssh_opts`/
`ssh_config_file`/`proxy_jump`/`remote_command`; `process_exec_completed`
emits `program` plus 512 characters of real command line; several emitters log
`path = %path.display()` verbatim. A denylist over that surface fails open the
moment someone adds an emitter.

**Empirical findings that shaped the surface.** GitHub supports per-field
query-param prefill for *issue* forms only; discussion forms accept just
`category`/`title`/`body`. Checkboxes cannot be prefilled at all. The URL cap
is ~8KB (measured: 4 079 chars → 302, 7 079 → 500, 8 079 → reset, 8 279+ →
414). `gh issue create` posts a raw body and bypasses issue-form validation
entirely, so no API path can satisfy a required-field template — it can only
imitate one.

## Decision

1. **Composition is client-side and pure.** `src/report/` holds a
   `compose(inputs) -> Composed` with no socket call, file read, subprocess, or
   clock. The CLI is a thin adapter; a future MCP or TUI entry point is another
   one. There is no `report.compose` socket verb: the log files are readable
   from `session::data_dir()` with the server dead, which is exactly when
   someone files a bug.

2. **The destination is compiled in from `CARGO_PKG_REPOSITORY`.** An override
   must be typed as `--repo owner/name`. The environment is not a source.

3. **The reporter writes four fields; the binary fills the rest.** Version,
   channel, build commit, OS, arch, terminal, shell, client protocol, and the
   running server's version/protocol/compatibility are collected, never asked.

4. **The provenance block has its own versioned schema.**
   `ReportProvenance { report_schema_version, .. }` — not `src/cli/status.rs`'s
   private `FullStatusJson`, which stays free to change. Follows the posture of
   `PROTOCOL_VERSION` and of `LogLine`'s `#[serde(default)]` additive fields.

5. **Diagnostics are allowlisted, scrubbed, and never enter the URL.** Named
   fields leave; everything else is dropped unread, so a new emitter's fields
   are excluded by default. The tail goes to the clipboard for a deliberate
   paste. `message` and `err` are kept-and-scrubbed rather than dropped —
   without `err` a record says only "something failed" — and the composed block
   is always previewed before anything can be sent.

6. **The binary never submits.** `--open` launches GitHub's real form,
   prefilled, and the human submits it. This is the only path that runs the
   template's required-field validation, keeps authorship correct, and needs no
   credential. Shipping a write-capable token in a public binary is refused
   outright.

7. **The reproducible-bug checkbox is left untouched.** GitHub cannot prefill
   it, and it is an attestation — a tool that ticks it forges a human's
   statement. This is a property to preserve, not a limitation to route around.

## Alternatives considered

**`gh issue create` as the default path.** Rejected: it bypasses issue-form
validation, so it cannot satisfy the template CONTRIBUTING requires — it can
only produce something template-shaped. Retained as a possible opt-in for
users with write permission, deferred out of the first release.

**A bundled GitHub App or OAuth token.** Rejected: a write credential in a
public binary is scraped and revoked, makes every issue authored by a bot, and
shares one rate limit across all users.

**A "URL that files on click".** Does not exist — issue creation is an
authenticated POST with CSRF. The prefilled form URL is the closest real thing.

**One abstraction over issues and discussions.** Rejected on evidence:
discussion forms have no per-field prefill and no required-field enforcement,
so they need a separate builder. Additionally this fork has Discussions
disabled (`hasDiscussionsEnabled: false`) while CONTRIBUTING.md and
`config.yml` both route feature requests there — a contradiction tracked
separately. Only the `bug` kind ships.

**A JSON round-trip for human input.** Rejected: it is more friction than the
browser form and strips validation until submit. The skeleton is markdown with
`## <field-id>` headings — the shape a human already writes in. JSON belongs on
a future machine-facing surface.

## Consequences

- Reports carry a build commit for the first time. `FLOCK_BUILD_COMMIT` was
  declared in `build.rs` and set by `preview.yml` but read by nothing; it is
  now consumed. **It is not yet set by `release.yml` or `nix/package.nix`**, so
  stable and Nix builds render no commit until that lands.
- The template field table is checked into `src/report/template.rs` with a
  test that re-reads `.github/ISSUE_TEMPLATE/bug.yml` and fails on drift. The
  test skips when `.github` is absent, which is the sandboxed-build case.
- Redaction is a new, permanent maintenance obligation: every new logging field
  is excluded by default, which is safe but means diagnostics silently lose
  value unless the allowlist is revisited.
- `flk report` is undiscoverable until failure paths point at it. Nothing in
  this ADR makes that happen; it is the first follow-up.

## Verification note

Redaction was checked against this machine's real 10 398-line log corpus:
5 002 extracted records contained zero occurrences of the home path, username,
worktree paths, project names, tokens, ssh targets, or IP addresses, against
1 265 raw occurrences of the home path and username in the source files.
