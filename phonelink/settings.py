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
}


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
            desktop = (
                "[Desktop Entry]\n"
                "Name=Phone Link\n"
                "Comment=Connect your Android phone — SMS, notifications, and files\n"
                f"Exec=/usr/bin/python3 {_RUN_PY}\n"
                "Icon=phonelink\n"
                "Terminal=false\n"
                "Type=Application\n"
                "Categories=Utility;Communication;\n"
                "X-GNOME-Autostart-enabled=true\n"
            )
            with open(_AUTOSTART_FILE, "w") as f:
                f.write(desktop)
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


# Module-level singleton
_instance: Settings | None = None


def get_settings() -> Settings:
    global _instance
    if _instance is None:
        _instance = Settings()
        _instance.sync_autostart_state()
    return _instance
