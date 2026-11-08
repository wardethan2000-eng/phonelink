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

use adw::prelude::*;
use gtk::{glib, pango};

use crate::core::contacts::contact_photo_path;
use crate::core::messages::{ChatAction, ChatFlags};
use crate::core::models::Conversation;
use crate::core::search::{self, Hit};

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
    let src = if !name.is_empty() {
        name
    } else if !phone.is_empty() {
        phone
    } else {
        "?"
    };
    src.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default()
}

fn build_avatar(name: &str, phone: &str, is_group: bool, group_count: usize) -> gtk::Widget {
    if !is_group && let Some(path) = contact_photo_path(phone) {
        let picture = gtk::Picture::new();
        picture.set_size_request(40, 40);
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.add_css_class("conversation-avatar-photo");
        // Decoded off the main thread at 2× (80 px): full-size contact
        // photos decoded per row made the first list build stall.
        let target = picture.clone();
        glib::spawn_future_local(async move {
            let decoded = gtk::gio::spawn_blocking(move || {
                super::decodable_image(std::path::Path::new(&path))?;
                let pb = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(&path, 80, 80, true).ok()?;
                Some((pb.read_pixel_bytes(), pb.width(), pb.height(), pb.rowstride() as usize, pb.has_alpha()))
            })
            .await;
            if let Ok(Some((bytes, w, h, stride, alpha))) = decoded {
                let format = if alpha { gtk::gdk::MemoryFormat::R8g8b8a8 } else { gtk::gdk::MemoryFormat::R8g8b8 };
                let tex = gtk::gdk::MemoryTexture::new(w, h, format, &bytes, stride);
                target.set_paintable(Some(&tex));
            }
        });
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
    /// Every member's number, for number-like searches.
    addresses: Vec<String>,
    time: String,
    preview: String,
    last_message: String,
    is_read: bool,
    pinned: bool,
    muted: bool,
    /// Rebuild the row when this changes (group, size, initial).
    avatar_key: (bool, usize, String),
}

impl Summary {
    fn of(c: &Conversation, flags: ChatFlags) -> Self {
        Self {
            pinned: flags.pinned,
            muted: flags.muted,
            name: display_text(c),
            address: c.address.clone(),
            addresses: c.addresses.clone(),
            time: c.time_label(),
            preview: c.preview(),
            last_message: c.last_message.clone(),
            is_read: c.is_read,
            avatar_key: (c.is_group(), c.addresses.len(), initial(&c.display_name, &c.address)),
        }
    }

    /// Matches by name or number (message text is [`Hit`]s).
    fn matches(&self, needle: &str) -> bool {
        let Some(q) = search::fold_query(needle) else {
            return true;
        };
        search::fold_query(&self.name).is_some_and(|n| n.contains(&q))
            || self.address.to_lowercase().contains(&q)
            || search::number_matches(&self.addresses, needle)
    }
}

/// Whether a row shows while searching for `needle`, and the message hit it
/// shows instead of its preview (only when the name or number didn't match).
fn search_view<'a>(summary: &Summary, needle: &str, hit: Option<&'a Hit>) -> (bool, Option<&'a Hit>) {
    if needle.trim().is_empty() {
        return (true, None);
    }
    if summary.matches(needle) {
        return (true, None);
    }
    (hit.is_some(), hit)
}

struct Row {
    row: gtk::ListBoxRow,
    name: gtk::Label,
    time: gtk::Label,
    preview: gtk::Label,
    dot: gtk::Label,
    pin: gtk::Image,
    mute: gtk::Image,
    summary: Summary,
    /// The message hit shown as the preview: (uid, snippet, count).
    showing_hit: Option<(i64, String, usize)>,
}

impl Row {
    fn new(c: &Conversation, flags: ChatFlags) -> Self {
        let summary = Summary::of(c, flags);
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

        // Muted and pinned, beside the time.
        let mute = gtk::Image::from_icon_name("notifications-disabled-symbolic");
        mute.set_pixel_size(12);
        mute.add_css_class("conversation-pin");
        mute.set_tooltip_text(Some("Muted"));
        top.append(&mute);
        let pin = gtk::Image::from_icon_name("view-pin-symbolic");
        pin.set_pixel_size(12);
        pin.add_css_class("conversation-pin");
        pin.set_tooltip_text(Some("Pinned"));
        top.append(&pin);
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

        let r = Self { row, name, time, preview, dot, pin, mute, summary, showing_hit: None };
        r.show(&r.summary.clone());
        r
    }

