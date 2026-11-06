//! The settings panel (port of `phonelink/ui/settings_dialog.py`): an
//! in-window page with a category list (General, Google Contacts,
//! Appearance, Notifications) and Back. Same `settings.json` keys as Python.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use super::google_sync::GoogleSync;
use super::icons;
use crate::core::google;
use crate::core::settings::Settings;

/// Phase 7 switch-over (Ethan, 2026-11-06): this app owns the autostart
/// entry and replaces the Python one.
pub const AUTOSTART_WIRED: bool = true;

/// The Exec for the autostart entry: the installed binary
/// (`data/install-rust.sh` puts it in ~/.local/bin) when there is one, else
/// whatever is running, so a test copy elsewhere doesn't get pinned.
pub fn autostart_exec() -> String {
    let installed = crate::core::settings::home_dir().join(".local/bin/phonelink-rs");
    let path = if installed.is_file() { Some(installed) } else { std::env::current_exe().ok() };
    path.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn apply_color_scheme(scheme: &str) {
    let scheme = match scheme {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::Default,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

struct GoogleRows {
    account: adw::ActionRow,
    connect: gtk::Button,
    refresh: gtk::Button,
    disconnect: gtk::Button,
    background: adw::SwitchRow,
}

pub struct SettingsPage {
    pub widget: gtk::Box,
    settings: Rc<RefCell<Settings>>,
    google: Rc<GoogleSync>,
    google_rows: GoogleRows,
    ignored_group: adw::PreferencesGroup,
    ignored_rows: RefCell<Vec<adw::ActionRow>>,
    /// Notification settings changed (the sidebar re-filters).
    on_notifications_changed: Rc<dyn Fn()>,
}

fn page_with(groups: &[&adw::PreferencesGroup]) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    for g in groups {
        page.add(*g);
    }
    page
}

fn group(title: &str) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::new();
    g.set_title(title);
    g
}

fn switch_row(title: &str, subtitle: &str, active: bool) -> adw::SwitchRow {
    let r = adw::SwitchRow::new();
    r.set_title(title);
    r.set_subtitle(subtitle);
    r.set_active(active);
    r
}

impl SettingsPage {
    pub fn new(
        settings: Rc<RefCell<Settings>>,
        google: Rc<GoogleSync>,
        on_back: impl Fn() + 'static,
        on_notifications_changed: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 0);

        // Left: Back, "Settings", categories.
        let left = gtk::Box::new(gtk::Orientation::Vertical, 0);
        left.set_size_request(220, -1);
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.set_margin_top(10);
        bar.set_margin_start(8);
        bar.set_margin_bottom(8);
        bar.set_margin_end(8);
        let back = gtk::Button::from_icon_name("go-previous-symbolic");
        back.add_css_class("flat");
        back.set_tooltip_text(Some("Back"));
        back.connect_clicked(move |_| on_back());
        bar.append(&back);
        let title = gtk::Label::new(Some("Settings"));
        title.add_css_class("heading");
        title.set_xalign(0.0);
        bar.append(&title);
        left.append(&bar);
        left.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let categories = gtk::ListBox::new();
        categories.set_selection_mode(gtk::SelectionMode::Single);
        categories.add_css_class("navigation-sidebar");
        categories.set_vexpand(true);
        let keys = ["general", "google", "appearance", "notifications"];
        for (icon, label) in [
            ("emblem-system-symbolic", "General"),
            ("avatar-default-symbolic", "Google Contacts"),
            ("display-brightness-symbolic", "Appearance"),
            ("xsi-notifications-symbolic", "Notifications"),
        ] {
            let row = gtk::ListBoxRow::new();
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            b.set_margin_start(12);
            b.set_margin_end(12);
            b.set_margin_top(9);
            b.set_margin_bottom(9);
            let img = gtk::Image::from_icon_name(&icons::resolve(icon));
            img.set_pixel_size(16);
            b.append(&img);
            let l = gtk::Label::new(Some(label));
            l.set_xalign(0.0);
            b.append(&l);
            row.set_child(Some(&b));
            categories.append(&row);
        }
        left.append(&categories);
        widget.append(&left);
        widget.append(&gtk::Separator::new(gtk::Orientation::Vertical));

        let content = gtk::Stack::new();
        content.set_hexpand(true);
        content.set_vexpand(true);
        content.set_transition_type(gtk::StackTransitionType::Crossfade);
        widget.append(&content);

        // General.
        let startup = group("Startup");
        let open_on_login = switch_row(
            "Open on Login",
            "Automatically launch PhoneRelay when you sign in",
            settings.borrow().open_on_startup(),
        );
        if !AUTOSTART_WIRED {
            open_on_login.set_sensitive(false);
            open_on_login.set_subtitle("The old Python Phone Link opens at login. This moves to the new version when you switch over.");
        }
        startup.add(&open_on_login);
        content.add_named(&page_with(&[&startup]), Some("general"));

        // Google Contacts.
        let account_group = group("Account");
        let account = adw::ActionRow::new();
        account.set_title("Google Contacts");
        let connect = gtk::Button::with_label("Connect");
        connect.add_css_class("suggested-action");
        connect.set_valign(gtk::Align::Center);
        let refresh = gtk::Button::with_label("Refresh Now");
        refresh.set_valign(gtk::Align::Center);
        let disconnect = gtk::Button::with_label("Disconnect");
        disconnect.add_css_class("destructive-action");
        disconnect.set_valign(gtk::Align::Center);
        account.add_suffix(&connect);
        account.add_suffix(&refresh);
        account.add_suffix(&disconnect);
        account_group.add(&account);
        let background = switch_row(
            "Background Refresh",
            "Refresh Google contacts at most once every 24 hours using the saved token.",
            settings.borrow().google_background_sync(),
        );
        account_group.add(&background);
        let info = group("How It Works");
        for (t, sub) in [
            (
                "Low-volume sync",
                "PhoneRelay only runs automatic Google refresh when a saved account exists and the last attempt is older than one day.",
            ),
            (
                "Contact photos",
                "Google photos are cached locally for the contacts already visible in your current conversations.",
            ),
        ] {
            let r = adw::ActionRow::new();
            r.set_title(t);
            r.set_subtitle(sub);
            info.add(&r);
        }
        content.add_named(&page_with(&[&account_group, &info]), Some("google"));

        // Appearance.
        let theme = group("Theme");
        let scheme = adw::ComboRow::new();
        scheme.set_title("Color Scheme");
        scheme.set_model(Some(&gtk::StringList::new(&["Follow system", "Light", "Dark"])));
        scheme.set_selected(match settings.borrow().color_scheme() {
            "light" => 1,
            "dark" => 2,
            _ => 0,
        });
        theme.add(&scheme);
        content.add_named(&page_with(&[&theme]), Some("appearance"));

        // Notifications.
        let sync_group = group("Notification Sync");
        let enabled = switch_row(
            "Show Phone Notifications",
            "Display your Android notifications in the tray",
            settings.borrow().notifications_enabled(),
        );
        sync_group.add(&enabled);
        let ignored_group = group("Hidden Apps");
        ignored_group.set_description(Some("Notifications from these apps will not appear in the tray."));
        let add_row = adw::EntryRow::new();
        add_row.set_title("Hide notifications from app…");
        add_row.set_show_apply_button(true);
        ignored_group.add(&add_row);
        content.add_named(&page_with(&[&sync_group, &ignored_group]), Some("notifications"));

        let this = Rc::new(Self {
            widget,
            settings: settings.clone(),
            google: google.clone(),
            google_rows: GoogleRows { account, connect: connect.clone(), refresh: refresh.clone(), disconnect: disconnect.clone(), background: background.clone() },
            ignored_group,
            ignored_rows: RefCell::default(),
            on_notifications_changed,
        });

        categories.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                content.set_visible_child_name(keys[row.index().max(0) as usize]);
            }
        });
        if let Some(first) = categories.row_at_index(0) {
            categories.select_row(Some(&first));
        }
        let s = settings.clone();
        open_on_login.connect_active_notify(move |r| {
            if AUTOSTART_WIRED
                && let Err(e) = s.borrow_mut().set_open_on_startup(r.is_active(), &autostart_exec())
            {
                log::warn!("autostart: {e}");
            }
        });
        let s = settings.clone();
        scheme.connect_selected_notify(move |c| {
            let value = ["system", "light", "dark"][(c.selected() as usize).min(2)];
            let _ = s.borrow_mut().set_color_scheme(value);
            apply_color_scheme(value);
        });
        let weak = Rc::downgrade(&this);
        enabled.connect_active_notify(move |r| {
            if let Some(this) = weak.upgrade() {
                let _ = this.settings.borrow_mut().set_notifications_enabled(r.is_active());
                (this.on_notifications_changed)();
            }
        });
        let weak = Rc::downgrade(&this);
        add_row.connect_apply(move |entry| {
            let Some(this) = weak.upgrade() else { return };
            let name = entry.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            let _ = this.settings.borrow_mut().add_ignored_app(&name);
            entry.set_text("");
            this.rebuild_ignored();
            (this.on_notifications_changed)();
        });
        let s = settings.clone();
        background.connect_active_notify(move |r| {
            if s.borrow().google_background_sync() != r.is_active() {
                let _ = s.borrow_mut().set_google_background_sync(r.is_active());
            }
        });
        for b in [&connect, &refresh] {
            let g = google.clone();
            b.connect_clicked(move |_| g.sync_now());
        }
        let g = google.clone();
        disconnect.connect_clicked(move |_| g.disconnect());
        let weak = Rc::downgrade(&this);
        google.connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.refresh_google_status();
            }
        });
        this.rebuild_ignored();
        this.refresh_google_status();
        this
    }

    /// Called each time the panel opens, as Python.
    pub fn refresh_google_status(&self) {
        let st = self.google.status();
        let label = if st.account_label.is_empty() { "Google account".to_string() } else { st.account_label.clone() };
        let subtitle = if st.in_flight {
            "Syncing Google Contacts now…".to_string()
        } else if st.connected {
            let mut s = format!("Connected as {label}.");
            if st.last_sync_ts > 0.0 {
                s += &format!(" Last synced {}.", google::last_sync_label(st.last_sync_ts, chrono::Local::now()));
            }
            s
        } else if st.configured {
            "Not connected. Authorize Google Contacts to import names and photos.".into()
        } else {
            format!("Google OAuth client file not found at {}.", st.config_path)
        };
        let r = &self.google_rows;
        r.account.set_subtitle(&subtitle);
        r.background.set_active(self.settings.borrow().google_background_sync());
        r.connect.set_visible(!st.connected);
        r.refresh.set_visible(st.connected);
        r.disconnect.set_visible(st.connected);
        for b in [&r.connect, &r.refresh, &r.disconnect] {
            b.set_sensitive(!st.in_flight);
        }
    }

    fn rebuild_ignored(self: &Rc<Self>) {
        for r in self.ignored_rows.borrow_mut().drain(..) {
            self.ignored_group.remove(&r);
        }
        let apps = self.settings.borrow().notifications_ignored_apps();
        for app in apps {
            let row = adw::ActionRow::new();
            row.set_title(&app);
            let del = gtk::Button::from_icon_name("edit-delete-symbolic");
            del.add_css_class("flat");
            del.add_css_class("destructive-action");
            del.set_valign(gtk::Align::Center);
            let weak = Rc::downgrade(self);
            del.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else { return };
                let _ = this.settings.borrow_mut().remove_ignored_app(&app);
                this.rebuild_ignored();
                (this.on_notifications_changed)();
            });
            row.add_suffix(&del);
            self.ignored_group.add(&row);
            self.ignored_rows.borrow_mut().push(row);
        }
    }
}
