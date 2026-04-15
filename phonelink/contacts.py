"""Contact name resolution — local JSON store + KDE Connect vCard cache + CSV import."""

import csv
import io
import json
import os
import re
import unicodedata
from pathlib import Path

# KDE Connect stores synced vCards here
VCARD_BASE = Path.home() / ".local" / "share" / "kpeoplevcard"

# Our own local contacts store
CONTACTS_DIR = Path.home() / ".local" / "share" / "phonelink"
CONTACTS_FILE = CONTACTS_DIR / "contacts.json"
CONTACT_PHOTOS_DIR = CONTACTS_DIR / "contact_photos"


def _normalize_phone(number: str) -> str:
    """Strip a phone number to digits only for comparison."""
    return re.sub(r"[^\d]", "", number)


def _clean_text(text: str) -> str:
    """Strip surrounding whitespace and invisible formatting marks."""
    if not text:
        return ""
    cleaned = "".join(ch for ch in str(text) if unicodedata.category(ch) != "Cf")
    return cleaned.strip()


def _phone_key(number: str) -> str:
    norm = _normalize_phone(number)
    return norm[-10:] if len(norm) >= 10 else norm


def _photo_suffix(content_type: str | None) -> str:
    content = (content_type or "").split(";", 1)[0].strip().lower()
    return {
        "image/jpeg": ".jpg",
        "image/jpg": ".jpg",
        "image/png": ".png",
        "image/webp": ".webp",
    }.get(content, ".jpg")


def _looks_like_phone_number(text: str) -> bool:
    """Return True when the text is effectively just a phone number."""
    cleaned = _clean_text(text)
    if not cleaned:
        return False
    candidate = cleaned.replace("+", "").replace("-", "").replace(" ", "")
    return candidate.isdigit()


def _is_messaging_app(app_name: str) -> bool:
    """Best-effort detection for SMS / messaging notification sources."""
    app = _clean_text(app_name).casefold()
    if not app:
        return False
    keywords = (
        "message",
        "messages",
        "messaging",
        "google messages",
        "samsung messages",
        "sms",
        "mms",
    )
    return any(keyword in app for keyword in keywords)


def _notification_message_text(props: dict) -> str:
    """Extract the user-visible message preview from a notification."""
    ticker = _clean_text(props.get("ticker", ""))
    text = _clean_text(props.get("text", ""))

    if ": " in ticker:
        return ticker.split(": ", 1)[1].strip()
    if text:
        return text.splitlines()[0].strip()
    return ticker


# ── Local JSON contacts store ──────────────────────────────────────

def _load_local_contacts() -> dict[str, str]:
    """Load our local contacts JSON: {normalized_phone: display_name}."""
    if not CONTACTS_FILE.is_file():
        return {}
    try:
        data = json.loads(CONTACTS_FILE.read_text(encoding="utf-8"))
        if isinstance(data, dict):
            return data
    except (json.JSONDecodeError, OSError):
        pass
    return {}


def _save_local_contacts(contacts: dict[str, str]):
    """Persist contacts to our JSON file."""
    CONTACTS_DIR.mkdir(parents=True, exist_ok=True)
    CONTACTS_FILE.write_text(
        json.dumps(contacts, indent=2, ensure_ascii=False),
        encoding="utf-8",
    )


def store_contact_photo(address: str, image_bytes: bytes, content_type: str | None = None) -> bool:
    norm = _normalize_phone(address)
    if not norm or not image_bytes:
        return False

    CONTACT_PHOTOS_DIR.mkdir(parents=True, exist_ok=True)
    target = CONTACT_PHOTOS_DIR / f"{norm}{_photo_suffix(content_type)}"

    try:
        if target.is_file() and target.read_bytes() == image_bytes:
            return False
    except OSError:
        pass

    for existing in CONTACT_PHOTOS_DIR.glob(f"{norm}.*"):
        if existing != target:
            try:
                existing.unlink()
            except OSError:
                pass

    try:
        target.write_bytes(image_bytes)
    except OSError:
        return False
    return True


def delete_contact_photo(address: str) -> bool:
    norm = _normalize_phone(address)
    if not norm or not CONTACT_PHOTOS_DIR.is_dir():
        return False

    removed = False
    for existing in CONTACT_PHOTOS_DIR.glob(f"{norm}.*"):
        try:
            existing.unlink()
            removed = True
        except OSError:
            continue
    return removed


def contact_photo_path(address: str) -> str | None:
    norm = _normalize_phone(address)
    if not norm or not CONTACT_PHOTOS_DIR.is_dir():
        return None

    for existing in CONTACT_PHOTOS_DIR.glob(f"{norm}.*"):
        if existing.is_file():
            return str(existing)

    key = _phone_key(norm)
    if not key:
        return None

    for existing in CONTACT_PHOTOS_DIR.iterdir():
        if existing.is_file() and _phone_key(existing.stem) == key:
            return str(existing)
    return None


