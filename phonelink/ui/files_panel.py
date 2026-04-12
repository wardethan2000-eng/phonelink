"""Files panel — photo grid, file browser, send/receive files."""

import os
import shutil
import threading
import time
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GLib, Gio, GObject, Gdk, GdkPixbuf

from phonelink.dbus_client import IFACE_SFTP, IFACE_SHARE

IMAGE_EXTS = {".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".heic", ".heif"}
THUMB_SIZE = 160


# ── Helper: human-readable file size ───────────────────────────────

def _human_size(size: int) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if size < 1024:
            return f"{size:.0f} {unit}" if unit == "B" else f"{size:.1f} {unit}"
        size /= 1024
    return f"{size:.1f} TB"


def _file_icon_name(name: str, is_dir: bool) -> str:
    if is_dir:
        return "folder-symbolic"
    ext = Path(name).suffix.lower()
    video_exts = {".mp4", ".mkv", ".avi", ".mov", ".webm", ".3gp"}
    audio_exts = {".mp3", ".flac", ".ogg", ".wav", ".aac", ".m4a"}
    doc_exts = {".pdf", ".doc", ".docx", ".txt", ".odt", ".rtf"}
    if ext in IMAGE_EXTS:
        return "image-x-generic-symbolic"
    if ext in video_exts:
        return "video-x-generic-symbolic"
    if ext in audio_exts:
        return "audio-x-generic-symbolic"
    if ext in doc_exts:
        return "x-office-document-symbolic"
    if ext == ".apk":
        return "application-x-executable-symbolic"
    if ext in {".zip", ".tar", ".gz", ".7z", ".rar"}:
        return "package-x-generic-symbolic"
    return "text-x-generic-symbolic"


def _is_image(name: str) -> bool:
    return Path(name).suffix.lower() in IMAGE_EXTS


# ── Photo tile widget ──────────────────────────────────────────────


class PhotoTile(Gtk.Overlay):
    """A single photo thumbnail in the grid."""

    def __init__(self, full_path: str, file_name: str):
        super().__init__()
        self.full_path = full_path
        self.file_name = file_name
        self._selected = False

        self.set_size_request(THUMB_SIZE, THUMB_SIZE)

        # Container that holds either placeholder or image
        self._stack = Gtk.Stack()
        self._stack.set_size_request(THUMB_SIZE, THUMB_SIZE)
        self._stack.add_css_class("photo-tile")
        self.set_child(self._stack)

        # Placeholder (visible while loading)
        placeholder = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        placeholder.set_halign(Gtk.Align.CENTER)
        placeholder.set_valign(Gtk.Align.CENTER)
        icon = Gtk.Image.new_from_icon_name("image-x-generic-symbolic")
        icon.set_pixel_size(32)
        icon.set_opacity(0.4)
        placeholder.append(icon)
        self._stack.add_named(placeholder, "placeholder")

        # Image area (shown once thumbnail loads)
        self._picture = Gtk.Picture()
        self._picture.set_can_shrink(True)
        self._picture.set_content_fit(Gtk.ContentFit.COVER)
        self._picture.set_size_request(THUMB_SIZE, THUMB_SIZE)
        self._stack.add_named(self._picture, "image")

        self._stack.set_visible_child_name("placeholder")

        # Selection check overlay
        self._check = Gtk.Image.new_from_icon_name("object-select-symbolic")
        self._check.set_pixel_size(24)
        self._check.set_halign(Gtk.Align.END)
        self._check.set_valign(Gtk.Align.START)
        self._check.set_margin_top(6)
        self._check.set_margin_end(6)
        self._check.add_css_class("photo-check")
        self._check.set_visible(False)
        self.add_overlay(self._check)

        # File name overlay at bottom
        name_label = Gtk.Label(label=file_name)
        name_label.set_ellipsize(3)
        name_label.set_halign(Gtk.Align.FILL)
        name_label.set_valign(Gtk.Align.END)
        name_label.add_css_class("photo-label")
        self.add_overlay(name_label)

    def set_thumbnail(self, pixbuf):
        """Set the loaded thumbnail pixbuf (called from main thread)."""
        if pixbuf:
            texture = Gdk.Texture.new_for_pixbuf(pixbuf)
            self._picture.set_paintable(texture)
            self._stack.set_visible_child_name("image")

    def set_selected(self, selected: bool):
        self._selected = selected
        self._check.set_visible(selected)
        if selected:
            self.add_css_class("photo-tile-selected")
        else:
            self.remove_css_class("photo-tile-selected")

    @property
    def is_selected(self) -> bool:
        return self._selected


# ── File row widget (for file browser) ─────────────────────────────


class FileRow(Gtk.Box):
    """A single file/directory entry."""

    def __init__(self, name: str, full_path: str, is_dir: bool, size: int = 0):
        super().__init__(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        self.set_margin_top(4)
        self.set_margin_bottom(4)
        self.set_margin_start(8)
        self.set_margin_end(8)
        self.full_path = full_path
        self.is_dir = is_dir
        self.file_name = name

        icon = Gtk.Image.new_from_icon_name(_file_icon_name(name, is_dir))
        icon.set_pixel_size(24)
        self.append(icon)

        label = Gtk.Label(label=name)
        label.set_xalign(0)
        label.set_hexpand(True)
        label.set_ellipsize(3)
        self.append(label)

        if not is_dir and size > 0:
            size_label = Gtk.Label(label=_human_size(size))
            size_label.add_css_class("caption")
            size_label.add_css_class("dim-label")
            self.append(size_label)


# ── Main panel ─────────────────────────────────────────────────────


class FilesPanel(Gtk.Box):
    """File browser + photo grid panel."""

    def __init__(self, client):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.client = client
        self._device = None
        self._mount_point = ""
        self._storage_base = ""  # e.g. mount_point/storage/emulated/0
        self._current_path = ""
        self._is_mounted = False
        self._signal_ids: list[int] = []
        self._downloads_dir = str(Path.home() / "Downloads")
        self._photo_tiles: list[PhotoTile] = []
        self._thumb_threads: list[threading.Thread] = []
        self._loading_cancelled = False

        # ── Main stack: status vs content ──────────────────────────
        self._outer_stack = Gtk.Stack()
        self._outer_stack.set_vexpand(True)
        self._outer_stack.set_hexpand(True)
        self._outer_stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.append(self._outer_stack)

        # Status page (disconnected)
        self._status = Adw.StatusPage()
        self._status.set_icon_name("folder-symbolic")
        self._status.set_title("Files")
        self._status.set_description("No device connected.\nPair a phone to browse files.")
        self._outer_stack.add_named(self._status, "status")

        # Content area
        content = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        self._outer_stack.add_named(content, "content")

        # ── Left: sidebar ──────────────────────────────────────────
        sidebar = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        sidebar.set_size_request(200, -1)
        sidebar.set_margin_top(8)
        sidebar.set_margin_bottom(8)
        content.append(sidebar)

        sidebar_label = Gtk.Label(label="Locations")
        sidebar_label.add_css_class("heading")
        sidebar_label.set_xalign(0)
        sidebar_label.set_margin_start(12)
        sidebar_label.set_margin_top(4)
        sidebar_label.set_margin_bottom(4)
        sidebar.append(sidebar_label)

        self._shortcuts_list = Gtk.ListBox()
        self._shortcuts_list.set_selection_mode(Gtk.SelectionMode.SINGLE)
        self._shortcuts_list.add_css_class("navigation-sidebar")
        self._shortcuts_list.connect("row-selected", self._on_shortcut_selected)
        sidebar.append(self._shortcuts_list)

        sidebar.append(Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL))

        # Action buttons
        actions = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        actions.set_margin_top(8)
        actions.set_margin_start(12)
        actions.set_margin_end(12)
        sidebar.append(actions)

        send_btn = Gtk.Button(label="Send File to Phone")
        send_btn.set_icon_name("document-send-symbolic")
        send_btn.connect("clicked", self._on_send_file)
        actions.append(send_btn)

        open_dl_btn = Gtk.Button(label="Open Downloads")
        open_dl_btn.set_icon_name("folder-download-symbolic")
        open_dl_btn.connect("clicked", self._on_open_downloads)
        actions.append(open_dl_btn)

        self._mount_btn = Gtk.Button(label="Mount Phone")
        self._mount_btn.set_icon_name("media-mount-symbolic")
        self._mount_btn.connect("clicked", self._on_mount_toggle)
        actions.append(self._mount_btn)

        content.append(Gtk.Separator(orientation=Gtk.Orientation.VERTICAL))

        # ── Right: view stack (photos / file browser) ──────────────
        right_area = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        right_area.set_hexpand(True)
        content.append(right_area)

        # View switcher bar
        view_bar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        view_bar.set_margin_top(8)
        view_bar.set_margin_bottom(4)
        view_bar.set_margin_start(12)
        view_bar.set_margin_end(12)
        right_area.append(view_bar)

        self._view_stack = Adw.ViewStack()
        switcher = Adw.ViewSwitcher()
        switcher.set_stack(self._view_stack)
        switcher.set_policy(Adw.ViewSwitcherPolicy.WIDE)
        view_bar.append(switcher)

        spacer = Gtk.Box()
        spacer.set_hexpand(True)
        view_bar.append(spacer)

        # Photo action buttons (visible in photo view)
        self._photo_actions = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        view_bar.append(self._photo_actions)

        self._select_all_btn = Gtk.Button(icon_name="edit-select-all-symbolic")
        self._select_all_btn.add_css_class("flat")
        self._select_all_btn.set_tooltip_text("Select all")
        self._select_all_btn.connect("clicked", self._on_select_all)
        self._photo_actions.append(self._select_all_btn)

        self._save_btn = Gtk.Button(icon_name="document-save-symbolic")
        self._save_btn.add_css_class("flat")
        self._save_btn.set_tooltip_text("Save selected to folder")
        self._save_btn.set_sensitive(False)
        self._save_btn.connect("clicked", self._on_save_selected)
        self._photo_actions.append(self._save_btn)

        self._copy_btn = Gtk.Button(icon_name="edit-copy-symbolic")
        self._copy_btn.add_css_class("flat")
        self._copy_btn.set_tooltip_text("Copy selected to clipboard")
        self._copy_btn.set_sensitive(False)
        self._copy_btn.connect("clicked", self._on_copy_selected)
        self._photo_actions.append(self._copy_btn)

        right_area.append(Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL))

        self._view_stack.set_vexpand(True)
        right_area.append(self._view_stack)

        # ── Photo grid view ────────────────────────────────────────
        photo_page = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)

        photo_scroll = Gtk.ScrolledWindow()
        photo_scroll.set_vexpand(True)
        photo_scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        photo_page.append(photo_scroll)

        self._photo_grid = Gtk.FlowBox()
        self._photo_grid.set_valign(Gtk.Align.START)
        self._photo_grid.set_max_children_per_line(20)
        self._photo_grid.set_min_children_per_line(2)
        self._photo_grid.set_column_spacing(4)
        self._photo_grid.set_row_spacing(4)
        self._photo_grid.set_margin_start(8)
        self._photo_grid.set_margin_end(8)
        self._photo_grid.set_margin_top(8)
        self._photo_grid.set_margin_bottom(8)
        self._photo_grid.set_homogeneous(True)
        self._photo_grid.set_selection_mode(Gtk.SelectionMode.NONE)
        photo_scroll.set_child(self._photo_grid)

        # Photo empty placeholder
        self._photo_empty = Adw.StatusPage()
        self._photo_empty.set_icon_name("camera-photo-symbolic")
        self._photo_empty.set_title("No Photos")
        self._photo_empty.set_description(
            "Mount your phone to see recent photos.\n"
            "Click \"Mount Phone\" in the sidebar."
        )
        self._photo_empty.set_vexpand(True)

        self._photo_stack = Gtk.Stack()
        self._photo_stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self._photo_stack.add_named(self._photo_empty, "empty")
        self._photo_stack.add_named(photo_page, "grid")
        self._photo_stack.set_visible_child_name("empty")

        p = self._view_stack.add_titled(self._photo_stack, "photos", "Recent Photos")
        p.set_icon_name("camera-photo-symbolic")

        # Photo status bar
        self._photo_status = Gtk.Label()
        self._photo_status.set_xalign(0)
        self._photo_status.set_margin_start(12)
        self._photo_status.set_margin_end(12)
        self._photo_status.set_margin_top(4)
        self._photo_status.set_margin_bottom(4)
        self._photo_status.add_css_class("caption")
        self._photo_status.add_css_class("dim-label")
        photo_page.append(self._photo_status)

        # ── File browser view ──────────────────────────────────────
        file_page = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)

        # Breadcrumb
        path_bar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        path_bar.set_margin_top(8)
        path_bar.set_margin_bottom(4)
        path_bar.set_margin_start(12)
        path_bar.set_margin_end(12)
        file_page.append(path_bar)

        self._back_btn = Gtk.Button(icon_name="go-previous-symbolic")
        self._back_btn.add_css_class("flat")
        self._back_btn.set_tooltip_text("Go up")
        self._back_btn.connect("clicked", self._on_go_up)
        path_bar.append(self._back_btn)

        self._path_label = Gtk.Label(label="/")
        self._path_label.set_xalign(0)
        self._path_label.set_hexpand(True)
        self._path_label.set_ellipsize(3)
        self._path_label.add_css_class("heading")
        path_bar.append(self._path_label)

        refresh_btn = Gtk.Button(icon_name="view-refresh-symbolic")
        refresh_btn.add_css_class("flat")
        refresh_btn.set_tooltip_text("Refresh")
        refresh_btn.connect("clicked", self._on_refresh)
        path_bar.append(refresh_btn)

        file_page.append(Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL))

        scroll = Gtk.ScrolledWindow()
        scroll.set_vexpand(True)
        scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        file_page.append(scroll)

        self._file_list = Gtk.ListBox()
        self._file_list.set_selection_mode(Gtk.SelectionMode.SINGLE)
        self._file_list.connect("row-activated", self._on_file_activated)
        scroll.set_child(self._file_list)

        self._file_empty = Adw.StatusPage()
        self._file_empty.set_icon_name("folder-open-symbolic")
        self._file_empty.set_title("No Files")
        self._file_empty.set_description("Mount your phone to browse its files.")
        self._file_list.set_placeholder(self._file_empty)

        self._status_bar = Gtk.Label()
        self._status_bar.set_xalign(0)
        self._status_bar.set_margin_start(12)
        self._status_bar.set_margin_end(12)
        self._status_bar.set_margin_top(4)
        self._status_bar.set_margin_bottom(4)
        self._status_bar.add_css_class("caption")
        self._status_bar.add_css_class("dim-label")
        file_page.append(self._status_bar)

        p = self._view_stack.add_titled(file_page, "browser", "Browse Files")
        p.set_icon_name("folder-symbolic")

        # Track which view is active for action button visibility
        self._view_stack.connect("notify::visible-child-name", self._on_view_changed)
        self._on_view_changed(None, None)

    # ── View switching ─────────────────────────────────────────────

    def _on_view_changed(self, *_args):
        is_photos = (self._view_stack.get_visible_child_name() == "photos")
        self._photo_actions.set_visible(is_photos)

    # ── Device connection ──────────────────────────────────────────

    def set_device(self, device):
        old = self._device
        self._device = device

        if device and device.reachable:
            if not old or old.id != device.id or not old.reachable:
                self._subscribe_signals(device.id)
                self._auto_mount_and_load(device.id)
                self._build_shortcuts(device.id)
            self._outer_stack.set_visible_child_name("content")
        elif device:
            self._status.set_description(
                f"{device.name} is disconnected.\n"
                "Connect your phone to browse files."
            )
            self._outer_stack.set_visible_child_name("status")
        else:
            self._status.set_description(
                "No device connected.\nPair a phone to browse files."
            )
            self._outer_stack.set_visible_child_name("status")

    def _subscribe_signals(self, device_id: str):
        if self.client.bus:
            for sid in self._signal_ids:
                self.client.bus.signal_unsubscribe(sid)
        self._signal_ids.clear()

        sftp_path = f"/modules/kdeconnect/devices/{device_id}"
        for signal_name, handler in [
            ("mounted", self._on_sftp_mounted),
            ("unmounted", self._on_sftp_unmounted),
        ]:
            sid = self.client.subscribe_signal(
                sftp_path, IFACE_SFTP, signal_name, handler
            )
            if sid is not None:
                self._signal_ids.append(sid)

        sid = self.client.subscribe_signal(
            sftp_path, IFACE_SHARE, "shareReceived", self._on_file_received
        )
        if sid is not None:
            self._signal_ids.append(sid)

    def _on_sftp_mounted(self, conn, sender, path, iface, signal, params):
        GLib.idle_add(self._refresh_mount_state)

    def _on_sftp_unmounted(self, conn, sender, path, iface, signal, params):
        GLib.idle_add(self._refresh_mount_state)

    def _on_file_received(self, conn, sender, path, iface, signal, params):
        url = params.unpack()[0]
        file_name = os.path.basename(url)
        GLib.idle_add(self._status_bar.set_label, f"Received: {file_name}")

    # ── Mount management ───────────────────────────────────────────

    def _auto_mount_and_load(self, device_id: str):
        """Auto-mount the phone if needed and load photos."""
        self._is_mounted = self.client.sftp_is_mounted(device_id)
        self._mount_point = self.client.sftp_mount_point(device_id)

        if not self._is_mounted:
            self._photo_status.set_label("Connecting to phone…")
            self._status_bar.set_label("Mounting…")
            ok = self.client.sftp_mount_and_wait(device_id)
            if ok:
                self._is_mounted = True
                self._mount_point = self.client.sftp_mount_point(device_id)
            else:
                err = self.client.sftp_get_mount_error(device_id)
                self._photo_status.set_label(f"Mount failed: {err}")
                self._status_bar.set_label(f"Mount failed: {err}")

        self._resolve_storage_base(device_id)
        self._update_mount_ui()
        if self._is_mounted and self._mount_point:
            self._load_recent_photos()

    def _refresh_mount_state(self):
        if not self._device:
            return
        self._is_mounted = self.client.sftp_is_mounted(self._device.id)
        self._mount_point = self.client.sftp_mount_point(self._device.id)
        if not self._is_mounted:
            ok = self.client.sftp_mount_and_wait(self._device.id)
            if ok:
                self._is_mounted = True
                self._mount_point = self.client.sftp_mount_point(self._device.id)
        self._resolve_storage_base(self._device.id)
        self._update_mount_ui()
        if self._is_mounted and self._mount_point:
            self._load_recent_photos()

    def _resolve_storage_base(self, device_id: str):
        """Find the actual accessible base dir (e.g. .../storage/emulated/0)."""
        self._storage_base = ""
        if not self._mount_point:
            return
        dirs = self.client.sftp_get_directories(device_id)
        if dirs:
            # Use the first directory returned by the phone
            self._storage_base = next(iter(dirs))
        else:
            # Fallback: try the mount point directly
            self._storage_base = self._mount_point

    def _update_mount_ui(self):
        if self._is_mounted:
            self._mount_btn.set_label("Unmount Phone")
            self._mount_btn.set_icon_name("media-eject-symbolic")
            self._file_empty.set_title("Empty Folder")
            self._file_empty.set_description("This folder is empty.")
        else:
            self._mount_btn.set_label("Mount Phone")
            self._mount_btn.set_icon_name("media-mount-symbolic")
            self._file_empty.set_title("Phone Not Mounted")
            self._file_empty.set_description(
                "Click \"Mount Phone\" to browse files.\n\n"
                "If mounting fails, grant \"Files and media\" permission\n"
                "to KDE Connect in your phone's Settings → Apps → "
                "KDE Connect → Permissions."
            )
            self._photo_empty.set_description(
                "Mount your phone to see recent photos.\n"
                "Click \"Mount Phone\" in the sidebar."
            )
            self._photo_stack.set_visible_child_name("empty")
            self._clear_photo_grid()
            self._clear_file_list()

    def _on_mount_toggle(self, _btn):
        if not self._device:
            return
        if self._is_mounted:
            self.client.sftp_unmount(self._device.id)
            self._is_mounted = False
            self._current_path = ""
            self._update_mount_ui()
        else:
            self._status_bar.set_label("Mounting…")
            self._photo_status.set_label("Mounting…")
            ok = self.client.sftp_mount_and_wait(self._device.id)
            if ok:
                self._refresh_mount_state()
                self._status_bar.set_label("Mounted successfully")
            else:
                err = self.client.sftp_get_mount_error(self._device.id)
                self._status_bar.set_label(f"Mount failed: {err}")
                self._photo_status.set_label(f"Mount failed: {err}")
                self._file_empty.set_description(
                    f"Mount failed: {err}\n\n"
                    "Grant \"Files and media\" permission to KDE Connect\n"
                    "in your phone's Settings → Apps → KDE Connect → Permissions.\n\n"
                    "You can still send files using the \"Send File to Phone\" button."
                )
                self._photo_empty.set_description(
                    f"Mount failed: {err}\n\n"
                    "Grant \"Files and media\" permission to KDE Connect\n"
                    "on your phone to see photos."
                )

    # ── Photo grid ─────────────────────────────────────────────────

    def _clear_photo_grid(self):
        self._loading_cancelled = True
        self._photo_tiles.clear()
        while True:
            child = self._photo_grid.get_child_at_index(0)
            if child is None:
                break
            self._photo_grid.remove(child)

    def _load_recent_photos(self):
        """Scan phone camera/pictures dirs and show recent photos."""
        if not self._mount_point:
            return

        self._clear_photo_grid()
        self._loading_cancelled = False
        self._photo_status.set_label("Scanning for photos…")

        # Use the resolved storage base (e.g. .../storage/emulated/0)
        base = self._storage_base or self._mount_point
        search_dirs = [
            os.path.join(base, "DCIM", "Camera"),
            os.path.join(base, "DCIM"),
            os.path.join(base, "Pictures"),
            os.path.join(base, "Download"),
        ]

        # Collect image files with mtime
        image_files: list[tuple[str, float]] = []
        seen = set()
        for d in search_dirs:
            if not os.path.isdir(d):
                continue
            try:
                for name in os.listdir(d):
                    full = os.path.join(d, name)
                    if full in seen:
                        continue
                    seen.add(full)
                    if os.path.isfile(full) and _is_image(name):
                        try:
                            mtime = os.path.getmtime(full)
                            image_files.append((full, mtime))
                        except OSError:
                            pass
                # Also check one level of subdirectories in DCIM
                if "DCIM" in d:
                    for sub in os.listdir(d):
                        subdir = os.path.join(d, sub)
                        if not os.path.isdir(subdir):
                            continue
                        try:
                            for name in os.listdir(subdir):
                                full = os.path.join(subdir, name)
                                if full in seen:
                                    continue
                                seen.add(full)
                                if os.path.isfile(full) and _is_image(name):
                                    mtime = os.path.getmtime(full)
                                    image_files.append((full, mtime))
                        except OSError:
                            pass
            except OSError:
                continue

        # Sort newest first, limit to 200
        image_files.sort(key=lambda x: x[1], reverse=True)
        image_files = image_files[:200]

        if not image_files:
            self._photo_empty.set_description(
                "No photos found on phone.\n"
                "Check DCIM/Camera or Pictures folders."
            )
            self._photo_stack.set_visible_child_name("empty")
            self._photo_status.set_label("No photos found")
            return

        self._photo_stack.set_visible_child_name("grid")
        self._photo_status.set_label(f"Loading {len(image_files)} photos…")

        # Create tiles immediately (with placeholders)
        for full_path, mtime in image_files:
            name = os.path.basename(full_path)
            tile = PhotoTile(full_path, name)
            self._photo_tiles.append(tile)

            # Wrap in a FlowBoxChild — add click gesture
            fb_child = Gtk.FlowBoxChild()
            fb_child.set_child(tile)
            gesture = Gtk.GestureClick()
            gesture.connect("pressed", self._on_photo_clicked, tile)
            fb_child.add_controller(gesture)
            self._photo_grid.append(fb_child)

        # Load thumbnails in background
        self._load_thumbnails_async(image_files)

    def _load_thumbnails_async(self, image_files: list[tuple[str, float]]):
        """Load thumbnails in a background thread, posting to main thread."""
        total = len(image_files)

        def worker():
            for i, (full_path, _) in enumerate(image_files):
                if self._loading_cancelled:
                    return
                try:
                    pixbuf = GdkPixbuf.Pixbuf.new_from_file_at_scale(
                        full_path, THUMB_SIZE * 2, THUMB_SIZE * 2, True
                    )
                    if not self._loading_cancelled and i < len(self._photo_tiles):
                        tile = self._photo_tiles[i]
                        GLib.idle_add(tile.set_thumbnail, pixbuf)
                        if (i + 1) % 5 == 0 or i == total - 1:
                            GLib.idle_add(
                                self._photo_status.set_label,
                                f"Loading {i + 1}/{total} photos\u2026"
                            )
                except GLib.Error:
                    pass
            if not self._loading_cancelled:
                GLib.idle_add(
                    self._photo_status.set_label,
                    f"{total} photos"
                )

        t = threading.Thread(target=worker, daemon=True)
        self._thumb_threads.append(t)
        t.start()

    def _on_photo_clicked(self, gesture, n_press, x, y, tile: PhotoTile):
        if n_press == 1:
            # Toggle selection
            tile.set_selected(not tile.is_selected)
            self._update_selection_buttons()
        elif n_press == 2:
            # Double-click: open the photo
            try:
                Gio.AppInfo.launch_default_for_uri(
                    f"file://{tile.full_path}", None
                )
            except GLib.Error as e:
                self._photo_status.set_label(f"Cannot open: {e.message}")

    def _update_selection_buttons(self):
        count = sum(1 for t in self._photo_tiles if t.is_selected)
        has_sel = count > 0
        self._save_btn.set_sensitive(has_sel)
        self._copy_btn.set_sensitive(has_sel)
        if has_sel:
            self._photo_status.set_label(f"{count} photo{'s' if count != 1 else ''} selected")

    def _on_select_all(self, _btn):
        all_selected = all(t.is_selected for t in self._photo_tiles)
        for tile in self._photo_tiles:
            tile.set_selected(not all_selected)
        self._update_selection_buttons()

    # ── Save / Copy photos ─────────────────────────────────────────

    def _get_selected_paths(self) -> list[str]:
        return [t.full_path for t in self._photo_tiles if t.is_selected]

    def _on_save_selected(self, _btn):
        paths = self._get_selected_paths()
        if not paths:
            return

        dialog = Gtk.FileDialog()
        dialog.set_title("Save Photos To…")
        dialog.select_folder(
            self.get_root(),
            None,
            self._on_save_folder_chosen,
        )

    def _on_save_folder_chosen(self, dialog, result):
        try:
            folder = dialog.select_folder_finish(result)
        except GLib.Error:
            return
        if not folder:
            return

        dest_dir = folder.get_path()
        if not dest_dir:
            return

        paths = self._get_selected_paths()
        saved = 0
        for src in paths:
            name = os.path.basename(src)
            dest = os.path.join(dest_dir, name)
            # Avoid overwrites
            if os.path.exists(dest):
                base, ext = os.path.splitext(name)
                counter = 1
                while os.path.exists(dest):
                    dest = os.path.join(dest_dir, f"{base}_{counter}{ext}")
                    counter += 1
            try:
                shutil.copy2(src, dest)
                saved += 1
            except OSError as e:
                self._photo_status.set_label(f"Error saving {name}: {e}")
                return

        # Deselect after save
        for t in self._photo_tiles:
            t.set_selected(False)
        self._update_selection_buttons()
        self._photo_status.set_label(f"Saved {saved} photo{'s' if saved != 1 else ''} to {dest_dir}")

    def _on_copy_selected(self, _btn):
        """Copy selected photos to the clipboard."""
        paths = self._get_selected_paths()
        if not paths:
            return

        if len(paths) == 1:
            # Single photo: copy the image data to clipboard
            try:
                pixbuf = GdkPixbuf.Pixbuf.new_from_file(paths[0])
                texture = Gdk.Texture.new_for_pixbuf(pixbuf)
                clipboard = self.get_clipboard()
                clipboard.set_texture(texture)
                self._photo_status.set_label("Photo copied to clipboard")
            except GLib.Error as e:
                self._photo_status.set_label(f"Copy failed: {e.message}")
        else:
            # Multiple: copy file URIs
            uris = "\n".join(f"file://{p}" for p in paths)
            clipboard = self.get_clipboard()
            clipboard.set(uris)
            self._photo_status.set_label(f"Copied {len(paths)} file paths to clipboard")

        # Deselect
        for t in self._photo_tiles:
            t.set_selected(False)
        self._update_selection_buttons()

    # ── Shortcuts sidebar ──────────────────────────────────────────

    def _build_shortcuts(self, device_id: str):
        while True:
            row = self._shortcuts_list.get_row_at_index(0)
            if row is None:
                break
            self._shortcuts_list.remove(row)

        # "Recent Photos" shortcut
        row = Gtk.ListBoxRow()
        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        box.set_margin_start(8)
        box.set_margin_end(8)
        box.set_margin_top(4)
        box.set_margin_bottom(4)
        icon = Gtk.Image.new_from_icon_name("camera-photo-symbolic")
        icon.set_pixel_size(16)
        box.append(icon)
        name_label = Gtk.Label(label="Recent Photos")
        name_label.set_xalign(0)
        box.append(name_label)
        row.set_child(box)
        row._shortcut_path = "__photos__"
        self._shortcuts_list.append(row)

        # Phone directories
        dirs = self.client.sftp_get_directories(device_id)
        for path, label in dirs.items():
            row = Gtk.ListBoxRow()
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
            box.set_margin_start(8)
            box.set_margin_end(8)
            box.set_margin_top(4)
            box.set_margin_bottom(4)

            icon_name = "folder-symbolic"
            if "camera" in label.lower():
                icon_name = "camera-photo-symbolic"
            elif "all" in label.lower():
                icon_name = "phone-symbolic"

            icon = Gtk.Image.new_from_icon_name(icon_name)
            icon.set_pixel_size(16)
            box.append(icon)

            name_label = Gtk.Label(label=label)
            name_label.set_xalign(0)
            box.append(name_label)

            row.set_child(box)
            row._shortcut_path = path
            self._shortcuts_list.append(row)

        # Local Downloads
        row = Gtk.ListBoxRow()
        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        box.set_margin_start(8)
        box.set_margin_end(8)
        box.set_margin_top(4)
        box.set_margin_bottom(4)
        icon = Gtk.Image.new_from_icon_name("folder-download-symbolic")
        icon.set_pixel_size(16)
        box.append(icon)
        name_label = Gtk.Label(label="Downloads (local)")
        name_label.set_xalign(0)
        box.append(name_label)
        row.set_child(box)
        row._shortcut_path = self._downloads_dir
        self._shortcuts_list.append(row)

        # Select "Recent Photos" by default
        first = self._shortcuts_list.get_row_at_index(0)
        if first:
            self._shortcuts_list.select_row(first)

    def _on_shortcut_selected(self, _listbox, row):
        if row is None:
            return
        path = row._shortcut_path

        if path == "__photos__":
            self._view_stack.set_visible_child_name("photos")
            if self._is_mounted and not self._photo_tiles:
                self._load_recent_photos()
            return

        # Switch to file browser view
        self._view_stack.set_visible_child_name("browser")

        if path.startswith("/run/user/"):
            if not self._is_mounted:
                self._status_bar.set_label("Mount the phone first to browse this location")
                return

        self._navigate_to(path)

    # ── File browsing ──────────────────────────────────────────────

    def _navigate_to(self, path: str):
        self._current_path = path
        display_path = path
        if self._mount_point and path.startswith(self._mount_point):
            display_path = "Phone" + path[len(self._mount_point):]
            if display_path == "Phone":
                display_path = "Phone /"
        self._path_label.set_label(display_path)
        self._back_btn.set_sensitive(path != self._mount_point and path != self._downloads_dir)
        self._load_directory(path)

    def _load_directory(self, path: str):
        self._clear_file_list()

        if not os.path.isdir(path):
            self._status_bar.set_label(f"Cannot access: {path}")
            return

        try:
            entries = sorted(os.listdir(path))
        except PermissionError:
            self._status_bar.set_label("Permission denied")
            return
        except OSError as e:
            self._status_bar.set_label(f"Error: {e}")
            return

        dirs = []
        files = []
        for name in entries:
            if name.startswith("."):
                continue
            full = os.path.join(path, name)
            try:
                is_dir = os.path.isdir(full)
                size = os.path.getsize(full) if not is_dir else 0
            except OSError:
                continue
            if is_dir:
                dirs.append((name, full, True, 0))
            else:
                files.append((name, full, False, size))

        for name, full, is_dir, size in dirs + files:
            row_widget = FileRow(name, full, is_dir, size)
            row = Gtk.ListBoxRow()
            row.set_child(row_widget)
            self._file_list.append(row)

        total = len(dirs) + len(files)
        self._status_bar.set_label(
            f"{len(dirs)} folders, {len(files)} files" if total else "Empty folder"
        )

    def _clear_file_list(self):
        while True:
            row = self._file_list.get_row_at_index(0)
            if row is None:
                break
            self._file_list.remove(row)

    def _on_file_activated(self, _listbox, row):
        child = row.get_child()
        if not isinstance(child, FileRow):
            return
        if child.is_dir:
            self._navigate_to(child.full_path)
        else:
            try:
                Gio.AppInfo.launch_default_for_uri(
                    f"file://{child.full_path}", None
                )
            except GLib.Error as e:
                self._status_bar.set_label(f"Cannot open: {e.message}")

    def _on_go_up(self, _btn):
        if not self._current_path:
            return
        parent = os.path.dirname(self._current_path)
        if self._mount_point and self._current_path.startswith(self._mount_point):
            if len(parent) < len(self._mount_point):
                return
        self._navigate_to(parent)

    def _on_refresh(self, _btn):
        if self._view_stack.get_visible_child_name() == "photos":
            if self._is_mounted:
                self._load_recent_photos()
        elif self._current_path:
            self._load_directory(self._current_path)

    # ── Send file to phone ─────────────────────────────────────────

    def _on_send_file(self, _btn):
        if not self._device or not self._device.reachable:
            self._status_bar.set_label("Phone not connected")
            return

        dialog = Gtk.FileDialog()
        dialog.set_title("Send File to Phone")
        dialog.open_multiple(
            self.get_root(),
            None,
            self._on_files_chosen,
        )

    def _on_files_chosen(self, dialog, result):
        try:
            files = dialog.open_multiple_finish(result)
        except GLib.Error:
            return
        if not files or not self._device:
            return

        urls = []
        for f in files:
            path = f.get_path()
            if path:
                urls.append(f"file://{path}")

        if len(urls) == 1:
            self.client.share_file(self._device.id, urls[0].replace("file://", ""))
        elif urls:
            self.client.share_urls(self._device.id, urls)

        names = [os.path.basename(u) for u in urls]
        self._status_bar.set_label(f"Sending: {', '.join(names)}")

    def _on_open_downloads(self, _btn):
        self._navigate_to(self._downloads_dir)
        self._view_stack.set_visible_child_name("browser")
