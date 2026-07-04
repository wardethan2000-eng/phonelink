"""Fabric panel — browse the Loom device fabric and pull files that live on other devices.

This is phonelink's window into Loom (the accountless personal/family device fabric): it lists files
in the fabric's catalog — including files whose bytes live *only on another device* — and lets you
**Open** one, which pulls it here (verified against its hash) and caches it, so the file follows you.

It talks to a locally-running ``loomd`` through the optional ``loom_sdk`` (see
:mod:`phonelink.loom_bridge`). With Loom not installed or ``loomd`` not running, the panel shows a
friendly setup hint instead of erroring — phonelink stays fully usable without the fabric.

Blocking SDK calls run off the GTK main thread via the shared ``client.bridge`` (AsyncBridge).
"""

import os

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GLib, Gio

from phonelink import loom_bridge


def _human_size(size: int) -> str:
    value = float(size)
    for unit in ("B", "KB", "MB", "GB"):
        if value < 1024:
            return f"{value:.0f} {unit}" if unit == "B" else f"{value:.1f} {unit}"
        value /= 1024
    return f"{value:.1f} TB"


def _downloads_dir() -> str:
    path = GLib.get_user_special_dir(GLib.UserDirectory.DIRECTORY_DOWNLOAD)
    if not path:
        path = os.path.join(os.path.expanduser("~"), "Downloads")
    os.makedirs(path, exist_ok=True)
    return path


