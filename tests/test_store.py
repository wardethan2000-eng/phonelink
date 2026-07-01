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

    def test_batch_coalesces_commits_but_persists_all(self):
        # sqlite3.Connection.commit is read-only (C attr), so wrap the connection
        # in a counting proxy to prove batching collapses N commits into 1.
        class CountingConn:
            def __init__(self, real):
                self._real = real
                self.commits = 0

            def commit(self):
                self.commits += 1
                return self._real.commit()

            def __getattr__(self, name):
                return getattr(self._real, name)

        self.store.upsert_conversation(
            "dev1", Conversation(thread_id=1, address="111", addresses=["111"])
        )
        proxy = CountingConn(self.store._conn)
        self.store._conn = proxy
        with self.store.batch():
            for uid in range(10):
                self.store.upsert_message(
                    "dev1", 1,
                    SmsMessage(uid=uid, body=f"m{uid}", address="111",
                               date=uid, thread_id=1),
                )
        self.assertEqual(proxy.commits, 1)  # one commit for the whole batch
        # …and every message is durably persisted.
        loaded = self.store.load_conversations("dev1")
        self.assertEqual(len(loaded[1].messages), 10)

    def test_orphan_message_synthesizes_conversation(self):
        # Message with no conversation row should still load under its thread.
        self.store.upsert_message(
            "dev1", 8, SmsMessage(uid=2, body="hi", address="888", thread_id=8)
        )
        loaded = self.store.load_conversations("dev1")
        self.assertIn(8, loaded)
        self.assertEqual(loaded[8].messages[0].body, "hi")


class ContactsStoreTests(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self._root = Path(self._dir.name)
        self.store = MessageStore(path=self._root / "messages.db")

    def tearDown(self):
        self.store.close()
        self._dir.cleanup()

    def test_save_load_delete_contact(self):
        self.store.save_contact("3165551212", "Jane")
        self.store.save_contact("3165559999", "Bob")
        self.assertEqual(
            self.store.load_contacts(),
            {"3165551212": "Jane", "3165559999": "Bob"},
        )
        self.store.save_contact("3165551212", "Jane Doe")  # upsert
        self.assertEqual(self.store.load_contacts()["3165551212"], "Jane Doe")
        self.store.delete_contact("3165559999")
        self.assertEqual(self.store.load_contacts(), {"3165551212": "Jane Doe"})

    def test_replace_contacts_makes_table_match_exactly(self):
        self.store.save_contact("111", "A")
        self.store.replace_contacts({"222": "B", "333": "C"})
        self.assertEqual(self.store.load_contacts(), {"222": "B", "333": "C"})

    def test_contacts_json_migrated_once(self):
        import json as _json
        # A pristine directory whose DB has never been opened, so the very first
        # store creation performs the one-time migration.
        fresh = self._root / "fresh"
        fresh.mkdir()
        (fresh / "contacts.json").write_text(
            _json.dumps({"5551110000": "Legacy One", "5551112222": "Legacy Two"})
        )
        store2 = MessageStore(path=fresh / "messages.db")
        try:
            self.assertEqual(
                store2.load_contacts(),
                {"5551110000": "Legacy One", "5551112222": "Legacy Two"},
            )
            # A later edit + reopen must NOT re-import the legacy file over it.
            store2.delete_contact("5551110000")
        finally:
            store2.close()
        store3 = MessageStore(path=fresh / "messages.db")
        try:
            self.assertEqual(store3.load_contacts(), {"5551112222": "Legacy Two"})
        finally:
            store3.close()


if __name__ == "__main__":
    unittest.main()
