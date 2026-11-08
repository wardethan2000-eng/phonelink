//! Canonical SMS conversation reconciliation (port of `phonelink/reconcile.py`).
//!
//! Conversations are normalized once, on ingest, into a stable local model.
//! Every message is keyed by a canonical participant-set identity (the sorted
//! set of last-10-digit phone keys, with the user's own number removed for
//! group threads). Two phone thread ids that share an identity collapse into
//! one [`Conversation`] and stay collapsed.
//!
//! Differences from Python worth knowing:
//! - [`IngestResult`] carries the primary thread id (`primary`) instead of a
//!   `Conversation` reference; fetch it with [`ConversationIndex::get`].
//! - [`ConversationIndex::register`] returns the surviving primary thread id.
//! - Iteration order of `conversations` is unspecified (HashMap);
//!   `secondary_threads` and `remove` return sorted ids for determinism.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;

use super::models::{Conversation, SmsMessage};

/// Turns (addresses, primary address) into a display name.
pub type NameResolver<'a> = &'a dyn Fn(&[String], &str) -> String;

/// Digits only (ASCII, like `contacts::normalize_phone` and the helper).
fn normalize_phone(number: &str) -> String {
    static NON_DIGIT: OnceLock<Regex> = OnceLock::new();
    let re = NON_DIGIT.get_or_init(|| Regex::new(r"[^0-9]").expect("static regex"));
    re.replace_all(number, "").into_owned()
}

/// Last 10 characters (Python `s[-10:]` if `len(s) >= 10`, else `s`).
fn last_ten(s: &str) -> String {
    let count = s.chars().count();
    if count >= 10 {
        s.chars().skip(count - 10).collect()
    } else {
        s.to_string()
    }
}

/// The comparison key for a phone number (last 10 digits). Business chats
/// (`name_agent@rbm.goog`) and e-mail addresses have no number: their key
/// is the whole address in lower case, so two agents whose names share
/// stray digits stay apart.
pub fn phone_key(number: &str) -> String {
    if number.contains('@') {
        return number.trim().to_lowercase();
    }
    last_ten(&normalize_phone(number))
}

/// `list(addresses or []) + ([fallback] if fallback else [])`.
fn raw_list<'a, S: AsRef<str>>(addresses: &'a [S], fallback_address: &'a str) -> Vec<&'a str> {
    let mut raw: Vec<&str> = addresses.iter().map(|s| s.as_ref()).collect();
    if !fallback_address.is_empty() {
        raw.push(fallback_address);
    }
    raw
}

/// Ordered, de-duplicated participant keys, with self removed for groups.
pub fn participant_keys<S: AsRef<str>>(
    addresses: &[S],
    fallback_address: &str,
    self_key: &str,
) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for raw in raw_list(addresses, fallback_address) {
        let key = phone_key(raw);
        if !key.is_empty() && !keys.contains(&key) {
            keys.push(key);
        }
    }
    if !self_key.is_empty() && keys.len() > 1 {
        let stripped: Vec<String> = keys.iter().filter(|k| k.as_str() != self_key).cloned().collect();
        if !stripped.is_empty() {
            keys = stripped;
        }
    }
    keys
}

/// Original address strings, de-duplicated by key, with self removed.
pub fn participant_addresses<S: AsRef<str>>(
    addresses: &[S],
    fallback_address: &str,
    self_key: &str,
) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for raw in raw_list(addresses, fallback_address) {
        let key = phone_key(raw);
        if key.is_empty() || seen.contains(&key) {
            continue;
        }
        seen.push(key);
        result.push(raw.to_string());
    }
    if !self_key.is_empty() && seen.len() > 1 {
        let stripped: Vec<String> = result
            .iter()
            .zip(seen.iter())
            .filter(|(_, key)| key.as_str() != self_key)
            .map(|(raw, _)| raw.clone())
            .collect();
        if !stripped.is_empty() {
            result = stripped;
        }
    }
    result
}

/// Canonical participant-set key for a conversation. Deterministic.
pub fn conversation_identity<S: AsRef<str>>(
    addresses: &[S],
    fallback_address: &str,
    self_key: &str,
) -> String {
    let mut keys = participant_keys(addresses, fallback_address, self_key);
    keys.sort();
    keys.join("|")
}

