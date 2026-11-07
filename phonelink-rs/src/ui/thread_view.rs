//! The open conversation: bubbles, date headers, "Show earlier messages",
//! and the compose bar. Port of `phonelink/ui/message_thread.py`.
//!
//! Rules carried over (port brief, rules 4 and 5):
//! - Render the newest [`THREAD_WINDOW`] messages; "Show earlier messages (N)"
//!   adds [`THREAD_WINDOW_STEP`].
//! - Open already at the bottom: the message area stays transparent until GTK
//!   has laid it out, then appears at the bottom in that frame. No visible
//!   scroll.
//! - Stay pinned to the bottom while images size in, unless the user
//!   scrolled up.
//! - History older than the window only updates the button count; it never
//!   rebuilds the thread.
//! - Images are measured and decoded off the main thread at 2× display size,
//!   with an LRU of [`IMAGE_CACHE_MAX`] textures.
//! - Find (Ctrl+F, or opening a chat from a search match) steps through
//!   matching messages; a match outside the window widens it, then the
//!   chat appears centred on the match instead of at the bottom.

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::LazyLock;

use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use regex::Regex;

use crate::core::attachments::{existing_full_in, full_cache_dir, image_ext, is_image, sanitize_name};
use crate::core::messages::SendRoute;
use crate::core::models::{Attachment, Conversation, SmsMessage};
use crate::core::private_tmp;
use crate::core::search;

pub const THREAD_WINDOW: usize = 60;
pub const THREAD_WINDOW_STEP: usize = 100;
/// Older messages built above a revealed search match.
const REVEAL_CONTEXT: usize = 10;
/// A centered time line goes in wherever this much time passed between two
/// messages (and at the start of each day). Per-bubble times are hidden
/// except on the newest message; tap a bubble to see its time. (Ethan,
/// 2026-11-05: a time and date on every bubble looked messy. Python showed
/// one per bubble unless the next came within 15 minutes.)
const TIME_LINE_GAP_MS: i64 = 60 * 60 * 1000;

const FULL_IMAGE_MAX_W: f64 = 380.0;
const FULL_IMAGE_MAX_H: f64 = 500.0;
const THUMB_IMAGE_MAX_W: f64 = 300.0;
const THUMB_IMAGE_MAX_H: f64 = 360.0;
const THUMB_IMAGE_MIN_LONG: f64 = 220.0;
const THUMB_IMAGE_UPSCALE_CAP: f64 = 2.2;
/// At or under this the image is a preview, not the full file.
const ATTACHMENT_THUMBNAIL_MAX: i32 = 256;
const IMAGE_DECODE_SCALE: i32 = 2;
const IMAGE_CACHE_MAX: usize = 64;
/// ...and at most this many bytes of pixels (64 full photos at 2× display
/// size would be ~190 MB).
const IMAGE_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
/// Opening a chat lays out this many of the newest bubbles at once; the rest
/// of the window is added above in batches of [`BUILD_BATCH`], one per idle
/// callback, so no frame waits on 60 bubbles (laptop, 2026-11-06: 80–155 ms
/// stalls per open, almost all GTK layout).
const FIRST_BATCH: usize = 15;
const BUILD_BATCH: usize = 8;

// ── pure helpers ───────────────────────────────────────────────────

/// Aspect-preserved on-screen size (Python `_scaled_image_size`).
pub fn scaled_image_size(width: i32, height: i32, full: bool) -> (i32, i32) {
    let (w, h) = (f64::from(width), f64::from(height));
    let (max_w, max_h, min_long, cap) = if full {
        (FULL_IMAGE_MAX_W, FULL_IMAGE_MAX_H, 0.0, 1.0)
    } else {
        (THUMB_IMAGE_MAX_W, THUMB_IMAGE_MAX_H, THUMB_IMAGE_MIN_LONG, THUMB_IMAGE_UPSCALE_CAP)
    };
    let fit = (max_w / w).min(max_h / h);
    let scale = if fit < 1.0 {
        fit
    } else if min_long > 0.0 {
        (min_long / w.max(h)).min(cap).min(fit).max(1.0)
    } else {
        1.0
    };
    (((w * scale).round() as i32).max(1), ((h * scale).round() as i32).max(1))
}

/// (display w, display h, awaiting the full file) for an image of this size.
fn image_display(width: i32, height: i32, can_fetch_full: bool) -> (i32, i32, bool) {
    let is_full = width.max(height) > ATTACHMENT_THUMBNAIL_MAX;
    let (w, h) = scaled_image_size(width, height, is_full);
    (w, h, !is_full && can_fetch_full)
}

fn attachment_name(att: &Attachment) -> String {
    let base = if att.file_name.is_empty() { "attachment" } else { &att.file_name };
    let name = sanitize_name(base);
    let ext = image_ext(&att.mime_type);
    if ext.is_empty() || name.to_lowercase().ends_with(ext) { name } else { format!("{name}{ext}") }
}

/// The inline file: base64 data from the phone, written to the private
/// scratch folder's `message_attachments` (`core::private_tmp`). A path or
/// `file://` payload (rows the Python app saved) is used only when it lies in
/// the app's own folders: the payload comes from the phone, and a rogue
/// device must not make a chat open `~/.ssh/id_ed25519`.
fn attachment_local_path(att: &Attachment) -> Option<PathBuf> {
    let payload = att.payload.as_str();
    if payload.is_empty() {
        return None;
    }
    let as_path = if payload.starts_with("file://") {
        gio::File::for_uri(payload).path()
    } else {
        Some(PathBuf::from(payload)).filter(|p| p.is_absolute())
    };
    if let Some(p) = as_path.filter(|p| p.is_file()) {
        return crate::core::attachments::is_app_file(&p).then_some(p);
    }
    if payload.starts_with("file://") {
        return None;
    }
    let dir = private_tmp::dir(Some("message_attachments"))
        .inspect_err(|e| log::warn!("attachment folder: {e}"))
        .ok()?;
    let uid = if att.unique_identifier.is_empty() {
        if att.part_id == 0 { "attachment".to_string() } else { att.part_id.to_string() }
    } else {
        att.unique_identifier.clone()
    };
    let digest = glib::compute_checksum_for_string(glib::ChecksumType::Sha1, payload)?;
    let target = dir.join(format!("{}-{}{}", sanitize_name(&uid), &digest[..12], image_ext(&att.mime_type)));
    if target.is_file() {
        return Some(target);
    }
    let data = glib::base64_decode(payload);
    if data.is_empty() {
        return None;
    }
    private_tmp::write_private(&target, &data).ok()?;
    Some(target)
}

static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:https?://|www\.)[^\s<>()]+|\b[a-zA-Z0-9][-a-zA-Z0-9]*\.(?:com|org|net|edu|gov|mil|int|info|biz|co|io|ly|me|tv|cc|us|ca|uk|de|fr|jp|app|dev|link|xyz)\b(?:/[^\s<>()]*)?",
    )
    .unwrap()
});
const URL_TRAILING_PUNCTUATION: &str = ".,!?;:)]}";

/// Pango markup with clickable links, and whether there were any.
pub fn message_markup(text: &str) -> (String, bool) {
    let mut out = String::new();
    let mut last = 0;
    let mut has_links = false;
    for m in URL_RE.find_iter(text) {
        let start = m.start();
        let raw = m.as_str().trim_end_matches(|c| URL_TRAILING_PUNCTUATION.contains(c));
        if raw.is_empty() {
            continue;
        }
        let end = start + raw.len();
        let uri = if raw.to_lowercase().starts_with("http://") || raw.to_lowercase().starts_with("https://") {
            raw.to_string()
        } else {
            format!("https://{raw}")
        };
        out.push_str(&glib::markup_escape_text(&text[last..start]));
        out.push_str(&format!("<a href=\"{}\">{}</a>", glib::markup_escape_text(&uri), glib::markup_escape_text(raw)));
        last = end;
        has_links = true;
    }
    if !has_links {
        return (glib::markup_escape_text(text).to_string(), false);
    }
    out.push_str(&glib::markup_escape_text(&text[last..]));
    (out, true)
}

fn clock(ts: chrono::DateTime<chrono::Local>) -> String {
    ts.format("%-I:%M %p").to_string()
}

/// "Today", "Yesterday", the weekday within a week, else the date (with the
/// year only when it isn't this year).
pub fn day_label(day: chrono::NaiveDate, today: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    let age = (today - day).num_days();
    match age {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => day.format("%A").to_string(),
        _ if day.year() == today.year() => day.format("%A, %B %-d").to_string(),
        _ => day.format("%A, %B %-d, %Y").to_string(),
    }
}

/// The centered line to put before `m`, if any: the day and time at a new
/// day, the time after an hour's gap.
pub fn time_line(prev: Option<&SmsMessage>, m: &SmsMessage, today: chrono::NaiveDate) -> Option<String> {
    let ts = m.timestamp();
    match prev {
        Some(p) if p.timestamp().date_naive() == ts.date_naive() => {
            (m.date - p.date >= TIME_LINE_GAP_MS).then(|| clock(ts))
        }
        _ => Some(format!("{} · {}", day_label(ts.date_naive(), today), clock(ts))),
    }
}

