"""Tests for the Loom notification transport (LoomPhoneClient).

Drives the client against a fake ``loom_sdk.Loom`` so no loomd is required: it must resolve the phone's
device node-id from the realm and deliver snapshot / posted / removed through its callbacks (mapped to
phonelink's ``Notification`` model).
"""

import threading

from loom_sdk import Notification as SdkNotification
from loom_sdk import NotificationEvent

from phonelink.loom_phone import LoomPhoneClient
from phonelink.models import Notification


class FakeStream:
    def __init__(self, events):
        self._events = list(events)
        self._i = 0
        self.closed = False

    def __iter__(self):
        return self

    def __next__(self):
        if self.closed or self._i >= len(self._events):
            raise StopIteration
        ev = self._events[self._i]
        self._i += 1
        return ev

    def close(self):
        self.closed = True


class FakeLoom:
    def __init__(self, devices, events=None, me="me-id"):
        self._devices = devices
        self._events = events or []
        self._me = me
        self.subscribed = None
        self.subscribe_count = 0

    def status(self):
        return {"device_id": self._me}

    def members(self):
        return {"users": [], "devices": self._devices}

    def subscribe_notifications(self, device_id):
        self.subscribed = device_id
        self.subscribe_count += 1
        # Only the first subscribe carries events; a reconnect gets an empty (immediately-ending)
        # stream so the test isn't sensitive to reconnect timing.
        return FakeStream(self._events if self.subscribe_count == 1 else [])


# ── device resolution ──────────────────────────────────────────────────────────────────────────


def test_resolve_prefers_phone_labelled_device():
    loom = FakeLoom(
        devices=[{"device": "laptop", "label": "desktop"}, {"device": "phone", "label": "Pixel 8"}],
        me="laptop",
    )
    client = LoomPhoneClient(loom_factory=lambda: loom)
    assert client._resolve_phone_device(loom) == "phone"


def test_resolve_picks_the_sole_other_device():
    loom = FakeLoom(
        devices=[{"device": "me", "label": "a"}, {"device": "other", "label": "b"}],
        me="me",
    )
    client = LoomPhoneClient(loom_factory=lambda: loom)
    assert client._resolve_phone_device(loom) == "other"


def test_resolve_honours_explicit_preferred_device():
    loom = FakeLoom(
        devices=[{"device": "p1", "label": "Galaxy"}, {"device": "p2", "label": "Pixel"}],
        me="me",
    )
    client = LoomPhoneClient(loom_factory=lambda: loom, preferred_device="p2")
    assert client._resolve_phone_device(loom) == "p2"


def test_resolve_none_when_only_self():
    loom = FakeLoom(devices=[{"device": "me", "label": "x"}], me="me")
    client = LoomPhoneClient(loom_factory=lambda: loom)
    assert client._resolve_phone_device(loom) is None


# ── streaming lifecycle ─────────────────────────────────────────────────────────────────────────


def test_start_delivers_snapshot_posted_removed():
    events = [
        NotificationEvent(
            "snapshot",
            notifications=[SdkNotification(public_id="a", app_name="WhatsApp", title="Alice", text="hi")],
        ),
        NotificationEvent("posted", notification=SdkNotification(public_id="b", title="Bob", text="yo")),
        NotificationEvent("removed", public_id="a"),
    ]
    loom = FakeLoom(devices=[{"device": "phone-id", "label": "Galaxy S25"}], events=events, me="laptop")
    client = LoomPhoneClient(loom_factory=lambda: loom)

    got = {"snapshot": None, "posted": [], "removed": []}
    done = threading.Event()

    def on_removed(pid):
        got["removed"].append(pid)
        done.set()

    client.start(
        on_snapshot=lambda ns: got.__setitem__("snapshot", ns),
        on_posted=lambda n: got["posted"].append(n),
        on_removed=on_removed,
    )
    try:
        assert done.wait(5), "did not receive the removed event in time"
    finally:
        client.stop()

    assert loom.subscribed == "phone-id"
    # Snapshot mapped to phonelink Notifications.
    assert got["snapshot"] and isinstance(got["snapshot"][0], Notification)
    assert got["snapshot"][0].public_id == "a" and got["snapshot"][0].title == "Alice"
    assert any(n.public_id == "b" and n.title == "Bob" for n in got["posted"])
    assert "a" in got["removed"]


def test_stop_is_idempotent_and_safe_without_start():
    client = LoomPhoneClient(loom_factory=lambda: FakeLoom(devices=[]))
    client.stop()  # never started — must not raise
    client.stop()
