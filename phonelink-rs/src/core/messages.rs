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
use super::calls::{self as call_log, CallEvent};
use super::contacts::{
    is_messaging_app, is_texting_notification, looks_like_phone_number, normalize_phone, resolve_name,
};
use super::gm::{ChatChange, Cursor, Delivery, GmConversation, GmMessage};
use super::hidden_texts::{HiddenTextDetector, body_matches, normalize_text, reply_preview_matches};
use super::message::{RawMessage, parse_message};
use super::models::{Attachment, Call, Conversation, SmsMessage};
use super::private_tmp;
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
    MarkRead {
        thread_id: i64,
    },
    /// `replyToConversation(conversation_id, text, attachments)` — the only
    /// send the S25 reliably delivers.
    Reply {
        conversation_id: i64,
        text: String,
        attachments: Vec<String>,
    },
    /// `notifications.sendReply(reply_id, text)`: answer through the texting
    /// app's own notification, so the app (Google Messages) sends it, as RCS
    /// in an RCS thread (Phase 9 route A). If the call fails, the UI calls
    /// [`Messages::notification_reply_failed`].
    ReplyViaNotification {
        thread_id: i64,
        reply_id: String,
        text: String,
    },
    /// A notification reply got no clear answer (e.g. a D-Bus timeout): the
    /// phone may already have sent it, so nothing is resent. The UI waits
    /// briefly for the phone's echo ([`Messages::awaiting_echo`]), then
    /// shows "may not have sent" with a retry button.
    MaybeNotSent {
        thread_id: i64,
        text: String,
    },
    /// `sendWithoutConversation(addresses, text)` — only for brand-new
    /// conversations; the phone silently drops it otherwise.
    SendNew {
        addresses: Vec<String>,
        text: String,
        attachments: Vec<String>,
    },
    /// Delete on the phone: KDE Connect's `deleteConversation(thread)` when
    /// the daemon has it, else Google Messages by the chat's members.
    DeleteOnPhone {
        thread_id: i64,
        addresses: Vec<String>,
    },
    /// Send through Google Messages (`send_text`, or `send_media` with a
    /// file): RCS where the chat is.
    /// `reply_to`: the Google id of the message it answers.
    GmSend {
        thread_id: i64,
        conv: String,
        text: String,
        file: Option<String>,
        tmp_id: String,
        reply_to: Option<String>,
    },
    /// Add, take back or switch my reaction on a message.
    GmReact {
        thread_id: i64,
        conv: String,
        msg: String,
        emoji: String,
        action: &'static str,
    },
    /// A text to a chat that isn't in the list yet (one person or a
    /// group): find or make it on the phone (`open_conversation`), then
    /// [`Messages::gm_opened`] sends it with its files.
    GmSendNew {
        addresses: Vec<String>,
        text: String,
        attachments: Vec<String>,
    },
    /// Tell the phone "I'm typing" in this Google chat.
    GmTyping {
        conv: String,
    },
    /// Fetch the page of chat `conv` older than `cursor`, then
    /// [`Messages::gm_page`].
    GmFetchOlder {
        thread_id: i64,
        conv: String,
        cursor: Cursor,
    },
    /// Archive, mute, block... a Google chat; then [`Messages::gm_chat_changed`].
    GmUpdateChat {
        thread_id: i64,
        conv: String,
        change: ChatChange,
    },
    /// Who is typing in the open chat changed: show [`Messages::typing_label`].
    Typing(i64),
    /// Whether the phone has older messages for this open chat changed
    /// ([`Messages::gm_more_history`]).
    MoreHistory(i64),
    /// Fetch a chat's newest messages from Google Messages (each of its
    /// Google chats, when several merged into one).
    GmFetch {
        thread_id: i64,
    },
    /// A shown message changed (its delivery state): redraw the open chat.
    Redraw(i64),
    RefreshList,
    ShowThread(i64),
    ShowEmpty,
    Toast(&'static str),
}

pub const TOAST_NOT_CONNECTED: &str = "Can't send — your phone isn't connected right now";
pub const TOAST_GM_REACT_FAILED: &str = "The phone didn't take that reaction";
pub const TOAST_GM_CHAT_CHANGE_FAILED: &str = "The phone didn't take that change";
pub const TOAST_GM_READ_ONLY: &str = "This chat can't be replied to";
pub const TOAST_GM_NOT_SENT: &str = "Couldn't send through Google Messages. Check the phone is online and try again.";
pub const TOAST_LOCAL_DELETE_ONLY: &str =
    "Removed from PhoneRelay only; phone-side deletion is not available on this KDE Connect version";

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
    /// Through Google Messages: RCS or not, and how far it got.
    Google { rcs: bool, delivery: Delivery },
}

impl SendRoute {
    pub fn label(self) -> &'static str {
        match self {
            SendRoute::Sms => "SMS",
            SendRoute::Notification => "via notification",
            SendRoute::Google { rcs: true, delivery } => match delivery {
                Delivery::Sending => "RCS · Sending",
                Delivery::Sent => "RCS · Sent",
                Delivery::Delivered => "RCS · Delivered",
                Delivery::Read => "RCS · Read",
                Delivery::Failed => "RCS · Not sent",
            },
            SendRoute::Google { rcs: false, delivery } => match delivery {
                Delivery::Sending => "SMS · Sending",
                Delivery::Sent => "SMS · Sent",
                Delivery::Delivered => "SMS · Delivered",
                Delivery::Read => "SMS · Read",
                Delivery::Failed => "SMS · Not sent",
            },
        }
    }
}

/// One attachment to fetch through the Google Messages helper.
#[derive(Debug, Clone, PartialEq)]
pub struct GmDownload {
    pub media: String,
    pub key: String,
    /// The attachment's unique identifier ("gm:<media>").
    pub unique: String,
}

/// Copy a full attachment into the (private) cache and note it on `att`.
fn cache_full(
    att: &mut Attachment,
    file_path: &std::path::Path,
    cache_dir: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let dest = attachments::full_cache_path_in(cache_dir, att)?;
    // Pictures people sent: the cache folder and its copies are private.
    let copied = cache_dir
        .parent()
        .map_or(Ok(()), private_tmp::private_data_dir)
        .and_then(|()| private_tmp::private_data_dir(cache_dir))
        .and_then(|()| std::fs::copy(file_path, &dest))
        .and_then(|_| private_tmp::tighten(&dest, 0o600));
    if let Err(e) = copied {
        log::warn!("caching a full attachment failed: {e}");
        return None;
    }
    att.extra.insert("fullPath".into(), dest.to_string_lossy().into_owned().into());
    Some(dest)
}

/// One Google Messages chat behind a local thread.
#[derive(Debug, Clone, Default)]
struct GmChat {
    conv: String,
    read_only: bool,
    last_ts: i64,
    members: Vec<String>,
    /// The group's or business's own name.
    name: String,
    group: bool,
    rcs: bool,
}

/// The Messages tab's state while Google Messages is its source.
#[derive(Debug, Default)]
pub struct GmMode {
    /// Local thread id → its Google chat.
    chats: HashMap<i64, GmChat>,
    /// My messages' route and delivery, by uid.
    delivery: HashMap<i64, SendRoute>,
    /// Texts sent from here whose echo hasn't come: tmp id → (thread,
    /// text, the uid of its bubble shown meanwhile, 0 before it shows).
    /// Real phones echo without the tmp id, so the text matches too.
    sent_here: HashMap<String, (i64, String, i64)>,
    /// Messages sent from here (their delivery is always shown).
    uids_sent_here: HashSet<i64>,
    /// Chats whose newest page was fetched this session.
    fetched: HashSet<i64>,
    /// The phone's names for numbers (normalized), used where the desktop
    /// has none; kept so a contacts reload doesn't lose them.
    names: HashMap<String, String>,
    sends: u64,
    /// Reactions and reply links, by uid (this session; a fetch on open
    /// fills them in).
    rich: HashMap<i64, Rich>,
    /// The next send replies to this Google message id.
    reply_next: Option<String>,
    /// Who is typing, by primary thread: (number, shown until, epoch s).
    typing: HashMap<i64, Vec<(String, f64)>>,
    /// When "I'm typing" last went to each Google chat (epoch s).
    typing_sent: HashMap<String, f64>,
    /// Where each Google chat's older messages start: `None` once the
    /// phone has no more. Missing until its first fetch.
    cursors: HashMap<String, Option<Cursor>>,
    /// Google chats whose older page is on its way.
    loading_older: HashSet<String>,
    /// Chats archived (on the phone, or here on the way), pinned on the
    /// phone, and pinned or muted here: by local thread id. Kept in the
    /// store's meta table, so the list is right before the link connects.
    archived: HashSet<i64>,
    pinned_phone: HashSet<i64>,
    /// Pinned or unpinned here (wins over the phone's pin).
    pins: HashMap<i64, bool>,
    muted: HashSet<i64>,
    /// The list shows archived chats instead of the others.
    show_archived: bool,
}

impl GmMode {
    /// The sends for a text and its files into Google chat `conv`: each
    /// file as its own message (Google Messages takes one per message),
    /// the text with the first; `reply_to` goes on the first.
    fn sends_for(
        &mut self,
        primary: i64,
        conv: &str,
        text: &str,
        attachments: Vec<String>,
        reply_to: Option<String>,
    ) -> Vec<Effect> {
        let mut reply_to = reply_to;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let files: Vec<Option<String>> =
            if attachments.is_empty() { vec![None] } else { attachments.into_iter().map(Some).collect() };
        let mut out = Vec::new();
        for (i, file) in files.into_iter().enumerate() {
            let text = if i == 0 { text.trim().to_string() } else { String::new() };
            self.sends += 1;
            let tmp_id = super::gm::new_tmp_id(now_ms, self.sends % 1000);
            self.sent_here.insert(tmp_id.clone(), (primary, normalize_text(&text), 0));
            out.push(Effect::GmSend {
                thread_id: primary,
                conv: conv.to_string(),
                text,
                file,
                tmp_id,
                reply_to: reply_to.take(),
            });
        }
        out
    }
}

/// How long "… is typing" stays without news (the mautrix bridge's 15 s).
const TYPING_SHOWN_SECS: f64 = 15.0;
/// "I'm typing" goes to the phone at most this often per chat.
const TYPING_SEND_EVERY_SECS: f64 = 5.0;

/// What the chat list shows on a row, and offers in its menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChatFlags {
    pub pinned: bool,
    pub muted: bool,
    pub archived: bool,
    /// Google Messages actions (archive, mute, block) apply.
    pub gm: bool,
}

/// What the user can do to a chat from its menu (Google Messages only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatAction {
    Pin,
    Unpin,
    Mute,
    Unmute,
    Archive,
    Unarchive,
    Block,
    /// Block and report as spam.
    Spam,
}

