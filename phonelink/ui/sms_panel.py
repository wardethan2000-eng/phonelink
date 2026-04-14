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
        # Extract all addresses from addresses list
        # addresses looks like [('22550',)] or [('+13166551221',), ('+13165559999',)]
        addrs = t[_I_ADDRS]
        raw_addresses = []
        if addrs:
            for entry in addrs:
                if isinstance(entry, (tuple, list)) and len(entry) > 0:
                    raw_addresses.append(str(entry[0]))
                else:
                    raw_addresses.append(str(entry))

        # Deduplicate addresses by normalized phone number (KDE Connect
        # often sends the same number twice for MMS-style threads).
        all_addresses = []
        seen_norms: set[str] = set()
        for a in raw_addresses:
            norm = re.sub(r"[^\d]", "", a)
            key = norm[-10:] if len(norm) >= 10 else norm
            if key and key in seen_norms:
                continue
            if key:
                seen_norms.add(key)
            all_addresses.append(a)

        address = all_addresses[0] if all_addresses else ""

        msg = SmsMessage(
            uid=int(t[_I_UID]),
            body=str(t[_I_BODY]),
            address=address,
            date=int(t[_I_DATE]),
            msg_type=int(t[_I_TYPE]),
            read=int(t[_I_READ]),
            thread_id=int(t[_I_THREAD]),
        )
        # Stash all addresses for group chat detection
        msg._all_addresses = all_addresses
        return msg
    except (IndexError, TypeError, ValueError) as e:
        print(f"[phonelink] Failed to parse message tuple: {e}")
        return None


