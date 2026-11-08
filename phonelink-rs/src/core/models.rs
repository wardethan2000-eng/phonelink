//! Data models (port of `phonelink/models.py`).

use chrono::{DateTime, Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A paired / known KDE Connect device.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub reachable: bool,
    pub paired: bool,
    /// -1 = unknown.
    pub battery_charge: i32,
    pub battery_charging: bool,
}

impl Device {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: "Unknown Device".into(),
            kind: "phone".into(),
            reachable: false,
            paired: false,
            battery_charge: -1,
            battery_charging: false,
        }
    }

    pub fn status_label(&self) -> &'static str {
        if !self.paired {
            "Not Paired"
        } else if self.reachable {
            "Connected"
        } else {
            "Disconnected"
        }
    }

    pub fn battery_label(&self) -> String {
        if self.battery_charge < 0 {
            return "Unknown".into();
        }
        let suffix = if self.battery_charging { " · Charging" } else { "" };
        format!("{}%{}", self.battery_charge, suffix)
    }

    pub fn type_icon_name(&self) -> &'static str {
        match self.kind.as_str() {
            "tablet" | "desktop" | "laptop" => "computer-symbolic",
            _ => "phone-symbolic",
        }
    }

    pub fn battery_icon_name(&self) -> String {
        let c = self.battery_charge;
        if c < 0 {
            return "battery-missing-symbolic".into();
        }
        let level = match c {
            80.. => "full",
            50.. => "good",
            20.. => "low",
            5.. => "caution",
            _ => "empty",
        };
        let suffix = if self.battery_charging { "-charging" } else { "" };
        format!("battery-{level}{suffix}-symbolic")
    }
}

/// One MMS attachment. Stored as a JSON object in `messages.attachments`;
/// keys the Rust app does not know are kept so Python and Rust can share the
/// store.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    #[serde(default)]
    pub part_id: i64,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub payload: String,
    #[serde(default)]
    pub unique_identifier: String,
    #[serde(default)]
    pub file_name: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub const MSG_TYPE_RECEIVED: i32 = 1;
pub const MSG_TYPE_SENT: i32 = 2;
/// Not Android values: a call shown in a chat's timeline (see [`Call`]).
pub const MSG_TYPE_CALL_INCOMING: i32 = 101;
pub const MSG_TYPE_CALL_MISSED: i32 = 102;

/// A phone call KDE Connect told us about (it only reports calls while it
/// runs, and only ringing and missed: not answered or ended).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Call {
    pub number: String,
    /// When it rang, ms since epoch.
    pub date: i64,
    pub missed: bool,
    /// Opened in the app since (a missed call unseen keeps the chat unread).
    pub seen: bool,
}

impl Call {
    pub fn label(&self) -> &'static str {
        if self.missed { "Missed call" } else { "Incoming call" }
    }

    /// The call as a timeline item. Its uid is negative so it never
    /// collides with a phone message, and changes when the call turns
    /// missed so the open chat redraws it.
    pub fn as_message(&self, thread_id: i64) -> SmsMessage {
        SmsMessage {
            uid: -(self.date.max(1) * 2 + i64::from(self.missed)),
            body: self.label().to_string(),
            address: self.number.clone(),
            date: self.date,
            msg_type: if self.missed { MSG_TYPE_CALL_MISSED } else { MSG_TYPE_CALL_INCOMING },
            read: 1,
            thread_id,
            ..Default::default()
        }
    }
}

/// A single SMS/MMS message.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SmsMessage {
    pub uid: i64,
    pub body: String,
    pub address: String,
    /// ms since epoch.
    pub date: i64,
    /// 1 = received, 2 = sent.
    pub msg_type: i32,
    pub read: i32,
    pub thread_id: i64,
    pub attachments: Vec<Attachment>,
    /// Every participant address on the wire (deduplicated), for group
    /// detection. Not persisted (Python: `msg._all_addresses`).
    pub all_addresses: Vec<String>,
}

