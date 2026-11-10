//! The calls button in the header: a count of missed calls not looked at
//! yet, the caller while the phone rings, and the recent calls in its
//! popover (KDE Connect reports calls only while it runs).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::core::messages::{CallShown, CallsView};
use crate::core::models::{local_from_ms, message_time_label};

type Handler = Rc<dyn Fn()>;
type NumberHandler = Rc<dyn Fn(&str, &str)>;

pub struct CallsButton {
    pub widget: gtk::MenuButton,
    icon: gtk::Image,
    badge: gtk::Label,
    caller: gtk::Label,
    list: gtk::ListBox,
    empty: gtk::Label,
    clear: gtk::Button,
    on_looked: RefCell<Option<Handler>>,
    on_clear: RefCell<Option<Handler>>,
    on_text: RefCell<Option<NumberHandler>>,
}

impl CallsButton {
    pub fn new() -> Rc<Self> {
        let widget = gtk::MenuButton::new();
        widget.set_tooltip_text(Some("Calls"));
        widget.add_css_class("calls-button");
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        // The count sits on the icon's corner.
        let icon = gtk::Image::from_icon_name(&super::icons::resolve("call-start-symbolic"));
        let badge = gtk::Label::new(None);
        badge.add_css_class("calls-badge");
        badge.set_halign(gtk::Align::End);
        badge.set_valign(gtk::Align::Start);
        badge.set_can_target(false);
        badge.set_visible(false);
        let over = gtk::Overlay::new();
        over.set_child(Some(&icon));
        over.add_overlay(&badge);
        content.append(&over);
        let caller = gtk::Label::new(None);
        caller.set_ellipsize(gtk::pango::EllipsizeMode::End);
        caller.set_max_width_chars(20);
        caller.set_visible(false);
        content.append(&caller);
        widget.set_child(Some(&content));

        let popover = gtk::Popover::new();
        let pop_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        pop_box.set_margin_top(6);
        pop_box.set_margin_bottom(6);
        pop_box.set_margin_start(6);
        pop_box.set_margin_end(6);
        pop_box.set_size_request(320, -1);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let title = gtk::Label::new(Some("Calls"));
        title.add_css_class("heading");
        title.set_xalign(0.0);
        title.set_hexpand(true);
        head.append(&title);
        let clear = gtk::Button::with_label("Clear");
        clear.add_css_class("flat");
        clear.set_tooltip_text(Some("Remove every call from this list"));
        head.append(&clear);
        pop_box.append(&head);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_max_content_height(420);
        scroll.set_child(Some(&list));
        pop_box.append(&scroll);
        let empty =
            gtk::Label::new(Some("No calls yet. Calls show here when your phone rings while PhoneRelay is open."));
        empty.set_wrap(true);
        empty.set_max_width_chars(36);
        empty.add_css_class("dim-label");
        empty.set_margin_top(12);
        empty.set_margin_bottom(12);
        pop_box.append(&empty);
        popover.set_child(Some(&pop_box));
        widget.set_popover(Some(&popover));

        let this = Rc::new(Self {
            widget,
            icon,
            badge,
            caller,
            list,
            empty,
            clear: clear.clone(),
            on_looked: RefCell::default(),
            on_clear: RefCell::default(),
            on_text: RefCell::default(),
        });
        // The list was looked at: its missed calls are seen (on closing, so
        // the new ones stay bold while it is open).
        let weak = Rc::downgrade(&this);
        popover.connect_closed(move |_| {
            let f = weak.upgrade().and_then(|t| t.on_looked.borrow().clone());
            if let Some(f) = f {
                f();
            }
        });
        let weak = Rc::downgrade(&this);
        clear.connect_clicked(move |_| {
            let f = weak.upgrade().and_then(|t| t.on_clear.borrow().clone());
            if let Some(f) = f {
                f();
            }
        });
        this.show(&CallsView::default());
        this
    }

