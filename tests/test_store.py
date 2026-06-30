import tempfile
import unittest
from pathlib import Path

from phonelink.models import Conversation, SmsMessage
from phonelink.store import MessageStore


class MessageStoreTests(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self.store = MessageStore(path=Path(self._dir.name) / "messages.db")

    def tearDown(self):
        self.store.close()
        self._dir.cleanup()

    def test_message_and_conversation_round_trip(self):
        conv = Conversation(
            thread_id=7,
            display_name="Jane Doe",
            address="+13165551212",
            addresses=["+13165551212"],
            last_message="hello",
            last_date=1000,
            is_read=False,
        )
        msg = SmsMessage(
            uid=42, body="hello", address="+13165551212", date=1000,
            msg_type=1, read=0, thread_id=7,
            attachments=[{"partId": 1, "fileName": "a.jpg"}],
        )
        self.store.upsert_conversation("dev1", conv)
        self.store.upsert_message("dev1", conv.thread_id, msg)

        loaded = self.store.load_conversations("dev1")
        self.assertIn(7, loaded)
        got = loaded[7]
        self.assertEqual(got.display_name, "Jane Doe")
        self.assertFalse(got.is_read)
        self.assertEqual(len(got.messages), 1)
        self.assertEqual(got.messages[0].uid, 42)
        self.assertEqual(got.messages[0].attachments[0]["fileName"], "a.jpg")

    def test_upsert_is_idempotent_by_uid(self):
        conv = Conversation(thread_id=1, address="111", addresses=["111"])
        self.store.upsert_conversation("dev1", conv)
        for body in ("v1", "v2"):
            self.store.upsert_message(
                "dev1", 1,
                SmsMessage(uid=5, body=body, address="111", date=1, thread_id=1),
            )
        loaded = self.store.load_conversations("dev1")
        self.assertEqual(len(loaded[1].messages), 1)
        self.assertEqual(loaded[1].messages[0].body, "v2")

    def test_drafts_are_not_persisted(self):
        conv = Conversation(thread_id=-1, address="222", addresses=["222"])
        self.store.upsert_conversation("dev1", conv)
        self.store.upsert_message(
            "dev1", -1, SmsMessage(uid=9, body="draft", address="222", thread_id=-1)
        )
        self.assertEqual(self.store.load_conversations("dev1"), {})

    def test_delete_removes_conversation_and_messages(self):
        conv = Conversation(thread_id=3, address="333", addresses=["333"])
        self.store.upsert_conversation("dev1", conv)
        self.store.upsert_message(
            "dev1", 3, SmsMessage(uid=1, body="x", address="333", thread_id=3)
        )
        self.store.delete_conversation("dev1", 3)
        self.assertEqual(self.store.load_conversations("dev1"), {})

    def test_devices_are_isolated(self):
        self.store.upsert_conversation(
            "devA", Conversation(thread_id=1, address="a", addresses=["a"])
        )
        self.store.upsert_conversation(
            "devB", Conversation(thread_id=1, address="b", addresses=["b"])
        )
        self.assertEqual(self.store.load_conversations("devA")[1].address, "a")
        self.assertEqual(self.store.load_conversations("devB")[1].address, "b")

    def test_orphan_message_synthesizes_conversation(self):
        # Message with no conversation row should still load under its thread.
        self.store.upsert_message(
            "dev1", 8, SmsMessage(uid=2, body="hi", address="888", thread_id=8)
        )
        loaded = self.store.load_conversations("dev1")
        self.assertIn(8, loaded)
        self.assertEqual(loaded[8].messages[0].body, "hi")


if __name__ == "__main__":
    unittest.main()
