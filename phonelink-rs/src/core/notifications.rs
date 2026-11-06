//! The notifications sidebar's list logic (port of the pure parts of
//! `phonelink/ui/notifications_panel.py`): which notifications show, in what
//! order, the count line, and the in-place row diff that keeps expanded rows
//! open.

use std::collections::HashMap;

use super::models::Notification;

/// Every notification the phone has right now, by public id.
#[derive(Debug, Default)]
pub struct NotificationList {
    items: HashMap<String, Notification>,
}

impl NotificationList {
    /// A full reload (`activeNotifications`). A notification that was already
    /// listed keeps the time it first arrived, so a reload doesn't reshuffle
    /// the list (Python stamped every one with the reload time).
    pub fn replace_all(&mut self, entries: impl IntoIterator<Item = Notification>) {
        let old = std::mem::take(&mut self.items);
        for mut n in entries {
            if let Some(prev) = old.get(&n.public_id) {
                n.timestamp = prev.timestamp;
            }
            self.items.insert(n.public_id.clone(), n);
        }
    }

    /// Posted or updated: an update moves it to the top, as in Python.
    pub fn upsert(&mut self, n: Notification) {
        self.items.insert(n.public_id.clone(), n);
    }

    pub fn remove(&mut self, id: &str) -> bool {
        self.items.remove(id).is_some()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn get(&self, id: &str) -> Option<&Notification> {
        self.items.get(id)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// What the sidebar shows, newest first: nothing while notifications are
    /// off, and never an app on the hidden list (exact, case-sensitive match).
    pub fn visible(&self, enabled: bool, ignored_apps: &[String]) -> Vec<&Notification> {
        if !enabled {
            return Vec::new();
        }
        let mut out: Vec<&Notification> =
            self.items.values().filter(|n| !ignored_apps.iter().any(|a| *a == n.app_name)).collect();
        out.sort_by(|a, b| b.timestamp.total_cmp(&a.timestamp).then_with(|| a.public_id.cmp(&b.public_id)));
        out
    }

    /// Ids "Dismiss all" asks the phone to dismiss.
    pub fn dismissable_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.items.values().filter(|n| n.dismissable).map(|n| n.public_id.clone()).collect();
        ids.sort();
        ids
    }
}

/// The line above the list.
pub fn count_label(enabled: bool, visible: usize) -> String {
    match (enabled, visible) {
        (false, _) => "Notifications disabled".into(),
        (true, 0) => "No notifications".into(),
        (true, 1) => "1 notification".into(),
        (true, n) => format!("{n} notifications"),
    }
}

/// The fields a row shows. A row is rebuilt only when these change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    app_name: String,
    title: String,
    ticker: String,
    text: String,
    dismissable: bool,
    can_reply: bool,
    has_icon: bool,
    icon_path: String,
}

pub fn signature(n: &Notification) -> Signature {
    Signature {
        app_name: n.app_name.clone(),
        title: n.title.clone(),
        ticker: n.ticker.clone(),
        text: n.text.clone(),
        dismissable: n.dismissable,
        can_reply: n.can_reply(),
        has_icon: n.has_icon,
        icon_path: n.icon_path.clone(),
    }
}

/// `diff_notification_rows`: rows to drop, rows to add, and present rows
/// whose content changed (rebuilt). Untouched rows keep their expansion.
#[derive(Debug, Default, PartialEq)]
pub struct RowDiff {
    pub remove: Vec<String>,
    pub add: Vec<String>,
    pub recreate: Vec<String>,
}

pub fn diff_rows(existing: &HashMap<String, Signature>, desired: &[(String, Signature)]) -> RowDiff {
    let wanted: HashMap<&str, &Signature> = desired.iter().map(|(id, s)| (id.as_str(), s)).collect();
    let mut remove: Vec<String> = existing.keys().filter(|id| !wanted.contains_key(id.as_str())).cloned().collect();
    remove.sort();
    let mut diff = RowDiff { remove, ..Default::default() };
    for (id, sig) in desired {
        match existing.get(id) {
            None => diff.add.push(id.clone()),
            Some(old) if old != sig => diff.recreate.push(id.clone()),
            Some(_) => {}
        }
    }
    diff
}

/// The row's bold line.
pub fn row_title(n: &Notification) -> &str {
    [&n.title, &n.ticker].into_iter().find(|s| !s.is_empty()).map_or("(no title)", |s| s.as_str())
}

/// The row's dim line: the first line of the text (or the ticker).
pub fn row_snippet(n: &Notification) -> &str {
    let body = if n.text.is_empty() { &n.ticker } else { &n.text };
    body.split('\n').next().unwrap_or_default()
}

