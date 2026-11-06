//! The Files tab (port of `phonelink/ui/files_panel.py`): mounts the phone's
//! storage through KDE Connect (sftp), shows the newest photos as a grid and
//! lets you browse folders. Every filesystem call runs off the main thread
//! (`gio::spawn_blocking`), because the FUSE mount can stall.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::gdk;

use super::icons;
use crate::core::files::{self, Listing, RefreshLimit, THUMB_SIZE};
use crate::core::models::Device;
use crate::dbus::signals::{self, Subscription};
use crate::dbus::{Client, IFACE_SFTP, device_path};

const PHOTOS: &str = "__photos__";

struct Tile {
    path: PathBuf,
    child: gtk::FlowBoxChild,
    stack: gtk::Stack,
    picture: gtk::Image,
    check: gtk::Image,
    selected: bool,
}

impl Tile {
    fn set_selected(&mut self, on: bool) {
        self.selected = on;
        self.check.set_visible(on);
        if on {
            self.child.add_css_class("photo-tile-selected");
        } else {
            self.child.remove_css_class("photo-tile-selected");
        }
    }
}

#[derive(Default)]
struct State {
    client: Option<Client>,
    device_id: Option<String>,
    reachable: bool,
    mounted: bool,
    mount_point: String,
    storage_base: String,
    directories: BTreeMap<String, String>,
    current_path: String,
    mounting: bool,
    /// Bumped by every new photo scan / directory listing; older results
    /// are dropped.
    photo_generation: u64,
    dir_generation: u64,
    tiles: Vec<Tile>,
    /// Sidebar row → location (a path, or [`PHOTOS`]).
    shortcuts: Vec<(gtk::ListBoxRow, String)>,
    /// Browse rows and what they stand for.
    entries: Vec<(gtk::ListBoxRow, files::Entry)>,
    refresh: RefreshLimit,
    subs: Vec<Subscription>,
}

pub struct FilesPage {
    pub widget: gtk::Stack,
    status: adw::StatusPage,
    spinner: gtk::Spinner,
    shortcuts_list: gtk::ListBox,
    view_stack: adw::ViewStack,
    photo_actions: gtk::Box,
    save_btn: gtk::Button,
    copy_btn: gtk::Button,
    photo_grid: gtk::FlowBox,
    photo_stack: gtk::Stack,
    photo_empty: adw::StatusPage,
    photo_status: gtk::Label,
    back_btn: gtk::Button,
    path_label: gtk::Label,
    file_list: gtk::ListBox,
    file_empty: adw::StatusPage,
    status_bar: gtk::Label,
    downloads: String,
    started: Instant,
    state: RefCell<State>,
}

fn caption(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_margin_start(12);
    l.set_margin_end(12);
    l.set_margin_top(4);
    l.set_margin_bottom(4);
    l.add_css_class("caption");
    l.add_css_class("dim-label");
    l
}

fn flat_icon(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("flat");
    b.set_tooltip_text(Some(tooltip));
    b
}

fn uri_of(path: &Path) -> String {
    gio::File::for_path(path).uri().to_string()
}

impl FilesPage {
    pub fn new() -> Rc<Self> {
        let widget = gtk::Stack::new();
        widget.set_vexpand(true);
        widget.set_hexpand(true);
        widget.set_transition_type(gtk::StackTransitionType::Crossfade);

        let status = adw::StatusPage::new();
        status.set_icon_name(Some("folder-symbolic"));
        status.set_title("Files");
        status.set_description(Some("No phone linked yet.\nPair your phone to browse its files here."));
        widget.add_named(&status, Some("status"));

        let mounting = gtk::Box::new(gtk::Orientation::Vertical, 16);
        mounting.set_valign(gtk::Align::Center);
        mounting.set_halign(gtk::Align::Center);
        let spinner = gtk::Spinner::new();
        spinner.set_size_request(48, 48);
        mounting.append(&spinner);
        let l = gtk::Label::new(Some("Mounting phone storage…"));
        l.add_css_class("title-3");
        l.add_css_class("dim-label");
        mounting.append(&l);
        let l = gtk::Label::new(Some("Grant file permission on your phone if prompted."));
        l.add_css_class("dim-label");
        l.add_css_class("caption");
        mounting.append(&l);
        widget.add_named(&mounting, Some("mounting"));

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        widget.add_named(&content, Some("content"));

        // Left: Locations and "Send File to Phone".
        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 4);
        sidebar.set_size_request(200, -1);
        sidebar.set_margin_top(8);
        sidebar.set_margin_bottom(8);
        let heading = gtk::Label::new(Some("Locations"));
        heading.add_css_class("heading");
        heading.set_xalign(0.0);
        heading.set_margin_start(12);
        heading.set_margin_top(4);
        heading.set_margin_bottom(4);
        sidebar.append(&heading);
        let shortcuts_list = gtk::ListBox::new();
        shortcuts_list.set_selection_mode(gtk::SelectionMode::Single);
        shortcuts_list.add_css_class("navigation-sidebar");
        sidebar.append(&shortcuts_list);
        sidebar.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let send_btn = gtk::Button::new();
        send_btn.add_css_class("flat");
        send_btn.set_margin_start(8);
        send_btn.set_margin_end(8);
        send_btn.set_margin_top(8);
        let send_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        send_row.append(&gtk::Image::from_icon_name("document-send-symbolic"));
        let send_label = gtk::Label::new(Some("Send File to Phone"));
        send_label.set_xalign(0.0);
        send_row.append(&send_label);
        send_btn.set_child(Some(&send_row));
        sidebar.append(&send_btn);
        content.append(&sidebar);
        content.append(&gtk::Separator::new(gtk::Orientation::Vertical));

