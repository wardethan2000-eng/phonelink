import unittest

from phonelink.hidden_texts import HiddenTextDetector, body_matches, normalize_text


class BodyMatchTests(unittest.TestCase):
    def test_exact_and_case_and_spacing(self):
        self.assertTrue(body_matches("See you at  Six", "see you at six"))

    def test_truncated_preview(self):
        self.assertTrue(body_matches("Can you pick up milk on the w…",
                                     "Can you pick up milk on the way home?"))

    def test_group_preview_with_sender(self):
        self.assertTrue(body_matches("Sam: running ten minutes late",
                                     "running ten minutes late"))

    def test_different_text(self):
        self.assertFalse(body_matches("running ten minutes late", "on my way now"))

    def test_normalize_strips_ellipsis(self):
        self.assertEqual(normalize_text("Hello there..."), "hello there")


class DetectorTests(unittest.TestCase):
    def setUp(self):
        self.d = HiddenTextDetector(settle_seconds=100, miss_window_seconds=1000,
                                    misses_to_warn=2)

    def test_short_and_media_previews_are_ignored(self):
        self.assertFalse(self.d.note_notification("ok", 0))
        self.assertFalse(self.d.note_notification("Photo", 0))
        self.assertTrue(self.d.note_notification("are we still on for dinner", 0))

    def test_waits_for_the_message_to_arrive(self):
        self.d.note_notification("are we still on for dinner", 0)
        self.assertFalse(self.d.check(50, lambda t, at: False))
        self.assertTrue(self.d.has_pending)

    def test_two_misses_warn(self):
        self.d.note_notification("are we still on for dinner", 0)
        self.d.note_notification("call me when you are free", 10)
        self.assertTrue(self.d.check(200, lambda t, at: False))
        self.assertTrue(self.d.warning)

    def test_one_miss_does_not_warn(self):
        self.d.note_notification("are we still on for dinner", 0)
        self.d.check(200, lambda t, at: False)
        self.assertFalse(self.d.warning)

    def test_misses_far_apart_do_not_warn(self):
        self.d.note_notification("are we still on for dinner", 0)
        self.d.check(200, lambda t, at: False)
        self.d.note_notification("call me when you are free", 5000)
        self.d.check(5200, lambda t, at: False)
        self.assertFalse(self.d.warning)

    def test_an_arrival_clears_the_warning(self):
        self.d.note_notification("are we still on for dinner", 0)
        self.d.note_notification("call me when you are free", 10)
        self.d.check(200, lambda t, at: False)
        self.d.note_notification("the package arrived today", 300)
        self.assertTrue(self.d.check(500, lambda t, at: True))
        self.assertFalse(self.d.warning)


if __name__ == "__main__":
    unittest.main()
