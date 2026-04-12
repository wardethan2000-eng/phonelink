# Phone Link for Linux

A desktop app that brings your Android phone's text messages, notifications, and files to your Linux desktop — similar to Microsoft's Phone Link on Windows. Built with Python, GTK4, and Libadwaita, it uses [KDE Connect](https://kdeconnect.kde.org/) as the communication backend.

![Python](https://img.shields.io/badge/python-3.10+-blue)
![GTK](https://img.shields.io/badge/GTK-4-green)
![License](https://img.shields.io/badge/license-MIT-brightgreen)

## Features

- **SMS Conversations** — View all your text message conversations and send/receive SMS from your desktop
- **Contact Names** — Automatically learns contact names from phone notifications, with manual import from VCF or CSV files
- **Device Sidebar** — See your phone's battery level, connection status, and device info at a glance
- **Ring Phone** — Lost your phone? Ring it from your desktop
- **Modern UI** — Native GNOME/GTK4 look with Libadwaita, dark mode support, and responsive layout

> **Status:** SMS is fully functional. Notifications panel and file browser are coming in future updates.

## Screenshots

*Coming soon*

---

## Requirements

- **Linux** with a GTK4-capable desktop (GNOME, Cinnamon, MATE, etc.)
- **Python 3.10+**
- **KDE Connect** installed on both your Linux PC and Android phone
- **GTK4** and **Libadwaita** GObject introspection bindings

### Tested On

- Linux Mint 22 Cinnamon
- Galaxy S25 (Android)
- KDE Connect v23.08.5

---

## Installation

### 1. Install system dependencies

**Debian / Ubuntu / Linux Mint:**

```bash
sudo apt install kdeconnect python3-gi gir1.2-gtk-4.0 gir1.2-adw-1
```

**Fedora:**

```bash
sudo dnf install kdeconnect kde-connect python3-gobject gtk4 libadwaita
```

**Arch Linux:**

```bash
sudo pacman -S kdeconnect python-gobject gtk4 libadwaita
```

### 2. Install KDE Connect on your Android phone

Download [KDE Connect from Google Play](https://play.google.com/store/apps/details?id=org.kde.kdeconnect_tp) or [F-Droid](https://f-droid.org/packages/org.kde.kdeconnect_tp/).

### 3. Pair your phone

1. Make sure your phone and PC are on the **same Wi-Fi network**
2. Open KDE Connect on your phone — your PC should appear
3. Tap to **pair** and accept on both devices
4. In the KDE Connect Android app, go to the paired device and enable these plugins:
   - **SMS** (required for messaging)
   - **Contacts** (optional)
   - **Notification sync** (recommended — enables automatic contact name discovery)
   - **Share** (recommended — enables VCF contact import)

### 4. Clone and run the app

```bash
git clone https://github.com/wardethan2000-eng/phonelink.git
cd phonelink
python3 run.py
```

That's it — no pip install or virtual environment needed. All dependencies are system packages.

---

## Importing Contacts

Phone Link for Linux can learn contact names in several ways. They're listed below from easiest to most manual.

### Option A: Automatic from notifications (zero effort)

Every time someone texts you, Android includes their contact name in the notification. The app captures this automatically and saves it. Over time, all your active contacts will be named. **This happens with no action from you.**

### Option B: Share contacts from your phone (recommended for first setup)

This imports **all** your contacts at once:

1. On your Android phone, open the **Contacts** app
2. Tap the **⋮** menu (three dots) → **Share**
3. Select **all contacts** (or tap "Select all")
4. Choose **Share as VCF** (or just "Share")
5. In the share sheet, pick **KDE Connect → your PC name**

The app detects the incoming `.vcf` file and imports every contact automatically. You'll see a confirmation dialog with the count.

If the app isn't running when you share, the VCF file lands in `~/Downloads/`. Next time you start the app, it checks that folder and imports it automatically.

### Option C: Import a file manually

Click the **👥** (people) icon next to the search bar in the Messages tab, then click **"Import File…"**. You can import:

- **VCF files** — Standard vCard format, exported from any contacts app
- **Google Contacts CSV** — Go to [contacts.google.com](https://contacts.google.com), select all contacts, click **Export → Google CSV**, download the file, and import it

### Option D: Set names individually

Right-click any conversation in the list → **"Set contact name"** to manually assign a name to that number.

### Where are contacts stored?

Contact names are saved to `~/.local/share/phonelink/contacts.json`. This file persists across app restarts. You can edit it directly if you want — it's a simple JSON object mapping phone number digits to names:

```json
{
  "13165551234": "John Smith",
  "13165559999": "Jane Doe"
}
```

---

## Usage

### Running the app

```bash
cd phonelink
python3 run.py
```

### Messages tab

- Your conversations appear in the left panel, sorted by most recent
- Click a conversation to view the full message thread
- Type in the compose bar at the bottom and press Enter or click Send
- Click **+** to compose a new message to a phone number
- **Right-click** a conversation to rename the contact

### Ring phone

Click the **phone icon** in the header bar to make your phone ring (useful for finding it).

### Keyboard shortcuts

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Ctrl+Q` | Quit |

---

## Troubleshooting

### "No device connected" / phone not appearing

- Make sure your phone and PC are on the **same Wi-Fi network**
- Check that `kdeconnectd` is running: `pgrep kdeconnectd`
- If not running: `kdeconnectd &` or restart it from your system tray
- Re-pair if needed: `kdeconnect-cli --pair --device <DEVICE_ID>`
- List available devices: `kdeconnect-cli --list-available`

### Messages not loading

- In the KDE Connect Android app, make sure the **SMS** plugin is enabled
- Grant KDE Connect the **SMS/MMS** permission on Android
- The app loads cached messages from the KDE Connect daemon — give it a few seconds on first launch

### Texts not sending

- Verify KDE Connect can send SMS: `kdeconnect-cli --send-sms "test" --destination "+1234567890" --device <DEVICE_ID>`
- Check that your phone has cellular signal

### Contact names not showing

- See the [Importing Contacts](#importing-contacts) section above
- The easiest option is sharing contacts from your phone via KDE Connect (Option B)
- Names from notifications are learned automatically over time

### App crashes on fullscreen

This was a known issue that has been fixed. If you experience it, make sure you have the latest version.

---

## Project Structure

```
phonelink/
├── run.py                          # Entry point
├── pyproject.toml                  # Project metadata
├── phonelink/
│   ├── __init__.py
│   ├── app.py                      # GTK Application class
│   ├── contacts.py                 # Contact name resolution (JSON, VCF, CSV, notifications)
│   ├── dbus_client.py              # KDE Connect D-Bus API wrapper
│   ├── models.py                   # Data models (Device, SmsMessage, Conversation)
│   ├── style.css                   # Custom GTK CSS
│   └── ui/
│       ├── __init__.py
│       ├── main_window.py          # Main window with sidebar + tab panels
│       ├── device_sidebar.py       # Phone info sidebar
│       ├── conversation_list.py    # SMS conversation list widget
│       ├── message_thread.py       # Chat bubble message view
│       ├── sms_panel.py            # SMS panel orchestrator
│       ├── notifications_panel.py  # Notifications (placeholder)
│       └── files_panel.py          # File browser (placeholder)
```

---

## How It Works

Phone Link for Linux is a **frontend only** — it doesn't implement any phone communication protocol itself. Instead, it talks to the [KDE Connect](https://kdeconnect.kde.org/) daemon (`kdeconnectd`) over **D-Bus**, the standard Linux inter-process communication system.

```
┌─────────────┐     D-Bus      ┌──────────────┐    TCP/TLS    ┌─────────────┐
│  Phone Link │ ◄────────────► │ kdeconnectd  │ ◄───────────► │  KDE Connect│
│  (this app) │  session bus   │  (daemon)    │   Wi-Fi       │  (Android)  │
└─────────────┘                └──────────────┘               └─────────────┘
```

- **SMS**: Uses the `org.kde.kdeconnect.device.conversations` D-Bus interface
- **Contacts**: Harvests names from Android SMS notifications (`org.kde.kdeconnect.device.notifications`) and supports manual VCF/CSV import
- **Battery/Status**: Reads from `org.kde.kdeconnect.device.battery` properties

---

## Roadmap

- [x] Phase 1: Foundation & device connection
- [x] Phase 2: SMS conversations (view + send + contacts)
- [ ] Phase 3: Notifications panel
- [ ] Phase 4: File browser & transfer
- [ ] Phase 5: Polish & desktop integration (autostart, system tray, desktop notifications)

---

## License

MIT — see [LICENSE](LICENSE).
