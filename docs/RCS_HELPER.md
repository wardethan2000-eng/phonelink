# phonelink-gm: Google Messages helper (Phase 9, Route B1)

Design, 2026-11-05. **Built so far (2026-11-06):** a `serve` mode with
`mark_read`, `delete` and per-chat unread events, used for read-state sync
and deletes (see `phonelink-gm/serve.go`; requests use an `op` field and
chats are matched by member keys, not ids). The rest is not built yet. Goal: full RCS in the Messages tab by
pairing the app as a Google Messages "for web" device.

## Why a separate helper
libgm (the Google Messages web protocol, from mautrix-gmessages) is Go and
AGPL-3.0, and the app is MIT. A separate program that the app talks to
through plain messages is not a combined work. So the helper is AGPL, the
app stays MIT, and both can ship together. A Rust rewrite of the protocol
(B2) is not needed for the license. It is only worth doing if libgm stops
being maintained or Go gets in the way of packaging.

**Proven on the laptop (2026-11-05):** gmcli (a Go CLI on libgm) paired with
Ethan's account through a Chromium sign-in and an emoji tap, synced, and
sent RCS.

## Shape
- `phonelink-gm/` in this repo: its own `go.mod`, `LICENSE` (AGPL-3.0) and
  README. It contains no app code. The repo README says the folder is AGPL.
- The app starts it as a child process (`gio::Subprocess`) only when Google
  Messages is linked, and stops it on quit. The protocol is JSON lines over
  stdin/stdout, read with `read_line_async` on the GLib main loop. That
  needs no socket, no D-Bus name, no tokio, and nothing to clean up if the
  app crashes: the pipe closes and the helper exits.
- **Helper owns:** the libgm client, the session file, sign-in, reconnects
  and the long poll.
- **App owns:** the store, the UI, contacts and every decision.
- **Binary:** a static Go build, about 25 MB, at `$libexecdir/phonelink-gm`.
  In development, `PHONELINK_GM` overrides the path.

## Protocol (version 1)
Every line is one JSON object. Requests carry `id`, and the helper answers
with `{"id", "ok": true, ...}` or `{"id", "ok": false, "error": {"kind", "message"}}`.
Events carry `event` and no `id`.

