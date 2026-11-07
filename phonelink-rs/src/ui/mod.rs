//! The GTK app. Pure logic lives in `core`; this layer only builds widgets,
//! runs D-Bus calls and applies the controllers' effects.

pub mod clipboard_page;
pub mod conversation_list;
pub mod files_page;
pub mod gm_link;
pub mod google_sync;
pub mod icons;
pub mod messages_page;
pub mod notifications_model;
pub mod notifications_panel;
pub mod perf;
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
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return false };
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
/* Settings open: no tab looks selected, the gear looks pressed instead. */
viewswitcher.in-settings button:checked { background: none; box-shadow: none; }
viewswitcher.in-settings button:checked:hover { background-color: alpha(currentColor, 0.07); }
button.settings-open { background-color: alpha(currentColor, 0.1); }
";

fn load_css() {
    // No beeps: GTK's error bell sounds when a key does nothing (e.g. an
    // arrow key at the end of the search text). Ethan: no noises at all.
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_error_bell(false);
    }
    let provider = gtk::CssProvider::new();
    // The same stylesheet as the Python app, so both look identical.
    provider.load_from_data(include_str!("../../../phonelink/style.css"));
    // Rust-only additions (pictures shown on their own, file chips).
    let extra = gtk::CssProvider::new();
    extra.load_from_data(EXTRA_CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        gtk::style_context_add_provider_for_display(&display, &extra, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
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
