//! App settings and atomic file writes
//! (port of `phonelink/settings.py` and `phonelink/atomicio.py`).
//!
//! Same file as the Python app (`~/.local/share/phonelink/settings.json`) and
//! the same keys. Python and Rust share it, so keys this module does not know
//! are kept in `extra` and written back untouched.
//!
//! Notes:
//! - Without serde_json's `preserve_order` feature keys are written sorted,
//!   while Python writes defaults-first. Content is identical; only the order
//!   differs (so the first save by the other app rewrites the file once).
//! - Setters return `io::Result<()>` because Python lets write errors
//!   propagate.
//! - `batch` takes a closure instead of being a context manager.
//! - The module-level singleton (`get_settings`) is not ported; the app owns
//!   one `Settings`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::core::autocap::AutocapRules;

/// GitHub-verified id, as Flathub requires (Ethan, 2026-11-05). The Python
/// app used `dev.phonelink.app`; its autostart entry is cleaned up as legacy.
pub const APPLICATION_ID: &str = "io.github.wardethan2000_eng.phonelink";
pub const DESKTOP_FILENAME: &str = "io.github.wardethan2000_eng.phonelink.desktop";
/// Autostart entries written by earlier versions; replaced by ours.
pub const LEGACY_DESKTOP_FILENAMES: [&str; 2] = ["dev.phonelink.app.desktop", "phonelink.desktop"];

pub const MESSAGE_FONT_SCALE_MIN: f64 = 0.8;
pub const MESSAGE_FONT_SCALE_MAX: f64 = 3.0;

/// `$HOME` like Python's `Path.home()` (falls back to `/`).
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `<home>/.local/share/phonelink`
pub fn data_dir(home: &Path) -> PathBuf {
    home.join(".local").join("share").join("phonelink")
}

/// `<home>/.local/share/phonelink/settings.json`
pub fn settings_file(home: &Path) -> PathBuf {
    data_dir(home).join("settings.json")
}

/// `<home>/.config/autostart`
pub fn autostart_dir(home: &Path) -> PathBuf {
    home.join(".config").join("autostart")
}

