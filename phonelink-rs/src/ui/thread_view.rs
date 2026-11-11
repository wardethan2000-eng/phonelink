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
use std::collections::{HashMap, HashSet, VecDeque};
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
use crate::core::rcs::{self, Extras};
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
/// A preview whose full picture is on its way already takes the room the
/// full one will (a phone photo fills the frame), so the chat does not
/// shift when it arrives.
fn image_display(width: i32, height: i32, can_fetch_full: bool) -> (i32, i32, bool) {
    let is_full = width.max(height) > ATTACHMENT_THUMBNAIL_MAX;
    let waiting = !is_full && can_fetch_full;
    let (w, h) = if waiting { full_frame_size(width, height) } else { scaled_image_size(width, height, is_full) };
    (w, h, waiting)
}

/// The largest size within the full-picture frame with this aspect ratio.
fn full_frame_size(width: i32, height: i32) -> (i32, i32) {
    let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let fit = (FULL_IMAGE_MAX_W / w).min(FULL_IMAGE_MAX_H / h);
    (((w * fit).round() as i32).max(1), ((h * fit).round() as i32).max(1))
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
    let dir =
        private_tmp::dir(Some("message_attachments")).inspect_err(|e| log::warn!("attachment folder: {e}")).ok()?;
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
    let Some((w, h)) = super::decodable_image(path) else {
        return Decoded { measured: None, pixels: None };
    };
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
    /// `(uid, emoji)`: react to a message (the same emoji again takes it
    /// back). Only bubbles Google Messages matched offer it.
    pub react: Rc<dyn Fn(i64, String)>,
    /// Reply to the message `uid` (through Google Messages).
    pub reply: Rc<dyn Fn(i64)>,
    /// Does the messages helper do this (`react`, `reply`...)?
    pub allowed: Rc<dyn Fn(&str) -> bool>,
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
    /// "Replying to …" above the bubble.
    quote_slot: gtk::Box,
    /// Reaction chips under it.
    reactions_slot: gtk::Box,
    /// What Google Messages says about it (None: not matched).
    extras: SharedExtras,
}

/// A bubble's Google Messages share, also read by its pictures' menus.
type SharedExtras = Rc<RefCell<Option<Extras>>>;

impl Bubble {
    fn new(msg: &SmsMessage, show_time: bool, route: Option<SendRoute>, actions: &BubbleActions) -> Rc<Self> {
        let sent = msg.is_sent();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 1);
        widget.set_halign(if sent { gtk::Align::End } else { gtk::Align::Start });
        widget.set_margin_top(1);
        widget.set_margin_bottom(1);
        widget.set_margin_start(if sent { 48 } else { 12 });
        widget.set_margin_end(if sent { 12 } else { 48 });
        let side = if sent { gtk::Align::End } else { gtk::Align::Start };
        let quote_slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        quote_slot.set_halign(side);
        quote_slot.set_margin_start(12);
        quote_slot.set_margin_end(12);
        quote_slot.set_visible(false);
        widget.append(&quote_slot);

