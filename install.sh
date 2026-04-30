#!/usr/bin/env sh
# Install the latest nightly `fai` binary from
# https://github.com/forailang/forai/releases/tag/nightly.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/forailang/forai/main/install.sh | sh
#
# Environment overrides:
#   INSTALL_DIR  Where to drop the binary (default: ~/.local/bin)
#   FAI_TAG      Release tag to install (default: nightly)
#   REPO         GitHub repo (default: forailang/forai)

set -eu

REPO="${REPO:-forailang/forai}"
TAG="${FAI_TAG:-nightly}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

err() { printf 'install.sh: error: %s\n' "$1" >&2; exit 1; }
info() { printf 'install.sh: %s\n' "$1"; }

uname_s=$(uname -s)
uname_m=$(uname -m)

case "$uname_s" in
    Linux)  os=linux ;;
    Darwin) os=darwin ;;
    *) err "unsupported OS: $uname_s (only linux and macOS are published)" ;;
esac

case "$uname_m" in
    x86_64|amd64)  arch=x86_64 ;;
    arm64|aarch64) arch=aarch64 ;;
    *) err "unsupported architecture: $uname_m" ;;
esac

# Only Apple Silicon Macs are published; Intel Macs aren't in the build matrix.
if [ "$os" = "darwin" ] && [ "$arch" = "x86_64" ]; then
    err "Intel Macs are not supported — only Apple Silicon (arm64) Darwin builds are published"
fi

asset="fai-${os}-${arch}.tar.gz"
url="https://github.com/${REPO}/releases/download/${TAG}/${asset}"
sha_url="${url}.sha256"

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q "$1" -O "$2"; }
else
    err "need curl or wget on PATH"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

info "downloading $asset from $TAG"
fetch "$url" "$tmp/$asset" || err "download failed: $url"

# Verify checksum when the .sha256 sidecar is published.
if fetch "$sha_url" "$tmp/$asset.sha256" 2>/dev/null; then
    info "verifying sha256"
    cd "$tmp"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$asset.sha256" >/dev/null || err "sha256 mismatch"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "$asset.sha256" >/dev/null || err "sha256 mismatch"
    else
        info "warning: no sha256 tool found; skipping verification"
    fi
    cd - >/dev/null
else
    info "warning: no sha256 sidecar; skipping verification"
fi

info "extracting"
tar -xzf "$tmp/$asset" -C "$tmp"
[ -f "$tmp/fai" ] || err "archive did not contain fai binary"

mkdir -p "$INSTALL_DIR"
mv "$tmp/fai" "$INSTALL_DIR/fai"
chmod +x "$INSTALL_DIR/fai"

info "installed to $INSTALL_DIR/fai"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) info "note: $INSTALL_DIR is not on your PATH — add it to your shell rc:"
       printf '    export PATH="%s:$PATH"\n' "$INSTALL_DIR"
       ;;
esac

[ -x "$INSTALL_DIR/fai" ] || err "binary at $INSTALL_DIR/fai is not executable"
info "ready: run \`fai\` (or \`$INSTALL_DIR/fai\` if not on PATH)"
