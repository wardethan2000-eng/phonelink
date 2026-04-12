# Phone Link for Linux

A desktop app that brings your Android phone's messages, notifications, and files to your Linux desktop — similar to Microsoft's Phone Link on Windows. Built with Python, GTK4, and Libadwaita, it uses [KDE Connect](https://kdeconnect.kde.org/) as the communication backend.

![Python](https://img.shields.io/badge/python-3.8+-blue)
![GTK](https://img.shields.io/badge/GTK-4-green)
![Libadwaita](https://img.shields.io/badge/Libadwaita-1.4+-purple)
![License](https://img.shields.io/badge/license-MIT-brightgreen)

## Features

- **SMS Conversations** — View all your text message conversations and send/receive SMS from your desktop
- **Contact Names** — Automatically learns contact names from incoming notifications, with manual import from VCF or CSV files
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

### 1. Install system dependencies

**Debian / Ubuntu / Linux Mint:**

```bash
sudo apt install kdeconnect python3-gi gir1.2-gtk-4.0 gir1.2-adw-1
```

**Fedora:**

```bash
sudo dnf install kdeconnect python3-gobject gtk4 libadwaita
```

**Arch Linux:**

```bash
sudo pacman -S kdeconnect python-gobject gtk4 libadwaita
```

### 2. Install KDE Connect on your Android phone

Download from [Google Play](https://play.google.com/store/apps/details?id=org.kde.kdeconnect_tp) or [F-Droid](https://f-droid.org/packages/org.kde.kdeconnect_tp/).

### 3. Pair your phone

1. Make sure your phone and PC are on the **same Wi-Fi network**
2. Open KDE Connect on your phone — your PC should appear automatically
3. Tap to **pair** and accept on both devices
4. In the KDE Connect Android app, open the paired device settings and enable:
   - **SMS** — required for the Messages tab
   - **Notification sync** — required for the Notifications tray (also enables automatic contact name discovery)
   - **Multimedia control / SFTP** — required for the Files tab
   - **Share** — required for sending files and receiving VCF contacts

### 4. Clone and run

```bash
git clone https://github.com/wardethan2000-eng/phonelink.git
cd phonelink
python3 run.py
```

No pip install or virtual environment needed. All dependencies are system packages.

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

### Option D: Set a name manually

Right-click any conversation → **Set contact name**.

### Where contacts are stored

`~/.local/share/phonelink/contacts.json` — a simple JSON mapping phone number digits to display names:

```json
{
  "13165551234": "John Smith",
  "13165559999": "Jane Doe"
}
```

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
- Grant KDE Connect **Notification access** on Android (Settings → Apps → Special app access → Notification access)

### Contact names not showing

- See [Importing Contacts](#importing-contacts)
- The fastest first-time fix is to share all contacts from the Contacts app via KDE Connect (Option B above)

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