/// The key a conversation is merged and hidden under: its participant
/// identity, or `thread:<id>` when it has no addresses at all. (Python used
/// `""` there, which merged every address-less phone thread into one
/// conversation and made them impossible to hide.)
pub fn conversation_key<S: AsRef<str>>(
    addresses: &[S],
    fallback_address: &str,
    self_key: &str,
    thread_id: i64,
) -> String {
    let identity = conversation_identity(addresses, fallback_address, self_key);
    if identity.is_empty() { format!("thread:{thread_id}") } else { identity }
}

/// A conversation's identity: its people, plus its own tag for a named
/// group (so two groups of the same people stay two chats).
pub fn identity_of(conv: &Conversation, self_key: &str) -> String {
    let key = conversation_key(&conv.addresses, &conv.address, self_key, conv.thread_id);
    if conv.separate.is_empty() { key } else { format!("{key}#{}", conv.separate) }
}

/// [`detect_self_key_with`] using Python's defaults (`min_appearances=3`, `top=5`).
pub fn detect_self_key<L, K>(message_key_lists: L, contact_keys: &[&str]) -> String
where
    L: IntoIterator,
    L::Item: IntoIterator<Item = K>,
    K: AsRef<str>,
{
    detect_self_key_with(message_key_lists, contact_keys, 3, 5)
}

/// Guess the user's own number key from parsed conversation data: a key that
/// appears in several multi-party threads, never as a single-party thread, and
/// is not a known contact.
///
/// Ties in appearance count are broken by first appearance (Python's order
/// there depends on set iteration and is arbitrary).
pub fn detect_self_key_with<L, K>(
    message_key_lists: L,
    contact_keys: &[&str],
    min_appearances: usize,
    top: usize,
) -> String
where
    L: IntoIterator,
    L::Item: IntoIterator<Item = K>,
    K: AsRef<str>,
{
    let mut multi: Vec<(String, usize)> = Vec::new();
    let mut single: HashSet<String> = HashSet::new();
    for keys in message_key_lists {
        let mut uniq: Vec<String> = Vec::new();
        for k in keys {
            let k = k.as_ref();
            if !k.is_empty() && !uniq.iter().any(|u| u == k) {
                uniq.push(k.to_string());
            }
        }
        if uniq.len() == 1 {
            single.insert(uniq.swap_remove(0));
        } else if uniq.len() >= 2 {
            for key in uniq {
                match multi.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, n)) => *n += 1,
                    None => multi.push((key, 1)),
                }
            }
        }
    }

    let contact_key_set: HashSet<String> = contact_keys.iter().map(|k| last_ten(k)).collect();
    // Stable sort: equal counts keep first-appearance order.
    multi.sort_by_key(|m| std::cmp::Reverse(m.1));
    for (num, count) in multi.into_iter().take(top) {
        if count < min_appearances {
            break;
        }
        if single.contains(&num) || contact_key_set.contains(&num) {
            continue;
        }
        return num;
    }
    String::new()
}

/// What [`ConversationIndex::ingest`] did, so the caller can react.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestResult {
    /// Primary thread id of the conversation the message landed in.
    pub primary: i64,
    /// A brand-new Conversation was created.
    pub created: bool,
    /// A non-duplicate message was appended.
    pub message_added: bool,
    /// Message advanced (or tied) the conversation's newest date.
    pub is_latest: bool,
    /// Message is strictly newer than the previous newest.
    pub is_newer: bool,
}

/// Owns the merged conversation model, keyed by canonical identity.
///
/// `conversations` maps a stable primary thread id to its merged
/// [`Conversation`]; `thread_to_primary` maps any phone thread id to its
/// primary. Main-thread only.
#[derive(Debug, Default)]
pub struct ConversationIndex {
    pub conversations: HashMap<i64, Conversation>,
    pub identity_to_primary: HashMap<String, i64>,
    pub thread_to_primary: HashMap<i64, i64>,
    pub self_key: String,
}

impl ConversationIndex {
    pub fn new() -> Self {
        Self::default()
    }

    // ── lookups ────────────────────────────────────────────────────

    pub fn primary_for(&self, thread_id: i64) -> i64 {
        self.thread_to_primary.get(&thread_id).copied().unwrap_or(thread_id)
    }

    pub fn get(&self, thread_id: i64) -> Option<&Conversation> {
        self.conversations.get(&self.primary_for(thread_id))
    }

