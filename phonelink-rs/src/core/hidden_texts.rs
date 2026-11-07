//! Notice when the phone is hiding texts from KDE Connect
//! (port of `phonelink/hidden_texts.py`).
//!
//! Android 17 marks most new messages as `restricted` and hides them from every
//! app that is not the default texting app, unless the app holds the
//! `READ_RESTRICTED_MESSAGES` app-op. The texting app's *notifications* still
//! come through, so a notification whose message never arrives, twice within a
//! few hours, means the phone is hiding messages.
//!
//! Time is injected: every method that needs a clock takes `now` in seconds
//! (any monotonic or wall-clock origin, as long as it is consistent).

use std::sync::OnceLock;

use regex::Regex;

/// Shown in the warning; run with the phone plugged in and USB debugging on.
pub const FIX_COMMAND: &str =
    "adb shell appops set org.kde.kdeconnect_tp READ_RESTRICTED_MESSAGES allow";

/// Give the message this long to arrive.
pub const SETTLE_SECONDS: f64 = 120.0;
pub const MISS_WINDOW_SECONDS: f64 = 6.0 * 3600.0;
pub const MISSES_TO_WARN: usize = 2;
/// Shorter previews match too loosely.
pub const MIN_TEXT_LENGTH: usize = 8;

/// Previews that stand in for media rather than quoting the message body.
const GENERIC_PREVIEWS: &[&str] = &[
    "photo", "image", "picture", "video", "sticker", "gif", "audio",
    "voice message", "sent a photo", "sent an image", "sent a video",
    "sent a sticker", "sent a gif", "attachment", "location", "contact",
];

fn whitespace_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+").expect("valid regex"))
}

/// Lower-case, single-spaced, without a trailing ellipsis.
///
/// Python uses `casefold()`; this uses `to_lowercase()` (differs only for a
/// few characters such as German sharp s, irrelevant for loose matching).
pub fn normalize_text(text: &str) -> String {
    let collapsed = whitespace_re().replace_all(text, " ");
    let t = collapsed.trim().to_lowercase();
    let stripped = t
        .strip_suffix('…')
        .or_else(|| t.strip_suffix("..."))
        .unwrap_or(&t);
    stripped.trim().to_string()
}

/// Whether a notification preview quotes this message body.
///
/// Previews can be truncated, or prefixed with the sender in group chats.
pub fn body_matches(preview: &str, body: &str) -> bool {
    let p = normalize_text(preview);
    let b = normalize_text(body);
    if p.is_empty() || b.is_empty() {
        return false;
    }
    p == b
        || (b.chars().count() >= MIN_TEXT_LENGTH && p.contains(&b))
        || b.starts_with(&p)
}

/// Stricter `body_matches` for choosing where a reply goes: a truncated
/// preview must still be long enough that it can't fit a text by chance.
pub fn reply_preview_matches(preview: &str, body: &str) -> bool {
    let p = normalize_text(preview);
    let b = normalize_text(body);
    if p.is_empty() || b.is_empty() {
        return false;
    }
    p == b
        || (b.chars().count() >= MIN_TEXT_LENGTH && p.contains(&b))
        || (p.chars().count() >= MIN_TEXT_LENGTH && b.starts_with(&p))
}

#[derive(Debug, Clone)]
pub struct HiddenTextDetector {
    pub settle_seconds: f64,
    pub miss_window_seconds: f64,
    pub misses_to_warn: usize,
    pending: Vec<(f64, String)>,
    misses: Vec<f64>,
    pub warning: bool,
}

impl Default for HiddenTextDetector {
    fn default() -> Self {
        Self::new(SETTLE_SECONDS, MISS_WINDOW_SECONDS, MISSES_TO_WARN)
    }
}

