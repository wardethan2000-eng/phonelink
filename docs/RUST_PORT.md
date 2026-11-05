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

## Phase 2 status (D-Bus layer)
`src/dbus/`: `client.rs` (every method of `dbus_client.py`, async on
`call_future`, same names, paths and timeouts), `variant.rs` (message and
attachment tuples, tolerant of int widths, `v` wrappers and `a(s)`/`av`/`as`
address lists), `introspect.rs` (introspection XML → interfaces/members),
`signals.rs` (subscriptions that unsubscribe on drop, daemon name watch,
logind `PrepareForSleep`). One change from Python: a device refresh is two
`GetAll` calls (device + battery) instead of six property reads.
`tests/mock_daemon.rs` runs the client against a mock `org.kde.kdeconnect`
on the build box's session bus; `cargo run --example mock_daemon` runs the
same mock for trying `--dump` (and later the UI) without a phone.
`phonelink-rs --dump [--dump-signatures]` is the live check on the laptop.
`--dump-signatures` also compares the daemon's introspected surface with the
list in rule 11, so `busctl` is not needed. Expected wire shape, unverified
until the laptop run: `(isa(s)xiixxxa(xsss))` inside `v`.
Live results (laptop, daemon 26.08, 2026-11-05):
- Wire shape inside `v` is `(isa(s)xiixixa(xsss))`: uID is `i` and subID is
  `x`. 342 of 342 `activeConversations` entries and 1144 of 1144 replayed
  signals parsed. `conversationLoaded` is `(xt)`.
- The surface matches rule 11 except as follows.
  - `deleteConversation` and `markConversationAsRead` are absent (both expected
    and handled).
  - `clipboard.content` and the `clipboardChanged` signal are absent. The
    clipboard plugin only has `sendClipboard` overloads, `isAutoShareDisabled`
    and `autoShareDisabledChanged`, so the Python clipboard tab's subscription
    never fires. Re-check what that tab can show before porting it (Phase 4).
  - Unused but available: `sms.requestAllConversations`, `sms.sendSms`
    (also with a sub id), `share.shareText`, `notifications.sendAction`,
    `device.hasPlugin` / `loadedPlugins`, and `battery.hasBattery`.
- Qt quirks the introspection reader now handles: signal args are marked
  `direction="out"`; a subclass repeats `org.kde.kdeconnect.daemon` as a second
  `<interface>` block, which is merged; methods with default arguments appear
  once per overload.
- `sendClipboard` exists only with no arguments; that form sends the
  desktop's current clipboard. The Python's `sendClipboard(s)` therefore
  fails on 26.08. The port will put the text on the local clipboard and then
  call `sendClipboard()` (Phase 4). Every signal's arguments are now listed
  correctly: `reachableChanged (b)`, `battery.refreshed (bi)`,
  `attachmentReceived (ss)`, `localCacheSynchronized (as)`,
  `callReceived (sss)`, and `notification*` / `shareReceived` `(s)`.
- Compared with the Python store (opened read-only, counts only): 342 thread rows = 342
  phone threads. For the 20 newest conversations, every thread under 100
  messages has the same count in both (1, 3, 7, 13). Larger threads are capped
  at 100 by the dump's replay; the store has the full history. Phase 2 is done.
- 342 phone threads merge into 326 conversations (47 groups). The dump finds
  no self number (it does not read contacts).
- Fixed in the port (Ethan approved 2026-11-05; Python unchanged): threads with no addresses all got identity `""`
  and merged into one conversation (seen: 2 phone threads), and could not be
  hidden. They are now keyed `thread:<id>` (`reconcile::conversation_key`).
- UI feature levels: GTK 4.10 / Adw 1.4 (Ethan, 2026-11-05: wide reach), the
  same minimum as the Python app and as `core::compat`.


