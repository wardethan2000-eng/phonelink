//! The conversation list: left column of the Messages tab (port of
//! `phonelink/ui/conversation_list.py`).
//!
//! Rows are kept in a map and moved into place on each update, so a new
//! message touches one row instead of rebuilding the list. Order is kept by
//! position (no `ListBox` sort function), and search hides rows rather than
//! using a filter function, so a row never needs to carry its data.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::pango;
use gtk::prelude::*;

use crate::core::contacts::contact_photo_path;
use crate::core::models::Conversation;

/// Number of avatar tint classes in style.css (`.avatar-hue-0` …).
const AVATAR_HUE_COUNT: u32 = 10;

/// zlib's CRC-32, as Python's `zlib.crc32`, so a contact keeps the tint it
/// had in the Python app.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn avatar_hue_class(key: &str) -> String {
    let key = if key.trim().is_empty() { "?".to_string() } else { key.trim().to_lowercase() };
    format!("avatar-hue-{}", crc32(key.as_bytes()) % AVATAR_HUE_COUNT)
}

/// First letter, uppercased, as Python's `(name or phone or "?")[0].upper()`.
fn initial(name: &str, phone: &str) -> String {
    let src = if !name.is_empty() { name } else if !phone.is_empty() { phone } else { "?" };
    src.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default()
}

fn build_avatar(name: &str, phone: &str, is_group: bool, group_count: usize) -> gtk::Widget {
    if !is_group && let Some(path) = contact_photo_path(phone) {
        let picture = gtk::Picture::for_filename(path);
        picture.set_size_request(40, 40);
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.add_css_class("conversation-avatar-photo");
        return picture.upcast();
    }
    let label = gtk::Label::new(None);
    label.set_size_request(40, 40);
    if is_group {
        label.set_label(&group_count.to_string());
        label.add_css_class("conversation-avatar-group");
    } else {
        label.set_label(&initial(name, phone));
    }
    label.add_css_class("conversation-avatar");
    label.add_css_class(&avatar_hue_class(if name.is_empty() { phone } else { name }));
    label.set_halign(gtk::Align::Center);
    label.set_valign(gtk::Align::Center);
    label.upcast()
}

fn display_text(c: &Conversation) -> String {
    if c.is_group() {
        if c.display_name.is_empty() { "Group".into() } else { c.display_name.clone() }
    } else if !c.display_name.is_empty() {
        c.display_name.clone()
    } else if !c.address.is_empty() {
        c.address.clone()
    } else {
        "Unknown".into()
    }
}

/// What a row shows; compared to skip no-op updates.
#[derive(Clone, PartialEq)]
struct Summary {
    name: String,
    address: String,
    time: String,
    preview: String,
    last_message: String,
    is_read: bool,
    /// Rebuild the row when this changes (group, size, initial).
    avatar_key: (bool, usize, String),
}

impl Summary {
    fn of(c: &Conversation) -> Self {
        Self {
            name: display_text(c),
            address: c.address.clone(),
            time: c.time_label(),
            preview: c.preview(),
            last_message: c.last_message.clone(),
            is_read: c.is_read,
            avatar_key: (c.is_group(), c.addresses.len(), initial(&c.display_name, &c.address)),
        }
    }

    fn matches(&self, needle: &str) -> bool {
        needle.is_empty()
            || self.name.to_lowercase().contains(needle)
            || self.address.to_lowercase().contains(needle)
            || self.last_message.to_lowercase().contains(needle)
    }
}

struct Row {
    row: gtk::ListBoxRow,
    name: gtk::Label,
    time: gtk::Label,
    preview: gtk::Label,
    dot: gtk::Label,
    summary: Summary,
}

