import unittest

from phonelink.settings import APPLICATION_ID, DESKTOP_FILENAME, _desktop_entry_text


class DesktopEntryTests(unittest.TestCase):
    def test_desktop_identity_matches_application_id(self):
        self.assertEqual(DESKTOP_FILENAME, f"{APPLICATION_ID}.desktop")

        entry = _desktop_entry_text()

        self.assertIn(f"StartupWMClass={APPLICATION_ID}", entry)
        self.assertIn("Keywords=phone;sms;notifications;kdeconnect;android;", entry)
        self.assertNotIn("__PHONELINK_RUNPY__", entry)
        self.assertRegex(entry, r"(?m)^Exec=.+$")


if __name__ == "__main__":
    unittest.main()
