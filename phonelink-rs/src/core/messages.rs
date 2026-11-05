//! The Messages tab's logic, without the widgets (port of the non-GTK parts
//! of `phonelink/ui/sms_panel.py`).
//!
//! [`Messages`] owns the conversation index, read state, drafts and the
//! catch-up machine. Each event method returns [`Effect`]s: D-Bus requests to
//! send and UI updates to make. The UI layer runs them; nothing here blocks or
//! touches GTK. The store and the hidden-conversation list are passed in so
//! tests can use an in-memory store and a fake.

use std::collections::{HashMap, HashSet};

use super::attachments;
use super::contacts::{is_messaging_app, looks_like_phone_number, normalize_phone, resolve_name};
use super::hidden_texts::{body_matches, normalize_text};
use super::message::{RawMessage, parse_message};
use super::models::{Conversation, SmsMessage};
use super::reconcile::{self, ConversationIndex};
use super::settings::Settings;
use super::store::MessageStore;
use super::sync::{self, CatchUp};

/// Where locally deleted conversations are remembered (`settings.json`).
pub trait HiddenStore {
    fn hidden_until(&self, device_id: &str, key: &str) -> i64;
    fn hide(&mut self, device_id: &str, key: &str, deleted_at: i64);
    fn unhide(&mut self, device_id: &str, key: &str);
}

impl HiddenStore for Settings {
    fn hidden_until(&self, device_id: &str, key: &str) -> i64 {
        self.conversation_hidden_until(device_id, key)
    }
    fn hide(&mut self, device_id: &str, key: &str, deleted_at: i64) {
        if let Err(e) = self.hide_conversation(device_id, key, deleted_at) {
            log::warn!("saving a hidden conversation failed: {e}");
        }
    }
    fn unhide(&mut self, device_id: &str, key: &str) {
        if let Err(e) = self.unhide_conversation(device_id, key) {
            log::warn!("unhiding a conversation failed: {e}");
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// A catch-up / gap-fill request (see [`sync::Action`]).
    Sync(sync::Action),
    /// `markConversationAsRead(thread)` — absent in daemon 26.08; ignore
    /// `UnknownMethod`.
    MarkRead { thread_id: i64 },
    /// `replyToConversation(conversation_id, text, attachments)` — the only
    /// send the S25 reliably delivers.
    Reply { conversation_id: i64, text: String, attachments: Vec<String> },
    /// `notifications.sendReply(reply_id, text)`: answer through the texting
    /// app's own notification, so the app (Google Messages) sends it, as RCS
    /// in an RCS thread (Phase 9 route A). If the call fails, the UI calls
    /// [`Messages::notification_reply_failed`].
    ReplyViaNotification { thread_id: i64, reply_id: String, text: String },
    /// A notification reply got no clear answer (e.g. a D-Bus timeout): the
    /// phone may already have sent it, so nothing is resent. The UI waits
    /// briefly for the phone's echo ([`Messages::awaiting_echo`]), then
    /// shows "may not have sent" with a retry button.
    MaybeNotSent { thread_id: i64, text: String },
    /// `sendWithoutConversation(addresses, text)` — only for brand-new
    /// conversations; the phone silently drops it otherwise.
    SendNew { addresses: Vec<String>, text: String, attachments: Vec<String> },
    /// `deleteConversation(thread)` on the phone.
    DeleteOnPhone { thread_id: i64 },
    RefreshList,
    ShowThread(i64),
    ShowEmpty,
    Toast(&'static str),
}

pub const TOAST_NOT_CONNECTED: &str = "Can't send — your phone isn't connected right now";
pub const TOAST_LOCAL_DELETE_ONLY: &str =
    "Removed from Phone Link only; phone-side deletion is not available on this KDE Connect version";

/// How a text sent from this app left it. Shown under the sent bubble so a
/// fallback to SMS is never silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendRoute {
    /// `replyToConversation`: KDE Connect sends through Android's SMS API.
    Sms,
    /// Through the texting app's notification reply. Labeled "via
    /// notification", not "RCS", until a phone test confirms it goes out as
    /// RCS.
    Notification,
}

impl SendRoute {
    pub fn label(self) -> &'static str {
        match self {
            SendRoute::Sms => "SMS",
            SendRoute::Notification => "via notification",
        }
    }
}

/// A live texting-app notification the phone lets us answer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReplyableNotification {
    pub public_id: String,
    pub app_name: String,
    pub title: String,
    /// The message preview (`contacts::notification_message_text`).
    pub text: String,
    pub reply_id: String,
}

/// Sends whose phone echo has not arrived yet: (conversation, text, route).
const PENDING_ROUTES_MAX: usize = 32;

#[derive(Debug, Default)]
pub struct Messages {
    device_id: Option<String>,
    reachable: bool,
    pub index: ConversationIndex,
    pub contact_map: HashMap<String, String>,
    read_thread_ids: HashSet<i64>,
    active: Option<i64>,
    next_draft_id: i64,
    pub catch_up: CatchUp,
    notifications: Vec<ReplyableNotification>,
    pending_routes: Vec<(i64, String, SendRoute)>,
    sent_routes: HashMap<i64, SendRoute>,
    /// Attachments whose full file was asked for this session.
    full_requested: HashSet<String>,
}

/// Display name for a conversation from the contact map.
pub fn name_for(contact_map: &HashMap<String, String>, addresses: &[String], address: &str) -> String {
    if addresses.len() > 1 {
        return addresses.iter().map(|a| resolve_name(contact_map, a)).collect::<Vec<_>>().join(", ");
    }
    let target = if !address.is_empty() { address } else { addresses.first().map_or("", String::as_str) };
    resolve_name(contact_map, target)
}

/// Same number, allowing for country codes (last 10 digits).
fn same_number(a_norm: &str, b_norm: &str) -> bool {
    // By chars: `normalize_phone` keeps any Unicode digit, not only ASCII.
    let last10 = |s: &str| s.chars().rev().take(10).collect::<String>();
    a_norm == b_norm
        || (a_norm.chars().count() >= 10 && b_norm.chars().count() >= 10 && last10(a_norm) == last10(b_norm))
}

