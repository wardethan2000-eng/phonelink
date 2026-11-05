//! Where attachment files live on disk (same paths as the Python app, so the
//! caches are shared).
//!
//! Inline images arrive as small thumbnails. The full-resolution file comes
//! from the phone on request (`requestAttachmentFile` → `attachmentReceived`)
//! and is kept in `~/.cache/phonelink/attachments_full/<uid><ext>`, recorded
//! on the attachment as `fullPath`.

use std::path::{Path, PathBuf};

use super::models::Attachment;
use super::settings::home_dir;

/// `$XDG_CACHE_HOME/phonelink/attachments_full`, else `~/.cache/…`.
pub fn full_cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home_dir().join(".cache"));
    base.join("phonelink").join("attachments_full")
}

/// Extension for the image types MMS carries (Python `_attachment_ext`).
pub fn image_ext(mime_type: &str) -> &'static str {
    match mime_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/heic" => ".heic",
        "image/heif" => ".heif",
        _ => "",
    }
}

/// Python `_sanitize_name`: alphanumerics and `-_.`; anything else is `_`.
pub fn sanitize_name(text: &str) -> String {
    let cleaned: String =
        text.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    let trimmed = cleaned.trim_matches(|c| c == '.' || c == '_');
    if trimmed.is_empty() { "attachment".into() } else { trimmed.to_string() }
}

pub fn is_image(att: &Attachment) -> bool {
    att.mime_type.starts_with("image")
}

/// The attachment's stable id: its unique identifier, else its part id.
pub fn attachment_key(att: &Attachment) -> String {
    let uid = att.unique_identifier.trim();
    if !uid.is_empty() {
        uid.to_string()
    } else if att.part_id != 0 {
        att.part_id.to_string()
    } else {
        String::new()
    }
}

/// Where the full file for this attachment goes in `dir`.
pub fn full_cache_path_in(dir: &Path, att: &Attachment) -> Option<PathBuf> {
    let key = attachment_key(att);
    if key.is_empty() {
        return None;
    }
    Some(dir.join(format!("{}{}", sanitize_name(&key), image_ext(&att.mime_type))))
}

/// The full-resolution file, if we already have it.
pub fn existing_full_in(dir: &Path, att: &Attachment) -> Option<PathBuf> {
    if let Some(p) = att.extra.get("fullPath").and_then(|v| v.as_str()).map(PathBuf::from)
        && p.is_file()
    {
        return Some(p);
    }
    full_cache_path_in(dir, att).filter(|p| p.is_file())
}

/// Does a file the phone sent (`attachmentReceived(path, name)`) belong to
/// this attachment? As Python: the name is its unique identifier or its file
/// name.
pub fn received_matches(att: &Attachment, file_name: &str) -> bool {
    !file_name.is_empty() && (att.unique_identifier == file_name || att.file_name == file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_match_python() {
        let att = Attachment {
            part_id: 9,
            mime_type: "image/jpeg".into(),
            unique_identifier: "PART 1/x".into(),
            ..Default::default()
        };
        let dir = Path::new("/c");
        assert_eq!(full_cache_path_in(dir, &att).unwrap(), Path::new("/c/PART_1_x.jpg"));
        let by_part = Attachment { part_id: 9, mime_type: "image/png".into(), ..Default::default() };
        assert_eq!(full_cache_path_in(dir, &by_part).unwrap(), Path::new("/c/9.png"));
        assert!(full_cache_path_in(dir, &Attachment::default()).is_none());
        assert_eq!(sanitize_name("..__"), "attachment");
    }
}