impl HiddenTextDetector {
    pub fn new(settle_seconds: f64, miss_window_seconds: f64, misses_to_warn: usize) -> Self {
        Self {
            settle_seconds,
            miss_window_seconds,
            misses_to_warn,
            pending: Vec::new(),
            misses: Vec::new(),
            warning: false,
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Remember a texting-app notification. Returns true if it is tracked.
    pub fn note_notification(&mut self, preview: &str, now: f64) -> bool {
        let text = normalize_text(preview);
        if text.chars().count() < MIN_TEXT_LENGTH || GENERIC_PREVIEWS.contains(&text.as_str()) {
            return false;
        }
        if self.pending.iter().any(|(_, t)| *t == text) {
            return false;
        }
        self.pending.push((now, text));
        true
    }

    /// Resolve notifications old enough to judge.
    ///
    /// `arrived(preview, posted_at)` says whether a matching message reached
    /// the desktop. Returns true when `warning` changed.
    pub fn check(&mut self, now: f64, mut arrived: impl FnMut(&str, f64) -> bool) -> bool {
        let before = self.warning;
        let mut still_waiting = Vec::new();
        for (posted_at, text) in std::mem::take(&mut self.pending) {
            if now - posted_at < self.settle_seconds {
                still_waiting.push((posted_at, text));
            } else if arrived(&text, posted_at) {
                // Texts are getting through again.
                self.misses.clear();
                self.warning = false;
            } else {
                self.misses.push(now);
            }
        }
        self.pending = still_waiting;
        let window = self.miss_window_seconds;
        self.misses.retain(|m| now - *m <= window);
        if self.misses.len() >= self.misses_to_warn {
            self.warning = true;
        }
        self.warning != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_exact_and_case_and_spacing() {
        assert!(body_matches("See you at  Six", "see you at six"));
    }

    #[test]
    fn body_truncated_preview() {
        assert!(body_matches(
            "Can you pick up milk on the w…",
            "Can you pick up milk on the way home?"
        ));
    }

    #[test]
    fn body_group_preview_with_sender() {
        assert!(body_matches("Sam: running ten minutes late", "running ten minutes late"));
    }

    #[test]
    fn body_different_text() {
        assert!(!body_matches("running ten minutes late", "on my way now"));
    }

    #[test]
    fn normalize_strips_ellipsis() {
        assert_eq!(normalize_text("Hello there..."), "hello there");
    }

    fn det() -> HiddenTextDetector {
        HiddenTextDetector::new(100.0, 1000.0, 2)
    }

    #[test]
    fn short_and_media_previews_are_ignored() {
        let mut d = det();
        assert!(!d.note_notification("ok", 0.0));
        assert!(!d.note_notification("Photo", 0.0));
        assert!(d.note_notification("are we still on for dinner", 0.0));
    }

    #[test]
    fn waits_for_the_message_to_arrive() {
        let mut d = det();
        d.note_notification("are we still on for dinner", 0.0);
        assert!(!d.check(50.0, |_, _| false));
        assert!(d.has_pending());
    }

    #[test]
    fn two_misses_warn() {
        let mut d = det();
        d.note_notification("are we still on for dinner", 0.0);
        d.note_notification("call me when you are free", 10.0);
        assert!(d.check(200.0, |_, _| false));
        assert!(d.warning);
    }

    #[test]
    fn one_miss_does_not_warn() {
        let mut d = det();
        d.note_notification("are we still on for dinner", 0.0);
        d.check(200.0, |_, _| false);
        assert!(!d.warning);
    }

    #[test]
    fn misses_far_apart_do_not_warn() {
        let mut d = det();
        d.note_notification("are we still on for dinner", 0.0);
        d.check(200.0, |_, _| false);
        d.note_notification("call me when you are free", 5000.0);
        d.check(5200.0, |_, _| false);
        assert!(!d.warning);
    }

    #[test]
    fn an_arrival_clears_the_warning() {
        let mut d = det();
        d.note_notification("are we still on for dinner", 0.0);
        d.note_notification("call me when you are free", 10.0);
        d.check(200.0, |_, _| false);
        d.note_notification("the package arrived today", 300.0);
        assert!(d.check(500.0, |_, _| true));
        assert!(!d.warning);
    }
}
