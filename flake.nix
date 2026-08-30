{
  description = "luvus — mission control for your AI coding agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

        # Tools luvus shells out to at runtime (see `Command::new(...)` in src):
        #   git  — the git tab + worktrees
        #   gh   — GitHub PR/issue views (degrades cleanly when absent)
        #   ps   — agent *identity* detection reads pane processes; without it
        #          detection falls back to weaker text heuristics (procps on
        #          Linux; on Darwin the system `ps` is used, so it is omitted)
        #   ssh  — `--remote` attach
        #   sh   — spawning panes / resume commands
        # NixOS has no implicit global PATH, so we bake these in with wrapProgram
        # rather than hope the user installed them. The original PATH is still
        # appended, so a user's own newer git/gh is preferred if present.
        runtimeTools =
          with pkgs;
          [
            git
            gh
            openssh
            bashInteractive
            coreutils
          ]
          ++ pkgs.lib.optionals stdenv.hostPlatform.isLinux [ procps ];

        luvus = pkgs.rustPlatform.buildRustPackage {
          pname = "luvus";
          version = cargoToml.package.version;

          # Only what cargo needs. Dropping `target/` (dirty build artifacts that
          # would otherwise invalidate the build) and the Astro site (its
          # `node_modules` is large and irrelevant to the crate) keeps the source
          # copied into the store small and the build hash stable. Neither name
          # occurs inside `src/`, so a plain basename match is safe.
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              let
                name = baseNameOf path;
              in
              !(name == "target" || name == "website")
              && pkgs.lib.cleanSourceFilter path type;
          };

          # The committed lockfile pins every dependency, so the build is
          # reproducible without a separate vendored hash to keep in sync.
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # On macOS, Cargo.toml deliberately links the SYSTEM sqlite
          # (/usr/lib/libsqlite3.dylib) instead of bundling it, so the crate
          # passes `-lsqlite3` at the final link. The Nix build sandbox has no
          # /usr/lib, so the link fails with `ld: library not found for
          # -lsqlite3`. Provide nixpkgs' sqlite on Darwin: the link resolves, and
          # the dylib lands in the runtime closure — more hermetic than the
          # release binary, which depends on an impure /usr/lib path. Linux keeps
          # rusqlite's bundled engine and needs nothing extra.
          buildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
            pkgs.sqlite
          ];

          # The suite spawns real PTYs, `ps`, and child processes, and reads
          # $HOME — all awkward inside the Nix build sandbox. CI runs the full
          # suite on every push (see .github/workflows/ci.yml); the package build
          # just compiles the release binary.
          doCheck = false;

          postFixup = ''
            wrapProgram $out/bin/luvus \
              --prefix PATH : ${pkgs.lib.makeBinPath runtimeTools}
          '';

          meta = with pkgs.lib; {
            description = "Mission control for your AI coding agents";
            homepage = "https://luvus.dev";
            license = licenses.asl20;
            mainProgram = "luvus";
            platforms = platforms.unix;
          };
        };
      in
      {
        packages.default = luvus;

        # `nix run github:RizRiyz/luvus`
        apps.default = flake-utils.lib.mkApp { drv = luvus; };

        # `nix develop` — a shell with the Rust toolchain and the runtime tools,
        # so `cargo run -- --local` behaves the same as an installed luvus.
        devShells.default = pkgs.mkShell {
          packages =
            with pkgs;
            [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
            ]
            ++ runtimeTools;
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      }
    );
}