## Reported GitHub issues (as of 2026-11-05)
| # | Issue | Plan |
|---|---|---|
| 8 | `pip install .` fails (flat-layout auto-discovery; deprecated license table). Closed by the reporter, **not fixed** | Won't fix in Python (Ethan, 2026-11-05): the Rust app replaces it, and Python packaging goes with it. When the Rust app is public, tell the reporter (they were packaging it for Fedora); a Rust binary plus the Flatpak (#2) are the install routes |
| 6 | Linux Mint 21.1: the window shows only controls, which do nothing | Likely cause, unverified: Mint 21 ships GTK 4.6 / Adw 1.1, and the app uses Adw 1.4 widgets (`OverlaySplitView`, `SwitchRow`). Rust app: check versions at startup and show a clear message; the Flatpak (#2) removes the dependency on distro versions. Ask the reporter for terminal output |
| 2 | Distribute as a Flatpak | **On hold (Ethan undecided). No reply posted; Ethan said not to reply for now.** If done, becomes the main install route for the Rust app, on the GNOME runtime with `--talk-name=org.kde.kdeconnect` and access to the sftp mount. KDE Connect stays on the host. Comes after cutover |
| 1 | Use the phone as a desktop camera (reporter offers to help) | KDE Connect has no camera plugin. A feasible route is scrcpy's camera source plus v4l2loopback, which needs adb. A new feature, after the port; Ethan decides |

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
Google Contacts. 7. Side-by-side on the laptop, measure, switch autostart. The two apps now
   have different app ids, so GApplication no longer stops both running at once.
   At startup the Rust app checks whether the session-bus name `dev.phonelink.app`
   is owned (the Python app is running). If it is, it says so and exits, because
   both would write the same store.
8. Flatpak: **on hold** (Ethan, 2026-11-05: not sure he wants it). See below.

## Public commitments
Things said in public that the project owes people. Close each one out when
it is done.
| Where | Said (date) | Owed |
|---|---|---|
| Issue #8 | 2026-11-05: rewriting in Rust; single binary, Flatpak planned; "I'll post here when it's ready" | A comment on #8 when the Rust app has a release, with install steps, inviting the reporter to package it for Fedora. The 2026-11-05 comment said a Flatpak was planned. While it is on hold, don't repeat that, and correct it in the follow-up if it's still undecided |
| Issue #6 | 2026-11-05: likely too-old GTK/Adw on Mint 21; the Rust version will check at startup and say so plainly; asked for `python3 run.py` output | Ship the startup version check, then comment on #6. Read any terminal output the reporter posts |
| Issue #1 | 2026-11-05: hard (KDE Connect can't stream the camera), not in the plans, Rust version in progress | Nothing owed; mention the Rust release there as a courtesy |

## Phase 8: Flatpak (ON HOLD)
On hold since 2026-11-05: Ethan hasn't decided whether to ship a Flatpak. Don't
start this work, and don't promise it in public, until he says so. The notes
below are kept for if he does.

Goal: one install that works on any distro with KDE Connect, whatever GTK and
libadwaita the distro ships. That closes #2 and makes #6 (Mint 21 too old)
go away.

- **Runtime:** `org.gnome.Platform` at a version that ships at least GTK 4.22
  and Adw 1.9, or lower the crate feature levels to match the runtime.
  Build with `org.freedesktop.Sdk.Extension.rust-stable`. Crates are
  vendored for offline builds (`flatpak-cargo-generator`).
- **KDE Connect stays on the host.** The sandbox talks to it over the
  session bus: `--talk-name=org.kde.kdeconnect`,
  `--system-talk-name=org.freedesktop.login1` (resume from sleep), and for
  the tray `--talk-name=org.kde.StatusNotifierWatcher` plus ownership of the
  SNI name it registers.
- **Files tab:** KDE Connect mounts the phone over sftp on the host. Find the
  real mount path from `getMountPoint` on the laptop, then grant just that
  (it is likely under `$XDG_RUNTIME_DIR`). Don't grant `--filesystem=home`.
- **Data:** Flatpak redirects `~/.local/share` into `~/.var/app/<id>/`. To
  keep the existing `messages.db`, `settings.json`, contacts and caches,
  either grant those exact dirs (`--filesystem=~/.local/share/phonelink:create`
  and the cache dir), or copy them over once on first run. Pick the first if
  the Python app is still in use, the second otherwise. KDE Connect's synced
  vCards (`~/.local/share/kpeoplevcard`) need read-only access.
- **Portals instead of host access:** autostart goes through the Background
  portal (`RequestBackground`), not a file written to `~/.config/autostart`.
  Open file/URL goes through OpenURI, and file choosing through FileChooser
  (`Gtk.FileDialog` already uses it). Google sign-in needs `--share=network`
  and opens the browser through OpenURI.
- **App id: `io.github.wardethan2000_eng.phonelink`** (decided 2026-11-05).
  Flathub verifies it from the GitHub account, with no domain needed. The Rust app uses
  it from day one (`core::settings::APPLICATION_ID`). The desktop file,
  icons and metainfo are named after it. The Python app keeps
  `dev.phonelink.app`. On first run the Rust app replaces the Python
  autostart entry (`dev.phonelink.app.desktop`) with its own.
- **Ship:** Flathub submission (manifest, metainfo with screenshots, OARS
  rating, desktop file, icons). A `.flatpak` bundle on GitHub releases in the
  meantime.
- **Then:** keep the promise to #8, and reply on #2 and #6 (texts shown to
  Ethan first).
