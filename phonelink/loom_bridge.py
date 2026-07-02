"""Optional bridge to the Loom device fabric.

phonelink works with or without Loom installed. This module imports ``loom_sdk`` lazily and exposes a
tiny surface the Fabric panel uses; when the SDK isn't installed, :func:`sdk_available` returns False
and the panel shows a friendly setup hint instead of erroring.

The SDK is a thin client over a locally-running ``loomd`` (see the ``loom`` repo, ``sdk/python``).
Blocking calls (they touch a Unix socket + the network) must run off the GTK thread — the panel
offloads them via the shared :class:`phonelink.async_bridge.AsyncBridge` on the KDE Connect client.
"""

from __future__ import annotations

try:  # loom_sdk is an optional dependency
    from loom_sdk import Entry, Loom, LoomError  # noqa: F401

    _IMPORT_ERROR = None
except Exception as exc:  # noqa: BLE001 — any import failure means "not available"
    Loom = None  # type: ignore[assignment]
    Entry = None  # type: ignore[assignment]
    LoomError = Exception  # type: ignore[assignment,misc]
    _IMPORT_ERROR = exc


def sdk_available() -> bool:
    """True if ``loom_sdk`` could be imported."""
    return Loom is not None


def import_hint() -> str:
    """A human hint explaining why the SDK isn't available (empty if it is)."""
    if _IMPORT_ERROR is None:
        return ""
    return str(_IMPORT_ERROR)


def connect():
    """Return a fresh :class:`loom_sdk.Loom` client, or raise if the SDK isn't installed."""
    if Loom is None:
        raise RuntimeError("loom_sdk is not installed")
    return Loom()