fn today() -> chrono::NaiveDate {
    chrono::Local::now().date_naive()
}

// ── image decoding off the main thread ─────────────────────────────

/// What a worker hands back: header size and decoded pixels.
struct Decoded {
    measured: Option<(i32, i32)>,
    pixels: Option<(glib::Bytes, i32, i32, usize, bool)>,
}

fn decode(path: &Path, can_fetch_full: bool) -> Decoded {
    let Some((w, h)) = super::decodable_image(path) else { return Decoded { measured: None, pixels: None } };
    let (dw, dh, _) = image_display(w, h, can_fetch_full);
    let pixels = Pixbuf::from_file_at_scale(path, dw * IMAGE_DECODE_SCALE, dh * IMAGE_DECODE_SCALE, true)
        .ok()
        .map(|p| p.apply_embedded_orientation().unwrap_or(p))
        .map(|p| (p.read_pixel_bytes(), p.width(), p.height(), p.rowstride() as usize, p.has_alpha()));
    Decoded { measured: Some((w, h)), pixels }
}

type ImageKey = (PathBuf, i64);

fn image_key(path: &Path) -> ImageKey {
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    (path.to_path_buf(), mtime)
}

#[derive(Default)]
struct ImageCache {
    sizes: std::collections::HashMap<ImageKey, Option<(i32, i32)>>,
    textures: VecDeque<(ImageKey, gdk::Texture)>,
}

impl ImageCache {
    fn texture(&mut self, key: &ImageKey) -> Option<gdk::Texture> {
        let i = self.textures.iter().position(|(k, _)| k == key)?;
        let entry = self.textures.remove(i)?;
        let tex = entry.1.clone();
        self.textures.push_back(entry);
        Some(tex)
    }
    fn remember(&mut self, key: ImageKey, tex: gdk::Texture) {
        self.textures.retain(|(k, _)| *k != key);
        self.textures.push_back((key, tex));
        let bytes = |c: &VecDeque<(ImageKey, gdk::Texture)>| {
            c.iter().map(|(_, t)| t.width() as usize * t.height() as usize * 4).sum::<usize>()
        };
        while self.textures.len() > IMAGE_CACHE_MAX
            || (self.textures.len() > 1 && bytes(&self.textures) > IMAGE_CACHE_MAX_BYTES)
        {
            self.textures.pop_front();
        }
    }
}

thread_local! {
    static IMAGES: RefCell<ImageCache> = RefCell::default();
}

// ── widgets ────────────────────────────────────────────────────────

/// Callbacks the bubbles need from the page.
#[derive(Clone)]
pub struct BubbleActions {
    /// `(part_id, unique_identifier, file_name)`: ask the phone for the file.
    pub download: Rc<dyn Fn(i64, String, String)>,
    /// "Text back" on a missed call: focus the compose box.
    pub text_back: Rc<dyn Fn()>,
}

struct Bubble {
    widget: gtk::Box,
    /// The text, when it has no links (links are markup already).
    body: Option<gtk::Label>,
    revealer: gtk::Revealer,
    /// The send route on its own, shown while the time is hidden.
    route_tag: Option<gtk::Label>,
    message: SmsMessage,
    /// The route it was drawn with (a delivery change redraws it).
    route: Option<SendRoute>,
}

impl Bubble {
    fn new(msg: &SmsMessage, show_time: bool, route: Option<SendRoute>, actions: &BubbleActions) -> Rc<Self> {
        if msg.is_call() {
            return Self::call(msg, actions);
        }
        let sent = msg.is_sent();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 1);
        widget.set_halign(if sent { gtk::Align::End } else { gtk::Align::Start });
        widget.set_margin_top(1);
        widget.set_margin_bottom(1);
        widget.set_margin_start(if sent { 48 } else { 12 });
        widget.set_margin_end(if sent { 12 } else { 48 });

        let frame = gtk::Box::new(gtk::Orientation::Vertical, 3);
        frame.set_margin_top(2);
        frame.set_margin_bottom(2);
        frame.set_margin_start(12);
        frame.set_margin_end(12);
        frame.add_css_class(if sent { "message-bubble-sent" } else { "message-bubble-received" });

        let (markup, has_links) = message_markup(&msg.body);
        let body = gtk::Label::new(None);
        body.set_wrap(true);
        body.set_wrap_mode(pango::WrapMode::WordChar);
        body.set_max_width_chars(50);
        body.set_halign(gtk::Align::Start);
        body.set_xalign(0.0);
        body.set_selectable(!has_links);
        body.add_css_class("message-zoomable");
        if has_links {
            body.set_markup(&markup);
        } else {
            body.set_text(&msg.body);
        }
        if !msg.body.is_empty() {
            frame.append(&body);
        }
        // Pictures show as themselves, above the text; other files are a
        // chip inside the bubble.
        for att in &msg.attachments {
            match image_widget(att, sent, actions) {
                Some(w) => widget.append(&w),
                None => frame.append(&file_chip(att, actions)),
            }
        }
        if frame.first_child().is_some() {
            widget.append(&frame);
        }

        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        revealer.set_transition_duration(160);
        let mut label = format!("{} {}", if sent { "Sent" } else { "Received" }, clock(msg.timestamp()));
        if let Some(r) = route {
            label.push_str(" · ");
            label.push_str(r.label());
        }
        let time = gtk::Label::new(Some(&label));
        time.set_halign(if sent { gtk::Align::End } else { gtk::Align::Start });
        time.add_css_class("dim-label");
        time.add_css_class("caption");
        time.add_css_class("message-timestamp");
        time.set_margin_start(if sent { 0 } else { 12 });
        time.set_margin_end(if sent { 12 } else { 0 });
        revealer.set_child(Some(&time));
        widget.append(&revealer);
        // How a text sent from this app left it: always shown (with the time
        // when that shows, else on its own), so a fallback to SMS is never
        // hidden.
        let route_tag = route.map(|r| {
            let tag = gtk::Label::new(Some(r.label()));
            tag.set_halign(gtk::Align::End);
            tag.add_css_class("dim-label");
            tag.add_css_class("caption");
            tag.add_css_class("message-timestamp");
            tag.set_margin_end(12);
            widget.append(&tag);
            tag
        });

        let body = (!has_links && !msg.body.is_empty()).then_some(body);
        let bubble = Rc::new(Self { widget, body, revealer, route_tag, message: msg.clone(), route });
        bubble.set_timestamp_visible(show_time);
        if !has_links {
            // Tap a bubble to show or hide its time.
            let click = gtk::GestureClick::new();
            let weak = Rc::downgrade(&bubble);
            click.connect_pressed(move |_, _, _, _| {
                if let Some(b) = weak.upgrade() {
                    b.set_timestamp_visible(!b.revealer.reveals_child());
                }
            });
            frame.add_controller(click);
        }
        bubble
    }

    /// A call in the timeline: a centred line, "Missed call · 3:12 PM",
    /// with "Text back" on a missed one.
    fn call(msg: &SmsMessage, actions: &BubbleActions) -> Rc<Self> {
        let missed = msg.msg_type == crate::core::models::MSG_TYPE_CALL_MISSED;
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.set_halign(gtk::Align::Center);
        widget.set_margin_top(6);
        widget.set_margin_bottom(6);
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        line.add_css_class("call-line");
        if missed {
            line.add_css_class("call-missed");
        }
        let icon = gtk::Image::from_icon_name(if missed { "call-missed-symbolic" } else { "call-incoming-symbolic" });
        let label = gtk::Label::new(Some(&format!("{} · {}", msg.body, clock(msg.timestamp()))));
        label.add_css_class("caption");
        line.append(&icon);
        line.append(&label);
        if missed {
            let back = gtk::Button::with_label("Text back");
            back.add_css_class("flat");
            back.add_css_class("caption");
            let text_back = actions.text_back.clone();
            back.connect_clicked(move |_| text_back());
            line.append(&back);
        }
        widget.append(&line);
        let revealer = gtk::Revealer::new();
        Rc::new(Self { widget, body: None, revealer, route_tag: None, message: msg.clone(), route: None })
    }

    /// Outline the bubble and bold `needle` in its text (None clears).
    fn set_search_hit(&self, needle: Option<&str>) {
        match needle {
            Some(n) => {
                self.widget.add_css_class("search-hit");
                if let (Some(body), Some(q)) = (&self.body, search::fold_query(n)) {
                    let ranges = search::fold(&self.message.body).find_all(&q);
                    body.set_markup(&search::bold_markup(&self.message.body, &ranges));
                }
            }
            None => {
                self.widget.remove_css_class("search-hit");
                if let Some(body) = &self.body {
                    body.set_text(&self.message.body);
                }
            }
        }
    }

    fn set_timestamp_visible(&self, visible: bool) {
        self.revealer.set_reveal_child(visible);
        if let Some(tag) = &self.route_tag {
            tag.set_visible(!visible);
        }
        if visible {
            self.widget.add_css_class("message-bubble-timestamp-visible");
        } else {
            self.widget.remove_css_class("message-bubble-timestamp-visible");
        }
    }
}

