"""A small answer parsed out of a file, reused while the file is unchanged.

``asq`` reads every account slot's files after each fleet frame
(``services.claude_accounts.overview``), and asks each slot's ``.claude.json`` for its identity
several times a read; Claude Code keeps tens of KB to tens of MB in that file.
``claude_accounts.read_identity`` reads it here. :func:`parsed` keeps the parser's answer under the
file's stat signature (its ``st_mtime_ns``, ``st_size`` and ``st_ino``): a call whose ``os.stat``
gives the signature of the answer kept for its path is handed that answer without opening the file;
any other call reads and parses it (the owner's decision on rlx#557: the file was read and hashed at
every tick, and is now only stat'ed).

What is kept: per path and parser, one signature and one answer, which may be ``None`` (a JSON
object with nothing to project). Nothing of a file's bytes, and never a login.

Kept only when the signature can be trusted:

- **Of the bytes read.** The signature is the ``fstat`` of the file the bytes came from, kept only
  when ``os.stat`` of the path still gives it after the parse: a link moved, or a file replaced or
  written while it was read, keeps nothing, and the next call reads again.
- **Settled.** A file written less than :data:`RACY_NS` before it was read could be written again
  inside its timestamp's granularity at the same size, which no stat tells apart, so its answer is
  not kept until it has aged (git's "racily clean" rule): a file still being written is read at
  every call.

Limits:

- A rewrite that keeps the size and inode and puts the old modification time back, done after the
  file settled, is not seen until something else of its stat changes. Claude Code never writes so;
  an editor's in-place save does not restore the time.
- A read that raises (no file there, any other ``OSError``) and a parse that raises (bytes that are
  no UTF-8, no JSON object, an empty file) drop the path's kept answer, and the exception goes on to
  the caller (:func:`parsed_or` turns ``OSError`` and ``ValueError`` into a fallback).
- At most :data:`MAX_KEPT` answers are kept, the least recently used dropped first, and an accounts
  overview drops those of slots it no longer lists (:func:`forget_under`). Both are eventual: a read
  that began before a slot was removed can keep its answer until the next sweep or eviction.

``asq`` reads accounts from more than one thread, so the map is read, written, reordered and swept
under one lock. A stat, a read and a parse run outside it; two callers that miss at once each
parse.
"""

from __future__ import annotations

import json
import os
import threading
import time
from collections import OrderedDict
from collections.abc import Callable, Collection
from pathlib import Path
from typing import Any, TypeVar, cast

T = TypeVar("T")
U = TypeVar("U")

MAX_KEPT = 32
"""The most answers kept at once: one a Claude slot's ``.claude.json``, with room for many slots."""
RACY_NS = 2_000_000_000
"""How long after its last write a file's answer is not kept: two seconds, past the coarsest
modification-time granularity a home is on (FAT's), nanoseconds as ``st_mtime_ns`` counts them."""

Signature = tuple[int, int, int]
"""A file's ``(st_mtime_ns, st_size, st_ino)``."""
_Key = tuple[Path, Callable[..., object]]
_KEPT: OrderedDict[_Key, tuple[Signature, object]] = OrderedDict()
_LOCK = threading.Lock()
_clock: Callable[[], int] = time.time_ns
"""Now, in nanoseconds since the epoch, as ``st_mtime_ns`` counts them (a test's seam)."""


def _signature(stat: os.stat_result) -> Signature:
    return (stat.st_mtime_ns, stat.st_size, stat.st_ino)


def _read(path: Path) -> tuple[Signature, bytes]:
    """The bytes of the file at ``path``, and the signature of the file they were read from."""
    with path.open("rb") as handle:
        return _signature(os.fstat(handle.fileno())), handle.read()


def _forget(key: _Key) -> None:
    with _LOCK:
        _KEPT.pop(key, None)


def parsed(path: Path, parse: Callable[[Path, str], T]) -> T:
    """``parse(path, text)`` of the file at ``path`` now, or the answer kept while its stat
    signature is unchanged (the module's docstring says when one is kept).

    Kept by ``path`` and ``parse``, so ``parse`` is a function defined once, never one made per
    call; every caller is handed the same answer, so it is immutable or each caller copies it.
    Raises what statting or reading the file raises (``FileNotFoundError`` where none is there,
    any other ``OSError``), ``UnicodeDecodeError`` and what ``parse`` raises; each drops the kept
    answer."""
    key = (path, parse)
    try:
        now = _signature(os.stat(path))
        with _LOCK:
            kept = _KEPT.get(key)
            if kept is not None and kept[0] == now:
                _KEPT.move_to_end(key)
                return cast(T, kept[1])
        read, body = _read(path)
        value = parse(path, body.decode("utf-8"))
        after = _signature(os.stat(path))
    except Exception:
        _forget(key)
        raise
    settled = _clock() - read[0] >= RACY_NS
    with _LOCK:
        if read == after and settled:
            _KEPT[key] = (read, value)
            _KEPT.move_to_end(key)
            while len(_KEPT) > MAX_KEPT:
                _KEPT.popitem(last=False)
        else:
            _KEPT.pop(key, None)  # moved, written or too fresh while read: kept by nobody
    return value


def parsed_or(path: Path, parse: Callable[[Path, str], T], unreadable: U) -> T | U:
    """:func:`parsed`, or ``unreadable`` where it raises ``OSError`` or ``ValueError``, whether
    from the read, the decoding or ``parse``; any other exception goes on to the caller."""
    try:
        return parsed(path, parse)
    except (OSError, ValueError):
        return unreadable


def forget_under(root: Path, keep: Collection[Path]) -> None:
    """Drop the answers kept for files under ``root`` that are under none of ``keep``: the slots an
    accounts overview no longer lists."""
    with _LOCK:
        gone = [
            key
            for key in _KEPT
            if key[0].is_relative_to(root) and not any(key[0].is_relative_to(k) for k in keep)
        ]
        for key in gone:
            _KEPT.pop(key)


def json_object(path: Path, text: str) -> dict[str, Any]:
    """The JSON object ``text``, the file at ``path``'s, holds: a parser's first step.
    ``ValueError`` when it holds none."""
    loaded: Any = json.loads(text)
    if not isinstance(loaded, dict):
        raise ValueError(f"{path} holds no JSON object")
    return loaded