class FabricPanel(Gtk.Box):
    """Browse the Loom fabric and pull files from other devices."""

    def __init__(self, client):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.client = client
        self._device_label = "this device"
        self._loaded_once = False

        self._toast_overlay = Adw.ToastOverlay()
        self._toast_overlay.set_vexpand(True)
        self._toast_overlay.set_hexpand(True)
        self.append(self._toast_overlay)

        # Stack: "unavailable" (SDK missing / loomd down), "empty" (no files), "content" (the list).
        self._stack = Gtk.Stack()
        self._stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self._toast_overlay.set_child(self._stack)

        self._build_unavailable_page()
        self._build_empty_page()
        self._build_content()

        # Refresh whenever the tab becomes visible (cheap, keeps holders/peers current).
        self.connect("map", lambda *_: self.refresh())

    # ── Pages ────────────────────────────────────────────────────────────────────────────────────

    def _build_unavailable_page(self):
        self._unavailable = Adw.StatusPage()
        self._unavailable.set_icon_name("network-workgroup-symbolic")
        self._unavailable.set_title("The Fabric isn't connected")

        retry = Gtk.Button(label="Try again")
        retry.add_css_class("pill")
        retry.add_css_class("suggested-action")
        retry.set_halign(Gtk.Align.CENTER)
        retry.connect("clicked", lambda *_: self.refresh())
        self._unavailable.set_child(retry)

        self._stack.add_named(self._unavailable, "unavailable")

    def _build_empty_page(self):
        self._empty = Adw.StatusPage()
        self._empty.set_icon_name("folder-remote-symbolic")
        self._empty.set_title("No files in the fabric yet")
        self._empty.set_description(
            "Add another device as a peer and sync to browse its files here.\n"
            "On a terminal:  loom peers add THEIR-ADDRESS   then   loom sync"
        )
        sync_btn = Gtk.Button(label="Sync now")
        sync_btn.add_css_class("pill")
        sync_btn.set_halign(Gtk.Align.CENTER)
        sync_btn.connect("clicked", lambda *_: self._do_sync())
        self._empty.set_child(sync_btn)
        self._stack.add_named(self._empty, "empty")

    def _build_content(self):
        content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)

        # Toolbar: fabric status on the left, Sync + Refresh on the right.
        toolbar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        toolbar.set_margin_start(12)
        toolbar.set_margin_end(12)
        toolbar.set_margin_top(8)
        toolbar.set_margin_bottom(8)

        status_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        status_box.set_hexpand(True)
        self._title_label = Gtk.Label(label="Fabric", xalign=0)
        self._title_label.add_css_class("heading")
        self._subtitle_label = Gtk.Label(label="", xalign=0)
        self._subtitle_label.add_css_class("caption")
        self._subtitle_label.add_css_class("dim-label")
        status_box.append(self._title_label)
        status_box.append(self._subtitle_label)
        toolbar.append(status_box)

        invite_btn = Gtk.Button(icon_name="contact-new-symbolic")
        invite_btn.add_css_class("flat")
        invite_btn.set_tooltip_text("Invite a device to your realm (shows a QR to scan)")
        invite_btn.connect("clicked", lambda *_: self._do_invite())
        toolbar.append(invite_btn)

        sync_btn = Gtk.Button(icon_name="emblem-synchronizing-symbolic")
        sync_btn.add_css_class("flat")
        sync_btn.set_tooltip_text("Sync catalogs with your peers")
        sync_btn.connect("clicked", lambda *_: self._do_sync())
        toolbar.append(sync_btn)

        refresh_btn = Gtk.Button(icon_name="view-refresh-symbolic")
        refresh_btn.add_css_class("flat")
        refresh_btn.set_tooltip_text("Refresh")
        refresh_btn.connect("clicked", lambda *_: self.refresh())
        toolbar.append(refresh_btn)

        content.append(toolbar)
        content.append(Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL))

        scroller = Gtk.ScrolledWindow()
        scroller.set_vexpand(True)
        scroller.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)

        clamp = Adw.Clamp()
        clamp.set_margin_top(12)
        clamp.set_margin_bottom(12)
        clamp.set_margin_start(12)
        clamp.set_margin_end(12)

        self._list = Gtk.ListBox()
        self._list.set_selection_mode(Gtk.SelectionMode.NONE)
        self._list.add_css_class("boxed-list")
        clamp.set_child(self._list)
        scroller.set_child(clamp)
        content.append(scroller)

        self._stack.add_named(content, "content")

    # ── Data flow ────────────────────────────────────────────────────────────────────────────────

    def set_device(self, device):
        """The fabric is independent of the KDE Connect device; ignore device changes."""
        # Present for API symmetry with the other panels (see main_window refresh loops).

    def refresh(self):
        """Reload status + catalog from loomd, off the GTK thread."""
        if not loom_bridge.sdk_available():
            self._unavailable.set_description(
                "Loom isn't installed on this device.\n"
                "Install the Loom SDK (loom repo → sdk/python) to browse your fabric here."
            )
            self._stack.set_visible_child_name("unavailable")
            return

        self.client.bridge.submit(self._fetch_state, on_result=self._apply_state, on_error=self._on_error)

    @staticmethod
    def _fetch_state():
        """Worker thread: one round-trip for status, one for the catalog."""
        loom = loom_bridge.connect()
        status = loom.status()
        entries = loom.browse()
        return status, entries

    def _apply_state(self, state):
        status, entries = state
        self._loaded_once = True
        self._device_label = status.get("device_id", "this device")
        peers = len(status.get("peers", []))
        self._subtitle_label.set_text(
            f"device {self._device_label} · {peers} peer(s) · {len(entries)} file(s)"
        )

        self._clear_list()
        if not entries:
            self._stack.set_visible_child_name("empty")
            return

        for entry in entries:
            self._list.append(self._make_row(entry))
        self._stack.set_visible_child_name("content")

    def _make_row(self, entry) -> Gtk.Widget:
        row = Adw.ActionRow()
        row.set_title(GLib.markup_escape_text(entry.path))
        holders = ", ".join(entry.holders) if entry.holders else "no known holder"
        row.set_subtitle(f"{_human_size(entry.size)} · on {holders}")

        open_btn = Gtk.Button(icon_name="folder-download-symbolic")
        open_btn.add_css_class("flat")
        open_btn.set_valign(Gtk.Align.CENTER)
        open_btn.set_tooltip_text("Pull this file to this device")
        open_btn.connect("clicked", lambda *_btn, e=entry: self._open_entry(e))
        row.add_suffix(open_btn)
        row.set_activatable_widget(open_btn)
        return row

    def _open_entry(self, entry):
        self._toast(f"Opening {entry.path}…")

        def work():
            loom = loom_bridge.connect()
            data = loom.open(entry.path)
            out = os.path.join(_downloads_dir(), os.path.basename(entry.path) or "loom-file")
            with open(out, "wb") as fh:
                fh.write(data)
            return out, len(data), entry.holders

        self.client.bridge.submit(work, on_result=self._on_opened, on_error=self._on_error)

    def _on_opened(self, result):
        out, size, holders = result
        src = f" from {holders[0]}" if holders else ""
        toast = Adw.Toast.new(f"Pulled {_human_size(size)}{src} → {os.path.basename(out)}")
        toast.set_button_label("Show")
        toast.connect("button-clicked", lambda *_: self._reveal(out))
        toast.set_timeout(5)
        self._toast_overlay.add_toast(toast)
        # Refresh so this device now shows as a holder.
        self.refresh()

    def _do_sync(self):
        self._toast("Syncing with peers…")

        def work():
            return loom_bridge.connect().sync()

        def done(synced):
            self._toast(f"Synced with {len(synced)} peer(s)")
            self.refresh()

        self.client.bridge.submit(work, on_result=done, on_error=self._on_error)

    # ── Invite a device (network enrollment) ───────────────────────────────────────────────────────

    def _do_invite(self):
        self._toast("Creating invite…")
        self.client.bridge.submit(
            self._make_invite, on_result=self._show_invite_dialog, on_error=self._on_error
        )

    @staticmethod
    def _make_invite():
        """Worker: mint an invite and render it as a scannable QR PNG."""
        result = loom_bridge.connect().enroll_invite(900)  # 15 minutes
        code = result.get("invite", "")
        realm = result.get("realm_id", "?")
        png = None
        try:
            import qrcode

            path = os.path.join(GLib.get_user_cache_dir(), "loom_invite.png")
            qrcode.make(code).save(path)
            png = path
        except Exception:  # noqa: BLE001 — no qrcode lib → fall back to the code text only
            png = None
        return code, realm, png

    def _show_invite_dialog(self, result):
        code, realm, png = result
        win = Adw.Window(modal=True, default_width=380, default_height=560)
        win.set_transient_for(self.get_root())
        win.set_title("Invite a device")

        view = Adw.ToolbarView()
        view.add_top_bar(Adw.HeaderBar())

        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=14)
        for m in ("set_margin_top", "set_margin_bottom", "set_margin_start", "set_margin_end"):
            getattr(box, m)(20)

        heading = Gtk.Label(label="Scan on the new device")
        heading.add_css_class("title-3")
        box.append(heading)

        steps = Gtk.Label(label="Open Loom → Join your realm → Scan invite QR")
        steps.add_css_class("dim-label")
        steps.set_wrap(True)
        steps.set_justify(Gtk.Justification.CENTER)
        box.append(steps)

        if png:
            picture = Gtk.Picture.new_for_filename(png)
            picture.set_size_request(280, 280)
            picture.set_halign(Gtk.Align.CENTER)
            box.append(picture)
        else:
            box.append(Gtk.Label(label="(install python3-qrcode to show a QR)"))

        or_lbl = Gtk.Label(label="…or paste this code into the app:")
        or_lbl.add_css_class("caption")
        or_lbl.add_css_class("dim-label")
        box.append(or_lbl)

        code_lbl = Gtk.Label(label=code)
        code_lbl.set_selectable(True)
        code_lbl.set_wrap(True)
        code_lbl.set_wrap_mode(0)  # WRAP_WORD_CHAR ~ break anywhere for the long code
        code_lbl.add_css_class("monospace")
        code_lbl.add_css_class("caption")
        box.append(code_lbl)

        copy_btn = Gtk.Button(label="Copy code")
        copy_btn.add_css_class("pill")
        copy_btn.set_halign(Gtk.Align.CENTER)
        copy_btn.connect("clicked", lambda *_: self._copy(win, code))
        box.append(copy_btn)

        footer = Gtk.Label(label=f"Realm “{realm}” · expires in 15 min · single use")
        footer.add_css_class("caption")
        footer.add_css_class("dim-label")
        box.append(footer)

        view.set_content(box)
        win.set_content(view)
        win.present()

    def _copy(self, win, text):
        try:
            win.get_clipboard().set(text)
            self._toast("Invite code copied")
        except Exception:  # noqa: BLE001
            self._toast("Couldn't copy — select the code and copy it manually")

    # ── Helpers ──────────────────────────────────────────────────────────────────────────────────

    def _reveal(self, path):
        try:
            Gtk.show_uri(self.get_root(), Gio.File.new_for_path(path).get_uri(), 0)
        except Exception:  # noqa: BLE001
            pass

    def _on_error(self, exc):
        # A failed status probe most often means loomd isn't running — guide the user there.
        if not self._loaded_once:
            detail = GLib.markup_escape_text(str(exc))
            self._unavailable.set_description(
                f"Couldn't reach loomd.\n{detail}\n\nStart it with:  loomd"
            )
            self._stack.set_visible_child_name("unavailable")
        else:
            self._toast(str(exc))

    def _toast(self, message: str):
        toast = Adw.Toast.new(message)
        toast.set_timeout(3)
        self._toast_overlay.add_toast(toast)

    def _clear_list(self):
        child = self._list.get_first_child()
        while child is not None:
            nxt = child.get_next_sibling()
            self._list.remove(child)
            child = nxt
