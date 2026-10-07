{
  lib,
  stdenv,
  rustPlatform,
  callPackage,
  runCommand,
  writeShellScriptBin,
  zig_0_15,
  zstd,
  pkg-config,
  git,
  apple-sdk ? null,
  cctools ? null,
  # Build identity (src/build_info.rs). With no channel the version is the
  # plain Cargo version: `main` only moves on a release, so a build of it is
  # exactly that release. A non-null channel renders "<base>-<channel>.<id>".
  # buildCommit is the full rev, recorded as provenance (report blocks)
  # without changing the version string.
  buildChannel ? null,
  buildId ? null,
  buildCommit ? null,
  # Build the `web` cargo feature (the `flk web` xterm bridge, gerchowl/flock#131).
  # Off by default so the standard build stays lean (no axum/rust-embed); the
  # flake exposes a `flock-web` package with this on. The binary is still
  # `bin/flk` — the feature only adds the `web` subcommand.
  withWeb ? false,
}:

let
  manifest = lib.importTOML ../Cargo.toml;
  zigDeps = callPackage ../vendor/libghostty-vt/build.zig.zon.nix {
    name = "flock-libghostty-vt-zig-cache";
    inherit zstd;
    linkFarm =
      name: entries:
      runCommand name { } ''
        mkdir -p $out
        ${lib.concatMapStringsSep "\n" (entry: ''
          cp -rL ${entry.path} $out/${entry.name}
        '') entries}
      '';
  };

  darwinSdkRoot = "${apple-sdk}/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk";
  darwinDeveloperDir = "${apple-sdk}/Platforms/MacOSX.platform/Developer";
  darwinXcodeSelect = writeShellScriptBin "xcode-select" ''
    if [ "$1" = "--print-path" ]; then
      echo ${lib.escapeShellArg darwinDeveloperDir}
      exit 0
    fi
    echo "unsupported xcode-select invocation: $*" >&2
    exit 1
  '';
  darwinXcrun = writeShellScriptBin "xcrun" ''
    if [ "$1" = "--sdk" ] && [ "$3" = "--show-sdk-path" ]; then
      echo ${lib.escapeShellArg darwinSdkRoot}
      exit 0
    fi
    echo "unsupported xcrun invocation: $*" >&2
    exit 1
  '';
in
rustPlatform.buildRustPackage {
  pname = "flock" + lib.optionalString withWeb "-web";
  version = manifest.package.version;

  buildFeatures = lib.optionals withWeb [ "web" ];

  src = lib.fileset.toSource {
    root = ./..;
    fileset = lib.fileset.intersection (lib.fileset.fromSource (lib.sources.cleanSource ./..)) (
      lib.fileset.unions [
        ../assets
        ../src
        ../vendor/libghostty-vt
        ../vendor/libghostty-vt.vendor.json
        ../build.rs
        ../Cargo.lock
        ../Cargo.toml
      ]
    );
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
  };

  nativeBuildInputs = [
    git
    pkg-config
  ]
  ++ lib.optionals stdenv.hostPlatform.isDarwin [
    cctools
    darwinXcodeSelect
    darwinXcrun
  ];

  env = {
    LIBGHOSTTY_VT_OPTIMIZE = "ReleaseFast";
    LIBGHOSTTY_VT_SIMD = "true";
    LIBGHOSTTY_VT_ZIG_SYSTEM_DIR = zigDeps;
    ZIG = lib.getExe zig_0_15;
  }
  // lib.optionalAttrs (buildChannel != null) {
    FLOCK_BUILD_CHANNEL = buildChannel;
  }
  // lib.optionalAttrs (buildId != null) {
    FLOCK_BUILD_ID = buildId;
  }
  // lib.optionalAttrs (buildCommit != null) {
    FLOCK_BUILD_COMMIT = buildCommit;
  }
  // lib.optionalAttrs stdenv.hostPlatform.isDarwin {
    SDKROOT = darwinSdkRoot;
  };

  preBuild = ''
    export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-global-cache"
    export ZIG_LOCAL_CACHE_DIR="$TMPDIR/zig-local-cache"
  '';

  # Rust tests are covered by the normal CI workflow. The Nix check is
  # intentionally build-only so it validates packaging inputs without
  # duplicating the full Rust test suite.
  doCheck = false;

  meta = {
    description = "Terminal workspace manager for AI coding agents";
    homepage = "https://flock.dev";
    license = lib.licenses.agpl3Plus;
    mainProgram = "flk";
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
