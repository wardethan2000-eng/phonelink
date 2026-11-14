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
/// KDE Connect says "mounted" as soon as it starts sshfs, before sshfs has
/// connected: check the storage this many times, this far apart, before
/// giving up on a mount.
pub const MOUNT_READY_POLLS: u32 = 16;
pub const MOUNT_READY_POLL_MS: u64 = 500;
/// One look at the mount (a stalled sftp link can hang a read) takes at
/// most this long.
pub const MOUNT_CHECK_TIMEOUT_MS: u64 = 4000;

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
pub fn recent_photos(base: &Path) -> Vec<Photo> {
    photos_in(&photo_folders(base))
}

/// The folders [`recent_photos`] reads: the ones above that exist, and the
/// subfolders of DCIM, Pictures, Download and Movies.
pub fn photo_folders(base: &Path) -> Vec<PathBuf> {
    let roots = [base.join("DCIM").join("Camera"), base.join("DCIM"), base.join("Pictures"), base.join("Download"), base.join("Movies")];
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in roots {
        if !dir.is_dir() {
            continue;
        }
        let top = dir.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        let subfolders = ["dcim", "pictures", "download", "movies"].contains(&top.as_str());
        let entries = if subfolders { std::fs::read_dir(&dir).ok() } else { None };
        out.push(dir);
        for e in entries.into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') && e.path().is_dir() && !out.contains(&e.path()) {
                out.push(e.path());
            }
        }
    }
    out
}

/// The pictures directly in `folders`, newest first, at most
/// [`MAX_RECENT_PHOTOS`].
pub fn photos_in(folders: &[PathBuf]) -> Vec<Photo> {
    let mut found: Vec<Photo> = Vec::new();
    let mut seen = HashSet::new();
    for dir in folders {
        collect_images(dir, &mut found, &mut seen);
    }
    found.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.path.cmp(&b.path)));
    found.truncate(MAX_RECENT_PHOTOS);
    found
}

/// [`photo_folders`], read fresh from the phone rather than from sshfs's
/// cache (see [`flush_cached`]).
pub fn photo_folders_fresh(base: &Path) -> Vec<PathBuf> {
    let roots = ["DCIM/Camera", "DCIM", "Pictures", "Download", "Movies"].map(|r| base.join(r));
    for dir in roots.iter().filter(|d| d.is_dir()) {
        flush_cached(dir);
    }
    let folders = photo_folders(base);
    for dir in folders.iter().filter(|d| !roots.contains(d)) {
        flush_cached(dir);
    }
    folders
}

/// The name of the empty folder [`flush_cached`] makes and removes at once.
pub const FLUSH_DIR_NAME: &str = ".phonerelay-refresh";

/// Make sshfs read `dir` from the phone again on the next look. It keeps
/// listings and change times for about 20 s, and drops a folder's copies
/// whenever something is made or removed in it: so make an empty hidden
/// folder there and remove it. Off the main thread (it reaches the phone).
/// Nothing happens where the phone doesn't allow it, and a leftover from
/// an earlier try is removed.
pub fn flush_cached(dir: &Path) {
    let probe = dir.join(FLUSH_DIR_NAME);
    let _ = std::fs::create_dir(&probe);
    let _ = std::fs::remove_dir(&probe);
}

/// How often Files looks for new pictures or files while it is on screen.
/// Each look only reads the watched folders' change times.
pub const WATCH_INTERVAL_SECONDS: u32 = 5;
/// After a folder changed, rescan on every look for this long. sshfs keeps
/// listings and change times for up to 20 s each, separately, so the first
/// rescan after a change can still read the old listing.
pub const WATCH_SETTLE_SECONDS: f64 = 30.0;

/// Folders on the phone and when each last changed, to spot a new photo or
/// file without listing them all again.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FolderWatch {
    pub folders: Vec<PathBuf>,
    stamps: Vec<Option<i64>>,
    /// Rescan on every look until then (clock seconds).
    settle_until: Option<f64>,
}

impl FolderWatch {
    /// Watch `folders` as they are now. Take it before reading them: a
    /// change in between shows on the next look.
    pub fn take(folders: Vec<PathBuf>) -> Self {
        let stamps = folder_stamps(&folders);
        Self { folders, stamps, settle_until: None }
    }

