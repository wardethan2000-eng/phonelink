//! Local SQLite store for conversations and messages (port of
//! `phonelink/store.py`).
//!
//! Same file, same schema, same PRAGMAs and the same JSON text encoding of
//! `addresses` / `attachments` (Python `json.dumps` defaults: `", "` / `": "`
//! separators, `ensure_ascii`), so a Python-written DB and a Rust-written DB
//! are interchangeable.
//!
//! Threading: the Rust store is used from the main thread only. There is no
//! Mutex; interior mutability is `Cell`s and the type is deliberately not
//! `Sync`. Like the Python store, write failures are logged, not returned.

use std::cell::Cell;
use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

use super::models::{Attachment, Conversation, SmsMessage};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS conversations (
    device_id     TEXT    NOT NULL,
    thread_id     INTEGER NOT NULL,
    address       TEXT,
    addresses     TEXT,
    display_name  TEXT,
    last_message  TEXT,
    last_date     INTEGER,
    is_read       INTEGER,
    PRIMARY KEY (device_id, thread_id)
);
CREATE TABLE IF NOT EXISTS messages (
    device_id   TEXT    NOT NULL,
    uid         INTEGER NOT NULL,
    thread_id   INTEGER,
    address     TEXT,
    body        TEXT,
    date        INTEGER,
    msg_type    INTEGER,
    read        INTEGER,
    attachments TEXT,
    PRIMARY KEY (device_id, uid)
);
CREATE INDEX IF NOT EXISTS idx_messages_thread
    ON messages (device_id, thread_id);
CREATE TABLE IF NOT EXISTS contacts (
    phone TEXT PRIMARY KEY,
    name  TEXT
);
CREATE TABLE IF NOT EXISTS contact_overrides (
    phone TEXT PRIMARY KEY,
    name  TEXT
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);
";

const UPSERT_CONVERSATION: &str = "
INSERT OR REPLACE INTO conversations
    (device_id, thread_id, address, addresses,
     display_name, last_message, last_date, is_read)
VALUES (?, ?, ?, ?, ?, ?, ?, ?)";

const UPSERT_MESSAGE: &str = "
INSERT OR REPLACE INTO messages
    (device_id, uid, thread_id, address, body, date,
     msg_type, read, attachments)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";

/// `~/.local/share/phonelink` (uses `$HOME`).
pub fn store_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".local").join("share").join("phonelink")
}

/// `~/.local/share/phonelink/messages.db`.
pub fn default_path() -> PathBuf {
    store_dir().join("messages.db")
}

/// SQLite-backed persistence for conversations and their messages.
/// Main thread only (see module docs).
pub struct MessageStore {
    path: String,
    conn: Option<Connection>,
    /// >0 while inside `batch()`: commits are deferred to the outermost exit.
    batch_depth: Cell<u32>,
    /// Number of commits issued (autocommit writes, transactions, batch ends).
    commits: Cell<u64>,
}

