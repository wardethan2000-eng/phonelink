# Phone Link for Linux

A desktop app that brings your Android phone's messages, notifications, and files to your Linux desktop — similar to Microsoft's Phone Link on Windows. Built with Python, GTK4, and Libadwaita, it uses [KDE Connect](https://kdeconnect.kde.org/) as the communication backend.

![Python](https://img.shields.io/badge/python-3.8+-blue)
![GTK](https://img.shields.io/badge/GTK-4-green)
![Libadwaita](https://img.shields.io/badge/Libadwaita-1.4+-purple)
![License](https://img.shields.io/badge/license-MIT-brightgreen)

## Features

- **SMS Conversations** — View all your text message conversations and send/receive SMS from your desktop
- **Contact Names** — Automatically learns names from notifications and calls, with full import from Google Contacts, VCF, or CSV
- **Notifications** — Live phone notifications in a collapsible sidebar tray; click any row to expand and see the full body; dismiss or reply right from the desktop
- **File Browser** — Browse your phone's entire file system over SFTP; navigate folders and open files
- **Photo Grid** — View your most recent camera roll photos as square thumbnails; click to select, right-click to copy or save, double-click to open; supports Ctrl+C
- **Send Files** — Send any file from your PC to the phone with one click
- **Battery & Status** — Phone battery level and connection status always visible in the header bar
- **Ring Phone** — Make your phone ring from the desktop (useful for finding it)
- **Modern UI** — Native GTK4/Libadwaita look with dark mode support and responsive layout

---

## Requirements

- **Linux** with a GTK4-capable desktop (GNOME, Cinnamon, MATE, KDE Plasma, etc.)
- **Python 3.8+**
- **KDE Connect** on your Linux PC (`kdeconnect` package)
- **KDE Connect** app on your Android phone
- GTK4 and Libadwaita GObject introspection bindings

### Tested On

| Component | Version |
|-----------|---------|
| OS | Linux Mint 22 Cinnamon |
| Phone | Samsung Galaxy S25 (Android) |
| KDE Connect (PC) | v23.08.5 |
| GTK4 | 4.14.5 |
| Libadwaita | 1.5.0 |
| Python | 3.12 |

---

## Installation

The core app runs with system packages only. Google Contacts import is optional and needs a few extra Python packages plus a Google OAuth desktop client configuration.

---

### Step 1 — Install system packages on your Linux PC

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

> Without Notification access, the Notifications tray in Phone Link will always be empty, even if the Notification sync plugin is enabled.

#### Special permission: Files (Android 11+)
On Android 11 and newer, "Files and media" in the normal permissions screen only grants partial access. For full file system browsing you may also need:

**Android Settings → Apps → Special app access → All files access → KDE Connect → toggle ON**

> If the Files tab of Phone Link shows "Connecting to phone…" indefinitely, this is almost always the cause.

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

### Step 6 — Clone and run Phone Link

```bash
git clone https://github.com/wardethan2000-eng/phonelink.git
cd phonelink
python3 run.py
```

No `pip install`, no virtual environment, no build step is required for the core app. All core dependencies are the system packages installed in Step 1.

### Optional — Enable native Google Contacts import

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

Then configure a Google OAuth desktop client for Phone Link.

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

## Usage

### Header Bar

The header bar is always visible and shows:

- **Left**: Phone icon, device name, and a coloured dot (green = connected, orange = paired but not reachable, grey = no device)
- **Centre**: Tab switcher (Messages / Files)
- **Right**: Battery icon and percentage, notification bell button, ring phone button

### Messages Tab

- Conversations are listed on the left, sorted by most recent
- Click a conversation to open the full message thread on the right
- Type in the compose bar at the bottom and press **Enter** or click **Send**
- Click **+** to start a new conversation with any phone number
- **Right-click** a conversation → **Set contact name** to assign a display name

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

- Shows up to 200 most recent photos from your phone's camera and Pictures folders as square thumbnails
- **Click** a tile to select it (blue checkmark appears); click again to deselect
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

In the Messages tab, click the **contacts icon** next to the search bar → **Import Google Contacts**.

On first use:

1. Phone Link opens your browser
2. You sign in to Google
3. You grant read-only contacts access
4. Phone Link imports your contacts automatically

After the first authorization, later imports can reuse the saved Google token and run with one click.

You can also manage the connection from **Preferences → Google Contacts**. That page shows the connected account, lets you refresh or disconnect it, and controls a low-volume background refresh that runs at most once every 24 hours.

When Google contact photos are available, Phone Link caches them locally and shows them for contacts that already appear in your active conversations.

### Option E: Set a name manually

Right-click any conversation → **Set contact name**.

### Where contacts are stored

`~/.local/share/phonelink/contacts.json` — a simple JSON mapping phone number digits to display names:

```json
{
  "13165551234": "John Smith",
  "13165559999": "Jane Doe"
}
```

For Google Contacts import, OAuth tokens are stored separately at `~/.local/share/phonelink/google_token.json`.

---

## Keyboard Shortcuts

| Shortcut | Where | Action |
|----------|-------|--------|
| `Enter` | Messages compose bar | Send message |
| `Ctrl+C` | Files → Recent Photos | Copy selected photos to clipboard |
| `Ctrl+Q` | Anywhere | Quit the app |

---

## Troubleshooting

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
├── run.py                          # Entry point
├── pyproject.toml                  # Project metadata
├── data/                           # Static assets (icons, desktop file, CSS)
├── phonelink/
│   ├── app.py                      # Adw.Application subclass, startup logic
│   ├── contacts.py                 # Contact name resolver (JSON, VCF, CSV, notifications)
│   ├── dbus_client.py              # KDE Connect D-Bus API wrapper
│   ├── models.py                   # Data models: Device, SmsMessage, Conversation, Notification
│   └── ui/
│       ├── main_window.py          # Main window — header bar, tab stack, notification tray
│       ├── sms_panel.py            # Messages tab (conversation list + message thread)
│       ├── conversation_list.py    # Conversation list widget
│       ├── message_thread.py       # Chat bubble thread view
│       ├── notifications_panel.py  # Notification tray — compact expandable list
│       └── files_panel.py          # Files tab — photo grid + SFTP file browser
```

---

## How It Works

Phone Link for Linux is a **thin frontend** — it does not implement any phone protocol itself. It talks to the [KDE Connect](https://kdeconnect.kde.org/) daemon over **D-Bus**:

```
┌─────────────┐     D-Bus      ┌──────────────┐    TCP/TLS    ┌─────────────┐
│  Phone Link │ ◄────────────► │ kdeconnectd  │ ◄───────────► │  KDE Connect│
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

## Roadmap

- [x] Phase 1: Foundation, D-Bus connection, device pairing
- [x] Phase 2: SMS conversations — view, send, contact names, VCF/CSV import
- [x] Phase 3: Live notifications tray with dismiss and reply
- [x] Phase 4: File browser (SFTP), photo grid, send/receive files, copy/save
- [ ] Phase 5: Desktop integration — autostart, system tray icon, desktop notifications

---

## License

MIT — see [LICENSE](LICENSE).
