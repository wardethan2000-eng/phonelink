import unittest

from phonelink.models import SmsMessage, Conversation
from phonelink import reconcile
from phonelink.reconcile import ConversationIndex


def name_for(addresses, address):
    """Trivial resolver: name is the primary address."""
    return address or (addresses[0] if addresses else "")


def msg(uid, thread_id, addrs, *, date=0, body="", msg_type=1, read=1):
    """Build a message the way the panel would, with its address list."""
    m = SmsMessage(
        uid=uid,
        body=body or f"m{uid}",
        address=addrs[0] if addrs else "",
        date=date or uid,
        msg_type=msg_type,
        read=read,
        thread_id=thread_id,
    )
    return m, addrs


# ── pure identity helpers ──────────────────────────────────────────


class IdentityTests(unittest.TestCase):
    def test_single_participant(self):
        self.assertEqual(
            reconcile.conversation_identity(["+13165551212"]), "3165551212"
        )

    def test_identity_is_order_independent(self):
        a = reconcile.conversation_identity(["+13165551212", "3165559999"])
        b = reconcile.conversation_identity(["3165559999", "+1-316-555-1212"])
        self.assertEqual(a, b)

    def test_country_code_variants_collapse(self):
        self.assertEqual(
            reconcile.conversation_identity(["+13165551212"]),
            reconcile.conversation_identity(["3165551212"]),
        )

    def test_self_number_stripped_for_groups(self):
        # A 1:1 MMS thread carries [self, alice]; stripping self makes it match
        # the plain SMS thread [alice].
        sms = reconcile.conversation_identity(["3165551111"], self_key="3169990000")
        mms = reconcile.conversation_identity(
            ["3169990000", "3165551111"], self_key="3169990000"
        )
        self.assertEqual(sms, mms)

    def test_self_number_not_stripped_from_one_on_one(self):
        # If only self is present we keep it (never produce an empty identity).
        self.assertEqual(
            reconcile.conversation_identity(["3169990000"], self_key="3169990000"),
            "3169990000",
        )

    def test_detect_self_key(self):
        # 5551110000 shows up across three group threads, never solo, unknown.
        key_lists = [
            ["3165551111", "5551110000"],
            ["3165552222", "5551110000"],
            ["3165553333", "5551110000"],
            ["3165551111"],  # solo thread for a real contact
        ]
        self.assertEqual(
            reconcile.detect_self_key(key_lists, contact_keys=[]), "5551110000"
        )

    def test_detect_self_key_ignores_known_contacts(self):
        # 5551110000 is the only group-only candidate (3165551111 has a solo
        # thread), but it is a known contact, so no self number is inferred.
        key_lists = [
            ["3165551111", "5551110000"],
            ["3165551111", "5551110000"],
            ["3165551111", "5551110000"],
            ["3165551111"],
        ]
        self.assertEqual(
            reconcile.detect_self_key(key_lists, contact_keys=["5551110000"]), ""
        )


# ── ingest / merge behaviour ───────────────────────────────────────


