"""SMS panel — conversation list + message thread, backed by D-Bus signals."""

import re

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, Gio, GLib

from phonelink.dbus_client import IFACE_CONVERSATIONS, IFACE_NOTIFICATIONS, IFACE_SHARE
from phonelink.contacts import (
    load_contact_map, resolve_name, import_vcf_file,
    harvest_contacts_from_notifications,
    harvest_contact_from_notification_signal,
)
from phonelink.models import Conversation, SmsMessage
from phonelink.ui.conversation_list import ConversationList
from phonelink.ui.message_thread import MessageThread

# KDE Connect conversation tuple indices
# (event, body, addresses, date, type, read, threadID, uID, subID, attachments)
_I_EVENT = 0
_I_BODY = 1
_I_ADDRS = 2
_I_DATE = 3
_I_TYPE = 4
_I_READ = 5
_I_THREAD = 6
_I_UID = 7
_I_SUB = 8
_I_ATT = 9


def _parse_message_tuple(t) -> SmsMessage | None:
    """Parse a KDE Connect message tuple into an SmsMessage."""
    try:
        if not isinstance(t, (tuple, list)) or len(t) < 8:
            return None
        # Extract first address from addresses list
        # addresses looks like [('22550',)] or [('+13166551221',), ...]
        addrs = t[_I_ADDRS]
        address = ""
        if addrs and len(addrs) > 0:
            first = addrs[0]
            if isinstance(first, (tuple, list)) and len(first) > 0:
                address = str(first[0])
            else:
                address = str(first)

        return SmsMessage(
            uid=int(t[_I_UID]),
            body=str(t[_I_BODY]),
            address=address,
            date=int(t[_I_DATE]),
            msg_type=int(t[_I_TYPE]),
            read=int(t[_I_READ]),
            thread_id=int(t[_I_THREAD]),
        )
    except (IndexError, TypeError, ValueError) as e:
        print(f"[phonelink] Failed to parse message tuple: {e}")
        return None


