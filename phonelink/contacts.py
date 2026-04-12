"""Contact name resolution — local JSON store + KDE Connect vCard cache + CSV import."""

import csv
import io
import json
import os
import re
from pathlib import Path

# KDE Connect stores synced vCards here
VCARD_BASE = Path.home() / ".local" / "share" / "kpeoplevcard"

# Our own local contacts store
CONTACTS_DIR = Path.home() / ".local" / "share" / "phonelink"
CONTACTS_FILE = CONTACTS_DIR / "contacts.json"


def _normalize_phone(number: str) -> str:
    """Strip a phone number to digits only for comparison."""
    return re.sub(r"[^\d]", "", number)


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
            app = props.get("appName", "")
            title = props.get("title", "").strip()
            ticker = props.get("ticker", "")

            if app != "Messages" or not title:
                continue
            # Skip if title looks like a raw phone number
            if title.replace("+", "").replace("-", "").replace(" ", "").isdigit():
                continue

            msg_text = ticker.split(": ", 1)[1].strip() if ": " in ticker else ""
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
        app = props.get("appName", "")
        title = props.get("title", "").strip()
        ticker = props.get("ticker", "")

        if app != "Messages" or not title:
            return None
        if title.replace("+", "").replace("-", "").replace(" ", "").isdigit():
            return None

        msg_text = ticker.split(": ", 1)[1].strip() if ": " in ticker else ""
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