    pub fn get_mut(&mut self, thread_id: i64) -> Option<&mut Conversation> {
        let primary = self.primary_for(thread_id);
        self.conversations.get_mut(&primary)
    }

    /// Thread ids merged into `primary` other than `primary` itself (sorted).
    pub fn secondary_threads(&self, primary: i64) -> Vec<i64> {
        let mut out: Vec<i64> = self
            .thread_to_primary
            .iter()
            .filter(|&(&tid, &p)| p == primary && tid != primary)
            .map(|(&tid, _)| tid)
            .collect();
        out.sort_unstable();
        out
    }

    /// Conversations for display — a pure, side-effect-free filter.
    pub fn visible(&self, is_hidden: Option<&dyn Fn(&Conversation) -> bool>) -> Vec<&Conversation> {
        match is_hidden {
            None => self.conversations.values().collect(),
            Some(hidden) => self.conversations.values().filter(|c| !hidden(c)).collect(),
        }
    }

    // ── mutation ───────────────────────────────────────────────────

    /// Clears the maps; like Python, `self_key` is kept.
    pub fn clear(&mut self) {
        self.conversations.clear();
        self.identity_to_primary.clear();
        self.thread_to_primary.clear();
    }

    /// Set the detected self number; true if it changed.
    pub fn set_self_key(&mut self, key: &str) -> bool {
        if key == self.self_key {
            return false;
        }
        self.self_key = key.to_string();
        true
    }

    /// Merge a parsed message into its canonical conversation.
    pub fn ingest(
        &mut self,
        msg: SmsMessage,
        all_addresses: Option<&[String]>,
        name_for: NameResolver,
    ) -> IngestResult {
        let raw_addrs: Vec<String> = match all_addresses {
            Some(a) if !a.is_empty() => a.to_vec(),
            _ if !msg.address.is_empty() => vec![msg.address.clone()],
            _ => Vec::new(),
        };
        let identity = conversation_key(&raw_addrs, &msg.address, &self.self_key, msg.thread_id);
        let addrs = participant_addresses(&raw_addrs, &msg.address, &self.self_key);

        let mut created = false;
        let primary = match self.identity_to_primary.get(&identity) {
            Some(&p) => p,
            None => {
                let primary = msg.thread_id;
                let address = addrs.first().cloned().unwrap_or_else(|| msg.address.clone());
                let conv = Conversation {
                    thread_id: primary,
                    identity: identity.clone(),
                    display_name: name_for(&addrs, &address),
                    address,
                    addresses: addrs.clone(),
                    thread_ids: vec![primary],
                    ..Default::default()
                };
                self.conversations.insert(primary, conv);
                self.identity_to_primary.insert(identity, primary);
                created = true;
                primary
            }
        };

        // Record the (possibly secondary) phone thread this message came from.
        self.thread_to_primary.insert(msg.thread_id, primary);
        let conv = self
            .conversations
            .get_mut(&primary)
            .expect("identity_to_primary points at a live conversation");
        if !conv.thread_ids.contains(&msg.thread_id) {
            conv.thread_ids.push(msg.thread_id);
        }

        // A later message can reveal more participants than we first saw.
        if addrs.len() > conv.addresses.len() {
            conv.addresses = addrs.clone();
            if let Some(first) = addrs.first() {
                conv.address = first.clone();
            }
            conv.display_name = name_for(&conv.addresses, &conv.address);
        }

        let date = msg.date;
        let body_for_latest = msg.body.clone();
        let mut message_added = false;
        if msg.uid != 0 && conv.messages.iter().all(|m| m.uid != msg.uid) {
            conv.messages.push(msg);
            message_added = true;
        }

        let prev_last = conv.last_date;
        let is_latest = date >= prev_last;
        let is_newer = date > prev_last;
        if is_latest {
            conv.last_date = date;
            conv.last_message = body_for_latest;
        }

        IngestResult { primary, created, message_added, is_latest, is_newer }
    }

