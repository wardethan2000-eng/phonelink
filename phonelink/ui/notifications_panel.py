"""Notifications panel — compact single-column list for the slide-out tray."""

import threading

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GLib, GdkPixbuf

from phonelink.dbus_client import IFACE_NOTIFICATIONS
from phonelink.models import Notification
from phonelink.settings import get_settings


def diff_notification_rows(existing: dict, desired: dict):
    """Diff two ``{id: signature}`` maps for an in-place list update.

    Pure (no GTK) so it can be unit-tested.  ``remove`` = rows to drop,
    ``add`` = new rows, ``recreate`` = present rows whose content changed,
    ``keep`` = present rows that are untouched (their expansion state must be
    preserved).
    """
    remove = [i for i in existing if i not in desired]
    add = [i for i in desired if i not in existing]
    recreate = [i for i in desired if i in existing and existing[i] != desired[i]]
    keep = [i for i in desired if i in existing and existing[i] == desired[i]]
    return remove, add, recreate, keep


# ── Single notification row (expandable in-place) ─────────────────


class NotifRow(Gtk.ListBoxRow):
    """A notification row that expands to show body + actions."""

    def __init__(self, notif: Notification):
        super().__init__()
        self.notif = notif
        self._expanded = False

        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.set_child(outer)

        # ── Summary row ────────────────────────────────────────────
        summary = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        summary.set_margin_start(12)
        summary.set_margin_end(12)
        summary.set_margin_top(8)
        summary.set_margin_bottom(8)
        outer.append(summary)

        # App icon (32 px)
        self._icon = Gtk.Image()
        self._icon.set_pixel_size(32)
        self._load_icon(notif)
        summary.append(self._icon)

        # Text block
        text = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=1)
        text.set_hexpand(True)
        summary.append(text)

        top_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        text.append(top_row)

        app_label = Gtk.Label(label=notif.app_name or "App")
        app_label.set_xalign(0)
        app_label.set_hexpand(True)
        app_label.set_ellipsize(3)
        app_label.add_css_class("caption")
        app_label.add_css_class("dim-label")
        top_row.append(app_label)

        time_label = Gtk.Label(label=notif.time_label)
        time_label.add_css_class("caption")
        time_label.add_css_class("dim-label")
        top_row.append(time_label)

        title = notif.title or notif.ticker or "(no title)"
        title_label = Gtk.Label(label=title)
        title_label.set_xalign(0)
        title_label.set_ellipsize(3)
        title_label.add_css_class("body")
        text.append(title_label)

        snippet = (notif.text or notif.ticker or "").split("\n")[0]
        if snippet:
            snip_label = Gtk.Label(label=snippet)
            snip_label.set_xalign(0)
            snip_label.set_ellipsize(3)
            snip_label.add_css_class("caption")
            snip_label.add_css_class("dim-label")
            text.append(snip_label)

        # Chevron
        self._chevron = Gtk.Image.new_from_icon_name("go-down-symbolic")
        self._chevron.set_pixel_size(12)
        self._chevron.set_opacity(0.5)
        summary.append(self._chevron)

        # ── Expanded area ───────────────────────────────────────────
        self._revealer = Gtk.Revealer()
        self._revealer.set_transition_type(Gtk.RevealerTransitionType.SLIDE_DOWN)
        self._revealer.set_reveal_child(False)
        outer.append(self._revealer)

        detail = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        detail.set_margin_start(54)   # indent past icon
        detail.set_margin_end(12)
        detail.set_margin_bottom(10)
        self._revealer.set_child(detail)

        # Full body (if longer than snippet)
        if notif.text and notif.text != snippet:
            full_label = Gtk.Label(label=notif.text)
            full_label.set_xalign(0)
            full_label.set_wrap(True)
            full_label.set_selectable(True)
            full_label.add_css_class("body")
            detail.append(full_label)

        # Action buttons row
        btn_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        detail.append(btn_row)

        self._dismiss_btn = None
        if notif.dismissable:
            self._dismiss_btn = Gtk.Button(label="Dismiss")
            self._dismiss_btn.add_css_class("destructive-action")
            self._dismiss_btn.add_css_class("flat")
            btn_row.append(self._dismiss_btn)

        # Reply entry
        self._reply_entry = None
        self._reply_btn = None
        if notif.can_reply:
            reply_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
            reply_box.set_margin_top(4)
            detail.append(reply_box)

            self._reply_entry = Gtk.Entry()
            self._reply_entry.set_hexpand(True)
            self._reply_entry.set_placeholder_text("Reply…")
            reply_box.append(self._reply_entry)

            self._reply_btn = Gtk.Button(icon_name="mail-send-symbolic")
            self._reply_btn.add_css_class("suggested-action")
            reply_box.append(self._reply_btn)

        outer.append(Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL))

    # ── helpers ────────────────────────────────────────────────────

    def _load_icon(self, notif: Notification):
        if notif.has_icon and notif.icon_path:
            try:
                pb = GdkPixbuf.Pixbuf.new_from_file_at_scale(
                    notif.icon_path, 32, 32, True
                )
                self._icon.set_from_pixbuf(pb)
                return
            except GLib.Error:
                pass
        icon_map = {
            "messages": "mail-unread-symbolic",
            "whatsapp": "mail-unread-symbolic",
            "telegram": "mail-unread-symbolic",
            "gmail": "mail-unread-symbolic",
            "mail": "mail-unread-symbolic",
            "phone": "call-start-symbolic",
            "clock": "alarm-symbolic",
            "chrome": "web-browser-symbolic",
            "firefox": "web-browser-symbolic",
            "youtube": "video-x-generic-symbolic",
        }
        name = (notif.app_name or "").lower()
        icon = next(
            (v for k, v in icon_map.items() if k in name),
            "dialog-information-symbolic",
        )
        self._icon.set_from_icon_name(icon)

    def toggle_expand(self):
        self._expanded = not self._expanded
        self._revealer.set_reveal_child(self._expanded)
        self._chevron.set_from_icon_name(
            "go-up-symbolic" if self._expanded else "go-down-symbolic"
        )


