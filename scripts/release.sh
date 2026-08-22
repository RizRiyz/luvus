#!/usr/bin/env bash
#
# release.sh — cut a new luvus version to crates.io, GitHub (binaries), and Homebrew.
#
#   scripts/release.sh 0.1.1             # full release (prompts before publishing)
#   scripts/release.sh 0.1.1 --dry-run   # bump + verify only, then revert — no release
#   scripts/release.sh 0.1.1 --yes       # skip the confirmation prompt
#   scripts/release.sh 0.1.1 --no-cargo-publish  # full release flow, skip `cargo publish`
#
# Prereqs:  `cargo login` done · `gh auth login` · push access to the repo.
# Tap:      the Homebrew formula in ./homebrew-luvus (or $LUVUS_TAP_DIR) — the real
#           `brew install RizRiyz/luvus/luvus` source — is bumped & pushed too.
set -euo pipefail

REPO="RizRiyz/luvus"

die()  { printf '\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }
step() { printf '\n\033[36m▸ %s\033[0m\n' "$1"; }
sha256() { if command -v shasum >/dev/null; then shasum -a 256; else sha256sum; fi | cut -d' ' -f1; }

# The terminal memory patches are separate crates because crates.io removes
# `[patch]` tables and local paths from published manifests. Development uses
# the checked-in paths; published Luvus releases resolve these exact versions.
wait_for_crate() {
  local package="$1" version="$2" waited=0
  while [ "$waited" -lt 300 ]; do
    cargo info "$package@$version" >/dev/null 2>&1 && return 0
    printf '  waiting for %s %s in the crates.io index… (%ss)\r' "$package" "$version" "$waited"
    sleep 5
    waited=$((waited + 5))
  done
  die "$package $version did not appear in the crates.io index"
}

publish_support_crate() {
  local package="$1" version="$2" manifest="$3"
  if cargo info "$package@$version" >/dev/null 2>&1; then
    echo "  $package $version already published"
    return 0
  fi
  cargo publish --manifest-path "$manifest"
  wait_for_crate "$package" "$version"
}
# Rewrite the formula in place for $TAG. The formula ships **prebuilt binaries**
# (one url+sha256 per platform) plus a source fallback for Intel macs, so this
# has to bump the version, every url, and every checksum — each from that
# platform's published `.sha256` asset. $SHA (the source tarball's checksum) is
# set before calling.
#
# Ordering matters: the binary assets only exist once the release workflow has
# built them, so `wait_for_assets` runs first.
FORMULA_TARGETS="aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl"

# Block until every prebuilt asset for $TAG is published (the workflow builds
# them after the tag push). Gives up after ~10 minutes rather than hanging.
wait_for_assets() {
  local waited=0
  while [ "$waited" -lt 600 ]; do
    local missing=0
    for t in $FORMULA_TARGETS; do
      gh release view "$TAG" --repo "$REPO" --json assets \
        --jq ".assets[].name" 2>/dev/null | grep -qx "luvus-$TAG-$t.sha256" || missing=1
    done
    [ "$missing" = 0 ] && return 0
    printf '  waiting for release binaries… (%ss)\r' "$waited"
    sleep 15
    waited=$((waited + 15))
  done
  die "release assets for $TAG never appeared — bump the tap by hand once the workflow finishes"
}

# The published checksum for one target.
asset_sha() {
  gh release download "$TAG" --repo "$REPO" --pattern "luvus-$TAG-$1.sha256" -O - 2>/dev/null \
    | awk '{print $1}'
}

bump_formula() {
  local f="$1" t sha
  # version + the Intel-mac source fallback
  perl -0pi -e "s/^  version \"[0-9.]+\"/  version \"$VERSION\"/m" "$f"
  perl -0pi -e "s{archive/refs/tags/v[0-9.]+\.tar\.gz}{archive/refs/tags/$TAG.tar.gz}g" "$f"
  # Each prebuilt: rewrite its url to $TAG, then the sha256 on the line after it.
  for t in $FORMULA_TARGETS; do
    sha="$(asset_sha "$t")"
    [ -n "$sha" ] || die "no published checksum for $t — cannot bump the formula"
    perl -0pi -e "s{releases/download/v[0-9.]+/luvus-v[0-9.]+-$t\.tar\.gz}{releases/download/$TAG/luvus-$TAG-$t.tar.gz}g" "$f"
    perl -0pi -e "s{(luvus-$TAG-$t\.tar\.gz\"\n\s*sha256 \")[0-9a-f]{64}}{\${1}$sha}s" "$f"
  done
  # The source fallback's checksum is the last one still on the old value.
  perl -0pi -e "s{(archive/refs/tags/$TAG\.tar\.gz\"\n\s*sha256 \")[0-9a-f]{64}}{\${1}$SHA}s" "$f"
  # Nothing may still point at an older tag.
  ! grep -qE "v[0-9]+\.[0-9]+\.[0-9]+" "$f" || grep -qE "$TAG" "$f" \
    || die "formula still references an old tag after the bump"
}

VERSION="${1:-}"
MODE=""
SKIP_CARGO_PUBLISH=0
[ $# -ge 1 ] || die "usage: scripts/release.sh X.Y.Z [--dry-run|--yes|--no-cargo-publish]"

shift
for arg in "$@"; do
  case "$arg" in
    --dry-run)
      [ -n "$MODE" ] && die "only one of --dry-run or --yes is allowed"
      MODE="--dry-run"
      ;;
    --yes)
      [ -n "$MODE" ] && die "only one of --dry-run or --yes is allowed"
      MODE="--yes"
      ;;
    --no-cargo-publish|--skip-cargo-publish|--no-publish)
      SKIP_CARGO_PUBLISH=1
      ;;
    *)
      die "usage: scripts/release.sh X.Y.Z [--dry-run|--yes] [--no-cargo-publish]"
      ;;
  esac
