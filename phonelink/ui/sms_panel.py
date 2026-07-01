"""SMS panel — conversation list + message thread, backed by D-Bus signals."""

import mimetypes
import re
import threading
import time

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, Gio, GLib, GObject

from phonelink.dbus_client import (
    IFACE_CONVERSATIONS,
    IFACE_NOTIFICATIONS,
    IFACE_SHARE,
    IFACE_TELEPHONY,
)
from phonelink.contacts import (
    _normalize_phone,
    load_contact_map, resolve_name, import_vcf_file,
    harvest_contacts_from_notifications,
    build_body_to_address_map,
    match_contact_from_notification_props,
    harvest_contact_from_telephony_signal,
    synced_vcard_count,
)
from phonelink.models import Conversation, SmsMessage
from phonelink import reconcile
from phonelink.reconcile import ConversationIndex
from phonelink.settings import get_settings
from phonelink.store import get_message_store
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


def _attachment_extension(mime_type: str) -> str:
    ext = mimetypes.guess_extension((mime_type or "").split(";", 1)[0].strip()) or ""
    if ext == ".jpe":
        return ".jpg"
    return ext


def _attachment_file_name(part_id: int, mime_type: str, unique_identifier: str) -> str:
    candidate = (unique_identifier or "").strip().split("/")[-1]
    ext = _attachment_extension(mime_type)
    if candidate and ("." in candidate or not ext):
        return candidate
    base = candidate or f"attachment-{part_id}"
    return f"{base}{ext}" if ext and not base.lower().endswith(ext.lower()) else base


def _parse_attachment_tuple(raw_attachment) -> dict | None:
    if not isinstance(raw_attachment, (tuple, list)) or len(raw_attachment) < 4:
        return None
    try:
        part_id = int(raw_attachment[0])
    except (TypeError, ValueError):
        return None

    mime_type = str(raw_attachment[1] or "")
    payload = str(raw_attachment[2] or "")
    unique_identifier = str(raw_attachment[3] or "")
    return {
        "partId": part_id,
        "mimeType": mime_type,
        "payload": payload,
        "uniqueIdentifier": unique_identifier,
        "fileName": _attachment_file_name(part_id, mime_type, unique_identifier),
    }


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
        if len(t) > _I_ATT:
            raw_attachments = t[_I_ATT] or []
            for raw_attachment in raw_attachments:
                attachment = _parse_attachment_tuple(raw_attachment)
                if attachment is not None:
                    msg.attachments.append(attachment)
        # Stash all addresses for group chat detection
        msg._all_addresses = all_addresses
        return msg
    except (IndexError, TypeError, ValueError) as e:
        print(f"[phonelink] Failed to parse message tuple: {e}")
        return None


