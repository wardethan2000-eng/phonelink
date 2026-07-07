"""Loom transport for SMS reads — the KDE Connect replacement (loom ``docs/12`` P5, read-only).

:class:`LoomSmsClient` mirrors the *read* slice of KDE Connect's conversations surface, but over the
Loom fabric: it resolves the phone's device node-id from the local ``loomd`` realm and pulls SMS
conversations / thread messages via the SDK (`loom/phone-action/0`), mapping each onto phonelink's
:class:`~phonelink.models.SmsMessage` so the SMS panel ingests them exactly like a KDE Connect message
(via ``SmsPanel._merge_message``). Because loomd dials the phone **by node-id**, this works off-LAN with
no KDE Connect on the network.

Read-only in this pass: listing conversations + messages. SMS **send** is a follow-on (it adds a
``sms.send`` action + the ``SEND_SMS`` permission on the phone). Every call blocks (a Unix socket + the
network), so drive it off the GTK main loop.
"""

from __future__ import annotations

from typing import Callable, List, Optional

from phonelink import loom_bridge
from phonelink.models import SmsMessage

# Same phone-labelling hints the notification transport uses to pick the phone among realm devices.
_PHONE_HINTS = ("phone", "android", "pixel", "galaxy", "samsung", "oneplus", "s25", "moto", "nexus")


class LoomSmsClient:
    """A read-only SMS source backed by a phone's Loom node.

    Parameters mirror :class:`~phonelink.loom_phone.LoomPhoneClient`: ``loom_factory`` returns a fresh
    ``loom_sdk.Loom`` (defaults to :func:`phonelink.loom_bridge.connect`); ``preferred_device`` pins an
    explicit phone device node-id, otherwise it's auto-resolved from the realm.
    """

    def __init__(self, loom_factory: Optional[Callable] = None, preferred_device: str = ""):
        self._loom_factory = loom_factory or loom_bridge.connect
        self._preferred_device = preferred_device

    @staticmethod
    def available() -> bool:
        """True if the Loom SDK is installed and the local loomd is reachable."""
        if not loom_bridge.sdk_available():
            return False
        try:
            return loom_bridge.connect().is_available()
        except Exception:  # noqa: BLE001
            return False

    def _resolve_phone_device(self, loom) -> Optional[str]:
        """Pick the phone's device node-id from the realm: the device that isn't us, preferring an
        explicit id, then a phone-looking label, then the sole other device."""
        me = ""
        try:
            me = loom.status().get("device_id", "")
        except Exception:  # noqa: BLE001
            pass
        devices = loom.members().get("devices", [])
        others = [d for d in devices if d.get("device") and d.get("device") != me]
        if self._preferred_device:
            for d in others:
                if d["device"] == self._preferred_device:
                    return d["device"]
        for d in others:
            if any(h in (d.get("label", "") or "").lower() for h in _PHONE_HINTS):
                return d["device"]
        return others[0]["device"] if others else None

    def _device(self, loom) -> str:
        device = self._resolve_phone_device(loom)
        if not device:
            raise loom_bridge.LoomError("no phone in your Loom realm to read SMS from")
        return device

    def conversations(self) -> List[SmsMessage]:
        """The latest message of each SMS conversation on the phone, as :class:`SmsMessage`\\ s ready to
        feed the SMS panel's ingest. Blocking; raises ``LoomError`` if the phone is unreachable, refuses
        us, or lacks SMS support. The panel merges these by canonical conversation identity."""
        loom = self._loom_factory()
        return [SmsMessage.from_loom(d) for d in loom.sms_conversations(self._device(loom))]

    def messages(self, thread_id, limit: int = 200) -> List[SmsMessage]:
        """Messages in one SMS thread (newest first, up to ``limit``), as :class:`SmsMessage`\\ s."""
        loom = self._loom_factory()
        return [
            SmsMessage.from_loom(d)
            for d in loom.sms_messages(self._device(loom), str(thread_id), limit)
        ]

    def send(self, addresses: List[str], text: str) -> None:
        """Send an SMS from the phone to ``addresses`` over Loom (P5). **Sends a real text.** Blocking;
        raises ``LoomError`` if the phone is unreachable, refuses us, or lacks SEND_SMS. ``addresses``
        with more than one recipient is a group send."""
        loom = self._loom_factory()
        loom.send_sms(self._device(loom), list(addresses), text)
