"""``cli.ui.paste_guard``: a bracketed paste holding a raw Escape reaches the app as one paste.

The parser tests feed a terminal's bytes to Textual's own ``XTermParser`` with the guard in
place and compare the whole list of messages it emits, never a count of keys: a ``Pilot``
press and a posted ``Paste`` never run the parser, so they cannot see what it makes of a raw
Escape. The parser times an escape sequence by a clock, which is faked: a pause between two
reads moves it past ``ESCAPE_DELAY`` and ticks the parser, as the driver ticks it. Where the
guard must change nothing, its output is compared with Textual's own parse
(:func:`paste_guard.textual_parse`) fed the same reads.

The canary tests (``test_canary_*``) fail naming ``cli/ui/paste_guard.py`` and textual#6802
when a Textual internal the guard wraps changes: the installed version, the parse's signature,
the names it reads, the parse's source, or its behaviour. The last one passing again means
Textual fixed the bug: drop the guard and the textual==8.2.8 pin.

Each consumer's own regression sits beside its paste or key tests: the pane
(``tests/test_terminal_pane.py``), the router, a dialog's field and a text area
(``tests/test_ui_shell.py``), the rename field (``tests/test_ui_project_cards.py``) and the
board (``tests/test_watch.py``).
"""

from __future__ import annotations

import ast
import asyncio
import hashlib
import importlib.metadata
import inspect
import sys
from collections.abc import Callable, Generator, Iterable
from typing import Any, ClassVar

import pytest
from textual import _parser, _xterm_parser, constants, events
from textual._xterm_parser import XTermParser
from textual.binding import Binding

from aisquare.cli.ui import paste_guard
from aisquare.cli.ui.clipboard import ClipboardApp
from tests.pane_harness import PASTE_END, PASTE_START, bracketed_paste

RAW_ESCAPE_BODIES = [
    pytest.param("one\x1b", id="escape last"),
    pytest.param("one\x1b\x1btwo", id="escape escape"),
    pytest.param("one\x1btwo", id="escape letter"),
    pytest.param("x\x1bq", id="escape q"),
    pytest.param("one\x1b\rtwo", id="escape enter"),
    pytest.param("one\x1b[Atwo", id="escape [A"),
    pytest.param("one\x1b[Itwo", id="focus report"),
    pytest.param("one\x1b[Otwo", id="blur report"),
    pytest.param("one\x1b[48;24;80;480;800ttwo", id="in-band resize report"),
    pytest.param("one\x1b[2two", id="escape [2"),
    pytest.param("one\x1b[20two", id="escape [20"),
    pytest.param("one\x1b[201two", id="escape [201"),
    pytest.param("one\x1b[201", id="escape [201 before the end marker"),
    pytest.param("\x1b[201x[201~y", id="escape [201x, then the end marker's tail alone"),
    pytest.param(
        "one\x1b" + "abcdefghij" * 4 + "\rz", id="escape and a tail longer than Textual's cut"
    ),
    pytest.param("\x1ba^x", id="escape a caret x"),
    pytest.param("\x1ba\x1bx", id="escape a escape x"),
]