class IngestTests(unittest.TestCase):
    def setUp(self):
        self.idx = ConversationIndex()

    def test_sms_and_mms_threads_collapse_to_one_conversation(self):
        # The classic split/merge bug: same contact, two phone thread IDs.
        self.idx.ingest(*msg(1, thread_id=10, addrs=["+13165551212"], date=100),
                         name_for=name_for)
        self.idx.ingest(*msg(2, thread_id=20, addrs=["3165551212"], date=200),
                        name_for=name_for)

        self.assertEqual(len(self.idx.conversations), 1)
        conv = self.idx.get(20)
        self.assertEqual(conv.thread_id, 10)  # stable first-seen primary
        self.assertEqual({m.uid for m in conv.messages}, {1, 2})
        self.assertEqual(self.idx.secondary_threads(10), [20])
        self.assertEqual(conv.last_date, 200)
        self.assertEqual(conv.last_message, "m2")

    def test_primary_is_stable_regardless_of_arrival_order(self):
        self.idx.ingest(*msg(2, thread_id=20, addrs=["3165551212"], date=200),
                        name_for=name_for)
        self.idx.ingest(*msg(1, thread_id=10, addrs=["3165551212"], date=100),
                        name_for=name_for)
        # Whichever thread's message lands first owns the primary; it does not
        # flip when an older message arrives on the other thread.
        conv = self.idx.get(10)
        self.assertEqual(conv.thread_id, 20)
        self.assertEqual(conv.last_date, 200)

    def test_duplicate_uid_is_ignored(self):
        self.idx.ingest(*msg(1, thread_id=10, addrs=["111"], date=100),
                        name_for=name_for)
        r = self.idx.ingest(*msg(1, thread_id=10, addrs=["111"], date=100),
                            name_for=name_for)
        self.assertFalse(r.message_added)
        self.assertEqual(len(self.idx.get(10).messages), 1)

    def test_distinct_contacts_stay_separate(self):
        self.idx.ingest(*msg(1, thread_id=10, addrs=["3165551111"]),
                        name_for=name_for)
        self.idx.ingest(*msg(2, thread_id=11, addrs=["3165552222"]),
                        name_for=name_for)
        self.assertEqual(len(self.idx.conversations), 2)

    def test_group_thread_is_not_merged_with_member(self):
        self.idx.ingest(*msg(1, thread_id=10, addrs=["3165551111"]),
                        name_for=name_for)
        self.idx.ingest(
            *msg(2, thread_id=20, addrs=["3165551111", "3165552222"]),
            name_for=name_for,
        )
        self.assertEqual(len(self.idx.conversations), 2)

    def test_visible_filters_hidden(self):
        self.idx.ingest(*msg(1, thread_id=10, addrs=["111"]), name_for=name_for)
        self.idx.ingest(*msg(2, thread_id=20, addrs=["222"]), name_for=name_for)
        hidden = self.idx.get(10)
        visible = self.idx.visible(lambda c: c is hidden)
        self.assertEqual([c.thread_id for c in visible], [20])

    def test_visible_is_side_effect_free(self):
        self.idx.ingest(*msg(1, thread_id=10, addrs=["111"], date=100),
                        name_for=name_for)
        self.idx.ingest(*msg(2, thread_id=20, addrs=["111"], date=200),
                        name_for=name_for)
        before = {tid: len(c.messages) for tid, c in self.idx.conversations.items()}
        for _ in range(5):
            self.idx.visible()
        after = {tid: len(c.messages) for tid, c in self.idx.conversations.items()}
        self.assertEqual(before, after)
        self.assertEqual(len(self.idx.conversations), 1)

    def test_remove_clears_all_mappings(self):
        self.idx.ingest(*msg(1, thread_id=10, addrs=["111"]), name_for=name_for)
        self.idx.ingest(*msg(2, thread_id=20, addrs=["111"]), name_for=name_for)
        removed = self.idx.remove(20)  # remove by secondary id
        self.assertEqual(set(removed), {10, 20})
        self.assertEqual(self.idx.conversations, {})
        self.assertEqual(self.idx.thread_to_primary, {})
        self.assertEqual(self.idx.identity_to_primary, {})


class RegisterAndReindexTests(unittest.TestCase):
    def test_register_collapses_legacy_duplicates(self):
        idx = ConversationIndex()
        # Two persisted rows for the same number (a legacy split).
        idx.register(Conversation(
            thread_id=10, address="3165551212", addresses=["3165551212"],
            last_date=100, last_message="a", is_read=True,
            messages=[SmsMessage(uid=1, date=100, thread_id=10)],
        ))
        idx.register(Conversation(
            thread_id=20, address="+13165551212", addresses=["+13165551212"],
            last_date=200, last_message="b", is_read=False,
            messages=[SmsMessage(uid=2, date=200, thread_id=20)],
        ))
        self.assertEqual(len(idx.conversations), 1)
        conv = idx.get(20)
        self.assertEqual(conv.thread_id, 10)
        self.assertEqual({m.uid for m in conv.messages}, {1, 2})
        self.assertEqual(conv.last_message, "b")   # newest wins
        self.assertFalse(conv.is_read)             # unread preserved

    def test_reindex_merges_after_self_key_learned(self):
        idx = ConversationIndex()
        # Before we know the self number, a 1:1 SMS and its MMS look different.
        idx.ingest(*msg(1, thread_id=10, addrs=["3165551111"]), name_for=name_for)
        idx.ingest(
            *msg(2, thread_id=20, addrs=["3169990000", "3165551111"]),
            name_for=name_for,
        )
        self.assertEqual(len(idx.conversations), 2)
        # Learn the self number and re-key — they collapse into one.
        self.assertTrue(idx.set_self_key("3169990000"))
        idx.reindex()
        self.assertEqual(len(idx.conversations), 1)


if __name__ == "__main__":
    unittest.main()