fn open_file(path: &Path) {
    let uri = gio::File::for_path(path).uri();
    if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE) {
        log::warn!("open attachment: {e}");
    }
}

/// What a picture or file can do. Offered on hover (pictures), right-click,
/// and click (open).
#[derive(Clone, Copy)]
enum FileAction {
    Open,
    CopyImage,
    Save,
    Download,
}

fn run_action(action: FileAction, widget: &gtk::Widget, path: Option<&Path>, att: &Attachment, actions: &BubbleActions) {
    let name = attachment_name(att);
    match (action, path) {
        (FileAction::Open, Some(p)) => open_file(p),
        (FileAction::CopyImage, Some(p)) => match super::decodable_image(p).map(|_| gdk::Texture::from_filename(p)) {
            Some(Ok(tex)) => widget.clipboard().set_texture(&tex),
            Some(Err(e)) => log::warn!("copy image: {e}"),
            None => {}
        },
        (FileAction::Save, Some(p)) => save_as(widget, p, &name),
        (FileAction::Download, _) | (_, None) => (actions.download)(att.part_id, att.unique_identifier.clone(), name),
    }
}

/// A right-click menu at the pointer.
fn context_menu(anchor: &gtk::Widget, x: f64, y: f64, items: &[(&'static str, FileAction)], run: Rc<dyn Fn(FileAction)>) {
    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.set_parent(anchor);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    for &(label, action) in items {
        let b = gtk::Button::with_label(label);
        b.add_css_class("flat");
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            l.set_xalign(0.0);
        }
        let (run, pop) = (run.clone(), popover.clone());
        b.connect_clicked(move |_| {
            pop.popdown();
            run(action);
        });
        menu.append(&b);
    }
    popover.set_child(Some(&menu));
    // A popover must be unparented before its anchor goes away.
    popover.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    popover.popup();
}

/// An inline picture, shown full width of its own (no bubble, no buttons
/// underneath). Hover shows Copy / Save / Open; right-click has the same;
/// click opens it. While only the phone's small preview is here, the full
/// file has been asked for and the picture redraws when it arrives.
fn image_widget(att: &Attachment, sent: bool, actions: &BubbleActions) -> Option<gtk::Widget> {
    if !is_image(att) {
        return None;
    }
    let full = existing_full_in(&full_cache_dir(), att);
    let have_full = full.is_some();
    let path = full.or_else(|| attachment_local_path(att))?;
    let can_fetch_full = att.part_id != 0 && !have_full;

    let overlay = gtk::Overlay::new();
    overlay.set_halign(if sent { gtk::Align::End } else { gtk::Align::Start });
    overlay.set_margin_start(12);
    overlay.set_margin_end(12);
    overlay.set_margin_top(2);
    overlay.set_margin_bottom(2);
    let picture = gtk::Picture::new();
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.set_overflow(gtk::Overflow::Hidden);
    picture.add_css_class("message-image");
    picture.set_cursor_from_name(Some("pointer"));
    overlay.set_child(Some(&picture));

    let run: Rc<dyn Fn(FileAction)> = {
        let (w, p, a, acts) = (overlay.clone().upcast::<gtk::Widget>(), path.clone(), att.clone(), actions.clone());
        Rc::new(move |action| run_action(action, &w, Some(&p), &a, &acts))
    };

    // Hover buttons, top right.
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    buttons.set_halign(gtk::Align::End);
    buttons.set_valign(gtk::Align::Start);
    buttons.set_margin_top(6);
    buttons.set_margin_end(6);
    buttons.set_visible(false);
    for (icon, tip, action) in [
        ("edit-copy-symbolic", "Copy image", FileAction::CopyImage),
        ("document-save-symbolic", "Save image…", FileAction::Save),
        ("view-fullscreen-symbolic", "Open", FileAction::Open),
    ] {
        let b = gtk::Button::from_icon_name(icon);
        b.set_tooltip_text(Some(tip));
        b.add_css_class("osd");
        b.add_css_class("circular");
        let run = run.clone();
        b.connect_clicked(move |_| run(action));
        buttons.append(&b);
    }
    overlay.add_overlay(&buttons);
    let motion = gtk::EventControllerMotion::new();
    {
        let (show, hide) = (buttons.clone(), buttons.clone());
        motion.connect_enter(move |_, _, _| show.set_visible(true));
        motion.connect_leave(move |_| hide.set_visible(false));
    }
    overlay.add_controller(motion);

    let left = gtk::GestureClick::new();
    left.set_button(gdk::BUTTON_PRIMARY);
    {
        let run = run.clone();
        left.connect_released(move |_, _, _, _| run(FileAction::Open));
    }
    picture.add_controller(left);
    let right = gtk::GestureClick::new();
    right.set_button(gdk::BUTTON_SECONDARY);
    {
        let (run, anchor) = (run.clone(), overlay.clone().upcast::<gtk::Widget>());
        right.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            context_menu(
                &anchor,
                x,
                y,
                &[("Open", FileAction::Open), ("Copy image", FileAction::CopyImage), ("Save image…", FileAction::Save)],
                run.clone(),
            );
        });
    }
    overlay.add_controller(right);

    let apply_size = {
        let picture = picture.clone();
        move |measured: Option<(i32, i32)>| {
            let (w, h) = measured.unwrap_or((FULL_IMAGE_MAX_W as i32, FULL_IMAGE_MAX_H as i32));
            let (dw, dh, waiting) = image_display(w, h, can_fetch_full);
            picture.set_size_request(dw, dh);
            if waiting {
                picture.add_css_class("message-attachment-loading");
                picture.set_tooltip_text(Some("Loading the full picture…"));
            } else {
                picture.remove_css_class("message-attachment-loading");
                picture.set_tooltip_text(None);
            }
        }
    };
    let key = image_key(&path);
    let (known, texture) = IMAGES.with_borrow_mut(|c| (c.sizes.get(&key).copied(), c.texture(&key)));
    match known {
        Some(measured) => apply_size(measured),
        None => picture.set_size_request(THUMB_IMAGE_MAX_W as i32, THUMB_IMAGE_MAX_H as i32),
    }
    match texture {
        Some(tex) => picture.set_paintable(Some(&tex)),
        None => {
            let picture = picture.clone();
            glib::spawn_future_local(async move {
                let p = path.clone();
                let Ok(decoded) = gio::spawn_blocking(move || decode(&p, can_fetch_full)).await else { return };
                IMAGES.with_borrow_mut(|c| c.sizes.insert(key.clone(), decoded.measured));
                apply_size(decoded.measured);
                if let Some((bytes, w, h, stride, alpha)) = decoded.pixels {
                    let format = if alpha { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
                    let tex: gdk::Texture = gdk::MemoryTexture::new(w, h, format, &bytes, stride).upcast();
                    IMAGES.with_borrow_mut(|c| c.remember(key, tex.clone()));
                    picture.set_paintable(Some(&tex));
                }
            });
        }
    }
    Some(overlay.upcast())
}

/// Any other file (or a picture we have no data for): a chip in the bubble.
/// Click opens it (or asks the phone for it); right-click has Open / Save.
fn file_chip(att: &Attachment, actions: &BubbleActions) -> gtk::Widget {
    let path = if is_image(att) { existing_full_in(&full_cache_dir(), att) } else { None }
        .or_else(|| attachment_local_path(att));
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("message-file-chip");
    row.set_cursor_from_name(Some("pointer"));
    let icon = gtk::Image::from_icon_name(if is_image(att) { "image-x-generic-symbolic" } else { "mail-attachment-symbolic" });
    icon.set_pixel_size(16);
    row.append(&icon);
    let label = gtk::Label::new(Some(&attachment_name(att)));
    label.set_ellipsize(pango::EllipsizeMode::Middle);
    label.set_max_width_chars(32);
    label.set_halign(gtk::Align::Start);
    row.append(&label);
    if path.is_none() && att.part_id != 0 {
        let hint = gtk::Label::new(Some("Click to download"));
        hint.add_css_class("caption");
        hint.add_css_class("dim-label");
        row.append(&hint);
    }
    let run: Rc<dyn Fn(FileAction)> = {
        let (w, p, a, acts) = (row.clone().upcast::<gtk::Widget>(), path.clone(), att.clone(), actions.clone());
        Rc::new(move |action| run_action(action, &w, p.as_deref(), &a, &acts))
    };
    let left = gtk::GestureClick::new();
    left.set_button(gdk::BUTTON_PRIMARY);
    {
        let (run, local) = (run.clone(), path.is_some());
        left.connect_released(move |_, _, _, _| run(if local { FileAction::Open } else { FileAction::Download }));
    }
    row.add_controller(left);
    let right = gtk::GestureClick::new();
    right.set_button(gdk::BUTTON_SECONDARY);
    {
        let (run, anchor, local) = (run.clone(), row.clone().upcast::<gtk::Widget>(), path.is_some());
        right.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            let items: &[(&'static str, FileAction)] = if local {
                &[("Open", FileAction::Open), ("Save as…", FileAction::Save)]
            } else {
                &[("Download from phone", FileAction::Download)]
            };
            context_menu(&anchor, x, y, items, run.clone());
        });
    }
    row.add_controller(right);
    row.upcast()
}

