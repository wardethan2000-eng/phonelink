"""Phone Link GTK Application."""

import os

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Gdk, Gio, Adw

from phonelink.dbus_client import KDEConnectClient
from phonelink.ui.main_window import MainWindow


class PhoneLinkApp(Adw.Application):
    def __init__(self):
        super().__init__(
            application_id="dev.phonelink.app",
            flags=Gio.ApplicationFlags.NON_UNIQUE,
        )
        self.client = KDEConnectClient()

    def do_startup(self):
        Adw.Application.do_startup(self)
        self._load_css()
        self._set_icon()

    def do_activate(self):
        win = self.props.active_window
        if not win:
            win = MainWindow(application=self, client=self.client)
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
        icon_path = os.path.join(
            os.path.dirname(os.path.dirname(__file__)), "data", "icons", "phonelink.svg"
        )
        if os.path.exists(icon_path):
            Gtk.Window.set_default_icon_name("phonelink")
            # Add the icon directory to the theme search path
            icon_theme = Gtk.IconTheme.get_for_display(Gdk.Display.get_default())
            icon_dir = os.path.dirname(icon_path)
            icon_theme.add_search_path(icon_dir)
