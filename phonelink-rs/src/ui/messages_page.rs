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
use crate::core::contacts::load_contact_map;
use crate::core::message::parse_message;
use crate::core::contacts::notification_message_text;
use crate::core::messages::{Effect, Messages, ReplyableNotification};
use crate::core::models::Device;
use crate::core::settings::Settings;
use crate::core::store::MessageStore;
use crate::core::sync::{self, Action, SIGNAL_BATCH_SIZE};
use crate::dbus::signals::{self, Subscription};
use crate::dbus::{
    Client, IFACE_CONTACTS, IFACE_CONVERSATIONS, IFACE_SHARE, IFACE_TELEPHONY, device_path,
    variant,
};

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
    /// A full picture arrived: redraw once things settle.
    redraw_pending: Option<glib::SourceId>,
    /// Threads we just asked the daemon to replay (their conversationLoaded
    /// is ours, not news), with when.
    recent_requests: std::collections::HashMap<i64, f64>,
}

pub struct MessagesPage {
    /// The page; toasts float over it.
    pub widget: adw::ToastOverlay,
    stack: gtk::Stack,
    list: Rc<ConversationList>,
    thread: Rc<ThreadView>,
    /// "Your phone is hiding some texts" (rule 1).
    hidden_banner: adw::Banner,
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
    inner: RefCell<Inner>,
}

