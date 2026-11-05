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
use crate::core::contacts::load_contact_map;
use crate::core::message::parse_message;
use crate::core::messages::{Effect, Messages};
use crate::core::models::Device;
use crate::core::settings::Settings;
use crate::core::store::MessageStore;
use crate::core::sync::{self, Action, SIGNAL_BATCH_SIZE};
use crate::dbus::signals::{self, Subscription};
use crate::dbus::{Client, IFACE_CONVERSATIONS, device_path, variant};

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
    pub widget: gtk::Stack,
    list: Rc<ConversationList>,
    thread_status: adw::StatusPage,
    inner: RefCell<Inner>,
}

impl MessagesPage {
    pub fn new(store: MessageStore, settings: Settings) -> Rc<Self> {
        let widget = gtk::Stack::new();
        widget.set_vexpand(true);
        widget.set_hexpand(true);
        widget.set_transition_type(gtk::StackTransitionType::Crossfade);

        let status = adw::StatusPage::new();
        status.set_icon_name(Some("mail-unread-symbolic"));
        status.set_title("Messages");
        status.set_description(Some("No phone linked yet.\nPair your phone in KDE Connect to see your texts here."));
        widget.add_named(&status, Some("status"));

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let list = ConversationList::new();
        content.append(&list.widget);
        content.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        // TODO(Phase 3 step 2): the thread view replaces this.
        let thread_status = adw::StatusPage::new();
        thread_status.set_icon_name(Some("mail-unread-symbolic"));
        thread_status.set_title("Select a conversation");
        thread_status.set_hexpand(true);
        content.append(&thread_status);
        widget.add_named(&content, Some("content"));
        widget.set_visible_child_name("status");

        let this = Rc::new(Self {
            widget,
            list,
            thread_status,
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
            self.widget.set_visible_child_name("status");
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
            let mut effects = i.m.load_cached(&i.store);
            effects.push(Effect::RefreshList);
            i.m.catch_up.catch_up(now(), sync::CATCH_UP_STARTUP_DELAY_SECONDS, 0.0);
            effects
        };
        self.widget.set_visible_child_name("content");
        self.run(effects);
        self.reschedule();

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
                Effect::ShowEmpty => {
                    self.thread_status.set_title("Select a conversation");
                    self.thread_status.set_description(None);
                }
                Effect::Toast(text) => log::info!("toast: {text}"),
                // TODO(Phase 3 step 3): sending and deleting. Nothing is sent
                // to the phone until compose is wired up and reviewed.
                Effect::Reply { .. } | Effect::SendNew { .. } | Effect::DeleteOnPhone { .. } => {
                    log::warn!("not wired up yet: {e:?}");
                }
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

    fn show_thread(&self, tid: i64) {
        let i = self.inner.borrow();
        let Some(conv) = i.m.index.get(tid) else { return };
        self.list.select_thread(conv.thread_id);
        self.thread_status.set_title(if conv.display_name.is_empty() { &conv.address } else { &conv.display_name });
        self.thread_status.set_description(Some(&format!(
            "{} messages loaded. The thread view is the next step of Phase 3.",
            conv.messages.len()
        )));
    }
}