/// "Save as" with the attachment's name filled in.
fn save_as(widget: &gtk::Widget, path: &Path, file_name: &str) {
    let dialog = gtk::FileDialog::new();
    dialog.set_title("Save");
    dialog.set_initial_name(Some(file_name));
    let root = widget.root().and_downcast::<gtk::Window>();
    let path = path.to_path_buf();
    dialog.save(root.as_ref(), gio::Cancellable::NONE, move |res| {
        let Some(target) = res.ok().and_then(|f| f.path()) else { return };
        if let Err(e) = std::fs::copy(&path, &target) {
            log::warn!("save attachment: {e}");
        }
    });
}

fn date_separator(text: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    b.set_halign(gtk::Align::Center);
    b.set_margin_top(12);
    b.set_margin_bottom(8);
    let label = gtk::Label::new(Some(text));
    label.add_css_class("dim-label");
    label.add_css_class("caption");
    label.add_css_class("date-separator");
    b.append(&label);
    b
}

// ── the thread view ────────────────────────────────────────────────

#[derive(Default)]
struct State {
    thread_id: Option<i64>,
    window: usize,
    /// The whole thread, oldest first.
    all: Vec<SmsMessage>,
    /// How each message sent from this app left it.
    routes: std::collections::HashMap<i64, SendRoute>,
    rendered: HashSet<i64>,
    last_rendered_date: i64,
    window_first_date: i64,
    bubbles: Vec<Rc<Bubble>>,
    earlier: Option<gtk::Button>,
    pinned: bool,
    revealing: bool,
    /// A scroll correction is waiting for the next frame.
    scroll_fix_queued: bool,
    /// Bumped on every rebuild, so a stale batch of older bubbles stops.
    generation: u64,
    /// While older bubbles are added above: the scroll distance from the
    /// bottom to hold.
    anchor_from_bottom: Option<f64>,
    /// Find: the text, the matching uids (oldest first) and the one shown.
    find_needle: String,
    find_matches: Vec<i64>,
    find_current: usize,
    /// The match outlined now.
    highlighted: Option<i64>,
    /// Show this message, not the bottom, when the build finishes.
    reveal: Option<i64>,
    /// A chat about to open from a search match: (thread, text, uid).
    pending_find: Option<(i64, String, i64)>,
}

pub struct ThreadView {
    pub widget: gtk::Stack,
    contact: gtk::Label,
    address: gtk::Label,
    scroll: gtk::ScrolledWindow,
    messages_box: gtk::Box,
    pub entry: gtk::Entry,
    pub send_button: gtk::Button,
    preview_bar: gtk::Box,
    preview_picture: gtk::Picture,
    pending_image: RefCell<Option<PathBuf>>,
    /// `(thread_id, text, picture)` → whether it was taken (then the compose
    /// bar clears).
    on_send: RefCell<Option<SendHandler>>,
    /// Message-text zoom (pinch, Ctrl+scroll, Ctrl +/−/0).
    font_scale: Cell<f64>,
    zoom_base: Cell<f64>,
    font_provider: gtk::CssProvider,
    on_zoom: RefCell<Option<ZoomHandler>>,
    /// Told the uids of messages just put on screen (for full pictures).
    on_rendered: RefCell<Option<RenderedHandler>>,
    actions: BubbleActions,
    find_bar: gtk::SearchBar,
    find_entry: gtk::SearchEntry,
    find_label: gtk::Label,
    state: RefCell<State>,
}

impl ThreadView {
    pub fn new(actions: BubbleActions) -> Rc<Self> {
        let widget = gtk::Stack::new();
        widget.set_hexpand(true);
        widget.set_vexpand(true);
        widget.set_transition_type(gtk::StackTransitionType::Crossfade);

        let empty = gtk::Box::new(gtk::Orientation::Vertical, 8);
        empty.set_valign(gtk::Align::Center);
        empty.set_halign(gtk::Align::Center);
        let icon = gtk::Image::from_icon_name("mail-unread-symbolic");
        icon.set_pixel_size(48);
        icon.set_opacity(0.2);
        empty.append(&icon);
        let hint = gtk::Label::new(Some("Select a conversation"));
        hint.add_css_class("dim-label");
        hint.add_css_class("title-4");
        empty.append(&hint);
        widget.add_named(&empty, Some("empty"));

        let thread_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_named(&thread_box, Some("thread"));
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.set_margin_start(12);
        header.set_margin_end(12);
        header.set_margin_top(8);
        header.set_margin_bottom(8);
        header.add_css_class("thread-header");
        let contact = gtk::Label::new(None);
        contact.add_css_class("title-3");
        contact.set_hexpand(true);
        contact.set_halign(gtk::Align::Start);
        contact.set_ellipsize(pango::EllipsizeMode::End);
        header.append(&contact);
        let address = gtk::Label::new(None);
        address.add_css_class("dim-label");
        address.add_css_class("caption");
        header.append(&address);
        thread_box.append(&header);
        thread_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // Find in this chat (Ctrl+F; opens by itself from a search match).
        let find_entry = gtk::SearchEntry::new();
        find_entry.set_placeholder_text(Some("Find in this chat"));
        find_entry.set_hexpand(true);
        let find_label = gtk::Label::new(None);
        find_label.add_css_class("dim-label");
        find_label.add_css_class("caption");
        find_label.add_css_class("numeric");
        let find_older = gtk::Button::from_icon_name("go-up-symbolic");
        find_older.set_tooltip_text(Some("Earlier match (Enter)"));
        let find_newer = gtk::Button::from_icon_name("go-down-symbolic");
        find_newer.set_tooltip_text(Some("Later match (Shift+Enter)"));
        let find_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        find_box.set_size_request(360, -1);
        find_box.append(&find_entry);
        find_box.append(&find_label);
        find_box.append(&find_older);
        find_box.append(&find_newer);
        let find_bar = gtk::SearchBar::new();
        find_bar.set_child(Some(&find_box));
        find_bar.set_show_close_button(true);
        find_bar.connect_entry(&find_entry);
        thread_box.append(&find_bar);

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        scroll.set_propagate_natural_width(false);
        let messages_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        messages_box.set_margin_bottom(8);
        scroll.set_child(Some(&messages_box));
        thread_box.append(&scroll);

        thread_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // A pasted picture waiting to go with the next send.
        let preview_bar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        preview_bar.set_margin_start(12);
        preview_bar.set_margin_end(12);
        preview_bar.set_margin_top(8);
        preview_bar.add_css_class("compose-image-preview-card");
        preview_bar.set_visible(false);
        let preview_picture = gtk::Picture::new();
        preview_picture.set_size_request(96, 96);
        preview_picture.set_can_shrink(true);
        preview_picture.set_content_fit(gtk::ContentFit::Cover);
        preview_picture.set_overflow(gtk::Overflow::Hidden);
        preview_picture.add_css_class("message-image");
        preview_bar.append(&preview_picture);
        let preview_text = gtk::Box::new(gtk::Orientation::Vertical, 4);
        preview_text.set_valign(gtk::Align::Center);
        preview_text.set_hexpand(true);
        let preview_label = gtk::Label::new(Some("Picture pasted"));
        preview_label.add_css_class("heading");
        preview_label.set_xalign(0.0);
        preview_text.append(&preview_label);
        let preview_hint = gtk::Label::new(Some("It goes with your next message"));
        preview_hint.add_css_class("dim-label");
        preview_hint.add_css_class("caption");
        preview_hint.set_xalign(0.0);
        preview_text.append(&preview_hint);
        preview_bar.append(&preview_text);
        let preview_remove = gtk::Button::from_icon_name("edit-clear-symbolic");
        preview_remove.add_css_class("flat");
        preview_remove.set_valign(gtk::Align::Center);
        preview_remove.set_tooltip_text(Some("Remove picture"));
        preview_bar.append(&preview_remove);
        thread_box.append(&preview_bar);

        let compose = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        compose.set_margin_start(12);
        compose.set_margin_end(12);
        compose.set_margin_top(8);
        compose.set_margin_bottom(8);
        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some("Type a message…  (Ctrl+V pastes a picture)"));
        entry.set_hexpand(true);
        compose.append(&entry);
        let send_button = gtk::Button::from_icon_name("mail-send-symbolic");
        send_button.add_css_class("suggested-action");
        send_button.set_tooltip_text(Some("Send"));
        compose.append(&send_button);
        thread_box.append(&compose);
        widget.set_visible_child_name("empty");

