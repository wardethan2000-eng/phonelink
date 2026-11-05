//! Contact name resolution: phone keys, vCard parsing, CSV import and
//! notification/telephony name harvesting
//! (port of the pure parts of `phonelink/contacts.py`).
//!
//! The SQLite store stays at the edge. Where Python loaded and saved the local
//! contact map itself, these functions take the map (`HashMap<String, String>`,
//! normalized phone -> name) and return what changed; the caller loads it with
//! `MessageStore::load_contacts()` and persists with `replace_contacts` /
//! `save_contact` (e.g. `if import_vcf_file(p, &mut map) > 0 { replace }`).
//! `save_contact` / `delete_contact` have no pure part: call
//! `normalize_phone(addr)` and then the store.
//!
//! Notification properties are `Props = HashMap<String, serde_json::Value>`
//! (what the D-Bus layer produces from `a{sv}`); string values are used as
//! is, other scalars via their JSON text, missing/null as "".
//!
//! Deviations: `casefold` is `to_lowercase`; `Cf` stripping uses a built-in
//! list of the Unicode format characters; quoted-printable charsets known are
//! utf-8, ascii and latin-1/windows-1252 (anything else falls back to utf-8,
//! like Python's LookupError path); `Path.glob` / `iterdir` order is not
//! reproduced (vCard files are read in name order for determinism).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::core::models::Conversation;
use crate::core::settings::{data_dir, home_dir};

pub type Props = HashMap<String, Value>;

/// `~/.local/share/kpeoplevcard` (KDE Connect stores synced vCards here).
pub fn vcard_base(home: &Path) -> PathBuf {
    home.join(".local").join("share").join("kpeoplevcard")
}

/// `~/.local/share/phonelink/contact_photos`
pub fn contact_photos_dir(home: &Path) -> PathBuf {
    data_dir(home).join("contact_photos")
}

// ── Phone / text helpers ───────────────────────────────────────────

fn non_digit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\D").expect("valid regex"))
}

/// Strip a phone number to digits only for comparison.
pub fn normalize_phone(number: &str) -> String {
    non_digit_re().replace_all(number, "").into_owned()
}

fn is_format_char(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891
        | 0x08E2 | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064
        | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD
        | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A
        | 0xE0001 | 0xE0020..=0xE007F)
}

/// Strip surrounding whitespace and invisible formatting marks.
pub fn clean_text(text: &str) -> String {
    let cleaned: String = text.chars().filter(|c| !is_format_char(*c)).collect();
    cleaned.trim().to_string()
}

fn last_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    let (i, _) = s.char_indices().nth(count - n).expect("index in range");
    &s[i..]
}

fn first_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Last 10 digits of a number (or all of them when shorter).
pub fn phone_key(number: &str) -> String {
    let norm = normalize_phone(number);
    if norm.chars().count() >= 10 {
        last_chars(&norm, 10).to_string()
    } else {
        norm
    }
}

pub fn photo_suffix(content_type: Option<&str>) -> &'static str {
    let content = content_type
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    match content.as_str() {
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/png" => ".png",
        "image/webp" => ".webp",
        _ => ".jpg",
    }
}

/// True when the text is effectively just a phone number. (ASCII digits only;
/// Python's `isdigit` also accepts exotic digit characters.)
pub fn looks_like_phone_number(text: &str) -> bool {
    let cleaned = clean_text(text);
    let candidate: String = cleaned.chars().filter(|c| !matches!(c, '+' | '-' | ' ')).collect();
    !candidate.is_empty() && candidate.chars().all(|c| c.is_ascii_digit())
}

/// Best-effort detection for SMS / messaging notification sources.
pub fn is_messaging_app(app_name: &str) -> bool {
    let app = clean_text(app_name).to_lowercase();
    if app.is_empty() {
        return false;
    }
    ["message", "messages", "messaging", "google messages", "samsung messages", "sms", "mms"]
        .iter()
        .any(|k| app.contains(k))
}

fn prop_text(props: &Props, key: &str) -> String {
    match props.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => clean_text(s),
        Some(other) => clean_text(&other.to_string()),
    }
}

/// First line the way Python's `str.splitlines()` splits.
fn first_line(s: &str) -> &str {
    let end = s
        .find(|c: char| {
            matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e'
                | '\u{85}' | '\u{2028}' | '\u{2029}')
        })
        .unwrap_or(s.len());
    &s[..end]
}