impl Row {
    fn new(c: &Conversation) -> Self {
        let summary = Summary::of(c);
        let row = gtk::ListBoxRow::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        hbox.set_margin_top(10);
        hbox.set_margin_bottom(10);
        hbox.set_margin_start(12);
        hbox.set_margin_end(12);
        row.set_child(Some(&hbox));
        hbox.append(&build_avatar(&c.display_name, &c.address, c.is_group(), c.addresses.len()));

        let text_col = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text_col.set_hexpand(true);
        hbox.append(&text_col);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        text_col.append(&top);

        let name = gtk::Label::new(None);
        name.set_xalign(0.0);
        name.set_halign(gtk::Align::Start);
        name.set_hexpand(true);
        name.set_ellipsize(pango::EllipsizeMode::End);
        name.set_max_width_chars(20);
        top.append(&name);

        let time = gtk::Label::new(None);
        time.add_css_class("dim-label");
        time.add_css_class("caption");
        top.append(&time);

        let preview = gtk::Label::new(None);
        preview.set_xalign(0.0);
        preview.set_halign(gtk::Align::Start);
        preview.set_ellipsize(pango::EllipsizeMode::End);
        preview.set_max_width_chars(30);
        preview.add_css_class("dim-label");
        preview.add_css_class("caption");
        text_col.append(&preview);

        // Reserve trailing width so read/unread changes don't resize the row.
        let dot = gtk::Label::new(Some("●"));
        dot.add_css_class("unread-dot");
        dot.set_xalign(0.5);
        dot.set_valign(gtk::Align::Center);
        hbox.append(&dot);

        let r = Self { row, name, time, preview, dot, summary };
        r.show(&r.summary.clone());
        r
    }

    fn show(&self, s: &Summary) {
        self.name.set_label(&s.name);
        self.time.set_label(&s.time);
        self.preview.set_label(&s.preview);
        if s.is_read {
            self.name.remove_css_class("conversation-name-unread");
            self.dot.add_css_class("unread-dot-hidden");
        } else {
            self.name.add_css_class("conversation-name-unread");
            self.dot.remove_css_class("unread-dot-hidden");
        }
    }
}

#[derive(Default)]
struct State {
    rows: HashMap<i64, Row>,
    /// Thread ids in list order (index = row position).
    order: Vec<i64>,
    search: String,
    /// Set while the list is being rearranged, so moves don't count as clicks.
    rebuilding: bool,
    last_emitted: Option<i64>,
}

type SelectedHandler = Box<dyn Fn(i64)>;

pub struct ConversationList {
    pub widget: gtk::Box,
    listbox: gtk::ListBox,
    state: RefCell<State>,
    on_selected: RefCell<Option<SelectedHandler>>,
}

impl ConversationList {
    pub fn new() -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.set_size_request(280, -1);
        widget.set_hexpand(false);

        let search_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        search_box.set_margin_top(8);
        search_box.set_margin_bottom(4);
        search_box.set_margin_start(8);
        search_box.set_margin_end(8);
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search conversations or contacts…"));
        search.set_hexpand(true);
        search_box.append(&search);
        // TODO(Phase 3): new-message and contact-sync buttons, contact
        // suggestions, right-click rename/delete.
        widget.append(&search_box);

        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_vexpand(true);
        scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        widget.append(&scrolled);

        let listbox = gtk::ListBox::new();
        listbox.set_selection_mode(gtk::SelectionMode::Single);
        listbox.set_activate_on_single_click(true);
        listbox.set_placeholder(Some(&placeholder()));
        scrolled.set_child(Some(&listbox));

        let this = Rc::new(Self {
            widget,
            listbox: listbox.clone(),
            state: RefCell::default(),
            on_selected: RefCell::default(),
        });

