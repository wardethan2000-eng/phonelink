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

pub const THREAD_WINDOW: usize = 60;
pub const THREAD_WINDOW_STEP: usize = 100;
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

/// The inline file: a `file://` URI, a path, or base64 data written to
/// `$TMPDIR/phonelink/message_attachments` (same names as Python).
fn attachment_local_path(att: &Attachment) -> Option<PathBuf> {
    let payload = att.payload.as_str();
    if payload.is_empty() {
        return None;
    }
    if payload.starts_with("file://") {
        return gio::File::for_uri(payload).path().filter(|p| p.is_file());
    }
    if Path::new(payload).is_file() {
        return Some(PathBuf::from(payload));
    }
    let dir = std::env::temp_dir().join("phonelink").join("message_attachments");
    std::fs::create_dir_all(&dir).ok()?;
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
    std::fs::write(&target, &data).ok()?;
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
    let Some((_, w, h)) = Pixbuf::file_info(path) else { return Decoded { measured: None, pixels: None } };
    if w <= 0 || h <= 0 {
        return Decoded { measured: None, pixels: None };
    }
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
        while self.textures.len() > IMAGE_CACHE_MAX {
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
}

struct Bubble {
    widget: gtk::Box,
    revealer: gtk::Revealer,
    message: SmsMessage,
}

impl Bubble {
    fn new(msg: &SmsMessage, show_time: bool, route: Option<SendRoute>, actions: &BubbleActions) -> Rc<Self> {
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
        let label = format!("{} {}", if sent { "Sent" } else { "Received" }, clock(msg.timestamp()));
        let time = gtk::Label::new(Some(&label));
        time.set_halign(if sent { gtk::Align::End } else { gtk::Align::Start });
        time.add_css_class("dim-label");
        time.add_css_class("caption");
        time.add_css_class("message-timestamp");
        time.set_margin_start(if sent { 0 } else { 12 });
        time.set_margin_end(if sent { 12 } else { 0 });
        revealer.set_child(Some(&time));
        widget.append(&revealer);
        // How a text sent from this app left it: always shown, so a fallback
        // to SMS is never hidden.
        if let Some(r) = route {
            let tag = gtk::Label::new(Some(r.label()));
            tag.set_halign(gtk::Align::End);
            tag.add_css_class("dim-label");
            tag.add_css_class("caption");
            tag.add_css_class("message-timestamp");
            tag.set_margin_end(12);
            widget.append(&tag);
        }

        let bubble = Rc::new(Self { widget, revealer, message: msg.clone() });
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

    fn set_timestamp_visible(&self, visible: bool) {
        self.revealer.set_reveal_child(visible);
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
        (FileAction::CopyImage, Some(p)) => match gdk::Texture::from_filename(p) {
            Ok(tex) => widget.clipboard().set_texture(&tex),
            Err(e) => log::warn!("copy image: {e}"),
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
}

pub struct ThreadView {
    pub widget: gtk::Stack,
    contact: gtk::Label,
    address: gtk::Label,
    scroll: gtk::ScrolledWindow,
    messages_box: gtk::Box,
    pub entry: gtk::Entry,
    pub send_button: gtk::Button,
    actions: BubbleActions,
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

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        scroll.set_propagate_natural_width(false);
        let messages_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        messages_box.set_margin_bottom(8);
        scroll.set_child(Some(&messages_box));
        thread_box.append(&scroll);

        thread_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let compose = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        compose.set_margin_start(12);
        compose.set_margin_end(12);
        compose.set_margin_top(8);
        compose.set_margin_bottom(8);
        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some("Type a message…"));
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
            entry,
            send_button,
            actions,
            state: RefCell::new(State { window: THREAD_WINDOW, pinned: true, ..Default::default() }),
        });

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
        adj.connect_changed(move |a| {
            if let Some(this) = weak.upgrade()
                && this.state.try_borrow().is_ok_and(|st| st.pinned)
            {
                a.set_value(a.upper() - a.page_size());
            }
        });
        this
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
        let same = self.state.borrow().thread_id == Some(conv.thread_id)
            && self.widget.visible_child_name().as_deref() == Some("thread");
        {
            let mut st = self.state.borrow_mut();
            if st.thread_id != Some(conv.thread_id) {
                st.window = THREAD_WINDOW;
                st.thread_id = Some(conv.thread_id);
            }
            st.routes = conv.messages.iter().filter_map(|m| route(m.uid).map(|r| (m.uid, r))).collect();
        }
        if !same {
            self.state.borrow_mut().all = sorted(&conv.messages);
            self.set_messages(false);
            return;
        }
        let (rebuild, new_msgs) = {
            let st = self.state.borrow();
            let current: HashSet<i64> = conv.messages.iter().map(|m| m.uid).filter(|u| *u != 0).collect();
            let new: Vec<SmsMessage> = conv
                .messages
                .iter()
                .filter(|m| m.uid != 0 && !st.rendered.contains(&m.uid) && m.date >= st.window_first_date)
                .cloned()
                .collect();
            let removed = st.rendered.iter().any(|u| !current.contains(u));
            let out_of_order = new.iter().any(|m| m.date < st.last_rendered_date);
            (removed || out_of_order, new)
        };
        self.state.borrow_mut().all = sorted(&conv.messages);
        if rebuild {
            self.set_messages(true);
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
    }

    /// Rebuild from `state.all`, rendering the newest `state.window`.
    fn set_messages(self: &Rc<Self>, keep_scroll: bool) {
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

        let mut bubbles = Vec::new();
        let today = today();
        for (i, m) in shown.iter().enumerate() {
            if let Some(line) = time_line(i.checked_sub(1).map(|j| &shown[j]), m, today) {
                self.messages_box.append(&date_separator(&line));
            }
            let newest = i + 1 == shown.len();
            let b = Bubble::new(m, newest, routes.get(&m.uid).copied(), &self.actions);
            self.messages_box.append(&b.widget);
            bubbles.push(b);
        }
        {
            let mut st = self.state.borrow_mut();
            st.rendered = shown.iter().map(|m| m.uid).filter(|u| *u != 0).collect();
            st.last_rendered_date = shown.last().map_or(0, |m| m.date);
            st.window_first_date = shown.first().map_or(0, |m| m.date);
            st.bubbles = bubbles;
        }
        self.widget.set_visible_child_name("thread");
        if !keep_scroll {
            self.reveal_at_bottom();
        }
    }

    /// Redraw the open conversation in place (a full picture arrived),
    /// keeping the scroll position.
    pub fn redraw(self: &Rc<Self>, conv: &Conversation, route: &dyn Fn(i64) -> Option<SendRoute>) {
        if self.state.borrow().thread_id != Some(conv.thread_id) {
            return;
        }
        {
            let mut st = self.state.borrow_mut();
            st.all = sorted(&conv.messages);
            st.routes = conv.messages.iter().filter_map(|m| route(m.uid).map(|r| (m.uid, r))).collect();
        }
        self.set_messages(true);
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
            if let Some(this) = weak.upgrade() {
                this.state.borrow_mut().revealing = false;
            }
            glib::ControlFlow::Break
        });
    }

    fn append_message(self: &Rc<Self>, m: &SmsMessage, route: Option<SendRoute>) {
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

fn earlier_label(hidden: usize) -> String {
    format!("Show earlier messages ({hidden})")
}

fn sorted(messages: &[SmsMessage]) -> Vec<SmsMessage> {
    let mut v = messages.to_vec();
    v.sort_by_key(|m| m.date);
    v
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
