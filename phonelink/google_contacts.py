"""Google Contacts import helpers for Phone Link."""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path

from phonelink.contacts import (
    _normalize_phone,
    delete_contact_photo,
    merge_contacts,
    store_contact_photo,
)

CONFIG_DIR = Path.home() / ".config" / "phonelink"
DATA_DIR = Path.home() / ".local" / "share" / "phonelink"
GOOGLE_CLIENT_FILE = CONFIG_DIR / "google_oauth.json"
GOOGLE_TOKEN_FILE = DATA_DIR / "google_token.json"

SCOPES = ["https://www.googleapis.com/auth/contacts"]


class GoogleContactsError(Exception):
    """Base class for Google Contacts import errors."""


class GoogleContactsDependencyError(GoogleContactsError):
    """Raised when the Google API packages are unavailable."""


class GoogleContactsConfigError(GoogleContactsError):
    """Raised when Google OAuth client configuration is missing."""


class GoogleContactsAuthRequiredError(GoogleContactsError):
    """Raised when the saved Google token can no longer be used silently."""


@dataclass
class GoogleContactsStatus:
    configured: bool
    connected: bool


@dataclass
class GoogleContactsImportResult:
    imported_contacts: int
    seen_people: int
    account_label: str = "Google account"
    imported_photos: int = 0


@dataclass
class GoogleContactUpsertResult:
    action: str
    account_label: str = "Google account"


def _import_google_deps():
    try:
        from google.auth.transport.requests import AuthorizedSession, Request
        from google.oauth2.credentials import Credentials
        from google_auth_oauthlib.flow import InstalledAppFlow
        from googleapiclient.discovery import build
    except ImportError as exc:
        raise GoogleContactsDependencyError(
            "Install Google Contacts support with your distro packages, for example on Debian/Ubuntu: sudo apt install python3-googleapi python3-google-auth python3-google-auth-oauthlib python3-google-auth-httplib2. If you prefer a virtualenv, create one with --system-site-packages and install the same libraries there."
        ) from exc
    return Request, AuthorizedSession, Credentials, InstalledAppFlow, build


def _client_config_from_env() -> dict | None:
    client_id = os.environ.get("PHONELINK_GOOGLE_CLIENT_ID", "").strip()
    client_secret = os.environ.get("PHONELINK_GOOGLE_CLIENT_SECRET", "").strip()
    if not client_id:
        return None
    return {
        "installed": {
            "client_id": client_id,
            "client_secret": client_secret,
            "auth_uri": "https://accounts.google.com/o/oauth2/auth",
            "token_uri": "https://oauth2.googleapis.com/token",
            "redirect_uris": ["http://127.0.0.1", "http://localhost"],
        }
    }


def _load_client_config() -> dict:
    env_config = _client_config_from_env()
    if env_config:
        return env_config
    if GOOGLE_CLIENT_FILE.is_file():
        try:
            return json.loads(GOOGLE_CLIENT_FILE.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError) as exc:
            raise GoogleContactsConfigError(
                f"Invalid Google OAuth config at {GOOGLE_CLIENT_FILE}"
            ) from exc
    raise GoogleContactsConfigError(
        "Google Contacts import needs an OAuth desktop client configuration. "
        f"Set PHONELINK_GOOGLE_CLIENT_ID/PHONELINK_GOOGLE_CLIENT_SECRET or add {GOOGLE_CLIENT_FILE}."
    )


def has_google_client_config() -> bool:
    try:
        _load_client_config()
    except GoogleContactsConfigError:
        return False
    return True


def has_saved_google_credentials() -> bool:
    return GOOGLE_TOKEN_FILE.is_file()


def disconnect_google_contacts() -> bool:
    try:
        GOOGLE_TOKEN_FILE.unlink()
        return True
    except FileNotFoundError:
        return False


def _load_saved_credentials(Credentials):
    if not GOOGLE_TOKEN_FILE.is_file():
        return None
    try:
        return Credentials.from_authorized_user_file(str(GOOGLE_TOKEN_FILE), SCOPES)
    except (OSError, ValueError):
        return None


