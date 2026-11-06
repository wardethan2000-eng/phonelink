//! The phone's notifications, subscribed once and shared: the sidebar lists
//! them, and Messages uses them for replies (Route A), hidden-text checks and
//! contact names. The window owns this and feeds it the active device.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use glib::Variant;

use crate::core::models::{Device, Notification};
use crate::dbus::signals::{self, Subscription};
use crate::dbus::{Client, IFACE_NOTIFICATIONS, device_path, variant};

/// One notification's D-Bus properties (`GetAll`).
pub type Props = BTreeMap<String, Variant>;

pub enum NoteEvent<'a> {
    /// A full list (first load, reconnect, refresh).
    Loaded(&'a [(String, Props)]),
    /// Posted or updated, with fresh properties.
    Posted(&'a str, &'a Props),
    Removed(&'a str),
    Cleared,
}

type Listener = Rc<dyn Fn(&NoteEvent)>;

#[derive(Default)]
pub struct NotificationsModel {
    client: RefCell<Option<Client>>,
    device_id: RefCell<Option<String>>,
    subs: RefCell<Vec<Subscription>>,
    /// Bumped on every device change, so a fetch for the old device is dropped.
    generation: Cell<u64>,
    listeners: RefCell<Vec<Listener>>,
}

/// The sidebar's view of a notification. `timestamp` is when it arrived here.
pub fn notification_from(id: &str, props: &Props, timestamp: f64) -> Notification {
    let text = |k: &str| props.get(k).map(variant::string).unwrap_or_default();
    let flag = |k: &str| props.get(k).and_then(variant::int).is_some_and(|n| n != 0);
    Notification {
        public_id: id.to_string(),
        app_name: text("appName"),
        title: text("title"),
        text: text("text").trim().to_string(),
        ticker: text("ticker"),
        icon_path: text("iconPath"),
        has_icon: flag("hasIcon"),
        dismissable: flag("dismissable"),
        silent: flag("silent"),
        reply_id: text("replyId"),
        internal_id: text("internalId"),
        timestamp,
    }
}

pub fn epoch_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

impl NotificationsModel {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// Call `f` for every change, in the order they happen.
    pub fn connect(&self, f: impl Fn(&NoteEvent) + 'static) {
        self.listeners.borrow_mut().push(Rc::new(f));
    }

    fn emit(&self, e: NoteEvent) {
        // Listeners may call back into the model; don't hold the borrow.
        let listeners = self.listeners.borrow().clone();
        for l in listeners {
            l(&e);
        }
    }

    /// The active device changed (or went away). Reloads when it changes or
    /// comes back online.
    pub fn set_device(self: &Rc<Self>, client: &Client, device: Option<&Device>, became_reachable: bool) {
        let id = device.map(|d| d.id.clone());
        if *self.device_id.borrow() == id {
            if became_reachable {
                self.reload();
            }
            return;
        }
        self.generation.set(self.generation.get() + 1);
        self.subs.borrow_mut().clear();
        *self.client.borrow_mut() = Some(client.clone());
        *self.device_id.borrow_mut() = id.clone();
        self.emit(NoteEvent::Cleared);
        if let Some(id) = id {
            *self.subs.borrow_mut() = self.subscribe(client, &id);
            self.reload();
        }
    }

    fn current(&self) -> Option<(Client, String, u64)> {
        let client = self.client.borrow().clone()?;
        let id = self.device_id.borrow().clone()?;
        Some((client, id, self.generation.get()))
    }

    fn subscribe(self: &Rc<Self>, client: &Client, device_id: &str) -> Vec<Subscription> {
        let path = format!("{}/notifications", device_path(device_id));
        let mut subs = Vec::new();
        for name in ["notificationPosted", "notificationUpdated"] {
            let weak = Rc::downgrade(self);
            subs.push(signals::subscribe(client.bus(), &path, IFACE_NOTIFICATIONS, name, move |p| {
                let (Some(this), Some((id,))) = (weak.upgrade(), p.get::<(String,)>()) else { return };
                this.fetch_one(id);
            }));
        }
        let weak = Rc::downgrade(self);
        subs.push(signals::subscribe(client.bus(), &path, IFACE_NOTIFICATIONS, "notificationRemoved", move |p| {
            let (Some(this), Some((id,))) = (weak.upgrade(), p.get::<(String,)>()) else { return };
            this.emit(NoteEvent::Removed(&id));
        }));
        let weak = Rc::downgrade(self);
        subs.push(signals::subscribe(client.bus(), &path, IFACE_NOTIFICATIONS, "allNotificationsRemoved", move |_| {
            if let Some(this) = weak.upgrade() {
                this.emit(NoteEvent::Cleared);
            }
        }));
        subs
    }

    /// Re-read every active notification from the daemon.
    pub fn reload(self: &Rc<Self>) {
        let Some((client, id, generation)) = self.current() else { return };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let all = client.fetch_active_notifications(&id).await;
            let Some(this) = weak.upgrade() else { return };
            if this.generation.get() == generation {
                this.emit(NoteEvent::Loaded(&all));
            }
        });
    }

    fn fetch_one(self: &Rc<Self>, notif_id: String) {
        let Some((client, id, generation)) = self.current() else { return };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let props = client.notification_properties(&id, &notif_id).await;
            let Some(this) = weak.upgrade() else { return };
            if this.generation.get() == generation && !props.is_empty() {
                this.emit(NoteEvent::Posted(&notif_id, &props));
            }
        });
    }

    /// Dismiss on the phone; it leaves the list right away.
    pub fn dismiss(self: &Rc<Self>, notif_ids: Vec<String>) {
        let Some((client, id, _)) = self.current() else { return };
        for n in &notif_ids {
            self.emit(NoteEvent::Removed(n));
        }
        glib::spawn_future_local(async move {
            for n in notif_ids {
                client.dismiss_notification(&id, &n).await;
            }
        });
    }

    /// Answer a notification through its own `sendReply`. `done(true)` once
    /// the daemon accepted it.
    pub fn reply(&self, notif_id: &str, text: &str, done: impl FnOnce(bool) + 'static) {
        let Some((client, id, _)) = self.current() else {
            done(false);
            return;
        };
        let (notif_id, text) = (notif_id.to_string(), text.to_string());
        glib::spawn_future_local(async move {
            done(client.reply_to_notification(&id, &notif_id, &text).await);
        });
    }
}
