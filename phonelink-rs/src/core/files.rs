//! The Files tab's logic (port of the pure parts of
//! `phonelink/ui/files_panel.py`): the Locations order, the recent-photo
//! scan, directory listings, sizes and icons, and saving copies with `_N`
//! names. Everything here touches only the local filesystem (the phone's
//! storage is an sftp mount), so the UI runs it off the main thread.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const THUMB_SIZE: i32 = 160;
pub const MAX_RECENT_PHOTOS: usize = 200;
/// `refresh_from_phone` runs at most this often.
pub const PHONE_REFRESH_MIN_INTERVAL_SECONDS: f64 = 30.0;
/// After `startBrowsing`, poll `isMounted` this many times, this far apart.
pub const BROWSE_POLLS: u32 = 10;
pub const BROWSE_POLL_MS: u64 = 500;

const IMAGE_EXTS: [&str; 8] = ["jpg", "jpeg", "png", "gif", "webp", "bmp", "heic", "heif"];

fn ext(name: &str) -> String {
    Path::new(name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

pub fn is_image(name: &str) -> bool {
    IMAGE_EXTS.contains(&ext(name).as_str())
}

/// "512 B", "1.5 KB", "3.2 GB".
pub fn human_size(size: u64) -> String {
    let mut s = size as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if s < 1024.0 {
            return if unit == "B" { format!("{s:.0} {unit}") } else { format!("{s:.1} {unit}") };
        }
        s /= 1024.0;
    }
    format!("{s:.1} TB")
}

pub fn file_icon_name(name: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "folder-symbolic";
    }
    match ext(name).as_str() {
        e if IMAGE_EXTS.contains(&e) => "image-x-generic-symbolic",
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "3gp" => "video-x-generic-symbolic",
        "mp3" | "flac" | "ogg" | "wav" | "aac" | "m4a" => "audio-x-generic-symbolic",
        "pdf" | "doc" | "docx" | "txt" | "odt" | "rtf" => "x-office-document-symbolic",
        "apk" => "application-x-executable-symbolic",
        "zip" | "tar" | "gz" | "7z" | "rar" => "package-x-generic-symbolic",
        _ => "text-x-generic-symbolic",
    }
}

/// `_directory_sort_key`: all files / `/storage/emulated/0` first, then
/// internal storage, then camera / DCIM, then the rest, each by label.
fn directory_rank(path: &str, label: &str) -> (u8, String) {
    let (l, p) = (label.to_lowercase(), path.to_lowercase());
    let rank = if p.ends_with("/storage/emulated/0") || l.contains("all files") {
        0
    } else if l.contains("internal") || l.contains("storage") {
        1
    } else if l.contains("camera") || p.contains("dcim") {
        2
    } else {
        3
    };
    (rank, l)
}

/// The phone's directories (`getDirectories`: path → label) in sidebar order.
pub fn sorted_directories(dirs: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = dirs.iter().map(|(p, l)| (p.clone(), l.clone())).collect();
    v.sort_by_cached_key(|(p, l)| directory_rank(p, l));
    v
}

/// Where the phone's files really start (e.g. `<mount>/storage/emulated/0`):
/// the first directory in sidebar order, else the mount point.
pub fn storage_base(dirs: &BTreeMap<String, String>, mount_point: &str) -> String {
    if mount_point.is_empty() {
        return String::new();
    }
    sorted_directories(dirs).into_iter().next().map_or_else(|| mount_point.to_string(), |(p, _)| p)
}

/// Sidebar icon for a phone directory.
pub fn directory_icon_name(label: &str) -> &'static str {
    let l = label.to_lowercase();
    if l.contains("camera") {
        "camera-photo-symbolic"
    } else if l.contains("all") {
        "phone-symbolic"
    } else {
        "folder-symbolic"
    }
}

/// The newest pictures under DCIM/Camera, DCIM, Pictures, Download and
/// Movies (plus one level of subfolders), dotfiles skipped, newest first,
/// at most [`MAX_RECENT_PHOTOS`].
pub fn recent_photos(base: &Path) -> Vec<PathBuf> {
    let roots = [base.join("DCIM").join("Camera"), base.join("DCIM"), base.join("Pictures"), base.join("Download"), base.join("Movies")];
    let mut found: Vec<(PathBuf, SystemTime)> = Vec::new();
    let mut seen = HashSet::new();
    for dir in &roots {
        if !dir.is_dir() {
            continue;
        }
        collect_images(dir, &mut found, &mut seen);
        let top = dir.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        if !["dcim", "pictures", "download", "movies"].contains(&top.as_str()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') && e.path().is_dir() {
                collect_images(&e.path(), &mut found, &mut seen);
            }
        }
    }
    found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    found.truncate(MAX_RECENT_PHOTOS);
    found.into_iter().map(|(p, _)| p).collect()
}

