"""Media controls widget — controls media playback on the connected phone."""

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, Gdk, GLib, GObject

from phonelink.dbus_client import IFACE_PROPS


class MediaControlsWidget(Gtk.Box):
    """A floating or sidebar widget that displays active media on the phone."""

    def __init__(self, client):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        self.client = client
        self._device = None
        self._signal_ids: list[int] = []
        self._updating_ui = False
        self._current_player = ""
        self._is_playing = False

        # Set margins
        self.set_margin_top(12)
        self.set_margin_bottom(12)
        self.set_margin_start(16)
        self.set_margin_end(16)

        # ── Stack: no active media vs player controls ───────────────
        self._stack = Gtk.Stack()
        self._stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.append(self._stack)

        # Placeholder: No Media Playing
        self._placeholder = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        self._placeholder.set_valign(Gtk.Align.CENTER)
        self._placeholder.set_halign(Gtk.Align.CENTER)
        self._placeholder.set_margin_top(24)
        self._placeholder.set_margin_bottom(24)

        music_icon = Gtk.Image.new_from_icon_name("audio-x-generic-symbolic")
        music_icon.set_pixel_size(48)
        music_icon.set_opacity(0.3)
        self._placeholder.append(music_icon)

        no_media_label = Gtk.Label(label="No media active")
        no_media_label.add_css_class("dim-label")
        self._placeholder.append(no_media_label)
        self._stack.add_named(self._placeholder, "placeholder")

        # Active Player View
        self._player_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        self._stack.add_named(self._player_box, "player")

        # Stylized track cover icon container
        track_art_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        track_art_box.set_halign(Gtk.Align.CENTER)
        track_art_box.set_valign(Gtk.Align.CENTER)
        track_art_box.add_css_class("media-art-container")
        self._player_box.append(track_art_box)

        self._art_icon = Gtk.Image.new_from_icon_name("audio-x-generic-symbolic")
        self._art_icon.set_pixel_size(40)
        self._art_icon.add_css_class("media-art-icon")
        track_art_box.append(self._art_icon)

        # Labels (Title, Artist, Player)
        labels_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        labels_box.set_halign(Gtk.Align.CENTER)
        self._player_box.append(labels_box)

        self._title_label = Gtk.Label()
        self._title_label.set_wrap(True)
        self._title_label.set_wrap_mode(2)  # WORD_CHAR
        self._title_label.set_max_width_chars(28)
        self._title_label.add_css_class("bold")
        self._title_label.set_justify(Gtk.Justification.CENTER)
        labels_box.append(self._title_label)

        self._artist_label = Gtk.Label()
        self._artist_label.set_wrap(True)
        self._artist_label.set_wrap_mode(2)
        self._artist_label.set_max_width_chars(32)
        self._artist_label.add_css_class("caption")
        self._artist_label.add_css_class("dim-label")
        self._artist_label.set_justify(Gtk.Justification.CENTER)
        labels_box.append(self._artist_label)

        # Player badge
        self._player_badge = Gtk.Label()
        self._player_badge.add_css_class("caption")
        self._player_badge.add_css_class("dim-label")
        self._player_badge.set_markup("")
        labels_box.append(self._player_badge)

        # Playback Control Buttons
        controls_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=16)
        controls_box.set_halign(Gtk.Align.CENTER)
        self._player_box.append(controls_box)

        self._prev_btn = Gtk.Button(icon_name="media-skip-backward-symbolic")
        self._prev_btn.add_css_class("flat")
        self._prev_btn.set_tooltip_text("Previous track")
        self._prev_btn.connect("clicked", self._on_prev)
        controls_box.append(self._prev_btn)

        self._play_btn = Gtk.Button(icon_name="media-playback-start-symbolic")
        self._play_btn.add_css_class("flat")
        self._play_btn.add_css_class("suggested-action")
        self._play_btn.set_size_request(44, 44)
        self._play_btn.set_tooltip_text("Play / Pause")
        self._play_btn.connect("clicked", self._on_play_pause)
        controls_box.append(self._play_btn)

        self._next_btn = Gtk.Button(icon_name="media-skip-forward-symbolic")
        self._next_btn.add_css_class("flat")
        self._next_btn.set_tooltip_text("Next track")
        self._next_btn.connect("clicked", self._on_next)
        controls_box.append(self._next_btn)

        # Volume control row
        self._player_box.append(Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL))

        volume_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        volume_box.set_margin_start(4)
        volume_box.set_margin_end(4)
        self._player_box.append(volume_box)

        vol_icon = Gtk.Image.new_from_icon_name("audio-volume-high-symbolic")
        vol_icon.set_opacity(0.6)
        volume_box.append(vol_icon)

        self._volume_scale = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 100, 5)
        self._volume_scale.set_hexpand(True)
        self._volume_scale.set_draw_value(False)
        self._volume_scale.connect("value-changed", self._on_volume_changed)
        volume_box.append(self._volume_scale)

        self._stack.set_visible_child_name("placeholder")

    def set_device(self, device):
        """Set the active device and monitor its media state."""
        self._unsubscribe()
        self._device = device

        if not device or not device.reachable:
            self._stack.set_visible_child_name("placeholder")
            self.emit("media-state-changed", False)
            return

        self._subscribe(device.id)
        # Pull properties on initial connection
        GLib.idle_add(self._reload_properties)

    def _subscribe(self, device_id):
        path = f"/modules/kdeconnect/devices/{device_id}/mprisremote"
        sid = self.client.subscribe_signal(
            path, "org.kde.kdeconnect.device.mprisremote", "propertiesChanged",
            self._on_properties_changed_signal
        )
        if sid is not None:
            self._signal_ids.append(sid)

    def _unsubscribe(self):
        if self.client.bus:
            for sid in self._signal_ids:
                self.client.bus.signal_unsubscribe(sid)
        self._signal_ids.clear()

    def _on_properties_changed_signal(self, conn, sender, path, iface, signal, params):
        GLib.idle_add(self._reload_properties)

    def _reload_properties(self):
        if not self._device:
            return GLib.SOURCE_REMOVE

        try:
            props = self.client.get_mpris_properties(self._device.id)
            self._update_ui(props)
        except Exception as exc:
            print(f"[phonelink] Failed to read MPRIS properties: {exc}")
            self._stack.set_visible_child_name("placeholder")
        return GLib.SOURCE_REMOVE

    def _update_ui(self, props: dict):
        self._updating_ui = True
        try:
            title = props.get("title", "")
            artist = props.get("artist", "")
            player = props.get("player", "")
            is_playing = props.get("isPlaying", False)
            volume = props.get("volume", 0)

            self._current_player = player
            self._is_playing = is_playing

            # If there's no player listed, or title is empty, show placeholder
            if not player or (not title and not artist and not is_playing):
                self._stack.set_visible_child_name("placeholder")
                # Fire notification if icon state changes (useful for parent popovers)
                self.emit("media-state-changed", False)
                return

            self._stack.set_visible_child_name("player")

            # Update Labels
            self._title_label.set_label(title or "Unknown Track")
            self._artist_label.set_label(artist or "Unknown Artist")
            self._player_badge.set_markup(f"<span alpha='65%'>on </span><b>{player}</b>")

            # Update Play/Pause Button
            if is_playing:
                self._play_btn.set_icon_name("media-playback-pause-symbolic")
                self._art_icon.set_from_icon_name("audio-volume-high-symbolic")
            else:
                self._play_btn.set_icon_name("media-playback-start-symbolic")
                self._art_icon.set_from_icon_name("audio-x-generic-symbolic")

            # Update Volume Slider (0-100)
            self._volume_scale.set_value(volume)
            self.emit("media-state-changed", True)
        finally:
            self._updating_ui = False

    # ── Actions ────────────────────────────────────────────────────

    def _on_prev(self, _btn):
        if self._device:
            self.client.send_mpris_action(self._device.id, "previous")

    def _on_next(self, _btn):
        if self._device:
            self.client.send_mpris_action(self._device.id, "next")

    def _on_play_pause(self, _btn):
        if self._device:
            self.client.send_mpris_action(self._device.id, "playpause")

    def _on_volume_changed(self, scale):
        if self._updating_ui or not self._device:
            return
        val = int(scale.get_value())
        try:
            self.client.set_mpris_volume(self._device.id, val)
        except Exception as exc:
            print(f"[phonelink] Failed to set volume: {exc}")


# GObject signals registration
GObject.type_register(MediaControlsWidget)
GObject.signal_new(
    "media-state-changed",
    MediaControlsWidget,
    GObject.SignalFlags.RUN_FIRST,
    None,
    (bool,),
)
