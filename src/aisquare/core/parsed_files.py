"""Account files parsed again only once they changed.

``asq`` reads every account slot's files after each fleet frame
(``services.claude_accounts.overview``): each slot's ``.claude.json``, in which Claude Code keeps
tens of KB to tens of MB, its login, its settings and its plugins, several times a slot a read.
:func:`parsed` keeps what a parser made of a file while the file's signature (device, inode,
size, modification and change times) is the one it was read under.

What is kept and what is not:

- The file is stat'ed, then read. A write that lands between the two is read, and kept under the
  stat before it, which the next call finds changed and reads again.
- A file written less than :data:`SETTLED` seconds ago is read and not kept: where a file
  system's clock ticks coarsely, two writes of one size inside one tick have one signature, and
  the first one's answer would be kept past the second.
- A read that raises (no file there, any other ``OSError``, bytes that are no UTF-8), a parse
  that raises, and an empty file keep nothing: the caller answers as it does for a file it
  cannot read, and the next call reads the file again.

``asq`` reads accounts from more than one thread, so the map of kept answers is read and written
under a lock; a read and a parse run outside it.
"""

from __future__ import annotations

import json
import os
import threading
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any, TypeVar, cast

T = TypeVar("T")
U = TypeVar("U")

SETTLED = 2.0
"""Seconds since a file's last write before its parse is kept: one tick of the coarsest clock
a file system here keeps times by (FAT's), inside which two writes show one signature."""

_Signature = tuple[int, int, int, int, int]
_KEPT: dict[tuple[Path, Callable[..., object]], tuple[_Signature, object]] = {}
_LOCK = threading.Lock()


def parsed(path: Path, parse: Callable[[Path, str], T]) -> T:
    """``parse(path, text)`` of the file at ``path``, or its kept answer while the file's
    signature is the one it was read under.

    Kept by ``path`` and ``parse``, so ``parse`` is a function defined once, never one made per
    call; every caller is handed the same answer, so it is immutable or each caller copies it.
    Raises what reading the file raises (``FileNotFoundError`` where none is there, any other
    ``OSError``, ``UnicodeDecodeError``) and what ``parse`` raises."""
    stat = os.stat(path)
    signature = (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns)
    key = (path, parse)
    with _LOCK:
        kept = _KEPT.get(key)
    if kept is not None and kept[0] == signature:
        return cast(T, kept[1])
    text = path.read_text(encoding="utf-8")
    value = parse(path, text)
    if text and time.time() - stat.st_mtime >= SETTLED:
        with _LOCK:
            _KEPT[key] = (signature, value)
    return value


def parsed_or(path: Path, parse: Callable[[Path, str], T], unreadable: U) -> T | U:
    """:func:`parsed`, or ``unreadable`` where the file is missing or cannot be read or ``parse``
    raises ``ValueError``: the answer of a reader that never raises."""
    try:
        return parsed(path, parse)
    except (OSError, ValueError):
        return unreadable


def json_object(path: Path, text: str) -> dict[str, Any]:
    """The JSON object ``text``, the file at ``path``'s, holds: a parser's first step.
    ``ValueError`` when it holds none."""
    loaded: Any = json.loads(text)
    if not isinstance(loaded, dict):
        raise ValueError(f"{path} holds no JSON object")
    return loaded
