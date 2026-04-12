"""Notifications panel — view, dismiss, and reply to phone notifications."""

import time

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GLib, GObject, Gio, GdkPixbuf

from phonelink.dbus_client import IFACE_NOTIFICATIONS
from phonelink.models import Notification


# ── Notification row widget ────────────────────────────────────────


class NotificationRow(Gtk.Box):
    """A single notification entry in the list."""

    def __init__(self, notif: Notification):
        super().__init__(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        self.set_margin_top(6)
        self.set_margin_bottom(6)
        self.set_margin_start(10)
        self.set_margin_end(10)
        self.notif = notif

        # Icon
        if notif.has_icon and notif.icon_path:
            try:
                pixbuf = GdkPixbuf.Pixbuf.new_from_file_at_scale(
                    notif.icon_path, 32, 32, True
                )
                texture = Gtk.Image.new_from_pixbuf(pixbuf)
                texture.set_pixel_size(32)
                self.append(texture)
            except GLib.Error:
                self._add_fallback_icon(notif.app_name)
        else:
            self._add_fallback_icon(notif.app_name)

        # Text column
        text_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        text_box.set_hexpand(True)
        self.append(text_box)

        # Top row: app name + time
        top = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        text_box.append(top)

        app_label = Gtk.Label(label=notif.app_name or "Unknown App")
        app_label.add_css_class("caption")
        app_label.add_css_class("dim-label")
        app_label.set_xalign(0)
        top.append(app_label)

        spacer = Gtk.Box()
        spacer.set_hexpand(True)
        top.append(spacer)

        time_label = Gtk.Label(label=notif.time_label)
        time_label.add_css_class("caption")
        time_label.add_css_class("dim-label")
        top.append(time_label)

        # Title
        title_label = Gtk.Label(label=notif.title or notif.ticker or "")
        title_label.set_xalign(0)
        title_label.set_ellipsize(3)  # PANGO_ELLIPSIZE_END
        title_label.add_css_class("heading")
        text_box.append(title_label)

        # Body preview
        body = notif.text or ""
        if body:
            body_label = Gtk.Label(label=body[:120])
            body_label.set_xalign(0)
            body_label.set_ellipsize(3)
            body_label.set_lines(2)
            body_label.set_wrap(True)
            body_label.add_css_class("dim-label")
            text_box.append(body_label)

        # Reply indicator
        if notif.can_reply:
            reply_icon = Gtk.Image.new_from_icon_name("mail-reply-sender-symbolic")
            reply_icon.set_pixel_size(14)
            reply_icon.set_opacity(0.4)
            reply_icon.set_tooltip_text("Repliable")
            self.append(reply_icon)

    def _add_fallback_icon(self, app_name: str):
        icon_map = {
            "Messages": "mail-unread-symbolic",
            "Messenger": "user-available-symbolic",
            "WhatsApp": "user-available-symbolic",
            "Calendar": "x-office-calendar-symbolic",
            "Gmail": "mail-unread-symbolic",
            "Phone": "call-start-symbolic",
            "Clock": "alarm-symbolic",
            "Chrome": "web-browser-symbolic",
            "Firefox": "web-browser-symbolic",
        }
        icon_name = "dialog-information-symbolic"
        for key, val in icon_map.items():
            if key.lower() in (app_name or "").lower():
                icon_name = val
                break
        icon = Gtk.Image.new_from_icon_name(icon_name)
        icon.set_pixel_size(32)
        icon.add_css_class("notif-icon")
        self.append(icon)


# ── Detail view ────────────────────────────────────────────────────


class NotificationDetail(Gtk.Box):
    """Right-side detail view for a selected notification."""

    __gsignals__ = {
        "dismiss-notification": (GObject.SignalFlags.RUN_FIRST, None, (str,)),
        "reply-notification": (GObject.SignalFlags.RUN_FIRST, None, (str, str)),
    }

    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        self.set_hexpand(True)
        self.set_vexpand(True)
        self._current_notif: Notification | None = None

        # Empty state
        self._empty = Adw.StatusPage()
        self._empty.set_icon_name("bell-outline-symbolic")
        self._empty.set_title("No Notification Selected")
        self._empty.set_description("Select a notification from the list.")
        self._empty.set_vexpand(True)

        # Detail content
        self._detail_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        self._detail_box.set_vexpand(True)

        # Header
        self._header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        self._header.set_margin_top(16)
        self._header.set_margin_bottom(12)
        self._header.set_margin_start(20)
        self._header.set_margin_end(20)
        self._detail_box.append(self._header)

        self._header_icon = Gtk.Image()
        self._header_icon.set_pixel_size(48)
        self._header.append(self._header_icon)

        header_text = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        header_text.set_hexpand(True)
        self._header.append(header_text)

        self._detail_app = Gtk.Label()
        self._detail_app.set_xalign(0)
        self._detail_app.add_css_class("caption")
        self._detail_app.add_css_class("dim-label")
        header_text.append(self._detail_app)

        self._detail_title = Gtk.Label()
        self._detail_title.set_xalign(0)
        self._detail_title.set_wrap(True)
        self._detail_title.add_css_class("title-2")
        header_text.append(self._detail_title)

        self._detail_time = Gtk.Label()
        self._detail_time.add_css_class("caption")
        self._detail_time.add_css_class("dim-label")
        self._header.append(self._detail_time)

        self._detail_box.append(Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL))

        # Body area (scrollable)
        body_scroll = Gtk.ScrolledWindow()
        body_scroll.set_vexpand(True)
        body_scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        self._detail_box.append(body_scroll)

        body_pad = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        body_pad.set_margin_top(16)
        body_pad.set_margin_bottom(16)
        body_pad.set_margin_start(20)
        body_pad.set_margin_end(20)
        body_scroll.set_child(body_pad)

        self._detail_body = Gtk.Label()
        self._detail_body.set_xalign(0)
        self._detail_body.set_wrap(True)
        self._detail_body.set_selectable(True)
        body_pad.append(self._detail_body)

        # Action bar
        self._action_bar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        self._action_bar.set_margin_top(8)
        self._action_bar.set_margin_bottom(12)
        self._action_bar.set_margin_start(20)
        self._action_bar.set_margin_end(20)
        self._detail_box.append(self._action_bar)

        # Reply entry + button
        self._reply_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        self._reply_box.set_hexpand(True)

        self._reply_entry = Gtk.Entry()
        self._reply_entry.set_placeholder_text("Type a reply…")
        self._reply_entry.set_hexpand(True)
        self._reply_entry.connect("activate", self._on_send_reply)
        self._reply_box.append(self._reply_entry)

        send_btn = Gtk.Button(icon_name="mail-send-symbolic")
        send_btn.add_css_class("suggested-action")
        send_btn.set_tooltip_text("Send reply")
        send_btn.connect("clicked", self._on_send_reply)
        self._reply_box.append(send_btn)

        self._action_bar.append(self._reply_box)

        # Dismiss button
        self._dismiss_btn = Gtk.Button(label="Dismiss")
        self._dismiss_btn.add_css_class("destructive-action")
        self._dismiss_btn.set_tooltip_text("Dismiss notification on phone")
        self._dismiss_btn.connect("clicked", self._on_dismiss)
        self._action_bar.append(self._dismiss_btn)

        # Stack
        self._stack = Gtk.Stack()
        self._stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self._stack.add_named(self._empty, "empty")
        self._stack.add_named(self._detail_box, "detail")
        self._stack.set_visible_child_name("empty")
        self.append(self._stack)

    def show_notification(self, notif: Notification | None):
        self._current_notif = notif
        if not notif:
            self._stack.set_visible_child_name("empty")
            return

        # Icon
        if notif.has_icon and notif.icon_path:
            try:
                pixbuf = GdkPixbuf.Pixbuf.new_from_file_at_scale(
                    notif.icon_path, 48, 48, True
                )
                self._header_icon.set_from_pixbuf(pixbuf)
            except GLib.Error:
                self._header_icon.set_from_icon_name("dialog-information-symbolic")
        else:
            self._header_icon.set_from_icon_name("dialog-information-symbolic")

        self._detail_app.set_label(notif.app_name or "Unknown App")
        self._detail_title.set_label(notif.title or notif.ticker or "")
        self._detail_time.set_label(notif.time_label)
        self._detail_body.set_label(notif.text or notif.ticker or "(no content)")

        # Show/hide reply
        self._reply_box.set_visible(notif.can_reply)
        self._reply_entry.set_text("")

        # Show/hide dismiss
        self._dismiss_btn.set_visible(notif.dismissable)

        self._stack.set_visible_child_name("detail")

    def _on_send_reply(self, _widget):
        if not self._current_notif:
            return
        text = self._reply_entry.get_text().strip()
        if not text:
            return
        self.emit("reply-notification", self._current_notif.public_id, text)
        self._reply_entry.set_text("")

    def _on_dismiss(self, _btn):
        if self._current_notif:
            self.emit("dismiss-notification", self._current_notif.public_id)