def _credentials_have_required_scopes(creds) -> bool:
    if hasattr(creds, "has_scopes"):
        try:
            return bool(creds.has_scopes(SCOPES))
        except Exception:
            pass
    scopes = set(getattr(creds, "scopes", []) or [])
    return scopes.issuperset(SCOPES)


def _save_credentials(creds):
    DATA_DIR.mkdir(parents=True, exist_ok=True)
    if hasattr(creds, "to_json"):
        GOOGLE_TOKEN_FILE.write_text(creds.to_json(), encoding="utf-8")
        return

    payload = {
        "token": getattr(creds, "token", None),
        "refresh_token": getattr(creds, "refresh_token", None),
        "token_uri": getattr(creds, "token_uri", None),
        "client_id": getattr(creds, "client_id", None),
        "client_secret": getattr(creds, "client_secret", None),
        "scopes": list(getattr(creds, "scopes", []) or []),
    }
    id_token = getattr(creds, "id_token", None)
    if id_token:
        payload["id_token"] = id_token

    GOOGLE_TOKEN_FILE.write_text(json.dumps(payload), encoding="utf-8")


def _ensure_credentials(allow_browser: bool = True):
    Request, _AuthorizedSession, Credentials, InstalledAppFlow, _build = _import_google_deps()
    creds = _load_saved_credentials(Credentials)

    if creds and creds.valid and _credentials_have_required_scopes(creds):
        return creds

    if creds and creds.expired and creds.refresh_token and _credentials_have_required_scopes(creds):
        try:
            creds.refresh(Request())
            _save_credentials(creds)
            return creds
        except Exception:
            if not allow_browser:
                disconnect_google_contacts()
                raise GoogleContactsAuthRequiredError(
                    "Google Contacts needs you to reconnect from Settings before background refresh can continue."
                )

    if not allow_browser:
        if creds is not None:
            disconnect_google_contacts()
        raise GoogleContactsAuthRequiredError(
            "Google Contacts needs interactive authorization from Settings."
        )

    flow = InstalledAppFlow.from_client_config(_load_client_config(), SCOPES)
    creds = flow.run_local_server(
        host="127.0.0.1",
        port=0,
        open_browser=True,
        authorization_prompt_message="Phone Link is opening your browser for Google Contacts access.",
        success_message="Google Contacts access granted. You can close this window and return to Phone Link.",
    )
    _save_credentials(creds)
    return creds


def _build_people_service(creds):
    _Request, _AuthorizedSession, _Credentials, _InstalledAppFlow, build = _import_google_deps()
    return build("people", "v1", credentials=creds, cache_discovery=False)


def _pick_primary_name(person: dict) -> str:
    names = person.get("names", []) or []
    primary = next((item for item in names if item.get("metadata", {}).get("primary")), None)
    chosen = primary or (names[0] if names else None)
    if not chosen:
        return ""
    return (chosen.get("displayName") or chosen.get("unstructuredName") or "").strip()


def _pick_account_label(service) -> str:
    try:
        profile = service.people().get(
            resourceName="people/me",
            personFields="names,emailAddresses",
        ).execute()
    except Exception:
        return "Google account"

    emails = profile.get("emailAddresses", []) or []
    primary = next((item for item in emails if item.get("metadata", {}).get("primary")), None)
    if primary and primary.get("value"):
        return primary["value"]
    name = _pick_primary_name(profile)
    return name or "Google account"


def _iter_connections(service, person_fields: str):
    page_token = None
    while True:
        response = service.people().connections().list(
            resourceName="people/me",
            pageSize=1000,
            pageToken=page_token,
            personFields=person_fields,
            sortOrder="FIRST_NAME_ASCENDING",
        ).execute()
        for person in response.get("connections", []) or []:
            yield person
        page_token = response.get("nextPageToken")
        if not page_token:
            break


def _pick_photo_url(person: dict) -> str:
    photos = person.get("photos", []) or []
    primary = next((item for item in photos if item.get("metadata", {}).get("primary")), None)
    ordered = [primary] if primary else []
    ordered.extend(item for item in photos if item is not primary)

    for photo in ordered:
        if not photo or photo.get("default"):
            continue
        url = (photo.get("url") or "").strip()
        if url:
            return url
    return ""


