"""Data models for Phone Link."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime


@dataclass
class Device:
    """Represents a paired / known KDE Connect device."""

    id: str
    name: str = "Unknown Device"
    type: str = "phone"
    reachable: bool = False
    paired: bool = False
    battery_charge: int = -1
    battery_charging: bool = False

    @property
    def status_label(self) -> str:
        if not self.paired:
            return "Not Paired"
        return "Connected" if self.reachable else "Disconnected"

    @property
    def battery_label(self) -> str:
        if self.battery_charge < 0:
            return "Unknown"
        suffix = " · Charging" if self.battery_charging else ""
        return f"{self.battery_charge}%{suffix}"

    @property
    def type_icon_name(self) -> str:
        return {
            "phone": "phone-symbolic",
            "smartphone": "phone-symbolic",
            "tablet": "computer-symbolic",
            "desktop": "computer-symbolic",
            "laptop": "computer-symbolic",
        }.get(self.type, "phone-symbolic")

    @property
    def battery_icon_name(self) -> str:
        c = self.battery_charge
        if c < 0:
            return "battery-missing-symbolic"

        prefix = "battery"
        if c >= 80:
            level = "full"
        elif c >= 50:
            level = "good"
        elif c >= 20:
            level = "low"
        elif c >= 5:
            level = "caution"
        else:
            level = "empty"

        suffix = "-charging" if self.battery_charging else ""
        return f"{prefix}-{level}{suffix}-symbolic"


# ── SMS models ─────────────────────────────────────────────────────


@dataclass
class SmsMessage:
    """A single SMS/MMS message."""

    uid: int = 0
    body: str = ""
    address: str = ""
    date: int = 0  # ms since epoch
    msg_type: int = 0  # 1=received, 2=sent
    read: int = 1
    thread_id: int = 0
    attachments: list[dict] = field(default_factory=list)

    @property
    def is_sent(self) -> bool:
        return self.msg_type == 2

    @property
    def timestamp(self) -> datetime:
        return datetime.fromtimestamp(self.date / 1000) if self.date else datetime.now()

    @property
    def time_label(self) -> str:
        ts = self.timestamp
        now = datetime.now()
        if ts.date() == now.date():
            return ts.strftime("%I:%M %p").lstrip("0")
        delta = (now.date() - ts.date()).days
        if delta == 1:
            return "Yesterday " + ts.strftime("%I:%M %p").lstrip("0")
        if delta < 7:
            return ts.strftime("%A %I:%M %p").lstrip("0")
        return ts.strftime("%b %d, %Y %I:%M %p").lstrip("0")


@dataclass
class Conversation:
    """A conversation thread (one entry per contact/group)."""

    thread_id: int = 0
    display_name: str = ""
    address: str = ""
    last_message: str = ""
    last_date: int = 0
    is_read: bool = True
    messages: list[SmsMessage] = field(default_factory=list)

    @property
    def preview(self) -> str:
        if self.last_message:
            text = self.last_message
        elif self.messages:
            text = self.messages[-1].body
        else:
            return ""
        return text[:80] + ("…" if len(text) > 80 else "")

    @property
    def time_label(self) -> str:
        ts = self.last_date or (self.messages[-1].date if self.messages else 0)
        if not ts:
            return ""
        dt = datetime.fromtimestamp(ts / 1000)
        now = datetime.now()
        if dt.date() == now.date():
            return dt.strftime("%I:%M %p").lstrip("0")
        delta = (now.date() - dt.date()).days
        if delta == 1:
            return "Yesterday"
        if delta < 7:
            return dt.strftime("%A")
        return dt.strftime("%b %d")

    @property
    def sort_key(self) -> int:
        return self.last_date or (self.messages[-1].date if self.messages else 0)
