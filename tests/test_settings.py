import unittest
import tempfile
from pathlib import Path
from unittest.mock import patch
from phonelink.settings import APPLICATION_ID, DESKTOP_FILENAME, _desktop_entry_text, Settings


class DesktopEntryTests(unittest.TestCase):
    def test_desktop_identity_matches_application_id(self):
        self.assertEqual(DESKTOP_FILENAME, f"{APPLICATION_ID}.desktop")

        entry = _desktop_entry_text()

        self.assertIn(f"StartupWMClass={APPLICATION_ID}", entry)
        self.assertIn("Keywords=phone;sms;notifications;kdeconnect;android;", entry)
        self.assertNotIn("__PHONELINK_RUNPY__", entry)
        self.assertRegex(entry, r"(?m)^Exec=.+$")


class SettingsTests(unittest.TestCase):
    def test_font_scale_defaults_and_mutation(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_settings_file = Path(tmp) / "settings.json"
            
            with patch("phonelink.settings._SETTINGS_FILE", tmp_settings_file):
                settings = Settings()
                
                # Test default value
                self.assertEqual(settings.font_scale, 1.0)
                
                # Test mutation and persistence
                settings.font_scale = 1.5
                self.assertEqual(settings.font_scale, 1.5)
                
                # Reload settings from disk to verify it persisted correctly
                new_settings = Settings()
                self.assertEqual(new_settings.font_scale, 1.5)


if __name__ == "__main__":
    unittest.main()
