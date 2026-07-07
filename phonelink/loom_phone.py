"""Loom transport for phone notifications — the KDE Connect replacement (loom ``docs/12`` P3).

:class:`LoomPhoneClient` mirrors the *notifications* slice of
:class:`~phonelink.dbus_client.KDEConnectClient`, but over the Loom fabric instead of the KDE Connect
daemon. It resolves the phone's device node-id from the local ``loomd`` realm, opens a streaming
``phone.subscribe`` subscription (``loom/phone/0``), and pushes snapshot / posted / removed into the
callbacks the notifications panel provides. Because loomd dials the phone **by node-id**, this works
off-LAN with no KDE Connect on the network.

Every loomd call blocks (a Unix socket + the network), so the subscription runs on a dedicated daemon
thread with an automatic reconnect loop; callbacks fire on that thread and the panel marshals them onto
the GTK main loop (mirroring how the Fabric panel offloads work — see ``ui/fabric_panel.py``).
"""

from __future__ import annotations

import threading
from typing import Callable, List, Optional

from phonelink import loom_bridge
from phonelink.models import Notification

# Labels that hint a realm device is the phone we want to mirror (best-effort; the common realm is
# just laptop + phone, where "the other device" is unambiguous).
_PHONE_HINTS = ("phone", "android", "pixel", "galaxy", "samsung", "oneplus", "s25", "moto", "nexus")


class LoomPhoneClient:
    """A live notification source backed by a phone's Loom node.

    Parameters
    ----------
    loom_factory:
        Returns a fresh ``loom_sdk.Loom`` client. Defaults to :func:`phonelink.loom_bridge.connect`.
    preferred_device:
        An explicit phone device node-id to subscribe to; otherwise it is auto-resolved from the realm.
    """

    def __init__(self, loom_factory: Optional[Callable] = None, preferred_device: str = ""):
        self._loom_factory = loom_factory or loom_bridge.connect
        self._preferred_device = preferred_device
        self._thread: Optional[threading.Thread] = None
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._stream = None
        # The phone node-id we're currently subscribed to; the P5 dismiss/reply actions target it.
        self._device_id: Optional[str] = None
        # Callbacks (set in start()); default to no-ops so a stray event is harmless.
        self._on_snapshot: Callable[[List[Notification]], None] = lambda _n: None
        self._on_posted: Callable[[Notification], None] = lambda _n: None
        self._on_removed: Callable[[str], None] = lambda _pid: None
        self._on_status: Callable[[str, str], None] = lambda _s, _d: None

    # --- availability / resolution ---------------------------------------------------------------

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

    # --- lifecycle -------------------------------------------------------------------------------

    def start(
        self,
        on_snapshot: Callable[[List[Notification]], None],
        on_posted: Callable[[Notification], None],
        on_removed: Callable[[str], None],
        on_status: Optional[Callable[[str, str], None]] = None,
    ) -> None:
        """Begin the background subscription. Idempotent — restarts cleanly if already running.

        ``on_status(state, detail)`` reports the connection lifecycle, ``state`` ∈
        ``{"connecting","connected","error","stopped"}``.
        """
        self.stop()
        self._on_snapshot = on_snapshot
        self._on_posted = on_posted
        self._on_removed = on_removed
        self._on_status = on_status or (lambda _s, _d: None)
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, name="loom-phone-notify", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        """Stop the subscription and its reconnect loop (safe to call from any thread)."""
        self._stop.set()
        with self._lock:
            stream = self._stream
            self._stream = None
            self._device_id = None
        if stream is not None:
            stream.close()  # unblocks a read parked in the worker thread
        thread = self._thread
        self._thread = None
        if thread is not None and thread is not threading.current_thread():
            thread.join(timeout=5)

    # --- worker ----------------------------------------------------------------------------------

    def _run(self) -> None:
        backoff = 1.0
        max_backoff = 30.0
        while not self._stop.is_set():
            try:
                loom = self._loom_factory()
            except Exception as e:  # noqa: BLE001 — SDK missing → no point retrying
                self._on_status("error", f"Loom SDK unavailable: {e}")
                break

            try:
                device = self._resolve_phone_device(loom)
            except loom_bridge.LoomError as e:
                self._on_status("error", f"loomd not reachable ({e})")
                device = None
            except Exception as e:  # noqa: BLE001
                self._on_status("error", str(e))
                device = None

            if not device:
                self._on_status("connecting", "Waiting for a phone in your Loom realm…")
                if self._stop.wait(backoff):
                    break
                backoff = min(backoff * 2, max_backoff)
                continue

            try:
                self._on_status("connecting", "Connecting to your phone over Loom…")
                stream = loom.subscribe_notifications(device)
            except loom_bridge.LoomError as e:
                self._on_status("error", str(e))
                if self._stop.wait(backoff):
                    break
                backoff = min(backoff * 2, max_backoff)
                continue

            backoff = 1.0
            with self._lock:
                self._stream = stream
                self._device_id = device
            self._on_status("connected", device)
            try:
                for ev in stream:
                    if self._stop.is_set():
                        break
                    self._dispatch(ev)
                    if ev.kind in ("end", "error"):
                        break
            except Exception:  # noqa: BLE001 — a broken stream just triggers a reconnect
                pass
            finally:
                with self._lock:
                    self._stream = None
                stream.close()

            if self._stop.is_set():
                break
            self._on_status("connecting", "Reconnecting to your phone over Loom…")
            if self._stop.wait(backoff):
                break
            backoff = min(backoff * 2, max_backoff)

        self._on_status("stopped", "")

    def _dispatch(self, ev) -> None:
        if ev.kind == "snapshot":
            self._on_snapshot([Notification.from_loom(n) for n in ev.notifications])
        elif ev.kind == "posted" and ev.notification is not None:
            self._on_posted(Notification.from_loom(ev.notification))
        elif ev.kind == "removed":
            self._on_removed(ev.public_id)

    # --- actions (P5: loom/phone-action/0) -------------------------------------------------------

    def _target_device(self, loom) -> str:
        """The phone node-id an action targets: the currently-subscribed device if we have one, else
        freshly resolved from the realm (so an action works even before the stream first connects)."""
        device = self._device_id or self._resolve_phone_device(loom)
        if not device:
            raise loom_bridge.LoomError("no phone in your Loom realm to act on")
        return device

    def dismiss_notification(self, public_id: str) -> None:
        """Ask the phone to dismiss one of its notifications over Loom (P5). **Blocking** (a loomd
        socket call + a phone dial), so call it off the GTK main loop; raises ``LoomError`` if the
        phone is unreachable, refuses us, or can't dismiss it."""
        loom = self._loom_factory()
        loom.dismiss_notification(self._target_device(loom), public_id)

    def reply_to_notification(self, reply_id: str, text: str) -> None:
        """Ask the phone to send an inline reply over Loom (P5). ``reply_id`` is the token on a
        repliable :class:`~phonelink.models.Notification`. Same blocking/error semantics as
        :meth:`dismiss_notification`."""
        loom = self._loom_factory()
        loom.reply_to_notification(self._target_device(loom), reply_id, text)
