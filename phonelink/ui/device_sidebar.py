"""Left sidebar showing the active device's info, battery, and status."""

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gtk


class DeviceSidebar(Gtk.Box):
    """Vertical sidebar that displays the primary connected device."""

    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.set_size_request(220, -1)
        self._all_devices = []

        self._stack = Gtk.Stack()
        self._stack.set_vexpand(True)
        self._stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.append(self._stack)

        # ── Empty state ────────────────────────────────────────────
        empty = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        empty.set_valign(Gtk.Align.CENTER)
        empty.set_halign(Gtk.Align.CENTER)
        empty.set_margin_start(24)
        empty.set_margin_end(24)

        empty_icon = Gtk.Image.new_from_icon_name("phone-symbolic")
        empty_icon.set_pixel_size(48)
        empty_icon.set_opacity(0.3)
        empty.append(empty_icon)

        empty_title = Gtk.Label(label="No Devices")
        empty_title.add_css_class("title-3")
        empty_title.add_css_class("dim-label")
        empty.append(empty_title)

        empty_desc = Gtk.Label(
            label="Install KDE Connect on your\nphone and pair it with this PC."
        )
        empty_desc.add_css_class("dim-label")
        empty_desc.set_wrap(True)
        empty_desc.set_justify(Gtk.Justification.CENTER)
        empty.append(empty_desc)

        self._stack.add_named(empty, "empty")

        # ── Device info state ──────────────────────────────────────
        device_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        device_box.set_margin_top(24)
        device_box.set_margin_bottom(24)
        device_box.set_margin_start(20)
        device_box.set_margin_end(20)

        # Device icon
        self._device_icon = Gtk.Image()
        self._device_icon.set_pixel_size(64)
        self._device_icon.set_halign(Gtk.Align.CENTER)
        device_box.append(self._device_icon)

        # Device name
        self._name_label = Gtk.Label()
        self._name_label.set_halign(Gtk.Align.CENTER)
        self._name_label.add_css_class("title-2")
        self._name_label.set_wrap(True)
        self._name_label.set_max_width_chars(18)
        self._name_label.set_justify(Gtk.Justification.CENTER)
        device_box.append(self._name_label)

        # Connection status (colored dot + text)
        self._status_label = Gtk.Label()
        self._status_label.set_halign(Gtk.Align.CENTER)
        self._status_label.set_use_markup(True)
        device_box.append(self._status_label)

        # Separator
        device_box.append(Gtk.Separator())

        # Battery section header
        bat_header = Gtk.Label(label="Battery")
        bat_header.set_halign(Gtk.Align.START)
        bat_header.add_css_class("dim-label")
        bat_header.add_css_class("caption")
        device_box.append(bat_header)

        # Battery icon + percentage
        bat_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        self._battery_icon = Gtk.Image()
        self._battery_icon.set_pixel_size(20)
        bat_row.append(self._battery_icon)
        self._battery_label = Gtk.Label()
        self._battery_label.set_hexpand(True)
        self._battery_label.set_halign(Gtk.Align.START)
        self._battery_label.add_css_class("title-4")
        bat_row.append(self._battery_label)
        device_box.append(bat_row)

        # Battery level bar
        self._battery_bar = Gtk.LevelBar()
        self._battery_bar.set_min_value(0)
        self._battery_bar.set_max_value(100)
        self._battery_bar.set_mode(Gtk.LevelBarMode.CONTINUOUS)
        # Custom offsets for battery colors
        self._battery_bar.add_offset_value("low", 20)
        self._battery_bar.add_offset_value("high", 50)
        self._battery_bar.add_offset_value("full", 100)
        device_box.append(self._battery_bar)

        # Spacer pushes everything to top
        spacer = Gtk.Box()
        spacer.set_vexpand(True)
        device_box.append(spacer)

        # Device count at bottom
        self._device_count = Gtk.Label()
        self._device_count.add_css_class("dim-label")
        self._device_count.add_css_class("caption")
        self._device_count.set_halign(Gtk.Align.CENTER)
        device_box.append(self._device_count)

        self._stack.add_named(device_box, "device")

        # Start with empty
        self._stack.set_visible_child_name("empty")

    def set_device(self, device):
        """Update sidebar to show the given device, or the empty state."""
        if device is None:
            self._stack.set_visible_child_name("empty")
            return

        self._stack.set_visible_child_name("device")
        self._device_icon.set_from_icon_name(device.type_icon_name)
        self._name_label.set_label(device.name)

        # Status with colored dot
        if device.reachable:
            color = "#2ec27e"
            text = "Connected"
        elif device.paired:
            color = "#c64600"
            text = "Disconnected"
        else:
            color = "#77767b"
            text = "Not Paired"
        self._status_label.set_markup(
            f'<span foreground="{color}">●</span>  {text}'
        )

        # Battery
        self._battery_icon.set_from_icon_name(device.battery_icon_name)
        self._battery_label.set_label(device.battery_label)
        if device.battery_charge >= 0:
            self._battery_bar.set_visible(True)
            self._battery_bar.set_value(device.battery_charge)
        else:
            self._battery_bar.set_visible(False)

    def set_all_devices(self, devices):
        """Store the full device list and update the count label."""
        self._all_devices = devices
        paired = [d for d in devices if d.paired]
        if len(paired) > 1:
            self._device_count.set_label(f"{len(paired)} paired devices")
            self._device_count.set_visible(True)
        else:
            self._device_count.set_visible(False)
