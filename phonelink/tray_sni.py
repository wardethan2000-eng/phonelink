"""In-process system tray via the StatusNotifierItem D-Bus spec.

The historical tray runs GTK3 (XApp/AppIndicator) in a *subprocess* because GTK3
can't be imported into this GTK4 process.  But the modern tray protocol —
``org.kde.StatusNotifierItem`` + ``com.canonical.dbusmenu`` — is just D-Bus, so
we can speak it directly with Gio and drop the subprocess entirely on desktops
that provide a StatusNotifierWatcher (KDE, most GNOME/Cinnamon setups via an
extension, xapp-sn-watcher, etc.).

``InProcessTray.start()`` returns ``True`` only if it successfully registered
with a watcher; the caller falls back to the subprocess tray otherwise, so this
never regresses a working tray.
"""

from __future__ import annotations

import os
from typing import Callable

from gi.repository import Gio, GLib

_SNI_IFACE = "org.kde.StatusNotifierItem"
_MENU_IFACE = "com.canonical.dbusmenu"
_WATCHER_NAME = "org.kde.StatusNotifierWatcher"
_WATCHER_PATH = "/StatusNotifierWatcher"

_SNI_XML = f"""
<node>
  <interface name="{_SNI_IFACE}">
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <method name="Activate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="SecondaryActivate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="ContextMenu"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="Scroll"><arg type="i" direction="in"/><arg type="s" direction="in"/></method>
    <signal name="NewIcon"/>
    <signal name="NewStatus"><arg type="s"/></signal>
    <signal name="NewTitle"/>
  </interface>
</node>
"""

# A tiny fixed two-item menu: "Show Phone Link" (id 1) and "Quit" (id 2).
_MENU_XML = f"""
<node>
  <interface name="{_MENU_IFACE}">
    <property name="Version" type="u" access="read"/>
    <property name="Status" type="s" access="read"/>
    <method name="GetLayout">
      <arg type="i" name="parentId" direction="in"/>
      <arg type="i" name="recursionDepth" direction="in"/>
      <arg type="as" name="propertyNames" direction="in"/>
      <arg type="u" name="revision" direction="out"/>
      <arg type="(ia{{sv}}av)" name="layout" direction="out"/>
    </method>
    <method name="GetGroupProperties">
      <arg type="ai" name="ids" direction="in"/>
      <arg type="as" name="propertyNames" direction="in"/>
      <arg type="a(ia{{sv}})" name="properties" direction="out"/>
    </method>
    <method name="GetProperty">
      <arg type="i" name="id" direction="in"/>
      <arg type="s" name="name" direction="in"/>
      <arg type="v" name="value" direction="out"/>
    </method>
    <method name="Event">
      <arg type="i" name="id" direction="in"/>
      <arg type="s" name="eventId" direction="in"/>
      <arg type="v" name="data" direction="in"/>
      <arg type="u" name="timestamp" direction="in"/>
    </method>
    <method name="AboutToShow">
      <arg type="i" name="id" direction="in"/>
      <arg type="b" name="needUpdate" direction="out"/>
    </method>
    <signal name="LayoutUpdated"><arg type="u"/><arg type="i"/></signal>
  </interface>
</node>
"""


def _menu_item(item_id: int, label: str) -> GLib.Variant:
    """A dbusmenu layout node: (id, properties, children)."""
    props = {
        "label": GLib.Variant("s", label),
        "enabled": GLib.Variant("b", True),
        "visible": GLib.Variant("b", True),
    }
    return GLib.Variant("(ia{sv}av)", (item_id, props, []))


