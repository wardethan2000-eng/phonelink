//! The lasting Google Messages link (`phonelink-gm serve`): its line
//! protocol and the chat keys both sides match chats by.
//!
//! KDE Connect can't mark chats read on the phone, and never tells us when
//! the phone read one. The helper keeps one Google Messages web connection
//! open: the app asks it to mark a chat read, and it reports each chat's
//! unread state as it changes.

use serde::Deserialize;
use serde_json::{Value, json};

use super::phone_delete::Outcome;
use super::reconcile::phone_key;

/// A chat member's match key, as the helper's `phoneKey`: the last 10
/// digits, or, for business chats and e-mail addresses (anything with
/// `@`), the whole address in lower case.
pub fn member_key(address: &str) -> String {
    if address.contains('@') {
        return address.trim().to_lowercase();
    }
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

/// One line from the helper.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Connected (with my own number keys); every chat's state follows.
    Ready { me: Vec<String> },
    /// A chat's state on the phone (sent for every chat after `Ready`,
    /// then whenever one changes).
    Conversation { chat: String, unread: bool, last_ts_ms: i64 },
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
        Some("conversation") => Some(Incoming::Conversation {
            chat: s("chat"),
            unread: v.get("unread").and_then(Value::as_bool).unwrap_or(true),
            last_ts_ms: v.get("last_ts_ms").and_then(Value::as_i64).unwrap_or(0),
        }),
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
            Some(Incoming::Conversation { chat: "3165551111".into(), unread: false, last_ts_ms: 17 })
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
}