KEYS_REPORTS_AND_PASTES = [
    pytest.param(bracketed_paste("one\rtwo"), id="paste: enter"),
    pytest.param(bracketed_paste("hello"), id="paste: plain"),
    pytest.param(bracketed_paste(""), id="paste: empty"),
    pytest.param(bracketed_paste("a\nb"), id="paste: line feed"),
    pytest.param(bracketed_paste("a\x00b"), id="paste: NUL"),
    pytest.param(bracketed_paste("one[201~two"), id="paste: the end marker's tail alone"),
    pytest.param("a" + bracketed_paste("mid") + "b", id="key a, paste, key b"),
    pytest.param(bracketed_paste("one") + bracketed_paste("two"), id="two pastes"),
    pytest.param("\r", id="key enter"),
    pytest.param("\n", id="key line feed"),
    pytest.param("\x1b", id="key escape"),
    pytest.param("\x1b\r", id="key escape enter"),
    pytest.param("\x1b\x1b\r", id="key escape escape enter"),
    pytest.param("\x1b\x7f", id="key escape delete"),
    pytest.param("\x1bp", id="key escape p"),
    pytest.param("\x1b[A", id="key up"),
    pytest.param("\x1bOP", id="key f1"),
    pytest.param("\x1b[13;2u", id="key shift+enter"),
    pytest.param("\x1b[13;5u", id="key ctrl+enter"),
    pytest.param("^", id="key caret"),
    pytest.param("q", id="key q"),
    pytest.param("\t", id="key tab"),
    pytest.param("\x03", id="key ctrl+c"),
    pytest.param("\x1b[I", id="focus report"),
    pytest.param("\x1b[O", id="blur report"),
    pytest.param("\x1b[48;24;80;480;800t", id="in-band resize report"),
    pytest.param("\x1b[<0;5;5M", id="mouse report"),
    pytest.param("\x1b[3;4R", id="cursor report"),
    pytest.param("\x1b[200x", id="a start marker's first characters, then x"),
    pytest.param("\x1b[", id="key escape, then ["),
    pytest.param("hello there", id="typing"),
    pytest.param("\x1b\x1b", id="key escape escape"),
    pytest.param("\x1b[1;3A", id="key alt+up"),
    pytest.param("\x1bq", id="key alt+q"),
]

PARSE_SOURCE_SHA256 = "f9837587f1778f18"
"""The start of the SHA-256 hex digest of ``XTermParser.parse``'s source as Textual 8.2.0 to
8.2.8 ship it, taken at 8.2.0, 8.2.4 and 8.2.8. The guard leans on how that parse reads a
paste: a release that changes it is read again before this moves."""

TEXTUAL_VERSION = "8.2.8"
"""The Textual release the guard was read against, and pinned to in ``pyproject.toml``."""

CANARY = (
    "cli/ui/paste_guard.py wraps Textual's XTermParser.parse to keep a raw Escape inside a"
    " bracketed paste (textual#6802, https://github.com/Textualize/textual/issues/6802)"
)
"""How every canary failure starts: which guard, and why it exists."""

Clock = list[float]


@pytest.fixture(autouse=True)
def guarded(monkeypatch: pytest.MonkeyPatch) -> None:
    """The guard in place, as building any rlx app puts it (``ClipboardApp``); the parse and
    the install record this process had before are put back after the test."""
    monkeypatch.setattr(XTermParser, "parse", XTermParser.parse)
    monkeypatch.setattr(paste_guard, "_installed", paste_guard._installed)
    assert paste_guard.install()


@pytest.fixture
def clock(monkeypatch: pytest.MonkeyPatch) -> Clock:
    now = [0.0]
    monkeypatch.setattr(_parser, "get_time", lambda: now[0])
    return now


def textuals_parse() -> paste_guard.Parse:
    parse = paste_guard.textual_parse()
    assert parse is not None
    return parse


def textuals_parser() -> XTermParser:
    """A parser on Textual's own parse, which the guard wraps."""
    with pytest.MonkeyPatch.context() as patch:
        patch.setattr(XTermParser, "parse", textuals_parse())
        return XTermParser()  # it binds its parse here, and keeps it


def fed(parser: XTermParser, reads: list[str], clock: Clock, *, paused: bool) -> list[str]:
    """Every message ``parser`` emits for ``reads``, by its repr. With ``paused`` the clock moves
    past ``ESCAPE_DELAY`` and the parser ticks between two reads; after the last it always
    does, so a sequence still pending resolves."""
    messages: list[Any] = []
    for index, read in enumerate(reads):
        if paused and index:
            clock[0] += 2 * constants.ESCAPE_DELAY
            messages += parser.tick()
        messages += parser.feed(read)
    clock[0] += 2 * constants.ESCAPE_DELAY
    messages += parser.tick()
    return [repr(message) for message in messages]