### App → helper
| Request | Fields | libgm |
|---|---|---|
| `hello` | `version` | answers with helper version, `paired`, `account` |
| `pair` | `browser?`, `cookies_file?` | browser sign-in (chromedp, as gmcli's `browserauth`), then `DoGaiaPairing` |
| `connect` / `disconnect` | | `Connect` / `Disconnect` |
| `unpair` | | `UnpairGaia`, then delete the session file |
| `set_active` | | `SetActiveSession` (see "One web session") |
| `list_conversations` | `count`, `folder?` | `ListConversations` |
| `fetch_messages` | `conv`, `count`, `cursor?` | `FetchMessages` (backfill, paging back) |
| `open_conversation` | `numbers[]` | `GetOrCreateConversation` (new chats) |
| `send_text` | `conv`, `text`, `tmp_id`, `reply_to?` | `SendMessage` |
| `send_media` | `conv`, `path`, `mime`, `text?`, `tmp_id` | `UploadMedia` + `SendMessage`. The helper reads the file itself, so no bytes go over the pipe |
| `download_media` | `msg`, `dest` | `DownloadMedia`, written to `dest` |
| `mark_read` | `conv`, `msg` | `MarkRead` |
| `typing` | `conv` | `SetTyping` |
| `react` | `msg`, `emoji`, `remove?` | `SendReaction` |

### Helper → app events
| Event | Fields | Meaning |
|---|---|---|
| `pair_emoji` | `emoji` | show it; Ethan taps the match on the phone |
| `paired` | `account` | |
| `ready` | | connected and syncing |
| `conversation` | `conv`, `name`, `participants[{number, name, is_me}]`, `group`, `transport` (`rcs`/`sms`), `last_ts`, `unread`, `read_only` | new or changed |
| `message` | `conv`, `msg`, `tmp_id?`, `ts_ms`, `from{number, name, is_me}`, `text`, `attachments[{id, mime, name, size}]`, `transport`, `status`, `reactions`, `reply_to?` | new or changed. Sends echo back with their `tmp_id` |
| `typing` | `conv`, `number`, `active` | |
| `phone` | `responding` | phone offline or back |
| `session` | `state`: `active`, `inactive`, `logged_out` | see below |
| `error` | `kind`, `message` | not tied to a request |

`status` is reduced to `sending`, `sent`, `delivered`, `read`, `failed`,
`received` from libgm's `MessageStatusType`. `transport` comes from the
conversation's `ConversationType` (RCS/SMS) and the message's `type`
(sms/mms/rcs).

## One web session at a time
Google keeps one web session active per account. If Ethan opens
messages.google.com in a browser, it takes over, and libgm reports the
helper as inactive (`BrowserActive` from another session). The mautrix
bridge retakes the session on its own. **We don't:** that would kick out
his browser tab every few seconds. Instead:
- On startup the helper calls `set_active` once.
- While inactive, the Messages tab shows a banner: "Google Messages is open
  in another browser. [Use here]". The button calls `set_active`.
- Sending while inactive takes the session back first, since the user asked
  for it.

## Messages tab when linked
**Decided (Ethan, 2026-11-05): while Google Messages is linked and
working, it is the only source for the Messages tab.** Google Messages
already holds every SMS, MMS and RCS on the phone, so one source means:
- no dedupe;
- no matching Google's conversations to KDE Connect threads;
- no hidden-texts problem;
- full history through `fetch_messages`.

KDE Connect stays the source for notifications, files, battery, ring,
clipboard and share. When not linked, logged out, or the helper won't
start, the tab falls back to KDE Connect as today (Route A included), with
a banner saying why. The other option, Google Messages for RCS threads and
KDE Connect for SMS, needs a merge by participants and a dedupe of messages
both sources see. It buys nothing over this.

## Store
- Google Messages data goes in the existing tables as its own "device":
  `device_id = "gm:<account email>"`. Every query is already per device,
  and the Python app never reads it.
- Google's ids are strings, and the store wants integers. A new table maps
  them:
  `gm_ids(kind TEXT, gm_id TEXT, local_id INTEGER, PRIMARY KEY(kind, gm_id))`.
  `local_id` is allocated in order.
- A new nullable `transport` column on `messages` drives the "RCS"/"SMS"
  label. Older rows stay NULL.
- `msg_type` uses the same Android values the KDE Connect rows use
  (1 = received, 2 = sent), so `core::messages` and the UI don't change.
  Sending, failed and delivered states map onto what the UI already shows.
- Contacts: names come from the Google Messages participants, used only
  when the desktop has none for that number.

## Rust side
- `core/gm.rs`: the protocol types (serde) and a pure state machine
  (linked, active or inactive, phone up or down) that turns events into
  store writes and `Effect`s. Tested without a process.
- `gm/process.rs`: spawn, the line reader, request ids, restart with
  backoff (1 s → 60 s), and giving up after 5 failures in 10 minutes.
- **Settings:** a "Google Messages" row with Link / Unlink, the account,
  and the state. Linking runs `pair`: the app shows a dialog ("Sign in in
  the window that opens") and then the emoji, large.
- **Tests:** a fake helper (`examples/fake_gm.rs`, speaking protocol v1
  from a script), the same idea as `mock_daemon`, for core and UI tests on
  the build box. The real helper is only run on the laptop.

## Sign-in
The helper drives a temporary Chromium profile, as gmcli does: it needs
Chromium or Chrome installed, which Ethan has. If none is found, the dialog
says so and offers the manual "copy as cURL" route (`cookies_file`). A
sign-in window inside the app (WebKitGTK) would drop the Chromium need, but
Google is starting to tie sign-in cookies to Chrome (device-bound
sessions), so it may not keep working. Try it later, not first.

## Session file and privacy
- The session file is `~/.local/share/phonelink/gm/session.json`, mode
  0600, holding the cookies, pairing keys and tokens. It is never logged,
  and the helper logs no message text.
- Later: keep it in the desktop keyring (Secret Service) instead of a
  file.
- Unlink calls `UnpairGaia`, deletes the file, and removes the
  `gm:` device's rows after asking.

## Limits to tell users
- Unofficial protocol: Google can break it, and the fix is a helper update.
- The phone must be online.
- After about 14 days unused, the pairing has to be redone. The app shows
  "Re-link" on `logged_out`.
- Only one web session at a time (above).

## Steps
1. **Helper CLI:** protocol v1 with pair, connect, list, fetch, send_text and
   events. Try it by hand on the laptop with JSON lines.
2. **Rust:** `core/gm.rs`, the process client and the fake helper. Tests on
   the box.
3. **Store and Messages tab:** the source switch, the banners and the
   settings row. Side by side with KDE Connect on the laptop.
4. **Media:** download and send media, mark read, typing, reactions.
5. **Packaging:** build the helper on the build box
   (`scripts/remote-build.sh` gets a `gm` target), install both together.

## Decisions (Ethan, 2026-11-05)
1. When linked, Google Messages is the only source for the Messages tab:
   **yes**.
2. Chromium needed for linking: **probably**. Size answer below. Any
   Chromium-based browser works (Chrome, Chromium, Brave, Edge, Vivaldi),
   because the helper drives it over the DevTools protocol. Firefox users
   get the manual cURL route.
3. The helper lives in this repo as an AGPL folder: **yes**.

## Install size
- Helper: one static Go binary, about 20-30 MB (estimate; gmcli, which also
  bundles SQLite, is the upper bound). This is the only size the app adds.
- Chromium is not part of the install. It is only needed on the machine
  when linking, and when re-linking after about 14 days unused. On Fedora
  it is roughly 350 MB if no Chromium-based browser is already there.