// ── Atomic writes (atomicio.py) ────────────────────────────────────

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `text` to `path` atomically (temp file in the same directory + fsync
/// + rename). Creates parent directories. The temp file is removed on failure.
pub fn atomic_write_text(path: impl AsRef<Path>, text: &str) -> io::Result<()> {
    atomic_write_with(path.as_ref(), text, |from, to| fs::rename(from, to))
}

/// Serialize `data` as pretty JSON (2-space indent, non-ASCII kept) and write
/// it atomically.
pub fn atomic_write_json<T: Serialize>(path: impl AsRef<Path>, data: &T) -> io::Result<()> {
    let text = serde_json::to_string_pretty(data).map_err(io::Error::other)?;
    atomic_write_text(path, &text)
}

/// `atomic_write_text` with an injectable rename step (tests simulate a crash
/// between writing the temp file and the swap).
pub fn atomic_write_with(
    path: &Path,
    text: &str,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut opened = None;
    for _ in 0..100 {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(f) => {
                opened = Some((f, tmp));
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    let (mut file, tmp) = opened.ok_or_else(|| {
        io::Error::new(io::ErrorKind::AlreadyExists, "could not create a temp file")
    })?;

    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

// ── Autostart entry ────────────────────────────────────────────────

/// Quote one desktop Exec argument according to the desktop-entry parser.
pub fn quote_desktop_exec_arg(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Text of the autostart `.desktop` file. `exec` is the full Exec value
/// (already quoted if needed, e.g. via `quote_desktop_exec_arg`).
pub fn desktop_entry_text(exec: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Name=PhoneRelay\n\
         Comment=Connect your Android phone - SMS, notifications, and files\n\
         Exec={exec}\n\
         Icon={APPLICATION_ID}\n\
         Terminal=false\n\
         Type=Application\n\
         Categories=Utility;Communication;\n\
         Keywords=phone;sms;notifications;kdeconnect;android;\n\
         StartupWMClass={APPLICATION_ID}\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

// ── Python-style coercions for tolerant loading ────────────────────

fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `bool(x)` where a missing key means `default`.
fn py_bool(v: Option<Value>, default: bool) -> bool {
    match v {
        None => default,
        Some(v) => py_truthy(&v),
    }
}

/// `float(x or fallback)`; unparsable values give `fallback` (Python would
/// raise for some of them).
fn py_float(v: Option<Value>, fallback: f64) -> f64 {
    match v {
        None => fallback,
        Some(v) if !py_truthy(&v) => fallback,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(fallback),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(fallback),
        Some(Value::Bool(true)) => 1.0,
        Some(_) => fallback,
    }
}

/// `int(x or 0)`; unparsable values give 0.
fn py_int(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

fn py_str(v: Option<Value>) -> String {
    match v {
        Some(Value::String(s)) => s,
        Some(v) if py_truthy(&v) => match v {
            Value::Bool(_) => "True".to_string(),
            other => other.to_string(),
        },
        _ => String::new(),
    }
}

/// 0 (automatic) or a factor between 1 and 2.
fn clamp_ui_scale(x: f64) -> f64 {
    if !x.is_finite() || x <= 0.0 {
        0.0
    } else {
        x.clamp(1.0, 2.0)
    }
}

fn clamp_scale(x: f64) -> f64 {
    MESSAGE_FONT_SCALE_MAX.min(MESSAGE_FONT_SCALE_MIN.max(x))
}

// ── Settings ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Settings {
    settings_file: PathBuf,
    autostart_file: PathBuf,
    legacy_autostart_files: Vec<PathBuf>,

    color_scheme: String,
    open_on_startup: bool,
    notifications_enabled: bool,
    ignored_apps: Vec<String>,
    google_background_sync: bool,
    google_account_label: String,
    google_last_sync_ts: f64,
    google_last_attempt_ts: f64,
    /// device id -> conversation key -> deleted_at
    hidden: BTreeMap<String, BTreeMap<String, i64>>,
    message_font_scale: f64,
    /// Size of the whole window's text and icons; 0 = automatic.
    ui_scale: f64,
    /// Main window size at the last close or quit (0 = never saved).
    window_width: i32,
    window_height: i32,
    window_maximized: bool,
    /// Files → Recent Photos: list instead of grid, oldest first, headings per day.
    photos_as_list: bool,
    photos_oldest_first: bool,
    photos_by_day: bool,
    /// Capitals while typing: at the start of sentences, and for "I".
    capitalize_sentences: bool,
    capitalize_i: bool,
    /// Keys we do not know (written by Python or a newer version); preserved.
    extra: Map<String, Value>,

    last_written: Option<String>,
    batch_depth: u32,
    batch_dirty: bool,
    write_count: usize,
}

impl Settings {
    /// Settings at the default location under `$HOME`.
    pub fn new() -> Self {
        let home = home_dir();
        Self::with_paths(settings_file(&home), autostart_dir(&home))
    }

    /// Settings with explicit paths (tests; any base directory).
    pub fn with_paths(settings_file: PathBuf, autostart_dir: PathBuf) -> Self {
        let mut s = Self {
            autostart_file: autostart_dir.join(DESKTOP_FILENAME),
            legacy_autostart_files: LEGACY_DESKTOP_FILENAMES.iter().map(|n| autostart_dir.join(n)).collect(),
            settings_file,
            color_scheme: "system".to_string(),
            open_on_startup: false,
            notifications_enabled: true,
            ignored_apps: Vec::new(),
            google_background_sync: true,
            google_account_label: String::new(),
            google_last_sync_ts: 0.0,
            google_last_attempt_ts: 0.0,
            hidden: BTreeMap::new(),
            message_font_scale: 1.0,
            ui_scale: 0.0,
            window_width: 0,
            window_height: 0,
            window_maximized: false,
            photos_as_list: false,
            photos_oldest_first: false,
            photos_by_day: true,
            capitalize_sentences: true,
            capitalize_i: true,
            extra: Map::new(),
            last_written: None,
            batch_depth: 0,
            batch_dirty: false,
            write_count: 0,
        };
        s.load();
        s
    }

    /// Number of times the file was actually written (test hook).
    pub fn write_count(&self) -> usize {
        self.write_count
    }

    pub fn path(&self) -> &Path {
        &self.settings_file
    }

    // ── Persistence ────────────────────────────────────────────────

    /// (Re)load from disk. Missing or corrupt file means defaults; every value
    /// is coerced the way the Python loader does.
    pub fn load(&mut self) {
        let mut m: Map<String, Value> = match fs::read_to_string(&self.settings_file) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(m)) => m,
                _ => Map::new(),
            },
            Err(_) => Map::new(),
        };

        self.color_scheme = match m.remove("color_scheme") {
            Some(Value::String(s)) => s,
            _ => "system".to_string(),
        };
        self.open_on_startup = py_bool(m.remove("open_on_startup"), false);
        self.notifications_enabled = py_bool(m.remove("notifications_enabled"), true);
        self.ignored_apps = match m.remove("notifications_ignored_apps") {
            Some(Value::Array(a)) => a
                .into_iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        self.google_background_sync = py_bool(m.remove("google_background_sync"), true);
        self.google_account_label = py_str(m.remove("google_account_label"));
        self.google_last_sync_ts = py_float(m.remove("google_last_sync_ts"), 0.0);
        self.google_last_attempt_ts = py_float(m.remove("google_last_attempt_ts"), 0.0);
        self.message_font_scale = clamp_scale(py_float(m.remove("message_font_scale"), 1.0));
        self.ui_scale = clamp_ui_scale(py_float(m.remove("ui_scale"), 0.0));
        self.window_width = py_int(m.remove("window_width").as_ref()).clamp(0, 100_000) as i32;
        self.window_height = py_int(m.remove("window_height").as_ref()).clamp(0, 100_000) as i32;
        self.window_maximized = py_bool(m.remove("window_maximized"), false);
        self.photos_as_list = py_bool(m.remove("photos_as_list"), false);
        self.photos_oldest_first = py_bool(m.remove("photos_oldest_first"), false);
        self.photos_by_day = py_bool(m.remove("photos_by_day"), true);
        self.capitalize_sentences = py_bool(m.remove("capitalize_sentences"), true);
        self.capitalize_i = py_bool(m.remove("capitalize_i"), true);
        m.remove("auto_capitalize"); // one switch for both, never released

        self.hidden = BTreeMap::new();
        if let Some(Value::Object(devices)) = m.remove("hidden_conversations") {
            for (device_id, entries) in devices {
                let Value::Object(entries) = entries else { continue };
                let mut normalized = BTreeMap::new();
                for (key, payload) in entries {
                    if key.is_empty() {
                        continue;
                    }
                    let Value::Object(payload) = payload else { continue };
                    normalized.insert(key, py_int(payload.get("deleted_at")));
                }
                if !normalized.is_empty() {
                    self.hidden.insert(device_id, normalized);
                }
            }
        }

        m.remove("notifications_transport");
        self.extra = m;
    }

    fn to_json_text(&self) -> String {
        let mut m = self.extra.clone();
        m.insert("color_scheme".into(), Value::from(self.color_scheme.clone()));
        m.insert("open_on_startup".into(), Value::from(self.open_on_startup));
        m.insert("notifications_enabled".into(), Value::from(self.notifications_enabled));
        m.insert(
            "notifications_ignored_apps".into(),
            Value::from(self.ignored_apps.clone()),
        );
        m.insert("google_background_sync".into(), Value::from(self.google_background_sync));
        m.insert(
            "google_account_label".into(),
            Value::from(self.google_account_label.clone()),
        );
        m.insert("google_last_sync_ts".into(), Value::from(self.google_last_sync_ts));
        m.insert("google_last_attempt_ts".into(), Value::from(self.google_last_attempt_ts));
        m.insert(
            "hidden_conversations".into(),
            serde_json::to_value(&self.hidden).unwrap_or_else(|_| Value::Object(Map::new())),
        );
        m.insert("message_font_scale".into(), Value::from(self.message_font_scale));
        m.insert("ui_scale".into(), Value::from(self.ui_scale));
        m.insert("window_width".into(), Value::from(self.window_width));
        m.insert("window_height".into(), Value::from(self.window_height));
        m.insert("window_maximized".into(), Value::from(self.window_maximized));
        m.insert("photos_as_list".into(), Value::from(self.photos_as_list));
        m.insert("photos_oldest_first".into(), Value::from(self.photos_oldest_first));
        m.insert("photos_by_day".into(), Value::from(self.photos_by_day));
        m.insert("capitalize_sentences".into(), Value::from(self.capitalize_sentences));
        m.insert("capitalize_i".into(), Value::from(self.capitalize_i));
        serde_json::to_string_pretty(&m).unwrap_or_else(|_| "{}".to_string())
    }

    /// Persist. Inside `batch` the write is deferred; an unchanged document
    /// is not rewritten.
    pub fn save(&mut self) -> io::Result<()> {
        if self.batch_depth > 0 {
            self.batch_dirty = true;
            return Ok(());
        }
        let serialized = self.to_json_text();
        if self.last_written.as_deref() == Some(serialized.as_str()) {
            return Ok(());
        }
        atomic_write_text(&self.settings_file, &serialized)?;
        self.write_count += 1;
        self.last_written = Some(serialized);
        Ok(())
    }

    /// Group several setters into a single atomic write on exit.
    pub fn batch<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> io::Result<R> {
        self.batch_depth += 1;
        let out = f(self);
        self.batch_depth -= 1;
        if self.batch_depth == 0 && self.batch_dirty {
            self.batch_dirty = false;
            self.save()?;
        }
        Ok(out)
    }

    // ── Accessors / mutators ───────────────────────────────────────

    pub fn color_scheme(&self) -> &str {
        &self.color_scheme
    }

    /// `value` must be "system", "light" or "dark".
    pub fn set_color_scheme(&mut self, value: &str) -> io::Result<()> {
        if !matches!(value, "system" | "light" | "dark") {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad color scheme"));
        }
        self.color_scheme = value.to_string();
        self.save()
    }

    pub fn message_font_scale(&self) -> f64 {
        self.message_font_scale
    }

    pub fn set_message_font_scale(&mut self, value: f64) -> io::Result<()> {
        self.message_font_scale = clamp_scale(value);
        self.save()
    }

    /// The window's size factor; 0 = automatic (see `ui::scale`).
    pub fn ui_scale(&self) -> f64 {
        self.ui_scale
    }

    pub fn set_ui_scale(&mut self, value: f64) -> io::Result<()> {
        self.ui_scale = clamp_ui_scale(value);
        self.save()
    }

    /// The main window's size at the last close, once one was saved.
    pub fn window_size(&self) -> Option<(i32, i32)> {
        (self.window_width > 0 && self.window_height > 0).then_some((self.window_width, self.window_height))
    }

    /// Recent Photos view: (as a list, oldest first, headings per day).
    pub fn photos_view(&self) -> (bool, bool, bool) {
        (self.photos_as_list, self.photos_oldest_first, self.photos_by_day)
    }

    pub fn set_photos_view(&mut self, as_list: bool, oldest_first: bool, by_day: bool) -> io::Result<()> {
        (self.photos_as_list, self.photos_oldest_first, self.photos_by_day) = (as_list, oldest_first, by_day);
        self.save()
    }

    pub fn window_maximized(&self) -> bool {
        self.window_maximized
    }

    /// Saves only when something changed (closing to the tray calls this often).
    pub fn set_window_state(&mut self, width: i32, height: i32, maximized: bool) -> io::Result<()> {
        let (w, h) = (width.max(0), height.max(0));
        if (w, h, maximized) == (self.window_width, self.window_height, self.window_maximized) {
            return Ok(());
        }
        (self.window_width, self.window_height, self.window_maximized) = (w, h, maximized);
        self.save()
    }

    pub fn autocap_rules(&self) -> AutocapRules {
        AutocapRules { sentences: self.capitalize_sentences, lone_i: self.capitalize_i }
    }

    pub fn set_capitalize_sentences(&mut self, value: bool) -> io::Result<()> {
        self.capitalize_sentences = value;
        self.save()
    }

    pub fn set_capitalize_i(&mut self, value: bool) -> io::Result<()> {
        self.capitalize_i = value;
        self.save()
    }

    pub fn open_on_startup(&self) -> bool {
        self.open_on_startup
    }

    /// Saves, then creates or removes the autostart entry. `exec` is the
    /// desktop Exec value for the Rust binary.
    pub fn set_open_on_startup(&mut self, value: bool, exec: &str) -> io::Result<()> {
        self.open_on_startup = value;
        self.save()?;
        self.apply_autostart(value, exec)
    }

    pub fn notifications_enabled(&self) -> bool {
        self.notifications_enabled
    }

    pub fn set_notifications_enabled(&mut self, value: bool) -> io::Result<()> {
        self.notifications_enabled = value;
        self.save()
    }

    pub fn notifications_ignored_apps(&self) -> Vec<String> {
        self.ignored_apps.clone()
    }

    pub fn google_background_sync(&self) -> bool {
        self.google_background_sync
    }

    pub fn set_google_background_sync(&mut self, value: bool) -> io::Result<()> {
        self.google_background_sync = value;
        self.save()
    }

    pub fn google_account_label(&self) -> &str {
        &self.google_account_label
    }

    pub fn set_google_account_label(&mut self, value: &str) -> io::Result<()> {
        self.google_account_label = value.to_string();
        self.save()
    }

    pub fn google_last_sync_ts(&self) -> f64 {
        self.google_last_sync_ts
    }

    pub fn set_google_last_sync_ts(&mut self, value: f64) -> io::Result<()> {
        self.google_last_sync_ts = value;
        self.save()
    }

    pub fn google_last_attempt_ts(&self) -> f64 {
        self.google_last_attempt_ts
    }

    pub fn set_google_last_attempt_ts(&mut self, value: f64) -> io::Result<()> {
        self.google_last_attempt_ts = value;
        self.save()
    }

    pub fn clear_google_account(&mut self) -> io::Result<()> {
        self.google_account_label.clear();
        self.google_last_sync_ts = 0.0;
        self.google_last_attempt_ts = 0.0;
        self.save()
    }

    // ── Hidden conversations ───────────────────────────────────────

    pub fn conversation_hidden_until(&self, device_id: &str, conversation_key: &str) -> i64 {
        if device_id.is_empty() || conversation_key.is_empty() {
            return 0;
        }
        self.hidden
            .get(device_id)
            .and_then(|m| m.get(conversation_key))
            .copied()
            .unwrap_or(0)
    }

    pub fn hide_conversation(
        &mut self,
        device_id: &str,
        conversation_key: &str,
        deleted_at: i64,
    ) -> io::Result<()> {
        if device_id.is_empty() || conversation_key.is_empty() {
            return Ok(());
        }
        self.hidden
            .entry(device_id.to_string())
            .or_default()
            .insert(conversation_key.to_string(), deleted_at);
        self.save()
    }

    /// Returns whether an entry was removed.
    pub fn unhide_conversation(&mut self, device_id: &str, conversation_key: &str) -> io::Result<bool> {
        if device_id.is_empty() || conversation_key.is_empty() {
            return Ok(false);
        }
        let Some(device_hidden) = self.hidden.get_mut(device_id) else {
            return Ok(false);
        };
        let removed = device_hidden.remove(conversation_key).is_some();
        if device_hidden.is_empty() {
            self.hidden.remove(device_id);
        }
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    // ── Ignored apps ───────────────────────────────────────────────

    pub fn add_ignored_app(&mut self, app_name: &str) -> io::Result<()> {
        if !self.is_app_ignored(app_name) {
            self.ignored_apps.push(app_name.to_string());
            self.save()?;
        }
        Ok(())
    }

    pub fn remove_ignored_app(&mut self, app_name: &str) -> io::Result<()> {
        if let Some(i) = self.ignored_apps.iter().position(|a| a == app_name) {
            self.ignored_apps.remove(i);
            self.save()?;
        }
        Ok(())
    }

    pub fn is_app_ignored(&self, app_name: &str) -> bool {
        self.ignored_apps.iter().any(|a| a == app_name)
    }

    // ── Autostart ──────────────────────────────────────────────────

    fn remove_ignoring_missing(path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn remove_legacy_autostart(&self) -> io::Result<()> {
        for f in &self.legacy_autostart_files {
            Self::remove_ignoring_missing(f)?;
        }
        Ok(())
    }

    fn apply_autostart(&self, enable: bool, exec: &str) -> io::Result<()> {
        if enable {
            atomic_write_text(&self.autostart_file, &desktop_entry_text(exec))?;
        } else {
            Self::remove_ignoring_missing(&self.autostart_file)?;
        }
        self.remove_legacy_autostart()
    }

    /// Sync the in-memory state with whether the autostart file exists, and
    /// rewrite a stale entry (e.g. one pointing at the Python launcher).
    pub fn sync_autostart_state(&mut self, exec: &str) -> io::Result<()> {
        let exists = self.autostart_file.exists() || self.legacy_autostart_files.iter().any(|f| f.exists());
        if exists != self.open_on_startup {
            self.open_on_startup = exists;
            self.save()?;
        }
        if exists {
            let current = fs::read_to_string(&self.autostart_file).unwrap_or_default();
            let desired = desktop_entry_text(exec);
            if current != desired {
                atomic_write_text(&self.autostart_file, &desired)?;
            }
            self.remove_legacy_autostart()?;
        }
        Ok(())
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod atomicio_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn writes_content_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a").join("b").join("settings.json");
        atomic_write_text(&target, "hello").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello");
    }

    #[test]
    fn json_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("c.json");
        atomic_write_json(&target, &json!({"x": [1, 2], "y": "é"})).unwrap();
        let back: Value = serde_json::from_str(&fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(back, json!({"x": [1, 2], "y": "é"}));
    }

    #[test]
    fn overwrite_is_atomic_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("d.txt");
        atomic_write_text(&target, "v1").unwrap();
        atomic_write_text(&target, "v2").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "v2");
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")));
        assert_eq!(names, vec!["d.txt".to_string()]);
    }

    #[test]
    fn failed_write_preserves_old_file_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("e.txt");
        atomic_write_text(&target, "original").unwrap();
        // Simulate a crash after the temp file is written but before the swap.
        let err = atomic_write_with(&target, "corrupt", |_, _| {
            Err(io::Error::other("boom"))
        });
        assert!(err.is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "original");
        let tmps: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(tmps.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_identity_matches_application_id() {
        assert_eq!(DESKTOP_FILENAME, format!("{APPLICATION_ID}.desktop"));

        let entry = desktop_entry_text("/usr/bin/phonelink-rs");

        assert!(entry.contains(&format!("StartupWMClass={APPLICATION_ID}")));
        assert!(entry.contains("Keywords=phone;sms;notifications;kdeconnect;android;"));
        assert!(!entry.contains("__PHONELINK_RUNPY__"));
        let exec_re = regex::Regex::new(r"(?m)^Exec=.+$").unwrap();
        assert!(exec_re.is_match(&entry));
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
        s: Settings,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let s = Settings::with_paths(path.clone(), dir.path().join("autostart"));
        Fixture { _dir: dir, path, s }
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn setter_writes_atomically() {
        let mut f = fixture();
        f.s.set_notifications_enabled(false).unwrap();
        assert!(f.path.exists());
        assert_eq!(read_json(&f.path)["notifications_enabled"], Value::Bool(false));
    }

    #[test]
    fn batch_collapses_writes_into_one() {
        let mut f = fixture();
        let before = f.s.write_count();
        f.s.batch(|s| {
            s.set_notifications_enabled(false).unwrap();
            s.set_google_account_label("me@example.com").unwrap();
            s.set_google_last_sync_ts(123.0).unwrap();
        })
        .unwrap();
        assert_eq!(f.s.write_count() - before, 1); // one write for the whole session
    }

    #[test]
    fn redundant_save_is_deduped() {
        let mut f = fixture();
        f.s.set_notifications_enabled(false).unwrap(); // first real write
        let before = f.s.write_count();
        f.s.set_notifications_enabled(false).unwrap(); // same value -> no write
        assert_eq!(f.s.write_count(), before);
    }

    #[test]
    fn window_size_is_remembered() {
        let mut f = fixture();
        assert_eq!(f.s.window_size(), None);
        f.s.set_window_state(1200, 800, true).unwrap();
        let mut again = Settings::with_paths(f.path.clone(), f._dir.path().join("autostart"));
        again.load();
        assert_eq!(again.window_size(), Some((1200, 800)));
        assert!(again.window_maximized());
        assert_eq!(again.photos_view(), (false, false, true));
        again.set_photos_view(true, true, false).unwrap();
        let mut third = Settings::with_paths(f.path.clone(), f._dir.path().join("autostart"));
        third.load();
        assert_eq!(third.photos_view(), (true, true, false));
    }

    #[test]
    fn capitals_have_two_switches_both_on_at_first() {
        let mut f = fixture();
        assert_eq!(f.s.autocap_rules(), AutocapRules { sentences: true, lone_i: true });
        f.s.set_capitalize_sentences(false).unwrap();
        let mut again = Settings::with_paths(f.path.clone(), f._dir.path().join("autostart"));
        again.load();
        assert_eq!(again.autocap_rules(), AutocapRules { sentences: false, lone_i: true });
        again.set_capitalize_i(false).unwrap();
        let mut third = Settings::with_paths(f.path.clone(), f._dir.path().join("autostart"));
        third.load();
        assert_eq!(third.autocap_rules(), AutocapRules::OFF);
    }

    #[test]
    fn ui_scale_is_automatic_until_set() {
        let mut f = fixture();
        assert_eq!(f.s.ui_scale(), 0.0);
        f.s.set_ui_scale(1.25).unwrap();
        assert_eq!(read_json(&f.path)["ui_scale"].as_f64().unwrap(), 1.25);
        f.s.set_ui_scale(9.0).unwrap();
        assert_eq!(f.s.ui_scale(), 2.0);
        f.s.set_ui_scale(-1.0).unwrap();
        assert_eq!(f.s.ui_scale(), 0.0);
    }

    #[test]
    fn message_font_scale_defaults_to_one() {
        let f = fixture();
        assert_eq!(f.s.message_font_scale(), 1.0);
    }

    #[test]
    fn message_font_scale_setter_clamps_and_persists() {
        let mut f = fixture();
        f.s.set_message_font_scale(99.0).unwrap(); // absurd -> clamped to max
        assert_eq!(f.s.message_font_scale(), MESSAGE_FONT_SCALE_MAX);
        assert_eq!(
            read_json(&f.path)["message_font_scale"].as_f64().unwrap(),
            MESSAGE_FONT_SCALE_MAX
        );
        f.s.set_message_font_scale(0.0).unwrap(); // below min -> clamped to min
        assert_eq!(f.s.message_font_scale(), MESSAGE_FONT_SCALE_MIN);
    }

    #[test]
    fn message_font_scale_loads_clamped_from_disk() {
        let f = fixture();
        fs::write(&f.path, r#"{"message_font_scale": 42}"#).unwrap();
        let reloaded = Settings::with_paths(f.path.clone(), f._dir.path().join("autostart"));
        assert_eq!(reloaded.message_font_scale(), MESSAGE_FONT_SCALE_MAX);
    }

    #[test]
    fn unknown_keys_survive_a_save() {
        let f = fixture();
        fs::write(&f.path, r#"{"future_key": {"a": [1]}, "notifications_transport": "loom"}"#)
            .unwrap();
        let mut s = Settings::with_paths(f.path.clone(), f._dir.path().join("autostart"));
        s.set_color_scheme("dark").unwrap();
        let v = read_json(&f.path);
        assert_eq!(v["future_key"], serde_json::json!({"a": [1]}));
        assert!(v.get("notifications_transport").is_none());
        assert_eq!(v["color_scheme"], "dark");
    }

    #[test]
    fn python_autostart_entry_is_replaced_by_ours() {
        let f = fixture();
        let dir = f._dir.path().join("autostart");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("dev.phonelink.app.desktop"), "[Desktop Entry]\nExec=python3 run.py\n").unwrap();
        let mut s = Settings::with_paths(f.path.clone(), dir.clone());
        s.sync_autostart_state("/opt/phonelink-rs").unwrap();
        assert!(s.open_on_startup());
        assert!(!dir.join("dev.phonelink.app.desktop").exists());
        let entry = fs::read_to_string(dir.join(DESKTOP_FILENAME)).unwrap();
        assert!(entry.contains("Exec=/opt/phonelink-rs"));
    }
}
