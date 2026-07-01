import unittest

from phonelink.models import SmsMessage
from phonelink.ui.message_thread import MessageThread, _message_markup


class AutoFetchFullImagesTests(unittest.TestCase):
    """Exercise MessageThread._auto_fetch_full_images without building a widget
    (headless GTK construction segfaults). This guards the real bug where the
    method referenced a MessageBubble-only attribute and crashed set_messages."""

    def _thread(self):
        t = MessageThread.__new__(MessageThread)
        t._thread_id = 1
        t._auto_fetched = set()
        t.requested = []
        t._request_attachment_download = lambda pid, uid, name: t.requested.append(
            (pid, uid, name)
        )
        return t

    def _image_msg(self, part_id=3, uid="PART_1.jpg", full=False):
        att = {"partId": part_id, "mimeType": "image/jpeg", "uniqueIdentifier": uid,
               "fileName": uid}
        if full:
            att["fullPath"] = __file__  # any existing file counts as "have full"
        return SmsMessage(uid=1, body="", date=1, msg_type=1, thread_id=1,
                          attachments=[att])

    def test_requests_full_res_once_per_attachment(self):
        t = self._thread()
        msg = self._image_msg()
        t._auto_fetch_full_images([msg])
        t._auto_fetch_full_images([msg])  # dedup — no second request
        self.assertEqual(t.requested, [(3, "PART_1.jpg", "PART_1.jpg")])

    def test_skips_when_full_already_present(self):
        t = self._thread()
        t._auto_fetch_full_images([self._image_msg(full=True)])
        self.assertEqual(t.requested, [])

    def test_skips_non_images_and_thumbnailless(self):
        t = self._thread()
        text_only = SmsMessage(uid=2, body="hi", date=1, msg_type=1, thread_id=1)
        no_partid = SmsMessage(uid=3, body="", date=1, msg_type=1, thread_id=1,
                               attachments=[{"mimeType": "image/png", "partId": 0}])
        t._auto_fetch_full_images([text_only, no_partid])
        self.assertEqual(t.requested, [])


class MessageThreadMarkupTests(unittest.TestCase):
    def test_no_links(self):
        text = "Hello, this is a plain message."
        markup, has_links = _message_markup(text)
        self.assertFalse(has_links)
        self.assertEqual(markup, "Hello, this is a plain message.")

    def test_escaping(self):
        text = "Hello <world> & friends."
        markup, has_links = _message_markup(text)
        self.assertFalse(has_links)
        self.assertEqual(markup, "Hello &lt;world&gt; &amp; friends.")

    def test_simple_link(self):
        text = "Check out https://google.com for info."
        markup, has_links = _message_markup(text)
        self.assertTrue(has_links)
        self.assertEqual(
            markup,
            'Check out <a href="https://google.com">https://google.com</a> for info.'
        )

    def test_multiple_links(self):
        text = "Visit www.example.org and google.com now."
        markup, has_links = _message_markup(text)
        self.assertTrue(has_links)
        self.assertEqual(
            markup,
            'Visit <a href="https://www.example.org">www.example.org</a> and '
            '<a href="https://google.com">google.com</a> now.'
        )

    def test_trailing_punctuation(self):
        text = "Go to github.com/google/repo. It is cool!"
        markup, has_links = _message_markup(text)
        self.assertTrue(has_links)
        self.assertEqual(
            markup,
            'Go to <a href="https://github.com/google/repo">github.com/google/repo</a>. It is cool!'
        )


if __name__ == "__main__":
    unittest.main()
