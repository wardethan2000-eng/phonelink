//! KDE Connect daemon client (port of `phonelink/dbus_client.py`).
//!
//! Every call is async (`call_future` on the GLib main loop; no worker
//! threads, unlike the Python `AsyncBridge`). As in Python, failures never
//! reach the UI as errors: each method returns a default and logs. Errors the
//! daemon is known to produce in normal use (a missing battery object,
//! `markConversationAsRead` on daemon 26.08) are logged at debug only.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use gio::prelude::*;
use glib::{Variant, VariantTy};

use super::introspect;
use super::variant::{self, string, string_map, unbox};
use super::*;
use crate::core::models::Device;

/// Whether a failure is expected (log at debug) or not (log at warn).
#[derive(Clone, Copy, PartialEq)]
enum Expect {
    Present,
    /// The object or method may legitimately be missing.
    MaybeMissing,
}

fn is_missing(err: &glib::Error) -> bool {
    err.matches(gio::DBusError::UnknownMethod)
        || err.matches(gio::DBusError::UnknownObject)
        || err.matches(gio::DBusError::UnknownInterface)
        || err.matches(gio::DBusError::UnknownProperty)
        || ["UnknownMethod", "UnknownObject", "UnknownInterface", "No such object path"]
            .iter()
            .any(|s| err.message().contains(s))
}

/// The daemon definitely refused a call (object, method or interface gone,
/// or arguments rejected), so it did nothing. A timeout or lost connection
/// is not a refusal: the call may have gone through.
pub fn is_definite_refusal(err: &glib::Error) -> bool {
    is_missing(err) || err.matches(gio::DBusError::InvalidArgs) || err.message().contains("InvalidArgs")
}

/// Return a correctly escaped file URI for a local path.
pub fn file_uri_for_path(path: &str) -> String {
    if path.starts_with("file://") {
        return path.to_string();
    }
    gio::File::for_path(path).uri().to_string()
}

/// `(object path, interface, method)`.
type MethodKey = (String, String, String);

#[derive(Clone)]
pub struct Client {
    bus: gio::DBusConnection,
    method_support: Rc<RefCell<HashMap<MethodKey, bool>>>,
}

/// SFTP state gathered in one go for the Files tab.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SftpState {
    pub mounted: bool,
    pub mount_point: String,
    pub directories: BTreeMap<String, String>,
}

impl Client {
    /// Connect to the session bus.
    pub async fn connect() -> Result<Self, glib::Error> {
        let bus = gio::bus_get_future(gio::BusType::Session).await?;
        Ok(Self::new(bus))
    }

    pub fn new(bus: gio::DBusConnection) -> Self {
        Self { bus, method_support: Rc::default() }
    }

    pub fn bus(&self) -> &gio::DBusConnection {
        &self.bus
    }

    // ── Low-level helpers ──────────────────────────────────────────

    /// One method call on the daemon, with its raw result.
    pub async fn try_call(
        &self,
        path: &str,
        iface: &str,
        method: &str,
        args: Option<Variant>,
        reply_type: Option<&str>,
        timeout_ms: i32,
    ) -> Result<Variant, glib::Error> {
        let rtype = reply_type.map(|t| VariantTy::new(t).expect("valid reply type"));
        self.bus
            .call_future(
                Some(BUS_NAME),
                path,
                iface,
                method,
                args.as_ref(),
                rtype,
                gio::DBusCallFlags::NONE,
                timeout_ms,
            )
            .await
    }

    async fn call_expect(
        &self,
        path: &str,
        iface: &str,
        method: &str,
        args: Option<Variant>,
        reply_type: Option<&str>,
        expect: Expect,
    ) -> Option<Variant> {
        let result = self.try_call(path, iface, method, args, reply_type, CALL_TIMEOUT_MS).await;
        Self::logged(result, path, iface, method, expect)
    }

    fn logged(
        result: Result<Variant, glib::Error>,
        path: &str,
        iface: &str,
        method: &str,
        expect: Expect,
    ) -> Option<Variant> {
        match result {
            Ok(v) => Some(v),
            Err(e) if expect == Expect::MaybeMissing && is_missing(&e) => {
                log::debug!("D-Bus: {iface}.{method} @ {path}: {}", e.message());
                None
            }
            Err(e) => {
                log::warn!("D-Bus: {iface}.{method} @ {path}: {}", e.message());
                None
            }
        }
    }