# ── Main panel ─────────────────────────────────────────────────────


class NotificationsPanel(Gtk.Box):
    """Compact notification list widget for the collapsible header tray."""

    def __init__(self, client, loom_client=None):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.client = client
        self._loom = loom_client            # LoomPhoneClient | None — the alternate transport
        self._loom_running = False
        self._transport = get_settings().notifications_transport
        self._device = None
        self._notifications: dict[str, Notification] = {}
        self._signal_ids: list[int] = []
        self._rows: dict[str, NotifRow] = {}

        # Stack: status page || list
        self._stack = Gtk.Stack()
        self._stack.set_vexpand(True)
        self._stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.append(self._stack)

        # Status page (no device / disconnected)
        self._status = Adw.StatusPage()
        self._status.set_icon_name("xsi-notifications-symbolic")
        self._status.set_title("No Notifications")
        self._status.set_description("No phone linked yet.")
        self._stack.add_named(self._status, "status")

        # List page
        list_outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self._stack.add_named(list_outer, "list")

        # Mini-header: count + refresh + dismiss-all
        hdr = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        hdr.set_margin_start(12)
        hdr.set_margin_end(4)
        hdr.set_margin_top(4)
        hdr.set_margin_bottom(4)
        list_outer.append(hdr)

        self._count_label = Gtk.Label()
        self._count_label.set_xalign(0)
        self._count_label.set_hexpand(True)
        self._count_label.add_css_class("caption")
        self._count_label.add_css_class("dim-label")
        hdr.append(self._count_label)

        refresh_btn = Gtk.Button(icon_name="view-refresh-symbolic")
        refresh_btn.add_css_class("flat")
        refresh_btn.set_tooltip_text("Refresh")
        refresh_btn.connect("clicked", self._on_refresh)
        hdr.append(refresh_btn)

        clear_btn = Gtk.Button(icon_name="edit-clear-all-symbolic")
        clear_btn.add_css_class("flat")
        clear_btn.set_tooltip_text("Dismiss all")
        clear_btn.connect("clicked", self._on_dismiss_all)
        hdr.append(clear_btn)

        list_outer.append(Gtk.Separator())

        scroll = Gtk.ScrolledWindow()
        scroll.set_vexpand(True)
        scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        list_outer.append(scroll)

        self._list_box = Gtk.ListBox()
        self._list_box.set_selection_mode(Gtk.SelectionMode.NONE)
        self._list_box.set_sort_func(self._sort_notifs)
        self._list_box.connect("row-activated", self._on_row_activated)
        scroll.set_child(self._list_box)

        ph = Adw.StatusPage()
        ph.set_icon_name("xsi-notifications-symbolic")
        ph.set_title("No Notifications")
        ph.set_description("Phone has no active notifications.")
        self._list_box.set_placeholder(ph)

        self._stack.set_visible_child_name("status")

    # ── Device ─────────────────────────────────────────────────────

    def set_device(self, device):
        old = self._device
        self._device = device

        # In Loom mode notifications come from the phone's node, not the KDE Connect device — so a KDE
        # device coming or going doesn't drive the list; we only make sure the Loom source is running.
        if self._transport == "loom":
            if not self._loom_running:
                self._start_loom()
            return

        if device and device.reachable:
            if not old or old.id != device.id or not old.reachable:
                self._subscribe_signals(device.id)
                self._load_notifications(device.id)
            self._stack.set_visible_child_name("list")
        elif device:
            self._status.set_description(
                f"{device.name} is offline.\nReconnect your phone to see notifications."
            )
            self._stack.set_visible_child_name("status")
        else:
            self._status.set_description("No phone linked yet.")
            self._stack.set_visible_child_name("status")

    # ── Transport (KDE Connect ⇆ Loom) ─────────────────────────────

    def apply_transport(self, transport: str):
        """Switch the notification source between ``"kdeconnect"`` and ``"loom"`` live.

        The panel's rows and layout are unchanged — only where the data comes from. Tears the current
        source down, clears the list, and starts the new one.
        """
        transport = transport if transport in ("kdeconnect", "loom") else "kdeconnect"
        if transport == self._transport and (transport != "loom" or self._loom_running):
            return

        # Tear down the old source.
        if self._transport == "loom":
            self._stop_loom()
        else:
            self._unsubscribe_kde()

        self._transport = transport
        self._notifications.clear()
        self._sync_list()

        if transport == "loom":
            self._start_loom()
        else:
            # Resume KDE Connect with the last known device.
            self.set_device(self._device)

    def _start_loom(self):
        if not self._loom:
            self._status.set_description(
                "Loom isn't set up.\nInstall the Loom SDK and start loomd to mirror "
                "notifications over Loom."
            )
            self._stack.set_visible_child_name("status")
            return
        self._loom_running = True
        self._status.set_description("Connecting to your phone over Loom…")
        self._stack.set_visible_child_name("status")
        self._loom.start(
            on_snapshot=lambda notifs: GLib.idle_add(self._loom_apply_snapshot, notifs),
            on_posted=lambda n: GLib.idle_add(self._loom_apply_posted, n),
            on_removed=lambda pid: GLib.idle_add(self._loom_apply_removed, pid),
            on_status=lambda state, detail: GLib.idle_add(self._loom_apply_status, state, detail),
        )

    def _stop_loom(self):
        self._loom_running = False
        if self._loom:
            self._loom.stop()

    def _unsubscribe_kde(self):
        if getattr(self.client, "bus", None):
            for sid in self._signal_ids:
                self.client.bus.signal_unsubscribe(sid)
        self._signal_ids.clear()

    # These run on the GTK thread (via GLib.idle_add), fed by the LoomPhoneClient worker. They reuse
    # the same `_notifications` model + `_sync_list()` as the KDE path — only the source differs.
    def _loom_apply_snapshot(self, notifs):
        if self._transport != "loom":
            return
        self._notifications = {n.public_id: n for n in notifs}
        self._sync_list()
        self._stack.set_visible_child_name("list")

    def _loom_apply_posted(self, notif):
        if self._transport != "loom":
            return
        self._notifications[notif.public_id] = notif
        self._sync_list()
        self._stack.set_visible_child_name("list")

    def _loom_apply_removed(self, public_id):
        if self._transport != "loom":
            return
        self._notifications.pop(public_id, None)
        self._sync_list()

    def _loom_apply_status(self, state, detail):
        if self._transport != "loom":
            return
        if state == "connected":
            self._stack.set_visible_child_name("list")
        elif state in ("connecting", "error") and not self._notifications:
            if state == "error":
                self._status.set_description(detail or "Couldn't reach your phone over Loom.")
            else:
                self._status.set_description(detail or "Connecting to your phone over Loom…")
            self._stack.set_visible_child_name("status")

    # ── D-Bus ──────────────────────────────────────────────────────

    def _subscribe_signals(self, device_id: str):
        if self.client.bus:
            for sid in self._signal_ids:
                self.client.bus.signal_unsubscribe(sid)
        self._signal_ids.clear()

        path = f"/modules/kdeconnect/devices/{device_id}"
        for sig, handler in [
            ("notificationPosted", self._on_notif_posted),
            ("notificationRemoved", self._on_notif_removed),
            ("notificationUpdated", self._on_notif_updated),
            ("allNotificationsRemoved", self._on_all_removed),
        ]:
            sid = self.client.subscribe_signal(path, IFACE_NOTIFICATIONS, sig, handler)
            if sid is not None:
                self._signal_ids.append(sid)

    def _load_notifications(self, device_id: str):
        # Fetch active notifications (1 + N D-Bus calls) on a worker thread.
        self.client.submit(
            self.client.fetch_active_notifications,
            device_id,
            on_result=lambda entries: self._apply_notifications(device_id, entries),
        )

    def _apply_notifications(self, device_id: str, entries: list):
        if not self._device or self._device.id != device_id:
            return
        self._notifications.clear()
        for nid, props in entries:
            self._notifications[nid] = Notification.from_properties(nid, props)
        self._sync_list()

    def _on_notif_posted(self, conn, sender, path, iface, signal, params):
        public_id = params.unpack()[0]
        GLib.idle_add(self._add_or_update, public_id)

    def _on_notif_updated(self, conn, sender, path, iface, signal, params):
        public_id = params.unpack()[0]
        GLib.idle_add(self._add_or_update, public_id)

    def _on_notif_removed(self, conn, sender, path, iface, signal, params):
        public_id = params.unpack()[0]
        GLib.idle_add(self._remove_one, public_id)

    def _on_all_removed(self, conn, sender, path, iface, signal, params):
        GLib.idle_add(self._clear_all)

    def _add_or_update(self, public_id: str):
        if not self._device:
            return
        device_id = self._device.id
        self.client.submit(
            self.client.get_notification_properties,
            device_id,
            public_id,
            on_result=lambda props: self._apply_one(device_id, public_id, props),
        )

    def _apply_one(self, device_id: str, public_id: str, props: dict):
        if not self._device or self._device.id != device_id:
            return
        if props:
            self._notifications[public_id] = Notification.from_properties(public_id, props)
            self._sync_list()

    def _remove_one(self, public_id: str):
        self._notifications.pop(public_id, None)
        self._sync_list()

    def _clear_all(self):
        self._notifications.clear()
        self._sync_list()

    # ── List ───────────────────────────────────────────────────────

    @staticmethod
    def _notif_signature(n: Notification) -> tuple:
        """Display-relevant fields — a row is only recreated when these change."""
        return (
            n.app_name, n.title, n.ticker, n.text,
            n.dismissable, n.can_reply, n.has_icon, n.icon_path,
        )

    def _sort_notifs(self, row_a, row_b) -> int:
        # Newest first.
        ta, tb = row_a.notif.timestamp, row_b.notif.timestamp
        return -1 if ta > tb else (1 if ta < tb else 0)

    def _remove_all_rows(self):
        for public_id in list(self._rows):
            self._list_box.remove(self._rows.pop(public_id))

    def _make_row(self, notif: Notification) -> NotifRow:
        row = NotifRow(notif)
        row._signature = self._notif_signature(notif)
        if row._dismiss_btn:
            row._dismiss_btn.connect(
                "clicked", self._on_dismiss_clicked, notif.public_id
            )
        if row._reply_btn and row._reply_entry:
            row._reply_btn.connect(
                "clicked", self._on_reply_clicked, notif.public_id, row._reply_entry
            )
            row._reply_entry.connect(
                "activate", self._on_reply_enter, notif.public_id
            )
        return row

    def _sync_list(self):
        """Update the list in place — untouched rows keep their expansion state.

        Only the changed rows are removed/added/recreated, so a single incoming
        notification no longer tears down and reconnects the whole list.
        """
        settings = get_settings()
        if not settings.notifications_enabled:
            self._remove_all_rows()
            self._count_label.set_label("Notifications disabled")
            return

        desired = {
            nid: n for nid, n in self._notifications.items()
            if not settings.is_app_ignored(n.app_name or "")
        }
        count = len(desired)
        self._count_label.set_label(
            f"{count} notification{'s' if count != 1 else ''}" if count else "No notifications"
        )

        existing_sig = {nid: row._signature for nid, row in self._rows.items()}
        desired_sig = {nid: self._notif_signature(n) for nid, n in desired.items()}
        remove, add, recreate, _keep = diff_notification_rows(existing_sig, desired_sig)

        for nid in remove:
            self._list_box.remove(self._rows.pop(nid))
        for nid in recreate:
            self._list_box.remove(self._rows.pop(nid))
        for nid in recreate + add:
            row = self._make_row(desired[nid])
            self._rows[nid] = row
            self._list_box.append(row)

        self._list_box.invalidate_sort()

    def _on_row_activated(self, _lb, row):
        if isinstance(row, NotifRow):
            row.toggle_expand()

    # ── Actions ────────────────────────────────────────────────────

    def _loom_action(self, fn, *args):
        """Run a blocking :class:`~phonelink.loom_phone.LoomPhoneClient` action (dismiss/reply) off the
        GTK main loop, so a slow phone dial never freezes the UI. On failure, surface it via the same
        status banner the connection uses (the optimistic row change is already applied; a live
        re-snapshot from the phone corrects the view if the action didn't take)."""
        def worker():
            try:
                fn(*args)
            except Exception as e:  # noqa: BLE001 — network/refusal: report, don't crash the UI
                GLib.idle_add(self._loom_apply_status, "error", f"Couldn't reach your phone: {e}")

        threading.Thread(target=worker, name="loom-phone-action", daemon=True).start()

    def _on_dismiss_clicked(self, _btn, public_id: str):
        # Both transports dismiss on the phone, then drop the row optimistically (the phone stays the
        # source of truth; a re-snapshot restores it if the action didn't take). Over Loom this is the
        # P5 `loom/phone-action/0` request; over KDE Connect it's the D-Bus call.
        if self._transport == "loom":
            if self._loom:
                self._loom_action(self._loom.dismiss_notification, public_id)
        elif self._device:
            self.client.submit(
                self.client.dismiss_notification, self._device.id, public_id
            )
        self._remove_one(public_id)

    def _on_reply_clicked(self, _btn, public_id: str, entry: Gtk.Entry):
        self._send_reply(public_id, entry)

    def _on_reply_enter(self, entry: Gtk.Entry, public_id: str):
        self._send_reply(public_id, entry)

    def _send_reply(self, public_id: str, entry: Gtk.Entry):
        text = entry.get_text().strip()
        if not text:
            return
        notif = self._notifications.get(public_id)
        if self._transport == "loom":
            # P5 reply over `loom/phone-action/0`: send the notification's captured reply token + text.
            if self._loom and notif and notif.reply_id:
                self._loom_action(self._loom.reply_to_notification, notif.reply_id, text)
            entry.set_text("")
            return
        # KDE Connect: reply keyed by the notification's public_id.
        if not self._device:
            return
        if notif and notif.reply_id:
            self.client.submit(
                self.client.reply_to_notification, self._device.id, public_id, text
            )
        entry.set_text("")

    def _on_refresh(self, _btn):
        if self._transport == "loom":
            # The Loom subscription is a live stream; force a fresh snapshot by reconnecting.
            if self._loom_running:
                self._start_loom()
            return
        if self._device and self._device.reachable:
            self._load_notifications(self._device.id)

    def _on_dismiss_all(self, _btn):
        dismissable_ids = [
            nid for nid, notif in self._notifications.items()
            if notif and notif.dismissable
        ]
        if self._transport == "loom":
            if self._loom and dismissable_ids:
                self._loom_action(self._dismiss_many_loom, dismissable_ids)
        elif self._device:
            if dismissable_ids:
                self.client.submit(
                    self._dismiss_many, self._device.id, dismissable_ids
                )
        self._clear_all()

    def _dismiss_many_loom(self, ids: list):
        for nid in ids:
            self._loom.dismiss_notification(nid)

    def _dismiss_many(self, device_id: str, ids: list):
        for nid in ids:
            self.client.dismiss_notification(device_id, nid)