done
[ -n "$VERSION" ] || die "usage: scripts/release.sh X.Y.Z [--dry-run|--yes]"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must be semver X.Y.Z (got '$VERSION')"
TAG="v$VERSION"
cd "$(git rev-parse --show-toplevel)"
RELEASE_ROOT="$(pwd)"
VTE_MANIFEST="vendor/vte/Cargo.toml"
ALACRITTY_MANIFEST="vendor/alacritty_terminal/Cargo.toml"
VTE_VERSION="$(sed -n -E 's/^version = "([^"]+)"/\1/p' "$VTE_MANIFEST" | head -1)"
ALACRITTY_VERSION="$(sed -n -E 's/^version = "([^"]+)"/\1/p' "$ALACRITTY_MANIFEST" | head -1)"
LOCAL_VTE_PATCH="patch.crates-io.luvus-vte.path=\"$RELEASE_ROOT/vendor/vte\""
LOCAL_ALACRITTY_PATCH="patch.crates-io.luvus-alacritty-terminal.path=\"$RELEASE_ROOT/vendor/alacritty_terminal\""

# Self-heal: if we bail out (failed check, abort, dry-run) before the release is
# committed, undo the version bump so the tree is never left half-updated.
committed=0
trap '[ "$committed" = 1 ] || git checkout -- Cargo.toml Cargo.lock nix/package.nix 2>/dev/null || true' EXIT

step "Preconditions"
[ "$(git branch --show-current)" = "main" ] || die "not on main"
# The release commit below includes this tag's changelog note, so a modified or
# untracked `changelog/<tag>.md` is allowed here; anything else dirty blocks the
# release (commit or stash it first).
DIRTY="$(git status --porcelain | grep -v -e "changelog/$TAG.md\$" || true)"
[ -z "$DIRTY" ] || die "working tree is dirty (besides the changelog) — commit or stash first"
git fetch --tags --quiet
git rev-parse "$TAG" >/dev/null 2>&1 && die "$TAG already exists"
CURRENT=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
echo "  $CURRENT  →  $VERSION"
# `changelog/<tag>.md` is the single source the GitHub Release *and* luvus.dev
# both render, so it has to exist and be committed before we tag. Generate the
# skeleton and stop, rather than shipping a release with auto-listed commits.
CHANGELOG="changelog/$TAG.md"
if [ ! -f "$CHANGELOG" ]; then
  bash scripts/changelog.sh "$TAG" --write
  die "wrote $CHANGELOG — edit it, commit it, then re-run this script"
fi
if grep -q 'Then delete this note' "$CHANGELOG"; then
  die "$CHANGELOG still has the placeholder note — write the summary, commit, then re-run"
fi
echo "  notes: $CHANGELOG"
# The Homebrew tap (its own git repo): the in-repo clone by default.
TAP="${LUVUS_TAP_DIR:-homebrew-luvus}"
if [ -f "$TAP/Formula/luvus.rb" ]; then
  [ -z "$(git -C "$TAP" status --porcelain)" ] || die "tap '$TAP' has uncommitted changes"
  echo "  tap: $TAP  (will bump + push)"
else
  echo "  tap: none at '$TAP' — Homebrew step will print manual instructions"
fi

step "Bump Cargo.toml + Cargo.lock"
# Only the [package] version is at the start of a line; deps use `name = "..."`.
perl -0pi -e "s/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/version = \"$VERSION\"/m" Cargo.toml
cargo check --quiet                       # syncs Cargo.lock's luvus version
grep -q "^version = \"$VERSION\"" Cargo.toml || die "Cargo.toml bump failed"

# Keep the nixpkgs package definition in step with the release: bump its version,
# and reset its source/cargo hashes to a placeholder (both are version-specific).
# Whoever cuts the nixpkgs PR recomputes them from the pushed tag (see
# nix/README.md); the release just guarantees the version never goes stale.
if [ -f nix/package.nix ]; then
  perl -0pi -e "s/^  version = \"[0-9]+\.[0-9]+\.[0-9]+\";/  version = \"$VERSION\";/m" nix/package.nix
  perl -0pi -e 's/^(\s*(?:cargoHash|hash)) = "sha256-[^"]*";/$1 = lib.fakeHash;/gm' nix/package.nix
  grep -q "  version = \"$VERSION\";" nix/package.nix || die "nix/package.nix bump failed"
fi

step "Verify (fmt · clippy · test · publish dry-run)"
cargo fmt --package luvus --check
cargo clippy --all-targets -- -D warnings
# Never let a test consume the release operator's terminal. Interactive
# compatibility paths must fail closed instead of blocking the entire release.
cargo test --locked </dev/null
# --allow-dirty: the version bump isn't committed yet at this point. This is only
# a build/package check; the REAL `cargo publish` below runs after the commit on a
# clean tree, so the published artifact still matches a committed state.
cargo publish --dry-run --allow-dirty --manifest-path "$VTE_MANIFEST"
cargo publish --dry-run --allow-dirty --manifest-path "$ALACRITTY_MANIFEST" \
  --config "$LOCAL_VTE_PATCH"
cargo publish --dry-run --allow-dirty \
  --config "$LOCAL_VTE_PATCH" \
  --config "$LOCAL_ALACRITTY_PATCH"

step "Release notes preview (what the workflow will publish on the GitHub Release)"
bash scripts/changelog.sh "$TAG"

if [ "$MODE" = "--dry-run" ]; then
  step "Dry run OK — everything passed. Re-run without --dry-run to release."
  exit 0 # the trap reverts the bump
fi

if [ "$MODE" != "--yes" ]; then
  printf "\nRelease \033[1m%s\033[0m to crates.io + GitHub + Homebrew. Continue? [y/N] " "$TAG"
  read -r ans
  [ "$ans" = "y" ] || [ "$ans" = "Y" ] || die "aborted" # the trap reverts the bump
fi

step "Commit + tag"
# The changelog note ships in the same release commit as the version bump, so a
# tag always has its notes (the GitHub Release + luvus.dev render from it).
git add Cargo.toml Cargo.lock "$CHANGELOG"
[ -f nix/package.nix ] && git add nix/package.nix
git commit -m "release: $TAG"
committed=1 # past here the bump is committed — the trap must not revert it
git tag -a "$TAG" -m "$TAG"

step "Push (triggers the release workflow → binaries)"
git push origin main
git push origin "$TAG"

step "Publish terminal support crates to crates.io"
if [ "$SKIP_CARGO_PUBLISH" = "1" ]; then
  echo "  skipping cargo publish by request"
else
  publish_support_crate "luvus-vte" "$VTE_VERSION" "$VTE_MANIFEST"
  publish_support_crate "luvus-alacritty-terminal" "$ALACRITTY_VERSION" "$ALACRITTY_MANIFEST"

  step "Publish Luvus to crates.io"
  cargo publish
fi

step "Homebrew formula (source tarball is ready the instant the tag is pushed)"
TARBALL="https://github.com/$REPO/archive/refs/tags/$TAG.tar.gz"
SHA=$(curl -fsSL --retry 5 --retry-delay 2 "$TARBALL" | sha256)
[ -n "$SHA" ] || die "could not fetch + hash $TARBALL"
echo "  sha256: $SHA"

# The tap (its own repo) is the single source of truth — `brew install` pulls it.

if [ -f "$TAP/Formula/luvus.rb" ]; then
  step "Update tap ($TAP)"
  wait_for_assets
  bump_formula "$TAP/Formula/luvus.rb"
  git -C "$TAP" add Formula/luvus.rb
  git -C "$TAP" commit -m "luvus $TAG"
  git -C "$TAP" push
  echo "  ✓ tap pushed — brew install $REPO/luvus now serves $TAG"
else
  step "Tap '$TAP' not found — finish Homebrew by hand:"
  echo "    git clone git@github.com:${REPO%%/*}/homebrew-luvus.git"
  echo "    # in it: set url → .../$TAG.tar.gz and sha256 → $SHA, then commit & push"
fi

step "Done — $TAG released 🎉"
echo "  cargo:    cargo install luvus"
echo "  binaries: https://github.com/$REPO/releases/tag/$TAG  (workflow building now)"
echo "  brew:     brew install $REPO/luvus"
echo "  nixpkgs:  scripts/nixpkgs-update.sh $VERSION --pr   (once luvus is in nixpkgs)"
