"""Tests for the notifications panel's incremental list update (#9).

The pure diff (`diff_notification_rows`) is tested directly.  The panel's
`_sync_list` orchestration is driven on a bare `NotificationsPanel` (no
`Gtk.Box.__init__`) with a fake list box and `_make_row` stubbed out, so we can
assert the behaviour that matters — a single incoming notification leaves the
other rows *untouched* (preserving their expansion state) instead of rebuilding
the whole list.
"""

import types
import unittest
from unittest import mock

from phonelink.models import Notification

try:
    from phonelink.ui import notifications_panel as npmod
    _IMPORT_ERR = None
except Exception as _e:  # noqa: BLE001
    npmod = None
    _IMPORT_ERR = _e


def notif(pid, *, app="App", title="t", text="b", ts=0.0,
          dismissable=False, can_reply=False):
    return Notification(
        public_id=pid, app_name=app, title=title, text=text,
        timestamp=ts, dismissable=dismissable,
        reply_id=("reply-" + pid if can_reply else ""),
    )


class FakeListBox:
    def __init__(self):
        self.rows = []

    def append(self, row):
        self.rows.append(row)

    def remove(self, row):
        self.rows.remove(row)

    def invalidate_sort(self):
        pass


class FakeLabel:
    def set_label(self, text):
        self.text = text


class FakeSettings:
    notifications_enabled = True

    def is_app_ignored(self, _name):
        return False


class DiffTests(unittest.TestCase):
    @unittest.skipIf(npmod is None, f"gi unavailable: {_IMPORT_ERR}")
    def test_diff_partitions_ids(self):
        existing = {"a": 1, "b": 2, "c": 3}
        desired = {"b": 2, "c": 99, "d": 4}  # b same, c changed, d new, a gone
        remove, add, recreate, keep = npmod.diff_notification_rows(existing, desired)
        self.assertEqual(remove, ["a"])
        self.assertEqual(add, ["d"])
        self.assertEqual(recreate, ["c"])
        self.assertEqual(keep, ["b"])


@unittest.skipIf(npmod is None, f"gi unavailable: {_IMPORT_ERR}")
class SyncListTests(unittest.TestCase):
    def _panel(self):
        p = npmod.NotificationsPanel.__new__(npmod.NotificationsPanel)
        p._notifications = {}
        p._rows = {}
        p._list_box = FakeListBox()
        p._count_label = FakeLabel()
        # Avoid constructing real GTK rows (segfaults headless).
        p._make_row = lambda n: types.SimpleNamespace(
            notif=n, _signature=p._notif_signature(n)
        )
        return p

    def test_new_notification_preserves_existing_rows(self):
        with mock.patch.object(npmod, "get_settings", return_value=FakeSettings()):
            p = self._panel()
            p._notifications = {"a": notif("a", ts=1)}
            p._sync_list()
            row_a = p._rows["a"]

            # A new notification arrives — the existing row must NOT be recreated.
            p._notifications["b"] = notif("b", ts=2)
            p._sync_list()

            self.assertIs(p._rows["a"], row_a)
            self.assertEqual(set(p._rows), {"a", "b"})
            self.assertEqual(len(p._list_box.rows), 2)

    def test_removed_notification_drops_only_its_row(self):
        with mock.patch.object(npmod, "get_settings", return_value=FakeSettings()):
            p = self._panel()
            p._notifications = {"a": notif("a", ts=1), "b": notif("b", ts=2)}
            p._sync_list()
            row_b = p._rows["b"]

            del p._notifications["a"]
            p._sync_list()

            self.assertEqual(set(p._rows), {"b"})
            self.assertIs(p._rows["b"], row_b)
            self.assertEqual(len(p._list_box.rows), 1)

    def test_content_change_recreates_only_that_row(self):
        with mock.patch.object(npmod, "get_settings", return_value=FakeSettings()):
            p = self._panel()
            p._notifications = {
                "a": notif("a", title="old", ts=1),
                "b": notif("b", ts=2),
            }
            p._sync_list()
            row_a, row_b = p._rows["a"], p._rows["b"]

            # Only 'a' changes content.
            p._notifications["a"] = notif("a", title="new", ts=1)
            p._sync_list()

            self.assertIsNot(p._rows["a"], row_a)  # recreated
            self.assertIs(p._rows["b"], row_b)     # untouched

    def test_disabled_clears_all_rows(self):
        with mock.patch.object(npmod, "get_settings", return_value=FakeSettings()):
            p = self._panel()
            p._notifications = {"a": notif("a")}
            p._sync_list()
            self.assertEqual(len(p._list_box.rows), 1)

        disabled = FakeSettings()
        disabled.notifications_enabled = False
        with mock.patch.object(npmod, "get_settings", return_value=disabled):
            p._sync_list()
            self.assertEqual(p._rows, {})
            self.assertEqual(len(p._list_box.rows), 0)


if __name__ == "__main__":
    unittest.main()
