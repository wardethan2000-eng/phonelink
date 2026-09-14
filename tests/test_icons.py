"""Tests for the theme-aware icon fallback resolver (#icons).

``resolve_icon`` falls back through alternative names when the active icon
theme lacks a name the app was written against (Mint's ``xsi-*`` names).
The GTK theme is stubbed so the tests run without a display.
"""

import unittest
from unittest import mock


class FakeTheme:
    def __init__(self, names):
        self._names = set(names)

    def has_icon(self, name):
        return name in self._names


class ResolveIconTests(unittest.TestCase):
    def test_returns_name_when_theme_has_it(self):
        from phonelink.ui import icons

        with mock.patch.object(icons, "_icon_theme", return_value=FakeTheme({"phone-symbolic"})):
            self.assertEqual(icons.resolve_icon("phone-symbolic"), "phone-symbolic")

    def test_falls_back_when_name_missing(self):
        from phonelink.ui import icons

        theme = FakeTheme({"notifications-symbolic"})
        with mock.patch.object(icons, "_icon_theme", return_value=theme):
            self.assertEqual(
                icons.resolve_icon("xsi-notifications-symbolic"),
                "notifications-symbolic",
            )

    def test_prefers_first_available_fallback(self):
        from phonelink.ui import icons

        theme = FakeTheme({"web-browser"})
        with mock.patch.object(icons, "_icon_theme", return_value=theme):
            self.assertEqual(icons.resolve_icon("web-browser-symbolic"), "web-browser")

    def test_returns_request_when_every_candidate_missing(self):
        from phonelink.ui import icons

        with mock.patch.object(icons, "_icon_theme", return_value=FakeTheme(set())):
            self.assertEqual(
                icons.resolve_icon("xsi-notifications-symbolic"),
                "xsi-notifications-symbolic",
            )

    def test_returns_request_without_display(self):
        from phonelink.ui import icons

        with mock.patch.object(icons, "_icon_theme", return_value=None):
            self.assertEqual(icons.resolve_icon("phone-symbolic"), "phone-symbolic")

    def test_unknown_name_is_returned_unchanged(self):
        from phonelink.ui import icons

        with mock.patch.object(icons, "_icon_theme", return_value=FakeTheme(set())):
            self.assertEqual(icons.resolve_icon("totally-made-up"), "totally-made-up")


class DeviceIconSizeTests(unittest.TestCase):
    def test_size_unchanged_on_non_breeze_theme(self):
        from phonelink.ui import icons

        with mock.patch.object(icons, "_icon_theme_name", return_value="Adwaita"):
            self.assertEqual(icons.device_icon_size(24), 24)

    def test_size_scaled_up_on_breeze(self):
        from phonelink.ui import icons

        with mock.patch.object(icons, "_icon_theme_name", return_value="breeze-dark"):
            self.assertGreater(icons.device_icon_size(24), 24)

    def test_size_unchanged_without_settings(self):
        from phonelink.ui import icons

        with mock.patch.object(icons, "_icon_theme_name", return_value=""):
            self.assertEqual(icons.device_icon_size(20), 20)


if __name__ == "__main__":
    unittest.main()
