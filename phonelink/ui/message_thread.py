"""Message thread view — chat-bubble display + text entry."""

import base64
import binascii
import hashlib
import html
import os
import re
import shutil
import tempfile
import webbrowser
from collections import OrderedDict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.parse import unquote, urlparse

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import Gtk, Gdk, GObject, Pango, GLib, Gio, GdkPixbuf

from phonelink.settings import (
    get_settings,
    MESSAGE_FONT_SCALE_MIN,
    MESSAGE_FONT_SCALE_MAX,
)
from phonelink.ui.icons import resolve_icon


ATTACHMENT_CACHE_DIR = Path(tempfile.gettempdir()) / "phonelink" / "message_attachments"

# Persistent cache for full-resolution attachment files the phone sends on
# request. Kept out of /tmp so upgraded images survive a restart.
FULL_ATTACHMENT_CACHE_DIR = Path(GLib.get_user_cache_dir()) / "phonelink" / "attachments_full"

# Full images render large and crisp within this box (scaled down to fit,
# never up). KDE Connect's inline previews are only ~100px, so until the full
# file arrives a thumbnail is shown as a placeholder, scaled up to a tidy floor
# (with a cap so it doesn't get too soft). Aspect ratio is always preserved.
FULL_IMAGE_MAX_W = 380
FULL_IMAGE_MAX_H = 500
THUMB_IMAGE_MAX_W = 300
THUMB_IMAGE_MAX_H = 360
THUMB_IMAGE_MIN_LONG = 220        # placeholder floor for the longer side
THUMB_IMAGE_UPSCALE_CAP = 2.2     # don't blow tiny thumbnails up past this
ATTACHMENT_THUMBNAIL_MAX = 256    # at/under this the image is a preview, not full
# Bubbles rendered per thread; older ones load on request.  Rendering a whole
# 2,000-message thread (and fetching every full-size image in it) froze the app.
THREAD_WINDOW = 150
# Of those, this many (about a screenful) are built before the chat shows; the
# rest are added above them in batches once it is on screen.
THREAD_FIRST_PAINT = 40
THREAD_FILL_BATCH = 30
TIME_GAP_SECONDS = 900  # messages closer than this share one timestamp


def full_attachment_cache_path(att: dict) -> Path | None:
    """Stable on-disk location for an attachment's full-resolution file."""
    uid = str(att.get("uniqueIdentifier") or att.get("partId") or "").strip()
    if not uid:
        return None
    ext = _attachment_ext(str(att.get("mimeType") or ""))
    return FULL_ATTACHMENT_CACHE_DIR / f"{_sanitize_name(uid)}{ext}"


def existing_full_attachment(att: dict) -> str | None:
    """Return the full-resolution file for an attachment if we already have it."""
    recorded = att.get("fullPath")
    if recorded and os.path.isfile(recorded):
        return recorded
    cached = full_attachment_cache_path(att)
    if cached and cached.is_file():
        return str(cached)
    return None


def _measure_image(path: str) -> tuple[int, int] | None:
    """Return (width, height) from the image header, or None if unreadable."""
    try:
        _fmt, width, height = GdkPixbuf.Pixbuf.get_file_info(path)
    except Exception:  # noqa: BLE001 — corrupt/unsupported header
        return None
    if not width or not height or width <= 0 or height <= 0:
        return None
    return width, height


# Inline images are decoded off the main thread, at their on-screen size, and
# kept for revisits.  Decoding full-resolution camera photos on the main thread
# (≈0.2 s each, ≈48 MB per 12 MP texture) made switching chats lag.
IMAGE_DECODE_SCALE = 2          # decode at 2× the display size so HiDPI stays crisp
IMAGE_CACHE_MAX = 64            # decoded textures kept (≈3 MB each at most)
_image_pool = ThreadPoolExecutor(max_workers=2, thread_name_prefix="phonelink-img")
_image_sizes: dict[tuple[str, float], tuple[int, int] | None] = {}
_image_textures: "OrderedDict[tuple[str, float], Gdk.Texture]" = OrderedDict()


def _image_key(path: str) -> tuple[str, float]:
    try:
        return path, os.path.getmtime(path)
    except OSError:
        return path, 0.0


