"""Notice when the phone is hiding texts from KDE Connect.

Android 17 marks most new messages (end-to-end-encrypted RCS especially) as
``restricted`` in the phone's message store and hides them from every app that
is not the default texting app, unless the app holds the
``READ_RESTRICTED_MESSAGES`` app-op.  KDE Connect does not get it by default,
so those texts never reach the desktop and nothing reports an error.

The texting app's *notifications* still come through, though.  A text whose
notification arrives but whose message never does, twice within a few hours,
means the phone is hiding messages.  This module holds that bookkeeping; it has
no GTK or D-Bus dependency so it can be tested directly.
"""

from __future__ import annotations

import re
from typing import Callable

# Shown in the warning; run with the phone plugged in and USB debugging on.
FIX_COMMAND = (
    "adb shell appops set org.kde.kdeconnect_tp READ_RESTRICTED_MESSAGES allow"
)

SETTLE_SECONDS = 120          # give the message this long to arrive
MISS_WINDOW_SECONDS = 6 * 3600
MISSES_TO_WARN = 2
MIN_TEXT_LENGTH = 8           # shorter previews match too loosely

# Previews that stand in for media rather than quoting the message body.
_GENERIC_PREVIEWS = {
    "photo", "image", "picture", "video", "sticker", "gif", "audio",
    "voice message", "sent a photo", "sent an image", "sent a video",
    "sent a sticker", "sent a gif", "attachment", "location", "contact",
}


def normalize_text(text: str) -> str:
    """Lower-case, single-spaced, without a trailing ellipsis."""
    t = re.sub(r"\s+", " ", (text or "")).strip().casefold()
    return re.sub(r"(…|\.\.\.)$", "", t).strip()


def body_matches(preview: str, body: str) -> bool:
    """Whether a notification preview quotes this message body.

    Previews can be truncated, or prefixed with the sender in group chats.
    """
    p, b = normalize_text(preview), normalize_text(body)
    if not p or not b:
        return False
    return p == b or (len(b) >= MIN_TEXT_LENGTH and b in p) or b.startswith(p)


class HiddenTextDetector:
    def __init__(self, *, settle_seconds: float = SETTLE_SECONDS,
                 miss_window_seconds: float = MISS_WINDOW_SECONDS,
                 misses_to_warn: int = MISSES_TO_WARN):
        self.settle_seconds = settle_seconds
        self.miss_window_seconds = miss_window_seconds
        self.misses_to_warn = misses_to_warn
        self._pending: list[tuple[float, str]] = []
        self._misses: list[float] = []
        self.warning = False

    @property
    def has_pending(self) -> bool:
        return bool(self._pending)

    def note_notification(self, preview: str, now: float) -> bool:
        """Remember a texting-app notification.  Returns True if it is tracked."""
        text = normalize_text(preview)
        if len(text) < MIN_TEXT_LENGTH or text in _GENERIC_PREVIEWS:
            return False
        if any(t == text for _, t in self._pending):
            return False
        self._pending.append((now, text))
        return True

    def check(self, now: float, arrived: Callable[[str, float], bool]) -> bool:
        """Resolve notifications old enough to judge.

        ``arrived(preview, posted_at)`` says whether a matching message reached
        the desktop.  Returns True when :attr:`warning` changed.
        """
        before = self.warning
        still_waiting = []
        for posted_at, text in self._pending:
            if now - posted_at < self.settle_seconds:
                still_waiting.append((posted_at, text))
            elif arrived(text, posted_at):
                # Texts are getting through again.
                self._misses.clear()
                self.warning = False
            else:
                self._misses.append(now)
        self._pending = still_waiting
        self._misses = [m for m in self._misses if now - m <= self.miss_window_seconds]
        if len(self._misses) >= self.misses_to_warn:
            self.warning = True
        return self.warning != before
