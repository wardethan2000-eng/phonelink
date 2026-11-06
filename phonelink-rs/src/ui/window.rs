//! Main window: header (device switcher, tabs, battery, Find Phone,
//! notifications, settings), the tabs, the notifications sidebar and the
//! window-wide toasts. Port of `phonelink/ui/main_window.py`; the settings
//! panel comes in Phase 6.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use super::clipboard_page::ClipboardPage;
use super::files_page::FilesPage;
use super::google_sync::GoogleSync;
use super::icons;
use super::messages_page::MessagesPage;
use super::notifications_model::NotificationsModel;
use super::notifications_panel::NotificationsPanel;
use super::settings_page::{self, SettingsPage};
use super::tray::{self, Tray};
use crate::core::models::Device;
use crate::core::settings::Settings;
use crate::core::shell::{self, RefreshGate};
use crate::core::store::MessageStore;
use crate::core::sync;
use crate::dbus::signals::{self, NameWatch, Subscription};
use crate::dbus::{Client, DAEMON_PATH, IFACE_BATTERY, IFACE_DAEMON, IFACE_DEVICE, IFACE_SHARE, device_path};

struct Header {
    device_btn: gtk::MenuButton,
    device_icon: gtk::Image,
    device_name: gtk::Label,
    status_dot: gtk::Label,
    status_text: gtk::Label,
    device_arrow: gtk::Image,
    device_popover: gtk::Popover,
    device_list: gtk::ListBox,
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
    toasts: adw::ToastOverlay,
    notif_toggle: gtk::ToggleButton,
    messages: Rc<MessagesPage>,
    files: Rc<FilesPage>,
    clipboard: Rc<ClipboardPage>,
    notif_panel: Rc<NotificationsPanel>,
    notifications: Rc<NotificationsModel>,
    settings_page: Rc<SettingsPage>,
    google: Rc<GoogleSync>,
    /// The tray icon, when a StatusNotifierWatcher took it. Without one,
    /// closing the window quits (else the window would be stranded).
    tray: RefCell<Option<Tray>>,
    tray_shown: Cell<bool>,
    quitting: Cell<bool>,
    client: RefCell<Option<Client>>,
    device: RefCell<Option<Device>>,
    devices: RefCell<Vec<Device>>,
    /// (row, device id) for the switcher popover.
    device_rows: RefCell<Vec<(gtk::ListBoxRow, String)>>,
    refresh: RefCell<RefreshGate>,
    daemon_up: Cell<bool>,
    connecting: Cell<bool>,
    device_subs: RefCell<Vec<Subscription>>,
    daemon_subs: RefCell<Vec<Subscription>>,
    sleep_sub: RefCell<Option<Subscription>>,
    watch: RefCell<Option<NameWatch>>,
}

impl Window {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        icons::register_app_icon();
        let mut settings = Settings::new();
        settings.load();
        settings_page::apply_color_scheme(settings.color_scheme());
        // Phase 7: this app owns "Open on Login". If either app's entry is
        // there, point it at this binary and drop the Python one.
        if super::settings_page::AUTOSTART_WIRED
            && let Err(e) = settings.sync_autostart_state(&super::settings_page::autostart_exec())
        {
            log::warn!("autostart: {e}");
        }
        let settings = Rc::new(RefCell::new(settings));

        let window = adw::ApplicationWindow::new(app);
        window.set_title(Some("Phone Link"));
        window.set_default_size(960, 640);
        window.set_size_request(640, 420);

        let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header_bar = adw::HeaderBar::new();
        // KDE's layout starts with "icon:", a tiny app icon beside the switcher.
        header_bar.set_decoration_layout(Some(":minimize,maximize,close"));

        // Left: device identity, a switcher when more than one is paired.
        let device_btn = gtk::MenuButton::new();
        device_btn.add_css_class("flat");
        let device_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let device_icon = gtk::Image::new();
        device_icon.set_pixel_size(icons::device_icon_size(24));
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
        let device_arrow = gtk::Image::from_icon_name("pan-down-symbolic");
        device_arrow.set_pixel_size(12);
        device_arrow.set_visible(false);
        device_box.append(&device_arrow);
        device_btn.set_child(Some(&device_box));
        let device_popover = gtk::Popover::new();
        let device_list = gtk::ListBox::new();
        device_list.set_selection_mode(gtk::SelectionMode::None);
        device_list.add_css_class("boxed-list");
        device_popover.set_child(Some(&device_list));
        device_btn.set_popover(Some(&device_popover));
        device_btn.set_sensitive(false);
        header_bar.pack_start(&device_btn);

