//! A per-user scratch folder for pasted pictures and decoded MMS files.
//!
//! `$XDG_RUNTIME_DIR/phonelink` (else `~/.cache/phonelink/tmp`), mode 0700
//! and owned by us. Never the shared `/tmp`: there other users could read the
//! pictures, or create the folder first and swap files in it (issue #9).
//!
//! Only files under this folder are ever deleted by the app; a picture the
//! user pasted from their own folders is sent as is and left alone.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::settings::home_dir;

/// The scratch folder's path (not created).
pub fn base_dir() -> PathBuf {
    base_dir_from(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from), std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
}

fn base_dir_from(runtime: Option<PathBuf>, cache: Option<PathBuf>) -> PathBuf {
    if let Some(rt) = runtime.filter(|p| p.is_absolute() && p.is_dir()) {
        return rt.join("phonelink");
    }
    let cache = cache.filter(|p| p.is_absolute()).unwrap_or_else(|| home_dir().join(".cache"));
    cache.join("phonelink").join("tmp")
}

/// The scratch folder (or `sub` inside it), created private if missing.
pub fn dir(sub: Option<&str>) -> io::Result<PathBuf> {
    dir_in(&base_dir(), sub)
}

fn dir_in(base: &Path, sub: Option<&str>) -> io::Result<PathBuf> {
    if let Some(parent) = base.parent() {
        fs::create_dir_all(parent)?;
    }
    ensure_private_dir(base)?;
    match sub {
        Some(s) => {
            let d = base.join(s);
            ensure_private_dir(&d)?;
            Ok(d)
        }
        None => Ok(base.to_path_buf()),
    }
}

/// Create `dir` with mode 0700, or accept an existing one only if it is a
/// real directory (not a symlink) owned by us; tighten its mode if needed.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() {
        return Err(io::Error::other(format!("{} is not a directory", dir.display())));
    }
    if let Some(uid) = my_uid()
        && meta.uid() != uid
    {
        return Err(io::Error::other(format!("{} is owned by another user", dir.display())));
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// A folder holding the user's own data (messages, cached pictures, contact
/// photos): created 0700 with any missing parents, and an existing one
/// tightened to 0700. Unlike the scratch folder a symlink is accepted, since
/// people move their data folders; the target is what gets tightened.
pub fn private_data_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    tighten(dir, 0o700)
}