        // Right: the photo grid or the file browser.
        let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
        right.set_hexpand(true);
        content.append(&right);
        let view_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        view_bar.set_margin_top(8);
        view_bar.set_margin_bottom(4);
        view_bar.set_margin_start(12);
        view_bar.set_margin_end(12);
        let view_stack = adw::ViewStack::new();
        let switcher = adw::ViewSwitcher::new();
        switcher.set_stack(Some(&view_stack));
        switcher.set_policy(adw::ViewSwitcherPolicy::Wide);
        view_bar.append(&switcher);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        view_bar.append(&spacer);
        let photo_actions = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let select_all = flat_icon("edit-select-all-symbolic", "Select all");
        let save_btn = flat_icon("document-save-symbolic", "Save selected to folder");
        save_btn.set_sensitive(false);
        let copy_btn = flat_icon("edit-copy-symbolic", "Copy selected to clipboard");
        copy_btn.set_sensitive(false);
        photo_actions.append(&select_all);
        photo_actions.append(&save_btn);
        photo_actions.append(&copy_btn);
        view_bar.append(&photo_actions);
        right.append(&view_bar);
        right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        view_stack.set_vexpand(true);
        right.append(&view_stack);

        // Photos.
        let photo_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let photo_scroll = gtk::ScrolledWindow::new();
        photo_scroll.set_vexpand(true);
        photo_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        let photo_grid = gtk::FlowBox::new();
        photo_grid.set_valign(gtk::Align::Start);
        photo_grid.set_max_children_per_line(20);
        photo_grid.set_min_children_per_line(2);
        photo_grid.set_column_spacing(4);
        photo_grid.set_row_spacing(4);
        photo_grid.set_margin_start(8);
        photo_grid.set_margin_end(8);
        photo_grid.set_margin_top(8);
        photo_grid.set_margin_bottom(8);
        photo_grid.set_homogeneous(false);
        photo_grid.set_selection_mode(gtk::SelectionMode::None);
        photo_scroll.set_child(Some(&photo_grid));
        photo_page.append(&photo_scroll);
        let photo_status = caption("");
        photo_page.append(&photo_status);
        let photo_empty = adw::StatusPage::new();
        photo_empty.set_icon_name(Some("camera-photo-symbolic"));
        photo_empty.set_title("No Photos");
        // Python also said to click a "Mount Phone" button that doesn't exist.
        photo_empty.set_description(Some("Connect your phone to see recent photos."));
        photo_empty.set_vexpand(true);
        let photo_stack = gtk::Stack::new();
        photo_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        photo_stack.add_named(&photo_empty, Some("empty"));
        photo_stack.add_named(&photo_page, Some("grid"));
        photo_stack.set_visible_child_name("empty");
        let p = view_stack.add_titled(&photo_stack, Some("photos"), "Recent Photos");
        p.set_icon_name(Some("camera-photo-symbolic"));

        // Browse.
        let file_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let path_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        path_bar.set_margin_top(8);
        path_bar.set_margin_bottom(4);
        path_bar.set_margin_start(12);
        path_bar.set_margin_end(12);
        let back_btn = flat_icon("go-previous-symbolic", "Go up");
        path_bar.append(&back_btn);
        let path_label = gtk::Label::new(Some("/"));
        path_label.set_xalign(0.0);
        path_label.set_hexpand(true);
        path_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        path_label.add_css_class("heading");
        path_bar.append(&path_label);
        let refresh_btn = flat_icon("view-refresh-symbolic", "Refresh");
        path_bar.append(&refresh_btn);
        file_page.append(&path_bar);
        file_page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        let file_list = gtk::ListBox::new();
        file_list.set_selection_mode(gtk::SelectionMode::Single);
        let file_empty = adw::StatusPage::new();
        file_empty.set_icon_name(Some("folder-open-symbolic"));
        file_empty.set_title("No Files");
        file_empty.set_description(Some("Mount your phone to browse its files."));
        file_list.set_placeholder(Some(&file_empty));
        scroll.set_child(Some(&file_list));
        file_page.append(&scroll);
        let status_bar = caption("");
        file_page.append(&status_bar);
        let p = view_stack.add_titled(&file_page, Some("browser"), "Browse Files");
        p.set_icon_name(Some("folder-symbolic"));

