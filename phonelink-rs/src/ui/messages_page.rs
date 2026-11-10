//! The Messages tab: wires [`Messages`] (the logic) to the daemon and the
//! widgets. Port of the GTK half of `phonelink/ui/sms_panel.py`.
//!
//! Everything runs on the GLib main loop. Signals are queued and merged in
//! batches of [`SIGNAL_BATCH_SIZE`] per idle callback, inside one store
//! transaction (rule 3). Catch-up runs on one timer set to the controller's
//! next deadline.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use adw::prelude::*;
use glib::Variant;

use super::conversation_list::{ConversationList, ListEvent};
use super::google_sync::GoogleSync;
use super::notifications_model::{NoteEvent, NotificationsModel, Props};
use super::thread_view::{BubbleActions, ThreadView};
use crate::core::attachments::full_cache_dir;
use crate::core::autocap::AutocapRules;
use crate::core::contacts::load_contact_map;
use crate::core::contacts::notification_message_text;
use crate::core::message::parse_message;
use crate::core::messages::{Effect, GmDownload, Messages, ReplyableNotification};
use crate::core::models::Device;
use crate::core::settings::Settings;
use crate::core::store::MessageStore;
use crate::core::sync::{self, Action, SIGNAL_BATCH_SIZE};
use crate::dbus::signals::{self, Subscription};
use crate::dbus::{Client, IFACE_CONTACTS, IFACE_CONVERSATIONS, IFACE_SHARE, IFACE_TELEPHONY, device_path, variant};

/// Google Messages chats/messages merged per idle callback.
const GM_BATCH_SIZE: usize = 60;
/// Messages fetched when a chat is opened.
const TOAST_GM_DOWNLOAD_FAILED: &str = "Couldn't download that file from Google Messages";
const GM_FETCH_COUNT: u32 = 50;

fn epoch_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Notification properties as `core::contacts::Props` (strings and flags).
fn as_props(props: &Props) -> crate::core::contacts::Props {
    props
        .iter()
        .map(|(k, v)| {
            let value = match v.get::<bool>() {
                Some(b) => serde_json::Value::Bool(b),
                None => serde_json::Value::from(variant::string(v)),
            };
            (k.clone(), value)
        })
        .collect()
}

/// A notification's properties in the shape `core` reads.
fn replyable(id: &str, props: &Props) -> ReplyableNotification {
    let text_of = |k: &str| props.get(k).map(variant::string).unwrap_or_default();
    let json: crate::core::contacts::Props =
        ["appName", "title", "text", "ticker"].iter().map(|k| (k.to_string(), text_of(k).into())).collect();
    ReplyableNotification {
        public_id: id.to_string(),
        app_name: text_of("appName"),
        title: text_of("title"),
        text: notification_message_text(&json),
        reply_id: text_of("replyId"),
        internal_id: text_of("internalId"),
    }
}

/// Monotonic seconds, as the controller expects.
fn now() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

struct Inner {
    m: Messages,
    store: MessageStore,
    settings: Rc<RefCell<Settings>>,
    client: Option<Client>,
    subs: Vec<Subscription>,
    queue: VecDeque<Variant>,
    draining: bool,
    timer: Option<glib::SourceId>,
    hidden_check: Option<glib::SourceId>,
    /// Debounced reload after conversationLoaded (as Python, 500 ms).
    loaded_reload: Option<glib::SourceId>,
    /// Reachability at the last set_device, to spot a reconnect.
    reachable_seen: bool,
    /// Devices already warned that KDE Connect synced no contacts.
    contact_sync_warned: std::collections::HashSet<String>,
    /// Full pictures to ask the phone for, one at a time (22 at once
    /// timed out the daemon on the laptop).
    full_queue: VecDeque<(i64, String)>,
    /// Requests to the phone in flight (at most FULL_PICTURE_WORKERS).
    full_pumping: usize,
    /// Google Messages attachments waiting to download (and whether to
    /// open each once here), and how many are downloading.
    gm_downloads: VecDeque<(GmDownload, bool)>,
    gm_downloading: usize,
    /// A full picture arrived: redraw once things settle.
    redraw_pending: Option<glib::SourceId>,
    /// Threads we just asked the daemon to replay (their conversationLoaded
    /// is ours, not news), with when.
    recent_requests: std::collections::HashMap<i64, f64>,
    /// The KDE Connect device (contacts, calls, notifications, and the
    /// Messages tab unless Google Messages is its source).
    kde_device: Option<Device>,
    /// Google Messages chats and messages waiting to be merged, a batch
    /// per idle callback (about 240 chats arrive at connect).
    gm_queue: VecDeque<crate::core::gm::Incoming>,
    gm_draining: bool,
    /// Clears "… is typing" when it runs out.
    typing_timer: Option<glib::SourceId>,
}

impl Inner {
    fn kde_device_id(&self) -> Option<String> {
        self.kde_device.as_ref().map(|d| d.id.clone())
    }
}

pub struct MessagesPage {
    /// The page; toasts float over it.
    pub widget: adw::ToastOverlay,
    stack: gtk::Stack,
    list: Rc<ConversationList>,
    thread: Rc<ThreadView>,
    /// "Your phone is hiding some texts" (rule 1).
    hidden_banner: adw::Banner,
    /// The Google Messages link's trouble (reconnecting, another browser,
    /// signed out).
    gm_banner: adw::Banner,
    /// The window's notifications (one subscription for the whole app).
    notifications: Rc<NotificationsModel>,
    /// The window's toast overlay, once set; until then, the page's own.
    toasts: RefCell<Option<adw::ToastOverlay>>,
    /// Google Contacts (import choice, rename write-back), once the window sets it.
    google: RefCell<Option<Rc<GoogleSync>>>,
    /// Folded message text for searching (kept outside `inner`: the list
    /// asks for hits while `inner` is borrowed).
    search: RefCell<crate::core::search::SearchIndex>,
    /// The lasting Google Messages link (read state, deletes), when paired.
    gm: RefCell<Option<Rc<super::gm_link::GmLink>>>,
    /// Chats Google Messages says are read on the phone (key → the phone's
    /// newest message, ms). Kept, not consumed: it is applied again whenever
    /// the chat list loads, since the link can connect before the phone does.
    phone_read: RefCell<std::collections::HashMap<String, i64>>,
    /// An apply of `phone_read` is queued on idle.
    phone_read_queued: std::cell::Cell<bool>,
    /// Chats opened while the window wasn't in front: marked read on the
    /// phone (which tells the sender) once it is, and only if still open.
    unseen_reads: RefCell<std::collections::HashSet<i64>>,
    /// The window's focus is watched (set once the page is in a window).
    watching_window: std::cell::Cell<bool>,
    inner: RefCell<Inner>,
}

