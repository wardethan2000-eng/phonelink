//! A fake `org.kde.kdeconnect` daemon with made-up conversations, for trying
//! the app (and `--dump`) on the build box without a phone:
//!
//!     cargo run --example mock_daemon &
//!     cargo run                      # the app
//!
//! All names, numbers and texts are invented. The daemon answers
//! `activeConversations`, replays a thread's messages for
//! `requestConversation`, and has a battery object.

use std::time::{SystemTime, UNIX_EPOCH};

use gio::prelude::*;
use glib::Variant;
use phonelink_rs::dbus::*;

const DEVICE: &str = "demodevice1";

const DAEMON_XML: &str = r#"<node><interface name="org.kde.kdeconnect.daemon">
  <method name="devices"><arg type="as" direction="out"/></method>
  <signal name="deviceListChanged"/>
</interface></node>"#;

const DEVICE_XML: &str = r#"<node>
<interface name="org.kde.kdeconnect.device">
  <property name="name" type="s" access="read"/>
  <property name="type" type="s" access="read"/>
  <property name="isReachable" type="b" access="read"/>
  <property name="isPaired" type="b" access="read"/>
  <signal name="reachableChanged"><arg type="b"/></signal>
</interface>
<interface name="org.kde.kdeconnect.device.conversations">
  <method name="activeConversations"><arg type="av" direction="out"/></method>
  <method name="requestAllConversationThreads"/>
  <method name="requestConversation">
    <arg type="x" direction="in"/><arg type="i" direction="in"/><arg type="i" direction="in"/>
  </method>
  <signal name="conversationCreated"><arg type="v"/></signal>
  <signal name="conversationUpdated"><arg type="v"/></signal>
  <signal name="conversationLoaded"><arg type="x"/><arg type="t"/></signal>
</interface>
</node>"#;

const SMS_XML: &str = r#"<node><interface name="org.kde.kdeconnect.device.sms">
  <method name="requestConversation">
    <arg type="x" direction="in"/><arg type="x" direction="in"/><arg type="x" direction="in"/>
  </method>
</interface></node>"#;

const BATTERY_XML: &str = r#"<node><interface name="org.kde.kdeconnect.device.battery">
  <property name="charge" type="i" access="read"/>
  <property name="isCharging" type="b" access="read"/>
  <signal name="refreshed"><arg type="b"/><arg type="i"/></signal>
</interface></node>"#;

/// (thread, addresses, unread, lines). Numbers use the 555-01xx fiction range.
const THREADS: &[(i64, &[&str], bool, &[&str])] = &[
    (11, &["+15555550101"], true, &["Are you still coming tonight?", "Running 10 min late"]),
    (12, &["+15555550102"], false, &["Thanks for the photos!", "That one of the lake is great"]),
    (13, &["+15555550103", "+15555550104", "+15555550105"], true, &["Who's bringing chips?", "I can"]),
    (14, &["+15555550106"], false, &["Your order has shipped"]),
    (15, &["+15555550107"], false, &["Call me when you get a chance", "ok"]),
    (16, &["+15555550108"], false, &["Happy birthday!!", "Thank you 🎉"]),
    (17, &["22550"], false, &["Your verification code is 000000"]),
    (18, &["+15555550109", "+15555550110"], false, &["Practice moved to 6:30"]),
    (19, &["+15555550111"], false, &["See you Monday"]),
    (20, &["+15555550112"], false, &["Did the package arrive?", "Yes, this morning"]),
    (21, &["+15555550113"], false, &["Can you send me the address again?"]),
    (22, &["+15555550114"], false, &["Lunch tomorrow?"]),
    (23, &["+15555550115"], false, &["Sounds good"]),
    (24, &["+15555550116"], false, &["Here's the recipe I mentioned, it takes about an hour start to finish"]),
];

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// One message in kdeconnectd 26.08's shape: `(isa(s)xiixixa(xsss))`.
fn message(thread: i64, uid: i32, addresses: &[&str], body: &str, date: i64, sent: bool, read: bool) -> Variant {
    let addrs: Vec<(String,)> = addresses.iter().map(|a| (a.to_string(),)).collect();
    let atts: Vec<(i64, String, String, String)> = Vec::new();
    Variant::tuple_from_iter([
        1i32.to_variant(),
        body.to_variant(),
        addrs.to_variant(),
        date.to_variant(),
        (if sent { 2i32 } else { 1i32 }).to_variant(),
        i32::from(read).to_variant(),
        thread.to_variant(),
        uid.to_variant(),
        (-1i64).to_variant(),
        atts.to_variant(),
    ])
}