def _number_key(number: str) -> str:
    norm = _normalize_phone(number)
    return norm[-10:] if len(norm) >= 10 else norm


def _phones_match(left: str, right: str) -> bool:
    left_norm = _normalize_phone(left)
    right_norm = _normalize_phone(right)
    if not left_norm or not right_norm:
        return False
    if left_norm == right_norm:
        return True
    if len(left_norm) >= 10 and len(right_norm) >= 10:
        return left_norm[-10:] == right_norm[-10:]
    return False


def _contact_body(name: str, phone: str) -> dict:
    return {
        "names": [{"unstructuredName": name, "displayName": name}],
        "phoneNumbers": [{"value": phone}],
    }


def upsert_google_contact(
    address: str,
    display_name: str,
    allow_browser: bool = False,
) -> GoogleContactUpsertResult:
    if not display_name.strip() or not _normalize_phone(address):
        raise GoogleContactsError("A valid phone number and contact name are required.")

    creds = _ensure_credentials(allow_browser=allow_browser)
    service = _build_people_service(creds)
    account_label = _pick_account_label(service)

    for person in _iter_connections(service, "names,phoneNumbers,metadata"):
        for phone in person.get("phoneNumbers", []) or []:
            value = (phone.get("value") or "").strip()
            if value and _phones_match(value, address):
                updated = dict(person)
                updated["names"] = _contact_body(display_name, address)["names"]
                service.people().updateContact(
                    resourceName=person["resourceName"],
                    updatePersonFields="names",
                    personFields="names,phoneNumbers",
                    body=updated,
                ).execute()
                return GoogleContactUpsertResult(
                    action="updated",
                    account_label=account_label,
                )

    service.people().createContact(body=_contact_body(display_name, address)).execute()
    return GoogleContactUpsertResult(
        action="created",
        account_label=account_label,
    )


def _download_contact_photo(session, url: str) -> tuple[bytes, str] | None:
    try:
        response = session.get(url, timeout=20)
    except Exception:
        return None
    if getattr(response, "status_code", 0) != 200:
        return None
    content = getattr(response, "content", b"")
    if not content:
        return None
    return content, response.headers.get("content-type", "")


def import_google_contacts(
    photo_numbers: set[str] | None = None,
    allow_browser: bool = True,
) -> GoogleContactsImportResult:
    """Import contacts from Google People API into the local store."""
    _Request, AuthorizedSession, _Credentials, _InstalledAppFlow, build = _import_google_deps()
    creds = _ensure_credentials(allow_browser=allow_browser)

    service = build("people", "v1", credentials=creds, cache_discovery=False)
    account_label = _pick_account_label(service)
    photo_keys = {
        _number_key(number)
        for number in (photo_numbers or set())
        if _number_key(number)
    }
    photo_session = AuthorizedSession(creds) if photo_keys else None

    imported: dict[str, str] = {}
    seen_people = 0
    imported_photos = 0
    for person in _iter_connections(service, "names,phoneNumbers,photos"):
        seen_people += 1
        name = _pick_primary_name(person)
        person_numbers: list[str] = []
        if not name:
            continue
        for phone in person.get("phoneNumbers", []) or []:
            value = (phone.get("value") or "").strip()
            if value:
                imported[value] = name
                norm = _normalize_phone(value)
                if norm:
                    person_numbers.append(norm)

        matched_photo_numbers = {
            number for number in person_numbers if _number_key(number) in photo_keys
        }
        if matched_photo_numbers and photo_session is not None:
            photo_url = _pick_photo_url(person)
            if photo_url:
                photo_result = _download_contact_photo(photo_session, photo_url)
                if photo_result:
                    photo_bytes, content_type = photo_result
                    for number in matched_photo_numbers:
                        if store_contact_photo(number, photo_bytes, content_type):
                            imported_photos += 1
            else:
                for number in matched_photo_numbers:
                    if delete_contact_photo(number):
                        imported_photos += 1

    changed = merge_contacts(imported)
    return GoogleContactsImportResult(
        imported_contacts=changed,
        seen_people=seen_people,
        account_label=account_label,
        imported_photos=imported_photos,
    )