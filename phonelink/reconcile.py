"""Canonical SMS conversation reconciliation (gi-free, unit-testable).

The old panel reconciled conversations with *render-time* heuristics that ran on
every refresh and mutated shared state as a side effect — the direct source of
the "inconsistent" feeling (threads splitting/merging, names flickering, unread
toggling, duplicate drafts).

This module normalizes conversations **once, on ingest**, into a stable local
model.  Every message is keyed by a canonical *participant-set identity* (the
sorted set of last-10-digit phone keys, with the user's own number removed for
group threads).  Two phone thread IDs that share an identity — e.g. an SMS and
an MMS thread for the same contact, or a dual-SIM split — collapse into one
``Conversation`` the moment their messages land, and stay collapsed.

Nothing here imports ``gi``/GTK, so the reconciliation logic can be exercised by
plain unit tests without a display or ``kdeconnectd``.
"""

from __future__ import annotations

from collections import Counter
from dataclasses import dataclass
from typing import Callable, Iterable

from phonelink.contacts import _normalize_phone
from phonelink.models import Conversation, SmsMessage

# A callable that turns (addresses, primary_address) into a display name.
NameResolver = Callable[[list[str], str], str]


def phone_key(number: str) -> str:
    """Return the comparison key for a phone number (last 10 digits)."""
    norm = _normalize_phone(number or "")
    return norm[-10:] if len(norm) >= 10 else norm


def participant_keys(
    addresses: Iterable[str] | None,
    fallback_address: str = "",
    self_key: str = "",
) -> list[str]:
    """Ordered, de-duplicated participant keys, with self removed for groups."""
    keys: list[str] = []
    for raw in list(addresses or []) + ([fallback_address] if fallback_address else []):
        key = phone_key(raw)
        if key and key not in keys:
            keys.append(key)
    if self_key and len(keys) > 1:
        stripped = [k for k in keys if k != self_key]
        if stripped:
            keys = stripped
    return keys


def participant_addresses(
    addresses: Iterable[str] | None,
    fallback_address: str = "",
    self_key: str = "",
) -> list[str]:
    """Original address strings, de-duplicated by key, with self removed."""
    result: list[str] = []
    seen: list[str] = []
    for raw in list(addresses or []) + ([fallback_address] if fallback_address else []):
        key = phone_key(raw)
        if not key or key in seen:
            continue
        seen.append(key)
        result.append(raw)
    if self_key and len(seen) > 1:
        stripped = [raw for raw, key in zip(result, seen) if key != self_key]
        if stripped:
            result = stripped
    return result


def conversation_identity(
    addresses: Iterable[str] | None,
    fallback_address: str = "",
    self_key: str = "",
) -> str:
    """Canonical participant-set key for a conversation.

    Deterministic: the same participants always produce the same string, so it
    is safe to use as a stable dictionary key and as the settings key for
    hidden/deleted conversations.
    """
    return "|".join(sorted(participant_keys(addresses, fallback_address, self_key)))


def detect_self_key(
    message_key_lists: Iterable[Iterable[str]],
    contact_keys: Iterable[str],
    *,
    min_appearances: int = 3,
    top: int = 5,
) -> str:
    """Guess the user's own number key from parsed conversation data.

    In MMS-style threads the phone includes the user's own number in the
    address list.  We spot it as a key that (1) appears in several *multi*-party
    threads, (2) never appears as a *single*-party thread of its own, and (3)
    is not a known contact.
    """
    multi: Counter[str] = Counter()
    single: set[str] = set()
    for keys in message_key_lists:
        uniq = list(dict.fromkeys(k for k in keys if k))
        if len(uniq) == 1:
            single.add(uniq[0])
        elif len(uniq) >= 2:
            for key in set(uniq):
                multi[key] += 1

    contact_key_set = {
        (k[-10:] if len(k) >= 10 else k) for k in contact_keys
    }
    for num, count in multi.most_common(top):
        if count < min_appearances:
            break
        if num in single or num in contact_key_set:
            continue
        return num
    return ""


@dataclass
class IngestResult:
    """What ``ConversationIndex.ingest`` did, so the caller can react."""

    conversation: Conversation
    created: bool         # a brand-new Conversation was created
    message_added: bool   # a non-duplicate message was appended
    is_latest: bool       # message advanced (or tied) the conversation's newest date
    is_newer: bool        # message is strictly newer than the previous newest