/// Extract the user-visible message preview from a notification.
pub fn notification_message_text(props: &Props) -> String {
    let ticker = prop_text(props, "ticker");
    let text = prop_text(props, "text");
    if let Some((_, rest)) = ticker.split_once(": ") {
        return rest.trim().to_string();
    }
    if !text.is_empty() {
        return first_line(&text).trim().to_string();
    }
    ticker
}

// ── Photos ─────────────────────────────────────────────────────────

fn photos_with_prefix(dir: &Path, norm: &str) -> Vec<PathBuf> {
    let prefix = format!("{norm}.");
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| e.path())
        .collect()
}

/// Returns true when a new file was written.
pub fn store_contact_photo_in(
    dir: &Path,
    address: &str,
    image_bytes: &[u8],
    content_type: Option<&str>,
) -> bool {
    let norm = normalize_phone(address);
    if norm.is_empty() || image_bytes.is_empty() {
        return false;
    }
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let target = dir.join(format!("{norm}{}", photo_suffix(content_type)));
    if target.is_file()
        && let Ok(existing) = fs::read(&target)
        && existing == image_bytes
    {
        return false;
    }
    for existing in photos_with_prefix(dir, &norm) {
        if existing != target {
            let _ = fs::remove_file(existing);
        }
    }
    fs::write(&target, image_bytes).is_ok()
}

pub fn store_contact_photo(address: &str, image_bytes: &[u8], content_type: Option<&str>) -> bool {
    store_contact_photo_in(&contact_photos_dir(&home_dir()), address, image_bytes, content_type)
}

pub fn delete_contact_photo_in(dir: &Path, address: &str) -> bool {
    let norm = normalize_phone(address);
    if norm.is_empty() || !dir.is_dir() {
        return false;
    }
    let mut removed = false;
    for existing in photos_with_prefix(dir, &norm) {
        if fs::remove_file(existing).is_ok() {
            removed = true;
        }
    }
    removed
}

pub fn delete_contact_photo(address: &str) -> bool {
    delete_contact_photo_in(&contact_photos_dir(&home_dir()), address)
}

/// `Path.stem` semantics.
fn file_stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => &name[..i],
        _ => name,
    }
}

pub fn contact_photo_path_in(dir: &Path, address: &str) -> Option<String> {
    let norm = normalize_phone(address);
    if norm.is_empty() || !dir.is_dir() {
        return None;
    }
    for existing in photos_with_prefix(dir, &norm) {
        if existing.is_file() {
            return Some(existing.to_string_lossy().into_owned());
        }
    }
    let key = phone_key(&norm);
    if key.is_empty() {
        return None;
    }
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if phone_key(file_stem(&name)) == key {
                return Some(path.to_string_lossy().into_owned());
            }
        }
    }
    None
}

pub fn contact_photo_path(address: &str) -> Option<String> {
    contact_photo_path_in(&contact_photos_dir(&home_dir()), address)
}

// ── Merging / importing into a contact map ─────────────────────────

/// Merge a batch of contact mappings into `contacts`; returns how many
/// changed. Persist `contacts` (replace_contacts) when the result is > 0.
pub fn merge_contacts(contacts: &mut HashMap<String, String>, contact_names: &HashMap<String, String>) -> usize {
    let mut changed = 0;
    for (address, name) in contact_names {
        let norm = normalize_phone(address);
        let cleaned = clean_text(name);
        if norm.is_empty() || cleaned.is_empty() {
            continue;
        }
        if contacts.get(&norm) == Some(&cleaned) {
            continue;
        }
        contacts.insert(norm, cleaned);
        changed += 1;
    }
    changed
}

/// Minimal CSV reader: quoted fields, `""` escapes, embedded newlines, CRLF.
/// Blank lines yield no record (like `csv.reader` + `DictReader`).
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut started = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => {
                in_quotes = true;
                started = true;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                started = true;
            }
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                if started {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                started = false;
            }
            _ => {
                field.push(c);
                started = true;
            }
        }
    }
    if started {
        row.push(field);
        rows.push(row);
    }
    rows
}

fn csv_field<'a>(row: &[(&'a str, &'a str)], key: &str) -> &'a str {
    row.iter()
        .find(|(h, _)| *h == key)
        .map(|(_, v)| v.trim())
        .unwrap_or("")
}

