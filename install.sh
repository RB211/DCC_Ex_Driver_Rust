#!/usr/bin/env bash
# Installer for the DCC-EX throttle (Rust version).
#
#   ./install.sh            build a release binary and install it
#   ./install.sh uninstall  remove the installed binary and desktop entry
#
# Installs to ~/.local/bin and ~/.local/share/applications, the same
# locations the old Python (PyInstaller) version used. The config file
# ~/.config/dccex-throttle.json is shared and never touched.
set -euo pipefail

BIN_NAME=dccex-throttle
BIN_DIR="$HOME/.local/bin"
DESKTOP_DIR="$HOME/.local/share/applications"
DESKTOP_FILE="$DESKTOP_DIR/$BIN_NAME.desktop"

if [[ "${1:-}" == "uninstall" ]]; then
    rm -f "$BIN_DIR/$BIN_NAME" "$DESKTOP_FILE"
    command -v update-desktop-database >/dev/null && update-desktop-database "$DESKTOP_DIR" || true
    echo "Uninstalled $BIN_NAME."
    exit 0
fi

cd "$(dirname "$0")"

echo "Building release binary..."
cargo build --release

# The binary knows its own version (--version); report what we install
# so "did the install take?" is answerable at a glance.
VERSION="$("target/release/$BIN_NAME" --version)"

mkdir -p "$BIN_DIR" "$DESKTOP_DIR"
install -m 755 "target/release/$BIN_NAME" "$BIN_DIR/$BIN_NAME"

cat > "$DESKTOP_FILE" <<EOF
[Desktop Entry]
Type=Application
Name=DCC-EX Throttle
Comment=Native-protocol throttle for a DCC-EX EX-CommandStation
Exec=$BIN_DIR/$BIN_NAME
Terminal=false
Categories=Utility;
EOF

command -v update-desktop-database >/dev/null && update-desktop-database "$DESKTOP_DIR" || true

echo "Installed $VERSION to $BIN_DIR/$BIN_NAME (desktop entry: $DESKTOP_FILE)."