def _image_display(width: int, height: int, can_fetch_full: bool):
    """(display_w, display_h, is_full, awaiting_full) for an image of this size."""
    # "Full" = an image already larger than a thumbnail (fetched full file, or a
    # high-res image we sent).  Thumbnails stay placeholders.
    is_full = max(width, height) > ATTACHMENT_THUMBNAIL_MAX
    disp_w, disp_h = _scaled_image_size(width, height, full=is_full)
    return disp_w, disp_h, is_full, (not is_full and can_fetch_full)


def _decode_image(path: str, can_fetch_full: bool):
    """Worker thread: measure and decode at display size.  Never touches GTK."""
    measured = _measure_image(path)
    if measured is None:
        return None, None
    disp_w, disp_h, _full, _awaiting = _image_display(*measured, can_fetch_full)
    try:
        pixbuf = GdkPixbuf.Pixbuf.new_from_file_at_scale(
            path, disp_w * IMAGE_DECODE_SCALE, disp_h * IMAGE_DECODE_SCALE, True
        )
        pixbuf = pixbuf.apply_embedded_orientation() or pixbuf
    except GLib.Error:
        pixbuf = None
    return measured, pixbuf


def _remember_texture(key, texture):
    _image_textures[key] = texture
    _image_textures.move_to_end(key)
    while len(_image_textures) > IMAGE_CACHE_MAX:
        _image_textures.popitem(last=False)


def _scaled_image_size(width: int, height: int, *, full: bool) -> tuple[int, int]:
    """Aspect-preserved on-screen size for an image of the given dimensions."""
    if full:
        max_w, max_h, min_long, cap = FULL_IMAGE_MAX_W, FULL_IMAGE_MAX_H, 0, 1.0
    else:
        max_w, max_h = THUMB_IMAGE_MAX_W, THUMB_IMAGE_MAX_H
        min_long, cap = THUMB_IMAGE_MIN_LONG, THUMB_IMAGE_UPSCALE_CAP
    fit = min(max_w / width, max_h / height)
    if fit < 1.0:
        scale = fit  # larger than the box → scale down to fit
    elif min_long:
        longer = max(width, height)
        scale = max(1.0, min(min_long / longer, cap, fit))
    else:
        scale = 1.0  # already-crisp full image smaller than the box → keep natural
    return max(1, round(width * scale)), max(1, round(height * scale))
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
URL_RE = re.compile(
    r"\b(?:https?://|www\.)[^\s<>()]+|"
    r"\b[a-zA-Z0-9][-a-zA-Z0-9]*\.(?:com|org|net|edu|gov|mil|int|info|biz|co|io|ly|me|tv|cc|us|ca|uk|de|fr|jp|app|dev|link|xyz)\b(?:/[^\s<>()]*)?",
    re.IGNORECASE
)
URL_TRAILING_PUNCTUATION = ".,!?;:)]}"


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


