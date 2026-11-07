# Next features (after the Rust port)

Written 2026-11-06. Ethan picked six ideas for the next round. Before writing
this plan, each one was checked against the KDE Connect 26.08.3 source
(`invent.kde.org/network/kdeconnect-kde`, tag `v26.08.3`), the version on the
laptop. That check changed three of them:

| # | Feature | Status after the check |
|---|---|---|
| A | Search all message text | **Go.** Small to medium |
| B | RCS sending (Google Messages link) | **Go.** The biggest item, in five steps |
| C | First public release + owed issue comments | **Go.** Small |
| D | Calls in the chat | **Smaller than first pitched.** KDE Connect already shows call pop-ups with a Mute button |
| E | Phone media controls | **Small value on Plasma or GNOME.** Phone players already show in the desktop's media widget |
| F | Take a photo with the phone | **Dropped.** KDE Connect 26.08 has no photo plugin on the desktop side |

Order (Ethan, 2026-11-06): A, then C, then D, then B. E is skipped. See
"Decisions" at the end.

All work goes on `rust-port`. All checks run on the build box through
`scripts/remote-build.sh`. Every item adds tests in `core/` and ends with a run on
the laptop against the real phone.

---

## A. Search all message text

**Today:** the search box in the conversation list matches only the name,
the number and the **last** message (`ConvSummary::matches`,
`ui/conversation_list.rs:123`). A word from an older text finds nothing.

**Goal:** typing in the search box finds every conversation with a matching
message anywhere in its history. Opening a result jumps to the matching
message and highlights it.

### Design
- **No new database table.** `Store::load_conversations` already loads every
  message into memory at startup, so the search runs over memory. A full-text
  table (FTS5) would also need the Python app's writes kept in sync, and that
  isn't worth it.
