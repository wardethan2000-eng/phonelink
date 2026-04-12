"""Main application window — header bar with device info + tabbed content."""

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GLib

from phonelink.models import Device
from phonelink.dbus_client import IFACE_DAEMON
from phonelink.ui.sms_panel import SmsPanel
from phonelink.ui.notifications_panel import NotificationsPanel
from phonelink.ui.files_panel import FilesPanel


class MainWindow(Adw.ApplicationWindow):
    def __init__(self, client, **kwargs):
        super().__init__(**kwargs)
        self.client = client
        self.active_device = None
        self._poll_source = None

        self.set_title("Phone Link")
        self.set_default_size(960, 640)
        self.set_size_request(640, 420)

        self.connect("close-request", self._on_close)

        # ── Connect to KDE Connect daemon ──────────────────────────
        if not self.client.connect():
            self._show_status(
                "dialog-error-symbolic",
                "D-Bus Error",
                "Cannot connect to the session bus.\n"
                "Make sure you are running a desktop session.",
            )
            return

        if not self.client.is_daemon_available():
            self._show_status(
                "dialog-warning-symbolic",
                "KDE Connect Not Found",
                "The KDE Connect daemon is not running.\n\n"
                "Install it:\n"
                "  sudo apt install kdeconnect\n\n"
                "Then launch the KDE Connect indicator or run:\n"
                "  kdeconnectd &",
            )
            return

        self._build_ui()
        self._refresh_devices()
        self._start_live_updates()

    # ── UI construction ────────────────────────────────────────────

    def _show_status(self, icon, title, description):
        status = Adw.StatusPage()
        status.set_icon_name(icon)
        status.set_title(title)
        status.set_description(description)
        self.set_content(status)

    def _build_ui(self):
        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.set_content(outer)

        # ── Header bar ─────────────────────────────────────────────
        header = Adw.HeaderBar()

        # Left side: device info (icon + name + status dot)
        self._device_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)

        self._device_icon = Gtk.Image()
        self._device_icon.set_pixel_size(20)
        self._device_box.append(self._device_icon)

        self._device_name = Gtk.Label()
        self._device_name.add_css_class("heading")
        self._device_box.append(self._device_name)

        self._status_dot = Gtk.Label()
        self._status_dot.set_use_markup(True)
        self._device_box.append(self._status_dot)

        header.pack_start(self._device_box)

        # Center: view switcher (Messages + Files only)
        self.stack = Adw.ViewStack()
        switcher = Adw.ViewSwitcher()
        switcher.set_stack(self.stack)
        switcher.set_policy(Adw.ViewSwitcherPolicy.WIDE)
        header.set_title_widget(switcher)

        # Right side: battery + notifications toggle + ring button
        right_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)

        self._battery_icon = Gtk.Image()
        self._battery_icon.set_pixel_size(16)
        right_box.append(self._battery_icon)

        self._battery_label = Gtk.Label()
        self._battery_label.add_css_class("caption")
        right_box.append(self._battery_label)

        header.pack_end(right_box)

        ring_btn = Gtk.Button(icon_name="audio-volume-high-symbolic")
        ring_btn.set_tooltip_text("Ring phone")
        ring_btn.connect("clicked", self._on_ring_phone)
        header.pack_end(ring_btn)

        self._notif_toggle = Gtk.ToggleButton(icon_name="bell-outline-symbolic")
        self._notif_toggle.set_tooltip_text("Notifications")
        self._notif_toggle.connect("toggled", self._on_notif_toggled)
        header.pack_end(self._notif_toggle)

        outer.append(header)

        # ── Body: [notif tray revealer] + [separator] + [main tabs] ─
        body = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        body.set_vexpand(True)
        outer.append(body)

        # Notifications tray (collapsible)
        self._notif_revealer = Gtk.Revealer()
        self._notif_revealer.set_transition_type(Gtk.RevealerTransitionType.SLIDE_RIGHT)
        self._notif_revealer.set_reveal_child(False)

        notif_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        notif_box.set_size_request(300, -1)

        notif_header = Gtk.Label(label="Notifications")
        notif_header.add_css_class("heading")
        notif_header.set_xalign(0)
        notif_header.set_margin_start(12)
        notif_header.set_margin_top(10)
        notif_header.set_margin_bottom(6)
        notif_box.append(notif_header)

        notif_box.append(Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL))

        self.notif_panel = NotificationsPanel(client=self.client)
        self.notif_panel.set_vexpand(True)
        notif_box.append(self.notif_panel)

        self._notif_revealer.set_child(notif_box)
        body.append(self._notif_revealer)

        self._notif_sep = Gtk.Separator(orientation=Gtk.Orientation.VERTICAL)
        self._notif_sep.set_visible(False)
        body.append(self._notif_sep)

        # Main tab content
        self.sms_panel = SmsPanel(client=self.client)
        page = self.stack.add_titled(self.sms_panel, "sms", "Messages")
        page.set_icon_name("mail-unread-symbolic")

        self.files_panel = FilesPanel(client=self.client)
        page = self.stack.add_titled(self.files_panel, "files", "Files")
        page.set_icon_name("folder-symbolic")

        self.stack.set_vexpand(True)
        self.stack.set_hexpand(True)
        body.append(self.stack)

    def _on_notif_toggled(self, btn):
        revealed = btn.get_active()
        self._notif_revealer.set_reveal_child(revealed)
        self._notif_sep.set_visible(revealed)

    # ── Data refresh ───────────────────────────────────────────────

    def _refresh_devices(self):
        device_ids = self.client.get_device_ids()
        devices = []
        for did in device_ids:
            dev = Device(
                id=did,
                name=self.client.get_device_name(did),
                type=self.client.get_device_type(did),
                reachable=self.client.is_device_reachable(did),
                paired=self.client.is_device_paired(did),
                battery_charge=self.client.get_battery_charge(did),
                battery_charging=self.client.is_battery_charging(did),
            )
            devices.append(dev)

        # Pick the best device to display
        paired_reachable = [d for d in devices if d.paired and d.reachable]
        paired = [d for d in devices if d.paired]
        self.active_device = (
            paired_reachable[0]
            if paired_reachable
            else paired[0]
            if paired
            else devices[0]
            if devices
            else None
        )

        self._update_device_header()

        # Update panels with active device
        for panel in (self.sms_panel, self.notif_panel, self.files_panel):
            panel.set_device(self.active_device)

    def _update_device_header(self):
        dev = self.active_device
        if not dev:
            self._device_icon.set_from_icon_name("phone-symbolic")
            self._device_name.set_label("No Device")
            self._status_dot.set_markup(
                '<span foreground="#77767b">●</span>'
            )
            self._battery_icon.set_visible(False)
            self._battery_label.set_visible(False)
            return

        self._device_icon.set_from_icon_name(dev.type_icon_name)
        self._device_name.set_label(dev.name)

        if dev.reachable:
            self._status_dot.set_markup(
                '<span foreground="#2ec27e">●</span>'
            )
        elif dev.paired:
            self._status_dot.set_markup(
                '<span foreground="#c64600">●</span>'
            )
        else:
            self._status_dot.set_markup(
                '<span foreground="#77767b">●</span>'
            )

        if dev.battery_charge >= 0:
            self._battery_icon.set_from_icon_name(dev.battery_icon_name)
            self._battery_label.set_label(dev.battery_label)
            self._battery_icon.set_visible(True)
            self._battery_label.set_visible(True)
        else:
            self._battery_icon.set_visible(False)
            self._battery_label.set_visible(False)

    # ── Live updates ───────────────────────────────────────────────

    def _start_live_updates(self):
        self.client.subscribe_signal(
            "/modules/kdeconnect",
            IFACE_DAEMON,
            "deviceListChanged",
            self._on_device_list_changed,
        )
        self._poll_source = GLib.timeout_add_seconds(30, self._on_poll)

    def _on_device_list_changed(self, conn, sender, path, iface, signal, params):
        GLib.idle_add(self._refresh_devices)

    def _on_poll(self):
        self._refresh_devices()
        return GLib.SOURCE_CONTINUE

    # ── Actions ────────────────────────────────────────────────────

    def _on_ring_phone(self, _btn):
        if self.active_device and self.active_device.reachable:
            self.client.ring_device(self.active_device.id)

    # ── Cleanup ────────────────────────────────────────────────────

    def _on_close(self, _window):
        if self._poll_source:
            GLib.source_remove(self._poll_source)
            self._poll_source = None
        self.client.cleanup()
        return False  # False = allow the close to proceed
