"""A bracketed paste reaches the app whole, a raw Escape in it included.

Textual's ``XTermParser.parse`` keeps decoding escape sequences while a bracketed paste is
open, so a raw Escape in the pasted text sends the rest of it to the app as keys, an Enter
among them, ahead of a cut-short paste. In an agent's pane that Enter submits a half-pasted
prompt.

:func:`install` wraps that parse, once per process. Once Textual's own parse has read a
paste's start marker, the text up to the end marker is read here and delivered as one
``Paste`` holding it, NULs dropped as Textual drops them; every character outside a paste
still goes through Textual's parse. A pause inside the start marker (a split read) lets
Textual time its first characters out as keys, as it must for a lone Escape; the guard keeps
those characters as a cut marker, and if the rest of the marker follows with no second pause
longer than Textual's escape timeout, the paste after it is still one ``Paste``. It leans on
private Textual names: ``XTermParser.parse``,
``textual._parser.Read1``, ``ParseTimeout`` and ``ParseEOF``, ``BRACKETED_PASTE_START`` and
``BRACKETED_PASTE_END``, and the empty ``Paste`` that Textual's parse flushes for an end marker
read with no text before it, which is how the wrapper closes Textual's own paste.
``tests/test_paste_guard.py`` pins them; should a name it reads be missing when asq starts,
Textual's parse stays as it is.

Delete this module, its install in ``ClipboardApp`` and the seam test once a Textual release
keeps an Escape inside a paste as text.
"""

from __future__ import annotations

import inspect
from collections.abc import Callable, Generator
from dataclasses import dataclass
from typing import TYPE_CHECKING, TypeAlias

from textual import events

if TYPE_CHECKING:  # at run time only install() reaches them, inside its try
    from textual._parser import Peek1, Read1
    from textual._xterm_parser import XTermParser
    from textual.message import Message

    Parse: TypeAlias = Callable[
        [XTermParser, Callable[[Message], None]], Generator[Read1 | Peek1, str, None]
    ]
    """The shape of ``XTermParser.parse``: given the parser and where its tokens go, a
    generator that yields what it waits for and is sent each character read."""


@dataclass(frozen=True)
class _Installed:
    textual_parse: Parse
    guarded_parse: Parse


_installed: _Installed | None = None


def textual_parse() -> Parse | None:
    """Textual's own ``XTermParser.parse``, which :func:`install` wrapped; None before it has."""
    return None if _installed is None else _installed.textual_parse


def install() -> bool:
    """Wrap ``XTermParser.parse`` (:func:`_guard`), once per process: whether it is wrapped.

    A Textual whose private names have moved, or whose parse is no longer a generator,
    keeps its own parse."""
    global _installed
    try:
        import textual._parser as textual_parser
        import textual._xterm_parser as xterm_parser

        current = xterm_parser.XTermParser.parse
        if _installed is not None and current is _installed.guarded_parse:
            return True
        guarded = _guard(
            current,
            read1=textual_parser.Read1,
            timeout=textual_parser.ParseTimeout,
            eof=textual_parser.ParseEOF,
            start=xterm_parser.BRACKETED_PASTE_START,
            end=xterm_parser.BRACKETED_PASTE_END,
        )
    except (ImportError, AttributeError):
        return False
    if not inspect.isgeneratorfunction(current):
        return False
    xterm_parser.XTermParser.parse = guarded  # type: ignore[method-assign, assignment]
    _installed = _Installed(current, guarded)
    return True


def _guard(
    textual: Parse,
    *,
    read1: type[Read1],
    timeout: type[Exception],
    eof: type[Exception],
    start: str,
    end: str,
) -> Parse:
    """``textual``'s parse with each paste's body read here: everything after ``start`` up
    to the first ``end``. ``start`` goes through ``textual``, which opens its paste; one that
    ``textual``'s escape timeout cut is kept, and its tail held back from the app (``withheld``)
    until it is whole or turns out to be no marker."""

    def parse(
        self: XTermParser, token_callback: Callable[[Message], None]
    ) -> Generator[Read1 | Peek1, str, None]:
        replaying_end = False
        # What ``textual`` makes of a cut marker's tail, while the marker may still be whole.
        withheld: list[Message] | None = None

        def on_token(token: Message) -> None:
            if replaying_end and isinstance(token, events.Paste) and not token.text:
                return  # the paste ``textual`` flushes for the end marker alone: ours follows
            if withheld is not None:
                withheld.append(token)
            else:
                token_callback(token)

        def pass_on_withheld() -> None:
            nonlocal withheld
            held, withheld = withheld or [], None
            for token in held:
                token_callback(token)

        inner = textual(self, on_token)
        awaiting = next(inner)
        opener = ""  # what has been read of ``start``
        cut_after: float | None = None  # the escape timeout that cut ``opener``
        tail_begun = False  # a cut marker's tail has begun: it may pause no longer than that
        while True:
            try:
                character = yield (read1(cut_after) if tail_begun else awaiting)
            except timeout as interruption:
                if withheld is not None:  # the tail paused again: no marker after all
                    opener, tail_begun = "", False
                    pass_on_withheld()
                    continue
                cut_after = awaiting.timeout
                try:
                    awaiting = inner.throw(interruption)  # the keys it makes of the cut head
                except StopIteration:
                    return
                if opener:
                    withheld = []
                continue
            except eof as interruption:
                pass_on_withheld()
                try:
                    awaiting = inner.throw(interruption)
                except StopIteration:
                    return
                continue
            try:
                awaiting = inner.send(character)
            except StopIteration:
                return
            if withheld is not None and not start.startswith(opener + character):
                opener = ""  # no marker's tail after all
                pass_on_withheld()
            opener = _opened(opener, character, start)
            tail_begun = withheld is not None
            if opener != start:
                continue
            opener, withheld, tail_begun = "", None, False  # the tail's keys are the marker's
            try:
                body = yield from _paste_body(read1, end)
            except eof:
                return  # as ``textual`` does: input that ends inside a paste is no paste
            replaying_end = True
            for marker_character in end:
                awaiting = inner.send(marker_character)
            replaying_end = False
            # Textual drops NULs from a paste (its issue 1661), and so does this.
            token_callback(events.Paste("".join(body[: -len(end)]).replace("\x00", "")))

    return parse


def _opened(opener: str, character: str, start: str) -> str:
    """What has been read of ``start`` once ``character`` follows ``opener``."""
    if start.startswith(opener + character):
        return opener + character
    return character if start.startswith(character) else ""


def _paste_body(read1: type[Read1], end: str) -> Generator[Read1, str, list[str]]:
    """The characters read up to and including the first ``end``, joined by the caller once."""
    body: list[str] = []
    while "".join(body[-len(end) :]) != end:
        body.append((yield read1()))
    return body
