"""Settings panel — split-view settings page embedded in the main window."""

from datetime import datetime

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw

from phonelink.settings import get_settings


class SettingsPanel(Gtk.Box):
    """In-window settings panel: category list on the left, content on the right."""

    def __init__(
        self,
        on_back,
        google_status_provider=None,
        on_google_connect=None,
        on_google_refresh=None,
        on_google_disconnect=None,
    ):
        super().__init__(orientation=Gtk.Orientation.HORIZONTAL)
        self._settings = get_settings()
        self._on_back = on_back
        self._google_status_provider = google_status_provider or (lambda: {})
        self._on_google_connect = on_google_connect
        self._on_google_refresh = on_google_refresh
        self._on_google_disconnect = on_google_disconnect
        self._ignored_rows: list = []
        self._build()

    def _build(self):
        # ── Left sidebar ───────────────────────────────────────────
        left = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        left.set_size_request(220, -1)
        self.append(left)

        # Back button + "Settings" heading
        back_bar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        back_bar.set_margin_top(10)
        back_bar.set_margin_start(8)
        back_bar.set_margin_bottom(8)
        back_bar.set_margin_end(8)
        left.append(back_bar)

        back_btn = Gtk.Button(icon_name="go-previous-symbolic")
        back_btn.add_css_class("flat")
        back_btn.set_tooltip_text("Back")
        back_btn.connect("clicked", lambda _: self._on_back())
        back_bar.append(back_btn)

        title_label = Gtk.Label(label="Settings")
        title_label.add_css_class("heading")
        title_label.set_xalign(0)
        back_bar.append(title_label)

        left.append(Gtk.Separator())

        # Category list
        self._cat_list = Gtk.ListBox()
        self._cat_list.set_selection_mode(Gtk.SelectionMode.SINGLE)
        self._cat_list.add_css_class("navigation-sidebar")
        self._cat_list.set_vexpand(True)
        self._cat_list.connect("row-selected", self._on_category_selected)
        left.append(self._cat_list)

        for icon_name, label, key in [
            ("emblem-system-symbolic", "General", "general"),
            ("avatar-default-symbolic", "Google Contacts", "google"),
            ("display-brightness-symbolic", "Appearance", "appearance"),
            ("xsi-notifications-symbolic", "Notifications", "notifications"),
        ]:
            row = Gtk.ListBoxRow()
            row._key = key
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
            box.set_margin_start(12)
            box.set_margin_end(12)
            box.set_margin_top(9)
            box.set_margin_bottom(9)
            img = Gtk.Image.new_from_icon_name(icon_name)
            img.set_pixel_size(16)
            box.append(img)
            lbl = Gtk.Label(label=label)
            lbl.set_xalign(0)
            box.append(lbl)
            row.set_child(box)
            self._cat_list.append(row)

        # ── Vertical divider ───────────────────────────────────────
        self.append(Gtk.Separator(orientation=Gtk.Orientation.VERTICAL))

        # ── Right content stack ────────────────────────────────────
        self._content_stack = Gtk.Stack()
        self._content_stack.set_hexpand(True)
        self._content_stack.set_vexpand(True)
        self._content_stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.append(self._content_stack)

        self._content_stack.add_named(self._build_general_page(), "general")
        self._content_stack.add_named(self._build_google_page(), "google")
        self._content_stack.add_named(self._build_appearance_page(), "appearance")
        self._content_stack.add_named(self._build_notifications_page(), "notifications")

        # Select first row by default
        first = self._cat_list.get_row_at_index(0)
        if first:
            self._cat_list.select_row(first)

    # ── Category switching ─────────────────────────────────────────

    def _on_category_selected(self, _listbox, row):
        if row is not None:
            self._content_stack.set_visible_child_name(row._key)

    # ── Page builders ──────────────────────────────────────────────

    def _build_general_page(self) -> Gtk.Widget:
        page = Adw.PreferencesPage()

        group = Adw.PreferencesGroup(title="Startup")
        page.add(group)

        row = Adw.SwitchRow(
            title="Open on Login",
            subtitle="Automatically launch Phone Link when you sign in",
        )
        row.set_active(self._settings.open_on_startup)
        row.connect(
            "notify::active",
            lambda r, _: setattr(self._settings, "open_on_startup", r.get_active()),
        )
        group.add(row)

        return page

    def _build_appearance_page(self) -> Gtk.Widget:
        page = Adw.PreferencesPage()

        group = Adw.PreferencesGroup(title="Theme")
        page.add(group)

        combo = Adw.ComboRow(title="Colour Scheme")
        combo.set_model(Gtk.StringList.new(["Follow system", "Light", "Dark"]))
        combo.set_selected(
            {"system": 0, "light": 1, "dark": 2}.get(self._settings.color_scheme, 0)
        )
        combo.connect("notify::selected", self._on_theme_changed)
        group.add(combo)

        return page

    def _build_google_page(self) -> Gtk.Widget:
        page = Adw.PreferencesPage()

        account_group = Adw.PreferencesGroup(title="Account")
        page.add(account_group)

        self._google_account_row = Adw.ActionRow(title="Google Contacts")

        self._google_connect_btn = Gtk.Button(label="Connect")
        self._google_connect_btn.add_css_class("suggested-action")
        self._google_connect_btn.connect("clicked", self._on_google_connect_clicked)
        self._google_account_row.add_suffix(self._google_connect_btn)

        self._google_refresh_btn = Gtk.Button(label="Refresh Now")
        self._google_refresh_btn.connect("clicked", self._on_google_refresh_clicked)
        self._google_account_row.add_suffix(self._google_refresh_btn)

        self._google_disconnect_btn = Gtk.Button(label="Disconnect")
        self._google_disconnect_btn.add_css_class("destructive-action")
        self._google_disconnect_btn.connect("clicked", self._on_google_disconnect_clicked)
        self._google_account_row.add_suffix(self._google_disconnect_btn)

        account_group.add(self._google_account_row)

        self._google_background_row = Adw.SwitchRow(
            title="Background Refresh",
            subtitle="Refresh Google contacts at most once every 24 hours using the saved token.",
        )
        self._google_background_row.set_active(self._settings.google_background_sync)
        self._google_background_row.connect(
            "notify::active",
            lambda r, _: setattr(self._settings, "google_background_sync", r.get_active()),
        )
        account_group.add(self._google_background_row)

        info_group = Adw.PreferencesGroup(title="How It Works")
        page.add(info_group)
        info_group.add(
            Adw.ActionRow(
                title="Low-volume sync",
                subtitle="Phone Link only runs automatic Google refresh when a saved account exists and the last attempt is older than one day.",
            )
        )
        info_group.add(
            Adw.ActionRow(
                title="Contact photos",
                subtitle="Google photos are cached locally for the contacts already visible in your current conversations.",
            )
        )

        self.refresh_google_status()
        return page

    def _build_notifications_page(self) -> Gtk.Widget:
        page = Adw.PreferencesPage()

        sync_group = Adw.PreferencesGroup(title="Notification Sync")
        page.add(sync_group)

        enabled_row = Adw.SwitchRow(
            title="Show Phone Notifications",
            subtitle="Display your Android notifications in the tray",
        )
        enabled_row.set_active(self._settings.notifications_enabled)
        enabled_row.connect(
            "notify::active",
            lambda r, _: setattr(self._settings, "notifications_enabled", r.get_active()),
        )
        sync_group.add(enabled_row)

        self._ignored_group = Adw.PreferencesGroup(
            title="Hidden Apps",
            description="Notifications from these apps will not appear in the tray.",
        )
        page.add(self._ignored_group)
        self._rebuild_ignored_list()

        add_row = Adw.EntryRow(title="Hide notifications from app…")
        add_row.set_show_apply_button(True)
        add_row.connect("apply", self._on_add_ignored_app)
        self._ignored_group.add(add_row)

        return page

    # ── Handlers ───────────────────────────────────────────────────

    def _on_theme_changed(self, row, _param):
        scheme = ["system", "light", "dark"][row.get_selected()]
        self._settings.color_scheme = scheme
        _apply_color_scheme(scheme)

    def _on_add_ignored_app(self, entry_row):
        name = entry_row.get_text().strip()
        if name:
            self._settings.add_ignored_app(name)
            entry_row.set_text("")
            self._rebuild_ignored_list()

    def refresh_google_status(self):
        status = self._google_status_provider() or {}
        configured = bool(status.get("configured"))
        connected = bool(status.get("connected"))
        in_flight = bool(status.get("sync_in_flight"))
        account_label = str(status.get("account_label") or "Google account")
        last_sync_ts = float(status.get("last_sync_ts") or 0.0)
        config_path = str(status.get("config_path") or "")

        if in_flight:
            subtitle = "Syncing Google Contacts now…"
        elif connected:
            subtitle = f"Connected as {account_label}."
            if last_sync_ts > 0:
                subtitle += f" Last synced {self._format_timestamp(last_sync_ts)}."
        elif configured:
            subtitle = "Not connected. Authorize Google Contacts to import names and photos."
        else:
            subtitle = f"Google OAuth client file not found at {config_path}."

        self._google_account_row.set_subtitle(subtitle)
        self._google_background_row.set_active(self._settings.google_background_sync)
        self._google_connect_btn.set_visible(not connected)
        self._google_refresh_btn.set_visible(connected)
        self._google_disconnect_btn.set_visible(connected)
        self._google_connect_btn.set_sensitive(not in_flight)
        self._google_refresh_btn.set_sensitive(not in_flight)
        self._google_disconnect_btn.set_sensitive(not in_flight)

    def _format_timestamp(self, timestamp: float) -> str:
        dt = datetime.fromtimestamp(timestamp)
        now = datetime.now()
        if dt.date() == now.date():
            return dt.strftime("today at %I:%M %p").replace(" 0", " ")
        return dt.strftime("%b %d at %I:%M %p").replace(" 0", " ")

    def _on_google_connect_clicked(self, _btn):
        if self._on_google_connect is not None:
            self._on_google_connect()
            self.refresh_google_status()

    def _on_google_refresh_clicked(self, _btn):
        if self._on_google_refresh is not None:
            self._on_google_refresh()
            self.refresh_google_status()

    def _on_google_disconnect_clicked(self, _btn):
        if self._on_google_disconnect is not None:
            self._on_google_disconnect()
            self.refresh_google_status()

    def _rebuild_ignored_list(self):
        for r in self._ignored_rows:
            self._ignored_group.remove(r)
        self._ignored_rows.clear()

        for app_name in self._settings.notifications_ignored_apps:
            row = Adw.ActionRow(title=app_name)
            del_btn = Gtk.Button(icon_name="edit-delete-symbolic")
            del_btn.add_css_class("flat")
            del_btn.add_css_class("destructive-action")
            del_btn.set_valign(Gtk.Align.CENTER)
            del_btn.connect("clicked", self._on_remove_ignored_app, app_name)
            row.add_suffix(del_btn)
            self._ignored_group.add(row)
            self._ignored_rows.append(row)

    def _on_remove_ignored_app(self, _btn, app_name: str):
        self._settings.remove_ignored_app(app_name)
        self._rebuild_ignored_list()


# ── Color scheme helpers (used by app.py at startup) ──────────────

def apply_saved_color_scheme():
    """Call once at startup to restore the saved color scheme."""
    _apply_color_scheme(get_settings().color_scheme)


def _apply_color_scheme(scheme: str):
    style_manager = Adw.StyleManager.get_default()
    mapping = {
        "system": Adw.ColorScheme.DEFAULT,
        "light": Adw.ColorScheme.FORCE_LIGHT,
        "dark": Adw.ColorScheme.FORCE_DARK,
    }
    style_manager.set_color_scheme(mapping.get(scheme, Adw.ColorScheme.DEFAULT))
