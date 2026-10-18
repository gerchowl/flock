# flock web client

The browser workbench (ADR-0038): Solid and Vite, built from the dependencies
pinned in `bun.lock`.

## Working on it

- `just client-check`: unit and DOM tests, the browser tests in `e2e/`, the
  type check and the production build.
- `just client-preview`: a fixture-mode build (a replayed session, no server
  needed) served on the loopback address.

## The production bundle in nix

`nix build .#flock-client` builds `dist/` reproducibly (ADR-0036 §7, #1731).
`$out` is what `bun run build` writes to `dist/`: `index.html` and `assets/`.
It is byte-identical to a local `bun run build`. S7-min slice 7a embeds this
output in `flk`; the package here is only the bundle.

How it is built (`nix/client.nix`):

1. A fixed-output derivation runs `bun install --frozen-lockfile` with
   network. It installs every platform's optional native packages, then keeps
   the rolldown and lightningcss bindings of the four systems the flake builds
   and drops TypeScript's per-platform compilers, which only type-check. Its
   output is therefore the same on every system, and one hash, pinned in
   `nix/client-deps.json`, covers them all.
2. The build copies those dependencies, keeps the build platform's bindings
   (patched to find their libraries on Linux), and runs `bun run build`
   offline under the same nodejs as the dev shell.

### After changing `bun.lock` or Bun

Run `just client-deps-hash` and commit `nix/client-deps.json`. The file
records the lock’s sha256 and Bun version. Changing either without refreshing
the pin fails evaluation with a message naming this command. Failed refreshes
and INT/TERM interruptions restore the original pin, including during verification.
`dist/` is never committed.

A same-version nixpkgs Bun patch can reuse the cached fixed-output outPath;
the version check cannot detect it. Run `just client-deps-hash` explicitly
after such a patch. The mismatch diagnostic documents this limit too.

The client-dependency workflow builds and rebuilds the FOD on x86_64-linux
and aarch64-darwin, records the actual NAR hashes, and requires matching
hashes, lock digests and Bun versions. Rebuilding prevents a shared cached
output from hiding a platform-specific difference.

### Known limits

- Only x86_64-linux (CI's `nix flake check`) and aarch64-darwin are built.
  aarch64-linux and x86_64-darwin are only evaluated.
- A new optional native package is kept for every platform, because only the
  rolldown and lightningcss bindings are pruned. That costs size, and on
  Darwin it goes unnoticed, since nothing there needs patching.
- The hash covers the lock and bun's version (both are in the fetch's name),
  but not the rest of nixpkgs.
