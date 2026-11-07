//! The lasting Google Messages link (`phonelink-gm serve`): its line
//! protocol, the chat keys both sides match chats by, and turning Google's
//! chats and messages into the app's own rows.
//!
//! The helper keeps one Google Messages web connection open. When it is
//! linked, it is the Messages tab's only source (docs/RCS_HELPER.md): chats
//! and messages come from it and texts are sent through it, so they go out
//! as RCS where the chat is RCS. KDE Connect keeps everything else.

use serde::Deserialize;
use serde_json::{Value, json};

use super::models::{Attachment, MSG_TYPE_RECEIVED, MSG_TYPE_SENT, SmsMessage};
use super::phone_delete::Outcome;
use super::reconcile::phone_key;

/// A chat member's match key, as the helper's `phoneKey`: the last 10
/// digits, or, for business chats and e-mail addresses (anything with
/// `@`), the whole address in lower case.
pub fn member_key(address: &str) -> String {
    phone_key(address)
}

/// The helper's key for a chat: the members other than me, sorted and
/// joined with commas. `me` are the user's own number keys (dropped
/// unless nothing else is left, as Google Messages leaves me out).
pub fn chat_key<S: AsRef<str>>(addresses: &[S], fallback: &str, me: &[String]) -> String {
    let mut keys: Vec<String> = Vec::new();
    for a in addresses.iter().map(AsRef::as_ref).chain(std::iter::once(fallback)) {
        let k = member_key(a);
        if !k.is_empty() && !keys.contains(&k) {
            keys.push(k);
        }
    }
    if keys.iter().any(|k| !me.contains(k)) {
        keys.retain(|k| !me.contains(k));
    }
    keys.sort();
    keys.join(",")
}

/// A chat member other than me.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct Member {
    #[serde(default)]
    pub number: String,
    /// The phone's name for them, when it has one.
    #[serde(default)]
    pub name: String,
}

/// One chat on the phone.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct GmConversation {
    /// The member key (`chat_key`).
    pub chat: String,
    /// Google's conversation id.
    pub conv: String,
    /// The group's name, or the phone's name for a business chat.
    pub name: String,
    pub members: Vec<Member>,
    pub group: bool,
    pub rcs: bool,
    /// Business chats that can't be replied to.
    pub read_only: bool,
    pub archived: bool,
    pub unread: bool,
    pub last_ts_ms: i64,
    pub preview: String,
}

