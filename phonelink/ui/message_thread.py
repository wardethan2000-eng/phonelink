"""Message thread view — chat-bubble display + text entry."""

import base64
import binascii
import hashlib
import os
import shutil
import tempfile
from pathlib import Path
from urllib.parse import unquote, urlparse

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gtk, Gdk, GObject, Pango, GLib, Gio


ATTACHMENT_CACHE_DIR = Path(tempfile.gettempdir()) / "phonelink" / "message_attachments"
IMAGE_CLIPBOARD_MIME_TYPES = (
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/webp",
    "image/gif",
    "image/bmp",
    "image/tiff",
    "image/heic",
    "image/heif",
)
IMAGE_FILE_EXTS = {".png", ".jpg", ".jpeg", ".webp", ".gif", ".bmp", ".tiff", ".tif", ".heic", ".heif"}


def _attachment_ext(mime_type: str) -> str:
    mime = (mime_type or "").split(";", 1)[0].strip().lower()
    mapping = {
        "image/jpeg": ".jpg",
        "image/jpg": ".jpg",
        "image/png": ".png",
        "image/gif": ".gif",
        "image/webp": ".webp",
        "image/heic": ".heic",
        "image/heif": ".heif",
    }
    return mapping.get(mime, "")


def _sanitize_name(text: str) -> str:
    cleaned = "".join(ch if ch.isalnum() or ch in {"-", "_", "."} else "_" for ch in text)
    return cleaned.strip("._") or "attachment"


def _attachment_name(att: dict) -> str:
    file_name = _sanitize_name(str(att.get("fileName") or "attachment"))
    ext = _attachment_ext(str(att.get("mimeType") or ""))
    return file_name if not ext or file_name.lower().endswith(ext.lower()) else f"{file_name}{ext}"


def _attachment_local_path(att: dict) -> str | None:
    payload = str(att.get("payload") or "")
    if not payload:
        return None

    if payload.startswith("file://"):
        file_path = unquote(urlparse(payload).path)
        return file_path if os.path.isfile(file_path) else None

    if os.path.isfile(payload):
        return payload

    ATTACHMENT_CACHE_DIR.mkdir(parents=True, exist_ok=True)
    unique_identifier = str(att.get("uniqueIdentifier") or att.get("partId") or "attachment")
    digest = hashlib.sha1(payload.encode("utf-8", errors="ignore")).hexdigest()[:12]
    target = ATTACHMENT_CACHE_DIR / f"{_sanitize_name(unique_identifier)}-{digest}{_attachment_ext(str(att.get('mimeType') or ''))}"
    if target.is_file():
        return str(target)

    try:
        data = base64.b64decode(payload, validate=False)
    except (binascii.Error, ValueError):
        return None
    if not data:
        return None
    try:
        target.write_bytes(data)
    except OSError:
        return None
    return str(target)