class ConversationIndex:
    """Owns the merged conversation model, keyed by canonical identity.

    ``conversations`` maps a stable *primary* thread ID to its merged
    ``Conversation``.  ``thread_to_primary`` maps *any* phone thread ID
    (primary or secondary) to its primary, replacing the old
    ``_thread_redirects`` heuristic.  All merging happens here, on ingest — the
    render path just reads ``visible()``.
    """

    def __init__(self) -> None:
        self.conversations: dict[int, Conversation] = {}
        self.identity_to_primary: dict[str, int] = {}
        self.thread_to_primary: dict[int, int] = {}
        self.self_key: str = ""

    # ── lookups ────────────────────────────────────────────────────

    def primary_for(self, thread_id: int) -> int:
        return self.thread_to_primary.get(thread_id, thread_id)

    def get(self, thread_id: int) -> Conversation | None:
        return self.conversations.get(self.primary_for(thread_id))

    def secondary_threads(self, primary: int) -> list[int]:
        return [
            tid for tid, p in self.thread_to_primary.items()
            if p == primary and tid != primary
        ]

    def visible(self, is_hidden: Callable[[Conversation], bool] | None = None) -> list[Conversation]:
        """Return conversations for display — a pure, side-effect-free filter."""
        convs = self.conversations.values()
        if is_hidden is None:
            return list(convs)
        return [c for c in convs if not is_hidden(c)]

    # ── mutation ───────────────────────────────────────────────────

    def clear(self) -> None:
        self.conversations.clear()
        self.identity_to_primary.clear()
        self.thread_to_primary.clear()

    def set_self_key(self, key: str) -> bool:
        """Set the detected self number; return True if it changed."""
        if key == self.self_key:
            return False
        self.self_key = key
        return True

    def ingest(
        self,
        msg: SmsMessage,
        all_addresses: list[str] | None,
        name_for: NameResolver,
    ) -> IngestResult:
        """Merge a parsed message into its canonical conversation."""
        raw_addrs = all_addresses or ([msg.address] if msg.address else [])
        identity = conversation_identity(raw_addrs, msg.address, self.self_key)
        addrs = participant_addresses(raw_addrs, msg.address, self.self_key)

        primary = self.identity_to_primary.get(identity)
        created = False
        if primary is None:
            primary = msg.thread_id
            conv = Conversation(
                thread_id=primary,
                identity=identity,
                address=addrs[0] if addrs else msg.address,
                addresses=list(addrs),
                display_name=name_for(addrs, addrs[0] if addrs else msg.address),
                thread_ids=[primary],
            )
            self.conversations[primary] = conv
            self.identity_to_primary[identity] = primary
            created = True

        conv = self.conversations[primary]

        # Record the (possibly secondary) phone thread this message came from.
        self.thread_to_primary[msg.thread_id] = primary
        if msg.thread_id not in conv.thread_ids:
            conv.thread_ids.append(msg.thread_id)

        # A later message can reveal more participants than we first saw.
        if len(addrs) > len(conv.addresses):
            conv.addresses = list(addrs)
            conv.address = addrs[0] if addrs else conv.address
            conv.display_name = name_for(conv.addresses, conv.address)

        message_added = False
        if msg.uid and all(m.uid != msg.uid for m in conv.messages):
            conv.messages.append(msg)
            message_added = True

        prev_last = conv.last_date
        is_latest = msg.date >= prev_last
        is_newer = msg.date > prev_last
        if is_latest:
            conv.last_date = msg.date
            conv.last_message = msg.body

        return IngestResult(conv, created, message_added, is_latest, is_newer)

    def register(self, conv: Conversation) -> Conversation:
        """Insert an already-built conversation (e.g. from the local store).

        Recomputes the identity and merges into an existing conversation if one
        already claims it, so legacy duplicates collapse deterministically.
        Returns the surviving (primary) conversation.
        """
        conv.identity = conversation_identity(conv.addresses, conv.address, self.self_key)
        if not conv.thread_ids:
            conv.thread_ids = [conv.thread_id]

        existing = self.identity_to_primary.get(conv.identity)
        if existing is None:
            self.conversations[conv.thread_id] = conv
            self.identity_to_primary[conv.identity] = conv.thread_id
            for tid in conv.thread_ids:
                self.thread_to_primary[tid] = conv.thread_id
            self.thread_to_primary[conv.thread_id] = conv.thread_id
            return conv

        primary = self.conversations[existing]
        self._absorb(primary, conv)
        return primary

    def reindex(self) -> None:
        """Rebuild all identities and merges (after the self key changes)."""
        convs = list(self.conversations.values())
        self.clear()
        for conv in sorted(convs, key=lambda c: c.thread_id):
            self.register(conv)

    def remove(self, thread_id: int) -> list[int]:
        """Drop a conversation (by any of its thread IDs); return removed IDs."""
        primary = self.primary_for(thread_id)
        conv = self.conversations.pop(primary, None)
        if conv is None:
            self.thread_to_primary.pop(thread_id, None)
            return []
        self.identity_to_primary.pop(conv.identity, None)
        removed = [tid for tid, p in list(self.thread_to_primary.items()) if p == primary]
        for tid in removed:
            self.thread_to_primary.pop(tid, None)
        self.thread_to_primary.pop(primary, None)
        if primary not in removed:
            removed.append(primary)
        return removed

    # ── internals ──────────────────────────────────────────────────

    def _absorb(self, primary: Conversation, other: Conversation) -> None:
        existing = {m.uid for m in primary.messages if m.uid}
        for m in other.messages:
            if m.uid and m.uid not in existing:
                primary.messages.append(m)
                existing.add(m.uid)
        for tid in (other.thread_ids or [other.thread_id]):
            if tid not in primary.thread_ids:
                primary.thread_ids.append(tid)
            self.thread_to_primary[tid] = primary.thread_id
        self.thread_to_primary[other.thread_id] = primary.thread_id
        if other.last_date > primary.last_date:
            primary.last_date = other.last_date
            primary.last_message = other.last_message
        if not other.is_read:
            primary.is_read = False