impl GmConversation {
    pub fn addresses(&self) -> Vec<String> {
        self.members.iter().map(|m| m.number.clone()).filter(|n| !n.is_empty()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct GmAttachment {
    pub id: String,
    pub mime: String,
    pub name: String,
    pub size: i64,
}

/// One message.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct GmMessage {
    pub conv: String,
    pub msg: String,
    pub tmp_id: String,
    pub ts_ms: i64,
    pub from_me: bool,
    /// The sender's number (theirs only).
    pub from: String,
    pub name: String,
    pub text: String,
    pub status: String,
    pub rcs: bool,
    pub deleted: bool,
    /// Replayed by the phone on connect, not new.
    pub old: bool,
    pub attachments: Vec<GmAttachment>,
}

/// Where a message I sent got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Sending,
    Sent,
    Delivered,
    Read,
    Failed,
}

impl Delivery {
    pub fn parse(status: &str) -> Option<Self> {
        Some(match status {
            "sending" => Self::Sending,
            "sent" => Self::Sent,
            "delivered" => Self::Delivered,
            "read" => Self::Read,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// The page cursor for older messages.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct Cursor {
    pub id: String,
    pub ts_ms: i64,
}

/// One line from the helper.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Connected (with my own number keys); every chat follows.
    Ready { me: Vec<String> },
    /// A chat (sent for every chat after `Ready`, then whenever one changes).
    Conversation(GmConversation),
    /// A new or changed message.
    Message(GmMessage),
    /// Another browser took the web session.
    Inactive,
    /// The phone stopped (false) or started (true) answering.
    Phone { responding: bool },
    /// The helper is exiting: `kind` is `connect_failed` or `logged_out`.
    Fatal { kind: String, message: String },
    /// The answer to request `id`.
    Reply(Reply),
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub results: Vec<Value>,
    /// `fetch`: newest first.
    #[serde(default)]
    pub messages: Vec<GmMessage>,
    /// `fetch`: where the next older page starts, if there is one.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// `open_conversation`: the chat's Google id.
    #[serde(default)]
    pub conv: String,
}

impl Reply {
    /// A delete's per-chat outcomes, as the one-shot helper's.
    pub fn delete_outcomes(&self, chats: usize) -> Result<Vec<Outcome>, String> {
        if self.results.is_empty() {
            return Err(if self.message.is_empty() { "the Google Messages helper gave no answer".into() } else { self.message.clone() });
        }
        let body = json!({ "results": self.results }).to_string();
        super::phone_delete::parse_output(&body, chats)
    }
}

pub fn parse_line(line: &str) -> Option<Incoming> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    match v.get("event").and_then(Value::as_str) {
        Some("ready") => Some(Incoming::Ready {
            me: v.get("me").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect(),
        }),
        Some("conversation") => serde_json::from_value(v).ok().map(Incoming::Conversation),
        Some("message") => serde_json::from_value(v).ok().map(Incoming::Message),
        Some("session") => (s("state") == "inactive").then_some(Incoming::Inactive),
        Some("phone") => Some(Incoming::Phone { responding: v.get("responding").and_then(Value::as_bool).unwrap_or(true) }),
        Some("error") => Some(Incoming::Fatal { kind: s("error"), message: s("message") }),
        Some(_) => None,
        None => serde_json::from_value::<Reply>(v).ok().filter(|r| r.id != 0).map(Incoming::Reply),
    }
}

pub fn mark_read_request(id: u64, chat: &str) -> String {
    json!({ "id": id, "op": "mark_read", "chat": chat }).to_string()
}

pub fn delete_request(id: u64, chats: &[String]) -> String {
    json!({ "id": id, "op": "delete", "chats": chats }).to_string()
}

/// Mark a chat read by Google's id (up to its newest message).
pub fn mark_conv_read_request(id: u64, conv: &str) -> String {
    json!({ "id": id, "op": "mark_read", "conv": conv }).to_string()
}

/// A page of a chat's messages, newest first; `before` for older ones.
pub fn fetch_request(id: u64, conv: &str, count: u32, before: Option<&Cursor>) -> String {
    let mut v = json!({ "id": id, "op": "fetch", "conv": conv, "count": count });
    if let Some(c) = before {
        v["before_id"] = json!(c.id);
        v["before_ts_ms"] = json!(c.ts_ms);
    }
    v.to_string()
}

pub fn send_text_request(id: u64, conv: &str, text: &str, tmp_id: &str) -> String {
    json!({ "id": id, "op": "send_text", "conv": conv, "text": text, "tmp_id": tmp_id }).to_string()
}

/// The chat with exactly these numbers, made on the phone if new.
pub fn open_conversation_request(id: u64, numbers: &[String]) -> String {
    json!({ "id": id, "op": "open_conversation", "numbers": numbers }).to_string()
}

pub fn set_active_request(id: u64) -> String {
    json!({ "id": id, "op": "set_active" }).to_string()
}

/// The device id the Messages tab keeps Google Messages rows under.
pub fn device_id(me: &[String]) -> Option<String> {
    me.first().map(|k| format!("gm:{k}"))
}

/// A Google message as the app's message row. `thread_id` and `uid` are
/// the local ids for its chat and itself; `members` are the chat's other
/// members (every message of a group carries them all, as KDE Connect's
/// do). `unread` marks a new incoming text.
pub fn to_sms(m: &GmMessage, thread_id: i64, uid: i64, members: &[String], unread: bool) -> SmsMessage {
    let address = if !m.from_me && !m.from.is_empty() { m.from.clone() } else { members.first().cloned().unwrap_or_default() };
    let attachments = m
        .attachments
        .iter()
        .map(|a| Attachment {
            mime_type: a.mime.clone(),
            unique_identifier: format!("gm:{}", a.id),
            file_name: a.name.clone(),
            ..Default::default()
        })
        .collect();
    SmsMessage {
        uid,
        body: m.text.clone(),
        address,
        date: m.ts_ms,
        msg_type: if m.from_me { MSG_TYPE_SENT } else { MSG_TYPE_RECEIVED },
        read: i32::from(m.from_me || !unread),
        thread_id,
        attachments,
        all_addresses: members.to_vec(),
    }
}

/// A fresh id for a text sent from here; its echo carries it back.
pub fn new_tmp_id(now_ms: i64, n: u64) -> String {
    format!("tmp_pr{now_ms}{n:03}")
}

/// How long after the phone's newest message a laptop message may be and
/// still count as read by "read on the phone" (the two sides stamp the
/// same text a little differently).
pub const READ_SLACK_MS: i64 = 5_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_keys_match_the_helpers() {
        let me = vec!["3165550000".to_string()];
        assert_eq!(chat_key(&["+1 (316) 555-1111"], "", &[]), "3165551111");
        // Business chats keep their whole address; digits inside it are not a number.
        assert_eq!(chat_key::<&str>(&[], "Shop_ab3_agent@rbm.goog", &[]), "shop_ab3_agent@rbm.goog");
        // Groups drop me and sort.
        assert_eq!(chat_key(&["+13165552222", "+13165550000", "3165551111"], "", &me), "3165551111,3165552222");
        // A chat with only me stays.
        assert_eq!(chat_key(&["+13165550000"], "", &me), "3165550000");
        assert_eq!(chat_key::<&str>(&[], "", &[]), "");
    }

