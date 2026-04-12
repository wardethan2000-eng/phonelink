"""D-Bus client for communicating with the KDE Connect daemon.

Uses Gio.DBusConnection for direct, synchronous communication with the
kdeconnectd service over the session bus.  Every public method handles
errors gracefully so the UI never crashes on a D-Bus timeout.
"""

import gi

gi.require_version("Gio", "2.0")
gi.require_version("GLib", "2.0")
from gi.repository import Gio, GLib

# ── D-Bus constants ────────────────────────────────────────────────
BUS_NAME = "org.kde.kdeconnect"
DAEMON_PATH = "/modules/kdeconnect"

IFACE_DAEMON = "org.kde.kdeconnect.daemon"
IFACE_DEVICE = "org.kde.kdeconnect.device"
IFACE_BATTERY = "org.kde.kdeconnect.device.battery"
IFACE_NOTIFICATIONS = "org.kde.kdeconnect.device.notifications"
IFACE_CONVERSATIONS = "org.kde.kdeconnect.device.conversations"
IFACE_CONTACTS = "org.kde.kdeconnect.device.contacts"
IFACE_SFTP = "org.kde.kdeconnect.device.sftp"
IFACE_SHARE = "org.kde.kdeconnect.device.share"
IFACE_CLIPBOARD = "org.kde.kdeconnect.device.clipboard"
IFACE_CONNECTIVITY = "org.kde.kdeconnect.device.connectivity_report"
IFACE_FINDPHONE = "org.kde.kdeconnect.device.findmyphone"
IFACE_PROPS = "org.freedesktop.DBus.Properties"

CALL_TIMEOUT_MS = 5000