class SmsPanel(Gtk.Box):
    def __init__(self, client):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.client = client
        self._device = None
        self._conversations: dict[int, Conversation] = {}  # thread_id → Conversation
        self._active_thread_id: int | None = None
        self._signal_ids: list[int] = []
        self._contact_map: dict[str, str] = {}  # normalised phone → display name

        # ── Stack: disconnected status vs content ──────────────────
        self._stack = Gtk.Stack()
        self._stack.set_vexpand(True)
        self._stack.set_hexpand(True)
        self._stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.append(self._stack)

        # Disconnected / empty status page
        self._status = Adw.StatusPage()
        self._status.set_icon_name("mail-unread-symbolic")
        self._status.set_title("Messages")
        self._status.set_description("No device connected.\nPair a phone to view messages.")
        self._stack.add_named(self._status, "status")

        # Message UI: conversation list + thread
        content = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        self._stack.add_named(content, "content")

        # Left: conversation list
        self._conv_list = ConversationList()
        self._conv_list.connect("conversation-selected", self._on_conversation_selected)
        self._conv_list.connect("new-conversation", self._on_new_conversation)
        self._conv_list.connect("rename-contact", self._on_rename_contact)
        self._conv_list.connect("import-contacts", self._on_import_contacts)
        content.append(self._conv_list)

        content.append(Gtk.Separator(orientation=Gtk.Orientation.VERTICAL))

        # Right: message thread
        self._thread = MessageThread()
        self._thread.connect("send-message", self._on_send_message)
        content.append(self._thread)

        self._stack.set_visible_child_name("status")

    # ── Device switching ───────────────────────────────────────────

    def set_device(self, device):
        old_id = self._device.id if self._device else None
        self._device = device

        if not device or not device.reachable:
            self._show_disconnected()
            self._unsubscribe_signals()
            return

        self._stack.set_visible_child_name("content")

        # Only re-request if the device changed
        if device.id != old_id:
            self._unsubscribe_signals()
            self._conversations.clear()
            self._conv_list.set_conversations([])
            self._thread.show_empty()
            self._active_thread_id = None
            # Load contact names from local store + vCard cache
            self._contact_map = load_contact_map(device.id)
            self._subscribe_signals(device.id)
            # Load cached conversations synchronously first
            self._load_active_conversations(device.id)
            # Then ask for a fresh sync from the phone
            self.client.request_all_conversations(device.id)
            # Harvest contact names from active SMS notifications
            self._harvest_from_notifications(device.id)
            # Scan Downloads for any VCF files shared from the phone
            self._scan_downloads_for_vcf(device.id)

    def _load_active_conversations(self, device_id):
        """Load cached conversations from the daemon via activeConversations()."""
        raw_list = self.client.get_active_conversations(device_id)
        count = 0
        for entry in raw_list:
            msg = _parse_message_tuple(entry)
            if msg:
                self._merge_message(msg)
                count += 1
        if count:
            self._refresh_conversation_list()

    def _show_disconnected(self):
        if self._device:
            self._status.set_description(
                f"{self._device.name} is disconnected.\n"
                "Connect your phone to view messages."
            )
        else:
            self._status.set_description(
                "No device connected.\nPair a phone to view messages."
            )
        self._stack.set_visible_child_name("status")

    # ── D-Bus signal wiring ────────────────────────────────────────

    def _subscribe_signals(self, device_id):
        path = f"/modules/kdeconnect/devices/{device_id}"

        for signal_name, handler in (
            ("conversationCreated", self._on_dbus_conversation_signal),
            ("conversationUpdated", self._on_dbus_conversation_signal),
            ("conversationLoaded", self._on_dbus_conversation_loaded),
            ("conversationRemoved", self._on_dbus_conversation_removed),
        ):
            sid = self.client.subscribe_signal(
                path, IFACE_CONVERSATIONS, signal_name, handler
            )
            if sid is not None:
                self._signal_ids.append(sid)

        # Listen for contact sync completion to reload names
        from phonelink.dbus_client import IFACE_CONTACTS
        sid = self.client.subscribe_signal(
            path + "/contacts", IFACE_CONTACTS,
            "localCacheSynchronized", self._on_contacts_synced
        )
        if sid is not None:
            self._signal_ids.append(sid)

        # Listen for new notifications to harvest SMS contact names
        sid = self.client.subscribe_signal(
            path + "/notifications", IFACE_NOTIFICATIONS,
            "notificationPosted", self._on_notification_posted
        )
        if sid is not None:
            self._signal_ids.append(sid)

        # Listen for files shared from the phone (e.g. contacts VCF)
        sid = self.client.subscribe_signal(
            path + "/share", IFACE_SHARE,
            "shareReceived", self._on_share_received
        )
        if sid is not None:
            self._signal_ids.append(sid)

    def _unsubscribe_signals(self):
        if self.client.bus:
            for sid in self._signal_ids:
                self.client.bus.signal_unsubscribe(sid)
        self._signal_ids.clear()

    # ── D-Bus signal handlers ──────────────────────────────────────

    def _on_dbus_conversation_signal(self, conn, sender, path, iface, sig, params):
        """Handle conversationCreated / conversationUpdated signals.

        Signal payload is (v,) — a variant wrapping a message tuple.
        """
        GLib.idle_add(self._handle_signal_variant, params)

    def _on_dbus_conversation_loaded(self, conn, sender, path, iface, sig, params):
        """A conversation finished loading — reload from cache."""
        if self._device:
            GLib.idle_add(self._load_active_conversations, self._device.id)

    def _on_contacts_synced(self, conn, sender, path, iface, sig, params):
        """Contact cache was updated — reload names."""
        GLib.idle_add(self._reload_contact_names)

    def _on_notification_posted(self, conn, sender, path, iface, sig, params):
        """A new notification arrived — check if it's an SMS with a contact name."""
        notif_id = params.unpack()[0] if params else None
        if notif_id and self._device:
            GLib.idle_add(self._handle_notification, notif_id)

    def _handle_notification(self, notif_id):
        """Check a new notification for SMS contact name info."""
        if not self._device:
            return
        result = harvest_contact_from_notification_signal(
            self.client, self._device.id, notif_id,
            self._conversations, self._contact_map
        )
        if result:
            norm_phone, name = result
            self._contact_map[norm_phone] = name
            # Update matching conversations
            for conv in self._conversations.values():
                from phonelink.contacts import _normalize_phone
                if _normalize_phone(conv.address) == norm_phone:
                    conv.display_name = name
            self._refresh_conversation_list()

    def _harvest_from_notifications(self, device_id):
        """Scan active notifications for SMS contact names (initial load)."""
        count = harvest_contacts_from_notifications(
            self.client, device_id, self._conversations
        )
        if count:
            self._contact_map = load_contact_map(device_id)
            self._reload_contact_names()

    def _scan_downloads_for_vcf(self, device_id):
        """Check Downloads folder for VCF files to auto-import."""
        import os
        from pathlib import Path
        downloads = Path.home() / "Downloads"
        if not downloads.is_dir():
            return
        # Find the most recent VCF file (shared from phone)
        vcf_files = sorted(
            [f for f in downloads.iterdir() if f.suffix.lower() == ".vcf"],
            key=lambda f: f.stat().st_mtime,
            reverse=True,
        )
        if not vcf_files:
            return
        # Only auto-import if we have very few contacts (first run scenario)
        if len(self._contact_map) > 5:
            return
        newest = vcf_files[0]
        count = import_vcf_file(str(newest))
        if count:
            self._contact_map = load_contact_map(device_id)
            self._reload_contact_names()
            print(f"[phonelink] Auto-imported {count} contacts from {newest.name}")

    def _on_share_received(self, conn, sender, path, iface, sig, params):
        """A file was shared from the phone — check if it's a contacts VCF."""
        url = params.unpack()[0] if params else ""
        if url:
            GLib.idle_add(self._handle_shared_file, url)

    def _handle_shared_file(self, url):
        """Auto-import a VCF file shared from the phone."""
        # url is typically a file:// URI or a local path
        file_path = url
        if file_path.startswith("file://"):
            from urllib.parse import unquote, urlparse
            file_path = unquote(urlparse(file_path).path)

        if not file_path.lower().endswith(".vcf"):
            return

        count = import_vcf_file(file_path)
        if count and self._device:
            self._contact_map = load_contact_map(self._device.id)
            self._reload_contact_names()
            # Notify user
            info = Adw.MessageDialog(
                transient_for=self.get_root(),
                heading="Contacts Imported",
                body=f"Automatically imported {count} contact names from shared file.",
            )
            info.add_response("ok", "OK")
            info.present()

    def _on_dbus_conversation_removed(self, conn, sender, path, iface, sig, params):
        unpacked = params.unpack()
        if unpacked:
            thread_id = unpacked[0] if isinstance(unpacked, tuple) else unpacked
            GLib.idle_add(self._remove_conversation, int(thread_id))

    def _handle_signal_variant(self, params):
        """Parse a signal variant containing a message tuple."""
        try:
            # params is a GLib.Variant with type "(v)" — one variant child
            child = params.get_child_value(0)  # the inner variant
            unpacked = child.unpack()

            # Should be a tuple like (event, body, addrs, date, type, read, threadID, uID, subID, atts)
            msg = _parse_message_tuple(unpacked)
            if msg:
                self._merge_message(msg)
                self._refresh_conversation_list()
                if self._active_thread_id == msg.thread_id:
                    self._show_thread(msg.thread_id)
        except Exception as e:
            print(f"[phonelink] SMS signal parse error: {e}")

    # ── Message ingestion ──────────────────────────────────────────

    def _merge_message(self, msg: SmsMessage):
        """Merge a parsed SmsMessage into the conversation store."""
        conv = self._conversations.get(msg.thread_id)
        if conv is None:
            display = resolve_name(self._contact_map, msg.address)
            conv = Conversation(
                thread_id=msg.thread_id,
                address=msg.address,
                display_name=display,
            )
            self._conversations[msg.thread_id] = conv

        # Avoid duplicates
        existing_uids = {m.uid for m in conv.messages}
        if msg.uid and msg.uid not in existing_uids:
            conv.messages.append(msg)

        # Update conversation metadata if this is the newest message
        if msg.date >= conv.last_date:
            conv.last_date = msg.date
            conv.last_message = msg.body
            conv.is_read = bool(msg.read)

    def _remove_conversation(self, thread_id):
        self._conversations.pop(thread_id, None)
        if self._active_thread_id == thread_id:
            self._thread.show_empty()
            self._active_thread_id = None
        self._refresh_conversation_list()

    def _reload_contact_names(self):
        """Reload vCard cache and update all conversation display names."""
        if not self._device:
            return
        self._contact_map = load_contact_map(self._device.id)
        if not self._contact_map:
            return
        for conv in self._conversations.values():
            new_name = resolve_name(self._contact_map, conv.address)
            conv.display_name = new_name
        self._conv_list.set_conversations(
            list(self._conversations.values()), force_rebuild=True
        )
        if self._active_thread_id:
            self._conv_list.select_thread(self._active_thread_id)

    def _refresh_conversation_list(self):
        self._conv_list.set_conversations(list(self._conversations.values()))
        if self._active_thread_id:
            self._conv_list.select_thread(self._active_thread_id)

    # ── UI event handlers ──────────────────────────────────────────

    def _on_conversation_selected(self, widget, thread_id):
        self._active_thread_id = thread_id
        conv = self._conversations.get(thread_id)
        if not conv:
            return

        # If we have few messages, request more from the phone
        if self._device and len(conv.messages) < 20:
            self._thread.show_loading(conv.display_name, conv.address)
            self.client.request_conversation(self._device.id, thread_id, 0, 50)
            # Show what we have while waiting
            GLib.timeout_add(300, lambda: self._show_thread(thread_id) or False)
        else:
            self._show_thread(thread_id)

    def _show_thread(self, thread_id):
        conv = self._conversations.get(thread_id)
        if not conv:
            return
        self._thread.set_messages(
            conv.messages,
            contact_name=conv.display_name,
            address=conv.address,
            thread_id=thread_id,
        )

    def _on_new_conversation(self, widget):
        """Show a dialog to compose a new message."""
        if not self._device or not self._device.reachable:
            return

        dialog = Adw.MessageDialog(
            transient_for=self.get_root(),
            heading="New Message",
            body="Enter a phone number and message:",
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("send", "Send")
        dialog.set_response_appearance("send", Adw.ResponseAppearance.SUGGESTED)

        content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        content.set_margin_start(12)
        content.set_margin_end(12)

        number_entry = Gtk.Entry()
        number_entry.set_placeholder_text("Phone number")
        content.append(number_entry)

        msg_entry = Gtk.Entry()
        msg_entry.set_placeholder_text("Message")
        content.append(msg_entry)

        dialog.set_extra_child(content)

        def on_response(dlg, response):
            if response == "send":
                number = number_entry.get_text().strip()
                text = msg_entry.get_text().strip()
                if number and text:
                    self.client.send_sms(self._device.id, [number], text)

        dialog.connect("response", on_response)
        dialog.present()

    def _on_send_message(self, widget, thread_id, text):
        """Handle send from the compose bar."""
        if not self._device or not self._device.reachable:
            return
        self.client.reply_to_conversation(self._device.id, thread_id, text)

    def _on_rename_contact(self, widget, thread_id, address):
        """Show a dialog to set a contact name for a phone number."""
        from phonelink.contacts import save_contact

        conv = self._conversations.get(thread_id)
        current_name = conv.display_name if conv else address

        dialog = Adw.MessageDialog(
            transient_for=self.get_root(),
            heading="Set Contact Name",
            body=f"Enter a name for {address}:",
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("save", "Save")
        dialog.set_response_appearance("save", Adw.ResponseAppearance.SUGGESTED)

        name_entry = Gtk.Entry()
        name_entry.set_text(current_name if current_name != address else "")
        name_entry.set_placeholder_text("Contact name")
        name_entry.set_margin_start(12)
        name_entry.set_margin_end(12)
        dialog.set_extra_child(name_entry)

        def on_response(dlg, response):
            if response == "save":
                name = name_entry.get_text().strip()
                if name:
                    save_contact(address, name)
                    # Update all conversations with this address
                    for c in self._conversations.values():
                        if c.address == address:
                            c.display_name = name
                    self._contact_map[re.sub(r"[^\d]", "", address)] = name
                    self._refresh_conversation_list()

        dialog.connect("response", on_response)
        dialog.present()

    def _on_import_contacts(self, widget):
        """Show contacts sync dialog with options."""
        from phonelink.contacts import import_google_csv, import_vcf_file

        dialog = Adw.MessageDialog(
            transient_for=self.get_root(),
            heading="Sync Contacts",
            body=(
                "To get all your contact names, share your contacts "
                "from your phone:\n\n"
                "1. Open Contacts on your Galaxy S25\n"
                "2. Tap ⋮ menu → Share\n"
                "3. Select all contacts\n"
                "4. Share via KDE Connect to this PC\n\n"
                "Contact names will appear automatically.\n\n"
                "Or import a file manually:"
            ),
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("file", "Import File…")
        dialog.set_response_appearance("file", Adw.ResponseAppearance.SUGGESTED)

        def on_response(dlg, response):
            if response == "file":
                self._open_contact_file_chooser()

        dialog.connect("response", on_response)
        dialog.present()

    def _open_contact_file_chooser(self):
        """Open a file chooser for CSV or VCF contact files."""
        from phonelink.contacts import import_google_csv, import_vcf_file

        dialog = Gtk.FileDialog()
        dialog.set_title("Import Contacts")
        all_filter = Gtk.FileFilter()
        all_filter.set_name("Contact files (*.vcf, *.csv)")
        all_filter.add_pattern("*.vcf")
        all_filter.add_pattern("*.csv")
        filters = Gio.ListStore.new(Gtk.FileFilter)
        filters.append(all_filter)
        dialog.set_filters(filters)

        def on_open(dlg, result):
            try:
                f = dlg.open_finish(result)
                if f:
                    path = f.get_path()
                    if path.lower().endswith(".vcf"):
                        count = import_vcf_file(path)
                    else:
                        count = import_google_csv(path)
                    if self._device:
                        self._contact_map = load_contact_map(self._device.id)
                        self._reload_contact_names()
                    info = Adw.MessageDialog(
                        transient_for=self.get_root(),
                        heading="Import Complete",
                        body=f"Imported {count} contact names.",
                    )
                    info.add_response("ok", "OK")
                    info.present()
            except GLib.Error:
                pass

        dialog.open(self.get_root(), None, on_open)