def paste(text: str) -> list[str]:
    """One paste of ``text``, by its repr, as :func:`fed` gives it."""
    return [repr(events.Paste(text))]


def two_reads(raw: str, positions: Iterable[int]) -> list[list[str]]:
    """``raw`` split in two at each of ``positions``."""
    return [[raw[:at], raw[at:]] for at in positions]


def everywhere(raw: str) -> range:
    """Every position ``raw`` can be split at."""
    return range(1, len(raw))


def inside_an_end_marker(raw: str) -> list[int]:
    """The positions in ``raw`` that split one of its end markers."""
    starts = [at for at in range(len(raw)) if raw.startswith(PASTE_END, at)]
    return [at + offset for at in starts for offset in range(1, len(PASTE_END))]


# --- a raw Escape inside a paste ---------------------------------------------------------------


@pytest.mark.parametrize("body", RAW_ESCAPE_BODIES)
def test_a_paste_holding_a_raw_escape_in_one_read_or_split_in_two_is_one_exact_paste(
    body: str, clock: Clock
) -> None:
    """Fed in one read, and in two reads split at each position with no pause between."""
    raw = bracketed_paste(body)
    for reads in [[raw], *two_reads(raw, everywhere(raw))]:
        assert fed(XTermParser(), reads, clock, paused=False) == paste(body), reads


@pytest.mark.parametrize("body", RAW_ESCAPE_BODIES)
def test_a_paste_holding_a_raw_escape_split_with_a_pause_after_its_start_marker_is_one_paste(
    body: str, clock: Clock
) -> None:
    """Fed in two reads split at each position from the end of the start marker on, with a
    pause between them."""
    raw = bracketed_paste(body)
    for reads in two_reads(raw, range(len(PASTE_START), len(raw))):
        assert fed(XTermParser(), reads, clock, paused=True) == paste(body), reads


@pytest.mark.parametrize("body", RAW_ESCAPE_BODIES)
def test_a_paste_holding_a_raw_escape_in_three_reads_is_one_exact_paste(
    body: str, clock: Clock
) -> None:
    """Fed in three reads: the start marker and the body's first half; the rest of the body
    and half the end marker; the rest. Once with no pause, once with a pause between each."""
    half = len(body) // 2
    reads = [PASTE_START + body[:half], body[half:] + PASTE_END[:3], PASTE_END[3:]]
    for paused in (False, True):
        assert fed(XTermParser(), reads, clock, paused=paused) == paste(body), paused


def test_a_mebibyte_paste_holding_one_escape_in_one_read_is_one_exact_paste() -> None:
    body = "x" * 2**19 + "\x1b" + "y" * (2**19 - 1)
    messages = list(XTermParser().feed(bracketed_paste(body)))
    assert len(messages) == 1
    (message,) = messages
    assert isinstance(message, events.Paste) and message.text == body


def test_input_that_ends_inside_a_paste_gives_no_paste_and_no_key() -> None:
    parser = XTermParser()
    messages = [*parser.feed(PASTE_START + "one\x1btwo"), *parser.feed("")]
    assert messages == []


def test_input_that_ends_after_a_whole_paste_gives_that_paste() -> None:
    parser = XTermParser()
    messages = [*parser.feed(bracketed_paste("one\x1btwo")), *parser.feed("")]
    assert [repr(message) for message in messages] == paste("one\x1btwo")


@pytest.mark.parametrize("body", [*RAW_ESCAPE_BODIES, "one\rtwo", ""])
def test_a_paste_that_ends_exactly_at_its_end_marker_is_delivered_there_with_no_tick(
    body: str,
) -> None:
    """No timer: the paste goes out as the end marker's last character is read, and nothing
    is left pending for a tick to flush."""
    parser = XTermParser()
    assert [repr(message) for message in parser.feed(bracketed_paste(body))] == paste(body)
    assert list(parser.tick()) == []


