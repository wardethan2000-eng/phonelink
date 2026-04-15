"""Conversation list widget — left column of the SMS panel."""

import re

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, GObject, Pango

from phonelink.contacts import contact_photo_path


def _build_avatar(name: str, phone: str, *, is_group: bool, group_count: int) -> Gtk.Widget:
    if not is_group:
        photo_path = contact_photo_path(phone)
        if photo_path:
            picture = Gtk.Picture.new_for_filename(photo_path)
            picture.set_size_request(40, 40)
            picture.set_can_shrink(True)
            picture.set_content_fit(Gtk.ContentFit.COVER)
            picture.add_css_class("conversation-avatar-photo")
            return picture

    avatar_label = Gtk.Label()
    avatar_label.set_size_request(40, 40)
    if is_group:
        avatar_label.set_label(str(group_count))
        avatar_label.add_css_class("conversation-avatar-group")
    else:
        avatar_label.set_label((name or phone or "?")[0].upper())
    avatar_label.add_css_class("conversation-avatar")
    avatar_label.set_halign(Gtk.Align.CENTER)
    avatar_label.set_valign(Gtk.Align.CENTER)
    return avatar_label


class ConversationRow(Gtk.ListBoxRow):
    """A single row in the conversation list."""

    def __init__(self, conversation):
        super().__init__()
        self.conversation = conversation

        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        box.set_margin_top(10)
        box.set_margin_bottom(10)
        box.set_margin_start(12)
        box.set_margin_end(12)
        self.set_child(box)

        box.append(
            _build_avatar(
                conversation.display_name,
                conversation.address,
                is_group=conversation.is_group,
                group_count=len(conversation.addresses),
            )
        )

        # Text column: name + preview
        text_col = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        text_col.set_hexpand(True)
        box.append(text_col)

        # Top row: name + time
        top_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        text_col.append(top_row)

        if conversation.is_group:
            display_text = conversation.display_name or "Group"
        else:
            display_text = conversation.display_name or conversation.address or "Unknown"

        self._name_label = Gtk.Label(label=display_text)
        self._name_label.set_xalign(0)
        self._name_label.set_halign(Gtk.Align.START)
        self._name_label.set_hexpand(True)
        self._name_label.set_ellipsize(Pango.EllipsizeMode.END)
        self._name_label.set_max_width_chars(20)
        top_row.append(self._name_label)

        time_label = Gtk.Label(label=conversation.time_label)
        time_label.add_css_class("dim-label")
        time_label.add_css_class("caption")
        top_row.append(time_label)

        # Preview line
        preview_label = Gtk.Label(label=conversation.preview)
        preview_label.set_xalign(0)
        preview_label.set_halign(Gtk.Align.START)
        preview_label.set_ellipsize(Pango.EllipsizeMode.END)
        preview_label.set_max_width_chars(30)
        preview_label.add_css_class("dim-label")
        preview_label.add_css_class("caption")
        text_col.append(preview_label)

        # Reserve trailing width so read/unread changes don't resize the row.
        self._unread_dot = Gtk.Label(label="●")
        self._unread_dot.add_css_class("unread-dot")
        self._unread_dot.set_xalign(0.5)
        self._unread_dot.set_valign(Gtk.Align.CENTER)
        box.append(self._unread_dot)

        self.set_read_state(conversation.is_read)

    def set_read_state(self, is_read: bool):
        self.conversation.is_read = bool(is_read)
        if self.conversation.is_read:
            self._name_label.remove_css_class("conversation-name-unread")
            self._unread_dot.add_css_class("unread-dot-hidden")
        else:
            self._name_label.add_css_class("conversation-name-unread")
            self._unread_dot.remove_css_class("unread-dot-hidden")


