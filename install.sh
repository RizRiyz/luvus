#!/bin/sh
# luvus installer — downloads the right prebuilt binary for your OS/arch from the
# GitHub releases and drops it on your PATH.
#
#   curl -fsSL https://luvus.dev/install.sh | sh
#
# Overrides:
#   LUVUS_VERSION=v0.1.0   install a specific tag (default: latest release)
#   LUVUS_INSTALL_DIR=...  where to put the binary (default: /usr/local/bin or ~/.local/bin)
set -eu

REPO="RizRiyz/luvus"
BIN="luvus"

err() { printf 'error: %s\n' "$1" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# ── pick a downloader ──
if have curl; then DL="curl -fsSL"; DLO="curl -fsSL -o"
elif have wget; then DL="wget -qO-"; DLO="wget -qO"
else err "need curl or wget"; fi

# ── detect target triple ──
os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Darwin)
    case "$arch" in
      x86_64) target="x86_64-apple-darwin" ;;
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      *) err "unsupported macOS arch: $arch" ;;
    esac ;;
  Linux)
    case "$arch" in
      x86_64) target="x86_64-unknown-linux-musl" ;;
      aarch64|arm64) target="aarch64-unknown-linux-musl" ;;
      *) err "unsupported Linux arch: $arch" ;;
    esac ;;
  *) err "unsupported OS: $os (on Windows, download the .zip from the releases page)" ;;
esac

# ── resolve version ──
if [ -z "${LUVUS_VERSION:-}" ] && [ -n "${BOHAY_VERSION:-}" ]; then
  LUVUS_VERSION=$BOHAY_VERSION
fi
if [ -n "${LUVUS_VERSION:-}" ]; then
  tag="$LUVUS_VERSION"
else
  tag=$($DL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep '"tag_name"' | head -1 | cut -d'"' -f4)
  [ -n "$tag" ] || err "could not find the latest release (set LUVUS_VERSION=vX.Y.Z)"
fi

asset="$BIN-$tag-$target.tar.gz"
url="https://github.com/$REPO/releases/download/$tag/$asset"
printf 'Installing %s %s (%s)…\n' "$BIN" "$tag" "$target"

# ── download + extract ──
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
$DLO "$tmp/$asset" "$url" || err "download failed: $url"
tar -xzf "$tmp/$asset" -C "$tmp" || err "extract failed"
[ -f "$tmp/$BIN" ] || err "archive did not contain '$BIN'"
chmod +x "$tmp/$BIN"

# ── choose an install dir on PATH ──
if [ -z "${LUVUS_INSTALL_DIR:-}" ] && [ -n "${BOHAY_INSTALL_DIR:-}" ]; then
  LUVUS_INSTALL_DIR=$BOHAY_INSTALL_DIR
fi
if [ -n "${LUVUS_INSTALL_DIR:-}" ]; then
  dir="$LUVUS_INSTALL_DIR"
elif [ -w /usr/local/bin ]; then
  dir="/usr/local/bin"
else
  dir="$HOME/.local/bin"
fi
mkdir -p "$dir"

if mv "$tmp/$BIN" "$dir/$BIN" 2>/dev/null; then :;
elif have sudo; then
  printf 'Writing to %s (needs sudo)…\n' "$dir"
  sudo mv "$tmp/$BIN" "$dir/$BIN"
else
  err "cannot write to $dir (set LUVUS_INSTALL_DIR to a writable dir)"
fi

printf '\n✓ installed to %s/%s\n' "$dir" "$BIN"
case ":$PATH:" in
  *":$dir:"*) printf 'Run: %s\n' "$BIN" ;;
  *) printf 'Add to PATH:  export PATH="%s:$PATH"\n' "$dir" ;;
esac