    #[test]
    fn parses_events_and_replies() {
        assert_eq!(parse_line(r#"{"event":"ready","me":["3165550000"]}"#), Some(Incoming::Ready { me: vec!["3165550000".into()] }));
        assert_eq!(parse_line(r#"{"event":"ready"}"#), Some(Incoming::Ready { me: vec![] }));
        assert_eq!(
            parse_line(r#"{"event":"conversation","chat":"3165551111","unread":false,"last_ts_ms":17}"#),
            Some(Incoming::Conversation(GmConversation { chat: "3165551111".into(), last_ts_ms: 17, ..Default::default() }))
        );
        assert_eq!(parse_line(r#"{"event":"session","state":"inactive"}"#), Some(Incoming::Inactive));
        assert_eq!(
            parse_line(r#"{"event":"error","error":"logged_out","message":"x"}"#),
            Some(Incoming::Fatal { kind: "logged_out".into(), message: "x".into() })
        );
        let Some(Incoming::Reply(r)) = parse_line(r#"{"id":4,"ok":false,"error":"not_found","message":"m"}"#) else { panic!() };
        assert_eq!((r.id, r.ok, r.error.as_str()), (4, false, "not_found"));
        assert_eq!(parse_line("not json"), None);
        assert_eq!(parse_line(r#"{"ok":false,"error":"usage"}"#), None);
    }

    #[test]
    fn delete_replies_become_outcomes() {
        let Some(Incoming::Reply(r)) =
            parse_line(r#"{"id":2,"ok":false,"results":[{"chat":"1","ok":true},{"chat":"2","ok":false,"error":"not_found"}]}"#)
        else {
            panic!()
        };
        assert_eq!(r.delete_outcomes(2), Ok(vec![Outcome::Deleted, Outcome::NotMatched]));
        let failed = Reply { id: 3, message: "helper stopped".into(), ..Default::default() };
        assert_eq!(failed.delete_outcomes(1), Err("helper stopped".into()));
    }

    #[test]
    fn parses_chats_messages_and_fetch_pages() {
        let line = r#"{"event":"conversation","chat":"3165551111,3165552222","conv":"12","name":"Family","members":[{"number":"+13165551111","name":"Al"},{"number":"+13165552222"}],"group":true,"rcs":true,"unread":true,"last_ts_ms":5,"preview":"hi"}"#;
        let Some(Incoming::Conversation(c)) = parse_line(line) else { panic!() };
        assert_eq!((c.conv.as_str(), c.group, c.rcs, c.unread, c.name.as_str()), ("12", true, true, true, "Family"));
        assert_eq!(c.addresses(), vec!["+13165551111".to_string(), "+13165552222".into()]);
        assert_eq!(c.members[0].name, "Al");

        let line = r#"{"event":"message","conv":"12","msg":"900","tmp_id":"tmp_1","ts_ms":7,"from_me":true,"text":"yo","status":"delivered","rcs":true}"#;
        let Some(Incoming::Message(m)) = parse_line(line) else { panic!() };
        assert_eq!((m.msg.as_str(), m.from_me, m.status.as_str(), m.tmp_id.as_str()), ("900", true, "delivered", "tmp_1"));
        assert_eq!(Delivery::parse(&m.status), Some(Delivery::Delivered));
        assert_eq!(Delivery::parse("received"), None);

        let line = r#"{"id":9,"ok":true,"messages":[{"conv":"12","msg":"2","ts_ms":2,"text":"b","status":"received"},{"conv":"12","msg":"1","ts_ms":1,"text":"a","status":"received"}],"cursor":{"id":"1","ts_ms":1}}"#;
        let Some(Incoming::Reply(r)) = parse_line(line) else { panic!() };
        assert_eq!(r.messages.len(), 2);
        assert_eq!(r.cursor, Some(Cursor { id: "1".into(), ts_ms: 1 }));
    }

    #[test]
    fn requests_are_one_json_line() {
        let v: Value = serde_json::from_str(&fetch_request(3, "12", 50, Some(&Cursor { id: "1".into(), ts_ms: 9 }))).unwrap();
        assert_eq!((v["op"].as_str(), v["conv"].as_str(), v["count"].as_u64(), v["before_id"].as_str(), v["before_ts_ms"].as_i64()), (Some("fetch"), Some("12"), Some(50), Some("1"), Some(9)));
        let v: Value = serde_json::from_str(&fetch_request(3, "12", 50, None)).unwrap();
        assert!(v.get("before_id").is_none());
        let line = send_text_request(4, "12", "line one\nline \"two\"", "tmp_x");
        assert!(!line.contains('\n'));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["text"].as_str(), Some("line one\nline \"two\""));
        let v: Value = serde_json::from_str(&open_conversation_request(5, &["+13165551111".into()])).unwrap();
        assert_eq!((v["op"].as_str(), v["numbers"][0].as_str()), (Some("open_conversation"), Some("+13165551111")));
        let Some(Incoming::Reply(r)) = parse_line(r#"{"id":5,"ok":true,"conv":"44"}"#) else { panic!() };
        assert_eq!(r.conv, "44");
        assert_eq!(device_id(&["3165550000".into()]), Some("gm:3165550000".into()));
        assert_eq!(device_id(&[]), None);
    }

    #[test]
    fn messages_become_rows() {
        let members = vec!["+13165551111".to_string(), "+13165552222".to_string()];
        let theirs = GmMessage { msg: "1".into(), ts_ms: 10, from: "+13165552222".into(), text: "hi".into(), status: "received".into(), ..Default::default() };
        let row = to_sms(&theirs, 4, 7, &members, true);
        assert_eq!((row.uid, row.thread_id, row.address.as_str(), row.msg_type, row.read, row.date), (7, 4, "+13165552222", MSG_TYPE_RECEIVED, 0, 10));
        assert_eq!(row.all_addresses, members);
        let mine = GmMessage { from_me: true, status: "sent".into(), ..theirs.clone() };
        let row = to_sms(&mine, 4, 8, &members, true);
        assert_eq!((row.address.as_str(), row.msg_type, row.read), ("+13165551111", MSG_TYPE_SENT, 1));
        let pic = GmMessage { attachments: vec![GmAttachment { id: "m1".into(), mime: "image/jpeg".into(), ..Default::default() }], ..theirs };
        assert_eq!(to_sms(&pic, 4, 9, &members, false).attachments[0].unique_identifier, "gm:m1");
    }
}
