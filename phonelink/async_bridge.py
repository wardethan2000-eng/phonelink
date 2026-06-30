"""Async bridge — runs blocking work off the GTK main thread.

The KDE Connect D-Bus calls in :mod:`phonelink.dbus_client` are synchronous and
can block for up to several seconds (30 s for an SFTP mount).  Calling them
directly from a GTK signal handler freezes the whole UI, because GTK runs its
event loop on the same thread.

``AsyncBridge`` solves this by running such work on a small thread pool and
delivering the result back on the main thread via ``GLib.idle_add``.  GDBus
connections are thread-safe, so the existing :class:`KDEConnectClient` methods
can be called unchanged from a worker thread.

Usage::

    client.submit(
        client.get_active_conversations, device_id,
        on_result=lambda convs: self._apply(convs),
    )

Rules of thumb:

* The *work* function runs on a worker thread.  It must only touch D-Bus,
  the network, the filesystem, or pure-Python data — never GTK widgets.
* ``on_result`` / ``on_error`` run on the main thread, so they may touch GTK.
* Callbacks should guard against stale state themselves (e.g. check that the
  active device is still the one the request was made for), because a result
  may arrive after the user has navigated away.
"""

from __future__ import annotations

import queue
import threading
from typing import Any, Callable

import gi

gi.require_version("GLib", "2.0")
from gi.repository import GLib

# Sentinel pushed to wake a worker so it can exit on shutdown.
_STOP = object()


class AsyncBridge:
    """Run blocking callables on a pool of daemon threads, results on the UI thread.

    Daemon threads are used deliberately: if the process exits while a worker is
    stuck in a slow D-Bus call (mounts can take up to 30 s), daemon threads are
    torn down with the interpreter instead of blocking exit.
    """

    def __init__(self, max_workers: int = 4):
        self._queue: queue.Queue = queue.Queue()
        self._shutdown = False
        self._workers: list[threading.Thread] = []
        for i in range(max(1, max_workers)):
            worker = threading.Thread(
                target=self._worker_loop,
                name=f"phonelink-dbus-{i}",
                daemon=True,
            )
            worker.start()
            self._workers.append(worker)

    def submit(
        self,
        func: Callable[..., Any],
        *args: Any,
        on_result: Callable[[Any], None] | None = None,
        on_error: Callable[[Exception], None] | None = None,
        **kwargs: Any,
    ) -> None:
        """Run ``func(*args, **kwargs)`` on a worker thread.

        ``on_result(value)`` is scheduled on the main thread with the return
        value; if the call raises, ``on_error(exc)`` is scheduled instead (or
        the error is logged when no handler is given).  Fire-and-forget calls
        simply omit both callbacks.
        """
        if self._shutdown:
            return
        self._queue.put((func, args, kwargs, on_result, on_error))

    def _worker_loop(self) -> None:
        while True:
            item = self._queue.get()
            if item is _STOP:
                return
            func, args, kwargs, on_result, on_error = item
            try:
                result = func(*args, **kwargs)
            except Exception as exc:  # noqa: BLE001 — worker must never crash
                if on_error is not None:
                    GLib.idle_add(self._dispatch, on_error, exc)
                else:
                    print(
                        f"[phonelink] async task "
                        f"{getattr(func, '__name__', func)!r} failed: {exc}"
                    )
            else:
                if on_result is not None:
                    GLib.idle_add(self._dispatch, on_result, result)

    @staticmethod
    def _dispatch(callback: Callable[[Any], None], value: Any) -> int:
        try:
            callback(value)
        except Exception as exc:  # noqa: BLE001 — keep the main loop alive
            print(f"[phonelink] async callback failed: {exc}")
        return GLib.SOURCE_REMOVE

    def shutdown(self) -> None:
        """Stop accepting work and signal workers to exit (non-blocking)."""
        if self._shutdown:
            return
        self._shutdown = True
        for _ in self._workers:
            self._queue.put(_STOP)
