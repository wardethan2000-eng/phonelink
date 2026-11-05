//! `phonelink-rs --dump`: a read-only check of the D-Bus layer against the
//! live daemon.
//!
//! It prints counts and shapes only, never message text, numbers or contact
//! names (the output gets pasted into a transcript). It calls only reads:
//! `NameHasOwner`, `devices`, property reads, `Introspect`,
//! `activeConversations`, and `requestConversation` on the conversations
//! interface, which replays the daemon's cache as signals (the same replay
//! the app's gap fill does). It never sends, marks read, deletes or mounts,
//! claims no app id, and does not open the message store.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use glib::Variant;
use phonelink_rs::core::message::parse_message;
use phonelink_rs::core::models::Device;
use phonelink_rs::core::reconcile::{detect_self_key, phone_key, ConversationIndex};
use phonelink_rs::dbus::introspect::Node;
use phonelink_rs::dbus::signals::{self, Subscription};
use phonelink_rs::dbus::variant;
use phonelink_rs::dbus::*;

/// How many conversations get their messages replayed and listed.
const NEWEST: usize = 20;
/// Messages replayed per phone thread (one gap-fill page).
const REPLAY_PER_THREAD: i32 = 100;
const QUIET_SECONDS: f64 = 3.0;
const MAX_WAIT_SECONDS: f64 = 30.0;

#[derive(Clone, Copy)]
enum Kind {
    Method,
    Signal,
    Property,
}

/// The D-Bus surface the Python app uses (rule 11 of the port brief), as
/// `(object below the device path, interface, kind, member)`. `None` = the
/// daemon object itself.
const EXPECTED: &[(Option<&str>, &str, Kind, &str)] = {
    use Kind::*;
    &[
        (None, IFACE_DAEMON, Method, "devices"),
        (None, IFACE_DAEMON, Signal, "deviceListChanged"),
        (Some(""), IFACE_DEVICE, Property, "name"),
        (Some(""), IFACE_DEVICE, Property, "type"),
        (Some(""), IFACE_DEVICE, Property, "isReachable"),
        (Some(""), IFACE_DEVICE, Property, "isPaired"),
        (Some(""), IFACE_DEVICE, Signal, "reachableChanged"),
        (Some(""), IFACE_CONVERSATIONS, Method, "activeConversations"),
        (Some(""), IFACE_CONVERSATIONS, Method, "requestAllConversationThreads"),
        (Some(""), IFACE_CONVERSATIONS, Method, "requestConversation"),
        (Some(""), IFACE_CONVERSATIONS, Method, "replyToConversation"),
        (Some(""), IFACE_CONVERSATIONS, Method, "sendWithoutConversation"),
        (Some(""), IFACE_CONVERSATIONS, Method, "requestAttachmentFile"),
        (Some(""), IFACE_CONVERSATIONS, Method, "deleteConversation"),
        (Some(""), IFACE_CONVERSATIONS, Method, "markConversationAsRead"),
        (Some(""), IFACE_CONVERSATIONS, Signal, "conversationCreated"),
        (Some(""), IFACE_CONVERSATIONS, Signal, "conversationUpdated"),
        (Some(""), IFACE_CONVERSATIONS, Signal, "conversationRemoved"),
        (Some(""), IFACE_CONVERSATIONS, Signal, "conversationLoaded"),
        (Some(""), IFACE_CONVERSATIONS, Signal, "attachmentReceived"),
        (Some("/sms"), IFACE_SMS, Method, "requestConversation"),
        (Some("/battery"), IFACE_BATTERY, Property, "charge"),
        (Some("/battery"), IFACE_BATTERY, Property, "isCharging"),
        (Some("/battery"), IFACE_BATTERY, Signal, "refreshed"),
        (Some("/notifications"), IFACE_NOTIFICATIONS, Method, "activeNotifications"),
        (Some("/notifications"), IFACE_NOTIFICATIONS, Method, "sendReply"),
        (Some("/notifications"), IFACE_NOTIFICATIONS, Signal, "notificationPosted"),
        (Some("/notifications"), IFACE_NOTIFICATIONS, Signal, "notificationUpdated"),
        (Some("/notifications"), IFACE_NOTIFICATIONS, Signal, "notificationRemoved"),
        (Some("/notifications"), IFACE_NOTIFICATIONS, Signal, "allNotificationsRemoved"),
        (Some("/clipboard"), IFACE_CLIPBOARD, Method, "sendClipboard"),
        (Some("/clipboard"), IFACE_CLIPBOARD, Method, "content"),
        (Some("/clipboard"), IFACE_CLIPBOARD, Signal, "clipboardChanged"),
        (Some("/share"), IFACE_SHARE, Method, "shareUrl"),
        (Some("/share"), IFACE_SHARE, Method, "shareUrls"),
        (Some("/share"), IFACE_SHARE, Signal, "shareReceived"),
        (Some("/sftp"), IFACE_SFTP, Method, "isMounted"),
        (Some("/sftp"), IFACE_SFTP, Method, "mountAndWait"),
        (Some("/sftp"), IFACE_SFTP, Method, "startBrowsing"),
        (Some("/sftp"), IFACE_SFTP, Method, "unmount"),
        (Some("/sftp"), IFACE_SFTP, Method, "mountPoint"),
        (Some("/sftp"), IFACE_SFTP, Method, "getMountError"),
        (Some("/sftp"), IFACE_SFTP, Method, "getDirectories"),
        (Some("/sftp"), IFACE_SFTP, Signal, "mounted"),
        (Some("/sftp"), IFACE_SFTP, Signal, "unmounted"),
        (Some("/findmyphone"), IFACE_FINDPHONE, Method, "ring"),
        (Some("/contacts"), IFACE_CONTACTS, Method, "synchronizeRemoteWithLocal"),
        (Some("/contacts"), IFACE_CONTACTS, Signal, "localCacheSynchronized"),
        (Some("/telephony"), IFACE_TELEPHONY, Signal, "callReceived"),
    ]
};

