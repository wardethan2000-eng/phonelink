//! The Files tab (port of `phonelink/ui/files_panel.py`): mounts the phone's
//! storage through KDE Connect (sftp), shows the newest photos as a grid and
//! lets you browse folders. Every filesystem call runs off the main thread
//! (`gio::spawn_blocking`), because the FUSE mount can stall.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::gdk;

use super::icons;
use crate::core::files::{self, FolderWatch, Listing, Photo, RefreshLimit, THUMB_SIZE};
use crate::core::models::Device;
use crate::core::settings::Settings;
use crate::core::thumbs;
use crate::dbus::signals::{self, Subscription};
use crate::dbus::{Client, IFACE_SFTP, device_path};

const PHOTOS: &str = "__photos__";
/// Thumbnail size in the list view.
const LIST_THUMB: i32 = 56;
/// How far the pointer moves before a press becomes a drag-select.
const DRAG_START: f64 = 8.0;

/// A drag-select in progress (Recent Photos, issue #10).
struct Drag {
    /// Where it started, in the photo area's coordinates.
    start: (f64, f64),
    /// The pointer's offset from the start at the last motion.
    offset: (f64, f64),
    /// The scroll position at the last motion.
    scrolled_at: f64,
    /// Which tiles were selected before (a drag adds to the selection).
    before: Vec<bool>,
    active: bool,
    tick: Option<glib::SourceId>,
}

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
    /// The last scan, newest first.
    photos: Vec<Photo>,
    /// Decoded thumbnails, kept across view changes.
    thumbs: HashMap<PathBuf, gdk::Texture>,
    drag: Option<Drag>,
    /// The photo last clicked on its own: where a Shift+click range starts.
    select_anchor: Option<PathBuf>,
    /// Sidebar row → location (a path, or [`PHOTOS`]).
    shortcuts: Vec<(gtk::ListBoxRow, String)>,
    /// Browse rows and what they stand for.
    entries: Vec<(gtk::ListBoxRow, files::Entry)>,
    refresh: RefreshLimit,
    /// Automatic remounts after a drop (a few, then the "Try Again" page).
    remounts: files::RemountGuard,
    /// The next automatic remount, while one waits.
    retry: Option<glib::SourceId>,
    /// The folders behind Recent Photos and the open Browse folder, to pick
    /// up new photos and files while Files is on screen.
    photo_watch: Option<FolderWatch>,
    dir_watch: Option<FolderWatch>,
    /// The Browse listing on screen.
    listing: Option<Listing>,
    /// Since when a look at the folders, or a rescan, has been running
    /// (clock seconds). A stalled sftp link can hang one; after a minute
    /// another may start.
    looking_since: Option<f64>,
    scanning_since: Option<f64>,
    subs: Vec<Subscription>,
}