        let extras = SharedExtras::default();
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 3);
        frame.set_margin_top(2);
        frame.set_margin_bottom(2);
        frame.set_margin_start(12);
        frame.set_margin_end(12);
        frame.add_css_class(if sent { "message-bubble-sent" } else { "message-bubble-received" });
        // Its own width, not a wider quote's above it.
        frame.set_halign(side);

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
            match image_widget(att, sent, actions, &extras, msg.uid) {
                Some(w) => widget.append(&w),
                None => frame.append(&file_chip(att, actions)),
            }
        }
        if frame.first_child().is_some() {
            widget.append(&frame);
        }
        let reactions_slot = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        reactions_slot.set_halign(side);
        reactions_slot.set_margin_start(16);
        reactions_slot.set_margin_end(16);
        reactions_slot.set_visible(false);
        widget.append(&reactions_slot);

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
        let bubble = Rc::new(Self {
            widget,
            body,
            revealer,
            route_tag,
            message: msg.clone(),
            route,
            quote_slot,
            reactions_slot,
            extras,
        });
        bubble.set_timestamp_visible(show_time);
        // Right-click (or long-press) a text: react, reply, copy. Capture
        // phase, so the selectable label's own menu doesn't take it.
        let menu_click = gtk::GestureClick::new();
        menu_click.set_button(gdk::BUTTON_SECONDARY);
        menu_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (weak, acts, at) = (Rc::downgrade(&bubble), actions.clone(), frame.clone());
        menu_click.connect_pressed(move |g, _, x, y| {
            if let Some(b) = weak.upgrade()
                && !on_file_chip(&at, x, y)
            {
                g.set_state(gtk::EventSequenceState::Claimed);
                b.menu(at.upcast_ref(), x, y, &acts);
            }
        });
        frame.add_controller(menu_click);
        let hold = gtk::GestureLongPress::new();
        let (weak, acts, at) = (Rc::downgrade(&bubble), actions.clone(), frame.clone());
        hold.connect_pressed(move |g, x, y| {
            if let Some(b) = weak.upgrade()
                && !on_file_chip(&at, x, y)
            {
                g.set_state(gtk::EventSequenceState::Claimed);
                b.menu(at.upcast_ref(), x, y, &acts);
            }
        });
        frame.add_controller(hold);
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

    /// Show Google Messages' reactions and quote (None: no longer matched).
    fn set_extras(&self, extras: Option<&Extras>, actions: &BubbleActions, jump: &Rc<dyn Fn(i64)>) {
        if self.extras.borrow().as_ref() == extras {
            return;
        }
        *self.extras.borrow_mut() = extras.cloned();
        clear_box(&self.quote_slot);
        clear_box(&self.reactions_slot);
        let Some(e) = extras else {
            self.quote_slot.set_visible(false);
            self.reactions_slot.set_visible(false);
            return;
        };
        if let Some(q) = &e.quote {
            let quote = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            quote.add_css_class("message-quote");
            let icon = gtk::Image::from_icon_name("mail-reply-sender-symbolic");
            icon.add_css_class("dim-label");
            quote.append(&icon);
            if !q.author.is_empty() {
                let who = gtk::Label::new(Some(&q.author));
                who.add_css_class("caption-heading");
                quote.append(&who);
            }
            let text = gtk::Label::new(Some(if q.text.is_empty() { "an earlier message" } else { &q.text }));
            text.add_css_class("caption");
            text.add_css_class("dim-label");
            text.set_ellipsize(pango::EllipsizeMode::End);
            text.set_max_width_chars(40);
            text.set_single_line_mode(true);
            quote.append(&text);
            if let Some(uid) = q.uid {
                quote.set_tooltip_text(Some("Show the message this replies to"));
                quote.set_cursor_from_name(Some("pointer"));
                let click = gtk::GestureClick::new();
                let jump = jump.clone();
                click.connect_released(move |_, _, _, _| jump(uid));
                quote.add_controller(click);
            }
            self.quote_slot.append(&quote);
        }
        self.quote_slot.set_visible(e.quote.is_some());
        for chip in &e.reactions {
            let label = if chip.count > 1 { format!("{} {}", chip.emoji, chip.count) } else { chip.emoji.clone() };
            let b = gtk::Button::with_label(&label);
            b.add_css_class("reaction-chip");
            if chip.mine {
                b.add_css_class("reaction-mine");
            }
            if !chip.who.is_empty() {
                b.set_tooltip_text(Some(&chip.who));
            }
            let (react, uid, emoji) = (actions.react.clone(), self.message.uid, chip.emoji.clone());
            b.connect_clicked(move |_| react(uid, emoji.clone()));
            self.reactions_slot.append(&b);
        }
        self.reactions_slot.set_visible(!e.reactions.is_empty());
    }

    /// The right-click menu: reactions and Reply when Google Messages knows
    /// the message, Copy always.
    fn menu(&self, anchor: &gtk::Widget, x: f64, y: f64, actions: &BubbleActions) {
        let popover = gtk::Popover::new();
        popover.set_has_arrow(false);
        popover.set_parent(anchor);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let uid = self.message.uid;
        if let Some(e) = self.extras.borrow().as_ref().filter(|e| !e.msg_id.is_empty()) {
            react_items(&menu, &popover, e, uid, actions);
        }
        if !self.message.body.is_empty() {
            let copy = gtk::Button::with_label("Copy text");
            copy.add_css_class("flat");
            let (text, pop) = (self.message.body.clone(), popover.clone());
            copy.connect_clicked(move |b| {
                b.clipboard().set_text(&text);
                pop.popdown();
            });
            menu.append(&copy);
        }
        for b in std::iter::successors(menu.first_child(), |w| w.next_sibling()) {
            if let Some(l) = b.downcast_ref::<gtk::Button>().and_then(|b| b.child()).and_downcast::<gtk::Label>() {
                l.set_xalign(0.0);
            }
        }
        if menu.first_child().is_none() {
            popover.unparent();
            return;
        }
        popover.set_child(Some(&menu));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
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

/// A file chip has its own right-click menu.
fn on_file_chip(frame: &gtk::Box, x: f64, y: f64) -> bool {
    let mut w = frame.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(widget) = w {
        if widget.has_css_class("message-file-chip") {
            return true;
        }
        if widget == *frame.upcast_ref::<gtk::Widget>() {
            return false;
        }
        w = widget.parent();
    }
    false
}

fn clear_box(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
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

fn run_action(
    action: FileAction,
    widget: &gtk::Widget,
    path: Option<&Path>,
    att: &Attachment,
    actions: &BubbleActions,
) {
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
/// The reactions row and Reply, at the top of a message's menu.
fn react_items(menu: &gtk::Box, popover: &gtk::Popover, e: &Extras, uid: i64, actions: &BubbleActions) {
    if (actions.allowed)("react") {
        react_row(menu, popover, e, uid, actions);
    }
    if !((actions.allowed)("reply") && (actions.allowed)("send_text")) {
        return;
    }
    let reply = gtk::Button::with_label("Reply");
    reply.add_css_class("flat");
    if let Some(l) = reply.child().and_downcast::<gtk::Label>() {
        l.set_xalign(0.0);
    }
    let (act, pop) = (actions.reply.clone(), popover.clone());
    reply.connect_clicked(move |_| {
        pop.popdown();
        act(uid);
    });
    menu.append(&reply);
}

/// The quick reactions, in a row.
fn react_row(menu: &gtk::Box, popover: &gtk::Popover, e: &Extras, uid: i64, actions: &BubbleActions) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    for emoji in rcs::QUICK_REACTIONS {
        let b = gtk::Button::with_label(emoji);
        b.add_css_class("flat");
        b.add_css_class("reaction-pick");
        if e.reactions.iter().any(|r| r.mine && r.emoji == emoji) {
            b.add_css_class("reaction-mine");
            b.set_tooltip_text(Some("Take back"));
        }
        let (react, pop) = (actions.react.clone(), popover.clone());
        b.connect_clicked(move |_| {
            pop.popdown();
            react(uid, emoji.to_string());
        });
        row.append(&b);
    }
    menu.append(&row);
}

/// `react`: the message's Google Messages share and uid, to put reactions
/// and Reply above the file actions.
fn context_menu(
    anchor: &gtk::Widget,
    x: f64,
    y: f64,
    items: &[(&'static str, FileAction)],
    run: Rc<dyn Fn(FileAction)>,
    react: Option<(&Extras, i64, &BubbleActions)>,
) {
    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.set_parent(anchor);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    if let Some((e, uid, actions)) = react {
        react_items(&menu, &popover, e, uid, actions);
    }
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
fn image_widget(
    att: &Attachment,
    sent: bool,
    actions: &BubbleActions,
    extras: &SharedExtras,
    uid: i64,
) -> Option<gtk::Widget> {
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
        let (extras, acts) = (extras.clone(), actions.clone());
        right.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            let e = extras.borrow().clone();
            context_menu(
                &anchor,
                x,
                y,
                &[("Open", FileAction::Open), ("Copy image", FileAction::CopyImage), ("Save image…", FileAction::Save)],
                run.clone(),
                e.as_ref().filter(|e| !e.msg_id.is_empty()).map(|e| (e, uid, &acts)),
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
    // The size now (only the file's header is read), so the chat is laid out
    // in full before it appears; sized later, every picture nudged it.
    let measured = known.unwrap_or_else(|| {
        let m = super::decodable_image(&path);
        IMAGES.with_borrow_mut(|c| c.sizes.insert(key.clone(), m));
        m
    });
    apply_size(measured);
    match texture {
        Some(tex) => picture.set_paintable(Some(&tex)),
        None => {
            let picture = picture.clone();
            glib::spawn_future_local(async move {
                let p = path.clone();
                let Ok(decoded) = gio::spawn_blocking(move || decode(&p, can_fetch_full)).await else {
                    return;
                };
                if decoded.measured != measured {
                    IMAGES.with_borrow_mut(|c| c.sizes.insert(key.clone(), decoded.measured));
                    apply_size(decoded.measured);
                }
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
    // Google Messages files of any kind land in the cache once fetched.
    let from_google = crate::core::gm::gm_media(att).is_some();
    let path = if is_image(att) || from_google { existing_full_in(&full_cache_dir(), att) } else { None }
        .or_else(|| attachment_local_path(att));
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("message-file-chip");
    row.set_cursor_from_name(Some("pointer"));
    let icon = gtk::Image::from_icon_name(&super::icons::resolve(file_icon(&att.mime_type)));
    // 16 px: the stylesheet sizes it (Settings > Size).
    row.append(&icon);
    let shown_name = match (att.file_name.is_empty(), att.mime_type.split('/').next()) {
        (true, Some("audio")) => "Voice message".to_string(),
        (true, Some("video")) => "Video".to_string(),
        _ => attachment_name(att),
    };
    let label = gtk::Label::new(Some(&shown_name));
    label.set_ellipsize(pango::EllipsizeMode::Middle);
    label.set_max_width_chars(32);
    label.set_halign(gtk::Align::Start);
    row.append(&label);
    if path.is_none() && (att.part_id != 0 || from_google) {
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
            context_menu(&anchor, x, y, items, run.clone(), None);
        });
    }
    row.add_controller(right);
    // A voice message or video that is here plays in the chat.
    match (path, att.mime_type.split('/').next()) {
        (Some(p), Some(kind @ ("audio" | "video"))) => {
            let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
            column.add_css_class("message-media");
            if kind == "video" {
                let video = gtk::Video::for_filename(Some(&p));
                video.set_autoplay(false);
                video.set_size_request(320, 200);
                column.append(&video);
            } else {
                let controls = gtk::MediaControls::new(Some(&gtk::MediaFile::for_filename(&p)));
                controls.set_size_request(260, -1);
                column.append(&controls);
            }
            column.append(&row);
            column.upcast()
        }
        _ => row.upcast(),
    }
}

/// "Save as" with the attachment's name filled in.
fn save_as(widget: &gtk::Widget, path: &Path, file_name: &str) {
    let dialog = gtk::FileDialog::new();
    dialog.set_title("Save");
    dialog.set_initial_name(Some(file_name));
    let root = widget.root().and_downcast::<gtk::Window>();
    let path = path.to_path_buf();
    dialog.save(root.as_ref(), gio::Cancellable::NONE, move |res| {
        let Some(target) = res.ok().and_then(|f| f.path()) else {
            return;
        };
        if let Err(e) = std::fs::copy(&path, &target) {
            log::warn!("save attachment: {e}");
        }
    });
}

/// A little more room where the other side starts talking (or, in a
/// group, someone else); runs from one person stay close together.
fn mark_turn(b: &Bubble, prev: Option<&SmsMessage>) {
    let Some(prev) = prev else { return };
    if prev.is_sent() != b.message.is_sent() || (!prev.is_sent() && prev.address != b.message.address) {
        b.widget.set_margin_top(super::scale::px(10));
    }
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

/// `c` as one capital letter (not "ß" → "SS").
fn capital(c: char) -> Option<String> {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) if u != c => Some(u.to_string()),
        _ => None,
    }
}

// ── the thread view ────────────────────────────────────────────────

/// A message typed in a chat but not sent.
struct Draft {
    text: String,
    files: Vec<PathBuf>,
}

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
    /// Google Messages' reactions and quotes, by uid.
    extras: HashMap<i64, Extras>,
    /// The next send replies to this Google Messages id.
    replying: Option<String>,
    /// The phone has older messages than these (Google Messages), and
    /// whether a page of them is on its way.
    more_on_phone: bool,
    loading_older: bool,
}

pub struct ThreadView {
    pub widget: gtk::Stack,
    contact: gtk::Label,
    address: gtk::Label,
    scroll: gtk::ScrolledWindow,
    messages_box: gtk::Box,
    pub entry: gtk::Entry,
    pub send_button: gtk::Button,
    /// The files waiting to go with the next send, as tiles.
    preview_bar: gtk::ScrolledWindow,
    pending_box: gtk::Box,
    reply_bar: gtk::Box,
    reply_label: gtk::Label,
    /// The typing bubble after the last message, and who (in a group).
    typing_row: gtk::Box,
    typing_names: gtk::Label,
    pending: RefCell<Vec<PathBuf>>,
    /// Unsent text and files of the chats not on screen, by thread, so each
    /// chat keeps its own half-typed message.
    drafts: RefCell<HashMap<i64, Draft>>,
    /// Set while a draft is put back, so that is not "typing".
    restoring: Cell<bool>,
    /// Whether to capitalize while typing (Settings), asked on each key.
    autocap_on: RefCell<Option<Box<dyn Fn() -> crate::core::autocap::AutocapRules>>>,
    /// The automatic capital just made (where, what it made, what was
    /// typed), until the next edit: Ctrl+Z then puts the typing back.
    autocap_last: RefCell<Option<(i32, String, String)>>,
    /// Names to capitalize (contacts and the built-in list).
    names: RefCell<crate::core::autocap::Names>,
    /// Set while a capital is being put in.
    autocap_busy: Cell<bool>,
    /// `(thread_id, text, files, reply_to)` → whether it was taken (then the
    /// compose bar clears).
    on_send: RefCell<Option<SendHandler>>,
    /// Asked for the phone's older messages of a chat.
    on_older: RefCell<Option<ThreadHandler>>,
    /// The user typed in a chat (for "is typing" on the phone).
    on_typing: RefCell<Option<ThreadHandler>>,
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
        super::scale::icon(&icon, 48);
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
        // On a wide window the two sides would sit a screen apart: keep the
        // conversation a readable width, centered.
        let column = adw::Clamp::new();
        column.set_maximum_size(super::scale::px(900));
        column.set_tightening_threshold(super::scale::px(700));
        // Someone typing: a bubble of bouncing dots after the last message,
        // as phones show it (a group names who, over it).
        let typing_names = gtk::Label::new(None);
        typing_names.set_xalign(0.0);
        typing_names.set_margin_start(12);
        typing_names.add_css_class("dim-label");
        typing_names.add_css_class("caption");
        typing_names.set_visible(false);
        let typing_bubble = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        typing_bubble.add_css_class("message-bubble-received");
        typing_bubble.add_css_class("typing-bubble");
        typing_bubble.set_halign(gtk::Align::Start);
        for n in 1..=3 {
            let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            dot.add_css_class("typing-dot");
            dot.add_css_class(&format!("typing-dot-{n}"));
            dot.set_valign(gtk::Align::Center);
            typing_bubble.append(&dot);
        }
        let typing_row = gtk::Box::new(gtk::Orientation::Vertical, 2);
        typing_row.set_halign(gtk::Align::Start);
        typing_row.set_margin_start(24);
        typing_row.set_margin_bottom(10);
        typing_row.append(&typing_names);
        typing_row.append(&typing_bubble);
        typing_row.set_visible(false);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&messages_box);
        content.append(&typing_row);
        column.set_child(Some(&content));
        scroll.set_child(Some(&column));
        thread_box.append(&scroll);

        thread_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // Files waiting to go with the next send: a row of tiles (a picture
        // in its own shape, or a file's icon and name), each with a remove
        // button on its corner.
        let pending_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let preview_bar = gtk::ScrolledWindow::new();
        preview_bar.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
        preview_bar.set_propagate_natural_height(true);
        preview_bar.set_margin_start(12);
        preview_bar.set_margin_end(12);
        preview_bar.set_margin_top(8);
        preview_bar.set_child(Some(&pending_box));
        preview_bar.set_visible(false);
        thread_box.append(&preview_bar);

        // "Replying to …": the next send answers that message.
        let reply_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        reply_bar.set_margin_start(12);
        reply_bar.set_margin_end(12);
        reply_bar.set_margin_top(8);
        reply_bar.add_css_class("compose-reply-bar");
        reply_bar.set_visible(false);
        let reply_icon = gtk::Image::from_icon_name("mail-reply-sender-symbolic");
        reply_bar.append(&reply_icon);
        let reply_label = gtk::Label::new(None);
        reply_label.set_hexpand(true);
        reply_label.set_xalign(0.0);
        reply_label.set_ellipsize(pango::EllipsizeMode::End);
        reply_label.set_single_line_mode(true);
        reply_bar.append(&reply_label);
        let reply_cancel = gtk::Button::from_icon_name("window-close-symbolic");
        reply_cancel.add_css_class("flat");
        reply_cancel.set_tooltip_text(Some("Don't reply (Esc)"));
        reply_bar.append(&reply_cancel);
        thread_box.append(&reply_bar);

        let compose = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        compose.set_margin_start(12);
        compose.set_margin_end(12);
        compose.set_margin_top(8);
        compose.set_margin_bottom(8);
        let attach_button = gtk::Button::from_icon_name("mail-attachment-symbolic");
        attach_button.add_css_class("flat");
        attach_button.add_css_class("compose-button");
        attach_button.set_tooltip_text(Some("Add pictures or files"));
        compose.append(&attach_button);
        let entry = gtk::Entry::new();
        // Its text follows the message zoom, like the bubbles.
        entry.add_css_class("compose-entry");
        entry.set_placeholder_text(Some("Type a message…  (Ctrl+V pastes a picture)"));
        entry.set_hexpand(true);
        compose.append(&entry);
        let send_button = gtk::Button::from_icon_name(&super::icons::send_icon());
        send_button.add_css_class("suggested-action");
        send_button.add_css_class("compose-button");
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
            pending_box,
            reply_bar,
            reply_label,
            typing_row,
            typing_names,
            pending: RefCell::default(),
            drafts: RefCell::default(),
            restoring: Cell::new(false),
            autocap_on: RefCell::default(),
            autocap_last: RefCell::default(),
            names: RefCell::new(crate::core::autocap::Names::new([], None)),
            autocap_busy: Cell::new(false),
            on_send: RefCell::default(),
            on_older: RefCell::default(),
            on_typing: RefCell::default(),
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
        attach_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.pick_files();
            }
        });
        // Files dragged from the file manager onto the chat.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let weak = Rc::downgrade(&this);
        drop.connect_drop(move |_, value, _, _| {
            let (Some(this), Ok(list)) = (weak.upgrade(), value.get::<gdk::FileList>()) else {
                return false;
            };
            let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).filter(|p| p.is_file()).collect();
            if paths.is_empty() {
                return false;
            }
            for p in paths {
                this.add_pending(p);
            }
            this.entry.grab_focus();
            true
        });
        thread_box.add_controller(drop);

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
        reply_cancel.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.clear_reply();
                this.entry.grab_focus();
            }
        });
        // Typing tells the phone (RCS chats show "… is typing").
        let weak = Rc::downgrade(&this);
        entry.connect_changed(move |e| {
            let Some(this) = weak.upgrade() else { return };
            let tid = this.state.borrow().thread_id;
            let f = this.on_typing.borrow().clone();
            if this.restoring.get() {
                return;
            }
            if let (Some(tid), Some(f), false) = (tid, f, e.text().trim().is_empty()) {
                f(tid);
            }
        });
        // Capitals at the start of a sentence and for "I", as typed.
        if let Some(text) = entry.delegate() {
            let weak = Rc::downgrade(&this);
            text.connect_insert_text(move |ed, typed, pos| {
                if let Some(this) = weak.upgrade() {
                    this.autocap_typed(ed, typed, *pos);
                }
            });
            let weak = Rc::downgrade(&this);
            text.connect_delete_text(move |_, _, _| {
                if let Some(this) = weak.upgrade()
                    && !this.autocap_busy.get()
                {
                    this.autocap_last.take();
                }
            });
        }
        // Ctrl+V / Shift+Insert with a picture on the clipboard attaches it.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, state| {
            let paste = (state.contains(gdk::ModifierType::CONTROL_MASK) && matches!(key, gdk::Key::v | gdk::Key::V))
                || (state.contains(gdk::ModifierType::SHIFT_MASK) && key == gdk::Key::Insert);
            match (paste, weak.upgrade()) {
                (true, Some(this)) if this.paste_picture() => glib::Propagation::Stop,
                (false, Some(this)) if key == gdk::Key::Escape && this.state.borrow().replying.is_some() => {
                    this.clear_reply();
                    glib::Propagation::Stop
                }
                // Ctrl+Z right after an automatic capital takes it back.
                (false, Some(this))
                    if matches!(key, gdk::Key::z | gdk::Key::Z)
                        && state.contains(gdk::ModifierType::CONTROL_MASK)
                        && !state.contains(gdk::ModifierType::SHIFT_MASK)
                        && this.undo_autocap() =>
                {
                    glib::Propagation::Stop
                }
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
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
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
                let Some(this) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                let next = if step == 0.0 { 1.0 } else { this.font_scale.get() + step };
                this.set_font_scale(next, true);
                glib::Propagation::Stop
            });
            shortcuts.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(trigger), Some(action)));
        }
        let weak = Rc::downgrade(&this);
        let find = gtk::CallbackAction::new(move |_, _| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
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
            super::perf::scroll("value", a.upper(), a.value(), a.page_size());
            if let Some(this) = weak.upgrade()
                && let Ok(mut st) = this.state.try_borrow_mut()
            {
                st.pinned = a.value() + a.page_size() >= a.upper() - 48.0;
            }
        });
        // Scrolled to the very top: the phone's older messages come in.
        let weak = Rc::downgrade(&this);
        scroll.connect_edge_reached(move |_, pos| {
            if pos == gtk::PositionType::Top
                && let Some(this) = weak.upgrade()
            {
                this.load_older_from_phone();
            }
        });
        // The fix goes in right after GTK's own layout pass of the same frame
        // (the frame clock lays out again when asked), so no frame is ever
        // painted at the old position. Connected after the window's layout
        // handler, so it runs after it.
        let weak = Rc::downgrade(&this);
        scroll.connect_realize(move |w| {
            let Some(clock) = w.frame_clock() else { return };
            let weak = weak.clone();
            clock.connect_layout(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.apply_scroll_fix();
                }
            });
        });
        let weak = Rc::downgrade(&this);
        adj.connect_changed(move |a| {
            super::perf::scroll("changed", a.upper(), a.value(), a.page_size());
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
            // The whole bubble, so its padding (in em) grows with the text.
            self.font_provider.load_from_data(&format!(
                ".message-bubble-sent, .message-bubble-received, .compose-entry {{ font-size: {pct}%; }}"
            ));
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
        let (Some(tid), Some(f)) = (self.state.borrow().thread_id, self.on_rendered.borrow().clone()) else {
            return;
        };
        if !uids.is_empty() {
            glib::idle_add_local_once(move || f(tid, uids));
        }
    }

    pub fn connect_send(&self, f: impl Fn(i64, &str, &[PathBuf], Option<&str>) -> bool + 'static) {
        *self.on_send.borrow_mut() = Some(Box::new(f));
    }

    /// `f(thread)`: the user wants the phone's older messages of the chat.
    pub fn connect_older(&self, f: impl Fn(i64) + 'static) {
        *self.on_older.borrow_mut() = Some(Rc::new(f));
    }

    /// `f(thread)` as the user types a message.
    pub fn connect_typing(&self, f: impl Fn(i64) + 'static) {
        *self.on_typing.borrow_mut() = Some(Rc::new(f));
    }

    /// The typing bubble (None hides it).
    pub fn set_typing(&self, who: Option<&crate::core::messages::TypingShown>) {
        let names = who.and_then(|w| w.names.as_deref());
        self.typing_names.set_label(names.unwrap_or(""));
        self.typing_names.set_visible(names.is_some());
        self.typing_row.set_tooltip_text(who.map(|w| w.text.as_str()));
        self.typing_row.update_property(&[gtk::accessible::Property::Label(who.map_or("", |w| w.text.as_str()))]);
        self.typing_row.set_visible(who.is_some());
    }

    /// Whether the phone has older messages of chat `tid` than those here,
    /// and whether some are on their way: the top button follows.
    pub fn set_more_history(self: &Rc<Self>, tid: i64, more: bool, loading: bool) {
        let (changed, hidden) = {
            let mut st = self.state.borrow_mut();
            if st.thread_id != Some(tid) {
                return;
            }
            let changed = st.more_on_phone != more;
            st.more_on_phone = more;
            st.loading_older = loading;
            let first = st.window_first_date;
            (changed, st.all.iter().filter(|m| m.date < first).count())
        };
        let button = self.state.borrow().earlier.clone();
        match button {
            Some(b) if hidden == 0 && !more => {
                // Nothing older anywhere: the button goes.
                self.messages_box.remove(&b);
                self.state.borrow_mut().earlier = None;
            }
            Some(b) => {
                b.set_label(&self.earlier_text(hidden));
                b.set_sensitive(!(hidden == 0 && loading));
            }
            None if changed && more && self.widget.visible_child_name().as_deref() == Some("thread") => {
                // The button can only go at the top; a rebuild places it.
                self.hold_scroll_through(|this| this.set_messages(true));
            }
            None => {}
        }
    }

    fn earlier_text(&self, hidden: usize) -> String {
        let st = self.state.borrow();
        match hidden {
            0 if st.loading_older => "Loading earlier messages…".to_string(),
            0 => "Load earlier messages from your phone".to_string(),
            n => earlier_label(n),
        }
    }

    /// Scrolled to the top, or the top button: show what is here, or ask
    /// the phone for older messages once everything here is shown.
    fn load_older_from_phone(self: &Rc<Self>) {
        let (tid, ask) = {
            let mut st = self.state.borrow_mut();
            let first = st.window_first_date;
            let hidden = st.all.iter().filter(|m| m.date < first).count();
            let ask = hidden == 0 && st.more_on_phone && !st.loading_older && !st.revealing;
            if ask {
                st.loading_older = true;
                // Room for the page about to arrive, so it shows as it lands.
                st.window += THREAD_WINDOW_STEP;
            }
            (st.thread_id, ask)
        };
        let (Some(tid), true) = (tid, ask) else {
            return;
        };
        if let Some(b) = self.state.borrow().earlier.clone() {
            b.set_label("Loading earlier messages…");
            b.set_sensitive(false);
        }
        let f = self.on_older.borrow().clone();
        if let Some(f) = f {
            f(tid);
        }
    }

    /// Run `f` (a rebuild) keeping on screen what the user is looking at,
    /// unless they are at the bottom.
    fn hold_scroll_through(self: &Rc<Self>, f: impl FnOnce(&Rc<Self>)) {
        let adj = self.scroll.vadjustment();
        let pinned = self.state.borrow().pinned;
        let from_bottom = adj.upper() - adj.value();
        f(self);
        if !pinned {
            self.state.borrow_mut().pinned = false;
            glib::idle_add_local_once(move || adj.set_value((adj.upper() - from_bottom).max(0.0)));
        }
    }

    fn send(&self) {
        let Some(tid) = self.state.borrow().thread_id else {
            return;
        };
        let text = self.entry.text().trim().to_string();
        let files = self.pending.borrow().clone();
        if text.is_empty() && files.is_empty() {
            return;
        }
        let reply_to = self.state.borrow().replying.clone();
        let taken = self.on_send.borrow().as_ref().is_some_and(|f| f(tid, &text, &files, reply_to.as_deref()));
        if taken {
            self.entry.set_text("");
            self.clear_reply();
            self.clear_pending(false);
            for path in files {
                // A pasted picture we saved ourselves is removed once KDE
                // Connect has had time to read it, as Python did. A file
                // from the user's own folders is theirs: never touched.
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
                let (Some(this), Ok(Some(tex))) = (weak.upgrade(), res) else {
                    return;
                };
                let dir = match private_tmp::dir(None) {
                    Ok(d) => d,
                    Err(e) => {
                        log::warn!("paste folder: {e}");
                        return;
                    }
                };
                let stamp =
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
                let path = dir.join(format!("paste_{stamp}.png"));
                if private_tmp::write_private(&path, &tex.save_to_png_bytes()).is_ok() {
                    this.add_pending(path);
                }
            });
            return true;
        }
        if formats.contain_mime_type("text/uri-list") {
            clipboard.read_text_async(gio::Cancellable::NONE, move |res| {
                let (Some(this), Ok(Some(text))) = (weak.upgrade(), res) else {
                    return;
                };
                for path in files_from_uri_list(&text) {
                    this.add_pending(path);
                }
            });
            return true;
        }
        false
    }

    /// The paperclip: choose pictures or other files to go with the next
    /// message.
    fn pick_files(self: &Rc<Self>) {
        let pictures = gtk::FileFilter::new();
        pictures.set_name(Some("Pictures"));
        for ext in IMAGE_FILE_EXTS {
            pictures.add_suffix(ext);
        }
        let all = gtk::FileFilter::new();
        all.set_name(Some("All files"));
        all.add_pattern("*");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&all);
        filters.append(&pictures);
        let dialog = gtk::FileDialog::new();
        dialog.set_title("Add pictures or files");
        dialog.set_filters(Some(&filters));
        dialog.set_default_filter(Some(&all));
        if let Some(dir) = glib::user_special_dir(glib::UserDirectory::Pictures) {
            dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
        }
        let window = self.entry.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(self);
        dialog.open_multiple(window.as_ref(), gio::Cancellable::NONE, move |res| {
            let (Some(this), Ok(files)) = (weak.upgrade(), res) else {
                return;
            };
            for f in files.iter::<gio::File>().flatten() {
                if let Some(path) = f.path().filter(|p| p.is_file()) {
                    this.add_pending(path);
                }
            }
            this.entry.grab_focus();
        });
    }

    /// Add a file to go with the next send (a picture shows as itself).
    pub fn add_pending(self: &Rc<Self>, path: PathBuf) {
        if self.pending.borrow().contains(&path) {
            return;
        }
        let tile = gtk::Overlay::new();
        tile.set_valign(gtk::Align::Start);
        let looks_like_picture = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| IMAGE_FILE_EXTS.contains(&e.to_ascii_lowercase().as_str()));
        let info = looks_like_picture.then(|| gtk::gdk_pixbuf::Pixbuf::file_info(&path)).flatten();
        match info {
            Some((_, w, h)) => {
                let (w, h) = preview_size(w.max(1), h.max(1));
                let picture = gtk::Picture::for_filename(&path);
                picture.set_can_shrink(true);
                picture.set_content_fit(gtk::ContentFit::Cover);
                picture.set_overflow(gtk::Overflow::Hidden);
                picture.add_css_class("compose-thumb");
                picture.set_size_request(w, h);
                // A picture asks for its full pixel size; the clamps hold it.
                let cw = adw::Clamp::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .maximum_size(w)
                    .tightening_threshold(w)
                    .child(&picture)
                    .build();
                let ch = adw::Clamp::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .maximum_size(h)
                    .tightening_threshold(h)
                    .child(&cw)
                    .build();
                tile.set_child(Some(&ch));
            }
            None => {
                let file = gtk::Box::new(gtk::Orientation::Vertical, 6);
                file.add_css_class("compose-file");
                file.set_size_request(120, 96);
                let mime = crate::core::gm::guess_mime(&path.to_string_lossy());
                let icon = gtk::Image::from_icon_name(&super::icons::resolve(file_icon(mime)));
                super::scale::icon(&icon, 32);
                icon.set_margin_top(14);
                file.append(&icon);
                let name = gtk::Label::new(path.file_name().and_then(|n| n.to_str()));
                name.set_ellipsize(pango::EllipsizeMode::Middle);
                name.set_max_width_chars(14);
                name.add_css_class("caption");
                name.set_margin_start(6);
                name.set_margin_end(6);
                file.append(&name);
                tile.set_child(Some(&file));
            }
        }
        let remove = gtk::Button::from_icon_name("window-close-symbolic");
        remove.add_css_class("osd");
        remove.add_css_class("circular");
        remove.set_halign(gtk::Align::End);
        remove.set_valign(gtk::Align::Start);
        remove.set_margin_top(6);
        remove.set_margin_end(6);
        remove.set_tooltip_text(Some("Remove"));
        tile.add_overlay(&remove);
        let (weak, p, t) = (Rc::downgrade(self), path.clone(), tile.clone());
        remove.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.remove_pending(&p, &t);
            }
        });
        self.pending_box.append(&tile);
        self.pending.borrow_mut().push(path);
        self.preview_bar.set_visible(true);
    }

    fn remove_pending(&self, path: &Path, tile: &gtk::Overlay) {
        self.pending.borrow_mut().retain(|p| p != path);
        self.pending_box.remove(tile);
        if private_tmp::contains(path) {
            let _ = std::fs::remove_file(path);
        }
        self.preview_bar.set_visible(!self.pending.borrow().is_empty());
    }

    /// Drop every pending file; `delete` removes pasted temp files.
    fn clear_pending(&self, delete: bool) {
        for path in self.pending.borrow_mut().drain(..) {
            if delete && private_tmp::contains(&path) {
                let _ = std::fs::remove_file(path);
            }
        }
        while let Some(child) = self.pending_box.first_child() {
            self.pending_box.remove(&child);
        }
        self.preview_bar.set_visible(false);
    }

    /// The content grew (older bubbles above, a picture sized in): put the
    /// view back where it belongs at the start of the next frame. Setting the
    /// value right in the adjustment's `changed` signal happens during the
    /// layout pass, too late for the viewport: the scroll bar moved but the
    /// content stayed put until the next scroll (Ethan, 2026-11-06).
    fn queue_scroll_fix(self: &Rc<Self>) {
        {
            let Ok(mut st) = self.state.try_borrow_mut() else {
                return;
            };
            if st.scroll_fix_queued || (st.anchor_from_bottom.is_none() && !st.pinned) {
                return;
            }
            st.scroll_fix_queued = true;
        }
        // Normally done by the frame clock's layout handler this frame; the
        // next frame's tick is the fallback.
        let weak = Rc::downgrade(self);
        self.scroll.add_tick_callback(move |_, _| match weak.upgrade() {
            Some(this) if !this.apply_scroll_fix() => glib::ControlFlow::Continue,
            _ => glib::ControlFlow::Break,
        });
    }

    /// Put the view where it belongs if a fix is queued. False when the
    /// state is busy (try again).
    fn apply_scroll_fix(&self) -> bool {
        let a = self.scroll.vadjustment();
        let target = {
            let Ok(mut st) = self.state.try_borrow_mut() else {
                return false;
            };
            if !st.scroll_fix_queued {
                return true;
            }
            st.scroll_fix_queued = false;
            match st.anchor_from_bottom {
                // Older bubbles going in above: keep what is on screen.
                Some(from_bottom) => Some(a.upper() - from_bottom),
                None if st.pinned => Some(a.upper() - a.page_size()),
                None => None,
            }
        };
        if let Some(v) = target {
            super::perf::scroll("fix", a.upper(), v, a.page_size());
            a.set_value(v);
        }
        true
    }

    /// The names to capitalize (when the contacts change).
    pub fn set_names(&self, names: crate::core::autocap::Names) {
        *self.names.borrow_mut() = names;
    }

    /// Capitalize while typing, if Settings allow it.
    pub fn set_autocap(&self, on: impl Fn() -> crate::core::autocap::AutocapRules + 'static) {
        *self.autocap_on.borrow_mut() = Some(Box::new(on));
    }

    /// One character was typed at `pos`: queue its capital (or the lone
    /// "i" before it). The change goes in right after the insert, before the
    /// next frame is drawn.
    fn autocap_typed(self: &Rc<Self>, ed: &gtk::Editable, typed: &str, pos: i32) {
        use crate::core::autocap::{Autocap, autocap, ends_word};
        if self.autocap_busy.get() {
            return;
        }
        self.autocap_last.take();
        let mut chars = typed.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return; // a paste or a draft put back
        };
        if self.restoring.get() {
            return;
        }
        let Some(rules) = self.autocap_on.borrow().as_ref().map(|f| f()) else {
            return;
        };
        let before = ed.chars(0, pos);
        // (where, what is there now, what it becomes)
        let change: Option<(i32, String, String)> = match autocap(&before, c).filter(|fix| rules.allows(fix)) {
            // The letter goes in after this signal: capitalize it then.
            Some(Autocap::Typed) => capital(c).map(|u| (pos, c.to_string(), u)),
            Some(Autocap::LoneI) => Some((pos - 1, "i".into(), "I".into())),
            None if rules.proper_nouns && ends_word(c) => self.names.borrow().fix(&before).map(|(start, proper)| {
                let word: String = before.chars().skip(start).collect();
                (start as i32, word, proper)
            }),
            None => None,
        };
        let Some((at, from, to)) = change else { return };
        let weak = Rc::downgrade(self);
        let ed = ed.clone();
        glib::idle_add_local_full(glib::Priority::HIGH, move || {
            if let Some(this) = weak.upgrade()
                && this.replace_text(&ed, at, &from, &to)
            {
                *this.autocap_last.borrow_mut() = Some((at, to.clone(), from.clone()));
            }
            glib::ControlFlow::Break
        });
    }

    /// Put back what was typed if the last thing that happened was an
    /// automatic capital. Whether it did.
    fn undo_autocap(&self) -> bool {
        let (Some((at, made, typed)), Some(ed)) = (self.autocap_last.take(), self.entry.delegate()) else {
            return false;
        };
        self.replace_text(&ed, at, &made, &typed)
    }

    /// Swap `from` (still there at `at`) for `to`, the cursor kept. Whether
    /// it did.
    fn replace_text(&self, ed: &gtk::Editable, at: i32, from: &str, to: &str) -> bool {
        let len = from.chars().count() as i32;
        if ed.chars(at, at + len) != from {
            return false;
        }
        let cursor = ed.position();
        self.autocap_busy.set(true);
        ed.delete_text(at, at + len);
        let mut p = at;
        ed.insert_text(to, &mut p);
        ed.set_position(cursor);
        self.autocap_busy.set(false);
        true
    }

    /// Put the cursor in the compose bar (after a chat was clicked open),
    /// without selecting a draft that is there.
    pub fn focus_compose(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(this) = weak.upgrade()
                && this.state.borrow().thread_id.is_some()
                && this.widget.visible_child_name().as_deref() == Some("thread")
            {
                this.entry.grab_focus_without_selecting();
                this.entry.set_position(-1);
            }
        });
    }

    pub fn show_empty(&self) {
        let old = self.state.borrow_mut().thread_id.take();
        if let Some(old) = old {
            self.stash_draft(old);
        }
        self.widget.set_visible_child_name("empty");
    }

    /// Keep what is typed (and attached) in chat `tid` for when it opens
    /// again, and empty the compose bar.
    fn stash_draft(&self, tid: i64) {
        let text = self.entry.text().to_string();
        let files = self.pending.borrow().clone();
        let mut drafts = self.drafts.borrow_mut();
        if text.trim().is_empty() && files.is_empty() {
            drafts.remove(&tid);
        } else {
            drafts.insert(tid, Draft { text, files });
        }
        drop(drafts);
        self.restoring.set(true);
        self.entry.set_text("");
        self.restoring.set(false);
        self.clear_pending(false);
    }

    /// Put chat `tid`'s unsent message back in the compose bar.
    fn restore_draft(self: &Rc<Self>, tid: i64) {
        let Some(draft) = self.drafts.borrow_mut().remove(&tid) else {
            return;
        };
        self.restoring.set(true);
        self.entry.set_text(&draft.text);
        self.restoring.set(false);
        self.entry.set_position(-1);
        for path in draft.files.into_iter().filter(|p| p.is_file()) {
            self.add_pending(path);
        }
    }

    /// Drop every chat's unsent message (the chats belong to another phone
    /// connection now, and their thread numbers mean other chats).
    pub fn forget_drafts(&self) {
        let drafts: Vec<Draft> = self.drafts.borrow_mut().drain().map(|(_, d)| d).collect();
        for path in drafts.into_iter().flat_map(|d| d.files) {
            if private_tmp::contains(&path) {
                let _ = std::fs::remove_file(path);
            }
        }
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
        let timeline = conv.messages.clone();
        let same = self.state.borrow().thread_id == Some(conv.thread_id)
            && self.widget.visible_child_name().as_deref() == Some("thread");
        let changed = self.state.borrow().thread_id != Some(conv.thread_id);
        if changed {
            let old = self.state.borrow().thread_id;
            if let Some(old) = old {
                self.stash_draft(old);
            }
            self.restore_draft(conv.thread_id);
        }
        {
            let mut st = self.state.borrow_mut();
            if changed {
                st.window = THREAD_WINDOW;
                st.thread_id = Some(conv.thread_id);
                st.reveal = None;
                st.extras.clear();
                st.replying = None;
                st.more_on_phone = false;
                st.loading_older = false;
                self.reply_bar.set_visible(false);
                self.typing_row.set_visible(false);
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
            // Older messages arriving above keep the view where it was.
            self.hold_scroll_through(|this| this.set_messages(true));
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
            Some(btn) => btn.set_label(&self.earlier_text(hidden)),
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
        let (more, loading) = {
            let st = self.state.borrow();
            (st.more_on_phone, st.loading_older)
        };
        if hidden > 0 || more {
            let btn = gtk::Button::with_label(&self.earlier_text(hidden));
            btn.set_sensitive(!(hidden == 0 && loading));
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
            let b = self.make_bubble(m, newest, routes.get(&m.uid).copied());
            mark_turn(&b, i.checked_sub(1).map(|j| &shown[j]));
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
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
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
                let b = this.make_bubble(m, false, routes.get(&m.uid).copied());
                mark_turn(&b, i.checked_sub(1).map(|x| &shown[x]));
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
            st.all = conv.messages.clone();
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
                        .find(|m| {
                            m.uid == b.message.uid
                                && (m.attachments != b.message.attachments || route(m.uid) != b.route)
                        })
                        .map(|m| (i, m.clone()))
                })
                .collect()
        };
        let pinned = self.state.borrow().pinned;
        // Pictures that changed (filled in, or a new copy) are new to fetch.
        let new_files: Vec<i64> = {
            let st = self.state.borrow();
            changed
                .iter()
                .filter(|(i, m)| m.attachments != st.bubbles[*i].message.attachments)
                .map(|(_, m)| m.uid)
                .collect()
        };
        for (i, m) in changed {
            let old = self.state.borrow().bubbles[i].clone();
            let fresh = self.make_bubble(&m, old.revealer.reveals_child(), route(m.uid));
            fresh.widget.set_margin_top(old.widget.margin_top());
            self.messages_box.insert_child_after(&fresh.widget, Some(&old.widget));
            self.messages_box.remove(&old.widget);
            self.state.borrow_mut().bubbles[i] = fresh;
        }
        if !new_files.is_empty() {
            self.rendered(new_files);
        }
        if pinned {
            let adj = self.scroll.vadjustment();
            glib::idle_add_local_once(move || adj.set_value(adj.upper() - adj.page_size()));
        }
        self.mark_highlight();
    }

    fn show_earlier(self: &Rc<Self>) {
        let hidden = {
            let st = self.state.borrow();
            let first = st.window_first_date;
            st.all.iter().filter(|m| m.date < first).count()
        };
        if hidden == 0 {
            self.load_older_from_phone();
            return;
        }
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
        let Some((_, y)) = widget.translate_coordinates(&self.messages_box, 0.0, 0.0) else {
            return;
        };
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
        let b = self.make_bubble(m, true, route);
        mark_turn(&b, prev.as_ref().map(|p| &p.message));
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

    fn make_bubble(self: &Rc<Self>, m: &SmsMessage, show_time: bool, route: Option<SendRoute>) -> Rc<Bubble> {
        let b = Bubble::new(m, show_time, route, &self.actions);
        let extras = self.state.borrow().extras.get(&m.uid).cloned();
        if extras.is_some() {
            b.set_extras(extras.as_ref(), &self.actions, &self.jumper());
        }
        b
    }

    fn jumper(self: &Rc<Self>) -> Rc<dyn Fn(i64)> {
        let weak = Rc::downgrade(self);
        Rc::new(move |uid| {
            if let Some(this) = weak.upgrade() {
                this.reveal(uid);
            }
        })
    }

    /// Google Messages' reactions and quotes for the open chat (by uid);
    /// bubbles whose share changed update in place.
    pub fn set_extras(self: &Rc<Self>, extras: HashMap<i64, Extras>) {
        let bubbles = {
            let mut st = self.state.borrow_mut();
            st.extras = extras;
            st.bubbles.clone()
        };
        let jump = self.jumper();
        let pinned = self.state.borrow().pinned;
        for b in bubbles {
            let e = self.state.borrow().extras.get(&b.message.uid).cloned();
            b.set_extras(e.as_ref(), &self.actions, &jump);
        }
        if pinned {
            let adj = self.scroll.vadjustment();
            glib::idle_add_local_once(move || adj.set_value(adj.upper() - adj.page_size()));
        }
    }

    /// The next send replies to Google Messages' `msg_id` (shown as
    /// "Replying to `author`: `text`").
    pub fn start_reply(&self, msg_id: &str, author: &str, text: &str) {
        self.state.borrow_mut().replying = Some(msg_id.to_string());
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        self.reply_label.set_markup(&format!(
            "Replying to <b>{}</b>: {}",
            glib::markup_escape_text(author),
            glib::markup_escape_text(&text)
        ));
        self.reply_bar.set_visible(true);
        self.entry.grab_focus();
    }

    fn clear_reply(&self) {
        self.state.borrow_mut().replying = None;
        self.reply_bar.set_visible(false);
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

/// `(thread_id, text, files, reply_to)` → whether it was taken.
type SendHandler = Box<dyn Fn(i64, &str, &[PathBuf], Option<&str>) -> bool>;
type ZoomHandler = Box<dyn Fn(f64)>;
type ThreadHandler = Rc<dyn Fn(i64)>;
type RenderedHandler = Rc<dyn Fn(i64, Vec<i64>)>;

const IMAGE_FILE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "tif", "heic", "heif"];

/// Every existing file in a pasted `text/uri-list` (or plain paths).
pub fn files_from_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            if line.starts_with("file://") { gio::File::for_uri(line).path() } else { Some(PathBuf::from(line)) }
        })
        .filter(|p| p.is_absolute() && p.is_file())
        .collect()
}