fn ids_to_meta(ids: impl IntoIterator<Item = i64>) -> String {
    let mut v: Vec<i64> = ids.into_iter().collect();
    v.sort_unstable();
    v.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

fn ids_from_meta(s: Option<String>) -> HashSet<i64> {
    s.unwrap_or_default().split(',').filter_map(|x| x.trim().parse().ok()).collect()
}

/// What Google Messages says about one message beyond its text.
#[derive(Debug, Clone, Default, PartialEq)]
struct Rich {
    conv: String,
    msg: String,
    reply_to: String,
    reactions: Vec<super::gm::GmReaction>,
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
    /// KDE Connect's `internalId`: Android's notification key.
    pub internal_id: String,
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
    /// Android 17 hiding texts from KDE Connect (port brief, rule 1).
    pub hidden_texts: HiddenTextDetector,
    /// My own number keys, as Google Messages reports them.
    pub gm_me: Vec<String>,
    /// Calls KDE Connect reported for this device, oldest first.
    calls: Vec<Call>,
    /// Set while Google Messages is the source (device id `gm:...`).
    gm: Option<GmMode>,
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
        // A `gm:` device is Google Messages (core::gm::device_id).
        self.gm = device_id.filter(|d| d.starts_with("gm:")).map(|_| GmMode::default());
        self.index.clear();
        self.read_thread_ids.clear();
        self.active = None;
        self.catch_up.cancel();
        self.notifications.clear();
        self.pending_routes.clear();
        self.sent_routes.clear();
        self.full_requested.clear();
        self.calls.clear();
        self.set_reachable(reachable);
    }

    pub fn set_reachable(&mut self, reachable: bool) {
        self.reachable = reachable;
        // Google Messages has no KDE Connect catch-up.
        self.catch_up.set_reachable(reachable && self.device_id.is_some() && self.gm.is_none());
    }

    /// Is Google Messages the source?
    pub fn is_gm(&self) -> bool {
        self.gm.is_some()
    }

    fn conversation_key(&self, addresses: &[String], fallback: &str, thread_id: i64) -> String {
        reconcile::conversation_key(addresses, fallback, &self.index.self_key, thread_id)
    }

    fn should_keep_hidden(&self, hidden: &mut dyn HiddenStore, key: &str, message_date: i64) -> bool {
        let Some(dev) = self.device_id.as_deref() else {
            return false;
        };
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
        let Some(dev) = self.device_id.clone() else {
            return Vec::new();
        };
        if let Some(gm) = self.gm.as_mut() {
            gm.archived = ids_from_meta(store.meta("gm_archived"));
            gm.pinned_phone = ids_from_meta(store.meta("gm_pinned"));
            gm.muted = ids_from_meta(store.meta("gm_muted"));
            let on = ids_from_meta(store.meta("gm_pins_on"));
            let off = ids_from_meta(store.meta("gm_pins_off"));
            gm.pins = on.into_iter().map(|t| (t, true)).chain(off.into_iter().map(|t| (t, false))).collect();
        }
        self.calls = store.load_calls(&dev);
        let mut cached: Vec<Conversation> = store.load_conversations(&dev).into_values().collect();
        if cached.is_empty() {
            self.attach_calls();
            return if self.calls.is_empty() { Vec::new() } else { vec![Effect::RefreshList] };
        }
        // Deterministic order, collapsing legacy duplicate rows.
        cached.sort_by_key(|c| c.thread_id);
        self.index.clear();
        for conv in cached {
            self.index.register(conv);
        }
        self.refresh_names();
        self.attach_calls();
        vec![Effect::RefreshList]
    }

    // ── calls ──────────────────────────────────────────────────────

    /// `telephony.callReceived` from the daemon.
    pub fn call_event(&mut self, kind: &str, number: &str, now_ms: i64, store: &MessageStore) -> Vec<Effect> {
        let (Some(dev), Some(event)) = (self.device_id.clone(), CallEvent::parse(kind)) else {
            return Vec::new();
        };
        let Some(mut call) = call_log::record(&mut self.calls, event, number, now_ms) else {
            return Vec::new();
        };
        // Ringing while its chat is open: seen as it happens.
        let open = self
            .active
            .and_then(|a| self.index.get(a))
            .is_some_and(|c| !c.is_group() && call_log::belongs(&call, &c.address));
        if open && !call.seen {
            call.seen = true;
            if let Some(c) = self.calls.iter_mut().rev().find(|c| c.date == call.date && call_log::belongs(c, number)) {
                c.seen = true;
            }
        }
        store.upsert_call(&dev, &call);
        self.attach_calls();
        let mut out = vec![Effect::RefreshList];
        if open && let Some(a) = self.active {
            out.push(Effect::ShowThread(a));
        }
        out
    }

    /// Put each call in its number's 1:1 chat (a call-only chat when there
    /// is none), and keep a chat with an unseen missed call unread.
    /// Idempotent; cheap when nothing changed.
    pub fn attach_calls(&mut self) {
        use std::collections::BTreeMap;
        let key = |n: &str| reconcile::phone_key(n);
        let mut by_key: BTreeMap<String, Vec<Call>> = BTreeMap::new();
        for c in &self.calls {
            by_key.entry(key(&c.number)).or_default().push(c.clone());
        }
        // Real 1:1 threads first, so a call-only chat gives way to one.
        let mut owner: HashMap<String, i64> = HashMap::new();
        let mut ids: Vec<(bool, i64)> =
            self.index.conversations.values().map(|c| (c.thread_id < 0, c.thread_id)).collect();
        ids.sort();
        let active = self.active;
        let mut stale_drafts = Vec::new();
        for (_, tid) in ids {
            let Some(conv) = self.index.conversations.get_mut(&tid) else {
                continue;
            };
            let k = if conv.is_group() { String::new() } else { key(&conv.address) };
            let want = match by_key.get(&k) {
                Some(calls) if !k.is_empty() && !owner.contains_key(&k) => {
                    owner.insert(k.clone(), tid);
                    calls.clone()
                }
                _ => Vec::new(),
            };
            if want.is_empty() && tid < 0 && conv.messages.is_empty() && !conv.calls.is_empty() && active != Some(tid) {
                stale_drafts.push(tid);
                continue;
            }
            if conv.calls != want {
                conv.calls = want;
            }
            if active != Some(tid) && conv.has_unseen_missed_call() {
                conv.is_read = false;
            }
        }
        for tid in stale_drafts {
            self.index.conversations.remove(&tid);
        }
        for (k, calls) in by_key {
            if k.is_empty() || owner.contains_key(&k) {
                continue;
            }
            let draft_id = self.next_draft_id;
            self.next_draft_id -= 1;
            let mut conv = Conversation::new(draft_id);
            conv.address = calls.last().map(|c| c.number.clone()).unwrap_or_default();
            conv.display_name = name_for(&self.contact_map, &[], &conv.address);
            conv.calls = calls;
            conv.is_read = !conv.has_unseen_missed_call();
            self.index.conversations.insert(draft_id, conv);
        }
    }

    /// The chat was opened: its calls are seen.
    fn see_calls(&mut self, thread_id: i64, store: &MessageStore) {
        let Some(dev) = self.device_id.clone() else {
            return;
        };
        let Some(conv) = self.index.get(thread_id) else {
            return;
        };
        if conv.is_group() || conv.calls.iter().all(|c| c.seen) {
            return;
        }
        let address = conv.address.clone();
        for c in self.calls.iter_mut().filter(|c| !c.seen && call_log::belongs(c, &address)) {
            c.seen = true;
            store.upsert_call(&dev, c);
        }
        if let Some(conv) = self.index.get_mut(thread_id) {
            for c in &mut conv.calls {
                c.seen = true;
            }
        }
    }

    /// Re-resolve every display name (contacts changed).
    pub fn refresh_names(&mut self) {
        self.apply_gm_names();
        for conv in self.index.conversations.values_mut() {
            conv.display_name = name_for(&self.contact_map, &conv.addresses, &conv.address);
        }
        let tids: Vec<i64> = self.index.conversations.keys().copied().collect();
        for tid in tids {
            self.gm_name(tid);
        }
    }

    /// The daemon's `activeConversations` reply (newest message per thread).
    pub fn apply_active(
        &mut self,
        raw: &[RawMessage],
        store: &MessageStore,
        hidden: &mut dyn HiddenStore,
        now: f64,
    ) -> Vec<Effect> {
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
            self.attach_calls();
            out.push(Effect::RefreshList);
        }
        out
    }

    /// Learn the user's own number from multi-address threads; group
    /// identities depend on it, so re-key when it changes.
    fn detect_self_number(&mut self, msgs: &[SmsMessage]) {
        let key_lists: Vec<Vec<String>> =
            msgs.iter().map(|m| m.all_addresses.iter().map(|a| reconcile::phone_key(a)).collect()).collect();
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
    pub fn merge_message(
        &mut self,
        msg: SmsMessage,
        store: &MessageStore,
        hidden: &mut dyn HiddenStore,
        now: f64,
    ) -> Vec<Effect> {
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

        // A new chat may take over a call-only one's calls.
        if result.created && !self.calls.is_empty() {
            self.attach_calls();
        }
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
                // A replayed text doesn't hide an unseen missed call.
                if !active && conv.has_unseen_missed_call() {
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
        if let Some(gm) = &self.gm {
            // The archive and the inbox are shown apart, as on the phone.
            v.retain(|c| gm.archived.contains(&c.thread_id) == gm.show_archived);
        }
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
        self.see_calls(thread_id, store);
        let Some(conv) = self.index.get_mut(thread_id) else {
            return out;
        };
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
        if let Some(gm) = self.gm.as_mut() {
            if gm.fetched.insert(thread_id) {
                out.push(Effect::GmFetch { thread_id });
            }
            return out;
        }
        let threads = self.conversation_threads(thread_id);
        self.catch_up.queue_conversation_gap_fill(&threads, now);
        out
    }

    /// My own number keys: Google Messages' and the guessed one.
    fn gm_me_keys(&self) -> Vec<String> {
        let mut me = self.gm_me.clone();
        if !self.index.self_key.is_empty() && !me.contains(&self.index.self_key) {
            me.push(self.index.self_key.clone());
        }
        me
    }

    /// The Google Messages helper's key for a conversation (`core::gm`).
    pub fn gm_chat_key(&self, thread_id: i64) -> Option<String> {
        let c = self.index.get(thread_id)?;
        let key = super::gm::chat_key(&c.addresses, &c.address, &self.gm_me_keys());
        (!key.is_empty()).then_some(key)
    }

    /// The helper's key for chat members (as `DeleteOnPhone` carries them).
    pub fn gm_key_for(&self, addresses: &[String]) -> String {
        super::gm::chat_key(addresses, "", &self.gm_me_keys())
    }

    /// Google Messages says these chats (key → the phone's newest message,
    /// ms) are read on the phone: clear their unread dot here, unless this
    /// side has a newer text the phone's state doesn't cover yet.
    pub fn read_on_phone(&mut self, read: &HashMap<String, i64>, store: &MessageStore) -> Vec<Effect> {
        let Some(dev) = self.device_id.clone() else {
            return Vec::new();
        };
        let me = self.gm_me_keys();
        let mut changed = false;
        for conv in self.index.conversations.values_mut() {
            // An unseen missed call isn't something Google Messages knows.
            if conv.is_read || conv.thread_id < 0 || conv.has_unseen_missed_call() {
                continue;
            }
            let key = super::gm::chat_key(&conv.addresses, &conv.address, &me);
            let Some(&phone_ts) = read.get(&key) else {
                continue;
            };
            if conv.last_date > phone_ts + super::gm::READ_SLACK_MS {
                continue;
            }
            conv.is_read = true;
            store.upsert_conversation(&dev, conv);
            changed = true;
        }
        if changed { vec![Effect::RefreshList] } else { Vec::new() }
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

    /// An existing real chat with exactly these members (a group).
    pub fn find_thread_for_members(&self, addresses: &[String]) -> Option<i64> {
        let want: HashSet<String> = addresses.iter().map(|a| normalize_phone(a)).filter(|a| !a.is_empty()).collect();
        if want.is_empty() {
            return None;
        }
        self.index
            .conversations
            .values()
            .filter(|c| c.thread_id >= 0)
            .find(|c| {
                let have: HashSet<String> =
                    c.addresses.iter().map(|a| normalize_phone(a)).filter(|a| !a.is_empty()).collect();
                have.len() == want.len() && want.iter().all(|w| have.iter().any(|h| same_number(h, w)))
            })
            .map(|c| c.thread_id)
    }

    /// Start (or open) a chat with several people: the group already
    /// there, else a draft the first send makes on the phone.
    pub fn open_group(&mut self, addresses: &[String], display_name: &str) -> Vec<Effect> {
        let mut members: Vec<String> = Vec::new();
        for a in addresses {
            let n = normalize_phone(a);
            if !a.trim().is_empty() && !members.iter().any(|m| same_number(&normalize_phone(m), &n)) {
                members.push(a.trim().to_string());
            }
        }
        match members.len() {
            0 => return Vec::new(),
            1 => return self.open_or_create(&members[0], display_name),
            _ => {}
        }
        if self.device_id.is_none() || !self.reachable {
            return Vec::new();
        }
        let existing = self.find_thread_for_members(&members).or_else(|| {
            self.index
                .conversations
                .values()
                .filter(|c| c.thread_id < 0 && c.addresses.len() == members.len())
                .find(|c| members.iter().all(|m| c.addresses.contains(m)))
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
        conv.address = members[0].clone();
        conv.addresses = members.clone();
        conv.display_name = name_for(&self.contact_map, &members, &conv.address);
        self.index.conversations.insert(draft_id, conv);
        self.active = Some(draft_id);
        vec![Effect::RefreshList, Effect::ShowThread(draft_id)]
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
            return self.send_existing(thread_id, text, attachments);
        }
        // A draft: a real thread may have loaded since it was created.
        let Some(draft) = self.index.conversations.get(&thread_id) else {
            return Vec::new();
        };
        let address = draft.address.clone();
        let addresses = if draft.addresses.len() > 1 { draft.addresses.clone() } else { vec![address.clone()] };
        self.index.conversations.remove(&thread_id);
        let existing = if addresses.len() > 1 {
            self.find_thread_for_members(&addresses)
        } else {
            self.find_thread_for_address(&address)
        };
        match existing {
            Some(existing) => {
                self.active = Some(existing);
                self.read_thread_ids.insert(existing);
                let mut out = self.send_existing(existing, text, attachments);
                out.extend([Effect::RefreshList, Effect::ShowThread(existing)]);
                out
            }
            None if self.gm.is_some() => {
                self.active = None;
                vec![Effect::GmSendNew { addresses, text: text.to_string(), attachments }, Effect::RefreshList]
            }
            None => {
                self.active = None;
                vec![
                    Effect::SendNew { addresses, text: text.to_string(), attachments },
                    Effect::ShowEmpty,
                    Effect::RefreshList,
                ]
            }
        }
    }

    /// A text for an existing thread: through the texting app's notification
    /// when one clearly belongs to this conversation, else `replyToConversation`.
    fn send_existing(&mut self, thread_id: i64, text: &str, attachments: Vec<String>) -> Vec<Effect> {
        let primary = self.index.primary_for(thread_id);
        if self.gm.is_some() {
            return self.gm_send(primary, text, attachments);
        }
        let notification = if attachments.is_empty() { self.notification_for(primary) } else { None };
        let (route, effect) = match notification {
            Some(n) => (
                SendRoute::Notification,
                Effect::ReplyViaNotification {
                    thread_id: primary,
                    reply_id: n.reply_id.clone(),
                    text: text.to_string(),
                },
            ),
            None => (SendRoute::Sms, self.reply(primary, text, attachments)),
        };
        self.note_pending_route(primary, text, route);
        vec![effect]
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
    /// Through Google Messages: its delivery, for texts sent from here and
    /// any that failed.
    pub fn route_of(&self, uid: i64) -> Option<SendRoute> {
        if let Some(gm) = &self.gm {
            let route = gm.delivery.get(&uid).copied()?;
            let failed = matches!(route, SendRoute::Google { delivery: Delivery::Failed, .. });
            return (failed || gm.uids_sent_here.contains(&uid)).then_some(route);
        }
        self.sent_routes.get(&uid).copied()
    }

    /// Name a number (right-click "Set contact name"): saved to the local
    /// contacts, used everywhere at once.
    pub fn rename_contact(&mut self, address: &str, name: &str, store: &MessageStore) -> Vec<Effect> {
        let norm = normalize_phone(address);
        let name = name.trim();
        if norm.is_empty() || name.is_empty() {
            return Vec::new();
        }
        // Typed by hand: beats the phone's contacts too (contact_overrides).
        store.save_contact_override(&norm, name);
        let key = super::contacts::phone_key(&norm);
        self.contact_map.retain(|k, _| super::contacts::phone_key(k) != key);
        self.contact_map.insert(norm, name.to_string());
        self.refresh_names();
        let mut out = vec![Effect::RefreshList];
        if let Some(active) = self.active {
            out.push(Effect::ShowThread(active));
        }
        out
    }

    // ── notifications: hidden texts and contact names ─────────────

    /// Any notification from the phone. A newly posted texting-app one
    /// (`watch_hidden`) is watched for the text it announces (if it never arrives, the phone is hiding texts),
    /// and may teach us a contact name, which is saved. Returns whether a
    /// hidden-texts check should be scheduled, and effects.
    pub fn notification_seen(
        &mut self,
        props: &super::contacts::Props,
        now_epoch: f64,
        store: &MessageStore,
        watch_hidden: bool,
    ) -> (bool, Vec<Effect>) {
        let app = props.get("appName").and_then(|v| v.as_str()).unwrap_or("");
        let schedule = watch_hidden
            && is_messaging_app(app)
            && self.hidden_texts.note_notification(&super::contacts::notification_message_text(props), now_epoch);
        let mut out = Vec::new();
        if let Some((norm, name)) = super::contacts::match_contact_from_notification_props(
            props,
            self.index.conversations.values(),
            &self.contact_map,
        ) {
            store.save_contact(&norm, &name);
            self.contact_map.insert(norm, name);
            self.refresh_names();
            out.push(Effect::RefreshList);
        }
        (schedule, out)
    }

    /// Did a text quoting `preview` arrive since 30 minutes before the
    /// notification (`posted_at`, epoch seconds)?
    pub fn text_arrived(&self, preview: &str, posted_at: f64) -> bool {
        let since_ms = ((posted_at - 30.0 * 60.0) * 1000.0) as i64;
        self.index
            .conversations
            .values()
            .filter(|c| c.last_date >= since_ms)
            .flat_map(|c| &c.messages)
            .any(|m| m.date >= since_ms && body_matches(preview, &m.body))
    }

    /// Judge the notifications old enough. Returns whether the warning
    /// changed (show or hide the banner).
    pub fn check_hidden_texts(&mut self, now_epoch: f64) -> bool {
        let mut detector = std::mem::take(&mut self.hidden_texts);
        let changed = detector.check(now_epoch, |preview, at| self.text_arrived(preview, at));
        self.hidden_texts = detector;
        changed
    }

    /// Contact names learned elsewhere (a call, a shared or imported
    /// contacts file): saved and applied. `found` is normalized number →
    /// name.
    pub fn learn_contacts(&mut self, found: HashMap<String, String>, store: &MessageStore) -> Vec<Effect> {
        if found.is_empty() {
            return Vec::new();
        }
        store.batch(|store| {
            for (phone, name) in &found {
                store.save_contact(phone, name);
            }
        });
        self.contact_map.extend(found);
        self.refresh_names();
        let mut out = vec![Effect::RefreshList];
        if let Some(active) = self.active {
            out.push(Effect::ShowThread(active));
        }
        out
    }

    /// Normalized numbers that already have a conversation (for contact
    /// suggestions).
    pub fn conversation_phones(&self) -> Vec<String> {
        self.index.conversations.values().map(|c| normalize_phone(&c.address)).filter(|n| !n.is_empty()).collect()
    }

    // ── full-resolution images ─────────────────────────────────────

    /// Image attachments that only have the thumbnail, among the messages
    /// `uids` (those on screen, as Python: a whole 2,000-message thread at
    /// once swamped the daemon), to ask the phone for
    /// (`requestAttachmentFile(part_id, unique_id)`). Each is returned once
    /// per session, and none while the phone is away.
    pub fn take_full_image_requests(
        &mut self,
        thread_id: i64,
        uids: &HashSet<i64>,
        cache_dir: &std::path::Path,
    ) -> Vec<(i64, String)> {
        if !self.reachable || thread_id < 0 {
            return Vec::new();
        }
        let Some(conv) = self.index.get(thread_id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for att in conv.messages.iter().filter(|m| uids.contains(&m.uid)).flat_map(|m| &m.attachments) {
            if !attachments::is_image(att)
                || att.part_id == 0
                || attachments::existing_full_in(cache_dir, att).is_some()
            {
                continue;
            }
            let key = attachments::attachment_key(att);
            if self.full_requested.insert(key) {
                out.push((att.part_id, att.unique_identifier.clone()));
            }
        }
        out
    }

    /// Google Messages pictures in `uids` (those on screen) not yet here,
    /// to download through the helper. Each is returned once per session.
    pub fn take_gm_downloads(
        &mut self,
        thread_id: i64,
        uids: &HashSet<i64>,
        cache_dir: &std::path::Path,
    ) -> Vec<GmDownload> {
        if self.gm.is_none() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for tid in self.conversation_threads(thread_id) {
            let Some(conv) = self.index.get(tid) else {
                continue;
            };
            for att in conv.messages.iter().filter(|m| uids.contains(&m.uid)).flat_map(|m| &m.attachments) {
                if !super::gm::fetch_on_sight(att) || attachments::existing_full_in(cache_dir, att).is_some() {
                    continue;
                }
                let Some((media, key)) = super::gm::gm_media(att) else {
                    continue;
                };
                if self.full_requested.insert(att.unique_identifier.clone()) {
                    out.push(GmDownload { media, key, unique: att.unique_identifier.clone() });
                }
            }
        }
        out
    }

    /// The Google Messages attachment `unique` (a click on its chip), even
    /// if it was asked for before.
    pub fn gm_download_for(&self, unique: &str) -> Option<GmDownload> {
        self.gm.as_ref()?;
        let att = self
            .index
            .conversations
            .values()
            .flat_map(|c| &c.messages)
            .flat_map(|m| &m.attachments)
            .find(|a| a.unique_identifier == unique)?;
        let (media, key) = super::gm::gm_media(att)?;
        Some(GmDownload { media, key, unique: unique.to_string() })
    }

    /// The helper saved attachment `unique` at `file_path`: keep it in the
    /// cache, record it on its message, and return (thread, cached path).
    pub fn gm_downloaded(
        &mut self,
        unique: &str,
        file_path: &std::path::Path,
        store: &MessageStore,
        cache_dir: &std::path::Path,
    ) -> Option<(i64, std::path::PathBuf)> {
        let dev = self.device_id.clone()?;
        for conv in self.index.conversations.values_mut() {
            for msg in conv.messages.iter_mut() {
                let Some(att) = msg.attachments.iter_mut().find(|a| a.unique_identifier == unique) else {
                    continue;
                };
                let dest = cache_full(att, file_path, cache_dir)?;
                store.upsert_message(&dev, conv.thread_id, msg);
                return Some((conv.thread_id, dest));
            }
        }
        None
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
                cache_full(att, file_path, cache_dir)?;
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
        if is_texting_notification(&n.app_name, &n.internal_id) && !n.reply_id.is_empty() && !n.text.is_empty() {
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
            if !reply_preview_matches(&n.text, &newest.body) {
                return false;
            }
            if !conv.is_group() && !Self::title_fits(&n.title, conv) {
                return false;
            }
            // The same preview fits another conversation too: ambiguous.
            !self.index.conversations.values().any(|other| {
                other.thread_id != conv.thread_id
                    && other.thread_id >= 0
                    && Self::newest_received(other).is_some_and(|m| reply_preview_matches(&n.text, &m.body))
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
        let Some(&primary) = self.index.identity_to_primary.get(key) else {
            return Vec::new();
        };
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
        let addresses = self
            .index
            .get(thread_id)
            .map(|c| if c.addresses.is_empty() { vec![c.address.clone()] } else { c.addresses.clone() })
            .unwrap_or_default();
        // A 1:1 chat's calls go with it.
        if let (Some(dev), Some(conv)) = (self.device_id.clone(), self.index.get(thread_id))
            && !conv.is_group()
            && !conv.calls.is_empty()
        {
            let address = conv.address.clone();
            let numbers: Vec<String> = self
                .calls
                .iter()
                .filter(|c| call_log::belongs(c, &address))
                .map(|c| c.number.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            self.calls.retain(|c| !call_log::belongs(c, &address));
            store.delete_calls(&dev, &numbers);
        }
        let key = self.hide_locally(thread_id, hidden, now_ms);
        if can_delete_on_phone && thread_id >= 0 {
            out.push(Effect::DeleteOnPhone { thread_id, addresses });
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

    // ── Google Messages ────────────────────────────────────────────

    /// Put the phone's names into the contact map where it has none.
    fn apply_gm_names(&mut self) {
        let Some(gm) = &self.gm else { return };
        for (norm, name) in &gm.names {
            if resolve_name(&self.contact_map, norm) == *norm {
                self.contact_map.insert(norm.clone(), name.clone());
            }
        }
    }

    /// The chat's own name from Google Messages, where the contacts give
    /// none (a group's name, a business's, a stranger the phone knows).
    fn gm_name(&mut self, thread_id: i64) {
        let Some(gm) = &self.gm else { return };
        let Some(conv) = self.index.conversations.get_mut(&thread_id) else {
            return;
        };
        let Some(chat) = conv.thread_ids.iter().find_map(|t| gm.chats.get(t)) else {
            return;
        };
        if chat.name.is_empty() {
            return;
        }
        let unnamed = conv.display_name.is_empty() || conv.display_name == conv.address || conv.address.contains('@');
        if chat.group || unnamed {
            conv.display_name = chat.name.clone();
        }
    }

    /// A chat from Google Messages (all of them after connecting, then each
    /// change). `thread_id` is its local id (`MessageStore::gm_local_id`).
    pub fn gm_conversation(
        &mut self,
        c: &GmConversation,
        thread_id: i64,
        store: &MessageStore,
        hidden: &mut dyn HiddenStore,
    ) -> Vec<Effect> {
        let (Some(dev), Some(_)) = (self.device_id.clone(), self.gm.as_ref()) else {
            return Vec::new();
        };
        let members = c.addresses();
        // The phone's names for people the desktop has no name for.
        if let Some(gm) = self.gm.as_mut() {
            for m in c.members.iter().filter(|m| !m.name.is_empty() && !m.number.contains('@')) {
                let norm = normalize_phone(&m.number);
                if !norm.is_empty() {
                    gm.names.insert(norm, m.name.clone());
                }
            }
        }
        self.apply_gm_names();
        if let Some(gm) = self.gm.as_mut() {
            gm.chats.insert(
                thread_id,
                GmChat {
                    conv: c.conv.clone(),
                    read_only: c.read_only,
                    last_ts: c.last_ts_ms,
                    members: members.clone(),
                    name: c.name.clone(),
                    group: c.group,
                    rcs: c.rcs,
                },
            );
        }
        let known = self.index.thread_to_primary.contains_key(&thread_id);
        if c.blocked {
            // Blocked or moved to spam on the phone: gone from the list.
            if let Some(gm) = self.gm.as_mut() {
                gm.chats.remove(&thread_id);
            }
            return if known { self.remove_conversation(thread_id, store) } else { Vec::new() };
        }
        if let Some(gm) = self.gm.as_mut() {
            let (a, p) = (gm.archived.contains(&thread_id), gm.pinned_phone.contains(&thread_id));
            if a != c.archived || p != c.pinned {
                if c.archived {
                    gm.archived.insert(thread_id);
                } else {
                    gm.archived.remove(&thread_id);
                }
                if c.pinned {
                    gm.pinned_phone.insert(thread_id);
                } else {
                    gm.pinned_phone.remove(&thread_id);
                }
                store.set_meta("gm_archived", &ids_to_meta(gm.archived.iter().copied()));
                store.set_meta("gm_pinned", &ids_to_meta(gm.pinned_phone.iter().copied()));
            }
        }
        if !known {
            if members.is_empty() {
                return Vec::new();
            }
            let identity = self.conversation_key(&members, "", thread_id);
            if !self.index.identity_to_primary.contains_key(&identity)
                && self.should_keep_hidden(hidden, &identity, c.last_ts_ms)
            {
                return Vec::new();
            }
            let mut conv = Conversation::new(thread_id);
            conv.address = members[0].clone();
            conv.addresses = members.clone();
            conv.display_name = name_for(&self.contact_map, &members, &conv.address);
            self.index.register(conv);
        }
        let primary = self.index.primary_for(thread_id);
        let active = self.active == Some(primary);
        let mut out = vec![Effect::RefreshList];
        if let Some(conv) = self.index.get_mut(primary) {
            if c.last_ts_ms >= conv.last_date && !c.preview.is_empty() {
                conv.last_date = c.last_ts_ms;
                conv.last_message = c.preview.clone();
            }
            if active {
                conv.is_read = true;
                if c.unread {
                    // A new text in the chat being read.
                    out.push(Effect::MarkRead { thread_id: primary });
                }
            } else {
                conv.is_read = !c.unread && !conv.has_unseen_missed_call();
            }
        }
        if !known && !self.calls.is_empty() {
            self.attach_calls();
        }
        self.gm_name(primary);
        if let Some(conv) = self.index.get(primary) {
            store.upsert_conversation(&dev, conv);
        }
        out
    }

    /// A message from Google Messages: new, replayed, fetched, or changed
    /// (delivery, deletion). `thread_id`/`uid` are its local ids.
    pub fn gm_message(
        &mut self,
        m: &GmMessage,
        thread_id: i64,
        uid: i64,
        store: &MessageStore,
        hidden: &mut dyn HiddenStore,
        now: f64,
    ) -> Vec<Effect> {
        let (Some(dev), Some(gm)) = (self.device_id.clone(), self.gm.as_mut()) else {
            return Vec::new();
        };
        let members = match gm.chats.get(&thread_id) {
            Some(c) => c.members.clone(),
            None if !m.from.is_empty() => vec![m.from.clone()],
            None => return Vec::new(),
        };
        let mut changed = false;
        // A text from someone typing: they stopped.
        let mut typing_changed = false;
        if !m.from_me && !m.old && !m.from.is_empty() {
            let primary = self.index.primary_for(thread_id);
            let key = normalize_phone(&m.from);
            if let Some(list) = gm.typing.get_mut(&primary) {
                let before = list.len();
                list.retain(|(n, _)| !same_number(&normalize_phone(n), &key));
                typing_changed = list.len() != before && self.active == Some(primary);
            }
        }
        if m.from_me
            && let Some(delivery) = Delivery::parse(&m.status)
        {
            let route = SendRoute::Google { rcs: m.rcs, delivery };
            changed = gm.delivery.insert(uid, route).is_some_and(|old| old != route);
        }
        if !m.deleted && !m.msg.is_empty() {
            let rich = Rich {
                conv: m.conv.clone(),
                msg: m.msg.clone(),
                reply_to: m.reply_to.clone(),
                reactions: m.reactions.clone(),
            };
            // A bubble shown at send time carries its reply link until the echo.
            let had = gm.rich.get(&uid).cloned();
            let rich = Rich {
                reply_to: if rich.reply_to.is_empty() {
                    had.as_ref().map(|h| h.reply_to.clone()).unwrap_or_default()
                } else {
                    rich.reply_to
                },
                ..rich
            };
            if had.as_ref() != Some(&rich) {
                changed |= had.is_some_and(|h| h.reactions != rich.reactions || h.reply_to != rich.reply_to);
                gm.rich.insert(uid, rich);
            }
        }
        let primary = self.index.primary_for(thread_id);
        if m.from_me {
            // Its bubble (shown at send time under `uid`), or a send with
            // no bubble yet; never another send's bubble.
            let key = normalize_text(&m.text);
            let fits = |e: &(i64, String, i64)| e.2 == uid || (e.2 == 0 && !gm.uids_sent_here.contains(&uid));
            let mine = if gm.sent_here.get(&m.tmp_id).is_some_and(fits) {
                Some(m.tmp_id.clone())
            } else {
                gm.sent_here.iter().find(|(_, e)| e.0 == primary && e.1 == key && fits(e)).map(|(k, _)| k.clone())
            };
            if let Some(k) = mine {
                gm.sent_here.remove(&k);
                gm.uids_sent_here.insert(uid);
            }
        }
        let shown = self.index.get(primary).is_some_and(|c| c.messages.iter().any(|x| x.uid == uid));
        if m.deleted {
            let Some(conv) = self.index.get_mut(primary).filter(|_| shown) else {
                return Vec::new();
            };
            conv.messages.retain(|x| x.uid != uid);
            store.delete_message(&dev, uid);
            return if self.active == Some(primary) { vec![Effect::ShowThread(primary)] } else { Vec::new() };
        }
        if typing_changed {
            changed = true;
        }
        if shown {
            // A picture that was only a preview came again with its full
            // file (a new id): show that one.
            let fresh = super::gm::to_sms(m, thread_id, uid, &members, false).attachments;
            if let Some(row) = self.index.get_mut(primary).and_then(|c| c.messages.iter_mut().find(|x| x.uid == uid)) {
                let ids = |v: &[Attachment]| v.iter().map(|a| a.unique_identifier.clone()).collect::<Vec<_>>();
                let from_google =
                    row.attachments.iter().all(|a| a.unique_identifier.starts_with(super::gm::GM_MEDIA_PREFIX));
                // Never back from the full picture to a preview.
                let full = m.attachments.iter().all(|a| !a.thumb);
                if !fresh.is_empty()
                    && full
                    && from_google
                    && ids(&row.attachments) != ids(&fresh)
                    && row.attachments.len() == fresh.len()
                {
                    row.attachments = fresh;
                    store.upsert_message(&dev, primary, row);
                    changed = true;
                }
            }
            // Rows saved before keys were kept: take the phone's keys now.
            let keys: Vec<(String, String)> = m
                .attachments
                .iter()
                .filter(|a| !a.key.is_empty())
                .map(|a| (format!("{}{}", super::gm::GM_MEDIA_PREFIX, a.id), a.key.clone()))
                .collect();
            if !keys.is_empty()
                && let Some(row) =
                    self.index.get_mut(primary).and_then(|c| c.messages.iter_mut().find(|x| x.uid == uid))
            {
                let mut keyed = false;
                for att in row.attachments.iter_mut().filter(|a| !a.extra.contains_key(super::gm::GM_KEY)) {
                    if let Some((_, key)) = keys.iter().find(|(u, _)| *u == att.unique_identifier) {
                        att.extra.insert(super::gm::GM_KEY.into(), key.clone().into());
                        keyed = true;
                    }
                }
                if keyed {
                    store.upsert_message(&dev, primary, row);
                    changed = true;
                }
            }
            let mut out = Vec::new();
            if changed && self.active == Some(primary) {
                out.push(Effect::Redraw(primary));
            }
            if typing_changed {
                out.push(Effect::Typing(primary));
            }
            return out;
        }
        // A new text arriving live is unread until seen.
        let unread = !m.from_me && !m.old;
        let row = super::gm::to_sms(m, thread_id, uid, &members, unread);
        let mut out = self.merge_message(row, store, hidden, now);
        let primary = self.index.primary_for(thread_id);
        if unread && self.active == Some(primary) {
            out.push(Effect::MarkRead { thread_id: primary });
        }
        if typing_changed {
            out.push(Effect::Typing(primary));
        }
        self.gm_name(primary);
        out
    }

    /// The Google chats behind a thread (several when merged), newest first.
    pub fn gm_convs(&self, thread_id: i64) -> Vec<String> {
        let Some(gm) = &self.gm else {
            return Vec::new();
        };
        let mut chats: Vec<&GmChat> =
            self.conversation_threads(thread_id).iter().filter_map(|t| gm.chats.get(t)).collect();
        chats.sort_by_key(|c| std::cmp::Reverse(c.last_ts));
        chats.into_iter().map(|c| c.conv.clone()).collect()
    }

    /// A text into an existing chat through Google Messages: into its
    /// newest Google chat that takes replies.
    /// Each file goes as its own message (Google Messages takes one per
    /// message), the text with the first.
    fn gm_send(&mut self, primary: i64, text: &str, attachments: Vec<String>) -> Vec<Effect> {
        let threads = self.conversation_threads(primary);
        let Some(gm) = self.gm.as_mut() else {
            return vec![Effect::Toast(TOAST_GM_NOT_SENT)];
        };
        let chat = threads
            .iter()
            .filter_map(|t| gm.chats.get(t))
            .filter(|c| !c.read_only)
            .max_by_key(|c| c.last_ts)
            .map(|c| c.conv.clone());
        let Some(mut conv) = chat else {
            return vec![Effect::Toast(TOAST_GM_READ_ONLY)];
        };
        // A reply goes to the chat the message is in (merged chats have several).
        let reply_to = gm.reply_next.take();
        if let Some(r) = reply_to.as_ref().and_then(|id| gm.rich.values().find(|x| &x.msg == id)) {
            conv = r.conv.clone();
        }
        gm.sends_for(primary, &conv, text, attachments, reply_to)
    }

    /// The next text sent replies to Google's message `msg` (None: not a reply).
    pub fn gm_reply_next(&mut self, msg: Option<String>) {
        if let Some(gm) = self.gm.as_mut() {
            gm.reply_next = msg;
        }
    }

    /// A bubble shown at send time answers `reply_to`: show the quote at once.
    pub fn gm_pending_reply(&mut self, uid: i64, conv: &str, reply_to: &str) {
        if let Some(gm) = self.gm.as_mut() {
            gm.rich.entry(uid).or_insert_with(|| Rich {
                conv: conv.into(),
                reply_to: reply_to.into(),
                ..Default::default()
            });
        }
    }

    /// Reactions and quotes for the bubbles of thread `primary`, by uid.
    pub fn gm_extras(&self, primary: i64, store: &MessageStore) -> HashMap<i64, super::rcs::Extras> {
        let (Some(gm), Some(conv)) = (&self.gm, self.index.get(primary)) else {
            return HashMap::new();
        };
        let name_of = |n: &str| resolve_name(&self.contact_map, n);
        let mut out = HashMap::new();
        for m in &conv.messages {
            let Some(r) = gm.rich.get(&m.uid) else {
                continue;
            };
            let quote = (!r.reply_to.is_empty()).then(|| {
                let uid = store.gm_known("msg", &r.reply_to);
                match uid.and_then(|u| conv.messages.iter().find(|q| q.uid == u)) {
                    Some(q) => super::rcs::Quote {
                        author: if q.msg_type == super::models::MSG_TYPE_RECEIVED {
                            name_of(&q.address)
                        } else {
                            "You".into()
                        },
                        text: if q.body.is_empty() && !q.attachments.is_empty() {
                            "Picture".into()
                        } else {
                            q.body.clone()
                        },
                        uid: Some(q.uid),
                    },
                    None => super::rcs::Quote::default(),
                }
            });
            let reactions: Vec<_> = r.reactions.iter().map(|x| super::rcs::chip(x, &name_of)).collect();
            // Not yet on the phone (still sending): no reactions or Reply yet.
            if r.msg.is_empty() && quote.is_none() {
                continue;
            }
            out.insert(m.uid, super::rcs::Extras { msg_id: r.msg.clone(), reactions, quote });
        }
        out
    }

    /// The user picked `emoji` on bubble `uid`: show it at once and ask the phone.
    pub fn gm_react(&mut self, uid: i64, emoji: &str) -> Vec<Effect> {
        let Some(primary) = self.active else {
            return Vec::new();
        };
        let Some(r) = self.gm.as_mut().and_then(|gm| gm.rich.get_mut(&uid)).filter(|r| !r.msg.is_empty()) else {
            return Vec::new();
        };
        let action = super::rcs::toggle(&mut r.reactions, emoji);
        vec![
            Effect::GmReact {
                thread_id: primary,
                conv: r.conv.clone(),
                msg: r.msg.clone(),
                emoji: emoji.into(),
                action,
            },
            Effect::Redraw(primary),
        ]
    }

    /// A reaction was refused: fetch the chat again for the phone's truth.
    pub fn gm_react_failed(&mut self, thread_id: i64) -> Vec<Effect> {
        let primary = self.index.primary_for(thread_id);
        let Some(gm) = self.gm.as_mut() else {
            return Vec::new();
        };
        gm.fetched.remove(&primary);
        vec![Effect::Toast(TOAST_GM_REACT_FAILED), Effect::GmFetch { thread_id: primary }]
    }

    /// The phone found or made the chat for a [`Effect::GmSendNew`]
    /// (`thread_id` is its local id, `conv` Google's): open it and send
    /// the text and its files.
    pub fn gm_opened(&mut self, thread_id: i64, conv: &str, text: &str, attachments: Vec<String>) -> Vec<Effect> {
        let primary = self.index.primary_for(thread_id);
        let Some(gm) = self.gm.as_mut() else {
            return Vec::new();
        };
        gm.fetched.insert(primary);
        let mut out = gm.sends_for(primary, conv, text, attachments, None);
        self.active = Some(primary);
        self.read_thread_ids.insert(primary);
        out.extend([Effect::RefreshList, Effect::ShowThread(primary)]);
        out
    }

    /// Show a text sent through Google Messages at once, as "Sending",
    /// before the phone echoes it. `uid` is a fresh local id for it; the
    /// echo takes the same uid ([`Messages::gm_pending_uid`]), so the bubble
    /// stays put and only its label changes.
    pub fn gm_add_pending(
        &mut self,
        (tmp_id, text, file): (&str, &str, Option<&str>),
        uid: i64,
        store: &MessageStore,
        hidden: &mut dyn HiddenStore,
        now: f64,
    ) -> Vec<Effect> {
        let Some(&(primary, _, _)) = self.gm.as_ref().and_then(|gm| gm.sent_here.get(tmp_id)) else {
            return Vec::new();
        };
        let threads = self.conversation_threads(primary);
        let Some(gm) = self.gm.as_mut() else {
            return Vec::new();
        };
        if let Some(entry) = gm.sent_here.get_mut(tmp_id) {
            entry.2 = uid;
        }
        let chat = threads.iter().filter_map(|t| gm.chats.get(t)).max_by_key(|c| c.last_ts);
        let members = chat.map(|c| c.members.clone()).unwrap_or_default();
        let rcs = chat.is_some_and(|c| c.rcs);
        gm.delivery.insert(uid, SendRoute::Google { rcs, delivery: Delivery::Sending });
        gm.uids_sent_here.insert(uid);
        let row = SmsMessage {
            uid,
            body: text.to_string(),
            address: members.first().cloned().unwrap_or_default(),
            date: chrono::Utc::now().timestamp_millis(),
            msg_type: super::models::MSG_TYPE_SENT,
            read: 1,
            thread_id: primary,
            attachments: file.map(super::gm::local_attachment).into_iter().collect(),
            all_addresses: members,
        };
        let mut out = self.merge_message(row, store, hidden, now);
        if self.active == Some(primary) && !out.contains(&Effect::ShowThread(primary)) {
            out.push(Effect::ShowThread(primary));
        }
        if !out.contains(&Effect::RefreshList) {
            out.push(Effect::RefreshList);
        }
        out
    }

    /// The uid already showing for this message: the bubble of a text sent
    /// from here that this is the phone's copy of.
    pub fn gm_pending_uid(&self, m: &GmMessage, thread_id: i64) -> Option<i64> {
        let gm = self.gm.as_ref()?;
        if !m.from_me {
            return None;
        }
        if let Some(&(_, _, uid)) = gm.sent_here.get(&m.tmp_id) {
            return (uid != 0).then_some(uid);
        }
        let primary = self.index.primary_for(thread_id);
        let key = normalize_text(&m.text);
        gm.sent_here.values().find(|(t, text, uid)| *t == primary && *text == key && *uid != 0).map(|e| e.2)
    }

    /// The phone didn't take a text sent through Google Messages: its
    /// bubble stays for now, marked "Not sent", but isn't kept on disk.
    pub fn gm_send_failed(&mut self, tmp_id: &str, store: &MessageStore) -> Vec<Effect> {
        let mut out = vec![Effect::Toast(TOAST_GM_NOT_SENT)];
        let dev = self.device_id.clone().unwrap_or_default();
        if let Some(gm) = self.gm.as_mut()
            && let Some((primary, _, uid)) = gm.sent_here.remove(tmp_id)
            && uid != 0
        {
            store.delete_message(&dev, uid);
            let rcs = matches!(gm.delivery.get(&uid), Some(SendRoute::Google { rcs: true, .. }));
            gm.delivery.insert(uid, SendRoute::Google { rcs, delivery: Delivery::Failed });
            out.push(Effect::Redraw(primary));
        }
        out
    }

    // ── Google Messages: typing ────────────────────────────────────

    /// Someone in Google chat (local thread) `thread_id` started or stopped
    /// typing (`now`: epoch seconds).
    pub fn gm_typing(&mut self, thread_id: i64, from: &str, typing: bool, now: f64) -> Vec<Effect> {
        let primary = self.index.primary_for(thread_id);
        let Some(gm) = self.gm.as_mut() else {
            return Vec::new();
        };
        let key = normalize_phone(from);
        let list = gm.typing.entry(primary).or_default();
        list.retain(|(n, until)| *until > now && !same_number(&normalize_phone(n), &key));
        if typing {
            list.push((from.to_string(), now + TYPING_SHOWN_SECS));
        }
        if self.active == Some(primary) { vec![Effect::Typing(primary)] } else { Vec::new() }
    }

    /// "Sam is typing…" for chat `primary`, and when it next needs looking
    /// at (epoch seconds); `None` when nobody is.
    pub fn typing_label(&self, primary: i64, now: f64) -> Option<(String, f64)> {
        let gm = self.gm.as_ref()?;
        let who: Vec<&(String, f64)> = gm.typing.get(&primary)?.iter().filter(|(_, until)| *until > now).collect();
        let next = who.iter().map(|w| w.1).reduce(f64::min)?;
        let first = |n: &str| {
            let name = resolve_name(&self.contact_map, n);
            // A group shows first names; a bare number stays whole.
            if looks_like_phone_number(&name) {
                name
            } else {
                name.split_whitespace().next().unwrap_or(&name).to_string()
            }
        };
        let text = match who.as_slice() {
            [one] => format!("{} is typing…", first(&one.0)),
            [a, b] => format!("{} and {} are typing…", first(&a.0), first(&b.0)),
            _ => "Several people are typing…".to_string(),
        };
        Some((text, next))
    }

    /// The user is typing in chat `primary`: tell the phone, at most every
    /// few seconds, and only in RCS chats (SMS has no typing).
    pub fn gm_typing_out(&mut self, primary: i64, now: f64) -> Vec<Effect> {
        let threads = self.conversation_threads(primary);
        let Some(gm) = self.gm.as_mut() else {
            return Vec::new();
        };
        let chat = threads.iter().filter_map(|t| gm.chats.get(t)).filter(|c| !c.read_only).max_by_key(|c| c.last_ts);
        let Some(conv) = chat.filter(|c| c.rcs).map(|c| c.conv.clone()) else {
            return Vec::new();
        };
        if gm.typing_sent.get(&conv).is_some_and(|t| now - t < TYPING_SEND_EVERY_SECS && now >= *t) {
            return Vec::new();
        }
        gm.typing_sent.insert(conv.clone(), now);
        vec![Effect::GmTyping { conv }]
    }

    // ── Google Messages: older messages ────────────────────────────

    /// A fetch of Google chat `conv` came back: `cursor` is where older
    /// messages start (`None`: no more), `older` whether it was an older
    /// page, `got` how many messages it had.
    pub fn gm_page(&mut self, conv: &str, cursor: Option<Cursor>, older: bool, got: usize) -> Vec<Effect> {
        let Some(gm) = self.gm.as_mut() else {
            return Vec::new();
        };
        let cursor = cursor.filter(|_| got > 0);
        if older {
            gm.loading_older.remove(conv);
            gm.cursors.insert(conv.to_string(), cursor);
        } else if !gm.cursors.contains_key(conv) {
            // A newest page again (reopened) doesn't undo older ones.
            gm.cursors.insert(conv.to_string(), cursor);
        }
        let thread = gm.chats.iter().find(|(_, c)| c.conv == conv).map(|(t, _)| *t);
        match thread.map(|t| self.index.primary_for(t)) {
            Some(p) if self.active == Some(p) => vec![Effect::MoreHistory(p)],
            _ => Vec::new(),
        }
    }

    /// Does the phone have messages older than those here for `primary`?
    pub fn gm_more_history(&self, primary: i64) -> bool {
        let Some(gm) = &self.gm else { return false };
        self.conversation_threads(primary)
            .iter()
            .filter_map(|t| gm.chats.get(t))
            .any(|c| matches!(gm.cursors.get(&c.conv), Some(Some(_))))
    }

    /// Is an older page of `primary` on its way?
    pub fn gm_loading_older(&self, primary: i64) -> bool {
        let Some(gm) = &self.gm else { return false };
        self.conversation_threads(primary)
            .iter()
            .filter_map(|t| gm.chats.get(t))
            .any(|c| gm.loading_older.contains(&c.conv))
    }

    /// Fetch the next older page of each of `primary`'s Google chats.
    pub fn gm_older(&mut self, primary: i64) -> Vec<Effect> {
        let threads = self.conversation_threads(primary);
        let Some(gm) = self.gm.as_mut() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for c in threads.iter().filter_map(|t| gm.chats.get(t)) {
            if let Some(Some(cursor)) = gm.cursors.get(&c.conv)
                && !gm.loading_older.contains(&c.conv)
            {
                out.push(Effect::GmFetchOlder { thread_id: primary, conv: c.conv.clone(), cursor: cursor.clone() });
            }
        }
        for e in &out {
            if let Effect::GmFetchOlder { conv, .. } = e {
                gm.loading_older.insert(conv.clone());
            }
        }
        out
    }

    // ── Google Messages: archive, pin, mute, block ─────────────────

    /// Each shown chat's pin, mute and archive state (empty unless Google
    /// Messages is the source).
    pub fn chat_flags(&self) -> HashMap<i64, ChatFlags> {
        let Some(gm) = &self.gm else {
            return HashMap::new();
        };
        self.index
            .conversations
            .values()
            .map(|c| {
                let threads = self.conversation_threads(c.thread_id);
                let any = |set: &HashSet<i64>| threads.iter().any(|t| set.contains(t));
                let pinned = gm.pins.get(&c.thread_id).copied().unwrap_or_else(|| any(&gm.pinned_phone));
                let flags = ChatFlags {
                    pinned,
                    muted: gm.muted.contains(&c.thread_id),
                    archived: gm.archived.contains(&c.thread_id),
                    gm: c.thread_id >= 0 && threads.iter().any(|t| gm.chats.contains_key(t)),
                };
                (c.thread_id, flags)
            })
            .collect()
    }

    /// Show the archive (true) or the other chats.
    pub fn set_show_archived(&mut self, on: bool) -> Vec<Effect> {
        let Some(gm) = self.gm.as_mut() else {
            return Vec::new();
        };
        if gm.show_archived == on {
            return Vec::new();
        }
        gm.show_archived = on;
        let mut out = vec![Effect::RefreshList];
        if self.active.is_some_and(|a| gm.archived.contains(&a) != on) {
            self.active = None;
            out.push(Effect::ShowEmpty);
        }
        out
    }

    pub fn showing_archived(&self) -> bool {
        self.gm.as_ref().is_some_and(|gm| gm.show_archived)
    }

    /// How many chats are archived (shown on the archive button).
    pub fn archived_count(&self) -> usize {
        let Some(gm) = &self.gm else { return 0 };
        gm.archived.iter().filter(|t| self.index.conversations.contains_key(t)).count()
    }

    fn save_flags(&self, store: &MessageStore) {
        let Some(gm) = &self.gm else { return };
        store.set_meta("gm_archived", &ids_to_meta(gm.archived.iter().copied()));
        store.set_meta("gm_muted", &ids_to_meta(gm.muted.iter().copied()));
        store.set_meta("gm_pins_on", &ids_to_meta(gm.pins.iter().filter(|(_, on)| **on).map(|(t, _)| *t)));
        store.set_meta("gm_pins_off", &ids_to_meta(gm.pins.iter().filter(|(_, on)| !**on).map(|(t, _)| *t)));
    }

    /// The user chose `action` for chat `primary` from its menu. Pins stay
    /// on this computer (the phone takes none from the web); the rest go
    /// to the phone and show at once, undone if the phone refuses.
    pub fn chat_action(&mut self, primary: i64, action: ChatAction, store: &MessageStore) -> Vec<Effect> {
        let threads = self.conversation_threads(primary);
        let Some(gm) = self.gm.as_mut() else {
            return Vec::new();
        };
        let convs: Vec<String> = threads.iter().filter_map(|t| gm.chats.get(t)).map(|c| c.conv.clone()).collect();
        let change = match action {
            ChatAction::Pin | ChatAction::Unpin => {
                gm.pins.insert(primary, action == ChatAction::Pin);
                self.save_flags(store);
                return vec![Effect::RefreshList];
            }
            ChatAction::Mute => {
                gm.muted.insert(primary);
                ChatChange::Mute
            }
            ChatAction::Unmute => {
                gm.muted.remove(&primary);
                ChatChange::Unmute
            }
            ChatAction::Archive => {
                gm.archived.extend(threads.iter().copied());
                ChatChange::Archive
            }
            ChatAction::Unarchive => {
                for t in &threads {
                    gm.archived.remove(t);
                }
                ChatChange::Unarchive
            }
            ChatAction::Block => ChatChange::Block,
            ChatAction::Spam => ChatChange::Spam,
        };
        let moved = matches!(action, ChatAction::Archive | ChatAction::Unarchive) && self.active == Some(primary);
        self.save_flags(store);
        let mut out: Vec<Effect> =
            convs.into_iter().map(|conv| Effect::GmUpdateChat { thread_id: primary, conv, change }).collect();
        out.push(Effect::RefreshList);
        if moved {
            // It left the list being shown.
            self.active = None;
            out.push(Effect::ShowEmpty);
        }
        out
    }

    /// The phone answered a [`Effect::GmUpdateChat`]. Refused: undo what
    /// showed at once. Blocked: the chat leaves the list.
    pub fn gm_chat_changed(
        &mut self,
        thread_id: i64,
        change: ChatChange,
        ok: bool,
        store: &MessageStore,
    ) -> Vec<Effect> {
        let primary = self.index.primary_for(thread_id);
        let threads = self.conversation_threads(primary);
        let Some(gm) = self.gm.as_mut() else {
            return Vec::new();
        };
        if ok {
            return match change {
                ChatChange::Block | ChatChange::Spam => {
                    for t in &threads {
                        gm.chats.remove(t);
                    }
                    self.remove_conversation(primary, store)
                }
                _ => Vec::new(),
            };
        }
        match change {
            ChatChange::Mute => {
                gm.muted.remove(&primary);
            }
            ChatChange::Unmute => {
                gm.muted.insert(primary);
            }
            ChatChange::Archive => {
                for t in &threads {
                    gm.archived.remove(t);
                }
            }
            ChatChange::Unarchive => gm.archived.extend(threads.iter().copied()),
            _ => {}
        }
        self.save_flags(store);
        vec![Effect::Toast(TOAST_GM_CHAT_CHANGE_FAILED), Effect::RefreshList]
    }

    // ── catch-up plumbing ──────────────────────────────────────────

    /// Run due catch-up work; the open conversation is gap-filled with every
    /// full pull.
    pub fn tick(&mut self, now: f64) -> Vec<Effect> {
        if self.gm.is_some() {
            self.catch_up.cancel();
            return Vec::new();
        }
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

    // ── calls ───────────────────────────────────────────────────────

    #[test]
    fn a_missed_call_is_a_line_in_its_chat_and_unread_until_opened() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["+13165551111"], 1_000)]);
        assert!(p.m.index.get(10).unwrap().is_read);
        let fx = p.m.call_event("callReceived", "3165551111", 5_000, &p.store);
        assert!(fx.contains(&Effect::RefreshList));
        p.m.call_event("missedCall", "3165551111", 20_000, &p.store);
        let c = p.m.index.get(10).unwrap();
        assert_eq!(c.calls.len(), 1);
        assert!(c.calls[0].missed && !c.is_read);
        assert_eq!(c.sort_key(), 5_000);
        assert_eq!(c.preview(), "Missed call");
        let tl = c.timeline();
        assert_eq!(tl.len(), 2);
        assert!(tl[1].is_call() && tl[1].uid < 0);
        // The phone replays the chat's (read) newest text: still unread.
        p.merge(tup(1, 10, &["+13165551111"], 1_000));
        assert!(!p.m.index.get(10).unwrap().is_read);
        // Opening it: read, and the call is seen for good.
        p.m.select(10, &p.store, 0.0);
        assert!(p.m.index.get(10).unwrap().is_read);
        assert!(p.store.load_calls("dev1")[0].seen);
        // A text after the call: the preview is the text again.
        p.apply(&[tup(2, 10, &["+13165551111"], 30_000)]);
        assert_eq!(p.m.index.get(10).unwrap().preview(), "hi");
    }

    #[test]
    fn a_call_from_a_new_number_gets_its_own_chat_until_a_text_arrives() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["+13165551111"], 1_000)]);
        p.m.call_event("missedCall", "+13165552222", 5_000, &p.store);
        let draft = p.visible().iter().find(|c| c.thread_id < 0).map(|c| (c.thread_id, c.is_read, c.calls.len()));
        let (draft_id, read, n) = draft.expect("a call-only chat");
        assert!(!read && n == 1);
        // The same calls come back after a restart.
        let mut again = Messages::new();
        again.reset_for_device(Some("dev1"), true);
        again.load_cached(&p.store);
        assert!(again.index.conversations.values().any(|c| c.thread_id < 0 && c.calls.len() == 1));
        // A text from that number: the calls move into the real chat.
        p.apply(&[tup(2, 20, &["3165552222"], 9_000)]);
        assert!(p.m.index.get(draft_id).is_none());
        assert_eq!(p.m.index.get(20).unwrap().calls.len(), 1);
        // Hidden callers make no chat.
        p.m.call_event("missedCall", "unknown number", 10_000, &p.store);
        assert_eq!(p.visible().len(), 2);
    }

    // ── read on the phone (Google Messages) ─────────────────────────

    #[test]
    fn read_on_the_phone_clears_the_dot_unless_this_side_has_newer() {
        let mut p = Panel::new();
        let unread = |uid, tid, a: &str, date| RawMessage { read: 0, ..tup(uid, tid, &[a], date) };
        p.apply(&[
            unread(1, 10, "+13165551111", 1_000_000),
            unread(2, 20, "shop_agent@rbm.goog", 2_000_000),
            unread(3, 30, "+13165553333", 9_000_000),
        ]);
        assert_eq!(p.m.gm_chat_key(20).as_deref(), Some("shop_agent@rbm.goog"));
        let read = HashMap::from([
            ("3165551111".to_string(), 1_000_500),
            ("shop_agent@rbm.goog".to_string(), 2_000_000),
            // The phone's state predates this side's newest text.
            ("3165553333".to_string(), 1_000_000),
        ]);
        let effects = p.m.read_on_phone(&read, &p.store);
        assert!(effects.contains(&Effect::RefreshList));
        // A group listing my own number matches once Google Messages says it's mine.
        p.apply(&[RawMessage { read: 0, ..tup(4, 40, &["+13165550000", "+13165551111", "+13165552222"], 3_000_000) }]);
        let group = HashMap::from([("3165551111,3165552222".to_string(), 3_000_000)]);
        p.m.read_on_phone(&group, &p.store);
        assert!(!p.m.index.get(40).unwrap().is_read);
        p.m.gm_me = vec!["3165550000".into()];
        p.m.read_on_phone(&group, &p.store);
        assert!(p.m.index.get(40).unwrap().is_read);
        let is_read = |p: &Panel, t| p.m.index.get(t).unwrap().is_read;
        assert!(is_read(&p, 10) && is_read(&p, 20));
        assert!(!is_read(&p, 30));
        // Nothing left to change: no refresh.
        assert!(p.m.read_on_phone(&read, &p.store).is_empty());
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
            internal_id: format!("0|com.google.android.apps.messaging|{id}|null|10123"),
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
            vec![Effect::ReplyViaNotification {
                thread_id: 10,
                reply_id: "reply-n1".into(),
                text: "Yes, leaving now".into()
            }]
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
        p.m.note_notification(ReplyableNotification {
            app_name: "Gmail".into(),
            internal_id: "0|com.google.android.gm|3|null|10100".into(),
            ..notif("n3", "Alex", "Running ten minutes late")
        });
        p.m.note_notification(ReplyableNotification {
            reply_id: String::new(),
            ..notif("n4", "Alex", "Running ten minutes late")
        });
        // Any app can call itself "Messages": the package decides.
        p.m.note_notification(ReplyableNotification {
            internal_id: "0|com.evil.app|7|null|10999".into(),
            ..notif("n7", "Alex", "Running ten minutes late")
        });
        // A short preview fits too many texts to trust.
        p.m.note_notification(notif("n8", "Alex", "Run"));
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
        // Only the messages on screen.
        assert_eq!(p.m.take_full_image_requests(10, &HashSet::from([2]), &cache), vec![(8, "PART_8.jpg".to_string())]);
        let mut asked = p.m.take_full_image_requests(10, &HashSet::from([1, 2]), &cache);
        asked.sort();
        assert_eq!(asked, vec![(7, "PART_7.jpg".to_string())]);
        assert!(p.m.take_full_image_requests(10, &HashSet::from([1, 2]), &cache).is_empty(), "asked twice");

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
        assert!(p.m.take_full_image_requests(10, &HashSet::from([1]), dir.path()).is_empty());
        p.m.set_reachable(true);
        assert_eq!(p.m.take_full_image_requests(10, &HashSet::from([1]), dir.path()).len(), 1);
    }

    #[test]
    fn renaming_a_number_names_its_chats_and_is_saved() {
        let mut p = Panel::new();
        p.apply(&[tup(1, 10, &["+13165551111"], 100)]);
        let effects = p.m.rename_contact("+1 316 555 1111", " Alex ", &p.store);
        assert_eq!(effects, vec![Effect::RefreshList]);
        assert_eq!(p.m.index.get(10).unwrap().display_name, "Alex");
        assert_eq!(p.store.load_contact_overrides().get("13165551111").map(String::as_str), Some("Alex"));
        assert!(p.store.load_contacts().is_empty());
        assert!(p.m.rename_contact("+13165551111", "  ", &p.store).is_empty());
    }

    fn messages_notification(title: &str, text: &str) -> crate::core::contacts::Props {
        [("appName", "Messages"), ("title", title), ("text", text), ("ticker", "")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::from(v)))
            .collect()
    }

    #[test]
    fn texts_that_never_arrive_raise_the_hidden_texts_warning() {
        let mut p = Panel::new();
        let t0 = 1_800_000_000.0;
        let date_ms = (t0 * 1000.0) as i64;
        p.apply(&[incoming(1, 10, "3165551111", "This one did arrive fine", date_ms)]);
        // A notification whose text arrived: no warning.
        let (schedule, _) =
            p.m.notification_seen(&messages_notification("Alex", "This one did arrive fine"), t0, &p.store, true);
        assert!(schedule);
        // Two that never arrive.
        p.m.notification_seen(&messages_notification("Sam", "Where are you right now?"), t0 + 10.0, &p.store, true);
        p.m.notification_seen(&messages_notification("Sam", "Hello?? Call me back"), t0 + 20.0, &p.store, true);
        assert!(p.m.check_hidden_texts(t0 + 1000.0));
        assert!(p.m.hidden_texts.warning);
    }

    #[test]
    fn a_notification_teaches_and_saves_a_contact_name() {
        let mut p = Panel::new();
        p.apply(&[incoming(1, 10, "3165551111", "See you at the game tonight", 100)]);
        let (_, effects) = p.m.notification_seen(
            &messages_notification("Alex Smith", "See you at the game tonight"),
            0.0,
            &p.store,
            false,
        );
        assert_eq!(effects, vec![Effect::RefreshList]);
        assert_eq!(p.m.index.get(10).unwrap().display_name, "Alex Smith");
        assert_eq!(p.store.load_contacts().get("3165551111").map(String::as_str), Some("Alex Smith"));
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
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::DeleteOnPhone { thread_id: 10, addresses } if !addresses.is_empty()))
        );
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
        assert_eq!(
            acts,
            vec![Effect::Sync(Action::ReplayFromDaemon { thread_id: 10, start: 0, end: GAP_FILL_MESSAGES })]
        );
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

    // ── Google Messages as the source ───────────────────────────────

    mod google {
        use super::*;
        use crate::core::gm::Member;

        fn chat(conv: &str, numbers: &[&str], unread: bool, ts: i64) -> GmConversation {
            GmConversation {
                chat: numbers.join(","),
                conv: conv.into(),
                members: numbers.iter().map(|n| Member { number: n.to_string(), name: String::new() }).collect(),
                group: numbers.len() > 1,
                rcs: true,
                unread,
                last_ts_ms: ts,
                preview: "preview".into(),
                ..Default::default()
            }
        }

        fn gmsg(conv: &str, id: &str, from_me: bool, ts: i64) -> GmMessage {
            GmMessage {
                conv: conv.into(),
                msg: id.into(),
                ts_ms: ts,
                from_me,
                from: if from_me { String::new() } else { "+13165551111".into() },
                text: format!("text {id}"),
                status: if from_me { "delivered".into() } else { "received".into() },
                rcs: true,
                ..Default::default()
            }
        }

        fn panel() -> Panel {
            let mut p = Panel::new();
            p.m.reset_for_device(Some("gm:3165550000"), true);
            p
        }

        impl Panel {
            fn chat(&mut self, c: &GmConversation) -> Vec<Effect> {
                let tid = self.store.gm_local_id("conv", &c.conv);
                self.m.gm_conversation(c, tid, &self.store, &mut self.hidden)
            }
            fn msg(&mut self, m: &GmMessage) -> Vec<Effect> {
                let tid = self.store.gm_local_id("conv", &m.conv);
                let uid = self.store.gm_local_id("msg", &m.msg);
                self.m.gm_message(m, tid, uid, &self.store, &mut self.hidden, 0.0)
            }
        }

        #[test]
        fn chats_list_from_conversation_events_and_keep_the_phones_unread() {
            let mut p = panel();
            assert!(p.m.is_gm());
            p.chat(&chat("a", &["+13165551111"], true, 2000));
            p.chat(&chat("b", &["+13165552222", "+13165553333"], false, 1000));
            let v = p.visible();
            assert_eq!(v.len(), 2);
            assert_eq!((v[0].last_message.as_str(), v[0].is_read), ("preview", false));
            assert!(v[1].is_read && v[1].is_group());
            // Read on the phone: the dot clears.
            p.chat(&chat("a", &["+13165551111"], false, 2000));
            assert!(p.visible()[0].is_read);
            // Stored under the gm device, so the next start shows them at once.
            assert_eq!(p.store.load_conversations("gm:3165550000").len(), 2);
            // No KDE Connect catch-up while Google Messages is the source.
            assert!(p.drain().is_empty());
        }

        #[test]
        fn opening_a_chat_fetches_it_once_and_marks_it_read() {
            let mut p = panel();
            p.chat(&chat("a", &["+13165551111"], true, 2000));
            let tid = p.store.gm_local_id("conv", "a");
            let e = p.m.select(tid, &p.store, 0.0);
            assert!(e.contains(&Effect::MarkRead { thread_id: tid }));
            assert!(e.contains(&Effect::GmFetch { thread_id: tid }));
            assert!(!e.iter().any(|e| matches!(e, Effect::Sync(_))));
            assert_eq!(p.m.gm_convs(tid), vec!["a".to_string()]);
            p.m.select(-99, &p.store, 0.0);
            let e = p.m.select(tid, &p.store, 0.0);
            assert!(!e.contains(&Effect::GmFetch { thread_id: tid }));
        }

        #[test]
        fn messages_merge_and_a_live_one_in_the_open_chat_is_marked_read() {
            let mut p = panel();
            p.chat(&chat("a", &["+13165551111"], false, 2000));
            let tid = p.store.gm_local_id("conv", "a");
            let mut old = gmsg("a", "1", false, 1000);
            old.old = true;
            p.msg(&old);
            p.msg(&gmsg("a", "2", true, 2000));
            let conv = p.m.index.get(tid).unwrap();
            assert_eq!(conv.messages.len(), 2);
            assert!(conv.is_read);
            // A live text in another chat is unread.
            p.chat(&chat("b", &["+13165552222"], false, 500));
            let mut live = gmsg("b", "3", false, 3000);
            live.from = "+13165552222".into();
            p.msg(&live);
            let b = p.store.gm_local_id("conv", "b");
            assert!(!p.m.index.get(b).unwrap().is_read);
            // ...and in the open chat, it is marked read on the phone.
            p.m.select(tid, &p.store, 0.0);
            let e = p.msg(&gmsg("a", "4", false, 4000));
            assert!(e.contains(&Effect::MarkRead { thread_id: tid }));
            assert!(e.contains(&Effect::ShowThread(tid)));
            // The same message again changes nothing.
            assert!(p.msg(&gmsg("a", "4", false, 4000)).is_empty());
        }

        #[test]
        fn sending_goes_through_google_and_shows_its_delivery() {
            let mut p = panel();
            p.chat(&chat("a", &["+13165551111"], false, 2000));
            let tid = p.store.gm_local_id("conv", "a");
            p.m.select(tid, &p.store, 0.0);
            let e = p.m.send(tid, "hello");
            let [Effect::GmSend { thread_id, conv, text, tmp_id, .. }] = e.as_slice() else { panic!("{e:?}") };
            assert_eq!((*thread_id, conv.as_str(), text.as_str()), (tid, "a", "hello"));
            // The echo carries the tmp id; then its delivery updates.
            let mut echo = gmsg("a", "9", true, 3000);
            echo.tmp_id = tmp_id.clone();
            echo.text = "hello".into();
            echo.status = "sending".into();
            p.msg(&echo);
            let uid = p.store.gm_local_id("msg", "9");
            assert_eq!(p.m.route_of(uid).map(SendRoute::label), Some("RCS · Sending"));
            echo.status = "read".into();
            assert_eq!(p.msg(&echo), vec![Effect::Redraw(tid)]);
            assert_eq!(p.m.route_of(uid).map(SendRoute::label), Some("RCS · Read"));
            // A real phone echoes without the tmp id: the text matches it.
            let [Effect::GmSend { .. }] = p.m.send(tid, "second one").as_slice() else { panic!() };
            let mut echo2 = gmsg("a", "12", true, 3050);
            echo2.text = "second one".into();
            p.msg(&echo2);
            assert_eq!(p.m.route_of(p.store.gm_local_id("msg", "12")).map(SendRoute::label), Some("RCS · Delivered"));
            // Texts sent from the phone show no route, unless they failed.
            p.msg(&gmsg("a", "10", true, 3100));
            assert_eq!(p.m.route_of(p.store.gm_local_id("msg", "10")), None);
            let mut failed = gmsg("a", "11", true, 3200);
            failed.status = "failed".into();
            p.msg(&failed);
            assert_eq!(p.m.route_of(p.store.gm_local_id("msg", "11")).map(SendRoute::label), Some("RCS · Not sent"));
            // Pictures can't go this way yet; read-only chats refuse.
            // Pictures go one per message, the text with the first.
            let e = p.m.send_with(tid, " look ", vec!["/tmp/a.jpg".into(), "/tmp/b.png".into()]);
            let files: Vec<_> = e
                .iter()
                .map(|e| match e {
                    Effect::GmSend { text, file, .. } => (text.as_str(), file.as_deref()),
                    _ => panic!("{e:?}"),
                })
                .collect();
            assert_eq!(files, [("look", Some("/tmp/a.jpg")), ("", Some("/tmp/b.png"))]);
            let Effect::GmSend { tmp_id, text, file, .. } = &e[1] else { panic!() };
            let uid = p.store.gm_local_id("msg", &format!("pending:{tmp_id}"));
            p.m.gm_add_pending((tmp_id, text, file.as_deref()), uid, &p.store, &mut p.hidden, 0.0);
            let shown = p.m.index.get(tid).unwrap().messages.iter().find(|m| m.uid == uid).unwrap();
            assert_eq!(shown.attachments[0].mime_type, "image/png");
            assert_eq!(shown.attachments[0].extra["fullPath"], "/tmp/b.png");
            let mut biz = chat("biz", &["shop_agent@rbm.goog"], false, 100);
            biz.read_only = true;
            biz.name = "The Shop".into();
            p.chat(&biz);
            let b = p.store.gm_local_id("conv", "biz");
            assert_eq!(p.m.index.get(b).unwrap().display_name, "The Shop");
            assert_eq!(p.m.send(b, "hi"), vec![Effect::Toast(TOAST_GM_READ_ONLY)]);
        }

        /// What the page does with a send: show it at once, then send.
        fn send_now(p: &mut Panel, tid: i64, text: &str) -> (String, i64, Vec<Effect>) {
            let e = p.m.send(tid, text);
            let [Effect::GmSend { tmp_id, .. }] = e.as_slice() else { panic!("{e:?}") };
            let uid = p.store.gm_local_id("msg", &format!("pending:{tmp_id}"));
            let shown = p.m.gm_add_pending((tmp_id, text, None), uid, &p.store, &mut p.hidden, 0.0);
            (tmp_id.clone(), uid, shown)
        }

        /// The page's id for an incoming message: a pending bubble's, if it's its echo.
        fn echo(p: &mut Panel, m: &GmMessage) -> (i64, Vec<Effect>) {
            let tid = p.store.gm_local_id("conv", &m.conv);
            let uid = match (p.store.gm_known("msg", &m.msg), p.m.gm_pending_uid(m, tid)) {
                (Some(uid), _) => uid,
                (None, Some(uid)) => {
                    p.store.gm_alias("msg", &m.msg, uid);
                    uid
                }
                (None, None) => p.store.gm_local_id("msg", &m.msg),
            };
            (uid, p.m.gm_message(m, tid, uid, &p.store, &mut p.hidden, 0.0))
        }

        #[test]
        fn a_sent_text_shows_at_once_and_its_echo_takes_the_same_bubble() {
            let mut p = panel();
            p.chat(&chat("a", &["+13165551111"], false, 2000));
            let tid = p.store.gm_local_id("conv", "a");
            p.m.select(tid, &p.store, 0.0);
            let (_, uid, shown) = send_now(&mut p, tid, "on my way");
            assert!(shown.contains(&Effect::ShowThread(tid)), "{shown:?}");
            let conv = p.m.index.get(tid).unwrap();
            assert_eq!(
                conv.messages.last().map(|m| (m.uid, m.body.as_str(), m.is_sent())),
                Some((uid, "on my way", true))
            );
            assert_eq!(p.m.route_of(uid).map(SendRoute::label), Some("RCS · Sending"));
            // Two of the same text: each echo takes its own bubble.
            let (_, uid2, _) = send_now(&mut p, tid, "on my way");
            assert_ne!(uid, uid2);
            let mut e = gmsg("a", "50", true, 3000);
            e.text = "on my way".into();
            e.status = "sending".into();
            let (got, _) = echo(&mut p, &e);
            assert!(got == uid || got == uid2);
            e.status = "delivered".into();
            let (again, fx) = echo(&mut p, &e);
            assert_eq!((again, fx), (got, vec![Effect::Redraw(tid)]));
            let mut e2 = gmsg("a", "51", true, 3001);
            e2.text = "on my way".into();
            let (got2, _) = echo(&mut p, &e2);
            assert_eq!([got, got2].iter().copied().collect::<HashSet<_>>(), [uid, uid2].into_iter().collect());
            assert_eq!(p.m.index.get(tid).unwrap().messages.len(), 2, "no doubled bubbles");
            assert_eq!(p.m.route_of(got).map(SendRoute::label), Some("RCS · Delivered"));
            // A failed send keeps its bubble, marked, but not on disk.
            let (tmp, uid3, _) = send_now(&mut p, tid, "nope");
            let fx = p.m.gm_send_failed(&tmp, &p.store);
            assert!(fx.contains(&Effect::Redraw(tid)) && fx.contains(&Effect::Toast(TOAST_GM_NOT_SENT)));
            assert_eq!(p.m.route_of(uid3).map(SendRoute::label), Some("RCS · Not sent"));
        }

        #[test]
        fn reactions_and_replies_show_and_go_to_the_phone() {
            use crate::core::gm::GmReaction;
            let mut p = panel();
            p.chat(&chat("a", &["+13165551111"], false, 2000));
            let tid = p.store.gm_local_id("conv", "a");
            let mut q = gmsg("a", "1", false, 1000);
            q.text = "dinner?".into();
            q.reactions =
                vec![GmReaction { emoji: "👍".into(), count: 1, mine: false, by: vec!["+13165551111".into()] }];
            p.msg(&q);
            let mut r = gmsg("a", "2", true, 1500);
            r.text = "yes".into();
            r.reply_to = "1".into();
            p.msg(&r);
            p.m.select(tid, &p.store, 0.0);
            let (u1, u2) = (p.store.gm_local_id("msg", "1"), p.store.gm_local_id("msg", "2"));
            let ex = p.m.gm_extras(tid, &p.store);
            assert_eq!(
                ex[&u1].reactions.iter().map(|c| (c.emoji.as_str(), c.count, c.mine)).collect::<Vec<_>>(),
                [("👍", 1, false)]
            );
            let quote = ex[&u2].quote.clone().unwrap();
            assert_eq!((quote.text.as_str(), quote.uid), ("dinner?", Some(u1)));
            assert!(ex[&u2].reactions.is_empty());
            // My reaction shows at once and goes to the phone.
            let fx = p.m.gm_react(u1, "😂");
            assert_eq!(
                fx,
                vec![
                    Effect::GmReact {
                        thread_id: tid,
                        conv: "a".into(),
                        msg: "1".into(),
                        emoji: "😂".into(),
                        action: "add"
                    },
                    Effect::Redraw(tid)
                ]
            );
            assert!(p.m.gm_extras(tid, &p.store)[&u1].reactions.iter().any(|c| c.emoji == "😂" && c.mine));
            // Their reaction arriving live redraws the chat.
            let mut later = q.clone();
            later.reactions.push(GmReaction { emoji: "😍".into(), count: 1, ..Default::default() });
            assert_eq!(echo(&mut p, &later).1, vec![Effect::Redraw(tid)]);
            // A reply carries the message it answers, once.
            p.m.gm_reply_next(Some("1".into()));
            let e = p.m.send(tid, "see you there");
            assert!(matches!(&e[..], [Effect::GmSend { reply_to: Some(r), .. }] if r == "1"), "{e:?}");
            let e = p.m.send(tid, "and dessert");
            assert!(matches!(&e[..], [Effect::GmSend { reply_to: None, .. }]), "{e:?}");
            // A refusal says so and fetches the phone's truth.
            assert_eq!(
                p.m.gm_react_failed(tid),
                vec![Effect::Toast(TOAST_GM_REACT_FAILED), Effect::GmFetch { thread_id: tid }]
            );
        }

        #[test]
        fn typing_shows_for_a_while_and_stops_with_their_text() {
            let mut p = panel();
            p.chat(&chat("a", &["+13165551111", "+13165552222"], false, 2000));
            let tid = p.store.gm_local_id("conv", "a");
            p.m.contact_map.insert(normalize_phone("+13165551111"), "Sam Ortiz".into());
            p.m.select(tid, &p.store, 0.0);
            assert_eq!(p.m.gm_typing(tid, "+13165551111", true, 100.0), vec![Effect::Typing(tid)]);
            assert_eq!(p.m.typing_label(tid, 101.0), Some(("Sam is typing…".to_string(), 100.0 + TYPING_SHOWN_SECS)));
            p.m.gm_typing(tid, "+13165552222", true, 102.0);
            assert_eq!(p.m.typing_label(tid, 103.0).unwrap().0, "Sam and +13165552222 are typing…");
            // Runs out without news.
            assert_eq!(p.m.typing_label(tid, 200.0), None);
            // Their text ends it.
            p.m.gm_typing(tid, "+13165551111", true, 300.0);
            let mut m = gmsg("a", "1", false, 3000);
            m.from = "+13165551111".into();
            assert!(p.msg(&m).contains(&Effect::Typing(tid)));
            assert_eq!(p.m.typing_label(tid, 301.0), None);
            // Mine goes to the phone, at most every few seconds.
            assert_eq!(p.m.gm_typing_out(tid, 10.0), vec![Effect::GmTyping { conv: "a".into() }]);
            assert!(p.m.gm_typing_out(tid, 12.0).is_empty());
            assert_eq!(p.m.gm_typing_out(tid, 16.0).len(), 1);
            // Not in an SMS chat.
            let mut sms = chat("b", &["+13165553333"], false, 2000);
            sms.rcs = false;
            p.chat(&sms);
            assert!(p.m.gm_typing_out(p.store.gm_local_id("conv", "b"), 100.0).is_empty());
        }

        #[test]
        fn older_pages_come_while_the_phone_has_more() {
            let mut p = panel();
            p.chat(&chat("a", &["+13165551111"], false, 2000));
            let tid = p.store.gm_local_id("conv", "a");
            p.m.select(tid, &p.store, 0.0);
            assert!(!p.m.gm_more_history(tid));
            let c1 = Cursor { id: "m50".into(), ts_ms: 50 };
            assert_eq!(p.m.gm_page("a", Some(c1.clone()), false, 50), vec![Effect::MoreHistory(tid)]);
            assert!(p.m.gm_more_history(tid));
            assert_eq!(p.m.gm_older(tid), vec![Effect::GmFetchOlder { thread_id: tid, conv: "a".into(), cursor: c1 }]);
            assert!(p.m.gm_loading_older(tid));
            assert!(p.m.gm_older(tid).is_empty(), "one page at a time");
            // A newest page again doesn't undo the older one.
            let c2 = Cursor { id: "m100".into(), ts_ms: 10 };
            p.m.gm_page("a", Some(c2.clone()), true, 50);
            p.m.gm_page("a", Some(Cursor { id: "m50".into(), ts_ms: 50 }), false, 50);
            assert_eq!(p.m.gm_older(tid), vec![Effect::GmFetchOlder { thread_id: tid, conv: "a".into(), cursor: c2 }]);
            // An empty page: no more.
            p.m.gm_page("a", Some(Cursor { id: "x".into(), ts_ms: 1 }), true, 0);
            assert!(!p.m.gm_more_history(tid) && !p.m.gm_loading_older(tid));
        }

        #[test]
        fn archive_pin_mute_and_block() {
            let mut p = panel();
            p.chat(&chat("a", &["+13165551111"], false, 3000));
            p.chat(&chat("b", &["+13165552222"], false, 2000));
            let mut arch = chat("c", &["+13165553333"], false, 1000);
            arch.archived = true;
            p.chat(&arch);
            let (a, b, c) =
                (p.store.gm_local_id("conv", "a"), p.store.gm_local_id("conv", "b"), p.store.gm_local_id("conv", "c"));
            let shown = |p: &Panel| p.m.visible(&p.hidden).iter().map(|c| c.thread_id).collect::<Vec<_>>();
            assert_eq!(shown(&p), [a, b]);
            assert_eq!(p.m.archived_count(), 1);
            p.m.set_show_archived(true);
            assert_eq!(shown(&p), [c]);
            p.m.set_show_archived(false);
            // Archiving shows at once and goes to the phone.
            let fx = p.m.chat_action(a, ChatAction::Archive, &p.store);
            assert_eq!(fx[0], Effect::GmUpdateChat { thread_id: a, conv: "a".into(), change: ChatChange::Archive });
            assert_eq!(shown(&p), [b]);
            // Refused: back it comes, with a word.
            assert_eq!(
                p.m.gm_chat_changed(a, ChatChange::Archive, false, &p.store)[0],
                Effect::Toast(TOAST_GM_CHAT_CHANGE_FAILED)
            );
            assert_eq!(shown(&p), [a, b]);
            // Pins are kept here (and survive a restart); mute goes to the phone too.
            assert_eq!(p.m.chat_action(b, ChatAction::Pin, &p.store), vec![Effect::RefreshList]);
            assert!(matches!(
                p.m.chat_action(b, ChatAction::Mute, &p.store)[0],
                Effect::GmUpdateChat { change: ChatChange::Mute, .. }
            ));
            let f = p.m.chat_flags();
            assert!(f[&b].pinned && f[&b].muted && f[&b].gm && !f[&a].pinned);
            p.m.reset_for_device(Some("gm:3165550000"), true);
            p.m.load_cached(&p.store);
            let f = p.m.chat_flags();
            assert!(f[&b].pinned && f[&b].muted && f[&c].archived);
            // The phone's own pin shows too.
            let mut pinned = chat("a", &["+13165551111"], false, 3000);
            pinned.pinned = true;
            p.chat(&pinned);
            assert!(p.m.chat_flags()[&a].pinned);
            // Blocked: it leaves the list once the phone took it.
            p.m.chat_action(a, ChatAction::Block, &p.store);
            assert!(shown(&p).contains(&a));
            p.m.gm_chat_changed(a, ChatChange::Block, true, &p.store);
            assert!(!shown(&p).contains(&a));
            // Moved to spam on the phone: same.
            let mut spam = chat("b", &["+13165552222"], false, 2000);
            spam.blocked = true;
            p.chat(&spam);
            assert!(!shown(&p).contains(&b));
        }

        #[test]
        fn groups_start_from_several_people_and_send_pictures_first() {
            let mut p = panel();
            p.chat(&chat("g", &["+13165551111", "+13165552222"], false, 2000));
            let g = p.store.gm_local_id("conv", "g");
            // The same people: the group that is there.
            let e = p.m.open_group(&["+1 316 555 2222".into(), "+13165551111".into()], "");
            assert!(e.contains(&Effect::ShowThread(g)), "{e:?}");
            // New people: a draft, made on the phone with its first text and pictures.
            let e = p.m.open_group(&["+13165551111".into(), "+13165553333".into()], "");
            let Some(Effect::ShowThread(draft)) = e.last().cloned() else { panic!("{e:?}") };
            assert!(draft < 0);
            let e = p.m.send_with(draft, "hi all", vec!["/tmp/a.jpg".into(), "/tmp/b.pdf".into()]);
            assert_eq!(
                e[0],
                Effect::GmSendNew {
                    addresses: vec!["+13165551111".into(), "+13165553333".into()],
                    text: "hi all".into(),
                    attachments: vec!["/tmp/a.jpg".into(), "/tmp/b.pdf".into()]
                }
            );
            p.chat(&chat("n", &["+13165551111", "+13165553333"], false, 3000));
            let tid = p.store.gm_local_id("conv", "n");
            let sends: Vec<(String, Option<String>)> =
                p.m.gm_opened(tid, "n", "hi all", vec!["/tmp/a.jpg".into(), "/tmp/b.pdf".into()])
                    .into_iter()
                    .filter_map(|e| match e {
                        Effect::GmSend { text, file, .. } => Some((text, file)),
                        _ => None,
                    })
                    .collect();
            assert_eq!(
                sends,
                [("hi all".into(), Some("/tmp/a.jpg".into())), (String::new(), Some("/tmp/b.pdf".into()))]
            );
        }

        #[test]
        fn a_preview_picture_gives_way_to_the_full_one() {
            let mut p = panel();
            p.chat(&chat("a", &["+13165551111"], false, 2000));
            let tid = p.store.gm_local_id("conv", "a");
            p.m.select(tid, &p.store, 0.0);
            let att = |id: &str, thumb: bool| crate::core::gm::GmAttachment {
                id: id.into(),
                mime: "image/jpeg".into(),
                key: "a2V5".into(),
                thumb,
                part: "p1".into(),
                ..Default::default()
            };
            let mut m = gmsg("a", "1", false, 1000);
            m.attachments = vec![att("thumb", true)];
            p.msg(&m);
            m.attachments = vec![att("full", false)];
            assert_eq!(p.msg(&m), vec![Effect::Redraw(tid)]);
            let uid = p.store.gm_local_id("msg", "1");
            let row = p.m.index.get(tid).unwrap().messages.iter().find(|x| x.uid == uid).unwrap();
            assert_eq!(row.attachments[0].unique_identifier, "gm:full");
            // Kept that way on disk.
            let saved = p.store.load_conversations("gm:3165550000");
            assert!(
                saved
                    .values()
                    .flat_map(|c| &c.messages)
                    .any(|x| { x.attachments.iter().any(|a| a.unique_identifier == "gm:full") })
            );
            // A late copy of the preview doesn't take its place again.
            m.attachments = vec![att("thumb", true)];
            assert!(p.msg(&m).is_empty());
        }

        #[test]
        fn pictures_on_screen_download_once_and_land_in_the_cache() {
            let mut p = panel();
            p.chat(&chat("a", &["+13165551111"], false, 2000));
            let tid = p.store.gm_local_id("conv", "a");
            let mut pic = gmsg("a", "1", false, 1000);
            pic.attachments = vec![crate::core::gm::GmAttachment {
                id: "m1".into(),
                mime: "image/jpeg".into(),
                key: "a2V5".into(),
                ..Default::default()
            }];
            p.msg(&pic);
            let uid = p.store.gm_local_id("msg", "1");
            let dir = tempfile::tempdir().unwrap();
            let cache = dir.path().join("cache");
            let on_screen: HashSet<i64> = [uid].into();
            let want = GmDownload { media: "m1".into(), key: "a2V5".into(), unique: "gm:m1".into() };
            assert_eq!(p.m.take_gm_downloads(tid, &on_screen, &cache), vec![want.clone()]);
            assert!(p.m.take_gm_downloads(tid, &on_screen, &cache).is_empty(), "asked once");
            // A click asks again.
            assert_eq!(p.m.gm_download_for("gm:m1"), Some(want));
            let got = dir.path().join("got");
            std::fs::write(&got, b"jpeg").unwrap();
            let (t, dest) = p.m.gm_downloaded("gm:m1", &got, &p.store, &cache).unwrap();
            assert_eq!((t, std::fs::read(&dest).unwrap()), (tid, b"jpeg".to_vec()));
            let att = &p.m.index.get(tid).unwrap().messages[0].attachments[0];
            assert_eq!(attachments::existing_full_in(&cache, att), Some(dest));
            // A row saved without its key takes it when the phone sends it again.
            let mut old = gmsg("a", "2", false, 1100);
            old.attachments =
                vec![crate::core::gm::GmAttachment { id: "m2".into(), mime: "image/png".into(), ..Default::default() }];
            p.msg(&old);
            let uid2 = p.store.gm_local_id("msg", "2");
            assert!(p.m.take_gm_downloads(tid, &[uid2].into(), &cache).is_empty(), "no key yet");
            old.attachments[0].key = "a2V5Mg==".into();
            p.msg(&old);
            assert_eq!(p.m.take_gm_downloads(tid, &[uid2].into(), &cache).len(), 1);
        }

        #[test]
        fn deleted_messages_leave_and_names_come_from_the_phone() {
            let mut p = panel();
            let mut c = chat("a", &["+13165551111"], false, 2000);
            c.members[0].name = "Alice".into();
            p.chat(&c);
            let tid = p.store.gm_local_id("conv", "a");
            assert_eq!(p.m.index.get(tid).unwrap().display_name, "Alice");
            p.msg(&gmsg("a", "1", false, 1000));
            let mut gone = gmsg("a", "1", false, 1000);
            gone.deleted = true;
            p.msg(&gone);
            assert!(p.m.index.get(tid).unwrap().messages.is_empty());
            // A named group keeps its name.
            let mut g = chat("g", &["+13165552222", "+13165553333"], false, 10);
            g.name = "Family".into();
            p.chat(&g);
            assert_eq!(p.m.index.get(p.store.gm_local_id("conv", "g")).unwrap().display_name, "Family");
            p.m.refresh_names();
            assert_eq!(p.m.index.get(p.store.gm_local_id("conv", "g")).unwrap().display_name, "Family");
        }

        #[test]
        fn a_new_chat_is_made_on_the_phone_then_sent_to() {
            let mut p = panel();
            p.m.open_or_create("+13165559999", "");
            let draft = p.m.active_thread().unwrap();
            let e = p.m.send(draft, "hi me");
            assert_eq!(
                e[0],
                Effect::GmSendNew { addresses: vec!["+13165559999".into()], text: "hi me".into(), attachments: vec![] }
            );
            // The phone answers with the chat; its conversation event lands first.
            p.chat(&chat("new", &["+13165559999"], false, 0));
            let tid = p.store.gm_local_id("conv", "new");
            let e = p.m.gm_opened(tid, "new", "hi me", vec![]);
            assert!(matches!(&e[0], Effect::GmSend { conv, text, .. } if conv == "new" && text == "hi me"));
            assert!(e.contains(&Effect::ShowThread(tid)));
            assert_eq!(p.m.active_thread(), Some(tid));
        }

        #[test]
        fn a_kde_device_leaves_google_mode() {
            let mut p = panel();
            p.m.reset_for_device(Some("dev1"), true);
            assert!(!p.m.is_gm());
        }
    }
}