pub struct FilesPage {
    pub widget: gtk::Stack,
    status: adw::StatusPage,
    /// The storage keeps failing to mount: why, and "Try Again".
    trouble: adw::StatusPage,
    spinner: gtk::Spinner,
    shortcuts_list: gtk::ListBox,
    view_stack: adw::ViewStack,
    photo_actions: gtk::Box,
    save_btn: gtk::Button,
    copy_btn: gtk::Button,
    photo_sections: gtk::Box,
    photo_area: gtk::Overlay,
    photo_scroll: gtk::ScrolledWindow,
    sort_btn: gtk::Button,
    settings: Rc<RefCell<Settings>>,
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

/// A look or a rescan running this long is taken as stuck on a stalled
/// sftp link; another may start.
const STALLED_SECONDS: f64 = 60.0;

/// The Recent Photos folders, watched from before they are read, and the
/// pictures in them; `fresh` reads them from the phone, past sshfs's cache.
/// Off the main thread: it reads the phone.
fn scan_photos(base: &Path, fresh: bool) -> (FolderWatch, Vec<Photo>) {
    let folders = if fresh { files::photo_folders_fresh(base) } else { files::photo_folders(base) };
    let watch = FolderWatch::take(folders);
    let found = files::photos_in(&watch.folders);
    (watch, found)
}

/// A Browse folder, watched from before it is read, and its listing.
fn read_directory(dir: &Path, fresh: bool) -> (FolderWatch, Result<Listing, String>) {
    if fresh {
        files::flush_cached(dir);
    }
    let watch = FolderWatch::take(vec![dir.to_path_buf()]);
    (watch, files::list_directory(dir))
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

/// One look at the mount point, off the main thread; a look that hangs
/// (a stalled sftp link) counts as there.
async fn check_once(mount_point: &str) -> files::MountCheck {
    let p = PathBuf::from(mount_point);
    let look = gio::spawn_blocking(move || files::check_mount(&p));
    match glib::future_with_timeout(Duration::from_millis(files::MOUNT_CHECK_TIMEOUT_MS), look).await {
        Ok(Ok(check)) => check,
        Ok(Err(_)) => files::MountCheck::Absent,
        Err(_) => files::MountCheck::Ready,
    }
}

/// Wait for a fresh mount to answer (sshfs connects a moment after KDE
/// Connect says "mounted").
async fn mount_state(mount_point: &str) -> files::MountCheck {
    for i in 0..files::MOUNT_READY_POLLS {
        if i > 0 {
            glib::timeout_future(Duration::from_millis(files::MOUNT_READY_POLL_MS)).await;
        }
        match check_once(mount_point).await {
            files::MountCheck::Absent => {}
            found => return found,
        }
    }
    files::MountCheck::Absent
}

async fn clear_stale(mount_point: &str) {
    let p = PathBuf::from(mount_point);
    let _ = gio::spawn_blocking(move || files::clear_stale_mount(&p)).await;
}

fn uri_of(path: &Path) -> String {
    gio::File::for_path(path).uri().to_string()
}

impl FilesPage {
    pub fn new(settings: Rc<RefCell<Settings>>) -> Rc<Self> {
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

        let trouble = adw::StatusPage::new();
        trouble.set_icon_name(Some(&icons::resolve("dialog-warning-symbolic")));
        trouble.set_title("Can't Open the Phone's Storage");
        let try_again = gtk::Button::with_label("Try Again");
        try_again.add_css_class("pill");
        try_again.add_css_class("suggested-action");
        try_again.set_halign(gtk::Align::Center);
        trouble.set_child(Some(&try_again));
        widget.add_named(&trouble, Some("trouble"));

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
        // Mount the phone's storage afresh (issue #12): clears a dead mount
        // left behind by a crash, then mounts and reloads.
        let reconnect_btn = gtk::Button::new();
        reconnect_btn.add_css_class("flat");
        reconnect_btn.set_margin_start(8);
        reconnect_btn.set_margin_end(8);
        reconnect_btn.set_tooltip_text(Some("Mount the phone's storage again and reload"));
        let reconnect_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        reconnect_row.append(&gtk::Image::from_icon_name("view-refresh-symbolic"));
        let reconnect_label = gtk::Label::new(Some("Reconnect Phone Storage"));
        reconnect_label.set_xalign(0.0);
        reconnect_row.append(&reconnect_label);
        reconnect_btn.set_child(Some(&reconnect_row));
        sidebar.append(&reconnect_btn);
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
        // How Recent Photos shows (issue #10): grid or list, order, days.
        let (as_list, oldest_first, by_day) = settings.borrow().photos_view();
        let grid_btn = gtk::ToggleButton::new();
        grid_btn.set_icon_name(&icons::resolve("view-grid-symbolic"));
        grid_btn.set_tooltip_text(Some("Show as a grid"));
        let list_btn = gtk::ToggleButton::new();
        list_btn.set_icon_name(&icons::resolve("view-list-symbolic"));
        list_btn.set_tooltip_text(Some("Show as a list"));
        list_btn.set_group(Some(&grid_btn));
        if as_list { list_btn.set_active(true) } else { grid_btn.set_active(true) }
        let layout = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        layout.add_css_class("linked");
        layout.append(&grid_btn);
        layout.append(&list_btn);
        photo_actions.append(&layout);
        let sort_btn = flat_icon(sort_icon(oldest_first), sort_tooltip(oldest_first));
        photo_actions.append(&sort_btn);
        let day_btn = gtk::ToggleButton::new();
        day_btn.set_icon_name(&icons::resolve("x-office-calendar-symbolic"));
        day_btn.set_tooltip_text(Some("Group by day"));
        day_btn.add_css_class("flat");
        day_btn.set_active(by_day);
        photo_actions.append(&day_btn);
        let photo_refresh = flat_icon("view-refresh-symbolic", "Check the phone for new photos");
        photo_actions.append(&photo_refresh);
        photo_actions.append(&gtk::Separator::new(gtk::Orientation::Vertical));
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
        // Sections (one per day, or one), each a heading and a FlowBox.
        let photo_sections = gtk::Box::new(gtk::Orientation::Vertical, 0);
        photo_sections.set_valign(gtk::Align::Start);
        photo_sections.set_margin_start(8);
        photo_sections.set_margin_end(8);
        photo_sections.set_margin_top(4);
        photo_sections.set_margin_bottom(8);
        let photo_area = gtk::Overlay::new();
        photo_area.set_child(Some(&photo_sections));
        photo_scroll.set_child(Some(&photo_area));
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
        // Like a file manager: click selects, Ctrl/Shift+click and Ctrl+A
        // select more, double-click (or Enter) opens.
        file_list.set_selection_mode(gtk::SelectionMode::Multiple);
        file_list.set_activate_on_single_click(false);
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
            trouble,
            spinner,
            shortcuts_list,
            view_stack,
            photo_actions,
            save_btn,
            copy_btn,
            photo_sections,
            photo_area,
            photo_scroll: photo_scroll.clone(),
            sort_btn,
            settings,
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
        list_btn.connect_toggled(move |b| {
            if let Some(this) = weak.upgrade() {
                this.change_view(|v| v.0 = b.is_active());
            }
        });
        let weak = Rc::downgrade(&this);
        this.sort_btn.connect_clicked(move |b| {
            if let Some(this) = weak.upgrade() {
                this.change_view(|v| v.1 = !v.1);
                let oldest = this.settings.borrow().photos_view().1;
                b.set_icon_name(&icons::resolve(sort_icon(oldest)));
                b.set_tooltip_text(Some(sort_tooltip(oldest)));
            }
        });
        let weak = Rc::downgrade(&this);
        day_btn.connect_toggled(move |b| {
            if let Some(this) = weak.upgrade() {
                this.change_view(|v| v.2 = b.is_active());
            }
        });
        this.wire_drag_select();
        let weak = Rc::downgrade(&this);
        glib::timeout_add_seconds_local(files::WATCH_INTERVAL_SECONDS, move || {
            let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break };
            this.look_for_changes();
            glib::ControlFlow::Continue
        });
        let weak = Rc::downgrade(&this);
        this.widget.connect_map(move |_| {
            if let Some(this) = weak.upgrade() {
                this.look_for_changes();
            }
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
        for b in [&reconnect_btn, &try_again] {
            let weak = Rc::downgrade(&this);
            b.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.reconnect();
                }
            });
        }
        let weak = Rc::downgrade(&this);
        photo_refresh.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.refresh_view();
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
        // Ctrl+C copies everything selected in the view that is showing.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if state.contains(gdk::ModifierType::CONTROL_MASK) && matches!(key, gdk::Key::c | gdk::Key::C) {
                let (paths, status) = if this.showing_photos() {
                    (this.selected_paths(), &this.photo_status)
                } else {
                    (this.selected_browse_files(), &this.status_bar)
                };
                if !paths.is_empty() {
                    this.copy_paths(&paths, status);
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
            if fresh || !reachable {
                s.remounts.reset();
                if let Some(t) = s.retry.take() {
                    t.remove();
                }
            }
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
                let Some(this) = weak.upgrade() else { return };
                // A mount of ours in progress reads the outcome itself.
                if this.state.borrow().mounting {
                    return;
                }
                if name == "mounted" {
                    this.sync_mount(true, false);
                } else if this.state.borrow().mounted {
                    // sshfs went away: mount again after a pause, a few times.
                    let mount_point = this.state.borrow().mount_point.clone();
                    this.mount_failed(files::MountCheck::Absent, mount_point, None);
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
        if self.state.borrow().mounting {
            return;
        }
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
            // The daemon says mounted; make sure the storage is really there.
            let check = mount_state(&st.mount_point).await;
            let Some(this) = weak.upgrade() else { return };
            if check != files::MountCheck::Ready {
                this.mount_failed(check, st.mount_point, None);
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

    /// `mountAndWait` (KDE Connect's 10 s timeout), then wait for the
    /// storage to answer: the daemon says "mounted" when it starts sshfs,
    /// not when sshfs has connected. A dead mount from an earlier crash is
    /// cleared first, or sshfs can't mount there.
    fn start_mount(self: &Rc<Self>) {
        let Some((client, id)) = self.current() else { return };
        if std::mem::replace(&mut self.state.borrow_mut().mounting, true) {
            return;
        }
        if let Some(t) = self.state.borrow_mut().retry.take() {
            t.remove();
        }
        self.widget.set_visible_child_name("mounting");
        self.spinner.start();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let point = client.sftp_mount_point(&id).await;
            if !point.is_empty() && check_once(&point).await == files::MountCheck::Stale {
                clear_stale(&point).await;
            }
            let ok = client.sftp_mount_and_wait(&id).await;
            let st = client.fetch_sftp_state(&id).await;
            let check = if ok && st.mounted && !st.mount_point.is_empty() {
                mount_state(&st.mount_point).await
            } else {
                files::MountCheck::Absent
            };
            let Some(this) = weak.upgrade() else { return };
            this.state.borrow_mut().mounting = false;
            if this.state.borrow().device_id.as_deref() != Some(id.as_str()) {
                return;
            }
            this.spinner.stop();
            if check != files::MountCheck::Ready {
                let error = if ok { None } else { Some(client.sftp_get_mount_error(&id).await) };
                if let Some(this) = weak.upgrade() {
                    let at = if st.mount_point.is_empty() { point } else { st.mount_point };
                    this.mount_failed(check, at, error);
                }
                return;
            }
            {
                let mut s = this.state.borrow_mut();
                s.mounted = true;
                s.mount_point = st.mount_point.clone();
                s.directories = st.directories.clone();
                s.storage_base = files::storage_base(&s.directories, &s.mount_point);
            }
            this.update_mount_ui();
            this.build_shortcuts();
            this.widget.set_visible_child_name("content");
            this.load_recent_photos();
        });
    }

    /// The storage didn't mount, or dropped: clear a dead mount, then mount
    /// again after a growing pause; after a few failures in a row, stop and
    /// show why, with "Try Again" (issue #12: it used to remount at once,
    /// forever).
    fn mount_failed(self: &Rc<Self>, check: files::MountCheck, mount_point: String, error: Option<String>) {
        let Some((client, id)) = self.current() else { return };
        let wait = {
            let mut s = self.state.borrow_mut();
            s.mounted = false;
            s.mount_point.clear();
            s.directories.clear();
            s.storage_base.clear();
            let now = self.started.elapsed().as_secs_f64();
            s.remounts.dropped(now)
        };
        let next = match wait {
            Some(w) => format!("trying again in {w} s"),
            None => "stopped trying".to_string(),
        };
        log::warn!("files: the phone's storage didn't mount ({check:?}); {next}");
        self.update_mount_ui();
        self.build_shortcuts();
        self.widget.set_visible_child_name("content");
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            // Stop the daemon's sshfs, then detach a dead mount it leaves.
            client.sftp_unmount(&id).await;
            let stale = check == files::MountCheck::Stale
                || (!mount_point.is_empty() && check_once(&mount_point).await == files::MountCheck::Stale);
            if stale && !mount_point.is_empty() {
                clear_stale(&mount_point).await;
            }
            let Some(this) = weak.upgrade() else { return };
            if this.state.borrow().device_id.as_deref() != Some(id.as_str()) {
                return;
            }
            match wait {
                Some(secs) => {
                    let weak = Rc::downgrade(&this);
                    let t = glib::timeout_add_local_once(Duration::from_secs_f64(secs), move || {
                        let Some(this) = weak.upgrade() else { return };
                        let go = {
                            let mut s = this.state.borrow_mut();
                            s.retry = None;
                            s.reachable && !s.mounted && s.device_id.as_deref() == Some(id.as_str())
                        };
                        if go {
                            this.start_mount();
                        }
                    });
                    if let Some(old) = this.state.borrow_mut().retry.replace(t) {
                        old.remove();
                    }
                }
                None => {
                    let error = match error {
                        Some(e) if !e.is_empty() => e,
                        _ => client.sftp_get_mount_error(&id).await,
                    };
                    let Some(this) = weak.upgrade() else { return };
                    this.show_trouble(&error);
                }
            }
        });
    }