- **Pure search in `core/search.rs`:**
  `search(convs, contacts, needle) -> Vec<Hit { thread_id, uid, snippet, ranges }>`.
  - Matching ignores case and accents: Unicode lowercase plus NFD with
    combining marks removed (`unicode-normalization` crate, or a small
    table if we'd rather not add a crate).
  - A query of digits only also matches phone numbers digit by digit
    (`(316) 555` finds `+13165551234`), using the existing number
    normalization in `core/contacts.rs`.
  - For each conversation, the hit is the newest matching message. The
    result also says how many messages matched ("3 matches").
  - The snippet is about 60 characters around the first match, cut at word
    boundaries with an ellipsis.
- **Speed:** each conversation caches its folded text (one `String` per
  message, built the first time anyone searches and dropped when that
  conversation changes). The search waits 150 ms after the last keystroke
  before running. Before shipping, measure on the laptop: with
  `PHONELINK_PERF=1`, a search over the whole store should take under 30 ms.

### UI
- **List:** a row that matches only in an older message shows the snippet,
  with the match in bold, in place of the last-message preview. A row that
  matches by name stays as it is now.
- **Opening a result:**
  - The thread view gets `reveal_message(uid)`. It widens the window of shown
    messages (today 60 shown, with "Show earlier" adding 100) until the match
    is included, then scrolls it to the middle of the view and flashes it
    with a highlight that fades out.
  - While a search is active, an "n of m" bar in the thread steps through the
    matches with up and down buttons, Enter and Shift+Enter. Esc closes the
    bar.
- **Search inside one chat:** Ctrl+F in an open thread opens the same bar
  for that chat only. Ctrl+F is free today; `window.rs` has only Ctrl+N and
  the zoom keys.

### Tests
- Core:
  - case- and accent-insensitive matching
  - digit-only queries matching numbers
  - newest hit chosen per conversation, with the right match count
  - snippet cut at a word boundary, including multi-byte text
  - an empty query returns nothing
  - hidden conversations are excluded
- UI: on the mock daemon (`scripts/ui-demo.sh`), take a screenshot of a
  result whose match is about 200 messages back, to check that the reveal
  and the highlight land on it.

**Status 2026-11-06:** built (`core/search.rs`, list, thread find bar).
Checked on the mock daemon in Broadway with a 2,226-message chat: a match
about 2,200 messages back opens centred and outlined; 278 matches step with
Enter and Shift+Enter; every search took under 10 ms in a debug build. The
list snippet puts only about 12 characters before the match, because the row
shows about 30 and cuts the end. Waiting on the laptop run.

**Done when:** on the laptop, a word from an old text finds its chat,
opening it lands on that text, and the perf log shows the search under
30 ms.

---

## B. RCS sending (Google Messages link)

**Today:** KDE Connect can only send SMS and MMS. An RCS chat, including
any RCS group, can be answered only while a phone notification for it is
still live (Route A, labeled "via notification"). The design for the
complete fix is in `docs/RCS_HELPER.md`: the `phonelink-gm` helper in Go on
libgm (AGPL), as a child process speaking JSON lines. Ethan made its three
decisions on 2026-11-05:
1. When linked, Google Messages is the only source for the Messages tab.
2. Chromium is used for linking.
3. The helper lives in this repo.

**Already there:**
- `phonelink-gm` exists as a one-shot CLI with `status` and `delete`. It
  uses the session file that gmcli paired (`~/.local/state/gmcli/session.json`).
- `data/install-rust.sh` installs it.
- Deleting a chat on the phone works through it (40baac4).

### Steps
Each step is shippable on its own.
1. **Long-running helper, protocol v1.** Add a `serve` mode alongside the
   CLI commands, which stay. It runs `hello`, `connect`, `list`, `fetch`,
   `send_text`, and sends events for new messages and status changes, as in
   the protocol tables in RCS_HELPER.md. For now it keeps using gmcli's
   session file, so linking isn't needed yet. Go tests with a fake libgm
   client. To try it by hand on the laptop, pipe JSON lines into it.
2. **Rust client.** `core/gm.rs` turns protocol lines into effects (pure,
   tested). `ui/` wiring starts the helper with `gio::Subprocess` and reads
   it with `read_line_async`. A fake helper, an example binary in the style
   of `examples/mock_daemon`, drives the tests on the box.
3. **Messages tab on Google Messages.**
   - **Source switch:** when linked, conversations and messages come from the
     helper. KDE Connect's SMS sync is turned off; notifications, files and
     the rest still go through KDE Connect.
   - **Store:** helper messages go into the same `messages` table under their
     own device id, so the Python app never sees them as its own.
   - **Sending:** goes through `send_text` for every chat, SMS and RCS
     alike. Route A's "via notification" label goes away while linked.
   - **Banners:** "Linked to Google Messages", "Re-link needed" (shown on
     `logged_out`) and "Google Messages is open in another browser"
     (RCS_HELPER.md "One web session at a time").
   - **Settings:** a row with Link / Unlink, and a status line.
4. **Linking inside the app.** Linking stops depending on gmcli: the helper
   drives a Chromium-based browser over the DevTools protocol to sign in to
   Google, then shows the emoji to tap on the phone, as RCS_HELPER.md "Sign-in"
   describes. Firefox-only users get the manual cURL route. The session file
   moves to the app's own state dir. gmcli's file is read once if present,
   so Ethan doesn't have to link again.
5. **Media and extras.** In order:
   - receive pictures and files
   - send pictures and files
   - mark as read on the phone
   - reactions shown in the chat (sending reactions only if it's cheap)
   - typing indicators last, if at all

**Packaging:** `scripts/remote-build.sh` gets a `gm` target, a static Go
build on the box. The release (item C) ships the helper next to the app.

**Done when:** on the laptop, with the phone's notifications cleared, a
reply to an RCS group goes out as RCS and shows in Google Messages on the
phone. Incoming RCS shows up within a few seconds without KDE Connect.

---

## C. First public release and the owed issue comments

**Today:**
- `main` on GitHub is the Rust app, but there is no tagged release.
- CI (`.github/workflows/ci.yml`) tests only the Python app.
- The README still says "Phone Link" in 14 places after the PhoneRelay
  rename (88cac19).
- Two comments are owed (see "Public commitments" in `docs/RUST_PORT.md`):
  - #8: install steps at the first release
  - #6: about the startup GTK version check

### Steps
1. **README:**
   - Finish the PhoneRelay rename. Keep "formerly Phone Link" once, so
     people who search for the old name still find the project.
   - Put the Rust install first. Move the Python steps to a "Python version
     (retired)" section at the bottom.
   - List the minimum versions plainly: GTK 4.10, libadwaita 1.4,
     KDE Connect, glibc 2.34. Give examples of distros that work (Fedora 40+,
     Ubuntu 24.04+, Mint 22+) and ones that don't (Ubuntu 22.04, Mint 21,
     Debian 12: GTK too old).
2. **Release archive.** `scripts/remote-build.sh release` builds on the box
   and packs `phonerelay-<version>-x86_64.tar.gz` containing:
   - `phonelink-rs`
   - `phonelink-gm` (AGPL, with its LICENSE next to it)
   - `data/` and `install-rust.sh`
   - a short INSTALL.txt

   Checksum it, then create the release with `gh release create` and these
   notes. Version: `v0.2.0` (Ethan, 2026-11-06); bump `Cargo.toml` to match.
3. **Rust CI on GitHub.** Add a `cargo test` + `clippy` job to `ci.yml` on
   `ubuntu-24.04`. The repo is public, so Actions minutes are free. The
   Python job stays until the Python app is removed. If the runner hangs on
   `core::google` `import_names_and_chat_photos` the way the box does,
   skip that test there with `--skip`.
4. **Comments.**
   - The drafts in `phonelink-handoffs/PUBLIC_COMMENT_DRAFTS.md` say "Phone
     Link" and need the new name and the release link.
   - Show both to Ethan before posting.
   - After posting #8 and #6, close their rows in `docs/RUST_PORT.md`.
   - #1: a courtesy line about the release (optional, Ethan's call).
   - #2: nothing until Ethan decides on the Flatpak.

**Done when:** the release page has the archive, a fresh user account on the
laptop can install from it by following the README alone, and both comments
are posted.

**Status 2026-11-06: done.** v0.2.0 released (main = rust-port at the tag),
archive built by `scripts/remote-build.sh release`, CI runs Rust and Go jobs
on ubuntu-24.04, #8 and #6 commented; #1 skipped (Ethan). The install was
checked into a throwaway `$HOME`, not a real second user account. Also done
ahead of B: deletes and read sync through `phonelink-gm serve`.

---

## D. Calls in the chat (reduced)

**What KDE Connect already does** (`plugins/telephony`, `plugins/pausemusic`
in 26.08.3):
- It shows a desktop pop-up for incoming and missed calls, with the phone's
  contact name and photo, and a **Mute Call** button while it rings.
- It pauses or mutes media on the PC during calls (on by default).
- The mute action is internal: it isn't on D-Bus (`sendMutePacket` is a
  private slot), so PhoneRelay can't offer mute itself.