impl MessagesPage {
    pub fn new(store: MessageStore, settings: Rc<RefCell<Settings>>, notifications: Rc<NotificationsModel>) -> Rc<Self> {
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
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
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
        });
        content.append(&thread.widget);
        stack.add_named(&content, Some("content"));
        stack.set_visible_child_name("status");

        let this = Rc::new(Self {
            widget,
            stack,
            hidden_banner,
            list,
            thread,
            notifications: notifications.clone(),
            toasts: RefCell::default(),
            google: RefCell::default(),
            search: RefCell::default(),
            gm: RefCell::default(),
            phone_read: RefCell::default(),
            phone_read_queued: Default::default(),
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
                redraw_pending: None,
                recent_requests: Default::default(),
            }),
        });
        *download_target.borrow_mut() = Rc::downgrade(&this);
        let weak = Rc::downgrade(&this);
        *this.gm.borrow_mut() = super::gm_link::GmLink::start(move |e| {
            if let Some(this) = weak.upgrade() {
                this.gm_event(e);
            }
        });
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
        this.thread.connect_send(move |tid, text, picture| {
            let Some(this) = weak.upgrade() else { return false };
            this.send(tid, text, picture)
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
            let (Some(this), Some(tid)) = (weak.upgrade(), variant::first_int(p)) else { return };
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
            let (Some(this), Some(tid)) = (weak.upgrade(), variant::first_int(p)) else { return };
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
        let Some(deadline) = i.m.catch_up.next_deadline() else { return };
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
                Effect::Sync(action) => self.sync(action),
                Effect::MarkRead { thread_id } => {
                    self.mark_read_on_google(thread_id);
                    self.with_device(move |c, d| async move {
                        c.mark_conversation_as_read(&d, thread_id).await;
                    })
                }
                Effect::ShowThread(tid) => self.show_thread(tid),
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
                        let Err(e) = c.try_send_notification_reply_by_id(&d, &reply_id, &text).await else { return };
                        let refused = crate::dbus::client::is_definite_refusal(&e);
                        log::warn!("notification reply failed ({}): {}", if refused { "refused" } else { "unclear" }, e.message());
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
        if let (Some(c), Some(d)) = (i.client.clone(), i.m.device_id().map(str::to_string)) {
            glib::spawn_future_local(f(c, d));
        }
    }

    fn list_event(self: &Rc<Self>, e: ListEvent) {
        match e {
            ListEvent::Start { address, name } => {
                let effects = self.inner.borrow_mut().m.open_or_create(&address, &name);
                if effects.is_empty() {
                    self.toast(crate::core::messages::TOAST_NOT_CONNECTED);
                }
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
        if let Incoming::Ready { me } = e {
            self.inner.borrow_mut().m.gm_me = me;
            return;
        }
        let Incoming::Conversation { chat, unread, last_ts_ms } = e else { return };
        if unread {
            self.phone_read.borrow_mut().remove(&chat);
            return;
        }
        self.phone_read.borrow_mut().insert(chat, last_ts_ms);
        self.apply_phone_read();
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
        let Some(gm) = self.gm.borrow().clone() else { return };
        let Some(key) = self.inner.borrow().m.gm_chat_key(thread_id) else { return };
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
            let device = this.inner.borrow().m.device_id().map(str::to_string);
            let any_real = thread_ids.iter().any(|t| *t >= 0);
            let kde_deletes = match (&client, &device) {
                (Some(c), Some(d)) if any_real => c.supports_conversation_deletion(d).await,
                _ => false,
            };
            let google_deletes = any_real && !kde_deletes && crate::core::phone_delete::available();
            let can_delete_on_phone = kde_deletes || google_deletes;
            let (heading, mut body) = if let [thread_id] = thread_ids.as_slice() {
                let i = this.inner.borrow();
                let name = i.m.index
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
                            effects.extend(i.m.delete(tid, can_delete_on_phone, store, &mut *i.settings.borrow_mut(), now_ms));
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
            let (Some(this), Some(path)) = (weak.upgrade(), res.ok().and_then(|f| f.path())) else { return };
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
        subs.push(signals::subscribe(client.bus(), &format!("{base}/contacts"), IFACE_CONTACTS, "localCacheSynchronized", move |_| {
            if let Some(this) = weak.upgrade() {
                this.reload_contacts();
            }
        }));
        // Incoming and missed calls name their caller.
        let weak = Rc::downgrade(self);
        subs.push(signals::subscribe(client.bus(), &format!("{base}/telephony"), IFACE_TELEPHONY, "callReceived", move |p| {
            let Some(this) = weak.upgrade() else { return };
            let Some((event, number, name)) = p.get::<(String, String, String)>() else { return };
            if !matches!(event.as_str(), "ringing" | "missedCall") {
                return;
            }
            if let Some((norm, name)) = crate::core::contacts::harvest_contact_from_telephony_signal(&number, &name) {
                let effects = {
                    let mut guard = this.inner.borrow_mut();
                    let i = &mut *guard;
                    i.m.learn_contacts([(norm, name)].into(), &i.store)
                };
                this.list.set_contacts(this.inner.borrow().m.contact_map.clone());
                this.run(effects);
            }
        }));
        // A contacts file shared from the phone is imported.
        let weak = Rc::downgrade(self);
        subs.push(signals::subscribe(client.bus(), &format!("{base}/share"), IFACE_SHARE, "shareReceived", move |p| {
            let (Some(this), Some((url,))) = (weak.upgrade(), p.get::<(String,)>()) else { return };
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
            let Some(dev) = i.m.device_id().map(str::to_string) else { return };
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
            let Some(dev) = this.inner.borrow().m.device_id().map(str::to_string) else { return };
            if crate::core::contacts::synced_vcard_count(&dev) > 0 || !this.inner.borrow_mut().contact_sync_warned.insert(dev) {
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
        let Ok(entries) = std::fs::read_dir(&downloads) else { return };
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
    fn send(self: &Rc<Self>, thread_id: i64, text: &str, picture: Option<&std::path::Path>) -> bool {
        let attachments: Vec<String> = picture.map(|p| p.to_string_lossy().into_owned()).into_iter().collect();
        let (effects, taken) = {
            let mut i = self.inner.borrow_mut();
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
        if schedule {
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
        let Some(conv) = i.m.index.get(tid) else { return };
        self.list.select_thread(conv.thread_id);
        let _t = super::perf::span("updating the open chat");
        self.thread.sync(conv, &|uid| i.m.route_of(uid));
    }

    /// Messages just went on screen: ask the phone for their full pictures
    /// (once each), one request at a time.
    fn want_full_pictures(self: &Rc<Self>, thread_id: i64, uids: Vec<i64>) {
        // A few at a time: one by one was slow, 22 at once timed out the
        // daemon (laptop, 2026-11-06).
        const FULL_PICTURE_WORKERS: usize = 3;
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
    fn request_attachment(&self, part_id: i64, unique_identifier: String, file_name: String) {
        if part_id == 0 {
            return;
        }
        log::info!("asking the phone for {file_name}");
        self.with_device(move |c, d| async move {
            if !c.request_attachment_file(&d, part_id, &unique_identifier).await {
                log::warn!("could not request {file_name} from the phone");
            }
        });
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