    fn show_trouble(&self, error: &str) {
        let why = if error.is_empty() { String::new() } else { format!("KDE Connect says: {error}\n\n") };
        self.trouble.set_description(Some(&format!(
            "{why}The connection to your phone's files keeps dropping. Try again; if it keeps \
             happening, check that KDE Connect on the phone may access files, or restart KDE \
             Connect on this computer."
        )));
        self.widget.set_visible_child_name("trouble");
    }

    /// "Try Again" and "Reconnect Phone Storage": unmount, clear a dead
    /// mount, mount again and reload.
    fn reconnect(self: &Rc<Self>) {
        let Some((client, id)) = self.current() else { return };
        {
            let mut s = self.state.borrow_mut();
            if s.mounting || !s.reachable {
                return;
            }
            s.remounts.reset();
            if let Some(t) = s.retry.take() {
                t.remove();
            }
            s.mounted = false;
            s.mount_point.clear();
            s.directories.clear();
            s.storage_base.clear();
        }
        self.update_mount_ui();
        self.widget.set_visible_child_name("mounting");
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            client.sftp_unmount(&id).await;
            let point = client.sftp_mount_point(&id).await;
            if !point.is_empty() && check_once(&point).await == files::MountCheck::Stale {
                clear_stale(&point).await;
            }
            if let Some(this) = weak.upgrade() {
                this.start_mount();
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
                this.rescan_photos(false);
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
            // 16 px: the stylesheet sizes it (Settings > Size).
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
        s.tiles.clear();
        s.photos.clear();
        while let Some(c) = self.photo_sections.first_child() {
            self.photo_sections.remove(&c);
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
            let Ok((watch, found)) = gio::spawn_blocking(move || {
                thumbs::prune(&thumbs::cache_dir(), 2000);
                scan_photos(&base, false)
            })
            .await
            else {
                return;
            };
            if let Some(this) = weak.upgrade() {
                if this.state.borrow().photo_generation == generation {
                    this.state.borrow_mut().photo_watch = Some(watch);
                }
                this.show_photos(generation, found);
            }
        });
    }