/// Every message of a thread, oldest first. Threads are spread over the last
/// two weeks so the list shows times, weekdays and dates.
fn thread_messages(index: usize) -> Vec<Variant> {
    let (thread, addrs, unread, lines) = THREADS[index];
    let base = now_ms() - (index as i64) * (index as i64) * 90 * 60_000;
    lines
        .iter()
        .enumerate()
        .map(|(i, body)| {
            let newest = i == lines.len() - 1;
            // Alternate, with the newest always received (so unread shows).
            let sent = (lines.len() - 1 - i) % 2 == 1;
            let date = base - ((lines.len() - 1 - i) as i64) * 7 * 60_000;
            let uid = (thread * 100) as i32 + i as i32;
            message(thread, uid, addrs, body, date, sent, !(unread && newest && !sent))
        })
        .collect()
}

fn main() {
    let ctx = glib::MainContext::default();
    ctx.block_on(async {
        let conn = gio::bus_get_future(gio::BusType::Session).await.expect("session bus");
        let path = device_path(DEVICE);

        let info = gio::DBusNodeInfo::for_xml(DAEMON_XML).unwrap();
        conn.register_object(DAEMON_PATH, &info.lookup_interface(IFACE_DAEMON).unwrap())
            .method_call(|_, _, _, _, _, _, inv| inv.return_value(Some(&(vec![DEVICE.to_string()],).to_variant())))
            .build()
            .unwrap();

        let dev = gio::DBusNodeInfo::for_xml(DEVICE_XML).unwrap();
        conn.register_object(&path, &dev.lookup_interface(IFACE_DEVICE).unwrap())
            .property(|_, _, _, _, prop| match prop {
                "name" => "Demo Phone".to_variant(),
                "type" => "phone".to_variant(),
                _ => true.to_variant(),
            })
            .build()
            .unwrap();
        conn.register_object(&path, &dev.lookup_interface(IFACE_CONVERSATIONS).unwrap())
            .method_call(|conn, _, path, iface, method, params, inv| match method {
                "activeConversations" => {
                    let latest: Vec<Variant> = (0..THREADS.len())
                        .map(|i| Variant::from_variant(thread_messages(i).last().unwrap()))
                        .collect();
                    inv.return_value(Some(&(latest,).to_variant()));
                }
                "requestConversation" => {
                    let (thread, _start, _end) = params.get::<(i64, i32, i32)>().unwrap();
                    inv.return_value(None);
                    let Some(i) = THREADS.iter().position(|t| t.0 == thread) else { return };
                    let msgs = thread_messages(i);
                    for m in &msgs {
                        let p = Variant::tuple_from_iter([Variant::from_variant(m)]);
                        conn.emit_signal(None, path, iface.unwrap(), "conversationUpdated", Some(&p)).unwrap();
                    }
                    let loaded = (thread, msgs.len() as u64).to_variant();
                    conn.emit_signal(None, path, iface.unwrap(), "conversationLoaded", Some(&loaded)).unwrap();
                }
                _ => inv.return_value(None),
            })
            .build()
            .unwrap();

        let sms = gio::DBusNodeInfo::for_xml(SMS_XML).unwrap();
        conn.register_object(&format!("{path}/sms"), &sms.lookup_interface(IFACE_SMS).unwrap())
            .method_call(|conn, _, _, _, _, params, inv| {
                // The phone "sends" the thread to the daemon, which then
                // only reports conversationLoaded.
                let (thread, _, count) = params.get::<(i64, i64, i64)>().unwrap();
                inv.return_value(None);
                let loaded = (thread, count.max(0) as u64).to_variant();
                let dev_path = device_path(DEVICE);
                conn.emit_signal(None, &dev_path, IFACE_CONVERSATIONS, "conversationLoaded", Some(&loaded)).unwrap();
            })
            .build()
            .unwrap();

        let bat = gio::DBusNodeInfo::for_xml(BATTERY_XML).unwrap();
        conn.register_object(&format!("{path}/battery"), &bat.lookup_interface(IFACE_BATTERY).unwrap())
            .property(|_, _, _, _, prop| match prop {
                "charge" => 70i32.to_variant(),
                _ => true.to_variant(),
            })
            .build()
            .unwrap();

        let reply = conn
            .call_future(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "RequestName",
                Some(&(BUS_NAME, 4u32).to_variant()),
                Some(glib::VariantTy::new("(u)").unwrap()),
                gio::DBusCallFlags::NONE,
                5000,
            )
            .await
            .unwrap();
        assert_eq!(reply.get::<(u32,)>(), Some((1,)), "{BUS_NAME} is already owned");
        eprintln!("demo org.kde.kdeconnect running (device {DEVICE}, {} conversations)", THREADS.len());
        std::future::pending::<()>().await;
    });
}
