//! Startup check for GTK / libadwaita versions (GitHub issue #6).
//!
//! On Linux Mint 21 (GTK 4.6, libadwaita 1.1) the Python app opened a window
//! with only some controls, which did nothing, because it uses widgets from
//! libadwaita 1.4. The Rust app checks first and says plainly what is missing.
//!
//! Hooking this up (Phase 3): call [`check`] with the runtime versions
//! (`gtk::major_version()` etc., `adw::major_version()` etc.) before building
//! any widget. If it fails, print the message and exit with status 1. Also
//! show it in a plain GTK dialog when GTK itself is new enough to open one.
//! The binary must be linked with lazy binding (`-C link-arg=-Wl,-z,lazy`).
//! Otherwise a library that lacks a newer symbol makes the dynamic loader
//! fail before `main` runs, and the user gets an "undefined symbol" error
//! instead of this message.

use std::fmt;

/// Lowest versions the app works with. They must match the crate feature
/// levels (`gtk4` `v4_x`, `libadwaita` `v1_y`). Code may not call anything
/// newer than these without a runtime check.
pub const MIN_GTK: Version = Version(4, 10, 0);
pub const MIN_ADW: Version = Version(1, 4, 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooOld {
    pub gtk: Option<Version>,
    pub adw: Option<Version>,
}

impl TooOld {
    /// What the user sees: what is too old, what is needed, what to do.
    pub fn message(&self) -> String {
        let mut lines = vec!["Phone Link can't start: this system's GTK libraries are too old.".to_string(), String::new()];
        if let Some(v) = self.gtk {
            lines.push(format!("  GTK {v} is installed; Phone Link needs {MIN_GTK} or newer."));
        }
        if let Some(v) = self.adw {
            lines.push(format!("  libadwaita {v} is installed; Phone Link needs {MIN_ADW} or newer."));
        }
        lines.push(String::new());
        lines.push(
            "Updating your distribution to a newer release usually fixes this \
             (for example Linux Mint 22 or Ubuntu 24.04 or later)."
                .to_string(),
        );
        lines.join("\n")
    }
}

/// `Ok` when both libraries are new enough.
pub fn check(gtk: Version, adw: Version) -> Result<(), TooOld> {
    let bad = TooOld { gtk: (gtk < MIN_GTK).then_some(gtk), adw: (adw < MIN_ADW).then_some(adw) };
    if bad.gtk.is_none() && bad.adw.is_none() { Ok(()) } else { Err(bad) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_21_is_refused_with_both_versions_named() {
        let err = check(Version(4, 6, 9), Version(1, 1, 7)).unwrap_err();
        let msg = err.message();
        assert!(msg.contains("GTK 4.6.9 is installed"));
        assert!(msg.contains("libadwaita 1.1.7 is installed"));
        assert!(msg.contains("4.10.0 or newer"));
    }

    #[test]
    fn only_the_old_library_is_named() {
        let err = check(Version(4, 14, 2), Version(1, 1, 0)).unwrap_err();
        assert_eq!(err.gtk, None);
        assert!(!err.message().contains("GTK 4.14"));
    }

    #[test]
    fn current_and_minimum_versions_pass() {
        assert!(check(Version(4, 22, 5), Version(1, 9, 4)).is_ok());
        assert!(check(MIN_GTK, MIN_ADW).is_ok());
    }
}
