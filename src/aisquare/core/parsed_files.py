"""A small answer parsed out of a file, parsed again only when the file's bytes differ.

``asq`` reads every account slot's files after each fleet frame
(``services.claude_accounts.overview``), and asks each slot's ``.claude.json`` for its identity
several times a read; Claude Code keeps tens of KB to tens of MB in that file.
``claude_accounts.read_identity`` reads it here. :func:`parsed` reads the file on every call and
keeps the parser's answer under the SHA-256 digest of the bytes it parsed: a call whose bytes have
that digest is handed the kept answer without a parse, any other is parsed. An answer handed out is
so the parse of the bytes just read, assuming no SHA-256 collision; no stat or clock decides it.

What is kept: per path and parser, one digest and one answer, which may be ``None`` (a JSON object
with nothing to project). Nothing of a file's bytes, and never a login.

Limits:

- A read that overlaps an in-place write can return valid partial or mixed JSON. That read's answer
  is the caller's, and it is kept under that read's own digest; the file's final bytes miss it.
- A read that raises (no file there, any other ``OSError``) and a parse that raises (bytes that are
  no UTF-8, no JSON object, an empty file) drop the path's kept answer, and the exception goes on to
  the caller (:func:`parsed_or` turns ``OSError`` and ``ValueError`` into a fallback).
- At most :data:`MAX_KEPT` answers are kept, the least recently used dropped first, and an accounts
  overview drops those of slots it no longer lists (:func:`forget_under`). Both are eventual: a read
  that began before a slot was removed can keep its answer until the next sweep or eviction.
  Neither decides an answer; the bytes read do.

``asq`` reads accounts from more than one thread, so the map is read, written, reordered and swept
under one lock. A read, its hash and a parse run outside it; two callers that miss at once each
parse.
"""

from __future__ import annotations

import hashlib
import json
import threading
from collections import OrderedDict
from collections.abc import Callable, Collection
from pathlib import Path
from typing import Any, TypeVar, cast

T = TypeVar("T")
U = TypeVar("U")

MAX_KEPT = 32
"""The most answers kept at once: one a Claude slot's ``.claude.json``, with room for many slots."""

_Key = tuple[Path, Callable[..., object]]
_KEPT: OrderedDict[_Key, tuple[bytes, object]] = OrderedDict()
_LOCK = threading.Lock()


def _forget(key: _Key) -> None:
    with _LOCK:
        _KEPT.pop(key, None)


def parsed(path: Path, parse: Callable[[Path, str], T]) -> T:
    """``parse(path, text)`` of the bytes the file at ``path`` holds now, or the answer kept for
    bytes with the same SHA-256 digest.

    Kept by ``path`` and ``parse``, so ``parse`` is a function defined once, never one made per
    call; every caller is handed the same answer, so it is immutable or each caller copies it.
    Raises what reading the file raises (``FileNotFoundError`` where none is there, any other
    ``OSError``), ``UnicodeDecodeError`` and what ``parse`` raises; each drops the kept answer."""
    key = (path, parse)
    try:
        body = path.read_bytes()
        digest = hashlib.sha256(body).digest()
        with _LOCK:
            kept = _KEPT.get(key)
            if kept is not None and kept[0] == digest:
                _KEPT.move_to_end(key)
                return cast(T, kept[1])
        value = parse(path, body.decode("utf-8"))
    except Exception:
        _forget(key)
        raise
    with _LOCK:
        _KEPT[key] = (digest, value)
        _KEPT.move_to_end(key)
        while len(_KEPT) > MAX_KEPT:
            _KEPT.popitem(last=False)
    return value


def parsed_or(path: Path, parse: Callable[[Path, str], T], unreadable: U) -> T | U:
    """:func:`parsed`, or ``unreadable`` where the file is missing or cannot be read or ``parse``
    raises ``ValueError``: the answer of a reader that never raises."""
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
