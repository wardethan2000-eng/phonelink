//! A mock `org.kde.kdeconnect` daemon: one device, two conversations,
//! `requestConversation` replays as signals, no battery object and no
//! `markConversationAsRead` / `deleteConversation` (as on daemon 26.08).
//! Shared by `tests/mock_daemon.rs` and `examples/mock_daemon.rs`.

use std::cell::RefCell;
use std::rc::Rc;

use gio::prelude::*;
use glib::Variant;
use phonelink_rs::dbus::*;

pub const DEVICE: &str = "mockdevice1";

const DAEMON_XML: &str = r#"<node>
  <interface name="org.kde.kdeconnect.daemon">
    <method name="devices"><arg type="as" direction="out"/></method>
    <signal name="deviceListChanged"/>
  </interface>
</node>"#;

pub const DEVICE_XML: &str = r#"<node>
  <interface name="org.kde.kdeconnect.device">
    <property name="name" type="s" access="read"/>
    <property name="type" type="s" access="read"/>
    <property name="isReachable" type="b" access="read"/>
    <property name="isPaired" type="b" access="read"/>
    <signal name="reachableChanged"><arg type="b"/></signal>
  </interface>
  <interface name="org.kde.kdeconnect.device.conversations">
    <method name="activeConversations"><arg type="av" direction="out"/></method>
    <method name="requestConversation">
      <arg type="x" direction="in"/><arg type="i" direction="in"/><arg type="i" direction="in"/>
    </method>
    <method name="replyToConversation">
      <arg type="x" direction="in"/><arg type="s" direction="in"/><arg type="av" direction="in"/>
    </method>
    <signal name="conversationUpdated"><arg type="v"/></signal>
    <signal name="conversationLoaded"><arg type="x"/><arg type="t"/></signal>
  </interface>
</node>"#;

pub fn message(thread: i64, uid: i64, addresses: &[&str]) -> Variant {
    let addrs: Vec<(String,)> = addresses.iter().map(|a| (a.to_string(),)).collect();
    let atts: Vec<(i64, String, String, String)> = vec![(uid, "image/jpeg".into(), String::new(), "IMG.jpg".into())];
    Variant::tuple_from_iter([
        1i32.to_variant(),
        "text".to_variant(),
        addrs.to_variant(),
        (1_700_000_000_000 + uid).to_variant(),
        1i32.to_variant(),
        1i32.to_variant(),
        thread.to_variant(),
        uid.to_variant(),
        (-1i64).to_variant(),
        atts.to_variant(),
    ])
}

/// What the mock saw, for assertions.
#[derive(Default)]
pub struct Seen {
    pub replies: RefCell<Vec<(i64, String, usize)>>,
}

pub fn register_mock(conn: &gio::DBusConnection, seen: Rc<Seen>) {
    let daemon = gio::DBusNodeInfo::for_xml(DAEMON_XML).unwrap();
    conn.register_object(DAEMON_PATH, &daemon.lookup_interface(IFACE_DAEMON).unwrap())
        .method_call(|_, _, _, _, method, _, inv| match method {
            "devices" => inv.return_value(Some(&(vec![DEVICE.to_string()],).to_variant())),
            _ => unreachable!(),
        })
        .build()
        .unwrap();

    let dev = gio::DBusNodeInfo::for_xml(DEVICE_XML).unwrap();
    let path = device_path(DEVICE);
    conn.register_object(&path, &dev.lookup_interface(IFACE_DEVICE).unwrap())
        .property(|_, _, _, _, prop| match prop {
            "name" => "Mock Phone".to_variant(),
            "type" => "phone".to_variant(),
            _ => true.to_variant(),
        })
        .build()
        .unwrap();
    conn.register_object(&path, &dev.lookup_interface(IFACE_CONVERSATIONS).unwrap())
        .method_call(move |conn, _, path, iface, method, params, inv| match method {
            "activeConversations" => {
                let list = vec![
                    Variant::from_variant(&message(615, 31001, &["+15550001111"])),
                    Variant::from_variant(&message(719, 72001, &["+15550002222", "+15550003333"])),
                ];
                inv.return_value(Some(&(list,).to_variant()));
            }
            "requestConversation" => {
                let (thread, start, end) = params.get::<(i64, i32, i32)>().unwrap();
                inv.return_value(None);
                for i in start..end {
                    let addrs: &[&str] =
                        if thread == 719 { &["+15550002222", "+15550003333"] } else { &["+15550001111"] };
                    let m = message(thread, thread * 1000 + i64::from(i), addrs);
                    let p = Variant::tuple_from_iter([Variant::from_variant(&m)]);
                    conn.emit_signal(None, path, iface.unwrap(), "conversationUpdated", Some(&p)).unwrap();
                }
                let n = u64::try_from(end - start).unwrap();
                conn.emit_signal(None, path, iface.unwrap(), "conversationLoaded", Some(&(thread, n).to_variant()))
                    .unwrap();
            }
            "replyToConversation" => {
                let thread = params.child_value(0).get::<i64>().unwrap();
                let body = params.child_value(1).get::<String>().unwrap();
                let atts = params.child_value(2).n_children();
                seen.replies.borrow_mut().push((thread, body, atts));
                inv.return_value(None);
            }
            _ => unreachable!(),
        })
        .build()
        .unwrap();
}

pub async fn own_name(conn: &gio::DBusConnection) {
    let reply = conn
        .call_future(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
            Some(&(BUS_NAME, 4u32).to_variant()), // DO_NOT_QUEUE
            Some(glib::VariantTy::new("(u)").unwrap()),
            gio::DBusCallFlags::NONE,
            5000,
        )
        .await
        .unwrap();
    assert_eq!(reply.get::<(u32,)>(), Some((1,)), "{BUS_NAME} is already owned on this bus");
}