    /// Synchronous-in-spirit `_call` of the Python: `None` on any failure.
    async fn call(
        &self,
        path: &str,
        iface: &str,
        method: &str,
        args: Option<Variant>,
        reply_type: Option<&str>,
    ) -> Option<Variant> {
        self.call_expect(path, iface, method, args, reply_type, Expect::Present).await
    }

    /// The first value of a reply tuple.
    fn first(reply: Option<Variant>) -> Option<Variant> {
        reply.and_then(|r| r.try_child_value(0)).map(|v| unbox(&v))
    }

    async fn get_prop(&self, path: &str, iface: &str, prop: &str, expect: Expect) -> Option<Variant> {
        let reply = self
            .call_expect(path, IFACE_PROPS, "Get", Some((iface, prop).to_variant()), Some("(v)"), expect)
            .await;
        Self::first(reply)
    }

    /// All properties of one interface (`GetAll`), unboxed.
    async fn get_all(&self, path: &str, iface: &str, expect: Expect) -> BTreeMap<String, Variant> {
        let reply = self
            .call_expect(path, IFACE_PROPS, "GetAll", Some((iface,).to_variant()), None, expect)
            .await;
        Self::first(reply).map(|m| string_map(&m).into_iter().collect()).unwrap_or_default()
    }

    /// Introspection of one object, parsed. `None` if the object is missing.
    pub async fn introspect(&self, path: &str) -> Option<introspect::Node> {
        let reply = self
            .call_expect(
                path,
                "org.freedesktop.DBus.Introspectable",
                "Introspect",
                None,
                Some("(s)"),
                Expect::MaybeMissing,
            )
            .await;
        Self::first(reply).and_then(|v| v.str().map(introspect::parse))
    }

    pub async fn supports_method(&self, path: &str, iface: &str, method: &str) -> bool {
        let key = (path.to_string(), iface.to_string(), method.to_string());
        if let Some(&known) = self.method_support.borrow().get(&key) {
            return known;
        }
        let supported = self
            .introspect(path)
            .await
            .and_then(|n| n.interface(iface).map(|i| i.method(method).is_some()))
            .unwrap_or(false);
        self.method_support.borrow_mut().insert(key, supported);
        supported
    }

    // ── Daemon ─────────────────────────────────────────────────────

    /// True if the KDE Connect daemon owns its bus name.
    pub async fn is_daemon_available(&self) -> bool {
        self.daemon_name_has_owner().await.unwrap_or(false)
    }

    /// `NameHasOwner` for the daemon's bus name, keeping the error so
    /// `--dump` can say why it could not tell.
    pub async fn daemon_name_has_owner(&self) -> Result<bool, String> {
        let reply = self
            .bus
            .call_future(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "NameHasOwner",
                Some(&(BUS_NAME,).to_variant()),
                Some(VariantTy::new("(b)").unwrap()),
                gio::DBusCallFlags::NONE,
                CALL_TIMEOUT_MS,
            )
            .await
            .map_err(|e| e.message().to_string())?;
        reply.get::<(bool,)>().map(|(b,)| b).ok_or_else(|| "NameHasOwner: unexpected reply".to_string())
    }

    /// Bus names on this session bus that mention KDE Connect. Diagnostic
    /// only: shows whether the daemon is on a different bus or a different name.
    pub async fn kdeconnect_bus_names(&self) -> Result<Vec<String>, String> {
        let reply = self
            .bus
            .call_future(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ListNames",
                None,
                Some(VariantTy::new("(as)").unwrap()),
                gio::DBusCallFlags::NONE,
                CALL_TIMEOUT_MS,
            )
            .await
            .map_err(|e| e.message().to_string())?;
        let (names,) = reply.get::<(Vec<String>,)>().ok_or_else(|| "ListNames: unexpected reply".to_string())?;
        Ok(names.into_iter().filter(|n| n.to_ascii_lowercase().contains("kdeconnect")).collect())
    }

    /// All known device ids.
    pub async fn device_ids(&self) -> Vec<String> {
        // The no-arg form (onlyReachable=false, onlyPaired=false) first.
        let reply = match self.call(DAEMON_PATH, IFACE_DAEMON, "devices", None, Some("(as)")).await {
            Some(r) => Some(r),
            None => {
                self.call(DAEMON_PATH, IFACE_DAEMON, "devices", Some((false, false).to_variant()), Some("(as)"))
                    .await
            }
        };
        reply.and_then(|r| r.get::<(Vec<String>,)>()).map(|(ids,)| ids).unwrap_or_default()
    }

