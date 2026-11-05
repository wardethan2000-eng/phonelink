# Rust port — design note (Phase 0)

Goal: a Rust app that does everything the Python app does, at least as fast,
with less memory. Same UI, same behavior, same data files. Not a redesign.

Baselines to beat (Python, measured 2026-11-05): idle RSS ~200–240 MB; opening
the four biggest chats 11–28 ms; the main loop never blocks >20 ms during a
2,600-message replay.

## Key decisions

### 1. D-Bus: gio (via gtk-rs), not zbus
The app talks to `kdeconnectd` with `gio::DBusConnection` / `gio::DBusProxy`
async calls (`call_future`) and `signal_subscribe`, all on the GLib main loop.

Why:
- Signals arrive on the main context, which is exactly where they must be
  queued and merged in batches (rule: 200 per idle callback, one SQLite
  transaction). zbus would deliver on its own executor and every signal would
  have to be posted back — an extra hop per message, in bursts of thousands.
- No second async runtime (tokio/async-io) and no second D-Bus stack in the
  process: smaller binary, less memory, one event loop to reason about.
- Async gio calls never block the UI, so the Python worker pool
  (`async_bridge.py`, 4 threads) is not needed for D-Bus at all.
- The Python app already uses GDBus, so timeouts, error names
  (`UnknownMethod`, `UnknownObject`) and variant shapes behave identically.
- Cost: variants are unpacked by hand (`glib::Variant`) instead of derived
  with serde. That code is small and lives in one tested module.

### 2. Tray: port `tray_sni.py` onto gio, not `ksni`
`ksni` brings zbus + tokio for one icon and a two-item menu. The Python tray
is a 240-line in-process StatusNotifierItem + DBusMenu exported over GDBus;
the Rust version does the same with `DBusConnection::register_object`.

### 3. Threading model
- Main thread: GTK, all D-Bus (async), the store, the reconcile index.
- `gio::spawn_blocking` for: image measure/decode (`Pixbuf::from_file_at_scale`
  + `apply_embedded_orientation`, at 2× display size), file copies, vCard
  parsing, Google Contacts HTTP. Results come back with
  `glib::MainContext::spawn_local` / `glib::spawn_future_local`.
- No tokio anywhere.

### 4. Store: same file, same schema
`rusqlite` against the system SQLite (no `bundled`), opening
`~/.local/share/phonelink/messages.db` with the same `CREATE TABLE IF NOT
EXISTS` schema, WAL, `synchronous=NORMAL`. Lists in `addresses`/`attachments`
stay JSON text. `settings.json` keeps the same keys; the attachment caches keep
the same paths. Python and Rust can be swapped back and forth with no migration.

### 5. Crates (all checked against Fedora 44: GTK 4.22, Adw 1.9, GLib 2.88)
| Need | Crate | Notes |
|---|---|---|
| UI | `gtk4`, `libadwaita` | feature levels picked to match the laptop's GTK/Adw; checked on the build box |
| D-Bus, main loop, files | `gio`, `glib` (re-exported by gtk4) | |
| Images | `gdk-pixbuf` | same loaders (HEIC etc.) as the Python app |
| SQLite | `rusqlite` (system lib) | |
| JSON | `serde`, `serde_json` | |
| Google Contacts | `ureq` (blocking, on a worker) + a small loopback OAuth flow | avoids reqwest/tokio; Phase 6 |
| Errors/logging | `anyhow`, `log` + `env_logger` | |

Toolchain: stable 1.98.1 (same as the laptop). CI, if any, runs in a
`fedora:44` container so the GTK feature levels match.

## Module map (Python → Rust)
Pure modules (no GTK, unit tested; the ported Python tests are the parity spec):

