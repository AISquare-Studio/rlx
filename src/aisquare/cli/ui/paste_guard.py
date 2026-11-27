"""A bracketed paste reaches the app whole, a raw Escape in it included.

Textual's ``XTermParser.parse`` keeps decoding escape sequences while a bracketed paste is
open, so a raw Escape in the pasted text sends the rest of it to the app as keys, an Enter
among them, ahead of a cut-short paste. In an agent's pane that Enter submits a half-pasted
prompt (textual#6802, https://github.com/Textualize/textual/issues/6802).

:func:`install` wraps that parse, once per process. Textual's own parse reads every character
until it has read a paste's start marker (``ESC[200~``). From there the guard reads the body
itself, with no timeout, up to the first exact end marker (``ESC[201~``), which it looks for
in a fixed trailing window of the last six characters read. It hands the end marker to
Textual's parse, so that parse closes its own paste, and delivers exactly one ``Paste`` with
the body's text, an empty one included, NULs dropped once as Textual drops them. Every
character outside a paste still goes through Textual's parse: typing, alt+key, reports, and a
lone Escape, which becomes the escape key after Textual's own ``ESCAPE_DELAY``.

Its known limits, each Textual's own behaviour or the price of no timer:

- A start marker split by a pause longer than ``ESCAPE_DELAY`` is not guarded: Textual times
  its head out as keys, and the paste after it is Textual's as before.
- A paste whose end marker never arrives holds later input until one does or the input ends;
  input that ends inside a paste gives no paste and no key.
- An exact end marker inside the pasted text ends the paste there.
- Focus and resize reports inside a paste are text.

It leans on private Textual names: ``XTermParser.parse``, ``textual._parser.Read1``,
``ParseTimeout`` and ``ParseEOF``, ``BRACKETED_PASTE_START`` and ``BRACKETED_PASTE_END``, and
the empty ``Paste`` that Textual's parse flushes for an end marker read with no text before
it. ``tests/test_paste_guard.py``'s canary tests pin each one, the parse's source and the
defect itself, and fail naming this module; should a name it reads be missing when rlx
starts, Textual's parse stays as it is.

Drop this guard and the textual==8.2.8 pin once Textual ships the fix for textual#6802: this
module, its install in ``ClipboardApp``, its tests, and the pin in ``pyproject.toml``.
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

GUARD_MARK = "_rlx_paste_guard_wraps"
"""The attribute :func:`install` sets on the guarded parse: the parse it wraps. Installing
reads it on whatever ``XTermParser.parse`` is now, so a parse already guarded is never wrapped
again, even when this module's record of it is gone (a reload)."""


def textual_parse() -> Parse | None:
    """Textual's own ``XTermParser.parse``, which :func:`install` wrapped; None before it has."""
    return None if _installed is None else _installed.textual_parse


def install() -> bool:
    """Wrap ``XTermParser.parse`` (:func:`_guard`), once per process: whether it is wrapped.

    Idempotent: a parse that already carries :data:`GUARD_MARK` is kept as it is, so
    installing twice wraps once. A Textual whose private names have moved, or whose parse is
    no longer a generator, keeps its own parse: a Textual release must never stop rlx from
    starting (the canary tests are what make such a release loud)."""
    global _installed
    try:
        import textual._parser as textual_parser
        import textual._xterm_parser as xterm_parser

        current = xterm_parser.XTermParser.parse
        wrapped = getattr(current, GUARD_MARK, None)
        if wrapped is not None:
            _installed = _Installed(wrapped, current)
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
    setattr(guarded, GUARD_MARK, current)
    xterm_parser.XTermParser.parse = guarded  # type: ignore[method-assign, assignment]
    _installed = _Installed(current, guarded)
    return True


# Drop this guard and the textual==8.2.8 pin once Textual ships the fix for textual#6802
# (https://github.com/Textualize/textual/issues/6802). Known limit, by choice: a start marker
# that a pause longer than ESCAPE_DELAY splits is not guarded, as stock Textual times its head
# out as keys (tests/test_paste_guard.py pins that behaviour).
def _guard(
    textual: Parse,
    *,
    read1: Callable[[], Read1],
    timeout: type[Exception],
    eof: type[Exception],
    start: str,
    end: str,
) -> Parse:
    """``textual``'s parse with each paste's body read here: everything after ``start``,
    which ``textual`` reads and so opens its paste, up to the first exact ``end``, read with
    no timeout and delivered as exactly one ``Paste``."""

    def parse(
        self: XTermParser, token_callback: Callable[[Message], None]
    ) -> Generator[Read1 | Peek1, str, None]:
        replaying_end = False

        def on_token(token: Message) -> None:
            if replaying_end and isinstance(token, events.Paste) and not token.text:
                return  # the paste ``textual`` flushes for the end marker alone: ours follows
            token_callback(token)

        inner = textual(self, on_token)
        awaiting = next(inner)
        last_read = ""
        while True:
            try:
                character = yield awaiting
            except (timeout, eof) as interruption:
                last_read = ""  # it ends the sequence ``textual`` was reading: no marker spans it
                try:
                    awaiting = inner.throw(interruption)
                except StopIteration:
                    return
                continue
            try:
                awaiting = inner.send(character)
            except StopIteration:
                return
            last_read = (last_read + character)[-len(start) :]
            if last_read != start:
                continue
            last_read = ""
            try:
                text = yield from _paste_body(read1, end)
            except eof:
                return  # as ``textual`` does: input that ends inside a paste is no paste
            replaying_end = True
            for marker_character in end:
                awaiting = inner.send(marker_character)
            replaying_end = False
            # Textual drops NULs from a paste (its issue 1661), and so does this, once.
            token_callback(events.Paste(text.replace("\x00", "")))

    return parse


def _paste_body(read1: Callable[[], Read1], end: str) -> Generator[Read1, str, str]:
    """The text read up to the first exact ``end``, which is read and left out. Read with no
    timeout, however long a read pauses; ``end`` is looked for in a fixed trailing window,
    the last ``len(end)`` characters, so each character costs the same in a long paste."""
    body: list[str] = []
    window = ""
    while window != end:
        character = yield read1()
        body.append(character)
        window = (window + character)[-len(end) :]
    return "".join(body[: -len(end)])
