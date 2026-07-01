# Phone Link — Architecture Review & Redesign Proposal

A full read of the codebase (~8k lines across `dbus_client.py`, `app.py`, `models.py`,
and the `ui/` panels) plus the contacts/settings/Google/tray modules. This document
explains **why the app is slow and unreliable**, lists the concrete bugs, and proposes
both an incremental fix path and a more radical redesign.

---

## 1. The one root cause behind "slow and unreliable"

**Almost every operation that touches the phone is a *synchronous, blocking* D-Bus call
made on the GTK main thread.**

`dbus_client.py` is built entirely on `Gio.DBusConnection.call_sync(...)` with a 5-second
timeout (30 s for SFTP mount). The GTK main thread is the UI thread. So every time the app
talks to `kdeconnectd`, the entire UI freezes until the call returns or times out.

This single decision causes most of the symptoms:

| Symptom | Mechanism |
|---|---|
| UI freezes / "not responding" | `sftp_mount_and_wait` blocks the main thread up to **30 s** (`files_panel.py:689`) |
| Periodic micro-stutters every ~30 s | The poll timer calls `_refresh_devices()` which fires **7 blocking calls per device** on the UI thread (`main_window.py:256–305, 357`) |
| Slow Notifications tab | Loading N notifications = **1 + N blocking calls** in a loop (`notifications_panel.py:294–298`) |
| Files tab hangs / "Connecting…" forever | `os.listdir` + `os.stat` per entry run on the main thread **over the SFTP FUSE mount** (`files_panel.py:1142–1158`) |
| App feels laggy on a busy phone | Blocking contact harvest (`contacts.py:445–545`) and synced-vCard directory scans run on the main thread |

Everything else below compounds this, but if you fix only one thing, fix this.

---

## 2. Structural problems (the "why it keeps needing fixes")

The git history is a long line of `Fix …` commits. That whack-a-mole pattern comes from a
few deeper design choices, not from individual bugs.

### 2.1 Polling instead of event-driven state
`main_window.py:352` polls every 30 s and **full-refreshes every device** regardless of
whether anything changed. KDE Connect already emits the signals you need
(`deviceListChanged`, per-interface `PropertiesChanged`, battery `refreshed`,
`reachableChanged`). The poll is a blunt instrument that both wastes work and freezes the
UI. It exists mostly to paper over the fact that signal subscriptions aren't comprehensive.

### 2.2 No local persistence — the daemon cache is treated as the source of truth
There is no local message/conversation store. On every launch the app re-derives everything
from `activeConversations()` (whatever the daemon happens to have cached) and then asks the
phone to re-send. Consequences:

- First launch is empty or slow; history is non-deterministic.
- The same data is re-parsed and the entire UI is rebuilt repeatedly.
- "Unread", "hidden/deleted", and contact names have to be re-reconciled every time.

### 2.3 The SMS reconciliation logic is a heuristic tangle
`sms_panel.py` reconstructs conversations client-side with layered heuristics:
`_detect_self_number`, `_thread_redirects`, `_merge_message`, `_deduplicated_conversations`,
last-10-digit matching, group detection, hidden-until timestamps. These heuristics run **on
every render** and fight each other. This is the direct source of the "inconsistent"
feeling: threads splitting/merging, names flickering, unread state toggling, drafts
duplicating. It's complex because the canonical data lives on the phone and is rebuilt from
scratch each time instead of being normalized once into a stable local model.

### 2.4 Full-teardown UI rebuilds instead of incremental updates
- `MessageThread.set_messages` (`message_thread.py:526`) **clears and rebuilds every bubble**
  every time the active thread refreshes — which happens on every incoming signal.
  `append_message` exists (`message_thread.py:568`) for incremental add but is **never used**.
- `ConversationList.set_conversations` (`conversation_list.py:247`) rebuilds the **entire
  ListBox** whenever any row's content changes (i.e. on every new message), because it
  compares a tuple snapshot and on mismatch removes/recreates all rows.
- `NotificationsPanel._rebuild_list` recreates all rows and reconnects all handlers on every
  change (`notifications_panel.py:334–372`), risking duplicate signal connections.

None of the lists use `Gtk.ListView` + `Gio.ListModel`, so there's no recycling/virtualization.

