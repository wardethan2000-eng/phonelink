"""Integration tests for the *real* SmsPanel reconciliation glue.

GTK4 cannot construct widgets headlessly here (no display → segfault), but the
panel's reconciliation methods never touch GTK — they operate on the
``ConversationIndex``, the SQLite store, and settings.  So we create a panel
instance *without* running ``Gtk.Box.__init__`` (via ``__new__``), inject
lightweight fakes for the two child widgets and the D-Bus client, and exercise
the actual ``_apply_active_conversations`` / ``_merge_message`` /
``_deduplicated_conversations`` / hide / delete / cached-load code paths against
real KDE Connect message tuples.

This is what makes M5 verifiable without a phone: the behaviour-sensitive panel
glue is executed and asserted, not reimplemented.
"""

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from gi.repository import GLib

from phonelink.reconcile import ConversationIndex
from phonelink.store import MessageStore

# Importing the panel needs gi + the GTK4/Adw typelibs (but not a display, since
# we never construct a widget).  Skip cleanly where they are unavailable.
try:
    from phonelink.ui import sms_panel
    _IMPORT_ERR = None
except Exception as _e:  # noqa: BLE001 — any import failure means "skip"
    sms_panel = None
    _IMPORT_ERR = _e


# ── fakes for the collaborators the glue talks to ──────────────────


class FakeConvList:
    def __init__(self):
        self.shown = []

    def set_contact_map(self, _m):
        pass

    def set_conversations(self, convs, force_rebuild=False):
        self.shown = list(convs)

    def select_thread(self, _tid):
        pass

    def set_thread_read_state(self, _tid, _state):
        pass


class FakeThread:
    def __init__(self):
        self.emptied = 0

    def show_empty(self):
        self.emptied += 1

    def set_messages(self, *a, **k):
        pass

    def sync(self, *a, **k):
        pass


class FakeSettings:
    def __init__(self):
        self._hidden = {}

    def conversation_hidden_until(self, dev, key):
        return self._hidden.get((dev, key), 0)

    def hide_conversation(self, dev, key, ts):
        self._hidden[(dev, key)] = ts

    def unhide_conversation(self, dev, key):
        return self._hidden.pop((dev, key), None) is not None


class FakeClient:
    bus = None

    def __init__(self):
        self.calls = []  # list of (method_name, positional_args)

    def submit(self, fn, *a, on_result=None, **k):
        self.calls.append((getattr(fn, "__name__", str(fn)), a))

    # Methods the panel passes to submit(); no-ops here (only the name matters).
    def send_sms(self, *a, **k):
        pass

    def reply_to_conversation(self, *a, **k):
        pass

    def reply_to_notification(self, *a, **k):
        pass

    def mark_conversation_as_read(self, *a, **k):
        pass

    def request_conversation(self, *a, **k):
        pass

    def request_thread_from_phone(self, *a, **k):
        pass

    def request_all_conversations(self, *a, **k):
        pass

    def supports_conversation_deletion(self, _dev):
        return False

    def call_names(self):
        return [name for name, _ in self.calls]


def make_panel():
    store = MessageStore(path=":memory:")
    p = sms_panel.SmsPanel.__new__(sms_panel.SmsPanel)
    p._index = ConversationIndex()
    p._store = store
    p._settings = FakeSettings()
    p._contact_map = {}
    p._device = SimpleNamespace(id="dev1", reachable=True, name="Phone")
    p._read_thread_ids = set()
    p._active_thread_id = None
    p._notification_reply_targets = {}
    p._conv_list = FakeConvList()
    p._thread = FakeThread()
    p.client = FakeClient()
    p._refreshing = False
    init_sync_state(p)
    return p


def init_sync_state(p):
    p._all_conversations_refresh_source = None
    p._last_catch_up = 0.0
    p._gap_fill_queue = []
    p._gap_fill_pending = {}
    p._gap_fill_source = None
    p._gap_fill_pages = {}
    p._signal_queue = []
    p._signal_queue_lock = __import__("threading").Lock()
    p._signal_drain_scheduled = False


def tup(uid, thread_id, addrs, date, body="hi", mtype=1, read=1):
    """A KDE Connect conversation tuple:
    (event, body, addresses, date, type, read, threadID, uID, subID, atts)."""
    return (0, body, [(a,) for a in addrs], date, mtype, read, thread_id, uid, 0, [])


