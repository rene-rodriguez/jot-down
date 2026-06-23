#!/usr/bin/env bash
set -euo pipefail

BINARY="jot-down"
VERSION=$(grep '^version' "$(dirname "$0")/../Cargo.toml" | head -1 | sed 's/.*= *"\(.*\)"/\1/')

# Determine install prefix
if [[ $# -ge 1 ]]; then
    PREFIX="$1"
elif [[ -w /usr/local/bin ]]; then
    PREFIX="/usr/local"
else
    PREFIX="$HOME/.local"
fi

BINDIR="$PREFIX/bin"

echo "Installing $BINARY $VERSION -> $BINDIR/$BINARY"

# Verify cargo is available
if ! command -v cargo &>/dev/null; then
    echo "error: cargo not found. Install Rust from https://rustup.rs" >&2
    exit 1
fi

# Build
echo "Building release binary..."
cargo build --release 2>&1

# Install
mkdir -p "$BINDIR"
install -m 755 "$(dirname "$0")/../target/release/$BINARY" "$BINDIR/$BINARY"

echo "Done. $BINARY installed to $BINDIR/$BINARY"

# PATH hint when installing to ~/.local/bin
if [[ "$BINDIR" == "$HOME/.local/bin" && ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    echo ""
    echo "Add $HOME/.local/bin to your PATH:"
    echo '  export PATH="$HOME/.local/bin:$PATH"'
fi
