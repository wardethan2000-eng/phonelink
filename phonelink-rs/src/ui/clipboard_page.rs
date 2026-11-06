//! The Clipboard tab (port of `phonelink/ui/clipboard_panel.py`): a history
//! of what passed through the desktop clipboard, and a button that sends the
//! current clipboard to the phone. See `core::clipboard` for why it polls.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use chrono::Local;

use crate::core::clipboard::{self, Change, Entry, History, Source};
use crate::core::models::Device;
use crate::dbus::Client;

pub struct ClipboardPage {
    pub widget: gtk::Stack,
    list_box: gtk::ListBox,
    history: RefCell<History>,
    device: RefCell<Option<(Client, String)>>,
    poll: RefCell<Option<glib::SourceId>>,
    started: Instant,
    toast: Rc<dyn Fn(&str)>,
}

fn tool_button(icon: &str, text: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::new();
    b.add_css_class("flat");
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.append(&gtk::Image::from_icon_name(icon));
    row.append(&gtk::Label::new(Some(text)));
    b.set_child(Some(&row));
    b.set_tooltip_text(Some(tooltip));
    b
}

impl ClipboardPage {
    pub fn new(toast: Rc<dyn Fn(&str)>) -> Rc<Self> {
        let widget = gtk::Stack::new();
        widget.set_vexpand(true);
        widget.set_hexpand(true);
        widget.set_transition_type(gtk::StackTransitionType::Crossfade);

        let status = adw::StatusPage::new();
        status.set_icon_name(Some("edit-paste-symbolic"));
        status.set_title("Clipboard Sync");
        status.set_description(Some("No phone linked yet.\nPair your phone to share clipboard between devices."));
        widget.add_named(&status, Some("status"));

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_named(&content, Some("content"));
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        toolbar.set_margin_start(12);
        toolbar.set_margin_end(12);
        toolbar.set_margin_top(8);
        toolbar.set_margin_bottom(8);
        let push = tool_button("go-up-symbolic", "Send Clipboard to Phone", "Push your current PC clipboard text to the phone");
        toolbar.append(&push);
        let pull = tool_button(
            "go-down-symbolic",
            "Read Synced Clipboard",
            "Read the current desktop clipboard text, including content synced in from your phone",
        );
        toolbar.append(&pull);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        toolbar.append(&spacer);
        let clear = gtk::Button::from_icon_name("user-trash-symbolic");
        clear.add_css_class("flat");
        clear.set_tooltip_text(Some("Clear history"));
        toolbar.append(&clear);
        content.append(&toolbar);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        let list_box = gtk::ListBox::new();
        list_box.set_selection_mode(gtk::SelectionMode::None);
        list_box.set_placeholder(Some(&placeholder()));
        scroll.set_child(Some(&list_box));
        content.append(&scroll);
        widget.set_visible_child_name("status");

        let this = Rc::new(Self {
            widget,
            list_box,
            history: RefCell::default(),
            device: RefCell::default(),
            poll: RefCell::default(),
            started: Instant::now(),
            toast,
        });
        let weak = Rc::downgrade(&this);
        push.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.send_to_phone();
            }
        });
        let weak = Rc::downgrade(&this);
        pull.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade()
                && this.device.borrow().is_some()
            {
                this.read_desktop(Source::Synced);
            }
        });
        let weak = Rc::downgrade(&this);
        clear.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.history.borrow_mut().clear();
                this.list_box.remove_all();
            }
        });
        this
    }

    /// Poll only while a phone is connected.
    pub fn set_device(self: &Rc<Self>, client: &Client, device: Option<&Device>) {
        let dev = device.filter(|d| d.reachable).map(|d| (client.clone(), d.id.clone()));
        let was = self.device.borrow().as_ref().map(|(_, id)| id.clone());
        let now = dev.as_ref().map(|(_, id)| id.clone());
        *self.device.borrow_mut() = dev;
        if was == now {
            return;
        }
        if let Some(id) = self.poll.borrow_mut().take() {
            id.remove();
        }
        self.history.borrow_mut().stop();
        if now.is_none() {
            self.widget.set_visible_child_name("status");
            return;
        }
        self.widget.set_visible_child_name("content");
        self.read_desktop(Source::Synced);
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_seconds_local(clipboard::POLL_SECONDS, move || {
            let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break };
            this.read_desktop(Source::Synced);
            glib::ControlFlow::Continue
        });
        *self.poll.borrow_mut() = Some(id);
    }

    fn clock(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn read_desktop(self: &Rc<Self>, source: Source) {
        if !self.history.borrow_mut().begin_read(self.clock()) {
            return;
        }
        let Some(display) = gtk::gdk::Display::default() else {
            self.history.borrow_mut().stop();
            return;
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let text = display.clipboard().read_text_future().await.ok().flatten();
            let Some(this) = weak.upgrade() else { return };
            let change = this.history.borrow_mut().read_done(text.as_deref(), source, Local::now());
            this.show(change);
        });
    }

    /// "Send Clipboard to Phone": the daemon sends the desktop clipboard, so
    /// read it here only to list it.
    fn send_to_phone(self: &Rc<Self>) {
        let Some((client, id)) = self.device.borrow().clone() else { return };
        let Some(display) = gtk::gdk::Display::default() else { return };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(text) = display.clipboard().read_text_future().await.ok().flatten().filter(|t| !t.is_empty())
            else {
                return;
            };
            let sent = client.send_clipboard(&id, &text).await;
            let Some(this) = weak.upgrade() else { return };
            if !sent {
                (this.toast)("Couldn't send the clipboard. Check the phone is connected and try again.");
                return;
            }
            let change = this.history.borrow_mut().sent(&text, Local::now());
            this.show(change);
        });
    }

    fn show(self: &Rc<Self>, change: Change) {
        let Change::Added { dropped_last } = change else { return };
        if dropped_last && let Some(last) = self.list_box.row_at_index(clipboard::MAX_HISTORY as i32 - 1) {
            self.list_box.remove(&last);
        }
        let entry = self.history.borrow().first().cloned();
        if let Some(entry) = entry {
            self.list_box.prepend(&self.make_row(&entry));
        }
    }

    fn make_row(self: &Rc<Self>, entry: &Entry) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        b.set_margin_start(12);
        b.set_margin_end(12);
        b.set_margin_top(8);
        b.set_margin_bottom(8);
        row.set_child(Some(&b));
        let icon = gtk::Image::from_icon_name(entry.source.icon_name());
        icon.set_pixel_size(16);
        icon.set_valign(gtk::Align::Start);
        icon.set_margin_top(4);
        icon.set_opacity(0.6);
        b.append(&icon);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        let preview = gtk::Label::new(Some(&entry.preview()));
        preview.set_xalign(0.0);
        preview.set_wrap(true);
        preview.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        preview.set_max_width_chars(60);
        preview.set_selectable(true);
        text.append(&preview);
        let time = gtk::Label::new(Some(&entry.time_label()));
        time.set_xalign(0.0);
        time.add_css_class("caption");
        time.add_css_class("dim-label");
        text.append(&time);
        b.append(&text);
        let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy.add_css_class("flat");
        copy.set_valign(gtk::Align::Center);
        copy.set_tooltip_text(Some("Copy to clipboard"));
        let (weak, full) = (Rc::downgrade(self), entry.text.clone());
        copy.connect_clicked(move |btn| {
            if let Some(this) = weak.upgrade() {
                this.history.borrow_mut().copied(&full);
                btn.clipboard().set_text(&full);
            }
        });
        b.append(&copy);
        row
    }
}

fn placeholder() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
    b.set_valign(gtk::Align::Center);
    b.set_margin_top(40);
    let icon = gtk::Image::from_icon_name("edit-paste-symbolic");
    icon.set_pixel_size(48);
    icon.set_opacity(0.3);
    b.append(&icon);
    let l = gtk::Label::new(Some("Clipboard history will appear here"));
    l.add_css_class("dim-label");
    b.append(&l);
    b
}
