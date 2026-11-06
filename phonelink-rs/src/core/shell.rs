//! The window shell's rules (port of the logic in `phonelink/ui/main_window.py`):
//! which device is active, and how device refreshes are coalesced.

use super::models::Device;

/// Seconds between the watchdog's device refreshes.
pub const WATCHDOG_SECONDS: u32 = 120;
/// Pull from the phone this long after waking from sleep (Wi-Fi comes back).
pub const RESUME_PULL_DELAY_SECONDS: u32 = 8;

/// Keep the current device while it is still paired; else the first paired
/// and reachable one; else the first paired one; else the first device at
/// all (which may be unpaired).
pub fn pick_active_device<'a>(devices: &'a [Device], current: Option<&str>) -> Option<&'a Device> {
    current
        .and_then(|id| devices.iter().find(|d| d.id == id && d.paired))
        .or_else(|| devices.iter().find(|d| d.paired && d.reachable))
        .or_else(|| devices.iter().find(|d| d.paired))
        .or_else(|| devices.first())
}

/// The devices the header's switcher lists.
pub fn switchable_devices(devices: &[Device]) -> Vec<&Device> {
    devices.iter().filter(|d| d.paired).collect()
}

/// One device refresh in flight, plus at most one queued behind it, however
/// many signals ask.
#[derive(Debug, Default)]
pub struct RefreshGate {
    in_flight: bool,
    pending: bool,
}

impl RefreshGate {
    /// Start a refresh now (`true`), or queue one behind the running one.
    pub fn request(&mut self) -> bool {
        if self.in_flight {
            self.pending = true;
            false
        } else {
            self.in_flight = true;
            true
        }
    }

    /// The running refresh finished. `true` = start the queued one now.
    pub fn finish(&mut self) -> bool {
        self.in_flight = false;
        if std::mem::take(&mut self.pending) {
            self.in_flight = true;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, paired: bool, reachable: bool) -> Device {
        Device { paired, reachable, ..Device::new(id) }
    }

    #[test]
    fn pick_order_matches_python() {
        let devices = [dev("unpaired", false, true), dev("offline", true, false), dev("online", true, true)];
        assert_eq!(pick_active_device(&devices, None).unwrap().id, "online");
        assert_eq!(pick_active_device(&devices, Some("offline")).unwrap().id, "offline");
        // An unpaired current device is dropped.
        assert_eq!(pick_active_device(&devices, Some("unpaired")).unwrap().id, "online");
        let devices = [dev("unpaired", false, true), dev("offline", true, false)];
        assert_eq!(pick_active_device(&devices, None).unwrap().id, "offline");
        // Last resort: devices[0], even unpaired.
        let devices = [dev("new", false, true)];
        assert_eq!(pick_active_device(&devices, None).unwrap().id, "new");
        assert!(pick_active_device(&[], Some("x")).is_none());
        assert_eq!(switchable_devices(&[dev("a", true, false), dev("b", false, true)]).len(), 1);
    }

    #[test]
    fn refreshes_coalesce() {
        let mut g = RefreshGate::default();
        assert!(g.request());
        assert!(!g.request());
        assert!(!g.request());
        assert!(g.finish()); // one queued refresh runs
        assert!(!g.finish());
        assert!(g.request());
    }
}
