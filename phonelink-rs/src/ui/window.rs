//! Main window: header (device, tabs, battery) and the tab stack. Port of
//! the Messages-tab parts of `phonelink/ui/main_window.py`; the other tabs,
//! notifications sidebar, ring button and settings come in Phases 4–6.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use super::messages_page::MessagesPage;
use crate::core::models::Device;
use crate::core::settings::Settings;
use crate::core::store::MessageStore;
use crate::dbus::signals::{self, NameWatch, Subscription};
use crate::dbus::{Client, DAEMON_PATH, IFACE_BATTERY, IFACE_DAEMON, IFACE_DEVICE, device_path};

struct Header {
    device_icon: gtk::Image,
    device_name: gtk::Label,
    status_dot: gtk::Label,
    status_text: gtk::Label,
    battery_icon: gtk::Image,
    battery_label: gtk::Label,
}

impl Header {
    fn show(&self, dev: Option<&Device>) {
        let Some(dev) = dev else {
            self.device_icon.set_icon_name(Some("phone-symbolic"));
            self.device_name.set_label("No phone linked");
            self.status_dot.set_markup("<span foreground=\"#77767b\">●</span>");
            self.status_text.set_label("Pair a phone to begin");
            self.battery_icon.set_visible(false);
            self.battery_label.set_visible(false);
            return;
        };
        self.device_icon.set_icon_name(Some(dev.type_icon_name()));
        self.device_name.set_label(&dev.name);
        let (color, text) = if dev.reachable {
            ("#2ec27e", "Connected")
        } else if dev.paired {
            ("#c64600", "Offline")
        } else {
            ("#77767b", "Not paired")
        };
        self.status_dot.set_markup(&format!("<span foreground=\"{color}\">●</span>"));
        self.status_text.set_label(text);
        let has_battery = dev.battery_charge >= 0;
        if has_battery {
            self.battery_icon.set_icon_name(Some(&dev.battery_icon_name()));
            self.battery_label.set_label(&dev.battery_label());
        }
        self.battery_icon.set_visible(has_battery);
        self.battery_label.set_visible(has_battery);
    }
}

pub struct Window {
    pub window: adw::ApplicationWindow,
    header: Header,
    body: gtk::Stack,
    messages: Rc<MessagesPage>,
    client: RefCell<Option<Client>>,
    device: RefCell<Option<Device>>,
    device_subs: RefCell<Vec<Subscription>>,
    daemon_subs: RefCell<Vec<Subscription>>,
    watch: RefCell<Option<NameWatch>>,
}

impl Window {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let window = adw::ApplicationWindow::new(app);
        window.set_title(Some("Phone Link"));
        window.set_default_size(960, 640);

        let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header_bar = adw::HeaderBar::new();
        header_bar.set_decoration_layout(Some(":minimize,maximize,close"));

        // Left: device identity.
        let device_btn = gtk::MenuButton::new();
        device_btn.add_css_class("flat");
        let device_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let device_icon = gtk::Image::new();
        device_icon.set_pixel_size(24);
        device_box.append(&device_icon);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_valign(gtk::Align::Center);
        let device_name = gtk::Label::new(None);
        device_name.add_css_class("heading");
        device_name.set_xalign(0.0);
        text.append(&device_name);
        let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        let status_dot = gtk::Label::new(None);
        status_row.append(&status_dot);
        let status_text = gtk::Label::new(None);
        status_text.set_xalign(0.0);
        status_text.add_css_class("caption");
        status_text.add_css_class("dim-label");
        status_row.append(&status_text);
        text.append(&status_row);
        device_box.append(&text);
        device_btn.set_child(Some(&device_box));
        // TODO(Phase 4): device switcher popover.
        header_bar.pack_start(&device_btn);

        // Center: tabs.
        let stack = adw::ViewStack::new();
        let switcher = adw::ViewSwitcher::new();
        switcher.set_stack(Some(&stack));
        switcher.set_policy(adw::ViewSwitcherPolicy::Wide);
        header_bar.set_title_widget(Some(&switcher));

        // Right: battery.
        let right = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let battery_icon = gtk::Image::new();
        battery_icon.set_pixel_size(16);
        right.append(&battery_icon);
        let battery_label = gtk::Label::new(None);
        battery_label.add_css_class("caption");
        right.append(&battery_label);
        header_bar.pack_end(&right);
        outer.append(&header_bar);

        let mut settings = Settings::new();
        settings.load();
        let messages = MessagesPage::new(MessageStore::open_default(), settings);
        let page = stack.add_titled(&messages.widget, Some("sms"), "Messages");
        page.set_icon_name(Some("mail-unread-symbolic"));
        stack.set_vexpand(true);

        // Body: the tabs, or a full-window status page when the daemon is
        // missing.
        let body = gtk::Stack::new();
        body.set_vexpand(true);
        body.set_transition_type(gtk::StackTransitionType::Crossfade);
        body.add_named(&stack, Some("main"));
        body.add_named(&adw::StatusPage::new(), Some("status"));
        outer.append(&body);
        window.set_content(Some(&outer));

