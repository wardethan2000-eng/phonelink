"""Message thread view — chat-bubble display + text entry."""

import os
import tempfile

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gtk, Gdk, GObject, Pango, GLib, Gio


class MessageBubble(Gtk.Box):
    """A single chat bubble."""

    def __init__(self, message, show_time=True):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        self.message = message

        sent = message.is_sent

        self.set_halign(Gtk.Align.END if sent else Gtk.Align.START)
        self.set_margin_top(2)
        self.set_margin_bottom(2)
        self.set_margin_start(12)
        self.set_margin_end(12)
        # Limit bubble width so sent/received don't span the full width
        self.set_size_request(-1, -1)
        if sent:
            self.set_margin_start(48)
        else:
            self.set_margin_end(48)

        # Bubble frame
        frame = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        frame.set_margin_top(4)
        frame.set_margin_bottom(4)
        frame.set_margin_start(12)
        frame.set_margin_end(12)
        frame.add_css_class("message-bubble-sent" if sent else "message-bubble-received")
        self.append(frame)

        # Message text
        body_label = Gtk.Label(label=message.body or "")
        body_label.set_wrap(True)
        body_label.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
        body_label.set_max_width_chars(50)
        body_label.set_halign(Gtk.Align.START)
        body_label.set_selectable(True)
        frame.append(body_label)

        # Attachment indicators
        if message.attachments:
            for att in message.attachments:
                mime = att.get("mimeType", "")
                fname = att.get("fileName", "attachment")
                att_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
                att_icon = Gtk.Image.new_from_icon_name(
                    "image-x-generic-symbolic" if mime.startswith("image")
                    else "mail-attachment-symbolic"
                )
                att_icon.set_pixel_size(14)
                att_row.append(att_icon)
                att_label = Gtk.Label(label=fname)
                att_label.add_css_class("caption")
                att_label.set_ellipsize(Pango.EllipsizeMode.END)
                att_row.append(att_label)
                frame.append(att_row)

        # Timestamp (only shown when there's a time gap from previous message)
        if show_time:
            time_label = Gtk.Label(label=message.time_label)
            time_label.set_halign(Gtk.Align.END if sent else Gtk.Align.START)
            time_label.add_css_class("dim-label")
            time_label.add_css_class("caption")
            time_label.set_margin_start(12 if not sent else 0)
            time_label.set_margin_end(12 if sent else 0)
            self.append(time_label)