impl SmsMessage {
    /// A call shown in the timeline, not a text.
    pub fn is_call(&self) -> bool {
        matches!(self.msg_type, MSG_TYPE_CALL_INCOMING | MSG_TYPE_CALL_MISSED)
    }

    pub fn is_sent(&self) -> bool {
        self.msg_type == MSG_TYPE_SENT
    }

    pub fn timestamp(&self) -> DateTime<Local> {
        if self.date != 0 {
            local_from_ms(self.date)
        } else {
            Local::now()
        }
    }

    pub fn time_label(&self) -> String {
        message_time_label(self.timestamp(), Local::now())
    }
}

/// A conversation thread (one entry per contact/group).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Conversation {
    pub thread_id: i64,
    pub display_name: String,
    pub address: String,
    pub addresses: Vec<String>,
    pub last_message: String,
    pub last_date: i64,
    pub is_read: bool,
    pub messages: Vec<SmsMessage>,
    /// Canonical participant-set key (in memory only; see `reconcile`).
    pub identity: String,
    /// Every phone thread id merged into this conversation.
    pub thread_ids: Vec<i64>,
    /// Keeps a named group apart from other chats with the same people
    /// (Google Messages names some groups). Empty for everything else.
    pub separate: String,
    /// Calls with this chat's number, oldest first (1:1 chats only; not in
    /// `messages`, so search, sync and the store never see them as texts).
    pub calls: Vec<Call>,
}

impl Conversation {
    pub fn new(thread_id: i64) -> Self {
        Self { thread_id, is_read: true, ..Default::default() }
    }

    pub fn is_group(&self) -> bool {
        self.addresses.len() > 1
    }

    /// The newest call, when it is newer than the newest text.
    pub fn newest_call(&self) -> Option<&Call> {
        let text_date = if self.last_date != 0 { self.last_date } else { self.messages.last().map_or(0, |m| m.date) };
        self.calls.last().filter(|c| c.date > text_date)
    }

    /// A missed call newer than every text, not opened yet: keeps the chat
    /// unread.
    pub fn has_unseen_missed_call(&self) -> bool {
        self.newest_call().is_some_and(|c| c.missed && !c.seen)
    }

    /// Texts and calls in time order.
    pub fn timeline(&self) -> Vec<SmsMessage> {
        let mut v = self.messages.clone();
        v.extend(self.calls.iter().map(|c| c.as_message(self.thread_id)));
        v.sort_by_key(|m| m.date);
        v
    }

    pub fn preview(&self) -> String {
        if let Some(call) = self.newest_call() {
            return call.label().to_string();
        }
        let text = if !self.last_message.is_empty() {
            self.last_message.as_str()
        } else if let Some(last) = self.messages.last() {
            last.body.as_str()
        } else {
            return String::new();
        };
        truncate_chars(text, 80)
    }

    pub fn sort_key(&self) -> i64 {
        let text = if self.last_date != 0 { self.last_date } else { self.messages.last().map_or(0, |m| m.date) };
        text.max(self.calls.last().map_or(0, |c| c.date))
    }

    pub fn time_label(&self) -> String {
        let ts = self.sort_key();
        if ts == 0 {
            return String::new();
        }
        conversation_time_label(local_from_ms(ts), Local::now())
    }
}

/// A phone notification mirrored via KDE Connect.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Notification {
    pub public_id: String,
    pub app_name: String,
    pub title: String,
    pub text: String,
    pub ticker: String,
    pub icon_path: String,
    pub has_icon: bool,
    pub dismissable: bool,
    pub silent: bool,
    pub reply_id: String,
    pub internal_id: String,
    /// Local time we received it, seconds since epoch.
    pub timestamp: f64,
}

impl Notification {
    pub fn can_reply(&self) -> bool {
        !self.reply_id.is_empty()
    }

    pub fn time_label(&self) -> String {
        if self.timestamp == 0.0 {
            return String::new();
        }
        let dt = local_from_ms((self.timestamp * 1000.0) as i64);
        notification_time_label(dt, Local::now())
    }
}