        let weak = Rc::downgrade(&this);
        listbox.connect_row_selected(move |_, row| {
            if let (Some(this), Some(row)) = (weak.upgrade(), row) {
                this.row_chosen(row);
            }
        });
        let weak = Rc::downgrade(&this);
        listbox.connect_row_activated(move |_, row| {
            if let Some(this) = weak.upgrade() {
                this.row_chosen(row);
            }
        });
        let weak = Rc::downgrade(&this);
        search.connect_search_changed(move |entry| {
            if let Some(this) = weak.upgrade() {
                this.set_search(&entry.text().to_lowercase());
            }
        });
        this
    }

    pub fn connect_selected(&self, f: impl Fn(i64) + 'static) {
        *self.on_selected.borrow_mut() = Some(Box::new(f));
    }

    fn row_chosen(&self, row: &gtk::ListBoxRow) {
        let tid = {
            // Busy means the list itself is moving rows (GTK emits
            // row-selected synchronously from remove/insert).
            let Ok(st) = self.state.try_borrow() else { return };
            if st.rebuilding {
                return;
            }
            let Some(&tid) = usize::try_from(row.index()).ok().and_then(|i| st.order.get(i)) else { return };
            if st.last_emitted == Some(tid) {
                return;
            }
            tid
        };
        self.state.borrow_mut().last_emitted = Some(tid);
        if let Some(f) = self.on_selected.borrow().as_ref() {
            f(tid);
        }
    }

    fn set_search(&self, needle: &str) {
        let mut st = self.state.borrow_mut();
        st.search = needle.to_string();
        for row in st.rows.values() {
            row.row.set_visible(row.summary.matches(needle));
        }
    }

    /// Show `conversations` (any order; sorted newest first here), updating
    /// rows in place.
    pub fn set_conversations(&self, conversations: &[&Conversation]) {
        let mut convs: Vec<&Conversation> = conversations.to_vec();
        convs.sort_by_key(|c| std::cmp::Reverse(c.sort_key()));

        let mut guard = self.state.borrow_mut();
        let st = &mut *guard;
        st.rebuilding = true;
        let selected = self.listbox.selected_row().and_then(|r| {
            usize::try_from(r.index()).ok().and_then(|i| st.order.get(i).copied())
        });

        // Drop rows that went away.
        let wanted: std::collections::HashSet<i64> = convs.iter().map(|c| c.thread_id).collect();
        let gone: Vec<i64> = st.rows.keys().copied().filter(|t| !wanted.contains(t)).collect();
        for tid in gone {
            if let Some(row) = st.rows.remove(&tid) {
                self.listbox.remove(&row.row);
            }
        }
        st.order.retain(|t| wanted.contains(t));

        for (pos, c) in convs.iter().enumerate() {
            let tid = c.thread_id;
            let summary = Summary::of(c);
            // A different avatar means a new row.
            if st.rows.get(&tid).is_some_and(|r| r.summary.avatar_key != summary.avatar_key)
                && let Some(old) = st.rows.remove(&tid)
            {
                self.listbox.remove(&old.row);
                st.order.retain(|t| *t != tid);
            }
            match st.rows.get_mut(&tid) {
                Some(row) => {
                    if row.summary != summary {
                        row.show(&summary);
                        row.summary = summary;
                    }
                }
                None => {
                    let row = Row::new(c);
                    self.listbox.insert(&row.row, pos as i32);
                    st.order.insert(pos, tid);
                    st.rows.insert(tid, row);
                }
            }
            if st.order.get(pos) != Some(&tid) {
                let widget = st.rows[&tid].row.clone();
                self.listbox.remove(&widget);
                st.order.retain(|t| *t != tid);
                self.listbox.insert(&widget, pos as i32);
                st.order.insert(pos, tid);
            }
            let row = &st.rows[&tid];
            row.row.set_visible(row.summary.matches(&st.search));
        }

        // Keep the selection on the same conversation after moves.
        if let Some(tid) = selected
            && let Some(row) = st.rows.get(&tid)
            && self.listbox.selected_row().as_ref() != Some(&row.row)
        {
            self.listbox.select_row(Some(&row.row));
        }
        st.rebuilding = false;
    }

    /// Select a row without telling the listener (the caller already knows).
    pub fn select_thread(&self, thread_id: i64) {
        let row = self.state.borrow().rows.get(&thread_id).map(|r| r.row.clone());
        if let Some(row) = row {
            self.state.borrow_mut().rebuilding = true;
            self.listbox.select_row(Some(&row));
            let mut st = self.state.borrow_mut();
            st.rebuilding = false;
            st.last_emitted = Some(thread_id);
        }
    }
}

fn placeholder() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
    b.set_valign(gtk::Align::Center);
    b.set_margin_top(48);
    let icon = gtk::Image::from_icon_name("mail-unread-symbolic");
    icon.set_pixel_size(32);
    icon.set_opacity(0.3);
    b.append(&icon);
    let label = gtk::Label::new(Some("No conversations yet"));
    label.add_css_class("dim-label");
    b.append(&label);
    let hint = gtk::Label::new(Some("Waiting for messages from phone…"));
    hint.add_css_class("dim-label");
    hint.add_css_class("caption");
    b.append(&hint);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_zlib() {
        // zlib.crc32(b"hello") == 907060870
        assert_eq!(crc32(b"hello"), 907_060_870);
        assert_eq!(avatar_hue_class("Hello "), format!("avatar-hue-{}", 907_060_870 % 10));
        assert_eq!(avatar_hue_class(""), format!("avatar-hue-{}", crc32(b"?") % 10));
    }

    #[test]
    fn initials() {
        assert_eq!(initial("ethan", "123"), "E");
        assert_eq!(initial("", "+1555"), "+");
        assert_eq!(initial("", ""), "?");
    }
}