        let this = Rc::new(Self {
            widget,
            contact,
            address,
            scroll: scroll.clone(),
            messages_box,
            entry: entry.clone(),
            send_button: send_button.clone(),
            preview_bar,
            preview_picture,
            pending_image: RefCell::default(),
            on_send: RefCell::default(),
            font_scale: Cell::new(1.0),
            zoom_base: Cell::new(1.0),
            font_provider: gtk::CssProvider::new(),
            on_zoom: RefCell::default(),
            on_rendered: RefCell::default(),
            actions,
            find_bar: find_bar.clone(),
            find_entry: find_entry.clone(),
            find_label,
            state: RefCell::new(State { window: THREAD_WINDOW, pinned: true, ..Default::default() }),
        });

        let weak = Rc::downgrade(&this);
        find_entry.connect_search_changed(move |e| {
            if let Some(this) = weak.upgrade() {
                this.find_changed(&e.text());
            }
        });
        for (button, step) in [(&find_older, -1), (&find_newer, 1)] {
            let weak = Rc::downgrade(&this);
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.find_step(step);
                }
            });
        }
        let weak = Rc::downgrade(&this);
        find_entry.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.find_step(-1);
            }
        });
        let weak = Rc::downgrade(&this);
        find_entry.connect_next_match(move |_| {
            if let Some(this) = weak.upgrade() {
                this.find_step(-1);
            }
        });
        let weak = Rc::downgrade(&this);
        find_entry.connect_previous_match(move |_| {
            if let Some(this) = weak.upgrade() {
                this.find_step(1);
            }
        });
        let (bar, compose) = (find_bar.clone(), this.entry.clone());
        find_entry.connect_stop_search(move |_| {
            bar.set_search_mode(false);
            compose.grab_focus();
        });
        let shift_enter = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&this);
        shift_enter.connect_key_pressed(move |_, key, _, state| {
            let enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
            match (enter && state.contains(gdk::ModifierType::SHIFT_MASK), weak.upgrade()) {
                (true, Some(this)) => {
                    this.find_step(1);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        find_entry.add_controller(shift_enter);
        let weak = Rc::downgrade(&this);
        find_bar.connect_search_mode_enabled_notify(move |bar| {
            if !bar.is_search_mode()
                && let Some(this) = weak.upgrade()
            {
                this.end_find();
            }
        });

        let weak = Rc::downgrade(&this);
        entry.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.send();
            }
        });
        let weak = Rc::downgrade(&this);
        send_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.send();
            }
        });
        let weak = Rc::downgrade(&this);
        preview_remove.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.clear_pending_image(true);
            }
        });
        // Ctrl+V / Shift+Insert with a picture on the clipboard attaches it.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, state| {
            let paste = (state.contains(gdk::ModifierType::CONTROL_MASK) && matches!(key, gdk::Key::v | gdk::Key::V))
                || (state.contains(gdk::ModifierType::SHIFT_MASK) && key == gdk::Key::Insert);
            match (paste, weak.upgrade()) {
                (true, Some(this)) if this.paste_picture() => glib::Propagation::Stop,
                _ => glib::Propagation::Proceed,
            }
        });
        this.entry.add_controller(keys);

        // Just above the app stylesheet, so the zoom wins.
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &this.font_provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }
        let pinch = gtk::GestureZoom::new();
        let weak = Rc::downgrade(&this);
        pinch.connect_begin(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.zoom_base.set(this.font_scale.get());
            }
        });
        let weak = Rc::downgrade(&this);
        pinch.connect_scale_changed(move |_, scale| {
            if let Some(this) = weak.upgrade() {
                this.set_font_scale(this.zoom_base.get() * scale, false);
            }
        });
        let weak = Rc::downgrade(&this);
        pinch.connect_end(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.set_font_scale(this.font_scale.get(), true);
            }
        });
        scroll.add_controller(pinch);
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        wheel.connect_scroll(move |c, _, dy| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if !c.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) || dy == 0.0 {
                return glib::Propagation::Proceed; // a normal scroll
            }
            this.set_font_scale(this.font_scale.get() + if dy < 0.0 { 0.1 } else { -0.1 }, true);
            glib::Propagation::Stop
        });
        scroll.add_controller(wheel);
        // Ctrl +/−/0 work anywhere in the window, as in Python (global
        // shortcuts, so focus doesn't have to be in the thread).
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_scope(gtk::ShortcutScope::Global);
        for (trigger, step) in [
            ("<Control>plus|<Control>equal|<Control>KP_Add", 0.1),
            ("<Control>minus|<Control>KP_Subtract", -0.1),
            ("<Control>0|<Control>KP_0", 0.0),
        ] {
            let weak = Rc::downgrade(&this);
            let action = gtk::CallbackAction::new(move |_, _| {
                let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
                let next = if step == 0.0 { 1.0 } else { this.font_scale.get() + step };
                this.set_font_scale(next, true);
                glib::Propagation::Stop
            });
            shortcuts.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(trigger), Some(action)));
        }
        let weak = Rc::downgrade(&this);
        let find = gtk::CallbackAction::new(move |_, _| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if !this.widget.is_mapped() || this.widget.visible_child_name().as_deref() != Some("thread") {
                return glib::Propagation::Proceed;
            }
            this.find_bar.set_search_mode(true);
            this.find_entry.grab_focus();
            this.find_entry.select_region(0, -1);
            glib::Propagation::Stop
        });
        shortcuts.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string("<Control>f"), Some(find)));
        this.widget.add_controller(shortcuts);

        // Pinned to the bottom unless the user scrolled up; stay there while
        // content grows (images sizing in).
        let adj = scroll.vadjustment();
        let weak = Rc::downgrade(&this);
        adj.connect_value_changed(move |a| {
            if let Some(this) = weak.upgrade()
                && let Ok(mut st) = this.state.try_borrow_mut()
            {
                st.pinned = a.value() + a.page_size() >= a.upper() - 48.0;
            }
        });
        let weak = Rc::downgrade(&this);
        adj.connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.queue_scroll_fix();
            }
        });
        this
    }

    /// Start at the saved scale; `persist` is called with each settled one.
    pub fn set_zoom(&self, scale: f64, persist: impl Fn(f64) + 'static) {
        *self.on_zoom.borrow_mut() = Some(Box::new(persist));
        self.font_scale.set(f64::NAN); // force the first apply
        self.set_font_scale(scale, false);
    }

    fn set_font_scale(&self, scale: f64, persist: bool) {
        use crate::core::settings::{MESSAGE_FONT_SCALE_MAX, MESSAGE_FONT_SCALE_MIN};
        let scale = scale.clamp(MESSAGE_FONT_SCALE_MIN, MESSAGE_FONT_SCALE_MAX);
        let old = self.font_scale.get();
        if old.is_nan() || (scale - old).abs() >= 0.001 {
            self.font_scale.set(scale);
            let pct = (scale * 100.0).round() as i64;
            self.font_provider.load_from_data(&format!(".message-zoomable {{ font-size: {pct}%; }}"));
        }
        if persist && let Some(f) = self.on_zoom.borrow().as_ref() {
            f(self.font_scale.get());
        }
    }

    /// `f(thread, uids)` after messages are put on screen. Called from an
    /// idle callback, so it may borrow whatever the caller of `sync` holds.
    pub fn connect_rendered(&self, f: impl Fn(i64, Vec<i64>) + 'static) {
        *self.on_rendered.borrow_mut() = Some(Rc::new(f));
    }

    fn rendered(&self, uids: Vec<i64>) {
        let (Some(tid), Some(f)) = (self.state.borrow().thread_id, self.on_rendered.borrow().clone()) else { return };
        if !uids.is_empty() {
            glib::idle_add_local_once(move || f(tid, uids));
        }
    }

    pub fn connect_send(&self, f: impl Fn(i64, &str, Option<&Path>) -> bool + 'static) {
        *self.on_send.borrow_mut() = Some(Box::new(f));
    }

    fn send(&self) {
        let Some(tid) = self.state.borrow().thread_id else { return };
        let text = self.entry.text().trim().to_string();
        let image = self.pending_image.borrow().clone();
        if text.is_empty() && image.is_none() {
            return;
        }
        let taken = self.on_send.borrow().as_ref().is_some_and(|f| f(tid, &text, image.as_deref()));
        if taken {
            self.entry.set_text("");
            if let Some(path) = image {
                self.clear_pending_image(false);
                // A pasted picture we saved ourselves is removed once KDE
                // Connect has had time to read it, as Python did. A file
                // pasted from the user's own folders is theirs: never touched.
                if private_tmp::contains(&path) {
                    glib::timeout_add_seconds_local_once(120, move || {
                        let _ = std::fs::remove_file(path);
                    });
                }
            }
        }
    }

    /// If the clipboard holds a picture (pixels, or an image file), make it
    /// the pending attachment. Returns whether it looked like one.
    fn paste_picture(self: &Rc<Self>) -> bool {
        let clipboard = self.entry.clipboard();
        let formats = clipboard.formats();
        let has_texture = formats.contains_type(gdk::Texture::static_type())
            || formats.mime_types().iter().any(|m| m.starts_with("image/"));
        let weak = Rc::downgrade(self);
        if has_texture {
            clipboard.read_texture_async(gio::Cancellable::NONE, move |res| {
                let (Some(this), Ok(Some(tex))) = (weak.upgrade(), res) else { return };
                let dir = match private_tmp::dir(None) {
                    Ok(d) => d,
                    Err(e) => {
                        log::warn!("paste folder: {e}");
                        return;
                    }
                };
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis());
                let path = dir.join(format!("paste_{stamp}.png"));
                if private_tmp::write_private(&path, &tex.save_to_png_bytes()).is_ok() {
                    this.set_pending_image(path);
                }
            });
            return true;
        }
        if formats.contain_mime_type("text/uri-list") {
            clipboard.read_text_async(gio::Cancellable::NONE, move |res| {
                let (Some(this), Ok(Some(text))) = (weak.upgrade(), res) else { return };
                if let Some(path) = image_path_from_uri_list(&text) {
                    this.set_pending_image(path);
                }
            });
            return true;
        }
        false
    }

    fn set_pending_image(&self, path: PathBuf) {
        self.clear_pending_image(true);
        self.preview_picture.set_filename(Some(&path));
        self.preview_bar.set_visible(true);
        *self.pending_image.borrow_mut() = Some(path);
    }

    /// Drop the pending picture; `delete` removes a pasted temp file.
    fn clear_pending_image(&self, delete: bool) {
        if let Some(path) = self.pending_image.borrow_mut().take()
            && delete
            && private_tmp::contains(&path)
        {
            let _ = std::fs::remove_file(path);
        }
        self.preview_picture.set_filename(None::<&Path>);
        self.preview_bar.set_visible(false);
    }

    /// The content grew (older bubbles above, a picture sized in): put the
    /// view back where it belongs at the start of the next frame. Setting the
    /// value right in the adjustment's `changed` signal happens during the
    /// layout pass, too late for the viewport: the scroll bar moved but the
    /// content stayed put until the next scroll (Ethan, 2026-11-06).
    fn queue_scroll_fix(self: &Rc<Self>) {
        {
            let Ok(mut st) = self.state.try_borrow_mut() else { return };
            if st.scroll_fix_queued || (st.anchor_from_bottom.is_none() && !st.pinned) {
                return;
            }
            st.scroll_fix_queued = true;
        }
        let weak = Rc::downgrade(self);
        self.scroll.add_tick_callback(move |widget, _| {
            let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break };
            let target = {
                let Ok(mut st) = this.state.try_borrow_mut() else { return glib::ControlFlow::Continue };
                st.scroll_fix_queued = false;
                let a = widget.vadjustment();
                match st.anchor_from_bottom {
                    // Older bubbles going in above: keep what is on screen.
                    Some(from_bottom) => Some(a.upper() - from_bottom),
                    None if st.pinned => Some(a.upper() - a.page_size()),
                    None => None,
                }
            };
            if let Some(v) = target {
                widget.vadjustment().set_value(v);
            }
            glib::ControlFlow::Break
        });
    }

    pub fn show_empty(&self) {
        self.state.borrow_mut().thread_id = None;
        self.widget.set_visible_child_name("empty");
    }

    fn set_header(&self, conv: &Conversation) {
        let name = &conv.display_name;
        let shown = if !name.is_empty() {
            name.as_str()
        } else if !conv.address.is_empty() {
            conv.address.as_str()
        } else {
            "Unknown"
        };
        self.contact.set_label(shown);
        self.address.set_label(if name.is_empty() || name == &conv.address { "" } else { &conv.address });
    }

    /// Show `conv`, doing as little as possible: new messages at the bottom
    /// are appended; anything else rebuilds (keeping the scroll position).
    pub fn sync(self: &Rc<Self>, conv: &Conversation, route: &dyn Fn(i64) -> Option<SendRoute>) {
        self.set_header(conv);
        // Texts and calls, in time order.
        let timeline = conv.timeline();
        let same = self.state.borrow().thread_id == Some(conv.thread_id)
            && self.widget.visible_child_name().as_deref() == Some("thread");
        let changed = self.state.borrow().thread_id != Some(conv.thread_id);
        {
            let mut st = self.state.borrow_mut();
            if changed {
                st.window = THREAD_WINDOW;
                st.thread_id = Some(conv.thread_id);
                st.reveal = None;
            }
            st.routes = conv.messages.iter().filter_map(|m| route(m.uid).map(|r| (m.uid, r))).collect();
        }
        if !same {
            let pending = {
                let mut st = self.state.borrow_mut();
                st.all = timeline.clone();
                st.pending_find.take().filter(|(tid, _, _)| *tid == conv.thread_id)
            };
            match pending {
                Some((_, needle, uid)) => self.start_find(&needle, Some(uid)),
                None if changed => self.find_bar.set_search_mode(false),
                None => {}
            }
            self.set_messages(false);
            return;
        }
        let (rebuild, new_msgs) = {
            let st = self.state.borrow();
            let current: HashSet<i64> = timeline.iter().map(|m| m.uid).filter(|u| *u != 0).collect();
            let new: Vec<SmsMessage> = timeline
                .iter()
                .filter(|m| m.uid != 0 && !st.rendered.contains(&m.uid) && m.date >= st.window_first_date)
                .cloned()
                .collect();
            let removed = st.rendered.iter().any(|u| !current.contains(u));
            let out_of_order = new.iter().any(|m| m.date < st.last_rendered_date);
            (removed || out_of_order, new)
        };
        self.state.borrow_mut().all = timeline;
        if rebuild {
            self.set_messages(true);
            self.refresh_find();
            return;
        }
        let hidden = {
            let st = self.state.borrow();
            let first = st.window_first_date;
            st.all.iter().filter(|m| m.date < first).count()
        };
        let has_button = self.state.borrow().earlier.clone();
        match has_button {
            Some(btn) => btn.set_label(&earlier_label(hidden)),
            None if hidden > 0 => {
                // The button can only go at the top; a rebuild places it.
                self.set_messages(true);
                return;
            }
            None => {}
        }
        let mut new_msgs = new_msgs;
        new_msgs.sort_by_key(|m| m.date);
        for m in &new_msgs {
            let r = self.state.borrow().routes.get(&m.uid).copied();
            self.append_message(m, r);
        }
        self.refresh_find();
    }

    /// Rebuild from `state.all`, rendering the newest `state.window`.
    fn set_messages(self: &Rc<Self>, keep_scroll: bool) {
        if !keep_scroll {
            super::perf::opening(self.state.borrow().all.len());
        }
        // A rebuild in place (a full picture arrived, older history) must
        // not lose "pinned to the bottom": clearing the box scrolls it.
        let was_pinned = self.state.borrow().pinned;
        self.state.borrow_mut().anchor_from_bottom = None;
        self.clear();
        let (shown, hidden, routes) = {
            let st = self.state.borrow();
            let shown: Vec<SmsMessage> = st.all[st.all.len().saturating_sub(st.window)..].to_vec();
            (shown.clone(), st.all.len() - shown.len(), st.routes.clone())
        };
        if hidden > 0 {
            let btn = gtk::Button::with_label(&earlier_label(hidden));
            btn.add_css_class("flat");
            btn.set_halign(gtk::Align::Center);
            btn.set_margin_top(8);
            let weak = Rc::downgrade(self);
            btn.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.show_earlier();
                }
            });
            self.messages_box.append(&btn);
            self.state.borrow_mut().earlier = Some(btn);
        }

        // Fresh opens build the newest few now and the rest in batches; an
        // in-place rebuild builds everything (its scroll is restored at once).
        let first = if keep_scroll { shown.len() } else { FIRST_BATCH.min(shown.len()) };
        let split = shown.len() - first;
        let today = today();
        let mut bubbles = Vec::new();
        for i in split..shown.len() {
            let m = &shown[i];
            if let Some(line) = time_line(i.checked_sub(1).map(|j| &shown[j]), m, today) {
                self.messages_box.append(&date_separator(&line));
            }
            let newest = i + 1 == shown.len();
            let b = Bubble::new(m, newest, routes.get(&m.uid).copied(), &self.actions);
            self.messages_box.append(&b.widget);
            bubbles.push(b);
        }
        self.rendered(shown[split..].iter().map(|m| m.uid).filter(|u| *u != 0).collect());
        let generation = {
            let mut st = self.state.borrow_mut();
            // All of the window counts as rendered (the rest is on its way),
            // so `sync` never appends one of them at the bottom.
            st.rendered = shown.iter().map(|m| m.uid).filter(|u| *u != 0).collect();
            st.last_rendered_date = shown.last().map_or(0, |m| m.date);
            st.window_first_date = shown.first().map_or(0, |m| m.date);
            st.bubbles = bubbles;
            st.generation += 1;
            st.generation
        };
        if split > 0 {
            // A fresh open stays transparent until every batch is in, then
            // appears at the bottom: no frame shows the chat shifting.
            self.scroll.set_opacity(0.0);
            self.state.borrow_mut().pinned = true;
            self.build_older(Rc::new(shown), routes, split, generation, !keep_scroll);
        }
        self.widget.set_visible_child_name("thread");
        if !keep_scroll {
            // With batches still to come, they reveal it when done.
            if split == 0 {
                super::perf::built();
                self.finish_open();
            }
        } else if self.scroll.opacity() == 0.0 && !self.state.borrow().revealing {
            // A rebuild cut short a fresh open's batches: show it now.
            self.reveal_at_bottom();
        } else if was_pinned {
            self.state.borrow_mut().pinned = true;
            let adj = self.scroll.vadjustment();
            glib::idle_add_local_once(move || adj.set_value(adj.upper() - adj.page_size()));
        }
        self.mark_highlight();
    }

    /// Add `shown[..upto]` above what is built, newest first, a batch per
    /// idle callback, until done or the chat is rebuilt.
    fn build_older(
        self: &Rc<Self>,
        shown: Rc<Vec<SmsMessage>>,
        routes: std::collections::HashMap<i64, SendRoute>,
        upto: usize,
        generation: u64,
        reveal_when_done: bool,
    ) {
        let weak = Rc::downgrade(self);
        let remaining = Cell::new(upto);
        glib::idle_add_local(move || {
            let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break };
            if this.state.borrow().generation != generation {
                return glib::ControlFlow::Break;
            }
            let k = remaining.get();
            let j = k.saturating_sub(BUILD_BATCH);
            {
                let adj = this.scroll.vadjustment();
                let mut st = this.state.borrow_mut();
                // At the bottom (or before the first layout) hold the
                // bottom; otherwise hold where the user scrolled to.
                st.anchor_from_bottom = Some(if st.pinned { 0.0 } else { adj.upper() - adj.value() });
            }
            let today = today();
            // Insert in order just below the "Show earlier" button (or at
            // the very top).
            let mut anchor: Option<gtk::Widget> = this.state.borrow().earlier.clone().map(|b| b.upcast());
            let mut put = |w: &gtk::Widget| {
                this.messages_box.insert_child_after(w, anchor.as_ref());
                anchor = Some(w.clone());
            };
            let mut batch = Vec::new();
            for i in j..k {
                let m = &shown[i];
                if let Some(line) = time_line(i.checked_sub(1).map(|x| &shown[x]), m, today) {
                    put(date_separator(&line).upcast_ref());
                }
                let b = Bubble::new(m, false, routes.get(&m.uid).copied(), &this.actions);
                put(b.widget.upcast_ref());
                batch.push(b);
            }
            this.rendered(shown[j..k].iter().map(|m| m.uid).filter(|u| *u != 0).collect());
            this.state.borrow_mut().bubbles.splice(0..0, batch);
            remaining.set(j);
            if j > 0 {
                return glib::ControlFlow::Continue;
            }
            if reveal_when_done {
                super::perf::built();
                this.finish_open();
            }
            // Let the last batch be laid out before letting go of the anchor.
            let weak = Rc::downgrade(&this);
            glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                if let Some(this) = weak.upgrade() {
                    let mut st = this.state.borrow_mut();
                    if st.generation == generation {
                        st.anchor_from_bottom = None;
                    }
                }
            });
            glib::ControlFlow::Break
        });
    }

    /// Full pictures arrived: swap only the bubbles whose attachments
    /// changed (a full rebuild re-laid out all 60, ~100 ms on the laptop).
    pub fn redraw(self: &Rc<Self>, conv: &Conversation, route: &dyn Fn(i64) -> Option<SendRoute>) {
        if self.state.borrow().thread_id != Some(conv.thread_id) {
            return;
        }
        {
            let mut st = self.state.borrow_mut();
            st.all = conv.timeline();
            st.routes = conv.messages.iter().filter_map(|m| route(m.uid).map(|r| (m.uid, r))).collect();
        }
        let changed: Vec<(usize, SmsMessage)> = {
            let st = self.state.borrow();
            st.bubbles
                .iter()
                .enumerate()
                .filter_map(|(i, b)| {
                    conv.messages
                        .iter()
                        .find(|m| m.uid == b.message.uid && (m.attachments != b.message.attachments || route(m.uid) != b.route))
                        .map(|m| (i, m.clone()))
                })
                .collect()
        };
        let pinned = self.state.borrow().pinned;
        for (i, m) in changed {
            let old = self.state.borrow().bubbles[i].clone();
            let fresh = Bubble::new(&m, old.revealer.reveals_child(), route(m.uid), &self.actions);
            self.messages_box.insert_child_after(&fresh.widget, Some(&old.widget));
            self.messages_box.remove(&old.widget);
            self.state.borrow_mut().bubbles[i] = fresh;
        }
        if pinned {
            let adj = self.scroll.vadjustment();
            glib::idle_add_local_once(move || adj.set_value(adj.upper() - adj.page_size()));
        }
        self.mark_highlight();
    }

    fn show_earlier(self: &Rc<Self>) {
        let adj = self.scroll.vadjustment();
        let from_bottom = adj.upper() - adj.value();
        {
            let mut st = self.state.borrow_mut();
            st.window += THREAD_WINDOW_STEP;
            st.pinned = false;
        }
        self.set_messages(true);
        glib::idle_add_local_once(move || adj.set_value((adj.upper() - from_bottom).max(0.0)));
    }

    /// Open at the newest message with no visible scroll: transparent until
    /// laid out, then shown already at the bottom.
    fn reveal_at_bottom(self: &Rc<Self>) {
        {
            let mut st = self.state.borrow_mut();
            st.pinned = true;
            if st.revealing {
                return;
            }
            st.revealing = true;
        }
        self.scroll.set_opacity(0.0);
        let frames = Cell::new(0);
        let weak = Rc::downgrade(self);
        self.scroll.add_tick_callback(move |widget, _| {
            frames.set(frames.get() + 1);
            if frames.get() < 2 {
                return glib::ControlFlow::Continue; // the first tick is before layout
            }
            let adj = widget.vadjustment();
            adj.set_value(adj.upper() - adj.page_size());
            widget.set_opacity(1.0);
            super::perf::revealed();
            if let Some(this) = weak.upgrade() {
                this.state.borrow_mut().revealing = false;
            }
            glib::ControlFlow::Break
        });
    }

    /// A fresh build is done: show the match being revealed, else the bottom.
    fn finish_open(self: &Rc<Self>) {
        let reveal = self.state.borrow_mut().reveal.take();
        match reveal {
            Some(uid) => self.reveal_match(uid),
            None => self.reveal_at_bottom(),
        }
    }

    /// As [`Self::reveal_at_bottom`], but centred on message `uid`.
    fn reveal_match(self: &Rc<Self>, uid: i64) {
        self.scroll.set_opacity(0.0);
        self.state.borrow_mut().revealing = true;
        let frames = Cell::new(0);
        let weak = Rc::downgrade(self);
        self.scroll.add_tick_callback(move |widget, _| {
            frames.set(frames.get() + 1);
            if frames.get() < 2 {
                return glib::ControlFlow::Continue; // the first tick is before layout
            }
            if let Some(this) = weak.upgrade() {
                this.state.borrow_mut().revealing = false;
                this.scroll_to(uid);
            }
            widget.set_opacity(1.0);
            super::perf::revealed();
            glib::ControlFlow::Break
        });
    }

    /// Centre a built bubble and outline it.
    fn scroll_to(&self, uid: i64) {
        let widget = {
            let mut st = self.state.borrow_mut();
            st.anchor_from_bottom = None;
            st.pinned = false;
            st.highlighted = Some(uid);
            st.bubbles.iter().find(|b| b.message.uid == uid).map(|b| b.widget.clone())
        };
        self.mark_highlight();
        let Some(widget) = widget else { return };
        #[allow(deprecated)] // compute_point is GTK 4.12; the app supports 4.10
        let Some((_, y)) = widget.translate_coordinates(&self.messages_box, 0.0, 0.0) else { return };
        let adj = self.scroll.vadjustment();
        let h = f64::from(widget.height());
        let top = (adj.upper() - adj.page_size()).max(adj.lower());
        adj.set_value((y - (adj.page_size() - h) / 2.0).clamp(adj.lower(), top));
    }

    /// Outline the highlighted match (bubbles are rebuilt often).
    fn mark_highlight(&self) {
        let st = self.state.borrow();
        let needle = (!st.find_needle.is_empty()).then_some(st.find_needle.as_str());
        for b in &st.bubbles {
            if needle.is_some() && st.highlighted == Some(b.message.uid) {
                b.set_search_hit(needle); // again: the text may have changed
            } else if b.widget.has_css_class("search-hit") {
                b.set_search_hit(None);
            }
        }
    }

    /// The next chat to open came from a search match: open it on `uid`.
    pub fn find_on_open(&self, thread_id: i64, needle: &str, uid: i64) {
        self.state.borrow_mut().pending_find = Some((thread_id, needle.to_string(), uid));
    }

    /// Open the find bar on `needle`, at match `at` (else the newest), and
    /// widen the window so the next build includes it.
    fn start_find(&self, needle: &str, at: Option<i64>) {
        {
            let mut st = self.state.borrow_mut();
            st.find_needle = needle.to_string();
            st.find_matches = search::matching_uids(&st.all, needle);
            st.find_current = at
                .and_then(|u| st.find_matches.iter().position(|m| *m == u))
                .unwrap_or(st.find_matches.len().saturating_sub(1));
            if let Some(&uid) = st.find_matches.get(st.find_current) {
                if let Some(idx) = st.all.iter().position(|m| m.uid == uid) {
                    st.window = st.window.max(st.all.len() - idx + REVEAL_CONTEXT);
                }
                st.reveal = Some(uid);
                st.highlighted = Some(uid);
            }
        }
        // The entry's own change signal comes later and finds nothing new.
        self.find_entry.set_text(needle);
        self.find_bar.set_search_mode(true);
        self.update_find_label();
    }

    fn find_changed(self: &Rc<Self>, text: &str) {
        {
            let mut st = self.state.borrow_mut();
            if st.find_needle == text {
                return;
            }
            st.find_needle = text.to_string();
            st.find_matches = search::matching_uids(&st.all, text);
            st.find_current = st.find_matches.len().saturating_sub(1);
        }
        self.show_find_current();
    }

    /// -1 = an earlier match, 1 = a later one. Stops at the ends.
    fn find_step(self: &Rc<Self>, step: isize) {
        {
            let mut st = self.state.borrow_mut();
            let n = st.find_matches.len();
            if n == 0 {
                return;
            }
            let next = st.find_current.saturating_add_signed(step).min(n - 1);
            if next == st.find_current {
                return;
            }
            st.find_current = next;
        }
        self.show_find_current();
    }

    fn show_find_current(self: &Rc<Self>) {
        self.update_find_label();
        let uid = {
            let st = self.state.borrow();
            st.find_matches.get(st.find_current).copied()
        };
        match uid {
            Some(uid) => self.reveal(uid),
            None => {
                self.state.borrow_mut().highlighted = None;
                self.mark_highlight();
            }
        }
    }

    /// Bring message `uid` on screen: scroll if it is built, else widen the
    /// window and rebuild around it.
    fn reveal(self: &Rc<Self>, uid: i64) {
        let (built, building, idx) = {
            let st = self.state.borrow();
            (
                st.bubbles.iter().any(|b| b.message.uid == uid),
                st.rendered.contains(&uid),
                st.all.iter().position(|m| m.uid == uid),
            )
        };
        if built {
            self.scroll_to(uid);
            return;
        }
        let mut st = self.state.borrow_mut();
        st.reveal = Some(uid);
        st.highlighted = Some(uid);
        if building {
            return; // the batches still being built reveal it when done
        }
        let Some(idx) = idx else { return };
        st.window = st.window.max(st.all.len() - idx + REVEAL_CONTEXT);
        drop(st);
        self.set_messages(false);
    }

    /// New or removed messages: recount the matches, keeping the current one.
    fn refresh_find(&self) {
        {
            let mut st = self.state.borrow_mut();
            if st.find_needle.is_empty() {
                return;
            }
            let current = st.find_matches.get(st.find_current).copied();
            st.find_matches = search::matching_uids(&st.all, &st.find_needle);
            st.find_current = current
                .and_then(|u| st.find_matches.iter().position(|m| *m == u))
                .unwrap_or(st.find_matches.len().saturating_sub(1));
        }
        self.update_find_label();
    }

    fn update_find_label(&self) {
        let st = self.state.borrow();
        let text = match st.find_matches.len() {
            _ if st.find_needle.trim().is_empty() => String::new(),
            0 => "No matches".into(),
            n => format!("{} of {n}", st.find_current + 1),
        };
        self.find_label.set_label(&text);
    }

    /// The find bar closed.
    fn end_find(&self) {
        {
            let mut st = self.state.borrow_mut();
            st.find_needle.clear();
            st.find_matches.clear();
            st.highlighted = None;
            st.reveal = None;
        }
        self.mark_highlight();
        self.find_entry.set_text("");
        self.update_find_label();
    }

    fn append_message(self: &Rc<Self>, m: &SmsMessage, route: Option<SendRoute>) {
        // A new message wins over holding the position for older ones.
        self.state.borrow_mut().anchor_from_bottom = None;
        let prev = self.state.borrow().bubbles.last().cloned();
        // Only the newest bubble shows its time.
        if let Some(prev) = &prev {
            prev.set_timestamp_visible(false);
        }
        if let Some(line) = time_line(prev.as_ref().map(|b| &b.message), m, today()) {
            self.messages_box.append(&date_separator(&line));
        }
        let b = Bubble::new(m, true, route, &self.actions);
        self.messages_box.append(&b.widget);
        self.rendered(vec![m.uid]);
        let mut st = self.state.borrow_mut();
        st.bubbles.push(b);
        if m.uid != 0 {
            st.rendered.insert(m.uid);
        }
        st.last_rendered_date = st.last_rendered_date.max(m.date);
        st.pinned = true;
        let adj = self.scroll.vadjustment();
        glib::idle_add_local_once(move || adj.set_value(adj.upper()));
    }

    fn clear(&self) {
        let mut st = self.state.borrow_mut();
        st.earlier = None;
        st.bubbles.clear();
        while let Some(child) = self.messages_box.first_child() {
            self.messages_box.remove(&child);
        }
    }
}