# --- a pause inside a marker -------------------------------------------------------------------


@pytest.mark.parametrize(
    ("body", "text"),
    [
        pytest.param("one\rtwo", "one\rtwo", id="enter"),
        pytest.param("", "", id="empty"),
        pytest.param("a\nb", "a\nb", id="line feed"),
        pytest.param("a\x00b", "ab", id="NUL, which Textual drops"),
    ],
)
def test_an_ordinary_paste_with_a_pause_inside_its_end_marker_is_one_exact_paste(
    body: str, text: str, clock: Clock
) -> None:
    """Textual's own parse times the end marker's Escape out there, loses the paste and
    swallows what follows: the escape timeout inside an open paste."""
    raw = bracketed_paste(body)
    for reads in two_reads(raw, inside_an_end_marker(raw)):
        assert fed(XTermParser(), reads, clock, paused=True) == paste(text), reads


@pytest.mark.parametrize(
    "raw",
    [
        pytest.param("a" + bracketed_paste("mid") + "b", id="key a, paste, key b"),
        pytest.param(bracketed_paste("one") + bracketed_paste("two"), id="two pastes"),
    ],
)
def test_input_around_a_paste_with_a_pause_inside_an_end_marker_is_as_in_one_read(
    raw: str, clock: Clock
) -> None:
    one_read = fed(textuals_parser(), [raw], clock, paused=False)
    for reads in two_reads(raw, inside_an_end_marker(raw)):
        assert fed(XTermParser(), reads, clock, paused=True) == one_read, reads


def test_a_key_after_a_paste_with_a_pause_inside_its_end_marker_is_that_key(
    clock: Clock,
) -> None:
    raw = bracketed_paste("one\rtwo")
    for first, rest in two_reads(raw, inside_an_end_marker(raw)):
        got = fed(XTermParser(), [first, rest, "x"], clock, paused=True)
        assert got == [*paste("one\rtwo"), repr(events.Key("x", "x"))], first


@pytest.mark.parametrize("body", RAW_ESCAPE_BODIES)
def test_a_pause_inside_the_start_marker_gives_what_textuals_parse_gives(
    body: str, clock: Clock
) -> None:
    """Pinned, not fixed: until its start marker is whole no paste is open, so these bytes
    are an escape sequence like any other, decided by ``ESCAPE_DELAY``. Fed in two reads
    split at each position inside the start marker, with a pause between them."""
    raw = bracketed_paste(body)
    for reads in two_reads(raw, range(1, len(PASTE_START))):
        expected = fed(textuals_parser(), reads, clock, paused=True)
        assert fed(XTermParser(), reads, clock, paused=True) == expected, reads


def test_a_start_marker_a_pause_splits_is_not_guarded_and_its_enter_still_leaks(
    clock: Clock,
) -> None:
    """The guard's known limit, pinned so a change to it is seen (``paste_guard``'s comment
    and the PR say it too): until ``ESC[200~`` is whole no paste is open, so a pause longer
    than ``ESCAPE_DELAY`` inside it lets Textual time its head out as keys, and the paste
    after it is Textual's as before, its Enter a key. Terminals write the marker in one go."""
    raw = bracketed_paste("one\rtwo")
    for reads in two_reads(raw, range(1, len(PASTE_START))):
        got = fed(XTermParser(), reads, clock, paused=True)
        assert got == fed(textuals_parser(), reads, clock, paused=True), reads
        assert repr(events.Key("enter", "\r")) in got, reads
        assert not any(shown.startswith("Paste(") for shown in got), reads


# --- everything else is Textual's --------------------------------------------------------------


