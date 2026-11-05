//! Signal subscriptions that end when their guard is dropped.
//!
//! The Python kept a list of subscription ids per panel and unsubscribed them
//! by hand on device switch; here a panel keeps its guards in a `Vec` and
//! clears it.

use glib::Variant;

use super::BUS_NAME;

/// Unsubscribes on drop. Holds a strong reference to the connection.
pub type Subscription = gio::SignalSubscription;

/// Subscribe to one daemon signal on one object; `f` gets the parameters.
pub fn subscribe(
    bus: &gio::DBusConnection,
    path: &str,
    iface: &str,
    signal: &str,
    f: impl Fn(&Variant) + 'static,
) -> Subscription {
    bus.subscribe_to_signal(
        Some(BUS_NAME),
        Some(iface),
        Some(signal),
        Some(path),
        None,
        gio::DBusSignalFlags::NONE,
        move |s| f(s.parameters),
    )
}

/// Watches the daemon's bus name; unwatches on drop.
// gio does not export its `WatcherId` type by name, so keep the unwatch call.
pub struct NameWatch(Option<Box<dyn FnOnce()>>);

impl Drop for NameWatch {
    fn drop(&mut self) {
        if let Some(unwatch) = self.0.take() {
            unwatch();
        }
    }
}

/// Call `appeared` when `org.kde.kdeconnect` gets an owner (also right away
/// if it already has one) and `vanished` when it loses it.
pub fn watch_daemon(appeared: impl Fn() + 'static, vanished: impl Fn() + 'static) -> NameWatch {
    let id = gio::bus_watch_name(
        gio::BusType::Session,
        BUS_NAME,
        gio::BusNameWatcherFlags::NONE,
        move |_, _, _| appeared(),
        move |_, _| vanished(),
    );
    NameWatch(Some(Box::new(move || gio::bus_unwatch_name(id))))
}

/// logind's `PrepareForSleep(bool start)` on the system bus: `f(true)` before
/// suspend, `f(false)` on resume (when the catch-up pull runs).
pub async fn watch_sleep(f: impl Fn(bool) + 'static) -> Result<Subscription, glib::Error> {
    let system = gio::bus_get_future(gio::BusType::System).await?;
    Ok(system.subscribe_to_signal(
        Some("org.freedesktop.login1"),
        Some("org.freedesktop.login1.Manager"),
        Some("PrepareForSleep"),
        Some("/org/freedesktop/login1"),
        None,
        gio::DBusSignalFlags::NONE,
        move |s| {
            if let Some((start,)) = s.parameters.get::<(bool,)>() {
                f(start);
            }
        },
    ))
}
