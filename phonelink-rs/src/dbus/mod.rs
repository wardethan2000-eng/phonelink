//! D-Bus layer over gio, on the GLib main loop (no zbus, no tokio; see
//! `docs/RUST_PORT.md`). `variant` and `introspect` are pure and unit
//! tested; `client` and `signals` need a live bus.

pub mod client;
pub mod introspect;
pub mod signals;
pub mod variant;

pub use client::Client;

pub const BUS_NAME: &str = "org.kde.kdeconnect";
pub const DAEMON_PATH: &str = "/modules/kdeconnect";

pub const IFACE_DAEMON: &str = "org.kde.kdeconnect.daemon";
pub const IFACE_DEVICE: &str = "org.kde.kdeconnect.device";
pub const IFACE_BATTERY: &str = "org.kde.kdeconnect.device.battery";
pub const IFACE_NOTIFICATIONS: &str = "org.kde.kdeconnect.device.notifications";
pub const IFACE_NOTIFICATION: &str = "org.kde.kdeconnect.device.notifications.notification";
pub const IFACE_CONVERSATIONS: &str = "org.kde.kdeconnect.device.conversations";
pub const IFACE_SMS: &str = "org.kde.kdeconnect.device.sms";
pub const IFACE_CONTACTS: &str = "org.kde.kdeconnect.device.contacts";
pub const IFACE_SFTP: &str = "org.kde.kdeconnect.device.sftp";
pub const IFACE_SHARE: &str = "org.kde.kdeconnect.device.share";
pub const IFACE_CLIPBOARD: &str = "org.kde.kdeconnect.device.clipboard";
pub const IFACE_CONNECTIVITY: &str = "org.kde.kdeconnect.device.connectivity_report";
pub const IFACE_FINDPHONE: &str = "org.kde.kdeconnect.device.findmyphone";
pub const IFACE_TELEPHONY: &str = "org.kde.kdeconnect.device.telephony";
pub const IFACE_PROPS: &str = "org.freedesktop.DBus.Properties";

pub const CALL_TIMEOUT_MS: i32 = 5000;
pub const MOUNT_TIMEOUT_MS: i32 = 30000;

/// `/modules/kdeconnect/devices/<id>`; plugins hang off it (`/sms`, `/battery`, …).
pub fn device_path(device_id: &str) -> String {
    format!("{DAEMON_PATH}/devices/{device_id}")
}
