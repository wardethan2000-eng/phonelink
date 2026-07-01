import tempfile
import unittest
from pathlib import Path
from unittest import mock

import phonelink.settings as settings_mod
from phonelink.settings import (
    APPLICATION_ID,
    DESKTOP_FILENAME,
    Settings,
    _desktop_entry_text,
)


class DesktopEntryTests(unittest.TestCase):
    def test_desktop_identity_matches_application_id(self):
        self.assertEqual(DESKTOP_FILENAME, f"{APPLICATION_ID}.desktop")

        entry = _desktop_entry_text()

        self.assertIn(f"StartupWMClass={APPLICATION_ID}", entry)
        self.assertIn("Keywords=phone;sms;notifications;kdeconnect;android;", entry)
        self.assertNotIn("__PHONELINK_RUNPY__", entry)
        self.assertRegex(entry, r"(?m)^Exec=.+$")


class SettingsPersistenceTests(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self._path = Path(self._dir.name) / "settings.json"
        # Redirect the module-level target away from the user's real settings.
        self._patch = mock.patch.object(settings_mod, "_SETTINGS_FILE", self._path)
        self._patch.start()
        self.s = Settings()  # loads defaults (file does not exist yet)

    def tearDown(self):
        self._patch.stop()
        self._dir.cleanup()

    def test_setter_writes_atomically(self):
        self.s.notifications_enabled = False
        self.assertTrue(self._path.exists())
        import json
        self.assertFalse(json.loads(self._path.read_text())["notifications_enabled"])

    def test_batch_collapses_writes_into_one(self):
        with mock.patch.object(settings_mod, "atomic_write_text") as w:
            with self.s.batch():
                self.s.notifications_enabled = False
                self.s.google_account_label = "me@example.com"
                self.s.google_last_sync_ts = 123.0
            self.assertEqual(w.call_count, 1)  # one write for the whole session

    def test_redundant_save_is_deduped(self):
        self.s.notifications_enabled = False  # first real write
        with mock.patch.object(settings_mod, "atomic_write_text") as w:
            self.s.notifications_enabled = False  # same value → no write
            self.assertEqual(w.call_count, 0)

    def test_message_font_scale_defaults_to_one(self):
        self.assertEqual(self.s.message_font_scale, 1.0)

    def test_message_font_scale_setter_clamps_and_persists(self):
        import json
        self.s.message_font_scale = 99.0  # absurd → clamped to max
        self.assertEqual(self.s.message_font_scale, settings_mod.MESSAGE_FONT_SCALE_MAX)
        self.assertEqual(
            json.loads(self._path.read_text())["message_font_scale"],
            settings_mod.MESSAGE_FONT_SCALE_MAX,
        )
        self.s.message_font_scale = 0.0  # below min → clamped to min
        self.assertEqual(self.s.message_font_scale, settings_mod.MESSAGE_FONT_SCALE_MIN)

    def test_message_font_scale_loads_clamped_from_disk(self):
        self._path.write_text('{"message_font_scale": 42}')
        reloaded = Settings()
        self.assertEqual(
            reloaded.message_font_scale, settings_mod.MESSAGE_FONT_SCALE_MAX
        )


if __name__ == "__main__":
    unittest.main()
