"""Conversation list widget — left column of the SMS panel."""

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gtk, GObject, Pango


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

        # Contact avatar placeholder (initial letter circle)
        avatar_label = Gtk.Label()
        avatar_label.set_size_request(40, 40)
        initial = (conversation.display_name or conversation.address or "?")[0].upper()
        avatar_label.set_label(initial)
        avatar_label.add_css_class("conversation-avatar")
        avatar_label.set_halign(Gtk.Align.CENTER)
        avatar_label.set_valign(Gtk.Align.CENTER)
        box.append(avatar_label)

        # Text column: name + preview
        text_col = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        text_col.set_hexpand(True)
        box.append(text_col)

        # Top row: name + time
        top_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        text_col.append(top_row)

        name_label = Gtk.Label(
            label=conversation.display_name or conversation.address or "Unknown"
        )
        name_label.set_halign(Gtk.Align.START)
        name_label.set_hexpand(True)
        name_label.set_ellipsize(Pango.EllipsizeMode.END)
        name_label.set_max_width_chars(20)
        if not conversation.is_read:
            name_label.add_css_class("heading")
        top_row.append(name_label)

        time_label = Gtk.Label(label=conversation.time_label)
        time_label.add_css_class("dim-label")
        time_label.add_css_class("caption")
        top_row.append(time_label)

        # Preview line
        preview_label = Gtk.Label(label=conversation.preview)
        preview_label.set_halign(Gtk.Align.START)
        preview_label.set_ellipsize(Pango.EllipsizeMode.END)
        preview_label.set_max_width_chars(30)
        preview_label.add_css_class("dim-label")
        preview_label.add_css_class("caption")
        text_col.append(preview_label)

        # Unread dot
        if not conversation.is_read:
            dot = Gtk.Label(label="●")
            dot.add_css_class("unread-dot")
            dot.set_valign(Gtk.Align.CENTER)
            box.append(dot)


class ConversationList(Gtk.Box):
    """Scrollable list of conversations with search bar."""

    __gsignals__ = {
        "conversation-selected": (GObject.SignalFlags.RUN_FIRST, None, (int,)),
        "new-conversation": (GObject.SignalFlags.RUN_FIRST, None, ()),
        "rename-contact": (GObject.SignalFlags.RUN_FIRST, None, (int, str)),
        "import-contacts": (GObject.SignalFlags.RUN_FIRST, None, ()),
    }

    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.set_size_request(280, -1)
        self._conversations = []
        self._rendered_read_states: dict[int, bool] = {}  # thread_id → is_read at last render

        # Search bar
        search_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        search_box.set_margin_top(8)
        search_box.set_margin_bottom(4)
        search_box.set_margin_start(8)
        search_box.set_margin_end(8)

        self._search_entry = Gtk.SearchEntry()
        self._search_entry.set_placeholder_text("Search messages…")
        self._search_entry.set_hexpand(True)
        self._search_entry.connect("search-changed", self._on_search_changed)
        search_box.append(self._search_entry)

        new_btn = Gtk.Button(icon_name="list-add-symbolic")
        new_btn.set_tooltip_text("New message")
        new_btn.connect("clicked", lambda _: self.emit("new-conversation"))
        search_box.append(new_btn)

        import_btn = Gtk.Button(icon_name="system-users-symbolic")
        import_btn.set_tooltip_text("Sync contacts from phone")
        import_btn.connect("clicked", lambda _: self.emit("import-contacts"))
        search_box.append(import_btn)

        self.append(search_box)

        # List
        scrolled = Gtk.ScrolledWindow()
        scrolled.set_vexpand(True)
        scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        self.append(scrolled)

        self._listbox = Gtk.ListBox()
        self._listbox.set_selection_mode(Gtk.SelectionMode.SINGLE)
        self._listbox.set_activate_on_single_click(True)
        self._listbox.set_placeholder(self._make_placeholder())
        self._listbox.connect("row-activated", self._on_row_activated)
        self._listbox.connect("row-selected", self._on_row_selected)
        self._listbox.set_filter_func(self._filter_func)
        scrolled.set_child(self._listbox)

        # Right-click menu for rename
        gesture = Gtk.GestureClick(button=3)  # right-click
        gesture.connect("pressed", self._on_right_click)
        self._listbox.add_controller(gesture)

        self._search_text = ""
        self._last_selected_id = None

    def set_conversations(self, conversations, force_rebuild=False):
        """Update the conversation list, rebuilding only when the set changes."""
        sorted_convs = sorted(conversations, key=lambda c: c.sort_key, reverse=True)

        # Build a set of current thread IDs for comparison
        new_ids = [c.thread_id for c in sorted_convs]
        old_ids = [c.thread_id for c in self._conversations]

        # Detect read-state changes (snapshot comparison, not object identity)
        new_read_states = {c.thread_id: c.is_read for c in sorted_convs}
        read_changed = new_read_states != self._rendered_read_states

        self._conversations = sorted_convs

        # Rebuild if the conversation set/order/read-state changed or forced
        if force_rebuild or new_ids != old_ids or read_changed:
            self._rendered_read_states = new_read_states
            self._last_selected_id = None
            # Remove old rows
            while True:
                row = self._listbox.get_row_at_index(0)
                if row is None:
                    break
                self._listbox.remove(row)
            # Add sorted rows
            for conv in sorted_convs:
                self._listbox.append(ConversationRow(conv))

    def select_thread(self, thread_id: int):
        """Programmatically select a conversation row by thread ID."""
        idx = 0
        while True:
            row = self._listbox.get_row_at_index(idx)
            if row is None:
                break
            if row.conversation.thread_id == thread_id:
                self._listbox.select_row(row)
                break
            idx += 1

    def _on_row_activated(self, listbox, row):
        if row and hasattr(row, "conversation"):
            self._last_selected_id = row.conversation.thread_id
            self.emit("conversation-selected", row.conversation.thread_id)

    def _on_row_selected(self, listbox, row):
        # Backup path: emit conversation-selected on selection change too,
        # unless row-activated already handled this exact row.
        if row and hasattr(row, "conversation"):
            tid = row.conversation.thread_id
            if getattr(self, "_last_selected_id", None) != tid:
                self._last_selected_id = tid
                self.emit("conversation-selected", tid)

    def _on_search_changed(self, entry):
        self._search_text = entry.get_text().lower()
        self._listbox.invalidate_filter()

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
