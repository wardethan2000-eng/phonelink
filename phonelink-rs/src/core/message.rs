//! KDE Connect message tuple → [`SmsMessage`] (port of the parsing helpers in
//! `phonelink/ui/sms_panel.py`).
//!
//! On the wire a message is `(event, body, addresses, date, type, read,
//! threadID, uID, subID, attachments)`. The D-Bus layer unpacks the variant
//! into [`RawMessage`] so the rules here stay testable without GLib.

use super::models::{Attachment, SmsMessage};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawAttachment {
    pub part_id: i64,
    pub mime_type: String,
    pub payload: String,
    pub unique_identifier: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawMessage {
    pub event: i32,
    pub body: String,
    /// Addresses in wire order (each `a(s)` entry's first string).
    pub addresses: Vec<String>,
    pub date: i64,
    pub msg_type: i32,
    pub read: i32,
    pub thread_id: i64,
    pub uid: i64,
    pub sub_id: i64,
    pub attachments: Vec<RawAttachment>,
}

/// Digits only, last 10 of them — the key used to drop duplicate addresses
/// (KDE Connect often sends the same number twice for MMS-style threads).
fn dedupe_key(address: &str) -> String {
    let digits: String = address.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() >= 10 {
        digits[digits.len() - 10..].to_string()
    } else {
        digits
    }
}

pub fn parse_message(raw: &RawMessage) -> SmsMessage {
    let mut seen = std::collections::HashSet::new();
    let mut all_addresses = Vec::new();
    for a in &raw.addresses {
        let key = dedupe_key(a);
        if !key.is_empty() && !seen.insert(key) {
            continue;
        }
        all_addresses.push(a.clone());
    }
    SmsMessage {
        uid: raw.uid,
        body: raw.body.clone(),
        address: all_addresses.first().cloned().unwrap_or_default(),
        date: raw.date,
        msg_type: raw.msg_type,
        read: raw.read,
        thread_id: raw.thread_id,
        attachments: raw.attachments.iter().map(parse_attachment).collect(),
        all_addresses,
    }
}

pub fn parse_attachment(raw: &RawAttachment) -> Attachment {
    Attachment {
        part_id: raw.part_id,
        mime_type: raw.mime_type.clone(),
        payload: raw.payload.clone(),
        unique_identifier: raw.unique_identifier.clone(),
        file_name: attachment_file_name(raw.part_id, &raw.mime_type, &raw.unique_identifier),
        extra: Default::default(),
    }
}

/// Extension for a MIME type, matching what Python's
/// `mimetypes.guess_extension` gives for the types MMS actually carries.
pub fn attachment_extension(mime_type: &str) -> &'static str {
    let base = mime_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match base.as_str() {
        "image/jpeg" | "image/jpg" | "image/pjpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/bmp" => ".bmp",
        "image/tiff" => ".tiff",
        "image/heic" => ".heic",
        "image/heif" => ".heif",
        "video/mp4" => ".mp4",
        "video/3gpp" => ".3gp",
        "video/quicktime" => ".mov",
        "video/webm" => ".webm",
        "audio/mpeg" => ".mp3",
        "audio/mp4" => ".m4a",
        "audio/amr" => ".amr",
        "audio/ogg" => ".ogg",
        "audio/wav" | "audio/x-wav" => ".wav",
        "text/plain" => ".txt",
        "text/vcard" | "text/x-vcard" => ".vcf",
        "application/pdf" => ".pdf",
        _ => "",
    }
}

pub fn attachment_file_name(part_id: i64, mime_type: &str, unique_identifier: &str) -> String {
    let candidate = unique_identifier.trim().rsplit('/').next().unwrap_or("");
    let ext = attachment_extension(mime_type);
    if !candidate.is_empty() && (candidate.contains('.') || ext.is_empty()) {
        return candidate.to_string();
    }
    let base = if candidate.is_empty() {
        format!("attachment-{part_id}")
    } else {
        candidate.to_string()
    };
    if !ext.is_empty() && !base.to_lowercase().ends_with(ext) {
        format!("{base}{ext}")
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_addresses_are_dropped_by_last_ten_digits() {
        let raw = RawMessage {
            addresses: vec!["+1 (316) 555-1221".into(), "3165551221".into(), "22550".into()],
            uid: 7,
            thread_id: 3,
            ..Default::default()
        };
        let m = parse_message(&raw);
        assert_eq!(m.all_addresses, vec!["+1 (316) 555-1221", "22550"]);
        assert_eq!(m.address, "+1 (316) 555-1221");
    }

    #[test]
    fn attachment_names() {
        assert_eq!(attachment_file_name(5, "image/jpeg", ""), "attachment-5.jpg");
        assert_eq!(attachment_file_name(5, "image/jpeg", "a/b/IMG_1.jpeg"), "IMG_1.jpeg");
        assert_eq!(attachment_file_name(5, "image/png", "part9"), "part9.png");
        assert_eq!(attachment_file_name(5, "x/unknown", "part9"), "part9");
        assert_eq!(attachment_file_name(5, "image/jpeg; name=x", ""), "attachment-5.jpg");
    }
}
