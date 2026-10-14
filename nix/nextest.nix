# #1393: upstream nextest #3553 prevents sibling capture-pipe inheritance.
# Remove this override when pinned nixpkgs provides nextest >= 0.9.145.
{ cargo-nextest, fetchFromGitHub, rustPlatform }:
let
  version = "0.9.146";
  src = fetchFromGitHub {
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