/// Members of each notification object (`/notifications/<id>`).
const NOTIFICATION_MEMBERS: &[&str] = &["dismiss", "sendReply"];

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// The type signature, plus what a `v` (or a `(v)` tuple) carries inside.
fn sig(v: &Variant) -> String {
    let outer = v.type_().as_str().to_string();
    let inner = if v.type_() == glib::VariantTy::VARIANT {
        Some(variant::unbox(v))
    } else if v.type_().is_tuple() && v.n_children() == 1 && v.child_value(0).type_() == glib::VariantTy::VARIANT {
        Some(variant::unbox(&v.child_value(0)))
    } else {
        None
    };
    match inner {
        Some(i) => format!("{outer} carrying {}", i.type_().as_str()),
        None => outer,
    }
}

/// Counts of each distinct signature.
#[derive(Default)]
struct Shapes(BTreeMap<String, usize>);

impl Shapes {
    fn add(&mut self, v: &Variant) {
        *self.0.entry(sig(v)).or_default() += 1;
    }
    fn print(&self, label: &str) {
        if self.0.is_empty() {
            println!("    {label}: (none seen)");
        }
        for (s, n) in &self.0 {
            println!("    {label}: {s}  ×{n}");
        }
    }
}

pub async fn run(signatures: bool) -> i32 {
    println!("phonelink-rs --dump (read-only; counts and shapes only)");
    let client = match Client::connect().await {
        Ok(c) => c,
        Err(e) => {
            println!("session bus: FAILED ({})", e.message());
            return 1;
        }
    };
    let present = client.is_daemon_available().await;
    println!("daemon: {}", if present { "present" } else { "NOT RUNNING" });
    if !present {
        return 1;
    }

    let ids = client.device_ids().await;
    println!("devices: {}", ids.len());
    let mut devices = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let d = client.fetch_device(id).await;
        let battery = if d.battery_charge < 0 { "unknown".to_string() } else { format!("{}%", d.battery_charge) };
        println!(
            "  [{}] id={} name={:?} type={} reachable={} paired={} battery={} charging={}",
            i + 1,
            d.id,
            d.name,
            d.kind,
            yes(d.reachable),
            yes(d.paired),
            battery,
            yes(d.battery_charging)
        );
        devices.push(d);
    }

    if signatures {
        print_surface(&client, ids.first().map(String::as_str)).await;
        if let Some(id) = ids.first() {
            print_property_shapes(&client, id).await;
        }
    }

    for (i, d) in devices.iter().enumerate() {
        if d.paired {
            dump_conversations(&client, i + 1, d, signatures).await;
        }
    }
    0
}

async fn print_property_shapes(client: &Client, device_id: &str) {
    println!("reply shapes:");
    let path = device_path(device_id);
    for (p, iface) in [(path.clone(), IFACE_DEVICE), (format!("{path}/battery"), IFACE_BATTERY)] {
        match client
            .try_call(&p, IFACE_PROPS, "GetAll", Some(glib::prelude::ToVariant::to_variant(&(iface,))), None, CALL_TIMEOUT_MS)
            .await
        {
            Ok(v) => println!("    GetAll({iface}): {}", sig(&v)),
            Err(e) => println!("    GetAll({iface}): error {}", e.message()),
        }
    }
    match client.try_call(DAEMON_PATH, IFACE_DAEMON, "devices", None, None, CALL_TIMEOUT_MS).await {
        Ok(v) => println!("    devices(): {}", sig(&v)),
        Err(e) => println!("    devices(): error {}", e.message()),
    }
}