        let header = Header { device_icon, device_name, status_dot, status_text, battery_icon, battery_label };
        header.show(None);
        let this = Rc::new(Self {
            window,
            header,
            body,
            messages,
            client: RefCell::default(),
            device: RefCell::default(),
            device_subs: RefCell::default(),
            daemon_subs: RefCell::default(),
            watch: RefCell::default(),
        });
        this.start();
        this
    }

    fn show_status(&self, icon: &str, title: &str, description: &str) {
        let Some(page) = self.body.child_by_name("status").and_downcast::<adw::StatusPage>() else { return };
        page.set_icon_name(Some(icon));
        page.set_title(title);
        page.set_description(Some(description));
        self.body.set_visible_child_name("status");
    }

    fn start(self: &Rc<Self>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            let client = match Client::connect().await {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("session bus: {}", e.message());
                    this.show_status(
                        "dialog-error-symbolic",
                        "Can't reach your desktop session",
                        "Phone Link couldn't connect to the service that links your phone.\n\
                         Sign in to a normal desktop session, then reopen Phone Link.",
                    );
                    return;
                }
            };
            *this.client.borrow_mut() = Some(client.clone());

            // React the moment the daemon starts or stops.
            let (up, down) = (Rc::downgrade(&this), Rc::downgrade(&this));
            *this.watch.borrow_mut() = Some(signals::watch_daemon(
                move || {
                    if let Some(this) = up.upgrade() {
                        this.daemon_appeared();
                    }
                },
                move || {
                    if let Some(this) = down.upgrade() {
                        this.daemon_vanished();
                    }
                },
            ));
            let weak = Rc::downgrade(&this);
            *this.daemon_subs.borrow_mut() =
                vec![signals::subscribe(client.bus(), DAEMON_PATH, IFACE_DAEMON, "deviceListChanged", move |_| {
                    if let Some(this) = weak.upgrade() {
                        this.refresh_devices();
                    }
                })];
        });
    }

    fn daemon_appeared(self: &Rc<Self>) {
        self.body.set_visible_child_name("main");
        self.refresh_devices();
    }

    fn daemon_vanished(self: &Rc<Self>) {
        self.show_status(
            "dialog-warning-symbolic",
            "Finish setting up your phone",
            "Phone Link uses KDE Connect to talk to your phone, and it isn't running yet.\n\n\
             Install it:\n  sudo apt install kdeconnect\n\n\
             Then open KDE Connect and pair your phone — Phone Link picks it up automatically.",
        );
    }

    /// Re-read every device and keep (or pick) the active one, as Python:
    /// the current one while it stays paired, else the first paired +
    /// reachable, else the first paired.
    fn refresh_devices(self: &Rc<Self>) {
        let Some(client) = self.client.borrow().clone() else { return };
        let this = self.clone();
        glib::spawn_future_local(async move {
            let devices = client.fetch_devices().await;
            let current = this.device.borrow().as_ref().map(|d| d.id.clone());
            let pick = current
                .and_then(|id| devices.iter().find(|d| d.id == id && d.paired))
                .or_else(|| devices.iter().find(|d| d.paired && d.reachable))
                .or_else(|| devices.iter().find(|d| d.paired))
                .cloned();
            this.set_device(&client, pick);
        });
    }

    fn set_device(self: &Rc<Self>, client: &Client, dev: Option<Device>) {
        let changed = self.device.borrow().as_ref().map(|d| &d.id) != dev.as_ref().map(|d| &d.id);
        self.header.show(dev.as_ref());
        if changed {
            self.subscribe_device_status(client, dev.as_ref());
        }
        self.messages.set_device(client, dev.as_ref());
        *self.device.borrow_mut() = dev;
    }

    /// Reachable + battery signals for the active device.
    fn subscribe_device_status(self: &Rc<Self>, client: &Client, dev: Option<&Device>) {
        let mut subs = Vec::new();
        if let Some(dev) = dev {
            let base = device_path(&dev.id);
            let weak = Rc::downgrade(self);
            subs.push(signals::subscribe(client.bus(), &base, IFACE_DEVICE, "reachableChanged", move |_| {
                if let Some(this) = weak.upgrade() {
                    this.refresh_devices();
                }
            }));
            let weak = Rc::downgrade(self);
            let battery = format!("{base}/battery");
            subs.push(signals::subscribe(client.bus(), &battery, IFACE_BATTERY, "refreshed", move |p| {
                let Some(this) = weak.upgrade() else { return };
                // refreshed(isCharging b, charge i)
                if let Some((charging, charge)) = p.get::<(bool, i32)>() {
                    let mut dev = this.device.borrow_mut();
                    if let Some(d) = dev.as_mut() {
                        d.battery_charging = charging;
                        d.battery_charge = charge;
                        this.header.show(Some(d));
                    }
                }
            }));
        }
        *self.device_subs.borrow_mut() = subs;
    }
}