class InProcessTray:
    def __init__(self, icon_name: str = "phonelink"):
        self._icon_name = icon_name
        self._conn: Gio.DBusConnection | None = None
        self._sni_reg = 0
        self._menu_reg = 0
        self._on_activate: Callable[[], None] = lambda: None
        self._on_quit: Callable[[], None] = lambda: None
        self._object_path = f"/StatusNotifierItem"

    # ── public ─────────────────────────────────────────────────────

    def start(self, on_activate: Callable[[], None], on_quit: Callable[[], None]) -> bool:
        self._on_activate = on_activate
        self._on_quit = on_quit
        try:
            self._conn = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            self._register_objects()
            return self._register_with_watcher()
        except GLib.Error as exc:
            print(f"[phonelink] in-process tray unavailable: {exc}")
            self.stop()
            return False

    def stop(self):
        if self._conn is None:
            return
        for reg in (self._sni_reg, self._menu_reg):
            if reg:
                try:
                    self._conn.unregister_object(reg)
                except Exception:
                    pass
        self._sni_reg = self._menu_reg = 0

    # ── registration ───────────────────────────────────────────────

    def _register_objects(self):
        assert self._conn is not None
        sni_info = Gio.DBusNodeInfo.new_for_xml(_SNI_XML).interfaces[0]
        menu_info = Gio.DBusNodeInfo.new_for_xml(_MENU_XML).interfaces[0]

        self._sni_reg = self._conn.register_object(
            self._object_path, sni_info,
            self._sni_method, self._sni_get_property, None,
        )
        self._menu_reg = self._conn.register_object(
            "/MenuBar", menu_info,
            self._menu_method, self._menu_get_property, None,
        )

    def _register_with_watcher(self) -> bool:
        assert self._conn is not None
        try:
            self._conn.call_sync(
                _WATCHER_NAME, _WATCHER_PATH, _WATCHER_NAME,
                "RegisterStatusNotifierItem",
                GLib.Variant("(s)", (self._conn.get_unique_name(),)),
                None, Gio.DBusCallFlags.NONE, 3000, None,
            )
            return True
        except GLib.Error as exc:
            print(f"[phonelink] no StatusNotifierWatcher ({exc.message}); using subprocess tray")
            return False

    # ── SNI interface ──────────────────────────────────────────────

    def _sni_get_property(self, conn, sender, path, iface, prop, *_):
        values = {
            "Category": GLib.Variant("s", "ApplicationStatus"),
            "Id": GLib.Variant("s", "phonelink"),
            "Title": GLib.Variant("s", "Phone Link"),
            "Status": GLib.Variant("s", "Active"),
            "IconName": GLib.Variant("s", self._icon_name),
            "ItemIsMenu": GLib.Variant("b", False),
            "Menu": GLib.Variant("o", "/MenuBar"),
        }
        return values.get(prop)

    def _sni_method(self, conn, sender, path, iface, method, params, invocation):
        if method in ("Activate", "SecondaryActivate"):
            GLib.idle_add(self._safe, self._on_activate)
        # ContextMenu / Scroll: nothing to do (host shows the Menu itself).
        invocation.return_value(None)

    # ── dbusmenu interface ─────────────────────────────────────────

    def _menu_get_property(self, conn, sender, path, iface, prop, *_):
        if prop == "Version":
            return GLib.Variant("u", 3)
        if prop == "Status":
            return GLib.Variant("s", "normal")
        return None

    def _menu_method(self, conn, sender, path, iface, method, params, invocation):
        try:
            self._menu_method_impl(conn, sender, path, iface, method, params, invocation)
        except Exception as exc:  # noqa: BLE001
            import traceback
            traceback.print_exc()
            try:
                invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.Failed", str(exc)
                )
            except Exception:
                pass

    def _menu_method_impl(self, conn, sender, path, iface, method, params, invocation):
        if method == "GetLayout":
            layout = (
                0,
                {"children-display": GLib.Variant("s", "submenu")},
                [_menu_item(1, "Show Phone Link"), _menu_item(2, "Quit")],
            )
            invocation.return_value(GLib.Variant("(u(ia{sv}av))", (1, layout)))
        elif method == "GetGroupProperties":
            entries = [
                (1, {"label": GLib.Variant("s", "Show Phone Link")}),
                (2, {"label": GLib.Variant("s", "Quit")}),
            ]
            invocation.return_value(GLib.Variant("(a(ia{sv}))", (entries,)))
        elif method == "GetProperty":
            invocation.return_value(GLib.Variant("(v)", (GLib.Variant("s", ""),)))
        elif method == "Event":
            item_id = params.get_child_value(0).get_int32()
            event_id = params.get_child_value(1).get_string()
            if event_id == "clicked":
                if item_id == 1:
                    GLib.idle_add(self._safe, self._on_activate)
                elif item_id == 2:
                    GLib.idle_add(self._safe, self._on_quit)
            invocation.return_value(None)
        elif method == "AboutToShow":
            invocation.return_value(GLib.Variant("(b)", (False,)))
        else:
            invocation.return_value(None)

    @staticmethod
    def _safe(fn):
        try:
            fn()
        except Exception as exc:  # noqa: BLE001
            print(f"[phonelink] tray callback error: {exc}")
        return False