    /// Insert an already-built conversation (e.g. from the local store).
    ///
    /// Recomputes the identity and merges into an existing conversation if one
    /// already claims it. Returns the surviving primary thread id.
    pub fn register(&mut self, mut conv: Conversation) -> i64 {
        conv.identity = identity_of(&conv, &self.self_key);
        if conv.thread_ids.is_empty() {
            conv.thread_ids = vec![conv.thread_id];
        }

        match self.identity_to_primary.get(&conv.identity).copied() {
            None => {
                let tid_primary = conv.thread_id;
                for &tid in &conv.thread_ids {
                    self.thread_to_primary.insert(tid, tid_primary);
                }
                self.thread_to_primary.insert(tid_primary, tid_primary);
                self.identity_to_primary.insert(conv.identity.clone(), tid_primary);
                self.conversations.insert(tid_primary, conv);
                tid_primary
            }
            Some(existing) => {
                let primary = self
                    .conversations
                    .get_mut(&existing)
                    .expect("identity_to_primary points at a live conversation");
                Self::absorb(&mut self.thread_to_primary, primary, conv);
                existing
            }
        }
    }

    /// Rebuild all identities and merges (after the self key changes).
    pub fn reindex(&mut self) {
        let mut convs: Vec<Conversation> = self.conversations.drain().map(|(_, c)| c).collect();
        self.clear();
        convs.sort_by_key(|c| c.thread_id);
        for conv in convs {
            self.register(conv);
        }
    }

    /// Drop a conversation (by any of its thread ids); return removed ids.
    pub fn remove(&mut self, thread_id: i64) -> Vec<i64> {
        let primary = self.primary_for(thread_id);
        if self.conversations.remove(&primary).is_none() {
            self.thread_to_primary.remove(&thread_id);
            return Vec::new();
        }
        // Every identity that led here (a merge can leave several).
        self.identity_to_primary.retain(|_, p| *p != primary);
        let mut removed: Vec<i64> = self
            .thread_to_primary
            .iter()
            .filter(|&(_, &p)| p == primary)
            .map(|(&tid, _)| tid)
            .collect();
        removed.sort_unstable();
        for tid in &removed {
            self.thread_to_primary.remove(tid);
        }
        self.thread_to_primary.remove(&primary);
        if !removed.contains(&primary) {
            removed.push(primary);
        }
        removed
    }

    // ── internals ──────────────────────────────────────────────────

