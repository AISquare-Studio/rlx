"""Small answers parsed out of a slot's files, parsed again only once the file changed.

``asq`` reads every account slot's files after each fleet frame
(``services.claude_accounts.overview``), several times a slot a read. Three readers keep what
they parse here: the account's identity from its ``.claude.json``, in which Claude Code keeps
tens of KB to tens of MB, and from its ``settings.json`` the plugins it enables and whether it
switches every hook off. Each keeps a small answer (an identity, a tuple, a bool), never a
login, never a whole decoded file. :func:`parsed` keeps an answer by the file's path and its
parser while the file's signature (device, inode, size, modification and change times) is the
one it was read under; a later call with that signature is handed the kept answer.

An answer is kept only when the read can be told apart from any later write:

- The file was last written at least :data:`SETTLED` seconds before it was stat'ed. A write
  after the stat then lands in a later tick of even a coarse clock, and changes the signature.
- Its stat after the read is the one before it, so no write landed while it was read.

A read that raises (no file there, any other ``OSError``, bytes that are no UTF-8), a parse that
raises (an empty file holds no JSON), and a read that is not kept each drop what the file had
kept: the caller answers as it does for a file it cannot read, and the next call reads again.
Two callers that miss at once each parse the file.

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
"""Seconds between a file's last write and its stat before its parse is kept: one tick of the
coarsest clock a file system here keeps times by (FAT's), inside which two writes show one
signature."""

_Signature = tuple[int, int, int, int, int]
_KEPT: dict[tuple[Path, Callable[..., object]], tuple[_Signature, object]] = {}
_LOCK = threading.Lock()


def _now() -> float:
    """The wall clock a file's modification time is judged by."""
    return time.time()


def _stat_signature(stat: os.stat_result) -> _Signature:
    return stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns


def _forget(key: tuple[Path, Callable[..., object]]) -> None:
    with _LOCK:
        _KEPT.pop(key, None)


def parsed(path: Path, parse: Callable[[Path, str], T]) -> T:
    """``parse(path, text)`` of the file at ``path``, or its kept answer while the file's
    signature is the one it was read under.

    Kept by ``path`` and ``parse``, so ``parse`` is a function defined once, never one made per
    call; every caller is handed the same answer, so it is immutable or each caller copies it.
    Raises what reading the file raises (``FileNotFoundError`` where none is there, any other
    ``OSError``, ``UnicodeDecodeError``) and what ``parse`` raises."""
    key = (path, parse)
    try:
        before = os.stat(path)
        settled = _now() - before.st_mtime >= SETTLED
        signature = _stat_signature(before)
        with _LOCK:
            kept = _KEPT.get(key)
        if kept is not None and kept[0] == signature:
            return cast(T, kept[1])
        value = parse(path, path.read_text(encoding="utf-8"))
        unchanged = _stat_signature(os.stat(path)) == signature
    except Exception:
        _forget(key)
        raise
    if settled and unchanged:
        with _LOCK:
            _KEPT[key] = (signature, value)
    else:
        _forget(key)
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