fn collect_images(dir: &Path, out: &mut Vec<(PathBuf, SystemTime)>, seen: &mut HashSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !is_image(&name) {
            continue;
        }
        let path = e.path();
        if !seen.insert(path.clone()) {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_file() {
            out.push((path, meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
        }
    }
}

/// A rescan found `found` while the grid shows `shown`. Rebuild only when
/// the set changed, and never for an empty scan (a flaky mount, not a
/// wiped phone).
pub fn rescan_changes_grid(shown: &[PathBuf], found: &[PathBuf]) -> bool {
    !found.is_empty() && found != shown
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Default, PartialEq)]
pub struct Listing {
    pub dirs: Vec<Entry>,
    pub files: Vec<Entry>,
}

impl Listing {
    pub fn summary(&self) -> String {
        if self.dirs.is_empty() && self.files.is_empty() {
            "Empty folder".into()
        } else {
            format!("{} folders, {} files", self.dirs.len(), self.files.len())
        }
    }
}

/// A directory's entries, folders first, dotfiles hidden, each sorted by
/// name. Errors read as the Python did.
pub fn list_directory(path: &Path) -> Result<Listing, String> {
    if !path.is_dir() {
        return Err(format!("Cannot access: {}", path.display()));
    }
    let entries = std::fs::read_dir(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
        _ => format!("Error: {e}"),
    })?;
    let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    let mut out = Listing::default();
    for name in names {
        if name.starts_with('.') {
            continue;
        }
        let full = path.join(&name);
        let Ok(meta) = std::fs::metadata(&full) else { continue };
        let is_dir = meta.is_dir();
        let entry = Entry { name, path: full, is_dir, size: if is_dir { 0 } else { meta.len() } };
        if is_dir { out.dirs.push(entry) } else { out.files.push(entry) }
    }
    Ok(out)
}

/// The path label: "Phone /DCIM" under the mount, else the path itself.
pub fn display_path(path: &str, mount_point: &str) -> String {
    if !mount_point.is_empty()
        && let Some(rest) = path.strip_prefix(mount_point)
    {
        return if rest.is_empty() { "Phone /".into() } else { format!("Phone {rest}") };
    }
    path.to_string()
}

/// "Go up" stops at the mount root and at the local Downloads folder.
pub fn parent_within(path: &str, mount_point: &str, downloads: &str) -> Option<String> {
    if path.is_empty() || path == mount_point || path == downloads {
        return None;
    }
    let parent = Path::new(path).parent()?.to_string_lossy().into_owned();
    if !mount_point.is_empty() && path.starts_with(mount_point) && parent.len() < mount_point.len() {
        return None;
    }
    Some(parent)
}

/// `dest_dir/name`, or `name_1`, `name_2`, … when taken.
pub fn unique_destination(dest_dir: &Path, name: &str) -> PathBuf {
    let first = dest_dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..).map(|n| dest_dir.join(format!("{stem}_{n}{ext}"))).find(|d| !d.exists()).unwrap()
}

/// Copy each file into `dest_dir` (never overwriting). Stops at the first
/// error: `(saved, Some("Error saving x: …"))`.
pub fn save_copies(paths: &[PathBuf], dest_dir: &Path) -> (usize, Option<String>) {
    let mut saved = 0;
    for src in paths {
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dest = unique_destination(dest_dir, &name);
        if let Err(e) = std::fs::copy(src, &dest) {
            return (saved, Some(format!("Error saving {name}: {e}")));
        }
        saved += 1;
    }
    (saved, None)
}

pub fn saved_label(saved: usize, dest_dir: &Path) -> String {
    format!("Saved {saved} file{} to {}", if saved == 1 { "" } else { "s" }, dest_dir.display())
}

pub fn copied_label(paths: &[PathBuf]) -> String {
    match paths {
        [one] => format!("Copied {} to clipboard", one.file_name().unwrap_or_default().to_string_lossy()),
        _ => format!("Copied {} files to clipboard", paths.len()),
    }
}

pub fn selected_label(count: usize) -> String {
    format!("{count} photo{} selected", if count == 1 { "" } else { "s" })
}

/// `text/uri-list` for the clipboard: one `file://` URI per line, CRLF.
pub fn uri_list(uris: &[String]) -> String {
    uris.iter().map(|u| format!("{u}\r\n")).collect()
}

/// Rate limit for pulls from the phone (reconnect, resume, window show).
#[derive(Debug, Default)]
pub struct RefreshLimit {
    last: Option<f64>,
}

