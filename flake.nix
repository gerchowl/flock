{
  description = "flock — terminal workspace manager for AI coding agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # Shareable code-quality / governance gates + toolbelt (prek/gitleaks/
    # cargo-deny/…), wired into the devShell so the discipline is the same
    # everywhere. Follows our nixpkgs to keep a single package set.
    guardrails.url = "github:gerchowl/guardrails";
    guardrails.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      guardrails,
    }:
    let
      lib = nixpkgs.lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };
      stableSccacheFor =
        pkgs:
        pkgs.writeShellScriptBin "sccache" ''
          set -eu
          SCCACHE_REAL=${pkgs.sccache}/bin/sccache
          ${builtins.readFile ./nix/sccache.sh}
        '';
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          flock = pkgs.callPackage ./nix/package.nix {
            buildCommit = self.rev or self.dirtyRev or null;
          };
          # Same binary plus the `web` feature (the `flk web` xterm bridge,
          # gerchowl/flock#131). Kept out of `default` so a stock build stays
          # lean; hosts that serve the web terminal pin this output.
          flock-web = pkgs.callPackage ./nix/package.nix {
            buildCommit = self.rev or self.dirtyRev or null;
            withWeb = true;
          };
        in
        {
          inherit flock flock-web;
          default = flock;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/flk";
          meta.description = "Run Flock";
        };
      });

      checks = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          flock = self.packages.${system}.default;
          sccache =
            pkgs.runCommand "flock-sccache-probe"
              {
                nativeBuildInputs = [
                  pkgs.python3
                  pkgs.rustc
                  (stableSccacheFor pkgs)
                ];
                RUSTC_WRAPPER = "${stableSccacheFor pkgs}/bin/sccache";
                FLOCK_TEST_SCCACHE_WRAPPER = "${stableSccacheFor pkgs}/bin/sccache";
                FLOCK_TEST_SCCACHE_RAW = "${pkgs.sccache}/bin/sccache";
              }
              ''
                python3 ${./scripts/probe_sccache.py}
                touch "$out"
              '';
          # The guardrails gate sweep, with the SAME env knobs as
          # .pre-commit-config.yaml — `nix flake check` ≡ `prek run --all-files`
          # for the gate layer; divergence means one of the two is misconfigured.
          # The sandbox tree has no .git, so a throwaway index is fabricated for
          # the gates' `git ls-files` / the DEBT.md census's `git grep`.
          gates =
            pkgs.runCommand "flock-guardrails-gates"
              {
                nativeBuildInputs = [
                  guardrails.packages.${system}.default
                  pkgs.gitMinimal
                  # `log-budget` parses the sample and budgets file in python
                  # (tomllib, so >=3.11). The sandbox has no interpreter of its
                  # own, and the gate `exec`s straight into it — so without
                  # this it dies 127 before it can even decide to skip.
                  pkgs.python3
                ];
              }
              ''
                cp -r ${self} tree && chmod -R +w tree && cd tree \
                  && export HOME="$TMPDIR" \
                  && git init -q && git add -A \
                  && guardrails-no-fake-impl . \
                  && env GUARDRAILS_OUTPUT_GLOBS='*/cli/*:*/cli.rs:*/update.rs:*/remote.rs:*/server/headless.rs:*/client/mod.rs:*/integration/mod.rs:*/web/*:scripts/*:vendor/*' \
                    guardrails-no-debug-leftovers . \
                  && guardrails-no-commented-code . \
                  && guardrails-no-conflict-markers . \
                  && env GUARDRAILS_TRACE_ALLOW_GLOBS='*/logging.rs' guardrails-no-raw-trace-fields src \
                  && guardrails-derived-docs . \
                  && guardrails-adr-matrix \
                  && env GUARDRAILS_CI_SHIM_ENFORCE=1 guardrails-ci-shim .github/workflows \
                  && guardrails-log-budget \
                  && python3 scripts/fixture_hosts.py $(find . -name '*.rs' -not -path './vendor/*') \
                  && python3 scripts/exec_name_gate.py \
                  && touch $out
              '';
          default = self.checks.${system}.flock;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          stableSccache = stableSccacheFor pkgs;
        in
        {
          # guardrails brings the governance toolbelt (prek/gitleaks/cargo-deny/
          # …) and auto-installs the pre-commit hooks; `extra` carries flock's
          # own build toolchain. SDKROOT comes from the darwin stdenv for free,
          # so the only env we restore is the libghostty-vt build tuning.
          default = guardrails.lib.${system}.mkDevShell {
            inherit pkgs;
            extra = with pkgs; [
              cargo
              (callPackage ./nix/nextest.nix { })
              clippy
              cmake
              # Maintenance tests exercise Git fixtures and workflow/doc scripts.
              git
              jq
              just
              ninja
              nodejs
              perl
              pkg-config
              rustc
              rustfmt
              # Maintenance tests parse hook YAML with Ruby’s standard library.
              ruby
              zig_0_15
            ];
            # Leave sccache's platform artifact cache unchanged and give the daemon a
            # persistent TMPDIR and its own socket (#1325). A wrapper covers
            # lazy startup and idle restarts without changing the shell TMPDIR.
            # SCCACHE_DIR alone only controls artifacts, not temporary files.
            # sccache 0.15 endpoint contract: https://github.com/mozilla/sccache/blob/v0.15.0/README.md#usage
            hook = ''
              export PATH="${stableSccache}/bin:$PATH"
              # The isolated cache-reuse probe also needs the unwrapped binary.
              export FLOCK_TEST_SCCACHE_WRAPPER="${stableSccache}/bin/sccache"
              export FLOCK_TEST_SCCACHE_RAW="${pkgs.sccache}/bin/sccache"
              case "$RUSTC_WRAPPER" in
                sccache|*/sccache)
                  export RUSTC_WRAPPER="${stableSccache}/bin/sccache"
                  ;;
              esac
              export LIBGHOSTTY_VT_OPTIMIZE=Debug
              export LIBGHOSTTY_VT_SIMD=true
            '';
          };
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt);

      overlays.default = final: _prev: {
        flock = final.callPackage ./nix/package.nix {
          buildCommit = self.rev or self.dirtyRev or null;
        };
      };
    };
}
