# flock task runner

# Pin a short, deterministic hostname for every test run. Several sidebar layout
# tests derive column offsets from `short_host_name()`; on CI runners with long
# generated hostnames (`fv-az…`) the name field truncates and the math breaks.
# The binary and the integration-test helpers both honor FLOCK_HOST_NAME.
export FLOCK_HOST_NAME := "host"

# Force git's default branch for every `git init` in the suite. The peer
# federation test waits for an indented `:main` workspace row; CI runners
# default new repos to `master` (dev machines default to `main`), so the row
# never matched and the test timed out — deterministic, not flaky. These
# GIT_CONFIG_* env keys make all git invocations (and the spawned binary's)
# default to `main` regardless of the host's git config.
export GIT_CONFIG_COUNT := "1"
export GIT_CONFIG_KEY_0 := "init.defaultBranch"
export GIT_CONFIG_VALUE_0 := "main"

# Run tests
test:
    cargo nextest run --locked --status-level fail --final-status-level fail --failure-output final --success-output never
    just script-tests

# Maintenance script tests plus the platform-coverage ratchet (#597).
# One discovery-based entry point shared by `test`, `check` and CI, so a new
# scripts/test_*.py can never be missing from a hand-written module list.
script-tests:
    python3 -m unittest discover -s scripts -p 'test_*.py' -t .
    @python3 scripts/platform_test_gap.py

# Run one nextest filter, e.g. `just test-one codex_stale_working`
test-one filter:
    cargo nextest run --locked "{{filter}}" --status-level fail --final-status-level fail --failure-output final --success-output never

# Run every guardrails gate over the tree (same set `nix flake check` runs).
# Standalone, NOT chained into `lint`: the host-bound CI runners (ci.yml) skip
# the guardrails devShell, so prek isn't on their PATH — gate enforcement in CI
# lives in nix.yml's `flake check` (checks.gates), locally in the git hooks.
gates:
    prek run --all-files

# Capture a fresh session log and gate it against log-budgets.toml (#318).
# The sample is MEASURED, not committed: the gate compares a real event
# distribution, which is the only way to catch level-vs-frequency defects that
# a source scan cannot see (the info! sits in a facade, the loop is elsewhere).
log-budget seconds='60':
    scripts/capture_log_sample.sh {{seconds}}
    guardrails-log-budget

# Run fast local lint checks
lint:
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings

# Run PR CI checks
ci filter='all()': lint
    cargo nextest run --locked -E "{{filter}}" --status-level fail --final-status-level slow --failure-output final --success-output never

# Check formatting + run unit tests + maintenance script tests
check: ci script-tests
    @echo "docs reminder: if this changes user-facing behavior, make sure the relevant release docs are updated or called out before release."

# Install the guardrails git hooks (gates + conventional-commit check) via prek.
# Normally automatic on `direnv`/`nix develop` entry; this is the manual path.
install-hooks:
    git config --local --unset-all core.hooksPath 2>/dev/null || true
    prek install --install-hooks -t pre-commit -t commit-msg -t pre-push
    @echo "installed guardrails hooks via prek (pre-commit + commit-msg + pre-push)"

# Build release binary
build:
    cargo build --release --locked

# Build the website and documentation
website-build:
    cd website && bun install --frozen-lockfile && bun run build

# Build the vendored libghostty-vt source dist
build-libghostty-vt:
    scripts/build_vendored_libghostty_vt.sh

# Check that release docs and changelog have been finalized from docs/next before release
release-docs-check:
    @for file in README.md CHANGELOG.md; do \
        if ! diff -u "$file" "docs/next/$file"; then \
            echo "error: $file differs from docs/next/$file; finalize release docs before releasing"; \
            exit 1; \
        fi; \
    done
    @for file in CONFIGURATION.md INTEGRATIONS.md SOCKET_API.md; do \
        if [ -e "$file" ]; then \
            echo "error: $file was replaced by website docs; remove the root copy"; \
            exit 1; \
        fi; \
    done
    @test -d docs/next/website/src/content/docs
    @for file in website/src/content/docs/*.mdx; do \
        staged="docs/next/website/src/content/docs/$(basename "$file")"; \
        if [ ! -f "$staged" ]; then \
            echo "error: $staged is missing; docs/next/website/src/content/docs must mirror website/src/content/docs"; \
            exit 1; \
        fi; \
        if ! diff -u "$file" "$staged"; then \
            echo "error: $file differs from $staged; finalize website docs before releasing"; \
            exit 1; \
        fi; \
    done
    @for file in docs/next/website/src/content/docs/*.mdx; do \
        released="website/src/content/docs/$(basename "$file")"; \
        if [ ! -f "$released" ]; then \
            echo "error: $file has no matching released website doc"; \
            exit 1; \
        fi; \
    done

# Report what landed since the last release and the bump it calls for (#509) — a recommendation, not an instruction
release-plan *since:
    python3 scripts/changelog.py plan{{if since != "" { " --since " + since } else { "" }}}

# Prepare the release commit without tagging or pushing; extra flags go to check-version (usage: just release-prepare 0.1.1 --allow-below-recommended)
release-prepare version *flags:
    @printf '%s\n' '{{version}}' | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { \
        echo "error: version must look like 1.0.0 without a v prefix"; \
        exit 1; \
    }
    @if [ -n "$(git status --porcelain)" ]; then \
        echo "error: commit your changes first"; \
        exit 1; \
    fi
    @git fetch origin main --tags
    @if git rev-parse "v{{version}}" >/dev/null 2>&1; then \
        echo "error: tag v{{version}} already exists"; \
        exit 1; \
    fi
    just release-docs-check
    python3 scripts/changelog.py check-version --version {{version}} {{flags}}
    python3 scripts/changelog.py prepare --version {{version}}
    cp CHANGELOG.md docs/next/CHANGELOG.md
    sed -i.bak 's/^version = ".*"/version = "{{version}}"/' Cargo.toml && rm -f Cargo.toml.bak
    cargo update -p flock-ai --offline
    just check
    git add CHANGELOG.md docs/next/CHANGELOG.md Cargo.toml Cargo.lock
    git diff --cached --quiet || git commit -m "release: v{{version}}"
    @echo "v{{version}} release commit prepared locally as a dry run. Cut the real release with: just release {{version}}"

# Promote opens the `release: vX.Y.Z` PR into dev; once that merges and passes its checks,
# release-follow-up.yml fast-forwards main to it and tags it. `main` moves no other way.
# `just release-prepare` is the local dry run of the same commit.
# Cut a release by dispatching the Promote workflow on dev (usage: just release, or just release 0.9.0)
release version='':
    @if [ -n '{{version}}' ]; then \
        printf '%s\n' '{{version}}' | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { \
            echo "error: version must look like 1.0.0 without a v prefix"; \
            exit 1; \
        }; \
    fi
    just release-plan
    gh workflow run promote.yml --ref dev{{if version != "" { " -f version=" + version } else { "" }}}
    @echo "promote.yml dispatched; follow it with: gh run list --workflow promote.yml --limit 1"

# Print default config
default-config:
    cargo run --release --locked -- --default-config
