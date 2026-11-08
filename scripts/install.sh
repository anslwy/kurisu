#!/bin/sh
# Kurisu installer: downloads the prebuilt `ku` for this OS/CPU from a GitHub
# release, checks its SHA-256, and installs it into ~/.local/bin (no sudo).
#   curl -fsSL https://raw.githubusercontent.com/anslwy/kurisu/main/scripts/install.sh | sh
# Options (environment): KURISU_VERSION=v0.1.0 (default: latest), KURISU_BIN_DIR=~/.local/bin
# Prefer building it yourself? See "Build from source" in the README; every binary
# also knows its commit and can rebuild itself (`ku source`).
set -eu
REPO="anslwy/kurisu"
BIN_DIR="${KURISU_BIN_DIR:-$HOME/.local/bin}"
VERSION="${KURISU_VERSION:-latest}"

for cmd in curl tar install cut mktemp; do
  command -v "$cmd" >/dev/null 2>&1 || {
    echo "kurisu: missing required command: $cmd (on Debian/Ubuntu: apt-get install curl ca-certificates coreutils tar)" >&2
    exit 1
  }
done

os=$(uname -s); arch=$(uname -m)
case "$os/$arch" in
  Linux/x86_64)          target=x86_64-unknown-linux-musl ;;
  Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-musl ;;
  Darwin/arm64)          target=aarch64-apple-darwin ;;
  Darwin/x86_64)         target=x86_64-apple-darwin ;;
  *) echo "kurisu: no prebuilt binary for $os/$arch yet; build from source (README)" >&2; exit 1 ;;
esac

if [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi
asset="kurisu-$target.tar.gz"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
echo "downloading $asset ..."
if ! curl -fsSL "$base/$asset" -o "$tmp/$asset"; then
  echo "kurisu: could not download $asset (release: $VERSION). Prebuilt releases may not be published yet." >&2
  echo "Build from source: https://github.com/$REPO#install (includes Rust setup)." >&2
  exit 1
fi
curl -fsSL "$base/$asset.sha256" -o "$tmp/$asset.sha256"
want=$(cut -d' ' -f1 < "$tmp/$asset.sha256")
if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1); else got=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1); fi
[ "$want" = "$got" ] || { echo "kurisu: checksum mismatch, not installing" >&2; exit 1; }
tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$BIN_DIR"
install -m 755 "$tmp/ku" "$BIN_DIR/ku"
ln -sf ku "$BIN_DIR/kurisu"
echo "installed $BIN_DIR/ku ($("$BIN_DIR/ku" --version | head -1))"
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "add $BIN_DIR to your PATH" ;; esac
command -v tmux >/dev/null 2>&1 || echo "note: Kurisu needs tmux (apt install tmux / brew install tmux)"
echo "next: install/configure Claude Code, Codex, or OpenCode (docs/opencode.md), then: ku doctor"