    fn absorb(
        thread_to_primary: &mut HashMap<i64, i64>,
        primary: &mut Conversation,
        other: Conversation,
    ) {
        let mut existing: HashSet<i64> =
            primary.messages.iter().filter(|m| m.uid != 0).map(|m| m.uid).collect();
        let other_threads = if other.thread_ids.is_empty() {
            vec![other.thread_id]
        } else {
            other.thread_ids.clone()
        };
        for m in other.messages {
            if m.uid != 0 && !existing.contains(&m.uid) {
                existing.insert(m.uid);
                primary.messages.push(m);
            }
        }
        for tid in other_threads {
            if !primary.thread_ids.contains(&tid) {
                primary.thread_ids.push(tid);
            }
            thread_to_primary.insert(tid, primary.thread_id);
        }
        thread_to_primary.insert(other.thread_id, primary.thread_id);
        if other.last_date > primary.last_date {
            primary.last_date = other.last_date;
            primary.last_message = other.last_message;
        }
        if !other.is_read {
            primary.is_read = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_for(addresses: &[String], address: &str) -> String {
        // Trivial resolver: name is the primary address.
        if !address.is_empty() {
            address.to_string()
        } else {
            addresses.first().cloned().unwrap_or_default()
        }
    }

    /// Build a message the way the panel would, with its address list.
    fn msg(uid: i64, thread_id: i64, addrs: &[&str], date: i64) -> (SmsMessage, Vec<String>) {
        let m = SmsMessage {
            uid,
            body: format!("m{uid}"),
            address: addrs.first().map(|s| s.to_string()).unwrap_or_default(),
            date: if date == 0 { uid } else { date },
            msg_type: 1,
            read: 1,
            thread_id,
            ..Default::default()
        };
        (m, addrs.iter().map(|s| s.to_string()).collect())
    }

    fn put(idx: &mut ConversationIndex, (m, addrs): (SmsMessage, Vec<String>)) -> IngestResult {
        idx.ingest(m, Some(&addrs), &name_for)
    }

    fn uids(conv: &Conversation) -> HashSet<i64> {
        conv.messages.iter().map(|m| m.uid).collect()
    }

    // ── pure identity helpers ──────────────────────────────────────

    #[test]
    fn test_single_participant() {
        assert_eq!(conversation_identity(&["+13165551212"], "", ""), "3165551212");
    }

    #[test]
    fn test_identity_is_order_independent() {
        let a = conversation_identity(&["+13165551212", "3165559999"], "", "");
        let b = conversation_identity(&["3165559999", "+1-316-555-1212"], "", "");
        assert_eq!(a, b);
    }

    #[test]
    fn test_country_code_variants_collapse() {
        assert_eq!(
            conversation_identity(&["+13165551212"], "", ""),
            conversation_identity(&["3165551212"], "", "")
        );
    }

    #[test]
    fn test_self_number_stripped_for_groups() {
        // A 1:1 MMS thread carries [self, alice]; stripping self makes it match
        // the plain SMS thread [alice].
        let sms = conversation_identity(&["3165551111"], "", "3169990000");
        let mms = conversation_identity(&["3169990000", "3165551111"], "", "3169990000");
        assert_eq!(sms, mms);
    }

    #[test]
    fn test_self_number_not_stripped_from_one_on_one() {
        // If only self is present we keep it (never produce an empty identity).
        assert_eq!(
            conversation_identity(&["3169990000"], "", "3169990000"),
            "3169990000"
        );
    }

    #[test]
    fn test_detect_self_key() {
        // 5551110000 shows up across three group threads, never solo, unknown.
        let key_lists = vec![
            vec!["3165551111", "5551110000"],
            vec!["3165552222", "5551110000"],
            vec!["3165553333", "5551110000"],
            vec!["3165551111"], // solo thread for a real contact
        ];
        assert_eq!(detect_self_key(key_lists, &[]), "5551110000");
    }

    #[test]
    fn test_detect_self_key_ignores_known_contacts() {
        // 5551110000 is the only group-only candidate (3165551111 has a solo
        // thread), but it is a known contact, so no self number is inferred.
        let key_lists = vec![
            vec!["3165551111", "5551110000"],
            vec!["3165551111", "5551110000"],
            vec!["3165551111", "5551110000"],
            vec!["3165551111"],
        ];
        assert_eq!(detect_self_key(key_lists, &["5551110000"]), "");
    }

    // ── ingest / merge behaviour ───────────────────────────────────

    #[test]
    fn test_sms_and_mms_threads_collapse_to_one_conversation() {
        // The classic split/merge bug: same contact, two phone thread IDs.
        let mut idx = ConversationIndex::new();
        put(&mut idx, msg(1, 10, &["+13165551212"], 100));
        put(&mut idx, msg(2, 20, &["3165551212"], 200));

        assert_eq!(idx.conversations.len(), 1);
        let conv = idx.get(20).unwrap();
        assert_eq!(conv.thread_id, 10); // stable first-seen primary
        assert_eq!(uids(conv), HashSet::from([1, 2]));
        assert_eq!(idx.secondary_threads(10), vec![20]);
        assert_eq!(conv.last_date, 200);
        assert_eq!(conv.last_message, "m2");
    }

    #[test]
    fn test_threads_without_addresses_stay_separate() {
        // Seen live: two address-less phone threads. Python merged them under
        // identity "".
        let mut idx = ConversationIndex::new();
        put(&mut idx, msg(1, 719, &[], 100));
        put(&mut idx, msg(2, 720, &[], 200));
        put(&mut idx, msg(3, 719, &[], 300));
        assert_eq!(idx.conversations.len(), 2);
        assert_eq!(idx.get(719).unwrap().identity, "thread:719");
        assert_eq!(uids(idx.get(719).unwrap()), HashSet::from([1, 3]));
        assert_eq!(idx.primary_for(720), 720);
        assert_eq!(conversation_key(&["3165551212"], "", "", 5), "3165551212");
    }

    #[test]
    fn test_primary_is_stable_regardless_of_arrival_order() {
        let mut idx = ConversationIndex::new();
        put(&mut idx, msg(2, 20, &["3165551212"], 200));
        put(&mut idx, msg(1, 10, &["3165551212"], 100));
        // Whichever thread's message lands first owns the primary; it does not
        // flip when an older message arrives on the other thread.
        let conv = idx.get(10).unwrap();
        assert_eq!(conv.thread_id, 20);
        assert_eq!(conv.last_date, 200);
    }

    #[test]
    fn test_duplicate_uid_is_ignored() {
        let mut idx = ConversationIndex::new();
        put(&mut idx, msg(1, 10, &["111"], 100));
        let r = put(&mut idx, msg(1, 10, &["111"], 100));
        assert!(!r.message_added);
        assert_eq!(idx.get(10).unwrap().messages.len(), 1);
    }

    #[test]
    fn test_distinct_contacts_stay_separate() {
        let mut idx = ConversationIndex::new();
        put(&mut idx, msg(1, 10, &["3165551111"], 0));
        put(&mut idx, msg(2, 11, &["3165552222"], 0));
        assert_eq!(idx.conversations.len(), 2);
    }

    #[test]
    fn test_group_thread_is_not_merged_with_member() {
        let mut idx = ConversationIndex::new();
        put(&mut idx, msg(1, 10, &["3165551111"], 0));
        put(&mut idx, msg(2, 20, &["3165551111", "3165552222"], 0));
        assert_eq!(idx.conversations.len(), 2);
    }

    #[test]
    fn test_visible_filters_hidden() {
        let mut idx = ConversationIndex::new();
        put(&mut idx, msg(1, 10, &["111"], 0));
        put(&mut idx, msg(2, 20, &["222"], 0));
        let hidden = idx.get(10).unwrap();
        let is_hidden = |c: &Conversation| std::ptr::eq(c, hidden);
        let visible = idx.visible(Some(&is_hidden));
        let ids: Vec<i64> = visible.iter().map(|c| c.thread_id).collect();
        assert_eq!(ids, vec![20]);
    }

    #[test]
    fn test_visible_is_side_effect_free() {
        let mut idx = ConversationIndex::new();
        put(&mut idx, msg(1, 10, &["111"], 100));
        put(&mut idx, msg(2, 20, &["111"], 200));
        let counts = |idx: &ConversationIndex| -> HashMap<i64, usize> {
            idx.conversations.iter().map(|(&tid, c)| (tid, c.messages.len())).collect()
        };
        let before = counts(&idx);
        for _ in 0..5 {
            idx.visible(None);
        }
        let after = counts(&idx);
        assert_eq!(before, after);
        assert_eq!(idx.conversations.len(), 1);
    }

    #[test]
    fn test_remove_clears_all_mappings() {
        let mut idx = ConversationIndex::new();
        put(&mut idx, msg(1, 10, &["111"], 0));
        put(&mut idx, msg(2, 20, &["111"], 0));
        let removed = idx.remove(20); // remove by secondary id
        assert_eq!(removed.into_iter().collect::<HashSet<_>>(), HashSet::from([10, 20]));
        assert!(idx.conversations.is_empty());
        assert!(idx.thread_to_primary.is_empty());
        assert!(idx.identity_to_primary.is_empty());
    }

    // ── register / reindex ─────────────────────────────────────────

    #[test]
    fn test_register_collapses_legacy_duplicates() {
        let mut idx = ConversationIndex::new();
        // Two persisted rows for the same number (a legacy split).
        idx.register(Conversation {
            thread_id: 10,
            address: "3165551212".into(),
            addresses: vec!["3165551212".into()],
            last_date: 100,
            last_message: "a".into(),
            is_read: true,
            messages: vec![SmsMessage { uid: 1, date: 100, thread_id: 10, ..Default::default() }],
            ..Default::default()
        });
        idx.register(Conversation {
            thread_id: 20,
            address: "+13165551212".into(),
            addresses: vec!["+13165551212".into()],
            last_date: 200,
            last_message: "b".into(),
            is_read: false,
            messages: vec![SmsMessage { uid: 2, date: 200, thread_id: 20, ..Default::default() }],
            ..Default::default()
        });
        assert_eq!(idx.conversations.len(), 1);
        let conv = idx.get(20).unwrap();
        assert_eq!(conv.thread_id, 10);
        assert_eq!(uids(conv), HashSet::from([1, 2]));
        assert_eq!(conv.last_message, "b"); // newest wins
        assert!(!conv.is_read); // unread preserved
    }

    #[test]
    fn test_reindex_merges_after_self_key_learned() {
        let mut idx = ConversationIndex::new();
        // Before we know the self number, a 1:1 SMS and its MMS look different.
        put(&mut idx, msg(1, 10, &["3165551111"], 0));
        put(&mut idx, msg(2, 20, &["3169990000", "3165551111"], 0));
        assert_eq!(idx.conversations.len(), 2);
        // Learn the self number and re-key — they collapse into one.
        assert!(idx.set_self_key("3169990000"));
        idx.reindex();
        assert_eq!(idx.conversations.len(), 1);
    }
}
