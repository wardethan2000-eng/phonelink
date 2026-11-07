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
  <method name="replyToConversation">
    <arg type="x" direction="in"/><arg type="s" direction="in"/><arg type="av" direction="in"/>
  </method>
  <method name="sendWithoutConversation">
    <arg type="av" direction="in"/><arg type="s" direction="in"/><arg type="av" direction="in"/>
  </method>
  <method name="requestAttachmentFile">
    <arg type="x" direction="in"/><arg type="s" direction="in"/>
  </method>
  <signal name="attachmentReceived"><arg type="s"/><arg type="s"/></signal>
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

const NOTIFICATIONS_XML: &str = r#"<node><interface name="org.kde.kdeconnect.device.notifications">
  <method name="activeNotifications"><arg type="as" direction="out"/></method>
  <method name="sendReply"><arg type="s" direction="in"/><arg type="s" direction="in"/></method>
  <signal name="notificationPosted"><arg type="s"/></signal>
  <signal name="notificationUpdated"><arg type="s"/></signal>
  <signal name="notificationRemoved"><arg type="s"/></signal>
  <signal name="allNotificationsRemoved"/>
</interface></node>"#;

const NOTIFICATION_XML: &str = r#"<node><interface name="org.kde.kdeconnect.device.notifications.notification">
  <property name="appName" type="s" access="read"/>
  <property name="title" type="s" access="read"/>
  <property name="text" type="s" access="read"/>
  <property name="ticker" type="s" access="read"/>
  <property name="replyId" type="s" access="read"/>
  <property name="dismissable" type="b" access="read"/>
  <property name="hasIcon" type="b" access="read"/>
  <property name="iconPath" type="s" access="read"/>
  <method name="dismiss"/>
  <method name="sendReply"><arg type="s" direction="in"/></method>
</interface></node>"#;

/// The other notifications on the demo phone: (id, app, title, text,
/// reply id, dismissable). "demo5" arrives 20 s after start.
const OTHER_NOTIFICATIONS: &[(&str, &str, &str, &str, &str, bool)] = &[
    ("demo2", "Gmail", "Weekly summary", "You have 3 new messages from the book club.\nThe next meeting is moved to Thursday at 7 PM, at the library on Main Street.", "", true),
    ("demo3", "Clock", "Alarm", "7:00 AM · Weekdays", "", false),
    ("demo4", "Calendar", "Dentist", "Tomorrow, 10:30 AM", "", true),
    ("demo5", "WhatsApp", "Sam", "Did you see the game last night?", "demo-reply-5", true),
];

const CLIPBOARD_XML: &str = r#"<node><interface name="org.kde.kdeconnect.device.clipboard">
  <method name="sendClipboard"/>
</interface></node>"#;

const FINDPHONE_XML: &str = r#"<node><interface name="org.kde.kdeconnect.device.findmyphone">
  <method name="ring"/>
</interface></node>"#;

const SHARE_XML: &str = r#"<node><interface name="org.kde.kdeconnect.device.share">
  <method name="shareUrl"><arg type="s" direction="in"/></method>
  <method name="shareUrls"><arg type="as" direction="in"/></method>
  <signal name="shareReceived"><arg type="s"/></signal>
</interface></node>"#;

const SFTP_XML: &str = r#"<node><interface name="org.kde.kdeconnect.device.sftp">
  <method name="isMounted"><arg type="b" direction="out"/></method>
  <method name="mountAndWait"><arg type="b" direction="out"/></method>
  <method name="startBrowsing"><arg type="b" direction="out"/></method>
  <method name="mountPoint"><arg type="s" direction="out"/></method>
  <method name="getMountError"><arg type="s" direction="out"/></method>
  <method name="getDirectories"><arg type="a{sv}" direction="out"/></method>
  <signal name="mounted"/>
  <signal name="unmounted"/>
</interface></node>"#;