/// Import contacts from the text of a Google Contacts CSV export into
/// `contacts`. Returns the number imported.
pub fn import_google_csv_text(text: &str, contacts: &mut HashMap<String, String>) -> usize {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rows = parse_csv(text);
    let Some((header, data)) = rows.split_first() else { return 0 };
    let mut count = 0;
    for record in data {
        // DictReader: later duplicate headers overwrite the value in place.
        let mut row: Vec<(&str, &str)> = Vec::new();
        for (h, v) in header.iter().zip(record.iter()) {
            if let Some(i) = row.iter().position(|(k, _)| *k == h.as_str()) {
                row[i].1 = v.as_str();
            } else {
                row.push((h.as_str(), v.as_str()));
            }
        }
        let mut name = csv_field(&row, "Name").to_string();
        if name.is_empty() {
            name = format!("{} {}", csv_field(&row, "First Name"), csv_field(&row, "Last Name")).trim().to_string();
        }
        if name.is_empty() {
            continue;
        }
        for (key, val) in &row {
            let k = key.to_lowercase();
            if k.contains("phone") && k.contains("value") {
                let norm = normalize_phone(val);
                if norm.chars().count() >= 7 {
                    contacts.insert(norm, name.clone());
                    count += 1;
                }
            }
        }
    }
    count
}

/// Import a Google Contacts CSV file. Returns the number imported (0 and a
/// log line on read errors). Persist `contacts` when the result is > 0.
pub fn import_google_csv(csv_path: &Path, contacts: &mut HashMap<String, String>) -> usize {
    match fs::read(csv_path) {
        Ok(bytes) => import_google_csv_text(&String::from_utf8_lossy(&bytes), contacts),
        Err(e) => {
            log::warn!("CSV import error: {e}");
            0
        }
    }
}

// ── vCard parsing (KDE Connect sync + VCF import) ──────────────────

pub type VcardParams = HashMap<String, Vec<String>>;

fn unfold_vcard_lines(text: &str) -> Vec<String> {
    let norm = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut unfolded: Vec<String> = Vec::new();
    for line in norm.split('\n') {
        if (line.starts_with(' ') || line.starts_with('\t')) && !unfolded.is_empty() {
            unfolded.last_mut().expect("non-empty").push_str(&line[1..]);
        } else {
            unfolded.push(line.to_string());
        }
    }
    unfolded
}

fn parse_vcard_property(header: &str) -> (String, VcardParams) {
    let mut parts = header.split(';');
    let prop_name = parts.next().unwrap_or("").trim().to_uppercase();
    let mut params = VcardParams::new();
    for raw in parts {
        if raw.is_empty() {
            continue;
        }
        match raw.split_once('=') {
            None => params.entry("TYPE".into()).or_default().push(raw.trim().to_string()),
            Some((key, value)) => {
                let items = value
                    .split(',')
                    .filter(|i| !i.trim().is_empty())
                    .map(|i| i.trim().trim_matches('"').to_string())
                    .collect();
                params.insert(key.trim().to_uppercase(), items);
            }
        }
    }
    (prop_name, params)
}

fn vcard_param_contains(params: &VcardParams, key: &str, value: &str) -> bool {
    let expected = value.to_lowercase();
    params
        .get(&key.to_uppercase())
        .is_some_and(|items| items.iter().any(|i| i.to_lowercase() == expected))
}

/// Python `quopri.decodestring` (`binascii.a2b_qp`, header=False).
fn qp_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'=' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let hi = b.get(i + 1).and_then(|c| (*c as char).to_digit(16));
        let lo = b.get(i + 2).and_then(|c| (*c as char).to_digit(16));
        if let (Some(h), Some(l)) = (hi, lo) {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else if b.get(i + 1) == Some(&b'\n') {
            i += 2; // soft line break
        } else if b[i + 1..].starts_with(b"\r\n") {
            i += 3;
        } else if i + 1 == b.len() {
            i += 1; // trailing '=' is a soft break at end of input
        } else {
            out.push(b'=');
            i += 1;
        }
    }
    out
}