    /// Full state of one device. One `GetAll` for the device and one for the
    /// battery, instead of the Python's six property reads.
    pub async fn fetch_device(&self, device_id: &str) -> Device {
        let path = device_path(device_id);
        let props = self.get_all(&path, IFACE_DEVICE, Expect::Present).await;
        let battery = self.get_all(&format!("{path}/battery"), IFACE_BATTERY, Expect::MaybeMissing).await;
        let text = |m: &BTreeMap<String, Variant>, k: &str| m.get(k).and_then(|v| v.str().map(str::to_string));
        let flag = |m: &BTreeMap<String, Variant>, k: &str| m.get(k).and_then(variant::int).is_some_and(|n| n != 0);
        Device {
            id: device_id.to_string(),
            name: text(&props, "name").filter(|s| !s.is_empty()).unwrap_or_else(|| "Unknown".into()),
            kind: text(&props, "type").filter(|s| !s.is_empty()).unwrap_or_else(|| "phone".into()),
            reachable: flag(&props, "isReachable"),
            paired: flag(&props, "isPaired"),
            battery_charge: battery.get("charge").and_then(variant::int).map_or(-1, |n| n as i32),
            battery_charging: flag(&battery, "isCharging"),
        }
    }

    /// Full state of every known device.
    pub async fn fetch_devices(&self) -> Vec<Device> {
        let mut devices = Vec::new();
        for id in self.device_ids().await {
            devices.push(self.fetch_device(&id).await);
        }
        devices
    }

    // ── Device properties ──────────────────────────────────────────

    pub async fn device_name(&self, device_id: &str) -> String {
        let v = self.get_prop(&device_path(device_id), IFACE_DEVICE, "name", Expect::Present).await;
        v.map(|v| string(&v)).filter(|s| !s.is_empty()).unwrap_or_else(|| "Unknown".into())
    }

    pub async fn device_type(&self, device_id: &str) -> String {
        let v = self.get_prop(&device_path(device_id), IFACE_DEVICE, "type", Expect::Present).await;
        v.map(|v| string(&v)).filter(|s| !s.is_empty()).unwrap_or_else(|| "phone".into())
    }

    pub async fn is_device_reachable(&self, device_id: &str) -> bool {
        let v = self.get_prop(&device_path(device_id), IFACE_DEVICE, "isReachable", Expect::Present).await;
        v.and_then(|v| variant::int(&v)).is_some_and(|n| n != 0)
    }

    pub async fn is_device_paired(&self, device_id: &str) -> bool {
        let v = self.get_prop(&device_path(device_id), IFACE_DEVICE, "isPaired", Expect::Present).await;
        v.and_then(|v| variant::int(&v)).is_some_and(|n| n != 0)
    }

    // ── Battery ────────────────────────────────────────────────────

    /// Charge percent, or -1 when unknown (the battery object is sometimes
    /// missing; that is logged at debug only).
    pub async fn battery_charge(&self, device_id: &str) -> i32 {
        let path = format!("{}/battery", device_path(device_id));
        let v = self.get_prop(&path, IFACE_BATTERY, "charge", Expect::MaybeMissing).await;
        v.and_then(|v| variant::int(&v)).map_or(-1, |n| n as i32)
    }

    pub async fn is_battery_charging(&self, device_id: &str) -> bool {
        let path = format!("{}/battery", device_path(device_id));
        let v = self.get_prop(&path, IFACE_BATTERY, "isCharging", Expect::MaybeMissing).await;
        v.and_then(|v| variant::int(&v)).is_some_and(|n| n != 0)
    }

    // ── Actions ────────────────────────────────────────────────────

    /// Make the phone play a find-my-phone sound.
    pub async fn ring_device(&self, device_id: &str) {
        let path = format!("{}/findmyphone", device_path(device_id));
        self.call(&path, IFACE_FINDPHONE, "ring", None, None).await;
    }

    /// Push the desktop clipboard to the device. Daemon 26.08 only has the
    /// argument-less `sendClipboard()`, which sends the desktop's current
    /// clipboard; `text` must already be on it. Older daemons that only take
    /// the text get `sendClipboard(text)`.
    pub async fn send_clipboard(&self, device_id: &str, text: &str) -> bool {
        let path = format!("{}/clipboard", device_path(device_id));
        match self.try_call(&path, IFACE_CLIPBOARD, "sendClipboard", None, None, CALL_TIMEOUT_MS).await {
            Ok(_) => true,
            Err(e) if is_definite_refusal(&e) => {
                self.call(&path, IFACE_CLIPBOARD, "sendClipboard", Some((text,).to_variant()), None).await.is_some()
            }
            Err(e) => {
                log::warn!("D-Bus: sendClipboard @ {path}: {}", e.message());
                false
            }
        }
    }