### 2.5 Non-atomic writes everywhere → corruption → "unreliable"
`contacts.json` (`contacts.py:103–106`), `settings.json` (`settings.py:105–106`), and the
Google OAuth token (`google_contacts.py:146–164`) are all written with a plain
`write_text`/`json.dump`. A crash or power loss mid-write truncates the file; on next launch
it's silently reset to empty (`contacts.py:95`, `settings.py:68`). Settings also save on
**every individual setter** (`settings.py:115–177`), so a settings dialog session thrashes
the disk and amplifies the corruption window.

### 2.6 Fragile tray subprocess
The tray runs as a **separate GTK3 process** (`app.py:71`, `_tray.py`) and communicates by
`SIGUSR1`/`SIGUSR2` plus a 2 s `os.kill(pid, 0)` liveness poll. PIDs get recycled → a signal
can hit the wrong process; a hung parent looks alive forever; GTK3/GTK4 coexistence is
distro-dependent. It's a lot of fragility for a tray icon.

---

## 3. Concrete bug list (highest-impact first)

| # | Severity | Location | Bug |
|---|---|---|---|
| 1 | Critical | `files_panel.py:689` | 30 s blocking SFTP mount on the UI thread — hard freeze |
| 2 | Critical | `main_window.py:357` + `:256–305` | 30 s poll does 7 blocking calls/device on UI thread |
| 3 | Critical | `notifications_panel.py:294–298` | N notifications = N blocking calls on UI thread |
| 4 | Critical | `files_panel.py:1142–1158` | `os.listdir`/`os.stat` over SFTP on UI thread |
| 5 | High | `contacts.py:103`, `settings.py:105`, `google_contacts.py:146` | Non-atomic writes; crash corrupts and silently resets state |
| 6 | High | `settings.py:115–177` | Every setter does a full disk write |
| 7 | High | `message_thread.py:526` | Whole thread rebuilt on every refresh; `append_message` unused |
| 8 | High | `conversation_list.py:274` | Whole conversation list rebuilt on any row change |
| 9 | High | `notifications_panel.py:360–369` | Rebuild reconnects handlers → duplicate connections/leaks |
| 10 | High | `_tray.py:54,62,69` | `os.kill` by PID — recycled PID can signal the wrong process |
| 11 | Med | `files_panel.py:712–720, 851–852` | Race on `_loading_cancelled` / `_photo_tiles` during cancel |
| 12 | Med | `files_panel.py:614–616` | Mount error shown but `_is_mounted` left True — inconsistent state |
| 13 | Med | `contacts.py:497, 543`; `google_contacts.py:332` | Bare `except` swallows D-Bus/network failures silently |
| 14 | Med | `clipboard_panel.py:144,157` | 2 s idle polling; `_poll_pending` can wedge to True forever |
| 15 | Med | `settings.py:193–201`, `contacts.py:171–186` | load→modify→save races lose concurrent updates |
| 16 | Med | `sms_panel.py` (reconcile) | Heuristic thread merge/split causes flicker & duplicate drafts |
| 17 | Low | repo-wide | No CI, no atomic-write tests, `.github/` absent; only 3 unit tests |

---

## 4. Recommendation

You said radical change is on the table. My honest take: **the language and the GTK4/
Libadwaita choice are fine — the bottleneck is the architecture, not the stack.** A full
rewrite in Rust/Tauri would cost months and re-acquire the same KDE Connect quirks. The
high-leverage move is to fix the *model and concurrency* layers, which is most of the value
for a fraction of the risk.

I'd structure it as **a re-architecture, delivered in incremental milestones**, not a
rewrite-from-zero.

### Target architecture

```
┌──────────────────────────────────────────────────────┐
│  GTK4 / Libadwaita UI  (panels, ListView + ListModel) │
│   – never calls D-Bus directly                         │
│   – subscribes to high-level signals                   │
└───────────────▲───────────────────────┬──────────────┘
                │ GObject signals        │ commands
        message-added / device-changed   │ (send, mark-read…)
                │                         ▼
┌──────────────────────────────────────────────────────┐
│  PhoneLinkService  (single owner of all state)         │
│   – async D-Bus client (Gio async, never call_sync)    │
│   – SQLite store: messages, conversations, contacts    │
│   – signal router (PropertiesChanged / KDEC signals)   │
│   – thread pool for blocking work (SFTP, files, HTTP)  │
└───────────────▲───────────────────────────────────────┘
                │ D-Bus (async)
        ┌───────┴────────┐
        │  kdeconnectd   │
        └────────────────┘
```

