"""The parts of the board's layout (B2, the board revamp): what ``cli.ui.board`` composes.

``board.BoardPanel`` lays them out and owns their data; each part here only draws what it is
handed and says what was picked. Every part a person can click names the key that does the
same (``board_key``): the board has a key for every click (B2's spec, and its test).
"""

from __future__ import annotations

from typing import ClassVar

from rich.text import Text
from textual import on
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.message import Message
from textual.widgets import Button, Static

WORK = "work"
STREAM = "stream"
PANES: tuple[str, str] = (WORK, STREAM)
"""The two halves of the board that share one column below ``board.WIDE_FROM``."""


class SwitchButton(Button, can_focus=False):
    """A half of :class:`BoardSwitch`: clicked, not tabbed to — ``b`` is its key."""

    board_key: ClassVar[str] = "b"


class BoardSwitch(Horizontal):
    """Below ``board.WIDE_FROM`` columns Work and the Stream share one column: this line says
    which shows, and switches to the other (``b``, or a click on its name)."""

    DEFAULT_CSS = """
    BoardSwitch { height: 1; }
    BoardSwitch Static { width: auto; height: 1; color: $text-muted; }
    BoardSwitch SwitchButton { min-width: 0; margin: 0 1 0 0; }
    BoardSwitch SwitchButton.-shown { text-style: bold reverse; }
    """
    board_key: ClassVar[str] = "b"

    class Picked(Message):
        """A half's name was clicked: show it."""

        def __init__(self, pane: str) -> None:
            self.pane = pane
            super().__init__()

    def compose(self) -> ComposeResult:
        yield SwitchButton("Work", id="switch-work", compact=True)
        yield SwitchButton("Stream", id="switch-stream", compact=True)
        yield Static(Text("b switches", style="dim"), id="switch-tail")

    def show(self, pane: str) -> None:
        for name in PANES:
            self.query_one(f"#switch-{name}", SwitchButton).set_class(name == pane, "-shown")

    @on(Button.Pressed)
    def _pressed(self, event: Button.Pressed) -> None:
        event.stop()
        self.post_message(self.Picked((event.button.id or "").removeprefix("switch-")))


LANE_ROWS = 5
"""The most items the needs-you lane lists; the rest are counted on its last row."""


class LaneRow(Static):
    """One thing that needs the person: its key, its glyph, its text and what the key does."""

    DEFAULT_CSS = """
    LaneRow { height: 1; text-wrap: nowrap; text-overflow: ellipsis; }
    LaneRow:hover { background: $boost; }
    """

    def __init__(self, index: int) -> None:
        super().__init__("", id=f"lane-{index}")
        self.index = index
        self.board_key = str(index + 1)
        """The key that does what a click on this row does (``1`` for the first)."""

    def on_click(self) -> None:
        self.post_message(NeedsYouLane.Picked(self.index))


class NeedsYouLane(Vertical):
    """What needs the person, one row each (B2): its text and its action, by key or click.

    The board's own; the app-wide strip (M7) keeps ``board.NeedsYouStrip``'s counts."""

    DEFAULT_CSS = """
    NeedsYouLane { height: auto; max-height: 7; padding: 0 1; }
    NeedsYouLane #lane-lead { height: 1; }
    """

    class Picked(Message):
        """A row was clicked, or its key pressed: act on item ``index``."""

        def __init__(self, index: int) -> None:
            self.index = index
            super().__init__()

    def compose(self) -> ComposeResult:
        yield Static("", id="lane-lead")
        for index in range(LANE_ROWS):
            yield LaneRow(index)

    def show(self, lead: Text, items: list[Text]) -> None:
        """``lead`` on the first row, then one row per item (the rest counted on the last)."""
        self.query_one("#lane-lead", Static).update(lead)
        rows = list(self.query(LaneRow))
        listed = items if len(items) <= LANE_ROWS else items[: LANE_ROWS - 1]
        for row, item in zip(rows, listed, strict=False):
            row.update(Text.assemble((f"{row.board_key}  ", "bold cyan"), item))
            row.display = True
        if len(items) > LANE_ROWS:
            more = rows[LANE_ROWS - 1]
            more.update(Text(f"   + {len(items) - len(listed)} more", style="dim"))
            more.display = True
            listed = [*listed, Text("")]
        for row in rows[len(listed) :]:
            row.display = False
