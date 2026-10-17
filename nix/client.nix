# The web client's production bundle, `web/client/dist`, built reproducibly
# (ADR-0036 §7, ADR-0038 §6; #1731).
#
# Two derivations:
#
# - `deps`, a fixed-output derivation: `bun install --frozen-lockfile` of
#   exactly what `web/client/bun.lock` pins, with network, and nothing else.
#   It installs the optional native packages of every platform (`--os='*'
#   --cpu='*'`) and keeps the bindings of the four systems below, so its
#   output is the same on every system and one hash covers them all.
#   Lifecycle scripts never run, and no fixup runs either, since a
#   fixed-output path may not refer to the store.
# - the bundle: the client sources plus a copy of `deps`, built offline
#   with `bun run build` (`vite build`), the same command and the same
#   nodejs runtime as `just client-check`. Only the build platform's native
#   bindings are kept, and on Linux they are patched to find their
#   libraries. `$out` is the built `dist/`: `index.html` plus `assets/`.
#
# The hash is pinned in `nix/client-deps.json`, together with the sha256
# of the `bun.lock` it was computed from. Evaluation fails, naming the file
# and the command, when the lock no longer matches: run
# `just client-deps-hash` and commit `nix/client-deps.json`.
#
# This is only the bundle. Serving it from `flk` (embedding, cache headers,
# refusing to start without one) is S7-min slice 7a's.
{
  lib,
  stdenv,
  stdenvNoCC,
  bun,
  nodejs,
  cacert,
  autoPatchelfHook,
}:
let
  root = ../web/client;
  lock = root + "/bun.lock";
  pinned = lib.importJSON ./client-deps.json;
  lockSha256 = builtins.hashFile "sha256" lock;

  # The native packages the build platform loads while building (vite's lightningcss and
  # rolldown's bindings, and fsevents on macOS); every other platform's are
  # removed before the build.
  suffixes = {
    aarch64-darwin = "darwin-arm64";
    x86_64-darwin = "darwin-x64";
    aarch64-linux = "linux-arm64-gnu";
    x86_64-linux = "linux-x64-gnu";
  };
  hostSuffix =
    suffixes.${stdenv.buildPlatform.system}
      or (throw "flock-client: no native bindings are known for ${stdenv.buildPlatform.system}");

  deps = stdenvNoCC.mkDerivation {
    pname = "flock-client-deps";
    # The fetching bun's version and the lock's digest are in the name, so
    # neither a changed lock nor a bun bump (which may lay node_modules out
    # differently) reuses an output fetched for another.
    version = "${bun.version}-${builtins.substring 0 16 lockSha256}";
    src = lib.fileset.toSource {
      inherit root;
      fileset = lib.fileset.unions [
        (root + "/package.json")
        lock
      ];
    };
    nativeBuildInputs = [ bun ];
    dontConfigure = true;
    dontFixup = true;
    buildPhase = ''
      runHook preBuild
      export HOME="$TMPDIR"
      export BUN_INSTALL_CACHE_DIR="$TMPDIR/bun-cache"
      bun install --frozen-lockfile --ignore-scripts --no-progress \
        --linker=hoisted --backend=copyfile --os='*' --cpu='*'
      runHook postBuild
    '';
    # Kept: what `vite build` loads on the systems below. TypeScript's own
    # per-platform compilers (about 400 MB) are type-checking only, and so
    # are dropped, as are other platforms' bindings. The rule depends on no
    # system, so the hash stays the same everywhere.
    installPhase = ''
      runHook preInstall
      rm -rf node_modules/.cache node_modules/@typescript/typescript-*
      for binding in node_modules/@rolldown/binding-* node_modules/lightningcss-*; do
        case "$binding" in
          ${lib.concatMapStringsSep "|" (suffix: "*-${suffix}") (lib.attrValues suffixes)}) ;;
          *) rm -rf "$binding" ;;
        esac
      done
      mkdir -p "$out"
      cp -R node_modules "$out/node_modules"
      runHook postInstall
    '';
    SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
    impureEnvVars = lib.fetchers.proxyImpureEnvVars;
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = if pinned.hash == "" then lib.fakeHash else pinned.hash;
  };
in
assert lib.assertMsg (pinned.lock == lockSha256) ''
  web/client/bun.lock changed since nix/client-deps.json was computed
  (pinned for sha256 ${pinned.lock}, the lock is now ${lockSha256}).
  Run `just client-deps-hash` and commit nix/client-deps.json.'';
stdenv.mkDerivation {
  pname = "flock-client";
  version = "0.0.0";
  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (root + "/package.json")
      lock
      (root + "/index.html")
      (root + "/vite.config.ts")
      (root + "/tsconfig.json")
      (root + "/src")
    ];
  };
  nativeBuildInputs = [
    bun
    nodejs
  ]
  # The bindings run during the build, so the build platform decides.
  ++ lib.optional stdenv.buildPlatform.isLinux autoPatchelfHook;
  buildInputs = lib.optional stdenv.buildPlatform.isLinux stdenv.cc.cc.lib;
  configurePhase = ''
    runHook preConfigure
    cp -R ${deps}/node_modules node_modules
    chmod -R u+w node_modules
    for binding in node_modules/@rolldown/binding-* node_modules/lightningcss-*; do
      case "$binding" in
        *-${hostSuffix}) ;;
        *) rm -rf "$binding" ;;
      esac
    done
    ${lib.optionalString (!stdenv.buildPlatform.isDarwin) "rm -rf node_modules/fsevents"}
    ${lib.optionalString stdenv.buildPlatform.isLinux "autoPatchelf node_modules"}
    patchShebangs node_modules
    runHook postConfigure
  '';
  buildPhase = ''
    runHook preBuild
    export HOME="$TMPDIR"
    bun run build
    runHook postBuild
  '';
  installPhase = ''
    runHook preInstall
    cp -R dist "$out"
    runHook postInstall
  '';
  passthru = { inherit deps; };
  meta = {
    description = "The flock web client's production bundle (web/client/dist)";
    platforms = [
      "aarch64-darwin"
      "x86_64-darwin"
      "aarch64-linux"
      "x86_64-linux"
    ];
  };
}
