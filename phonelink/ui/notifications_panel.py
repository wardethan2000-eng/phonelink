"""Notifications panel — placeholder for Phase 3."""

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw


class NotificationsPanel(Gtk.Box):
    def __init__(self, client):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.client = client
        self._device = None

        self._status = Adw.StatusPage()
        self._status.set_icon_name("dialog-information-symbolic")
        self._status.set_title("Notifications")
        self._status.set_description(
            "View and manage your phone's notifications.\n"
            "This feature is coming in Phase 3."
        )
        self._status.set_vexpand(True)
        self.append(self._status)

    def set_device(self, device):
        self._device = device
        if device and device.reachable:
            self._status.set_description(
                f"Connected to {device.name}.\n"
                "Notification management is coming in Phase 3."
            )
        elif device:
            self._status.set_description(
                f"{device.name} is disconnected.\n"
                "Connect your phone to see notifications."
            )
        else:
            self._status.set_description(
                "No device connected.\n"
                "Pair a phone to see notifications."
            )
