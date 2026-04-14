"""Clipboard sync panel — shared clipboard history between PC and phone."""

from datetime import datetime

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, Gdk, GLib

from phonelink.dbus_client import IFACE_CLIPBOARD


class ClipboardPanel(Gtk.Box):
    """Displays clipboard history synced between PC and phone."""

    MAX_HISTORY = 50

    def __init__(self, client):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.client = client
        self._device = None
        self._signal_ids: list[int] = []
        self._history: list[dict] = []  # {text, source, timestamp}

        # ── Outer stack: status vs content ─────────────────────────
        self._outer_stack = Gtk.Stack()
        self._outer_stack.set_vexpand(True)
        self._outer_stack.set_hexpand(True)
        self._outer_stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.append(self._outer_stack)

        # Status page (no device)
        status = Adw.StatusPage()
        status.set_icon_name("edit-paste-symbolic")
        status.set_title("Clipboard Sync")
        status.set_description(
            "No device connected.\n"
            "Pair a phone to sync clipboard content."
        )
        self._outer_stack.add_named(status, "status")

        # ── Content ────────────────────────────────────────────────
        content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self._outer_stack.add_named(content, "content")

        # Toolbar
        toolbar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        toolbar.set_margin_start(12)
        toolbar.set_margin_end(12)
        toolbar.set_margin_top(8)
        toolbar.set_margin_bottom(8)
        content.append(toolbar)

        push_btn = Gtk.Button()
        push_btn.add_css_class("flat")
        push_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        push_row.append(Gtk.Image.new_from_icon_name("go-up-symbolic"))
        push_row.append(Gtk.Label(label="Send Clipboard to Phone"))
        push_btn.set_child(push_row)
        push_btn.set_tooltip_text("Push your current PC clipboard text to the phone")
        push_btn.connect("clicked", self._on_push_clipboard)
        toolbar.append(push_btn)

        pull_btn = Gtk.Button()
        pull_btn.add_css_class("flat")
        pull_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        pull_row.append(Gtk.Image.new_from_icon_name("go-down-symbolic"))
        pull_row.append(Gtk.Label(label="Get from Phone"))
        pull_btn.set_child(pull_row)
        pull_btn.set_tooltip_text("Fetch the phone's current clipboard content")
        pull_btn.connect("clicked", self._on_pull_clipboard)
        toolbar.append(pull_btn)

        spacer = Gtk.Box()
        spacer.set_hexpand(True)
        toolbar.append(spacer)

        clear_btn = Gtk.Button(icon_name="user-trash-symbolic")
        clear_btn.add_css_class("flat")
        clear_btn.set_tooltip_text("Clear history")
        clear_btn.connect("clicked", self._on_clear_history)
        toolbar.append(clear_btn)

        content.append(Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL))

        # Clipboard history list
        scroll = Gtk.ScrolledWindow()
        scroll.set_vexpand(True)
        scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        content.append(scroll)

        self._list_box = Gtk.ListBox()
        self._list_box.set_selection_mode(Gtk.SelectionMode.NONE)
        self._list_box.set_placeholder(self._make_placeholder())
        scroll.set_child(self._list_box)

    def _make_placeholder(self):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        box.set_valign(Gtk.Align.CENTER)
        box.set_margin_top(40)
        icon = Gtk.Image.new_from_icon_name("edit-paste-symbolic")
        icon.set_pixel_size(48)
        icon.set_opacity(0.3)
        box.append(icon)
        label = Gtk.Label(label="Clipboard history will appear here")
        label.add_css_class("dim-label")
        box.append(label)
        return box

    # ── Device management ──────────────────────────────────────────

    def set_device(self, device):
        self._unsubscribe()
        self._device = device
        if not device or not device.reachable:
            self._outer_stack.set_visible_child_name("status")
            return
        self._outer_stack.set_visible_child_name("content")
        self._subscribe(device.id)

    def _subscribe(self, device_id):
        path = f"/modules/kdeconnect/devices/{device_id}"
        sid = self.client.subscribe_signal(
            path, IFACE_CLIPBOARD, "clipboardChanged",
            self._on_clipboard_changed,
        )
        if sid is not None:
            self._signal_ids.append(sid)

    def _unsubscribe(self):
        if self.client.bus:
            for sid in self._signal_ids:
                self.client.bus.signal_unsubscribe(sid)
        self._signal_ids.clear()

    # ── Signal handlers ────────────────────────────────────────────

    def _on_clipboard_changed(self, conn, sender, path, iface, signal, params):
        text = params.unpack()[0]
        if text:
            GLib.idle_add(self._add_entry, text, "phone")

    # ── Actions ────────────────────────────────────────────────────

    def _on_push_clipboard(self, _btn):
        if not self._device or not self._device.reachable:
            return
        clipboard = Gdk.Display.get_default().get_clipboard()
        clipboard.read_text_async(None, self._on_local_text_read)

    def _on_local_text_read(self, clipboard, result):
        try:
            text = clipboard.read_text_finish(result)
        except GLib.Error:
            return
        if not text or not self._device:
            return
        self.client.send_clipboard(self._device.id, text)
        self._add_entry(text, "pc")

    def _on_pull_clipboard(self, _btn):
        if not self._device or not self._device.reachable:
            return
        text = self.client.get_clipboard_content(self._device.id)
        if text:
            clipboard = Gdk.Display.get_default().get_clipboard()
            clipboard.set(text)
            self._add_entry(text, "phone")

    def _on_clear_history(self, _btn):
        self._history.clear()
        self._list_box.remove_all()

    # ── History management ─────────────────────────────────────────

    def _add_entry(self, text: str, source: str):
        """Add a clipboard entry to the history list."""
        # Skip duplicates of the most recent entry
        if self._history and self._history[0]["text"] == text:
            return

        entry = {
            "text": text,
            "source": source,
            "timestamp": datetime.now(),
        }
        self._history.insert(0, entry)

        # Trim history
        if len(self._history) > self.MAX_HISTORY:
            self._history.pop()
            last = self._list_box.get_row_at_index(self.MAX_HISTORY - 1)
            if last:
                self._list_box.remove(last)

        row = self._make_row(entry)
        self._list_box.prepend(row)

    def _make_row(self, entry: dict) -> Gtk.ListBoxRow:
        row = Gtk.ListBoxRow()
        row.set_activatable(False)

        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        box.set_margin_start(12)
        box.set_margin_end(12)
        box.set_margin_top(8)
        box.set_margin_bottom(8)
        row.set_child(box)

        # Source icon
        icon_name = (
            "phone-symbolic" if entry["source"] == "phone"
            else "computer-symbolic"
        )
        icon = Gtk.Image.new_from_icon_name(icon_name)
        icon.set_pixel_size(16)
        icon.set_valign(Gtk.Align.START)
        icon.set_margin_top(4)
        icon.set_opacity(0.6)
        box.append(icon)

        # Text content
        text_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        text_box.set_hexpand(True)
        box.append(text_box)

        preview = entry["text"][:200]
        if len(entry["text"]) > 200:
            preview += "…"
        label = Gtk.Label(label=preview)
        label.set_xalign(0)
        label.set_wrap(True)
        label.set_wrap_mode(2)  # WORD_CHAR
        label.set_max_width_chars(60)
        label.set_selectable(True)
        text_box.append(label)

        time_str = entry["timestamp"].strftime("%I:%M %p").lstrip("0")
        time_label = Gtk.Label(label=time_str)
        time_label.set_xalign(0)
        time_label.add_css_class("caption")
        time_label.add_css_class("dim-label")
        text_box.append(time_label)

        # Copy button
        copy_btn = Gtk.Button(icon_name="edit-copy-symbolic")
        copy_btn.add_css_class("flat")
        copy_btn.set_valign(Gtk.Align.CENTER)
        copy_btn.set_tooltip_text("Copy to clipboard")
        copy_btn.connect("clicked", self._on_copy_entry, entry["text"])
        box.append(copy_btn)

        return row

    def _on_copy_entry(self, _btn, text):
        clipboard = Gdk.Display.get_default().get_clipboard()
        clipboard.set(text)
