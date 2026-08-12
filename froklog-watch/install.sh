#!/usr/bin/env bash
# Install froklog-watch for the current user.
#
# The binary alone is enough to run, but the dock will not know what icon to
# draw for it: it identifies a window by WM_CLASS and looks that up in the
# installed desktop entries. So the icon has to live in the icon theme and the
# desktop entry has to claim the window — that is what this does.
set -euo pipefail

cd "$(dirname "$0")"

BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
ICON_ROOT="$HOME/.local/share/icons/hicolor"

cargo build --release

install -Dm755 target/release/froklog-watch "$BIN_DIR/froklog-watch"

for size in 16 32 48 64 128 256; do
    install -Dm644 "assets/icons/froklog-watch-${size}.png" \
        "$ICON_ROOT/${size}x${size}/apps/froklog-watch.png"
done

install -Dm644 froklog-watch.desktop "$APP_DIR/froklog-watch.desktop"

# Caches are advisory — a missing tool is not a failed install.
command -v update-desktop-database >/dev/null && update-desktop-database "$APP_DIR" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qtf "$ICON_ROOT" 2>/dev/null || true

echo "installed:"
echo "  $BIN_DIR/froklog-watch"
echo "  $APP_DIR/froklog-watch.desktop"
echo "  $ICON_ROOT/*/apps/froklog-watch.png"
echo
echo "Make sure $BIN_DIR is on your PATH, then launch froklog-watch from your app menu."