/// A local folder standing in for the phone's sftp mount: camera pictures,
/// screenshots and a few other files, with spread-out times.
fn fake_phone_storage() -> std::path::PathBuf {
    let root = phonelink_rs::core::settings::home_dir().join("demo-sftp");
    let base = root.join("storage/emulated/0");
    let colors: [u32; 6] = [0x35_84_e4_ff, 0xe6_61_00_ff, 0x26_a2_69_ff, 0x91_41_ac_ff, 0xc0_1c_28_ff, 0xe5_a5_0a_ff];
    for (i, dir) in ["DCIM/Camera", "DCIM/Camera", "DCIM/Camera", "DCIM/Screenshots", "Pictures", "Download"]
        .iter()
        .cycle()
        .take(18)
        .enumerate()
    {
        let d = base.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        let file = d.join(format!("IMG_2026110{}_{:04}.jpg", 1 + i % 5, 1000 + i));
        let pb = gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 800, 600).unwrap();
        pb.fill(colors[i % colors.len()]);
        pb.new_subpixbuf(0, 380, 800, 220).fill(0xf6_f5_f4_ff);
        pb.new_subpixbuf(80 + (i as i32 * 30) % 500, 200, 160, 160).fill(0x24_1f_31_ff);
        pb.savev(&file, "jpeg", &[("quality", "80")]).unwrap();
        let t = SystemTime::now() - std::time::Duration::from_secs(3600 * i as u64);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(t).unwrap();
    }
    for (name, bytes) in [("Download/boarding-pass.pdf", 48_000usize), ("Download/notes.txt", 900), ("Music/song.mp3", 3_400_000)] {
        let f = base.join(name);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, vec![0u8; bytes]).unwrap();
    }
    root
}

/// A second paired device, offline, so the header shows the switcher.
const TABLET: &str = "demotablet2";

/// One answerable "Messages" notification, for the newest text in thread 11,
/// so a reply there goes the notification route.
const DEMO_NOTIFICATION: &str = "demo1";

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

/// Thread 11 gets a long history (to show the 60-message window and
/// "Show earlier messages") and one photo.
const LONG_THREAD: i64 = 11;
/// `DEMO_LONG=N` sets its length (default 150; Ethan's biggest is ~2,226).
fn long_thread_len() -> usize {
    std::env::var("DEMO_LONG").ok().and_then(|v| v.parse().ok()).unwrap_or(150)
}

/// A small solid-color PNG, base64, like KDE Connect's inline thumbnails.
fn thumbnail_png() -> String {
    let pb = gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 100, 75).unwrap();
    pb.fill(0xe5_a5_0a_ff); // amber, so it stands out from the sent-bubble color
    let bytes = pb.save_to_bufferv("png", &[]).unwrap();
    glib::base64_encode(&bytes).to_string()
}

/// The "full" photo: 1200×900, a different color from the thumbnail so the
/// swap is visible. Written under $HOME/demo-downloads.
fn full_picture(name: &str) -> std::path::PathBuf {
    let dir = phonelink_rs::core::settings::home_dir().join("demo-downloads");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(name);
    let pb = gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 1200, 900).unwrap();
    pb.fill(0x26_a2_69_ff); // green
    // A lighter band so it reads as a picture, not a block of color.
    pb.new_subpixbuf(0, 600, 1200, 300).fill(0x8f_f0_a4_ff);
    pb.savev(&file, "png", &[]).unwrap();
    file
}

static NEXT_SENT_UID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(90_000);

/// What the phone does after sending: the sent text comes back as a
/// conversationUpdated a moment later.
fn echo_sent(conn: &gio::DBusConnection, thread: i64, text: String) {
    let conn = conn.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
        let i = THREADS.iter().position(|t| t.0 == thread).unwrap_or(0);
        let uid = NEXT_SENT_UID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let m = message(thread, uid, THREADS[i].1, &text, now_ms(), true, true);
        let p = Variant::tuple_from_iter([Variant::from_variant(&m)]);
        conn.emit_signal(None, &device_path(DEVICE), IFACE_CONVERSATIONS, "conversationUpdated", Some(&p)).unwrap();
    });
}

/// History dates are fixed at startup: a real phone's don't move, and
/// moving ones make every reload look like new texts.
fn demo_start_ms() -> i64 {
    static START: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *START.get_or_init(now_ms)
}

