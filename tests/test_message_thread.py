import unittest
from phonelink.ui.message_thread import _message_markup


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
