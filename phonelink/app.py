"""Phone Link GTK Application."""

import os
import subprocess
import sys
import signal

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Gdk, Gio, Adw, GLib

from phonelink.dbus_client import KDEConnectClient
from phonelink.ui.main_window import MainWindow
from phonelink.ui.settings_dialog import apply_saved_color_scheme


class PhoneLinkApp(Adw.Application):
    def __init__(self):
        super().__init__(
            application_id="dev.phonelink.app",
        )
        self.client = KDEConnectClient()
        self._tray_proc = None

    def do_startup(self):
        Adw.Application.do_startup(self)
        self._load_css()
        self._set_icon()
        apply_saved_color_scheme()
        self._setup_tray_icon()

    def do_activate(self):
        win = self.props.active_window
        if not win:
            win = MainWindow(application=self, client=self.client)
        elif not win.get_visible():
            win.set_visible(True)
        win.present()

    def _load_css(self):
        css_path = os.path.join(os.path.dirname(__file__), "style.css")
        if not os.path.exists(css_path):
            return
        provider = Gtk.CssProvider()
        provider.load_from_path(css_path)
        display = Gdk.Display.get_default()
        if display:
            Gtk.StyleContext.add_provider_for_display(
                display,
                provider,
                Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION,
            )

    def _set_icon(self):
        icon_dir = os.path.join(
            os.path.dirname(os.path.dirname(__file__)), "data", "icons"
        )
        icon_svg = os.path.join(icon_dir, "phonelink.svg")
        if not os.path.exists(icon_svg):
            return
        display = Gdk.Display.get_default()
        if display is None:
            return
        icon_theme = Gtk.IconTheme.get_for_display(display)
        icon_theme.add_search_path(icon_dir)
        Gtk.Window.set_default_icon_name("phonelink")

    def _setup_tray_icon(self):
        """Launch the tray icon as a separate subprocess (avoids GTK3/GTK4 conflict)."""
        icon_path = os.path.join(
            os.path.dirname(os.path.dirname(__file__)), "data", "icons", "phonelink-128.png"
        )
        if not os.path.exists(icon_path):
            icon_path = os.path.join(
                os.path.dirname(os.path.dirname(__file__)), "data", "icons", "phonelink-64.png"
            )
        if not os.path.exists(icon_path):
            return

        tray_script = os.path.join(os.path.dirname(__file__), "_tray.py")
        main_pid = str(os.getpid())
        try:
            self._tray_proc = subprocess.Popen(
                [sys.executable, tray_script, icon_path, main_pid],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except Exception as e:
            print(f"[phonelink] Failed to start tray icon: {e}")

        # Listen for SIGUSR1 (show window) from tray subprocess
        GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGUSR1, self._on_tray_show)
        # Listen for SIGUSR2 (quit) from tray subprocess
        GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGUSR2, self._on_tray_quit)

    def _on_tray_show(self):
        """Toggle the main window visibility (triggered by tray icon)."""
        win = self.props.active_window
        if win and win.get_visible():
            win.set_visible(False)
        elif win:
            win.set_visible(True)
            win.present()
        else:
            self.do_activate()
        return GLib.SOURCE_CONTINUE

    def _on_tray_quit(self):
        """Actually quit the application (triggered by tray icon SIGUSR2)."""
        self._cleanup_tray()
        win = self.props.active_window
        if win:
            win._quitting = True
            win.close()
        self.quit()
        return GLib.SOURCE_REMOVE

    def do_shutdown(self):
        self._cleanup_tray()
        Adw.Application.do_shutdown(self)

    def _cleanup_tray(self):
        """Terminate the tray subprocess if it's still running."""
        if self._tray_proc is not None:
            try:
                self._tray_proc.terminate()
                self._tray_proc.wait(timeout=2)
            except Exception:
                pass
            self._tray_proc = None