        widget.set_visible_child_name("status");
        let this = Rc::new(Self {
            widget,
            status,
            spinner,
            shortcuts_list,
            view_stack,
            photo_actions,
            save_btn,
            copy_btn,
            photo_grid,
            photo_stack,
            photo_empty,
            photo_status,
            back_btn,
            path_label,
            file_list,
            file_empty,
            status_bar,
            downloads: crate::core::settings::home_dir().join("Downloads").to_string_lossy().into_owned(),
            started: Instant::now(),
            state: RefCell::default(),
        });

        let weak = Rc::downgrade(&this);
        this.view_stack.connect_visible_child_name_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.photo_actions.set_visible(this.showing_photos());
            }
        });
        let weak = Rc::downgrade(&this);
        this.shortcuts_list.connect_row_selected(move |_, row| {
            if let (Some(this), Some(row)) = (weak.upgrade(), row) {
                this.shortcut_chosen(row);
            }
        });
        let weak = Rc::downgrade(&this);
        send_btn.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.send_files();
            }
        });
        let weak = Rc::downgrade(&this);
        select_all.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.select_all();
            }
        });
        let weak = Rc::downgrade(&this);
        this.save_btn.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let paths = this.selected_paths();
                if !paths.is_empty() {
                    this.save_dialog(paths, this.photo_status.clone());
                }
            }
        });
        let weak = Rc::downgrade(&this);
        this.copy_btn.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.copy_selected();
            }
        });
        let weak = Rc::downgrade(&this);
        this.back_btn.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.go_up();
            }
        });
        let weak = Rc::downgrade(&this);
        refresh_btn.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.refresh_view();
            }
        });
        let weak = Rc::downgrade(&this);
        this.file_list.connect_row_activated(move |_, row| {
            let Some(this) = weak.upgrade() else { return };
            let entry = this.state.borrow().entries.iter().find(|(r, _)| r == row).map(|(_, e)| e.clone());
            if let Some(entry) = entry {
                if entry.is_dir {
                    this.navigate_to(&entry.path.to_string_lossy());
                } else {
                    open_file(&entry.path, &this.status_bar);
                }
            }
        });
        // Ctrl+C copies the selected photos.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if state.contains(gdk::ModifierType::CONTROL_MASK) && key == gdk::Key::c {
                let paths = this.selected_paths();
                if !paths.is_empty() {
                    this.copy_paths(&paths, &this.photo_status);
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        this.widget.add_controller(keys);
        this.photo_actions.set_visible(true);
        this
    }

    fn showing_photos(&self) -> bool {
        self.view_stack.visible_child_name().as_deref() == Some("photos")
    }

    fn clock(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn current(&self) -> Option<(Client, String)> {
        let s = self.state.borrow();
        Some((s.client.clone()?, s.device_id.clone()?))
    }

    pub fn set_device(self: &Rc<Self>, client: &Client, device: Option<&Device>) {
        let (fresh, reachable) = {
            let mut s = self.state.borrow_mut();
            let id = device.map(|d| d.id.clone());
            let reachable = device.is_some_and(|d| d.reachable);
            let fresh = reachable && (s.device_id != id || !s.reachable);
            if s.device_id != id {
                s.subs.clear();
                s.mounted = false;
                s.mount_point.clear();
                s.storage_base.clear();
                s.directories.clear();
                s.current_path.clear();
            }
            s.client = Some(client.clone());
            s.device_id = id;
            s.reachable = reachable;
            (fresh, reachable)
        };
        match device {
            Some(d) if reachable => {
                if fresh {
                    self.subscribe(client, &d.id);
                    self.sync_mount(true, false);
                } else {
                    self.sync_mount(false, true);
                }
                if !self.state.borrow().mounting {
                    self.widget.set_visible_child_name("content");
                }
            }
            Some(d) => {
                self.status.set_description(Some(&format!(
                    "{} is disconnected.\nConnect your phone to browse files.",
                    d.name
                )));
                self.widget.set_visible_child_name("status");
            }
            None => {
                self.status.set_description(Some("No device connected.\nPair a phone to browse files."));
                self.widget.set_visible_child_name("status");
            }
        }
    }

    /// `mounted` / `unmounted`. (Python subscribed on the device object
    /// instead of `/sftp`, so these never reached it.)
    fn subscribe(self: &Rc<Self>, client: &Client, device_id: &str) {
        let path = format!("{}/sftp", device_path(device_id));
        let mut subs = Vec::new();
        for name in ["mounted", "unmounted"] {
            let weak = Rc::downgrade(self);
            subs.push(signals::subscribe(client.bus(), &path, IFACE_SFTP, name, move |_| {
                if let Some(this) = weak.upgrade() {
                    let mounted = name == "mounted";
                    this.sync_mount(mounted, false);
                }
            }));
        }
        self.state.borrow_mut().subs = subs;
    }

    /// A file arrived from the phone (`shareReceived`, fanned out by the window).
    pub fn share_received(&self, url: &str) {
        let path = gio::File::for_uri(url).path().unwrap_or_else(|| PathBuf::from(url));
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.status_bar.set_label(&format!("Received: {name}"));
    }

    /// Read the sftp state and act on it: mount when not mounted, rebuild
    /// Locations when the mount point changed, then load photos if asked.
    fn sync_mount(self: &Rc<Self>, load_photos: bool, load_if_photo_view_empty: bool) {
        let Some((client, id)) = self.current() else { return };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let st = client.fetch_sftp_state(&id).await;
            let Some(this) = weak.upgrade() else { return };
            if this.state.borrow().device_id.as_deref() != Some(id.as_str()) {
                return;
            }
            if !st.mounted || st.mount_point.is_empty() {
                {
                    let mut s = this.state.borrow_mut();
                    s.mounted = false;
                    s.mount_point.clear();
                    s.directories.clear();
                }
                this.update_mount_ui();
                this.start_mount();
                return;
            }
            let changed = {
                let mut s = this.state.borrow_mut();
                let changed = !s.mounted || s.mount_point != st.mount_point || s.storage_base.is_empty();
                s.mounted = true;
                s.mount_point = st.mount_point;
                s.directories = st.directories;
                if changed {
                    s.storage_base = files::storage_base(&s.directories, &s.mount_point);
                }
                changed
            };
            if changed {
                this.update_mount_ui();
                this.build_shortcuts();
            }
            this.widget.set_visible_child_name("content");
            let empty = this.state.borrow().tiles.is_empty();
            if load_photos || (load_if_photo_view_empty && this.showing_photos() && empty) {
                this.load_recent_photos();
            }
        });
    }

    /// `mountAndWait` (30 s); if that fails, `startBrowsing` and poll
    /// `isMounted` for 5 s; else show the daemon's mount error.
    fn start_mount(self: &Rc<Self>) {
        let Some((client, id)) = self.current() else { return };
        if std::mem::replace(&mut self.state.borrow_mut().mounting, true) {
            return;
        }
        self.widget.set_visible_child_name("mounting");
        self.spinner.start();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut ok = client.sftp_mount_and_wait(&id).await;
            if !ok && client.sftp_start_browsing(&id).await {
                for _ in 0..files::BROWSE_POLLS {
                    if client.sftp_is_mounted(&id).await {
                        break;
                    }
                    glib::timeout_future(Duration::from_millis(files::BROWSE_POLL_MS)).await;
                }
                ok = client.sftp_is_mounted(&id).await;
            }
            let result = if ok { Ok(client.fetch_sftp_state(&id).await) } else { Err(client.sftp_get_mount_error(&id).await) };
            let Some(this) = weak.upgrade() else { return };
            this.state.borrow_mut().mounting = false;
            if this.state.borrow().device_id.as_deref() != Some(id.as_str()) {
                return;
            }
            this.spinner.stop();
            {
                let mut s = this.state.borrow_mut();
                match &result {
                    Ok(st) => {
                        s.mounted = true;
                        s.mount_point = st.mount_point.clone();
                        s.directories = st.directories.clone();
                    }
                    Err(_) => {
                        s.mounted = false;
                        s.mount_point.clear();
                        s.directories.clear();
                    }
                }
                s.storage_base = files::storage_base(&s.directories, &s.mount_point);
            }
            if let Err(e) = &result {
                let text = format!("Mount failed: {e}");
                this.photo_status.set_label(&text);
                this.status_bar.set_label(&text);
            }
            this.update_mount_ui();
            this.build_shortcuts();
            this.widget.set_visible_child_name("content");
            let ready = {
                let s = this.state.borrow();
                s.mounted && !s.mount_point.is_empty()
            };
            if ready {
                this.load_recent_photos();
            }
        });
    }

    /// Reconnect, resume or window show: re-check the mount and pick up new
    /// photos or files, at most every 30 s and never mid-mount.
    pub fn refresh_from_phone(self: &Rc<Self>) {
        let now = self.clock();
        let Some((client, id)) = self.current() else { return };
        {
            let mut s = self.state.borrow_mut();
            if !s.reachable || s.mounting || !s.refresh.allow(now) {
                return;
            }
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let st = client.fetch_sftp_state(&id).await;
            let Some(this) = weak.upgrade() else { return };
            let (same_device, same_mount) = {
                let s = this.state.borrow();
                (s.device_id.as_deref() == Some(id.as_str()), st.mounted && st.mount_point == s.mount_point)
            };
            if !same_device {
                return;
            }
            if !same_mount {
                // A new connection unmounts the old storage: mount and reload.
                this.sync_mount(true, false);
            } else if this.showing_photos() {
                this.rescan_photos();
            } else {
                let path = this.state.borrow().current_path.clone();
                if !path.is_empty() {
                    this.load_directory(&path);
                }
            }
        });
    }

    fn update_mount_ui(&self) {
        if self.state.borrow().mounted {
            self.file_empty.set_title("Empty Folder");
            self.file_empty.set_description(Some("This folder is empty."));
            return;
        }
        self.file_empty.set_title("Connecting to phone…");
        self.file_empty.set_description(Some(
            "Waiting for file system access.\n\n\
             If this persists, grant \"Files and media\" permission\n\
             to KDE Connect in your phone's Settings → Apps → KDE Connect → Permissions.",
        ));
        self.photo_empty.set_description(Some(
            "Connecting to phone…\nGrant \"Files and media\" permission to KDE Connect if prompted.",
        ));
        self.photo_stack.set_visible_child_name("empty");
        self.clear_photos();
        self.clear_listing();
    }

    // ── Locations ──────────────────────────────────────────────────

    fn build_shortcuts(self: &Rc<Self>) {
        super::clear_list_box(&self.shortcuts_list);
        let dirs = files::sorted_directories(&self.state.borrow().directories);
        let mut items: Vec<(&str, String, String)> = vec![("camera-photo-symbolic", "Recent Photos".into(), PHOTOS.into())];
        for (path, label) in dirs {
            items.push((files::directory_icon_name(&label), label, path));
        }
        items.push(("folder-download-symbolic", "Downloads (local)".into(), self.downloads.clone()));
        let mut rows = Vec::new();
        for (icon, label, target) in items {
            let row = gtk::ListBoxRow::new();
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            b.set_margin_start(8);
            b.set_margin_end(8);
            b.set_margin_top(4);
            b.set_margin_bottom(4);
            let img = gtk::Image::from_icon_name(icon);
            img.set_pixel_size(16);
            b.append(&img);
            let l = gtk::Label::new(Some(&label));
            l.set_xalign(0.0);
            b.append(&l);
            row.set_child(Some(&b));
            self.shortcuts_list.append(&row);
            rows.push((row, target));
        }
        let first = rows.first().map(|(r, _)| r.clone());
        self.state.borrow_mut().shortcuts = rows;
        // "Recent Photos" is the default; selecting it emits row-selected.
        if let Some(first) = first {
            self.shortcuts_list.select_row(Some(&first));
        }
    }

    fn shortcut_chosen(self: &Rc<Self>, row: &gtk::ListBoxRow) {
        let Some(target) = self.state.borrow().shortcuts.iter().find(|(r, _)| r == row).map(|(_, t)| t.clone()) else {
            return;
        };
        if target == PHOTOS {
            self.view_stack.set_visible_child_name("photos");
            let (mounted, empty) = {
                let s = self.state.borrow();
                (s.mounted, s.tiles.is_empty())
            };
            if mounted && empty {
                self.load_recent_photos();
            }
            return;
        }
        self.view_stack.set_visible_child_name("browser");
        let mounted = self.state.borrow().mounted;
        if target != self.downloads && !mounted {
            self.status_bar.set_label("Mount the phone first to browse this location");
            return;
        }
        self.navigate_to(&target);
    }

    // ── Recent photos ──────────────────────────────────────────────

    fn clear_photos(&self) {
        let mut s = self.state.borrow_mut();
        s.photo_generation += 1;
        for t in s.tiles.drain(..) {
            self.photo_grid.remove(&t.child);
        }
    }

    fn photo_base(&self) -> Option<PathBuf> {
        let s = self.state.borrow();
        if s.mount_point.is_empty() {
            return None;
        }
        Some(PathBuf::from(if s.storage_base.is_empty() { &s.mount_point } else { &s.storage_base }))
    }

    fn load_recent_photos(self: &Rc<Self>) {
        let Some(base) = self.photo_base() else { return };
        self.clear_photos();
        self.photo_status.set_label("Scanning for photos…");
        let generation = self.state.borrow().photo_generation;
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(found) = gio::spawn_blocking(move || files::recent_photos(&base)).await else { return };
            if let Some(this) = weak.upgrade() {
                this.show_photos(generation, found);
            }
        });
    }

    /// Rescan; rebuild the grid only when the set changed.
    fn rescan_photos(self: &Rc<Self>) {
        let Some(base) = self.photo_base() else { return };
        let shown: Vec<PathBuf> = self.state.borrow().tiles.iter().map(|t| t.path.clone()).collect();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(found) = gio::spawn_blocking(move || files::recent_photos(&base)).await else { return };
            let Some(this) = weak.upgrade() else { return };
            let current: Vec<PathBuf> = this.state.borrow().tiles.iter().map(|t| t.path.clone()).collect();
            if current != shown || !files::rescan_changes_grid(&current, &found) {
                return;
            }
            this.clear_photos();
            let generation = this.state.borrow().photo_generation;
            this.show_photos(generation, found);
        });
    }

    fn show_photos(self: &Rc<Self>, generation: u64, found: Vec<PathBuf>) {
        if self.state.borrow().photo_generation != generation {
            return;
        }
        if found.is_empty() {
            self.photo_empty.set_description(Some("No photos found on phone.\nCheck DCIM/Camera or Pictures folders."));
            self.photo_stack.set_visible_child_name("empty");
            self.photo_status.set_label("No photos found");
            return;
        }
        self.photo_stack.set_visible_child_name("grid");
        self.photo_status.set_label(&format!("Loading {} photos…", found.len()));
        for path in &found {
            let tile = self.make_tile(path);
            self.photo_grid.append(&tile.child);
            self.state.borrow_mut().tiles.push(tile);
        }
        // Decode only once the tab is on screen: 18 thumbnails held ~7 MB
        // from startup even when Files was never opened (heaptrack,
        // 2026-11-06).
        if self.widget.is_mapped() {
            self.load_thumbnails(generation, found);
            return;
        }
        let weak = Rc::downgrade(self);
        let handler: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::default();
        let h = handler.clone();
        let id = self.widget.connect_map(move |w| {
            if let Some(id) = h.borrow_mut().take() {
                w.disconnect(id);
            }
            if let Some(this) = weak.upgrade() {
                this.load_thumbnails(generation, found.clone());
            }
        });
        *handler.borrow_mut() = Some(id);
    }

    fn make_tile(self: &Rc<Self>, path: &Path) -> Tile {
        let overlay = gtk::Overlay::new();
        overlay.set_size_request(THUMB_SIZE, THUMB_SIZE);
        let stack = gtk::Stack::new();
        stack.set_size_request(THUMB_SIZE, THUMB_SIZE);
        stack.add_css_class("photo-tile");
        overlay.set_child(Some(&stack));
        let placeholder = gtk::Image::from_icon_name(&icons::resolve("image-x-generic-symbolic"));
        placeholder.set_pixel_size(32);
        placeholder.set_opacity(0.4);
        stack.add_named(&placeholder, Some("placeholder"));
        // A square crop drawn at exactly the tile size (a Picture would grow
        // to the texture's own size).
        let picture = gtk::Image::new();
        picture.set_pixel_size(THUMB_SIZE);
        stack.add_named(&picture, Some("image"));
        stack.set_visible_child_name("placeholder");
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        check.set_pixel_size(24);
        check.set_halign(gtk::Align::End);
        check.set_valign(gtk::Align::Start);
        check.set_margin_top(6);
        check.set_margin_end(6);
        check.add_css_class("photo-check");
        check.set_visible(false);
        overlay.add_overlay(&check);
        let name = gtk::Label::new(path.file_name().map(|n| n.to_string_lossy()).as_deref());
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_halign(gtk::Align::Fill);
        name.set_valign(gtk::Align::End);
        name.add_css_class("photo-label");
        overlay.add_overlay(&name);

        let child = gtk::FlowBoxChild::new();
        child.set_size_request(THUMB_SIZE, THUMB_SIZE);
        child.set_halign(gtk::Align::Center);
        child.set_valign(gtk::Align::Start);
        child.set_child(Some(&overlay));
        let click = gtk::GestureClick::new();
        let (weak, p) = (Rc::downgrade(self), path.to_path_buf());
        click.connect_pressed(move |_, n, _, _| {
            let Some(this) = weak.upgrade() else { return };
            match n {
                1 => this.toggle_photo(&p),
                2 => open_file(&p, &this.photo_status),
                _ => {}
            }
        });
        child.add_controller(click);
        let right = gtk::GestureClick::new();
        right.set_button(gdk::BUTTON_SECONDARY);
        let (weak, p, anchor) = (Rc::downgrade(self), path.to_path_buf(), child.clone());
        right.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            if let Some(this) = weak.upgrade() {
                this.file_menu(anchor.upcast_ref(), x, y, &p, false, this.photo_status.clone());
            }
        });
        child.add_controller(right);
        Tile { path: path.to_path_buf(), child, stack, picture, check, selected: false }
    }

    /// One decode at a time, off the main thread, at 2× tile size; stops as
    /// soon as a newer scan starts.
    fn load_thumbnails(self: &Rc<Self>, generation: u64, paths: Vec<PathBuf>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let total = paths.len();
            for (i, path) in paths.into_iter().enumerate() {
                let alive = |w: &std::rc::Weak<Self>| w.upgrade().filter(|t| t.state.borrow().photo_generation == generation);
                if alive(&weak).is_none() {
                    return;
                }
                let p = path.clone();
                let decoded = gio::spawn_blocking(move || decode_thumbnail(&p)).await.ok().flatten();
                let Some(this) = alive(&weak) else { return };
                if let Some((bytes, w, h, stride, alpha)) = decoded {
                    let format = if alpha { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
                    let tex: gdk::Texture = gdk::MemoryTexture::new(w, h, format, &bytes, stride).upcast();
                    if let Some(t) = this.state.borrow().tiles.get(i).filter(|t| t.path == path) {
                        t.picture.set_paintable(Some(&tex));
                        t.stack.set_visible_child_name("image");
                    }
                }
                if (i + 1) % 5 == 0 || i + 1 == total {
                    this.photo_status.set_label(&format!("Loading {}/{total} photos…", i + 1));
                }
            }
            if let Some(this) = weak.upgrade().filter(|t| t.state.borrow().photo_generation == generation) {
                this.photo_status.set_label(&format!("{total} photos"));
            }
        });
    }

    fn toggle_photo(&self, path: &Path) {
        if let Some(t) = self.state.borrow_mut().tiles.iter_mut().find(|t| t.path == path) {
            let on = !t.selected;
            t.set_selected(on);
        }
        self.update_selection_buttons();
    }

    fn select_all(&self) {
        {
            let mut s = self.state.borrow_mut();
            let all = s.tiles.iter().all(|t| t.selected);
            for t in &mut s.tiles {
                t.set_selected(!all);
            }
        }
        self.update_selection_buttons();
    }

    fn clear_selection(&self) {
        for t in &mut self.state.borrow_mut().tiles {
            t.set_selected(false);
        }
        self.update_selection_buttons();
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        self.state.borrow().tiles.iter().filter(|t| t.selected).map(|t| t.path.clone()).collect()
    }

    fn update_selection_buttons(&self) {
        let count = self.state.borrow().tiles.iter().filter(|t| t.selected).count();
        self.save_btn.set_sensitive(count > 0);
        self.copy_btn.set_sensitive(count > 0);
        if count > 0 {
            self.photo_status.set_label(&files::selected_label(count));
        }
    }

    fn copy_selected(&self) {
        let paths = self.selected_paths();
        if !paths.is_empty() {
            self.copy_paths(&paths, &self.photo_status);
            self.clear_selection();
        }
    }

    // ── Shared file actions ────────────────────────────────────────

    /// Files on the clipboard as `text/uri-list`, so a file manager pastes them.
    fn copy_paths(&self, paths: &[PathBuf], status: &gtk::Label) {
        let uris: Vec<String> = paths.iter().map(|p| uri_of(p)).collect();
        let bytes = glib::Bytes::from_owned(files::uri_list(&uris).into_bytes());
        let provider = gdk::ContentProvider::for_bytes("text/uri-list", &bytes);
        if let Err(e) = self.widget.clipboard().set_content(Some(&provider)) {
            status.set_label(&format!("Cannot copy: {e}"));
            return;
        }
        status.set_label(&files::copied_label(paths));
    }

    /// Pick a folder, then copy there off the main thread (`_1`, `_2` on a clash).
    fn save_dialog(self: &Rc<Self>, paths: Vec<PathBuf>, status: gtk::Label) {
        let dialog = gtk::FileDialog::new();
        dialog.set_title("Save to Folder…");
        let root = self.widget.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(self);
        dialog.select_folder(root.as_ref(), gio::Cancellable::NONE, move |res| {
            let Some(dest) = res.ok().and_then(|f| f.path()) else { return };
            status.set_label("Saving…");
            glib::spawn_future_local(async move {
                let d = dest.clone();
                let Ok((saved, err)) = gio::spawn_blocking(move || files::save_copies(&paths, &d)).await else { return };
                if let Some(err) = err {
                    status.set_label(&err);
                    return;
                }
                if let Some(this) = weak.upgrade() {
                    this.clear_selection();
                }
                status.set_label(&files::saved_label(saved, &dest));
            });
        });
    }

    fn file_menu(self: &Rc<Self>, anchor: &gtk::Widget, x: f64, y: f64, path: &Path, is_dir: bool, status: gtk::Label) {
        let mut items: Vec<super::MenuItem> = Vec::new();
        let (weak, p, st) = (Rc::downgrade(self), path.to_path_buf(), status.clone());
        items.push((
            "Open",
            Rc::new(move || {
                if is_dir {
                    if let Some(this) = weak.upgrade() {
                        this.navigate_to(&p.to_string_lossy());
                    }
                } else {
                    open_file(&p, &st);
                }
            }),
        ));
        if !is_dir {
            let (weak, p, st) = (Rc::downgrade(self), path.to_path_buf(), status.clone());
            items.push((
                "Copy",
                Rc::new(move || {
                    if let Some(this) = weak.upgrade() {
                        this.copy_paths(std::slice::from_ref(&p), &st);
                    }
                }),
            ));
            let (weak, p, st) = (Rc::downgrade(self), path.to_path_buf(), status);
            items.push((
                "Save As…",
                Rc::new(move || {
                    if let Some(this) = weak.upgrade() {
                        this.save_dialog(vec![p.clone()], st.clone());
                    }
                }),
            ));
        }
        super::popup_menu(anchor, x, y, items);
    }

    // ── Browse ─────────────────────────────────────────────────────

    fn navigate_to(self: &Rc<Self>, path: &str) {
        let mount = {
            let mut s = self.state.borrow_mut();
            s.current_path = path.to_string();
            s.mount_point.clone()
        };
        self.path_label.set_label(&files::display_path(path, &mount));
        self.back_btn.set_sensitive(path != mount && path != self.downloads);
        self.load_directory(path);
    }

    fn clear_listing(&self) {
        super::clear_list_box(&self.file_list);
        self.state.borrow_mut().entries.clear();
    }

    fn load_directory(self: &Rc<Self>, path: &str) {
        self.clear_listing();
        let generation = {
            let mut s = self.state.borrow_mut();
            s.dir_generation += 1;
            s.dir_generation
        };
        self.status_bar.set_label("Loading…");
        let (weak, dir) = (Rc::downgrade(self), path.to_string());
        glib::spawn_future_local(async move {
            let d = PathBuf::from(&dir);
            let Ok(result) = gio::spawn_blocking(move || files::list_directory(&d)).await else { return };
            let Some(this) = weak.upgrade() else { return };
            {
                let s = this.state.borrow();
                if s.dir_generation != generation || s.current_path != dir {
                    return;
                }
            }
            match result {
                Ok(listing) => this.show_listing(&listing),
                Err(e) => this.status_bar.set_label(&e),
            }
        });
    }

    fn show_listing(self: &Rc<Self>, listing: &Listing) {
        for e in listing.dirs.iter().chain(&listing.files) {
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            b.set_margin_top(4);
            b.set_margin_bottom(4);
            b.set_margin_start(8);
            b.set_margin_end(8);
            let icon = gtk::Image::from_icon_name(&icons::resolve(files::file_icon_name(&e.name, e.is_dir)));
            icon.set_pixel_size(24);
            b.append(&icon);
            let name = gtk::Label::new(Some(&e.name));
            name.set_xalign(0.0);
            name.set_hexpand(true);
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            b.append(&name);
            if !e.is_dir && e.size > 0 {
                let size = gtk::Label::new(Some(&files::human_size(e.size)));
                size.add_css_class("caption");
                size.add_css_class("dim-label");
                b.append(&size);
            }
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&b));
            self.state.borrow_mut().entries.push((row.clone(), e.clone()));
            let right = gtk::GestureClick::new();
            right.set_button(gdk::BUTTON_SECONDARY);
            let (weak, entry, anchor) = (Rc::downgrade(self), e.clone(), row.clone());
            right.connect_pressed(move |g, _, x, y| {
                g.set_state(gtk::EventSequenceState::Claimed);
                if let Some(this) = weak.upgrade() {
                    this.file_menu(anchor.upcast_ref(), x, y, &entry.path, entry.is_dir, this.status_bar.clone());
                }
            });
            row.add_controller(right);
            self.file_list.append(&row);
        }
        self.status_bar.set_label(&listing.summary());
    }

    fn go_up(self: &Rc<Self>) {
        let parent = {
            let s = self.state.borrow();
            files::parent_within(&s.current_path, &s.mount_point, &self.downloads)
        };
        if let Some(p) = parent {
            self.navigate_to(&p);
        }
    }

    fn refresh_view(self: &Rc<Self>) {
        if self.showing_photos() {
            if self.state.borrow().mounted {
                self.load_recent_photos();
            }
            return;
        }
        let path = self.state.borrow().current_path.clone();
        if !path.is_empty() {
            self.load_directory(&path);
        }
    }

    // ── Send to phone ──────────────────────────────────────────────

    fn send_files(self: &Rc<Self>) {
        if self.current().is_none() || !self.state.borrow().reachable {
            self.status_bar.set_label("Phone not connected");
            return;
        }
        let dialog = gtk::FileDialog::new();
        dialog.set_title("Send File to Phone");
        let root = self.widget.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(self);
        dialog.open_multiple(root.as_ref(), gio::Cancellable::NONE, move |res| {
            let (Some(this), Ok(model)) = (weak.upgrade(), res) else { return };
            let Some((client, id)) = this.current() else { return };
            let paths: Vec<String> = (0..model.n_items())
                .filter_map(|i| model.item(i).and_downcast::<gio::File>())
                .filter_map(|f| f.path())
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            if paths.is_empty() {
                return;
            }
            let names: Vec<String> = paths
                .iter()
                .map(|p| Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
                .collect();
            this.status_bar.set_label(&format!("Sending: {}", names.join(", ")));
            glib::spawn_future_local(async move {
                match paths.as_slice() {
                    [one] => client.share_file(&id, one).await,
                    many => client.share_files(&id, &many.iter().map(String::as_str).collect::<Vec<_>>()).await,
                }
            });
        });
    }
}