impl RefreshLimit {
    pub fn allow(&mut self, now: f64) -> bool {
        if self.last.is_some_and(|t| now - t < PHONE_REFRESH_MIN_INTERVAL_SECONDS) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path, age_s: u64) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
        let t = SystemTime::now() - std::time::Duration::from_secs(age_s);
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn sizes_and_icons() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(file_icon_name("a.JPG", false), "image-x-generic-symbolic");
        assert_eq!(file_icon_name("a.apk", false), "application-x-executable-symbolic");
        assert_eq!(file_icon_name("x", true), "folder-symbolic");
        assert_eq!(file_icon_name("notes", false), "text-x-generic-symbolic");
        assert!(is_image("p.heic") && !is_image("p.mp4"));
    }

    #[test]
    fn directories_sort_like_python() {
        let dirs: BTreeMap<String, String> = [
            ("/m/storage/emulated/0/DCIM/Camera".into(), "Camera pictures".into()),
            ("/m/sdcard/Music".into(), "Music".into()),
            ("/m/storage/emulated/0".into(), "All files".into()),
            ("/m/storage/1234".into(), "SD card storage".into()),
        ]
        .into();
        let order: Vec<_> = sorted_directories(&dirs).into_iter().map(|(_, l)| l).collect();
        assert_eq!(order, ["All files", "SD card storage", "Camera pictures", "Music"]);
        assert_eq!(storage_base(&dirs, "/m"), "/m/storage/emulated/0");
        assert_eq!(storage_base(&BTreeMap::new(), "/m"), "/m");
        assert_eq!(storage_base(&dirs, ""), "");
        assert_eq!(directory_icon_name("All files"), "phone-symbolic");
    }

    #[test]
    fn recent_photos_newest_first_with_one_level_of_subfolders() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        touch(&b.join("DCIM/Camera/old.jpg"), 300);
        touch(&b.join("DCIM/Camera/new.jpg"), 10);
        touch(&b.join("DCIM/Screenshots/shot.png"), 20);
        touch(&b.join("DCIM/Screenshots/deeper/no.png"), 1);
        touch(&b.join("Pictures/.hidden.jpg"), 1);
        touch(&b.join("Pictures/.thumbs/t.jpg"), 1);
        touch(&b.join("Download/doc.pdf"), 1);
        touch(&b.join("Movies/clip.webp"), 100);
        let got: Vec<_> = recent_photos(b).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(got, ["new.jpg", "shot.png", "clip.webp", "old.jpg"]);
    }

    #[test]
    fn listing_folders_first_without_dotfiles() {
        let t = tempfile::tempdir().unwrap();
        touch(&t.path().join("b.txt"), 0);
        touch(&t.path().join(".secret"), 0);
        touch(&t.path().join("a/x"), 0);
        let l = list_directory(t.path()).unwrap();
        assert_eq!(l.dirs.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a"]);
        assert_eq!(l.files.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["b.txt"]);
        assert_eq!(l.files[0].size, 1);
        assert_eq!(l.summary(), "1 folders, 1 files");
        assert!(list_directory(&t.path().join("missing")).unwrap_err().starts_with("Cannot access"));
        assert_eq!(list_directory(&t.path().join("a")).unwrap().files.len(), 1);
    }

    #[test]
    fn paths_and_going_up() {
        assert_eq!(display_path("/run/m", "/run/m"), "Phone /");
        assert_eq!(display_path("/run/m/DCIM", "/run/m"), "Phone /DCIM");
        assert_eq!(display_path("/home/u/Downloads", "/run/m"), "/home/u/Downloads");
        assert_eq!(parent_within("/run/m/DCIM", "/run/m", "/d").as_deref(), Some("/run/m"));
        assert_eq!(parent_within("/run/m", "/run/m", "/d"), None);
        assert_eq!(parent_within("/d", "/run/m", "/d"), None);
        assert_eq!(parent_within("/d/sub", "/run/m", "/d").as_deref(), Some("/d"));
    }

    #[test]
    fn saves_never_overwrite() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        touch(&src.path().join("a.jpg"), 0);
        touch(&dst.path().join("a.jpg"), 0);
        touch(&dst.path().join("a_1.jpg"), 0);
        let (n, err) = save_copies(&[src.path().join("a.jpg"), src.path().join("gone.jpg")], dst.path());
        assert_eq!(n, 1);
        assert!(err.unwrap().starts_with("Error saving gone.jpg"));
        assert!(dst.path().join("a_2.jpg").exists());
        assert_eq!(unique_destination(dst.path(), "noext"), dst.path().join("noext"));
    }

    #[test]
    fn rescans_and_labels() {
        let a = vec![PathBuf::from("/a.jpg")];
        assert!(!rescan_changes_grid(&a, &[]));
        assert!(!rescan_changes_grid(&a, &a));
        assert!(rescan_changes_grid(&a, &[PathBuf::from("/b.jpg")]));
        assert_eq!(copied_label(&a), "Copied a.jpg to clipboard");
        assert_eq!(selected_label(1), "1 photo selected");
        assert_eq!(uri_list(&["file:///a".into(), "file:///b".into()]), "file:///a\r\nfile:///b\r\n");
        let mut r = RefreshLimit::default();
        assert!(r.allow(0.0) && !r.allow(29.0) && r.allow(31.0));
    }
}