    /// The phone's current clipboard content.
    pub async fn clipboard_content(&self, device_id: &str) -> String {
        let path = format!("{}/clipboard", device_path(device_id));
        let reply = self.call(&path, IFACE_CLIPBOARD, "content", None, Some("(s)")).await;
        reply.and_then(|r| r.get::<(String,)>()).map(|(s,)| s).unwrap_or_default()
    }

    pub async fn share_url(&self, device_id: &str, url: &str) {
        let path = format!("{}/share", device_path(device_id));
        self.call(&path, IFACE_SHARE, "shareUrl", Some((url,).to_variant()), None).await;
    }

    pub async fn share_file(&self, device_id: &str, file: &str) {
        self.share_url(device_id, &file_uri_for_path(file)).await;
    }

    pub async fn share_files(&self, device_id: &str, files: &[&str]) {
        let urls: Vec<String> = files.iter().map(|p| file_uri_for_path(p)).collect();
        self.share_urls(device_id, &urls).await;
    }

    pub async fn share_urls(&self, device_id: &str, urls: &[String]) {
        let path = format!("{}/share", device_path(device_id));
        self.call(&path, IFACE_SHARE, "shareUrls", Some((urls.to_vec(),).to_variant()), None).await;
    }

    // ── SMS / Conversations ────────────────────────────────────────

    /// The daemon's cached conversation list: the newest message of each
    /// thread, as raw variants (parse with [`variant::parse_message`]).
    pub async fn active_conversations(&self, device_id: &str) -> Vec<Variant> {
        let reply = self
            .call(&device_path(device_id), IFACE_CONVERSATIONS, "activeConversations", None, Some("(av)"))
            .await;
        reply.map(|r| variant::message_variants(&r)).unwrap_or_default()
    }

    /// Ask the phone for its conversation list; results arrive as
    /// `conversationCreated` / `conversationUpdated` signals.
    pub async fn request_all_conversations(&self, device_id: &str) {
        self.call(&device_path(device_id), IFACE_CONVERSATIONS, "requestAllConversationThreads", None, None)
            .await;
    }

    /// Replay messages `start..end` (newest-first indices) of a thread from the
    /// daemon's cache, as `conversationUpdated` signals.
    pub async fn request_conversation(&self, device_id: &str, thread_id: i64, start: i32, end: i32) {
        self.call(
            &device_path(device_id),
            IFACE_CONVERSATIONS,
            "requestConversation",
            Some((thread_id, start, end).to_variant()),
            None,
        )
        .await;
    }

    /// Ask the phone itself for `count` messages of a thread (the newest, or
    /// those at or before `older_than_ms`). Bypasses the daemon's cache; the
    /// daemon emits only `conversationLoaded`, so replay with
    /// [`Self::request_conversation`].
    pub async fn request_thread_from_phone(&self, device_id: &str, thread_id: i64, count: i64, older_than_ms: i64) {
        let path = format!("{}/sms", device_path(device_id));
        self.call(&path, IFACE_SMS, "requestConversation", Some((thread_id, older_than_ms, count).to_variant()), None)
            .await;
    }

    /// Ask the phone to send an attachment file to the desktop.
    pub async fn request_attachment_file(&self, device_id: &str, part_id: i64, unique_identifier: &str) -> bool {
        self.call(
            &device_path(device_id),
            IFACE_CONVERSATIONS,
            "requestAttachmentFile",
            Some((part_id, unique_identifier).to_variant()),
            None,
        )
        .await
        .is_some()
    }

    /// Tell the phone a conversation was read. Absent in daemon 26.08; that
    /// `UnknownMethod` is expected and logged at debug.
    pub async fn mark_conversation_as_read(&self, device_id: &str, thread_id: i64) {
        self.call_expect(
            &device_path(device_id),
            IFACE_CONVERSATIONS,
            "markConversationAsRead",
            Some((thread_id,).to_variant()),
            None,
            Expect::MaybeMissing,
        )
        .await;
    }