fn open_file(path: &Path, status: &gtk::Label) {
    if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri_of(path), gio::AppLaunchContext::NONE) {
        status.set_label(&format!("Cannot open: {}", e.message()));
    }
}

/// A centered square at 2× tile size, upright, as raw pixels the main
/// thread turns into a texture (the tile "covers", as Python's did).
fn decode_thumbnail(path: &Path) -> Option<(glib::Bytes, i32, i32, usize, bool)> {
    let s = THUMB_SIZE * 2;
    let (_, w, h) = gtk::gdk_pixbuf::Pixbuf::file_info(path)?;
    if w <= 0 || h <= 0 {
        return None;
    }
    // Scale so the short side is `s`, then cut the middle square.
    let k = s as f64 / w.min(h) as f64;
    let (sw, sh) = (((w as f64 * k).round() as i32).max(s), ((h as f64 * k).round() as i32).max(s));
    let pb = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, sw, sh, false).ok()?;
    let pb = pb.apply_embedded_orientation().unwrap_or(pb);
    let side = pb.width().min(pb.height());
    let pb = pb.new_subpixbuf((pb.width() - side) / 2, (pb.height() - side) / 2, side, side).copy()?;
    Some((pb.read_pixel_bytes(), pb.width(), pb.height(), pb.rowstride() as usize, pb.has_alpha()))
}