- Only `callReceived(event, number, contactName)` is exposed, and the app
  already uses it to learn names.

A pop-up of our own would just duplicate KDE Connect's. **What's left for the
app** is calls in the conversation timeline:
- Each `ringing` or `missedCall` event becomes a line in that number's chat
  ("Missed call · 3:12 PM"). The chat moves to the top of the list, and a
  missed call counts as unread.
- A missed-call line has a **Text back** button that focuses the compose box.
- Calls are stored in a new `calls` table (device_id, number, event, time).
  The Python app ignores tables it doesn't know.
- **Limit:** only calls that happen while the app or KDE Connect is
  running. KDE Connect can't read the phone's call log, so past calls can't
  be filled in.

Effort: small to medium. **Ethan: yes (2026-11-06).**

---

## E. Phone media controls (reduced)

**What KDE Connect already does:** the `mprisremote` plugin (on by default)
publishes each phone player on the desktop as an MPRIS player
(`org.mpris.MediaPlayer2.kdeconnect.*`). Plasma's media widget, the lock
screen and GNOME's media controls already show the phone's song, with
play/pause, skip and seek.

**What the app could add:** a small "now playing" strip, either in the
header or the notifications sidebar, showing:
- album art (`localAlbumArtUrl`), title and artist
- previous, play/pause and next
- a volume slider
- a player picker when the phone has more than one player

The D-Bus surface (26.08.3, `/modules/kdeconnect/devices/<id>/mprisremote`,
`org.kde.kdeconnect.device.mprisremote`):
- **Properties:** `playerList`, `player` (settable), `isPlaying`, `title`,
  `artist`, `album`, `localAlbumArtUrl`, `volume` (settable), `position`,
  `length`, `canSeek`
- **Methods:** `sendAction(s)` (`PlayPause`, `Play`, `Pause`, `Next`,
  `Previous`, `Stop`), `seek(i)`, `requestPlayerList()`
- **Signal:** `propertiesChanged()`, which carries no arguments, so re-read
  the properties when it fires

Effort: small. Its value is mainly for desktops without an MPRIS widget, so
it's low on Plasma, which Ethan uses. **Skipped (Ethan, 2026-11-06).**

---

## F. Take a photo with the phone (dropped)

KDE Connect 26.08.3 has no `photo` plugin on the desktop side: the plugins
folder has none and no file mentions `requestPhoto`. The app has nothing to
call, so this feature can't be built on KDE Connect. The only other route
is scrcpy's camera source over adb, the same one noted for issue #1, and that's a
separate project.

---

## Decisions (Ethan, 2026-11-06)
1. **D, calls in the chat:** yes, in the reduced form above. Done after C,
   before B.
2. **E, media strip:** skipped.
3. **Release version:** `v0.2.0`.
4. **Order:** A → C → D → B.
