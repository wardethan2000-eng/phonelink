"""Main application window — sidebar + tabbed content area."""

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GLib

from phonelink.models import Device
from phonelink.dbus_client import IFACE_DAEMON
from phonelink.ui.device_sidebar import DeviceSidebar
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

        # ── Header bar with view switcher ──────────────────────────
        header = Adw.HeaderBar()

        self.stack = Adw.ViewStack()
        switcher = Adw.ViewSwitcher()
        switcher.set_stack(self.stack)
        switcher.set_policy(Adw.ViewSwitcherPolicy.WIDE)
        header.set_title_widget(switcher)

        # Ring-phone button in header
        ring_btn = Gtk.Button(icon_name="audio-volume-high-symbolic")
        ring_btn.set_tooltip_text("Ring phone")
        ring_btn.connect("clicked", self._on_ring_phone)
        header.pack_end(ring_btn)

        outer.append(header)

        # ── Main content: sidebar + tab area ───────────────────────
        content = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        content.set_vexpand(True)
        outer.append(content)

        # Sidebar
        self.sidebar = DeviceSidebar()
        sidebar_sw = Gtk.ScrolledWindow()
        sidebar_sw.set_child(self.sidebar)
        sidebar_sw.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        sidebar_sw.set_size_request(240, -1)
        sidebar_sw.add_css_class("sidebar-pane")
        content.append(sidebar_sw)

        # Vertical separator
        content.append(Gtk.Separator(orientation=Gtk.Orientation.VERTICAL))

        # Tab content
        self.sms_panel = SmsPanel(client=self.client)
        page = self.stack.add_titled(self.sms_panel, "sms", "Messages")
        page.set_icon_name("mail-unread-symbolic")

        self.notif_panel = NotificationsPanel(client=self.client)
        page = self.stack.add_titled(self.notif_panel, "notifications", "Notifications")
        page.set_icon_name("dialog-information-symbolic")

        self.files_panel = FilesPanel(client=self.client)
        page = self.stack.add_titled(self.files_panel, "files", "Files")
        page.set_icon_name("folder-symbolic")

        self.stack.set_vexpand(True)
        self.stack.set_hexpand(True)
        content.append(self.stack)

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

        self.sidebar.set_device(self.active_device)
        self.sidebar.set_all_devices(devices)

        # Update panels with active device
        for panel in (self.sms_panel, self.notif_panel, self.files_panel):
            panel.set_device(self.active_device)

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