// ── time labels (Python strftime + lstrip("0") semantics) ─────────

pub fn local_from_ms(ms: i64) -> DateTime<Local> {
    Local.timestamp_millis_opt(ms).single().unwrap_or_else(Local::now)
}

/// `%I:%M %p` with leading zeros stripped, as Python's `.lstrip("0")` does.
fn clock(dt: &DateTime<Local>) -> String {
    dt.format("%I:%M %p").to_string().trim_start_matches('0').to_string()
}

fn days_between(earlier: NaiveDate, later: NaiveDate) -> i64 {
    (later - earlier).num_days()
}

pub fn message_time_label(ts: DateTime<Local>, now: DateTime<Local>) -> String {
    let delta = days_between(ts.date_naive(), now.date_naive());
    if delta == 0 {
        clock(&ts)
    } else if delta == 1 {
        format!("Yesterday {}", clock(&ts))
    } else if delta < 7 {
        ts.format("%A %I:%M %p").to_string()
    } else {
        ts.format("%b %d, %Y %I:%M %p").to_string()
    }
}

pub fn conversation_time_label(ts: DateTime<Local>, now: DateTime<Local>) -> String {
    let delta = days_between(ts.date_naive(), now.date_naive());
    if delta == 0 {
        clock(&ts)
    } else if delta == 1 {
        "Yesterday".into()
    } else if delta < 7 {
        ts.format("%A").to_string()
    } else {
        ts.format("%b %d").to_string()
    }
}

pub fn notification_time_label(ts: DateTime<Local>, now: DateTime<Local>) -> String {
    let delta = days_between(ts.date_naive(), now.date_naive());
    if delta == 0 {
        clock(&ts)
    } else if delta == 1 {
        format!("Yesterday {}", clock(&ts))
    } else {
        ts.format("%b %d %I:%M %p").to_string()
    }
}

/// First `max` characters, plus "…" if anything was cut (Python `text[:80]`).
pub fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).single().unwrap()
    }

    #[test]
    fn battery_icon_levels() {
        let mut d = Device::new("x");
        assert_eq!(d.battery_icon_name(), "battery-missing-symbolic");
        d.battery_charge = 85;
        assert_eq!(d.battery_icon_name(), "battery-full-symbolic");
        d.battery_charge = 4;
        d.battery_charging = true;
        assert_eq!(d.battery_icon_name(), "battery-empty-charging-symbolic");
        assert_eq!(d.battery_label(), "4% · Charging");
    }

    #[test]
    fn time_labels_strip_leading_zero_like_python() {
        let now = at(2026, 11, 5, 15, 0);
        assert_eq!(message_time_label(at(2026, 11, 5, 9, 5), now), "9:05 AM");
        assert_eq!(message_time_label(at(2026, 11, 4, 9, 5), now), "Yesterday 9:05 AM");
        assert_eq!(message_time_label(at(2026, 11, 2, 9, 5), now), "Monday 09:05 AM");
        assert_eq!(message_time_label(at(2026, 10, 1, 9, 5), now), "Oct 01, 2026 09:05 AM");
        assert_eq!(conversation_time_label(at(2026, 10, 1, 9, 5), now), "Oct 01");
    }

    #[test]
    fn preview_truncates_on_chars() {
        let mut c = Conversation::new(1);
        c.last_message = "é".repeat(81);
        let p = c.preview();
        assert_eq!(p.chars().count(), 81);
        assert!(p.ends_with('…'));
    }

    #[test]
    fn attachment_keeps_unknown_keys() {
        let json = r#"{"partId":3,"mimeType":"image/jpeg","fullPath":"/x.jpg"}"#;
        let a: Attachment = serde_json::from_str(json).unwrap();
        assert_eq!(a.part_id, 3);
        let back = serde_json::to_value(&a).unwrap();
        assert_eq!(back["fullPath"], "/x.jpg");
    }
}
