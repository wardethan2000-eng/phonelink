//! The GTK app. Pure logic lives in `core`; this layer only builds widgets,
//! runs D-Bus calls and applies the controllers' effects.

pub mod calls_button;
pub mod clipboard_page;
pub mod conversation_list;
pub mod files_page;
pub mod gm_link;
pub mod gm_pair;
pub mod google_sync;
pub mod icons;
pub mod messages_page;
pub mod notifications_model;
pub mod notifications_panel;
pub mod perf;
pub mod scale;
pub mod settings_page;
pub mod thread_view;
pub mod tray;
pub mod window;

use adw::prelude::*;

use crate::core::compat::{self, Version};
use crate::core::settings::APPLICATION_ID;

/// The Python app's id: while it owns this bus name it is running, and both
/// apps would write the same message store.
const PYTHON_APP_ID: &str = "dev.phonelink.app";

/// Exit with a message when GTK / libadwaita are older than the app needs
/// (GitHub issue #6).
fn check_versions() -> Result<(), String> {
    // The bindings refuse to report versions before initialization.
    if let Err(e) = gtk::init() {
        return Err(format!("PhoneRelay could not start GTK: {e}"));
    }
    if let Err(e) = adw::init() {
        return Err(format!("PhoneRelay could not start libadwaita: {e}"));
    }
    let gtk_v = Version(gtk::major_version(), gtk::minor_version(), gtk::micro_version());
    let adw_v = Version(adw::major_version(), adw::minor_version(), adw::micro_version());
    compat::check(gtk_v, adw_v).map_err(|e| e.message())
}

fn python_app_running() -> bool {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return false;
    };
    bus.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
        Some(&(PYTHON_APP_ID,).to_variant()),
        Some(glib::VariantTy::new("(b)").unwrap()),
        gio::DBusCallFlags::NONE,
        2000,
        gio::Cancellable::NONE,
    )
    .ok()
    .and_then(|r| r.get::<(bool,)>())
    .is_some_and(|(b,)| b)
}

/// `ListBox::remove_all` needs GTK 4.12; the app supports 4.10.
pub fn clear_list_box(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

/// A menu entry: label and what it does.
pub type MenuItem<'a> = (&'a str, std::rc::Rc<dyn Fn()>);

/// A small right-click menu of plain buttons at (x, y) on `anchor`.
pub fn popup_menu(anchor: &gtk::Widget, x: f64, y: f64, items: Vec<MenuItem>) {
    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.set_parent(anchor);
    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    for (label, action) in items {
        let b = gtk::Button::with_label(label);
        b.add_css_class("flat");
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            l.set_xalign(0.0);
        }
        let pop = popover.clone();
        b.connect_clicked(move |_| {
            pop.popdown();
            action();
        });
        menu.append(&b);
    }
    popover.set_child(Some(&menu));
    // A popover must be unparented before its anchor goes away.
    popover.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    popover.popup();
}

const EXTRA_CSS: &str = "
.message-image { border-radius: 18px; background-color: alpha(currentColor, 0.06); }
.message-file-chip { padding: 6px 4px; }
.compose-thumb { border-radius: 12px; }
.compose-file { border-radius: 12px; background-color: alpha(currentColor, 0.08); }
/* Someone typing: three dots bouncing in turn inside a received bubble,
   in em so they follow the message zoom. */
.message-bubble-received.typing-bubble { padding: 0.85em 0.95em; }
.typing-dot { min-width: 0.6em; min-height: 0.6em; margin: 0 0.15em; border-radius: 9999px; background-color: currentColor; opacity: 0.35; animation: typing-bounce 1.2s ease-in-out infinite; }
.typing-dot-2 { animation-delay: 0.15s; }
.typing-dot-3 { animation-delay: 0.3s; }
@keyframes typing-bounce {
  0% { transform: translateY(0); opacity: 0.35; }
  25% { transform: translateY(-0.3em); opacity: 0.9; }
  50% { transform: translateY(0); opacity: 0.35; }
  100% { transform: translateY(0); opacity: 0.35; }
}
flowboxchild.photo-row { border-radius: 8px; padding: 0; }
flowboxchild.photo-row.photo-tile-selected { background-color: alpha(@accent_bg_color, 0.18); }
.conversation-pin { opacity: 0.6; }
/* Settings open: no tab looks selected, the gear looks pressed instead. */
viewswitcher.in-settings button:checked { background: none; box-shadow: none; }
viewswitcher.in-settings button:checked:hover { background-color: alpha(currentColor, 0.07); }
button.settings-open { background-color: alpha(currentColor, 0.1); }
.message-quote { padding: 3px 10px; margin-bottom: 2px; border-left: 3px solid alpha(currentColor, 0.3); border-radius: 4px; background-color: alpha(currentColor, 0.05); }
.reaction-chip { min-height: 22px; padding: 0 8px; margin-top: -4px; border-radius: 11px; background-color: @view_bg_color; box-shadow: 0 0 0 1px alpha(currentColor, 0.15); }
.reaction-chip.reaction-mine { background-color: mix(@view_bg_color, @accent_bg_color, 0.25); box-shadow: 0 0 0 1px @accent_color; }
.reaction-pick { font-size: 1.35em; padding: 2px 6px; min-width: 0; }
.reaction-pick.reaction-mine { background-color: alpha(@accent_bg_color, 0.25); }
/* Conversation list: a name a little over body size, the preview at it. */
.conversation-name { font-size: 1.08em; }
.conversation-preview { font-size: 1em; }
.conversation-avatar, .conversation-avatar-photo { min-width: 46px; min-height: 46px; }
.conversation-avatar { font-size: 18px; }
/* The message box: in em, so it grows with the message zoom. */
.compose-entry { min-height: 2.3em; padding: 0 0.75em; border-radius: 1.15em; }
.compose-button { min-width: 40px; min-height: 40px; padding: 0; border-radius: 20px; }
.compose-button image { -gtk-icon-size: 20px; }
/* Bubbles in em: they keep their shape when the text is zoomed. */
.message-bubble-sent { padding: 0.45em 0.85em; border-radius: 1.15em 1.15em 0.3em 1.15em; }
.message-bubble-received { padding: 0.45em 0.85em; border-radius: 1.15em 1.15em 1.15em 0.3em; }
/* The calls button: a count of missed calls on the icon's corner; green
   with the caller's name while the phone rings. */
