#!/usr/bin/env bash
set -euo pipefail

# ── Phone Link Installer ─────────────────────────────────────────────
# Installs system dependencies and sets up Phone Link for Linux.
# ────────────────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
APPS_DIR="$HOME/.local/share/applications"
ICONS_DIR="$HOME/.local/share/icons/hicolor"

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'
info() { echo -e "${GREEN}[INFO]${NC} $*"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }

# ── 1. System packages ───────────────────────────────────────────────
info "Installing system dependencies..."
sudo apt-get update -qq
sudo apt-get install -y \
    kdeconnect \
    python3-gi \
    python3-gi-cairo \
    gir1.2-gtk-4.0 \
    gir1.2-adw-1 \
    gir1.2-xapp-1.0 \
    hicolor-icon-theme

# ── 2. Verify KDE Connect daemon ─────────────────────────────────────
if ! command -v kdeconnect-cli >/dev/null 2>&1; then
    warn "kdeconnect-cli not found after install — try restarting your session."
else
    info "KDE Connect installed. Pair your phone before launching Phone Link."
fi

# ── 3. Launcher script ───────────────────────────────────────────────
info "Installing launcher..."
mkdir -p "$BIN_DIR"
cat > "$BIN_DIR/phonelink" << EOF
#!/usr/bin/env bash
exec python3 "$SCRIPT_DIR/run.py" "\$@"
EOF
chmod +x "$BIN_DIR/phonelink"

# ── 4. PATH fix ──────────────────────────────────────────────────────
if [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
    info "Adding $BIN_DIR to PATH in ~/.bashrc..."
    echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$HOME/.bashrc"
    export PATH="$BIN_DIR:$PATH"
fi

# ── 5. Desktop entry and icons ───────────────────────────────────────
info "Installing desktop entry and icons..."
mkdir -p "$APPS_DIR" "$ICONS_DIR/scalable/apps"

sed "s|Exec=.*|Exec=$BIN_DIR/phonelink|" \
    "$SCRIPT_DIR/data/phonelink.desktop" > "$APPS_DIR/phonelink.desktop"

cp "$SCRIPT_DIR/data/icons/phonelink.svg" "$ICONS_DIR/scalable/apps/phonelink.svg"

for size in 48 64 128 256; do
    png="$SCRIPT_DIR/data/icons/phonelink-${size}.png"
    if [[ -f "$png" ]]; then
        mkdir -p "$ICONS_DIR/${size}x${size}/apps"
        cp "$png" "$ICONS_DIR/${size}x${size}/apps/phonelink.png"
    fi
done

update-desktop-database "$APPS_DIR" 2>/dev/null || true
gtk-update-icon-cache -f -t "$ICONS_DIR" 2>/dev/null || true

# ── Done ─────────────────────────────────────────────────────────────
echo ""
info "============================================"
info " Phone Link installed successfully!"
info "============================================"
echo ""
echo "  Launch from your app menu, or run:"
echo "    phonelink"
echo ""
echo "  Before launching, make sure:"
echo "    1. KDE Connect is installed on your Android phone"
echo "    2. Your phone and PC are on the same Wi-Fi network"
echo "    3. Pair from the KDE Connect Android app"
echo ""
echo "  If Phone Link shows 'KDE Connect Not Found', start the daemon:"
echo "    kdeconnectd &"
echo ""
