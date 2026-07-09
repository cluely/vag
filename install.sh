#!/bin/sh
# vag installer — https://github.com/OWNER/vag
#
#   curl -fsSL https://raw.githubusercontent.com/OWNER/vag/main/install.sh | sh
#
# Downloads the latest release binary for your platform, or builds from
# source with cargo when no prebuilt binary matches. Installs to
# /usr/local/bin (or ~/.local/bin without write access; override with
# VAG_INSTALL_DIR).
#
# NOTE: OWNER is a placeholder until the repository is published.
set -eu

REPO="${VAG_REPO:-OWNER/vag}"
INSTALL_DIR="${VAG_INSTALL_DIR:-}"

say()  { printf '\033[1;36mvag\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31mvag\033[0m %s\n' "$*" >&2; exit 1; }

# ---- pick install dir -------------------------------------------------------
if [ -z "$INSTALL_DIR" ]; then
    if [ -w /usr/local/bin ]; then
        INSTALL_DIR=/usr/local/bin
    else
        INSTALL_DIR="$HOME/.local/bin"
    fi
fi
mkdir -p "$INSTALL_DIR"

# ---- platform ---------------------------------------------------------------
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
    Darwin) os=apple-darwin ;;
    Linux)  os=unknown-linux-gnu ;;
    *)      fail "unsupported OS: $OS (build from source: cargo install --git https://github.com/$REPO)" ;;
esac
case "$ARCH" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64|amd64)  arch=x86_64 ;;
    *)             fail "unsupported architecture: $ARCH" ;;
esac
TARGET="$arch-$os"

# ---- try a prebuilt release -------------------------------------------------
LATEST_URL="https://github.com/$REPO/releases/latest/download/vag-$TARGET.tar.gz"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say "fetching latest release for $TARGET…"
if curl -fsSL "$LATEST_URL" -o "$TMP/vag.tar.gz" 2>/dev/null; then
    tar -xzf "$TMP/vag.tar.gz" -C "$TMP"
    install -m 755 "$TMP/vag" "$INSTALL_DIR/vag"
    say "installed $("$INSTALL_DIR/vag" --version) → $INSTALL_DIR/vag"
else
    say "no prebuilt binary available — building from source"
    command -v cargo >/dev/null 2>&1 \
        || fail "cargo not found. Install Rust first: https://rustup.rs"
    cargo install --git "https://github.com/$REPO" --root "$TMP/cargo" vag
    install -m 755 "$TMP/cargo/bin/vag" "$INSTALL_DIR/vag"
    say "built and installed $("$INSTALL_DIR/vag" --version) → $INSTALL_DIR/vag"
fi

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) say "note: $INSTALL_DIR is not on your PATH — add it to your shell profile" ;;
esac

say "run \`vag doctor\` to verify your claude/codex setup, then \`vag\` to start"