/// An icon for a file by its type.
fn file_icon(mime: &str) -> &'static str {
    match mime.split('/').next().unwrap_or("") {
        "image" => "image-x-generic-symbolic",
        "video" => "video-x-generic-symbolic",
        "audio" => "audio-x-generic-symbolic",
        _ if mime == "application/pdf" => "x-office-document-symbolic",
        _ if mime.starts_with("text/") => "text-x-generic-symbolic",
        _ => "mail-attachment-symbolic",
    }
}

/// The compose bar's picture fits in this box (px), keeping its shape.
const PREVIEW_MAX: (f64, f64) = (280.0, 200.0);

/// Size of the compose bar's picture for a `w`×`h` image: as large as fits
/// in [`PREVIEW_MAX`], never blown up past its own size.
fn preview_size(w: i32, h: i32) -> (i32, i32) {
    let (fw, fh) = (f64::from(w), f64::from(h));
    let fit = (PREVIEW_MAX.0 / fw).min(PREVIEW_MAX.1 / fh).min(1.0);
    (((fw * fit).round() as i32).max(24), ((fh * fit).round() as i32).max(24))
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
        // A preview waiting for its full picture takes the full one's room
        // (no shift when it lands); one with nothing to fetch stays small.
        assert_eq!(image_display(100, 75, true), (380, 285, true));
        assert_eq!(image_display(100, 75, false), (220, 165, false));
        assert_eq!(image_display(4000, 3000, false).0, image_display(100, 75, true).0);
        assert_eq!(image_display(75, 100, true), (375, 500, true));
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
        // The picture about to go keeps its shape.
        assert_eq!(preview_size(1600, 1200), (267, 200));
        assert_eq!(preview_size(1080, 1920), (113, 200));
        assert_eq!(preview_size(4000, 1000), (280, 70));
        assert_eq!(preview_size(100, 80), (100, 80));
        let att = Attachment { file_name: "IMG 1".into(), mime_type: "image/png".into(), ..Default::default() };
        assert_eq!(attachment_name(&att), "IMG_1.png");
    }
}