    /// Show or hide the row for a search, with the matching text in place of
    /// the preview when the match is in a message.
    fn apply_search(&mut self, needle: &str, hit: Option<&Hit>) {
        let (visible, hit) = search_view(&self.summary, needle, hit);
        self.row.set_visible(visible);
        let wanted = hit.map(|h| (h.uid, h.snippet.clone(), h.count));
        if wanted == self.showing_hit {
            return;
        }
        match hit {
            Some(h) => {
                let mut markup = search::bold_markup(&h.snippet, &h.ranges);
                if h.count > 1 {
                    markup = format!("{} matches · {markup}", h.count);
                }
                self.preview.set_use_markup(true);
                self.preview.set_markup(&markup);
            }
            None => {
                self.preview.set_use_markup(false);
                self.preview.set_label(&self.summary.preview);
            }
        }
        self.showing_hit = wanted;
    }

    fn show(&self, s: &Summary) {
        self.pin.set_visible(s.pinned);
        self.mute.set_visible(s.muted);
        self.name.set_label(&s.name);
        self.time.set_label(&s.time);
        self.preview.set_use_markup(false);
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
    /// Message-text matches for `search`, by thread (from the page).
    hits: HashMap<i64, Hit>,
    /// For contact suggestions while searching.
    contacts: HashMap<String, String>,
    conversation_phones: Vec<String>,
    /// Set while the list is being rearranged, so moves don't count as clicks.
    rebuilding: bool,
    last_emitted: Option<i64>,
    /// Where a Shift+click range starts: the last plain or Ctrl click.
    anchor: Option<i64>,
    /// Pin, mute and archive state by thread (Google Messages).
    flags: HashMap<i64, ChatFlags>,
    /// Showing the archive.
    archive_shown: bool,
}

/// Rows created in the first fill, then per later call.
const FIRST_ROWS: usize = 30;
const MORE_ROWS: usize = 15;

type SelectedHandler = Box<dyn Fn(i64)>;

/// Everything else the list asks of the page.
pub enum ListEvent {
    /// Start (or open) a chat with a suggested contact or typed number.
    Start {
        address: String,
        name: String,
    },
    /// Start (or open) a group chat with these people.
    StartGroup {
        addresses: Vec<String>,
    },
    /// Pin, mute, archive or block a chat (its menu).
    ChatAction {
        thread_id: i64,
        action: ChatAction,
    },
    /// Show the archived chats (true) or the others.
    ShowArchived(bool),
    Rename {
        thread_id: i64,
        address: String,
    },
    Delete {
        thread_id: i64,
    },
    /// Several chats selected with Shift/Ctrl+click.
    DeleteMany {
        thread_ids: Vec<i64>,
    },
    ImportContacts,
    /// The search text changed; the page answers with [`ConversationList::set_hits`].
    Search(String),
}

type EventHandler = Rc<dyn Fn(ListEvent)>;

pub struct ConversationList {
    pub widget: gtk::Box,
    listbox: gtk::ListBox,
    state: RefCell<State>,
    on_selected: RefCell<Option<SelectedHandler>>,
    on_event: RefCell<Option<EventHandler>>,
    search_entry: gtk::SearchEntry,
    contacts_header: gtk::Label,
    contacts_list: gtk::ListBox,
    /// "N selected · Cancel · Delete", while more than one chat is selected.
    selection_bar: gtk::ActionBar,
    selection_label: gtk::Label,
    /// "Archived chats (N)" / "Back to chats", under the list.
    archive_button: gtk::Button,
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
        let new_btn = gtk::Button::from_icon_name("list-add-symbolic");
        new_btn.set_tooltip_text(Some("New message or group"));
        search_box.append(&new_btn);
        let import_btn = gtk::Button::from_icon_name("system-users-symbolic");
        import_btn.set_tooltip_text(Some("Import contacts"));
        search_box.append(&import_btn);
        widget.append(&search_box);

        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_vexpand(true);
        scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        widget.append(&scrolled);