impl MessagesPage {
    pub fn new(
        store: MessageStore,
        settings: Rc<RefCell<Settings>>,
        notifications: Rc<NotificationsModel>,
    ) -> Rc<Self> {
        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.set_hexpand(true);
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        let hidden_banner = adw::Banner::new("Your phone is hiding some texts from PhoneRelay");
        hidden_banner.set_button_label(Some("Copy fix"));
        hidden_banner.connect_button_clicked(|b| {
            b.clipboard().set_text(crate::core::hidden_texts::FIX_COMMAND);
            b.set_title("Fix copied: run it in a terminal with the phone plugged in (adb)");
        });
        let gm_banner = adw::Banner::new("");
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.append(&gm_banner);
        column.append(&hidden_banner);
        column.append(&stack);
        let widget = adw::ToastOverlay::new();
        widget.set_child(Some(&column));

        let status = adw::StatusPage::new();
        status.set_icon_name(Some("mail-unread-symbolic"));
        status.set_title("Messages");
        status.set_description(Some("No phone linked yet.\nPair your phone in KDE Connect to see your texts here."));
        stack.add_named(&status, Some("status"));

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let list = ConversationList::new();
        content.append(&list.widget);
        content.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        // Set once `this` exists (the action needs a weak reference to it).
        let download_target: Rc<RefCell<Weak<MessagesPage>>> = Rc::default();
        let thread = ThreadView::new(BubbleActions {
            download: {
                let target = download_target.clone();
                Rc::new(move |part, uid, name| {
                    if let Some(page) = target.borrow().upgrade() {
                        page.request_attachment(part, uid, name);
                    }
                })
            },
            text_back: {
                let target = download_target.clone();
                Rc::new(move || {
                    if let Some(page) = target.borrow().upgrade() {
                        page.thread.entry.grab_focus();
                    }
                })
            },
            react: {
                let target = download_target.clone();
                Rc::new(move |uid, emoji| {
                    if let Some(page) = target.borrow().upgrade() {
                        let effects = page.inner.borrow_mut().m.gm_react(uid, &emoji);
                        page.run(effects);
                    }
                })
            },
            reply: {
                let target = download_target.clone();
                Rc::new(move |uid| {
                    if let Some(page) = target.borrow().upgrade() {
                        page.start_reply(uid);
                    }
                })
            },
        });
        content.append(&thread.widget);
        stack.add_named(&content, Some("content"));
        stack.set_visible_child_name("status");

        let this = Rc::new(Self {
            widget,
            stack,
            hidden_banner,
            gm_banner,
            list,
            thread,
            notifications: notifications.clone(),
            toasts: RefCell::default(),
            google: RefCell::default(),
            search: RefCell::default(),
            gm: RefCell::default(),
            phone_read: RefCell::default(),
            phone_read_queued: Default::default(),
            unseen_reads: RefCell::default(),
            watching_window: Default::default(),
            inner: RefCell::new(Inner {
                m: Messages::new(),
                store,
                settings,
                client: None,
                subs: Vec::new(),
                queue: VecDeque::new(),
                draining: false,
                timer: None,
                hidden_check: None,
                loaded_reload: None,
                reachable_seen: false,
                contact_sync_warned: Default::default(),
                full_queue: VecDeque::new(),
                full_pumping: 0,
                gm_downloads: VecDeque::new(),
                gm_downloading: 0,
                redraw_pending: None,
                recent_requests: Default::default(),
                kde_device: None,
                gm_queue: VecDeque::new(),
                gm_draining: false,
                typing_timer: None,
            }),
        });
        *download_target.borrow_mut() = Rc::downgrade(&this);
        this.start_gm_link();
        let weak = Rc::downgrade(&this);
        this.gm_banner.connect_button_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.gm_banner_clicked();
            }
        });
        // Linked before: show Google Messages' chats from the store at once
        // instead of KDE Connect's until the link connects.
        let cached = this.inner.borrow().store.meta("gm_device").filter(|d| !d.is_empty());
        if let Some(dev) = cached.filter(|_| this.gm.borrow().is_some()) {
            this.switch_to_gm(&dev);
            this.show_gm_banner("Connecting to Google Messages…", None);
        }
        let settings = this.inner.borrow().settings.clone();
        this.thread.set_autocap(move || settings.try_borrow().map_or(AutocapRules::OFF, |s| s.autocap_rules()));
        let saved_scale = this.inner.borrow().settings.borrow().message_font_scale();
        let weak = Rc::downgrade(&this);
        this.thread.set_zoom(saved_scale, move |scale| {
            if let Some(this) = weak.upgrade()
                && let Err(e) = this.inner.borrow().settings.borrow_mut().set_message_font_scale(scale)
            {
                log::warn!("saving the text size failed: {e}");
            }
        });
        let weak = Rc::downgrade(&this);
        notifications.connect(move |e| {
            if let Some(this) = weak.upgrade() {
                this.notification_event(e);
            }
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_event(move |e| {
            if let Some(this) = weak.upgrade() {
                this.list_event(e);
            }
        });
        let weak = Rc::downgrade(&this);
        this.thread.connect_rendered(move |tid, uids| {
            if let Some(this) = weak.upgrade() {
                this.want_full_pictures(tid, uids);
            }
        });
        let weak = Rc::downgrade(&this);
        this.thread.connect_send(move |tid, text, files, reply_to| {
            let Some(this) = weak.upgrade() else {
                return false;
            };
            this.send(tid, text, files, reply_to)
        });
        let weak = Rc::downgrade(&this);
        this.thread.connect_older(move |tid| {
            let Some(this) = weak.upgrade() else { return };
            let effects = this.inner.borrow_mut().m.gm_older(tid);
            this.run(effects);
        });
        let weak = Rc::downgrade(&this);
        this.thread.connect_typing(move |tid| {
            let Some(this) = weak.upgrade() else { return };
            let effects = this.inner.borrow_mut().m.gm_typing_out(tid, now());
            this.run(effects);
        });
        // Read receipts wait until the window is in front (see `unseen_reads`).
        let weak = Rc::downgrade(&this);
        this.widget.connect_map(move |_| {
            if let Some(this) = weak.upgrade() {
                this.watch_window();
                this.flush_unseen_reads();
            }
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_selected(move |tid| {
            if let Some(this) = weak.upgrade() {
                // Opened from a message match: land on that message.
                if let Some((needle, uid)) = this.list.reveal_for(tid) {
                    this.thread.find_on_open(tid, &needle, uid);
                }
                let effects = {
                    let mut guard = this.inner.borrow_mut();
                    let i = &mut *guard;
                    i.m.select(tid, &i.store, now())
                };
                this.run(effects);
            }
        });
        this
    }

    /// The active device changed (or its reachability did).
    pub fn set_device(self: &Rc<Self>, client: &Client, device: Option<&Device>) {
        let reachable = device.is_some_and(|d| d.reachable);
        if self.inner.borrow().m.is_gm() {
            // Chats come from Google Messages; KDE Connect keeps contacts,
            // calls and notifications.
            let (changed, back) = {
                let mut i = self.inner.borrow_mut();
                let changed = i.kde_device_id().as_deref() != device.map(|d| d.id.as_str());
                let back = reachable && !std::mem::replace(&mut i.reachable_seen, reachable);
                i.client = Some(client.clone());
                i.kde_device = device.cloned();
                (changed, back)
            };
            if changed {
                let subs = device.map(|d| self.subscribe_contact_sources(client, &d.id)).unwrap_or_default();
                self.inner.borrow_mut().subs = subs;
            }
            if changed || back {
                self.reload_contacts();
                if reachable {
                    self.request_contact_sync();
                }
            }
            return;
        }
        let same = {
            let i = self.inner.borrow();
            i.m.device_id() == device.map(|d| d.id.as_str())
        };
        if same {
            let was = std::mem::replace(&mut self.inner.borrow_mut().reachable_seen, reachable);
            self.inner.borrow_mut().m.set_reachable(reachable);
            if reachable {
                self.inner.borrow_mut().m.catch_up.catch_up(
                    now(),
                    sync::CATCH_UP_DELAY_SECONDS,
                    sync::CATCH_UP_MIN_INTERVAL_SECONDS,
                );
                if !was {
                    // Back after a disconnect: refresh as on first connect.
                    self.reload_contacts();
                    self.request_contact_sync();
                    // (The window's notifications model reloads itself.)
                    self.load_active();
                }
            }
            self.reschedule();
            return;
        }

        let Some(dev) = device else {
            let mut i = self.inner.borrow_mut();
            i.subs.clear();
            i.queue.clear();
            i.kde_device = None;
            i.m.reset_for_device(None, false);
            self.stack.set_visible_child_name("status");
            return;
        };
        let effects = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            i.subs.clear();
            i.queue.clear();
            i.client = Some(client.clone());
            i.kde_device = Some(dev.clone());
            i.m.reset_for_device(Some(&dev.id), reachable);
            i.m.contact_map = load_contact_map(&dev.id, i.store.load_contacts(), i.store.load_contact_overrides());
            self.list.set_contacts(i.m.contact_map.clone());
            i.subs = self.subscribe(client, &dev.id);
            i.subs.extend(self.subscribe_contact_sources(client, &dev.id));
            i.reachable_seen = reachable;
            let mut effects = i.m.load_cached(&i.store);
            effects.push(Effect::RefreshList);
            i.m.catch_up.catch_up(now(), sync::CATCH_UP_STARTUP_DELAY_SECONDS, 0.0);
            effects
        };
        self.stack.set_visible_child_name("content");
        self.run(effects);
        self.reschedule();
        self.apply_phone_read();

        self.load_active();
        if reachable {
            self.request_contact_sync();
        }
        self.scan_downloads_for_vcf();
    }

    /// The daemon's cached newest-message-per-thread list.
    fn load_active(self: &Rc<Self>) {
        if self.inner.borrow().m.is_gm() {
            return;
        }
        let weak = Rc::downgrade(self);
        self.with_device(move |client, id| async move {
            let entries = client.active_conversations(&id).await;
            let raw: Vec<_> = entries.iter().filter_map(variant::parse_message).collect();
            let Some(this) = weak.upgrade() else { return };
            let effects = {
                let mut guard = this.inner.borrow_mut();
                let i = &mut *guard;
                if i.m.device_id() != Some(id.as_str()) {
                    return;
                }
                let _t = super::perf::span("merging activeConversations");
                i.m.apply_active(&raw, &i.store, &mut *i.settings.borrow_mut(), now())
            };
            log::info!("activeConversations: {} entries, {} parsed", entries.len(), raw.len());
            this.run(effects);
            this.reschedule();
            // The phone's own read flags just landed; Google Messages' are newer.
            this.apply_phone_read();
        });
    }

    fn subscribe(self: &Rc<Self>, client: &Client, device_id: &str) -> Vec<Subscription> {
        let path = device_path(device_id);
        let bus = client.bus();
        let mut subs = Vec::new();
        for name in ["conversationCreated", "conversationUpdated"] {
            let weak = Rc::downgrade(self);
            subs.push(signals::subscribe(bus, &path, IFACE_CONVERSATIONS, name, move |p| {
                if let Some(this) = weak.upgrade() {
                    this.enqueue(p.clone());
                }
            }));
        }
        let weak = Rc::downgrade(self);
        subs.push(signals::subscribe(bus, &path, IFACE_CONVERSATIONS, "conversationLoaded", move |p| {
            let (Some(this), Some(tid)) = (weak.upgrade(), variant::first_int(p)) else {
                return;
            };
            let effects = this.inner.borrow_mut().m.on_conversation_loaded(tid, now());
            // Our own request (a gap fill, or its replay) finishing: the
            // messages come as signals, so there is nothing to reload. A
            // reload here re-queued gap fills and looped every 5–20 s on the
            // laptop (run 3).
            let ours = !effects.is_empty() || {
                let mut i = this.inner.borrow_mut();
                let t = now();
                i.recent_requests.retain(|_, at| t - *at < 60.0);
                i.recent_requests.remove(&tid).is_some()
            };
            this.run(effects);
            this.reschedule();
            if ours {
                return;
            }
            // As Python: a debounced reload of the daemon's list and the
            // notification names, 500 ms after the last load.
            if let Some(old) = this.inner.borrow_mut().loaded_reload.take() {
                old.remove();
            }
            let weak = Rc::downgrade(&this);
            let id = glib::timeout_add_local_once(Duration::from_millis(500), move || {
                let Some(this) = weak.upgrade() else { return };
                this.inner.borrow_mut().loaded_reload = None;
                this.load_active();
                this.load_notifications();
            });
            this.inner.borrow_mut().loaded_reload = Some(id);
        }));
        let weak = Rc::downgrade(self);
        subs.push(signals::subscribe(bus, &path, IFACE_CONVERSATIONS, "attachmentReceived", move |p| {
            // attachmentReceived(filePath s, fileName s)
            let Some(this) = weak.upgrade() else { return };
            if let Some((file_path, file_name)) = p.get::<(String, String)>() {
                this.attachment_received(&file_path, &file_name);
            }
        }));
        let weak = Rc::downgrade(self);
        subs.push(signals::subscribe(bus, &path, IFACE_CONVERSATIONS, "conversationRemoved", move |p| {
            let (Some(this), Some(tid)) = (weak.upgrade(), variant::first_int(p)) else {
                return;
            };
            let effects = {
                let mut guard = this.inner.borrow_mut();
                let i = &mut *guard;
                i.m.remove_conversation(tid, &i.store)
            };
            this.run(effects);
        }));
        subs
    }

    // ── signal batching (rule 3) ───────────────────────────────────

    fn enqueue(self: &Rc<Self>, params: Variant) {
        let start = {
            let mut i = self.inner.borrow_mut();
            i.queue.push_back(params);
            !std::mem::replace(&mut i.draining, true)
        };
        if start {
            let weak = Rc::downgrade(self);
            glib::idle_add_local(move || match weak.upgrade() {
                Some(this) => this.drain_batch(),
                None => glib::ControlFlow::Break,
            });
        }
    }

    fn drain_batch(self: &Rc<Self>) -> glib::ControlFlow {
        let (effects, more) = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            let n = i.queue.len().min(SIGNAL_BATCH_SIZE);
            let batch: Vec<Variant> = i.queue.drain(..n).collect();
            let mut settings = i.settings.borrow_mut();
            let (m, settings) = (&mut i.m, &mut *settings);
            let t = now();
            let _t = super::perf::span("merging a signal batch");
            let effects = i.store.batch(|store| {
                let mut effects = Vec::new();
                for p in &batch {
                    match variant::parse_message_signal(p) {
                        Some(raw) => effects.extend(m.merge_message(parse_message(&raw), store, settings, t)),
                        None => log::warn!("unparsable conversation signal: {}", p.type_()),
                    }
                }
                effects
            });
            let more = !i.queue.is_empty();
            if !more {
                i.draining = false;
            }
            // As Python: the list is refreshed after every batch (new
            // previews, times, order, unread).
            let mut effects = effects;
            if !batch.is_empty() {
                effects.push(Effect::RefreshList);
            }
            (effects, more)
        };
        self.run(effects);
        self.reschedule();
        if more { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
    }

    // ── catch-up timer ─────────────────────────────────────────────

    fn reschedule(self: &Rc<Self>) {
        let mut i = self.inner.borrow_mut();
        if let Some(id) = i.timer.take() {
            id.remove();
        }
        let Some(deadline) = i.m.catch_up.next_deadline() else {
            return;
        };
        let delay = Duration::from_secs_f64((deadline - now()).max(0.0));
        let weak: Weak<Self> = Rc::downgrade(self);
        i.timer = Some(glib::timeout_add_local_once(delay, move || {
            let Some(this) = weak.upgrade() else { return };
            let effects = {
                let mut i = this.inner.borrow_mut();
                i.timer = None; // this source is finished; don't remove it again
                i.m.tick(now())
            };
            this.run(effects);
            this.reschedule();
        }));
    }

    // ── effects ────────────────────────────────────────────────────

    fn run(self: &Rc<Self>, effects: Vec<Effect>) {
        let mut refresh = false;
        for e in effects {
            match e {
                Effect::RefreshList => refresh = true,
                Effect::Sync(_) if self.inner.borrow().m.is_gm() => {}
                Effect::Sync(action) => self.sync(action),
                Effect::MarkRead { thread_id } => self.mark_read(thread_id),
                Effect::GmSend { thread_id: _, conv, text, file, tmp_id, reply_to } => {
                    // Show it at once as "Sending"; the phone's copy takes
                    // over the same bubble.
                    let shown = {
                        let mut i = self.inner.borrow_mut();
                        let i = &mut *i;
                        let mut settings = i.settings.borrow_mut();
                        let uid = i.store.gm_local_id("msg", &format!("pending:{tmp_id}"));
                        if let Some(r) = &reply_to {
                            i.m.gm_pending_reply(uid, &conv, r);
                        }
                        i.m.gm_add_pending((&tmp_id, &text, file.as_deref()), uid, &i.store, &mut *settings, now())
                    };
                    self.run(shown);
                    self.gm_send(conv, text, file, tmp_id, reply_to);
                }
                Effect::GmReact { thread_id, conv, msg, emoji, action } => {
                    self.gm_react(thread_id, conv, msg, emoji, action)
                }
                Effect::GmFetch { thread_id } => self.gm_fetch(thread_id),
                Effect::GmSendNew { addresses, text, attachments } => self.gm_send_new(addresses, text, attachments),
                Effect::GmTyping { conv } => {
                    if let Some(gm) = self.gm.borrow().clone() {
                        gm.request(|id| crate::core::gm::typing_request(id, &conv), |_| {});
                    }
                }
                Effect::GmFetchOlder { thread_id, conv, cursor } => self.gm_fetch_older(thread_id, conv, cursor),
                Effect::GmUpdateChat { thread_id, conv, change } => self.gm_update_chat(thread_id, conv, change),
                Effect::Typing(tid) => self.show_typing(tid),
                Effect::MoreHistory(tid) => self.show_more_history(tid),
                Effect::Redraw(tid) => {
                    {
                        let i = self.inner.borrow();
                        if let Some(conv) = i.m.index.get(tid) {
                            self.thread.redraw(conv, &|uid| i.m.route_of(uid));
                        }
                    }
                    self.show_extras(tid);
                }
                Effect::ShowThread(tid) => {
                    self.show_thread(tid);
                    self.after_open(tid);
                }
                Effect::ShowEmpty => self.thread.show_empty(),
                Effect::Toast(text) => self.toast(text),
                Effect::Reply { conversation_id, text, attachments } => {
                    let weak = Rc::downgrade(self);
                    self.with_device(move |c, d| async move {
                        if !c.reply_to_conversation(&d, conversation_id, &text, &attachments).await
                            && let Some(this) = weak.upgrade()
                        {
                            this.toast("Couldn't send that text. Check the phone is connected and try again.");
                        }
                    });
                }
                Effect::ReplyViaNotification { thread_id, reply_id, text } => {
                    let weak = Rc::downgrade(self);
                    self.with_device(move |c, d| async move {
                        let Err(e) = c.try_send_notification_reply_by_id(&d, &reply_id, &text).await else {
                            return;
                        };
                        let refused = crate::dbus::client::is_definite_refusal(&e);
                        log::warn!(
                            "notification reply failed ({}): {}",
                            if refused { "refused" } else { "unclear" },
                            e.message()
                        );
                        let Some(this) = weak.upgrade() else { return };
                        let effects = this.inner.borrow_mut().m.notification_reply_failed(thread_id, &text, refused);
                        this.run(effects);
                    });
                }
                Effect::MaybeNotSent { thread_id, text } => self.watch_for_echo(thread_id, text),
                Effect::SendNew { addresses, text, attachments } => self.with_device(move |c, d| async move {
                    c.send_sms(&d, &addresses, &text, &attachments).await;
                }),
                Effect::DeleteOnPhone { thread_id, addresses } => self.delete_on_phone(vec![(thread_id, addresses)]),
            }
        }
        if refresh {
            let more = {
                let i = self.inner.borrow();
                let _t = super::perf::span("refreshing the conversation list");
                self.list
                    .set_flags(i.m.chat_flags(), i.m.is_gm().then(|| (i.m.archived_count(), i.m.showing_archived())));
                self.list.set_conversations(&i.m.visible(&*i.settings.borrow()))
            };
            // Matches follow new and removed messages.
            let needle = self.list.search_text();
            if !needle.trim().is_empty() {
                self.search(&needle);
            }
            if more {
                // The rest of the rows, a batch per idle callback.
                let weak = Rc::downgrade(self);
                glib::idle_add_local_once(move || {
                    if let Some(this) = weak.upgrade() {
                        this.run(vec![Effect::RefreshList]);
                    }
                });
            }
        }
    }

    fn sync(self: &Rc<Self>, action: Action) {
        match action {
            Action::RequestAllThreads => self.with_device(|c, d| async move {
                c.request_all_conversations(&d).await;
            }),
            Action::RequestFromPhone { thread_id, count, older_than } => self.with_device(move |c, d| async move {
                c.request_thread_from_phone(&d, thread_id, count, older_than.unwrap_or(-1)).await;
            }),
            Action::ReplayFromDaemon { thread_id, start, end } => {
                self.inner.borrow_mut().recent_requests.insert(thread_id, now());
                self.with_device(move |c, d| async move {
                    let clamp = |n: i64| i32::try_from(n).unwrap_or(i32::MAX);
                    c.request_conversation(&d, thread_id, clamp(start), clamp(end)).await;
                })
            }
        }
    }

    /// Run a D-Bus call for the current device, if there is one.
    fn with_device<F, Fut>(&self, f: F)
    where
        F: FnOnce(Client, String) -> Fut,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        let i = self.inner.borrow();
        if let (Some(c), Some(d)) = (i.client.clone(), i.kde_device_id()) {
            glib::spawn_future_local(f(c, d));
        }
    }

    fn list_event(self: &Rc<Self>, e: ListEvent) {
        match e {
            ListEvent::Clicked(_) => self.thread.focus_compose(),
            ListEvent::Contacts(names) => {
                let ordinary = crate::core::autocap::dictionary_words();
                if ordinary.is_none() {
                    log::info!("capitals: no spelling dictionary, so contacts' names are not capitalized");
                }
                self.thread.set_names(crate::core::autocap::Names::new(names.iter().map(String::as_str), ordinary));
            }
            ListEvent::Start { address, name } => {
                let effects = self.inner.borrow_mut().m.open_or_create(&address, &name);
                if effects.is_empty() {
                    self.toast(crate::core::messages::TOAST_NOT_CONNECTED);
                }
                self.run(effects);
            }
            ListEvent::StartGroup { addresses } => {
                let effects = self.inner.borrow_mut().m.open_group(&addresses, "");
                if effects.is_empty() {
                    self.toast(crate::core::messages::TOAST_NOT_CONNECTED);
                }
                self.run(effects);
            }
            ListEvent::ChatAction { thread_id, action } => self.chat_action(thread_id, action),
            ListEvent::ShowArchived(on) => {
                let effects = self.inner.borrow_mut().m.set_show_archived(on);
                self.run(effects);
            }
            ListEvent::Rename { thread_id, address } => self.rename_dialog(thread_id, address),
            ListEvent::Delete { thread_id } => self.delete_dialog(vec![thread_id]),
            ListEvent::DeleteMany { thread_ids } => self.delete_dialog(thread_ids),
            ListEvent::ImportContacts => self.import_dialog(),
            ListEvent::Search(needle) => self.search(&needle),
        }
    }

    /// Message-text matches for the list's search.
    fn search(&self, needle: &str) {
        if needle.trim().is_empty() {
            return;
        }
        let hits = {
            let i = self.inner.borrow();
            let _t = super::perf::span("searching messages");
            let visible = i.m.visible(&*i.settings.borrow());
            self.search.borrow_mut().search(visible, needle)
        };
        self.list.set_hits(needle, hits);
    }

    fn parent_window(&self) -> Option<gtk::Window> {
        self.widget.root().and_downcast::<gtk::Window>()
    }

    fn rename_dialog(self: &Rc<Self>, thread_id: i64, address: String) {
        let current = {
            let i = self.inner.borrow();
            i.m.index.get(thread_id).map(|c| c.display_name.clone()).unwrap_or_default()
        };
        let dialog = adw::MessageDialog::new(
            self.parent_window().as_ref(),
            Some("Set Contact Name"),
            Some(&format!(
                "Enter a name for {address}:{}",
                if self.google.borrow().as_ref().is_some_and(|g| g.is_connected()) {
                    "\n\nThis will also update your connected Google Contacts account."
                } else {
                    ""
                }
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        let entry = gtk::Entry::new();
        entry.set_text(if current == address { "" } else { &current });
        entry.set_placeholder_text(Some("Contact name"));
        entry.set_activates_default(true);
        dialog.set_extra_child(Some(&entry));
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if response != "save" {
                return;
            }
            let Some(this) = weak.upgrade() else { return };
            let name = entry.text().trim().to_string();
            let effects = {
                let mut guard = this.inner.borrow_mut();
                let i = &mut *guard;
                i.m.rename_contact(&address, &name, &i.store)
            };
            if !name.is_empty()
                && let Some(g) = this.google.borrow().as_ref()
            {
                g.push_rename(&address, &name);
            }
            this.list.set_contacts(this.inner.borrow().m.contact_map.clone());
            this.run(effects);
        });
        dialog.present();
    }

    /// Delete chats on the phone: KDE Connect when the daemon can, else
    /// Google Messages.
    fn delete_on_phone(self: &Rc<Self>, chats: Vec<(i64, Vec<String>)>) {
        if self.inner.borrow().m.is_gm() {
            self.delete_on_google(chats);
            return;
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let (client, device) = {
                let i = this.inner.borrow();
                (i.client.clone(), i.m.device_id().map(str::to_string))
            };
            if let (Some(c), Some(d)) = (&client, &device)
                && c.supports_conversation_deletion(d).await
            {
                for (thread_id, _) in &chats {
                    c.delete_conversation(d, *thread_id).await;
                }
                return;
            }
            this.delete_on_google(chats);
        });
    }

    // ── Google Messages link ───────────────────────────────────────

    fn gm_event(self: &Rc<Self>, e: crate::core::gm::Incoming) {
        use crate::core::gm::Incoming;
        let gm_source = self.inner.borrow().m.is_gm();
        match e {
            Incoming::Ready { me } => {
                self.inner.borrow_mut().m.gm_me = me.clone();
                self.gm_banner.set_revealed(false);
                // Linked: Google Messages becomes the tab's source.
                let Some(dev) = crate::core::gm::device_id(&me) else {
                    return;
                };
                if self.inner.borrow().m.device_id() != Some(dev.as_str()) {
                    self.switch_to_gm(&dev);
                }
                self.inner.borrow_mut().m.set_reachable(true);
            }
            e @ (Incoming::Conversation(_) | Incoming::Message(_)) if gm_source => self.gm_enqueue(e),
            Incoming::Conversation(c) => {
                // KDE Connect is the source: only read state comes from here.
                if c.unread {
                    self.phone_read.borrow_mut().remove(&c.chat);
                    return;
                }
                self.phone_read.borrow_mut().insert(c.chat, c.last_ts_ms);
                self.apply_phone_read();
            }
            Incoming::Typing { conv, from, typing } if gm_source => {
                let effects = {
                    let mut guard = self.inner.borrow_mut();
                    let i = &mut *guard;
                    let tid = i.store.gm_local_id("conv", &conv);
                    i.m.gm_typing(tid, &from, typing, now())
                };
                self.run(effects);
            }
            Incoming::Message(_) | Incoming::Reply(_) | Incoming::Typing { .. } => {}
            Incoming::Inactive => {
                if gm_source {
                    self.show_gm_banner("Google Messages is open in another browser", Some("Use here"));
                }
            }
            Incoming::Phone { responding } => {
                if !gm_source {
                } else if responding {
                    self.gm_banner.set_revealed(false);
                } else {
                    self.show_gm_banner("Your phone isn't answering Google Messages. Is it online?", None);
                }
            }
            Incoming::Fatal { kind, message: _ } => {
                if kind == "logged_out" {
                    self.switch_to_kde();
                    self.show_gm_banner("Google Messages signed out. Showing texts from KDE Connect.", None);
                } else if gm_source {
                    self.show_gm_banner("Reconnecting to Google Messages…", None);
                }
            }
        }
    }

    /// Start the Google Messages link if it is linked (and not running).
    fn start_gm_link(self: &Rc<Self>) {
        let existing = self.gm.borrow().clone();
        if let Some(gm) = existing {
            gm.resume();
            return;
        }
        let weak = Rc::downgrade(self);
        *self.gm.borrow_mut() = super::gm_link::GmLink::start(move |e| {
            if let Some(this) = weak.upgrade() {
                this.gm_event(e);
            }
        });
    }

    /// Settings' Link / Unlink.
    pub fn gm_controls(self: &Rc<Self>) -> super::settings_page::GmControls {
        let (w1, w2) = (Rc::downgrade(self), Rc::downgrade(self));
        super::settings_page::GmControls {
            linked: Rc::new(crate::core::phone_delete::available),
            linked_now: Rc::new(move || {
                if let Some(this) = w1.upgrade() {
                    this.show_gm_banner("Connecting to Google Messages…", None);
                    this.start_gm_link();
                }
            }),
            unlink: Rc::new(move |done| {
                let Some(this) = w2.upgrade() else { return };
                this.unlink_google(done);
            }),
        }
    }

    /// Stop the link, unlink on the phone, and go back to KDE Connect.
    fn unlink_google(self: &Rc<Self>, done: super::settings_page::UnlinkDone) {
        let weak = Rc::downgrade(self);
        let after_stop = move || {
            super::gm_pair::unpair(move |ok, told| {
                if let Some(this) = weak.upgrade() {
                    this.switch_to_kde();
                    this.gm_banner.set_revealed(false);
                }
                done(ok, told);
            });
        };
        let gm = self.gm.borrow().clone();
        match gm {
            Some(gm) => gm.stop(after_stop),
            None => after_stop(),
        }
    }

    fn show_gm_banner(&self, title: &str, button: Option<&str>) {
        self.gm_banner.set_title(title);
        self.gm_banner.set_button_label(button);
        self.gm_banner.set_revealed(true);
    }

    fn gm_banner_clicked(self: &Rc<Self>) {
        let Some(gm) = self.gm.borrow().clone() else {
            return;
        };
        let weak = Rc::downgrade(self);
        gm.request(crate::core::gm::set_active_request, move |r| {
            let Some(this) = weak.upgrade() else { return };
            if r.ok {
                this.gm_banner.set_revealed(false);
            } else {
                log::warn!("Google Messages: taking the session back failed: {} {}", r.error, r.message);
                this.toast("Couldn't take Google Messages back from the other browser");
            }
        });
    }

    /// Make Google Messages the tab's source (`dev` is its `gm:` device).
    fn switch_to_gm(self: &Rc<Self>, dev: &str) {
        log::info!("Messages: Google Messages is the source");
        let effects = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            // KDE Connect's conversation signals go; its contacts and calls stay.
            i.subs.clear();
            i.queue.clear();
            i.gm_queue.clear();
            if let (Some(c), Some(d)) = (i.client.clone(), i.kde_device_id()) {
                i.subs = self.subscribe_contact_sources(&c, &d);
            }
            if let Some(id) = i.timer.take() {
                id.remove();
            }
            let me = std::mem::take(&mut i.m.gm_me);
            i.m.reset_for_device(Some(dev), true);
            i.m.gm_me = me;
            let names_from = i.kde_device_id().unwrap_or_default();
            i.m.contact_map = load_contact_map(&names_from, i.store.load_contacts(), i.store.load_contact_overrides());
            self.list.set_contacts(i.m.contact_map.clone());
            i.store.set_meta("gm_device", dev);
            let mut effects = i.m.load_cached(&i.store);
            effects.push(Effect::RefreshList);
            effects
        };
        self.hidden_banner.set_revealed(false);
        self.thread.show_empty();
        self.thread.forget_drafts();
        self.stack.set_visible_child_name("content");
        self.run(effects);
    }

    /// Google Messages is gone (signed out): back to KDE Connect.
    fn switch_to_kde(self: &Rc<Self>) {
        let (client, device) = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            if !i.m.is_gm() {
                return;
            }
            log::info!("Messages: back to KDE Connect");
            i.store.set_meta("gm_device", "");
            i.gm_queue.clear();
            i.m.reset_for_device(None, false);
            (i.client.clone(), i.kde_device.clone())
        };
        self.thread.show_empty();
        self.thread.forget_drafts();
        if let Some(client) = client {
            self.set_device(&client, device.as_ref());
        } else {
            self.stack.set_visible_child_name("status");
        }
    }

    fn gm_enqueue(self: &Rc<Self>, e: crate::core::gm::Incoming) {
        let start = {
            let mut i = self.inner.borrow_mut();
            i.gm_queue.push_back(e);
            !std::mem::replace(&mut i.gm_draining, true)
        };
        if start {
            let weak = Rc::downgrade(self);
            glib::idle_add_local(move || match weak.upgrade() {
                Some(this) => this.drain_gm(),
                None => glib::ControlFlow::Break,
            });
        }
    }

    /// Merge a batch of Google Messages chats and messages in one store
    /// transaction.
    fn drain_gm(self: &Rc<Self>) -> glib::ControlFlow {
        use crate::core::gm::Incoming;
        let (effects, more) = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            let n = i.gm_queue.len().min(GM_BATCH_SIZE);
            let batch: Vec<Incoming> = i.gm_queue.drain(..n).collect();
            let mut settings = i.settings.borrow_mut();
            let (m, settings) = (&mut i.m, &mut *settings);
            let t = now();
            let _t = super::perf::span("merging Google Messages");
            let effects = i.store.batch(|store| {
                let mut effects = Vec::new();
                for e in &batch {
                    match e {
                        Incoming::Conversation(c) => {
                            let tid = store.gm_local_id("conv", &c.conv);
                            effects.extend(m.gm_conversation(c, tid, store, settings));
                        }
                        Incoming::Message(msg) => {
                            let tid = store.gm_local_id("conv", &msg.conv);
                            let known = store.gm_known("msg", &msg.msg);
                            // Seen before (an update, or the full-size picture
                            // for a preview): never unread again.
                            let again;
                            let msg = if known.is_some() && !msg.old {
                                again = crate::core::gm::GmMessage { old: true, ..msg.clone() };
                                &again
                            } else {
                                msg
                            };
                            let uid = match (known, m.gm_pending_uid(msg, tid)) {
                                (Some(uid), _) => uid,
                                (None, Some(uid)) => {
                                    store.gm_alias("msg", &msg.msg, uid);
                                    uid
                                }
                                (None, None) => store.gm_local_id("msg", &msg.msg),
                            };
                            effects.extend(m.gm_message(msg, tid, uid, store, settings, t));
                        }
                        _ => {}
                    }
                }
                effects
            });
            let more = !i.gm_queue.is_empty();
            if !more {
                i.gm_draining = false;
            }
            let mut effects = effects;
            if !batch.is_empty() {
                effects.push(Effect::RefreshList);
            }
            // Many updates to one open chat redraw it once.
            let mut shown = std::collections::HashSet::new();
            effects.retain(|e| match e {
                Effect::ShowThread(t) | Effect::Redraw(t) => shown.insert((matches!(e, Effect::Redraw(_)), *t)),
                _ => true,
            });
            (effects, more)
        };
        self.run(effects);
        if more { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
    }

    /// Send a text through Google Messages; its echo arrives as a message.
    fn gm_send(
        self: &Rc<Self>,
        conv: String,
        text: String,
        file: Option<String>,
        tmp_id: String,
        reply_to: Option<String>,
    ) {
        let gm = self.gm.borrow().clone();
        let weak = Rc::downgrade(self);
        let tmp = tmp_id.clone();
        let sent = gm.is_some_and(|gm| {
            gm.request(
                |id| match &file {
                    Some(f) => crate::core::gm::send_media_request(
                        id,
                        &conv,
                        f,
                        crate::core::gm::guess_mime(f),
                        &text,
                        &tmp_id,
                        reply_to.as_deref(),
                    ),
                    None => crate::core::gm::send_text_request(id, &conv, &text, &tmp_id, reply_to.as_deref()),
                },
                move |r| {
                    let Some(this) = weak.upgrade() else { return };
                    if r.ok {
                        return;
                    }
                    log::warn!("Google Messages send failed: {} {}", r.error, r.message);
                    let effects = {
                        let mut i = this.inner.borrow_mut();
                        let i = &mut *i;
                        i.m.gm_send_failed(&tmp, &i.store)
                    };
                    this.run(effects);
                },
            )
        });
        if !sent {
            let effects = {
                let mut i = self.inner.borrow_mut();
                let i = &mut *i;
                i.m.gm_send_failed(&tmp_id, &i.store)
            };
            self.run(effects);
        }
    }

    /// React (or take it back) on the phone; a refusal toasts and refetches.
    fn gm_react(self: &Rc<Self>, thread_id: i64, conv: String, msg: String, emoji: String, action: &'static str) {
        let gm = self.gm.borrow().clone();
        let weak = Rc::downgrade(self);
        let sent = gm.is_some_and(|gm| {
            gm.request(
                |id| crate::core::gm::react_request(id, &conv, &msg, &emoji, action),
                move |r| {
                    let Some(this) = weak.upgrade() else { return };
                    if r.ok {
                        return;
                    }
                    log::warn!("Google Messages reaction failed: {} {}", r.error, r.message);
                    let effects = this.inner.borrow_mut().m.gm_react_failed(thread_id);
                    this.run(effects);
                },
            )
        });
        if !sent {
            let effects = self.inner.borrow_mut().m.gm_react_failed(thread_id);
            self.run(effects);
        }
    }

    /// Reply on bubble `uid`: the compose bar says what it answers.
    fn start_reply(&self, uid: i64) {
        let i = self.inner.borrow();
        let Some(tid) = i.m.active_thread() else {
            return;
        };
        let Some(e) = i.m.gm_extras(tid, &i.store).remove(&uid).filter(|e| !e.msg_id.is_empty()) else {
            return;
        };
        let Some(m) = i.m.index.get(tid).and_then(|c| c.messages.iter().find(|m| m.uid == uid)) else {
            return;
        };
        let author = if m.msg_type == crate::core::models::MSG_TYPE_RECEIVED {
            crate::core::contacts::resolve_name(&i.m.contact_map, &m.address)
        } else {
            "You".to_string()
        };
        let text = if m.body.is_empty() { "Picture".to_string() } else { m.body.clone() };
        self.thread.start_reply(&e.msg_id, &author, &text);
    }

    /// Reactions and quotes for the open chat, when it is `tid`.
    fn show_extras(&self, tid: i64) {
        let extras = {
            let i = self.inner.borrow();
            if !i.m.is_gm() || i.m.active_thread() != Some(tid) {
                return;
            }
            i.m.gm_extras(tid, &i.store)
        };
        self.thread.set_extras(extras);
    }

    /// A text to a new chat: have the phone find or make it, then send.
    fn gm_send_new(self: &Rc<Self>, addresses: Vec<String>, text: String, attachments: Vec<String>) {
        let gm = self.gm.borrow().clone();
        let weak = Rc::downgrade(self);
        let sent = gm.is_some_and(|gm| {
            gm.request(
                |id| crate::core::gm::open_conversation_request(id, &addresses),
                move |r| {
                    let Some(this) = weak.upgrade() else { return };
                    if !r.ok || r.conv.is_empty() {
                        log::warn!("Google Messages: couldn't open the chat: {} {}", r.error, r.message);
                        this.toast(crate::core::messages::TOAST_GM_NOT_SENT);
                        return;
                    }
                    // The chat's conversation event came first; let it merge.
                    let weak = Rc::downgrade(&this);
                    glib::idle_add_local_once(move || {
                        let Some(this) = weak.upgrade() else { return };
                        let effects = {
                            let mut guard = this.inner.borrow_mut();
                            let i = &mut *guard;
                            let tid = i.store.gm_local_id("conv", &r.conv);
                            i.m.gm_opened(tid, &r.conv, &text, attachments)
                        };
                        this.run(effects);
                    });
                },
            )
        });
        if !sent {
            self.toast(crate::core::messages::TOAST_GM_NOT_SENT);
        }
    }

    /// Fetch a chat's newest messages (each of its Google chats).
    fn gm_fetch(self: &Rc<Self>, thread_id: i64) {
        let Some(gm) = self.gm.borrow().clone() else {
            return;
        };
        let convs = self.inner.borrow().m.gm_convs(thread_id);
        for conv in convs {
            let weak = Rc::downgrade(self);
            let conv_for_page = conv.clone();
            gm.request(
                |id| crate::core::gm::fetch_request(id, &conv, GM_FETCH_COUNT, None),
                move |r| {
                    let Some(this) = weak.upgrade() else { return };
                    if !r.ok {
                        log::info!("Google Messages fetch: {} {}", r.error, r.message);
                        return;
                    }
                    let effects =
                        this.inner.borrow_mut().m.gm_page(&conv_for_page, r.cursor.clone(), false, r.messages.len());
                    for mut m in r.messages {
                        m.old = true;
                        this.gm_enqueue(crate::core::gm::Incoming::Message(m));
                    }
                    this.run(effects);
                },
            );
        }
    }

    /// An older page of a chat, as the user scrolls back.
    fn gm_fetch_older(self: &Rc<Self>, thread_id: i64, conv: String, cursor: crate::core::gm::Cursor) {
        let Some(gm) = self.gm.borrow().clone() else {
            return;
        };
        let weak = Rc::downgrade(self);
        let before = cursor.clone();
        let sent = gm.request(|id| crate::core::gm::fetch_request(id, &conv, GM_FETCH_COUNT, Some(&before)), {
            let conv = conv.clone();
            move |r| {
                let Some(this) = weak.upgrade() else { return };
                if !r.ok {
                    log::info!("Google Messages older page: {} {}", r.error, r.message);
                }
                // A failed page ends paging for now (the cursor stays).
                let cursor = if r.ok { r.cursor.clone() } else { Some(cursor) };
                let got = if r.ok { r.messages.len() } else { 1 };
                for mut m in r.messages {
                    m.old = true;
                    this.gm_enqueue(crate::core::gm::Incoming::Message(m));
                }
                let effects = this.inner.borrow_mut().m.gm_page(&conv, cursor, true, got);
                this.run(effects);
            }
        });
        if !sent {
            let effects = self.inner.borrow_mut().m.gm_page(&conv, None, true, 0);
            self.run(effects);
        }
        self.show_more_history(thread_id);
    }

    /// Archive, mute or block a chat on the phone.
    fn gm_update_chat(self: &Rc<Self>, thread_id: i64, conv: String, change: crate::core::gm::ChatChange) {
        let gm = self.gm.borrow().clone();
        let weak = Rc::downgrade(self);
        let done = move |ok: bool, why: String| {
            let Some(this) = weak.upgrade() else { return };
            if !ok {
                log::warn!("Google Messages: {} refused: {why}", change.as_str());
            }
            let effects = {
                let mut guard = this.inner.borrow_mut();
                let i = &mut *guard;
                i.m.gm_chat_changed(thread_id, change, ok, &i.store)
            };
            this.run(effects);
        };
        let done = Rc::new(RefCell::new(Some(done)));
        let d = done.clone();
        let sent = gm.is_some_and(|gm| {
            gm.request(
                |id| crate::core::gm::update_chat_request(id, &conv, change),
                move |r| {
                    if let Some(f) = d.borrow_mut().take() {
                        f(r.ok, format!("{} {}", r.error, r.message));
                    }
                },
            )
        });
        if !sent && let Some(f) = done.borrow_mut().take() {
            f(false, "the link isn't running".into());
        }
    }

    /// A chat's menu: pin, mute, archive, block.
    fn chat_action(self: &Rc<Self>, thread_id: i64, action: crate::core::messages::ChatAction) {
        use crate::core::messages::ChatAction;
        if matches!(action, ChatAction::Block | ChatAction::Spam) {
            self.block_dialog(thread_id, action == ChatAction::Spam);
            return;
        }
        let effects = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            i.m.chat_action(thread_id, action, &i.store)
        };
        self.run(effects);
    }

    /// Blocking is hard to undo from here (blocked chats leave the list):
    /// ask first.
    fn block_dialog(self: &Rc<Self>, thread_id: i64, spam: bool) {
        use crate::core::messages::ChatAction;
        let name = self
            .inner
            .borrow()
            .m
            .index
            .get(thread_id)
            .map(|c| if c.display_name.is_empty() { c.address.clone() } else { c.display_name.clone() });
        let Some(name) = name else { return };
        let (heading, body, button) = if spam {
            (
                format!("Report {name} as spam?"),
                "Google Messages blocks them and reports the chat as spam. The chat leaves this list; unblock it on your phone (Google Messages → Spam & blocked).",
                "Report spam",
            )
        } else {
            (
                format!("Block {name}?"),
                "You won't get texts from them on your phone. The chat leaves this list; unblock it on your phone (Google Messages → Spam & blocked).",
                "Block",
            )
        };
        let dialog = adw::MessageDialog::new(self.parent_window().as_ref(), Some(&heading), Some(body));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("block", button);
        dialog.set_response_appearance("block", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if response != "block" {
                return;
            }
            let Some(this) = weak.upgrade() else { return };
            let action = if spam { ChatAction::Spam } else { ChatAction::Block };
            let effects = {
                let mut guard = this.inner.borrow_mut();
                let i = &mut *guard;
                i.m.chat_action(thread_id, action, &i.store)
            };
            this.run(effects);
        });
        dialog.present();
    }

    /// The typing bubble at the end of the open chat, gone when it runs out.
    fn show_typing(self: &Rc<Self>, tid: i64) {
        let label = {
            let i = self.inner.borrow();
            if i.m.active_thread().map(|a| i.m.index.primary_for(a)) != Some(tid) {
                return;
            }
            i.m.typing_label(tid, now())
        };
        if let Some(old) = self.inner.borrow_mut().typing_timer.take() {
            old.remove();
        }
        self.thread.set_typing(label.as_ref());
        if let Some(until) = label.map(|l| l.until) {
            let wait = Duration::from_secs_f64((until - now()).max(0.0) + 0.05);
            let weak = Rc::downgrade(self);
            let id = glib::timeout_add_local_once(wait, move || {
                let Some(this) = weak.upgrade() else { return };
                this.inner.borrow_mut().typing_timer = None;
                this.show_typing(tid);
            });
            self.inner.borrow_mut().typing_timer = Some(id);
        }
    }

    /// Tell the open chat whether the phone has older messages.
    fn show_more_history(&self, tid: i64) {
        let (more, loading) = {
            let i = self.inner.borrow();
            if i.m.active_thread() != Some(tid) {
                return;
            }
            (i.m.gm_more_history(tid), i.m.gm_loading_older(tid))
        };
        self.thread.set_more_history(tid, more, loading);
    }

    /// Mark a chat read here and on the phone. On the phone that tells the
    /// sender ("Read"), so it waits until the window is in front.
    fn mark_read(self: &Rc<Self>, thread_id: i64) {
        if !self.in_front() {
            self.unseen_reads.borrow_mut().insert(thread_id);
            return;
        }
        self.mark_read_on_google(thread_id);
        if !self.inner.borrow().m.is_gm() {
            self.with_device(move |c, d| async move {
                c.mark_conversation_as_read(&d, thread_id).await;
            })
        }
    }

    /// Is the Messages tab on screen in the focused window?
    fn in_front(&self) -> bool {
        self.widget.is_mapped() && self.parent_window().is_some_and(|w| w.is_active())
    }

    fn watch_window(self: &Rc<Self>) {
        if self.watching_window.get() {
            return;
        }
        let Some(window) = self.parent_window() else {
            return;
        };
        self.watching_window.set(true);
        let weak = Rc::downgrade(self);
        window.connect_is_active_notify(move |w| {
            if w.is_active()
                && let Some(this) = weak.upgrade()
            {
                this.flush_unseen_reads();
            }
        });
    }

    /// The window came to the front: send the reads still waiting, for the
    /// chat still open (others were left unseen).
    fn flush_unseen_reads(self: &Rc<Self>) {
        if !self.in_front() {
            return;
        }
        let waiting: Vec<i64> = self.unseen_reads.borrow_mut().drain().collect();
        let active = {
            let i = self.inner.borrow();
            i.m.active_thread().map(|a| i.m.index.primary_for(a))
        };
        for tid in waiting {
            let primary = self.inner.borrow().m.index.primary_for(tid);
            if Some(primary) == active {
                self.mark_read(tid);
            }
        }
    }

    /// Clear unread dots on chats read on the phone (once per idle, so the
    /// ~250 events at connect are one pass).
    fn apply_phone_read(self: &Rc<Self>) {
        if self.phone_read_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(this) = weak.upgrade() else { return };
            this.phone_read_queued.set(false);
            let effects = {
                let read = this.phone_read.borrow();
                if read.is_empty() {
                    return;
                }
                let mut guard = this.inner.borrow_mut();
                let i = &mut *guard;
                i.m.read_on_phone(&read, &i.store)
            };
            this.run(effects);
        });
    }

    /// Opened on the laptop: mark it read on the phone too.
    fn mark_read_on_google(&self, thread_id: i64) {
        let Some(gm) = self.gm.borrow().clone() else {
            return;
        };
        let convs = self.inner.borrow().m.gm_convs(thread_id);
        if !convs.is_empty() {
            for conv in convs {
                gm.request(
                    |id| crate::core::gm::mark_conv_read_request(id, &conv),
                    |r| {
                        if !r.ok {
                            log::info!("mark read on the phone: {} {}", r.error, r.message);
                        }
                    },
                );
            }
            return;
        }
        let Some(key) = self.inner.borrow().m.gm_chat_key(thread_id) else {
            return;
        };
        gm.request(
            |id| crate::core::gm::mark_read_request(id, &key),
            |r| {
                if !r.ok {
                    log::info!("mark read on the phone: {} {}", r.error, r.message);
                }
            },
        );
    }

    /// Delete on the phone through Google Messages (the lasting link when
    /// it runs, else a one-shot helper off the main thread), then say how
    /// it went.
    fn delete_on_google(self: &Rc<Self>, chats: Vec<(i64, Vec<String>)>) {
        if !crate::core::phone_delete::available() {
            self.toast("Removed from PhoneRelay only: Google Messages isn't paired on this computer");
            return;
        }
        let weak = Rc::downgrade(self);
        let keys: Vec<String> = {
            let i = self.inner.borrow();
            chats.iter().map(|(_, a)| i.m.gm_key_for(a)).filter(|k| !k.is_empty()).collect()
        };
        if keys.is_empty() {
            self.toast("Removed from PhoneRelay only: the chat has no number or address to find it by");
            return;
        }
        let gm = self.gm.borrow().clone();
        if let Some(gm) = gm.filter(|g| g.is_running()) {
            let n = keys.len();
            let sent = gm.request(
                |id| crate::core::gm::delete_request(id, &keys),
                move |reply| {
                    let Some(this) = weak.upgrade() else { return };
                    let result = reply.delete_outcomes(n);
                    if let Err(e) = &result {
                        log::warn!("delete on phone: {e}");
                    }
                    this.toast(&crate::core::phone_delete::summary(&result));
                },
            );
            if sent {
                return;
            }
        }
        let weak = Rc::downgrade(self);
        let members: Vec<Vec<String>> = keys.into_iter().map(|k| vec![k]).collect();
        glib::spawn_future_local(async move {
            let done = gio::spawn_blocking(move || crate::core::phone_delete::delete_chats(&members)).await;
            let Some(this) = weak.upgrade() else { return };
            match done {
                Ok(result) => {
                    if let Err(e) = &result {
                        log::warn!("delete on phone: {e}");
                    }
                    this.toast(&crate::core::phone_delete::summary(&result));
                }
                Err(_) => this.toast("Removed from PhoneRelay only: the Google Messages helper crashed"),
            }
        });
    }

    /// Confirm, then delete one or several chats (here, and on the phone
    /// when the daemon can).
    fn delete_dialog(self: &Rc<Self>, thread_ids: Vec<i64>) {
        if thread_ids.is_empty() {
            return;
        }
        let this = self.clone();
        let client = self.inner.borrow().client.clone();
        glib::spawn_future_local(async move {
            let device = this.inner.borrow().kde_device_id();
            let any_real = thread_ids.iter().any(|t| *t >= 0);
            let gm_source = this.inner.borrow().m.is_gm();
            let kde_deletes = match (&client, &device) {
                (Some(c), Some(d)) if any_real && !gm_source => c.supports_conversation_deletion(d).await,
                _ => false,
            };
            let google_deletes = any_real && !kde_deletes && crate::core::phone_delete::available();
            let can_delete_on_phone = kde_deletes || google_deletes;
            let (heading, mut body) = if let [thread_id] = thread_ids.as_slice() {
                let i = this.inner.borrow();
                let name =
                    i.m.index
                        .get(*thread_id)
                        .map(|c| if c.display_name.is_empty() { c.address.clone() } else { c.display_name.clone() })
                        .unwrap_or_else(|| format!("Thread {thread_id}"));
                ("Delete Conversation".to_string(), format!("Remove the conversation with {name} from PhoneRelay?"))
            } else {
                let n = thread_ids.len();
                (format!("Delete {n} Conversations"), format!("Remove these {n} conversations from PhoneRelay?"))
            };
            body.push_str(if google_deletes {
                "\n\nThis also deletes them from your phone (Google Messages). It can't be undone."
            } else if kde_deletes {
                "\n\nPhone Link will also ask your phone to delete them there."
            } else {
                "\n\nThis removes them from PhoneRelay only: phone-side deletion needs Google Messages paired (gmcli) and the phonelink-gm helper."
            });
            if thread_ids.len() == 1 {
                body = body
                    .replace("deletes them from", "deletes it from")
                    .replace("delete them there", "delete it there")
                    .replace("removes them from", "removes it from");
            }
            let dialog = adw::MessageDialog::new(this.parent_window().as_ref(), Some(&heading), Some(&body));
            dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
            dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            let weak = Rc::downgrade(&this);
            dialog.connect_response(None, move |_, response| {
                if response != "delete" {
                    return;
                }
                let Some(this) = weak.upgrade() else { return };
                let now_ms = chrono::Utc::now().timestamp_millis();
                let (effects, to_phone) = {
                    let mut guard = this.inner.borrow_mut();
                    let i = &mut *guard;
                    let mut effects = Vec::new();
                    i.store.batch(|store| {
                        for &tid in &thread_ids {
                            effects.extend(i.m.delete(
                                tid,
                                can_delete_on_phone,
                                store,
                                &mut *i.settings.borrow_mut(),
                                now_ms,
                            ));
                        }
                    });
                    // Google deletes go to the helper in one run (it connects
                    // once), not one per chat.
                    let mut to_phone = Vec::new();
                    effects.retain(|e| match e {
                        Effect::DeleteOnPhone { thread_id, addresses } if google_deletes => {
                            to_phone.push((*thread_id, addresses.clone()));
                            false
                        }
                        _ => true,
                    });
                    // One "only from PhoneRelay" note, not one per chat.
                    let mut toasted = false;
                    effects.retain(|e| !matches!(e, Effect::Toast(_)) || !std::mem::replace(&mut toasted, true));
                    effects.push(Effect::ShowEmpty);
                    (effects, to_phone)
                };
                // After the borrow ends: this reads `inner` (chat keys).
                if !to_phone.is_empty() {
                    this.delete_on_google(to_phone);
                }
                this.run(effects);
            });
            dialog.present();
        });
    }

    /// "Sync Contacts": Google Contacts, a share from the phone, or a file.
    fn import_dialog(self: &Rc<Self>) {
        let dialog = adw::MessageDialog::new(
            self.parent_window().as_ref(),
            Some("Sync Contacts"),
            Some(
                "Choose how to load your full contact list:\n\n\
                 Google Contacts:\nAuthorize in your browser and import directly into PhoneRelay.\n\n\
                 Phone share:\n1. Open Contacts on your phone\n2. Tap ⋮ menu → Share\n3. Select all contacts\n\
                 4. Share via KDE Connect to this PC\n\n\
                 Manual file:\nImport a VCF or Google Contacts CSV if you already have one.",
            ),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("google", "Import Google Contacts"), ("file", "Import File…")]);
        dialog.set_response_appearance("google", adw::ResponseAppearance::Suggested);
        dialog.set_response_appearance("file", adw::ResponseAppearance::Suggested);
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(this) = weak.upgrade() else { return };
            match response {
                "google" => {
                    if let Some(g) = this.google.borrow().clone() {
                        g.sync_now();
                    }
                }
                "file" => this.contact_file_dialog(),
                _ => {}
            }
        });
        dialog.present();
    }

    pub fn set_google(&self, google: Rc<GoogleSync>) {
        *self.google.borrow_mut() = Some(google);
    }

    /// Every number in the current conversations (Google photos are fetched
    /// for these only).
    pub fn chat_numbers(&self) -> std::collections::HashSet<String> {
        let i = self.inner.borrow();
        i.m.index
            .conversations
            .values()
            .flat_map(|c| std::iter::once(c.address.clone()).chain(c.addresses.iter().cloned()))
            .filter(|a| !a.is_empty())
            .collect()
    }

    /// Names learned elsewhere (Google): saved and shown. Returns how many
    /// were new or changed.
    pub fn learn_contacts(self: &Rc<Self>, found: std::collections::HashMap<String, String>) -> usize {
        let found: std::collections::HashMap<String, String> = {
            let i = self.inner.borrow();
            found.into_iter().filter(|(k, v)| i.m.contact_map.get(k) != Some(v)).collect()
        };
        let changed = found.len();
        let effects = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            i.m.learn_contacts(found, &i.store)
        };
        self.list.set_contacts(self.inner.borrow().m.contact_map.clone());
        self.run(effects);
        changed
    }

    /// Import contacts from a VCF or Google Contacts CSV file.
    fn contact_file_dialog(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::new();
        dialog.set_title("Import Contacts");
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Contact files (*.vcf, *.csv)"));
        for pat in ["*.vcf", "*.VCF", "*.csv", "*.CSV"] {
            filter.add_pattern(pat);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));
        let weak = Rc::downgrade(self);
        dialog.open(self.parent_window().as_ref(), gio::Cancellable::NONE, move |res| {
            let (Some(this), Some(path)) = (weak.upgrade(), res.ok().and_then(|f| f.path())) else {
                return;
            };
            let count = this.import_contacts_file(&path, true);
            this.toast(&format!("Imported {count} contacts"));
        });
    }

    // ── contacts from other sources ────────────────────────────────

    fn subscribe_contact_sources(self: &Rc<Self>, client: &Client, device_id: &str) -> Vec<Subscription> {
        let base = device_path(device_id);
        let mut subs = Vec::new();
        // KDE Connect finished syncing the phone's contacts (vCards).
        let weak = Rc::downgrade(self);
        subs.push(signals::subscribe(
            client.bus(),
            &format!("{base}/contacts"),
            IFACE_CONTACTS,
            "localCacheSynchronized",
            move |_| {
                if let Some(this) = weak.upgrade() {
                    this.reload_contacts();
                }
            },
        ));
        // Incoming and missed calls name their caller.
        let weak = Rc::downgrade(self);
        subs.push(signals::subscribe(
            client.bus(),
            &format!("{base}/telephony"),
            IFACE_TELEPHONY,
            "callReceived",
            move |p| {
                let Some(this) = weak.upgrade() else { return };
                let Some((event, number, name)) = p.get::<(String, String, String)>() else {
                    return;
                };
                // KDE Connect 26.08 says "callReceived" for a ringing call.
                if crate::core::calls::CallEvent::parse(&event).is_none() {
                    return;
                }
                // The call, as a line in that number's chat.
                let effects = {
                    let mut guard = this.inner.borrow_mut();
                    let i = &mut *guard;
                    i.m.call_event(&event, &number, chrono::Utc::now().timestamp_millis(), &i.store)
                };
                this.run(effects);
                if let Some((norm, name)) = crate::core::contacts::harvest_contact_from_telephony_signal(&number, &name)
                {
                    let effects = {
                        let mut guard = this.inner.borrow_mut();
                        let i = &mut *guard;
                        i.m.learn_contacts([(norm, name)].into(), &i.store)
                    };
                    this.list.set_contacts(this.inner.borrow().m.contact_map.clone());
                    this.run(effects);
                }
            },
        ));
        // A contacts file shared from the phone is imported.
        let weak = Rc::downgrade(self);
        subs.push(signals::subscribe(client.bus(), &format!("{base}/share"), IFACE_SHARE, "shareReceived", move |p| {
            let (Some(this), Some((url,))) = (weak.upgrade(), p.get::<(String,)>()) else {
                return;
            };
            let path = if url.starts_with("file://") {
                gio::File::for_uri(&url).path()
            } else {
                Some(std::path::PathBuf::from(&url))
            };
            if let Some(path) = path.filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("vcf"))) {
                this.import_contacts_file(&path, false);
            }
        }));
        subs
    }

    /// Re-read the contact names (vCards + saved) and redraw.
    fn reload_contacts(self: &Rc<Self>) {
        let effects = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            let Some(dev) = i.kde_device_id().or_else(|| i.m.device_id().map(str::to_string)) else {
                return;
            };
            i.m.contact_map = load_contact_map(&dev, i.store.load_contacts(), i.store.load_contact_overrides());
            i.m.refresh_names();
            let mut e = vec![Effect::RefreshList];
            if let Some(a) = i.m.active_thread() {
                e.push(Effect::ShowThread(a));
            }
            e
        };
        self.list.set_contacts(self.inner.borrow().m.contact_map.clone());
        self.run(effects);
    }

    /// Ask KDE Connect to sync the phone's contacts; warn once if, 8 s
    /// later, it has synced none (as Python).
    fn request_contact_sync(self: &Rc<Self>) {
        self.with_device(|c, d| async move {
            c.sync_contacts(&d).await;
        });
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local_once(8, move || {
            let Some(this) = weak.upgrade() else { return };
            let Some(dev) = this.inner.borrow().kde_device_id() else {
                return;
            };
            if crate::core::contacts::synced_vcard_count(&dev) > 0
                || !this.inner.borrow_mut().contact_sync_warned.insert(dev)
            {
                return;
            }
            this.toast(
                "KDE Connect did not populate desktop contacts. Check the phone's Contacts permission or import a contacts file.",
            );
        });
    }

    /// First run with almost no contacts: import the newest contacts file in
    /// ~/Downloads (often shared from the phone), as Python.
    fn scan_downloads_for_vcf(self: &Rc<Self>) {
        if self.inner.borrow().m.contact_map.len() > 5 {
            return;
        }
        let downloads = crate::core::settings::home_dir().join("Downloads");
        let Ok(entries) = std::fs::read_dir(&downloads) else {
            return;
        };
        let newest = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("vcf")))
            .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        if let Some(path) = newest {
            self.import_contacts_file(&path, false);
        }
    }

    /// Import a VCF or Google CSV file; returns how many contacts it had.
    /// `rename`: the user picked the file, so its names win. A file that
    /// came from the phone only names unknown numbers.
    fn import_contacts_file(self: &Rc<Self>, path: &std::path::Path, rename: bool) -> usize {
        let mut found = std::collections::HashMap::new();
        let is_csv = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv"));
        let count = if is_csv {
            crate::core::contacts::import_google_csv(path, &mut found)
        } else {
            crate::core::contacts::import_vcf_file(path, &mut found)
        };
        let effects = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            let found = if rename { found } else { crate::core::contacts::only_unnamed(found, &i.m.contact_map) };
            i.m.learn_contacts(found, &i.store)
        };
        self.list.set_contacts(self.inner.borrow().m.contact_map.clone());
        self.run(effects);
        log::info!("imported {count} contacts from {}", path.display());
        count
    }

    /// Show this page's toasts on the window's overlay, so they queue with
    /// the window's own instead of stacking on top of them.
    pub fn use_toasts(&self, overlay: &adw::ToastOverlay) {
        *self.toasts.borrow_mut() = Some(overlay.clone());
    }

    fn add_toast(&self, toast: adw::Toast) {
        match self.toasts.borrow().as_ref() {
            Some(overlay) => overlay.add_toast(toast),
            None => self.widget.add_toast(toast),
        }
    }

    fn toast(&self, text: &str) {
        self.add_toast(adw::Toast::new(text));
    }

    /// The compose bar's send. Returns whether it was taken.
    fn send(self: &Rc<Self>, thread_id: i64, text: &str, files: &[std::path::PathBuf], reply_to: Option<&str>) -> bool {
        let attachments: Vec<String> = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let (effects, taken) = {
            let mut i = self.inner.borrow_mut();
            i.m.gm_reply_next(reply_to.map(str::to_string));
            let effects = i.m.send_with(thread_id, text, attachments);
            // Refused (phone away): keep the text so it can be sent later.
            let taken = !effects.iter().any(|e| matches!(e, Effect::Toast(_))) && !effects.is_empty();
            (effects, taken)
        };
        self.run(effects);
        taken
    }

    /// A notification reply got no clear answer. Give the phone's echo a
    /// moment; if it never comes, say so and offer to send it again (as SMS).
    fn watch_for_echo(self: &Rc<Self>, thread_id: i64, text: String) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local_once(15, move || {
            let Some(this) = weak.upgrade() else { return };
            if !this.inner.borrow().m.awaiting_echo(thread_id, &text) {
                return; // it went out after all
            }
            let toast = adw::Toast::new("That text may not have sent");
            toast.set_button_label(Some("Send again"));
            toast.set_timeout(0);
            let weak = Rc::downgrade(&this);
            toast.connect_button_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    let effects = this.inner.borrow_mut().m.notification_reply_failed(thread_id, &text, true);
                    this.run(effects);
                }
            });
            this.add_toast(toast);
        });
    }

    // ── notifications (for replying through them) ─────────────────

    /// The window's notifications model reports a change. A full load is
    /// what was already on the phone: names only, not the hidden-texts watch.
    fn notification_event(self: &Rc<Self>, e: &NoteEvent) {
        let (schedule, effects) = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            match e {
                NoteEvent::Loaded(all) => {
                    let mut effects = Vec::new();
                    for (id, props) in all.iter() {
                        i.m.note_notification(replyable(id, props));
                        effects.extend(i.m.notification_seen(&as_props(props), epoch_now(), &i.store, false).1);
                    }
                    (false, effects)
                }
                NoteEvent::Posted(id, props) => {
                    i.m.note_notification(replyable(id, props));
                    i.m.notification_seen(&as_props(props), epoch_now(), &i.store, true)
                }
                NoteEvent::Removed(id) => {
                    i.m.remove_notification(id);
                    (false, Vec::new())
                }
                NoteEvent::Cleared => {
                    i.m.clear_notifications();
                    (false, Vec::new())
                }
            }
        };
        self.run(effects);
        // Google Messages hides nothing (the phone's restricted texts are a
        // KDE Connect problem).
        if schedule && !self.inner.borrow().m.is_gm() {
            self.schedule_hidden_check();
        }
    }

    /// Re-read the phone's notifications (through the window's model).
    fn load_notifications(&self) {
        self.notifications.reload();
    }

    /// Catch up on texts the phone never pushed (resume, window shown,
    /// reconnect), at most once per `min_interval` seconds.
    pub fn catch_up(self: &Rc<Self>, min_interval: f64) {
        if self.inner.borrow().m.is_gm() {
            return;
        }
        self.inner.borrow_mut().m.catch_up.catch_up(now(), sync::CATCH_UP_DELAY_SECONDS, min_interval);
        self.reschedule();
    }

    /// Judge texting notifications once they're old enough.
    fn schedule_hidden_check(self: &Rc<Self>) {
        if self.inner.borrow().hidden_check.is_some() {
            return;
        }
        let delay = self.inner.borrow().m.hidden_texts.settle_seconds as u32 + 5;
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_seconds_local_once(delay, move || {
            let Some(this) = weak.upgrade() else { return };
            let (changed, warning, pending) = {
                let mut i = this.inner.borrow_mut();
                i.hidden_check = None;
                let changed = i.m.check_hidden_texts(epoch_now());
                (changed, i.m.hidden_texts.warning, i.m.hidden_texts.has_pending())
            };
            if changed {
                if warning {
                    this.hidden_banner.set_title("Your phone is hiding some texts from PhoneRelay");
                }
                this.hidden_banner.set_revealed(warning);
            }
            if pending {
                this.schedule_hidden_check();
            }
        });
        self.inner.borrow_mut().hidden_check = Some(id);
    }

    fn show_thread(&self, tid: i64) {
        let i = self.inner.borrow();
        let Some(conv) = i.m.index.get(tid) else {
            return;
        };
        self.list.select_thread(conv.thread_id);
        let _t = super::perf::span("updating the open chat");
        self.thread.sync(conv, &|uid| i.m.route_of(uid));
        drop(i);
        self.show_extras(tid);
    }

    /// After a chat opens: its typing line and whether older messages wait.
    fn after_open(self: &Rc<Self>, tid: i64) {
        self.show_typing(tid);
        self.show_more_history(tid);
    }

    /// Messages just went on screen: ask the phone for their full pictures
    /// (once each), one request at a time.
    fn want_full_pictures(self: &Rc<Self>, thread_id: i64, uids: Vec<i64>) {
        // A few at a time: one by one was slow, 22 at once timed out the
        // daemon (laptop, 2026-11-06).
        const FULL_PICTURE_WORKERS: usize = 3;
        if self.inner.borrow().m.is_gm() {
            let reqs = {
                let mut guard = self.inner.borrow_mut();
                let uids: std::collections::HashSet<i64> = uids.into_iter().collect();
                guard.m.take_gm_downloads(thread_id, &uids, &full_cache_dir())
            };
            self.gm_download(reqs.into_iter().map(|r| (r, false)));
            return;
        }
        let start = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            let uids: std::collections::HashSet<i64> = uids.into_iter().collect();
            let reqs = i.m.take_full_image_requests(thread_id, &uids, &full_cache_dir());
            i.full_queue.extend(reqs);
            let start = FULL_PICTURE_WORKERS.saturating_sub(i.full_pumping).min(i.full_queue.len());
            i.full_pumping += start;
            start
        };
        for _ in 0..start {
            let weak = Rc::downgrade(self);
            self.with_device(move |c, d| async move {
                loop {
                    let Some(this) = weak.upgrade() else { return };
                    let next = this.inner.borrow_mut().full_queue.pop_front();
                    let Some((part, uid)) = next else {
                        this.inner.borrow_mut().full_pumping -= 1;
                        return;
                    };
                    drop(this);
                    c.request_attachment_file(&d, part, &uid).await;
                }
            });
        }
    }

    /// The user asked for a file (Download, or a picture still loading).
    fn request_attachment(self: &Rc<Self>, part_id: i64, unique_identifier: String, file_name: String) {
        if part_id == 0 {
            let req = self.inner.borrow().m.gm_download_for(&unique_identifier);
            if let Some(req) = req {
                log::info!("downloading {file_name} from Google Messages");
                self.gm_download(std::iter::once((req, true)));
            }
            return;
        }
        log::info!("asking the phone for {file_name}");
        self.with_device(move |c, d| async move {
            if !c.request_attachment_file(&d, part_id, &unique_identifier).await {
                log::warn!("could not request {file_name} from the phone");
            }
        });
    }

    /// Download Google Messages attachments through the helper, a few at
    /// a time, then show (or open) them.
    fn gm_download(self: &Rc<Self>, reqs: impl IntoIterator<Item = (GmDownload, bool)>) {
        const WORKERS: usize = 3;
        self.inner.borrow_mut().gm_downloads.extend(reqs);
        loop {
            let next = {
                let mut i = self.inner.borrow_mut();
                if i.gm_downloading >= WORKERS {
                    return;
                }
                let Some(next) = i.gm_downloads.pop_front() else {
                    return;
                };
                i.gm_downloading += 1;
                next
            };
            if !self.gm_download_one(next) {
                self.inner.borrow_mut().gm_downloading -= 1;
            }
        }
    }

    fn gm_download_one(self: &Rc<Self>, (req, open): (GmDownload, bool)) -> bool {
        let Some(gm) = self.gm.borrow().clone() else {
            return false;
        };
        let dir = match crate::core::private_tmp::dir(Some("gm_downloads")) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("download folder: {e}");
                return false;
            }
        };
        let tmp = dir.join(crate::core::attachments::sanitize_name(&req.unique));
        let weak = Rc::downgrade(self);
        let path = tmp.to_string_lossy().into_owned();
        gm.request(
            |id| crate::core::gm::download_request(id, &req.media, &req.key, &path),
            move |r| {
                let Some(this) = weak.upgrade() else { return };
                this.inner.borrow_mut().gm_downloading -= 1;
                let done = if r.ok {
                    let mut guard = this.inner.borrow_mut();
                    let i = &mut *guard;
                    i.m.gm_downloaded(&req.unique, &tmp, &i.store, &full_cache_dir())
                } else {
                    log::warn!("Google Messages download failed: {} {}", r.error, r.message);
                    None
                };
                let _ = std::fs::remove_file(&tmp);
                match done {
                    Some((tid, dest)) => {
                        if open {
                            let uri = gio::File::for_path(&dest).uri();
                            if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE) {
                                log::warn!("open attachment: {e}");
                            }
                        }
                        this.pictures_arrived(tid);
                    }
                    None if open => this.toast(TOAST_GM_DOWNLOAD_FAILED),
                    None => {}
                }
                this.gm_download(std::iter::empty());
            },
        )
    }

    fn attachment_received(self: &Rc<Self>, file_path: &str, file_name: &str) {
        let tid = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            i.m.attachment_received(std::path::Path::new(file_path), file_name, &i.store, &full_cache_dir())
        };
        let Some(tid) = tid else {
            // Not an inline picture: KDE Connect saved a download.
            log::info!("the phone sent {file_name} ({file_path})");
            return;
        };
        self.pictures_arrived(tid);
    }

    /// Full pictures for chat `tid` are here: redraw it if it's open.
    fn pictures_arrived(self: &Rc<Self>, tid: i64) {
        let concerns_open_chat = {
            let i = self.inner.borrow();
            i.m.active_thread().is_some_and(|a| i.m.index.primary_for(a) == i.m.index.primary_for(tid))
        };
        if !concerns_open_chat {
            return;
        }
        // Pictures tend to arrive in a burst: one redraw after the last.
        if let Some(old) = self.inner.borrow_mut().redraw_pending.take() {
            old.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(300), move || {
            let Some(this) = weak.upgrade() else { return };
            this.inner.borrow_mut().redraw_pending = None;
            let i = this.inner.borrow();
            if let Some(conv) = i.m.active_thread().and_then(|a| i.m.index.get(a)) {
                let _t = super::perf::span("redrawing the chat for new pictures");
                this.thread.redraw(conv, &|uid| i.m.route_of(uid));
            }
        });
        self.inner.borrow_mut().redraw_pending = Some(id);
    }
}