    /// A look at `now` found `stamps` (from [`folder_stamps`] on
    /// `self.folders`): should Files rescan?
    pub fn changed(&mut self, stamps: Vec<Option<i64>>, now: f64) -> bool {
        if stamps != self.stamps {
            self.stamps = stamps;
            self.settle_until = Some(now + WATCH_SETTLE_SECONDS);
            return true;
        }
        match self.settle_until {
            Some(t) if now < t => true,
            Some(_) => {
                self.settle_until = None;
                false
            }
            None => false,
        }
    }

    /// A rescan replaced the watch: keep rescanning while the last change
    /// is still settling.
    pub fn carry_settling(&mut self, old: &FolderWatch) {
        self.settle_until = old.settle_until;
    }

    /// A rescan was dropped: keep rescanning on later looks.
    pub fn keep_settling(&mut self, now: f64) {
        self.settle_until = Some(now + WATCH_SETTLE_SECONDS);
    }
}

/// When each folder last changed (seconds since 1970), None when it can't be read.
pub fn folder_stamps(folders: &[PathBuf]) -> Vec<Option<i64>> {
    folders
        .iter()
        .map(|d| {
            let t = std::fs::metadata(d).ok()?.modified().ok()?;
            Some(t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0))
        })
        .collect()
}

/// One picture from [`recent_photos`].
#[derive(Debug, Clone, PartialEq)]
pub struct Photo {
    pub path: PathBuf,
    /// Last changed, in seconds since 1970.
    pub modified: i64,
    pub size: u64,
}

/// `photos` (newest first) in the order asked for.
pub fn ordered_photos(photos: &[Photo], oldest_first: bool) -> Vec<Photo> {
    let mut out = photos.to_vec();
    if oldest_first {
        out.reverse();
    }
    out
}

/// Runs of `photos` that share a `day` (a day key per time), in order, as
/// (day, indexes).
pub fn group_by_day<T: PartialEq>(photos: &[Photo], day: impl Fn(i64) -> T) -> Vec<(T, Vec<usize>)> {
    let mut out: Vec<(T, Vec<usize>)> = Vec::new();
    for (i, p) in photos.iter().enumerate() {
        let d = day(p.modified);
        match out.last_mut() {
            Some((last, ids)) if *last == d => ids.push(i),
            _ => out.push((d, vec![i])),
        }
    }
    out
}

/// A tile's place on screen: (x, y, width, height).
pub type Rect = (f64, f64, f64, f64);

/// The photo at point `p`, given the tiles' places in the order shown. Off a
/// tile, the last one before the point in reading order (the first when the
/// point is above them all). None without tiles.
pub fn photo_at(tiles: &[Rect], p: (f64, f64)) -> Option<usize> {
    if tiles.is_empty() {
        return None;
    }
    let (px, py) = p;
    if let Some(i) = tiles.iter().position(|&(x, y, w, h)| px >= x && px < x + w && py >= y && py < y + h) {
        return Some(i);
    }
    // Before the point: a row that ends above it, or the same row, left of it.
    let before = |&(x, y, _, h): &Rect| y + h <= py || (y <= py && x <= px);
    Some(tiles.iter().rposition(before).unwrap_or(0))
}

/// The paths of `photos`.
pub fn photo_paths(photos: &[Photo]) -> Vec<PathBuf> {
    photos.iter().map(|p| p.path.clone()).collect()
}