        let listbox = gtk::ListBox::new();
        // Shift+click selects a range and Ctrl+click toggles, for deleting
        // several chats at once; a plain click still opens one.
        listbox.set_selection_mode(gtk::SelectionMode::Multiple);
        listbox.set_activate_on_single_click(true);
        listbox.set_placeholder(Some(&placeholder()));
        let scroll_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        scroll_box.append(&listbox);
        // Contacts to start a chat with, shown while searching.
        let contacts_header = gtk::Label::new(Some("Contacts"));
        contacts_header.set_halign(gtk::Align::Start);
        contacts_header.set_margin_start(12);
        contacts_header.set_margin_top(12);
        contacts_header.set_margin_bottom(4);
        contacts_header.add_css_class("heading");
        contacts_header.add_css_class("dim-label");
        contacts_header.set_visible(false);
        scroll_box.append(&contacts_header);
        let contacts_list = gtk::ListBox::new();
        contacts_list.set_selection_mode(gtk::SelectionMode::None);
        contacts_list.set_activate_on_single_click(true);
        scroll_box.append(&contacts_list);
        scrolled.set_child(Some(&scroll_box));

        let archive_button = gtk::Button::new();
        archive_button.add_css_class("flat");
        archive_button.set_margin_top(4);
        archive_button.set_margin_bottom(4);
        archive_button.set_margin_start(8);
        archive_button.set_margin_end(8);
        archive_button.set_visible(false);
        widget.append(&archive_button);

        let selection_bar = gtk::ActionBar::new();
        let selection_label = gtk::Label::new(None);
        selection_label.add_css_class("heading");
        selection_bar.pack_start(&selection_label);
        let delete_selected = gtk::Button::with_label("Delete");
        delete_selected.add_css_class("destructive-action");
        selection_bar.pack_end(&delete_selected);
        let cancel_selection = gtk::Button::with_label("Cancel");
        selection_bar.pack_end(&cancel_selection);
        selection_bar.set_revealed(false);
        widget.append(&selection_bar);

