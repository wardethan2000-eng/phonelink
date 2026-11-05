//! Phone Link for Linux — Rust port. See `docs/RUST_PORT.md`.
//!
//! `core` holds everything that has no GTK or D-Bus dependency, so it can be
//! unit tested headless; it is the parity spec against the Python app.

pub mod core;
