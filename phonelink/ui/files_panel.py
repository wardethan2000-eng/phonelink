"""Files panel — browse phone storage, send/receive files."""

import os
import time
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GLib, Gio, GObject

from phonelink.dbus_client import IFACE_SFTP, IFACE_SHARE


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
    image_exts = {".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".svg"}
    video_exts = {".mp4", ".mkv", ".avi", ".mov", ".webm", ".3gp"}
    audio_exts = {".mp3", ".flac", ".ogg", ".wav", ".aac", ".m4a"}
    doc_exts = {".pdf", ".doc", ".docx", ".txt", ".odt", ".rtf"}
    if ext in image_exts:
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


# ── File row widget ────────────────────────────────────────────────


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
    """File browser + send/receive panel."""

    def __init__(self, client):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.client = client
        self._device = None
        self._mount_point = ""
        self._current_path = ""
        self._is_mounted = False
        self._signal_ids: list[int] = []
        self._downloads_dir = str(Path.home() / "Downloads")

        # ── Main stack: status vs content ──────────────────────────
        self._stack = Gtk.Stack()
        self._stack.set_vexpand(True)
        self._stack.set_hexpand(True)
        self._stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.append(self._stack)

        # Status page
        self._status = Adw.StatusPage()
        self._status.set_icon_name("folder-symbolic")
        self._status.set_title("Files")
        self._status.set_description("No device connected.\nPair a phone to browse files.")
        self._stack.add_named(self._status, "status")

        # Content area
        content = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        self._stack.add_named(content, "content")

        # ── Left: shortcuts sidebar ────────────────────────────────
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

        mount_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        actions.append(mount_box)

        self._mount_btn = Gtk.Button(label="Mount Phone")
        self._mount_btn.set_icon_name("media-mount-symbolic")
        self._mount_btn.set_hexpand(True)
        self._mount_btn.connect("clicked", self._on_mount_toggle)
        mount_box.append(self._mount_btn)

        content.append(Gtk.Separator(orientation=Gtk.Orientation.VERTICAL))

        # ── Right: file list ───────────────────────────────────────
        file_area = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        file_area.set_hexpand(True)
        content.append(file_area)

        # Breadcrumb / path bar
        path_bar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        path_bar.set_margin_top(8)
        path_bar.set_margin_bottom(4)
        path_bar.set_margin_start(12)
        path_bar.set_margin_end(12)
        file_area.append(path_bar)

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

        file_area.append(Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL))

        # File list
        scroll = Gtk.ScrolledWindow()
        scroll.set_vexpand(True)
        scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        file_area.append(scroll)

        self._file_list = Gtk.ListBox()
        self._file_list.set_selection_mode(Gtk.SelectionMode.SINGLE)
        self._file_list.connect("row-activated", self._on_file_activated)
        scroll.set_child(self._file_list)

        # Empty state
        self._file_empty = Adw.StatusPage()
        self._file_empty.set_icon_name("folder-open-symbolic")
        self._file_empty.set_title("No Files")
        self._file_empty.set_description("Mount your phone to browse its files.")
        self._file_list.set_placeholder(self._file_empty)

        # Status bar
        self._status_bar = Gtk.Label()
        self._status_bar.set_xalign(0)
        self._status_bar.set_margin_start(12)
        self._status_bar.set_margin_end(12)
        self._status_bar.set_margin_top(4)
        self._status_bar.set_margin_bottom(4)
        self._status_bar.add_css_class("caption")
        self._status_bar.add_css_class("dim-label")
        file_area.append(self._status_bar)

    # ── Device connection ──────────────────────────────────────────

    def set_device(self, device):
        old = self._device
        self._device = device

        if device and device.reachable:
            if not old or old.id != device.id or not old.reachable:
                self._subscribe_signals(device.id)
                self._check_mount(device.id)
                self._build_shortcuts(device.id)
            self._stack.set_visible_child_name("content")
        elif device:
            self._status.set_description(
                f"{device.name} is disconnected.\n"
                "Connect your phone to browse files."
            )
            self._stack.set_visible_child_name("status")
        else:
            self._status.set_description(
                "No device connected.\nPair a phone to browse files."
            )
            self._stack.set_visible_child_name("status")

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

        # Listen for received files
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
        GLib.idle_add(
            self._status_bar.set_label,
            f"Received: {file_name}"
        )

    # ── Mount management ───────────────────────────────────────────

    def _check_mount(self, device_id: str):
        self._mount_point = self.client.sftp_mount_point(device_id)
        self._is_mounted = self.client.sftp_is_mounted(device_id)
        self._update_mount_ui()
        if self._is_mounted and self._mount_point:
            self._navigate_to(self._mount_point)

    def _refresh_mount_state(self):
        if not self._device:
            return
        self._is_mounted = self.client.sftp_is_mounted(self._device.id)
        self._mount_point = self.client.sftp_mount_point(self._device.id)
        self._update_mount_ui()
        if self._is_mounted and self._mount_point:
            if not self._current_path:
                self._navigate_to(self._mount_point)

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
            ok = self.client.sftp_mount_and_wait(self._device.id)
            if ok:
                self._refresh_mount_state()
                self._status_bar.set_label("Mounted successfully")
            else:
                err = self.client.sftp_get_mount_error(self._device.id)
                self._status_bar.set_label(f"Mount failed: {err}")
                self._file_empty.set_description(
                    f"Mount failed: {err}\n\n"
                    "Grant \"Files and media\" permission to KDE Connect\n"
                    "in your phone's Settings → Apps → KDE Connect → Permissions.\n\n"
                    "You can still send files using the \"Send File to Phone\" button."
                )

    # ── Shortcuts sidebar ──────────────────────────────────────────

    def _build_shortcuts(self, device_id: str):
        # Clear existing
        while True:
            row = self._shortcuts_list.get_row_at_index(0)
            if row is None:
                break
            self._shortcuts_list.remove(row)

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

    def _on_shortcut_selected(self, _listbox, row):
        if row is None:
            return
        path = row._shortcut_path

        # If it's a phone path, require mount
        if path.startswith("/run/user/"):
            if not self._is_mounted:
                self._status_bar.set_label("Mount the phone first to browse this location")
                return

        self._navigate_to(path)

    # ── File browsing ──────────────────────────────────────────────

    def _navigate_to(self, path: str):
        self._current_path = path
        # Show user-friendly path
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

        # Directories first, then files
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
            # Open the file with the default application
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
        # Don't navigate above mount point or downloads
        if self._mount_point and self._current_path.startswith(self._mount_point):
            if len(parent) < len(self._mount_point):
                return
        self._navigate_to(parent)

    def _on_refresh(self, _btn):
        if self._current_path:
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
            return  # User cancelled
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

    # ── Open local Downloads folder ────────────────────────────────

    def _on_open_downloads(self, _btn):
        self._navigate_to(self._downloads_dir)
