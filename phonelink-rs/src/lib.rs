//! PhoneRelay (formerly Phone Link for Linux). See `docs/RUST_PORT.md`.
//!
//! `core` holds everything that has no GTK or D-Bus dependency, so it can be
//! unit tested headless; it is the parity spec against the Python app.
//! `dbus` talks to the KDE Connect daemon over gio; `ui` is the GTK app.

pub mod core;
pub mod dbus;
pub mod ui;
