# Submitting luvus to nixpkgs

`package.nix` in this folder is a ready-to-submit nixpkgs definition. It builds a
**released tag** (not the local tree), so it is independent of `flake.nix` (which
is for `nix run github:RizRiyz/luvus` and dev shells). Follow these steps to get
`pkgs.luvus` into nixpkgs.

## 1. Version (kept in sync automatically)

`scripts/release.sh` bumps `package.nix`'s `version` on every release and resets
the two hashes to `lib.fakeHash`, so the version here always matches the newest
**released tag**. You only ever need to recompute the hashes (next step). If you
are packaging by hand, confirm `version` matches an existing tag at
`github.com/RizRiyz/luvus/tags` first.

## 2. Fill in the two hashes

Both start as `lib.fakeHash`; Nix prints the real value on the first build. Do the
**source** hash first, then the **cargo** hash.

```sh
# From a nixpkgs checkout (or with this file copied into one), build the package.
# The build fails with "hash mismatch ... got: sha256-XXXX=" — paste that in.
nix-build -A luvus

# Repeat once more: after fixing the src hash, the next failure gives cargoHash.
```

Or compute the source hash directly, without a build:

```sh
nix-prefetch-url --unpack \
  https://github.com/RizRiyz/luvus/archive/refs/tags/v0.9.4.tar.gz
# convert to SRI: nix hash to-sri --type sha256 <the-hash-it-printed>
```

## 3. Add yourself as a maintainer

Edit `maintainers/maintainer-list.nix` in your nixpkgs fork and add an entry
(alphabetical by handle):

```nix
rizriyz = {
  email = "2667489+RizRiyz@users.noreply.github.com";
  github = "RizRiyz";
  githubId = 2667489;
  name = "Riz";
};
```

Then reference it in `package.nix`:

```nix
maintainers = with lib.maintainers; [ rizriyz ];
```

## 4. Place the file and build

```sh
git clone https://github.com/NixOS/nixpkgs   # or your fork
mkdir -p nixpkgs/pkgs/by-name/lu/luvus
cp package.nix nixpkgs/pkgs/by-name/lu/luvus/package.nix
cd nixpkgs
nix-build -A luvus            # must succeed and produce ./result/bin/luvus
./result/bin/luvus --version  # smoke test
```

## 5. Review-check and open the PR

```sh
nix-shell -p nixpkgs-review --run "nixpkgs-review rev HEAD"   # after committing
```

Commit message follows the nixpkgs convention:

```
luvus: init at 0.9.4
```

Open the PR against `NixOS/nixpkgs` (base branch `master`). Expect ofborg CI and a
human review; keep the PR description short and note that tests are disabled
(`doCheck = false`) because they need PTYs and a writable `$HOME`, and that CI
runs the full suite upstream.

## Keeping it updated after each release

nixpkgs does **not** auto-track your releases. Once luvus is merged, every new
version needs its nixpkgs entry bumped (version + both hashes recomputed) and an
update PR (`luvus: 0.9.5 -> 0.9.6`). There is a script for exactly this.

### The script (one command)

After `scripts/release.sh` publishes a new tag, run:

```sh
# needs a nixpkgs checkout that already has luvus, plus `nix` + `nix-update`
# (nix profile install nixpkgs#nix-update), and `gh` for --pr.
LUVUS_NIXPKGS_DIR=~/nixpkgs scripts/nixpkgs-update.sh          # newest tag
LUVUS_NIXPKGS_DIR=~/nixpkgs scripts/nixpkgs-update.sh 0.9.6    # a specific version
scripts/nixpkgs-update.sh 0.9.6 --nixpkgs ~/nixpkgs --pr       # bump + build + open the PR
```

It branches off master in your nixpkgs checkout, runs `nix-update` to rewrite the
version and both hashes from the new tag, `nix-build`s and smoke-tests the binary,
commits `luvus: <old> -> <new>`, and (with `--pr`) pushes and opens the PR. The
`release.sh` "Done" banner prints this command as a reminder.

### By hand (if you skip the script)

From your nixpkgs checkout: `nix-update luvus --version 0.9.6`, then `nix-build -A
luvus`, commit `luvus: 0.9.5 -> 0.9.6`, and open the PR. Or set both hashes back
to `lib.fakeHash`, `nix-build -A luvus` twice, and paste each `got: sha256-…` in
(the same loop as the initial submission above).

### The other paths

The nixpkgs `r-ryantm` auto-update bot may also open version-bump PRs on its own,
but it lags your tags. And your `flake.nix` stays the always-current option for
Nix users who add it as an input, independent of nixpkgs entirely.