def _message_markup(text: str) -> tuple[str, bool]:
    """Return Pango markup with clickable URL anchors."""
    parts: list[str] = []
    last = 0
    has_links = False

    for match in URL_RE.finditer(text or ""):
        start, end = match.span()
        raw_url = match.group(0)
        while raw_url and raw_url[-1] in URL_TRAILING_PUNCTUATION:
            raw_url = raw_url[:-1]
            end -= 1
        if not raw_url:
            continue

        uri = raw_url if raw_url.lower().startswith(("http://", "https://")) else f"https://{raw_url}"
        parts.append(html.escape(text[last:start]))
        parts.append(
            f'<a href="{html.escape(uri, quote=True)}">'
            f"{html.escape(raw_url)}"
            "</a>"
        )
        last = end
        has_links = True

    if not has_links:
        return html.escape(text or ""), False

    parts.append(html.escape(text[last:]))
    return "".join(parts), True


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

        body_markup, has_links = _message_markup(message.body or "")
        if not has_links:
            click = Gtk.GestureClick(button=1)
            click.connect("pressed", self._on_bubble_pressed)
            frame.add_controller(click)

        # Message text
        body_label = Gtk.Label()
        body_label.set_wrap(True)
        body_label.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
        body_label.set_max_width_chars(50)
        body_label.set_halign(Gtk.Align.START)
        body_label.set_selectable(not has_links)
        body_label.add_css_class("message-zoomable")  # scales with pinch/ctrl-zoom
        if has_links:
            body_label.set_markup(body_markup)
            body_label.connect("activate-link", self._on_body_link_activated)
        else:
            body_label.set_text(message.body or "")
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

        part_id = int(attachment.get("partId") or 0)
        unique_identifier = str(attachment.get("uniqueIdentifier") or "")
        can_fetch_full = bool(part_id and self._on_download_attachment is not None)

        # Prefer the full-resolution file if we have it; the inline thumbnail is
        # only a placeholder shown until the full image arrives. Copy/Open/Save
        # below act on this too, so they never hand back the tiny thumbnail.
        full_path = existing_full_attachment(attachment) if mime.startswith("image") else None
        best_path = full_path or local_path

        if mime.startswith("image") and best_path:
            display_path = best_path
            key = _image_key(display_path)
            picture = Gtk.Picture()
            picture.set_can_shrink(True)
            picture.set_content_fit(Gtk.ContentFit.CONTAIN)
            # Pin to the start edge so it never stretches to fill a wide bubble.
            picture.set_halign(Gtk.Align.START)
            picture.set_hexpand(False)
            picture.add_css_class("message-attachment-image")
            picture.set_cursor(Gdk.Cursor.new_from_name("pointer", None))
            state = {"awaiting_full": can_fetch_full}

            def on_click(*_a, p=display_path):
                if state["awaiting_full"]:
                    # Manual retry if the automatic fetch hasn't landed yet.
                    self._on_download_attachment(part_id, unique_identifier, file_name)
                else:
                    self._on_open_attachment(None, p)

            click = Gtk.GestureClick(button=1)
            click.connect("released", on_click)
            picture.add_controller(click)

            def apply_size(measured):
                width, height = measured or (FULL_IMAGE_MAX_W, FULL_IMAGE_MAX_H)
                disp_w, disp_h, _full, awaiting = _image_display(
                    width, height, can_fetch_full
                )
                state["awaiting_full"] = measured is None and can_fetch_full or awaiting
                # Size the widget to the image's real aspect ratio.
                picture.set_size_request(disp_w, disp_h)
                if state["awaiting_full"]:
                    picture.add_css_class("message-attachment-loading")
                    picture.set_tooltip_text("Loading full image…")
                else:
                    picture.remove_css_class("message-attachment-loading")
                    picture.set_tooltip_text("Click to open image")

            if key in _image_sizes:
                apply_size(_image_sizes[key])
            else:
                # Placeholder box until the worker has measured the image.
                picture.set_size_request(THUMB_IMAGE_MAX_W, THUMB_IMAGE_MAX_H)
            texture = _image_textures.get(key)
            if texture is not None:
                _image_textures.move_to_end(key)
                picture.set_paintable(texture)
            else:
                def done(measured, pixbuf):
                    _image_sizes[key] = measured
                    apply_size(measured)
                    if pixbuf is not None:
                        tex = Gdk.Texture.new_for_pixbuf(pixbuf)
                        _remember_texture(key, tex)
                        picture.set_paintable(tex)
                    return GLib.SOURCE_REMOVE

                future = _image_pool.submit(_decode_image, display_path, can_fetch_full)
                future.add_done_callback(
                    lambda f: GLib.idle_add(done, *(f.result() if not f.exception() else (None, None)))
                )
            frame.append(picture)

        att_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        att_row.add_css_class("message-attachment-row")

        att_icon = Gtk.Image.new_from_icon_name(
            resolve_icon(
                "image-x-generic-symbolic" if mime.startswith("image") else "mail-attachment-symbolic"
            )
        )
        att_icon.set_pixel_size(14)
        att_row.append(att_icon)

        att_label = Gtk.Label(label=file_name)
        att_label.add_css_class("caption")
        att_label.set_hexpand(True)
        att_label.set_halign(Gtk.Align.START)
        att_label.set_ellipsize(Pango.EllipsizeMode.END)
        att_row.append(att_label)

        if best_path:
            if mime.startswith("image"):
                copy_btn = Gtk.Button(label="Copy")
                copy_btn.add_css_class("flat")
                copy_btn.connect("clicked", self._on_copy_image, best_path)
                att_row.append(copy_btn)

            open_btn = Gtk.Button(label="Open")
            open_btn.add_css_class("flat")
            open_btn.connect("clicked", self._on_open_attachment, best_path)
            att_row.append(open_btn)

            save_btn = Gtk.Button(label="Save")
            save_btn.add_css_class("flat")
            save_btn.connect("clicked", self._on_save_attachment, best_path, file_name)
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

    def _on_body_link_activated(self, _label, uri: str) -> bool:
        try:
            webbrowser.open(uri)
        except Exception:
            pass
        return True


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
        self._rendered_uids: set[int] = set()  # uids currently shown as bubbles
        self._last_rendered_date = 0  # date of the newest rendered message
        self._address = ""
        self._all_messages = []  # the whole thread, oldest first
        self._window_size = THREAD_WINDOW  # how many of the newest are rendered
        self._window_first_date = 0  # date of the oldest rendered message
        self._earlier_button = None
        self._fill_source = None   # idle source adding older bubbles
        self._fill_pending = []    # older window messages not built yet
        self._auto_fetched: set[str] = set()  # attachments we've asked full-res for
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
        # Images size themselves after they decode; stay at the newest message
        # while they do, unless the user has scrolled up.
        self._pinned_to_bottom = True
        vadj = self._scroll.get_vadjustment()
        vadj.connect("value-changed", self._on_scroll_value_changed)
        vadj.connect("changed", self._on_scroll_extent_changed)

        # ── Message-text zoom (pinch, Ctrl+scroll, Ctrl +/-/0) ────────
        self._settings = get_settings()
        self._font_scale = self._settings.message_font_scale
        self._zoom_base_scale = self._font_scale
        self._font_provider = Gtk.CssProvider()
        display = Gdk.Display.get_default()
        if display is not None:
            # Just above the app stylesheet so our dynamic font-size wins.
            Gtk.StyleContext.add_provider_for_display(
                display, self._font_provider,
                Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            )
        self._apply_font_scale()

        pinch = Gtk.GestureZoom()
        pinch.connect("begin", self._on_zoom_begin)
        pinch.connect("scale-changed", self._on_zoom_scale_changed)
        pinch.connect("end", self._on_zoom_end)
        self._scroll.add_controller(pinch)

        ctrl_scroll = Gtk.EventControllerScroll.new(
            Gtk.EventControllerScrollFlags.VERTICAL
        )
        ctrl_scroll.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        ctrl_scroll.connect("scroll", self._on_ctrl_scroll)
        self._scroll.add_controller(ctrl_scroll)

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
                     thread_id: int, *, keep_scroll: bool = False):
        if thread_id != self._thread_id:
            self._window_size = THREAD_WINDOW
        self._thread_id = thread_id
        self._contact_name = contact_name
        self._address = address
        self._contact_label.set_label(contact_name or address or "Unknown")
        self._address_label.set_label(address if contact_name else "")

        self._loading_spinner.stop()
        self._loading_spinner.set_visible(False)

        self._clear_messages()

        # Sort messages by date ascending; render only the newest window.
        self._all_messages = sorted(messages, key=lambda m: m.date)
        sorted_msgs = self._all_messages[-self._window_size:]
        self._earlier_button = None
        self._add_earlier_button(len(self._all_messages) - len(sorted_msgs))

        # Build the newest bubbles now; when the view sits at the bottom, the
        # older part of the window follows in idle batches above them.
        first = sorted_msgs if keep_scroll else sorted_msgs[-THREAD_FIRST_PAINT:]
        for widget in self._build_message_widgets(first, None):
            self._messages_box.append(widget)
        self._fill_pending = sorted_msgs[:len(sorted_msgs) - len(first)]
        if self._fill_pending:
            self._fill_source = GLib.idle_add(
                self._fill_older, priority=GLib.PRIORITY_LOW
            )

        self._last_message = sorted_msgs[-1] if sorted_msgs else None
        self._rendered_uids = {m.uid for m in first if m.uid}
        self._last_rendered_date = sorted_msgs[-1].date if sorted_msgs else 0
        self._window_first_date = sorted_msgs[0].date if sorted_msgs else 0
        self._stack.set_visible_child_name("thread")

        self._auto_fetch_full_images(first)

        # Scroll to bottom after layout
        if not keep_scroll:
            GLib.idle_add(self._scroll_to_bottom)

    def _build_message_widgets(self, msgs, next_after):
        """Date separators and bubbles for ``msgs`` (oldest first), in order.

        ``next_after`` is the message that follows the last one, if any, so the
        last bubble's timestamp groups correctly with it.
        """
        widgets = []
        last_date_str = ""
        for i, msg in enumerate(msgs):
            date_str = msg.timestamp.strftime("%A, %B %d, %Y")
            if date_str != last_date_str:
                widgets.append(DateSeparator(date_str))
                last_date_str = date_str
            next_msg = msgs[i + 1] if i + 1 < len(msgs) else next_after
            widgets.append(MessageBubble(
                msg,
                show_time=not self._messages_group_together(
                    msg, next_msg, TIME_GAP_SECONDS),
                on_download_attachment=self._request_attachment_download,
            ))
        return widgets

    def _fill_older(self):
        """Idle: add the next older batch of the window above what is shown."""
        if not self._fill_pending:
            self._fill_source = None
            return GLib.SOURCE_REMOVE
        batch = self._fill_pending[-THREAD_FILL_BATCH:]
        del self._fill_pending[-THREAD_FILL_BATCH:]
        anchor = self._earlier_button  # None → insert at the very top
        top = anchor.get_next_sibling() if anchor else self._messages_box.get_first_child()
        next_after = None
        node = top
        while node is not None and not isinstance(node, MessageBubble):
            node = node.get_next_sibling()
        if node is not None:
            next_after = node.message
        # The shown part starts with its own day header; drop it if the batch
        # ends on that same day so the header isn't repeated.
        if (isinstance(top, DateSeparator) and next_after is not None
                and batch[-1].timestamp.date() == next_after.timestamp.date()):
            self._messages_box.remove(top)
        sibling = anchor
        for widget in self._build_message_widgets(batch, next_after):
            self._messages_box.insert_child_after(widget, sibling)
            sibling = widget
        self._rendered_uids.update(m.uid for m in batch if m.uid)
        self._auto_fetch_full_images(batch)
        if self._fill_pending:
            return GLib.SOURCE_CONTINUE
        self._fill_source = None
        return GLib.SOURCE_REMOVE

    def _add_earlier_button(self, hidden: int):
        if hidden <= 0:
            return
        btn = Gtk.Button(label=self._earlier_label(hidden))
        btn.add_css_class("flat")
        btn.set_halign(Gtk.Align.CENTER)
        btn.set_margin_top(8)
        btn.connect("clicked", self._on_show_earlier)
        self._messages_box.append(btn)
        self._earlier_button = btn

    @staticmethod
    def _earlier_label(hidden: int) -> str:
        return f"Show earlier messages ({hidden})"

    def _on_show_earlier(self, _btn):
        adj = self._scroll.get_vadjustment()
        from_bottom = adj.get_upper() - adj.get_value()
        self._window_size += THREAD_WINDOW
        self.set_messages(self._all_messages, self._contact_name, self._address,
                          self._thread_id, keep_scroll=True)

        def restore():
            adj.set_value(max(0, adj.get_upper() - from_bottom))
            return GLib.SOURCE_REMOVE
        GLib.idle_add(restore)

    def sync(self, messages, contact_name: str, address: str, thread_id: int):
        """Reflect the latest messages with minimal work.

        Appends only genuinely-new (newest) messages instead of rebuilding the
        whole thread.  Falls back to a full rebuild when the thread changes, a
        message was removed, or older messages arrived out of order (history
        backfill).  The header is always refreshed so contact-name changes show.
        """
        self._contact_label.set_label(contact_name or address or "Unknown")
        self._address_label.set_label(address if contact_name else "")

        if (thread_id != self._thread_id
                or self._stack.get_visible_child_name() != "thread"):
            self.set_messages(messages, contact_name, address, thread_id)
            return

        current_uids = {m.uid for m in messages if m.uid}
        # Messages older than the rendered window (history backfill) only
        # change the "Show earlier" count; they never force a rebuild.
        pending = {m.uid for m in getattr(self, "_fill_pending", ()) if m.uid}
        new_msgs = [m for m in messages
                    if m.uid and m.uid not in self._rendered_uids
                    and m.uid not in pending
                    and m.date >= self._window_first_date]
        removed = self._rendered_uids - current_uids
        out_of_order = any(m.date < self._last_rendered_date for m in new_msgs)

        if removed or out_of_order:
            self.set_messages(messages, contact_name, address, thread_id,
                              keep_scroll=True)
            return

        self._all_messages = sorted(messages, key=lambda m: m.date)
        hidden = sum(1 for m in self._all_messages if m.date < self._window_first_date)
        if self._earlier_button is not None:
            self._earlier_button.set_label(self._earlier_label(hidden))
        elif hidden:
            # Can only appear at the top of the box; a rebuild places it there.
            self.set_messages(messages, contact_name, address, thread_id,
                              keep_scroll=True)
            return

        for msg in sorted(new_msgs, key=lambda m: m.date):
            self.append_message(msg)

    def append_message(self, message):
        """Add a single new message and scroll down."""
        previous_bubble = self._last_message_bubble()
        if previous_bubble and self._messages_group_together(
            previous_bubble.message,
            message,
            900,
        ):
            previous_bubble.set_default_timestamp_visible(False)

        # Insert a date header when the day changes (or for the first bubble).
        if (self._last_message is None
                or self._last_message.timestamp.date() != message.timestamp.date()):
            self._messages_box.append(
                DateSeparator(message.timestamp.strftime("%A, %B %d, %Y"))
            )

        self._messages_box.append(
            MessageBubble(
                message,
                show_time=True,
                on_download_attachment=self._request_attachment_download,
            )
        )
        self._last_message = message
        if message.uid:
            self._rendered_uids.add(message.uid)
        if message.date > self._last_rendered_date:
            self._last_rendered_date = message.date
        self._auto_fetch_full_images([message])
        GLib.idle_add(self._scroll_to_bottom)

    def _auto_fetch_full_images(self, messages):
        """Ask the phone for the full-resolution file of any inline image we
        only have a thumbnail for, so the chat shows the real image, not a
        blurry preview. Each attachment is requested at most once per session."""
        if not self._thread_id:
            return
        for msg in messages:
            for att in (msg.attachments or []):
                if not str(att.get("mimeType") or "").startswith("image"):
                    continue
                part_id = int(att.get("partId") or 0)
                if not part_id or existing_full_attachment(att):
                    continue
                unique_identifier = str(att.get("uniqueIdentifier") or "")
                key = unique_identifier or str(part_id)
                if key in self._auto_fetched:
                    continue
                self._auto_fetched.add(key)
                self._request_attachment_download(
                    part_id, unique_identifier, _attachment_name(att)
                )

    def _clear_messages(self):
        self._earlier_button = None
        if getattr(self, "_fill_source", None):
            GLib.source_remove(self._fill_source)
        self._fill_source = None
        self._fill_pending = []
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
        self._pinned_to_bottom = True
        adj.set_value(adj.get_upper())
        return GLib.SOURCE_REMOVE

    def _on_scroll_value_changed(self, adj):
        self._pinned_to_bottom = (
            adj.get_value() + adj.get_page_size() >= adj.get_upper() - 48
        )

    def _on_scroll_extent_changed(self, adj):
        if self._pinned_to_bottom:
            adj.set_value(adj.get_upper() - adj.get_page_size())

    # ── Message-text zoom ──────────────────────────────────────────

    def _apply_font_scale(self):
        pct = int(round(self._font_scale * 100))
        css = f".message-zoomable {{ font-size: {pct}%; }}"
        if hasattr(self._font_provider, "load_from_string"):
            self._font_provider.load_from_string(css)  # GTK 4.12+
        else:
            self._font_provider.load_from_data(css.encode("utf-8"))

    def _set_font_scale(self, scale: float, *, persist: bool):
        scale = min(MESSAGE_FONT_SCALE_MAX, max(MESSAGE_FONT_SCALE_MIN, scale))
        changed = abs(scale - self._font_scale) >= 0.001
        if changed:
            self._font_scale = scale
            self._apply_font_scale()
        if persist and (changed or self._settings.message_font_scale != scale):
            self._settings.message_font_scale = scale

    def zoom_in(self):
        self._set_font_scale(self._font_scale + 0.1, persist=True)

    def zoom_out(self):
        self._set_font_scale(self._font_scale - 0.1, persist=True)

    def zoom_reset(self):
        self._set_font_scale(1.0, persist=True)

    def _on_zoom_begin(self, _gesture, _sequence):
        self._zoom_base_scale = self._font_scale

    def _on_zoom_scale_changed(self, _gesture, scale):
        self._set_font_scale(self._zoom_base_scale * scale, persist=False)

    def _on_zoom_end(self, _gesture, _sequence):
        self._settings.message_font_scale = self._font_scale

    def _on_ctrl_scroll(self, controller, _dx, dy):
        if not (controller.get_current_event_state() & Gdk.ModifierType.CONTROL_MASK):
            return False  # normal scroll — let the list scroll
        if dy == 0:
            return False
        self._set_font_scale(self._font_scale + (0.1 if dy < 0 else -0.1), persist=True)
        return True

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
