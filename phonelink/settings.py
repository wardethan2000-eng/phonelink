"""App settings — persisted to ~/.local/share/phonelink/settings.json."""

import json
import os
import shutil
from pathlib import Path

_DATA_DIR = Path.home() / ".local" / "share" / "phonelink"
_SETTINGS_FILE = _DATA_DIR / "settings.json"

_AUTOSTART_DIR = Path.home() / ".config" / "autostart"
_AUTOSTART_FILE = _AUTOSTART_DIR / "phonelink.desktop"

# Detect the run.py absolute path (stored once at import time)
_RUN_PY = str(Path(__file__).parent.parent / "run.py")

_DEFAULTS = {
    "color_scheme": "system",   # "system" | "light" | "dark"
    "open_on_startup": False,
    "notifications_enabled": True,
    "notifications_ignored_apps": [],   # list of app_name strings to hide
    "google_background_sync": True,
    "google_account_label": "",
    "google_last_sync_ts": 0.0,
    "google_last_attempt_ts": 0.0,
    "hidden_conversations": {},
}


def _desktop_entry_text() -> str:
    return (
        "[Desktop Entry]\n"
        "Name=Phone Link\n"
        "Comment=Connect your Android phone — SMS, notifications, and files\n"
        f"Exec=/usr/bin/python3 {_RUN_PY}\n"
        "Icon=phonelink\n"
        "Terminal=false\n"
        "Type=Application\n"
        "Categories=Utility;Communication;\n"
        "StartupWMClass=dev.phonelink.app\n"
        "X-GNOME-Autostart-enabled=true\n"
    )