class KDEConnectClient:
    """Thin wrapper around the KDE Connect D-Bus API."""

    def __init__(self):
        self.bus: Gio.DBusConnection | None = None
        self._subscriptions: list[int] = []

    # ── Connection ─────────────────────────────────────────────────

    def connect(self) -> bool:
        """Open a connection to the session bus.  Returns True on success."""
        try:
            self.bus = Gio.bus_get_sync(Gio.BusType.SESSION)
            return True
        except GLib.Error as exc:
            print(f"[phonelink] D-Bus connection failed: {exc.message}")
            return False

    def is_daemon_available(self) -> bool:
        """Return True if the KDE Connect daemon owns its bus name."""
        if not self.bus:
            return False
        try:
            result = self.bus.call_sync(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "NameHasOwner",
                GLib.Variant("(s)", (BUS_NAME,)),
                GLib.VariantType.new("(b)"),
                Gio.DBusCallFlags.NONE,
                CALL_TIMEOUT_MS,
                None,
            )
            return result.unpack()[0]
        except GLib.Error:
            return False

    # ── Low-level helpers ──────────────────────────────────────────

    def _call(self, path, iface, method, args=None, reply_type=None):
        """Synchronous D-Bus method call.  Returns GLib.Variant or None."""
        if not self.bus:
            return None
        try:
            rtype = (
                GLib.VariantType.new(reply_type)
                if isinstance(reply_type, str)
                else reply_type
            )
            return self.bus.call_sync(
                BUS_NAME,
                path,
                iface,
                method,
                args,
                rtype,
                Gio.DBusCallFlags.NONE,
                CALL_TIMEOUT_MS,
                None,
            )
        except GLib.Error as exc:
            # Silently ignore — the UI handles missing data
            print(f"[phonelink] D-Bus: {iface}.{method} @ {path}: {exc.message}")
            return None

    def _get_prop(self, path, iface, prop):
        """Read a single D-Bus property.  Returns the unwrapped value or None."""
        result = self._call(
            path,
            IFACE_PROPS,
            "Get",
            GLib.Variant("(ss)", (iface, prop)),
            "(v)",
        )
        return result.unpack()[0] if result else None

    def _device_path(self, device_id):
        return f"{DAEMON_PATH}/devices/{device_id}"

    # ── Daemon ─────────────────────────────────────────────────────

    def get_device_ids(self) -> list[str]:
        """Return a list of all known device ID strings."""
        # Try no-arg form first (uses default: onlyReachable=false, onlyPaired=false)
        result = self._call(
            DAEMON_PATH, IFACE_DAEMON, "devices", reply_type="(as)"
        )
        if result:
            return list(result.unpack()[0])
        # Fallback: explicit boolean args
        result = self._call(
            DAEMON_PATH,
            IFACE_DAEMON,
            "devices",
            GLib.Variant("(bb)", (False, False)),
            "(as)",
        )
        return list(result.unpack()[0]) if result else []

    # ── Device properties ──────────────────────────────────────────

    def get_device_name(self, device_id) -> str:
        return self._get_prop(
            self._device_path(device_id), IFACE_DEVICE, "name"
        ) or "Unknown"

    def get_device_type(self, device_id) -> str:
        return self._get_prop(
            self._device_path(device_id), IFACE_DEVICE, "type"
        ) or "phone"

    def is_device_reachable(self, device_id) -> bool:
        return bool(
            self._get_prop(
                self._device_path(device_id), IFACE_DEVICE, "isReachable"
            )
        )

    def is_device_paired(self, device_id) -> bool:
        return bool(
            self._get_prop(
                self._device_path(device_id), IFACE_DEVICE, "isPaired"
            )
        )

    # ── Battery ────────────────────────────────────────────────────

    def get_battery_charge(self, device_id) -> int:
        val = self._get_prop(
            self._device_path(device_id) + "/battery",
            IFACE_BATTERY,
            "charge",
        )
        return val if isinstance(val, int) else -1

    def is_battery_charging(self, device_id) -> bool:
        return bool(
            self._get_prop(
                self._device_path(device_id) + "/battery",
                IFACE_BATTERY,
                "isCharging",
            )
        )

    # ── Actions ────────────────────────────────────────────────────

    def ring_device(self, device_id):
        """Make the phone play a find-my-phone sound."""
        self._call(
            self._device_path(device_id) + "/findmyphone",
            IFACE_FINDPHONE,
            "ring",
        )

    def send_clipboard(self, device_id, text: str):
        """Push clipboard text to the device."""
        self._call(
            self._device_path(device_id) + "/clipboard",
            IFACE_CLIPBOARD,
            "sendClipboard",
            GLib.Variant("(s)", (text,)),
        )

    def share_url(self, device_id, url: str):
        """Send a URL to the device."""
        self._call(
            self._device_path(device_id) + "/share",
            IFACE_SHARE,
            "shareUrl",
            GLib.Variant("(s)", (url,)),
        )

    def share_file(self, device_id, path: str):
        """Send a file to the device."""
        self._call(
            self._device_path(device_id) + "/share",
            IFACE_SHARE,
            "shareUrl",
            GLib.Variant("(s)", (f"file://{path}",)),
        )

    # ── SMS / Conversations ────────────────────────────────────────

    def get_active_conversations(self, device_id) -> list:
        """Return the cached conversation list synchronously.

        Each entry is a GLib.Variant that unpacks to a tuple:
        (event, body, addresses, date, type, read, threadID, uID, subID, attachments)
        """
        result = self._call(
            self._device_path(device_id),
            IFACE_CONVERSATIONS,
            "activeConversations",
            reply_type="(av)",
        )
        if result:
            variants = result.unpack()[0]
            return list(variants)
        return []

    def request_all_conversations(self, device_id):
        """Ask the phone to send its conversation list.

        Results arrive asynchronously via conversationCreated /
        conversationUpdated signals.
        """
        self._call(
            self._device_path(device_id),
            IFACE_CONVERSATIONS,
            "requestAllConversationThreads",
        )

    def request_conversation(self, device_id, thread_id: int,
                             start: int = 0, end: int = 50):
        """Request a range of messages from a specific conversation.

        Results arrive via the conversationUpdated signal.
        """
        self._call(
            self._device_path(device_id),
            IFACE_CONVERSATIONS,
            "requestConversation",
            GLib.Variant("(xii)", (thread_id, start, end)),
        )

    def mark_conversation_as_read(self, device_id, thread_id: int):
        """Inform the phone that a conversation has been read."""
        self._call(
            self._device_path(device_id),
            IFACE_CONVERSATIONS,
            "markConversationAsRead",
            GLib.Variant("(x)", (thread_id,)),
        )

    def reply_to_conversation(self, device_id, thread_id: int,
                               message: str, attachments=None):
        """Reply to an existing conversation thread."""
        att_list = [GLib.Variant("s", a) for a in (attachments or [])]
        self._call(
            self._device_path(device_id),
            IFACE_CONVERSATIONS,
            "replyToConversation",
            GLib.Variant.new_tuple(
                GLib.Variant("x", thread_id),
                GLib.Variant("s", message),
                GLib.Variant("av", att_list),
            ),
        )

    def send_sms(self, device_id, addresses: list[str], message: str):
        """Send a new SMS to one or more phone numbers."""
        addr_variants = [GLib.Variant("s", a) for a in addresses]
        self._call(
            self._device_path(device_id),
            IFACE_CONVERSATIONS,
            "sendWithoutConversation",
            GLib.Variant.new_tuple(
                GLib.Variant("av", addr_variants),
                GLib.Variant("s", message),
                GLib.Variant("av", []),
            ),
        )

    # ── Contacts ───────────────────────────────────────────────────

    def sync_contacts(self, device_id):
        """Ask the phone to sync its contacts to the local vCard cache."""
        self._call(
            self._device_path(device_id) + "/contacts",
            IFACE_CONTACTS,
            "synchronizeRemoteWithLocal",
        )

    # ── SFTP / File browsing ───────────────────────────────────────

    def sftp_is_mounted(self, device_id) -> bool:
        result = self._call(
            self._device_path(device_id) + "/sftp",
            IFACE_SFTP,
            "isMounted",
            reply_type="(b)",
        )
        return result.unpack()[0] if result else False

    def sftp_mount_and_wait(self, device_id) -> bool:
        result = self._call(
            self._device_path(device_id) + "/sftp",
            IFACE_SFTP,
            "mountAndWait",
            reply_type="(b)",
        )
        return result.unpack()[0] if result else False

    def sftp_unmount(self, device_id):
        self._call(
            self._device_path(device_id) + "/sftp",
            IFACE_SFTP,
            "unmount",
        )

    def sftp_mount_point(self, device_id) -> str:
        result = self._call(
            self._device_path(device_id) + "/sftp",
            IFACE_SFTP,
            "mountPoint",
            reply_type="(s)",
        )
        return result.unpack()[0] if result else ""

    def sftp_get_mount_error(self, device_id) -> str:
        result = self._call(
            self._device_path(device_id) + "/sftp",
            IFACE_SFTP,
            "getMountError",
            reply_type="(s)",
        )
        return result.unpack()[0] if result else ""

    def sftp_get_directories(self, device_id) -> dict[str, str]:
        result = self._call(
            self._device_path(device_id) + "/sftp",
            IFACE_SFTP,
            "getDirectories",
            reply_type="(a{sv})",
        )
        if result:
            return {k: v for k, v in result.unpack()[0].items()}
        return {}

    def share_urls(self, device_id, urls: list[str]):
        """Send multiple files/URLs to the device."""
        self._call(
            self._device_path(device_id) + "/share",
            IFACE_SHARE,
            "shareUrls",
            GLib.Variant("(as)", (urls,)),
        )

    # ── Notifications ──────────────────────────────────────────────

    def get_active_notification_ids(self, device_id) -> list[str]:
        """Return list of active notification public IDs."""
        result = self._call(
            self._device_path(device_id) + "/notifications",
            IFACE_NOTIFICATIONS,
            "activeNotifications",
        )
        if result:
            return list(result.unpack()[0])
        return []

    def get_notification_properties(self, device_id, notif_id: str) -> dict:
        """Read all properties of a notification object."""
        path = self._device_path(device_id) + f"/notifications/{notif_id}"
        result = self._call(
            path, IFACE_PROPS, "GetAll",
            GLib.Variant("(s)", (
                "org.kde.kdeconnect.device.notifications.notification",
            )),
        )
        if result:
            return result.unpack()[0]
        return {}

    def dismiss_notification(self, device_id, notif_id: str):
        """Dismiss a notification on the phone."""
        self._call(
            self._device_path(device_id) + f"/notifications/{notif_id}",
            "org.kde.kdeconnect.device.notifications.notification",
            "dismiss",
        )

    def reply_to_notification(self, device_id, notif_id: str, message: str):
        """Reply to a notification via its sendReply method."""
        self._call(
            self._device_path(device_id) + f"/notifications/{notif_id}",
            "org.kde.kdeconnect.device.notifications.notification",
            "sendReply",
            GLib.Variant("(s)", (message,)),
        )

    def send_notification_reply_by_id(self, device_id, reply_id: str, message: str):
        """Reply to a notification using the replyId via the notifications interface."""
        self._call(
            self._device_path(device_id) + "/notifications",
            IFACE_NOTIFICATIONS,
            "sendReply",
            GLib.Variant("(ss)", (reply_id, message)),
        )

    # ── Signals ────────────────────────────────────────────────────

    def subscribe_signal(self, path, iface, signal_name, callback) -> int | None:
        """Subscribe to a D-Bus signal.  Returns subscription id."""
        if not self.bus:
            return None
        sid = self.bus.signal_subscribe(
            BUS_NAME,
            iface,
            signal_name,
            path,
            None,
            Gio.DBusSignalFlags.NONE,
            callback,
        )
        self._subscriptions.append(sid)
        return sid

    def cleanup(self):
        """Unsubscribe from all signals."""
        if self.bus:
            for sid in self._subscriptions:
                self.bus.signal_unsubscribe(sid)
        self._subscriptions.clear()