/// Icon for a notification without its own picture, by app name keyword.
pub fn app_icon_name(app_name: &str) -> &'static str {
    const MAP: [(&str, &str); 10] = [
        ("messages", "mail-unread-symbolic"),
        ("whatsapp", "mail-unread-symbolic"),
        ("telegram", "mail-unread-symbolic"),
        ("gmail", "mail-unread-symbolic"),
        ("mail", "mail-unread-symbolic"),
        ("phone", "call-start-symbolic"),
        ("clock", "alarm-symbolic"),
        ("chrome", "web-browser-symbolic"),
        ("firefox", "web-browser-symbolic"),
        ("youtube", "video-x-generic-symbolic"),
    ];
    let name = app_name.to_lowercase();
    MAP.iter().find(|(k, _)| name.contains(k)).map_or("dialog-information-symbolic", |(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(id: &str, app: &str, ts: f64) -> Notification {
        Notification {
            public_id: id.into(),
            app_name: app.into(),
            title: format!("title {id}"),
            dismissable: true,
            timestamp: ts,
            ..Default::default()
        }
    }

    #[test]
    fn newest_first_and_filtered() {
        let mut l = NotificationList::default();
        l.replace_all([n("a", "Messages", 1.0), n("b", "Gmail", 3.0), n("c", "Clock", 2.0)]);
        let ids: Vec<_> = l.visible(true, &[]).iter().map(|n| n.public_id.as_str()).collect();
        assert_eq!(ids, ["b", "c", "a"]);
        // Exact, case-sensitive app match, as Python.
        let ids: Vec<_> = l.visible(true, &["Gmail".into(), "clock".into()]).iter().map(|n| n.public_id.clone()).collect();
        assert_eq!(ids, ["c", "a"]);
        assert!(l.visible(false, &[]).is_empty());
    }

    #[test]
    fn reload_keeps_first_seen_time_and_update_moves_to_top() {
        let mut l = NotificationList::default();
        l.replace_all([n("a", "X", 1.0), n("b", "X", 2.0)]);
        l.replace_all([n("a", "X", 9.0), n("b", "X", 9.0), n("c", "X", 9.0)]);
        assert_eq!(l.get("a").unwrap().timestamp, 1.0);
        assert_eq!(l.get("c").unwrap().timestamp, 9.0);
        l.upsert(n("a", "X", 10.0));
        assert_eq!(l.visible(true, &[])[0].public_id, "a");
        assert!(l.remove("b"));
        assert!(!l.remove("b"));
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn counts() {
        assert_eq!(count_label(true, 0), "No notifications");
        assert_eq!(count_label(true, 1), "1 notification");
        assert_eq!(count_label(true, 4), "4 notifications");
        assert_eq!(count_label(false, 4), "Notifications disabled");
    }

    #[test]
    fn diff_keeps_unchanged_rows() {
        // Python's test_diff_notification_rows.
        let a = n("a", "X", 1.0);
        let mut b = n("b", "X", 1.0);
        let existing: HashMap<_, _> =
            [("a".to_string(), signature(&a)), ("b".to_string(), signature(&b)), ("gone".to_string(), signature(&a))]
                .into();
        b.text = "changed".into();
        let new = n("new", "X", 1.0);
        let desired = vec![("a".into(), signature(&a)), ("b".into(), signature(&b)), ("new".into(), signature(&new))];
        let d = diff_rows(&existing, &desired);
        assert_eq!(d.remove, ["gone"]);
        assert_eq!(d.add, ["new"]);
        assert_eq!(d.recreate, ["b"]);
    }

    #[test]
    fn row_text_fallbacks() {
        let mut x = n("a", "X", 1.0);
        x.title.clear();
        assert_eq!(row_title(&x), "(no title)");
        x.ticker = "Alex: hi".into();
        assert_eq!(row_title(&x), "Alex: hi");
        x.text = "line one\nline two".into();
        assert_eq!(row_snippet(&x), "line one");
        assert_eq!(app_icon_name("Google Messages"), "mail-unread-symbolic");
        assert_eq!(app_icon_name("YouTube"), "video-x-generic-symbolic");
        assert_eq!(app_icon_name("Bank"), "dialog-information-symbolic");
    }

    #[test]
    fn only_dismissable_are_dismissed() {
        let mut l = NotificationList::default();
        let mut keep = n("ongoing", "Music", 1.0);
        keep.dismissable = false;
        l.replace_all([n("b", "X", 1.0), keep, n("a", "X", 1.0)]);
        assert_eq!(l.dismissable_ids(), ["a", "b"]);
    }
}