@unittest.skipIf(sms_panel is None, f"GTK/gi unavailable: {_IMPORT_ERR}")
class PanelReconcileTests(unittest.TestCase):
    def setUp(self):
        self.p = make_panel()

    def tearDown(self):
        self.p._store.close()

    # ── the split/merge bug, end to end through the panel ──────────

    def test_sms_and_mms_threads_show_as_one_conversation(self):
        self.p._apply_active_conversations("dev1", [
            tup(1, thread_id=10, addrs=["+13165551212"], date=100),
            tup(2, thread_id=20, addrs=["3165551212"], date=200),
        ])
        visible = self.p._deduplicated_conversations()
        self.assertEqual(len(visible), 1)
        self.assertEqual({m.uid for m in visible[0].messages}, {1, 2})
        # …and the merge is stable: re-running the render path never splits it.
        for _ in range(5):
            self.assertEqual(len(self.p._deduplicated_conversations()), 1)

    def test_group_thread_not_merged_with_member(self):
        self.p._apply_active_conversations("dev1", [
            tup(1, thread_id=10, addrs=["3165551111"], date=100),
            tup(2, thread_id=20, addrs=["3165551111", "3165552222"], date=200),
        ])
        self.assertEqual(len(self.p._deduplicated_conversations()), 2)

    def test_unread_incoming_message_marks_conversation_unread(self):
        self.p._apply_active_conversations("dev1", [
            tup(1, thread_id=10, addrs=["111"], date=100, read=0, mtype=1),
        ])
        conv = self.p._deduplicated_conversations()[0]
        self.assertFalse(conv.is_read)

    def test_opened_thread_stays_read_on_own_reply(self):
        self.p._apply_active_conversations("dev1", [
            tup(1, thread_id=10, addrs=["111"], date=100, read=1),
        ])
        conv = self.p._deduplicated_conversations()[0]
        self.p._active_thread_id = conv.thread_id
        # A sent message (type 2) on the active thread must not flip to unread.
        self.p._merge_message(sms_panel._parse_message_tuple(
            tup(2, thread_id=10, addrs=["111"], date=200, read=1, mtype=2)))
        self.assertTrue(conv.is_read)

    # ── send routing (duplicate-SMS fix) ───────────────────────────

    def test_one_to_one_send_uses_reply_to_conversation(self):
        # sendWithoutConversation is silently dropped by the phone; replyToConversation
        # is the reliable path (what KDE Connect's own SMS app uses), so an existing
        # 1:1 thread replies in-thread by conversation id — never send_sms/notification.
        self.p._apply_active_conversations("dev1", [
            tup(1, thread_id=10, addrs=["+13165551212"], date=100),
        ])
        conv = self.p._deduplicated_conversations()[0]
        self.p.client.calls.clear()
        self.p._send_text_reply(conv.thread_id, "hi there")
        names = self.p.client.call_names()
        self.assertIn("reply_to_conversation", names)
        self.assertNotIn("send_sms", names)
        self.assertNotIn("reply_to_notification", names)
        # …addressed to the conversation id, not the phone number.
        args = dict(self.p.client.calls)["reply_to_conversation"]
        self.assertEqual(args[1], conv.thread_id)

    def test_group_send_replies_in_thread(self):
        self.p._apply_active_conversations("dev1", [
            tup(1, thread_id=20, addrs=["3165551111", "3165552222"], date=100),
        ])
        conv = self.p._deduplicated_conversations()[0]
        self.assertTrue(conv.is_group)
        self.p.client.calls.clear()
        self.p._send_text_reply(conv.thread_id, "hi all")
        names = self.p.client.call_names()
        self.assertIn("reply_to_conversation", names)  # preserves recipients
        self.assertNotIn("send_sms", names)

    # ── hide / delete persistence ──────────────────────────────────

    def test_hidden_conversation_stays_hidden_until_newer_message(self):
        # Use realistic epoch-millis dates: hide stamps hidden_until with the
        # wall clock, so "newer" must be measured on that same scale.
        import time
        now = int(time.time() * 1000)
        self.p._apply_active_conversations("dev1", [
            tup(1, thread_id=10, addrs=["111"], date=now - 10_000),
        ])
        conv = self.p._deduplicated_conversations()[0]
        key = self.p._hide_conversation_locally(conv)
        self.p._purge_loaded_conversations(key)
        self.assertEqual(self.p._deduplicated_conversations(), [])

        # The same (older) message must NOT resurrect it.
        self.p._apply_active_conversations("dev1", [
            tup(1, thread_id=10, addrs=["111"], date=now - 10_000),
        ])
        self.assertEqual(self.p._deduplicated_conversations(), [])

        # A genuinely newer message brings it back.
        self.p._apply_active_conversations("dev1", [
            tup(3, thread_id=10, addrs=["111"], date=now + 60_000),
        ])
        self.assertEqual(len(self.p._deduplicated_conversations()), 1)

    # ── instant startup from the store, deterministically merged ───

    def test_cached_load_collapses_split_threads_after_restart(self):
        # Ingest a split SMS/MMS conversation and let it persist.
        self.p._apply_active_conversations("dev1", [
            tup(1, thread_id=10, addrs=["+13165551212"], date=100),
            tup(2, thread_id=20, addrs=["3165551212"], date=200),
        ])
        store_path = self.p._store._path  # in-memory shared handle
        # Simulate a fresh launch reusing the same DB handle.
        p2 = sms_panel.SmsPanel.__new__(sms_panel.SmsPanel)
        p2._index = ConversationIndex()
        p2._store = self.p._store
        p2._settings = FakeSettings()
        p2._contact_map = {}
        p2._device = self.p._device
        p2._read_thread_ids = set()
        p2._active_thread_id = None
        p2._notification_reply_targets = {}
        p2._conv_list = FakeConvList()
        p2._thread = FakeThread()
        p2.client = FakeClient()
        p2._refreshing = False
        init_sync_state(p2)

        p2._load_cached_conversations("dev1")
        self.assertEqual(len(p2._deduplicated_conversations()), 1)
        self.assertEqual(
            {m.uid for m in p2._deduplicated_conversations()[0].messages}, {1, 2}
        )
        self.assertIsInstance(store_path, str)


