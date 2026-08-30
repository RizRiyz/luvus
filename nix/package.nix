# nixpkgs package definition for luvus.
#
# This is written to drop straight into nixpkgs at
# `pkgs/by-name/lu/luvus/package.nix`. It differs from the repo's `flake.nix`
# in the two ways nixpkgs requires: it fetches a *released tag* with a fixed
# hash (rather than the local tree), and it vendors dependencies via `cargoHash`
# (rather than a local `cargoLock.lockFile`). See `nix/README.md` for how to fill
# in the two `lib.fakeHash` placeholders and submit the PR.
{
  lib,
  rustPlatform,
  fetchFromGitHub,
  makeWrapper,
  git,
  gh,
  openssh,
  bashInteractive,
  coreutils,
  procps,
  sqlite,
  stdenv,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "luvus";
  version = "0.13.2";

  # Required for new by-name packages (nixpkgs-vet NPV-166).
  __structuredAttrs = true;

  src = fetchFromGitHub {
    owner = "RizRiyz";
    repo = "luvus";
    tag = "v${finalAttrs.version}";
    hash = lib.fakeHash;
  };

  cargoHash = lib.fakeHash;

  nativeBuildInputs = [ makeWrapper ];

  # On macOS, Cargo.toml deliberately links the SYSTEM sqlite
  # (/usr/lib/libsqlite3.dylib) instead of bundling it, so the crate passes
  # `-lsqlite3` at the final link. The Nix build sandbox has no /usr/lib, so
  # the link fails with `ld: library not found for -lsqlite3`. Provide nixpkgs'
  # sqlite on Darwin: the link resolves, and the dylib lands in the runtime
  # closure. Linux keeps rusqlite's bundled engine and needs nothing extra.
  buildInputs = lib.optionals stdenv.hostPlatform.isDarwin [ sqlite ];

  # The test suite spawns real PTYs, `ps`, and child processes and reads $HOME,
  # all awkward inside the Nix sandbox; upstream CI runs the full suite on every
  # push, so the package build just compiles the release binary.
  doCheck = false;

  # luvus shells out to these at runtime; bake them into PATH because NixOS has
  # no implicit global one. The user's own PATH is still appended, so a newer
  # git/gh they installed wins. `ps` is Linux-only here (procps); on Darwin the
  # system `ps` is used.
  postFixup = ''
    wrapProgram $out/bin/luvus \
      --prefix PATH : ${
        lib.makeBinPath (
          [
            git
            gh
            openssh
            bashInteractive
            coreutils
          ]
          ++ lib.optionals stdenv.hostPlatform.isLinux [ procps ]
        )
      }
  '';

  meta = {
    description = "Mission control for your AI coding agents";
    homepage = "https://luvus.dev";
    changelog = "https://github.com/RizRiyz/luvus/releases/tag/v${finalAttrs.version}";
    license = lib.licenses.asl20;
    mainProgram = "luvus";
    maintainers = with lib.maintainers; [ rizriyz ];
    platforms = lib.platforms.unix;
  };
})