        // Center: tabs.
        let stack = adw::ViewStack::new();
        let switcher = adw::ViewSwitcher::new();
        switcher.set_stack(Some(&stack));
        switcher.set_policy(adw::ViewSwitcherPolicy::Wide);
        header_bar.set_title_widget(Some(&switcher));

        // Right, from the edge in: battery, Find Phone, notifications, settings.
        let right = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let battery_icon = gtk::Image::new();
        battery_icon.set_pixel_size(16);
        right.append(&battery_icon);
        let battery_label = gtk::Label::new(None);
        battery_label.add_css_class("caption");
        right.append(&battery_label);
        header_bar.pack_end(&right);
        let find_btn = gtk::Button::from_icon_name("find-location-symbolic");
        find_btn.set_tooltip_text(Some("Find Phone — makes your phone ring via KDE Connect"));
        header_bar.pack_end(&find_btn);
        let notif_toggle = gtk::ToggleButton::new();
        notif_toggle.set_icon_name(&icons::resolve("xsi-notifications-symbolic"));
        notif_toggle.set_tooltip_text(Some("Notifications"));
        header_bar.pack_end(&notif_toggle);
        let settings_btn = gtk::Button::from_icon_name("emblem-system-symbolic");
        settings_btn.set_tooltip_text(Some("Preferences"));
        header_bar.pack_end(&settings_btn);
        outer.append(&header_bar);

        // Window-wide toasts (3 s, as Python).
        let toasts = adw::ToastOverlay::new();
        let toast: Rc<dyn Fn(&str)> = {
            let toasts = toasts.downgrade();
            Rc::new(move |text: &str| {
                if let Some(t) = toasts.upgrade() {
                    let toast = adw::Toast::new(text);
                    toast.set_timeout(3);
                    t.add_toast(toast);
                }
            })
        };

        let notifications = NotificationsModel::new();
        let messages = MessagesPage::new(MessageStore::open_default(), settings.clone(), notifications.clone());
        messages.use_toasts(&toasts);
        let page = stack.add_titled(&messages.widget, Some("sms"), "Messages");
        page.set_icon_name(Some("mail-unread-symbolic"));
        let files = FilesPage::new();
        let page = stack.add_titled(&files.widget, Some("files"), "Files");
        page.set_icon_name(Some("folder-symbolic"));
        let clipboard = ClipboardPage::new(toast.clone());
        let page = stack.add_titled(&clipboard.widget, Some("clipboard"), "Clipboard");
        page.set_icon_name(Some("edit-paste-symbolic"));
        stack.set_vexpand(true);
        stack.set_hexpand(true);

        // Notifications sidebar: overlays the tabs from the end, hidden at first.
        let notif_panel = NotificationsPanel::new(notifications.clone(), settings.clone(), toast.clone());
        let split = adw::OverlaySplitView::new();
        split.set_sidebar_position(gtk::PackType::End);
        split.set_sidebar(Some(&notif_panel.widget));
        split.set_content(Some(&stack));
        split.set_show_sidebar(false);
        split.set_vexpand(true);
        notif_toggle.bind_property("active", &split, "show-sidebar").bidirectional().build();

        // Body: the tabs, the settings panel, or a full-window status page
        // when the daemon is missing.
        let body = gtk::Stack::new();
        body.set_vexpand(true);
        body.set_transition_type(gtk::StackTransitionType::Crossfade);
        body.add_named(&split, Some("main"));
        let google = GoogleSync::new(settings.clone(), window.upcast_ref(), toast.clone());
        google.set_messages(&messages);
        messages.set_google(google.clone());
        let settings_page = {
            let body = body.downgrade();
            let panel = Rc::downgrade(&notif_panel);
            SettingsPage::new(
                settings.clone(),
                google.clone(),
                move || {
                    if let Some(b) = body.upgrade() {
                        b.set_visible_child_name("main");
                    }
                },
                Rc::new(move || {
                    if let Some(p) = panel.upgrade() {
                        p.refilter();
                    }
                }),
            )
        };
        body.add_named(&settings_page.widget, Some("settings"));
        body.add_named(&adw::StatusPage::new(), Some("status"));
        toasts.set_child(Some(&body));
        toasts.set_vexpand(true);
        outer.append(&toasts);
        window.set_content(Some(&outer));

