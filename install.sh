#!/usr/bin/env bash
set -euo pipefail

# ── Phone Link Installer ─────────────────────────────────────────────
# Installs system dependencies and sets up Phone Link for Linux.
# ────────────────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
APPS_DIR="$HOME/.local/share/applications"
ICONS_DIR="$HOME/.local/share/icons/hicolor"
DESKTOP_ID="dev.phonelink.app"

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info() { echo -e "${GREEN}[INFO]${NC} $*"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
status() { echo -e "${BLUE}[STATUS]${NC} $*"; }

# ── 1. Dependency Pre-check ──────────────────────────────────────────
check_dependencies() {
    if ! command -v kdeconnect-cli >/dev/null 2>&1; then
        return 1
    fi
    # Check GTK4 + Adw (Main App)
    if ! python3 -c "
import gi
try:
    gi.require_version('Gtk', '4.0')
    gi.require_version('Adw', '1')
    from gi.repository import Gtk, Gdk, Adw, GLib, Gio
except Exception:
    exit(1)
" >/dev/null 2>&1; then
        return 1
    fi
    # Check GTK3 + XApp (Tray App)
    if ! python3 -c "
import gi
try:
    gi.require_version('Gtk', '3.0')
    gi.require_version('XApp', '1.0')
    from gi.repository import Gtk, XApp
except Exception:
    exit(1)
" >/dev/null 2>&1; then
        return 1
    fi
    return 0
}

# ── 2. Distro Package Installation ──────────────────────────────────
install_system_dependencies() {
    if check_dependencies; then
        info "All system dependencies are already satisfied! Skipping package installation."
        return 0
    fi

    status "Checking system package manager..."
    if command -v apt-get >/dev/null 2>&1; then
        info "Debian/Ubuntu detected. Installing dependencies via apt..."
        sudo apt-get update -qq
        sudo apt-get install -y \
            kdeconnect \
            python3-gi \
            python3-gi-cairo \
            gir1.2-gtk-4.0 \
            gir1.2-adw-1 \
            gir1.2-xapp-1.0 \
            hicolor-icon-theme
    elif command -v dnf >/dev/null 2>&1; then
        info "Fedora detected. Installing dependencies via dnf..."
        sudo dnf install -y \
            kde-connect \
            python3-gobject \
            gtk4 \
            libadwaita \
            xapps \
            hicolor-icon-theme
    elif command -v pacman >/dev/null 2>&1; then
        info "Arch Linux detected. Installing dependencies via pacman..."
        sudo pacman -Syu --noconfirm \
            kdeconnect \
            python-gobject \
            gtk4 \
            libadwaita \
            xapp \
            hicolor-icon-theme
    elif command -v zypper >/dev/null 2>&1; then
        info "openSUSE detected. Installing dependencies via zypper..."
        sudo zypper install -y \
            kdeconnect-kde \
            python3-gobject \
            gtk4 \
            libadwaita1 \
            xapps \
            hicolor-icon-theme
    else
        warn "Unsupported or undetected distribution."
        warn "Please ensure you manually install:"
        warn "  - kdeconnect"
        warn "  - python3-gobject (PyGObject)"
        warn "  - gtk4 (and gir bindings)"
        warn "  - libadwaita (and gir bindings)"
        warn "  - xapp / xapps (and gir bindings)"
    fi
}

install_system_dependencies

# ── 3. Verify KDE Connect daemon ─────────────────────────────────────
verify_kdeconnect_daemon() {
    if ! command -v kdeconnect-cli >/dev/null 2>&1; then
        warn "kdeconnect-cli not found. Please verify KDE Connect is installed."
        return 1
    fi

    if pgrep -x "kdeconnectd" >/dev/null 2>&1; then
        info "KDE Connect daemon is running."
    else
        status "KDE Connect daemon is not running. Attempting to start it..."
        if command -v systemctl >/dev/null 2>&1 && systemctl --user is-enabled kdeconnect >/dev/null 2>&1; then
            systemctl --user start kdeconnect
        else
            # Try launching the daemon executable in different common libexec paths
            if command -v kdeconnectd >/dev/null 2>&1; then
                kdeconnectd &
            elif [[ -f "/usr/lib/libexec/kdeconnectd" ]]; then
                "/usr/lib/libexec/kdeconnectd" &
            elif [[ -f "/usr/lib/x86_64-linux-gnu/libexec/kdeconnectd" ]]; then
                "/usr/lib/x86_64-linux-gnu/libexec/kdeconnectd" &
            fi
        fi

        sleep 1
        if pgrep -x "kdeconnectd" >/dev/null 2>&1; then
            info "Successfully started KDE Connect daemon."
        else
            warn "Could not start KDE Connect daemon automatically."
            warn "You may need to run: kdeconnectd &"
        fi
    fi
}

verify_kdeconnect_daemon

# ── 4. Launcher script ───────────────────────────────────────────────
info "Installing launcher..."
mkdir -p "$BIN_DIR"
cat > "$BIN_DIR/phonelink" << EOF
#!/usr/bin/env bash
exec python3 "$SCRIPT_DIR/run.py" "\$@"
EOF
chmod +x "$BIN_DIR/phonelink"

# ── 5. PATH Configuration ────────────────────────────────────────────
configure_path() {
    local shell_name
    shell_name="$(basename "$SHELL")"
    local profile_file=""

    if [[ "$shell_name" == "zsh" ]]; then
        profile_file="$HOME/.zshrc"
    elif [[ "$shell_name" == "bash" ]]; then
        profile_file="$HOME/.bashrc"
    else
        profile_file="$HOME/.profile"
    fi

    if [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
        local export_line='export PATH="$HOME/.local/bin:$PATH"'
        if [[ -f "$profile_file" ]] && grep -qF "$export_line" "$profile_file"; then
            info "$BIN_DIR is configured in $profile_file but not currently active in this terminal."
            info "Run 'source $profile_file' or restart your terminal to activate it."
        else
            info "Adding $BIN_DIR to PATH in $profile_file..."
            echo "" >> "$profile_file"
            echo "# Added by Phone Link Installer" >> "$profile_file"
            echo "$export_line" >> "$profile_file"
            info "PATH updated! Run 'source $profile_file' to apply to the current session."
        fi
        export PATH="$BIN_DIR:$PATH"
    else
        info "$BIN_DIR is already in your PATH."
    fi
}

configure_path

# ── 6. Desktop entry and icons ───────────────────────────────────────
info "Installing desktop entry and icons..."
mkdir -p "$APPS_DIR" "$ICONS_DIR/scalable/apps"

sed "s|Exec=.*|Exec=\"${BIN_DIR}/phonelink\"|" \
    "$SCRIPT_DIR/data/phonelink.desktop" > "$APPS_DIR/${DESKTOP_ID}.desktop"
rm -f "$APPS_DIR/phonelink.desktop"

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
info "==============================================="
info " Phone Link installed successfully!"
info "==============================================="
echo ""
echo "  Launch Phone Link from your applications menu, or run:"
echo "    phonelink"
echo ""
echo "  First-time pairing setup:"
echo "    1. Install KDE Connect on your Android phone"
echo "    2. Ensure phone and PC are on the same Wi-Fi network"
echo "    3. Run 'kdeconnect-cli --list-devices' to see your phone"
echo "    4. Pair using 'kdeconnect-cli -d <device-id> --pair'"
echo ""