impl MessageStore {
    /// Open (creating if needed) the store at `path`; `":memory:"` is allowed.
    /// If it cannot be opened the store is inert (reads empty, writes no-op),
    /// like Python.
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_string_lossy().into_owned();
        let conn = match Self::open_connection(&path) {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("message store unavailable: {e}");
                None
            }
        };
        let store = Self { path, conn, batch_depth: Cell::new(0), commits: Cell::new(0) };
        if store.conn.is_some() {
            store.migrate_contacts_json_logged();
        }
        store
    }

    /// Open `~/.local/share/phonelink/messages.db`.
    pub fn open_default() -> Self {
        Self::open(default_path())
    }

    /// An in-memory store (tests).
    pub fn in_memory() -> Self {
        Self::open(":memory:")
    }

    /// The path this store was opened with.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// True if the database opened.
    pub fn is_open(&self) -> bool {
        self.conn.is_some()
    }

    /// Commits issued so far (a batch of any size counts as one).
    pub fn commit_count(&self) -> u64 {
        self.commits.get()
    }

    fn open_connection(path: &str) -> Result<Connection, Box<dyn Error>> {
        if path != ":memory:"
            && let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // journal_mode returns a row, so read it rather than execute().
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(conn)
    }

    /// One-time import of the legacy `contacts.json` next to the DB, guarded by
    /// a `meta` flag. The JSON file is left in place as a backup.
    ///
    /// Deviation: Python also looks for `./contacts.json` when the store is
    /// `:memory:`; here an in-memory store never imports.
    fn migrate_contacts_json(&self) -> Result<(), Box<dyn Error>> {
        let Some(conn) = self.conn.as_ref() else { return Ok(()) };
        let flag: Option<Option<String>> = conn
            .query_row("SELECT value FROM meta WHERE key='contacts_migrated'", [], |r| r.get(0))
            .optional()?;
        if flag.is_some() {
            return Ok(());
        }
        let mut rows: Vec<(String, String)> = Vec::new();
        let mut imported = 0usize;
        if self.path != ":memory:" {
            let legacy = Path::new(&self.path)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default()
                .join("contacts.json");
            if legacy.is_file() {
                let text = std::fs::read_to_string(&legacy)?;
                let data: Value = serde_json::from_str(&text)?;
                if let Value::Object(map) = data {
                    imported = map.len();
                    for (k, v) in &map {
                        if k.is_empty() {
                            continue;
                        }
                        let name = match v {
                            Value::Null | Value::Bool(false) => continue,
                            Value::String(s) if s.is_empty() => continue,
                            Value::Array(a) if a.is_empty() => continue,
                            Value::Object(o) if o.is_empty() => continue,
                            Value::Number(n) if n.as_f64() == Some(0.0) => continue,
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        rows.push((k.clone(), name));
                    }
                }
            }
        }
        self.with_txn(|c| {
            {
                let mut stmt =
                    c.prepare_cached("INSERT OR REPLACE INTO contacts (phone, name) VALUES (?, ?)")?;
                for (phone, name) in &rows {
                    stmt.execute(params![phone, name])?;
                }
            }
            c.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('contacts_migrated', '1')",
                [],
            )?;
            Ok(())
        })?;
        if imported > 0 {
            log::info!("migrated {imported} contacts from contacts.json");
        }
        Ok(())
    }

    fn migrate_contacts_json_logged(&self) {
        if let Err(e) = self.migrate_contacts_json() {
            log::warn!("contacts migration skipped: {e}");
        }
    }

    pub fn close(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = conn.close();
        }
    }

    // ── transactions ───────────────────────────────────────────────

    /// Coalesce many writes into a single commit (bulk sync path). Nested
    /// batches coalesce into the outermost one (depth counter, as in Python).
    pub fn batch<R>(&self, f: impl FnOnce(&Self) -> R) -> R {
        let depth = self.batch_depth.get();
        if depth == 0
            && let Some(conn) = self.conn.as_ref()
            && let Err(e) = conn.execute_batch("BEGIN")
        {
            log::warn!("store.batch begin failed: {e}");
        }
        self.batch_depth.set(depth + 1);
        let out = f(self);
        let depth = self.batch_depth.get().saturating_sub(1);
        self.batch_depth.set(depth);
        if depth == 0
            && let Some(conn) = self.conn.as_ref()
        {
            match conn.execute_batch("COMMIT") {
                Ok(()) => self.commits.set(self.commits.get() + 1),
                Err(e) => log::warn!("store.batch commit failed: {e}"),
            }
        }
        out
    }

    /// Run several statements atomically (joins the batch if one is open).
    fn with_txn<F>(&self, f: F) -> Result<(), rusqlite::Error>
    where
        F: FnOnce(&Connection) -> Result<(), rusqlite::Error>,
    {
        let Some(conn) = self.conn.as_ref() else { return Ok(()) };
        if self.batch_depth.get() > 0 {
            return f(conn);
        }
        conn.execute_batch("BEGIN")?;
        match f(conn) {
            Ok(()) => {
                conn.execute_batch("COMMIT")?;
                self.commits.set(self.commits.get() + 1);
                Ok(())
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// A single autocommit statement (or part of the open batch); failures are
    /// logged under `label`.
    fn write<F>(&self, label: &str, f: F)
    where
        F: FnOnce(&Connection) -> Result<(), rusqlite::Error>,
    {
        let Some(conn) = self.conn.as_ref() else { return };
        match f(conn) {
            Ok(()) => {
                if self.batch_depth.get() == 0 {
                    self.commits.set(self.commits.get() + 1);
                }
            }
            Err(e) => log::warn!("store.{label} failed: {e}"),
        }
    }

    fn write_txn<F>(&self, label: &str, f: F)
    where
        F: FnOnce(&Connection) -> Result<(), rusqlite::Error>,
    {
        if let Err(e) = self.with_txn(f) {
            log::warn!("store.{label} failed: {e}");
        }
    }

    // ── writes ─────────────────────────────────────────────────────

    /// Persist a single message under `thread_id` (the primary thread).
    /// Negative thread ids (drafts) are not persisted.
    pub fn upsert_message(&self, device_id: &str, thread_id: i64, msg: &SmsMessage) {
        if self.conn.is_none() || thread_id < 0 {
            return;
        }
        let attachments = encode_attachments(&msg.attachments);
        self.write("upsert_message", |c| {
            c.execute(
                UPSERT_MESSAGE,
                params![
                    device_id,
                    msg.uid,
                    thread_id,
                    msg.address,
                    msg.body,
                    msg.date,
                    msg.msg_type,
                    msg.read,
                    attachments
                ],
            )?;
            Ok(())
        });
    }

    /// Persist a conversation's metadata (not its messages).
    pub fn upsert_conversation(&self, device_id: &str, conv: &Conversation) {
        if self.conn.is_none() || conv.thread_id < 0 {
            return;
        }
        let addresses = encode_strings(&conv.addresses);
        self.write("upsert_conversation", |c| {
            c.execute(
                UPSERT_CONVERSATION,
                params![
                    device_id,
                    conv.thread_id,
                    conv.address,
                    addresses,
                    conv.display_name,
                    conv.last_message,
                    conv.last_date,
                    i32::from(conv.is_read)
                ],
            )?;
            Ok(())
        });
    }

    /// Persist many conversation metadata rows in one transaction.
    pub fn upsert_conversations<'a>(
        &self,
        device_id: &str,
        convs: impl IntoIterator<Item = &'a Conversation>,
    ) {
        if self.conn.is_none() {
            return;
        }
        let rows: Vec<(i64, &Conversation, String)> = convs
            .into_iter()
            .filter(|c| c.thread_id >= 0)
            .map(|c| (c.thread_id, c, encode_strings(&c.addresses)))
            .collect();
        if rows.is_empty() {
            return;
        }
        self.write_txn("upsert_conversations", |c| {
            let mut stmt = c.prepare_cached(UPSERT_CONVERSATION)?;
            for (thread_id, conv, addresses) in &rows {
                stmt.execute(params![
                    device_id,
                    thread_id,
                    conv.address,
                    addresses,
                    conv.display_name,
                    conv.last_message,
                    conv.last_date,
                    i32::from(conv.is_read)
                ])?;
            }
            Ok(())
        });
    }

    /// Remove a conversation and all of its messages.
    pub fn delete_conversation(&self, device_id: &str, thread_id: i64) {
        self.write_txn("delete_conversation", |c| {
            c.execute(
                "DELETE FROM conversations WHERE device_id=? AND thread_id=?",
                params![device_id, thread_id],
            )?;
            c.execute(
                "DELETE FROM messages WHERE device_id=? AND thread_id=?",
                params![device_id, thread_id],
            )?;
            Ok(())
        });
    }

    // ── contacts (normalised phone → display name) ─────────────────

    /// Names learned by guessing (notifications, calls, imported files).
    /// The phone's own contacts beat these; see `load_contact_map`.
    pub fn load_contacts(&self) -> HashMap<String, String> {
        self.load_name_table("contacts")
    }

    /// Names typed by hand ("Set contact name"): beat everything.
    pub fn load_contact_overrides(&self) -> HashMap<String, String> {
        self.load_name_table("contact_overrides")
    }

    pub fn save_contact_override(&self, phone: &str, name: &str) {
        if phone.is_empty() {
            return;
        }
        self.write("save_contact_override", |c| {
            c.execute(
                "INSERT OR REPLACE INTO contact_overrides (phone, name) VALUES (?, ?)",
                params![phone, name],
            )?;
            Ok(())
        });
    }

    fn load_name_table(&self, table: &str) -> HashMap<String, String> {
        let Some(conn) = self.conn.as_ref() else { return HashMap::new() };
        let result = (|| -> Result<HashMap<String, String>, rusqlite::Error> {
            let mut stmt = conn.prepare(&format!("SELECT phone, name FROM {table}"))?;
            let rows = stmt.query_map([], |r| Ok((text(r, 0), text(r, 1))))?;
            let mut out = HashMap::new();
            for row in rows {
                let (phone, name) = row?;
                if let Some(phone) = phone.filter(|p| !p.is_empty()) {
                    out.insert(phone, name.unwrap_or_default());
                }
            }
            Ok(out)
        })();
        result.unwrap_or_else(|e| {
            log::warn!("store.load {table} failed: {e}");
            HashMap::new()
        })
    }

    pub fn save_contact(&self, phone: &str, name: &str) {
        if phone.is_empty() {
            return;
        }
        self.write("save_contact", |c| {
            c.execute(
                "INSERT OR REPLACE INTO contacts (phone, name) VALUES (?, ?)",
                params![phone, name],
            )?;
            Ok(())
        });
    }

    pub fn delete_contact(&self, phone: &str) {
        if phone.is_empty() {
            return;
        }
        self.write("delete_contact", |c| {
            c.execute("DELETE FROM contacts WHERE phone=?", params![phone])?;
            Ok(())
        });
    }

    /// Make the contacts table exactly match `mapping` (one transaction).
    /// Entries with an empty key are skipped.
    pub fn replace_contacts(&self, mapping: &HashMap<String, String>) {
        self.write_txn("replace_contacts", |c| {
            c.execute("DELETE FROM contacts", [])?;
            let mut stmt =
                c.prepare_cached("INSERT OR REPLACE INTO contacts (phone, name) VALUES (?, ?)")?;
            for (phone, name) in mapping.iter().filter(|(k, _)| !k.is_empty()) {
                stmt.execute(params![phone, name])?;
            }
            Ok(())
        });
    }

    // ── reads ──────────────────────────────────────────────────────

    /// `{thread_id: Conversation}` with messages attached (oldest first).
    pub fn load_conversations(&self, device_id: &str) -> HashMap<i64, Conversation> {
        let Some(conn) = self.conn.as_ref() else { return HashMap::new() };
        match Self::read_conversations(conn, device_id) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("store.load_conversations failed: {e}");
                HashMap::new()
            }
        }
    }

    fn read_conversations(
        conn: &Connection,
        device_id: &str,
    ) -> Result<HashMap<i64, Conversation>, rusqlite::Error> {
        let mut stmt = conn.prepare(
            "SELECT thread_id, address, addresses, display_name,
                    last_message, last_date, is_read
             FROM conversations WHERE device_id=?",
        )?;
        let conv_rows = stmt
            .query_map(params![device_id], |r| {
                Ok((int(r, 0), text(r, 1), text(r, 2), text(r, 3), text(r, 4), int(r, 5), int(r, 6)))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut stmt = conn.prepare(
            "SELECT uid, thread_id, address, body, date, msg_type,
                    read, attachments
             FROM messages WHERE device_id=? ORDER BY date ASC",
        )?;
        let msg_rows = stmt
            .query_map(params![device_id], |r| {
                Ok((
                    int(r, 0),
                    int(r, 1),
                    text(r, 2),
                    text(r, 3),
                    int(r, 4),
                    int(r, 5),
                    int(r, 6),
                    text(r, 7),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut convs: HashMap<i64, Conversation> = HashMap::new();
        for (thread_id, address, addresses, display_name, last_message, last_date, is_read) in
            conv_rows
        {
            let thread_id = thread_id.unwrap_or(0);
            convs.insert(
                thread_id,
                Conversation {
                    thread_id,
                    address: address.unwrap_or_default(),
                    addresses: loads_strings(addresses.as_deref()),
                    display_name: display_name.unwrap_or_default(),
                    last_message: last_message.unwrap_or_default(),
                    last_date: last_date.unwrap_or(0),
                    is_read: is_read.unwrap_or(0) != 0,
                    ..Default::default()
                },
            );
        }

        for (uid, thread_id, address, body, date, msg_type, read, attachments) in msg_rows {
            let thread_id = thread_id.unwrap_or(0);
            let msg = SmsMessage {
                uid: uid.unwrap_or(0),
                body: body.unwrap_or_default(),
                address: address.unwrap_or_default(),
                date: date.unwrap_or(0),
                msg_type: msg_type.unwrap_or(0) as i32,
                read: read.unwrap_or(1) as i32,
                thread_id,
                attachments: loads_attachments(attachments.as_deref()),
                all_addresses: Vec::new(),
            };
            let conv = convs.entry(thread_id).or_insert_with(|| {
                // Orphan message (conversation row missing) — synthesize one.
                Conversation {
                    thread_id,
                    address: msg.address.clone(),
                    addresses: if msg.address.is_empty() {
                        Vec::new()
                    } else {
                        vec![msg.address.clone()]
                    },
                    last_message: msg.body.clone(),
                    last_date: msg.date,
                    is_read: msg.read != 0,
                    ..Default::default()
                }
            });
            conv.messages.push(msg);
        }
        Ok(convs)
    }
}

// ── row helpers (tolerant of whatever type Python stored) ──────────

fn text(row: &Row<'_>, i: usize) -> Option<String> {
    row.get_ref(i)
        .ok()
        .and_then(|v| v.as_str_or_null().ok().flatten())
        .map(str::to_owned)
}

fn int(row: &Row<'_>, i: usize) -> Option<i64> {
    row.get_ref(i).ok().and_then(|v| v.as_i64_or_null().ok().flatten())
}

/// Python `_loads_list`: NULL / empty / invalid / non-list → empty.
fn loads_list(value: Option<&str>) -> Vec<Value> {
    let Some(s) = value.filter(|s| !s.is_empty()) else { return Vec::new() };
    match serde_json::from_str::<Value>(s) {
        Ok(Value::Array(a)) => a,
        _ => Vec::new(),
    }
}

fn loads_strings(value: Option<&str>) -> Vec<String> {
    loads_list(value)
        .into_iter()
        .filter_map(|v| match v {
            Value::String(s) => Some(s),
            _ => None,
        })
        .collect()
}

fn loads_attachments(value: Option<&str>) -> Vec<Attachment> {
    loads_list(value)
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Attachment>(v).ok())
        .collect()
}

// ── Python-compatible JSON encoding ────────────────────────────────

/// Append `s` as a JSON string exactly as Python `json.dumps` (ensure_ascii).
fn push_py_string(out: &mut String, s: &str) {
    use std::fmt::Write;
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(ch),
            _ => {
                let mut buf = [0u16; 2];
                for unit in ch.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{:04x}", unit);
                }
            }
        }
    }
    out.push('"');
}

/// Append any JSON value the way Python `json.dumps` would (insertion order is
/// whatever the `Value` map holds; floats use serde's formatting).
fn push_py_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => push_py_string(out, s),
        Value::Array(a) => {
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                push_py_value(out, item);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                push_py_string(out, k);
                out.push_str(": ");
                push_py_value(out, item);
            }
            out.push('}');
        }
    }
}