.calls-badge { min-width: 15px; min-height: 15px; padding: 0 3px; margin: -5px -7px 0 0; border-radius: 8px; background-color: @error_bg_color; color: @error_fg_color; font-size: 10px; font-weight: bold; }
button.calls-ringing, menubutton.calls-ringing > button { background-color: @success_bg_color; color: @success_fg_color; }
menubutton.calls-ringing > button:hover { background-color: mix(@success_bg_color, white, 0.12); }
menubutton.calls-ringing image { animation: calls-ring 1s ease-in-out infinite; }
@keyframes calls-ring {
  0% { transform: rotate(0deg); }
  10% { transform: rotate(-14deg); }
  20% { transform: rotate(14deg); }
  30% { transform: rotate(-14deg); }
  40% { transform: rotate(0deg); }
  100% { transform: rotate(0deg); }
}
.compose-reply-bar { padding: 6px 4px 6px 10px; border-left: 3px solid @accent_color; border-radius: 4px; background-color: alpha(currentColor, 0.05); }
";

fn load_css() {
    // No beeps: GTK's error bell sounds when a key does nothing (e.g. an
    // arrow key at the end of the search text). Ethan: no noises at all.
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_error_bell(false);
    }
    scale::apply(crate::core::settings::Settings::new().ui_scale(), &app_css());
}

/// The same stylesheet as the Python app, so both look identical, plus the
/// Rust-only additions (pictures shown on their own, file chips).
pub fn app_css() -> String {
    format!("{}\n{}", include_str!("../../../phonelink/style.css"), EXTRA_CSS)
}

/// GTK's CPU renderer unless the user picked one. On Ethan's laptop
/// (2026-11-06, Plasma, Intel) it idled at 132 MB against 167 MB for the
/// default Vulkan renderer (which loads Mesa and its LLVM), with the same
/// freezes and no visible difference. `GSK_RENDERER=vulkan` (or `ngl`)
/// still overrides it. Not under Broadway (the UI demo), which draws only
/// with its own renderer: forcing cairo there gave a blank window.
const DEFAULT_RENDERER: &str = "cairo";

/// A picture's header size, only when it is safe to decode
/// (`attachments::safe_image_size`). Reads the header, not the pixels.
pub(crate) fn decodable_image(path: &std::path::Path) -> Option<(i32, i32)> {
    let (_, w, h) = gtk::gdk_pixbuf::Pixbuf::file_info(path)?;
    if crate::core::attachments::safe_image_size(w, h) {
        Some((w, h))
    } else {
        log::warn!("not decoding a {w}×{h} picture: too large");
        None
    }
}

pub fn run() -> glib::ExitCode {
    let broadway = std::env::var("GDK_BACKEND").is_ok_and(|b| b.contains("broadway"));
    if std::env::var_os("GSK_RENDERER").is_none() && !broadway {
        // SAFETY: called first thing on the main thread, before GTK or any
        // other thread starts, so nothing else reads the environment yet.
        unsafe { std::env::set_var("GSK_RENDERER", DEFAULT_RENDERER) };
    }
    if let Err(msg) = check_versions() {
        eprintln!("{msg}");
        return glib::ExitCode::FAILURE;
    }
    if python_app_running() {
        eprintln!(
            "The old Python Phone Link app is running. Quit it first (tray icon → Quit): \
             both apps use the same message store."
        );
        return glib::ExitCode::FAILURE;
    }
    crate::core::private_tmp::tighten_existing_data(
        &crate::core::settings::home_dir(),
        std::env::var_os("XDG_CACHE_HOME").map(std::path::PathBuf::from),
    );
    let app = adw::Application::builder().application_id(APPLICATION_ID).build();
    app.connect_startup(|_| load_css());
    app.connect_activate(|app| {
        // A second launch (or the window hidden to the tray) shows it again.
        if let Some(w) = app.active_window().or_else(|| app.windows().into_iter().next()) {
            w.present();
            return;
        }
        let started = std::time::Instant::now();
        let win = window::Window::new(app);
        win.window.present();
        // Stalls are counted from the first frame on; startup is reported
        // on its own (the Python baselines did not include it).
        let frames = std::cell::Cell::new(0);
        win.window.add_tick_callback(move |_, _| {
            frames.set(frames.get() + 1);
            if frames.get() < 2 {
                return glib::ControlFlow::Continue;
            }
            perf::started(started);
            glib::ControlFlow::Break
        });
        // The window owns everything through its signal handlers; keep the
        // controller alive for the app's lifetime.
        std::mem::forget(win);
    });
    // GApplication would reject our own flags; it gets none.
    app.run_with_args::<&str>(&[])
}
