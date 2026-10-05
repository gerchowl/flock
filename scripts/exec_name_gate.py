#!/usr/bin/env python3
"""Gate: `flock` is the product, `flk` is the command. Never both in one sentence.

The executable is `flk` (`Cargo.toml`'s `[[bin]] name`, `nix/package.nix`'s
`mainProgram`). The product, the repo, `flock.dev`, `~/.config/flock`, `FLOCK_*`
and the log filenames are all still `flock` (ADR-0003). Both halves of that
sentence are load-bearing, and the failure is silent in the worst way: on Linux a
`flock` on `PATH` **is** util-linux `flock(1)`, so the wrong name does not error
loudly — it locks a file instead of launching a multiplexer.

It shipped that way once. #86 renamed the binary and swept the source; the three
peer-command literals it named are correct (`src/config/model.rs:887`,
`src/peers.rs:1012`, `src/peers.rs:1086`). It did not sweep the surfaces around
the source, and nothing since has looked at them. `website/install.sh` installed
`BIN="flock"`, so the `curl | sh` path would have shadowed util-linux `flock(1)`
on every Linux box with `~/.local/bin` ahead of `/usr/bin`. The released docs
carried 189 invocations of a command that does not exist, and `SKILL.md` — the
file a harness loads into an agent at session start — carried 44. Both were
already correct in `docs/next`, which is where the rename was actually applied:
the sweep went to the staging tree and was never promoted into the released one.

Why a gate rather than a cleanup, given it is now fixed: `release-docs-check`
compares the released docs against `docs/next`, but it is a **release-time** gate
and is not in `just check`, so the two trees are free to drift between releases.
And no existing gate could have caught any of it — `derived-docs` checks that
declared regions match a generator, and `ssh-hosts` judges bare hostnames.
Neither looks at *command position*, which is the only place this bug lives.

**Derived, never maintained.** The set of invocable tokens is read out of the
dispatchers (`src/cli.rs`'s top-level match, `src/main.rs`'s early dispatch,
`src/cli/help.rs`'s VERBS table), so adding a CLI group extends this gate with
no vocabulary to keep in sync — the same reason `scripts/ssh_hosts_gate.py`
derives its candidates from this machine's own ssh state rather than from a
checked-in list.

**Context-scoped, because `flock` is a common noun.** Measured, not assumed:
`flock pane`, `flock server`, `flock session` and `flock-managed` are ordinary
product prose — "the focused flock pane", "each flock session runs a server" —
and a whole-word `flock`+token rule flags every one of them. A gate that cries
wolf on ordinary sentences gets `--no-verify`'d within a day, and that habit then
covers the real leak too. So the token rule only fires where a command can
actually be typed:

* **inside a fenced code block** — `flock` followed by an invocable token or a
  `--flag`, or standing alone as the whole line. Code blocks are commands. This
  is also what keeps `brew install flock` and `mise use -g flock` clean: there
  `flock` is an *argument*, at end of line, never followed by a token.
* **inside an inline code span that begins with `flock`** — `` `flock update` ``,
  `` `flock --remote` ``, `` `flock` ``. A span that merely *contains* the word
  mid-sentence is prose and is left alone.

That last rule is a **register** judgment, and worth being explicit about because
it is the heuristic's real edge. A bare backticked long name is presumed to be the
command, because this tree's user- and agent-facing prose is written as
instruction and all 20 of its bare uses are commands. The same construct in a
Rust doc comment is description — src/logging.rs's "whose namespace stays
`flock` as part of the logging identity" is correct as written — which is the
whole reason Rust is out of scope below. Where the presumption is wrong, the
escape hatch is the release valve; the one time it was needed is the skill's
`name:` in SKILL.md's frontmatter, which really is spelled `flock`.

Exempt by construction: `flock(1)`, `flock-ai`, `flock.dev`, `~/.config/flock`,
`FLOCK_*`, `flock.log`, the `flock_` MCP tool-name prefix, and the product noun
in "flock — terminal workspace manager". Escape hatch, matching the other gates:
``guardrails-ok(exec-name): <reason>``.

**Rust source is deliberately out of scope**, and that is a measured decision
rather than an oversight. The bug this gate exists for lived in prose addressed to
a human or an agent; #86 already swept the source, and the part of the source an
agent can observe — the help and usage strings — is asserted by tests. In a Rust
doc comment the long name in backticks is nearly always the product noun rather
than a command — src/ui/sidebar.rs's "local `flock` (1 checkout)" and
src/logging.rs's "whose namespace stays `flock` as part of the logging identity"
are both correct as written — so the token rule there fires on correct code and
would teach the next person to reach for the escape hatch. One genuine instance
did turn up while measuring: a doc comment in src/pane.rs naming the live-handoff
verb. It is fixed by hand rather than gated.

Out of scope for the same reason: a path to a built artifact
(``target/release/flock``) is a fourth shape; it occurred once in the docs, is
fixed, and is not gated. Both gaps are stated rather than hidden.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ESCAPE = "guardrails-ok"

ROOT = Path(__file__).resolve().parent.parent

# Where a `flock` token can mean "the command". CHANGELOG.md is excluded on
# purpose: rewriting published release notes is a product decision, not a sweep
# (docs/next/CHANGELOG.md is itself only half-migrated).
DEFAULT_GLOBS = (
    "README.md",
    "SKILL.md",
    "website/src/content/docs/**/*.mdx",
    "website/install.sh",
    "docs/next/README.md",
    "docs/next/website/src/content/docs/**/*.mdx",
)

FENCE = re.compile(r"^\s*(?:```|~~~)")
INLINE_CODE = re.compile(r"`+([^`\n]+?)`+")


def invocable_tokens() -> set[str]:
    """Every token a user can type after `flk`, read from the dispatchers.

    Fails closed: if the dispatchers cannot be read this raises rather than
    returning an empty set, because an empty set would make every rule below
    silently vacuous — a gate that cannot see anything must not report clean.
    """
    cli = ROOT / "src" / "cli.rs"
    main = ROOT / "src" / "main.rs"
    if not (cli.is_file() and main.is_file()):
        raise SystemExit(
            "exec-name-gate: cannot read src/cli.rs or src/main.rs, so the "
            "invocable-token set is unknown. Refusing to report clean."
        )
    cli_src = cli.read_text(encoding="utf8")
    main_src = main.read_text(encoding="utf8")

    # `src/cli.rs`: the one top-level `match command { ... }` block. Bounding by
    # the block rather than by indentation keeps the nested per-verb matches
    # (`"list"`, `"output"`, `"recent"`, …) out of the set.
    try:
        start = cli_src.index("let exit_code = match command {")
        end = cli_src.index("\n    Ok(CommandOutcome::Handled(exit_code))", start)
    except ValueError as exc:  # pragma: no cover - a refactor moved the match
        raise SystemExit(f"exec-name-gate: cannot locate the cli.rs dispatch: {exc}")
    tokens = set(re.findall(r'"([a-z][a-z-]*)"\s*=>', cli_src[start:end]))

    # `src/main.rs`: `server`, `client` and `update` are dispatched before the TUI
    # starts, so they never reach the block above.
    tokens |= set(re.findall(r'== Some\("([a-z][a-z-]*)"\)', main_src))

    # Every `<group> <verb>` row in `src/cli/help.rs`'s VERBS table is a real verb,
    # and several (`output`, `agent-status`, …) are what a user types after a group.
    help_rs = ROOT / "src" / "cli" / "help.rs"
    if help_rs.is_file():
        help_src = help_rs.read_text(encoding="utf8")
        try:
            verbs_block = help_src[
                help_src.index("const VERBS") : help_src.index("const LITERAL_TEXT")
            ]
        except ValueError:  # pragma: no cover
            verbs_block = ""
        tokens |= set(re.findall(r'\(\s*"[a-z-]+",\s*"([a-z0-9-]+)"', verbs_block))

    if not tokens:  # pragma: no cover - guarded by the raises above
        raise SystemExit("exec-name-gate: derived an empty token set; refusing to pass.")
    return tokens


def build_rules(tokens: set[str]) -> tuple[re.Pattern[str], re.Pattern[str]]:
    """(code-block rule, inline-code rule) for a given token set."""
    alternation = "|".join(sorted((re.escape(t) for t in tokens), key=len, reverse=True))
    # `flock` followed by an invocable token or a `--flag`.
    #
    # The lookbehind is load-bearing and was added after it fired: without a left
    # boundary, the long name inside a PATH or a repo slug matched whenever a
    # flag followed it, so `--repo gerchowl/flock --title ...` was rewritten to
    # `gerchowl/flk` — a repository that does not exist. `/ . _ -` are exactly the
    # characters that make the preceding token a path, a slug or a flag, which is
    # also why this deliberately does not catch `target/release/flock`: that is a
    # build-artifact path, out of scope below, and fixed by hand.
    invoked = re.compile(r"(?<![\w/._-])flock(?=[ \t]+(?:%s|--[a-z0-9-]+))" % alternation)
    # `flock` standing alone, as the whole line (optionally `$ `-prefixed).
    alone = re.compile(r"^[ \t]*\$?[ \t]*flock[ \t]*$")
    # An inline span that BEGINS with `flock` — `flock update`, `flock --remote`,
    # or bare `flock`. `flock(1)` / `flock-ai` / `flock.dev` are excluded by the
    # trailing class, `flock_agent_list` by the space requirement, and a slug or
    # path by the `^` anchor.
    inline = re.compile(r"^flock(?=[ \t]+(?:%s|--[a-z0-9-]+)|$)" % alternation)
    return re.compile("(?:%s)|(?:%s)" % (invoked.pattern, alone.pattern)), inline


def check_text(text: str, rules) -> list[tuple[int, str]]:
    code_rule, inline_rule = rules
    findings: list[tuple[int, str]] = []
    in_fence = False
    for number, line in enumerate(text.splitlines(), 1):
        if f"{ESCAPE}(exec-name)" in line:
            continue
        if FENCE.match(line):
            in_fence = not in_fence
            continue
        if in_fence:
            if code_rule.search(line):
                findings.append((number, line.strip()[:100]))
            continue
        for span in INLINE_CODE.findall(line):
            if inline_rule.search(span.strip()):
                findings.append((number, f"`{span.strip()}`"))
                break
    return findings


def check_file(path: Path, rules) -> list[str]:
    out = []
    for number, evidence in check_text(
        path.read_text(encoding="utf8", errors="ignore"), rules
    ):
        out.append(
            f"{path.relative_to(ROOT)}:{number}: `flock` used as the command — "
            f"ADR-0003:46-52 · {evidence}"
        )
    return out


def collect_files(globs: tuple[str, ...]) -> list[Path]:
    files: set[Path] = set()
    for pattern in globs:
        if "*" in pattern:
            files.update(p for p in ROOT.glob(pattern) if p.is_file())
        else:
            candidate = ROOT / pattern
            if candidate.is_file():
                files.add(candidate)
    return sorted(files)


def installer_matches_bin_name() -> list[str]:
    """`install.sh` must install the name Cargo.toml actually builds.

    This is the assertion that would have caught the P0 directly: two literals in
    two files, with no shared source, is exactly the shape that rots.
    """
    cargo = ROOT / "Cargo.toml"
    install = ROOT / "website" / "install.sh"
    if not (cargo.is_file() and install.is_file()):
        return []
    match = re.search(
        r"\[\[bin\]\]\s*\nname\s*=\s*\"([^\"]+)\"",
        cargo.read_text(encoding="utf8"),
    )
    if not match:  # pragma: no cover
        return []
    built = match.group(1)
    bin_match = re.search(
        r'^BIN="([^"]+)"', install.read_text(encoding="utf8"), re.M
    )
    if not bin_match:  # pragma: no cover
        return ["website/install.sh: no BIN=\"...\" literal to check"]
    installed = bin_match.group(1)
    if installed != built:
        return [
            f"website/install.sh: BIN={installed!r} but Cargo.toml's [[bin]] name is "
            f"{built!r} — the installer is not installing the built binary"
        ]
    return []


def main(argv: list[str]) -> int:
    globs = DEFAULT_GLOBS
    args = argv[1:]
    if args and args[0] == "--globs":  # pragma: no cover - convenience for tests
        globs = tuple(args[1].split(",")) if len(args) > 1 else DEFAULT_GLOBS

    rules = build_rules(invocable_tokens())
    findings: list[str] = []
    for path in collect_files(globs):
        findings.extend(check_file(path, rules))
    findings.extend(installer_matches_bin_name())

    if findings:
        print(
            f"exec-name-gate: {len(findings)} use(s) of `flock` where the command is "
            f"`flk` (see ADR-0003:46-52)",
            file=sys.stderr,
        )
        for finding in findings:
            print(f"  {finding}", file=sys.stderr)
        print(
            f"\n  Fix: write `flk`. Leave the product noun alone — flock.dev,\n"
            f"  ~/.config/flock, FLOCK_*, flock.log, flock(1), flock-ai and\n"
            f"  'flock - terminal workspace manager' are all correct. Escape hatch:\n"
            f"  {ESCAPE}(exec-name): <reason> on the line.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))