@pytest.mark.parametrize("raw", KEYS_REPORTS_AND_PASTES)
def test_keys_reports_and_ordinary_pastes_give_what_textuals_parse_gives(
    raw: str, clock: Clock
) -> None:
    """Fed in one read; in two reads split at each position; and in two reads split at each
    position but inside an end marker, with a pause between them. Inside an end marker
    Textual's own parse loses the paste."""
    paused_at = [at for at in everywhere(raw) if at not in inside_an_end_marker(raw)]
    readings = [
        ([raw], False),
        *((reads, False) for reads in two_reads(raw, everywhere(raw))),
        *((reads, True) for reads in two_reads(raw, paused_at)),
    ]
    for reads, paused in readings:
        expected = fed(textuals_parser(), reads, clock, paused=paused)
        assert fed(XTermParser(), reads, clock, paused=paused) == expected, (reads, paused)


# --- the canary: the Textual seam the guard wraps ----------------------------------------------


def test_canary_the_installed_textual_is_the_release_the_guard_was_read_against() -> None:
    """(a) A bump past the pin, by hand or by Dependabot, is read against the guard first."""
    installed = importlib.metadata.version("textual")
    assert installed == TEXTUAL_VERSION, (
        f"{CANARY}: it was read against Textual {TEXTUAL_VERSION}, and Textual {installed} is"
        " installed. If that release fixes textual#6802, drop the guard and the pin; if not,"
        " read its XTermParser.parse against paste_guard, then move the pin and this test."
    )


def test_canary_textuals_parse_keeps_the_signature_the_guard_wraps() -> None:
    """(b) The guard is called as ``parse(self, token_callback)`` and drives a generator."""
    parse = textuals_parse()
    parameters = list(inspect.signature(parse).parameters)
    assert parameters == ["self", "token_callback"], (
        f"{CANARY}: XTermParser.parse now takes {parameters}, not (self, token_callback)."
    )
    assert inspect.isgeneratorfunction(parse), (
        f"{CANARY}: XTermParser.parse is no longer a generator function, so install() leaves"
        " it unguarded."
    )


def test_canary_the_textual_names_the_guard_reads_are_where_it_reads_them() -> None:
    """(c) The private names: the markers, the read request, the two interruptions, the
    escape delay, and the parser's ``read1``, ``feed`` and ``tick`` that drive them."""
    names = {
        "textual._xterm_parser.BRACKETED_PASTE_START": (
            getattr(_xterm_parser, "BRACKETED_PASTE_START", None) == PASTE_START
        ),
        "textual._xterm_parser.BRACKETED_PASTE_END": (
            getattr(_xterm_parser, "BRACKETED_PASTE_END", None) == PASTE_END
        ),
        "textual._parser.Read1(timeout=None)": (
            getattr(getattr(_parser, "Read1", None), "_fields", None) == ("timeout",)
            and _parser.Read1().timeout is None
        ),
        "XTermParser.read1 is textual._parser.Read1": (
            getattr(XTermParser, "read1", None) is getattr(_parser, "Read1", object())
        ),
        "textual._parser.ParseTimeout": isinstance(getattr(_parser, "ParseTimeout", None), type)
        and issubclass(_parser.ParseTimeout, Exception),
        "textual._parser.ParseEOF": isinstance(getattr(_parser, "ParseEOF", None), type)
        and issubclass(_parser.ParseEOF, Exception),
        "textual.constants.ESCAPE_DELAY": isinstance(
            getattr(constants, "ESCAPE_DELAY", None), float
        ),
        "Parser.feed and Parser.tick": all(
            callable(getattr(_parser.Parser, name, None)) for name in ("feed", "tick")
        ),
    }
    moved = [name for name, present in names.items() if not present]
    assert moved == [], f"{CANARY}: these Textual names moved or changed: {moved}."