fn collect_images(dir: &Path, out: &mut Vec<Photo>, seen: &mut HashSet<PathBuf>) {
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
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let secs = modified.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            out.push(Photo { path, modified: secs, size: meta.len() });
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

#[derive(Debug, Default, Clone, PartialEq)]
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

/// What a right-click Copy / Save acts on: the whole selection when the
/// clicked item is part of it (as file managers do), else just that item.
pub fn menu_targets(clicked: &Path, selected: Vec<PathBuf>) -> Vec<PathBuf> {
    if selected.iter().any(|p| p == clicked) { selected } else { vec![clicked.to_path_buf()] }
}

pub fn copy_menu_label(count: usize) -> String {
    if count > 1 { format!("Copy {count} Files") } else { "Copy".into() }
}

pub fn save_menu_label(count: usize) -> String {
    if count > 1 { format!("Save {count} Files to Folder…") } else { "Save As…".into() }
}

/// Shift+click: the run of photos (in screen order) from the last one
/// clicked to this one, either way round. `None` when either is gone.
pub fn shift_range(order: &[PathBuf], anchor: &Path, clicked: &Path) -> Option<std::ops::RangeInclusive<usize>> {
    let a = order.iter().position(|p| p == anchor)?;
    let b = order.iter().position(|p| p == clicked)?;
    Some(a.min(b)..=a.max(b))
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

/// When the phone's storage drops, mount again after a growing wait, but
/// only a few times in a row: an sshfs that keeps failing (a dead mount
/// left by a crash, issue #12) otherwise mounts and drops forever, with a
/// KDE Connect error notification each time.
#[derive(Debug, Default)]
pub struct RemountGuard {
    drops: Vec<f64>,
}

/// Drops further apart than this don't add up.
pub const REMOUNT_WINDOW_SECONDS: f64 = 120.0;

impl RemountGuard {
    /// The mount dropped or failed at `now`: how long to wait before trying
    /// again, or `None` to stop and let the user retry.
    pub fn dropped(&mut self, now: f64) -> Option<f64> {
        self.drops.retain(|t| now - t < REMOUNT_WINDOW_SECONDS);
        self.drops.push(now);
        match self.drops.len() {
            1 => Some(1.0),
            2 => Some(4.0),
            3 => Some(10.0),
            _ => None,
        }
    }

    /// The user asked to try again, or another phone: start over.
    pub fn reset(&mut self) {
        self.drops.clear();
    }
}

/// What is at the mount point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountCheck {
    /// Mounted and readable.
    Ready,
    /// Nothing mounted there (yet).
    Absent,
    /// Mounted but dead: sshfs is gone and reads fail ("Transport endpoint
    /// is not connected"). Only a lazy unmount clears it.
    Stale,
}

/// `\040`-style escapes in `/proc/self/mountinfo` paths.
fn unescape_mountinfo(field: &str) -> String {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && let Ok(v) = u8::from_str_radix(&field[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `mountinfo` (the text of `/proc/self/mountinfo`) lists a mount
/// at `mount_point`.
pub fn mountinfo_lists(mountinfo: &str, mount_point: &str) -> bool {
    let want = mount_point.trim_end_matches('/');
    mountinfo.lines().filter_map(|l| l.split_whitespace().nth(4)).any(|m| unescape_mountinfo(m) == want)
}

/// Look at the mount point (blocking: run it off the main thread). Before
/// sshfs connects it is an empty folder; the phone's storage is a mount
/// with something in it.
pub fn check_mount(mount_point: &Path) -> MountCheck {
    match std::fs::read_dir(mount_point) {
        // ENOTCONN: the FUSE server (sshfs) is gone.
        Err(e) if e.raw_os_error() == Some(107) => MountCheck::Stale,
        Err(_) => MountCheck::Absent,
        Ok(mut entries) => {
            let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
            if mountinfo_lists(&info, &mount_point.to_string_lossy()) || entries.next().is_some() {
                MountCheck::Ready
            } else {
                MountCheck::Absent
            }
        }
    }
}

/// Clear a dead mount (blocking). KDE Connect runs `fusermount -u`, which
/// refuses a mount that is "busy"; the lazy `-z` always detaches it.
pub fn clear_stale_mount(mount_point: &Path) {
    for tool in ["fusermount3", "fusermount"] {
        let done = std::process::Command::new(tool)
            .args(["-u", "-z"])
            .arg(mount_point)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if done.is_ok_and(|s| s.success()) {
            log::info!("files: cleared a dead mount at {}", mount_point.display());
            return;
        }
    }
    log::warn!("files: could not clear the dead mount at {}", mount_point.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remounts_back_off_then_stop_until_asked() {
        let mut g = RemountGuard::default();
        assert_eq!(g.dropped(0.0), Some(1.0));
        assert_eq!(g.dropped(3.0), Some(4.0));
        assert_eq!(g.dropped(10.0), Some(10.0));
        // A fourth drop in a row: stop.
        assert_eq!(g.dropped(25.0), None);
        assert_eq!(g.dropped(26.0), None);
        g.reset();
        assert_eq!(g.dropped(30.0), Some(1.0));
        // A drop long after the last ones starts over.
        let mut g = RemountGuard::default();
        g.dropped(0.0);
        g.dropped(5.0);
        assert_eq!(g.dropped(5.0 + REMOUNT_WINDOW_SECONDS + 1.0), Some(1.0));
    }

    #[test]
    fn finds_a_mount_point_in_mountinfo() {
        let info = "22 1 0:21 / /proc rw,nosuid - proc proc rw\n\
                    98 29 0:52 / /run/user/1000/abc123 rw,nosuid,nodev - fuse.sshfs kdeconnect@192.168.1.5:/ rw\n\
                    99 29 0:53 / /mnt/my\\040disk rw - ext4 /dev/sdb1 rw\n";
        assert!(mountinfo_lists(info, "/run/user/1000/abc123"));
        assert!(mountinfo_lists(info, "/run/user/1000/abc123/"));
        assert!(!mountinfo_lists(info, "/run/user/1000/abc"));
        assert!(mountinfo_lists(info, "/mnt/my disk"));
        assert!(!mountinfo_lists("", "/run/user/1000/abc123"));
    }

    #[test]
    fn an_empty_folder_is_not_mounted_yet() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(check_mount(t.path()), MountCheck::Absent);
        assert_eq!(check_mount(&t.path().join("missing")), MountCheck::Absent);
        std::fs::create_dir(t.path().join("storage")).unwrap();
        assert_eq!(check_mount(t.path()), MountCheck::Ready);
    }

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
        let photos = recent_photos(b);
        let got: Vec<_> = photos.iter().map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(got, ["new.jpg", "shot.png", "clip.webp", "old.jpg"]);
        assert!(photos[0].modified > photos[3].modified);
    }

    #[test]
    fn photo_folders_are_the_ones_the_scan_reads() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        touch(&b.join("DCIM/Camera/a.jpg"), 1);
        touch(&b.join("DCIM/.thumbnails/t.jpg"), 1);
        touch(&b.join("Pictures/Screenshots/s.png"), 1);
        touch(&b.join("Pictures/Screenshots/deeper/no.png"), 1);
        let got: Vec<_> = photo_folders(b).iter().map(|p| p.strip_prefix(b).unwrap().to_path_buf()).collect();
        let want: Vec<PathBuf> =
            ["DCIM/Camera", "DCIM", "Pictures", "Pictures/Screenshots"].iter().map(PathBuf::from).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn flushing_leaves_nothing_behind() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        touch(&b.join("DCIM/Camera/a.jpg"), 1);
        touch(&b.join("Pictures/Screenshots/s.png"), 1);
        // A leftover from a try that never finished is cleaned up too.
        std::fs::create_dir(b.join("DCIM/Camera").join(FLUSH_DIR_NAME)).unwrap();
        assert_eq!(photo_folders_fresh(b), photo_folders(b));
        assert!(!b.join("DCIM/Camera").join(FLUSH_DIR_NAME).exists());
        assert!(!b.join("Pictures/Screenshots").join(FLUSH_DIR_NAME).exists());
        // A missing folder is left alone.
        flush_cached(&b.join("Movies"));
        assert!(!b.join("Movies").exists());
    }

    #[test]
    fn a_folder_change_rescans_until_it_settles() {
        let t = tempfile::tempdir().unwrap();
        let cam = t.path().join("DCIM/Camera");
        touch(&cam.join("a.jpg"), 0);
        let mut w = FolderWatch::take(vec![cam.clone()]);
        let same = folder_stamps(&w.folders);
        assert!(!w.changed(same.clone(), 0.0));
        // A new photo moves the folder's change time.
        let moved = vec![same[0].map(|s| s + 7)];
        assert!(w.changed(moved.clone(), 5.0));
        // sshfs may still list the old folder: keep looking for a while.
        assert!(w.changed(moved.clone(), 20.0));
        let mut fresh = FolderWatch::take(vec![cam]);
        fresh.carry_settling(&w);
        assert!(fresh.changed(fresh.stamps.clone(), 34.0));
        assert!(!fresh.changed(fresh.stamps.clone(), 36.0));
        assert!(!fresh.changed(fresh.stamps.clone(), 40.0));
        // A folder that disappears counts as a change.
        assert!(fresh.changed(vec![None], 45.0));
        // A dropped rescan tries again later.
        let mut quiet = FolderWatch::take(fresh.folders.clone());
        quiet.keep_settling(100.0);
        assert!(quiet.changed(quiet.stamps.clone(), 110.0));
    }

    #[test]
    fn photos_sort_and_group_by_day() {
        let p = |name: &str, modified: i64| Photo { path: PathBuf::from(name), modified, size: 0 };
        let photos = vec![p("a", 200), p("b", 150), p("c", 90), p("d", 10)];
        let names = |v: &[Photo]| v.iter().map(|p| p.path.to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(names(&ordered_photos(&photos, false)), ["a", "b", "c", "d"]);
        assert_eq!(names(&ordered_photos(&photos, true)), ["d", "c", "b", "a"]);
        // Days of 100 seconds.
        assert_eq!(group_by_day(&photos, |t| t / 100), vec![(2, vec![0]), (1, vec![1]), (0, vec![2, 3])]);
        assert!(group_by_day(&[], |t| t).is_empty());
    }

    #[test]
    fn photo_under_the_pointer() {
        // Two rows of three 10x10 tiles with 2 px gaps; a second section of one tile lower down.
        let mut t: Vec<Rect> = Vec::new();
        for row in 0..2 {
            for col in 0..3 {
                t.push((col as f64 * 12.0, row as f64 * 12.0, 10.0, 10.0));
            }
        }
        t.push((0.0, 40.0, 10.0, 10.0));
        assert_eq!(photo_at(&t, (5.0, 5.0)), Some(0));
        assert_eq!(photo_at(&t, (17.0, 17.0)), Some(4));
        assert_eq!(photo_at(&t, (11.0, 5.0)), Some(0), "the gap after a tile");
        assert_eq!(photo_at(&t, (100.0, 17.0)), Some(5), "right of a row");
        assert_eq!(photo_at(&t, (5.0, 30.0)), Some(5), "between sections");
        assert_eq!(photo_at(&t, (5.0, -3.0)), Some(0), "above everything");
        assert_eq!(photo_at(&t, (50.0, 90.0)), Some(6), "below everything");
        assert_eq!(photo_at(&[], (0.0, 0.0)), None);
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
        let two = vec![PathBuf::from("/a.jpg"), PathBuf::from("/b.jpg")];
        assert_eq!(copied_label(&two), "Copied 2 files to clipboard");
        assert_eq!(menu_targets(Path::new("/b.jpg"), two.clone()), two);
        assert_eq!(menu_targets(Path::new("/c.jpg"), two.clone()), vec![PathBuf::from("/c.jpg")]);
        assert_eq!(menu_targets(Path::new("/c.jpg"), vec![]), vec![PathBuf::from("/c.jpg")]);
        let row: Vec<PathBuf> = ["/1", "/2", "/3", "/4"].iter().map(PathBuf::from).collect();
        assert_eq!(shift_range(&row, Path::new("/2"), Path::new("/4")), Some(1..=3));
        assert_eq!(shift_range(&row, Path::new("/4"), Path::new("/1")), Some(0..=3));
        assert_eq!(shift_range(&row, Path::new("/3"), Path::new("/3")), Some(2..=2));
        assert_eq!(shift_range(&row, Path::new("/gone"), Path::new("/3")), None);
        assert_eq!((copy_menu_label(1).as_str(), copy_menu_label(2).as_str()), ("Copy", "Copy 2 Files"));
        assert_eq!((save_menu_label(1).as_str(), save_menu_label(3).as_str()), ("Save As…", "Save 3 Files to Folder…"));
        assert_eq!(uri_list(&["file:///a".into(), "file:///b".into()]), "file:///a\r\nfile:///b\r\n");
        let mut r = RefreshLimit::default();
        assert!(r.allow(0.0) && !r.allow(29.0) && r.allow(31.0));
    }
}
