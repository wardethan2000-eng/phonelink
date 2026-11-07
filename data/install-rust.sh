#!/bin/bash
# Install the Rust PhoneRelay for the current user (Phase 7 switch-over):
#   data/install-rust.sh path/to/phonelink-rs
# Puts the binary in ~/.local/bin, a desktop file and icons under the app id
# io.github.wardethan2000_eng.phonelink. It does not touch autostart: turn on
# "Open on Login" in the app's Settings, which replaces the Python entry.
# The Python app stays installed and runnable.
# If phonelink-gm/phonelink-gm is built (deleting chats and syncing read state
# with the phone through Google Messages; build it with `go build` in phonelink-gm/), it is installed
# next to the app.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
APP_ID="io.github.wardethan2000_eng.phonelink"
SRC="${1:?usage: $0 path/to/phonelink-rs}"
BIN="$HOME/.local/bin/phonelink-rs"

mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"
install -m 755 "$SRC" "$BIN.new" && mv -f "$BIN.new" "$BIN"
GM="$SCRIPT_DIR/../phonelink-gm/phonelink-gm"
if [ -f "$GM" ]; then
    install -m 755 "$GM" "$HOME/.local/bin/phonelink-gm.new" && mv -f "$HOME/.local/bin/phonelink-gm.new" "$HOME/.local/bin/phonelink-gm"
    echo "Installed $HOME/.local/bin/phonelink-gm (deletes and read state on the phone)."
fi

cat > "$HOME/.local/share/applications/$APP_ID.desktop" <<DESKTOP
[Desktop Entry]
Name=PhoneRelay
Comment=Connect your Android phone - SMS, notifications, and files
Exec="$BIN"
Icon=$APP_ID
Terminal=false
Type=Application
Categories=Utility;Communication;
Keywords=phone;phone link;sms;notifications;kdeconnect;android;
StartupWMClass=$APP_ID
DESKTOP

for size in 48 64 128 256 512; do
    png="$SCRIPT_DIR/icons/phonelink-${size}.png"
    [ -f "$png" ] || continue
    mkdir -p "$HOME/.local/share/icons/hicolor/${size}x${size}/apps"
    cp "$png" "$HOME/.local/share/icons/hicolor/${size}x${size}/apps/$APP_ID.png"
done

update-desktop-database "$HOME/.local/share/applications/" 2>/dev/null || true
gtk-update-icon-cache "$HOME/.local/share/icons/hicolor/" 2>/dev/null || true
echo "Installed $BIN and the \"PhoneRelay\" menu entry ($APP_ID)."