    /// Reply to an existing thread (the only send path the S25 honors for
    /// existing conversations).
    pub async fn reply_to_conversation(
        &self,
        device_id: &str,
        thread_id: i64,
        message: &str,
        attachments: &[String],
    ) -> bool {
        let atts: Vec<Variant> = attachments.iter().map(|a| a.to_variant()).collect();
        let args = Variant::tuple_from_iter([thread_id.to_variant(), message.to_variant(), atts.to_variant()]);
        self.call(&device_path(device_id), IFACE_CONVERSATIONS, "replyToConversation", Some(args), None)
            .await
            .is_some()
    }

    /// Delete a conversation on the phone, if the daemon supports it.
    pub async fn delete_conversation(&self, device_id: &str, thread_id: i64) -> bool {
        if !self.supports_conversation_deletion(device_id).await {
            return false;
        }
        self.call(
            &device_path(device_id),
            IFACE_CONVERSATIONS,
            "deleteConversation",
            Some((thread_id,).to_variant()),
            None,
        )
        .await
        .is_some()
    }

    pub async fn supports_conversation_deletion(&self, device_id: &str) -> bool {
        self.supports_method(&device_path(device_id), IFACE_CONVERSATIONS, "deleteConversation").await
    }

    /// Send to new recipients. Only for brand-new conversations: the S25
    /// silently drops it for existing threads (use `reply_to_conversation`).
    pub async fn send_sms(&self, device_id: &str, addresses: &[String], message: &str, attachments: &[String]) {
        let addrs: Vec<Variant> = addresses.iter().map(|a| a.to_variant()).collect();
        let atts: Vec<Variant> = attachments.iter().map(|a| a.to_variant()).collect();
        let args = Variant::tuple_from_iter([addrs.to_variant(), message.to_variant(), atts.to_variant()]);
        self.call(&device_path(device_id), IFACE_CONVERSATIONS, "sendWithoutConversation", Some(args), None)
            .await;
    }

    // ── Contacts ───────────────────────────────────────────────────

    /// Ask the phone to sync its contacts to the local vCard cache.
    pub async fn sync_contacts(&self, device_id: &str) {
        let path = format!("{}/contacts", device_path(device_id));
        self.call(&path, IFACE_CONTACTS, "synchronizeRemoteWithLocal", None, None).await;
    }

    // ── SFTP / File browsing ───────────────────────────────────────

    fn sftp_path(device_id: &str) -> String {
        format!("{}/sftp", device_path(device_id))
    }

    async fn sftp_bool(&self, device_id: &str, method: &str, timeout_ms: i32) -> bool {
        let path = Self::sftp_path(device_id);
        let result = self.try_call(&path, IFACE_SFTP, method, None, Some("(b)"), timeout_ms).await;
        let reply = Self::logged(result, &path, IFACE_SFTP, method, Expect::Present);
        reply.and_then(|r| r.get::<(bool,)>()).is_some_and(|(b,)| b)
    }

    async fn sftp_string(&self, device_id: &str, method: &str) -> String {
        let reply = self.call(&Self::sftp_path(device_id), IFACE_SFTP, method, None, Some("(s)")).await;
        reply.and_then(|r| r.get::<(String,)>()).map(|(s,)| s).unwrap_or_default()
    }

    pub async fn sftp_is_mounted(&self, device_id: &str) -> bool {
        self.sftp_bool(device_id, "isMounted", CALL_TIMEOUT_MS).await
    }

    pub async fn sftp_mount_and_wait(&self, device_id: &str) -> bool {
        self.sftp_bool(device_id, "mountAndWait", MOUNT_TIMEOUT_MS).await
    }

    pub async fn sftp_unmount(&self, device_id: &str) {
        self.call(&Self::sftp_path(device_id), IFACE_SFTP, "unmount", None, None).await;
    }

    pub async fn sftp_mount_point(&self, device_id: &str) -> String {
        self.sftp_string(device_id, "mountPoint").await
    }

    pub async fn sftp_get_mount_error(&self, device_id: &str) -> String {
        self.sftp_string(device_id, "getMountError").await
    }

    /// Exposed directories: path → display name.
    pub async fn sftp_get_directories(&self, device_id: &str) -> BTreeMap<String, String> {
        let reply = self.call(&Self::sftp_path(device_id), IFACE_SFTP, "getDirectories", None, Some("(a{sv})")).await;
        Self::first(reply)
            .map(|m| string_map(&m).into_iter().map(|(k, v)| (k, string(&v))).collect())
            .unwrap_or_default()
    }