class Settings:
    def __init__(self):
        self._data: dict = {}
        self.load()

    # ── Persistence ────────────────────────────────────────────────

    def load(self):
        try:
            with open(_SETTINGS_FILE) as f:
                stored = json.load(f)
        except (FileNotFoundError, json.JSONDecodeError):
            stored = {}
        self._data = {**_DEFAULTS, **stored}
        self._data["notifications_ignored_apps"] = list(
            self._data.get("notifications_ignored_apps", []) or []
        )
        self._data["google_background_sync"] = bool(
            self._data.get("google_background_sync", True)
        )
        self._data["google_account_label"] = str(
            self._data.get("google_account_label", "") or ""
        )
        self._data["google_last_sync_ts"] = float(
            self._data.get("google_last_sync_ts", 0.0) or 0.0
        )
        self._data["google_last_attempt_ts"] = float(
            self._data.get("google_last_attempt_ts", 0.0) or 0.0
        )
        raw_hidden = self._data.get("hidden_conversations", {}) or {}
        normalized_hidden: dict[str, dict[str, dict[str, int]]] = {}
        if isinstance(raw_hidden, dict):
            for device_id, entries in raw_hidden.items():
                if not isinstance(entries, dict):
                    continue
                normalized_entries: dict[str, dict[str, int]] = {}
                for conversation_key, payload in entries.items():
                    if not conversation_key or not isinstance(payload, dict):
                        continue
                    normalized_entries[str(conversation_key)] = {
                        "deleted_at": int(payload.get("deleted_at", 0) or 0),
                    }
                if normalized_entries:
                    normalized_hidden[str(device_id)] = normalized_entries
        self._data["hidden_conversations"] = normalized_hidden

    def save(self):
        _DATA_DIR.mkdir(parents=True, exist_ok=True)
        with open(_SETTINGS_FILE, "w") as f:
            json.dump(self._data, f, indent=2)

    # ── Accessors / mutators ───────────────────────────────────────

    @property
    def color_scheme(self) -> str:
        return self._data["color_scheme"]

    @color_scheme.setter
    def color_scheme(self, value: str):
        assert value in ("system", "light", "dark")
        self._data["color_scheme"] = value
        self.save()

    @property
    def open_on_startup(self) -> bool:
        return self._data["open_on_startup"]

    @open_on_startup.setter
    def open_on_startup(self, value: bool):
        self._data["open_on_startup"] = bool(value)
        self.save()
        self._apply_autostart(bool(value))

    @property
    def notifications_enabled(self) -> bool:
        return self._data["notifications_enabled"]

    @notifications_enabled.setter
    def notifications_enabled(self, value: bool):
        self._data["notifications_enabled"] = bool(value)
        self.save()

    @property
    def notifications_ignored_apps(self) -> list[str]:
        return list(self._data["notifications_ignored_apps"])

    @property
    def google_background_sync(self) -> bool:
        return self._data["google_background_sync"]

    @google_background_sync.setter
    def google_background_sync(self, value: bool):
        self._data["google_background_sync"] = bool(value)
        self.save()

    @property
    def google_account_label(self) -> str:
        return self._data["google_account_label"]

    @google_account_label.setter
    def google_account_label(self, value: str):
        self._data["google_account_label"] = str(value or "")
        self.save()

    @property
    def google_last_sync_ts(self) -> float:
        return float(self._data["google_last_sync_ts"])

    @google_last_sync_ts.setter
    def google_last_sync_ts(self, value: float):
        self._data["google_last_sync_ts"] = float(value or 0.0)
        self.save()

    @property
    def google_last_attempt_ts(self) -> float:
        return float(self._data["google_last_attempt_ts"])

    @google_last_attempt_ts.setter
    def google_last_attempt_ts(self, value: float):
        self._data["google_last_attempt_ts"] = float(value or 0.0)
        self.save()

    def clear_google_account(self):
        self._data["google_account_label"] = ""
        self._data["google_last_sync_ts"] = 0.0
        self._data["google_last_attempt_ts"] = 0.0
        self.save()

    def conversation_hidden_until(self, device_id: str, conversation_key: str) -> int:
        if not device_id or not conversation_key:
            return 0
        hidden = self._data.get("hidden_conversations", {})
        return int(
            hidden.get(device_id, {}).get(conversation_key, {}).get("deleted_at", 0) or 0
        )

    def hide_conversation(self, device_id: str, conversation_key: str, deleted_at: int):
        if not device_id or not conversation_key:
            return
        hidden = self._data.setdefault("hidden_conversations", {})
        device_hidden = hidden.setdefault(device_id, {})
        device_hidden[conversation_key] = {
            "deleted_at": int(deleted_at or 0),
        }
        self.save()

    def unhide_conversation(self, device_id: str, conversation_key: str) -> bool:
        if not device_id or not conversation_key:
            return False
        hidden = self._data.get("hidden_conversations", {})
        device_hidden = hidden.get(device_id)
        if not device_hidden:
            return False
        removed = device_hidden.pop(conversation_key, None) is not None
        if not device_hidden:
            hidden.pop(device_id, None)
        if removed:
            self.save()
        return removed

    def add_ignored_app(self, app_name: str):
        if app_name not in self._data["notifications_ignored_apps"]:
            self._data["notifications_ignored_apps"].append(app_name)
            self.save()

    def remove_ignored_app(self, app_name: str):
        try:
            self._data["notifications_ignored_apps"].remove(app_name)
            self.save()
        except ValueError:
            pass

    def is_app_ignored(self, app_name: str) -> bool:
        return app_name in self._data["notifications_ignored_apps"]

    # ── Autostart ──────────────────────────────────────────────────

    def _apply_autostart(self, enable: bool):
        if enable:
            _AUTOSTART_DIR.mkdir(parents=True, exist_ok=True)
            with open(_AUTOSTART_FILE, "w") as f:
                f.write(_desktop_entry_text())
        else:
            try:
                _AUTOSTART_FILE.unlink()
            except FileNotFoundError:
                pass

    def sync_autostart_state(self):
        """Sync the in-memory state with whether the autostart file exists."""
        exists = _AUTOSTART_FILE.exists()
        if exists != self._data["open_on_startup"]:
            self._data["open_on_startup"] = exists
            self.save()
        if exists:
            try:
                current = _AUTOSTART_FILE.read_text(encoding="utf-8")
            except OSError:
                current = ""
            desired = _desktop_entry_text()
            if current != desired:
                _AUTOSTART_DIR.mkdir(parents=True, exist_ok=True)
                _AUTOSTART_FILE.write_text(desired, encoding="utf-8")


# Module-level singleton
_instance: Settings | None = None


def get_settings() -> Settings:
    global _instance
    if _instance is None:
        _instance = Settings()
        _instance.sync_autostart_state()
    return _instance