# ── Main panel ─────────────────────────────────────────────────────


class NotificationsPanel(Gtk.Box):
    """Full notifications panel with list + detail view."""

    def __init__(self, client):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.client = client
        self._device = None
        self._notifications: dict[str, Notification] = {}  # public_id → Notification
        self._signal_ids: list[int] = []
        self._selected_id: str | None = None

        # Stack: disconnected vs content
        self._stack = Gtk.Stack()
        self._stack.set_vexpand(True)
        self._stack.set_hexpand(True)
        self._stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.append(self._stack)

        # Status page
        self._status = Adw.StatusPage()
        self._status.set_icon_name("bell-outline-symbolic")
        self._status.set_title("Notifications")
        self._status.set_description("No device connected.\nPair a phone to see notifications.")
        self._stack.add_named(self._status, "status")

        # Content: list + detail
        content = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        self._stack.add_named(content, "content")

        # Left: notification list
        list_box_outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        list_box_outer.set_size_request(340, -1)
        content.append(list_box_outer)

        # Header with count + refresh
        list_header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        list_header.set_margin_top(8)
        list_header.set_margin_bottom(4)
        list_header.set_margin_start(12)
        list_header.set_margin_end(12)
        list_box_outer.append(list_header)

        self._count_label = Gtk.Label(label="Notifications")
        self._count_label.set_xalign(0)
        self._count_label.set_hexpand(True)
        self._count_label.add_css_class("heading")
        list_header.append(self._count_label)

        refresh_btn = Gtk.Button(icon_name="view-refresh-symbolic")
        refresh_btn.set_tooltip_text("Refresh notifications")
        refresh_btn.add_css_class("flat")
        refresh_btn.connect("clicked", self._on_refresh)
        list_header.append(refresh_btn)

        # Scrollable list
        scroll = Gtk.ScrolledWindow()
        scroll.set_vexpand(True)
        scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        list_box_outer.append(scroll)

        self._list_box = Gtk.ListBox()
        self._list_box.set_selection_mode(Gtk.SelectionMode.SINGLE)
        self._list_box.add_css_class("navigation-sidebar")
        self._list_box.connect("row-selected", self._on_row_selected)
        scroll.set_child(self._list_box)

        # Empty list placeholder
        empty_row = Adw.StatusPage()
        empty_row.set_icon_name("bell-outline-symbolic")
        empty_row.set_title("No Notifications")
        empty_row.set_description("Your phone has no active notifications.")
        self._list_box.set_placeholder(empty_row)

        content.append(Gtk.Separator(orientation=Gtk.Orientation.VERTICAL))

        # Right: detail view
        self._detail = NotificationDetail()
        self._detail.connect("dismiss-notification", self._on_dismiss)
        self._detail.connect("reply-notification", self._on_reply)
        content.append(self._detail)

    # ── Device connection ──────────────────────────────────────────

    def set_device(self, device):
        old = self._device
        self._device = device

        if device and device.reachable:
            if not old or old.id != device.id or not old.reachable:
                self._subscribe_signals(device.id)
                self._load_notifications(device.id)
            self._stack.set_visible_child_name("content")
        elif device:
            self._status.set_description(
                f"{device.name} is disconnected.\n"
                "Connect your phone to see notifications."
            )
            self._stack.set_visible_child_name("status")
        else:
            self._status.set_description(
                "No device connected.\nPair a phone to see notifications."
            )
            self._stack.set_visible_child_name("status")

    # ── D-Bus signal subscriptions ─────────────────────────────────

    def _subscribe_signals(self, device_id: str):
        # Unsub old
        if self.client.bus:
            for sid in self._signal_ids:
                self.client.bus.signal_unsubscribe(sid)
        self._signal_ids.clear()

        notif_path = f"/modules/kdeconnect/devices/{device_id}"

        for signal_name, handler in [
            ("notificationPosted", self._on_notif_posted),
            ("notificationRemoved", self._on_notif_removed),
            ("notificationUpdated", self._on_notif_updated),
            ("allNotificationsRemoved", self._on_all_removed),
        ]:
            sid = self.client.subscribe_signal(
                notif_path, IFACE_NOTIFICATIONS, signal_name, handler
            )
            if sid is not None:
                self._signal_ids.append(sid)

    # ── Load existing notifications ────────────────────────────────

    def _load_notifications(self, device_id: str):
        self._notifications.clear()
        ids = self.client.get_active_notification_ids(device_id)
        for nid in ids:
            props = self.client.get_notification_properties(device_id, nid)
            if props:
                notif = Notification.from_properties(nid, props)
                self._notifications[nid] = notif
        self._rebuild_list()

    # ── Signal handlers ────────────────────────────────────────────

    def _on_notif_posted(self, conn, sender, path, iface, signal, params):
        public_id = params.unpack()[0]
        GLib.idle_add(self._add_or_update_notification, public_id)

    def _on_notif_updated(self, conn, sender, path, iface, signal, params):
        public_id = params.unpack()[0]
        GLib.idle_add(self._add_or_update_notification, public_id)

    def _on_notif_removed(self, conn, sender, path, iface, signal, params):
        public_id = params.unpack()[0]
        GLib.idle_add(self._remove_notification, public_id)

    def _on_all_removed(self, conn, sender, path, iface, signal, params):
        GLib.idle_add(self._clear_all)

    def _add_or_update_notification(self, public_id: str):
        if not self._device:
            return
        props = self.client.get_notification_properties(self._device.id, public_id)
        if props:
            notif = Notification.from_properties(public_id, props)
            self._notifications[public_id] = notif
            self._rebuild_list()
            # If this was the selected one, refresh detail
            if self._selected_id == public_id:
                self._detail.show_notification(notif)

    def _remove_notification(self, public_id: str):
        if public_id in self._notifications:
            del self._notifications[public_id]
            if self._selected_id == public_id:
                self._selected_id = None
                self._detail.show_notification(None)
            self._rebuild_list()

    def _clear_all(self):
        self._notifications.clear()
        self._selected_id = None
        self._detail.show_notification(None)
        self._rebuild_list()

    # ── List management ────────────────────────────────────────────

    def _rebuild_list(self):
        # Remove all rows
        while True:
            row = self._list_box.get_row_at_index(0)
            if row is None:
                break
            self._list_box.remove(row)

        # Sort by timestamp descending (newest first)
        sorted_notifs = sorted(
            self._notifications.values(),
            key=lambda n: n.timestamp,
            reverse=True,
        )

        count = len(sorted_notifs)
        self._count_label.set_label(
            f"Notifications ({count})" if count else "Notifications"
        )

        select_row = None
        for notif in sorted_notifs:
            row_widget = NotificationRow(notif)
            row = Gtk.ListBoxRow()
            row.set_child(row_widget)
            row._notif_id = notif.public_id
            self._list_box.append(row)
            if notif.public_id == self._selected_id:
                select_row = row

        if select_row:
            self._list_box.select_row(select_row)

    def _on_row_selected(self, _listbox, row):
        if row is None:
            self._selected_id = None
            self._detail.show_notification(None)
            return
        nid = row._notif_id
        self._selected_id = nid
        notif = self._notifications.get(nid)
        self._detail.show_notification(notif)

    # ── Actions ────────────────────────────────────────────────────

    def _on_dismiss(self, _widget, public_id: str):
        if not self._device:
            return
        self.client.dismiss_notification(self._device.id, public_id)
        # Remove from local view immediately
        self._remove_notification(public_id)

    def _on_reply(self, _widget, public_id: str, message: str):
        if not self._device:
            return
        notif = self._notifications.get(public_id)
        if notif and notif.reply_id:
            self.client.reply_to_notification(self._device.id, public_id, message)

    def _on_refresh(self, _btn):
        if self._device and self._device.reachable:
            self._load_notifications(self._device.id)
