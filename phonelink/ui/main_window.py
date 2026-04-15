"""Main application window — header bar with device info + tabbed content."""

import os

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Gdk, Adw, GLib, Gio

from phonelink.models import Device
from phonelink.dbus_client import IFACE_DAEMON, IFACE_SHARE
from phonelink.ui.sms_panel import SmsPanel
from phonelink.ui.notifications_panel import NotificationsPanel
from phonelink.ui.files_panel import FilesPanel
from phonelink.ui.clipboard_panel import ClipboardPanel
from phonelink.ui.settings_dialog import SettingsPanel


class MainWindow(Adw.ApplicationWindow):
    def __init__(self, client, **kwargs):
        super().__init__(**kwargs)
        self.client = client
        self.active_device = None
        self._all_devices: list[Device] = []
        self._poll_source = None
        self._ui_built = False
        self._daemon_was_available = False
        self._share_signal_id = None

        self.set_title("Phone Link")
        self.set_default_size(960, 640)
        self.set_size_request(640, 420)

        self.connect("close-request", self._on_close)

        # Keyboard shortcuts
        key_ctrl = Gtk.EventControllerKey()
        key_ctrl.connect("key-pressed", self._on_key_pressed)
        self.add_controller(key_ctrl)

        # ── Connect to KDE Connect daemon ──────────────────────────
        if not self.client.connect():
            self._show_status(
                "dialog-error-symbolic",
                "D-Bus Error",
                "Cannot connect to the session bus.\n"
                "Make sure you are running a desktop session.",
            )
            # Still start polling so we can recover if the bus appears later
            self._start_live_updates()
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
            self._start_live_updates()
            return

        self._daemon_was_available = True
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
        self._ui_built = True
        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.set_content(outer)

        # ── Header bar ─────────────────────────────────────────────
        header = Adw.HeaderBar()

        # Left side: device info — clickable button with popover for switching
        self._device_btn = Gtk.MenuButton()
        self._device_btn.add_css_class("flat")

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

        # Down arrow indicator for multi-device
        self._device_arrow = Gtk.Image.new_from_icon_name("pan-down-symbolic")
        self._device_arrow.set_pixel_size(12)
        self._device_arrow.set_visible(False)
        self._device_box.append(self._device_arrow)

        self._device_btn.set_child(self._device_box)

        # Device switcher popover
        self._device_popover = Gtk.Popover()
        self._device_list_box = Gtk.ListBox()
        self._device_list_box.set_selection_mode(Gtk.SelectionMode.NONE)
        self._device_list_box.connect("row-activated", self._on_device_row_activated)
        self._device_list_box.add_css_class("boxed-list")
        self._device_popover.set_child(self._device_list_box)
        self._device_btn.set_popover(self._device_popover)

        header.pack_start(self._device_btn)

        # Center title
        self.stack = Adw.ViewStack()
        header.set_title_widget(Gtk.Label(label="Phone Link"))

        # Right side: battery + notifications toggle + ring button
        right_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)

        self._battery_icon = Gtk.Image()
        self._battery_icon.set_pixel_size(16)
        right_box.append(self._battery_icon)

        self._battery_label = Gtk.Label()
        self._battery_label.add_css_class("caption")
        right_box.append(self._battery_label)

        header.pack_end(right_box)

        find_btn = Gtk.Button(icon_name="find-location-symbolic")
        find_btn.set_tooltip_text("Find Phone — makes your phone ring via KDE Connect")
        find_btn.connect("clicked", self._on_ring_phone)
        header.pack_end(find_btn)

        self._notif_toggle = Gtk.ToggleButton(icon_name="xsi-notifications-symbolic")
        self._notif_toggle.set_tooltip_text("Notifications")
        self._notif_toggle.connect("toggled", self._on_notif_toggled)
        header.pack_end(self._notif_toggle)

        settings_btn = Gtk.Button(icon_name="emblem-system-symbolic")
        settings_btn.set_tooltip_text("Preferences")
        settings_btn.connect("clicked", self._on_open_settings)
        header.pack_end(settings_btn)

        outer.append(header)

        switcher_bar = Adw.ViewSwitcherBar()
        switcher_bar.set_stack(self.stack)
        switcher_bar.set_reveal(True)
        outer.append(switcher_bar)

        # ── Main tab content ────────────────────────────────────────
        self.sms_panel = SmsPanel(client=self.client)
        page = self.stack.add_titled(self.sms_panel, "sms", "Messages")
        page.set_icon_name("mail-unread-symbolic")

        self.files_panel = FilesPanel(client=self.client)
        page = self.stack.add_titled(self.files_panel, "files", "Files")
        page.set_icon_name("folder-symbolic")

        self.clipboard_panel = ClipboardPanel(client=self.client)
        page = self.stack.add_titled(self.clipboard_panel, "clipboard", "Clipboard")
        page.set_icon_name("edit-paste-symbolic")

        self.stack.set_vexpand(True)
        self.stack.set_hexpand(True)

        # ── Drag-and-drop: accept files dropped anywhere on the window ──
        drop_target = Gtk.DropTarget.new(Gio.File, Gdk.DragAction.COPY)
        drop_target.set_gtypes([Gio.File])
        drop_target.connect("drop", self._on_file_drop)
        self.stack.add_controller(drop_target)

        # ── Notifications sidebar ───────────────────────────────────
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

        # OverlaySplitView: sidebar overlays content (doesn't push it)
        self._split_view = Adw.OverlaySplitView()
        self._split_view.set_sidebar_position(Gtk.PackType.END)
        self._split_view.set_sidebar(notif_box)
        self._split_view.set_content(self.stack)
        self._split_view.set_show_sidebar(False)
        self._split_view.set_vexpand(True)
        # Keep toggle button in sync if user swipes sidebar closed
        self._split_view.connect("notify::show-sidebar", self._on_sidebar_notify)

        # Body stack — "main" (normal view) or "settings" (settings panel)
        self._body_stack = Gtk.Stack()
        self._body_stack.set_vexpand(True)
        self._body_stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self._body_stack.add_named(self._split_view, "main")

        self._settings_panel = SettingsPanel(
            on_back=self._show_main,
            google_status_provider=self.sms_panel.get_google_status,
            on_google_connect=self.sms_panel.connect_google_contacts,
            on_google_refresh=self.sms_panel.refresh_google_contacts,
            on_google_disconnect=self.sms_panel.disconnect_google_contacts,
        )
        self.sms_panel.connect(
            "google-status-changed",
            lambda *_args: self._settings_panel.refresh_google_status(),
        )
        self._body_stack.add_named(self._settings_panel, "settings")

        self._toast_overlay = Adw.ToastOverlay()
        self._toast_overlay.set_child(self._body_stack)
        self._toast_overlay.set_vexpand(True)
        outer.append(self._toast_overlay)

    def _on_notif_toggled(self, btn):
        self._split_view.set_show_sidebar(btn.get_active())

    def _on_sidebar_notify(self, split_view, _param):
        # Sync the toggle button when sidebar is dismissed via gesture
        self._notif_toggle.set_active(split_view.get_show_sidebar())

    def _on_open_settings(self, _btn):
        self._settings_panel.refresh_google_status()
        self._body_stack.set_visible_child_name("settings")

    def _show_main(self):
        self._body_stack.set_visible_child_name("main")

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

        self._all_devices = devices

        # Pick the best device to display (keep current if still valid)
        if self.active_device:
            still_valid = next(
                (d for d in devices if d.id == self.active_device.id and d.paired),
                None,
            )
            if still_valid:
                self.active_device = still_valid
            else:
                self.active_device = None

        if not self.active_device:
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
        self._update_device_popover()
        self._subscribe_share_signal()

        # Update panels with active device
        for panel in (self.sms_panel, self.notif_panel, self.files_panel,
                      self.clipboard_panel):
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
        # Check if daemon is (still) available
        if not self.client.bus:
            self.client.connect()

        daemon_ok = self.client.is_daemon_available() if self.client.bus else False

        if daemon_ok and not self._daemon_was_available:
            # Daemon just appeared — build the UI if needed
            self._daemon_was_available = True
            if not self._ui_built:
                self._build_ui()
            self._refresh_devices()
        elif daemon_ok:
            self._refresh_devices()
        elif not daemon_ok and self._daemon_was_available:
            # Daemon disappeared mid-session
            self._daemon_was_available = False
            self.active_device = None
            if self._ui_built:
                for panel in (self.sms_panel, self.notif_panel, self.files_panel,
                              self.clipboard_panel):
                    panel.set_device(None)
                self._update_device_header()

        return GLib.SOURCE_CONTINUE

    # ── Device switcher popover ──────────────────────────────────

    def _update_device_popover(self):
        """Rebuild the device list popover."""
        paired = [d for d in self._all_devices if d.paired]
        # Only show the arrow + make button clickable if there are multiple devices
        self._device_arrow.set_visible(len(paired) > 1)
        self._device_btn.set_sensitive(len(paired) > 1)

        # Remove old rows
        while True:
            row = self._device_list_box.get_row_at_index(0)
            if row is None:
                break
            self._device_list_box.remove(row)

        for dev in paired:
            row = Gtk.ListBoxRow()
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
            box.set_margin_start(10)
            box.set_margin_end(10)
            box.set_margin_top(6)
            box.set_margin_bottom(6)

            icon = Gtk.Image.new_from_icon_name(dev.type_icon_name)
            icon.set_pixel_size(20)
            box.append(icon)

            name_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
            name_box.set_hexpand(True)
            name_label = Gtk.Label(label=dev.name)
            name_label.set_xalign(0)
            name_label.add_css_class("heading")
            name_box.append(name_label)

            status_label = Gtk.Label(label=dev.status_label)
            status_label.set_xalign(0)
            status_label.add_css_class("caption")
            status_label.add_css_class("dim-label")
            name_box.append(status_label)
            box.append(name_box)

            if self.active_device and dev.id == self.active_device.id:
                check = Gtk.Image.new_from_icon_name("object-select-symbolic")
                check.set_pixel_size(16)
                box.append(check)

            row.set_child(box)
            row._device_id = dev.id
            self._device_list_box.append(row)

    def _on_device_row_activated(self, list_box, row):
        """Switch to the selected device."""
        self._device_popover.popdown()
        target_id = row._device_id
        if self.active_device and self.active_device.id == target_id:
            return
        target = next((d for d in self._all_devices if d.id == target_id), None)
        if target:
            self.active_device = target
            self._update_device_header()
            self._update_device_popover()
            self._subscribe_share_signal()
            for panel in (self.sms_panel, self.notif_panel, self.files_panel,
                          self.clipboard_panel):
                panel.set_device(self.active_device)

    # ── Drag-and-drop file transfer ───────────────────────────────

    def _on_file_drop(self, drop_target, value, x, y):
        """Handle files dropped onto the window."""
        if not self.active_device or not self.active_device.reachable:
            self._show_toast("No connected device — cannot send file")
            return False
        path = value.get_path()
        if not path:
            return False
        self.client.share_file(self.active_device.id, path)
        name = os.path.basename(path)
        self._show_toast(f"Sending {name} to {self.active_device.name}…")
        return True

    # ── Continue on PC (URL share from phone) ─────────────────────

    def _subscribe_share_signal(self):
        """Subscribe to shareReceived so URLs from the phone open in the browser."""
        if self._share_signal_id is not None and self.client.bus:
            self.client.bus.signal_unsubscribe(self._share_signal_id)
            self._share_signal_id = None
        if not self.active_device:
            return
        path = f"/modules/kdeconnect/devices/{self.active_device.id}"
        sid = self.client.subscribe_signal(
            path, IFACE_SHARE, "shareReceived",
            self._on_share_received,
        )
        self._share_signal_id = sid

    def _on_share_received(self, conn, sender, path, iface, signal, params):
        url = params.unpack()[0]
        if url.startswith("http://") or url.startswith("https://"):
            GLib.idle_add(self._open_url_on_pc, url)

    def _open_url_on_pc(self, url: str):
        """Open a URL received from the phone in the default browser."""
        import subprocess
        try:
            subprocess.Popen(["xdg-open", url])
        except FileNotFoundError:
            pass
        self._show_toast(f"Opened link from phone")

    # ── Actions ────────────────────────────────────────────────────

    def _on_key_pressed(self, controller, keyval, keycode, state):
        """Handle global keyboard shortcuts."""
        ctrl = state & Gdk.ModifierType.CONTROL_MASK
        if ctrl and keyval == Gdk.KEY_n:
            self._notif_toggle.set_active(not self._notif_toggle.get_active())
            return True
        return False

    def _on_ring_phone(self, _btn):
        if not self.active_device:
            self._show_toast("No paired device found")
            return
        self.client.ring_device(self.active_device.id)
        self._show_toast("Finding your phone…")

    def _show_toast(self, message: str):
        toast = Adw.Toast.new(message)
        toast.set_timeout(3)
        self._toast_overlay.add_toast(toast)

    # ── Cleanup ────────────────────────────────────────────────────

    def _on_close(self, _window):
        # If quitting from the tray menu, allow close to proceed
        if getattr(self, '_quitting', False):
            if self._poll_source:
                GLib.source_remove(self._poll_source)
                self._poll_source = None
            self.client.cleanup()
            return False  # allow close

        # Otherwise, hide to system tray instead of quitting
        self.set_visible(False)
        return True  # True = prevent the close / destroy