def merge_contacts(contact_names: dict[str, str]) -> int:
    """Merge a batch of contact mappings into the local store."""
    contacts = _load_local_contacts()
    changed = 0
    for address, name in contact_names.items():
        norm = _normalize_phone(address)
        cleaned_name = _clean_text(name)
        if not norm or not cleaned_name:
            continue
        if contacts.get(norm) == cleaned_name:
            continue
        contacts[norm] = cleaned_name
        changed += 1
    if changed:
        _save_local_contacts(contacts)
    return changed


def save_contact(address: str, name: str):
    """Save or update a single contact name for a phone number."""
    contacts = _load_local_contacts()
    norm = _normalize_phone(address)
    if norm:
        contacts[norm] = name
        _save_local_contacts(contacts)


def delete_contact(address: str):
    """Remove a contact by phone number."""
    contacts = _load_local_contacts()
    norm = _normalize_phone(address)
    if norm and norm in contacts:
        del contacts[norm]
        _save_local_contacts(contacts)


def import_google_csv(csv_path: str) -> int:
    """Import contacts from a Google Contacts CSV export.

    Returns the number of contacts imported.
    """
    contacts = _load_local_contacts()
    count = 0
    try:
        with open(csv_path, "r", encoding="utf-8-sig") as f:
            reader = csv.DictReader(f)
            for row in reader:
                # Google CSV has "Name" or "First Name"/"Last Name" and phone columns
                name = row.get("Name", "").strip()
                if not name:
                    first = row.get("First Name", "").strip()
                    last = row.get("Last Name", "").strip()
                    name = f"{first} {last}".strip()
                if not name:
                    continue

                # Google CSV has Phone 1 - Value, Phone 2 - Value, etc.
                for key, val in row.items():
                    if "phone" in key.lower() and "value" in key.lower():
                        norm = _normalize_phone(val)
                        if norm and len(norm) >= 7:  # skip very short numbers
                            contacts[norm] = name
                            count += 1
    except (OSError, csv.Error) as e:
        print(f"[phonelink] CSV import error: {e}")
    if count:
        _save_local_contacts(contacts)
    return count


# ── vCard parsing (KDE Connect sync + VCF import) ─────────────────

def _parse_vcard_name(path: Path) -> tuple[str, list[str]]:
    """Extract display name and phone numbers from a vCard file."""
    name = ""
    phones = []
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
        for line in text.splitlines():
            upper = line.upper()
            if upper.startswith("FN:") or upper.startswith("FN;"):
                name = line.split(":", 1)[1].strip()
            elif upper.startswith("TEL") and ":" in line:
                raw = line.split(":", 1)[1].strip()
                norm = _normalize_phone(raw)
                if norm:
                    phones.append(norm)
    except OSError:
        pass
    return name, phones


def _parse_vcf_text(text: str) -> list[tuple[str, list[str]]]:
    """Parse a VCF string that may contain multiple vCard entries.

    Returns list of (display_name, [normalized_phones]).
    """
    results = []
    name = ""
    phones = []

    for line in text.splitlines():
        upper = line.upper().strip()
        if upper == "BEGIN:VCARD":
            name = ""
            phones = []
        elif upper == "END:VCARD":
            if name and phones:
                results.append((name, phones))
            name = ""
            phones = []
        elif upper.startswith("FN:") or upper.startswith("FN;"):
            name = line.split(":", 1)[1].strip()
        elif upper.startswith("TEL") and ":" in line:
            raw = line.split(":", 1)[1].strip()
            norm = _normalize_phone(raw)
            if norm:
                phones.append(norm)

    return results


def import_vcf_file(vcf_path: str) -> int:
    """Import contacts from a .vcf file (single or multi-vCard).

    Returns the number of contacts imported.
    """
    try:
        text = Path(vcf_path).read_text(encoding="utf-8", errors="replace")
    except OSError as e:
        print(f"[phonelink] VCF read error: {e}")
        return 0

    entries = _parse_vcf_text(text)
    if not entries:
        return 0

    contacts = _load_local_contacts()
    count = 0
    for name, phones in entries:
        for phone in phones:
            if phone not in contacts or contacts[phone] != name:
                contacts[phone] = name
                count += 1

    if count:
        _save_local_contacts(contacts)
    return count


def load_contact_map(device_id: str) -> dict[str, str]:
    """Build a mapping of normalised phone number → display name.

    Merges: local JSON contacts (priority) + KDE Connect vCard cache.
    """
    contact_map: dict[str, str] = {}

    # 1) KDE Connect vCards (lower priority — gets overridden by local)
    vcard_dir = VCARD_BASE / f"kdeconnect-{device_id}"
    if vcard_dir.is_dir():
        for entry in vcard_dir.iterdir():
            if entry.suffix.lower() == ".vcf" and entry.is_file():
                name, phones = _parse_vcard_name(entry)
                if name:
                    for phone in phones:
                        contact_map[phone] = name

    # 2) Local JSON contacts (higher priority)
    contact_map.update(_load_local_contacts())

    return contact_map


