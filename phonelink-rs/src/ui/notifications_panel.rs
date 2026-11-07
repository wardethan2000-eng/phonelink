//! The notifications sidebar (port of `phonelink/ui/notifications_panel.py`):
//! a compact list that expands a row in place to the full text, Dismiss and
//! Reply. Rows are diffed by content, so a new notification doesn't close
//! an expanded one.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;

use super::icons;
use super::notifications_model::{NoteEvent, NotificationsModel, epoch_now, notification_from};
use crate::core::models::{Device, Notification};
use crate::core::notifications::{self as notes, NotificationList, Signature};
use crate::core::settings::Settings;

struct Row {
    row: gtk::ListBoxRow,
    revealer: gtk::Revealer,
    chevron: gtk::Image,
    signature: Signature,
}

impl Row {
    fn toggle(&self) {
        let open = !self.revealer.reveals_child();
        self.revealer.set_reveal_child(open);
        self.chevron.set_icon_name(Some(if open { "go-up-symbolic" } else { "go-down-symbolic" }));
    }
}

pub struct NotificationsPanel {
    /// Heading, separator and the list (the split view's sidebar).
    pub widget: gtk::Box,
    stack: gtk::Stack,
    status: adw::StatusPage,
    count: gtk::Label,
    list_box: gtk::ListBox,
    model: Rc<NotificationsModel>,
    settings: Rc<RefCell<Settings>>,
    toast: Rc<dyn Fn(&str)>,
    list: RefCell<NotificationList>,
    rows: RefCell<HashMap<String, Row>>,
    reachable: Cell<bool>,
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    for c in classes {
        l.add_css_class(c);
    }
    l
}

impl NotificationsPanel {
    pub fn new(model: Rc<NotificationsModel>, settings: Rc<RefCell<Settings>>, toast: Rc<dyn Fn(&str)>) -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.set_size_request(300, -1);
        let heading = label("Notifications", &["heading"]);
        heading.set_margin_start(12);
        heading.set_margin_top(10);
        heading.set_margin_bottom(6);
        widget.append(&heading);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        widget.append(&stack);

        let status = adw::StatusPage::new();
        status.set_icon_name(Some(&icons::resolve("xsi-notifications-symbolic")));
        status.set_title("No Notifications");
        status.set_description(Some("No phone linked yet."));
        stack.add_named(&status, Some("status"));

        let list_outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        stack.add_named(&list_outer, Some("list"));
        let hdr = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        hdr.set_margin_start(12);
        hdr.set_margin_end(4);
        hdr.set_margin_top(4);
        hdr.set_margin_bottom(4);
        let count = label("", &["caption", "dim-label"]);
        count.set_hexpand(true);
        hdr.append(&count);
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.add_css_class("flat");
        refresh.set_tooltip_text(Some("Refresh"));
        hdr.append(&refresh);
        let clear = gtk::Button::from_icon_name("edit-clear-all-symbolic");
        clear.add_css_class("flat");
        clear.set_tooltip_text(Some("Dismiss all"));
        hdr.append(&clear);
        list_outer.append(&hdr);
        list_outer.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        let list_box = gtk::ListBox::new();
        list_box.set_selection_mode(gtk::SelectionMode::None);
        let placeholder = adw::StatusPage::new();
        placeholder.set_icon_name(Some(&icons::resolve("xsi-notifications-symbolic")));
        placeholder.set_title("No Notifications");
        placeholder.set_description(Some("Phone has no active notifications."));
        list_box.set_placeholder(Some(&placeholder));
        scroll.set_child(Some(&list_box));
        list_outer.append(&scroll);
        stack.set_visible_child_name("status");

        let this = Rc::new(Self {
            widget,
            stack,
            status,
            count,
            list_box,
            model: model.clone(),
            settings,
            toast,
            list: RefCell::default(),
            rows: RefCell::default(),
            reachable: Cell::new(false),
        });

