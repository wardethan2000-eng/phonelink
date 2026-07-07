"""Tests for the Loom SMS read transport (LoomSmsClient) + the SmsMessage.from_loom mapping.

Drives the client against a fake ``loom_sdk.Loom`` so no loomd is required: it must resolve the phone's
device node-id and return conversations / thread messages mapped onto phonelink's ``SmsMessage`` model
(the same model the SMS panel ingests via ``_merge_message``).
"""

import pytest

from phonelink import loom_bridge
from phonelink.loom_sms import LoomSmsClient
from phonelink.models import SmsMessage


class FakeLoom:
    def __init__(self, devices, convos=None, messages=None, me="me-id"):
        self._devices = devices
        self._convos = convos or []
        self._messages = messages or []
        self._me = me
        self.msg_query = None

    def status(self):
        return {"device_id": self._me}

    def members(self):
        return {"users": [], "devices": self._devices}

    def sms_conversations(self, device_id):
        self.convo_device = device_id
        return self._convos

    def sms_messages(self, device_id, thread_id, limit=200):
        self.msg_query = (device_id, thread_id, limit)
        return self._messages

    def send_sms(self, device_id, addresses, text):
        self.sent = (device_id, list(addresses), text)


# ── SmsMessage.from_loom mapping ─────────────────────────────────────────────────────────────────


def test_from_loom_maps_all_fields():
    d = {"thread_id": "42", "address": "+15551234567", "body": "hey", "date": 1751740000000,
         "type": 2, "read": 0, "uid": 9}
    m = SmsMessage.from_loom(d)
    assert isinstance(m, SmsMessage)
    assert m.thread_id == 42 and isinstance(m.thread_id, int)  # coerced from str
    assert m.address == "+15551234567"
    assert m.body == "hey"
    assert m.date == 1751740000000
    assert m.msg_type == 2 and m.is_sent
    assert m.read == 0


def test_from_loom_group_addresses_attached():
    d = {"thread_id": "5", "address": "+1555000", "body": "hi",
         "addresses": ["+1555000", "+1555111"]}
    m = SmsMessage.from_loom(d)
    assert getattr(m, "_all_addresses", None) == ["+1555000", "+1555111"]


def test_from_loom_tolerates_missing_fields():
    m = SmsMessage.from_loom({"thread_id": "1"})
    assert m.thread_id == 1 and m.body == "" and m.msg_type == 1 and m.read == 1


# ── LoomSmsClient ────────────────────────────────────────────────────────────────────────────────


def test_conversations_maps_to_sms_messages():
    loom = FakeLoom(
        devices=[{"device": "laptop", "label": "desktop"}, {"device": "phone", "label": "Galaxy S25"}],
        convos=[{"thread_id": "7", "address": "+15550001111", "body": "yo", "date": 1, "type": 1, "read": 1}],
        me="laptop",
    )
    client = LoomSmsClient(loom_factory=lambda: loom)
    convos = client.conversations()
    assert loom.convo_device == "phone"
    assert len(convos) == 1 and isinstance(convos[0], SmsMessage)
    assert convos[0].thread_id == 7 and convos[0].address == "+15550001111"


def test_messages_passes_thread_and_limit_as_str():
    loom = FakeLoom(
        devices=[{"device": "laptop", "label": "d"}, {"device": "phone", "label": "Pixel"}],
        messages=[{"thread_id": "7", "body": "m1", "type": 2}],
        me="laptop",
    )
    client = LoomSmsClient(loom_factory=lambda: loom)
    msgs = client.messages(7, limit=25)
    assert loom.msg_query == ("phone", "7", 25)  # thread_id coerced to str for the wire
    assert msgs[0].thread_id == 7 and msgs[0].is_sent


def test_send_targets_resolved_phone():
    loom = FakeLoom(
        devices=[{"device": "laptop", "label": "d"}, {"device": "phone", "label": "Galaxy"}],
        me="laptop",
    )
    client = LoomSmsClient(loom_factory=lambda: loom)
    client.send(["+15551234567"], "omw")
    assert loom.sent == ("phone", ["+15551234567"], "omw")


def test_raises_when_no_phone_in_realm():
    loom = FakeLoom(devices=[{"device": "me", "label": "x"}], me="me")
    client = LoomSmsClient(loom_factory=lambda: loom)
    with pytest.raises(loom_bridge.LoomError):
        client.conversations()