/// Decoded pictures held: (count, approximate bytes).
pub fn image_cache_stats() -> (usize, usize) {
    IMAGES.with_borrow(|c| {
        let bytes = c.textures.iter().map(|(_, t)| t.width() as usize * t.height() as usize * 4).sum();
        (c.textures.len(), bytes)
    })
}

type SendHandler = Box<dyn Fn(i64, &str, Option<&Path>) -> bool>;
type ZoomHandler = Box<dyn Fn(f64)>;
type RenderedHandler = Rc<dyn Fn(i64, Vec<i64>)>;

const IMAGE_FILE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "tif", "heic", "heif"];

/// The first existing image file in a pasted `text/uri-list` (or plain paths).
pub fn image_path_from_uri_list(text: &str) -> Option<PathBuf> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).find_map(|line| {
        let path = if line.starts_with("file://") { gio::File::for_uri(line).path()? } else { PathBuf::from(line) };
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        (path.is_file() && IMAGE_FILE_EXTS.contains(&ext.as_str())).then_some(path)
    })
}

fn earlier_label(hidden: usize) -> String {
    format!("Show earlier messages ({hidden})")
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_sizes_match_python() {
        // Big photo: fit inside 380×500.
        assert_eq!(scaled_image_size(4000, 3000, true), (380, 285));
        // Small full image: natural size.
        assert_eq!(scaled_image_size(300, 200, true), (300, 200));
        // 100 px thumbnail: scaled up to 220 on the long side.
        assert_eq!(scaled_image_size(100, 75, false), (220, 165));
        // Tiny thumbnail: upscale capped at 2.2×.
        assert_eq!(scaled_image_size(50, 50, false), (110, 110));
        assert_eq!(image_display(100, 75, true), (220, 165, true));
        assert!(!image_display(1000, 750, true).2);
    }

    #[test]
    fn time_lines_only_at_new_days_and_long_gaps() {
        use chrono::{Local, NaiveDate, TimeZone};
        let at = |y, mo, d, h, mi| {
            let ts = Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap();
            SmsMessage { date: ts.timestamp_millis(), ..Default::default() }
        };
        let today = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        let a = at(2026, 11, 3, 11, 9);
        let b = at(2026, 11, 3, 11, 40);
        let c = at(2026, 11, 3, 13, 5);
        let d = at(2026, 11, 5, 9, 0);
        assert_eq!(time_line(None, &a, today).as_deref(), Some("Tuesday · 11:09 AM"));
        assert_eq!(time_line(Some(&a), &b, today), None);
        assert_eq!(time_line(Some(&b), &c, today).as_deref(), Some("1:05 PM"));
        assert_eq!(time_line(Some(&c), &d, today).as_deref(), Some("Today · 9:00 AM"));
        let old = at(2026, 9, 1, 8, 0);
        assert_eq!(time_line(None, &old, today).as_deref(), Some("Tuesday, September 1 · 8:00 AM"));
        let older = at(2025, 12, 31, 23, 59);
        assert_eq!(time_line(None, &older, today).as_deref(), Some("Wednesday, December 31, 2025 · 11:59 PM"));
        assert_eq!(day_label(NaiveDate::from_ymd_opt(2026, 11, 4).unwrap(), today), "Yesterday");
    }

    #[test]
    fn links_become_markup() {
        let (m, links) = message_markup("see example.com/a, ok & <b>");
        assert!(links);
        assert_eq!(m, "see <a href=\"https://example.com/a\">example.com/a</a>, ok &amp; &lt;b&gt;");
        let (m, links) = message_markup("no links <here>");
        assert!(!links);
        assert_eq!(m, "no links &lt;here&gt;");
    }

    #[test]
    fn names_are_sanitized_as_python() {
        assert_eq!(sanitize_name("a b/c.jpg"), "a_b_c.jpg");
        assert_eq!(sanitize_name("..__"), "attachment");
        let att = Attachment { file_name: "IMG 1".into(), mime_type: "image/png".into(), ..Default::default() };
        assert_eq!(attachment_name(&att), "IMG_1.png");
    }
}