fn decode_charset(bytes: &[u8], charset: &str) -> String {
    match charset.to_lowercase().replace('_', "-").as_str() {
        "ascii" | "us-ascii" => bytes
            .iter()
            .map(|&b| if b < 128 { b as char } else { '\u{FFFD}' })
            .collect(),
        "iso-8859-1" | "latin-1" | "latin1" | "l1" | "windows-1252" | "cp1252" => {
            bytes.iter().map(|&b| b as char).collect()
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn decode_vcard_value(value: &str, params: &VcardParams) -> String {
    let mut value = value.to_string();
    if vcard_param_contains(params, "ENCODING", "QUOTED-PRINTABLE") {
        let charset = params
            .get("CHARSET")
            .and_then(|v| v.first())
            .filter(|c| !c.is_empty())
            .map(String::as_str)
            .unwrap_or("utf-8");
        value = decode_charset(&qp_decode(&value), charset);
    }
    value
        .replace("\\n", "\n")
        .replace("\\N", "\n")
        .replace("\\,", ",")
        .replace("\\;", ";")
        .replace("\\\\", "\\")
        .trim()
        .to_string()
}

fn name_from_structured_n(value: &str) -> String {
    let mut parts: Vec<&str> = value.split(';').collect();
    parts.extend(["", "", "", "", ""]);
    parts.truncate(5);
    let (family, given, additional, prefix, suffix) =
        (parts[0], parts[1], parts[2], parts[3], parts[4]);
    [prefix, given, additional, family, suffix]
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse a VCF string that may hold several vCards. Returns
/// `(display_name, [normalized_phones])` per card that has both.
pub fn parse_vcf_text(text: &str) -> Vec<(String, Vec<String>)> {
    let mut results = Vec::new();
    let mut name = String::new();
    let mut structured_name = String::new();
    let mut phones: Vec<String> = Vec::new();
    let mut in_card = false;

    for line in unfold_vcard_lines(text) {
        if line.is_empty() {
            continue;
        }
        let upper = line.to_uppercase();
        let upper = upper.trim();
        if upper == "BEGIN:VCARD" {
            name.clear();
            structured_name.clear();
            phones.clear();
            in_card = true;
            continue;
        }
        if upper == "END:VCARD" {
            let display = if name.is_empty() {
                name_from_structured_n(&structured_name)
            } else {
                name.clone()
            };
            if !display.is_empty() && !phones.is_empty() {
                results.push((display, std::mem::take(&mut phones)));
            }
            name.clear();
            structured_name.clear();
            phones.clear();
            in_card = false;
            continue;
        }
        if !in_card {
            continue;
        }
        let Some((header, raw_value)) = line.split_once(':') else { continue };
        let (prop_name, params) = parse_vcard_property(header);
        let value = decode_vcard_value(raw_value, &params);
        match prop_name.as_str() {
            "FN" => name = value,
            "N" if structured_name.is_empty() => structured_name = value,
            "TEL" => {
                let norm = normalize_phone(&value);
                if !norm.is_empty() {
                    phones.push(norm);
                }
            }
            _ => {}
        }
    }
    results
}

/// Display name and phone numbers of the first vCard in a file
/// (`("", [])` when unreadable or empty).
pub fn parse_vcard_name(path: &Path) -> (String, Vec<String>) {
    let Ok(bytes) = fs::read(path) else { return (String::new(), Vec::new()) };
    parse_vcf_text(&String::from_utf8_lossy(&bytes))
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// Import contacts from a `.vcf` file into `contacts`. Returns the number
/// added or changed. Persist `contacts` when the result is > 0.
pub fn import_vcf_file(vcf_path: &Path, contacts: &mut HashMap<String, String>) -> usize {
    let bytes = match fs::read(vcf_path) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("VCF read error: {e}");
            return 0;
        }
    };
    let mut count = 0;
    for (name, phones) in parse_vcf_text(&String::from_utf8_lossy(&bytes)) {
        for phone in phones {
            if contacts.get(&phone) != Some(&name) {
                contacts.insert(phone, name.clone());
                count += 1;
            }
        }
    }
    count
}

fn vcf_files(vcard_dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(vcard_dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("vcf"))
        })
        .collect();
    files.sort();
    files
}

/// Normalised phone -> display name: KDE Connect vCards under `vcard_base`
/// (lower priority) overridden by `local` contacts.
pub fn load_contact_map_in(
    vcard_base: &Path,
    device_id: &str,
    local: HashMap<String, String>,
) -> HashMap<String, String> {
    let mut contact_map = HashMap::new();
    let vcard_dir = vcard_base.join(format!("kdeconnect-{device_id}"));
    if vcard_dir.is_dir() {
        for entry in vcf_files(&vcard_dir) {
            let (name, phones) = parse_vcard_name(&entry);
            if !name.is_empty() {
                for phone in phones {
                    contact_map.insert(phone, name.clone());
                }
            }
        }
    }
    contact_map.extend(local);
    contact_map
}

/// `load_contact_map` at the default vCard location; `local` is
/// `MessageStore::load_contacts()`.
pub fn load_contact_map(device_id: &str, local: HashMap<String, String>) -> HashMap<String, String> {
    load_contact_map_in(&vcard_base(&home_dir()), device_id, local)
}

pub fn synced_vcard_count_in(vcard_base: &Path, device_id: &str) -> usize {
    let vcard_dir = vcard_base.join(format!("kdeconnect-{device_id}"));
    if !vcard_dir.is_dir() {
        return 0;
    }
    vcf_files(&vcard_dir).len()
}

/// Number of KDE Connect vCards currently cached for a device.
pub fn synced_vcard_count(device_id: &str) -> usize {
    synced_vcard_count_in(&vcard_base(&home_dir()), device_id)
}

/// Display name for a phone number, or the address itself when unknown.
/// The last-10-digits fallback scans in hash order (Python: insertion order).
pub fn resolve_name(contact_map: &HashMap<String, String>, address: &str) -> String {
    if address.is_empty() {
        return address.to_string();
    }
    let norm = normalize_phone(address);
    if let Some(name) = contact_map.get(&norm) {
        return name.clone();
    }
    if norm.chars().count() >= 10 {
        let short = last_chars(&norm, 10);
        for (key, name) in contact_map {
            if key.ends_with(short) {
                return name.clone();
            }
        }
    }
    address.to_string()
}

// ── Notification-based contact harvesting ──────────────────────────

/// Insertion-ordered map of message body -> conversation address (Python dict
/// semantics: re-inserting a key keeps its original position). Order matters
/// for the "first prefix match wins" lookup.
#[derive(Debug, Default, Clone)]
pub struct BodyToAddr {
    entries: Vec<(String, String)>,
    index: HashMap<String, usize>,
}

impl BodyToAddr {
    pub fn insert(&mut self, body: String, addr: String) {
        match self.index.get(&body) {
            Some(&i) => self.entries[i].1 = addr,
            None => {
                self.index.insert(body.clone(), self.entries.len());
                self.entries.push((body, addr));
            }
        }
    }

    pub fn get(&self, body: &str) -> Option<&str> {
        self.index.get(body).map(|&i| self.entries[i].1.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(b, a)| (b.as_str(), a.as_str()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Reverse lookup of received-message body -> conversation address, built
/// from in-memory conversations (in their iteration order).
pub fn build_body_to_address_map<'a>(
    conversations: impl IntoIterator<Item = &'a Conversation>,
) -> BodyToAddr {
    let mut map = BodyToAddr::default();
    for conv in conversations {
        for msg in &conv.messages {
            if msg.msg_type == 1 && !msg.body.is_empty() {
                map.insert(msg.body.trim().to_string(), conv.address.clone());
            }
        }
        if !conv.last_message.is_empty() {
            map.insert(conv.last_message.trim().to_string(), conv.address.clone());
        }
    }
    map
}

/// Shared filter: `(title, message_text)` of a notification worth matching.
fn messaging_title_and_text(props: &Props) -> Option<(String, String)> {
    let app = prop_text(props, "appName");
    let title = prop_text(props, "title");
    if !is_messaging_app(&app) || title.is_empty() {
        return None;
    }
    if title == "(No title)" || looks_like_phone_number(&title) {
        return None;
    }
    let msg_text = notification_message_text(props);
    if msg_text.is_empty() {
        return None;
    }
    Some((title, msg_text))
}

/// Learn phone -> contact name from already-fetched active notifications.
///
/// Signature change: Python's `(client, device_id, body_to_addr)` fetched the
/// notifications over D-Bus and saved the result. Here the caller passes the
/// fetched `notifications` (skipping any whose fetch failed) and the local
/// contact map `existing` (updated in place); persist it when the return value
/// (new/changed contacts) is > 0.
pub fn harvest_contacts_from_notifications(
    notifications: &[Props],
    body_to_addr: &BodyToAddr,
    existing: &mut HashMap<String, String>,
) -> usize {
    let mut new_count = 0;
    for props in notifications {
        let Some((title, msg_text)) = messaging_title_and_text(props) else { continue };

        let mut matched = body_to_addr.get(&msg_text).filter(|a| !a.is_empty());
        if matched.is_none() {
            matched = body_to_addr
                .iter()
                .find(|(body, _)| body.starts_with(&msg_text) || msg_text.starts_with(body))
                .map(|(_, addr)| addr)
                .filter(|a| !a.is_empty());
        }
        if let Some(addr) = matched {
            let norm = normalize_phone(addr);
            if !norm.is_empty() && existing.get(&norm) != Some(&title) {
                existing.insert(norm, title);
                new_count += 1;
            }
        }
    }
    new_count
}

/// Match one notification's already-fetched props against in-memory
/// conversations. Returns `(normalized_phone, contact_name)` for a new
/// mapping; the caller persists it (`MessageStore::save_contact`).
///
/// Covers both Python's `match_contact_from_notification_props` and
/// `harvest_contact_from_notification_signal` (identical apart from fetching
/// the props over D-Bus and calling `save_contact`).
pub fn match_contact_from_notification_props<'a>(
    props: &Props,
    conversations: impl IntoIterator<Item = &'a Conversation>,
    contact_map: &HashMap<String, String>,
) -> Option<(String, String)> {
    let (title, msg_text) = messaging_title_and_text(props)?;
    let is_new = |norm: &str| !norm.is_empty() && contact_map.get(norm) != Some(&title);

    for conv in conversations {
        let last = conv.last_message.trim();
        if !conv.last_message.is_empty()
            && (last == msg_text || msg_text.starts_with(first_chars(last, 30)))
        {
            let norm = normalize_phone(&conv.address);
            if is_new(&norm) {
                return Some((norm, title.clone()));
            }
        }
        for msg in &conv.messages {
            if msg.msg_type == 1 && !msg.body.is_empty() && msg.body.trim() == msg_text {
                let norm = normalize_phone(&conv.address);
                if is_new(&norm) {
                    return Some((norm, title.clone()));
                }
            }
        }
    }
    None
}

/// Contact learned from a telephony event (incoming/missed call). Returns
/// `(normalized_phone, name)` for the caller to save; no store access here.
pub fn harvest_contact_from_telephony_signal(
    address: &str,
    contact_name: &str,
) -> Option<(String, String)> {
    let norm = normalize_phone(address);
    let name = clean_text(contact_name);
    if norm.is_empty() || name.is_empty() {
        return None;
    }
    if looks_like_phone_number(&name) {
        return None;
    }
    if matches!(name.to_lowercase().as_str(), "unknown number" | "unknown" | "private number") {
        return None;
    }
    Some((norm, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(name: &str, phones: &[&str]) -> Vec<(String, Vec<String>)> {
        vec![(name.to_string(), phones.iter().map(|s| s.to_string()).collect())]
    }

    #[test]
    fn folded_full_name_and_phone() {
        let got = parse_vcf_text(
            "BEGIN:VCARD\r\n\
             VERSION:3.0\r\n\
             FN:Jane\r\n\
             \x20 Doe\r\n\
             TEL;TYPE=CELL:+1 (316) 555-1212\r\n\
             END:VCARD\r\n",
        );
        assert_eq!(got, entries("Jane Doe", &["13165551212"]));
    }

    #[test]
    fn quoted_printable_structured_name_fallback() {
        let got = parse_vcf_text(
            "BEGIN:VCARD\n\
             VERSION:2.1\n\
             N;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE:Doe;Jos=C3=A9;;;\n\
             TEL:+44 20 7946 0958\n\
             END:VCARD\n",
        );
        assert_eq!(got, entries("José Doe", &["442079460958"]));
    }

    #[test]
    fn escaped_full_name() {
        let got = parse_vcf_text(
            "BEGIN:VCARD\n\
             FN:Doe\\, Jane\n\
             TEL;TYPE=HOME:(316) 555-0000\n\
             END:VCARD\n",
        );
        assert_eq!(got, entries("Doe, Jane", &["3165550000"]));
    }

    #[test]
    fn parse_vcard_name_reads_first_entry_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.vcf");
        fs::write(
            &path,
            "BEGIN:VCARD\n\
             FN:First Contact\n\
             TEL:+1 555 111 2222\n\
             END:VCARD\n\
             BEGIN:VCARD\n\
             FN:Second Contact\n\
             TEL:+1 555 333 4444\n\
             END:VCARD\n",
        )
        .unwrap();

        assert_eq!(
            parse_vcard_name(&path),
            ("First Contact".to_string(), vec!["15551112222".to_string()])
        );
    }
}
