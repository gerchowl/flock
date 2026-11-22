# #1393: upstream nextest #3553 prevents sibling capture-pipe inheritance.
# Remove this override when pinned nixpkgs supplies the shared nextest-version.
{ cargo-nextest, fetchFromGitHub, rustPlatform }:
let
  # Shared with the CI installers via scripts/nextest_version.py.
  # A version bump requires refreshing both the source and cargo vendor hashes.
  version = builtins.replaceStrings [ "\n" ] [ "" ] (builtins.readFile ./nextest-version);
  src = fetchFromGitHub {
    # A stale hash must not reuse a cached source from a different version.
    name = "cargo-nextest-${version}-source";
    owner = "nextest-rs";
    repo = "nextest";
    tag = "cargo-nextest-${version}";
    hash = "sha256-2ndrHZ1pmu1oBZGlq5pyv0vhSEUf23gNRyyenNw0fdk=";
  };
in
cargo-nextest.overrideAttrs {
  inherit version src;
  cargoDeps = rustPlatform.fetchCargoVendor {
    inherit src;
    hash = "sha256-ZW0/JQ9RECxfDn479ww4bO4ixoMGaM+GtGUOCWy2Cyg=";
  };
}
