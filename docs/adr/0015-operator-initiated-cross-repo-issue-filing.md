# ADR 0015 — The operator may file an issue from flock, over the API, into any repo their own token can reach

- Status: Proposed
- Date: 2026-08-27
- Issues: #371 (the spike and the implementation). **Amends ADR-0010 decision
  6**, which is otherwise unchanged. Constrained by ADR-0014 (agent-initiated
  spawn — deliberately not in play here) and ADR-0002 (config layers).
- Decision owner: operator; design from five independent fresh-context reviews
  on #371 (TUI/text-editing, GitHub platform, architecture/ADR-consistency,
  operator-workflow, and one assigned to defend the filed proposal), plus
  probes against the live GraphQL schema.

## Context

ADR-0010 decided that **`flk report` never submits**: it opens GitHub's real
form, prefilled, and a human presses the button. That decision reasoned about a
*publicly distributed binary* shipping a *write credential* so *end users*
could file *bug reports against flock*.

#371 is a different case. The operator is working in one repo, has a thought
belonging to another, and today must leave flock or interrupt an agent that
happens to be in the right repo. They are on their own machine, with their own
`gh` token, filing into repos that token already writes to.

The question was whether decision 6 transfers, and if not, how narrowly the
exception can be drawn.

## What decided it

**Two measurements retired the prefilled-URL route for this case.**

`report::url::MAX_URL_LEN` is 7 500, measured against github.com in ADR-0010.
#371's own body is **6 739 bytes raw and 9 791 URL-encoded**. The URL route
would have silently truncated the very issue that motivated the feature — and
the issues worth filing are exactly the long, investigated ones. Separately,
there is **no URL query parameter for an issue type**, so the two-axis
requirement (labels *and* types) is unreachable by construction on that route.
A browser hand-off is also the largest available context switch, which is what
#371 exists to avoid.

**Decision 6's stated premise has gone stale.** It rejected `gh issue create`
because it "bypasses issue-form validation … it can only produce something
template-shaped". That was true of `gh issue create --title --body`. Schema
introspection today returns:

```
CreateIssueInput: … labelIds  issueTemplate  issueTypeId  issueFields …
Repository:       issueTemplates  issueType  issueTypes
```

`issueTemplate` and `issueFields` are first-class inputs and the server runs
the form's own required-field validation on the mutation path. ADR-0010 had
already left the door open, retaining `gh issue create` as "a possible opt-in
for users with write permission, **deferred** out of the first release."

**flock carries no YAML parser, and that is still true.** ADR-0010 declined to
add one. So flock can *detect* that a repo defines a template but cannot fill
one in, which means the template hole is real and must be surfaced rather than
posted through.

**`Repository.issueTemplates` cannot be used for that detection.** Verified: it
returns `[]` for `gerchowl/flock`, which defines `bug.yml`, and three entries
for `cli/cli`, which uses `.md`. It reports only legacy markdown templates.
Detection through it would report "no template" for every modern issue-form
repo — precisely the silent bypass ADR-0010 warned about.

**No overlay text editor.** flock's only text primitive is the single-line
`LineEditor`. Beyond the wrap/viewport/cursor work an issue body would need,
`handle_paste` early-returns unless the mode is `Terminal` (an overlay cannot
accept a pasted URL) and #327 means overlay text cannot be selected or copied
out. A pane is a real PTY that already has all three.

## Decision

1. **The operator may file over the API; the binary still ships no
   credential.** The token is resolved from the operator's environment or their
   local `gh` login, exactly as `pr_poll` already does. Shipping a
   write-capable token in a public binary remains refused — that half of
   ADR-0010 decision 6 is untouched.

2. **Nothing is filed without an explicit `--file-it`,** and on an interactive
   terminal the composed issue is previewed and confirmed before the mutation.
   ADR-0010's preview-before-send invariant is preserved on the write path.

3. **A repo that defines a template is not posted through.** Detection reads
   the repository tree at `.github/ISSUE_TEMPLATE`, excluding `config.yml`.
   When a template exists the operator gets an advisory naming it and is
   directed to the browser form, which does run the form. Filling `issueFields`
   is a follow-up, gated on a YAML parser being justified on its own merits.

4. **Authorship stays the human's.** The issue is authored by the operator's
   own token. No bot identity is introduced.

5. **The mutation is excluded from the MCP surface.** flock's MCP table
   deliberately omits every mutating verb, and ADR-0014's finding — that a
   narrowed wrapper is the only safe shape for a capability granted to an
   agent — applies unchanged. Whether an agent should ever file is left open
   and is the operator's call, not a gap to be closed by exposing this.

6. **The body is composed in a real PTY, never in an overlay.** The dialog
   collects only single-line fields and hands off to `$EDITOR` in a NEW pane —
   never the focused one, which is #371's hard requirement.

7. **One transport.** `src/github/graphql.rs` owns the `curl` subprocess, the
   token cache and the error classification; `pr_poll` calls it rather than
   keeping a second copy. `PrPollErrorKind` becomes an alias of the shared
   enum, so the `PeersSummary` wire contract is unchanged.

## Alternatives considered

**Keep the prefilled URL as the default and make the mutation an opt-in flag.**
Rejected on the measurement: the default would truncate real issues at 7 500
characters and could never set an issue type. The URL route is retained where
it is genuinely better — a repo with a template — rather than as the default.

**Reuse `report::compose::ReportInputs`.** Rejected. That type is
bug-report-shaped: it carries `ReportProvenance` and a redacted log tail, and
its field table is gated against *flock's own* `bug.yml`. Threading an
arbitrary idea for a third-party repo through it would drag diagnostics into a
feature unrelated to them. The genuinely shared part — destination validation
in `report::url` — is reused directly.

**A multi-line editor in the overlay.** Rejected on cost and on outcome: it
lands text-editing debt ahead of #120's modal-stack umbrella, and with paste
routing and #327 unfixed it would be a worse editor than the one the operator
already has.

**A scratch agent that investigates and files (#371's P1).** Deferred. A fresh
agent given a one-line prompt about a repo it has **no checkout of** will cite
`file:line` that do not exist, and the operator — still in the other repo —
cannot fact-check it. An unreviewed agent-authored issue is plausibly wrong
where a terse human note is honestly incomplete.

## Consequences

- flock gains its **first write path to GitHub**. It is operator-initiated,
  confirmed, off the MCP surface, and uses no credential the operator did not
  already have.
- The token now needs write scope for the destination. `GraphQlErrorKind`
  gains a `Forbidden` variant so "your token cannot write here" is not
  reported as "log in again" — different remedies must not collapse.
- Issue **types** are supported where configured. `Repository.issueTypes` is
  `null` (not `[]`) when an org configures none, which is the common case; the
  axis hides rather than rendering an empty required picker.
- Templates remain unanswerable until a YAML parser is justified. Until then
  every templated repo routes to the browser, which is correct but is friction
  on repos that define forms.
- The repository directory is cached with a 15-minute TTL. A repo created since
  the last fetch is still reachable by typing `owner/name`, which is what makes
  a long TTL safe.