class MessageBubble(Gtk.Box):
    """A single chat bubble."""

    def __init__(self, message, show_time=True, on_download_attachment=None):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=1)
        self.message = message
        self._on_download_attachment = on_download_attachment
        self._timestamp_visible = bool(show_time)

        sent = message.is_sent

        self.set_halign(Gtk.Align.END if sent else Gtk.Align.START)
        self.set_margin_top(1)
        self.set_margin_bottom(1)
        self.set_margin_start(12)
        self.set_margin_end(12)
        # Limit bubble width so sent/received don't span the full width
        self.set_size_request(-1, -1)
        if sent:
            self.set_margin_start(48)
        else:
            self.set_margin_end(48)

        # Bubble frame
        frame = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=3)
        frame.set_margin_top(2)
        frame.set_margin_bottom(2)
        frame.set_margin_start(12)
        frame.set_margin_end(12)
        frame.add_css_class("message-bubble-sent" if sent else "message-bubble-received")
        self.append(frame)

        click = Gtk.GestureClick(button=1)
        click.connect("pressed", self._on_bubble_pressed)
        frame.add_controller(click)

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
                self._append_attachment(frame, att)

        self._time_revealer = Gtk.Revealer()
        self._time_revealer.set_transition_type(Gtk.RevealerTransitionType.SLIDE_DOWN)
        self._time_revealer.set_transition_duration(160)

        time_label = Gtk.Label(label=("Sent " if sent else "Received ") + message.time_label)
        time_label.set_halign(Gtk.Align.END if sent else Gtk.Align.START)
        time_label.add_css_class("dim-label")
        time_label.add_css_class("caption")
        time_label.add_css_class("message-timestamp")
        time_label.set_margin_start(12 if not sent else 0)
        time_label.set_margin_end(12 if sent else 0)
        self._time_revealer.set_child(time_label)
        self._time_revealer.set_reveal_child(show_time)
        self.append(self._time_revealer)

        if show_time:
            self.add_css_class("message-bubble-timestamp-visible")

    def _append_attachment(self, frame: Gtk.Box, attachment: dict):
        mime = str(attachment.get("mimeType") or "")
        file_name = _attachment_name(attachment)
        local_path = _attachment_local_path(attachment)

        if mime.startswith("image") and local_path:
            picture = Gtk.Picture.new_for_filename(local_path)
            picture.set_can_shrink(True)
            picture.set_content_fit(Gtk.ContentFit.SCALE_DOWN)
            picture.set_size_request(240, 180)
            picture.add_css_class("message-attachment-image")
            frame.append(picture)

        att_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        att_row.add_css_class("message-attachment-row")

        att_icon = Gtk.Image.new_from_icon_name(
            "image-x-generic-symbolic" if mime.startswith("image") else "mail-attachment-symbolic"
        )
        att_icon.set_pixel_size(14)
        att_row.append(att_icon)

        att_label = Gtk.Label(label=file_name)
        att_label.add_css_class("caption")
        att_label.set_hexpand(True)
        att_label.set_halign(Gtk.Align.START)
        att_label.set_ellipsize(Pango.EllipsizeMode.END)
        att_row.append(att_label)

        if local_path:
            if mime.startswith("image"):
                copy_btn = Gtk.Button(label="Copy")
                copy_btn.add_css_class("flat")
                copy_btn.connect("clicked", self._on_copy_image, local_path)
                att_row.append(copy_btn)

            open_btn = Gtk.Button(label="Open")
            open_btn.add_css_class("flat")
            open_btn.connect("clicked", self._on_open_attachment, local_path)
            att_row.append(open_btn)

            save_btn = Gtk.Button(label="Save")
            save_btn.add_css_class("flat")
            save_btn.connect("clicked", self._on_save_attachment, local_path, file_name)
            att_row.append(save_btn)
        elif self._on_download_attachment is not None:
            download_btn = Gtk.Button(label="Download")
            download_btn.add_css_class("flat")
            download_btn.connect("clicked", self._on_download_clicked, attachment, file_name)
            att_row.append(download_btn)

        frame.append(att_row)

    def _on_download_clicked(self, _btn, attachment: dict, file_name: str):
        self._on_download_attachment(
            int(attachment.get("partId") or 0),
            str(attachment.get("uniqueIdentifier") or ""),
            file_name,
        )

    def _on_copy_image(self, _btn, local_path: str):
        display = self.get_display()
        if display is None:
            return
        try:
            texture = Gdk.Texture.new_from_filename(local_path)
        except GLib.Error:
            return
        display.get_clipboard().set_texture(texture)

    def _on_open_attachment(self, _btn, local_path: str):
        try:
            Gio.AppInfo.launch_default_for_uri(f"file://{local_path}", None)
        except GLib.Error:
            pass

    def _on_save_attachment(self, _btn, local_path: str, file_name: str):
        dialog = Gtk.FileDialog()
        dialog.set_title("Save Attachment")
        dialog.select_folder(
            self.get_root(),
            None,
            lambda d, r: self._finish_save_attachment(d, r, local_path, file_name),
        )

    def _finish_save_attachment(self, dialog, result, local_path: str, file_name: str):
        try:
            folder = dialog.select_folder_finish(result)
        except GLib.Error:
            return
        if not folder:
            return
        folder_path = folder.get_path()
        if not folder_path:
            return
        target = os.path.join(folder_path, file_name)
        if os.path.exists(target):
            stem, ext = os.path.splitext(file_name)
            counter = 1
            while os.path.exists(target):
                target = os.path.join(folder_path, f"{stem}_{counter}{ext}")
                counter += 1
        try:
            shutil.copy2(local_path, target)
        except OSError:
            pass

    def set_default_timestamp_visible(self, visible: bool):
        self._timestamp_visible = bool(visible)
        self._time_revealer.set_reveal_child(self._timestamp_visible)
        if self._timestamp_visible:
            self.add_css_class("message-bubble-timestamp-visible")
        else:
            self.remove_css_class("message-bubble-timestamp-visible")

    def _on_bubble_pressed(self, *_args):
        self.set_default_timestamp_visible(not self._timestamp_visible)


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
        "download-attachment": (GObject.SignalFlags.RUN_FIRST, None, (int, int, str, str)),
    }

    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self._thread_id = 0
        self._contact_name = ""
        self._pending_image_path = None  # path to pasted image awaiting send
        self._retained_temp_paths: set[str] = set()
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
        self._image_preview_bar = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        self._image_preview_bar.set_margin_start(12)
        self._image_preview_bar.set_margin_end(12)
        self._image_preview_bar.set_margin_top(6)
        self._image_preview_bar.set_margin_bottom(2)
        self._image_preview_bar.add_css_class("compose-image-preview-card")
        self._image_preview_bar.set_visible(False)

        self._image_preview_picture = Gtk.Picture()
        self._image_preview_picture.set_halign(Gtk.Align.START)
        self._image_preview_picture.set_size_request(240, 240)
        self._image_preview_picture.set_can_shrink(True)
        self._image_preview_picture.set_content_fit(Gtk.ContentFit.COVER)
        self._image_preview_picture.add_css_class("compose-image-preview")
        self._image_preview_bar.append(self._image_preview_picture)

        image_preview_footer = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        image_preview_footer.set_hexpand(True)
        image_preview_footer.set_valign(Gtk.Align.CENTER)
        self._image_preview_bar.append(image_preview_footer)

        image_preview_text = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        image_preview_text.set_hexpand(True)
        image_preview_footer.append(image_preview_text)

        self._image_preview_label = Gtk.Label(label="Image pasted")
        self._image_preview_label.add_css_class("caption")
        self._image_preview_label.add_css_class("heading")
        self._image_preview_label.set_halign(Gtk.Align.START)
        self._image_preview_label.set_xalign(0)
        self._image_preview_label.set_wrap(True)
        image_preview_text.append(self._image_preview_label)

        self._image_preview_hint = Gtk.Label(label="This image will be sent with your next message")
        self._image_preview_hint.add_css_class("dim-label")
        self._image_preview_hint.add_css_class("caption")
        self._image_preview_hint.set_halign(Gtk.Align.START)
        self._image_preview_hint.set_xalign(0)
        self._image_preview_hint.set_wrap(True)
        image_preview_text.append(self._image_preview_hint)

        remove_img_btn = Gtk.Button(icon_name="edit-clear-symbolic")
        remove_img_btn.add_css_class("flat")
        remove_img_btn.set_tooltip_text("Remove image")
        remove_img_btn.connect("clicked", self._on_remove_image)
        image_preview_footer.append(remove_img_btn)

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
        paste_controller.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
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

        # Insert messages with date separators and one default timestamp
        # at the end of each nearby time cluster.
        TIME_GAP_SECONDS = 900  # 15 minutes
        last_date_str = ""
        for i, msg in enumerate(sorted_msgs):
            date_str = msg.timestamp.strftime("%A, %B %d, %Y")
            if date_str != last_date_str:
                self._messages_box.append(DateSeparator(date_str))
                last_date_str = date_str

            next_msg = sorted_msgs[i + 1] if i + 1 < len(sorted_msgs) else None
            show_time = not self._messages_group_together(msg, next_msg, TIME_GAP_SECONDS)

            self._messages_box.append(
                MessageBubble(
                    msg,
                    show_time=show_time,
                    on_download_attachment=self._request_attachment_download,
                )
            )

        self._last_message = sorted_msgs[-1] if sorted_msgs else None
        self._stack.set_visible_child_name("thread")

        # Scroll to bottom after layout
        GLib.idle_add(self._scroll_to_bottom)

    def append_message(self, message):
        """Add a single new message and scroll down."""
        previous_bubble = self._last_message_bubble()
        if previous_bubble and self._messages_group_together(
            previous_bubble.message,
            message,
            900,
        ):
            previous_bubble.set_default_timestamp_visible(False)

        self._messages_box.append(
            MessageBubble(
                message,
                show_time=True,
                on_download_attachment=self._request_attachment_download,
            )
        )
        self._last_message = message
        GLib.idle_add(self._scroll_to_bottom)

    def _clear_messages(self):
        while True:
            child = self._messages_box.get_first_child()
            if child is None:
                break
            self._messages_box.remove(child)

    def _last_message_bubble(self) -> MessageBubble | None:
        child = self._messages_box.get_last_child()
        while child is not None and not isinstance(child, MessageBubble):
            child = child.get_prev_sibling()
        return child

    def _messages_group_together(self, current, next_msg, gap_seconds: int) -> bool:
        if next_msg is None:
            return False
        if current.timestamp.date() != next_msg.timestamp.date():
            return False
        return abs(next_msg.date - current.date) < gap_seconds * 1000

    def _scroll_to_bottom(self):
        adj = self._scroll.get_vadjustment()
        adj.set_value(adj.get_upper())
        return GLib.SOURCE_REMOVE

    def _on_send(self, _widget):
        text = self._text_entry.get_text().strip()
        if self._pending_image_path and self._thread_id:
            image_path = self._pending_image_path
            self.emit("send-message-with-attachment", self._thread_id, text, image_path)
            self._text_entry.set_text("")
            self._clear_pending_image(delete_file=False)
            self._retain_temp_path(image_path)
        elif text and self._thread_id:
            self.emit("send-message", self._thread_id, text)
            self._text_entry.set_text("")

    def _on_key_pressed(self, controller, keyval, keycode, state):
        """Handle Ctrl+V to paste images from clipboard."""
        is_ctrl_v = (state & Gdk.ModifierType.CONTROL_MASK) and keyval in (Gdk.KEY_v, Gdk.KEY_V)
        is_shift_insert = (state & Gdk.ModifierType.SHIFT_MASK) and keyval == Gdk.KEY_Insert
        if is_ctrl_v or is_shift_insert:
            display = Gdk.Display.get_default()
            if display is None:
                return False
            clipboard = display.get_clipboard()
            formats = clipboard.get_formats()
            has_texture = formats.contain_gtype(Gdk.Texture)
            has_image_mime = any(formats.contain_mime_type(mime) for mime in IMAGE_CLIPBOARD_MIME_TYPES)
            has_uri_list = formats.contain_mime_type("text/uri-list")
            if has_texture or has_image_mime:
                clipboard.read_texture_async(None, self._on_clipboard_texture)
                return True  # handled
            if has_uri_list:
                clipboard.read_text_async(None, self._on_clipboard_text)
                return True
        return False

    def _on_clipboard_text(self, clipboard, result):
        try:
            text = clipboard.read_text_finish(result)
        except GLib.Error:
            return
        if not text:
            return

        image_path = self._image_path_from_clipboard_text(text)
        if not image_path:
            return

        self._clear_pending_image()
        self._pending_image_path = image_path
        self._show_pending_image_preview(image_path)

    def _image_path_from_clipboard_text(self, text: str) -> str | None:
        for raw_line in text.splitlines():
            line = raw_line.strip()
            if not line:
                continue
            if line.startswith("file://"):
                path = unquote(urlparse(line).path)
            else:
                path = line
            if not os.path.isfile(path):
                continue
            if Path(path).suffix.lower() not in IMAGE_FILE_EXTS:
                continue
            return path
        return None

    def _on_clipboard_texture(self, clipboard, result):
        """Callback when clipboard texture is read."""
        try:
            texture = clipboard.read_texture_finish(result)
            if texture is None:
                return
            self._clear_pending_image()
            tmp_dir = os.path.join(tempfile.gettempdir(), "phonelink")
            os.makedirs(tmp_dir, exist_ok=True)
            fd, tmp_path = tempfile.mkstemp(
                prefix="paste_",
                suffix=".png",
                dir=tmp_dir,
            )
            os.close(fd)
            texture.save_to_png(tmp_path)
            self._pending_image_path = tmp_path
            self._show_pending_image_preview(tmp_path)
        except Exception as e:
            print(f"[phonelink] Clipboard paste error: {e}")

    def _on_remove_image(self, _btn):
        self._clear_pending_image()

    def _clear_pending_image(self, delete_file: bool = True):
        if delete_file and self._pending_image_path and os.path.exists(self._pending_image_path):
            try:
                os.unlink(self._pending_image_path)
            except OSError:
                pass
        self._pending_image_path = None
        self._image_preview_picture.set_filename(None)
        self._image_preview_bar.set_visible(False)

    def _show_pending_image_preview(self, image_path: str):
        self._image_preview_picture.set_filename(image_path)
        self._image_preview_label.set_label(os.path.basename(image_path))
        self._image_preview_bar.set_visible(True)

    def _retain_temp_path(self, path: str):
        if not path:
            return
        self._retained_temp_paths.add(path)

        def cleanup():
            self._retained_temp_paths.discard(path)
            if os.path.exists(path):
                try:
                    os.unlink(path)
                except OSError:
                    pass
            return GLib.SOURCE_REMOVE

        GLib.timeout_add_seconds(120, cleanup)

    def _request_attachment_download(self, part_id: int, unique_identifier: str, file_name: str):
        if not self._thread_id:
            return
        self.emit(
            "download-attachment",
            self._thread_id,
            int(part_id),
            unique_identifier,
            file_name,
        )
