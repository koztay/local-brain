#!/bin/sh
# local-brain installer: downloads a release binary when one exists for this platform,
# otherwise builds from source with cargo.
#
#   curl -fsSL https://raw.githubusercontent.com/koztay/local-brain/main/install.sh | sh
#
# Environment: LOCAL_BRAIN_VERSION (default: latest), LOCAL_BRAIN_INSTALL_DIR (default: ~/.local/bin)
set -eu

REPO="koztay/local-brain"
VERSION="${LOCAL_BRAIN_VERSION:-latest}"
INSTALL_DIR="${LOCAL_BRAIN_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "local-brain: $*"; }
fail() { say "error: $*" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Darwin) os_part="apple-darwin" ;;
  Linux) os_part="unknown-linux-gnu" ;;
  *) os_part="" ;;
esac
case "$arch" in
  x86_64|amd64) arch_part="x86_64" ;;
  arm64|aarch64) arch_part="aarch64" ;;
  *) arch_part="" ;;
esac

download() {
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then wget -q "$1" -O "$2"
  else return 1; fi
}

try_release() {
  [ -n "$os_part" ] && [ -n "$arch_part" ] || return 1
  target="${arch_part}-${os_part}"
  if [ "$VERSION" = "latest" ]; then
    url="https://github.com/$REPO/releases/latest/download/local-brain-$target.tar.gz"
  else
    url="https://github.com/$REPO/releases/download/$VERSION/local-brain-$target.tar.gz"
  fi
  tmp=$(mktemp -d)
  say "downloading $url"
  download "$url" "$tmp/lb.tar.gz" 2>/dev/null || { rm -rf "$tmp"; return 1; }
  # Verify the checksum published with the release.
  if download "$(dirname "$url")/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null; then
    expected=$(grep "local-brain-$target.tar.gz" "$tmp/SHA256SUMS" | awk '{print $1}')
    if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp/lb.tar.gz" | awk '{print $1}')
    else actual=$(shasum -a 256 "$tmp/lb.tar.gz" | awk '{print $1}'); fi
    if [ -n "$expected" ] && [ "$expected" != "$actual" ]; then
      rm -rf "$tmp"; fail "checksum mismatch for local-brain-$target.tar.gz"
    fi
    say "checksum verified"
  fi
  tar -xzf "$tmp/lb.tar.gz" -C "$tmp" || { rm -rf "$tmp"; return 1; }
  mkdir -p "$INSTALL_DIR"
  mv "$tmp/local-brain" "$INSTALL_DIR/local-brain"
  chmod +x "$INSTALL_DIR/local-brain"
  # Downloaded binaries are not notarized; clear the quarantine flag so macOS runs it.
  if [ "$os" = "Darwin" ] && command -v xattr >/dev/null 2>&1; then
    xattr -d com.apple.quarantine "$INSTALL_DIR/local-brain" 2>/dev/null || true
  fi
  rm -rf "$tmp"
  return 0
}

build_from_source() {
  command -v cargo >/dev/null 2>&1 || fail "no release binary for this platform and cargo is not installed. Install Rust from https://rustup.rs and re-run."
  say "building from source with cargo (this takes a few minutes)…"
  if [ "$VERSION" = "latest" ]; then
    cargo install --git "https://github.com/$REPO" --locked --root "$INSTALL_DIR/.." --force
  else
    cargo install --git "https://github.com/$REPO" --tag "$VERSION" --locked --root "$INSTALL_DIR/.." --force
  fi
}

if try_release; then
  say "installed $INSTALL_DIR/local-brain"
else
  say "no prebuilt binary available; falling back to a source build"
  build_from_source
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say "add $INSTALL_DIR to your PATH, e.g.: echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc" ;;
esac
say "done. Next: cd your-project && local-brain init && local-brain learn"