/// `json.dumps(list_of_str)`.
fn encode_strings(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        push_py_string(&mut out, s);
    }
    out.push(']');
    out
}

/// `json.dumps(list_of_attachment_dicts)`. Known keys are written in the order
/// the Python app builds them (partId, mimeType, payload, uniqueIdentifier,
/// fileName), then any unknown keys.
fn encode_attachments(items: &[Attachment]) -> String {
    let mut out = String::from("[");
    for (i, a) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str("{\"partId\": ");
        out.push_str(&a.part_id.to_string());
        for (key, val) in [
            ("mimeType", &a.mime_type),
            ("payload", &a.payload),
            ("uniqueIdentifier", &a.unique_identifier),
            ("fileName", &a.file_name),
        ] {
            out.push_str(", ");
            push_py_string(&mut out, key);
            out.push_str(": ");
            push_py_string(&mut out, val);
        }
        for (k, v) in &a.extra {
            out.push_str(", ");
            push_py_string(&mut out, k);
            out.push_str(": ");
            push_py_value(&mut out, v);
        }
        out.push('}');
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(thread_id: i64, address: &str) -> Conversation {
        Conversation {
            thread_id,
            address: address.into(),
            addresses: vec![address.into()],
            ..Default::default()
        }
    }

    fn sms(uid: i64, thread_id: i64, address: &str, body: &str) -> SmsMessage {
        SmsMessage {
            uid,
            body: body.into(),
            address: address.into(),
            thread_id,
            ..Default::default()
        }
    }

    // ── encoder matches Python json.dumps byte-for-byte ────────────

    #[test]
    fn json_encoding_matches_python() {
        let att = Attachment {
            part_id: 1,
            mime_type: "image/jpeg".into(),
            file_name: "a.jpg".into(),
            ..Default::default()
        };
        assert_eq!(
            encode_attachments(&[att]),
            r#"[{"partId": 1, "mimeType": "image/jpeg", "payload": "", "uniqueIdentifier": "", "fileName": "a.jpg"}]"#
        );
        let strs = vec![
            "+1 (316) 555-1212".to_string(),
            "é\"\\\n\t\u{1}\u{7f}😀".to_string(),
            "日本".to_string(),
        ];
        assert_eq!(
            encode_strings(&strs),
            r#"["+1 (316) 555-1212", "\u00e9\"\\\n\t\u0001\u007f\ud83d\ude00", "\u65e5\u672c"]"#
        );
        assert_eq!(encode_strings(&[]), "[]");
        assert_eq!(encode_attachments(&[]), "[]");
    }

    #[test]
    fn loads_list_tolerates_bad_values() {
        assert!(loads_list(None).is_empty());
        assert!(loads_list(Some("")).is_empty());
        assert!(loads_list(Some("not json")).is_empty());
        assert!(loads_list(Some(r#"{"a": 1}"#)).is_empty());
        assert_eq!(loads_strings(Some(r#"["a", "b"]"#)), vec!["a", "b"]);
    }

    // ── MessageStoreTests ──────────────────────────────────────────

    struct Fixture {
        store: MessageStore, // dropped (closed) before the directory goes
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = MessageStore::open(dir.path().join("messages.db"));
        Fixture { _dir: dir, store }
    }

    #[test]
    fn test_message_and_conversation_round_trip() {
        let f = fixture();
        let c = Conversation {
            thread_id: 7,
            display_name: "Jane Doe".into(),
            address: "+13165551212".into(),
            addresses: vec!["+13165551212".into()],
            last_message: "hello".into(),
            last_date: 1000,
            is_read: false,
            ..Default::default()
        };
        let msg = SmsMessage {
            uid: 42,
            body: "hello".into(),
            address: "+13165551212".into(),
            date: 1000,
            msg_type: 1,
            read: 0,
            thread_id: 7,
            attachments: vec![Attachment {
                part_id: 1,
                file_name: "a.jpg".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        f.store.upsert_conversation("dev1", &c);
        f.store.upsert_message("dev1", c.thread_id, &msg);

        let loaded = f.store.load_conversations("dev1");
        assert!(loaded.contains_key(&7));
        let got = &loaded[&7];
        assert_eq!(got.display_name, "Jane Doe");
        assert!(!got.is_read);
        assert_eq!(got.messages.len(), 1);
        assert_eq!(got.messages[0].uid, 42);
        assert_eq!(got.messages[0].attachments[0].file_name, "a.jpg");
    }

    #[test]
    fn test_upsert_is_idempotent_by_uid() {
        let f = fixture();
        f.store.upsert_conversation("dev1", &conv(1, "111"));
        for body in ["v1", "v2"] {
            let mut m = sms(5, 1, "111", body);
            m.date = 1;
            f.store.upsert_message("dev1", 1, &m);
        }
        let loaded = f.store.load_conversations("dev1");
        assert_eq!(loaded[&1].messages.len(), 1);
        assert_eq!(loaded[&1].messages[0].body, "v2");
    }

    #[test]
    fn test_drafts_are_not_persisted() {
        let f = fixture();
        f.store.upsert_conversation("dev1", &conv(-1, "222"));
        f.store.upsert_message("dev1", -1, &sms(9, -1, "222", "draft"));
        assert!(f.store.load_conversations("dev1").is_empty());
    }

    #[test]
    fn test_delete_removes_conversation_and_messages() {
        let f = fixture();
        f.store.upsert_conversation("dev1", &conv(3, "333"));
        f.store.upsert_message("dev1", 3, &sms(1, 3, "333", "x"));
        f.store.delete_conversation("dev1", 3);
        assert!(f.store.load_conversations("dev1").is_empty());
    }

    #[test]
    fn test_devices_are_isolated() {
        let f = fixture();
        f.store.upsert_conversation("devA", &conv(1, "a"));
        f.store.upsert_conversation("devB", &conv(1, "b"));
        assert_eq!(f.store.load_conversations("devA")[&1].address, "a");
        assert_eq!(f.store.load_conversations("devB")[&1].address, "b");
    }

    #[test]
    fn test_batch_coalesces_commits_but_persists_all() {
        let f = fixture();
        f.store.upsert_conversation("dev1", &conv(1, "111"));
        let before = f.store.commit_count();
        f.store.batch(|s| {
            for uid in 0..10 {
                let mut m = sms(uid, 1, "111", &format!("m{uid}"));
                m.date = uid;
                s.upsert_message("dev1", 1, &m);
                // Nested batches coalesce into the outer one.
                s.batch(|s2| s2.upsert_message("dev1", 1, &m));
            }
        });
        // One commit for the whole batch...
        assert_eq!(f.store.commit_count() - before, 1);
        // ...and every message is durably persisted.
        let loaded = f.store.load_conversations("dev1");
        assert_eq!(loaded[&1].messages.len(), 10);
    }

    #[test]
    fn test_orphan_message_synthesizes_conversation() {
        // Message with no conversation row should still load under its thread.
        let f = fixture();
        f.store.upsert_message("dev1", 8, &sms(2, 8, "888", "hi"));
        let loaded = f.store.load_conversations("dev1");
        assert!(loaded.contains_key(&8));
        assert_eq!(loaded[&8].messages[0].body, "hi");
    }

    #[test]
    fn python_written_odd_columns_load() {
        let f = fixture();
        let conn = f.store.conn.as_ref().unwrap();
        conn.execute(
            "INSERT INTO conversations VALUES ('d', 1, NULL, '{\"x\": 1}', NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages VALUES ('d', 1, 1, NULL, NULL, NULL, NULL, NULL, 'oops')",
            [],
        )
        .unwrap();
        let loaded = f.store.load_conversations("d");
        let c = &loaded[&1];
        assert!(c.addresses.is_empty() && !c.is_read && c.last_date == 0);
        assert_eq!(c.messages[0].read, 1);
        assert!(c.messages[0].attachments.is_empty());
    }

    // ── ContactsStoreTests ─────────────────────────────────────────

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn test_save_load_delete_contact() {
        let f = fixture();
        f.store.save_contact("3165551212", "Jane");
        f.store.save_contact("3165559999", "Bob");
        assert_eq!(
            f.store.load_contacts(),
            map(&[("3165551212", "Jane"), ("3165559999", "Bob")])
        );
        f.store.save_contact("3165551212", "Jane Doe"); // upsert
        assert_eq!(f.store.load_contacts()["3165551212"], "Jane Doe");
        f.store.delete_contact("3165559999");
        assert_eq!(f.store.load_contacts(), map(&[("3165551212", "Jane Doe")]));
    }

    #[test]
    fn test_replace_contacts_makes_table_match_exactly() {
        let f = fixture();
        f.store.save_contact("111", "A");
        f.store.replace_contacts(&map(&[("222", "B"), ("333", "C")]));
        assert_eq!(f.store.load_contacts(), map(&[("222", "B"), ("333", "C")]));
    }

    #[test]
    fn test_contacts_json_migrated_once() {
        let f = fixture();
        // A pristine directory whose DB has never been opened, so the very
        // first store creation performs the one-time migration.
        let fresh = f._dir.path().join("fresh");
        std::fs::create_dir(&fresh).unwrap();
        std::fs::write(
            fresh.join("contacts.json"),
            r#"{"5551110000": "Legacy One", "5551112222": "Legacy Two"}"#,
        )
        .unwrap();
        let mut store2 = MessageStore::open(fresh.join("messages.db"));
        assert_eq!(
            store2.load_contacts(),
            map(&[("5551110000", "Legacy One"), ("5551112222", "Legacy Two")])
        );
        // A later edit + reopen must NOT re-import the legacy file over it.
        store2.delete_contact("5551110000");
        store2.close();
        let mut store3 = MessageStore::open(fresh.join("messages.db"));
        assert_eq!(store3.load_contacts(), map(&[("5551112222", "Legacy Two")]));
        store3.close();
    }

    #[test]
    fn in_memory_store_works() {
        let s = MessageStore::in_memory();
        s.save_contact("1", "A");
        assert_eq!(s.load_contacts(), map(&[("1", "A")]));
    }
}
