//! The GTK app. Pure logic lives in `core`; this layer only builds widgets,
//! runs D-Bus calls and applies the controllers' effects.

pub mod conversation_list;
pub mod messages_page;
pub mod thread_view;
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
        return Err(format!("Phone Link could not start GTK: {e}"));
    }
    if let Err(e) = adw::init() {
        return Err(format!("Phone Link could not start libadwaita: {e}"));
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

const EXTRA_CSS: &str = "
.message-image { border-radius: 18px; background-color: alpha(currentColor, 0.06); }
.message-file-chip { padding: 6px 4px; }
";

fn load_css() {
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

pub fn run() -> glib::ExitCode {
    if let Err(msg) = check_versions() {
        eprintln!("{msg}");
        return glib::ExitCode::FAILURE;
    }
    if python_app_running() {
        eprintln!(
            "The Python Phone Link app is running. Quit it first (tray icon → Quit): \
             both apps use the same message store."
        );
        return glib::ExitCode::FAILURE;
    }
    let app = adw::Application::builder().application_id(APPLICATION_ID).build();
    app.connect_startup(|_| load_css());
    app.connect_activate(|app| {
        if let Some(w) = app.active_window() {
            w.present();
            return;
        }
        let win = window::Window::new(app);
        win.window.present();
        // The window owns everything through its signal handlers; keep the
        // controller alive for the app's lifetime.
        std::mem::forget(win);
    });
    // GApplication would reject our own flags; it gets none.
    app.run_with_args::<&str>(&[])
}