class SmsPanel(Gtk.Box):
    __gsignals__ = {
        "google-status-changed": (GObject.SignalFlags.RUN_FIRST, None, ()),
    }

    def __init__(self, client):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.client = client
        self._settings = get_settings()
        self._store = get_message_store()
        self._device = None
        # Conversations are normalized once on ingest and keyed by a canonical
        # participant-set identity — no render-time thread splitting/merging.
        self._index = ConversationIndex()
        self._active_thread_id: int | None = None
        self._signal_ids: list[int] = []
        self._contact_map: dict[str, str] = {}  # normalised phone → display name
        self._read_thread_ids: set[int] = set()  # threads the user has opened this session
        self._was_reachable: bool = False  # track reachability transitions
        self._contact_sync_check_source: int | None = None
        self._contact_sync_warned: set[str] = set()
        self._google_sync_in_flight = False
        self._all_conversations_refresh_source: int | None = None
        self._conversation_refresh_source: int | None = None
        self._pending_refresh_force = False
        self._pending_show_thread_id: int | None = None

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
        self._thread.connect("download-attachment", self._on_download_attachment)
        content.append(self._thread)

        self._stack.set_visible_child_name("status")

    # ── Conversation model access ──────────────────────────────────

    @property
    def _conversations(self) -> dict[int, Conversation]:
        """The merged conversations, keyed by primary thread_id."""
        return self._index.conversations

    def _name_for(self, addresses: list[str], address: str) -> str:
        """Resolve a display name for a conversation from the contact map."""
        if addresses and len(addresses) > 1:
            return ", ".join(resolve_name(self._contact_map, a) for a in addresses)
        target = address or (addresses[0] if addresses else "")
        return resolve_name(self._contact_map, target)

    # ── Device switching ───────────────────────────────────────────

    def set_device(self, device):
        old_id = self._device.id if self._device else None
        self._device = device

        if not device:
            self._contact_map = {}
            self._conv_list.set_contact_map({})
            self._show_disconnected()
            self._unsubscribe_signals()
            self._cancel_all_conversations_refresh()
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
            self._index.clear()
            self._read_thread_ids.clear()
            self._conv_list.set_conversations([])
            self._thread.show_empty()
            self._active_thread_id = None
            self._load_contacts(device.id)
            self._load_cached_conversations(device.id)
            self._subscribe_signals(device.id)
            self._request_contact_sync(device.id)
            self._cancel_all_conversations_refresh()
            self._load_active_conversations(device.id, schedule_if_empty=True)
            self._run_startup_contact_backfill(device.id)
            self._maybe_refresh_google_contacts_in_background()
        elif not self._signal_ids or just_reconnected:
            # Same device reconnected — re-subscribe signals and refresh
            if not self._signal_ids:
                self._subscribe_signals(device.id)
            self._load_contacts(device.id)
            self._request_contact_sync(device.id)
            self._cancel_all_conversations_refresh()
            self._load_active_conversations(device.id, schedule_if_empty=True)
            self._run_startup_contact_backfill(device.id)
            self._maybe_refresh_google_contacts_in_background()

    def _load_contacts(self, device_id: str):
        """Refresh the saved contact map for the active device."""
        self._contact_map = load_contact_map(device_id)
        self._conv_list.set_contact_map(self._contact_map)

    def _load_cached_conversations(self, device_id: str):
        """Show persisted history immediately, before the daemon responds."""
        try:
            cached = self._store.load_conversations(device_id)
        except Exception as exc:  # noqa: BLE001 — never block startup on the cache
            print(f"[phonelink] failed to load cached conversations: {exc}")
            return
        if not cached:
            return
        # Register into the identity index (deterministic order), collapsing any
        # legacy duplicate rows from before conversations were normalized.
        self._index.clear()
        for conv in sorted(cached.values(), key=lambda c: c.thread_id):
            self._index.register(conv)
        # Refine display names against the current contact map.
        for conv in self._conversations.values():
            conv.display_name = self._name_for(conv.addresses, conv.address)
        self._refresh_conversation_list(force_rebuild=True)

    def _request_contact_sync(self, device_id: str):
        """Kick off KDE Connect contact sync so names can populate automatically."""
        self.client.submit(self.client.sync_contacts, device_id)
        self._schedule_contact_sync_check(device_id)

    def _run_startup_contact_backfill(self, device_id: str):
        """Run retroactive contact recovery sources used during startup."""
        self._harvest_from_notifications(device_id)
        self._scan_downloads_for_vcf(device_id)

    def _conversation_identity(self, addresses: list[str] | None, fallback_address: str = "") -> str:
        return reconcile.conversation_identity(
            addresses, fallback_address, self._index.self_key
        )

    def _conversation_hidden_until(self, conversation_key: str) -> int:
        if not self._device or not conversation_key:
            return 0
        return self._settings.conversation_hidden_until(self._device.id, conversation_key)

    def _should_keep_conversation_hidden(self, conversation_key: str, message_date: int) -> bool:
        hidden_until = self._conversation_hidden_until(conversation_key)
        if not hidden_until:
            return False
        if int(message_date or 0) > hidden_until:
            self._settings.unhide_conversation(self._device.id, conversation_key)
            return False
        return True

    def _hide_conversation_locally(self, conv: Conversation):
        if not self._device or conv.thread_id < 0:
            return ""
        conversation_key = self._conversation_identity(conv.addresses, conv.address)
        if not conversation_key:
            return ""
        deleted_at = max(int(time.time() * 1000), int(conv.last_date or 0))
        self._settings.hide_conversation(self._device.id, conversation_key, deleted_at)
        return conversation_key

    def _purge_loaded_conversations(self, conversation_key: str):
        if not conversation_key:
            return

        primary = self._index.identity_to_primary.get(conversation_key)
        if primary is None:
            return
        removed_ids = set(self._index.remove(primary))
        if not removed_ids:
            return

        for thread_id in removed_ids:
            if self._device:
                self._store.delete_conversation(self._device.id, thread_id)
        self._read_thread_ids.difference_update(removed_ids)
        if self._active_thread_id in removed_ids:
            self._thread.show_empty()
            self._active_thread_id = None

    def _is_hidden_conversation(self, conv: Conversation) -> bool:
        conversation_key = self._conversation_identity(conv.addresses, conv.address)
        if not conversation_key:
            return False
        message_date = conv.last_date or (conv.messages[-1].date if conv.messages else 0)
        return self._should_keep_conversation_hidden(conversation_key, message_date)

    def _load_active_conversations(self, device_id, schedule_if_empty: bool = False):
        """Load cached conversations from the daemon (off the main thread)."""
        self.client.submit(
            self.client.get_active_conversations,
            device_id,
            on_result=lambda raw: self._apply_active_conversations(
                device_id, raw, schedule_if_empty
            ),
        )

    def _apply_active_conversations(self, device_id, raw_list,
                                    schedule_if_empty: bool = False):
        if not self._device or self._device.id != device_id:
            return
        # Detect user's own number before merging (appears in many multi-
        # address conversations but has no dedicated single-address convo).
        self._detect_self_number(raw_list)
        count = 0
        # Coalesce the hundreds of per-message store writes into one commit.
        with self._store.batch():
            for entry in raw_list:
                msg = _parse_message_tuple(entry)
                if msg:
                    self._merge_message(msg)
                    count += 1
        if count:
            self._refresh_conversation_list()
        if schedule_if_empty and count == 0:
            self._schedule_all_conversations_refresh(device_id)

    def _schedule_all_conversations_refresh(self, device_id: str, delay_seconds: int = 10):
        """Defer the expensive full-phone conversation sync until startup is interactive."""
        self._cancel_all_conversations_refresh()
        self._all_conversations_refresh_source = GLib.timeout_add_seconds(
            delay_seconds,
            self._run_all_conversations_refresh,
            device_id,
        )

    def _cancel_all_conversations_refresh(self):
        if self._all_conversations_refresh_source:
            GLib.source_remove(self._all_conversations_refresh_source)
            self._all_conversations_refresh_source = None

    def _run_all_conversations_refresh(self, device_id: str):
        self._all_conversations_refresh_source = None
        if not self._device or self._device.id != device_id or not self._device.reachable:
            return GLib.SOURCE_REMOVE
        self.client.submit(self.client.request_all_conversations, device_id)
        return GLib.SOURCE_REMOVE

    def _detect_self_number(self, raw_list):
        """Detect the user's own number and re-key identities if it changed.

        In MMS-style threads the phone includes the user's own number in the
        address list; it is needed to canonicalise group-thread identities.  The
        detection itself is a pure function in :mod:`phonelink.reconcile`.
        """
        key_lists = []
        for entry in raw_list:
            msg = _parse_message_tuple(entry)
            if not msg:
                continue
            addrs = getattr(msg, "_all_addresses", [])
            key_lists.append([reconcile.phone_key(a) for a in addrs])

        self_key = reconcile.detect_self_key(key_lists, self._contact_map.keys())
        if self._index.set_self_key(self_key):
            # Identities of group threads depend on the self key — rebuild them.
            self._index.reindex()
            if (
                self._active_thread_id is not None
                and self._active_thread_id not in self._conversations
            ):
                self._active_thread_id = self._index.primary_for(self._active_thread_id)

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
            ("attachmentReceived", self._on_attachment_received),
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

        sid = self.client.subscribe_signal(
            path + "/telephony", IFACE_TELEPHONY,
            "callReceived", self._on_telephony_call_received
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
            self._harvest_from_notifications(self._device.id)
        return GLib.SOURCE_REMOVE

    def _schedule_contact_sync_check(self, device_id: str):
        self._cancel_contact_sync_check()
        self._contact_sync_check_source = GLib.timeout_add_seconds(
            8, self._check_contact_sync_health, device_id
        )

    def _cancel_contact_sync_check(self):
        if self._contact_sync_check_source:
            GLib.source_remove(self._contact_sync_check_source)
            self._contact_sync_check_source = None

    def _check_contact_sync_health(self, device_id: str):
        self._contact_sync_check_source = None
        if not self._device or self._device.id != device_id:
            return GLib.SOURCE_REMOVE
        if synced_vcard_count(device_id) > 0:
            return GLib.SOURCE_REMOVE
        if device_id in self._contact_sync_warned:
            return GLib.SOURCE_REMOVE
        self._contact_sync_warned.add(device_id)
        self._show_toast(
            "KDE Connect did not populate desktop contacts. Check the phone's Contacts permission or use Google Contacts import."
        )
        return GLib.SOURCE_REMOVE

    def _on_contacts_synced(self, conn, sender, path, iface, sig, params):
        """Contact cache was updated — reload names."""
        self._cancel_contact_sync_check()
        GLib.idle_add(self._reload_contact_names)

    def _on_notification_posted(self, conn, sender, path, iface, sig, params):
        """A new notification arrived — check if it's an SMS with a contact name."""
        notif_id = params.unpack()[0] if params else None
        if notif_id and self._device:
            GLib.idle_add(self._handle_notification, notif_id)

    def _on_telephony_call_received(self, conn, sender, path, iface, sig, params):
        """Persist contact names learned from incoming or missed calls."""
        unpacked = params.unpack() if params else ()
        if len(unpacked) >= 3:
            event, number, contact_name = unpacked[:3]
            GLib.idle_add(self._handle_telephony_contact, event, number, contact_name)

    def _handle_notification(self, notif_id):
        """Check a new notification for SMS contact name info (off the main thread)."""
        if not self._device:
            return
        device_id = self._device.id
        self.client.submit(
            self.client.get_notification_properties,
            device_id,
            notif_id,
            on_result=lambda props: self._apply_notification_props(
                device_id, notif_id, props
            ),
        )

    def _apply_notification_props(self, device_id, notif_id, props):
        if not self._device or self._device.id != device_id or not props:
            return
        result = match_contact_from_notification_props(
            props, self._conversations, self._contact_map
        )
        if result:
            norm_phone, name = result
            self._contact_map[norm_phone] = name
            # Update matching conversations
            for conv in self._conversations.values():
                if _normalize_phone(conv.address) == norm_phone:
                    conv.display_name = name
            self._refresh_conversation_list()

    def _send_text_reply(self, thread_id: int, text: str, attachments=None):
        """Send into an existing thread, off the main thread.

        On the Galaxy S25 (and other Android/KDE Connect builds) replying via the
        messaging notification *or* ``replyToConversation`` duplicates the SMS.
        ``sendWithoutConversation`` is the only reliable path for a one-to-one
        thread, so we use it whenever we know the single recipient's address.
        Group threads must reply in-thread to preserve all recipients.
        """
        if not self._device:
            return
        device_id = self._device.id
        attachments = list(attachments) if attachments else None

        conv = self._conversations.get(self._index.primary_for(thread_id))
        if conv and conv.address and not conv.is_group:
            self.client.submit(
                self.client.send_sms, device_id, [conv.address], text or "",
                attachments=attachments,
            )
        else:
            self.client.submit(
                self.client.reply_to_conversation, device_id, thread_id, text or "",
                attachments=attachments,
            )

    def _handle_telephony_contact(self, event: str, number: str, contact_name: str):
        """Persist a contact name emitted by the telephony plugin."""
        if event not in {"ringing", "missedCall"}:
            return
        result = harvest_contact_from_telephony_signal(number, contact_name)
        if not result:
            return
        norm_phone, name = result
        self._contact_map[norm_phone] = name
        for conv in self._conversations.values():
            from phonelink.contacts import _normalize_phone
            conv_norm = _normalize_phone(conv.address)
            if conv_norm == norm_phone or (
                len(conv_norm) >= 10 and len(norm_phone) >= 10
                and conv_norm[-10:] == norm_phone[-10:]
            ):
                conv.display_name = name
        self._refresh_conversation_list(force_rebuild=True)
        if self._active_thread_id is not None:
            self._show_thread(self._active_thread_id)

    def _show_toast(self, message: str):
        root = self.get_root()
        if root and hasattr(root, "_show_toast"):
            root._show_toast(message)

    def get_google_status(self) -> dict:
        from phonelink.google_contacts import (
            GOOGLE_CLIENT_FILE,
            has_google_client_config,
            has_saved_google_credentials,
        )

        account_label = self._settings.google_account_label.strip()
        return {
            "configured": has_google_client_config(),
            "connected": has_saved_google_credentials(),
            "account_label": account_label,
            "last_sync_ts": self._settings.google_last_sync_ts,
            "background_sync": self._settings.google_background_sync,
            "sync_in_flight": self._google_sync_in_flight,
            "config_path": str(GOOGLE_CLIENT_FILE),
        }

    def connect_google_contacts(self):
        self._start_google_contacts_sync(interactive=True, source="settings-connect")

    def refresh_google_contacts(self):
        self._start_google_contacts_sync(interactive=True, source="settings-refresh")

    def disconnect_google_contacts(self):
        from phonelink.google_contacts import disconnect_google_contacts

        disconnected = disconnect_google_contacts()
        self._settings.google_background_sync = False
        self._settings.clear_google_account()
        self.emit("google-status-changed")
        if disconnected:
            self._show_toast("Google Contacts disconnected")

    def _maybe_refresh_google_contacts_in_background(self):
        if self._google_sync_in_flight:
            return
        if not self._settings.google_background_sync:
            return
        if not self._google_background_sync_due():
            return
        self._start_google_contacts_sync(interactive=False, source="background")

    def _google_background_sync_due(self) -> bool:
        from phonelink.google_contacts import has_saved_google_credentials

        if not has_saved_google_credentials():
            return False
        last_attempt = self._settings.google_last_attempt_ts
        return (time.time() - last_attempt) >= 24 * 60 * 60

    def _collect_google_photo_numbers(self) -> set[str]:
        numbers: set[str] = set()
        for conv in self._deduplicated_conversations():
            if conv.address:
                numbers.add(conv.address)
            for address in conv.addresses:
                if address:
                    numbers.add(address)
        return numbers

    def _start_google_contacts_sync(self, interactive: bool, source: str):
        if self._google_sync_in_flight:
            return
        if not interactive and source == "background" and not self._google_background_sync_due():
            return

        self._google_sync_in_flight = True
        self._settings.google_last_attempt_ts = time.time()
        self.emit("google-status-changed")
        if interactive:
            self._show_toast("Opening Google Contacts import in your browser…")

        photo_numbers = self._collect_google_photo_numbers()
        worker = threading.Thread(
            target=self._run_google_contacts_sync,
            args=(interactive, source, photo_numbers),
            daemon=True,
        )
        worker.start()

    def _run_google_contacts_sync(self, interactive: bool, source: str, photo_numbers: set[str]):
        try:
            from phonelink.google_contacts import import_google_contacts

            result = import_google_contacts(
                photo_numbers=photo_numbers,
                allow_browser=interactive,
            )
            GLib.idle_add(
                self._finish_google_contacts_sync,
                result,
                None,
                interactive,
                source,
            )
        except Exception as exc:
            GLib.idle_add(
                self._finish_google_contacts_sync,
                None,
                exc,
                interactive,
                source,
            )

    def _finish_google_contacts_sync(self, result, error, interactive: bool, source: str):
        self._google_sync_in_flight = False

        if error is not None:
            from phonelink.google_contacts import GoogleContactsAuthRequiredError

            if isinstance(error, GoogleContactsAuthRequiredError):
                self._settings.clear_google_account()
            self.emit("google-status-changed")
            if not interactive:
                return GLib.SOURCE_REMOVE

            from phonelink.google_contacts import (
                GoogleContactsAuthRequiredError,
                GoogleContactsConfigError,
                GoogleContactsDependencyError,
            )

            if isinstance(error, GoogleContactsDependencyError):
                heading = "Google Contacts Support Missing"
                body = str(error)
            elif isinstance(error, GoogleContactsAuthRequiredError):
                heading = "Google Contacts Reconnection Needed"
                body = str(error)
            elif isinstance(error, GoogleContactsConfigError):
                heading = "Google Contacts Not Configured"
                body = str(error)
            else:
                heading = "Google Contacts Import Failed"
                body = str(error)

            dialog = Adw.MessageDialog(
                transient_for=self.get_root(),
                heading=heading,
                body=body,
            )
            dialog.add_response("ok", "OK")
            dialog.present()
            return GLib.SOURCE_REMOVE

        self._settings.google_account_label = result.account_label
        self._settings.google_last_sync_ts = time.time()
        self.emit("google-status-changed")

        if self._device:
            self._contact_map = load_contact_map(self._device.id)
            self._reload_contact_names()

        if interactive:
            dialog = Adw.MessageDialog(
                transient_for=self.get_root(),
                heading="Google Contacts Imported",
                body=(
                    f"Imported {result.imported_contacts} contact mappings from {result.account_label}.\n\n"
                    f"Updated {result.imported_photos} contact photos.\n"
                    f"Google returned {result.seen_people} people in total."
                ),
            )
            dialog.add_response("ok", "OK")
            dialog.present()
            self._show_toast("Google Contacts import finished")
        elif result.imported_contacts or result.imported_photos:
            self._show_toast("Google Contacts refreshed in the background")
        return GLib.SOURCE_REMOVE

    def _harvest_from_notifications(self, device_id):
        """Scan active notifications for SMS contact names (off the main thread)."""
        # Snapshot the body→address map on the main thread so the worker never
        # iterates the live conversation store.
        body_to_addr = build_body_to_address_map(self._conversations)
        self.client.submit(
            harvest_contacts_from_notifications,
            self.client,
            device_id,
            body_to_addr,
            on_result=lambda count: self._after_notification_harvest(device_id, count),
        )

    def _after_notification_harvest(self, device_id, count):
        if count and self._device and self._device.id == device_id:
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

    def _on_attachment_received(self, conn, sender, path, iface, sig, params):
        unpacked = params.unpack() if params else ()
        if len(unpacked) >= 2:
            file_path, file_name = unpacked[:2]
            GLib.idle_add(self._handle_attachment_received, str(file_path), str(file_name))

    def _handle_attachment_received(self, file_path: str, file_name: str):
        if self._active_thread_id is not None:
            self._show_thread(self._active_thread_id)
        self._show_toast(f"Downloaded {file_name} to {file_path}")
        return GLib.SOURCE_REMOVE

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
                self._schedule_conversation_refresh(
                    show_thread_id=msg.thread_id if self._active_thread_id == msg.thread_id else None
                )
        except (TypeError, ValueError, IndexError) as e:
            print(f"[phonelink] SMS signal parse error: {type(e).__name__}: {e}")

    def _schedule_conversation_refresh(self, *, force_rebuild: bool = False,
                                       show_thread_id: int | None = None):
        self._pending_refresh_force = self._pending_refresh_force or bool(force_rebuild)
        if show_thread_id is not None:
            self._pending_show_thread_id = show_thread_id
        if self._conversation_refresh_source:
            return
        self._conversation_refresh_source = GLib.timeout_add(
            100,
            self._flush_conversation_refresh,
        )

    def _flush_conversation_refresh(self):
        self._conversation_refresh_source = None
        force_rebuild = self._pending_refresh_force
        show_thread_id = self._pending_show_thread_id
        self._pending_refresh_force = False
        self._pending_show_thread_id = None

        self._refresh_conversation_list(force_rebuild=force_rebuild)
        if (
            show_thread_id is not None
            and self._active_thread_id == show_thread_id
            and show_thread_id in self._conversations
        ):
            self._sync_thread(show_thread_id)
        return GLib.SOURCE_REMOVE

    # ── Message ingestion ──────────────────────────────────────────

    def _merge_message(self, msg: SmsMessage):
        """Merge a parsed SmsMessage into its canonical conversation.

        Identity (which phone thread IDs belong together) is computed once, on
        ingest, by :class:`~phonelink.reconcile.ConversationIndex` — no
        render-time thread splitting/merging.
        """
        all_addrs = getattr(msg, "_all_addresses", None) or (
            [msg.address] if msg.address else []
        )

        # Respect a locally-hidden (deleted) conversation until a newer message
        # arrives — but only for a genuinely new conversation.  A message on an
        # already-visible conversation is never re-hidden.
        if self._index.get(msg.thread_id) is None:
            identity = self._conversation_identity(all_addrs, msg.address)
            if identity not in self._index.identity_to_primary:
                if self._should_keep_conversation_hidden(identity, msg.date):
                    return

        result = self._index.ingest(msg, all_addrs, self._name_for)
        conv = result.conversation

        if self._device and result.message_added:
            self._store.upsert_message(self._device.id, conv.thread_id, msg)

        # Update unread state (tracked by the stable primary thread_id).
        if result.is_latest:
            if conv.thread_id == self._active_thread_id:
                conv.is_read = True
            elif conv.thread_id not in self._read_thread_ids:
                conv.is_read = bool(msg.read)
            elif result.is_newer and not msg.is_sent and not msg.read:
                # New incoming message on a previously-opened conversation the
                # user is no longer viewing — mark unread again.
                conv.is_read = False
                self._read_thread_ids.discard(conv.thread_id)

        if self._device:
            self._store.upsert_conversation(self._device.id, conv)

    def _remove_conversation(self, thread_id):
        removed_ids = set(self._index.remove(thread_id)) or {thread_id}
        for tid in removed_ids:
            if self._device:
                self._store.delete_conversation(self._device.id, tid)
            self._read_thread_ids.discard(tid)
        if self._active_thread_id in removed_ids:
            self._thread.show_empty()
            self._active_thread_id = None
        self._refresh_conversation_list()

    def _reload_contact_names(self):
        """Reload vCard cache and update all conversation display names."""
        if not self._device:
            return
        self._contact_map = load_contact_map(self._device.id)
        self._conv_list.set_contact_map(self._contact_map)
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
        self._store.upsert_conversations(
            self._device.id, list(self._conversations.values())
        )
        self._refresh_conversation_list(force_rebuild=True)
        if self._active_thread_id is not None:
            self._show_thread(self._active_thread_id)

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
        """Return the conversations to display.

        Merging now happens once, on ingest (see :mod:`phonelink.reconcile`), so
        this is a pure, side-effect-free filter that just hides locally-deleted
        conversations.
        """
        return self._index.visible(self._is_hidden_conversation)

    # ── UI event handlers ──────────────────────────────────────────

    def _on_conversation_selected(self, widget, thread_id):
        if self._active_thread_id == thread_id:
            return

        self._active_thread_id = thread_id
        conv = self._conversations.get(thread_id)
        if not conv:
            return

        # Mark as read locally
        self._read_thread_ids.add(thread_id)
        was_unread = not conv.is_read
        conv.is_read = True

        if was_unread:
            self._conv_list.set_thread_read_state(thread_id, True)
            if self._device:
                self._store.upsert_conversation(self._device.id, conv)

        # Draft conversations (negative thread_id) don't exist on the phone
        if thread_id < 0:
            self._show_thread(thread_id)
            return

        # Notify the phone (after the UI update so the user sees instant feedback)
        if was_unread and self._device:
            self.client.submit(
                self.client.mark_conversation_as_read, self._device.id, thread_id
            )

        # Show cached messages immediately, then request more in the background
        # if this thread looks only partially loaded.
        self._show_thread(thread_id)

        if self._device and len(conv.messages) < 20:
            self.client.submit(
                self.client.request_conversation, self._device.id, thread_id, 0, 50
            )
            # Also request messages from any secondary threads that were
            # merged into this conversation (SMS/MMS split, dual-SIM, etc.)
            for sec_tid in self._index.secondary_threads(thread_id):
                self.client.submit(
                    self.client.request_conversation,
                    self._device.id, sec_tid, 0, 50
                )

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

    def _sync_thread(self, thread_id):
        """Incrementally reflect new messages in the active thread."""
        conv = self._conversations.get(thread_id)
        if not conv:
            return
        self._thread.sync(
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
                    self._send_text_reply(existing_tid, text)
                    self._conversations.pop(thread_id, None)
                    self._active_thread_id = existing_tid
                    self._read_thread_ids.add(existing_tid)
                    self._refresh_conversation_list()
                    self._show_thread(existing_tid)
                else:
                    # Truly new contact — create a new thread on the phone
                    self.client.submit(
                        self.client.send_sms, self._device.id, [conv.address], text
                    )
                    self._conversations.pop(thread_id, None)
                    self._active_thread_id = None
                    self._thread.show_empty()
                    self._refresh_conversation_list()
        else:
            self._send_text_reply(thread_id, text)

    def _on_send_message_with_attachment(self, widget, thread_id, text, image_path):
        """Handle send with an image attachment."""
        if not self._device or not self._device.reachable:
            return

        if thread_id < 0:
            conv = self._conversations.get(thread_id)
            if conv:
                existing_tid = self._find_thread_for_address(conv.address)
                if existing_tid is not None:
                    self._send_text_reply(existing_tid, text, attachments=[image_path])
                    self._conversations.pop(thread_id, None)
                    self._active_thread_id = existing_tid
                    self._read_thread_ids.add(existing_tid)
                    self._refresh_conversation_list()
                    self._show_thread(existing_tid)
                else:
                    self.client.submit(
                        self.client.send_sms,
                        self._device.id, [conv.address], text or "",
                        attachments=[image_path],
                    )
                    self._conversations.pop(thread_id, None)
                    self._active_thread_id = None
                    self._thread.show_empty()
                    self._refresh_conversation_list()
        else:
            self._send_text_reply(thread_id, text, attachments=[image_path])

    def _on_download_attachment(self, widget, thread_id, part_id, unique_identifier, file_name):
        if not self._device or not self._device.reachable:
            return

        def on_done(requested):
            if requested:
                self._show_toast(f"Requesting {file_name} from your phone…")
            else:
                self._show_toast(f"Could not request {file_name} from your phone")

        self.client.submit(
            self.client.request_attachment_file,
            self._device.id,
            int(part_id),
            unique_identifier,
            on_result=on_done,
        )

    def _on_delete_conversation(self, widget, thread_id):
        """Ask for confirmation, then delete the conversation locally and on the phone."""
        conv = self._conversations.get(thread_id)
        name = (conv.display_name or conv.address) if conv else f"Thread {thread_id}"
        can_delete_on_phone = bool(
            self._device
            and thread_id >= 0
            and self.client.supports_conversation_deletion(self._device.id)
        )
        body = f"Remove the conversation with {name} from Phone Link?"
        if can_delete_on_phone:
            body += "\n\nPhone Link will also ask your phone to delete it there."
        else:
            body += "\n\nYour current KDE Connect build does not expose remote SMS deletion, so this will remove it only from Phone Link."

        dialog = Adw.MessageDialog(
            transient_for=self.get_root(),
            heading="Delete Conversation",
            body=body,
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("delete", "Delete")
        dialog.set_response_appearance("delete", Adw.ResponseAppearance.DESTRUCTIVE)

        def on_response(dlg, response):
            if response == "delete":
                conversation_key = ""
                if conv is not None:
                    conversation_key = self._hide_conversation_locally(conv)
                if can_delete_on_phone:
                    def on_deleted(deleted):
                        if not deleted:
                            self._show_toast("Phone-side deletion failed; removing the conversation only from Phone Link")
                    self.client.submit(
                        self.client.delete_conversation,
                        self._device.id, thread_id,
                        on_result=on_deleted,
                    )
                elif thread_id >= 0:
                    self._show_toast("Removed from Phone Link only; phone-side deletion is not available on this KDE Connect version")
                if conversation_key:
                    self._purge_loaded_conversations(conversation_key)
                    self._refresh_conversation_list(force_rebuild=True)
                else:
                    self._remove_conversation(thread_id)

        dialog.connect("response", on_response)
        dialog.present()

    def _on_rename_contact(self, widget, thread_id, address):
        """Show a dialog to set a contact name for a phone number."""
        from phonelink.contacts import save_contact
        from phonelink.google_contacts import has_saved_google_credentials

        conv = self._conversations.get(thread_id)
        current_name = conv.display_name if conv else address

        dialog = Adw.MessageDialog(
            transient_for=self.get_root(),
            heading="Set Contact Name",
            body=(
                f"Enter a name for {address}:"
                + ("\n\nThis will also update your connected Google Contacts account." if has_saved_google_credentials() else "")
            ),
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
                    self._sync_google_contact_name(address, name)

        dialog.connect("response", on_response)
        dialog.present()

    def _sync_google_contact_name(self, address: str, name: str):
        from phonelink.google_contacts import has_saved_google_credentials

        if not has_saved_google_credentials():
            return

        worker = threading.Thread(
            target=self._run_google_contact_upsert,
            args=(address, name),
            daemon=True,
        )
        worker.start()

    def _run_google_contact_upsert(self, address: str, name: str):
        try:
            from phonelink.google_contacts import upsert_google_contact

            result = upsert_google_contact(address, name, allow_browser=False)
            GLib.idle_add(self._finish_google_contact_upsert, result, None, name)
        except Exception as exc:
            GLib.idle_add(self._finish_google_contact_upsert, None, exc, name)

    def _finish_google_contact_upsert(self, result, error, name: str):
        if error is not None:
            self._show_toast(f"Saved {name} locally; reconnect Google Contacts in Settings if you want cloud updates")
            return GLib.SOURCE_REMOVE

        self._show_toast(
            f"{name} {result.action} in {result.account_label}"
        )
        return GLib.SOURCE_REMOVE

    def _on_import_contacts(self, widget):
        """Show contacts sync dialog with options."""
        dialog = Adw.MessageDialog(
            transient_for=self.get_root(),
            heading="Sync Contacts",
            body=(
                "Choose how to load your full contact list:\n\n"
                "Google Contacts:\n"
                "Authorise in your browser and import directly into Phone Link.\n\n"
                "Phone share:\n"
                "1. Open Contacts on your Galaxy S25\n"
                "2. Tap ⋮ menu → Share\n"
                "3. Select all contacts\n"
                "4. Share via KDE Connect to this PC\n\n"
                "Manual file:\n"
                "Import a VCF or Google Contacts CSV if you already have one."
            ),
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("google", "Import Google Contacts")
        dialog.add_response("file", "Import File…")
        dialog.set_response_appearance("google", Adw.ResponseAppearance.SUGGESTED)
        dialog.set_response_appearance("file", Adw.ResponseAppearance.SUGGESTED)

        def on_response(dlg, response):
            if response == "google":
                self.connect_google_contacts()
            elif response == "file":
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