    /// Mount status, mount point and directories in one go.
    pub async fn fetch_sftp_state(&self, device_id: &str) -> SftpState {
        let mounted = self.sftp_is_mounted(device_id).await;
        let mount_point = self.sftp_mount_point(device_id).await;
        let directories = if mounted { self.sftp_get_directories(device_id).await } else { BTreeMap::new() };
        SftpState { mounted, mount_point, directories }
    }

    // ── Notifications ──────────────────────────────────────────────

    /// Public ids of the phone's active notifications.
    pub async fn active_notification_ids(&self, device_id: &str) -> Vec<String> {
        let path = format!("{}/notifications", device_path(device_id));
        let reply = self.call(&path, IFACE_NOTIFICATIONS, "activeNotifications", None, Some("(as)")).await;
        reply.and_then(|r| r.get::<(Vec<String>,)>()).map(|(ids,)| ids).unwrap_or_default()
    }

    /// All properties of one notification object.
    pub async fn notification_properties(&self, device_id: &str, notif_id: &str) -> BTreeMap<String, Variant> {
        let path = format!("{}/notifications/{notif_id}", device_path(device_id));
        self.get_all(&path, IFACE_NOTIFICATION, Expect::Present).await
    }

    /// `[(id, properties)]` for every active notification.
    pub async fn fetch_active_notifications(&self, device_id: &str) -> Vec<(String, BTreeMap<String, Variant>)> {
        let mut out = Vec::new();
        for id in self.active_notification_ids(device_id).await {
            let props = self.notification_properties(device_id, &id).await;
            if !props.is_empty() {
                out.push((id, props));
            }
        }
        out
    }

    pub async fn dismiss_notification(&self, device_id: &str, notif_id: &str) {
        let path = format!("{}/notifications/{notif_id}", device_path(device_id));
        self.call(&path, IFACE_NOTIFICATION, "dismiss", None, None).await;
    }

    /// Reply through the notification object's own `sendReply`.
    pub async fn reply_to_notification(&self, device_id: &str, notif_id: &str, message: &str) -> bool {
        let path = format!("{}/notifications/{notif_id}", device_path(device_id));
        self.call(&path, IFACE_NOTIFICATION, "sendReply", Some((message,).to_variant()), None)
            .await
            .is_some()
    }

    /// [`Self::send_notification_reply_by_id`] with the error, so a caller can
    /// tell a refusal ([`is_definite_refusal`]) from a timeout.
    pub async fn try_send_notification_reply_by_id(
        &self,
        device_id: &str,
        reply_id: &str,
        message: &str,
    ) -> Result<(), glib::Error> {
        let path = format!("{}/notifications", device_path(device_id));
        let args = Some((reply_id, message).to_variant());
        self.try_call(&path, IFACE_NOTIFICATIONS, "sendReply", args, None, CALL_TIMEOUT_MS).await.map(|_| ())
    }

    /// Reply by `replyId` through the notifications interface.
    pub async fn send_notification_reply_by_id(&self, device_id: &str, reply_id: &str, message: &str) -> bool {
        let path = format!("{}/notifications", device_path(device_id));
        self.call(&path, IFACE_NOTIFICATIONS, "sendReply", Some((reply_id, message).to_variant()), None)
            .await
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_are_escaped() {
        assert_eq!(file_uri_for_path("file:///a%20b"), "file:///a%20b");
        assert_eq!(file_uri_for_path("/tmp/a b#1.jpg"), "file:///tmp/a%20b%231.jpg");
    }

    #[test]
    fn missing_errors_are_recognized() {
        let e = glib::Error::new(gio::DBusError::UnknownMethod, "no markConversationAsRead");
        assert!(is_missing(&e));
        let e = glib::Error::new(gio::IOErrorEnum::Failed, "GDBus.Error:org.freedesktop.DBus.Error.UnknownObject: x");
        assert!(is_missing(&e));
        let e = glib::Error::new(gio::IOErrorEnum::TimedOut, "Timeout was reached");
        assert!(!is_missing(&e));
        assert!(!is_definite_refusal(&e));
        let e = glib::Error::new(gio::DBusError::InvalidArgs, "bad");
        assert!(is_definite_refusal(&e));
        let e = glib::Error::new(gio::DBusError::NoReply, "no reply");
        assert!(!is_definite_refusal(&e));
    }
}