class DateSeparator(Gtk.Box):
    """A date header between message groups."""

    def __init__(self, label_text: str):
        super().__init__(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        self.set_halign(Gtk.Align.CENTER)
        self.set_margin_top(12)
        self.set_margin_bottom(8)

        label = Gtk.Label(label=label_text)
        label.add_css_class("dim-label")
        label.add_css_class("caption")
        label.add_css_class("date-separator")
        self.append(label)


class MessageThread(Gtk.Box):
    """Full message thread: header, scrolling messages, and compose bar."""

    __gsignals__ = {
        "send-message": (GObject.SignalFlags.RUN_FIRST, None, (int, str)),
        "send-message-with-attachment": (GObject.SignalFlags.RUN_FIRST, None, (int, str, str)),
    }

    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self._thread_id = 0
        self._contact_name = ""
        self._pending_image_path = None  # path to pasted image awaiting send
        self._last_message = None  # track last message for smart timestamps
        self.set_hexpand(True)
        self.set_vexpand(True)

        # ── Stack: empty state vs thread ───────────────────────────
        self._stack = Gtk.Stack()
        self._stack.set_vexpand(True)
        self._stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.append(self._stack)

        # Empty state
        empty = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        empty.set_valign(Gtk.Align.CENTER)
        empty.set_halign(Gtk.Align.CENTER)
        icon = Gtk.Image.new_from_icon_name("mail-unread-symbolic")
        icon.set_pixel_size(48)
        icon.set_opacity(0.2)
        empty.append(icon)
        hint = Gtk.Label(label="Select a conversation")
        hint.add_css_class("dim-label")
        hint.add_css_class("title-4")
        empty.append(hint)
        self._stack.add_named(empty, "empty")

        # Thread view
        thread_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self._stack.add_named(thread_box, "thread")

        # ── Contact header bar inside thread ──────────────────────
        header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        header.set_margin_start(12)
        header.set_margin_end(12)
        header.set_margin_top(8)
        header.set_margin_bottom(8)
        header.add_css_class("thread-header")

        self._contact_label = Gtk.Label()
        self._contact_label.add_css_class("title-3")
        self._contact_label.set_hexpand(True)
        self._contact_label.set_halign(Gtk.Align.START)
        self._contact_label.set_ellipsize(Pango.EllipsizeMode.END)
        header.append(self._contact_label)

        self._address_label = Gtk.Label()
        self._address_label.add_css_class("dim-label")
        self._address_label.add_css_class("caption")
        header.append(self._address_label)

        thread_box.append(header)
        thread_box.append(Gtk.Separator())

        # ── Messages scroll area ──────────────────────────────────
        self._scroll = Gtk.ScrolledWindow()
        self._scroll.set_vexpand(True)
        self._scroll.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
        self._scroll.set_propagate_natural_width(False)
        thread_box.append(self._scroll)

        self._messages_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self._messages_box.set_margin_bottom(8)
        self._scroll.set_child(self._messages_box)

        # Loading indicator
        self._loading_spinner = Gtk.Spinner()
        self._loading_spinner.set_halign(Gtk.Align.CENTER)
        self._loading_spinner.set_margin_top(12)
        self._loading_spinner.set_visible(False)
        thread_box.append(self._loading_spinner)

        # ── Compose bar ───────────────────────────────────────────
        thread_box.append(Gtk.Separator())

        # Image preview bar (shown when an image is pasted)
        self._image_preview_bar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        self._image_preview_bar.set_margin_start(12)
        self._image_preview_bar.set_margin_end(12)
        self._image_preview_bar.set_margin_top(6)
        self._image_preview_bar.set_visible(False)

        self._image_preview_icon = Gtk.Image.new_from_icon_name("image-x-generic-symbolic")
        self._image_preview_icon.set_pixel_size(20)
        self._image_preview_bar.append(self._image_preview_icon)

        self._image_preview_label = Gtk.Label(label="Image pasted")
        self._image_preview_label.add_css_class("caption")
        self._image_preview_label.set_hexpand(True)
        self._image_preview_label.set_halign(Gtk.Align.START)
        self._image_preview_bar.append(self._image_preview_label)

        remove_img_btn = Gtk.Button(icon_name="edit-clear-symbolic")
        remove_img_btn.add_css_class("flat")
        remove_img_btn.set_tooltip_text("Remove image")
        remove_img_btn.connect("clicked", self._on_remove_image)
        self._image_preview_bar.append(remove_img_btn)

        thread_box.append(self._image_preview_bar)

        compose = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        compose.set_margin_start(12)
        compose.set_margin_end(12)
        compose.set_margin_top(8)
        compose.set_margin_bottom(8)

        self._text_entry = Gtk.Entry()
        self._text_entry.set_placeholder_text("Type a message…")
        self._text_entry.set_hexpand(True)
        self._text_entry.connect("activate", self._on_send)

        # Ctrl+V paste handler for images
        paste_controller = Gtk.EventControllerKey()
        paste_controller.connect("key-pressed", self._on_key_pressed)
        self._text_entry.add_controller(paste_controller)

        compose.append(self._text_entry)

        send_btn = Gtk.Button(icon_name="mail-send-symbolic")
        send_btn.add_css_class("suggested-action")
        send_btn.set_tooltip_text("Send")
        send_btn.connect("clicked", self._on_send)
        compose.append(send_btn)

        thread_box.append(compose)

        self._stack.set_visible_child_name("empty")

    def show_empty(self):
        self._stack.set_visible_child_name("empty")

    def show_loading(self, contact_name: str, address: str):
        self._contact_label.set_label(contact_name or address or "…")
        self._address_label.set_label(address if contact_name else "")
        self._clear_messages()
        self._loading_spinner.set_visible(True)
        self._loading_spinner.start()
        self._stack.set_visible_child_name("thread")

    def set_messages(self, messages, contact_name: str, address: str,
                     thread_id: int):
        self._thread_id = thread_id
        self._contact_name = contact_name
        self._contact_label.set_label(contact_name or address or "Unknown")
        self._address_label.set_label(address if contact_name else "")

        self._loading_spinner.stop()
        self._loading_spinner.set_visible(False)

        self._clear_messages()

        # Sort messages by date ascending
        sorted_msgs = sorted(messages, key=lambda m: m.date)

        # Insert messages with date separators and smart timestamps
        # Show timestamp when: first message, sender changes, or 5+ minute gap
        TIME_GAP_SECONDS = 300  # 5 minutes
        last_date_str = ""
        prev_msg = None
        for i, msg in enumerate(sorted_msgs):
            date_str = msg.timestamp.strftime("%A, %B %d, %Y")
            if date_str != last_date_str:
                self._messages_box.append(DateSeparator(date_str))
                last_date_str = date_str

            is_last = (i == len(sorted_msgs) - 1)
            if is_last:
                show_time = True
            elif prev_msg is None:
                show_time = True
            elif msg.is_sent != prev_msg.is_sent:
                show_time = True
            elif abs(msg.date - prev_msg.date) >= TIME_GAP_SECONDS * 1000:
                show_time = True
            else:
                # Peek ahead: show time if next message changes sender or has gap
                next_msg = sorted_msgs[i + 1]
                if next_msg.is_sent != msg.is_sent or abs(next_msg.date - msg.date) >= TIME_GAP_SECONDS * 1000:
                    show_time = True
                else:
                    show_time = False

            self._messages_box.append(MessageBubble(msg, show_time=show_time))
            prev_msg = msg

        self._last_message = sorted_msgs[-1] if sorted_msgs else None
        self._stack.set_visible_child_name("thread")

        # Scroll to bottom after layout
        GLib.idle_add(self._scroll_to_bottom)

    def append_message(self, message):
        """Add a single new message and scroll down."""
        # Always show time on the latest appended message
        self._messages_box.append(MessageBubble(message, show_time=True))
        self._last_message = message
        GLib.idle_add(self._scroll_to_bottom)

    def _clear_messages(self):
        while True:
            child = self._messages_box.get_first_child()
            if child is None:
                break
            self._messages_box.remove(child)

    def _scroll_to_bottom(self):
        adj = self._scroll.get_vadjustment()
        adj.set_value(adj.get_upper())
        return GLib.SOURCE_REMOVE

    def _on_send(self, _widget):
        text = self._text_entry.get_text().strip()
        if self._pending_image_path and self._thread_id:
            self.emit("send-message-with-attachment", self._thread_id, text, self._pending_image_path)
            self._text_entry.set_text("")
            self._clear_pending_image()
        elif text and self._thread_id:
            self.emit("send-message", self._thread_id, text)
            self._text_entry.set_text("")

    def _on_key_pressed(self, controller, keyval, keycode, state):
        """Handle Ctrl+V to paste images from clipboard."""
        if keyval == Gdk.KEY_v and (state & Gdk.ModifierType.CONTROL_MASK):
            clipboard = Gdk.Display.get_default().get_clipboard()
            # Check for image content
            formats = clipboard.get_formats()
            if formats.contain_mime_type("image/png") or formats.contain_mime_type("image/jpeg"):
                clipboard.read_texture_async(None, self._on_clipboard_texture)
                return True  # handled
        return False

    def _on_clipboard_texture(self, clipboard, result):
        """Callback when clipboard texture is read."""
        try:
            texture = clipboard.read_texture_finish(result)
            if texture is None:
                return
            # Save texture to a temp file
            tmp_dir = os.path.join(tempfile.gettempdir(), "phonelink")
            os.makedirs(tmp_dir, exist_ok=True)
            tmp_path = os.path.join(tmp_dir, f"paste_{os.getpid()}.png")
            texture.save_to_png(tmp_path)
            self._pending_image_path = tmp_path
            self._image_preview_label.set_label("Image ready to send")
            self._image_preview_bar.set_visible(True)
        except Exception as e:
            print(f"[phonelink] Clipboard paste error: {e}")

    def _on_remove_image(self, _btn):
        self._clear_pending_image()

    def _clear_pending_image(self):
        if self._pending_image_path and os.path.exists(self._pending_image_path):
            try:
                os.unlink(self._pending_image_path)
            except OSError:
                pass
        self._pending_image_path = None
        self._image_preview_bar.set_visible(False)