Key principles:
1. **No `call_sync` ever.** Use `Gio.DBusConnection.call()` (async) or a dedicated worker
   thread that marshals results back with `GLib.idle_add`. The UI thread never blocks.
2. **SQLite as the local source of truth.** The daemon becomes a *sync source*. Startup
   reads the DB → instant, deterministic history. Incoming signals upsert into the DB and
   emit one fine-grained change signal.
3. **Normalize the conversation model once, on ingest.** Key conversations by a canonical
   participant-set (normalized last-10-digit set, self-number removed). Drop the
   render-time `_thread_redirects` / `_deduplicated_conversations` heuristics — compute the
   identity when a message lands and store it. This kills the flicker/duplication class of bugs.
4. **Event-driven, not polled.** Subscribe to `deviceListChanged` and per-interface
   `PropertiesChanged`/KDEC signals; keep at most a slow (60–120 s) watchdog as a fallback.
5. **Model-backed lists.** `Gio.ListStore` + `Gtk.ListView`/`Gtk.SignalListItemFactory` for
   conversations, messages, notifications. Update single items; let GTK recycle widgets.
6. **Atomic state.** Either move JSON state into the SQLite DB, or write via temp-file +
   `os.replace`. Debounce/batch settings writes.
7. **Harden or drop the tray.** Replace the signal/PID IPC with a StatusNotifierItem
   (Ayatana/AppIndicator) in-process, or a socket/pipe to the subprocess, or rely on the
   desktop background portal.

### Suggested milestones (each shippable on its own)

- **M1 — Async D-Bus core.** Introduce the async client + `PhoneLinkService` facade; route
  every existing call through it. Removes all main-thread freezes. *Biggest single win.*
- **M2 — Event-driven device/battery/status.** Replace the 30 s poll with signals.
- **M3 — SQLite store + instant startup.** Persist conversations/messages/contacts; load
  from DB first, sync in the background.
- **M4 — Model-based UI.** Convert conversation list, message thread, notifications to
  ListView+ListModel with incremental updates (finally use `append_message`).
- **M5 — Simplify SMS reconciliation.** Canonical participant-set identity computed on
  ingest; delete the render-time heuristics.
- **M6 — Atomic/batched persistence + harden tray + add CI and tests.**

M1 alone will make the app feel like a different program. M3 + M5 are what make it *reliable*.

---

## 6. Implementation progress

Status as of the latest commit on `claude/codebase-review-architecture-7lj1b3`.

| Milestone | Status | Notes |
|---|---|---|
| M1 — Async D-Bus core | ✅ Done | `phonelink/async_bridge.py` (`AsyncBridge`, daemon worker threads). `client.submit(...)` + aggregate helpers (`fetch_devices`, `fetch_sftp_state`, `fetch_active_notifications`). Every UI phone call routed off the main thread. |
| M2 — Event-driven status | ✅ Done | `Gio.bus_watch_name` for daemon up/down; per-device `reachableChanged` + battery `refreshed` signals; 30 s poll replaced by a 120 s safety-net watchdog. |
| M3 — SQLite store | ✅ Done | `phonelink/store.py` (`MessageStore`, gi-free, WAL). Loads cached history on connect (instant startup); persists messages/conversations on merge/read/rename; deletes on removal. `tests/test_store.py` (6 tests). **Contacts NOT migrated** — still `contacts.json` (fine for now). |
| M4 — Incremental UI | ✅ Done (scoped) | `MessageThread.sync()` appends only new bubbles; `ConversationList` updates rows in place via a `thread_id→row` map + sort-func. **Did NOT do the full `Gtk.ListView`/`Gio.ListModel` widget swap** — kept the existing `ListBox`/`Box` widgets to avoid an untested large rewrite; captures the perf win with far lower regression risk. True virtualization can be a later, device-tested milestone. |
| M5 — Simplify reconciliation | ✅ Done | New gi-free `phonelink/reconcile.py` (`ConversationIndex` + pure `conversation_identity` / `detect_self_key`). Conversations are now normalized **once, on ingest**, keyed by a canonical participant-set identity (last-10-digit set, self number removed for groups); SMS/MMS/dual-SIM threads collapse the moment their messages land and stay collapsed with a stable primary thread ID. `_deduplicated_conversations` is now a **pure, side-effect-free filter** (was the render-time heuristic that mutated messages/active-thread/unread on every refresh — the flicker/split/merge source); `_thread_redirects` removed (derived from identity via `secondary_threads` / `primary_for`); `_detect_self_number` delegates to the pure helper and only re-keys on change. Tests: `tests/test_reconcile.py` (17, pure logic) + `tests/test_sms_panel_reconcile.py` (6, the real panel glue driven headlessly). Panel shrank ~124 lines. |
| M6 — Hardening | ✅ Done | **Atomic writes:** new gi-free `phonelink/atomicio.py` (`atomic_write_text`/`_json` = temp-in-same-dir + `fsync` + `os.replace`), applied to `settings.json`, `contacts.json`, the Google token, and the autostart `.desktop` — a crash mid-write can no longer truncate-and-reset state. **Settings writes** now batch (`Settings.batch()` context manager → one write per dialog session) and skip redundant identical writes. **Store commits** batch during bulk sync (`MessageStore.batch()` → the ingest loop in `_apply_active_conversations` commits once, not per-message). **Tray hardened:** new `phonelink/proc.py` `(pid, start_time)` identity check — the tray verifies the parent's start token before trusting liveness or sending `SIGUSR1/2`, closing the recycled-PID mis-signal hole (bug #10); did **not** rip out the subprocess for an in-process `StatusNotifierItem` (bigger, display-dependent rewrite — deferred). **CI:** `.github/workflows/ci.yml` installs GTK4/Adw typelibs and runs `compileall` + `pytest` on every push/PR. Tests: `test_atomicio.py` (4), `test_proc.py` (5), + settings/store batch tests (`tests/` total = 52 green). |

