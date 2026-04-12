"""Settings dialog — Adw.PreferencesDialog for app preferences."""

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw

from phonelink.settings import get_settings


class SettingsDialog(Adw.PreferencesDialog):
    """App preferences dialog."""

    def __init__(self):
        super().__init__()
        self.set_title("Preferences")
        self.set_search_enabled(False)
        self._settings = get_settings()
        self._build()

    def _build(self):
        # ── General page ───────────────────────────────────────────
        general_page = Adw.PreferencesPage(title="General", icon_name="preferences-system-symbolic")
        self.add(general_page)

        # Startup group
        startup_group = Adw.PreferencesGroup(title="Startup")
        general_page.add(startup_group)

        startup_row = Adw.SwitchRow(
            title="Open on Login",
            subtitle="Automatically launch Phone Link when you sign in",
        )
        startup_row.set_active(self._settings.open_on_startup)
        startup_row.connect("notify::active", self._on_startup_toggled)
        startup_group.add(startup_row)

        # ── Appearance page ────────────────────────────────────────
        appearance_page = Adw.PreferencesPage(title="Appearance", icon_name="display-brightness-symbolic")
        self.add(appearance_page)

        theme_group = Adw.PreferencesGroup(title="Theme")
        appearance_page.add(theme_group)

        theme_row = Adw.ComboRow(title="Colour Scheme")
        theme_model = Gtk.StringList.new(["Follow system", "Light", "Dark"])
        theme_row.set_model(theme_model)
        current = {"system": 0, "light": 1, "dark": 2}.get(self._settings.color_scheme, 0)
        theme_row.set_selected(current)
        theme_row.connect("notify::selected", self._on_theme_changed)
        theme_group.add(theme_row)

        # ── Notifications page ─────────────────────────────────────
        notif_page = Adw.PreferencesPage(title="Notifications", icon_name="xsi-notifications-symbolic")
        self.add(notif_page)

        notif_group = Adw.PreferencesGroup(title="Notification Sync")
        notif_page.add(notif_group)

        enabled_row = Adw.SwitchRow(
            title="Show Phone Notifications",
            subtitle="Display your Android notifications in the tray",
        )
        enabled_row.set_active(self._settings.notifications_enabled)
        enabled_row.connect("notify::active", self._on_notif_enabled_toggled)
        notif_group.add(enabled_row)

        sound_row = Adw.SwitchRow(
            title="Play Sound",
            subtitle="Play a sound when a new notification arrives (requires libcanberra)",
        )
        sound_row.set_active(self._settings.notifications_sound)
        sound_row.connect("notify::active", self._on_notif_sound_toggled)
        notif_group.add(sound_row)

        # Ignored apps group
        ignored_group = Adw.PreferencesGroup(
            title="Hidden Apps",
            description="Notifications from these apps will not appear in the tray.",
        )
        notif_page.add(ignored_group)
        self._ignored_group = ignored_group
        self._rebuild_ignored_list()

        # Add-app entry row
        add_row = Adw.EntryRow(title="Hide notifications from app…")
        add_row.set_show_apply_button(True)
        add_row.connect("apply", self._on_add_ignored_app)
        ignored_group.add(add_row)
        self._add_row = add_row

    # ── Handlers ───────────────────────────────────────────────────

    def _on_startup_toggled(self, row, _param):
        self._settings.open_on_startup = row.get_active()

    def _on_theme_changed(self, row, _param):
        idx = row.get_selected()
        scheme = ["system", "light", "dark"][idx]
        self._settings.color_scheme = scheme
        _apply_color_scheme(scheme)

    def _on_notif_enabled_toggled(self, row, _param):
        self._settings.notifications_enabled = row.get_active()

    def _on_notif_sound_toggled(self, row, _param):
        self._settings.notifications_sound = row.get_active()

    def _on_add_ignored_app(self, entry_row):
        name = entry_row.get_text().strip()
        if name:
            self._settings.add_ignored_app(name)
            entry_row.set_text("")
            self._rebuild_ignored_list()

    def _rebuild_ignored_list(self):
        """Remove all current ignore-row children and re-add from settings."""
        # Remove all ActionRow children (keep the EntryRow at the end)
        child = self._ignored_group.get_first_child()
        to_remove = []
        while child:
            # ActionRows have a delete button we added — collect them
            if isinstance(child, Adw.ActionRow) and child is not self._add_row:
                to_remove.append(child)
            child = child.get_next_sibling()
        for r in to_remove:
            self._ignored_group.remove(r)

        for app_name in self._settings.notifications_ignored_apps:
            row = Adw.ActionRow(title=app_name)
            del_btn = Gtk.Button(icon_name="edit-delete-symbolic")
            del_btn.add_css_class("flat")
            del_btn.add_css_class("destructive-action")
            del_btn.set_valign(Gtk.Align.CENTER)
            del_btn.connect("clicked", self._on_remove_ignored_app, app_name)
            row.add_suffix(del_btn)
            # Insert before the entry row
            self._ignored_group.add(row)

    def _on_remove_ignored_app(self, _btn, app_name: str):
        self._settings.remove_ignored_app(app_name)
        self._rebuild_ignored_list()


# ── Helper: apply color scheme globally ───────────────────────────

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
