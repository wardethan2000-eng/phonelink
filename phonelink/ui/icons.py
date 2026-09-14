"""Theme-aware icon helpers.

Phone Link was written on Linux Mint, whose icon themes (Mint-Y / XApp)
provide names such as ``xsi-notifications-symbolic``.  Other desktop icon
themes do not, and GTK then draws its red "image-missing" placeholder.
``resolve_icon`` picks the first available alternative for each such name.

Breeze (KDE) also draws its phone glyph much smaller inside its canvas than
Adwaita does, so ``device_icon_size`` scales device icons up there.
"""

from __future__ import annotations

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gtk, Gdk

# Ordered alternatives for names that are not part of the freedesktop icon
# naming spec or that some themes ship under a different name.
_FALLBACKS: dict[str, tuple[str, ...]] = {
    "xsi-notifications-symbolic": (
        "notifications-symbolic",
        "notification-symbolic",
        "preferences-system-notifications-symbolic",
    ),
    "image-x-generic-symbolic": ("image-x-generic", "image-loading-symbolic"),
    "video-x-generic-symbolic": ("video-x-generic", "camera-video-symbolic"),
    "audio-x-generic-symbolic": ("audio-x-generic", "audio-symbolic"),
    "x-office-document-symbolic": ("x-office-document", "text-x-generic"),
    "application-x-executable-symbolic": ("application-x-executable",),
    "package-x-generic-symbolic": ("package-x-generic",),
    "text-x-generic-symbolic": ("text-x-generic",),
    "web-browser-symbolic": (
        "internet-web-browser-symbolic",
        "web-browser",
        "globe-symbolic",
    ),
    "display-brightness-symbolic": (
        "display-brightness",
        "brightness-high-symbolic",
    ),
    "dialog-error-symbolic": ("state-error-symbolic", "error-symbolic"),
    "dialog-warning-symbolic": (
        "state-warning-symbolic",
        "messagebox_warning-symbolic",
    ),
    "dialog-information-symbolic": (
        "state-information-symbolic",
        "info-symbolic",
    ),
}


def _icon_theme() -> Gtk.IconTheme | None:
    display = Gdk.Display.get_default()
    if display is None:
        return None
    return Gtk.IconTheme.get_for_display(display)


def resolve_icon(name: str) -> str:
    """Return the requested icon name, or the best fallback the theme has."""
    theme = _icon_theme()
    if theme is None or theme.has_icon(name):
        return name
    for candidate in _FALLBACKS.get(name, ()):
        if theme.has_icon(candidate):
            return candidate
    return name


def _icon_theme_name() -> str:
    settings = Gtk.Settings.get_default()
    if settings is None:
        return ""
    return str(settings.get_property("gtk-icon-theme-name") or "")


def device_icon_size(size: int) -> int:
    """Scale device icons up on Breeze, whose phone glyph is undersized."""
    if "breeze" in _icon_theme_name().lower():
        return round(size * 1.3)
    return size