/// Set `path` to `mode` when group or others have any access to it. A
/// missing path is fine (SQLite's -wal/-shm come and go).
pub fn tighten(path: &Path, mode: u32) -> io::Result<()> {
    match fs::metadata(path) {
        Ok(m) if m.mode() & 0o077 != 0 => fs::set_permissions(path, fs::Permissions::from_mode(mode)),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Earlier versions created the data and cache folders readable by everyone
/// (0755, files 0644). Close them at startup; a 0700 folder keeps the old
/// files inside private too.
pub fn tighten_existing_data(home: &Path, cache_home: Option<PathBuf>) {
    let cache = cache_home.filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(".cache"));
    for dir in [super::settings::data_dir(home), cache.join("phonelink")] {
        if dir.is_dir()
            && let Err(e) = tighten(&dir, 0o700)
        {
            log::warn!("couldn't make {} private: {e}", dir.display());
        }
    }
}

/// Our effective uid: the owner of `/proc/self` (std has no getuid).
fn my_uid() -> Option<u32> {
    fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// Whether `path` lies inside the scratch folder, so the app may delete it.
pub fn contains(path: &Path) -> bool {
    contains_in(&base_dir(), path)
}

fn contains_in(base: &Path, path: &Path) -> bool {
    path.is_absolute()
        && !path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        && path.starts_with(base)
        && path != base
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `data` to `path` readable only by us (0600): into a fresh temp
/// sibling, then renamed over `path`, so a reader never sees half a file.
pub fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| io::Error::other("no parent folder"))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for _ in 0..100 {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
        let mut file = match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let written = file.write_all(data).and_then(|()| file.sync_all());
        drop(file);
        if let Err(e) = written.and_then(|()| fs::rename(&tmp, path)) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        return Ok(());
    }
    Err(io::Error::other("could not pick a temp file name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(p: &Path) -> u32 {
        fs::symlink_metadata(p).unwrap().mode() & 0o777
    }

    #[test]
    fn runtime_dir_wins_over_cache() {
        let rt = tempfile::tempdir().unwrap();
        let base = base_dir_from(Some(rt.path().to_path_buf()), Some("/c".into()));
        assert_eq!(base, rt.path().join("phonelink"));
    }

    #[test]
    fn falls_back_to_cache_without_runtime_dir() {
        let base = base_dir_from(None, Some("/c".into()));
        assert_eq!(base, PathBuf::from("/c/phonelink/tmp"));
        let base = base_dir_from(Some("relative".into()), Some("/c".into()));
        assert_eq!(base, PathBuf::from("/c/phonelink/tmp"));
    }

    #[test]
    fn creates_private_dirs() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("phonelink");
        let sub = dir_in(&base, Some("message_attachments")).unwrap();
        assert_eq!(sub, base.join("message_attachments"));
        assert_eq!(mode(&base), 0o700);
        assert_eq!(mode(&sub), 0o700);
    }

    #[test]
    fn tightens_an_open_existing_dir() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("phonelink");
        fs::create_dir(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o777)).unwrap();
        dir_in(&base, None).unwrap();
        assert_eq!(mode(&base), 0o700);
    }

    #[test]
    fn refuses_a_symlink_or_file() {
        let t = tempfile::tempdir().unwrap();
        let elsewhere = t.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let link = t.path().join("phonelink");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(dir_in(&link, None).is_err());
        let file = t.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(dir_in(&file, None).is_err());
    }

    #[test]
    fn contains_only_paths_inside() {
        let base = Path::new("/run/user/1000/phonelink");
        assert!(contains_in(base, Path::new("/run/user/1000/phonelink/paste_1.png")));
        assert!(contains_in(base, Path::new("/run/user/1000/phonelink/message_attachments/a.jpg")));
        assert!(!contains_in(base, base));
        assert!(!contains_in(base, Path::new("/home/u/Pictures/photo.jpg")));
        assert!(!contains_in(base, Path::new("/tmp/phonelink/paste_1.png")));
        assert!(!contains_in(base, Path::new("/run/user/1000/phonelink/../../../home/u/photo.jpg")));
        assert!(!contains_in(base, Path::new("/run/user/1000/phonelinkX/a.png")));
    }

    #[test]
    fn data_dirs_are_created_and_tightened_private() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("share").join("phonelink");
        private_data_dir(&d).unwrap();
        assert_eq!(mode(&d), 0o700);
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        private_data_dir(&d).unwrap();
        assert_eq!(mode(&d), 0o700);
        let f = d.join("messages.db");
        fs::write(&f, b"x").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
        tighten(&f, 0o600).unwrap();
        assert_eq!(mode(&f), 0o600);
        tighten(&d.join("missing-wal"), 0o600).unwrap();
    }

    #[test]
    fn startup_closes_old_open_folders() {
        let home = tempfile::tempdir().unwrap();
        let data = super::super::settings::data_dir(home.path());
        let cache = home.path().join(".cache").join("phonelink");
        for d in [&data, &cache] {
            fs::create_dir_all(d).unwrap();
            fs::set_permissions(d, fs::Permissions::from_mode(0o755)).unwrap();
        }
        tighten_existing_data(home.path(), None);
        assert_eq!(mode(&data), 0o700);
        assert_eq!(mode(&cache), 0o700);
    }

    #[test]
    fn writes_owner_only_files() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a.png");
        write_private(&p, b"one").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"one");
        assert_eq!(mode(&p), 0o600);
        write_private(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 1, "no temp files left behind");
    }
}