def synced_vcard_count(device_id: str) -> int:
    """Return the number of KDE Connect vCards currently cached for a device."""
    vcard_dir = VCARD_BASE / f"kdeconnect-{device_id}"
    if not vcard_dir.is_dir():
        return 0
    return sum(
        1 for entry in vcard_dir.iterdir()
        if entry.is_file() and entry.suffix.lower() == ".vcf"
    )


def resolve_name(contact_map: dict[str, str], address: str) -> str:
    """Look up a display name for a phone number.

    Returns the contact name if found, otherwise the original address.
    """
    if not address:
        return address
    norm = _normalize_phone(address)
    # Try exact match
    if norm in contact_map:
        return contact_map[norm]
    # Try matching last 10 digits (handles country code differences)
    if len(norm) >= 10:
        short = norm[-10:]
        for key, name in contact_map.items():
            if key.endswith(short):
                return name
    return address


# ── Notification-based contact harvesting ──────────────────────────

def harvest_contacts_from_notifications(client, device_id: str,
                                         conversations: dict) -> int:
    """Scan active notifications for SMS contact names and save them.

    Matches "Messages" app notifications to conversations by message body
    to learn the mapping of phone number -> contact name.

    Returns number of new contacts discovered.
    """
    notif_ids = client.get_active_notification_ids(device_id)
    if not notif_ids:
        return 0

    # Build a reverse lookup: message body -> address (from received msgs only)
    body_to_addr: dict[str, str] = {}
    for conv in conversations.values():
        for msg in conv.messages:
            if msg.msg_type == 1 and msg.body:  # type 1 = received
                body_to_addr[msg.body.strip()] = conv.address
        if conv.last_message:
            body_to_addr[conv.last_message.strip()] = conv.address

    existing = _load_local_contacts()
    new_count = 0

    for nid in notif_ids:
        try:
            props = client.get_notification_properties(device_id, nid)
            app = _clean_text(props.get("appName", ""))
            title = _clean_text(props.get("title", ""))

            if not _is_messaging_app(app) or not title:
                continue
            if title == "(No title)" or _looks_like_phone_number(title):
                continue

            msg_text = _notification_message_text(props)
            if not msg_text:
                continue

            matched_addr = body_to_addr.get(msg_text)
            if not matched_addr:
                for body, addr in body_to_addr.items():
                    if body.startswith(msg_text) or msg_text.startswith(body):
                        matched_addr = addr
                        break

            if matched_addr:
                norm = _normalize_phone(matched_addr)
                if norm and existing.get(norm) != title:
                    existing[norm] = title
                    new_count += 1
        except Exception:
            continue

    if new_count:
        _save_local_contacts(existing)

    return new_count


def harvest_contact_from_notification_signal(
    client, device_id: str, notif_id: str,
    conversations: dict, contact_map: dict[str, str]
) -> tuple[str, str] | None:
    """Handle a single new notification for SMS contact name discovery.

    Returns (normalized_phone, contact_name) if a new mapping was found.
    """
    try:
        props = client.get_notification_properties(device_id, notif_id)
        app = _clean_text(props.get("appName", ""))
        title = _clean_text(props.get("title", ""))

        if not _is_messaging_app(app) or not title:
            return None
        if title == "(No title)" or _looks_like_phone_number(title):
            return None

        msg_text = _notification_message_text(props)
        if not msg_text:
            return None

        for conv in conversations.values():
            if conv.last_message and (
                conv.last_message.strip() == msg_text
                or msg_text.startswith(conv.last_message.strip()[:30])
            ):
                norm = _normalize_phone(conv.address)
                if norm and contact_map.get(norm) != title:
                    save_contact(conv.address, title)
                    return (norm, title)
            for msg in conv.messages:
                if msg.msg_type == 1 and msg.body and msg.body.strip() == msg_text:
                    norm = _normalize_phone(conv.address)
                    if norm and contact_map.get(norm) != title:
                        save_contact(conv.address, title)
                        return (norm, title)
    except Exception:
        pass
    return None


def harvest_contact_from_telephony_signal(
    address: str, contact_name: str,
) -> tuple[str, str] | None:
    """Persist a contact learned from a telephony event.

    KDE Connect exposes contact names for incoming and missed calls even when
    the desktop contact cache is unavailable.
    """
    norm = _normalize_phone(address)
    name = _clean_text(contact_name)
    if not norm or not name:
        return None
    if _looks_like_phone_number(name):
        return None
    if name.casefold() in {"unknown number", "unknown", "private number"}:
        return None
    save_contact(address, name)
    return (norm, name)