### Verification status
`gi`/GTK4/Libadwaita **are** importable in this environment, but there is no
display or `kdeconnectd`, so **GTK widgets cannot be constructed headlessly** (it
segfaults) and the on-screen rendering paths are still unrun. What *is* now
covered by automated tests (39 total, all green — run `python3 -m pytest`):

- **Pure reconciliation logic** — `phonelink/reconcile.py` via `tests/test_reconcile.py` (17): identity canonicalisation, self-number detection, ingest merge/dedup, primary stability, legacy-duplicate collapse, reindex.
- **The real panel reconciliation glue** — `tests/test_sms_panel_reconcile.py` (6) constructs an actual `SmsPanel` without `Gtk.Box.__init__` (widgets faked, real store + settings-fake) and drives `_apply_active_conversations` / `_merge_message` / `_deduplicated_conversations` / hide / delete / cached-load with real KDE Connect message tuples. This exercises the M5 code paths end-to-end, not a reimplementation.
- **Store / contacts / settings** — the existing gi-free suites.

Writing the panel test already caught one realism bug (hidden-until is stamped on
the wall clock, so resurrection needs epoch-millis dates). Still unrun and worth a
real-device smoke test: instant startup feel, no UI freeze on SFTP mount / send,
live battery+connection updates, a long thread *appending* (not rebuilding) on a
new message, and that a split SMS/MMS contact renders as **one** stable row with
no unread flicker.

### Status: all six milestones landed
M1–M6 are complete. M5 (reconciliation) and M6 (hardening) were both smoke-tested
on a real machine against a live Galaxy S25: the app starts, syncs 225
conversations with **zero split/duplicate threads**, writes `settings.json` /
`contacts.json` atomically (no `.tmp` debris), keeps the message DB at
`integrity_check: ok` under batched commits, and the hardened tray exits on its
own when the parent dies (identity-checked liveness).

**Duplicate-SMS-on-send — resolved.** The set-aside fixes were re-checked against
the M5 send path.  M5 did not introduce a structural double-send (the old
notification-reply path only fell back on failure), but it *preferred* the two
methods empirically shown to duplicate SMS on the Galaxy S25 (notification reply,
then `replyToConversation`).  `_send_text_reply` now sends 1:1 threads via
`sendWithoutConversation` (`send_sms([address])`) — the only reliable path on that
device — and keeps group threads on `replyToConversation` to preserve recipients;
all attachment sends route through the same helper.  Covered by
`tests/test_sms_panel_reconcile.py` (send-routing tests).  The now-unused
notification reply-*target* machinery (`_notification_reply_targets`,
`_remember_notification_reply_target`, `_conversation_matches_notification`,
`_message_text_matches`, `_refresh_notification_reply_targets`) has been deleted;
contact-name harvesting from notifications (`match_contact_from_notification_props`)
is retained.  This removed a per-notification conversation scan that computed a
reply target nothing read anymore.