    /// Rescan; rebuild the grid only when the set changed, keeping the
    /// selection and the thumbnails already decoded. `fresh` (the Refresh
    /// button) reads the phone itself rather than sshfs's copy, and runs
    /// even while an automatic rescan does: that one's older result is
    /// then dropped, as the photos on screen changed under it.
    fn rescan_photos(self: &Rc<Self>, fresh: bool) {
        let Some(base) = self.photo_base() else { return };
        let now = self.clock();
        let shown = {
            let mut s = self.state.borrow_mut();
            if !fresh && s.scanning_since.is_some_and(|t| now - t < STALLED_SECONDS) {
                return;
            }
            s.scanning_since = Some(now);
            files::photo_paths(&s.photos)
        };
        if fresh {
            self.photo_status.set_label("Checking the phone for new photos…");
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || scan_photos(&base, fresh)).await;
            let Some(this) = weak.upgrade() else { return };
            this.state.borrow_mut().scanning_since = None;
            if fresh {
                this.show_photo_count();
            }
            let Ok((mut watch, found)) = result else { return };
            let now = this.clock();
            let generation = {
                let mut s = this.state.borrow_mut();
                if s.drag.is_some() {
                    // Not mid drag-select: try again on a later look.
                    if let Some(w) = s.photo_watch.as_mut() {
                        w.keep_settling(now);
                    }
                    return;
                }
                if let Some(old) = &s.photo_watch {
                    watch.carry_settling(old);
                }
                s.photo_watch = Some(watch);
                let current = files::photo_paths(&s.photos);
                if current != shown || !files::rescan_changes_grid(&current, &files::photo_paths(&found)) {
                    return;
                }
                // Drops thumbnails still decoding for the old set; the
                // redraw asks again for any it still lacks.
                s.photo_generation += 1;
                s.photo_generation
            };
            this.show_photos(generation, found);
        });
    }

    /// Every few seconds while Files is on screen: has a watched folder
    /// changed (a new photo, a file saved on the phone)? Then rescan it.
    /// Only the folders' change times are read, not their listings.
    fn look_for_changes(self: &Rc<Self>) {
        if !self.widget.is_mapped() {
            return;
        }
        let photos = self.showing_photos();
        let now = self.clock();
        let folders = {
            let mut s = self.state.borrow_mut();
            let busy = |t: Option<f64>| t.is_some_and(|t| now - t < STALLED_SECONDS);
            if !s.mounted || s.mounting || s.drag.is_some() || busy(s.looking_since) || busy(s.scanning_since) {
                return;
            }
            let watch = if photos { &s.photo_watch } else { &s.dir_watch };
            let Some(folders) = watch.as_ref().map(|w| w.folders.clone()) else { return };
            s.looking_since = Some(now);
            folders
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let watched = folders.clone();
            let result = gio::spawn_blocking(move || files::folder_stamps(&watched)).await;
            let Some(this) = weak.upgrade() else { return };
            this.state.borrow_mut().looking_since = None;
            let Ok(stamps) = result else { return };
            let changed = {
                let mut s = this.state.borrow_mut();
                let watch = if photos { s.photo_watch.as_mut() } else { s.dir_watch.as_mut() };
                // A scan or another folder may have replaced the watch meanwhile.
                match watch {
                    Some(w) if w.folders == folders => w.changed(stamps, this.clock()),
                    _ => false,
                }
            };
            if !changed || photos != this.showing_photos() {
                return;
            }
            if photos {
                this.rescan_photos(false);
            } else {
                this.refresh_directory();
            }
        });
    }

    fn show_photos(self: &Rc<Self>, generation: u64, found: Vec<Photo>) {
        if self.state.borrow().photo_generation != generation {
            return;
        }
        if found.is_empty() {
            self.photo_empty.set_description(Some("No photos found on phone.\nCheck DCIM/Camera or Pictures folders."));
            self.photo_stack.set_visible_child_name("empty");
            self.photo_status.set_label("No photos found");
            return;
        }
        {
            let mut s = self.state.borrow_mut();
            let keep: HashSet<&PathBuf> = found.iter().map(|p| &p.path).collect();
            s.thumbs.retain(|p, _| keep.contains(p));
            s.photos = found;
        }
        self.photo_stack.set_visible_child_name("grid");
        self.render_photos();
    }

    /// Change the view settings (as a list, oldest first, by day) and redraw.
    fn change_view(self: &Rc<Self>, f: impl FnOnce(&mut (bool, bool, bool))) {
        let mut v = self.settings.borrow().photos_view();
        f(&mut v);
        if let Err(e) = self.settings.borrow_mut().set_photos_view(v.0, v.1, v.2) {
            log::warn!("save photo view: {e}");
        }
        self.render_photos();
    }

    /// (Re)build the tiles from the last scan in the chosen view, keeping
    /// the selection and the thumbnails already decoded.
    fn render_photos(self: &Rc<Self>) {
        let (as_list, oldest_first, by_day) = self.settings.borrow().photos_view();
        let (photos, selected, generation) = {
            let mut s = self.state.borrow_mut();
            let selected: HashSet<PathBuf> = s.tiles.iter().filter(|t| t.selected).map(|t| t.path.clone()).collect();
            s.tiles.clear();
            (files::ordered_photos(&s.photos, oldest_first), selected, s.photo_generation)
        };
        while let Some(c) = self.photo_sections.first_child() {
            self.photo_sections.remove(&c);
        }
        if photos.is_empty() {
            return;
        }
        let groups = if by_day {
            files::group_by_day(&photos, day_key)
        } else {
            vec![((0, 0), (0..photos.len()).collect())]
        };
        let mut missing = Vec::new();
        for (n, (_, ids)) in groups.iter().enumerate() {
            if by_day {
                let heading = gtk::Label::new(Some(&day_heading(photos[ids[0]].modified)));
                heading.add_css_class("heading");
                heading.set_xalign(0.0);
                heading.set_margin_start(4);
                heading.set_margin_top(if n == 0 { 4 } else { 16 });
                heading.set_margin_bottom(6);
                self.photo_sections.append(&heading);
            }
            let flow = gtk::FlowBox::new();
            flow.set_valign(gtk::Align::Start);
            // Packed from the left: a day with two photos doesn't spread
            // them across the width.
            if !as_list {
                flow.set_halign(gtk::Align::Start);
            }
            flow.set_selection_mode(gtk::SelectionMode::None);
            if as_list {
                flow.set_max_children_per_line(1);
                flow.set_min_children_per_line(1);
                flow.set_row_spacing(2);
                flow.set_homogeneous(true);
            } else {
                flow.set_max_children_per_line(20);
                flow.set_min_children_per_line(2);
                flow.set_column_spacing(4);
                flow.set_row_spacing(4);
                flow.set_homogeneous(false);
            }
            if !by_day {
                flow.set_margin_top(4);
            }
            self.photo_sections.append(&flow);
            for &i in ids {
                let photo = &photos[i];
                let mut tile = if as_list { self.make_row(photo, by_day) } else { self.make_tile(&photo.path) };
                if selected.contains(&photo.path) {
                    tile.set_selected(true);
                }
                if let Some(tex) = self.state.borrow().thumbs.get(&photo.path) {
                    tile.picture.set_paintable(Some(tex));
                    tile.stack.set_visible_child_name("image");
                } else {
                    missing.push(photo.clone());
                }
                flow.append(&tile.child);
                self.state.borrow_mut().tiles.push(tile);
            }
        }
        let total = photos.len();
        if missing.is_empty() {
            self.photo_status.set_label(&format!("{total} photos"));
            self.update_selection_buttons();
            return;
        }
        self.photo_status.set_label(&format!("Loading {total} photos…"));
        self.update_selection_buttons();
        // Decode only once the tab is on screen: 18 thumbnails held ~7 MB
        // from startup even when Files was never opened (heaptrack,
        // 2026-11-06).
        if self.widget.is_mapped() {
            self.load_thumbnails(generation, missing, total);
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
                this.load_thumbnails(generation, missing.clone(), total);
            }
        });
        *handler.borrow_mut() = Some(id);
    }

    /// Clicks on a tile or row: one toggles it (unless that press became a
    /// drag-select), two open it; right-click shows the menu.
    fn wire_tile(self: &Rc<Self>, child: &gtk::FlowBoxChild, path: &Path) {
        let click = gtk::GestureClick::new();
        let (weak, p) = (Rc::downgrade(self), path.to_path_buf());
        // Double-click on press (the FlowBox claims the second press);
        // single on release, so a drag that starts on a tile doesn't toggle it.
        click.connect_pressed(move |_, n, _, _| {
            if n == 2
                && let Some(this) = weak.upgrade()
            {
                open_file(&p, &this.photo_status);
            }
        });
        let (weak, p) = (Rc::downgrade(self), path.to_path_buf());
        click.connect_released(move |g, n, _, _| {
            let Some(this) = weak.upgrade() else { return };
            let dragged = this.state.borrow().drag.as_ref().is_some_and(|d| d.active);
            if n == 1 && !dragged {
                if g.current_event_state().contains(gdk::ModifierType::SHIFT_MASK) {
                    this.select_range_to(&p);
                } else {
                    this.toggle_photo(&p);
                }
            }
        });
        child.add_controller(click);
        let right = gtk::GestureClick::new();
        right.set_button(gdk::BUTTON_SECONDARY);
        let (weak, p, anchor) = (Rc::downgrade(self), path.to_path_buf(), child.clone());
        right.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            if let Some(this) = weak.upgrade() {
                let targets = files::menu_targets(&p, this.selected_paths());
                this.file_menu(anchor.upcast_ref(), x, y, &p, false, targets, this.photo_status.clone());
            }
        });
        child.add_controller(right);
    }

    fn make_tile(self: &Rc<Self>, path: &Path) -> Tile {
        let overlay = gtk::Overlay::new();
        overlay.set_size_request(THUMB_SIZE, THUMB_SIZE);
        let stack = gtk::Stack::new();
        stack.set_size_request(THUMB_SIZE, THUMB_SIZE);
        stack.add_css_class("photo-tile");
        overlay.set_child(Some(&stack));
        let placeholder = gtk::Image::from_icon_name(&icons::resolve("image-x-generic-symbolic"));
        super::scale::icon(&placeholder, 32);
        placeholder.set_opacity(0.4);
        stack.add_named(&placeholder, Some("placeholder"));
        // A square crop drawn at exactly the tile size (a Picture would grow
        // to the texture's own size).
        let picture = gtk::Image::new();
        picture.set_pixel_size(THUMB_SIZE);
        stack.add_named(&picture, Some("image"));
        stack.set_visible_child_name("placeholder");
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        super::scale::icon(&check, 24);
        check.set_halign(gtk::Align::End);
        check.set_valign(gtk::Align::Start);
        check.set_margin_top(6);
        check.set_margin_end(6);
        check.add_css_class("photo-check");
        check.set_visible(false);
        overlay.add_overlay(&check);
        // No name label over the photo (Ethan, 2026-11-06); it shows on hover.
        overlay.set_tooltip_text(path.file_name().map(|n| n.to_string_lossy()).as_deref());

        let child = gtk::FlowBoxChild::new();
        child.set_size_request(THUMB_SIZE, THUMB_SIZE);
        child.set_halign(gtk::Align::Center);
        child.set_valign(gtk::Align::Start);
        child.set_child(Some(&overlay));
        self.wire_tile(&child, path);
        Tile { path: path.to_path_buf(), child, stack, picture, check, selected: false }
    }

    /// A list row: small thumbnail, name, time and size. `time_only` when a
    /// day heading already says the date.
    fn make_row(self: &Rc<Self>, photo: &Photo, time_only: bool) -> Tile {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.set_margin_start(4);
        row.set_margin_end(8);
        row.set_margin_top(3);
        row.set_margin_bottom(3);
        let stack = gtk::Stack::new();
        stack.set_size_request(LIST_THUMB, LIST_THUMB);
        stack.add_css_class("photo-tile");
        let placeholder = gtk::Image::from_icon_name(&icons::resolve("image-x-generic-symbolic"));
        super::scale::icon(&placeholder, 24);
        placeholder.set_opacity(0.4);
        stack.add_named(&placeholder, Some("placeholder"));
        let picture = gtk::Image::new();
        picture.set_pixel_size(LIST_THUMB);
        stack.add_named(&picture, Some("image"));
        stack.set_visible_child_name("placeholder");
        row.append(&stack);
        let name = gtk::Label::new(photo.path.file_name().map(|n| n.to_string_lossy()).as_deref());
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        row.append(&name);
        let when = gtk::Label::new(Some(&photo_time(photo.modified, time_only)));
        when.add_css_class("dim-label");
        when.add_css_class("numeric");
        row.append(&when);
        let size = gtk::Label::new(Some(&glib::format_size(photo.size)));
        size.add_css_class("dim-label");
        size.add_css_class("numeric");
        size.set_width_chars(8);
        size.set_xalign(1.0);
        row.append(&size);
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        super::scale::icon(&check, 20);
        check.add_css_class("photo-check");
        check.set_valign(gtk::Align::Center);
        check.set_visible(false);
        row.append(&check);

        let child = gtk::FlowBoxChild::new();
        child.add_css_class("photo-row");
        child.set_child(Some(&row));
        self.wire_tile(&child, &photo.path);
        Tile { path: photo.path.clone(), child, stack, picture, check, selected: false }
    }

    /// Thumbnails off the main thread, many at once: each comes from the
    /// disk cache, or the photo's built-in preview (the first ~60 KB), or
    /// the whole photo, in that order (see `core::thumbs`). Tiles fill in
    /// batch order. Stops as soon as a newer scan starts.
    fn load_thumbnails(self: &Rc<Self>, generation: u64, photos: Vec<Photo>, total: usize) {
        // Reading a preview is mostly waiting on Wi-Fi, so many go at once
        // (16 previews: 3.7 s, against ~1.5 s per whole photo one by one).
        const BATCH: usize = 16;
        let weak = Rc::downgrade(self);
        let dir = thumbs::cache_dir();
        glib::spawn_future_local(async move {
            let count = photos.len();
            let alive = |w: &std::rc::Weak<Self>| w.upgrade().filter(|t| t.state.borrow().photo_generation == generation);
            let mut start = 0;
            while start < count {
                if alive(&weak).is_none() {
                    return;
                }
                let end = (start + BATCH).min(count);
                // All of the batch is decoding in the thread pool at once.
                let jobs: Vec<_> = photos[start..end]
                    .iter()
                    .map(|p| {
                        let (p, dir) = (p.clone(), dir.clone());
                        gio::spawn_blocking(move || decode_thumbnail(&p, &dir))
                    })
                    .collect();
                for (offset, job) in jobs.into_iter().enumerate() {
                    let decoded = job.await.ok().flatten();
                    let Some(this) = alive(&weak) else { return };
                    let path = &photos[start + offset].path;
                    if let Some((bytes, w, h, stride, alpha)) = decoded {
                        let format = if alpha { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
                        let tex: gdk::Texture = gdk::MemoryTexture::new(w, h, format, &bytes, stride).upcast();
                        let mut s = this.state.borrow_mut();
                        // The view may have been rebuilt meanwhile: find the tile by path.
                        if let Some(t) = s.tiles.iter().find(|t| &t.path == path) {
                            t.picture.set_paintable(Some(&tex));
                            t.stack.set_visible_child_name("image");
                        }
                        s.thumbs.insert(path.clone(), tex);
                    }
                }
                if let Some(this) = alive(&weak) {
                    let have = this.state.borrow().thumbs.len().min(total);
                    this.photo_status.set_label(&format!("Loading {have}/{total} photos…"));
                }
                start = end;
            }
            if let Some(this) = alive(&weak) {
                this.photo_status.set_label(&format!("{total} photos"));
                this.update_selection_buttons();
            }
        });
    }

    // ── Drag-select ────────────────────────────────────────────────

    fn wire_drag_select(self: &Rc<Self>) {
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        let weak = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, x, y| {
            let Some(this) = weak.upgrade() else { return };
            let before = this.state.borrow().tiles.iter().map(|t| t.selected).collect();
            let scrolled_at = this.photo_scroll.vadjustment().value();
            this.stop_drag();
            this.state.borrow_mut().drag =
                Some(Drag { start: (x, y), offset: (0.0, 0.0), scrolled_at, before, active: false, tick: None });
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |g, ox, oy| {
            let Some(this) = weak.upgrade() else { return };
            let scrolled_at = this.photo_scroll.vadjustment().value();
            let start_now = {
                let mut s = this.state.borrow_mut();
                let Some(d) = s.drag.as_mut() else { return };
                d.offset = (ox, oy);
                d.scrolled_at = scrolled_at;
                let start_now = !d.active && ox.hypot(oy) >= DRAG_START;
                if start_now {
                    d.active = true;
                }
                start_now
            };
            if start_now {
                // Ours from here: the tile under the press doesn't toggle.
                g.set_state(gtk::EventSequenceState::Claimed);
                let weak = Rc::downgrade(&this);
                let tick = glib::timeout_add_local(Duration::from_millis(40), move || {
                    let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break };
                    this.autoscroll();
                    glib::ControlFlow::Continue
                });
                if let Some(d) = this.state.borrow_mut().drag.as_mut() {
                    d.tick = Some(tick);
                }
            }
            this.update_drag();
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                this.stop_drag();
            }
        });
        self.photo_area.add_controller(drag);
    }

    fn stop_drag(&self) {
        if let Some(d) = self.state.borrow_mut().drag.take()
            && let Some(t) = d.tick
        {
            t.remove();
        }
    }

    /// The pointer's point in the photo area, allowing for scrolling since
    /// the last motion.
    fn drag_point(&self) -> Option<((f64, f64), (f64, f64))> {
        let s = self.state.borrow();
        let d = s.drag.as_ref().filter(|d| d.active)?;
        let scrolled = self.photo_scroll.vadjustment().value() - d.scrolled_at;
        Some((d.start, (d.start.0 + d.offset.0, d.start.1 + d.offset.1 + scrolled)))
    }

    /// Select every photo from the one the drag started on to the one under
    /// the pointer, in the order shown (no box: Ethan, 2026-11-08).
    fn update_drag(&self) {
        let Some((a, b)) = self.drag_point() else { return };
        {
            let mut s = self.state.borrow_mut();
            let s = &mut *s;
            let Some(d) = s.drag.as_ref() else { return };
            let rects: Vec<files::Rect> = s
                .tiles
                .iter()
                .map(|t| {
                    t.child.compute_bounds(&self.photo_area).map_or((0.0, 0.0, 0.0, 0.0), |r| {
                        (r.x() as f64, r.y() as f64, r.width() as f64, r.height() as f64)
                    })
                })
                .collect();
            let (Some(i), Some(j)) = (files::photo_at(&rects, a), files::photo_at(&rects, b)) else { return };
            let (lo, hi) = (i.min(j), i.max(j));
            for (k, t) in s.tiles.iter_mut().enumerate() {
                let on = d.before.get(k).copied().unwrap_or(false) || (lo..=hi).contains(&k);
                if on != t.selected {
                    t.set_selected(on);
                }
            }
        }
        self.update_selection_buttons();
    }

    /// While dragging near the top or bottom edge, scroll that way.
    fn autoscroll(&self) {
        let Some((_, p)) = self.drag_point() else { return };
        let adj = self.photo_scroll.vadjustment();
        let y = p.1 - adj.value();
        let edge = 48.0;
        let step = if y < edge {
            -(edge - y).min(edge) / 2.0
        } else if y > adj.page_size() - edge {
            (y - (adj.page_size() - edge)).min(edge) / 2.0
        } else {
            return;
        };
        let to = (adj.value() + step).clamp(adj.lower(), (adj.upper() - adj.page_size()).max(adj.lower()));
        if to != adj.value() {
            adj.set_value(to);
            self.update_drag();
        }
    }

    fn toggle_photo(&self, path: &Path) {
        {
            let mut s = self.state.borrow_mut();
            if let Some(t) = s.tiles.iter_mut().find(|t| t.path == path) {
                let on = !t.selected;
                t.set_selected(on);
            }
            s.select_anchor = Some(path.to_path_buf());
        }
        self.update_selection_buttons();
    }

    /// Shift+click: select every photo between the last one clicked and
    /// this one, adding to what is already selected (the anchor stays put,
    /// so another Shift+click extends from the same photo).
    fn select_range_to(&self, path: &Path) {
        let range = {
            let s = self.state.borrow();
            let order: Vec<PathBuf> = s.tiles.iter().map(|t| t.path.clone()).collect();
            s.select_anchor.as_deref().and_then(|a| files::shift_range(&order, a, path))
        };
        let Some(range) = range else {
            // Nothing clicked yet (or that photo is gone): a plain click.
            self.toggle_photo(path);
            return;
        };
        for t in &mut self.state.borrow_mut().tiles[range] {
            if !t.selected {
                t.set_selected(true);
            }
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

    /// The status line under the photos: how many, or how many selected.
    fn show_photo_count(&self) {
        let total = self.state.borrow().photos.len();
        self.photo_status.set_label(&if total > 0 { format!("{total} photos") } else { "No photos found".into() });
        self.update_selection_buttons();
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

    /// Right-click menu for `path`. Copy and Save act on `targets`: the
    /// selected files when `path` is one of them, else `path` alone.
    #[allow(clippy::too_many_arguments)]
    fn file_menu(
        self: &Rc<Self>,
        anchor: &gtk::Widget,
        x: f64,
        y: f64,
        path: &Path,
        is_dir: bool,
        targets: Vec<PathBuf>,
        status: gtk::Label,
    ) {
        let (copy_label, save_label) = (files::copy_menu_label(targets.len()), files::save_menu_label(targets.len()));
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
        if !targets.is_empty() {
            let (weak, t, st) = (Rc::downgrade(self), targets.clone(), status.clone());
            items.push((
                copy_label.as_str(),
                Rc::new(move || {
                    if let Some(this) = weak.upgrade() {
                        this.copy_paths(&t, &st);
                    }
                }),
            ));
            let (weak, st) = (Rc::downgrade(self), status);
            items.push((
                save_label.as_str(),
                Rc::new(move || {
                    if let Some(this) = weak.upgrade() {
                        this.save_dialog(targets.clone(), st.clone());
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
        self.read_folder(path, false);
    }

    /// List `path` into Browse; `fresh` reads it from the phone, past sshfs's cache.
    fn read_folder(self: &Rc<Self>, path: &str, fresh: bool) {
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
            let Ok((watch, result)) = gio::spawn_blocking(move || read_directory(&d, fresh)).await else { return };
            let Some(this) = weak.upgrade() else { return };
            {
                let mut s = this.state.borrow_mut();
                if s.dir_generation != generation || s.current_path != dir {
                    return;
                }
                s.dir_watch = Some(watch);
                s.listing = result.as_ref().ok().cloned();
            }
            match result {
                Ok(listing) => this.show_listing(&listing),
                Err(e) => this.status_bar.set_label(&e),
            }
        });
    }

    /// The open folder changed on the phone: list it again, and redraw only
    /// when the listing differs, keeping the selection.
    fn refresh_directory(self: &Rc<Self>) {
        let now = self.clock();
        let (dir, generation) = {
            let mut s = self.state.borrow_mut();
            if s.current_path.is_empty() || s.scanning_since.is_some_and(|t| now - t < STALLED_SECONDS) {
                return;
            }
            s.scanning_since = Some(now);
            (s.current_path.clone(), s.dir_generation)
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let d = PathBuf::from(&dir);
            let result = gio::spawn_blocking(move || read_directory(&d, false)).await;
            let Some(this) = weak.upgrade() else { return };
            this.state.borrow_mut().scanning_since = None;
            let Ok((mut watch, Ok(listing))) = result else { return };
            {
                let mut s = this.state.borrow_mut();
                if s.dir_generation != generation || s.current_path != dir {
                    return;
                }
                if let Some(old) = &s.dir_watch {
                    watch.carry_settling(old);
                }
                s.dir_watch = Some(watch);
                if s.listing.as_ref() == Some(&listing) {
                    return;
                }
                s.listing = Some(listing.clone());
            }
            let selected: HashSet<PathBuf> = this
                .state
                .borrow()
                .entries
                .iter()
                .filter(|(r, _)| r.is_selected())
                .map(|(_, e)| e.path.clone())
                .collect();
            this.clear_listing();
            this.show_listing(&listing);
            let rows: Vec<gtk::ListBoxRow> = this
                .state
                .borrow()
                .entries
                .iter()
                .filter(|(_, e)| selected.contains(&e.path))
                .map(|(r, _)| r.clone())
                .collect();
            for row in rows {
                this.file_list.select_row(Some(&row));
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
            super::scale::icon(&icon, 24);
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
                    if !anchor.is_selected() {
                        this.file_list.unselect_all();
                        this.file_list.select_row(Some(&anchor));
                    }
                    let targets = this.selected_browse_files();
                    this.file_menu(anchor.upcast_ref(), x, y, &entry.path, entry.is_dir, targets, this.status_bar.clone());
                }
            });
            row.add_controller(right);
            self.file_list.append(&row);
        }
        self.status_bar.set_label(&listing.summary());
    }

    /// The selected files in the browse list, top to bottom. Folders are
    /// left out: copying a whole folder off the phone isn't offered.
    fn selected_browse_files(&self) -> Vec<PathBuf> {
        self.state
            .borrow()
            .entries
            .iter()
            .filter(|(row, e)| !e.is_dir && row.is_selected())
            .map(|(_, e)| e.path.clone())
            .collect()
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

    /// The Refresh buttons: read the phone itself, not sshfs's copy.
    fn refresh_view(self: &Rc<Self>) {
        if self.showing_photos() {
            if self.state.borrow().mounted {
                self.rescan_photos(true);
            }
            return;
        }
        let path = self.state.borrow().current_path.clone();
        if !path.is_empty() {
            self.read_folder(&path, true);
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

fn sort_icon(oldest_first: bool) -> &'static str {
    if oldest_first { "view-sort-ascending-symbolic" } else { "view-sort-descending-symbolic" }
}

fn sort_tooltip(oldest_first: bool) -> &'static str {
    if oldest_first { "Oldest first (click for newest first)" } else { "Newest first (click for oldest first)" }
}

fn local(t: i64) -> Option<glib::DateTime> {
    glib::DateTime::from_unix_local(t).ok()
}

/// A local calendar day, for grouping.
fn day_key(t: i64) -> (i32, i32) {
    local(t).map(|d| (d.year(), d.day_of_year())).unwrap_or((0, 0))
}

/// "Today", "Yesterday", "Wednesday, November 5", or with the year when
/// it isn't this year.
fn day_heading(t: i64) -> String {
    let (Some(d), Ok(now)) = (local(t), glib::DateTime::now_local()) else { return String::new() };
    let today = (now.year(), now.day_of_year());
    let yesterday = now.add_days(-1).map(|y| (y.year(), y.day_of_year())).ok();
    let key = (d.year(), d.day_of_year());
    if key == today {
        return "Today".into();
    }
    if Some(key) == yesterday {
        return "Yesterday".into();
    }
    let fmt = if d.year() == now.year() { "%A, %B %-d" } else { "%A, %B %-d, %Y" };
    d.format(fmt).map(|s| s.to_string()).unwrap_or_default()
}

/// "3:42 PM", or "Nov 5, 3:42 PM" without a day heading.
fn photo_time(t: i64, time_only: bool) -> String {
    let Some(d) = local(t) else { return String::new() };
    let fmt = if time_only { "%-l:%M %p" } else { "%b %-d, %-l:%M %p" };
    d.format(fmt).map(|s| s.to_string()).unwrap_or_default()
}

type Raw = (glib::Bytes, i32, i32, usize, bool);

fn raw(pb: &gtk::gdk_pixbuf::Pixbuf) -> Raw {
    (pb.read_pixel_bytes(), pb.width(), pb.height(), pb.rowstride() as usize, pb.has_alpha())
}

/// A photo's thumbnail: a centered square at 2× tile size, upright, as raw
/// pixels the main thread turns into a texture. From the disk cache when
/// there; otherwise made (from the built-in preview when it's big enough,
/// else the whole photo) and cached.
fn decode_thumbnail(photo: &Photo, dir: &Path) -> Option<Raw> {
    use gtk::gdk_pixbuf::Pixbuf;
    let s = THUMB_SIZE * 2;
    let cached = thumbs::cache_file(dir, &photo.path, photo.size, photo.modified);
    if let Ok(pb) = Pixbuf::from_file(&cached)
        && pb.width() == s
        && pb.height() == s
    {
        return Some(raw(&pb));
    }
    let pb = preview_pixbuf(&photo.path, s).or_else(|| whole_pixbuf(&photo.path, s))?;
    let square = center_square(&pb, s)?;
    if std::fs::create_dir_all(dir).is_ok()
        && let Err(e) = square.savev(&cached, "jpeg", &[("quality", "88")])
    {
        log::debug!("thumbnail cache: {e}");
    }
    Some(raw(&square))
}

/// The photo's built-in EXIF preview, upright and without padding bars,
/// when it's at least 3/4 of `s` on its short side.
fn preview_pixbuf(path: &Path, s: i32) -> Option<gtk::gdk_pixbuf::Pixbuf> {
    use gtk::gdk_pixbuf::{PixbufLoader, PixbufRotation};
    use std::io::Read;
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    if ext != "jpg" && ext != "jpeg" {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = Vec::with_capacity(thumbs::HEAD_BYTES);
    (&mut file).take(thumbs::HEAD_BYTES as u64).read_to_end(&mut head).ok()?;
    let p = thumbs::exif_preview(&head)?;
    if p.end() > head.len() {
        (&mut file).take((p.end() - head.len()) as u64).read_to_end(&mut head).ok()?;
    }
    let loader = PixbufLoader::new();
    loader.write(head.get(p.start..p.end())?).ok()?;
    loader.close().ok()?;
    let pb = loader.pixbuf()?;
    if pb.width().min(pb.height()) < s * 3 / 4 {
        return None;
    }
    let (x, y, w, h) = thumbs::preview_content(pb.width(), pb.height(), p.width.zip(p.height));
    let pb = pb.new_subpixbuf(x, y, w, h).copy()?;
    // As gdk-pixbuf's own apply_embedded_orientation.
    match p.orientation {
        2 => pb.flip(true),
        3 => pb.rotate_simple(PixbufRotation::Upsidedown),
        4 => pb.flip(false),
        5 => pb.rotate_simple(PixbufRotation::Counterclockwise).and_then(|r| r.flip(false)),
        6 => pb.rotate_simple(PixbufRotation::Clockwise),
        7 => pb.rotate_simple(PixbufRotation::Clockwise).and_then(|r| r.flip(false)),
        8 => pb.rotate_simple(PixbufRotation::Counterclockwise),
        _ => Some(pb),
    }
}

/// The whole photo, scaled so its short side is `s`, upright.
fn whole_pixbuf(path: &Path, s: i32) -> Option<gtk::gdk_pixbuf::Pixbuf> {
    let (w, h) = super::decodable_image(path)?;
    let k = s as f64 / w.min(h) as f64;
    let (sw, sh) = (((w as f64 * k).round() as i32).max(s), ((h as f64 * k).round() as i32).max(s));
    let pb = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, sw, sh, false).ok()?;
    Some(pb.apply_embedded_orientation().unwrap_or(pb))
}

/// The middle square of `pb`, `s` pixels a side (the tile "covers").
fn center_square(pb: &gtk::gdk_pixbuf::Pixbuf, s: i32) -> Option<gtk::gdk_pixbuf::Pixbuf> {
    let side = pb.width().min(pb.height());
    let sq = pb.new_subpixbuf((pb.width() - side) / 2, (pb.height() - side) / 2, side, side);
    if side == s { sq.copy() } else { sq.scale_simple(s, s, gtk::gdk_pixbuf::InterpType::Bilinear) }
}