    /// `f()` when the list closes after being looked at.
    pub fn connect_looked(&self, f: impl Fn() + 'static) {
        *self.on_looked.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_clear(&self, f: impl Fn() + 'static) {
        *self.on_clear.borrow_mut() = Some(Rc::new(f));
    }

    /// `f(number, name)`: "Text" on a call.
    pub fn connect_text(&self, f: impl Fn(&str, &str) + 'static) {
        *self.on_text.borrow_mut() = Some(Rc::new(f));
    }

    pub fn show(self: &Rc<Self>, view: &CallsView) {
        // The badge: missed calls not looked at.
        self.badge.set_label(&if view.missed > 9 { "9+".to_string() } else { view.missed.to_string() });
        self.badge.set_visible(view.missed > 0);
        // Ringing: the caller, on a green button.
        match &view.ringing {
            Some((who, _)) => {
                self.caller.set_label(&who.name);
                self.caller.set_visible(true);
                self.icon.set_icon_name(Some(&super::icons::resolve("call-incoming-symbolic")));
                self.widget.add_css_class("calls-ringing");
                let from =
                    if who.name != who.number { format!("{} ({})", who.name, who.number) } else { who.name.clone() };
                self.widget.set_tooltip_text(Some(&format!("Incoming call from {from}")));
            }
            None => {
                self.caller.set_visible(false);
                self.icon.set_icon_name(Some(&super::icons::resolve("call-start-symbolic")));
                self.widget.remove_css_class("calls-ringing");
                self.widget.set_tooltip_text(Some(&match view.missed {
                    0 => "Calls".to_string(),
                    1 => "Calls: 1 missed".to_string(),
                    n => format!("Calls: {n} missed"),
                }));
            }
        }
        super::clear_list_box(&self.list);
        let ringing = view.ringing.as_ref().map(|(c, _)| c.date);
        for call in &view.recent {
            self.list.append(&self.row(call, ringing == Some(call.date)));
        }
        self.list.set_visible(!view.recent.is_empty());
        self.empty.set_visible(view.recent.is_empty());
        self.clear.set_sensitive(!view.recent.is_empty());
    }

    fn row(self: &Rc<Self>, call: &CallShown, ringing: bool) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        line.set_margin_top(6);
        line.set_margin_bottom(6);
        line.set_margin_start(10);
        line.set_margin_end(6);
        let icon = gtk::Image::from_icon_name(&super::icons::resolve(if call.missed {
            "call-missed-symbolic"
        } else {
            "call-incoming-symbolic"
        }));
        if call.missed {
            icon.add_css_class("error");
        }
        line.append(&icon);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        let name = gtk::Label::new(Some(&call.name));
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if call.unseen {
            name.add_css_class("heading");
        }
        text.append(&name);
        let what = if ringing {
            "Ringing now".to_string()
        } else {
            let kind = if call.missed { "Missed" } else { "Incoming" };
            format!("{kind} · {}", message_time_label(local_from_ms(call.date), chrono::Local::now()))
        };
        let detail = if call.name != call.number { format!("{} · {what}", call.number) } else { what };
        let sub = gtk::Label::new(Some(&detail));
        sub.set_xalign(0.0);
        sub.set_ellipsize(gtk::pango::EllipsizeMode::End);
        sub.add_css_class("caption");
        sub.add_css_class("dim-label");
        text.append(&sub);
        line.append(&text);
        let back = gtk::Button::from_icon_name("mail-message-new-symbolic");
        back.add_css_class("flat");
        back.set_valign(gtk::Align::Center);
        back.set_tooltip_text(Some("Text them"));
        let weak = Rc::downgrade(self);
        let (number, who) = (call.number.clone(), call.name.clone());
        back.connect_clicked(move |b| {
            let Some(this) = weak.upgrade() else { return };
            if let Some(p) = b.ancestor(gtk::Popover::static_type()).and_downcast::<gtk::Popover>() {
                p.popdown();
            }
            let f = this.on_text.borrow().clone();
            if let Some(f) = f {
                f(&number, if who != number { &who } else { "" });
            }
        });
        line.append(&back);
        row.set_child(Some(&line));
        row
    }
}
