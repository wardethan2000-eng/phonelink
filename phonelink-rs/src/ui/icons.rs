//! Theme-aware icon names (port of `phonelink/ui/icons.py`). Some names come
//! from Linux Mint's themes, and other themes draw GTK's red "missing" icon
//! for them; `resolve` picks the first alternative the theme has. Breeze
//! draws its phone glyph small, so device icons are scaled up there.

/// Ordered alternatives for names outside the freedesktop naming spec or
/// that some themes ship under another name.
const FALLBACKS: &[(&str, &[&str])] = &[
    (
        "xsi-notifications-symbolic",
        &["notifications-symbolic", "notification-symbolic", "preferences-system-notifications-symbolic"],
    ),
    ("image-x-generic-symbolic", &["image-x-generic", "image-loading-symbolic"]),
    ("video-x-generic-symbolic", &["video-x-generic", "camera-video-symbolic"]),
    ("audio-x-generic-symbolic", &["audio-x-generic", "audio-symbolic"]),
    ("x-office-document-symbolic", &["x-office-document", "text-x-generic"]),
    ("application-x-executable-symbolic", &["application-x-executable"]),
    ("package-x-generic-symbolic", &["package-x-generic"]),
    ("text-x-generic-symbolic", &["text-x-generic"]),
    ("web-browser-symbolic", &["internet-web-browser-symbolic", "web-browser", "globe-symbolic"]),
    ("display-brightness-symbolic", &["display-brightness", "brightness-high-symbolic"]),
    ("dialog-error-symbolic", &["state-error-symbolic", "error-symbolic"]),
    ("dialog-warning-symbolic", &["state-warning-symbolic", "messagebox_warning-symbolic"]),
    ("dialog-information-symbolic", &["state-information-symbolic", "info-symbolic"]),
    // Breeze has no gear by this name; its emblem one is tiny.
    ("preferences-system-symbolic", &["settings-configure-symbolic", "configure-symbolic", "emblem-system-symbolic"]),
    // Find Phone: a phone (Breeze's location icon looked like a photo).
    ("phone-ring-symbolic", &["smartphone-symbolic", "phone-symbolic", "find-location-symbolic"]),
    ("view-pin-symbolic", &["pin-symbolic", "window-pin-symbolic"]),
];

fn theme() -> Option<gtk::IconTheme> {
    gtk::gdk::Display::default().map(|d| gtk::IconTheme::for_display(&d))
}

/// The requested icon name, or the best fallback the theme has.
pub fn resolve(name: &str) -> String {
    let Some(theme) = theme() else { return name.to_string() };
    if theme.has_icon(name) {
        return name.to_string();
    }
    FALLBACKS
        .iter()
        .find(|(n, _)| *n == name)
        .and_then(|(_, alts)| alts.iter().find(|a| theme.has_icon(a)))
        .map_or_else(|| name.to_string(), |a| a.to_string())
}

fn breeze() -> bool {
    gtk::Settings::default().and_then(|s| s.gtk_icon_theme_name()).is_some_and(|n| n.to_lowercase().contains("breeze"))
}

/// Device icons at `size`, 1.3× on Breeze.
pub fn device_icon_size(size: i32) -> i32 {
    if breeze() { (size as f64 * 1.3).round() as i32 } else { size }
}

/// The send button's paper plane: Breeze draws it as `document-send`
/// (its `mail-send` is an envelope), Adwaita as `mail-send`.
pub fn send_icon() -> String {
    resolve(if breeze() { "document-send-symbolic" } else { "mail-send-symbolic" })
}

/// The bundled app icon (see `build.rs`), named after the app id.
pub fn register_app_icon() {
    if let Err(e) = gio::resources_register_include!("icons.gresource") {
        log::warn!("app icon: {e}");
        return;
    }
    if let Some(d) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&d).add_resource_path("/io/github/wardethan2000_eng/phonelink/icons");
    }
    gtk::Window::set_default_icon_name(crate::core::settings::APPLICATION_ID);
}
