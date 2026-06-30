"""Local SQLite store for conversations and messages.

Makes startup instant and message history durable.  Without it, the app
re-derives everything from the KDE Connect daemon's volatile cache on every
launch (slow, non-deterministic, empty on first run).  With it, the UI loads
from the local database immediately and the daemon becomes a *sync source*:
incoming messages are upserted here and survive restarts.

The store is gi-free (pure ``sqlite3`` + the data models) so it can be unit
tested without a GTK runtime.  All access is guarded by a lock and the
connection is opened with ``check_same_thread=False`` so it is safe to call
from the async worker pool as well as the main thread.
"""

from __future__ import annotations

import json
import sqlite3
import threading
from pathlib import Path

from phonelink.models import Conversation, SmsMessage

STORE_DIR = Path.home() / ".local" / "share" / "phonelink"
STORE_FILE = STORE_DIR / "messages.db"


class MessageStore:
    """SQLite-backed persistence for conversations and their messages."""

    def __init__(self, path: str | Path | None = None):
        self._path = str(path or STORE_FILE)
        self._lock = threading.Lock()
        self._conn: sqlite3.Connection | None = None
        self._open()

    # ── lifecycle ──────────────────────────────────────────────────

    def _open(self):
        try:
            if self._path != ":memory:":
                Path(self._path).parent.mkdir(parents=True, exist_ok=True)
            self._conn = sqlite3.connect(self._path, check_same_thread=False)
            self._conn.execute("PRAGMA journal_mode=WAL")
            self._conn.execute("PRAGMA synchronous=NORMAL")
            self._create_schema()
        except sqlite3.Error as exc:
            print(f"[phonelink] message store unavailable: {exc}")
            self._conn = None

    def _create_schema(self):
        assert self._conn is not None
        self._conn.executescript(
            """
            CREATE TABLE IF NOT EXISTS conversations (
                device_id     TEXT    NOT NULL,
                thread_id     INTEGER NOT NULL,
                address       TEXT,
                addresses     TEXT,
                display_name  TEXT,
                last_message  TEXT,
                last_date     INTEGER,
                is_read       INTEGER,
                PRIMARY KEY (device_id, thread_id)
            );
            CREATE TABLE IF NOT EXISTS messages (
                device_id   TEXT    NOT NULL,
                uid         INTEGER NOT NULL,
                thread_id   INTEGER,
                address     TEXT,
                body        TEXT,
                date        INTEGER,
                msg_type    INTEGER,
                read        INTEGER,
                attachments TEXT,
                PRIMARY KEY (device_id, uid)
            );
            CREATE INDEX IF NOT EXISTS idx_messages_thread
                ON messages (device_id, thread_id);
            """
        )
        self._conn.commit()

    def close(self):
        with self._lock:
            if self._conn is not None:
                try:
                    self._conn.close()
                finally:
                    self._conn = None

    # ── writes ─────────────────────────────────────────────────────

    def upsert_message(self, device_id: str, thread_id: int, msg: SmsMessage):
        """Persist a single message under ``thread_id`` (the primary thread)."""
        if self._conn is None or thread_id < 0:
            return
        with self._lock:
            try:
                self._conn.execute(
                    """
                    INSERT OR REPLACE INTO messages
                        (device_id, uid, thread_id, address, body, date,
                         msg_type, read, attachments)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                    """,
                    (
                        device_id, int(msg.uid), int(thread_id), msg.address,
                        msg.body, int(msg.date), int(msg.msg_type),
                        int(msg.read), json.dumps(msg.attachments or []),
                    ),
                )
                self._conn.commit()
            except sqlite3.Error as exc:
                print(f"[phonelink] store.upsert_message failed: {exc}")

    def upsert_conversation(self, device_id: str, conv: Conversation):
        """Persist a conversation's metadata (not its messages)."""
        if self._conn is None or conv.thread_id < 0:
            return
        with self._lock:
            try:
                self._conn.execute(
                    """
                    INSERT OR REPLACE INTO conversations
                        (device_id, thread_id, address, addresses,
                         display_name, last_message, last_date, is_read)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                    """,
                    (
                        device_id, int(conv.thread_id), conv.address,
                        json.dumps(conv.addresses or []), conv.display_name,
                        conv.last_message, int(conv.last_date or 0),
                        1 if conv.is_read else 0,
                    ),
                )
                self._conn.commit()
            except sqlite3.Error as exc:
                print(f"[phonelink] store.upsert_conversation failed: {exc}")

    def upsert_conversations(self, device_id: str, convs):
        """Persist many conversation metadata rows in one transaction."""
        if self._conn is None:
            return
        rows = [
            (
                device_id, int(c.thread_id), c.address,
                json.dumps(c.addresses or []), c.display_name,
                c.last_message, int(c.last_date or 0),
                1 if c.is_read else 0,
            )
            for c in convs if c.thread_id >= 0
        ]
        if not rows:
            return
        with self._lock:
            try:
                self._conn.executemany(
                    """
                    INSERT OR REPLACE INTO conversations
                        (device_id, thread_id, address, addresses,
                         display_name, last_message, last_date, is_read)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                    """,
                    rows,
                )
                self._conn.commit()
            except sqlite3.Error as exc:
                print(f"[phonelink] store.upsert_conversations failed: {exc}")

    def delete_conversation(self, device_id: str, thread_id: int):
        """Remove a conversation and all of its messages."""
        if self._conn is None:
            return
        with self._lock:
            try:
                self._conn.execute(
                    "DELETE FROM conversations WHERE device_id=? AND thread_id=?",
                    (device_id, int(thread_id)),
                )
                self._conn.execute(
                    "DELETE FROM messages WHERE device_id=? AND thread_id=?",
                    (device_id, int(thread_id)),
                )
                self._conn.commit()
            except sqlite3.Error as exc:
                print(f"[phonelink] store.delete_conversation failed: {exc}")

    # ── reads ──────────────────────────────────────────────────────

    def load_conversations(self, device_id: str) -> dict[int, Conversation]:
        """Return ``{thread_id: Conversation}`` with messages attached."""
        if self._conn is None:
            return {}
        with self._lock:
            try:
                conv_rows = self._conn.execute(
                    """
                    SELECT thread_id, address, addresses, display_name,
                           last_message, last_date, is_read
                    FROM conversations WHERE device_id=?
                    """,
                    (device_id,),
                ).fetchall()
                msg_rows = self._conn.execute(
                    """
                    SELECT uid, thread_id, address, body, date, msg_type,
                           read, attachments
                    FROM messages WHERE device_id=? ORDER BY date ASC
                    """,
                    (device_id,),
                ).fetchall()
            except sqlite3.Error as exc:
                print(f"[phonelink] store.load_conversations failed: {exc}")
                return {}

        convs: dict[int, Conversation] = {}
        for (thread_id, address, addresses, display_name,
             last_message, last_date, is_read) in conv_rows:
            convs[thread_id] = Conversation(
                thread_id=thread_id,
                address=address or "",
                addresses=_loads_list(addresses),
                display_name=display_name or "",
                last_message=last_message or "",
                last_date=last_date or 0,
                is_read=bool(is_read),
            )

        for (uid, thread_id, address, body, date, msg_type,
             read, attachments) in msg_rows:
            msg = SmsMessage(
                uid=uid,
                body=body or "",
                address=address or "",
                date=date or 0,
                msg_type=msg_type or 0,
                read=read if read is not None else 1,
                thread_id=thread_id or 0,
                attachments=_loads_list(attachments),
            )
            conv = convs.get(thread_id)
            if conv is None:
                # Orphan message (conversation row missing) — synthesize one.
                conv = Conversation(
                    thread_id=thread_id,
                    address=msg.address,
                    addresses=[msg.address] if msg.address else [],
                    last_message=msg.body,
                    last_date=msg.date,
                    is_read=bool(msg.read),
                )
                convs[thread_id] = conv
            conv.messages.append(msg)

        return convs


def _loads_list(value) -> list:
    if not value:
        return []
    try:
        data = json.loads(value)
        return data if isinstance(data, list) else []
    except (ValueError, TypeError):
        return []


# ── module singleton ───────────────────────────────────────────────

_store_instance: MessageStore | None = None


def get_message_store() -> MessageStore:
    global _store_instance
    if _store_instance is None:
        _store_instance = MessageStore()
    return _store_instance