| Python | Rust | Contents |
|---|---|---|
| `models.py` | `core/models.rs` | Device, SmsMessage, Conversation, Notification + labels |
| `sms_panel.py` (tuple parsing, `_I_*`) | `core/message.rs` | KDE Connect message tuple → SmsMessage |
| `reconcile.py` | `core/reconcile.rs` | participant keys, self-number detection, ConversationIndex |
| `store.py` | `core/store.rs` | same schema, `batch()` as a transaction guard |
| `hidden_texts.py` | `core/hidden_texts.rs` | missing-text detector + fix command |
| `sms_panel.py` (catch-up, gap fill, signal queue) | `core/sync.rs` | **new pure state machine**; today this logic is tangled into the panel. Timers are inputs, D-Bus requests are outputs, so it is testable without a daemon |
| `contacts.py` | `core/contacts.rs` | phone normalization, vCard parse, notification/telephony name harvest |
| `settings.py`, `atomicio.py` | `core/settings.rs` | same JSON keys, atomic writes, autostart .desktop |
| `google_contacts.py` | `core/google.rs` | Phase 6 |

Glue and UI:

| Python | Rust |
|---|---|
| `dbus_client.py` | `dbus/client.rs` (methods) + `dbus/signals.rs` |
| `async_bridge.py` | gone (gio async + `spawn_blocking`) |
| `tray_sni.py`, `_tray.py` | `tray.rs` |
| `app.py`, `ui/main_window.py` | `app.rs`, `ui/window.rs` |
| `ui/sms_panel.py`, `conversation_list.py`, `message_thread.py` | `ui/messages/{panel,list,thread,images}.rs` |
| `ui/notifications_panel.py`, `clipboard_panel.py`, `files_panel.py`, `settings_dialog.py`, `icons.py` | `ui/*.rs` one-to-one |

Layout: one Cargo package `phonelink-rs/` in this repo with a `core` module
tree that has no `gtk` imports, so `cargo test` for core needs no display.

## UI notes
- The Python UI builds widgets in code (no `.ui`/Blueprint files), so the
  Rust UI does the same. Custom GObject signals on panels
  (`conversation-selected`, `send-message`, …) become plain Rust callbacks,
  not GObject subclasses. Subclassing is only for widgets that really need it.
- `phonelink/style.css` is embedded with `include_str!`. The app icons are
  still added from `data/icons` with `IconTheme::add_search_path`.
- Lists keep `ListBox` with sort/filter funcs, and the photo grid keeps
  `FlowBox`, which matches today's behavior. Moving them to
  `ListView`/`GridView` is a later optimization, made only if measurements
  call for it.
- The Python app uses 7 `Adw.MessageDialog`s, which are deprecated since Adw
  1.6. The port uses `Adw.AlertDialog`, which looks the same.
- The minimum versions used today are Adw 1.4 and GTK 4.10. The laptop has
  Adw 1.9 and GTK 4.22.
- The Files tab mounts through KDE Connect over D-Bus (`mountAndWait`); there
  is no sshfs call in the app. The only external process is `xdg-open`, which
  becomes `gio::AppInfo::launch_default_for_uri`.
- `_tray.py` is not ported. It is a separate GTK3 + XApp tray process,
  used only when no StatusNotifierWatcher exists (Mint). Fedora has a watcher.

## Behavior that must carry over
The handoff's eleven rules (hidden texts, catch-up + gap fill, signal
batching, the 60/100 thread window without visible scroll, off-thread image
decode with a 64-texture LRU, `replyToConversation` vs
`sendWithoutConversation`, tuple layout, conversation identity, same store,
contacts, the D-Bus surface) are each tied to a test or a module above. The
D-Bus surface is verified against kdeconnect 26.08 before Phase 2 is called
done; `markConversationAsRead` (absent in 26.08) and a missing battery object
are handled quietly.

## Deferred / out of scope
- No redesign, no new features during the port.
- Google Contacts and the settings dialog come last (Phase 6); the Rust app
  reads the same saved Google token file so no re-login is needed.
- The Python app stays the default until cutover and stays runnable for a
  few weeks after.

## Phases
0. This note. 1. Core with ported tests. 2. D-Bus client + read-only
`phonelink-rs --dump`. 3. Messages tab. 4. Notifications, clipboard,
header/battery, ring, share. 5. Files tab. 6. Tray, settings, autostart,
Google Contacts. 7. Side-by-side on the laptop, measure, switch autostart.
