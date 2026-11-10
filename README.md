# PhoneRelay (formerly Phone Link for Linux)

PhoneRelay brings your Android phone's texts, notifications and files to your Linux desktop, much like Microsoft's Phone Link on Windows. It is a GTK 4 / libadwaita app and uses [KDE Connect](https://kdeconnect.kde.org/) to talk to the phone.

![GTK](https://img.shields.io/badge/GTK-4.10+-green)
![Libadwaita](https://img.shields.io/badge/Libadwaita-1.4+-purple)
![License](https://img.shields.io/badge/license-MIT-brightgreen)

## Features

- **Texts** — every conversation from the phone; send texts and pictures from the desktop, with capitals at sentence starts and for names as you type (each can be turned off)
- **Search** — finds a word in any message, however old; opening a result jumps to that message. **Ctrl+F** searches inside one chat
- **Contact names** — learned from notifications and calls, or imported from Google Contacts (one click: **Settings** (gear button) **→ Google Contacts → Connect**), a VCF or a CSV
- **Notifications** — live phone notifications in a side tray; expand, dismiss or reply from the desktop
- **Files** — browse the phone's storage, see recent photos as a grid or list grouped by day, copy, save, or send files to the phone
- **Calls** — a phone button in the header counts missed calls and shows who is calling while the phone rings; click it for recent calls and to text the caller back
- **Battery and status** in the header bar; **ring the phone** to find it
- **Tray icon** and **Open on Login**
- **Optional Google Messages link** — texts go as RCS where the chat is RCS, with reactions, replies, typing, read receipts, pictures, voice and video, groups, and archive/mute/pin; chats delete and read/unread stay in step with the phone (see [Google Messages link](#optional-google-messages-link))
- **Size** setting for the whole window (Settings → Appearance), and Ctrl+scroll to zoom the messages

---

## Requirements

- **GTK 4.10+** and **libadwaita 1.4+**. The app checks at startup and says plainly if yours are older.
- **glibc 2.39+** for the ready-made download (build from source on older systems).
- **KDE Connect** on the PC (`kdeconnect` package) and the **KDE Connect** app on your Android phone.

| Works | Too old (GTK or libadwaita) |
|---|---|
| Fedora 40 and newer | Ubuntu 22.04 |
| Ubuntu 24.04 and newer | Linux Mint 21 |
| Linux Mint 22 and newer | Debian 12 |
| Debian 13, Arch, openSUSE Tumbleweed | |

Tested on Fedora 44 (KDE Plasma) with a Samsung Galaxy S25.

---

## Install

### Step 1 — Install KDE Connect on your PC

```bash
# Fedora
sudo dnf install kdeconnect
# Debian / Ubuntu / Mint
sudo apt install kdeconnect
# Arch / Manjaro
sudo pacman -S kdeconnect
# openSUSE
sudo zypper install kdeconnect-kde
```

Check it runs: `kdeconnect-cli --list-available`. If that errors, start the daemon with `kdeconnectd &`.

### Step 1b — Install PhoneRelay

Download `phonerelay-<version>-x86_64.tar.gz` and its `.sha256` file from the [latest release](https://github.com/wardethan2000-eng/phonelink/releases/latest), then:

```bash
sha256sum -c phonerelay-*-x86_64.tar.gz.sha256   # must say OK
tar xzf phonerelay-*-x86_64.tar.gz
cd phonerelay-*/
./data/install-rust.sh ./phonelink-rs
phonelink-rs
```

This puts `phonelink-rs` (and the optional Google Messages helper, `phonelink-gm`) in `~/.local/bin`, and adds a menu entry and icons. Nothing needs root. Turn on **Settings → Open on Login** to start it at login.

To update, download the new release and run the same two lines again. Quit PhoneRelay first (tray icon → Quit), or the copy into `~/.local/bin` can fail with "Text file busy".

**Build from source instead** (needs Rust 1.98+):

```bash
# Fedora
sudo dnf install kdeconnect gtk4-devel libadwaita-devel gcc
# Debian / Ubuntu / Mint
sudo apt install kdeconnect libgtk-4-dev libadwaita-1-dev build-essential

git clone https://github.com/wardethan2000-eng/phonelink.git
cd phonelink
cargo build --release
data/install-rust.sh target/release/phonelink-rs
```

A source build has no built-in Google client for Contacts import; see [Using your own Google client](#using-your-own-google-client).

### Step 2 — Install KDE Connect on your Android phone

Install the **KDE Connect** app on your Android phone from one of these sources:

- [Google Play Store](https://play.google.com/store/apps/details?id=org.kde.kdeconnect_tp)
- [F-Droid](https://f-droid.org/packages/org.kde.kdeconnect_tp/) (open-source store, no Google account needed)

When you first open the app, Android will prompt you to grant permissions. **Grant all of them.** If you accidentally denied any, fix them in the next step.

---

### Step 3 — Grant Android permissions to KDE Connect

This is the most important setup step. KDE Connect needs several Android system permissions to function. Without them, features will be missing or broken.

#### Normal app permissions
Go to: **Android Settings → Apps → KDE Connect → Permissions**

| Permission | Grant it for… |
|-----------|--------------|
| **SMS** (Send and view SMS messages) | Reading your conversation history and sending texts from your PC |
| **Contacts** | Looking up contact names |
| **Files and media** (or "All files access" on Android 11+) | Browsing your phone's file system and viewing photos in the Files tab |
| **Phone** | Optional — some features may use this |

#### Special permission: Notification access
This permission is **not** in the normal Permissions screen. You must find it separately:

**Android Settings → Apps → Special app access → Notification access → KDE Connect → toggle ON**

> Without Notification access, the Notifications tray in PhoneRelay will always be empty, even if the Notification sync plugin is enabled.

#### Special permission: Files (Android 11+)
On Android 11 and newer, "Files and media" in the normal permissions screen only grants partial access. For full file system browsing you may also need:

**Android Settings → Apps → Special app access → All files access → KDE Connect → toggle ON**

> If the Files tab of PhoneRelay shows "Connecting to phone…" indefinitely, this is almost always the cause.

---

### Step 4 — Pair your phone with your PC

1. Make sure your phone and PC are on the **same Wi-Fi network**
2. Open KDE Connect on your Android phone
3. Your PC should appear in the device list — tap it
4. Tap **Pair** — a pairing request notification will appear on your PC; click **Accept**
5. The device status should now show **"Paired and reachable"**

If your PC does not appear on the phone, try:
```bash
# On your PC — list discovered devices
kdeconnect-cli --list-available

# Refresh discovery
kdeconnect-cli --refresh
```

---

### Step 5 — Enable KDE Connect plugins for your PC

After pairing, open the paired device in the KDE Connect Android app, go to its settings, and confirm these plugins are enabled:

| Plugin name (on Android) | Required for |
|--------------------------|-------------|
| **SMS** | Messages tab |
| **Notification sync** | Notifications tray |
| **SFTP / Expose filesystem** | File browser and photo grid |
| **Share** | Sending files to phone; receiving `.vcf` contacts |
| **Battery report** | Battery level in header bar (usually on by default) |
| **Find my phone** | Ring phone button (usually on by default) |

---

### Step 6 — Open PhoneRelay

Start **PhoneRelay** from the menu (or run `phonelink-rs`). Your phone should appear in the header bar within a few seconds, and the Messages tab fills in.

---

## Optional: Google Messages link

KDE Connect sends texts as plain SMS and can't see RCS features. If your phone uses Google Messages, PhoneRelay can run the Messages tab through a Google Messages "for web" connection instead:

- **RCS:** texts go as RCS where the chat is RCS (SMS otherwise), shown as Sending → Delivered → Read.
- **Reactions and replies:** react to a message or reply to it; others' reactions and quoted replies show in the chat.
- **Typing** shows both ways; **read receipts** are sent only while PhoneRelay's window is in front.
- **Pictures, voice messages, videos and files**, several at once (paperclip, drag and drop, or Ctrl+V); full-size pictures; iPhone photos show as Google's JPEG copy.
- **New chats and groups**, older messages from the phone, and **archive, mute, pin, block and report spam**.
- **Delete a chat** and it is deleted on the phone too; **opening a chat** marks it read on the phone, and reading one on the phone clears it here.

This runs through a small helper, `phonelink-gm`, installed next to the app. It is AGPL-3.0 because it uses [libgm](https://github.com/mautrix/gmessages) (see `phonelink-gm/LICENSE`); the app itself stays MIT and only runs the helper as a separate program.

To link, open **Settings → Google Messages → Link**. A sign-in window opens in Chrome, Chromium, Brave, Edge or Vivaldi (a throwaway profile, not yours); sign in with the Google account Google Messages uses on the phone, then tap the emoji the dialog shows on the phone. **Unlink** there removes the link, on the phone too when it can be reached. An existing gmcli session (`~/.local/state/gmcli/session.json`) is picked up once.

Limits: it's an unofficial protocol, so Google can break it until the helper is updated. The phone must be online. Google drops a link unused for about 14 days. Google Messages allows one web session at a time, so opening messages.google.com in a browser takes the link away from PhoneRelay.

---

## Usage

### Header Bar

The header bar is always visible and shows:

- **Left**: Phone icon, device name, and a coloured dot (green = connected, orange = paired but not reachable, grey = no device)
- **Centre**: Tab switcher (Messages / Files / Clipboard)
- **Right**: Battery icon and percentage, ring phone button, notification bell button, calls button (missed calls count; the caller's name while the phone rings)

### Messages Tab

- Conversations are listed on the left, sorted by most recent
- Click a conversation to open the full message thread on the right, ready to type
- Type in the compose bar at the bottom and press **Enter** or click **Send**; a half-typed message stays with its own chat when you switch
- Capitals go in as you type: the start of each sentence, and proper nouns ("I", your contacts' names, days, months, holidays, some places and brands). **Ctrl+Z** right after one keeps the letter as typed; **Settings → General → Typing** turns either off
- Click **+** to start a new conversation with any phone number
- **Right-click** a conversation → **Set contact name** to assign a display name, or **Delete**
- Type in the search box to find any message; **Ctrl+F** finds text inside the open chat
- Paste a picture with **Ctrl+V** to send it

### Notifications Tray

- Click the **bell icon** in the header bar to slide in the notifications panel from the right
- Each notification shows the app icon, app name, timestamp, title, and a body snippet
- **Click a row** to expand it in place — shows the full body text, a **Dismiss** button, and a **Reply** field (for apps that support replies, e.g. SMS, WhatsApp)
- The **refresh** button (top of tray) reloads notifications from the phone
- The **clear all** button dismisses every dismissable notification at once
- Click the bell button again (or press it a second time) to close the tray

### Files Tab

The Files tab has two views, switchable at the top:

#### Recent Photos

- Shows up to 200 most recent photos from your phone's camera and Pictures folders, as a **grid** or a **list** (with time and size), **newest or oldest first**, optionally **grouped by day**; the view is remembered
- Thumbnails come from each photo's built-in preview and are kept on the computer, so they load quickly and each photo is fetched only once
- **Click** a tile to select it (blue checkmark appears); click again to deselect
- **Drag** across photos to select a run of them, from the first to the last you pass over (it scrolls at the edges)
- **Double-click** to open the photo in your default image viewer
- **Right-click** → **Open / Copy / Save As…**
- **Ctrl+C** copies selected photos to the clipboard (single image → image data; multiple → file URIs)
- Toolbar buttons: **Select All**, **Save selected to folder**, **Copy selected to clipboard**

#### Browse Files

- Navigate your phone's full file system
- Click **folders** to open them; click **files** to open them in the default app
- Use the **← back** button or the breadcrumb path label to navigate up
- **Right-click** any file → **Open / Copy / Save As…**
- **Right-click** any folder → **Open**

#### Sidebar (Files Tab)

- **Locations** list: Recent Photos, phone storage directories (Camera, Downloads, etc.), and local Downloads folder
- **Send File to Phone** button — opens a file picker and sends the chosen file(s) to the phone via KDE Connect Share

---

## Importing Contacts

Contact names appear in the Messages tab next to each conversation. The app resolves names in several ways:

### Option A: Automatic (no action required)

When someone sends you a text, Android includes their name in the notification. The app captures this automatically and persists it. All active contacts will appear named over time.

### Option B: Share from your phone (recommended for first-time setup)

1. Open the **Contacts** app on Android
2. Tap **⋮ → Share → Select all → Share as VCF**
3. In the share sheet, choose **KDE Connect → your PC**

The app imports every contact immediately. If the app is not running at the time, the `.vcf` file is saved to `~/Downloads/` and imported automatically the next time the app launches.

### Option C: Import a file manually

In the Messages tab, click the **contacts icon** next to the search bar → **Import File…**. Supported formats:

- **VCF** — exported from any Android contacts app
- **Google Contacts CSV** — go to [contacts.google.com](https://contacts.google.com), select all, **Export → Google CSV**

### Option D: Import Google Contacts directly

Open **Settings → Google Contacts → Connect**.

On first use:

1. PhoneRelay opens your browser
2. You sign in to Google
3. You grant read-only contacts access
4. PhoneRelay imports your contacts automatically

After the first authorization, later imports can reuse the saved Google token and run with one click.

You can also manage the connection from **Settings → Google Contacts**. That page shows the connected account, lets you refresh or disconnect it, and controls a low-volume background refresh that runs at most once every 24 hours.

When Google contact photos are available, PhoneRelay caches them locally and shows them for contacts that already appear in your active conversations.

### Option E: Set a name manually

Right-click any conversation → **Set contact name**.

### Where contacts are stored

In `~/.local/share/phonelink/messages.db` (a SQLite file, shared with the retired Python version). For Google Contacts import, OAuth tokens are stored separately at `~/.local/share/phonelink/google_token.json`.

---

### Using your own Google client

Release downloads have PhoneRelay's Google client built in. A source build (or anyone who prefers their own Google Cloud project) can use a Desktop OAuth client instead:

Save it as `~/.config/phonelink/google_oauth.json`:

```json
{
  "installed": {
    "client_id": "your-google-oauth-client-id",
    "client_secret": "your-google-oauth-client-secret",
    "auth_uri": "https://accounts.google.com/o/oauth2/auth",
    "token_uri": "https://oauth2.googleapis.com/token",
    "redirect_uris": ["http://127.0.0.1", "http://localhost"]
  }
}
```

Or set `PHONELINK_GOOGLE_CLIENT_ID` and `PHONELINK_GOOGLE_CLIENT_SECRET`.

---

## Keyboard Shortcuts

| Shortcut | Where | Action |
|----------|-------|--------|
| `Enter` | Messages compose bar | Send message |
| `Ctrl+V` | Messages compose bar | Paste a picture to send |
| `Ctrl+F` | Open chat | Find text in this chat (`Enter` / `Shift+Enter` step through matches, `Esc` closes) |
| `Ctrl+N` | Anywhere | Show or hide the notifications tray |
| `Ctrl+plus` / `Ctrl+minus` / `Ctrl+0` | Open chat | Text size |
| `Ctrl+C` | Files → Recent Photos | Copy selected photos to clipboard |

---

## Troubleshooting

### "GTK or libadwaita is too old" at startup

Your distro's GTK 4 or libadwaita is older than PhoneRelay needs (see [Requirements](#requirements)). Ubuntu 22.04, Mint 21 and Debian 12 are too old; upgrading the distro is the fix.

### "No device connected" / phone not appearing

- Make sure your phone and PC are on the **same Wi-Fi network**
- Check that `kdeconnectd` is running: `pgrep kdeconnectd`
- If not running: `kdeconnectd &` or relaunch it from the system tray
- List available devices: `kdeconnect-cli --list-available`
- Re-pair if needed: open KDE Connect on the phone, remove the PC, and pair again

### Messages not loading

- Make sure the **SMS** plugin is enabled in KDE Connect on Android
- Grant KDE Connect **SMS/MMS** permission in Android Settings → Apps → KDE Connect → Permissions
- The app fetches messages from the KDE Connect daemon cache — wait a few seconds on first launch

### Texts not sending

- Check KDE Connect can send SMS: `kdeconnect-cli --send-sms "test" --destination "+1234567890" --device <ID>`
- Confirm your phone has cellular signal and SMS is not blocked

### Files tab blank / "Connecting to phone…"

- The Files tab mounts your phone over SFTP automatically when you connect
- If mounting fails, grant KDE Connect **Files and media** permission in Android Settings → Apps → KDE Connect → Permissions
- You can still send files to the phone with **Send File to Phone** even if mounting fails

### Photo thumbnails not loading

- Thumbnails are loaded from the SFTP mount — verify the mount succeeded (status bar in file browser should show folder counts, not "Connecting…")
- If you see "No photos found", check that `DCIM/Camera` or `Pictures` exists on your phone

### Notifications tray is blank

- Make sure **Notification sync** is enabled in the KDE Connect Android app for your paired device
- This feature requires a **special permission** that is NOT in the normal app permissions screen:
  Android Settings → Apps → Special app access → Notification access → find KDE Connect → toggle ON
- Without this, KDE Connect cannot read your phone's notifications and the tray will always be empty

### Contact names not showing

- See [Importing Contacts](#importing-contacts)
- If KDE Connect desktop contact sync is empty, the app now warns about it and suggests a fallback import path
- The fastest first-time fix is to use **Import Google Contacts** or share all contacts from the Contacts app via KDE Connect (Option B above)

---

## Project Structure

```
phonelink/
├── phonelink-rs/        # The app (Rust): core/ logic, dbus/ KDE Connect client, ui/ GTK
├── phonelink-gm/        # Google Messages helper (Go, AGPL-3.0)
├── data/                # Icons, desktop file, install-rust.sh
├── docs/                # Design notes and plans
├── scripts/             # Build and test helpers
└── phonelink/, run.py   # The retired Python version
```

---

## How It Works

PhoneRelay is a **thin frontend** — it does not implement any phone protocol itself. It talks to the [KDE Connect](https://kdeconnect.kde.org/) daemon over **D-Bus**:

```
┌─────────────┐     D-Bus      ┌──────────────┐    TCP/TLS    ┌─────────────┐
│  PhoneRelay │ ◄────────────► │ kdeconnectd  │ ◄───────────► │  KDE Connect│
│  (this app) │  session bus   │  (daemon)    │   Wi-Fi       │  (Android)  │
└─────────────┘                └──────────────┘               └─────────────┘
```

| Feature | D-Bus Interface |
|---------|----------------|
| SMS send/receive | `org.kde.kdeconnect.device.conversations` |
| Notifications | `org.kde.kdeconnect.device.notifications` |
| File system (SFTP) | `org.kde.kdeconnect.device.sftp` |
| Send file | `org.kde.kdeconnect.device.share` |
| Battery | `org.kde.kdeconnect.device.battery` |
| Ring phone | `org.kde.kdeconnect.device.findmyphone` |

---

## Python version (retired)

PhoneRelay started as a Python app (`phonelink/`, `run.py`). The Rust app replaces it and reads the same messages, contacts and settings, so you can switch over without losing anything. Quit the Python app before starting the Rust one: they share the message store, and the Rust app refuses to start while the Python one is open. The Python version gets no new features and will be removed later; these steps are kept for anyone still using it.

### Installing the Python version

The core app runs with system packages only. Google Contacts import is optional and needs a few extra Python packages plus a Google OAuth desktop client configuration.

---

#### System packages

These packages provide Python, GTK4, Libadwaita, and the KDE Connect daemon. Install the group for your distro:

**Debian / Ubuntu / Linux Mint:**
```bash
sudo apt update
sudo apt install kdeconnect python3-gi python3-gi-cairo gir1.2-gtk-4.0 gir1.2-adw-1
```

**Fedora:**
```bash
sudo dnf install kdeconnect python3-gobject gtk4 libadwaita
```

**Arch Linux / Manjaro:**
```bash
sudo pacman -S kdeconnect python-gobject gtk4 libadwaita
```

**openSUSE:**
```bash
sudo zypper install kdeconnect-kde python3-gobject typelib-1_0-Gtk-4_0 typelib-1_0-Adw-1
```

What each package does:

| Package | Purpose |
|---------|---------|
| `kdeconnect` | The KDE Connect daemon (`kdeconnectd`) — handles all phone communication |
| `python3-gi` | Python GObject Introspection bindings — lets Python talk to GTK |
| `gir1.2-gtk-4.0` | GTK4 introspection data |
| `gir1.2-adw-1` | Libadwaita introspection data (modern GNOME UI widgets) |

After installing, verify KDE Connect is available:
```bash
kdeconnect-cli --list-available
```
If that command errors, start the daemon manually: `kdeconnectd &`

---

#### Clone and run

```bash
git clone https://github.com/wardethan2000-eng/phonelink.git
cd phonelink
./install.sh
phonelink
```

The installer creates a local launcher, installs the desktop entry/icons, and refreshes desktop caches so PhoneRelay can appear in application search. You can still run a development checkout directly with `python3 run.py`.

#### Optional — Google Contacts import (Python)

If you want the in-app **Import Google Contacts** button to work, install the Google API packages.

On Debian / Ubuntu / Linux Mint, prefer distro packages because the system Python is externally managed:

```bash
sudo apt install python3-googleapi python3-google-auth python3-google-auth-oauthlib python3-google-auth-httplib2
```

If you prefer a virtual environment, create one that can still see the system GTK bindings:

```bash
python3 -m venv --system-site-packages .venv
. .venv/bin/activate
pip install google-auth-oauthlib google-api-python-client google-auth-httplib2
```

Then run the app from that virtualenv:

```bash
.venv/bin/python run.py
```

If you are not on Debian-based Linux, you can use pip directly in a virtualenv, or install equivalent distro packages if your distro provides them.

```bash
python3 -m pip install google-auth-oauthlib google-api-python-client google-auth-httplib2
```

Then configure a Google OAuth desktop client for PhoneRelay.

Option A: environment variables

```bash
export PHONELINK_GOOGLE_CLIENT_ID="your-google-oauth-client-id"
export PHONELINK_GOOGLE_CLIENT_SECRET="your-google-oauth-client-secret"
```

Option B: config file at `~/.config/phonelink/google_oauth.json`

```json
{
  "installed": {
    "client_id": "your-google-oauth-client-id",
    "client_secret": "your-google-oauth-client-secret",
    "auth_uri": "https://accounts.google.com/o/oauth2/auth",
    "token_uri": "https://oauth2.googleapis.com/token",
    "redirect_uris": [
      "http://127.0.0.1",
      "http://localhost"
    ]
  }
}
```

Once configured, the app opens your browser for Google sign-in on the first import and reuses the saved token on later imports.

If the app starts and shows a **"KDE Connect Not Found"** error, the daemon is not running. Start it:
```bash
kdeconnectd &
```
Then relaunch the app.

---


---

## License

MIT — see [LICENSE](LICENSE). The Google Messages helper in `phonelink-gm/` is AGPL-3.0-or-later (see [phonelink-gm/LICENSE](phonelink-gm/LICENSE)).