def textuals_parse_source(module_source: str) -> str:
    """``XTermParser.parse``'s source, read from its module's syntax tree: the class's own
    method, not any function named ``parse``."""
    for node in ast.parse(module_source).body:
        if isinstance(node, ast.ClassDef) and node.name == "XTermParser":
            for member in node.body:
                if isinstance(member, ast.FunctionDef) and member.name == "parse":
                    return ast.get_source_segment(module_source, member) or ""
    raise AssertionError(f"{CANARY}: there is no XTermParser.parse in textual._xterm_parser.")


def test_canary_textuals_parse_is_the_source_the_guard_was_read_against() -> None:
    """(d) The parse the guard wraps, by its source."""
    source = textuals_parse_source(inspect.getsource(_xterm_parser))
    digest = hashlib.sha256(source.encode()).hexdigest()
    assert digest[: len(PARSE_SOURCE_SHA256)] == PARSE_SOURCE_SHA256, (
        f"{CANARY}: Textual's XTermParser.parse has changed (sha256 {digest[:16]}). Read it"
        " against paste_guard before the pin moves; if it fixes textual#6802, drop the guard."
    )


def test_canary_textuals_parse_still_behaves_as_the_guard_expects(clock: Clock) -> None:
    """(e) The empty paste Textual's parse flushes for an end marker read alone, which is how
    the guard closes that parse's paste; and the defect itself, still in it."""
    handshake = fed(textuals_parser(), [PASTE_START, PASTE_END], clock, paused=False)
    assert handshake == paste(""), (
        f"{CANARY}: Textual's parse no longer flushes one empty Paste for a start and end"
        f" marker alone ({handshake}), which the guard relies on to close that parse's paste."
    )
    leaked = fed(textuals_parser(), [bracketed_paste("one\x1b\rtwo")], clock, paused=False)
    assert any(shown.startswith("Key(") for shown in leaked), (
        f"{CANARY}: Textual's own parse now keeps a raw Escape inside a paste as text"
        f" ({leaked}): textual#6802 looks fixed. Drop the guard and the textual==8.2.8 pin."
    )


# --- the install -------------------------------------------------------------------------------


@pytest.fixture
def uninstalled(monkeypatch: pytest.MonkeyPatch) -> None:
    """Textual's own parse in place and nothing installed, as in a process that has built no
    asq app yet; both undone after."""
    monkeypatch.setattr(XTermParser, "parse", textuals_parse())
    monkeypatch.setattr(paste_guard, "_installed", None)


@pytest.mark.usefixtures("uninstalled")
def test_building_a_clipboard_app_installs_the_guard(clock: Clock) -> None:
    raw = bracketed_paste("one\x1b\rtwo")
    before = fed(XTermParser(), [raw], clock, paused=False)
    ClipboardApp()
    after = fed(XTermParser(), [raw], clock, paused=False)
    assert before != paste("one\x1b\rtwo"), "the premise: Textual's own parse sends keys"
    assert after == paste("one\x1b\rtwo")


def test_a_second_install_says_installed_and_keeps_the_guard_and_textuals_parse() -> None:
    parse, textual = XTermParser.parse, paste_guard.textual_parse()
    assert paste_guard.install() is True
    assert (XTermParser.parse, paste_guard.textual_parse()) == (parse, textual)


@pytest.mark.usefixtures("uninstalled")
def test_installing_twice_wraps_textuals_parse_once() -> None:
    textual = XTermParser.parse
    assert paste_guard.install() is True
    guarded = XTermParser.parse
    assert paste_guard.install() is True
    assert XTermParser.parse is guarded is not textual
    assert getattr(guarded, paste_guard.GUARD_MARK) is textual  # one layer: Textual's own
    assert not hasattr(textual, paste_guard.GUARD_MARK)


@pytest.mark.usefixtures("uninstalled")
def test_an_install_that_lost_its_record_still_wraps_once() -> None:
    """As after a reload of the module: the record is gone, the guarded parse is still in
    place, and installing again keeps it rather than wrapping it a second time."""
    textual = XTermParser.parse
    assert paste_guard.install() is True
    guarded = XTermParser.parse
    paste_guard._installed = None
    assert paste_guard.install() is True
    assert XTermParser.parse is guarded
    assert paste_guard.textual_parse() is textual


