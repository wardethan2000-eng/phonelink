"""Atomic file writes (gi-free).

Plain ``write_text``/``json.dump`` truncate the target *in place*: a crash or
power loss mid-write leaves a half-written file, which the loaders then treat as
corrupt and silently reset to empty — losing the user's contacts, settings, or
Google token.  Writing to a temporary file in the same directory, flushing it to
disk, and then ``os.replace``-ing it onto the target makes the swap atomic: a
reader ever only sees the complete old file or the complete new one.
"""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path


def atomic_write_text(path: str | os.PathLike, text: str, *, encoding: str = "utf-8") -> None:
    """Write ``text`` to ``path`` atomically (temp file + fsync + os.replace)."""
    target = Path(path)
    target.parent.mkdir(parents=True, exist_ok=True)

    # NamedTemporaryFile in the *same directory* guarantees os.replace stays on
    # one filesystem (rename across filesystems is not atomic).
    fd, tmp_name = tempfile.mkstemp(
        dir=str(target.parent), prefix=f".{target.name}.", suffix=".tmp"
    )
    try:
        with os.fdopen(fd, "w", encoding=encoding) as f:
            f.write(text)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp_name, target)
    except BaseException:
        # Never leave the stray temp file behind on failure.
        try:
            os.unlink(tmp_name)
        except OSError:
            pass
        raise


def atomic_write_json(path: str | os.PathLike, data, *, indent: int | None = 2,
                      ensure_ascii: bool = False) -> None:
    """Serialise ``data`` to JSON and write it atomically."""
    atomic_write_text(path, json.dumps(data, indent=indent, ensure_ascii=ensure_ascii))