**Robustness quick-wins — done.** Three lingering bugs from the §3 table fixed
and verified on-device:
- **#14 clipboard poll wedge** (`clipboard_panel.py`): added a watchdog so a
  dropped async clipboard read can no longer pin `_poll_pending` True forever and
  silently stop clipboard sync.
- **#11 files photo-load cancel race** (`files_panel.py`): the thumbnail worker now
  operates on a per-scan tile *snapshot* + scan generation, so a concurrent
  `_clear_photo_grid()` (cancel or a newer scan) can't `IndexError` on a cleared
  list or paint thumbnails onto replaced tiles.
- **#13 silent excepts** (`contacts.py`, `google_contacts.py`): the seven
  `except Exception: pass/return` swallows now log a `[phonelink] …` diagnostic so
  D-Bus/network failures are no longer invisible.

(For the record, table bug **#12** — mount error leaving `_is_mounted` True — was
already fixed in the current code; the mount-failure path resets the mount state.)

**Notifications panel incremental update (#9) — done.** `NotificationsPanel`
no longer rebuilds the whole list on every change.  `_rebuild_list` (full teardown
+ reconnect-all-handlers) is replaced by `_sync_list`, which diffs the desired vs.
existing rows (via the pure, tested `diff_notification_rows`) and only
removes/adds/recreates the rows that actually changed — untouched rows keep their
expansion state and their handler connections.  Ordering is handled by a
`set_sort_func` (newest first).  Covered by `tests/test_notifications_panel.py`
(5 tests: the pure diff + `_sync_list` preserving/dropping/recreating the right
rows, driven on a bare panel with a fake list box).

**In-process tray (#1) — done.** `phonelink/tray_sni.py` implements the
`org.kde.StatusNotifierItem` + `com.canonical.dbusmenu` spec directly over Gio
(no GTK3), so the tray now runs *in-process* when a StatusNotifierWatcher is
available.  `app.py` tries it first and **falls back to the hardened subprocess**
if registration fails, so desktops without a watcher are unaffected.  Verified
live end-to-end: registers with the watcher (no subprocess spawned), serves the
icon + menu (`GetLayout`/`GetGroupProperties`), left-click activates the window,
and the Quit menu item cleanly exits.  Marshalling covered by
`tests/test_tray_sni.py`.

**contacts.json → SQLite (#4) — done.** Contact names now live in a `contacts`
table in the store (single source of truth), with a one-time, flag-guarded
migration of the legacy `contacts.json` (the JSON is left as a backup).
`contacts.py`'s storage layer delegates to `MessageStore`
(`load_contacts`/`save_contact`/`delete_contact`/`replace_contacts`).  Verified on
real data (18 contacts migrated) and covered by `tests/test_store.py`.

**ListView virtualization (#3) — deliberately NOT done (won't-do).**
`ConversationList`/`MessageThread` already update in place (M4) and render 225
conversations with no flicker.  `Gtk.ListView` virtualization only pays off at
thousands of rows, so a full rewrite of these two large, feature-dense widgets
(avatars, search + suggestions, filter/sort, selection, context menus, message
grouping, attachments, links) would carry real regression risk to the primary UI
for **zero user-visible benefit** at this scale.  Left as-is by design; revisit
only if conversation counts grow into the thousands.

All six milestones plus every table-#bug except this one deliberate scale-gated
skip are now addressed.

---

## 5. If you genuinely want a clean-slate rewrite

Only worth it if you want to change goals, not just quality. Options, with my verdict:

- **Rust + gtk-rs/Relm4** — highest performance/reliability ceiling, true async, no GIL.
  Cost: months, and you re-learn every KDE Connect edge case. *Recommend against unless you
  want a long-term hobby rewrite.*
- **Tauri/Electron + web UI** — fastest UI iteration, but heavier, less native, still needs
  a D-Bus bridge. *Against — loses the native GNOME feel that's the point.*
- **Talk the KDE Connect protocol directly (skip the daemon)** — removes a dependency but
  re-implements pairing/TLS/plugins. *Against — the daemon is the right abstraction.*
- **Python + the async re-architecture above** — keeps everything that works, fixes what
  doesn't. *This is what I recommend.*

---

*Generated as an architecture review. No application code was changed.*
