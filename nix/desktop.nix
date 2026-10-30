# ADR-0038 §6; #2785. The shell stays independent of flk's closure.
{
  lib,
  stdenvNoCC,
  rustPlatform,
  bun,
  nodejs,
  cacert,
  cargo-tauri,
  writeShellScriptBin,
  rcodesign,
  jq,
  flock-client,
}:
let
  root = ../desktop;
  manifest = lib.importTOML (root + "/Cargo.toml");
  npm = lib.importJSON (root + "/package.json");
  pinned = lib.importJSON ./desktop-deps.json;
  lockSha256 = builtins.hashFile "sha256" (root + "/bun.lock");
  npmSha256 = builtins.hashFile "sha256" (root + "/package.json");
  macos = (lib.importJSON (root + "/tauri.macos.conf.json")).bundle.macOS;
  config = builtins.toJSON {
    build = {
      frontendDist = "${flock-client}";
      beforeBuildCommand = "";
    };
  };
  deps = stdenvNoCC.mkDerivation {
    pname = "flock-desktop-deps";
    version = "${bun.version}-${builtins.substring 0 16 lockSha256}-${
      builtins.substring 0 16 npmSha256
    }";
    src = lib.fileset.toSource {
      inherit root;
      fileset = lib.fileset.unions [
        (root + "/package.json")
        (root + "/bun.lock")
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
        --linker=hoisted --backend=copyfile --os=darwin --cpu='*'
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      mkdir -p "$out/node_modules"
      # Like flock-client, omit Bun's transient installation cache.
      (
        shopt -s dotglob nullglob
        for desktop_dependency in node_modules/*; do
          if [ "$desktop_dependency" != node_modules/.cache ]; then
            cp -R "$desktop_dependency" "$out/node_modules/"
          fi
        done
      )
      runHook postInstall
    '';
    SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
    impureEnvVars = lib.fetchers.proxyImpureEnvVars;
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = if pinned.hash == "" then lib.fakeHash else pinned.hash;
    passthru.bunVersion = bun.version;
  };
  # The npm CLI is the same Rust CLI with a Node entry point. Cargo passes
  # its subcommand name as argv[1]; the npm entry point expects only its args.
  cli = writeShellScriptBin "cargo-tauri" ''
    if [ "''${1-}" = tauri ]; then shift; fi
    exec ${nodejs}/bin/node ${deps}/node_modules/@tauri-apps/cli/tauri.js "$@"
  '';
in
assert lib.assertMsg (pinned.lock == lockSha256) ''
  desktop/bun.lock changed since nix/desktop-deps.json was computed.
  Run `just desktop-deps-hash` and commit nix/desktop-deps.json.'';
assert lib.assertMsg ((pinned.manifest or null) == npmSha256) ''
  desktop/package.json changed since nix/desktop-deps.json was computed.
  Run `just desktop-deps-hash` and commit nix/desktop-deps.json.'';
assert lib.assertMsg (pinned.hash == "" || (pinned.bun or null) == bun.version) ''
  Bun changed since nix/desktop-deps.json was computed.
  Run `just desktop-deps-hash` and commit nix/desktop-deps.json.
  A same-version nixpkgs Bun patch can reuse the cached fixed-output path;
  refresh explicitly with the same command after such a patch.'';
assert lib.assertMsg (
  lib.versions.majorMinor npm.devDependencies."@tauri-apps/cli"
  == lib.versions.majorMinor (lib.removePrefix "=" manifest.dependencies.tauri.version)
) "flock-desktop: review CLI/crate compatibility before changing their major/minor versions";
assert lib.assertMsg (
  macos.signingIdentity == "-" && macos.hardenedRuntime
) "flock-desktop requires ad-hoc signing and hardened runtime";
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "flock-desktop";
  version = manifest.package.version;
  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (root + "/Cargo.toml")
      (root + "/Cargo.lock")
      (root + "/package.json")
      (root + "/bun.lock")
      (root + "/build.rs")
      (root + "/src")
      (root + "/tests")
      (root + "/capabilities")
      (root + "/icons")
      (root + "/tauri.conf.json")
      (root + "/tauri.macos.conf.json")
      (root + "/entitlements.plist")
    ];
  };
  cargoLock.lockFile = root + "/Cargo.lock";
  nativeBuildInputs = [
    (cargo-tauri.hook.override { cargo-tauri = cli; })
    bun
    jq
  ];
  # Use Tauri's explicit no-sign stage; sign the final bundle after stripping
  # and install-name fixups, which otherwise invalidate its signature.
  tauriBuildFlags = [
    "--no-sign"
    "--config"
    config
  ];
  # --config supplies the CLI build; TAURI_CONFIG also reaches the separate
  # cargo test check phase, whose build.rs/tauri::generate_context! must use
  # the same store frontend instead of the absent local client dist directory.
  env.TAURI_CONFIG = config;
  # The existing Rust hardening test checks the manifest. Also check Cargo's
  # resolved features so enabling tauri/devtools through build flags fails.
  preBuild = ''
    # Darwin build directories are randomized. Keep file!/panic paths stable
    # before linking and signing, so they do not change the binary or signature.
    export RUSTFLAGS="''${RUSTFLAGS:-} --remap-path-prefix=$NIX_BUILD_TOP=/build"
    cargo metadata --offline --locked --format-version 1 \
      ${
        lib.optionalString ((finalAttrs.cargoBuildFeatures or [ ]) != [ ]) (
          "--features " + lib.escapeShellArg (lib.concatStringsSep "," finalAttrs.cargoBuildFeatures)
        )
      } > desktop-metadata.json
    if ! jq -e '
      (.packages[] | select(.name == "tauri") | .id) as $id
      | .resolve.nodes[] | select(.id == $id)
      | .features | index("devtools") == null
    ' desktop-metadata.json; then
      echo "flock-desktop: release builds must not enable tauri/devtools" >&2
      exit 1
    fi
  '';
  doCheck = true;
  checkType = "release";
  checkFlags = [ "--test-threads=2" ];
  postCheck = ''
    bun test
  '';
  postFixup = ''
    ${rcodesign}/bin/rcodesign sign \
      --timestamp-url none \
      --code-signature-flags runtime \
      --entitlements-xml-file "${finalAttrs.src}/${macos.entitlements}" \
      "$out/Applications/flock.app"
  '';
  passthru = {
    inherit deps;
    cliVersion = npm.devDependencies."@tauri-apps/cli";
  };
  meta = {
    description = "Flock desktop shell with the flock-client bundle";
    license = lib.licenses.agpl3Plus;
    # WebKitGTK wrapping and Linux desktop integration need their own slice (#2790).
    platforms = lib.platforms.darwin;
  };
})
