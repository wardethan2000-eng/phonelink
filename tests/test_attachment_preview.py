"""Tests for inline image-attachment previews and full-resolution upgrade.

Covers the pure sizing/cache helpers plus the real ``SmsPanel`` glue that
caches an arrived full-resolution file, records it on the message, and persists
it — exercised without constructing GTK widgets (see test_sms_panel_reconcile
for why: headless widget construction segfaults, the logic under test doesn't
touch GTK).
"""

import os
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from phonelink.models import Conversation, SmsMessage
from phonelink.store import MessageStore

try:
    from phonelink.ui import message_thread as mt
    from phonelink.ui import sms_panel
    _IMPORT_ERR = None
except Exception as _e:  # noqa: BLE001 — missing typelibs → skip cleanly
    mt = None
    sms_panel = None
    _IMPORT_ERR = _e


@unittest.skipIf(mt is None, f"UI import unavailable: {_IMPORT_ERR}")
class ScaledImageSizeTests(unittest.TestCase):
    def test_thumbnail_is_scaled_up_to_the_preview_floor(self):
        # A 100x100 KDE Connect thumbnail should render as a tidy preview, not
        # a tiny stamp — but not upscaled past the cap.
        w, h = mt._scaled_image_size(100, 100, full=False)
        self.assertEqual(w, h)
        self.assertGreaterEqual(w, 200)
        self.assertLessEqual(w, round(100 * mt.THUMB_IMAGE_UPSCALE_CAP))

    def test_full_image_scales_down_to_fit_and_keeps_aspect(self):
        w, h = mt._scaled_image_size(2000, 1000, full=True)
        self.assertLessEqual(w, mt.FULL_IMAGE_MAX_W)
        self.assertLessEqual(h, mt.FULL_IMAGE_MAX_H)
        self.assertAlmostEqual(w / h, 2.0, places=1)  # aspect preserved

    def test_full_image_smaller_than_box_is_not_upscaled(self):
        # A crisp full image already smaller than the box must stay natural so
        # it never looks blurry.
        w, h = mt._scaled_image_size(320, 240, full=True)
        self.assertEqual((w, h), (320, 240))


@unittest.skipIf(mt is None, f"UI import unavailable: {_IMPORT_ERR}")
class FullAttachmentCacheTests(unittest.TestCase):
    def test_path_is_derived_from_unique_identifier_and_mime(self):
        att = {"uniqueIdentifier": "PART_42.jpg", "mimeType": "image/jpeg"}
        p = mt.full_attachment_cache_path(att)
        self.assertIsNotNone(p)
        self.assertTrue(str(p).endswith(".jpg"))

    def test_existing_full_attachment_prefers_recorded_path(self):
        with tempfile.TemporaryDirectory() as d:
            real = Path(d) / "full.jpg"
            real.write_bytes(b"x")
            att = {"uniqueIdentifier": "u1", "mimeType": "image/jpeg",
                   "fullPath": str(real)}
            self.assertEqual(mt.existing_full_attachment(att), str(real))

    def test_existing_full_attachment_none_when_missing(self):
        att = {"uniqueIdentifier": "nope", "mimeType": "image/jpeg"}
        self.assertIsNone(mt.existing_full_attachment(att))


@unittest.skipIf(sms_panel is None, f"UI import unavailable: {_IMPORT_ERR}")
class StoreFullAttachmentTests(unittest.TestCase):
    def setUp(self):
        self._cache = tempfile.TemporaryDirectory()
        self._orig_cache_dir = mt.FULL_ATTACHMENT_CACHE_DIR
        mt.FULL_ATTACHMENT_CACHE_DIR = Path(self._cache.name)

    def tearDown(self):
        mt.FULL_ATTACHMENT_CACHE_DIR = self._orig_cache_dir
        self._cache.cleanup()

    def _panel(self, store):
        p = sms_panel.SmsPanel.__new__(sms_panel.SmsPanel)
        p._store = store
        p._device = SimpleNamespace(id="dev1", reachable=True, name="Phone")
        # _conversations is a read-only property → self._index.conversations
        p._index = SimpleNamespace(conversations={}, primary_for=lambda t: t)
        return p

    def _conv_with_image(self, uid="PART_99.jpg"):
        att = {"partId": 3, "mimeType": "image/jpeg", "payload": "thumb-b64",
               "uniqueIdentifier": uid, "fileName": uid}
        msg = SmsMessage(uid=1001, address="+15551234567", body="", date=1_700_000_000,
                         thread_id=7, attachments=[att])
        conv = Conversation(thread_id=7, address="+15551234567", messages=[msg])
        return conv, msg, att

    def test_upgrades_and_persists_matching_image(self):
        store = MessageStore(path=":memory:")
        panel = self._panel(store)
        conv, msg, att = self._conv_with_image()
        panel._index.conversations = {7: conv}

        with tempfile.TemporaryDirectory() as d:
            arrived = Path(d) / "PART_99.jpg"
            arrived.write_bytes(b"the-real-full-image")
            thread_id = panel._store_full_attachment(str(arrived), "PART_99.jpg")

        self.assertEqual(thread_id, 7)
        self.assertIn("fullPath", att)
        self.assertTrue(os.path.isfile(att["fullPath"]))
        self.assertEqual(Path(att["fullPath"]).read_bytes(), b"the-real-full-image")
        # Persisted to the store under the same attachment.
        loaded = store.load_conversations("dev1")
        loaded_atts = loaded[7].messages[0].attachments
        self.assertEqual(loaded_atts[0].get("fullPath"), att["fullPath"])

    def test_no_match_returns_none(self):
        store = MessageStore(path=":memory:")
        panel = self._panel(store)
        conv, _msg, _att = self._conv_with_image(uid="PART_99.jpg")
        panel._index.conversations = {7: conv}
        with tempfile.TemporaryDirectory() as d:
            arrived = Path(d) / "SOMETHING_ELSE.jpg"
            arrived.write_bytes(b"data")
            self.assertIsNone(
                panel._store_full_attachment(str(arrived), "SOMETHING_ELSE.jpg")
            )


if __name__ == "__main__":
    unittest.main()