@unittest.skipIf(sms_panel is None, f"GTK/gi unavailable: {_IMPORT_ERR}")
class PanelCatchUpTests(unittest.TestCase):
    """Pulling texts the phone never pushed (it only pushes while connected)."""

    def setUp(self):
        self.p = make_panel()
        self.p._apply_active_conversations("dev1", [
            tup(1, thread_id=10, addrs=["111"], date=100),
        ])
        self.p.client.calls.clear()

    def tearDown(self):
        self.p._cancel_gap_fills()
        self.p._cancel_all_conversations_refresh()
        self.p._store.close()

    def drain(self):
        while self.p._drain_gap_fill() == GLib.SOURCE_CONTINUE:
            pass

    def loaded(self, thread_id, count=50):
        self.p._on_dbus_conversation_loaded(
            None, None, None, None, "conversationLoaded",
            GLib.Variant("(xt)", (thread_id, count)),
        )

    def test_newer_message_on_known_thread_pulls_the_thread(self):
        self.p._merge_message(sms_panel._parse_message_tuple(
            tup(3, thread_id=10, addrs=["111"], date=300)))
        self.drain()
        self.assertEqual(self.p.client.calls, [
            ("request_thread_from_phone", ("dev1", 10, sms_panel.GAP_FILL_MESSAGES)),
        ])

    def test_older_or_new_thread_messages_do_not_pull(self):
        self.p._merge_message(sms_panel._parse_message_tuple(
            tup(0, thread_id=10, addrs=["111"], date=50)))
        self.p._merge_message(sms_panel._parse_message_tuple(
            tup(2, thread_id=99, addrs=["222"], date=500)))
        self.assertEqual(self.p._gap_fill_queue, [])

    def test_loaded_thread_is_replayed_once(self):
        self.p._queue_gap_fill(10)
        self.drain()
        self.p.client.calls.clear()
        self.loaded(77)  # someone else's request
        self.loaded(10)
        self.loaded(10)
        self.assertEqual(self.p.client.calls, [
            ("request_conversation", ("dev1", 10, 0, sms_panel.GAP_FILL_MESSAGES)),
        ])

    def test_pending_thread_is_not_asked_twice(self):
        self.p._queue_gap_fill(10)
        self.drain()
        self.p._queue_gap_fill(10)
        self.drain()
        self.assertEqual(self.p.client.call_names(), ["request_thread_from_phone"])

    def test_opening_a_thread_pulls_it_even_when_cached(self):
        conv = self.p._deduplicated_conversations()[0]
        for uid in range(100, 130):  # well past the old "< 20 cached" cut-off
            self.p._merge_message(sms_panel._parse_message_tuple(
                tup(uid, thread_id=10, addrs=["111"], date=uid)))
        self.p._cancel_gap_fills()
        self.p._on_conversation_selected(None, conv.thread_id)
        self.assertEqual(self.p._gap_fill_queue, [conv.thread_id])

    def test_catch_up_pulls_everything_and_the_open_thread(self):
        self.p._active_thread_id = 10
        self.p.catch_up(delay_seconds=0, min_interval=0)
        self.assertIsNotNone(self.p._all_conversations_refresh_source)
        self.p._run_all_conversations_refresh("dev1")
        self.assertEqual(self.p.client.call_names(), ["request_all_conversations"])
        self.assertEqual(self.p._gap_fill_queue, [10])

    def test_catch_up_bursts_collapse(self):
        self.p._run_all_conversations_refresh("dev1")
        self.p.catch_up()
        self.assertIsNone(self.p._all_conversations_refresh_source)
        self.p.catch_up(min_interval=0)
        self.assertIsNotNone(self.p._all_conversations_refresh_source)

    # ── paging back through a thread ──────────────────────────────

    def _fill_page(self, uids, date0):
        for i, uid in enumerate(uids):
            self.p._merge_message(sms_panel._parse_message_tuple(
                tup(uid, thread_id=10, addrs=["111"], date=date0 + i)))

    def test_full_page_of_new_messages_pulls_the_next_older_page(self):
        self.p._queue_gap_fill(10)
        self.drain()
        self.loaded(10)
        self.p.client.calls.clear()
        self._fill_page(range(1000, 1000 + sms_panel.GAP_FILL_MESSAGES), 5000)
        self.p._check_gap_fill_page(10)
        self.assertEqual(self.p.client.calls, [
            ("request_thread_from_phone",
             ("dev1", 10, sms_panel.GAP_FILL_MESSAGES, 5000)),
        ])
        # The second page is replayed from where the first ended.
        self.p.client.calls.clear()
        self.loaded(10)
        n = sms_panel.GAP_FILL_MESSAGES
        self.assertEqual(self.p.client.calls, [
            ("request_conversation", ("dev1", 10, n - 10, 2 * n)),
        ])

    def test_page_that_reaches_known_messages_stops(self):
        self.p._queue_gap_fill(10)
        self.drain()
        self.loaded(10)
        self.p.client.calls.clear()
        self._fill_page(range(1000, 1010), 5000)  # a short page: thread start
        self.p._check_gap_fill_page(10)
        self.assertEqual(self.p.client.calls, [])
        self.assertNotIn(10, self.p._gap_fill_pages)

    def test_paging_stops_at_the_page_limit(self):
        self.p._queue_gap_fill(10)
        self.drain()
        self.p._gap_fill_pages[10]["pages"] = sms_panel.GAP_FILL_MAX_PAGES
        self._fill_page(range(1000, 1000 + sms_panel.GAP_FILL_MESSAGES), 5000)
        self.p.client.calls.clear()
        self.p._check_gap_fill_page(10)
        self.assertEqual(self.p.client.calls, [])

    # ── incoming signals are merged in batches ─────────────────────

    def test_signals_drain_in_batches(self):
        handled = []
        self.p._handle_signal_variant = handled.append
        n = sms_panel.SIGNAL_BATCH_SIZE
        self.p._signal_queue.extend(range(n + 5))
        self.assertEqual(self.p._drain_signal_queue(), GLib.SOURCE_CONTINUE)
        self.assertEqual(len(handled), n)
        self.p._signal_drain_scheduled = True
        self.assertEqual(self.p._drain_signal_queue(), GLib.SOURCE_REMOVE)
        self.assertEqual(len(handled), n + 5)
        self.assertFalse(self.p._signal_drain_scheduled)

    def test_catch_up_waits_for_the_phone(self):
        self.p._device.reachable = False
        self.p.catch_up(min_interval=0)
        self.assertIsNone(self.p._all_conversations_refresh_source)


if __name__ == "__main__":
    unittest.main()
