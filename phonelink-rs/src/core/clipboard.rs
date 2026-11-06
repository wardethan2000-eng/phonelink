//! The Clipboard tab's history (port of the pure parts of
//! `phonelink/ui/clipboard_panel.py`).
//!
//! On KDE Connect 26.08 the daemon has no clipboard content and no
//! `clipboardChanged`, and `sendClipboard` only sends the desktop's current
//! clipboard. KDE Connect itself puts the phone's clipboard on the desktop
//! clipboard, so the tab watches the desktop clipboard (every 2 s) and lists
//! what passes through it.

use std::collections::VecDeque;

use chrono::{DateTime, Local};

use super::models::truncate_chars;

pub const MAX_HISTORY: usize = 50;
pub const POLL_SECONDS: u32 = 2;
/// A read still pending after this long lost its callback; allow a new one.
pub const POLL_STUCK_SECONDS: f64 = 10.0;
pub const PREVIEW_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Sent from this computer with "Send Clipboard to Phone".
    Pc,
    /// Seen on the desktop clipboard (copied here, or synced from the phone).
    Synced,
}

impl Source {
    pub fn icon_name(self) -> &'static str {
        match self {
            Source::Pc => "computer-symbolic",
            Source::Synced => "edit-paste-symbolic",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub text: String,
    pub source: Source,
    pub at: DateTime<Local>,
}

impl Entry {
    pub fn preview(&self) -> String {
        truncate_chars(&self.text, PREVIEW_CHARS)
    }

    /// `%I:%M %p` without the leading zero.
    pub fn time_label(&self) -> String {
        self.at.format("%I:%M %p").to_string().trim_start_matches('0').to_string()
    }
}

/// What a change did to the list, so the UI can mirror it without a rebuild.
#[derive(Debug, PartialEq)]
pub enum Change {
    None,
    /// A new entry at the top; `dropped_last` = the oldest row fell off.
    Added { dropped_last: bool },
}

#[derive(Debug, Default)]
pub struct History {
    entries: VecDeque<Entry>,
    /// The last desktop clipboard text seen or set by the app; a poll that
    /// reads it again adds nothing.
    last_text: String,
    /// When the pending desktop-clipboard read started (monotonic seconds).
    pending_since: Option<f64>,
}

impl History {
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    pub fn first(&self) -> Option<&Entry> {
        self.entries.front()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// May a desktop-clipboard read start now? At most one at a time, unless
    /// the last one has been pending for over [`POLL_STUCK_SECONDS`].
    pub fn begin_read(&mut self, now: f64) -> bool {
        if let Some(since) = self.pending_since
            && now - since <= POLL_STUCK_SECONDS
        {
            return false;
        }
        self.pending_since = Some(now);
        true
    }

    /// A read finished. `text` is `None` when it failed or wasn't text.
    pub fn read_done(&mut self, text: Option<&str>, source: Source, at: DateTime<Local>) -> Change {
        self.pending_since = None;
        let Some(text) = text.filter(|t| !t.is_empty() && *t != self.last_text) else { return Change::None };
        self.last_text = text.to_string();
        self.add(text, source, at)
    }

    /// Sent to the phone: list it, and don't list it again when polled.
    pub fn sent(&mut self, text: &str, at: DateTime<Local>) -> Change {
        self.last_text = text.to_string();
        self.add(text, Source::Pc, at)
    }

    /// An entry was copied back to the desktop clipboard.
    pub fn copied(&mut self, text: &str) {
        self.last_text = text.to_string();
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Stop polling (device gone): forget a pending read.
    pub fn stop(&mut self) {
        self.pending_since = None;
    }

    fn add(&mut self, text: &str, source: Source, at: DateTime<Local>) -> Change {
        if self.entries.front().is_some_and(|e| e.text == text) {
            return Change::None;
        }
        self.entries.push_front(Entry { text: text.to_string(), source, at });
        let dropped_last = self.entries.len() > MAX_HISTORY;
        if dropped_last {
            self.entries.pop_back();
        }
        Change::Added { dropped_last }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 11, 5, h, m, 0).single().unwrap()
    }

    #[test]
    fn polls_add_only_new_text() {
        let mut h = History::default();
        assert!(h.begin_read(0.0));
        assert_eq!(h.read_done(Some("hello"), Source::Synced, at(9, 5)), Change::Added { dropped_last: false });
        assert!(h.begin_read(2.0));
        assert_eq!(h.read_done(Some("hello"), Source::Synced, at(9, 5)), Change::None);
        assert!(h.begin_read(4.0));
        assert_eq!(h.read_done(None, Source::Synced, at(9, 5)), Change::None);
        assert!(h.begin_read(6.0));
        assert_eq!(h.read_done(Some(""), Source::Synced, at(9, 5)), Change::None);
        assert_eq!(h.len(), 1);
        assert_eq!(h.first().unwrap().time_label(), "9:05 AM");
    }

    #[test]
    fn one_read_at_a_time_until_stuck() {
        let mut h = History::default();
        assert!(h.begin_read(0.0));
        assert!(!h.begin_read(2.0));
        assert!(!h.begin_read(10.0));
        assert!(h.begin_read(10.5));
    }

    #[test]
    fn copy_and_send_are_not_relisted() {
        let mut h = History::default();
        h.sent("to phone", at(10, 0));
        assert_eq!(h.first().unwrap().source, Source::Pc);
        h.begin_read(0.0);
        assert_eq!(h.read_done(Some("to phone"), Source::Synced, at(10, 0)), Change::None);
        h.read_done(Some("other"), Source::Synced, at(10, 1));
        h.copied("to phone");
        h.begin_read(1.0);
        assert_eq!(h.read_done(Some("to phone"), Source::Synced, at(10, 2)), Change::None);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn capped_at_fifty_newest_first() {
        let mut h = History::default();
        for i in 0..MAX_HISTORY {
            assert_eq!(h.sent(&format!("t{i}"), at(10, 0)), Change::Added { dropped_last: false });
        }
        assert_eq!(h.sent("last", at(10, 0)), Change::Added { dropped_last: true });
        assert_eq!(h.len(), MAX_HISTORY);
        assert_eq!(h.first().unwrap().text, "last");
        assert_eq!(h.entries().last().unwrap().text, "t1");
    }

    #[test]
    fn preview_is_cut_at_200_chars() {
        let e = Entry { text: "é".repeat(201), source: Source::Synced, at: at(13, 0) };
        assert_eq!(e.preview().chars().count(), 201);
        assert!(e.preview().ends_with('…'));
        assert_eq!(e.time_label(), "1:00 PM");
    }
}