        let header = Header {
            device_btn,
            device_icon,
            device_name,
            status_dot,
            status_text,
            device_arrow,
            device_popover,
            device_list,
            battery_icon,
            battery_label,
        };
        header.show(None);
        let this = Rc::new(Self {
            window,
            header,
            body,
            toasts,
            notif_toggle,
            messages,
            files,
            clipboard,
            notif_panel,
            notifications,
            settings_page,
            google,
            tray: RefCell::default(),
            tray_shown: Cell::new(false),
            quitting: Cell::new(false),
            client: RefCell::default(),
            device: RefCell::default(),
            devices: RefCell::default(),
            device_rows: RefCell::default(),
            refresh: RefCell::default(),
            daemon_up: Cell::new(false),
            connecting: Cell::new(false),
            device_subs: RefCell::default(),
            daemon_subs: RefCell::default(),
            sleep_sub: RefCell::default(),
            watch: RefCell::default(),
        });

        let weak = Rc::downgrade(&this);
        find_btn.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.ring();
            }
        });
        let weak = Rc::downgrade(&this);
        settings_btn.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.settings_page.refresh_google_status();
                this.body.set_visible_child_name("settings");
            }
        });
        let weak = Rc::downgrade(&this);
        this.header.device_list.connect_row_activated(move |_, row| {
            if let Some(this) = weak.upgrade() {
                this.device_row_chosen(row);
            }
        });
        this.add_file_drop(&stack);
        this.add_shortcuts();
        let weak = Rc::downgrade(&this);
        this.window.connect_visible_notify(move |w| {
            if let Some(this) = weak.upgrade()
                && w.is_visible()
            {
                this.pull_from_phone(sync::CATCH_UP_ON_SHOW_INTERVAL_SECONDS);
            }
        });
        // Safety net in case a signal is ever missed; events do the real work.
        let weak = Rc::downgrade(&this);
        glib::timeout_add_seconds_local(shell::WATCHDOG_SECONDS, move || {
            let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break };
            if this.client.borrow().is_none() {
                this.start();
            } else if this.daemon_up.get() {
                this.refresh_devices();
            }
            glib::ControlFlow::Continue
        });
        this.start_tray(app);
        // Close hides to the tray; without a tray (or from the tray's Quit) it quits.
        let weak = Rc::downgrade(&this);
        this.window.connect_close_request(move |w| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if this.tray_shown.get() && !this.quitting.get() {
                w.set_visible(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        this.start();
        this
    }

    fn start_tray(self: &Rc<Self>, app: &adw::Application) {
        let Some(conn) = app.dbus_connection() else { return };
        let weak = Rc::downgrade(self);
        let toggle: Rc<dyn Fn()> = Rc::new(move || {
            let Some(this) = weak.upgrade() else { return };
            if this.window.is_visible() {
                this.window.set_visible(false);
            } else {
                this.window.present();
            }
        });
        let (weak, app_weak) = (Rc::downgrade(self), app.downgrade());
        let quit: Rc<dyn Fn()> = Rc::new(move || {
            if let Some(this) = weak.upgrade() {
                this.quitting.set(true);
                this.tray.borrow_mut().take();
            }
            if let Some(app) = app_weak.upgrade() {
                app.quit();
            }
        });
        let weak = Rc::downgrade(self);
        let shown: Rc<dyn Fn(bool)> = Rc::new(move |on| {
            if let Some(this) = weak.upgrade() {
                this.tray_shown.set(on);
            }
        });
        *self.tray.borrow_mut() = tray::start(&conn, toggle, quit, shown);
    }

    fn toast(&self, text: &str) {
        let toast = adw::Toast::new(text);
        toast.set_timeout(3);
        self.toasts.add_toast(toast);
    }

    fn show_status(&self, icon: &str, title: &str, description: &str) {
        let Some(page) = self.body.child_by_name("status").and_downcast::<adw::StatusPage>() else { return };
        page.set_icon_name(Some(&icons::resolve(icon)));
        page.set_title(title);
        page.set_description(Some(description));
        self.body.set_visible_child_name("status");
    }

    /// Connect to the session bus and start watching the daemon. Retried by
    /// the watchdog when the bus wasn't reachable.
    fn start(self: &Rc<Self>) {
        if self.connecting.replace(true) {
            return;
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let client = match Client::connect().await {
                Ok(c) => c,
                Err(e) => {
                    this.connecting.set(false);
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

            // React the moment the daemon starts or stops. When it isn't
            // running at startup, `vanished` fires right away.
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

            // Texts that reached the phone while the laptop slept were never
            // pushed, so pull after every resume.
            let weak = Rc::downgrade(&this);
            match signals::watch_sleep(move |going_to_sleep| {
                if going_to_sleep {
                    return;
                }
                let weak = weak.clone();
                glib::timeout_add_seconds_local_once(shell::RESUME_PULL_DELAY_SECONDS, move || {
                    if let Some(this) = weak.upgrade() {
                        this.pull_from_phone(sync::CATCH_UP_MIN_INTERVAL_SECONDS);
                    }
                });
            })
            .await
            {
                Ok(sub) => *this.sleep_sub.borrow_mut() = Some(sub),
                Err(e) => log::warn!("can't watch for resume from sleep: {}", e.message()),
            }
        });
    }

    fn daemon_appeared(self: &Rc<Self>) {
        self.daemon_up.set(true);
        self.body.set_visible_child_name("main");
        self.refresh_devices();
    }

    fn daemon_vanished(self: &Rc<Self>) {
        self.daemon_up.set(false);
        self.devices.borrow_mut().clear();
        if let Some(client) = self.client.borrow().clone() {
            self.set_device(&client, None);
        }
        self.update_switcher();
        self.show_status(
            "dialog-warning-symbolic",
            "Finish setting up your phone",
            "Phone Link uses KDE Connect to talk to your phone, and it isn't running yet.\n\n\
             Install it:\n  sudo apt install kdeconnect\n\n\
             Then open KDE Connect and pair your phone — Phone Link picks it up automatically.",
        );
    }

    /// Re-read every device. A burst of requests runs one refresh at a time,
    /// with at most one queued behind it.
    fn refresh_devices(self: &Rc<Self>) {
        if self.refresh.borrow_mut().request() {
            self.run_refresh();
        }
    }

    fn run_refresh(self: &Rc<Self>) {
        let Some(client) = self.client.borrow().clone() else {
            self.refresh.borrow_mut().finish();
            return;
        };
        let this = self.clone();
        glib::spawn_future_local(async move {
            let devices = client.fetch_devices().await;
            let current = this.device.borrow().as_ref().map(|d| d.id.clone());
            let pick = shell::pick_active_device(&devices, current.as_deref()).cloned();
            *this.devices.borrow_mut() = devices;
            this.set_device(&client, pick);
            this.update_switcher();
            if this.refresh.borrow_mut().finish() {
                this.run_refresh();
            }
        });
    }

    fn set_device(self: &Rc<Self>, client: &Client, dev: Option<Device>) {
        let (changed, became_reachable) = {
            let old = self.device.borrow();
            let changed = old.as_ref().map(|d| &d.id) != dev.as_ref().map(|d| &d.id);
            let was_reachable = old.as_ref().is_some_and(|d| d.reachable);
            (changed, !changed && !was_reachable && dev.as_ref().is_some_and(|d| d.reachable))
        };
        self.header.show(dev.as_ref());
        if changed {
            self.subscribe_device(client, dev.as_ref());
        }
        *self.device.borrow_mut() = dev.clone();
        // Messages first: it resets for a new device before the
        // notifications for that device arrive.
        self.messages.set_device(client, dev.as_ref());
        self.notifications.set_device(client, dev.as_ref(), became_reachable);
        self.notif_panel.set_device(dev.as_ref());
        if dev.as_ref().is_some_and(|d| d.reachable) && (changed || became_reachable) {
            self.google.maybe_background();
        }
        self.files.set_device(client, dev.as_ref());
        self.clipboard.set_device(client, dev.as_ref());
    }

    /// Paired devices in the popover; the button works only with two or more.
    fn update_switcher(self: &Rc<Self>) {
        let devices = self.devices.borrow();
        let paired = shell::switchable_devices(&devices);
        let multi = paired.len() > 1;
        self.header.device_arrow.set_visible(multi);
        self.header.device_btn.set_sensitive(multi);
        super::clear_list_box(&self.header.device_list);
        let active = self.device.borrow().as_ref().map(|d| d.id.clone());
        let mut rows = Vec::new();
        for dev in paired {
            let row = gtk::ListBoxRow::new();
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            b.set_margin_start(10);
            b.set_margin_end(10);
            b.set_margin_top(6);
            b.set_margin_bottom(6);
            let icon = gtk::Image::from_icon_name(dev.type_icon_name());
            icon.set_pixel_size(icons::device_icon_size(20));
            b.append(&icon);
            let names = gtk::Box::new(gtk::Orientation::Vertical, 2);
            names.set_hexpand(true);
            let name = gtk::Label::new(Some(&dev.name));
            name.set_xalign(0.0);
            name.add_css_class("heading");
            names.append(&name);
            let status = gtk::Label::new(Some(dev.status_label()));
            status.set_xalign(0.0);
            status.add_css_class("caption");
            status.add_css_class("dim-label");
            names.append(&status);
            b.append(&names);
            if active.as_deref() == Some(dev.id.as_str()) {
                let check = gtk::Image::from_icon_name("object-select-symbolic");
                check.set_pixel_size(16);
                b.append(&check);
            }
            row.set_child(Some(&b));
            self.header.device_list.append(&row);
            rows.push((row, dev.id.clone()));
        }
        *self.device_rows.borrow_mut() = rows;
    }

    fn device_row_chosen(self: &Rc<Self>, row: &gtk::ListBoxRow) {
        self.header.device_popover.popdown();
        let Some(id) = self.device_rows.borrow().iter().find(|(r, _)| r == row).map(|(_, id)| id.clone()) else {
            return;
        };
        if self.device.borrow().as_ref().is_some_and(|d| d.id == id) {
            return;
        }
        let target = self.devices.borrow().iter().find(|d| d.id == id).cloned();
        let Some(client) = self.client.borrow().clone() else { return };
        if let Some(target) = target {
            self.set_device(&client, Some(target));
            self.update_switcher();
        }
    }

    /// Reachable, battery and share signals for the active device.
    fn subscribe_device(self: &Rc<Self>, client: &Client, dev: Option<&Device>) {
        let mut subs = Vec::new();
        if let Some(dev) = dev {
            let base = device_path(&dev.id);
            let weak = Rc::downgrade(self);
            subs.push(signals::subscribe(client.bus(), &base, IFACE_DEVICE, "reachableChanged", move |p| {
                let Some(this) = weak.upgrade() else { return };
                this.refresh_devices();
                // A quick drop and return can look like "still reachable" to
                // the refresh, so act on the signal itself: a new connection
                // means the phone forgot what it didn't send over the old one.
                if p.get::<(bool,)>().is_some_and(|(r,)| r) {
                    this.pull_from_phone(sync::CATCH_UP_MIN_INTERVAL_SECONDS);
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
            // Continue on PC: links shared from the phone open here.
            let weak = Rc::downgrade(self);
            let share = format!("{base}/share");
            subs.push(signals::subscribe(client.bus(), &share, IFACE_SHARE, "shareReceived", move |p| {
                let (Some(this), Some((url,))) = (weak.upgrade(), p.get::<(String,)>()) else { return };
                this.files.share_received(&url);
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return;
                }
                match gio::AppInfo::launch_default_for_uri(&url, gio::AppLaunchContext::NONE) {
                    Ok(()) => this.toast("Opened link from phone"),
                    Err(e) => log::warn!("open link from phone: {e}"),
                }
            }));
        }
        *self.device_subs.borrow_mut() = subs;
    }

    /// Catch up on texts and photos the phone didn't push.
    fn pull_from_phone(&self, min_interval: f64) {
        self.messages.catch_up(min_interval);
        self.files.refresh_from_phone();
    }

    fn ring(&self) {
        let dev = self.device.borrow().as_ref().map(|d| d.id.clone());
        let (Some(id), Some(client)) = (dev, self.client.borrow().clone()) else {
            self.toast("No paired device found");
            return;
        };
        glib::spawn_future_local(async move { client.ring_device(&id).await });
        self.toast("Finding your phone…");
    }

    /// A file dropped anywhere on the tabs is sent to the phone.
    fn add_file_drop(self: &Rc<Self>, target: &impl IsA<gtk::Widget>) {
        let drop = gtk::DropTarget::new(gio::File::static_type(), gtk::gdk::DragAction::COPY);
        let weak = Rc::downgrade(self);
        drop.connect_drop(move |_, value, _, _| {
            let Some(this) = weak.upgrade() else { return false };
            let dev = this.device.borrow().clone().filter(|d| d.reachable);
            let (Some(dev), Some(client)) = (dev, this.client.borrow().clone()) else {
                this.toast("No connected device — cannot send file");
                return false;
            };
            let Some(path) = value.get::<gio::File>().ok().and_then(|f| f.path()) else { return false };
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            this.toast(&format!("Sending {name} to {}…", dev.name));
            let path = path.to_string_lossy().into_owned();
            glib::spawn_future_local(async move { client.share_file(&dev.id, &path).await });
            true
        });
        target.add_controller(drop);
    }

    /// Ctrl+N shows or hides the notifications. (Messages owns the zoom keys.)
    fn add_shortcuts(self: &Rc<Self>) {
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if state.contains(gtk::gdk::ModifierType::CONTROL_MASK) && key == gtk::gdk::Key::n {
                this.notif_toggle.set_active(!this.notif_toggle.is_active());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(keys);
    }
}
