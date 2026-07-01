"""Process-identity helpers to guard against PID reuse (gi-free).

The tray subprocess talks to the main app purely by PID: it polls
``os.kill(parent_pid, 0)`` for liveness and sends ``SIGUSR1``/``SIGUSR2`` to it.
If the parent dies and the kernel recycles its PID for an unrelated process, the
tray both (a) thinks the parent is still alive and (b) can deliver a *quit*
signal to a stranger.  A ``(pid, start_time)`` pair uniquely identifies a running
process — the recycled PID will have a different start time — so checking the
start time before trusting/​signalling the PID closes the hole.
"""

from __future__ import annotations


def proc_start_time(pid: int) -> int | None:
    """Return the process start time (clock ticks since boot) from ``/proc``.

    Returns ``None`` if the process does not exist or ``/proc`` is unavailable
    (e.g. non-Linux) — callers treat that as "cannot confirm identity".
    """
    try:
        with open(f"/proc/{int(pid)}/stat", "rb") as f:
            data = f.read()
    except (OSError, ValueError):
        return None
    # Field 2 (comm) is wrapped in parentheses and may itself contain spaces or
    # ')', so parse everything after the final ')'.  Field 22 (starttime) is then
    # index 19 in the remaining whitespace-split fields (which start at field 3).
    try:
        tail = data[data.rindex(b")") + 2:].split()
        return int(tail[19])
    except (ValueError, IndexError):
        return None


def process_matches(pid: int, start_token: int | None) -> bool:
    """True iff ``pid`` is alive and its start time equals ``start_token``."""
    if not start_token:
        return False
    return proc_start_time(pid) == int(start_token)