        let this = Rc::new(Self {
            widget,
            listbox: listbox.clone(),
            state: RefCell::default(),
            on_selected: RefCell::default(),
            on_event: RefCell::default(),
            search_entry: search.clone(),
            contacts_header,
            contacts_list: contacts_list.clone(),
            selection_bar,
            selection_label,
            archive_button: archive_button.clone(),
        });
        let weak = Rc::downgrade(&this);
        archive_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let on = !this.state.borrow().archive_shown;
                this.emit(ListEvent::ShowArchived(on));
            }
        });
        let weak = Rc::downgrade(&this);
        delete_selected.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let thread_ids = this.selected_threads();
                this.emit(ListEvent::DeleteMany { thread_ids });
            }
        });
        let weak = Rc::downgrade(&this);
        cancel_selection.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.collapse_selection();
            }
        });
        // Shift+click selects from the last clicked chat to this one;
        // Ctrl+click toggles one. Done here rather than by GtkListBox,
        // whose range anchor is lost whenever rows move (they move on every
        // new message).
        let clicks = gtk::GestureClick::new();
        clicks.set_button(gtk::gdk::BUTTON_PRIMARY);
        clicks.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        clicks.connect_pressed(move |g, _, _, y| {
            let Some(this) = weak.upgrade() else { return };
            let state = g.current_event_state();
            let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
            let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            let Some(row) = this.listbox.row_at_y(y as i32) else {
                return;
            };
            let Some(tid) = this.thread_of(&row) else {
                return;
            };
            g.set_state(gtk::EventSequenceState::Claimed);
            if !shift && !ctrl {
                // A plain click: just this chat, opened. (In multiple mode
                // GtkListBox would add it to the selection instead.)
                this.state.borrow_mut().anchor = Some(tid);
                let only = this.selected_threads() == [tid];
                if !only {
                    this.listbox.unselect_all();
                    this.listbox.select_row(Some(&row));
                }
                row.grab_focus();
                return;
            }
            if ctrl && !shift {
                if row.is_selected() {
                    this.listbox.unselect_row(&row);
                } else {
                    this.listbox.select_row(Some(&row));
                }
                this.state.borrow_mut().anchor = Some(tid);
                return;
            }
            let anchor = {
                let st = this.state.borrow();
                st.anchor.or(st.last_emitted)
            };
            let Some(from) = anchor.and_then(|a| this.state.borrow().rows.get(&a).map(|r| r.row.index())) else {
                this.listbox.select_row(Some(&row));
                return;
            };
            let (lo, hi) = (from.min(row.index()), from.max(row.index()));
            if !ctrl {
                this.listbox.unselect_all();
            }
            for i in lo..=hi {
                if let Some(r) = this.listbox.row_at_index(i)
                    && r.is_visible()
                {
                    this.listbox.select_row(Some(&r));
                }
            }
        });
        listbox.add_controller(clicks);

        // Esc leaves multi-select, keeping the open chat selected.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, _| match (key, weak.upgrade()) {
            (gtk::gdk::Key::Escape, Some(this)) if this.selected_threads().len() > 1 => {
                this.collapse_selection();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        listbox.add_controller(keys);
        let weak = Rc::downgrade(&this);
        new_btn.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.new_chat_dialog();
            }
        });
        let weak = Rc::downgrade(&this);
        import_btn.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.emit(ListEvent::ImportContacts);
            }
        });
        let weak = Rc::downgrade(&this);
        contacts_list.connect_row_activated(move |_, row| {
            let Some(this) = weak.upgrade() else { return };
            let (name, address) = {
                let st = this.state.borrow();
                let s = crate::core::contacts::contact_suggestions(
                    &st.search.to_lowercase(),
                    &st.contacts,
                    &st.conversation_phones,
                );
                match usize::try_from(row.index()).ok().and_then(|i| s.get(i).cloned()) {
                    Some(x) => x,
                    None => return,
                }
            };
            this.search_entry.set_text("");
            this.emit(ListEvent::Start { address, name });
        });
        // Right-click a conversation: rename its number, or delete it.
        let right = gtk::GestureClick::new();
        right.set_button(gtk::gdk::BUTTON_SECONDARY);
        let weak = Rc::downgrade(&this);
        right.connect_pressed(move |g, _, x, y| {
            let Some(this) = weak.upgrade() else { return };
            let Some(row) = this.listbox.row_at_y(y as i32) else {
                return;
            };
            let Some(tid) = usize::try_from(row.index()).ok().and_then(|i| this.state.borrow().order.get(i).copied())
            else {
                return;
            };
            g.set_state(gtk::EventSequenceState::Claimed);
            let selected = this.selected_threads();
            if selected.len() > 1 && selected.contains(&tid) {
                this.many_menu(selected, x, y);
                return;
            }
            this.select_only(&row);
            this.row_menu(&row, tid, x, y);
        });
        listbox.add_controller(right);

        let weak = Rc::downgrade(&this);
        listbox.connect_selected_rows_changed(move |lb| {
            let Some(this) = weak.upgrade() else { return };
            let rows = lb.selected_rows();
            if let Ok(st) = this.state.try_borrow()
                && st.rebuilding
            {
                return;
            }
            this.update_selection_bar(rows.len());
            if let [only] = rows.as_slice() {
                this.row_chosen(only);
            }
        });
        let weak = Rc::downgrade(&this);
        listbox.connect_row_activated(move |lb, row| {
            if let Some(this) = weak.upgrade()
                && lb.selected_rows().len() <= 1
            {
                this.row_chosen(row);
            }
        });
        let weak = Rc::downgrade(&this);
        search.connect_search_changed(move |entry| {
            if let Some(this) = weak.upgrade() {
                let text = entry.text().to_string();
                this.set_search(&text);
                this.emit(ListEvent::Search(text));
            }
        });
        this
    }

    pub fn connect_selected(&self, f: impl Fn(i64) + 'static) {
        *self.on_selected.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_event(&self, f: impl Fn(ListEvent) + 'static) {
        *self.on_event.borrow_mut() = Some(Rc::new(f));
    }

    fn emit(&self, e: ListEvent) {
        let handler = self.on_event.borrow().clone();
        if let Some(f) = handler {
            f(e);
        }
    }

    fn thread_of(&self, row: &gtk::ListBoxRow) -> Option<i64> {
        let st = self.state.try_borrow().ok()?;
        usize::try_from(row.index()).ok().and_then(|i| st.order.get(i).copied())
    }

    /// The selected chats, in list order.
    pub fn selected_threads(&self) -> Vec<i64> {
        let mut rows = self.listbox.selected_rows();
        rows.sort_by_key(|r| r.index());
        rows.iter().filter_map(|r| self.thread_of(r)).collect()
    }

    fn update_selection_bar(&self, count: usize) {
        self.selection_label.set_label(&format!("{count} selected"));
        self.selection_bar.set_revealed(count > 1);
    }

    fn select_only(&self, row: &gtk::ListBoxRow) {
        if let Ok(mut st) = self.state.try_borrow_mut() {
            st.rebuilding = true;
        }
        self.listbox.unselect_all();
        if let Ok(mut st) = self.state.try_borrow_mut() {
            st.rebuilding = false;
        }
        self.listbox.select_row(Some(row));
    }

    /// Back to one selected chat: the open one.
    fn collapse_selection(&self) {
        let open = self.state.borrow().last_emitted;
        self.listbox.unselect_all();
        self.update_selection_bar(0);
        if let Some(tid) = open {
            self.select_thread(tid);
        }
    }

    fn many_menu(self: &Rc<Self>, thread_ids: Vec<i64>, x: f64, y: f64) {
        let popover = gtk::Popover::new();
        popover.set_parent(&self.listbox);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        let delete = gtk::Button::with_label(&format!("Delete {} conversations", thread_ids.len()));
        delete.add_css_class("flat");
        delete.add_css_class("destructive-action");
        popover.set_child(Some(&delete));
        let (weak, pop) = (Rc::downgrade(self), popover.clone());
        delete.connect_clicked(move |_| {
            pop.popdown();
            if let Some(this) = weak.upgrade() {
                this.emit(ListEvent::DeleteMany { thread_ids: thread_ids.clone() });
            }
        });
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    fn row_menu(self: &Rc<Self>, row: &gtk::ListBoxRow, thread_id: i64, x: f64, y: f64) {
        let address = self.state.borrow().rows.get(&thread_id).map(|r| r.summary.address.clone()).unwrap_or_default();
        let popover = gtk::Popover::new();
        popover.set_has_arrow(true);
        popover.set_parent(&self.listbox);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let flags = self.state.borrow().flags.get(&thread_id).copied().unwrap_or_default();
        if flags.gm {
            let items = [
                if flags.pinned { ("Unpin", ChatAction::Unpin) } else { ("Pin to top", ChatAction::Pin) },
                if flags.muted { ("Unmute", ChatAction::Unmute) } else { ("Mute", ChatAction::Mute) },
                if flags.archived { ("Unarchive", ChatAction::Unarchive) } else { ("Archive", ChatAction::Archive) },
                ("Block…", ChatAction::Block),
                ("Report spam…", ChatAction::Spam),
            ];
            for (label, action) in items {
                let b = gtk::Button::with_label(label);
                b.add_css_class("flat");
                if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                    l.set_xalign(0.0);
                }
                let (weak, pop) = (Rc::downgrade(self), popover.clone());
                b.connect_clicked(move |_| {
                    pop.popdown();
                    if let Some(this) = weak.upgrade() {
                        this.emit(ListEvent::ChatAction { thread_id, action });
                    }
                });
                menu.append(&b);
            }
            menu.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }
        let rename = gtk::Button::with_label("Set contact name");
        rename.add_css_class("flat");
        let delete = gtk::Button::with_label("Delete conversation");
        delete.add_css_class("flat");
        delete.add_css_class("destructive-action");
        for b in [&rename, &delete] {
            if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                l.set_xalign(0.0);
            }
        }
        menu.append(&rename);
        menu.append(&delete);
        popover.set_child(Some(&menu));
        let (weak, pop) = (Rc::downgrade(self), popover.clone());
        rename.connect_clicked(move |_| {
            pop.popdown();
            if let Some(this) = weak.upgrade() {
                this.emit(ListEvent::Rename { thread_id, address: address.clone() });
            }
        });
        let (weak, pop) = (Rc::downgrade(self), popover.clone());
        delete.connect_clicked(move |_| {
            pop.popdown();
            if let Some(this) = weak.upgrade() {
                this.emit(ListEvent::Delete { thread_id });
            }
        });
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        let _ = row;
        popover.popup();
    }

    /// The contact names used for suggestions.
    /// Pin, mute and archive state, and the archive button: `archive` is
    /// (archived chats, showing them), or None when there is no archive
    /// (KDE Connect).
    pub fn set_flags(&self, flags: HashMap<i64, ChatFlags>, archive: Option<(usize, bool)>) {
        let mut st = self.state.borrow_mut();
        st.flags = flags;
        st.archive_shown = archive.is_some_and(|a| a.1);
        match archive {
            Some((_, true)) => {
                self.archive_button.set_label("← Back to chats");
                self.archive_button.set_visible(true);
            }
            Some((n, false)) if n > 0 => {
                self.archive_button.set_label(&format!("Archived chats ({n})"));
                self.archive_button.set_visible(true);
            }
            _ => self.archive_button.set_visible(false),
        }
    }

    /// "+": pick one person or several (a group) to text.
    fn new_chat_dialog(self: &Rc<Self>) {
        let window = adw::Window::new();
        window.set_title(Some("New message"));
        window.set_modal(true);
        window.set_default_size(380, 520);
        if let Some(root) = self.widget.root().and_downcast::<gtk::Window>() {
            window.set_transient_for(Some(&root));
        }
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header = adw::HeaderBar::new();
        header.set_show_end_title_buttons(false);
        let cancel = gtk::Button::with_label("Cancel");
        header.pack_start(&cancel);
        let start = gtk::Button::with_label("Start");
        start.add_css_class("suggested-action");
        start.set_sensitive(false);
        header.pack_end(&start);
        column.append(&header);

        let chosen_box = gtk::FlowBox::new();
        chosen_box.set_selection_mode(gtk::SelectionMode::None);
        chosen_box.set_margin_start(12);
        chosen_box.set_margin_end(12);
        chosen_box.set_margin_top(8);
        chosen_box.set_column_spacing(6);
        chosen_box.set_row_spacing(6);
        column.append(&chosen_box);
        let entry = gtk::SearchEntry::new();
        entry.set_placeholder_text(Some("Name or number (add several for a group)"));
        entry.set_margin_start(12);
        entry.set_margin_end(12);
        entry.set_margin_top(8);
        entry.set_margin_bottom(8);
        column.append(&entry);
        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_vexpand(true);
        scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.set_activate_on_single_click(true);
        list.add_css_class("navigation-sidebar");
        scrolled.set_child(Some(&list));
        column.append(&scrolled);
        window.set_content(Some(&column));

        // (name, number) chosen so far, and the suggestions shown.
        let chosen: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
        let shown: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
        let contacts = self.state.borrow().contacts.clone();
        let refresh_chosen: Rc<dyn Fn()> = {
            let (chosen, chosen_box, start) = (chosen.clone(), chosen_box.clone(), start.clone());
            Rc::new(move || {
                while let Some(c) = chosen_box.first_child() {
                    chosen_box.remove(&c);
                }
                for (name, number) in chosen.borrow().iter() {
                    let chip = gtk::Box::new(gtk::Orientation::Horizontal, 2);
                    chip.add_css_class("card");
                    let l = gtk::Label::new(Some(if name.is_empty() { number } else { name }));
                    l.set_margin_start(8);
                    l.set_tooltip_text(Some(number));
                    chip.append(&l);
                    let x = gtk::Button::from_icon_name("window-close-symbolic");
                    x.add_css_class("flat");
                    x.add_css_class("circular");
                    x.set_tooltip_text(Some("Remove"));
                    let (chosen, again, start, number) =
                        (chosen.clone(), chosen_box.clone(), start.clone(), number.clone());
                    x.connect_clicked(move |b| {
                        chosen.borrow_mut().retain(|(_, n)| *n != number);
                        // The chip (inside its FlowBox child) goes.
                        if let Some(child) = b.parent().and_then(|chip| chip.parent()) {
                            again.remove(&child);
                        }
                        let n = chosen.borrow().len();
                        start.set_sensitive(n > 0);
                        start.set_label(if n > 1 { "Start group" } else { "Start" });
                    });
                    chip.append(&x);
                    chosen_box.append(&chip);
                }
                start.set_sensitive(!chosen.borrow().is_empty());
                start.set_label(if chosen.borrow().len() > 1 { "Start group" } else { "Start" });
            })
        };
        let refresh_list: Rc<dyn Fn(&str)> = {
            let (list, shown) = (list.clone(), shown.clone());
            Rc::new(move |q: &str| {
                while let Some(r) = list.row_at_index(0) {
                    list.remove(&r);
                }
                let found = crate::core::contacts::contact_suggestions(&q.to_lowercase(), &contacts, &[]);
                for (name, phone) in &found {
                    list.append(&suggestion_row(name, phone));
                }
                *shown.borrow_mut() = found;
            })
        };
        let add: Rc<dyn Fn((String, String))> = {
            let (chosen, entry, refresh_chosen) = (chosen.clone(), entry.clone(), refresh_chosen.clone());
            Rc::new(move |(name, number): (String, String)| {
                let n = crate::core::contacts::normalize_phone(&number);
                let dup = chosen.borrow().iter().any(|(_, x)| crate::core::contacts::normalize_phone(x) == n);
                if !dup && !number.trim().is_empty() {
                    chosen.borrow_mut().push((name, number));
                }
                entry.set_text("");
                refresh_chosen();
            })
        };
        {
            let refresh_list = refresh_list.clone();
            entry.connect_search_changed(move |e| refresh_list(&e.text()));
        }
        {
            let (add, shown) = (add.clone(), shown.clone());
            list.connect_row_activated(move |_, row| {
                let pick = usize::try_from(row.index()).ok().and_then(|i| shown.borrow().get(i).cloned());
                if let Some(p) = pick {
                    add(p);
                }
            });
        }
        {
            let (add, shown) = (add.clone(), shown.clone());
            entry.connect_activate(move |_| {
                let first = shown.borrow().first().cloned();
                if let Some(p) = first {
                    add(p);
                }
            });
        }
        {
            let w = window.clone();
            cancel.connect_clicked(move |_| w.close());
        }
        {
            let (w, weak, chosen) = (window.clone(), Rc::downgrade(self), chosen.clone());
            start.connect_clicked(move |_| {
                let people = chosen.borrow().clone();
                w.close();
                let Some(this) = weak.upgrade() else { return };
                match people.as_slice() {
                    [] => {}
                    [(name, number)] => this.emit(ListEvent::Start { address: number.clone(), name: name.clone() }),
                    _ => this.emit(ListEvent::StartGroup { addresses: people.iter().map(|p| p.1.clone()).collect() }),
                }
            });
        }
        let esc = gtk::EventControllerKey::new();
        {
            let w = window.clone();
            esc.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    w.close();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        window.add_controller(esc);
        window.present();
        entry.grab_focus();
    }

    pub fn set_contacts(&self, contacts: HashMap<String, String>) {
        self.state.borrow_mut().contacts = contacts;
        self.update_suggestions();
    }

    fn update_suggestions(&self) {
        while let Some(row) = self.contacts_list.row_at_index(0) {
            self.contacts_list.remove(&row);
        }
        let suggestions = {
            let st = self.state.borrow();
            crate::core::contacts::contact_suggestions(&st.search.to_lowercase(), &st.contacts, &st.conversation_phones)
        };
        self.contacts_header.set_visible(!suggestions.is_empty());
        for (name, phone) in suggestions {
            self.contacts_list.append(&suggestion_row(&name, &phone));
        }
    }

    fn row_chosen(&self, row: &gtk::ListBoxRow) {
        let tid = {
            // Busy means the list itself is moving rows (GTK emits
            // row-selected synchronously from remove/insert).
            let Ok(st) = self.state.try_borrow() else {
                return;
            };
            if st.rebuilding {
                return;
            }
            let Some(&tid) = usize::try_from(row.index()).ok().and_then(|i| st.order.get(i)) else {
                return;
            };
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
        // "No conversations yet" is wrong while a search just has no match.
        if needle.is_empty() {
            self.listbox.set_placeholder(Some(&placeholder()));
        } else {
            self.listbox.set_placeholder(None::<&gtk::Widget>);
        }
        {
            let mut guard = self.state.borrow_mut();
            let st = &mut *guard;
            st.search = needle.to_string();
            // The old hits are for other text; the page sends new ones.
            st.hits.clear();
            for row in st.rows.values_mut() {
                row.apply_search(needle, None);
            }
        }
        self.update_suggestions();
    }

    /// The text being searched for ("" when not searching).
    pub fn search_text(&self) -> String {
        self.state.borrow().search.clone()
    }

    /// Message-text matches for `needle`; ignored if the search has moved on.
    pub fn set_hits(&self, needle: &str, hits: HashMap<i64, Hit>) {
        let mut guard = self.state.borrow_mut();
        let st = &mut *guard;
        if st.search != needle {
            return;
        }
        st.hits = hits;
        for (tid, row) in st.rows.iter_mut() {
            row.apply_search(needle, st.hits.get(tid));
        }
    }

    /// For a chat opened from a message match: the search text and the
    /// message to show.
    pub fn reveal_for(&self, thread_id: i64) -> Option<(String, i64)> {
        let st = self.state.borrow();
        let row = st.rows.get(&thread_id)?;
        let (uid, _, _) = row.showing_hit.as_ref()?;
        Some((st.search.clone(), *uid))
    }

    /// Show `conversations` (any order; sorted newest first here), updating
    /// rows in place. New rows are created at most [`FIRST_ROWS`] on the
    /// first call and [`MORE_ROWS`] per call after; returns true while rows
    /// are still missing, and the caller calls again from an idle callback.
    /// (Building all 326 rows at once stalled startup 496 ms on the laptop.)
    pub fn set_conversations(&self, conversations: &[&Conversation]) -> bool {
        let mut convs: Vec<&Conversation> = conversations.to_vec();
        let mut guard = self.state.borrow_mut();
        let st = &mut *guard;
        let flags_of = |tid: i64| st.flags.get(&tid).copied().unwrap_or_default();
        // Pinned chats first, then newest first.
        convs.sort_by_key(|c| std::cmp::Reverse((flags_of(c.thread_id).pinned, c.sort_key())));
        st.rebuilding = true;
        let selected: Vec<i64> = self
            .listbox
            .selected_rows()
            .iter()
            .filter_map(|r| usize::try_from(r.index()).ok().and_then(|i| st.order.get(i).copied()))
            .collect();

        // Drop rows that went away.
        let wanted: std::collections::HashSet<i64> = convs.iter().map(|c| c.thread_id).collect();
        let gone: Vec<i64> = st.rows.keys().copied().filter(|t| !wanted.contains(t)).collect();
        for tid in gone {
            if let Some(row) = st.rows.remove(&tid) {
                self.listbox.remove(&row.row);
            }
        }
        st.order.retain(|t| wanted.contains(t));
        st.conversation_phones = convs
            .iter()
            .map(|c| crate::core::contacts::normalize_phone(&c.address))
            .filter(|n| !n.is_empty())
            .collect();

        let mut budget = if st.rows.is_empty() { FIRST_ROWS } else { MORE_ROWS };
        let mut pending = false;
        let mut pos = 0usize;
        for c in &convs {
            let tid = c.thread_id;
            if !st.rows.contains_key(&tid) {
                if budget == 0 {
                    pending = true;
                    continue;
                }
                budget -= 1;
            }
            let flags = st.flags.get(&tid).copied().unwrap_or_default();
            let summary = Summary::of(c, flags);
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
                        row.showing_hit = None;
                    }
                }
                None => {
                    let row = Row::new(c, flags);
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
            let hit = st.hits.get(&tid);
            if let Some(row) = st.rows.get_mut(&tid) {
                row.apply_search(&st.search, hit);
            }
            pos += 1;
        }

        // Keep the selection on the same conversations after moves (a
        // moved row loses it).
        for tid in &selected {
            if let Some(row) = st.rows.get(tid)
                && !row.row.is_selected()
            {
                self.listbox.select_row(Some(&row.row));
            }
        }
        st.rebuilding = false;
        let searching = !st.search.is_empty();
        drop(guard);
        if searching {
            self.update_suggestions();
        }
        pending
    }

    /// Select a row without telling the listener (the caller already knows).
    pub fn select_thread(&self, thread_id: i64) {
        let row = self.state.borrow().rows.get(&thread_id).map(|r| r.row.clone());
        if let Some(row) = row {
            // During a multi-selection the open chat is one of them already.
            if row.is_selected() {
                return;
            }
            self.state.borrow_mut().rebuilding = true;
            self.listbox.unselect_all();
            self.listbox.select_row(Some(&row));
            let mut st = self.state.borrow_mut();
            st.rebuilding = false;
            st.last_emitted = Some(thread_id);
            drop(st);
            self.update_selection_bar(1);
        }
    }
}

fn suggestion_row(name: &str, phone: &str) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    b.set_margin_top(8);
    b.set_margin_bottom(8);
    b.set_margin_start(12);
    b.set_margin_end(12);
    b.append(&build_avatar(name, phone, false, 1));
    let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
    col.set_hexpand(true);
    let n = gtk::Label::new(Some(if name.is_empty() { phone } else { name }));
    n.set_halign(gtk::Align::Start);
    n.set_ellipsize(pango::EllipsizeMode::End);
    n.set_max_width_chars(20);
    col.append(&n);
    let p = gtk::Label::new(Some(phone));
    p.set_halign(gtk::Align::Start);
    p.add_css_class("dim-label");
    p.add_css_class("caption");
    col.append(&p);
    b.append(&col);
    let icon = gtk::Image::from_icon_name("mail-send-symbolic");
    icon.set_opacity(0.4);
    icon.set_valign(gtk::Align::Center);
    b.append(&icon);
    row.set_child(Some(&b));
    row
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
