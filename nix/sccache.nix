# Keep the devShell upgrade independent of the rest of pinned nixpkgs.
{
  sccache,
  fetchFromGitHub,
  rustPlatform,
}:
let
  # A version bump needs both source and cargo vendor hashes refreshed.
  version = "0.18.0";
  src = fetchFromGitHub {
    name = "sccache-${version}-source";
    owner = "mozilla";
    repo = "sccache";
    tag = "v${version}";
    hash = "sha256-QGsDxUAQzAr6Aia5/D21cGbVV0YJyuH3aBmEr9NynXQ=";
  };
in
sccache.overrideAttrs {
  inherit version src;
  cargoDeps = rustPlatform.fetchCargoVendor {
    inherit src;
    hash = "sha256-Jr4+46/yhcuTC017TA2UZQJUNIBuioI3xZnxxD/rKuc=";
  };
}