class SmsPanel(Gtk.Box):
    def __init__(self, client):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.client = client
        self._device = None
        self._conversations: dict[int, Conversation] = {}  # thread_id → Conversation
        self._thread_redirects: dict[int, int] = {}  # secondary thread_id → primary thread_id
        self._active_thread_id: int | None = None
        self._signal_ids: list[int] = []
        self._contact_map: dict[str, str] = {}  # normalised phone → display name
        self._self_number: str = ""  # user's own phone number (last-10 digits), detected from data
        self._read_thread_ids: set[int] = set()  # threads the user has opened this session
        self._was_reachable: bool = False  # track reachability transitions

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

        # Message UI: fixed-width conversation pane + flexible thread pane
        content = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        self._stack.add_named(content, "content")

        # Left: conversation list
        sidebar = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        sidebar.set_size_request(280, -1)
        sidebar.set_hexpand(False)

        self._conv_list = ConversationList()
        self._conv_list.connect("conversation-selected", self._on_conversation_selected)
        self._conv_list.connect("start-conversation", self._on_start_conversation)
        self._conv_list.connect("rename-contact", self._on_rename_contact)
        self._conv_list.connect("delete-conversation", self._on_delete_conversation)
        self._conv_list.connect("import-contacts", self._on_import_contacts)
        sidebar.append(self._conv_list)
        content.append(sidebar)

        content.append(Gtk.Separator(orientation=Gtk.Orientation.VERTICAL))

        # Right: message thread
        self._thread = MessageThread()
        self._thread.connect("send-message", self._on_send_message)
        self._thread.connect("send-message-with-attachment", self._on_send_message_with_attachment)
        content.append(self._thread)

        self._stack.set_visible_child_name("status")

    # ── Device switching ───────────────────────────────────────────

    def set_device(self, device):
        old_id = self._device.id if self._device else None
        self._device = device

        if not device:
            self._show_disconnected()
            self._unsubscribe_signals()
            return

        if not device.reachable:
            # Keep existing conversations visible if we have any;
            # only show the disconnected status page when we have nothing.
            # Keep signal subscriptions alive so we pick up the reconnection
            # instantly (D-Bus signals resume when the phone comes back).
            if self._conversations:
                self._stack.set_visible_child_name("content")
            else:
                self._show_disconnected()
            self._was_reachable = False
            return

        self._stack.set_visible_child_name("content")
        just_reconnected = not self._was_reachable
        self._was_reachable = True

        if device.id != old_id:
            # New device — full reset
            self._unsubscribe_signals()
            self._conversations.clear()
            self._thread_redirects.clear()
            self._read_thread_ids.clear()
            self._conv_list.set_conversations([])
            self._thread.show_empty()
            self._active_thread_id = None
            self._contact_map = load_contact_map(device.id)
            self._subscribe_signals(device.id)
            self._load_active_conversations(device.id)
            self.client.request_all_conversations(device.id)
            self._harvest_from_notifications(device.id)
            self._scan_downloads_for_vcf(device.id)
        elif not self._signal_ids or just_reconnected:
            # Same device reconnected — re-subscribe signals and refresh
            if not self._signal_ids:
                self._subscribe_signals(device.id)
            self._load_active_conversations(device.id)
            self.client.request_all_conversations(device.id)

    def _load_active_conversations(self, device_id):
        """Load cached conversations from the daemon via activeConversations()."""
        raw_list = self.client.get_active_conversations(device_id)
        # Detect user's own number before merging (appears in many multi-
        # address conversations but has no dedicated single-address convo).
        self._detect_self_number(raw_list)
        count = 0
        for entry in raw_list:
            msg = _parse_message_tuple(entry)
            if msg:
                self._merge_message(msg)
                count += 1
        if count:
            self._refresh_conversation_list()

    def _detect_self_number(self, raw_list):
        """Detect the user's own phone number from conversation data.

        In MMS-style threads, the phone includes the user's own number in
        the addresses list.  We detect it by finding a number that:
        1. Appears in many multi-address conversations
        2. Has no dedicated single-address conversation
        3. Is not in the contact map
        """
        from phonelink.contacts import _normalize_phone
        from collections import Counter

        multi_appearances: Counter[str] = Counter()
        single_addr_keys: set[str] = set()

        for entry in raw_list:
            msg = _parse_message_tuple(entry)
            if not msg:
                continue
            addrs = getattr(msg, '_all_addresses', [])
            norms = []
            for a in addrs:
                n = _normalize_phone(a)
                key = n[-10:] if len(n) >= 10 else n
                if key:
                    norms.append(key)
            if len(norms) == 1:
                single_addr_keys.add(norms[0])
            elif len(norms) >= 2:
                for key in set(norms):
                    multi_appearances[key] += 1

        # The user's own number appears in many multi-addr convos,
        # has no single-addr convo, and isn't a known contact.
        for num, count in multi_appearances.most_common(5):
            if count < 3:
                break
            if num in single_addr_keys:
                continue
            # Check if this number is in the contact map
            in_contacts = False
            for key in self._contact_map:
                ckey = key[-10:] if len(key) >= 10 else key
                if ckey == num:
                    in_contacts = True
                    break
            if not in_contacts:
                self._self_number = num
                return
        self._self_number = ""

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
        """A conversation finished loading — debounce and reload from cache."""
        # Cancel any pending reload and schedule a new one 500ms from now.
        # This prevents hammering reload when many signals fire in sequence.
        if hasattr(self, '_conv_loaded_timer') and self._conv_loaded_timer:
            GLib.source_remove(self._conv_loaded_timer)
        self._conv_loaded_timer = GLib.timeout_add(
            500, self._deferred_reload_conversations
        )

    def _deferred_reload_conversations(self):
        """Debounced handler for conversationLoaded signals."""
        self._conv_loaded_timer = None
        if self._device:
            self._load_active_conversations(self._device.id)
        return GLib.SOURCE_REMOVE

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
        except (TypeError, ValueError, IndexError) as e:
            print(f"[phonelink] SMS signal parse error: {type(e).__name__}: {e}")

    # ── Message ingestion ──────────────────────────────────────────

    def _merge_message(self, msg: SmsMessage):
        """Merge a parsed SmsMessage into the conversation store.

        When the phone has separate thread IDs for the same contact (e.g.
        SMS vs MMS threads, dual-SIM), we redirect the secondary thread
        into the primary conversation so the user sees a single entry.
        """
        from phonelink.contacts import _normalize_phone

        # Follow any existing redirect
        primary_tid = self._thread_redirects.get(msg.thread_id, msg.thread_id)
        conv = self._conversations.get(primary_tid)

        if conv is None:
            all_addrs = getattr(msg, '_all_addresses', [msg.address] if msg.address else [])

            # Filter out the user's own number from the address list
            # (MMS-style threads include all participants including self).
            if self._self_number and len(all_addrs) > 1:
                filtered = []
                for a in all_addrs:
                    n = _normalize_phone(a)
                    key = n[-10:] if len(n) >= 10 else n
                    if key != self._self_number:
                        filtered.append(a)
                if filtered:
                    all_addrs = filtered
                    # Update primary address to first non-self address
                    msg.address = all_addrs[0]

            # Before creating a new conversation, check if one already exists
            # for this phone number under a different thread_id.
            if len(all_addrs) <= 1 and msg.address:
                norm = _normalize_phone(msg.address)
                if norm and len(norm) >= 7:
                    for existing in self._conversations.values():
                        if existing.is_group or existing.thread_id < 0:
                            continue
                        existing_norm = _normalize_phone(existing.address)
                        if existing_norm == norm or (
                            len(norm) >= 10 and len(existing_norm) >= 10
                            and existing_norm[-10:] == norm[-10:]
                        ):
                            # Redirect this thread to the existing conversation
                            self._thread_redirects[msg.thread_id] = existing.thread_id
                            conv = existing
                            break

            if conv is None:
                if len(all_addrs) > 1:
                    names = [resolve_name(self._contact_map, a) for a in all_addrs]
                    display = ", ".join(names)
                else:
                    display = resolve_name(self._contact_map, msg.address)
                conv = Conversation(
                    thread_id=msg.thread_id,
                    address=msg.address,
                    addresses=all_addrs,
                    display_name=display,
                )
                self._conversations[msg.thread_id] = conv
        else:
            # Update addresses list if we got more addresses from this message
            all_addrs = getattr(msg, '_all_addresses', [])
            if all_addrs and len(all_addrs) > len(conv.addresses):
                conv.addresses = all_addrs

        # Avoid duplicates
        existing_uids = {m.uid for m in conv.messages}
        if msg.uid and msg.uid not in existing_uids:
            conv.messages.append(msg)

        # Update conversation metadata if this is the newest message
        if msg.date >= conv.last_date:
            is_new_message = msg.date > conv.last_date
            conv.last_date = msg.date
            conv.last_message = msg.body
            # Don't downgrade a thread the user is currently viewing;
            # but DO mark as unread if the user has navigated away and a
            # genuinely new message arrives (date strictly greater).
            if msg.thread_id == self._active_thread_id:
                conv.is_read = True
            elif msg.thread_id not in self._read_thread_ids:
                conv.is_read = bool(msg.read)
            elif is_new_message and not msg.is_sent and not msg.read:
                # New incoming message on a previously-opened conversation
                # that the user is no longer viewing — mark unread again.
                conv.is_read = False
                self._read_thread_ids.discard(msg.thread_id)

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
            if conv.is_group:
                # Build group display name from resolved participant names
                names = []
                for addr in conv.addresses:
                    names.append(resolve_name(self._contact_map, addr))
                conv.display_name = ", ".join(names)
            else:
                new_name = resolve_name(self._contact_map, conv.address)
                conv.display_name = new_name
        self._refresh_conversation_list(force_rebuild=True)

    def _refresh_conversation_list(self, force_rebuild=False):
        # Guard against re-entrant calls (select_thread can trigger
        # row-selected → conversation-selected → refresh again).
        if getattr(self, "_refreshing", False):
            return
        self._refreshing = True
        try:
            # Safety net: always ensure the conversation the user is currently
            # viewing cannot appear as unread, regardless of what signals or
            # cache data may have done to conv.is_read in the meantime.
            if self._active_thread_id is not None:
                active_conv = self._conversations.get(self._active_thread_id)
                if active_conv:
                    active_conv.is_read = True
            self._conv_list.set_contact_map(self._contact_map)
            self._conv_list.set_conversations(
                self._deduplicated_conversations(), force_rebuild=force_rebuild
            )
            if self._active_thread_id:
                self._conv_list.select_thread(self._active_thread_id)
        finally:
            self._refreshing = False

    def _deduplicated_conversations(self) -> list[Conversation]:
        """Return conversations with same-address non-group threads merged.

        When the phone creates multiple thread IDs for the same contact
        (e.g. separate SMS and MMS threads), we present them as one entry
        using the newest thread's metadata and combining all messages.
        """
        from phonelink.contacts import _normalize_phone

        # Group conversations are never merged
        groups = []
        by_address: dict[str, list[Conversation]] = {}
        for conv in self._conversations.values():
            if conv.is_group or conv.thread_id < 0:
                groups.append(conv)
                continue
            norm = _normalize_phone(conv.address)
            # Use last-10 digits as the key to handle country-code differences
            key = norm[-10:] if len(norm) >= 10 else norm
            if not key:
                groups.append(conv)
                continue
            by_address.setdefault(key, []).append(conv)

        result = list(groups)
        for convs in by_address.values():
            if len(convs) == 1:
                result.append(convs[0])
                continue
            # Pick the conversation with the newest message as the primary
            primary = max(convs, key=lambda c: c.last_date)
            # Merge messages from other threads into the primary
            primary_uids = {m.uid for m in primary.messages}
            for other in convs:
                if other.thread_id == primary.thread_id:
                    continue
                for msg in other.messages:
                    if msg.uid and msg.uid not in primary_uids:
                        primary.messages.append(msg)
                        primary_uids.add(msg.uid)
                # If the user had the secondary thread active, redirect
                if self._active_thread_id == other.thread_id:
                    self._active_thread_id = primary.thread_id
                # Preserve unread state
                if not other.is_read:
                    primary.is_read = False
            result.append(primary)
        return result

    # ── UI event handlers ──────────────────────────────────────────

    def _on_conversation_selected(self, widget, thread_id):
        self._active_thread_id = thread_id
        conv = self._conversations.get(thread_id)
        if not conv:
            return

        # Mark as read locally
        self._read_thread_ids.add(thread_id)
        was_unread = not conv.is_read
        conv.is_read = True

        # Rebuild the list immediately so the unread dot disappears
        self._refresh_conversation_list(force_rebuild=True)

        # Draft conversations (negative thread_id) don't exist on the phone
        if thread_id < 0:
            self._show_thread(thread_id)
            return

        # Notify the phone (after the UI update so the user sees instant feedback)
        if was_unread and self._device:
            self.client.mark_conversation_as_read(self._device.id, thread_id)

        # If we have few messages, request more from the phone
        if self._device and len(conv.messages) < 20:
            self._thread.show_loading(conv.display_name, conv.address)
            self.client.request_conversation(self._device.id, thread_id, 0, 50)
            # Also request messages from any secondary threads that were
            # redirected into this conversation (SMS/MMS split, etc.)
            for sec_tid, primary_tid in self._thread_redirects.items():
                if primary_tid == thread_id:
                    self.client.request_conversation(
                        self._device.id, sec_tid, 0, 50
                    )
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

    _next_draft_id = -1  # class-level counter for draft thread IDs

    def _on_start_conversation(self, widget, address, name):
        """Handle starting a new conversation from contact search."""
        if not self._device or not self._device.reachable:
            return
        self._open_or_create_conversation(address, name)

    def _open_or_create_conversation(self, address, display_name):
        """Navigate to an existing conversation or create a draft for a new one."""
        from phonelink.contacts import _normalize_phone

        norm = _normalize_phone(address)

        # Check if a conversation already exists for this number
        for conv in self._conversations.values():
            conv_norm = _normalize_phone(conv.address)
            if conv_norm == norm or (
                len(norm) >= 10 and len(conv_norm) >= 10
                and conv_norm[-10:] == norm[-10:]
            ):
                # Existing conversation — just select it
                self._active_thread_id = conv.thread_id
                self._read_thread_ids.add(conv.thread_id)
                conv.is_read = True
                self._refresh_conversation_list()
                self._show_thread(conv.thread_id)
                return

        # Create a draft conversation with a negative thread_id
        draft_id = SmsPanel._next_draft_id
        SmsPanel._next_draft_id -= 1

        conv = Conversation(
            thread_id=draft_id,
            address=address,
            display_name=display_name if display_name != address else "",
            is_read=True,
        )
        self._conversations[draft_id] = conv
        self._active_thread_id = draft_id
        self._refresh_conversation_list()
        self._conv_list.select_thread(draft_id)
        self._show_thread(draft_id)

    def _find_thread_for_address(self, address: str) -> int | None:
        """Find an existing real thread_id for a phone number, or None."""
        from phonelink.contacts import _normalize_phone
        norm = _normalize_phone(address)
        if not norm:
            return None
        for conv in self._conversations.values():
            if conv.thread_id < 0 or conv.is_group:
                continue
            conv_norm = _normalize_phone(conv.address)
            if conv_norm == norm or (
                len(norm) >= 10 and len(conv_norm) >= 10
                and conv_norm[-10:] == norm[-10:]
            ):
                return conv.thread_id
        return None

    def _on_send_message(self, widget, thread_id, text):
        """Handle send from the compose bar."""
        if not self._device or not self._device.reachable:
            return

        if thread_id < 0:
            # Draft conversation — try to find an existing thread first
            # (one may have loaded since the draft was created)
            conv = self._conversations.get(thread_id)
            if conv:
                existing_tid = self._find_thread_for_address(conv.address)
                if existing_tid is not None:
                    # Use existing thread — avoids creating a duplicate on the phone
                    self.client.reply_to_conversation(
                        self._device.id, existing_tid, text
                    )
                    self._conversations.pop(thread_id, None)
                    self._active_thread_id = existing_tid
                    self._read_thread_ids.add(existing_tid)
                    self._refresh_conversation_list()
                    self._show_thread(existing_tid)
                else:
                    # Truly new contact — create a new thread on the phone
                    self.client.send_sms(self._device.id, [conv.address], text)
                    self._conversations.pop(thread_id, None)
                    self._active_thread_id = None
                    self._thread.show_empty()
                    self._refresh_conversation_list()
        else:
            self.client.reply_to_conversation(self._device.id, thread_id, text)

    def _on_send_message_with_attachment(self, widget, thread_id, text, image_path):
        """Handle send with an image attachment."""
        if not self._device or not self._device.reachable:
            return

        if thread_id < 0:
            conv = self._conversations.get(thread_id)
            if conv:
                existing_tid = self._find_thread_for_address(conv.address)
                if existing_tid is not None:
                    self.client.reply_to_conversation(
                        self._device.id, existing_tid, text or "",
                        attachments=[image_path],
                    )
                    self._conversations.pop(thread_id, None)
                    self._active_thread_id = existing_tid
                    self._read_thread_ids.add(existing_tid)
                    self._refresh_conversation_list()
                    self._show_thread(existing_tid)
                else:
                    self.client.send_sms(
                        self._device.id, [conv.address], text or "",
                        attachments=[image_path],
                    )
                    self._conversations.pop(thread_id, None)
                    self._active_thread_id = None
                    self._thread.show_empty()
                    self._refresh_conversation_list()
        else:
            self.client.reply_to_conversation(
                self._device.id, thread_id, text or "",
                attachments=[image_path],
            )

    def _on_delete_conversation(self, widget, thread_id):
        """Ask for confirmation, then delete the conversation locally and on the phone."""
        conv = self._conversations.get(thread_id)
        name = (conv.display_name or conv.address) if conv else f"Thread {thread_id}"

        dialog = Adw.MessageDialog(
            transient_for=self.get_root(),
            heading="Delete Conversation",
            body=f"Delete the conversation with {name}?\n\nThis will also try to delete it on your phone.",
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("delete", "Delete")
        dialog.set_response_appearance("delete", Adw.ResponseAppearance.DESTRUCTIVE)

        def on_response(dlg, response):
            if response == "delete":
                # Try to delete on the phone (silently fails on older KDE Connect)
                if self._device and self._client:
                    self._client.delete_conversation(self._device.id, thread_id)
                # Remove locally
                self._remove_conversation(thread_id)

        dialog.connect("response", on_response)
        dialog.present()

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
