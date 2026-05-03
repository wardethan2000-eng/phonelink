"""System tray icon subprocess for Phone Link.

Runs in a separate process to avoid GTK3/GTK4 conflicts.
Communicates with the main app via POSIX signals:
  SIGUSR1 → main app: show/raise the window
  SIGUSR2 → main app: quit
"""

import os
import signal
import sys

import gi
gi.require_version("Gtk", "3.0")
try:
    gi.require_version("XApp", "1.0")
    from gi.repository import XApp
except (ValueError, ImportError):
    XApp = None
from gi.repository import Gtk, GLib


def main():
    if len(sys.argv) < 3:
        print("Usage: _tray.py <icon_path> <parent_pid>", file=sys.stderr)
        sys.exit(1)

    icon_path = sys.argv[1]
    parent_pid = int(sys.argv[2])

    if XApp is None:
        print("[phonelink] XApp not available — tray icon disabled. Install gir1.2-xapp-1.0")
        sys.exit(0)

    # Exit if the parent process dies
    def check_parent():
        try:
            os.kill(parent_pid, 0)
        except OSError:
            Gtk.main_quit()
            return False
        return True

    GLib.timeout_add_seconds(2, check_parent)

    icon = XApp.StatusIcon()
    icon.set_icon_name("phonelink")
    icon.set_tooltip_text("Phone Link")
    icon.set_name("Phone Link")

    # Left-click: show window
    def on_activate(status_icon, button, time):
        if button == 1:  # left click
            os.kill(parent_pid, signal.SIGUSR1)

    icon.connect("activate", on_activate)

    # Right-click menu
    menu = Gtk.Menu()

    show_item = Gtk.MenuItem(label="Show Phone Link")
    show_item.connect("activate", lambda _: os.kill(parent_pid, signal.SIGUSR1))
    menu.append(show_item)

    menu.append(Gtk.SeparatorMenuItem())

    quit_item = Gtk.MenuItem(label="Quit")
    quit_item.connect("activate", lambda _: (
        os.kill(parent_pid, signal.SIGUSR2),
        GLib.timeout_add(500, Gtk.main_quit),
    ))
    menu.append(quit_item)

    menu.show_all()
    icon.set_secondary_menu(menu)

    Gtk.main()


if __name__ == "__main__":
    main()