async fn print_surface(client: &Client, device_id: Option<&str>) {
    println!("D-Bus surface (expected by the app vs what the daemon exports):");
    let mut nodes: BTreeMap<String, Option<Node>> = BTreeMap::new();
    let mut missing = 0;
    let mut extras: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for &(sub, iface, kind, member) in EXPECTED {
        let path = match (sub, device_id) {
            (None, _) => DAEMON_PATH.to_string(),
            (Some(s), Some(id)) => format!("{}{s}", device_path(id)),
            (Some(_), None) => continue,
        };
        if !nodes.contains_key(&path) {
            let node = client.introspect(&path).await;
            nodes.insert(path.clone(), node);
        }
        let shown = match sub {
            None => "<daemon>".to_string(),
            Some("") => "<device>".to_string(),
            Some(s) => format!("<device>{s}"),
        };
        let iface_info = nodes[&path].as_ref().and_then(|n| n.interface(iface));
        let found = iface_info.and_then(|i| match kind {
            Kind::Method => i.method(member).map(|m| format!("method ({})->({})", m.args_in, m.args_out)),
            Kind::Signal => i.signal(member).map(|m| format!("signal ({})", m.args_in)),
            Kind::Property => i.property(member).map(|t| format!("property {t}")),
        });
        match found {
            Some(desc) => println!("    ok       {shown} {member}: {desc}"),
            None => {
                missing += 1;
                let why = if nodes[&path].is_none() {
                    "object missing"
                } else if iface_info.is_none() {
                    "interface missing"
                } else {
                    "member missing"
                };
                println!("    MISSING  {shown} {iface} {member} ({why})");
            }
        }
        if let Some(i) = iface_info {
            let set = extras.entry((shown.clone(), iface.to_string())).or_default();
            set.extend(i.methods.iter().map(|m| format!("method {}({})->({})", m.name, m.args_in, m.args_out)));
            set.extend(i.signals.iter().map(|m| format!("signal {}({})", m.name, m.args_in)));
            set.extend(i.properties.iter().map(|(n, t)| format!("property {n} {t}")));
        }
    }
    println!("    {missing} expected member(s) missing");

    // Members the daemon has that the app does not use (useful to know).
    let used: BTreeSet<(&str, &str)> = EXPECTED.iter().map(|&(_, i, _, m)| (i, m)).collect();
    println!("  unused members on those interfaces:");
    for ((shown, iface), members) in &extras {
        for m in members {
            let name = m.split([' ', '(']).nth(1).unwrap_or("");
            if !used.contains(&(iface.as_str(), name)) {
                println!("    {shown} {iface}: {m}");
            }
        }
    }

    if let Some(id) = device_id {
        if let Some(Some(dev)) = nodes.get(&device_path(id)) {
            println!("  objects under <device>: {}", dev.children.join(" "));
        }
        let notif_path = format!("{}/notifications", device_path(id));
        let first = nodes.get(&notif_path).and_then(|n| n.as_ref()).and_then(|n| n.children.first().cloned());
        match first {
            Some(child) => {
                let node = client.introspect(&format!("{notif_path}/{child}")).await;
                let iface = node.as_ref().and_then(|n| n.interface(IFACE_NOTIFICATION));
                for m in NOTIFICATION_MEMBERS {
                    let ok = iface.is_some_and(|i| i.method(m).is_some());
                    println!("    {}  <notification> {m}", if ok { "ok     " } else { "MISSING" });
                }
                if let Some(i) = iface {
                    let props: Vec<String> = i.properties.iter().map(|(n, t)| format!("{n}:{t}")).collect();
                    println!("    <notification> properties: {}", props.join(" "));
                }
            }
            None => println!("    (no notification objects to check dismiss/sendReply on)"),
        }
    }
}