class ContactSuggestionRow(Gtk.ListBoxRow):
    """A contact row shown in search results for starting a new conversation."""

    def __init__(self, name, phone):
        super().__init__()
        self.contact_name = name
        self.contact_phone = phone

        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        box.set_margin_top(8)
        box.set_margin_bottom(8)
        box.set_margin_start(12)
        box.set_margin_end(12)
        self.set_child(box)

        box.append(_build_avatar(name, phone, is_group=False, group_count=1))

        text_col = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        text_col.set_hexpand(True)
        box.append(text_col)

        name_label = Gtk.Label(label=name or phone)
        name_label.set_halign(Gtk.Align.START)
        name_label.set_hexpand(True)
        name_label.set_ellipsize(Pango.EllipsizeMode.END)
        name_label.set_max_width_chars(20)
        text_col.append(name_label)

        phone_label = Gtk.Label(label=phone)
        phone_label.set_halign(Gtk.Align.START)
        phone_label.add_css_class("dim-label")
        phone_label.add_css_class("caption")
        text_col.append(phone_label)

        icon = Gtk.Image.new_from_icon_name("mail-send-symbolic")
        icon.set_opacity(0.4)
        icon.set_valign(Gtk.Align.CENTER)
        box.append(icon)


class ConversationList(Gtk.Box):
    """Scrollable list of conversations with search bar."""

    __gsignals__ = {
        "conversation-selected": (GObject.SignalFlags.RUN_FIRST, None, (int,)),
        "start-conversation": (GObject.SignalFlags.RUN_FIRST, None, (str, str)),
        "rename-contact": (GObject.SignalFlags.RUN_FIRST, None, (int, str)),
        "delete-conversation": (GObject.SignalFlags.RUN_FIRST, None, (int,)),
        "import-contacts": (GObject.SignalFlags.RUN_FIRST, None, ()),
    }

    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.set_size_request(280, -1)
        self.set_hexpand(False)
        self._conversations = []
        self._rendered_rows: list[tuple] = []
        self._contact_map: dict[str, str] = {}  # normalized phone → display name
        self._conversation_phones: set[str] = set()  # normalized phones with conversations

        # Search bar
        search_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        search_box.set_margin_top(8)
        search_box.set_margin_bottom(4)
        search_box.set_margin_start(8)
        search_box.set_margin_end(8)

        self._search_entry = Gtk.SearchEntry()
        self._search_entry.set_placeholder_text("Search conversations or contacts…")
        self._search_entry.set_hexpand(True)
        self._search_entry.connect("search-changed", self._on_search_changed)
        search_box.append(self._search_entry)

        new_btn = Gtk.Button(icon_name="list-add-symbolic")
        new_btn.set_tooltip_text("New message")
        new_btn.connect("clicked", self._on_new_clicked)
        search_box.append(new_btn)

        import_btn = Gtk.Button(icon_name="system-users-symbolic")
        import_btn.set_tooltip_text("Sync contacts from phone")
        import_btn.connect("clicked", lambda _: self.emit("import-contacts"))
        search_box.append(import_btn)

        self.append(search_box)

        # Scrollable area: conversations + contact suggestions
        scrolled = Gtk.ScrolledWindow()
        scrolled.set_vexpand(True)
        scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        self.append(scrolled)

        scroll_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        scrolled.set_child(scroll_box)

        self._listbox = Gtk.ListBox()
        self._listbox.set_selection_mode(Gtk.SelectionMode.SINGLE)
        self._listbox.set_activate_on_single_click(True)
        self._listbox.set_placeholder(self._make_placeholder())
        self._listbox.connect("row-activated", self._on_row_activated)
        self._listbox.connect("row-selected", self._on_row_selected)
        self._listbox.set_filter_func(self._filter_func)
        scroll_box.append(self._listbox)

        # Right-click menu for rename
        gesture = Gtk.GestureClick(button=3)  # right-click
        gesture.connect("pressed", self._on_right_click)
        self._listbox.add_controller(gesture)

        # Contact suggestions section (visible when searching)
        self._contacts_header = Gtk.Label(label="Contacts")
        self._contacts_header.set_halign(Gtk.Align.START)
        self._contacts_header.set_margin_start(12)
        self._contacts_header.set_margin_top(12)
        self._contacts_header.set_margin_bottom(4)
        self._contacts_header.add_css_class("heading")
        self._contacts_header.add_css_class("dim-label")
        self._contacts_header.set_visible(False)
        scroll_box.append(self._contacts_header)

        self._contacts_listbox = Gtk.ListBox()
        self._contacts_listbox.set_selection_mode(Gtk.SelectionMode.NONE)
        self._contacts_listbox.set_activate_on_single_click(True)
        self._contacts_listbox.connect("row-activated", self._on_contact_activated)
        scroll_box.append(self._contacts_listbox)

        self._search_text = ""
        self._rebuilding = False
        self._suppress_selection_signal = False
        self._last_emitted_thread_id: int | None = None

    def set_conversations(self, conversations, force_rebuild=False):
        """Update the conversation list, rebuilding only when the set changes."""
        sorted_convs = sorted(conversations, key=lambda c: c.sort_key, reverse=True)

        new_rows = [
            (
                c.thread_id,
                c.display_name,
                c.address,
                c.last_message,
                c.last_date,
                c.is_read,
                tuple(c.addresses),
            )
            for c in sorted_convs
        ]

        self._conversations = sorted_convs

        # Track which phone numbers have existing conversations
        self._conversation_phones = set()
        for c in sorted_convs:
            norm = re.sub(r"[^\d]", "", c.address)
            if norm:
                self._conversation_phones.add(norm)

        # Rebuild if any visible row content changed or forced
        if force_rebuild or new_rows != self._rendered_rows:
            self._rendered_rows = new_rows
            self._rebuilding = True
            # Remove old rows
            while True:
                row = self._listbox.get_row_at_index(0)
                if row is None:
                    break
                self._listbox.remove(row)
            # Add sorted rows
            for conv in sorted_convs:
                self._listbox.append(ConversationRow(conv))
            self._rebuilding = False

        self._update_contact_suggestions()

    def select_thread(self, thread_id: int):
        """Programmatically select a conversation row by thread ID."""
        idx = 0
        while True:
            row = self._listbox.get_row_at_index(idx)
            if row is None:
                break
            if row.conversation.thread_id == thread_id:
                if self._listbox.get_selected_row() is row:
                    break
                self._suppress_selection_signal = True
                self._listbox.select_row(row)
                self._suppress_selection_signal = False
                break
            idx += 1

    def set_thread_read_state(self, thread_id: int, is_read: bool):
        idx = 0
        while True:
            row = self._listbox.get_row_at_index(idx)
            if row is None:
                break
            if row.conversation.thread_id == thread_id and hasattr(row, "set_read_state"):
                row.set_read_state(is_read)
                break
            idx += 1

        updated_rows = []
        for rendered in self._rendered_rows:
            if rendered[0] == thread_id:
                updated_rows.append(rendered[:5] + (bool(is_read),) + rendered[6:])
            else:
                updated_rows.append(rendered)
        self._rendered_rows = updated_rows

    def _emit_conversation_selected(self, thread_id: int):
        if self._suppress_selection_signal:
            return
        if self._last_emitted_thread_id == thread_id:
            return
        self._last_emitted_thread_id = thread_id
        self.emit("conversation-selected", thread_id)

    def _on_row_activated(self, listbox, row):
        if row and hasattr(row, "conversation"):
            self._emit_conversation_selected(row.conversation.thread_id)

    def _on_row_selected(self, listbox, row):
        if self._rebuilding:
            return
        if row and hasattr(row, "conversation"):
            self._emit_conversation_selected(row.conversation.thread_id)

    def _on_search_changed(self, entry):
        self._search_text = entry.get_text().lower()
        self._listbox.invalidate_filter()
        self._update_contact_suggestions()

    def _filter_func(self, row):
        if not self._search_text:
            return True
        conv = row.conversation
        text = self._search_text
        return (
            text in (conv.display_name or "").lower()
            or text in (conv.address or "").lower()
            or text in (conv.last_message or "").lower()
        )

    def set_contact_map(self, contact_map: dict[str, str]):
        """Set the contact map for search suggestions."""
        self._contact_map = contact_map
        self._update_contact_suggestions()

    def _on_new_clicked(self, btn):
        """Focus search bar to find a contact or enter a number."""
        self._search_entry.grab_focus()

    def _update_contact_suggestions(self):
        """Populate contact suggestions matching the current search text."""
        # Clear existing suggestion rows
        while True:
            row = self._contacts_listbox.get_row_at_index(0)
            if row is None:
                break
            self._contacts_listbox.remove(row)

        text = self._search_text
        if not text:
            self._contacts_header.set_visible(False)
            return

        suggestions = []

        # Check if search looks like a phone number
        digits = re.sub(r"[^\d+]", "", text)
        if digits and len(digits) >= 3:
            norm_digits = re.sub(r"[^\d]", "", digits)
            if not self._phone_has_conversation(norm_digits):
                # Resolve name if known, otherwise use the raw number
                display = self._contact_map.get(norm_digits, digits)
                if display == digits and len(norm_digits) >= 10:
                    for key, name in self._contact_map.items():
                        if key[-10:] == norm_digits[-10:]:
                            display = name
                            break
                suggestions.append((display, digits))

        # Search contacts by name or number
        for norm_phone, name in self._contact_map.items():
            if self._phone_has_conversation(norm_phone):
                continue
            if text in name.lower() or text in norm_phone:
                suggestions.append((name, norm_phone))

        # Deduplicate (the typed-number entry may duplicate a contact)
        seen = set()
        unique = []
        for name, phone in suggestions:
            norm = re.sub(r"[^\d]", "", phone)
            key = norm[-10:] if len(norm) >= 10 else norm
            if key not in seen:
                seen.add(key)
                unique.append((name, phone))
        suggestions = unique[:25]

        if suggestions:
            self._contacts_header.set_visible(True)
            for name, phone in suggestions:
                self._contacts_listbox.append(ContactSuggestionRow(name, phone))
        else:
            self._contacts_header.set_visible(False)

    def _phone_has_conversation(self, norm_phone: str) -> bool:
        """Check if a normalized phone number already has a conversation."""
        if norm_phone in self._conversation_phones:
            return True
        if len(norm_phone) >= 10:
            short = norm_phone[-10:]
            for cp in self._conversation_phones:
                if len(cp) >= 10 and cp[-10:] == short:
                    return True
        return False

    def _on_contact_activated(self, listbox, row):
        """Handle clicking a contact suggestion."""
        if hasattr(row, "contact_phone"):
            self._search_entry.set_text("")
            self.emit("start-conversation", row.contact_phone, row.contact_name)

    def _on_right_click(self, gesture, n_press, x, y):
        """Show a context menu to rename the contact."""
        row = self._listbox.get_row_at_y(int(y))
        if not row or not hasattr(row, "conversation"):
            return
        conv = row.conversation
        self._listbox.select_row(row)

        popover = Gtk.PopoverMenu()
        menu = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        menu.set_margin_top(4)
        menu.set_margin_bottom(4)
        menu.set_margin_start(4)
        menu.set_margin_end(4)

        rename_btn = Gtk.Button(label="Set contact name")
        rename_btn.add_css_class("flat")
        rename_btn.connect("clicked", lambda _: (
            popover.popdown(),
            self.emit("rename-contact", conv.thread_id, conv.address),
        ))
        menu.append(rename_btn)

        delete_btn = Gtk.Button(label="Delete conversation")
        delete_btn.add_css_class("flat")
        delete_btn.add_css_class("destructive-action")
        delete_btn.connect("clicked", lambda _: (
            popover.popdown(),
            self.emit("delete-conversation", conv.thread_id),
        ))
        menu.append(delete_btn)

        popover.set_child(menu)
        popover.set_parent(row)
        popover.set_has_arrow(True)
        popover.popup()

    def _make_placeholder(self):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        box.set_valign(Gtk.Align.CENTER)
        box.set_margin_top(48)

        icon = Gtk.Image.new_from_icon_name("mail-unread-symbolic")
        icon.set_pixel_size(32)
        icon.set_opacity(0.3)
        box.append(icon)

        label = Gtk.Label(label="No conversations yet")
        label.add_css_class("dim-label")
        box.append(label)

        hint = Gtk.Label(label="Waiting for messages from phone…")
        hint.add_css_class("dim-label")
        hint.add_css_class("caption")
        box.append(hint)

        return box