impl Messages {
    pub fn new() -> Self {
        Self { next_draft_id: -1, ..Default::default() }
    }

    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }

    pub fn active_thread(&self) -> Option<i64> {
        self.active
    }

    /// A different device became active: forget everything from the last one.
    pub fn reset_for_device(&mut self, device_id: Option<&str>, reachable: bool) {
        self.device_id = device_id.map(str::to_string);
        self.index.clear();
        self.read_thread_ids.clear();
        self.active = None;
        self.catch_up.cancel();
        self.notifications.clear();
        self.pending_routes.clear();
        self.sent_routes.clear();
        self.full_requested.clear();
        self.set_reachable(reachable);
    }

    pub fn set_reachable(&mut self, reachable: bool) {
        self.reachable = reachable;
        self.catch_up.set_reachable(reachable && self.device_id.is_some());
    }

    fn conversation_key(&self, addresses: &[String], fallback: &str, thread_id: i64) -> String {
        reconcile::conversation_key(addresses, fallback, &self.index.self_key, thread_id)
    }

    fn should_keep_hidden(&self, hidden: &mut dyn HiddenStore, key: &str, message_date: i64) -> bool {
        let Some(dev) = self.device_id.as_deref() else { return false };
        if key.is_empty() {
            return false;
        }
        let until = hidden.hidden_until(dev, key);
        if until == 0 {
            return false;
        }
        if message_date > until {
            hidden.unhide(dev, key);
            return false;
        }
        true
    }

    // ── loading ────────────────────────────────────────────────────

    /// Show persisted history immediately, before the daemon responds.
    pub fn load_cached(&mut self, store: &MessageStore) -> Vec<Effect> {
        let Some(dev) = self.device_id.clone() else { return Vec::new() };
        let mut cached: Vec<Conversation> = store.load_conversations(&dev).into_values().collect();
        if cached.is_empty() {
            return Vec::new();
        }
        // Deterministic order, collapsing legacy duplicate rows.
        cached.sort_by_key(|c| c.thread_id);
        self.index.clear();
        for conv in cached {
            self.index.register(conv);
        }
        self.refresh_names();
        vec![Effect::RefreshList]
    }

    /// Re-resolve every display name (contacts changed).
    pub fn refresh_names(&mut self) {
        for conv in self.index.conversations.values_mut() {
            conv.display_name = name_for(&self.contact_map, &conv.addresses, &conv.address);
        }
    }

    /// The daemon's `activeConversations` reply (newest message per thread).
    pub fn apply_active(&mut self, raw: &[RawMessage], store: &MessageStore, hidden: &mut dyn HiddenStore, now: f64) -> Vec<Effect> {
        if self.device_id.is_none() {
            return Vec::new();
        }
        let msgs: Vec<SmsMessage> = raw.iter().map(parse_message).collect();
        self.detect_self_number(&msgs);
        let mut out = Vec::new();
        store.batch(|store| {
            for msg in &msgs {
                out.extend(self.merge_message(msg.clone(), store, hidden, now));
            }
        });
        if !msgs.is_empty() {
            out.push(Effect::RefreshList);
        }
        out
    }

    /// Learn the user's own number from multi-address threads; group
    /// identities depend on it, so re-key when it changes.
    fn detect_self_number(&mut self, msgs: &[SmsMessage]) {
        let key_lists: Vec<Vec<String>> = msgs
            .iter()
            .map(|m| m.all_addresses.iter().map(|a| reconcile::phone_key(a)).collect())
            .collect();
        let contact_keys: Vec<&str> = self.contact_map.keys().map(String::as_str).collect();
        let self_key = reconcile::detect_self_key(&key_lists, &contact_keys);
        if self.index.set_self_key(&self_key) {
            self.index.reindex();
            if let Some(active) = self.active
                && !self.index.conversations.contains_key(&active)
            {
                self.active = Some(self.index.primary_for(active));
            }
        }
    }

    // ── ingestion ──────────────────────────────────────────────────

    /// Merge one message (a signal, or one row of a bulk reply). Call inside
    /// `store.batch` when merging many.
    pub fn merge_message(&mut self, msg: SmsMessage, store: &MessageStore, hidden: &mut dyn HiddenStore, now: f64) -> Vec<Effect> {
        let mut out = Vec::new();
        let all_addrs: Vec<String> = if !msg.all_addresses.is_empty() {
            msg.all_addresses.clone()
        } else if !msg.address.is_empty() {
            vec![msg.address.clone()]
        } else {
            Vec::new()
        };

        // A locally deleted conversation stays hidden until a newer message
        // arrives — only for a conversation not already showing.
        if self.index.get(msg.thread_id).is_none() {
            let identity = self.conversation_key(&all_addrs, &msg.address, msg.thread_id);
            if !self.index.identity_to_primary.contains_key(&identity)
                && self.should_keep_hidden(hidden, &identity, msg.date)
            {
                return out;
            }
        }

        let (thread_id, date, read, is_sent) = (msg.thread_id, msg.date, msg.read, msg.is_sent());
        let stored = msg.clone();
        let contact_map = &self.contact_map;
        let resolver = |a: &[String], ad: &str| name_for(contact_map, a, ad);
        let result = self.index.ingest(msg, Some(&all_addrs), &resolver);

        // A newer latest message on a known thread means the phone may have
        // received others in between that it never sent us.
        if result.is_newer && !result.created {
            self.catch_up.queue_gap_fill(thread_id, now);
        }
        self.catch_up.note_message(thread_id, date, result.message_added);

        let dev = self.device_id.clone().unwrap_or_default();
        if result.message_added {
            store.upsert_message(&dev, result.primary, &stored);
            // The phone's copy of a text we sent: remember which way it left.
            if is_sent {
                let key = normalize_text(&stored.body);
                if let Some(i) = self.pending_routes.iter().position(|p| p.0 == result.primary && p.1 == key) {
                    let (_, _, route) = self.pending_routes.remove(i);
                    self.sent_routes.insert(stored.uid, route);
                }
            }
        }

        let primary = result.primary;
        if result.is_latest {
            let active = self.active == Some(primary);
            let was_opened = self.read_thread_ids.contains(&primary);
            let reopen_unread = result.is_newer && !is_sent && read == 0;
            if let Some(conv) = self.index.get_mut(primary) {
                if active {
                    conv.is_read = true;
                } else if !was_opened {
                    conv.is_read = read != 0;
                } else if reopen_unread {
                    // New incoming text on a chat opened earlier but not
                    // being viewed now: unread again.
                    conv.is_read = false;
                }
            }
            if !active && was_opened && reopen_unread {
                self.read_thread_ids.remove(&primary);
            }
        }
        if let Some(conv) = self.index.get(primary) {
            store.upsert_conversation(&dev, conv);
        }
        if self.active == Some(primary) {
            out.push(Effect::ShowThread(primary));
        }
        out
    }

    /// `conversationRemoved` from the daemon.
    pub fn remove_conversation(&mut self, thread_id: i64, store: &MessageStore) -> Vec<Effect> {
        let mut removed = self.index.remove(thread_id);
        if removed.is_empty() {
            removed.push(thread_id);
        }
        self.forget(&removed, store)
    }

    fn forget(&mut self, removed: &[i64], store: &MessageStore) -> Vec<Effect> {
        let mut out = Vec::new();
        if let Some(dev) = self.device_id.as_deref() {
            for &tid in removed {
                store.delete_conversation(dev, tid);
            }
        }
        for tid in removed {
            self.read_thread_ids.remove(tid);
        }
        if self.active.is_some_and(|a| removed.contains(&a)) {
            self.active = None;
            out.push(Effect::ShowEmpty);
        }
        out.push(Effect::RefreshList);
        out
    }

    /// Conversations to show (locally deleted ones filtered out), newest first.
    pub fn visible(&self, hidden: &dyn HiddenStore) -> Vec<&Conversation> {
        let dev = self.device_id.as_deref().unwrap_or("");
        let is_hidden = |c: &Conversation| {
            let key = self.conversation_key(&c.addresses, &c.address, c.thread_id);
            if key.is_empty() || dev.is_empty() {
                return false;
            }
            let until = hidden.hidden_until(dev, &key);
            until != 0 && c.sort_key() <= until
        };
        let mut v = self.index.visible(Some(&is_hidden));
        v.sort_by_key(|c| std::cmp::Reverse(c.sort_key()));
        v
    }

    // ── user actions ───────────────────────────────────────────────

    /// The user opened a conversation.
    pub fn select(&mut self, thread_id: i64, store: &MessageStore, now: f64) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.active == Some(thread_id) {
            return out;
        }
        self.active = Some(thread_id);
        let Some(conv) = self.index.get_mut(thread_id) else { return out };
        self.read_thread_ids.insert(thread_id);
        let was_unread = !conv.is_read;
        conv.is_read = true;
        if was_unread {
            out.push(Effect::RefreshList);
            if let Some(dev) = self.device_id.as_deref() {
                store.upsert_conversation(dev, conv);
            }
        }
        // Drafts don't exist on the phone.
        if thread_id < 0 {
            out.push(Effect::ShowThread(thread_id));
            return out;
        }
        if was_unread && self.device_id.is_some() {
            out.push(Effect::MarkRead { thread_id });
        }
        // Show cached messages now, then pull the thread (and any phone
        // threads merged into it) so missed texts fill in.
        out.push(Effect::ShowThread(thread_id));
        let threads = self.conversation_threads(thread_id);
        self.catch_up.queue_conversation_gap_fill(&threads, now);
        out
    }

    /// The conversation's primary phone thread followed by merged ones.
    pub fn conversation_threads(&self, thread_id: i64) -> Vec<i64> {
        let primary = self.index.primary_for(thread_id);
        let mut v = vec![primary];
        v.extend(self.index.secondary_threads(primary));
        v
    }

    /// Start a conversation from contact search: open the 1:1 thread for that
    /// number, or a draft already started for it, or create a draft
    /// (negative id). Group chats never count, even when the number is
    /// their first member (Python opened the group).
    pub fn open_or_create(&mut self, address: &str, display_name: &str) -> Vec<Effect> {
        if self.device_id.is_none() || !self.reachable {
            return Vec::new();
        }
        let norm = normalize_phone(address);
        let existing = self.find_thread_for_address(address).or_else(|| {
            self.index
                .conversations
                .values()
                .filter(|c| c.thread_id < 0)
                .find(|c| same_number(&normalize_phone(&c.address), &norm))
                .map(|c| c.thread_id)
        });
        if let Some(tid) = existing {
            self.active = Some(tid);
            self.read_thread_ids.insert(tid);
            if let Some(c) = self.index.get_mut(tid) {
                c.is_read = true;
            }
            return vec![Effect::RefreshList, Effect::ShowThread(tid)];
        }
        let draft_id = self.next_draft_id;
        self.next_draft_id -= 1;
        let mut conv = Conversation::new(draft_id);
        conv.address = address.to_string();
        conv.display_name = if display_name != address { display_name.to_string() } else { String::new() };
        self.index.conversations.insert(draft_id, conv);
        self.active = Some(draft_id);
        vec![Effect::RefreshList, Effect::ShowThread(draft_id)]
    }

    /// An existing real 1:1 thread for a number.
    pub fn find_thread_for_address(&self, address: &str) -> Option<i64> {
        let norm = normalize_phone(address);
        if norm.is_empty() {
            return None;
        }
        self.index
            .conversations
            .values()
            .filter(|c| c.thread_id >= 0 && !c.is_group())
            .find(|c| same_number(&normalize_phone(&c.address), &norm))
            .map(|c| c.thread_id)
    }

    /// Send from the compose bar.
    pub fn send(&mut self, thread_id: i64, text: &str) -> Vec<Effect> {
        self.send_with(thread_id, text, Vec::new())
    }

    /// Send text and/or attachments (local file paths, as Python passes
    /// them). A text with attachments always goes the usual way: a
    /// notification reply can only carry text.
    pub fn send_with(&mut self, thread_id: i64, text: &str, attachments: Vec<String>) -> Vec<Effect> {
        if self.device_id.is_none() || !self.reachable {
            return vec![Effect::Toast(TOAST_NOT_CONNECTED)];
        }
        if text.trim().is_empty() && attachments.is_empty() {
            return Vec::new();
        }
        if thread_id >= 0 {
            return vec![self.send_existing(thread_id, text, attachments)];
        }
        // A draft: a real thread may have loaded since it was created.
        let Some(draft) = self.index.conversations.get(&thread_id) else { return Vec::new() };
        let address = draft.address.clone();
        self.index.conversations.remove(&thread_id);
        match self.find_thread_for_address(&address) {
            Some(existing) => {
                self.active = Some(existing);
                self.read_thread_ids.insert(existing);
                vec![self.send_existing(existing, text, attachments), Effect::RefreshList, Effect::ShowThread(existing)]
            }
            None => {
                self.active = None;
                vec![
                    Effect::SendNew { addresses: vec![address], text: text.to_string(), attachments },
                    Effect::ShowEmpty,
                    Effect::RefreshList,
                ]
            }
        }
    }

    /// A text for an existing thread: through the texting app's notification
    /// when one clearly belongs to this conversation, else `replyToConversation`.
    fn send_existing(&mut self, thread_id: i64, text: &str, attachments: Vec<String>) -> Effect {
        let primary = self.index.primary_for(thread_id);
        let notification = if attachments.is_empty() { self.notification_for(primary) } else { None };
        let (route, effect) = match notification {
            Some(n) => (
                SendRoute::Notification,
                Effect::ReplyViaNotification { thread_id: primary, reply_id: n.reply_id.clone(), text: text.to_string() },
            ),
            None => (SendRoute::Sms, self.reply(primary, text, attachments)),
        };
        self.note_pending_route(primary, text, route);
        effect
    }

    fn note_pending_route(&mut self, primary: i64, text: &str, route: SendRoute) {
        self.pending_routes.push((primary, normalize_text(text), route));
        if self.pending_routes.len() > PENDING_ROUTES_MAX {
            self.pending_routes.remove(0);
        }
    }

    /// `sendReply` failed. Only a definite refusal (`refused`: the
    /// notification or method is gone, or the arguments were rejected) is
    /// resent the usual way and labeled SMS. Anything else, such as a
    /// timeout, may have reached the phone already, and resending would send
    /// the text twice.
    pub fn notification_reply_failed(&mut self, thread_id: i64, text: &str, refused: bool) -> Vec<Effect> {
        if !refused {
            return vec![Effect::MaybeNotSent { thread_id, text: text.to_string() }];
        }
        let key = normalize_text(text);
        if let Some(p) = self.pending_routes.iter_mut().rev().find(|p| p.0 == thread_id && p.1 == key) {
            p.2 = SendRoute::Sms;
        }
        vec![self.reply(thread_id, text, Vec::new())]
    }

    /// True while a text sent to this conversation has not come back from the
    /// phone yet.
    pub fn awaiting_echo(&self, thread_id: i64, text: &str) -> bool {
        let key = normalize_text(text);
        self.pending_routes.iter().any(|p| p.0 == thread_id && p.1 == key)
    }

    /// How a sent message (by uid) left this app, if it was sent from here.
    pub fn route_of(&self, uid: i64) -> Option<SendRoute> {
        self.sent_routes.get(&uid).copied()
    }

    // ── full-resolution images ─────────────────────────────────────

    /// Image attachments in a conversation that only have the thumbnail, to
    /// ask the phone for (`requestAttachmentFile(part_id, unique_id)`). Each
    /// is returned once per session, and none while the phone is away.
    pub fn take_full_image_requests(&mut self, thread_id: i64, cache_dir: &std::path::Path) -> Vec<(i64, String)> {
        if !self.reachable || thread_id < 0 {
            return Vec::new();
        }
        let Some(conv) = self.index.get(thread_id) else { return Vec::new() };
        let mut out = Vec::new();
        for att in conv.messages.iter().flat_map(|m| &m.attachments) {
            if !attachments::is_image(att) || att.part_id == 0 || attachments::existing_full_in(cache_dir, att).is_some() {
                continue;
            }
            let key = attachments::attachment_key(att);
            if self.full_requested.insert(key) {
                out.push((att.part_id, att.unique_identifier.clone()));
            }
        }
        out
    }

    /// The phone sent a file we asked for. If it is an inline image's full
    /// version, copy it into the cache, record it on the message, persist,
    /// and return the conversation to redraw. `None` for anything else (a
    /// plain download: the caller says where it landed).
    pub fn attachment_received(
        &mut self,
        file_path: &std::path::Path,
        file_name: &str,
        store: &MessageStore,
        cache_dir: &std::path::Path,
    ) -> Option<i64> {
        if !file_path.is_file() {
            return None;
        }
        let dev = self.device_id.clone()?;
        for conv in self.index.conversations.values_mut() {
            for msg in conv.messages.iter_mut() {
                let Some(att) = msg
                    .attachments
                    .iter_mut()
                    .find(|a| attachments::is_image(a) && attachments::received_matches(a, file_name))
                else {
                    continue;
                };
                let dest = attachments::full_cache_path_in(cache_dir, att)?;
                let copied = std::fs::create_dir_all(cache_dir).and_then(|_| std::fs::copy(file_path, &dest));
                if let Err(e) = copied {
                    log::warn!("caching a full attachment failed: {e}");
                    return None;
                }
                att.extra.insert("fullPath".into(), dest.to_string_lossy().into_owned().into());
                store.upsert_message(&dev, conv.thread_id, msg);
                return Some(conv.thread_id);
            }
        }
        None
    }

    // ── notifications (send route A) ──────────────────────────────

    /// A notification was posted or updated. Kept only if it comes from a
    /// texting app and can be answered.
    pub fn note_notification(&mut self, n: ReplyableNotification) {
        self.notifications.retain(|o| o.public_id != n.public_id);
        if is_messaging_app(&n.app_name) && !n.reply_id.is_empty() && !n.text.is_empty() {
            self.notifications.push(n);
        }
    }

    pub fn remove_notification(&mut self, public_id: &str) {
        self.notifications.retain(|n| n.public_id != public_id);
    }

    pub fn clear_notifications(&mut self) {
        self.notifications.clear();
    }

    /// A 1:1 notification's title: the number itself, or (when the desktop
    /// knows the contact) that contact's name. A name we can't check passes;
    /// the text match still has to be unique.
    fn title_fits(title: &str, conv: &Conversation) -> bool {
        if looks_like_phone_number(title) {
            return same_number(&normalize_phone(title), &normalize_phone(&conv.address));
        }
        // Without a contact the display name is just the address.
        let known_name = !conv.display_name.is_empty()
            && conv.display_name != conv.address
            && !looks_like_phone_number(&conv.display_name);
        !known_name || normalize_text(title) == normalize_text(&conv.display_name)
    }

    /// Newest text received in a conversation.
    fn newest_received(conv: &Conversation) -> Option<&SmsMessage> {
        conv.messages.iter().filter(|m| !m.is_sent() && !m.body.trim().is_empty()).max_by_key(|m| m.date)
    }

    /// The notification to answer for a conversation, only when the match is
    /// unambiguous; a wrong match would send the text to someone else.
    ///
    /// The notification's preview must quote the conversation's newest
    /// received text, and no other conversation's; for a 1:1 with a known
    /// name its title must be that name; and exactly one notification may
    /// qualify.
    fn notification_for(&self, primary: i64) -> Option<&ReplyableNotification> {
        let conv = self.index.get(primary)?;
        if conv.thread_id < 0 {
            return None;
        }
        let newest = Self::newest_received(conv)?;
        let mut candidates = self.notifications.iter().filter(|n| {
            if !body_matches(&n.text, &newest.body) {
                return false;
            }
            if !conv.is_group() && !Self::title_fits(&n.title, conv) {
                return false;
            }
            // The same preview fits another conversation too: ambiguous.
            !self.index.conversations.values().any(|other| {
                other.thread_id != conv.thread_id
                    && other.thread_id >= 0
                    && Self::newest_received(other).is_some_and(|m| body_matches(&n.text, &m.body))
            })
        });
        let found = candidates.next()?;
        candidates.next().is_none().then_some(found)
    }

    /// Reply into an existing thread, addressed by conversation id.
    pub fn reply(&self, thread_id: i64, text: &str, attachments: Vec<String>) -> Effect {
        Effect::Reply { conversation_id: self.index.primary_for(thread_id), text: text.to_string(), attachments }
    }

    /// Remember a conversation as deleted until a newer message arrives.
    /// Returns its identity key ("" if it can't be hidden).
    pub fn hide_locally(&self, thread_id: i64, hidden: &mut dyn HiddenStore, now_ms: i64) -> String {
        let (Some(dev), Some(conv)) = (self.device_id.as_deref(), self.index.get(thread_id)) else {
            return String::new();
        };
        if conv.thread_id < 0 {
            return String::new();
        }
        let key = self.conversation_key(&conv.addresses, &conv.address, conv.thread_id);
        if key.is_empty() {
            return key;
        }
        hidden.hide(dev, &key, now_ms.max(conv.last_date));
        key
    }

    /// Drop a hidden conversation from memory and the store.
    pub fn purge(&mut self, key: &str, store: &MessageStore) -> Vec<Effect> {
        let Some(&primary) = self.index.identity_to_primary.get(key) else { return Vec::new() };
        let removed = self.index.remove(primary);
        if removed.is_empty() {
            return Vec::new();
        }
        self.forget(&removed, store)
    }

    /// The user confirmed "Delete Conversation".
    pub fn delete(
        &mut self,
        thread_id: i64,
        can_delete_on_phone: bool,
        store: &MessageStore,
        hidden: &mut dyn HiddenStore,
        now_ms: i64,
    ) -> Vec<Effect> {
        let mut out = Vec::new();
        let key = self.hide_locally(thread_id, hidden, now_ms);
        if can_delete_on_phone && thread_id >= 0 {
            out.push(Effect::DeleteOnPhone { thread_id });
        } else if thread_id >= 0 {
            out.push(Effect::Toast(TOAST_LOCAL_DELETE_ONLY));
        }
        if key.is_empty() {
            out.extend(self.remove_conversation(thread_id, store));
        } else {
            out.extend(self.purge(&key, store));
        }
        out
    }

    // ── catch-up plumbing ──────────────────────────────────────────

    /// Run due catch-up work; the open conversation is gap-filled with every
    /// full pull.
    pub fn tick(&mut self, now: f64) -> Vec<Effect> {
        let active = match self.active {
            Some(a) if a >= 0 => self.conversation_threads(a),
            _ => Vec::new(),
        };
        self.catch_up.tick(now, &active).into_iter().map(Effect::Sync).collect()
    }

    pub fn on_conversation_loaded(&mut self, thread_id: i64, now: f64) -> Vec<Effect> {
        self.catch_up.on_conversation_loaded(thread_id, now).map(Effect::Sync).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    //! Ports of `tests/test_sms_panel_reconcile.py` (the panel glue).
    use super::*;
    use crate::core::sync::{Action, GAP_FILL_MAX_PAGES, GAP_FILL_MESSAGES};

    #[derive(Default)]
    struct FakeHidden(HashMap<(String, String), i64>);

    impl HiddenStore for FakeHidden {
        fn hidden_until(&self, d: &str, k: &str) -> i64 {
            self.0.get(&(d.to_string(), k.to_string())).copied().unwrap_or(0)
        }
        fn hide(&mut self, d: &str, k: &str, at: i64) {
            self.0.insert((d.to_string(), k.to_string()), at);
        }
        fn unhide(&mut self, d: &str, k: &str) {
            self.0.remove(&(d.to_string(), k.to_string()));
        }
    }

    struct Panel {
        m: Messages,
        store: MessageStore,
        hidden: FakeHidden,
    }

    impl Panel {
        fn new() -> Self {
            let mut m = Messages::new();
            m.reset_for_device(Some("dev1"), true);
            Self { m, store: MessageStore::in_memory(), hidden: FakeHidden::default() }
        }
        fn apply(&mut self, rows: &[RawMessage]) -> Vec<Effect> {
            self.m.apply_active(rows, &self.store, &mut self.hidden, 0.0)
        }
        fn merge(&mut self, row: RawMessage) -> Vec<Effect> {
            self.m.merge_message(parse_message(&row), &self.store, &mut self.hidden, 0.0)
        }
        fn visible(&self) -> Vec<&Conversation> {
            self.m.visible(&self.hidden)
        }
        /// Every sync request due within the next minute.
        fn drain(&mut self) -> Vec<Action> {
            let mut out = Vec::new();
            while let Some(t) = self.m.catch_up.next_deadline() {
                if t > 1e9 {
                    break;
                }
                out.extend(self.m.tick(t).into_iter().map(|e| match e {
                    Effect::Sync(a) => a,
                    other => panic!("unexpected {other:?}"),
                }));
            }
            out
        }
    }

    fn tup(uid: i64, thread_id: i64, addrs: &[&str], date: i64) -> RawMessage {
        RawMessage {
            body: "hi".into(),
            addresses: addrs.iter().map(|a| a.to_string()).collect(),
            date,
            msg_type: 1,
            read: 1,
            thread_id,
            uid,
            ..Default::default()
        }
    }

    fn uids(c: &Conversation) -> HashSet<i64> {
        c.messages.iter().map(|m| m.uid).collect()
    }

    // ── PanelReconcileTests ─────────────────────────────────────────

    #[test]
    fn test_sms_and_mms_threads_show_as_one_conversation() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["+13165551212"], 100), tup(2, 20, &["3165551212"], 200)]);
        let v = p.visible();
        assert_eq!(v.len(), 1);
        assert_eq!(uids(v[0]), HashSet::from([1, 2]));
        for _ in 0..5 {
            assert_eq!(p.visible().len(), 1);
        }
    }

    #[test]
    fn test_group_thread_not_merged_with_member() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["3165551111"], 100), tup(2, 20, &["3165551111", "3165552222"], 200)]);
        assert_eq!(p.visible().len(), 2);
    }

    #[test]
    fn test_unread_incoming_message_marks_conversation_unread() {
        let mut p = Panel::new();
        let mut row = tup(1, 10, &["111"], 100);
        row.read = 0;
        p.apply(&[row]);
        assert!(!p.visible()[0].is_read);
    }

    #[test]
    fn test_opened_thread_stays_read_on_own_reply() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["111"], 100)]);
        let tid = p.visible()[0].thread_id;
        p.m.active = Some(tid);
        let mut sent = tup(2, 10, &["111"], 200);
        sent.msg_type = 2;
        p.merge(sent);
        assert!(p.m.index.get(tid).unwrap().is_read);
    }

    #[test]
    fn opened_then_left_chat_turns_unread_on_a_new_incoming_text() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["111"], 100)]);
        p.m.select(10, &p.store, 0.0);
        p.m.active = None; // the user moved on
        let mut row = tup(2, 10, &["111"], 200);
        row.read = 0;
        p.merge(row);
        assert!(!p.m.index.get(10).unwrap().is_read);
    }

    #[test]
    fn test_one_to_one_send_uses_reply_to_conversation() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["+13165551212"], 100)]);
        let tid = p.visible()[0].thread_id;
        assert_eq!(
            p.m.send(tid, "hi there"),
            vec![Effect::Reply { conversation_id: tid, text: "hi there".into(), attachments: vec![] }]
        );
    }

    #[test]
    fn test_group_send_replies_in_thread() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 20, &["3165551111", "3165552222"], 100)]);
        let c = p.visible()[0];
        assert!(c.is_group());
        let tid = c.thread_id;
        let effects = p.m.send(tid, "hi all");
        assert!(matches!(effects[..], [Effect::Reply { conversation_id: 20, .. }]));
    }

    #[test]
    fn send_while_disconnected_only_explains() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["111"], 100)]);
        p.m.set_reachable(false);
        assert_eq!(p.m.send(10, "x"), vec![Effect::Toast(TOAST_NOT_CONNECTED)]);
    }

    #[test]
    fn draft_send_uses_an_existing_thread_or_starts_a_new_one() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["+13165551212"], 100)]);
        // Created before the thread loaded? Simulate with an unknown number first.
        p.m.open_or_create("3165559999", "New Person");
        let draft = p.m.active_thread().unwrap();
        assert!(draft < 0);
        let effects = p.m.send(draft, "hello");
        assert_eq!(
            effects[0],
            Effect::SendNew { addresses: vec!["3165559999".into()], text: "hello".into(), attachments: vec![] }
        );
        assert!(p.m.index.get(draft).is_none());
        // A number we already have a 1:1 thread with opens that thread.
        let effects = p.m.open_or_create("316-555-1212", "");
        assert_eq!(effects.last(), Some(&Effect::ShowThread(10)));
    }

    #[test]
    fn starting_a_chat_with_a_group_member_never_opens_the_group() {
        let mut p = Panel::new();
        // A group whose first member is 3165551111, and no 1:1 yet.
        p.apply(&[tup(1, 20, &["3165551111", "3165552222"], 100)]);
        let effects = p.m.open_or_create("3165551111", "");
        let draft = p.m.active_thread().unwrap();
        assert!(draft < 0, "opened {draft} instead of a new draft");
        assert_eq!(effects.last(), Some(&Effect::ShowThread(draft)));
        // Asking again reopens the same draft rather than making another.
        p.m.open_or_create("+1 316 555 1111", "");
        assert_eq!(p.m.active_thread(), Some(draft));
        // Once the 1:1 exists, it wins over the group.
        p.apply(&[tup(2, 30, &["3165551111"], 200)]);
        let effects = p.m.open_or_create("3165551111", "");
        assert_eq!(effects.last(), Some(&Effect::ShowThread(30)));
    }

    // ── send route A: reply through the notification ──────────────

    fn incoming(uid: i64, thread_id: i64, addr: &str, body: &str, date: i64) -> RawMessage {
        RawMessage { body: body.into(), ..tup(uid, thread_id, &[addr], date) }
    }

    fn notif(id: &str, title: &str, text: &str) -> ReplyableNotification {
        ReplyableNotification {
            public_id: id.into(),
            app_name: "Messages".into(),
            title: title.into(),
            text: text.into(),
            reply_id: format!("reply-{id}"),
        }
    }

    fn sent_echo(uid: i64, thread_id: i64, addr: &str, body: &str, date: i64) -> RawMessage {
        RawMessage { msg_type: 2, ..incoming(uid, thread_id, addr, body, date) }
    }

    #[test]
    fn reply_goes_through_the_matching_notification_and_is_labeled() {
        let mut p = Panel::new();
        p.m.contact_map.insert("3165551111".into(), "Alex".into());
        p.apply(&[incoming(1, 10, "3165551111", "Are you still coming tonight?", 100)]);
        p.m.note_notification(notif("n1", "Alex", "Are you still coming tonight?"));
        let effects = p.m.send(10, "Yes, leaving now");
        assert_eq!(
            effects,
            vec![Effect::ReplyViaNotification { thread_id: 10, reply_id: "reply-n1".into(), text: "Yes, leaving now".into() }]
        );
        p.merge(sent_echo(2, 10, "3165551111", "Yes, leaving now", 200));
        assert_eq!(p.m.route_of(2), Some(SendRoute::Notification));
        assert_eq!(SendRoute::Notification.label(), "via notification");
    }

    #[test]
    fn without_a_notification_it_sends_as_now_and_says_sms() {
        let mut p = Panel::new();
        p.apply(&[incoming(1, 10, "3165551111", "Are you still coming tonight?", 100)]);
        let effects = p.m.send(10, "Yes");
        assert!(matches!(&effects[0], Effect::Reply { conversation_id: 10, .. }));
        p.merge(sent_echo(2, 10, "3165551111", "Yes", 200));
        assert_eq!(p.m.route_of(2), Some(SendRoute::Sms));
        // A text sent on the phone itself gets no label.
        p.merge(sent_echo(3, 10, "3165551111", "typed on the phone", 300));
        assert_eq!(p.m.route_of(3), None);
    }

    #[test]
    fn ambiguous_or_mismatched_notifications_are_not_used() {
        let mut p = Panel::new();
        p.m.contact_map.insert("3165551111".into(), "Alex".into());
        p.apply(&[
            incoming(1, 10, "3165551111", "See you at the game tonight", 100),
            incoming(2, 20, "3165552222", "See you at the game tonight", 110),
        ]);
        // The same preview fits both chats: never guess.
        p.m.note_notification(notif("n1", "Alex", "See you at the game tonight"));
        assert!(matches!(p.m.send(10, "ok")[0], Effect::Reply { .. }));
        // A title that isn't this contact's name.
        p.apply(&[incoming(3, 10, "3165551111", "Running ten minutes late", 120)]);
        p.m.note_notification(notif("n2", "Sam", "Running ten minutes late"));
        assert!(matches!(p.m.send(10, "ok")[0], Effect::Reply { .. }));
        // A number title that is a different number.
        p.m.note_notification(notif("n6", "+1 316 555 9999", "Running ten minutes late"));
        assert!(matches!(p.m.send(10, "ok")[0], Effect::Reply { .. }));
        p.m.remove_notification("n6");
        // Not a texting app, or no reply action.
        p.m.note_notification(ReplyableNotification { app_name: "Gmail".into(), ..notif("n3", "Alex", "Running ten minutes late") });
        p.m.note_notification(ReplyableNotification { reply_id: String::new(), ..notif("n4", "Alex", "Running ten minutes late") });
        assert!(matches!(p.m.send(10, "ok")[0], Effect::Reply { .. }));
        // The right one, then removed from the phone: back to SMS.
        p.m.note_notification(notif("n5", "Alex", "Running ten minutes late"));
        assert!(matches!(p.m.send(10, "ok")[0], Effect::ReplyViaNotification { .. }));
        p.m.remove_notification("n5");
        assert!(matches!(p.m.send(10, "ok")[0], Effect::Reply { .. }));
    }

    #[test]
    fn a_failed_notification_reply_falls_back_to_sms_and_says_so() {
        let mut p = Panel::new();
        p.apply(&[incoming(1, 10, "3165551111", "Are you still coming tonight?", 100)]);
        p.m.note_notification(notif("n1", "+13165551111", "Are you still coming tonight?"));
        assert!(matches!(p.m.send(10, "On my way")[0], Effect::ReplyViaNotification { .. }));
        let effects = p.m.notification_reply_failed(10, "On my way", true);
        assert!(matches!(&effects[0], Effect::Reply { conversation_id: 10, text, .. } if text == "On my way"));
        p.merge(sent_echo(2, 10, "3165551111", "On my way", 200));
        assert_eq!(p.m.route_of(2), Some(SendRoute::Sms));
    }

    #[test]
    fn pictures_go_the_usual_way_and_empty_sends_do_nothing() {
        let mut p = Panel::new();
        p.apply(&[incoming(1, 10, "3165551111", "Are you still coming tonight?", 100)]);
        p.m.note_notification(notif("n1", "+13165551111", "Are you still coming tonight?"));
        let effects = p.m.send_with(10, "look", vec!["/tmp/a.png".into()]);
        assert!(
            matches!(&effects[0], Effect::Reply { attachments, .. } if attachments == &vec!["/tmp/a.png".to_string()])
        );
        assert!(p.m.send_with(10, "   ", vec![]).is_empty());
        // A picture with no text is fine.
        assert!(matches!(p.m.send_with(10, "", vec!["/tmp/b.png".into()])[0], Effect::Reply { .. }));
    }

    #[test]
    fn a_timed_out_notification_reply_is_never_resent_on_its_own() {
        let mut p = Panel::new();
        p.apply(&[incoming(1, 10, "3165551111", "Are you still coming tonight?", 100)]);
        p.m.note_notification(notif("n1", "+13165551111", "Are you still coming tonight?"));
        p.m.send(10, "On my way");
        let effects = p.m.notification_reply_failed(10, "On my way", false);
        assert_eq!(effects, vec![Effect::MaybeNotSent { thread_id: 10, text: "On my way".into() }]);
        assert!(p.m.awaiting_echo(10, "On my way"));
        // It did go out: the echo arrives and keeps the notification route.
        p.merge(sent_echo(2, 10, "3165551111", "On my way", 200));
        assert!(!p.m.awaiting_echo(10, "On my way"));
        assert_eq!(p.m.route_of(2), Some(SendRoute::Notification));
    }

    // ── full-resolution images ─────────────────────────────────────

    fn with_image(uid: i64, thread_id: i64, part: i64, unique: &str) -> RawMessage {
        RawMessage {
            attachments: vec![crate::core::message::RawAttachment {
                part_id: part,
                mime_type: "image/jpeg".into(),
                payload: String::new(),
                unique_identifier: unique.into(),
            }],
            ..tup(uid, thread_id, &["3165551111"], 100 + uid)
        }
    }

    #[test]
    fn full_images_are_asked_for_once_and_recorded_when_they_arrive() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("full");
        let mut p = Panel::new();
        p.merge(with_image(1, 10, 7, "PART_7.jpg"));
        p.merge(with_image(2, 10, 8, "PART_8.jpg"));
        let asked = p.m.take_full_image_requests(10, &cache);
        assert_eq!(asked, vec![(7, "PART_7.jpg".to_string()), (8, "PART_8.jpg".to_string())]);
        assert!(p.m.take_full_image_requests(10, &cache).is_empty(), "asked twice");

        // The phone sends PART_7 back.
        let sent = dir.path().join("download.jpg");
        std::fs::write(&sent, b"jpeg bytes").unwrap();
        assert_eq!(p.m.attachment_received(&sent, "PART_7.jpg", &p.store, &cache), Some(10));
        let att = &p.m.index.get(10).unwrap().messages.iter().find(|m| m.uid == 1).unwrap().attachments[0];
        let full = crate::core::attachments::existing_full_in(&cache, att).unwrap();
        assert_eq!(std::fs::read(full).unwrap(), b"jpeg bytes");
        // Persisted with the path, as Python does.
        let stored = p.store.load_conversations("dev1");
        let msg = stored[&10].messages.iter().find(|m| m.uid == 1).unwrap().clone();
        assert!(msg.attachments[0].extra.contains_key("fullPath"));
        // An unrelated download is not an inline image.
        assert_eq!(p.m.attachment_received(&sent, "notes.pdf", &p.store, &cache), None);
    }

    #[test]
    fn no_full_image_requests_while_the_phone_is_away() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Panel::new();
        p.merge(with_image(1, 10, 7, "PART_7.jpg"));
        p.m.set_reachable(false);
        assert!(p.m.take_full_image_requests(10, dir.path()).is_empty());
        p.m.set_reachable(true);
        assert_eq!(p.m.take_full_image_requests(10, dir.path()).len(), 1);
    }

    #[test]
    fn test_hidden_conversation_stays_hidden_until_newer_message() {
        let mut p = Panel::new();
        let now = 1_800_000_000_000;
        p.apply(&[tup(1, 10, &["111"], now - 10_000)]);
        let tid = p.visible()[0].thread_id;
        let key = p.m.hide_locally(tid, &mut p.hidden, now);
        p.m.purge(&key, &p.store);
        assert!(p.visible().is_empty());
        // The same (older) message must not resurrect it.
        p.apply(&[tup(1, 10, &["111"], now - 10_000)]);
        assert!(p.visible().is_empty());
        // A newer one brings it back.
        p.apply(&[tup(3, 10, &["111"], now + 60_000)]);
        assert_eq!(p.visible().len(), 1);
    }

    #[test]
    fn address_less_threads_are_separate_and_can_be_hidden() {
        let mut p = Panel::new();
        let now = 1_800_000_000_000;
        p.apply(&[tup(1, 719, &[], now - 10_000), tup(2, 720, &[], now - 5_000)]);
        assert_eq!(p.visible().len(), 2);
        let key = p.m.hide_locally(719, &mut p.hidden, now);
        assert_eq!(key, "thread:719");
        p.m.purge(&key, &p.store);
        let left: Vec<i64> = p.visible().iter().map(|c| c.thread_id).collect();
        assert_eq!(left, vec![720]);
    }

    #[test]
    fn delete_hides_and_asks_the_phone_when_it_can() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["111"], 100)]);
        p.m.select(10, &p.store, 0.0);
        let effects = p.m.delete(10, true, &p.store, &mut p.hidden, 1_000);
        assert!(effects.contains(&Effect::DeleteOnPhone { thread_id: 10 }));
        assert!(effects.contains(&Effect::ShowEmpty));
        assert!(p.visible().is_empty());
        assert!(p.store.load_conversations("dev1").is_empty());
        let effects = {
            p.apply(&[tup(5, 11, &["222"], 100)]);
            p.m.delete(11, false, &p.store, &mut p.hidden, 1_000)
        };
        assert!(effects.contains(&Effect::Toast(TOAST_LOCAL_DELETE_ONLY)));
    }

    #[test]
    fn test_cached_load_collapses_split_threads_after_restart() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["+13165551212"], 100), tup(2, 20, &["3165551212"], 200)]);
        let mut fresh = Messages::new();
        fresh.reset_for_device(Some("dev1"), true);
        fresh.load_cached(&p.store);
        let v = fresh.visible(&p.hidden);
        assert_eq!(v.len(), 1);
        assert_eq!(uids(v[0]), HashSet::from([1, 2]));
    }

    // ── PanelCatchUpTests ───────────────────────────────────────────

    fn catch_up_panel() -> Panel {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["111"], 100)]);
        p.m.catch_up.cancel();
        p
    }

    /// Like the phone after a sleep: the thread's newest message is already
    /// known, and the gap fill brings in older ones.
    fn paging_panel() -> Panel {
        let mut p = catch_up_panel();
        p.merge(tup(2, 10, &["111"], 10_000_000));
        p.m.catch_up.cancel();
        p
    }

    fn phone(thread_id: i64, older_than: Option<i64>) -> Action {
        Action::RequestFromPhone { thread_id, count: GAP_FILL_MESSAGES, older_than }
    }

    #[test]
    fn test_newer_message_on_known_thread_pulls_the_thread() {
        let mut p = catch_up_panel();
        p.merge(tup(3, 10, &["111"], 300));
        assert_eq!(p.drain(), vec![phone(10, None)]);
    }

    #[test]
    fn test_older_or_new_thread_messages_do_not_pull() {
        let mut p = catch_up_panel();
        p.merge(tup(0, 10, &["111"], 50));
        p.merge(tup(2, 99, &["222"], 500));
        assert!(p.drain().is_empty());
    }

    #[test]
    fn test_loaded_thread_is_replayed_once() {
        let mut p = catch_up_panel();
        p.m.catch_up.queue_gap_fill(10, 0.0);
        p.drain();
        let mut acts = p.m.on_conversation_loaded(77, 1.0);
        acts.extend(p.m.on_conversation_loaded(10, 1.0));
        acts.extend(p.m.on_conversation_loaded(10, 1.0));
        assert_eq!(acts, vec![Effect::Sync(Action::ReplayFromDaemon { thread_id: 10, start: 0, end: GAP_FILL_MESSAGES })]);
    }

    #[test]
    fn test_pending_thread_is_not_asked_twice() {
        let mut p = catch_up_panel();
        p.m.catch_up.queue_gap_fill(10, 0.0);
        let first = p.drain();
        p.m.catch_up.queue_gap_fill(10, 1.0);
        let second = p.drain();
        assert_eq!((first.len(), second.len()), (1, 0));
    }

    #[test]
    fn test_opening_a_thread_pulls_it_even_when_cached() {
        let mut p = catch_up_panel();
        for uid in 100..130 {
            p.merge(tup(uid, 10, &["111"], uid));
        }
        p.m.catch_up.cancel();
        p.m.select(10, &p.store, 0.0);
        assert_eq!(p.drain(), vec![phone(10, None)]);
    }

    #[test]
    fn test_catch_up_pulls_everything_and_the_open_thread() {
        let mut p = catch_up_panel();
        p.m.active = Some(10);
        p.m.catch_up.catch_up(0.0, 0.0, 0.0);
        assert_eq!(p.drain(), vec![Action::RequestAllThreads, phone(10, None)]);
    }

    #[test]
    fn test_catch_up_bursts_collapse() {
        let mut p = catch_up_panel();
        p.m.catch_up.catch_up(0.0, 0.0, 0.0);
        p.drain();
        p.m.catch_up.catch_up(1.0, 3.0, sync::CATCH_UP_MIN_INTERVAL_SECONDS);
        assert_eq!(p.m.catch_up.next_deadline(), None);
        p.m.catch_up.catch_up(1.0, 3.0, 0.0);
        assert!(p.m.catch_up.next_deadline().is_some());
    }

    fn fill_page(p: &mut Panel, uids: std::ops::Range<i64>, date0: i64) {
        for (i, uid) in uids.enumerate() {
            p.merge(tup(uid, 10, &["111"], date0 + i as i64));
        }
    }

    #[test]
    fn test_full_page_of_new_messages_pulls_the_next_older_page() {
        let mut p = paging_panel();
        p.m.catch_up.queue_gap_fill(10, 0.0);
        p.drain();
        p.m.on_conversation_loaded(10, 1.0);
        fill_page(&mut p, 1000..1000 + GAP_FILL_MESSAGES, 5000);
        assert_eq!(p.drain(), vec![phone(10, Some(5000))]);
        let n = GAP_FILL_MESSAGES;
        assert_eq!(
            p.m.on_conversation_loaded(10, 10.0),
            vec![Effect::Sync(Action::ReplayFromDaemon { thread_id: 10, start: n - 10, end: 2 * n })]
        );
    }

    #[test]
    fn test_page_that_reaches_known_messages_stops() {
        let mut p = paging_panel();
        p.m.catch_up.queue_gap_fill(10, 0.0);
        p.drain();
        p.m.on_conversation_loaded(10, 1.0);
        fill_page(&mut p, 1000..1010, 5000);
        assert!(p.drain().is_empty());
        assert_eq!(p.m.catch_up.next_deadline(), None);
    }

    #[test]
    fn test_paging_stops_at_the_page_limit() {
        let mut p = paging_panel();
        p.m.catch_up.queue_gap_fill(10, 0.0);
        p.drain();
        let mut now = 1.0;
        let mut pages = 1;
        loop {
            p.m.on_conversation_loaded(10, now);
            fill_page(&mut p, pages * 1000..pages * 1000 + GAP_FILL_MESSAGES, 1_000_000 - pages * 1000);
            if p.drain().is_empty() {
                break;
            }
            pages += 1;
            now += 10.0;
        }
        assert_eq!(pages, GAP_FILL_MAX_PAGES as i64);
    }

    #[test]
    fn test_catch_up_waits_for_the_phone() {
        let mut p = catch_up_panel();
        p.m.set_reachable(false);
        p.m.catch_up.catch_up(0.0, 0.0, 0.0);
        assert_eq!(p.m.catch_up.next_deadline(), None);
    }
}