async fn dump_conversations(client: &Client, n: usize, device: &Device, signatures: bool) {
    println!("device [{n}] conversations:");
    let entries = client.active_conversations(&device.id).await;
    let mut entry_shapes = Shapes::default();
    let parsed: Vec<_> = entries
        .iter()
        .filter_map(|v| {
            entry_shapes.add(v);
            variant::parse_message(v).map(|raw| parse_message(&raw))
        })
        .collect();
    println!(
        "  activeConversations: {} entries ({} parsed, {} malformed)",
        entries.len(),
        parsed.len(),
        entries.len() - parsed.len()
    );
    if signatures {
        entry_shapes.print("entry");
    }

    // Self-number detection as the app does it, minus the contact exclusion
    // (the dump does not read contacts).
    let key_lists: Vec<Vec<String>> =
        parsed.iter().map(|m| m.all_addresses.iter().map(|a| phone_key(a)).collect()).collect();
    let self_key = detect_self_key(&key_lists, &[]);
    let mut index = ConversationIndex::new();
    index.set_self_key(&self_key);
    let no_name = |_: &[String], _: &str| String::new();
    let threads: BTreeSet<i64> = parsed.iter().map(|m| m.thread_id).collect();
    for m in &parsed {
        let all = m.all_addresses.clone();
        index.ingest(m.clone(), Some(&all), &no_name);
    }
    let mut convs: Vec<_> = index.visible(None).into_iter().cloned().collect();
    convs.sort_by_key(|c| std::cmp::Reverse(c.sort_key()));
    println!(
        "  phone threads: {}   merged conversations: {}   groups: {}   self number found: {}",
        threads.len(),
        convs.len(),
        convs.iter().filter(|c| c.is_group()).count(),
        yes(!self_key.is_empty())
    );

    // Replay the newest conversations from the daemon's cache and count what
    // arrives as signals.
    let newest: Vec<_> = convs.iter().take(NEWEST).collect();
    let received: Rc<RefCell<Vec<Variant>>> = Rc::default();
    let loaded: Rc<RefCell<Vec<Variant>>> = Rc::default();
    let last_activity = Rc::new(Cell::new(Instant::now()));
    let path = device_path(&device.id);
    let mut subs: Vec<Subscription> = Vec::new();
    for name in ["conversationUpdated", "conversationCreated"] {
        let (received, last) = (received.clone(), last_activity.clone());
        subs.push(signals::subscribe(client.bus(), &path, IFACE_CONVERSATIONS, name, move |p| {
            received.borrow_mut().push(p.clone());
            last.set(Instant::now());
        }));
    }
    {
        let (loaded, last) = (loaded.clone(), last_activity.clone());
        subs.push(signals::subscribe(client.bus(), &path, IFACE_CONVERSATIONS, "conversationLoaded", move |p| {
            loaded.borrow_mut().push(p.clone());
            last.set(Instant::now());
        }));
    }
    let started = Instant::now();
    let mut requested = 0;
    for c in &newest {
        for &tid in &c.thread_ids {
            client.request_conversation(&device.id, tid, 0, REPLAY_PER_THREAD).await;
            requested += 1;
        }
    }
    last_activity.set(Instant::now());
    while last_activity.get().elapsed().as_secs_f64() < QUIET_SECONDS
        && started.elapsed().as_secs_f64() < MAX_WAIT_SECONDS
    {
        glib::timeout_future(Duration::from_millis(200)).await;
    }
    drop(subs);

    let received = received.take();
    let loaded = loaded.take();
    let mut signal_shapes = Shapes::default();
    let mut bad = 0;
    for p in &received {
        signal_shapes.add(p);
        match variant::parse_message_signal(p) {
            Some(raw) => {
                let m = parse_message(&raw);
                let all = m.all_addresses.clone();
                index.ingest(m, Some(&all), &no_name);
            }
            None => bad += 1,
        }
    }
    let mut loaded_shapes = Shapes::default();
    for p in &loaded {
        loaded_shapes.add(p);
    }
    println!(
        "  replay of the newest {}: {requested} thread request(s), {} message signal(s) ({bad} malformed), \
         {} conversationLoaded, {:.1} s",
        newest.len(),
        received.len(),
        loaded.len(),
        started.elapsed().as_secs_f64()
    );
    if signatures {
        signal_shapes.print("conversationUpdated/Created params");
        loaded_shapes.print("conversationLoaded params");
    }

    println!("  newest {} (after replay; up to {REPLAY_PER_THREAD} messages per phone thread):", newest.len());
    println!("     #   thread  phone-threads  addresses  group  messages  attachments");
    let (mut total_msgs, mut total_atts) = (0, 0);
    for (i, c) in newest.iter().enumerate() {
        let conv = index.get(c.thread_id).unwrap_or(c);
        let atts: usize = conv.messages.iter().map(|m| m.attachments.len()).sum();
        total_msgs += conv.messages.len();
        total_atts += atts;
        println!(
            "    {:>2}  {:>7}  {:>13}  {:>9}  {:>5}  {:>8}  {:>11}",
            i + 1,
            conv.thread_id,
            conv.thread_ids.len(),
            conv.addresses.len(),
            yes(conv.is_group()),
            conv.messages.len(),
            atts
        );
    }
    println!("    total: {total_msgs} messages, {total_atts} attachments");
    println!("  merged conversations after replay: {}", index.visible(None).len());
}