/// `DEMO_EXTRA=N` adds N one-message filler chats (to try a big list).
fn extra_threads() -> usize {
    std::env::var("DEMO_EXTRA").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn thread_count() -> usize {
    THREADS.len() + extra_threads()
}

/// `DEMO_FILLER_MSGS=N` texts per filler chat (default 1).
fn filler_len() -> usize {
    std::env::var("DEMO_FILLER_MSGS").ok().and_then(|v| v.parse().ok()).unwrap_or(1).max(1)
}

/// Filler chat `index` (≥ THREADS.len()): thread 1000+, old texts.
fn filler_messages(index: usize) -> Vec<Variant> {
    let n = index - THREADS.len();
    let thread = 1000 + n as i64;
    let addr = format!("+1555550{:04}", 2000 + n);
    let newest = demo_start_ms() - (3 + n as i64) * 24 * 60 * 60_000;
    let len = filler_len();
    (0..len)
        .map(|k| {
            let date = newest - ((len - 1 - k) as i64) * 20 * 60_000;
            let uid = 500_000 + (n * len + k) as i32;
            let body = format!("Filler text {k} with a few more words so it looks like a real message");
            message(thread, uid, &[addr.as_str()], &body, date, k % 2 == 1, true)
        })
        .collect()
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// One message in kdeconnectd 26.08's shape: `(isa(s)xiixixa(xsss))`.
fn message(thread: i64, uid: i32, addresses: &[&str], body: &str, date: i64, sent: bool, read: bool) -> Variant {
    message_with(thread, uid, addresses, body, date, sent, read, Vec::new())
}

#[allow(clippy::too_many_arguments)]
fn message_with(
    thread: i64,
    uid: i32,
    addresses: &[&str],
    body: &str,
    date: i64,
    sent: bool,
    read: bool,
    atts: Vec<(i64, String, String, String)>,
) -> Variant {
    let addrs: Vec<(String,)> = addresses.iter().map(|a| (a.to_string(),)).collect();
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
    if index >= THREADS.len() {
        return filler_messages(index);
    }
    let (thread, addrs, unread, lines) = THREADS[index];
    let base = demo_start_ms() - (index as i64) * (index as i64) * 90 * 60_000;
    let mut out = Vec::new();
    if thread == LONG_THREAD {
        // Older history in bursts like real chats (5 texts a couple of
        // minutes apart, then a few hours' gap), with a photo.
        let chatter = ["ok", "sounds good", "on my way", "see you there", "haha", "did you see that?", "yes", "tomorrow works"];
        for k in 0..long_thread_len() {
            let (burst, within) = ((long_thread_len() - k) as i64 / 5, (k % 5) as i64);
            let date = base - burst * 5 * 60 * 60_000 - 10 * 60_000 + within * 2 * 60_000;
            let uid = 5000 + k as i32;
            // One old text to find with search (item A in NEXT_FEATURES.md).
            let body = if k == 20 { "Don't forget the birthday party at the lake house on Saturday" } else { chatter[k % chatter.len()] };
            // A photo every 60 texts (and one near the end).
            let atts = if k == long_thread_len() - 3 || k % 60 == 57 {
                vec![(uid as i64, "image/png".to_string(), thumbnail_png(), format!("PART_{uid}.png"))]
            } else {
                Vec::new()
            };
            let text = if atts.is_empty() { body } else { "" };
            out.push(message_with(thread, uid, addrs, text, date, k % 3 == 0, true, atts));
        }
    }
    out.extend(lines
        .iter()
        .enumerate()
        .map(|(i, body)| {
            let newest = i == lines.len() - 1;
            // Alternate, with the newest always received (so unread shows).
            let sent = (lines.len() - 1 - i) % 2 == 1;
            let date = base - ((lines.len() - 1 - i) as i64) * 7 * 60_000;
            let uid = (thread * 100) as i32 + i as i32;
            message(thread, uid, addrs, body, date, sent, !(unread && newest && !sent))
        }));
    out
}

fn main() {
    let ctx = glib::MainContext::default();
    ctx.block_on(async {
        let conn = gio::bus_get_future(gio::BusType::Session).await.expect("session bus");
        let path = device_path(DEVICE);

        let info = gio::DBusNodeInfo::for_xml(DAEMON_XML).unwrap();
        conn.register_object(DAEMON_PATH, &info.lookup_interface(IFACE_DAEMON).unwrap())
            .method_call(|_, _, _, _, _, _, inv| {
                inv.return_value(Some(&(vec![DEVICE.to_string(), TABLET.to_string()],).to_variant()))
            })
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
                    let latest: Vec<Variant> = (0..thread_count())
                        .map(|i| Variant::from_variant(thread_messages(i).last().unwrap()))
                        .collect();
                    inv.return_value(Some(&(latest,).to_variant()));
                }
                "requestConversation" => {
                    let (thread, _start, _end) = params.get::<(i64, i32, i32)>().unwrap();
                    inv.return_value(None);
                    let i = match THREADS.iter().position(|t| t.0 == thread) {
                        Some(i) => i,
                        None if thread >= 1000 => THREADS.len() + (thread - 1000) as usize,
                        None => return,
                    };
                    let msgs = thread_messages(i);
                    for m in &msgs {
                        let p = Variant::tuple_from_iter([Variant::from_variant(m)]);
                        conn.emit_signal(None, path, iface.unwrap(), "conversationUpdated", Some(&p)).unwrap();
                    }
                    let loaded = (thread, msgs.len() as u64).to_variant();
                    conn.emit_signal(None, path, iface.unwrap(), "conversationLoaded", Some(&loaded)).unwrap();
                }
                "requestAllConversationThreads" => {
                    // Like the phone after connecting: every message again.
                    inv.return_value(None);
                    let (conn, path, iface) = (conn.clone(), path.to_string(), iface.unwrap().to_string());
                    glib::idle_add_local_once(move || {
                        for i in 0..thread_count() {
                            for m in thread_messages(i) {
                                let p = Variant::tuple_from_iter([Variant::from_variant(&m)]);
                                conn.emit_signal(None, &path, &iface, "conversationUpdated", Some(&p)).unwrap();
                            }
                        }
                    });
                }
                "replyToConversation" => {
                    let thread = params.child_value(0).get::<i64>().unwrap();
                    let text = params.child_value(1).get::<String>().unwrap();
                    eprintln!("demo: replyToConversation({thread}, {} chars)", text.len());
                    inv.return_value(None);
                    echo_sent(&conn, thread, text);
                }
                "sendWithoutConversation" => {
                    eprintln!("demo: sendWithoutConversation");
                    inv.return_value(None);
                }
                "requestAttachmentFile" => {
                    // The phone "sends" the full picture a moment later.
                    let (_part, uid) = params.get::<(i64, String)>().unwrap();
                    inv.return_value(None);
                    let (conn, path, iface) = (conn.clone(), path.to_string(), iface.unwrap().to_string());
                    glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
                        let file = full_picture(&uid);
                        let p = (file.to_string_lossy().into_owned(), uid).to_variant();
                        conn.emit_signal(None, &path, &iface, "attachmentReceived", Some(&p)).unwrap();
                    });
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

        let active: std::rc::Rc<std::cell::RefCell<Vec<String>>> = std::rc::Rc::new(
            [DEMO_NOTIFICATION, "demo2", "demo3", "demo4"].iter().map(|s| s.to_string()).collect::<Vec<_>>().into(),
        );
        let notes = gio::DBusNodeInfo::for_xml(NOTIFICATIONS_XML).unwrap();
        let listed = active.clone();
        conn.register_object(&format!("{path}/notifications"), &notes.lookup_interface(IFACE_NOTIFICATIONS).unwrap())
            .method_call(move |conn, _, path, iface, method, params, inv| match method {
                "activeNotifications" => inv.return_value(Some(&(listed.borrow().clone(),).to_variant())),
                "sendReply" => {
                    let (reply_id, text) = params.get::<(String, String)>().unwrap();
                    eprintln!("demo: sendReply({reply_id}, {} chars)", text.len());
                    inv.return_value(None);
                    echo_sent(&conn, 11, text);
                    let gone = (DEMO_NOTIFICATION,).to_variant();
                    conn.emit_signal(None, path, iface.unwrap(), "notificationRemoved", Some(&gone)).unwrap();
                }
                _ => inv.return_value(None),
            })
            .build()
            .unwrap();
        let note = gio::DBusNodeInfo::for_xml(NOTIFICATION_XML).unwrap();
        let all = std::iter::once((DEMO_NOTIFICATION, "Messages", "+15555550101", "Running 10 min late", "demo-reply-1", true))
            .chain(OTHER_NOTIFICATIONS.iter().copied());
        for (id, app, title, text, reply, dismissable) in all {
            let gone = active.clone();
            conn.register_object(&format!("{path}/notifications/{id}"), &note.lookup_interface(IFACE_NOTIFICATION).unwrap())
                .property(move |_, _, _, _, prop| match prop {
                    "appName" => app.to_variant(),
                    "title" => title.to_variant(),
                    "text" => text.to_variant(),
                    "replyId" => reply.to_variant(),
                    "dismissable" => dismissable.to_variant(),
                    "hasIcon" => false.to_variant(),
                    _ => "".to_variant(),
                })
                .method_call(move |conn, _, _, _, method, params, inv| {
                    inv.return_value(None);
                    if method == "sendReply" {
                        eprintln!("demo: notification {id} sendReply({} chars)", params.child_value(0).str().unwrap_or("").len());
                        return;
                    }
                    eprintln!("demo: dismiss {id}");
                    gone.borrow_mut().retain(|n| n != id);
                    let list = format!("{}/notifications", device_path(DEVICE));
                    conn.emit_signal(None, &list, IFACE_NOTIFICATIONS, "notificationRemoved", Some(&(id,).to_variant()))
                        .unwrap();
                })
                .build()
                .unwrap();
        }
        // `DEMO_CALLS=1`: a call rings and is missed in an existing chat
        // (+15555550107), and a new number calls and is missed.
        if std::env::var_os("DEMO_CALLS").is_some() {
            let calls = conn.clone();
            let tel = format!("{}/telephony", device_path(DEVICE));
            let iface = phonelink_rs::dbus::IFACE_TELEPHONY;
            for (secs, kind, number) in [
                (20, "callReceived", "+15555550107"),
                (22, "missedCall", "+15555550107"),
                (23, "missedCall", "+15555550199"),
            ] {
                let (c, t) = (calls.clone(), tel.clone());
                glib::timeout_add_seconds_local_once(secs, move || {
                    let p = (kind, number, number).to_variant();
                    c.emit_signal(None, &t, iface, "callReceived", Some(&p)).unwrap();
                });
            }
        }
        // A new notification arrives a little later.
        let (late, late_conn) = (active.clone(), conn.clone());
        glib::timeout_add_seconds_local_once(20, move || {
            late.borrow_mut().push("demo5".into());
            let list = format!("{}/notifications", device_path(DEVICE));
            late_conn.emit_signal(None, &list, IFACE_NOTIFICATIONS, "notificationPosted", Some(&("demo5",).to_variant())).unwrap();
        });

        for (xml, iface, sub) in
            [(CLIPBOARD_XML, IFACE_CLIPBOARD, "clipboard"), (FINDPHONE_XML, IFACE_FINDPHONE, "findmyphone"), (SHARE_XML, IFACE_SHARE, "share")]
        {
            let info = gio::DBusNodeInfo::for_xml(xml).unwrap();
            conn.register_object(&format!("{path}/{sub}"), &info.lookup_interface(iface).unwrap())
                .method_call(|_, _, _, _, method, params, inv| {
                    eprintln!("demo: {method}{}", params.print(false));
                    inv.return_value(None);
                })
                .build()
                .unwrap();
        }

        let mount = fake_phone_storage().to_string_lossy().into_owned();
        let mounted = std::rc::Rc::new(std::cell::Cell::new(false));
        let sftp = gio::DBusNodeInfo::for_xml(SFTP_XML).unwrap();
        conn.register_object(&format!("{path}/sftp"), &sftp.lookup_interface(IFACE_SFTP).unwrap())
            .method_call(move |conn, _, path, iface, method, _, inv| match method {
                "isMounted" => inv.return_value(Some(&(mounted.get(),).to_variant())),
                "mountAndWait" | "startBrowsing" => {
                    eprintln!("demo: {method}");
                    mounted.set(true);
                    // Like the phone: the mount takes a moment.
                    let (conn, path, iface) = (conn.clone(), path.to_string(), iface.unwrap().to_string());
                    glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                        conn.emit_signal(None, &path, &iface, "mounted", None).unwrap();
                        inv.return_value(Some(&(true,).to_variant()));
                    });
                }
                "mountPoint" => inv.return_value(Some(&(mount.as_str(),).to_variant())),
                "getMountError" => inv.return_value(Some(&("",).to_variant())),
                "getDirectories" => {
                    let dirs = glib::VariantDict::new(None);
                    dirs.insert_value(&format!("{mount}/storage/emulated/0"), &"All files".to_variant());
                    dirs.insert_value(&format!("{mount}/storage/emulated/0/DCIM/Camera"), &"Camera pictures".to_variant());
                    inv.return_value(Some(&glib::Variant::tuple_from_iter([dirs.end()])));
                }
                _ => inv.return_value(None),
            })
            .build()
            .unwrap();

        let tablet = device_path(TABLET);
        conn.register_object(&tablet, &dev.lookup_interface(IFACE_DEVICE).unwrap())
            .property(|_, _, _, _, prop| match prop {
                "name" => "Demo Tablet".to_variant(),
                "type" => "tablet".to_variant(),
                "isReachable" => false.to_variant(),
                _ => true.to_variant(),
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

        // A stand-in StatusNotifierWatcher (the tray host): it reads the
        // app's tray item and menu back and prints what it got.
        const WATCHER_XML: &str = r#"<node><interface name="org.kde.StatusNotifierWatcher">
  <method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
</interface></node>"#;
        let watcher = gio::DBusNodeInfo::for_xml(WATCHER_XML).unwrap();
        conn.register_object("/StatusNotifierWatcher", &watcher.lookup_interface("org.kde.StatusNotifierWatcher").unwrap())
            .method_call(|conn, sender, _, _, _, params, inv| {
                inv.return_value(None);
                let service = params.child_value(0).get::<String>().unwrap_or_default();
                eprintln!("demo: tray registered by {service} (sender {})", sender.unwrap_or(""));
                let conn = conn.clone();
                glib::spawn_future_local(async move {
                    let props = conn
                        .call_future(Some(&service), "/StatusNotifierItem", "org.freedesktop.DBus.Properties", "GetAll",
                            Some(&("org.kde.StatusNotifierItem",).to_variant()), None, gio::DBusCallFlags::NONE, 3000)
                        .await;
                    match props {
                        Ok(v) => {
                            let m = glib::VariantDict::new(Some(&v.child_value(0)));
                            let n = v.child_value(0).n_children();
                            let icon = m.lookup_value("IconName", None).map(|v| v.print(false).to_string()).unwrap_or_default();
                            let pix = m.lookup_value("IconPixmap", None).map(|v| v.n_children()).unwrap_or(0);
                            eprintln!("demo: tray item has {n} properties, IconName {icon}, {pix} pixmaps");
                        }
                        Err(e) => eprintln!("demo: tray GetAll failed: {e}"),
                    }
                    let layout = conn
                        .call_future(Some(&service), "/MenuBar", "com.canonical.dbusmenu", "GetLayout",
                            Some(&(0i32, -1i32, Vec::<String>::new()).to_variant()), None, gio::DBusCallFlags::NONE, 3000)
                        .await;
                    match layout {
                        Ok(v) => eprintln!("demo: tray menu {}", v.print(false)),
                        Err(e) => eprintln!("demo: tray GetLayout failed: {e}"),
                    }
                });
            })
            .build()
            .unwrap();
        let _ = conn
            .call_future(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", "org.freedesktop.DBus", "RequestName",
                Some(&("org.kde.StatusNotifierWatcher", 4u32).to_variant()), None, gio::DBusCallFlags::NONE, 5000)
            .await;

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
        eprintln!("demo org.kde.kdeconnect running (device {DEVICE}, {} conversations)", thread_count());
        std::future::pending::<()>().await;
    });
}
