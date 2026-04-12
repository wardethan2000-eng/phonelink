#!/bin/bash
# Install Phone Link desktop entry and icons for the current user.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
APP_DIR="$(dirname "$SCRIPT_DIR")"

echo "Installing Phone Link..."

# Update Exec path in .desktop file
sed "s|Exec=.*|Exec=/usr/bin/python3 ${APP_DIR}/run.py|" \
    "$SCRIPT_DIR/phonelink.desktop" > ~/.local/share/applications/phonelink.desktop

# Install icons
mkdir -p ~/.local/share/icons/hicolor/scalable/apps
cp "$SCRIPT_DIR/icons/phonelink.svg" ~/.local/share/icons/hicolor/scalable/apps/phonelink.svg

for size in 48 64 128 256; do
    png="$SCRIPT_DIR/icons/phonelink-${size}.png"
    if [ -f "$png" ]; then
        mkdir -p ~/.local/share/icons/hicolor/${size}x${size}/apps
        cp "$png" ~/.local/share/icons/hicolor/${size}x${size}/apps/phonelink.png
    fi
done

# Refresh caches
update-desktop-database ~/.local/share/applications/ 2>/dev/null || true
gtk-update-icon-cache ~/.local/share/icons/hicolor/ 2>/dev/null || true

echo "Done! Phone Link should now appear in your application menu."
echo "You may need to log out and back in for the icon to appear."