def _no_generator(self: Any, token_callback: Callable[[Any], None]) -> None:
    """A parse that is not a generator: not the protocol the guard wraps."""


@pytest.mark.parametrize(
    "move",
    [
        pytest.param(
            lambda patch: patch.setitem(sys.modules, "textual._parser", None), id="module"
        ),
        pytest.param(lambda patch: patch.delattr(_xterm_parser, "BRACKETED_PASTE_END"), id="name"),
        pytest.param(lambda patch: patch.setattr(XTermParser, "parse", _no_generator), id="parse"),
    ],
)
@pytest.mark.usefixtures("uninstalled")
def test_with_a_textual_name_moved_or_its_parse_no_generator_install_says_no_and_leaves_it(
    move: Callable[[pytest.MonkeyPatch], None], monkeypatch: pytest.MonkeyPatch
) -> None:
    move(monkeypatch)
    before = XTermParser.parse
    assert paste_guard.install() is False
    assert XTermParser.parse is before


def _flushes_no_paste_but_its_own(
    self: Any, token_callback: Callable[[Any], None]
) -> Generator[Any, str, None]:
    """A parse that flushes nothing for an end marker, and takes one paste itself: an empty
    one, which ``!`` stands for."""
    while True:
        if (yield _parser.Read1()) == "!":
            token_callback(events.Paste(""))


@pytest.mark.usefixtures("uninstalled")
def test_a_parse_that_flushes_nothing_for_the_end_marker_keeps_its_later_empty_paste(
    monkeypatch: pytest.MonkeyPatch, clock: Clock
) -> None:
    """The guard drops the empty paste Textual's parse flushes while the end marker is
    replayed to it, and none after: a parse that flushes nothing there has its own empty
    paste later."""
    monkeypatch.setattr(XTermParser, "parse", _flushes_no_paste_but_its_own)
    assert paste_guard.install()
    got = fed(XTermParser(), [bracketed_paste("a") + "!"], clock, paused=False)
    assert got == [*paste("a"), *paste("")]


# --- the app -----------------------------------------------------------------------------------


class InputRecorder(ClipboardApp[None]):
    """An rlx app (so the guard is installed) that records each paste, each key and each
    escape its binding takes."""

    BINDINGS: ClassVar = [Binding("escape", "escaped", priority=True)]

    def __init__(self) -> None:
        super().__init__()
        self.pastes: list[str] = []
        self.keys: list[str] = []
        self.escapes = 0

    def action_escaped(self) -> None:
        self.escapes += 1

    def on_paste(self, event: events.Paste) -> None:
        self.pastes.append(event.text)

    def on_key(self, event: events.Key) -> None:
        self.keys.append(event.key)


@pytest.mark.usefixtures("uninstalled")
def test_a_raw_escape_paste_then_a_lone_escape_reach_the_app_as_one_paste_then_escape(
    clock: Clock,
) -> None:
    """From the terminal's bytes, through a parser built after the app, as the driver builds
    it: the paste arrives whole with no key (no Enter), and a lone Escape pressed after it
    still reaches the app as the escape key once Textual's ``ESCAPE_DELAY`` has passed."""

    async def drive() -> InputRecorder:
        app = InputRecorder()
        async with app.run_test() as pilot:
            parser = XTermParser()
            messages = [*parser.feed(bracketed_paste("one\x1b\rtwo")), *parser.feed("\x1b")]
            clock[0] += 2 * constants.ESCAPE_DELAY
            messages += parser.tick()
            for message in messages:
                app.post_message(message)
            await pilot.pause()
        return app

    app = asyncio.run(drive())
    assert app.pastes == ["one\x1b\rtwo"]
    assert app.escapes == 1
    assert "enter" not in app.keys, app.keys