        let weak = Rc::downgrade(&this);
        model.connect(move |e| {
            if let Some(this) = weak.upgrade() {
                this.on_event(e);
            }
        });
        let weak = Rc::downgrade(&this);
        refresh.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade()
                && this.reachable.get()
            {
                this.model.reload();
            }
        });
        let weak = Rc::downgrade(&this);
        clear.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            let ids = this.list.borrow().dismissable_ids();
            // As Python: the whole list clears, not only the dismissable ones;
            // the rest come back with the next refresh.
            this.list.borrow_mut().clear();
            this.sync();
            this.model.dismiss(ids);
        });
        let weak = Rc::downgrade(&this);
        this.list_box.connect_row_activated(move |_, row| {
            let Some(this) = weak.upgrade() else { return };
            if let Some(r) = this.rows.borrow().values().find(|r| r.row == *row) {
                r.toggle();
            }
        });
        this
    }

    pub fn set_device(&self, device: Option<&Device>) {
        self.reachable.set(device.is_some_and(|d| d.reachable));
        match device {
            Some(d) if d.reachable => self.stack.set_visible_child_name("list"),
            Some(d) => {
                self.status
                    .set_description(Some(&format!("{} is offline.\nReconnect your phone to see notifications.", d.name)));
                self.stack.set_visible_child_name("status");
            }
            None => {
                self.status.set_description(Some("No phone linked yet."));
                self.stack.set_visible_child_name("status");
            }
        }
    }

    /// Settings changed (on/off, hidden apps): re-filter.
    pub fn refilter(self: &Rc<Self>) {
        self.sync();
    }

    fn on_event(self: &Rc<Self>, e: &NoteEvent) {
        let now = epoch_now();
        {
            let mut list = self.list.borrow_mut();
            match e {
                NoteEvent::Loaded(all) => list.replace_all(all.iter().map(|(id, p)| notification_from(id, p, now))),
                NoteEvent::Posted(id, p) => list.upsert(notification_from(id, p, now)),
                NoteEvent::Removed(id) => {
                    list.remove(id);
                }
                NoteEvent::Cleared => list.clear(),
            }
        }
        self.sync();
    }

    /// Update the rows in place: only changed rows are rebuilt.
    fn sync(self: &Rc<Self>) {
        let (enabled, ignored) = {
            let s = self.settings.borrow();
            (s.notifications_enabled(), s.notifications_ignored_apps())
        };
        let list = self.list.borrow();
        let visible = list.visible(enabled, &ignored);
        self.count.set_label(&notes::count_label(enabled, visible.len()));
        let desired: Vec<(String, Signature)> =
            visible.iter().map(|n| (n.public_id.clone(), notes::signature(n))).collect();
        let mut rows = self.rows.borrow_mut();
        let existing: HashMap<String, Signature> = rows.iter().map(|(k, r)| (k.clone(), r.signature.clone())).collect();
        let diff = notes::diff_rows(&existing, &desired);
        for id in diff.remove.iter().chain(&diff.recreate) {
            if let Some(r) = rows.remove(id) {
                self.list_box.remove(&r.row);
            }
        }
        for n in &visible {
            if !rows.contains_key(&n.public_id) {
                let row = self.make_row(n);
                self.list_box.append(&row.row);
                rows.insert(n.public_id.clone(), row);
            }
        }
        // Newest first: move rows that are out of place.
        for (i, n) in visible.iter().enumerate() {
            let row = &rows[&n.public_id].row;
            if row.index() != i as i32 {
                self.list_box.remove(row);
                self.list_box.insert(row, i as i32);
            }
        }
    }

    fn make_row(self: &Rc<Self>, n: &Notification) -> Row {
        let row = gtk::ListBoxRow::new();
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        row.set_child(Some(&outer));

        let summary = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        summary.set_margin_start(12);
        summary.set_margin_end(12);
        summary.set_margin_top(8);
        summary.set_margin_bottom(8);
        outer.append(&summary);
        let icon = gtk::Image::new();
        icon.set_pixel_size(32);
        load_icon(&icon, n);
        summary.append(&icon);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
        text.set_hexpand(true);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let app = label(if n.app_name.is_empty() { "App" } else { &n.app_name }, &["caption", "dim-label"]);
        app.set_hexpand(true);
        app.set_ellipsize(gtk::pango::EllipsizeMode::End);
        top.append(&app);
        top.append(&label(&n.time_label(), &["caption", "dim-label"]));
        text.append(&top);
        let title = label(notes::row_title(n), &["body"]);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.append(&title);
        let snippet = notes::row_snippet(n);
        if !snippet.is_empty() {
            let s = label(snippet, &["caption", "dim-label"]);
            s.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text.append(&s);
        }
        summary.append(&text);
        let chevron = gtk::Image::from_icon_name("go-down-symbolic");
        chevron.set_pixel_size(12);
        chevron.set_opacity(0.5);
        summary.append(&chevron);

        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        outer.append(&revealer);
        let detail = gtk::Box::new(gtk::Orientation::Vertical, 8);
        detail.set_margin_start(54);
        detail.set_margin_end(12);
        detail.set_margin_bottom(10);
        revealer.set_child(Some(&detail));
        if !n.text.is_empty() && n.text != snippet {
            let full = label(&n.text, &["body"]);
            full.set_wrap(true);
            full.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            full.set_selectable(true);
            detail.append(&full);
        }
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        detail.append(&buttons);
        if n.dismissable {
            let dismiss = gtk::Button::with_label("Dismiss");
            dismiss.add_css_class("destructive-action");
            dismiss.add_css_class("flat");
            let (weak, id) = (Rc::downgrade(self), n.public_id.clone());
            dismiss.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.model.dismiss(vec![id.clone()]);
                }
            });
            buttons.append(&dismiss);
        }
        if n.can_reply() {
            let reply_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            reply_box.set_margin_top(4);
            let entry = gtk::Entry::new();
            entry.set_hexpand(true);
            entry.set_placeholder_text(Some("Reply…"));
            reply_box.append(&entry);
            let send = gtk::Button::from_icon_name("mail-send-symbolic");
            send.add_css_class("suggested-action");
            reply_box.append(&send);
            detail.append(&reply_box);
            let (weak, id) = (Rc::downgrade(self), n.public_id.clone());
            let go = Rc::new(move |entry: &gtk::Entry| {
                if let Some(this) = weak.upgrade() {
                    this.send_reply(&id, entry);
                }
            });
            let go2 = go.clone();
            entry.connect_activate(move |e| go2(e));
            let e = entry.clone();
            send.connect_clicked(move |_| go(&e));
        }
        outer.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        Row { row, revealer, chevron, signature: notes::signature(n) }
    }

    /// Unlike Python, the text stays in the box unless the phone took it: a
    /// notification gone in the meantime, or a refused reply, says so.
    fn send_reply(self: &Rc<Self>, notif_id: &str, entry: &gtk::Entry) {
        let text = entry.text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let answerable = self.list.borrow().get(notif_id).is_some_and(|n| n.can_reply());
        if !answerable {
            (self.toast)("That notification is gone from your phone, so the reply wasn't sent");
            return;
        }
        entry.set_sensitive(false);
        let (weak, entry) = (Rc::downgrade(self), entry.clone());
        self.model.reply(notif_id, &text, move |ok| {
            entry.set_sensitive(true);
            if ok {
                entry.set_text("");
            } else if let Some(this) = weak.upgrade() {
                (this.toast)("Couldn't send that reply. Check the phone is connected and try again.");
            }
        });
    }
}

/// The notification's own picture (32 px), else an icon by app name.
fn load_icon(image: &gtk::Image, n: &Notification) {
    if n.has_icon
        && !n.icon_path.is_empty()
        && super::decodable_image(std::path::Path::new(&n.icon_path)).is_some()
        && let Ok(pb) = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(&n.icon_path, 32, 32, true)
    {
        #[allow(deprecated)] // Texture::for_pixbuf: the replacement needs GTK 4.20.
        image.set_paintable(Some(&gtk::gdk::Texture::for_pixbuf(&pb)));
        return;
    }
    image.set_icon_name(Some(&icons::resolve(notes::app_icon_name(&n.app_name))));
}
