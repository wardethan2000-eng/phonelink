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

use super::conversation_list::ConversationList;
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
use crate::dbus::{Client, IFACE_CONVERSATIONS, IFACE_NOTIFICATIONS, device_path, variant};

/// A notification's properties in the shape `core` reads.
fn replyable(id: &str, props: &std::collections::BTreeMap<String, Variant>) -> ReplyableNotification {
    let text_of = |k: &str| props.get(k).map(variant::string).unwrap_or_default();
    let json: crate::core::contacts::Props =
        ["appName", "title", "text", "ticker"].iter().map(|k| (k.to_string(), text_of(k).into())).collect();
    ReplyableNotification {
        public_id: id.to_string(),
        app_name: text_of("appName"),
        title: text_of("title"),
        text: notification_message_text(&json),
        reply_id: text_of("replyId"),
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
    settings: Settings,
    client: Option<Client>,
    subs: Vec<Subscription>,
    queue: VecDeque<Variant>,
    draining: bool,
    timer: Option<glib::SourceId>,
}

pub struct MessagesPage {
    /// The page; toasts float over it.
    pub widget: adw::ToastOverlay,
    stack: gtk::Stack,
    list: Rc<ConversationList>,
    thread: Rc<ThreadView>,
    inner: RefCell<Inner>,
}

impl MessagesPage {
    pub fn new(store: MessageStore, settings: Settings) -> Rc<Self> {
        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.set_hexpand(true);
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        let widget = adw::ToastOverlay::new();
        widget.set_child(Some(&stack));

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
            list,
            thread,
            inner: RefCell::new(Inner {
                m: Messages::new(),
                store,
                settings,
                client: None,
                subs: Vec::new(),
                queue: VecDeque::new(),
                draining: false,
                timer: None,
            }),
        });
        *download_target.borrow_mut() = Rc::downgrade(&this);
        let weak = Rc::downgrade(&this);
        this.thread.connect_send(move |tid, text, picture| {
            let Some(this) = weak.upgrade() else { return false };
            this.send(tid, text, picture)
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_selected(move |tid| {
            if let Some(this) = weak.upgrade() {
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
            self.inner.borrow_mut().m.set_reachable(reachable);
            if reachable {
                self.inner.borrow_mut().m.catch_up.catch_up(
                    now(),
                    sync::CATCH_UP_DELAY_SECONDS,
                    sync::CATCH_UP_MIN_INTERVAL_SECONDS,
                );
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
            i.m.contact_map = load_contact_map(&dev.id, i.store.load_contacts());
            i.subs = self.subscribe(client, &dev.id);
            i.subs.extend(self.subscribe_notifications(client, &dev.id));
            let mut effects = i.m.load_cached(&i.store);
            effects.push(Effect::RefreshList);
            i.m.catch_up.catch_up(now(), sync::CATCH_UP_STARTUP_DELAY_SECONDS, 0.0);
            effects
        };
        self.stack.set_visible_child_name("content");
        self.run(effects);
        self.reschedule();

        self.load_notifications();

        // The daemon's cached newest-message-per-thread list.
        let (weak, client, id) = (Rc::downgrade(self), client.clone(), dev.id.clone());
        glib::spawn_future_local(async move {
            let entries = client.active_conversations(&id).await;
            let raw: Vec<_> = entries.iter().filter_map(variant::parse_message).collect();
            let Some(this) = weak.upgrade() else { return };
            let effects = {
                let mut guard = this.inner.borrow_mut();
                let i = &mut *guard;
                if i.m.device_id() != Some(id.as_str()) {
                    return;
                }
                i.m.apply_active(&raw, &i.store, &mut i.settings, now())
            };
            log::info!("activeConversations: {} entries, {} parsed", entries.len(), raw.len());
            this.run(effects);
            this.reschedule();
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
            this.run(effects);
            this.reschedule();
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
            let (m, settings) = (&mut i.m, &mut i.settings);
            let t = now();
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
                Effect::MarkRead { thread_id } => self.with_device(move |c, d| async move {
                    c.mark_conversation_as_read(&d, thread_id).await;
                }),
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
                Effect::DeleteOnPhone { thread_id } => self.with_device(move |c, d| async move {
                    c.delete_conversation(&d, thread_id).await;
                }),
            }
        }
        if refresh {
            let i = self.inner.borrow();
            self.list.set_conversations(&i.m.visible(&i.settings));
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
            Action::ReplayFromDaemon { thread_id, start, end } => self.with_device(move |c, d| async move {
                let clamp = |n: i64| i32::try_from(n).unwrap_or(i32::MAX);
                c.request_conversation(&d, thread_id, clamp(start), clamp(end)).await;
            }),
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

    fn toast(&self, text: &str) {
        self.widget.add_toast(adw::Toast::new(text));
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
            this.widget.add_toast(toast);
        });
    }

    // ── notifications (for replying through them) ─────────────────

    fn subscribe_notifications(self: &Rc<Self>, client: &Client, device_id: &str) -> Vec<Subscription> {
        let path = format!("{}/notifications", device_path(device_id));
        let mut subs = Vec::new();
        for name in ["notificationPosted", "notificationUpdated"] {
            let weak = Rc::downgrade(self);
            subs.push(signals::subscribe(client.bus(), &path, IFACE_NOTIFICATIONS, name, move |p| {
                let (Some(this), Some(id)) = (weak.upgrade(), p.get::<(String,)>().map(|t| t.0)) else { return };
                this.load_notification(id);
            }));
        }
        let weak = Rc::downgrade(self);
        subs.push(signals::subscribe(client.bus(), &path, IFACE_NOTIFICATIONS, "notificationRemoved", move |p| {
            let (Some(this), Some(id)) = (weak.upgrade(), p.get::<(String,)>().map(|t| t.0)) else { return };
            this.inner.borrow_mut().m.remove_notification(&id);
        }));
        let weak = Rc::downgrade(self);
        subs.push(signals::subscribe(client.bus(), &path, IFACE_NOTIFICATIONS, "allNotificationsRemoved", move |_| {
            if let Some(this) = weak.upgrade() {
                this.inner.borrow_mut().m.clear_notifications();
            }
        }));
        subs
    }

    fn load_notifications(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.with_device(move |c, d| async move {
            let all = c.fetch_active_notifications(&d).await;
            let Some(this) = weak.upgrade() else { return };
            let mut i = this.inner.borrow_mut();
            for (id, props) in all {
                i.m.note_notification(replyable(&id, &props));
            }
        });
    }

    fn load_notification(self: &Rc<Self>, id: String) {
        let weak = Rc::downgrade(self);
        self.with_device(move |c, d| async move {
            let props = c.notification_properties(&d, &id).await;
            if let Some(this) = weak.upgrade() {
                this.inner.borrow_mut().m.note_notification(replyable(&id, &props));
            }
        });
    }

    fn show_thread(&self, tid: i64) {
        let requests = {
            let mut guard = self.inner.borrow_mut();
            let i = &mut *guard;
            let Some(conv) = i.m.index.get(tid) else { return };
            self.list.select_thread(conv.thread_id);
            self.thread.sync(conv, &|uid| i.m.route_of(uid));
            // Pictures here only as the phone's small preview: ask for the
            // full files (once each).
            i.m.take_full_image_requests(tid, &full_cache_dir())
        };
        for (part, uid) in requests {
            self.with_device(move |c, d| async move {
                c.request_attachment_file(&d, part, &uid).await;
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

    fn attachment_received(&self, file_path: &str, file_name: &str) {
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
        let i = self.inner.borrow();
        if let Some(active) = i.m.active_thread()
            && i.m.index.primary_for(active) == i.m.index.primary_for(tid)
            && let Some(conv) = i.m.index.get(active)
        {
            self.thread.redraw(conv, &|uid| i.m.route_of(uid));
        }
    }
}
