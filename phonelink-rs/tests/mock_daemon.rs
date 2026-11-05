//! The D-Bus client against a mock `org.kde.kdeconnect` on the session bus.
//!
//! Needs a session bus (the build box has one; it owns the daemon name for the
//! length of the test). Without `DBUS_SESSION_BUS_ADDRESS` the test says so
//! and passes, so a bus-less CI runner does not fail on it.

#[path = "support/mock.rs"]
mod mock;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use mock::*;
use phonelink_rs::dbus::signals;
use phonelink_rs::dbus::variant;
use phonelink_rs::dbus::*;

/// Run the main loop until `done` or a 5 s timeout.
async fn wait_for(done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() && start.elapsed() < Duration::from_secs(5) {
        glib::timeout_future(Duration::from_millis(20)).await;
    }
}

#[test]
fn client_against_mock_daemon() {
    let Ok(address) = std::env::var("DBUS_SESSION_BUS_ADDRESS") else {
        eprintln!("SKIPPED: no session bus (DBUS_SESSION_BUS_ADDRESS unset)");
        return;
    };
    let ctx = glib::MainContext::new();
    ctx.with_thread_default(|| ctx.block_on(scenario(address))).unwrap();
}

async fn scenario(address: String) {
    let client = Client::connect().await.unwrap();
    // Watch the name before the mock exists: `appeared` must fire once it does.
    let appeared = Rc::new(Cell::new(0));
    let vanished = Rc::new(Cell::new(0));
    let _watch = {
        let (a, v) = (appeared.clone(), vanished.clone());
        signals::watch_daemon(move || a.set(a.get() + 1), move || v.set(v.get() + 1))
    };
    assert!(!client.is_daemon_available().await, "a real KDE Connect daemon is on this bus");

    let mock = gio::DBusConnection::for_address_future(
        &address,
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None,
    )
    .await
    .unwrap();
    let seen = Rc::new(Seen::default());
    register_mock(&mock, seen.clone());
    own_name(&mock).await;
    wait_for(|| appeared.get() > 0).await;
    assert_eq!(appeared.get(), 1);
    // gio reports "vanished" once at the start when the name has no owner.
    let vanished_before = vanished.get();
    assert!(client.is_daemon_available().await);

    // Devices and properties; the mock has no battery object, which must come
    // back as "unknown" rather than an error.
    assert_eq!(client.device_ids().await, vec![DEVICE]);
    let d = client.fetch_device(DEVICE).await;
    assert_eq!((d.name.as_str(), d.kind.as_str(), d.reachable, d.paired), ("Mock Phone", "phone", true, true));
    assert_eq!((d.battery_charge, d.battery_charging), (-1, false));
    assert_eq!(client.device_name(DEVICE).await, "Mock Phone");
    assert!(client.is_device_reachable(DEVICE).await);
    assert_eq!(client.battery_charge(DEVICE).await, -1);
    assert_eq!(client.fetch_devices().await.len(), 1);

    // activeConversations → parsed messages.
    let entries = client.active_conversations(DEVICE).await;
    let parsed: Vec<_> = entries.iter().filter_map(variant::parse_message).collect();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[1].addresses.len(), 2);
    assert_eq!(parsed[1].attachments[0].unique_identifier, "IMG.jpg");

    // Methods the mock lacks: introspection says so, and calling one is quiet.
    assert!(!client.supports_conversation_deletion(DEVICE).await);
    assert!(client.supports_method(&device_path(DEVICE), IFACE_CONVERSATIONS, "requestConversation").await);
    assert!(!client.delete_conversation(DEVICE, 615).await);
    client.mark_conversation_as_read(DEVICE, 615).await;
    let node = client.introspect(DAEMON_PATH).await.unwrap();
    assert!(node.interface(IFACE_DAEMON).unwrap().method("devices").is_some());

    // Arguments arrive with the types the daemon expects.
    assert!(client.reply_to_conversation(DEVICE, 615, "hi", &["file:///tmp/a.jpg".into()]).await);
    assert_eq!(*seen.replies.borrow(), vec![(615, "hi".to_string(), 1)]);

    // Signals: a replay arrives as conversationUpdated + conversationLoaded.
    let got = Rc::new(RefCell::new(Vec::new()));
    let loaded = Rc::new(Cell::new(None));
    let path = device_path(DEVICE);
    let sub = {
        let got = got.clone();
        signals::subscribe(client.bus(), &path, IFACE_CONVERSATIONS, "conversationUpdated", move |p| {
            got.borrow_mut().push(variant::parse_message_signal(p).unwrap());
        })
    };
    let sub_loaded = {
        let loaded = loaded.clone();
        signals::subscribe(client.bus(), &path, IFACE_CONVERSATIONS, "conversationLoaded", move |p| {
            loaded.set(variant::first_int(p));
        })
    };
    client.request_conversation(DEVICE, 719, 0, 3).await;
    wait_for(|| loaded.get().is_some()).await;
    assert_eq!(loaded.get(), Some(719));
    assert_eq!(got.borrow().len(), 3);
    assert!(got.borrow().iter().all(|m| m.thread_id == 719));

    // Dropping the guard unsubscribes.
    drop(sub);
    drop(sub_loaded);
    client.request_conversation(DEVICE, 719, 0, 2).await;
    // The signals were sent before the reply; give them a moment to (not) arrive.
    glib::timeout_future(Duration::from_millis(300)).await;
    assert_eq!(got.borrow().len(), 3);

    // The daemon going away is noticed.
    mock.close_future().await.unwrap();
    wait_for(|| vanished.get() > vanished_before).await;
    assert_eq!(vanished.get(), vanished_before + 1);
    assert!(!client.is_daemon_available().await);
}
