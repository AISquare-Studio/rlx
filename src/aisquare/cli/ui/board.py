"""The Board: the widgets of ``aisquare board -w``, as one panel any view can host.

``BoardPanel`` is the sessions block, the task table, the live feed and the
detail pane of the board TUI, with the behaviours they had as ``BoardApp``:
click or Enter on a task or a feed line shows its full content below; ``d``
flips the table to the done archive; ``o`` opens the author session's
transcript at the selected moment; ``v`` freezes the feed into selectable text;
``a`` toggles autoscroll; ``b`` hides the board column; ``r`` refreshes now.
Every line between two parts is a ``Divider``: drag it to resize, double-click
it to put the stylesheet's size back; each size is remembered in ``state.json``
under one key for every project's board.

Lifted out of ``cli.watch`` (docs/plans/fleet-tui.md §4.2, §9 Phase 6) so the
fleet UI's Project view can show a project's board as one tab while
``aisquare board -w`` keeps composing the same widget under its own footer and
theme picker. The pure renderers — ``feed_line``, ``_session_lines``, the
detail texts, the transcript helpers — stay in ``cli.watch``: the Rich fallback
there needs them without Textual, and other modules import them from there.

**What needs you** (#286) is one line over the board: the waiting panes, the owner's
questions, the cards waiting on a gate and the blocked ones, counted by the captain's
attention queue's own classifier (``services.captain.queue.observe``) over what the
tick already read — no second definition of "needs the owner", no tmux call, no extra
store read, and nothing written to ``captain/queue.json``. Each count is a button that
goes to the first thing it counts. With no open tasks the table says where they come
from.

All data comes from ``services.team``; nothing here writes. The panel resolves
its project ONCE (or takes it from the caller) and passes it to every board
read, so a tick never costs a ``git rev-parse`` and never touches ``os.environ``.

Two bounds, both because the fleet UI keeps every view it builds (one
``ProjectView`` per project ever selected, cli/ui/app.py): a tick behind a
hidden tab is SKIPPED and made up in ``on_show``, the way ``TerminalPane``
skips frames for a pane that is not on screen; and the feed keeps its last
:attr:`BoardPanel.FEED_LIMIT` lines, evicting the oldest from the OptionList
and the event cache together. Without them, N visited projects cost N store
reads every ``interval`` seconds forever, and each panel's feed grows for as
long as the session lives.
"""

from __future__ import annotations

import contextlib
import functools
import textwrap
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, ClassVar

from rich.cells import cell_len
from rich.text import Text
from textual import events, on
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.coordinate import Coordinate
from textual.geometry import Size
from textual.message import Message
from textual.screen import ModalScreen
from textual.widget import Widget
from textual.widgets import Button, DataTable, Input, OptionList, Static, TabbedContent
from textual.widgets.option_list import Option, OptionDoesNotExist
from textual.worker import Worker, WorkerState

from aisquare.cli import board_names, watch
from aisquare.cli.ui.autosave import Autosave
from aisquare.cli.ui.board_parts import STREAM, WORK, BoardSwitch, NeedsYouLane
from aisquare.cli.ui.clipboard import copy_and_tell
from aisquare.cli.ui.divider import Divider, floor_of
from aisquare.cli.ui.ticks import while_running
from aisquare.core.state_file import read_state
from aisquare.core.store import unmet_needs
from aisquare.core.terminal import mac_keyboard
from aisquare.models import (
    CLOSED_STATUSES,
    FleetAgentStatus,
    ProjectInfo,
    TeamEvent,
    TeamSession,
    TeamTask,
)
from aisquare.services import claude_accounts as accounts_service
from aisquare.services import settings as settings_service
from aisquare.services import team as team_service

OPEN_STATUSES = ("todo", "doing", "review", "blocked")


def _now() -> datetime:
    """The board's clock: what a card's age and a question's day are read against (one place,
    so a test pins it rather than race the wall clock: #823)."""
    return datetime.now(UTC)


LABELS_TTL = 30.0
"""How long the sessions block keeps the slot labels before asking the registry again."""
LABELS_WORKER = "board-account-labels"
HISTORY_WORKER = "board-task-history"
"""The reader's history read, off the UI thread: one store query per task opened."""
TASKS_HIDDEN = "tasks hidden — b shows them"
"""What the feed's title adds while ``b`` has the Work column out of sight (wide)."""
WIDE_FROM = 120
"""Panel width (columns) from which Work and the Stream sit side by side (B2). Below it they
share one column, and :class:`~aisquare.cli.ui.board_parts.BoardSwitch` says which shows.
Measured against the PANEL, not the terminal: asq's sidebar takes about 31 beside it."""
TRANSCRIPT_HINT = "\npress o to open the transcript at this moment"
COLUMN_WIDTH_KEY = "board_column_width"
"""The ``state.json`` key of the board column's width (the line beside the live feed)."""
TEAM_HEIGHT_KEY = "board_team_height"
"""The ``state.json`` key of the team block's height (the line above the tasks)."""
DETAIL_HEIGHT_KEY = "board_detail_height"
"""The ``state.json`` key of the detail pane's height (the line above it)."""
DIVIDER_KEYS: dict[str, tuple[str, str]] = {
    "side-divider": ("left_square_bracket", "right_square_bracket"),
    "team-divider": ("left_parenthesis", "right_parenthesis"),
    "detail-divider": ("left_curly_bracket", "right_curly_bracket"),
}
"""Each line's keys, (shrink, grow) its target by :data:`DIVIDER_STEP`: the keys a drag has
(B2: every click has a key). ``=`` puts every line back, as a double-click puts one back."""
DIVIDER_STEP = 4
VIEW_KEY = "board_view:"
"""The ``state.json`` key of a project's board view, before the project's id (B2): its folds,
its Stream filter and, below :data:`WIDE_FROM`, the half it shows. One key per project, so two
boards never write over each other's."""

WAITING = "waiting"
"""Not a status: a ``todo`` whose ``needs`` are not all done yet, which gets its own icon."""
STATUS_ICONS: dict[str, tuple[str, str, str]] = {
    "todo": ("○", "bold", "to do — nobody has it yet"),
    WAITING: ("⧗", "yellow", "to do, waiting on a task it needs"),
    "doing": ("▶", "green", "doing — claimed and being worked on"),
    "review": ("👀", "cyan", "in review — waiting to be checked"),
    "blocked": ("⛔", "red", "blocked — the detail says on what"),
    "done": ("✓", "green", "done"),
    "dropped": ("✗", "dim", "dropped — no longer worth doing"),
}
"""status → (glyph, style, what it means): the tasks table's one icon per state, and the
legend the help screens print from it. Each glyph is one or two cells wide."""
ICON_CELLS = 2
"""The status column's width: the widest glyph, so every row's title starts at one column."""
TASK_COLUMNS = ("#", "st", "title", "owner", "age")
"""The Work table (B2): the number on the board, the status icon, the title, who has it, and how
long it has been in its status."""
WORK_GROUPS: tuple[tuple[str, str, frozenset[str]], ...] = (
    ("doing", "Doing", frozenset({"doing"})),
    ("blocked", "Blocked", frozenset({"blocked"})),
    ("review", "Review", frozenset({"review"})),
    ("todo", "To do", frozenset({"todo"})),
    ("done", "Done", frozenset(CLOSED_STATUSES)),
)
"""Work's groups in order: (key, heading, the statuses it holds). Done holds the dropped too."""
GROUP_ROW = "group:"
"""The row key of a group's heading: ``group:done``. No task's id starts so."""
DEFAULT_FOLDS: frozenset[str] = frozenset({"done"})
"""The groups folded on a board that never had a fold changed: Done (B2)."""
OWNER_CELLS = 14
"""The widest the owner column grows; a longer name ends in ``…``."""
BOARD_KEYS: tuple[tuple[str, str], ...] = (
    ("click", "a task or stream line: its full content below"),
    ("d", "unfold the done archive · fold it again"),
    ("f", "fold or unfold the group at the cursor"),
    ("o", "the author's transcript at that moment"),
    ("/", "filter the stream · Esc clears the filter"),
    ("Enter", "a task: its reader, full height, with history"),
    ("Esc", "back from the reader"),
    ("v  c", "freeze the feed to select text · copy it"),
    ("a", "feed autoscroll on or off"),
    ("b", "Work or Stream (narrow) · hide Work (wide)"),
    ("r", "refresh now"),
    ("lines", "drag a line to resize; double-click resets it"),
    ("[ ] ( ) { }", "the same by key: Work, the team, the detail"),
    ("=", "every line back to its first size"),
)
"""The board's own keys, as the help screens list them."""
MAC_COPY_KEY = ("v  c", "freeze the feed to select text · c copies (not ⌘C)")
"""The copy row for a Mac's keyboard, where a copy key left unnamed reads as ⌘C: Terminal.app and
iTerm2 keep ⌘C as their own Copy, so it never reaches the board."""


def board_keys() -> tuple[tuple[str, str], ...]:
    """:data:`BOARD_KEYS` as the user's help reads them: with :data:`MAC_COPY_KEY` when they
    type on a Mac (``core.terminal.mac_keyboard``), whatever the host runs."""
    if not mac_keyboard():
        return BOARD_KEYS
    return tuple(
        MAC_COPY_KEY if key == MAC_COPY_KEY[0] else (key, what) for key, what in BOARD_KEYS
    )


def status_icon(status: str, *, waiting: bool = False) -> Text:
    """The status as its icon, coloured, padded to :data:`ICON_CELLS`."""
    glyph, style, _meaning = STATUS_ICONS.get(WAITING if waiting else status, ("?", "dim", ""))
    return Text(glyph + " " * max(0, ICON_CELLS - cell_len(glyph)), style=style)


EMPTY_TASKS = "No open tasks. The manager writes them when you give it a goal on the Manager tab."
"""The task table's body with no open task: where tasks come from (NN/g's empty state)."""
EMPTY_TASKS_ALONE = (
    "No open tasks. The manager writes them when you give it a goal: in rlx, on this "
    "project's Manager tab."
)
""":data:`EMPTY_TASKS` in ``board -w``, which has no Manager tab of its own (B1 #8)."""
EMPTY_ROW = "__no-open-tasks__"
"""The row key of :data:`EMPTY_TASKS`: no task's id, so a pick on it shows nothing."""
NEEDS_YOU: tuple[tuple[str, str, str, str], ...] = (
    ("waiting", "🔔", "waiting", "waiting"),
    ("question", "❓", "question", "questions"),
    ("review", "👀", "in review", "in review"),
    ("blocked", "⛔", "blocked", "blocked"),
)
"""The strip's counts in order: (kind, glyph, one, many). ``stale`` is left to the captain."""
NOTHING_NEEDS_YOU = "nothing needs you — the manager is working; talk to it on the Manager tab"
NOTHING_NEEDS_YOU_ALONE = (
    "nothing needs you — the manager is working; talk to it in rlx, on this project's Manager tab"
)
""":data:`NOTHING_NEEDS_YOU` in ``board -w`` (B1 #8)."""
WHAT_TO_DO_HERE: tuple[tuple[str, str], ...] = (
    ("🔔  ❓", "need you: click the agent and answer in its pane"),
    ("👀", "waits for a reviewer agent, or for you on the PR"),
    ("typing", "nothing here is typed into — you talk to the manager"),
)
"""What a human does on the board (#286), as the help screens list it."""
WHAT_TO_DO_ALONE: tuple[tuple[str, str], ...] = (
    ("🔔  ❓", "need you: click one for what it asks; answer in rlx"),
    WHAT_TO_DO_HERE[1],
    WHAT_TO_DO_HERE[2],
)
""":data:`WHAT_TO_DO_HERE` in ``board -w``: no left pane to select the agent in (B1 #8, #10)."""


@dataclass(frozen=True)
class NeedsYou:
    """What the strip shows: per kind, what it counts — task ids or agent labels."""

    tasks: dict[str, list[str]] = field(default_factory=dict)
    """``blocked`` and ``review``: the cards, in the table's order."""
    agents: dict[str, list[str]] = field(default_factory=dict)
    """``waiting`` and ``question``: the agents' labels."""

    def count(self, kind: str) -> int:
        return len(self.tasks.get(kind, ())) + len(self.agents.get(kind, ()))

    def line(self) -> str:
        """The strip as one line of text: what a test, and a screen reader, reads."""
        parts = [
            f"{glyph} {self.count(kind)} {one if self.count(kind) == 1 else many}"
            for kind, glyph, one, many in NEEDS_YOU
            if self.count(kind)
        ]
        if not parts:
            return NOTHING_NEEDS_YOU
        return "needs you: " + " · ".join(parts) + " — click one to go to it"


@dataclass(frozen=True)
class _TickSources:
    """The attention queue's sources over what one board tick read: one project, its
    sessions, tasks and the feed's events. No pane is read (the fleet's agents are tmux's)
    and no pull request asked for, so nothing here leaves the tick."""

    project: ProjectInfo
    read_sessions: list[TeamSession]
    read_tasks: list[TeamTask]
    read_events: list[TeamEvent]
    seq: int

    def projects(self) -> list[ProjectInfo]:
        return [self.project]

    def agents(self, project: ProjectInfo) -> list[FleetAgentStatus]:
        return []

    def tasks(self, project_id: str) -> list[TeamTask]:
        return self.read_tasks

    def events(self, project_id: str, *, since_seq: int) -> list[TeamEvent]:
        return [event for event in self.read_events if event.seq > since_seq]

    def recent_events(self, project_id: str, *, limit: int) -> list[TeamEvent]:
        return self.read_events[-limit:]

    def events_about(self, project_id: str, task_id: str, *, since: datetime) -> list[TeamEvent]:
        return [e for e in self.read_events if e.task_id == task_id and e.created_at >= since]

    def latest_seq(self, project_id: str) -> int:
        return self.seq

    def sessions(self, project_id: str) -> list[TeamSession]:
        return self.read_sessions

    def pane_tail(self, status: FleetAgentStatus) -> None:
        return None

    def pull_requests(self, project: ProjectInfo) -> list[Any]:
        return []


def needs_you(
    project: ProjectInfo,
    sessions: Sequence[TeamSession],
    tasks: Sequence[TeamTask],
    events: Sequence[TeamEvent],
    *,
    seq: int,
    now: datetime | None = None,
    fleet_labels: Mapping[str, str] | None = None,
) -> NeedsYou:
    """What needs the owner on one board, from what its tick read (#286).

    Questions, blocked cards and cards waiting on a gate are the attention queue's
    observations (``queue.observe``). A question counts while it is still its asker's last
    word on the board and the asker is live: the queue's own rows are resolved by the
    owner, which a board cannot see. A waiting pane is a live session in the fleet's
    ``attention`` state — the state the queue reads as "waits on you".
    """
    from aisquare.services.captain import queue  # lazy: the captain's module, read on a tick

    sources = _TickSources(project, list(sessions), list(tasks), list(events), seq)
    seen = queue.observe(sources, cursors={}, now=now or _now())
    live_sessions = {session.id: session for session in sessions if session.ended_at is None}
    last_said: dict[str, int] = {}
    by_id = {event.id: event for event in events}
    for event in events:
        if event.session_id:
            last_said[event.session_id] = max(last_said.get(event.session_id, 0), event.seq)
    found = NeedsYou()
    for observation in seen.observations:
        if observation.kind in ("blocked", "review") and observation.source_ref:
            found.tasks.setdefault(observation.kind, []).append(observation.source_ref)
        elif observation.kind == "question":
            asked = by_id.get(observation.source_ref or "")
            asker = asked.session_id if asked is not None else None
            if (
                asked is None
                or asker not in live_sessions
                or last_said.get(asker or "") != asked.seq
            ):
                continue  # answered since (the asker spoke again), or nobody left to answer
            asking = live_sessions.get(asker or "")
            label = (
                team_service.board_name(asking, fleet_labels) if asking else observation.agent or ""
            )
            if label not in found.agents.get("question", []):
                found.agents.setdefault("question", []).append(label)
    for session in live_sessions.values():
        if session.state == "attention":
            # The left pane's name, as a relabel left it (#140): the button finds the agent by it.
            found.agents.setdefault("waiting", []).append(
                team_service.board_name(session, fleet_labels)
            )
    return found


NOTHING_SHORT = "nothing needs you"
""":data:`NOTHING_NEEDS_YOU` where the whole sentence does not fit."""
STRIP_TAIL = "— click one to go to it"
BUTTON_CELLS = 3
"""What a compact button adds to its label in cells: its padding and the margin after it."""
BUTTON_PAD = 2
"""A compact button's padding, one cell each side of its label."""


class NeedsYouStrip(Horizontal):
    """The line over the board: what needs you, each count a button to it (#286).

    It fits the width it has rather than run off it: the whole line, else without its
    tail, else each count as its glyph and number alone (``🔔 1``). Inside `asq` at 80
    columns the board is about 50 wide, which the whole line overran."""

    DEFAULT_CSS = """
    NeedsYouStrip { height: 1; }
    NeedsYouStrip Static { width: auto; height: 1; }
    NeedsYouStrip Button { margin: 0 1 0 0; min-width: 0; }
    """

    class Picked(Message):
        """A count was pressed: go to the first thing it counts."""

        def __init__(self, kind: str) -> None:
            self.kind = kind
            super().__init__()

    def __init__(self, *, id: str | None = None, nothing: str = NOTHING_NEEDS_YOU) -> None:
        super().__init__(id=id)
        self.needs = NeedsYou()
        self.nothing = nothing
        """What the strip says when nothing needs you: where its host sends the person."""

    def compose(self) -> ComposeResult:
        yield Static("", id="needs-you-lead")
        for kind, *_rest in NEEDS_YOU:
            # Focusable only while a count shows it (``_fit``). Textual's first focus takes a
            # hidden widget too (``focusable`` reads visibility, not display): ``board -w``
            # opened on a count no count showed, and the arrows reached nothing until a Tab or
            # a click (tester's note on #554).
            button = Button("", id=f"needs-you-{kind}", compact=True)
            button.can_focus = False
            yield button
        yield Static("", id="needs-you-tail")

    def show(self, needs: NeedsYou) -> None:
        self.needs = needs
        self._fit(self.size.width)

    def on_resize(self, event: events.Resize) -> None:
        self._fit(event.size.width)

    def _fit(self, width: int) -> None:
        """The longest form of the line that fits ``width`` (all of them before a size)."""
        lead = self.query_one("#needs-you-lead", Static)
        tail = self.query_one("#needs-you-tail", Static)
        counts = [
            (kind, glyph, one, many, self.needs.count(kind)) for kind, glyph, one, many in NEEDS_YOU
        ]
        shown = [entry for entry in counts if entry[4]]
        for kind, *_rest, count in counts:
            button = self.query_one(f"#needs-you-{kind}", Button)
            button.display = button.can_focus = bool(count)
        if not shown:
            fits = width <= 0 or cell_len(self.nothing) <= width
            lead.update(Text(self.nothing if fits else NOTHING_SHORT, style="dim"))
            tail.update("")
            return
        lead_text = "needs you: "
        for compact, with_tail in ((False, True), (False, False), (True, False)):
            labels = {
                kind: f"{glyph} {count}"
                if compact
                else f"{glyph} {count} {one if count == 1 else many}"
                for kind, glyph, one, many, count in shown
            }
            used = cell_len(lead_text) + sum(
                cell_len(label) + BUTTON_CELLS for label in labels.values()
            )
            if with_tail:
                used += cell_len(STRIP_TAIL)
            if width <= 0 or used <= width or compact:
                break
        for kind, label in labels.items():
            button = self.query_one(f"#needs-you-{kind}", Button)
            button.label = label
            # Sized from the label, not left to ``width: auto``: a Button keeps the width
            # it measured for its LAST label, so a terminal shrunk from 160 columns to 80
            # kept "🔔 1 waiting"'s 14 cells for "🔔 1", and the last counts ran off the
            # strip (review of #324, round 2, from the tmux walk).
            button.styles.width = cell_len(label) + BUTTON_PAD
        lead.update(Text(lead_text, style="bold"))
        tail.update(Text(STRIP_TAIL, style="dim") if with_tail else "")

    @on(Button.Pressed)
    def _pressed(self, event: Button.Pressed) -> None:
        event.stop()
        kind = (event.button.id or "").removeprefix("needs-you-")
        self.post_message(self.Picked(kind))


def stream_line(
    event: TeamEvent,
    roles: dict[str, str],
    names: Mapping[str, str] | None = None,
    statuses: Mapping[str, str] | None = None,
) -> Text:
    """One Stream line (B2), on one row: its time, then a chip for the agent, one for the kind,
    and — for a line about a task — one for that task's status now, then the text."""
    emoji, name, style = watch._who(event, roles, names)
    verb_emoji, verb = watch._KIND_VERB.get(event.kind, ("•", event.kind))
    line = Text(no_wrap=True, overflow="ellipsis")
    line.append(f"{watch.clock(event.created_at)} ", style="dim")
    line.append(f" {emoji} {name} ", style=f"{style} reverse")
    line.append(" ")
    line.append(f" {verb_emoji} {verb} ", style="bold reverse")
    status = (statuses or {}).get(event.task_id or "")
    if status is not None:
        glyph, chip_style, _meaning = STATUS_ICONS.get(status, ("?", "dim", ""))
        line.append(" ")
        line.append(f" {glyph} {status} ", style=f"{chip_style} reverse")
    line.append(" ")
    line.append(watch.one_row(watch.shown_text(event)))
    if event.to_role:
        line.append(f" → {event.to_role}", style="italic yellow")
    return line


def board_help(*, standalone: bool = False) -> Text:
    """The board's keys, what a human does here, and the icon legend, for a help screen;
    ``standalone`` for ``board -w``'s, which has no left pane beside it."""
    text = Text()
    for key, what in board_keys():
        text.append(f"  {key:<11}", style="bold cyan")
        text.append(f" {what}\n")
    text.append("\n  what to do here\n", style="bold")
    for key, what in WHAT_TO_DO_ALONE if standalone else WHAT_TO_DO_HERE:
        text.append(f"  {key:<11}", style="bold cyan")
        text.append(f" {what}\n")
    text.append("\n  task icons\n", style="bold")
    for state, (glyph, style, meaning) in STATUS_ICONS.items():
        text.append("  ")
        text.append(glyph + " " * max(0, ICON_CELLS - cell_len(glyph)), style=style)
        text.append(f"  {meaning}", style="dim" if state == "dropped" else "")
        text.append("\n")
    return text


def click_key(widget: Widget) -> str | None:
    """The key that does what a click on ``widget`` does (B2: every click has a key): its
    ``board_key``, a line's grow key (:data:`DIVIDER_KEYS`), or ``None`` for one with none."""
    key = getattr(widget, "board_key", None)
    if isinstance(key, str):
        return key
    if isinstance(widget, Divider) and widget.id in DIVIDER_KEYS:
        return DIVIDER_KEYS[widget.id][1]
    return None


class StreamList(OptionList):
    """The Stream's lines: a click shows a line's detail, as Enter does."""

    board_key: ClassVar[str] = "enter"


class SearchField(Input, can_focus=False):
    """The Stream's filter: ``/`` opens it, as a click on it does. Focusable only while open:
    hidden, Textual's first focus would take it (``focusable`` reads visibility, not display)."""

    board_key: ClassVar[str] = "slash"


class PickableTable(DataTable[Text]):
    """A DataTable that reports real mouse picks on data rows.

    ``DataTable._on_click`` stops the event inside the widget for data cells
    (textual 8.2.8, ``_data_table.py`` ``_on_click``), so a handler on an
    ancestor never sees a row click — while the header/blank clicks that DO
    bubble aren't row picks at all. The only place that sees exactly the right
    events is the widget's own click path: post ``RowPicked`` keyed off the
    click's own ``style.meta`` (``row >= 0`` — headers are ``row == -1`` and
    blank space carries no meta; ``out_of_bounds`` clicks past the last column
    stay accepted for a row cursor, exactly as the widget itself accepts them).

    Deliberately NO ``super()._on_click`` call: textual dispatches ``_on_click``
    once per class in the MRO (``message_pump._get_dispatch_methods`` walks
    ``__mro__``), so ``DataTable._on_click`` runs after this handler anyway.
    Calling it explicitly would run it twice per click — a double cursor move
    plus a spurious ``RowSelected`` from the second pass, which finds the cursor
    already on the clicked cell. Reading the row from the event meta (not the
    cursor) keeps this correct in either dispatch order.

    Rebuilds never pass through here, so a rebuild can never touch the detail
    pane; ``RowSelected`` stays for Enter.
    """

    board_key: ClassVar[str] = "enter"
    """A click on a row does what Enter does: a heading folds, a task opens in the reader."""

    class RowPicked(Message):
        def __init__(self, row_key: Any) -> None:
            self.row_key = row_key
            super().__init__()

    class Resized(Message):
        """The table's width changed (a terminal, a dragged line): titles refit to it."""

    def on_resize(self, event: events.Resize) -> None:
        self.post_message(self.Resized())

    async def _on_click(self, event: events.Click) -> None:
        meta = event.style.meta
        row = meta.get("row", -1)
        if row < 0:  # header click, or blank space with no row meta
            return
        if self.cursor_type != "row" and meta.get("out_of_bounds", False):
            return  # mirror DataTable's own out-of-bounds guard
        if self.show_cursor and Coordinate(row, meta.get("column", 0)) == self.cursor_coordinate:
            # DataTable selects a click on the cell under the cursor itself (RowSelected): a
            # pick here as well folded a heading twice, and so not at all (review of #573).
            return
        try:
            key = self.coordinate_to_cell_key(Coordinate(row, 0)).row_key
        except Exception:  # click raced a rebuild; the row is gone
            return
        self.post_message(self.RowPicked(key))


class TeamBlock(Static):
    """The team block: as tall as its lines, up to :attr:`AUTO_ROWS` — until a drag says otherwise.

    The cap was the stylesheet's ``max-height: 10``; the column now writes the
    block's ``max-height`` as the room the tasks leave it (:class:`BoardColumn`),
    so a dragged height may go past ten rows. Capping the AUTO height here keeps
    the undragged block what it was — Textual asks for the content height only
    when the height is ``auto`` — and a height the divider set is the user's.
    """

    AUTO_ROWS: ClassVar[int] = 8
    """Content rows an undragged block shows: ten rows less its border."""

    def get_content_height(self, container: Size, viewport: Size, width: int) -> int:
        return min(super().get_content_height(container, viewport, width), self.AUTO_ROWS)


def _number_cell(task: TeamTask) -> Text:
    """The task's number on its board; its id's tail, dimmed, for a task that has none."""
    if task.number is not None:
        return Text(str(task.number))
    return Text(task.id[-8:], style="dim")


class BoardHelp(ModalScreen[None]):
    """``aisquare board -w``'s keys and the task icons; Esc, q or ? closes.

    The fleet UI's own help screen prints the same :func:`board_help` under its
    "Board" section; ``extra`` is the hosting app's keys (a theme picker, quit).
    """

    CSS = """
    BoardHelp { align: center middle; }
    #boardhelpbox { width: 72; height: auto; max-height: 90%; border: heavy $accent;
                    background: $surface; padding: 1 2; }
    #boardhelpbox Static { height: auto; }
    """
    BINDINGS: ClassVar = [
        ("escape", "close_help", "close"),
        ("q", "close_help", "close"),
        ("question_mark", "close_help", "close"),
    ]

    def __init__(
        self, extra: tuple[tuple[str, str], ...] = (), *, standalone: bool = False
    ) -> None:
        super().__init__()
        self._extra = extra
        self._standalone = standalone

    def compose(self) -> ComposeResult:
        text = Text("Board — keys\n\n", style="bold")  # no command to type (R13)
        text.append_text(board_help(standalone=self._standalone))
        if self._extra:
            text.append("\n")
            for key, what in self._extra:
                text.append(f"  {key:<11}", style="bold cyan")
                text.append(f" {what}\n")
        text.append("\nEsc closes this", style="dim")
        with VerticalScroll(id="boardhelpbox"):
            yield Static(text)

    def action_close_help(self) -> None:
        self.dismiss(None)


class BoardColumn(Vertical):
    """The board column: the team block over the tasks, and the line between them.

    The column keeps the team block's ceiling — its height less the line and
    the rows the tasks must keep — as the block's ``max-height`` whenever its
    own height changes: the panel resizing, or the detail pane's line moving.
    Textual then clamps the block on every layout pass, as ``app.Panes`` does
    for the navigator.
    """

    def on_resize(self, event: events.Resize) -> None:
        if event.size.height <= 0:
            return  # behind a hidden tab: nothing to fit, and the next show resizes
        for panel in self.ancestors:
            if isinstance(panel, BoardPanel):
                panel._keep_alone()  # a width its line set: noted, or cleared while alone
                break
        sessions = self.query_children("#sessions").first()
        tasks = self.query_children("#tasks").first()
        floor = floor_of(sessions, "height")
        sessions.styles.max_height = max(floor, event.size.height - 1 - floor_of(tasks, "height"))


class BoardPanel(Vertical):
    """Sessions, tasks, the live feed and a detail pane for one project's board.

    ``project`` may be ``None``: the board then resolves from the process cwd
    on every read, which is what ``board -w`` does when its own resolution
    failed at start. ``interval`` is the refresh period in seconds.

    Three lines resize it, each a :class:`Divider` that finds its target among
    its own siblings — the fleet UI mounts a board per project at once, and the
    ids repeat: the column beside the feed, the detail pane below everything,
    and the team block over the tasks (:class:`BoardColumn`). The panel keeps
    the first two ceilings current from its own size, so the feed and the
    board above the detail pane keep their stylesheet minimums whatever the
    drag or the terminal does.
    """

    DEFAULT_CSS = """
    BoardPanel { height: 1fr; }
    BoardPanel #board-main { height: 1fr; min-height: 8; }
    BoardPanel #board-side { width: 58; min-width: 32; }
    BoardPanel #board-side.alone { width: 1fr; }
    BoardPanel #board-side.collapsed { display: none; }
    BoardPanel #side-divider.collapsed { display: none; }
    BoardPanel #sessions { height: auto; min-height: 3; border: round $primary;
                           padding: 0 1; text-wrap: nowrap; text-overflow: ellipsis; }
    BoardPanel #tasks { height: 1fr; min-height: 4; border: round $primary; }
    BoardPanel #feedpane { width: 1fr; min-width: 32; }
    BoardPanel #feedpane.collapsed { display: none; }
    BoardPanel #feed { height: 1fr; border: round $secondary;
                       text-wrap: nowrap; text-overflow: ellipsis; }
    BoardPanel #feed.hidden { display: none; }
    BoardPanel #stream-search { display: none; height: 1; border: none; padding: 0 1; }
    BoardPanel #feedpane.searching #stream-search { display: block; }
    BoardPanel #feedtext { height: 1fr; border: round $success; display: none;
                           padding: 0 1; }
    BoardPanel #feedtext.active { display: block; }
    BoardPanel #detailwrap { height: 8; min-height: 3; border: heavy $warning;
                             padding: 0 1; }
    BoardPanel #reader { display: none; height: 1fr; border: heavy $accent; padding: 0 1; }
    BoardPanel.reading #reader { display: block; }
    BoardPanel.reading #board-main, BoardPanel.reading #detail-divider,
    BoardPanel.reading #detailwrap { display: none; }
    """
    # The footer shows five (b d a o /), in words short enough that asq's footer and
    # ``board -w``'s fit 90 columns (B1 #9); ``f``, ``v``, ``r``, ``c`` and Esc are on the ?
    # screen (``BOARD_KEYS``), and ``c`` is named by the feed's title in select mode, the one
    # time it copies anything.
    BINDINGS: ClassVar = [
        Binding("b", "toggle_board", "tasks"),
        Binding("d", "toggle_done", "done"),
        Binding("f", "fold_group", "fold", show=False),
        Binding("a", "toggle_autoscroll", "autoscroll"),
        Binding("v", "toggle_select", "select", show=False),
        Binding("o", "open_transcript", "transcript"),
        Binding("slash", "search", "search", key_display="/"),
        *(
            Binding(str(n), f"lane_item({n - 1})", f"needs you {n}", show=False)
            for n in range(1, 10)
        ),
        Binding("escape", "back", "back", show=False),
        *(
            Binding(key, f"resize('{line}', {sign})", "resize", show=False)
            for line, keys in DIVIDER_KEYS.items()
            for key, sign in zip(keys, (-1, 1), strict=True)
        ),
        Binding("equals_sign", "reset_sizes", "sizes back", show=False),
        Binding("r", "refresh_now", "refresh", show=False),
        Binding("c", "copy_selection", "copy", show=False),
    ]

    FEED_LIMIT: ClassVar[int] = 2000
    """Feed lines kept before the oldest are dropped.

    The panel outlives every tab switch, so the feed is the one structure here
    that only grows: ``_events_by_id``, ``_feed_order`` and the OptionList all
    gain a row per event for the life of the app. 2000 lines is ~5x the first
    fetch (``board_read(events=400)``) — deep enough that scrolling back has
    somewhere to go, bounded enough that a week-long session cannot swell.
    """

    class Refreshed(Message):
        """A refresh landed; ``project`` is the board it read."""

        def __init__(self, project: ProjectInfo) -> None:
            self.project = project
            super().__init__()

    class AgentWanted(Message):
        """A 🔔 or ❓ count was pressed: the agent it names, by its label on ``project_id``'s
        board. The fleet UI selects that agent's row, where its pane is answered."""

        def __init__(self, project_id: str, label: str) -> None:
            self.project_id = project_id
            self.label = label
            super().__init__()

    def __init__(
        self,
        project: ProjectInfo | None,
        *,
        interval: float = 2.0,
        id: str | None = None,
        standalone: bool = False,
    ) -> None:
        super().__init__(id=id)
        self.project = project
        self.interval = interval
        self.standalone = standalone
        """Hosted by ``board -w``, not asq: no Manager tab to point at and no left pane to select
        an agent in, so a 🔔 or ❓ count shows what it counts here (B1 #8, #10)."""
        self._events_by_id: dict[str, TeamEvent] = {}
        self._feed_order: list[str] = []
        self._tasks_by_id: dict[str, TeamTask] = {}
        self._roles: dict[str, str] = {}
        self._names: dict[str, str] = {}
        """Each session's name as asq's left pane gives its agent (``cli.board_names``, #140)."""
        self._fleet_labels: dict[str, str] = {}
        """Each session's fleet row's label, live or ended (``team.fleet_labels``), the last
        refresh's."""
        self._statuses: dict[str, str] = {}
        self._last_seq = 0
        self._terminal_by_task: dict[str, TeamEvent] = {}
        self._terminal_fetched = False
        self._prev_states: dict[str, str] = {}
        self._select_mode = False
        self.pane = WORK
        """Which half shows while the panel is narrower than :data:`WIDE_FROM`."""
        self.folds: set[str] = set(DEFAULT_FOLDS)
        """Work's folded groups, by key (:data:`WORK_GROUPS`)."""
        self._work_hidden = False
        """``b`` hid the Work column while the panel is wide (B1's ``b``)."""
        self._side_width: Any = None
        """Work's width beside the Stream as its line set it (a drag's, a key's, or the one the
        line remembered and restored), ``None`` for the stylesheet's: kept while Work has the
        column to itself, and put back when the Stream returns (review of #573)."""
        self._sessions: dict[str, TeamSession] = {}
        self._switches: dict[str, str] = {}
        """The ``fleet switch`` of each limited session the last refresh read, by its id."""
        self._labels: Mapping[int, str] = {}
        self._labels_asked_at: float | None = None
        self._all_tasks: list[TeamTask] = []
        self._read_project: ProjectInfo | None = project
        """The board the last refresh read: ``project``, or the one it resolved to."""
        self.needs = NeedsYou()
        """What the strip shows, from the last refresh (:func:`needs_you`)."""
        # Display ▸ *Follow the board's live feed* (``[ui] board_autoscroll``, R17) says how it
        # opens, in asq and in ``board -w``; ``a`` toggles it from there.
        self.autoscroll = settings_service.ui_settings().board_autoscroll
        self._following = True
        """The feed shows its last line: scrolled up, new lines wait below it (B1 #11)."""
        self.unseen = 0
        """Feed lines that came in below the view while it was not following: the ``↓ N new``
        marker under the feed."""
        self._catch_up = False
        """The Stream followed its last line while out of sight: it scrolls there on showing."""
        self._unseen_since: datetime | None = None
        """When the first of :attr:`unseen` was written: the marker's "since"."""
        self.stream_filter = ""
        """What ``/`` filters the Stream by (case-insensitive, over the line as shown)."""
        self._view_saver: Autosave | None = None
        """The debounced save of this project's view (:data:`VIEW_KEY`); ``None`` with no
        project to keep it for."""
        self.lane: list[tuple[str, str]] = []
        """The needs-you lane's items in order, each (kind, task id or agent label)."""
        self.detail_text = ""
        """The plain text of the detail pane (what a test reads)."""
        self.reader_text = ""
        """The plain text of the reader, while a task is open in it (what a test reads)."""
        self._reading: str | None = None
        """The task the reader shows, by id; ``None`` while it is closed."""
        self.detail_moment: tuple[str | None, datetime] | None = None
        """(author session id, timestamp) of the selected item, for the transcript jump."""

    def compose(self) -> ComposeResult:
        yield NeedsYouLane(id="needs-you-lane")
        yield BoardSwitch(id="board-switch")
        with Horizontal(id="board-main"):
            with BoardColumn(id="board-side"):
                yield TeamBlock(id="sessions")
                yield Divider(
                    "#sessions",
                    orientation="horizontal",
                    state_key=TEAM_HEIGHT_KEY,
                    what="the board's team height",
                    id="team-divider",
                )
                yield PickableTable(id="tasks", cursor_type="row")
            yield Divider(
                "#board-side",
                state_key=COLUMN_WIDTH_KEY,
                what="the board column's width",
                id="side-divider",
            )
            with Vertical(id="feedpane"):
                yield SearchField(
                    placeholder="filter the stream — Enter keeps it, Esc clears it",
                    id="stream-search",
                )
                yield StreamList(id="feed")
                with VerticalScroll(id="feedtext"):
                    yield Static(id="feedstatic")
        yield Divider(
            "#detailwrap",
            orientation="horizontal",
            side="after",
            state_key=DETAIL_HEIGHT_KEY,
            what="the board's detail height",
            id="detail-divider",
        )
        with VerticalScroll(id="detailwrap"):
            yield Static(id="detail")
        with VerticalScroll(id="reader"):
            yield Static(id="reader-text")

    def on_mount(self) -> None:
        table = self.query_one("#tasks", DataTable)
        table.add_columns(*TASK_COLUMNS)
        table.border_title = "tasks"
        self.query_one("#sessions", Static).border_title = "team"
        feed = self.query_one("#feed", OptionList)
        feed.border_title = self._feed_title()
        self.watch(feed, "scroll_y", self._feed_scrolled, init=False)
        self.query_one(
            "#detailwrap", VerticalScroll
        ).border_title = "detail — click a task or feed line"
        self._restore_view()
        self.refresh_data()
        self.set_interval(self.interval, self._tick)

    def _view(self) -> dict[str, object]:
        return {"folds": sorted(self.folds), "filter": self.stream_filter, "pane": self.pane}

    def _restore_view(self) -> None:
        """This project's folds, filter and narrow half, as it last left them (B2). A value
        that is not one of them is ignored, never an error: the file is the user's too."""
        if self.project is None:
            return
        key = VIEW_KEY + self.project.id
        saved = read_state().get(key)
        if isinstance(saved, dict):
            folds = saved.get("folds")
            if isinstance(folds, list):
                known = {group for group, _heading, _statuses in WORK_GROUPS}
                self.folds = {str(group) for group in folds if group in known}
            if isinstance(saved.get("filter"), str):
                self.stream_filter = saved["filter"]
            if saved.get("pane") in (WORK, STREAM):
                self.pane = saved["pane"]
        self._view_saver = Autosave(
            self, key, what="the board's folds and filter", initial=self._view()
        )

    def _remember_view(self) -> None:
        """Save the view when it changed: a resize lays the board out again and writes nothing."""
        view = self._view()
        if self._view_saver is not None and view != self._view_saver.latest:
            self._view_saver.remember(view)

    @while_running
    def _tick(self) -> None:
        """The polled read — skipped while the panel is off screen.

        The fleet UI keeps a ``ProjectView`` per project ever selected, so an
        ungated timer means N store reads every ``interval`` seconds for the
        life of the app, N-1 of them behind ``display: none``. What skipping
        costs: a hidden panel's data is stale, so :meth:`on_show` re-reads the
        moment the tab comes back (as ``TerminalPane`` does for its frames).
        """
        if self.is_on_screen:
            self.refresh_data()

    def on_show(self) -> None:
        """A hidden tab came back: catch up on what the skipped ticks missed, and take the keys
        when the person came here by its tab."""
        self.refresh_data()
        self._take_the_keys()

    def _take_the_keys(self) -> None:
        """Focus the board, so its keys answer at once (B1 #1).

        Opened from its tab, the focus stayed on the tab row, outside the panel, and ``d``,
        ``b`` or ``a`` did nothing until a click inside it. Taken only from the tabs this panel
        sits in (their row, or the pane just hidden) or from nothing: a board shown because
        the sidebar moved to its project leaves the focus in the sidebar.
        """
        focused = self.app.focused
        if focused is not None:
            if self in focused.ancestors_with_self and focused in self.screen.focus_chain:
                return  # on something of the board's that shows: the keys are here
            tabs = next((node for node in self.ancestors if isinstance(node, TabbedContent)), None)
            if tabs is None or tabs not in focused.ancestors:
                return
        self._keys_target().focus()

    def _keys_target(self) -> Widget:
        """What takes the focus in the panel: the tasks, else the feed as it shows."""
        if not self.query_one("#board-side").has_class("collapsed"):
            return self.query_one("#tasks")
        return self._feed_shown()

    def _feed_shown(self) -> Widget:
        """The feed as it shows: the live list, or its frozen text in select mode."""
        feed = self.query_one("#feed")
        return self.query_one("#feedtext") if feed.has_class("hidden") else feed

    # --- data ---------------------------------------------------------------------

    def refresh_data(self) -> None:
        """Re-read the board; keep the last frame when the store is briefly busy.

        The captain's audit is left out of the feed, first read and every tick, as
        ``aisquare board`` leaves it out (``team.CAPTAIN_AUDIT_KINDS``; review of #240,
        finding 11): ``aisquare captain log`` reads it.
        """
        try:
            read = team_service.board_read(
                events=400 if self._last_seq == 0 else 200,
                since_seq=self._last_seq or None,
                project=self.project,
                exclude_kinds=team_service.CAPTAIN_AUDIT_KINDS,
            )
            project, sessions, tasks, events, switches = read[:5]
        except team_service.TeamDisabledError:
            self.query_one("#sessions", Static).update(
                Text("orchestrator disabled (AISQUARE_TEAM=0)", style="bold red")
            )
            return
        except Exception:  # store briefly unavailable — keep the last frame
            return
        self._roles = {s.id: s.role for s in sessions}
        self._fleet_labels = dict(read.fleet_labels)
        self._rename(
            board_names.board_names(
                sessions, self._fleet_labels, cells=board_names.BOARD_NAME_CELLS
            )
        )
        self._sessions = {s.id: s for s in sessions}
        moved = {t.id for t in tasks if self._statuses.get(t.id, t.status) != t.status}
        self._statuses = {t.id: t.status for t in tasks}
        self._restatus(moved)
        self._all_tasks = tasks
        self._refresh_labels(sessions)
        self._switches = switches
        self.query_one("#sessions", Static).update(
            watch._session_lines(sessions, self._labels, switches=self._switches, names=self._names)
        )
        self._ring_on_attention(sessions)
        # Events first: a task_done/dropped this tick flushes the attribution
        # cache BEFORE the archive rebuilds, so the closer is never one frame stale.
        self._append_events(events)
        self._refresh_tasks(tasks)
        self._read_project = project
        shown = [self._events_by_id[event_id] for event_id in self._feed_order]
        self.needs = needs_you(
            project, sessions, tasks, shown, seq=self._last_seq, fleet_labels=self._fleet_labels
        )
        self._show_lane()
        self.post_message(self.Refreshed(project))

    def _refresh_labels(self, sessions: list[TeamSession]) -> None:
        """Ask the registry for the slot labels OFF the UI thread, at most every LABELS_TTL s.

        ``cli.watch._session_lines`` used to read them itself — a ``context.db``
        open with a busy timeout of seconds, a scan of the account directories
        and, on a registry not yet reconciled, its writes — on the event loop,
        at every tick of every open board (review of #205, fourth round). The
        block paints the last good map (the built-in names before the first
        answer) and repaints when the worker answers, as the agent header does
        (``views.agent``). Asked only once several accounts are in play, the
        only time the block shows one.
        """
        if len({s.account for s in sessions if s.ended_at is None and s.account}) < 2:
            return
        now = time.monotonic()
        if self._labels_asked_at is not None and now - self._labels_asked_at < LABELS_TTL:
            return
        self._labels_asked_at = now
        self.run_worker(
            accounts_service.slot_labels,
            name=LABELS_WORKER,
            group=LABELS_WORKER,
            exclusive=True,
            thread=True,
            exit_on_error=False,
        )

    @on(Worker.StateChanged)
    def _labels_answered(self, event: Worker.StateChanged) -> None:
        if event.worker.group != LABELS_WORKER:
            return
        if event.state is WorkerState.SUCCESS and isinstance(event.worker.result, dict):
            self._labels = event.worker.result
            if self.is_mounted:
                self.query_one("#sessions", Static).update(
                    watch._session_lines(
                        list(self._sessions.values()),
                        self._labels,
                        switches=self._switches,
                        names=self._names,
                    )
                )

    def _ring_on_attention(self, sessions: list[TeamSession]) -> None:
        """Terminal bell when a session newly flips to needing the user."""
        states = {s.id: s.state for s in sessions if s.ended_at is None}
        for session_id, state in states.items():
            previous = self._prev_states.get(session_id)
            if state == "attention" and previous not in (None, "attention"):
                self.app.bell()
                break
        self._prev_states = states

    def _done_event_for(self, task_id: str) -> TeamEvent | None:
        """The latest done/dropped event for a task (who closed it, when).

        Resolved from the store, not the bounded feed cache — a task closed
        thousands of events ago still knows who closed it. The cache is
        flushed in ``_append_events`` whenever a new terminal event arrives, so
        a reopen-and-reclose re-attributes correctly. Fetched once per flush,
        not per row: the populated dict is the cache, so a task with NO
        terminal event returns ``None`` from ``.get()`` without re-running the
        store query every tick (negative caching).
        """
        if not self._terminal_fetched:
            try:
                self._terminal_by_task = team_service.terminal_attribution(project=self.project)
            except Exception:
                return None
            self._terminal_fetched = True
        return self._terminal_by_task.get(task_id)

    def _refresh_tasks(self, tasks: list[TeamTask]) -> None:
        """Rebuild Work (B2): Doing, Blocked, Review, To do, then Done, each under a heading row
        that folds it (Done folded at first); a task's row is its number, its status as an icon,
        its title, who has it and how long it has been so.

        The title is cut, with an ellipsis, to the room the table has, so it takes the rest of
        the width and the table never scrolls sideways; a resize rebuilds
        (``PickableTable.Resized``). The scroll and the cursor stay where they were (B1 #7).
        """
        table = self.query_one("#tasks", DataTable)
        selected = self._cursor_task_id(table)
        scrolled = table.scroll_y
        # The columns too: a column keeps the widest cell it ever held, so a title cut
        # to a narrower table still scrolled sideways to the width of the uncut one.
        table.clear(columns=True)
        table.add_columns(*TASK_COLUMNS)
        self._tasks_by_id = {}
        now = _now()
        open_count = sum(1 for t in tasks if t.status in OPEN_STATUSES)
        table.border_title = f"work — {open_count} open · f folds, d done"
        rows: list[tuple[str, list[Text]]] = []
        for key, heading, statuses in WORK_GROUPS:
            members = [t for t in tasks if t.status in statuses]
            if not members:
                continue
            if key == "done":
                members.sort(key=lambda t: t.updated_at, reverse=True)
            folded = key in self.folds
            mark = Text("▸" if folded else "▾", style="bold")
            label = Text(f"{heading} ({len(members)})", style="bold")
            rows.append((GROUP_ROW + key, [Text(""), mark, label, Text(""), Text("")]))
            if folded:
                continue
            for task in members:
                self._tasks_by_id[task.id] = task
                waiting = task.status == "todo" and bool(unmet_needs(task, self._statuses))
                holder = task.claimed_by
                owner = (self._names.get(holder) or team_service.short_id(holder)) if holder else ""
                cells = [
                    _number_cell(task),
                    status_icon(task.status, waiting=waiting),
                    Text(task.title, no_wrap=True, overflow="ellipsis"),
                    Text(owner, style="dim", no_wrap=True, overflow="ellipsis"),
                    Text(team_service.age_text(task.updated_at, now), style="dim"),
                ]
                cells[3].truncate(OWNER_CELLS, overflow="ellipsis")
                rows.append((task.id, cells))
        if not open_count:
            # The empty state: where tasks come from, wrapped to the room the table has.
            empty = EMPTY_TASKS_ALONE if self.standalone else EMPTY_TASKS
            width = self._title_room(table, rows) or len(empty)
            lines = textwrap.wrap(empty, max(width, 16))
            body = Text("\n".join(lines), style="dim")
            blank = [Text(""), Text(""), body, Text(""), Text("")]
            rows.insert(0, (EMPTY_ROW, blank))
        room = self._title_room(table, rows)
        for key, cells in rows:
            if room is not None and key != EMPTY_ROW:
                cells[2].truncate(room, overflow="ellipsis")
            height = len(cells[2].plain.splitlines()) if key == EMPTY_ROW else 1
            table.add_row(*cells, key=key, height=height)
        if selected is not None and selected in {key for key, _ in rows}:
            table.move_cursor(row=table.get_row_index(selected), scroll=False)
        # Where the person had scrolled to, once the rows are measured: a rebuild every tick
        # put the table back at its top (B1 #7).
        if scrolled:
            self.call_after_refresh(table.scroll_to, y=scrolled, animate=False)

    @staticmethod
    def _title_room(table: DataTable[Any], rows: list[tuple[str, list[Text]]]) -> int | None:
        """The cells the title column may take: what the table has less the other columns and
        every column's padding — ``None`` before the table has a size."""
        width = table.scrollable_content_region.width
        if width <= 0:
            return None
        pad = 2 * table.cell_padding
        others = 0
        for column in (0, 1, 3, 4):  # every column but the title's, its heading included
            cells = [cells[column].cell_len for key, cells in rows if key != EMPTY_ROW]
            floor = ICON_CELLS if column == 1 else 1
            others += max([floor, cell_len(TASK_COLUMNS[column]), *cells]) + pad
        return max(8, width - others - pad)

    def _fold(self, group: str) -> None:
        """Fold ``group`` (a :data:`WORK_GROUPS` key), or unfold it."""
        self.folds.symmetric_difference_update({group})
        self._refresh_tasks(self._all_tasks)
        self._remember_view()

    def action_fold_group(self) -> None:
        """``f``: fold the group at the cursor — its heading's, or the task's — or unfold it."""
        table = self.query_one("#tasks", DataTable)
        key = self._cursor_task_id(table)
        if key is None or key == EMPTY_ROW:
            return
        if key.startswith(GROUP_ROW):
            self._fold(key.removeprefix(GROUP_ROW))
            return
        task = self._tasks_by_id.get(key)
        group = next(
            (g for g, _h, statuses in WORK_GROUPS if task and task.status in statuses), None
        )
        if group is not None:
            self._fold(group)
            with contextlib.suppress(Exception):  # the heading is where the cursor goes
                table.move_cursor(row=table.get_row_index(GROUP_ROW + group))

    def on_pickable_table_resized(self, event: PickableTable.Resized) -> None:
        self._refresh_tasks(self._all_tasks)

    def _show_lane(self) -> None:
        """The needs-you lane (B2): one row per thing that needs the person, from what this
        tick read (:func:`needs_you` and the panel's caches; no read of its own), each with its
        text and what its key does."""
        tasks = {t.id: t for t in self._all_tasks}
        self.lane = []
        rows: list[Text] = []
        for kind, glyph, _one, _many in NEEDS_YOU:
            targets = self.needs.agents.get(kind) or self.needs.tasks.get(kind) or []
            for target in targets:
                self.lane.append((kind, target))
                rows.append(self._lane_text(kind, glyph, target, tasks.get(target)))
        if rows:
            lead = Text("needs you", style="bold")
            lead.append(" — a number or a click acts on its row", style="dim")
        else:
            nothing = NOTHING_NEEDS_YOU_ALONE if self.standalone else NOTHING_NEEDS_YOU
            lead = Text(nothing, style="dim")
        self.query_one(NeedsYouLane).show(lead, rows)

    def _lane_text(self, kind: str, glyph: str, target: str, task: TeamTask | None) -> Text:
        text = Text(f"{glyph} ")
        if kind in ("review", "blocked"):
            text.append(f"#{task.number} " if task and task.number else "", style="bold")
            text.append(task.title if task else target)
            if kind == "blocked":
                why = self._blocked_said(target)
                text.append(f" — {why}" if why else " — blocked", style="red")
            else:
                text.append(" — in review", style="cyan")
            text.append("   opens it", style="dim")
            return text
        if kind == "question":
            asked = self._question_of(target)
            text.append(f"{target} asks: ", style="bold")
            text.append(watch.one_row(asked.text) if asked else "a question")
        else:
            text.append(f"{target} waits for you in its pane", style="bold")
        text.append("   shows it" if self.standalone else "   selects it", style="dim")
        return text

    def _blocked_said(self, task_id: str) -> str | None:
        """Why the card is blocked, from the newest block line the feed holds: its text past
        the title (``team.stopped_because``'s shape), with no store read on a tick."""
        for event_id in reversed(self._feed_order):
            event = self._events_by_id[event_id]
            if event.kind == "task_blocked" and event.task_id == task_id:
                _title, said, why = event.text.partition(" — ")
                return why if said else None
        return None

    def _question_of(self, label: str) -> TeamEvent | None:
        """The newest question the agent named ``label`` asked, from the feed."""
        asking = {
            sid
            for sid, session in self._sessions.items()
            if team_service.board_name(session, self._fleet_labels) == label
        }
        for event_id in reversed(self._feed_order):
            event = self._events_by_id[event_id]
            if event.kind == "question" and event.session_id in asking:
                return event
        return None

    def on_needs_you_lane_picked(self, event: NeedsYouLane.Picked) -> None:
        self.action_lane_item(event.index)

    def action_lane_item(self, index: int) -> None:
        """``1`` to ``9``, or a click on the lane's row: act on that item. A card opens in the
        reader; an agent is selected in asq's left pane (:class:`AgentWanted`), or in
        ``board -w`` what it asks is shown."""
        lane = self.query_one(NeedsYouLane)
        if index == lane.more_row and not lane.expanded:
            lane.expanded = True  # "+ N more": the lane opens to its nine rows
            self._show_lane()
            return
        if not 0 <= index < min(lane.drawn, len(self.lane)):
            return  # a row not drawn acts on nothing (review of #573)
        kind, target = self.lane[index]
        if kind in ("review", "blocked"):
            self._show_work()
            group = next(g for g, _h, statuses in WORK_GROUPS if kind in statuses)
            if group in self.folds:
                self._fold(group)
            table = self.query_one("#tasks", DataTable)
            with contextlib.suppress(Exception):  # the row went in a rebuild since
                table.move_cursor(row=table.get_row_index(target))
            self._show_task_detail(target)
            self.open_reader(target)
            return
        if self.standalone:
            self._show_agent_need(kind, target)
            return
        project = self._read_project
        if project is not None:
            self.post_message(self.AgentWanted(project.id, target))

    def _show_agent_need(self, kind: str, label: str) -> None:
        """In ``board -w``, a 🔔 or ❓ count shows what it counts: the question itself, picked in
        the feed, or the agent waiting in its pane (B1 #10). asq selects the agent instead
        (:class:`AgentWanted`); here there is no left pane to select it in."""
        asking = [
            sid
            for sid, session in self._sessions.items()
            if team_service.board_name(session, self._fleet_labels) == label
        ]
        if kind == "question":
            for index in range(len(self._feed_order) - 1, -1, -1):
                event = self._events_by_id[self._feed_order[index]]
                if event.kind == "question" and event.session_id in asking:
                    feed = self.query_one("#feed", OptionList)
                    with contextlib.suppress(Exception):  # the line went since
                        feed.highlighted = feed.get_option_index(event.id)
                    self._show_event_detail(event)
                    return
        detail = Text()
        if kind == "question":  # asked, but its line has left the feed this board keeps
            detail.append(f"❓ {label} asked you a question", style="bold")
            detail.append(" — it is no longer on this feed.", style="dim")
        else:
            detail.append(f"🔔 {label} waits for you in its pane", style="bold")
        detail.append("\nAnswer it in rlx: its row in the left pane opens the pane.", style="dim")
        self.detail_moment = None
        self._show_detail(detail)

    def _cursor_task_id(self, table: DataTable[Any]) -> str | None:
        try:
            key = table.coordinate_to_cell_key(table.cursor_coordinate).row_key.value
            return str(key) if key is not None else None
        except Exception:
            return None

    def _rename(self, names: dict[str, str]) -> None:
        """Take this refresh's names (#140), and redraw each feed line whose author's name moved:
        a restart that gave an earlier session's name to a new one tells the earlier's lines
        apart too, which were drawn when its name was its own."""
        moved = {sid for sid, name in names.items() if self._names.get(sid, name) != name}
        self._names = names
        if not moved or self._select_mode:
            return
        feed = self.query_one("#feed", OptionList)
        for event_id in self._feed_order:
            event = self._events_by_id[event_id]
            if event.session_id in moved:
                with contextlib.suppress(OptionDoesNotExist):  # filtered out of the Stream
                    feed.replace_option_prompt(event_id, self._stream_prompt(event))

    def _stream_prompt(self, event: TeamEvent) -> Text:
        return stream_line(event, self._roles, self._names, self._statuses)

    def _restatus(self, tasks: set[str]) -> None:
        """Redraw the Stream lines about ``tasks``, whose status moved: a status chip says the
        task's status now, not when the line was written."""
        if not tasks or self._select_mode or not self.is_mounted:
            return
        feed = self.query_one("#feed", OptionList)
        for event_id in self._feed_order:
            event = self._events_by_id[event_id]
            if event.task_id in tasks:
                with contextlib.suppress(OptionDoesNotExist):
                    feed.replace_option_prompt(event_id, self._stream_prompt(event))

    def _shows(self, event: TeamEvent) -> bool:
        """Whether the Stream's filter lets ``event`` through."""
        wanted = self.stream_filter.casefold()
        return not wanted or wanted in self._stream_prompt(event).plain.casefold()

    def _refilter(self) -> None:
        """Rebuild the Stream for the filter: the lines it lets through, in order."""
        feed = self.query_one("#feed", OptionList)
        feed.clear_options()
        shown = [
            Option(self._stream_prompt(event), id=event.id)
            for event in (self._events_by_id[event_id] for event_id in self._feed_order)
            if self._shows(event)
        ]
        feed.add_options(shown)
        self._retitle_feed()
        feed.scroll_end(animate=False)

    def _append_events(self, events: list[TeamEvent]) -> None:
        if self._select_mode:
            return  # frozen while selecting; the backlog lands on exit
        feed = self.query_one("#feed", OptionList)
        fresh = [e for e in events if e.seq > self._last_seq]
        if not fresh:
            return
        following = self.autoscroll and self._following
        hidden = self.query_one("#feedpane").has_class("collapsed") or self._reading is not None
        for event in fresh:
            self._events_by_id[event.id] = event
            self._feed_order.append(event.id)
            if self._shows(event):
                feed.add_option(Option(self._stream_prompt(event), id=event.id))
            self._last_seq = max(self._last_seq, event.seq)
            if event.kind in ("task_done", "task_dropped"):
                # A close happened — attribution (positive AND negative entries)
                # is stale; a reopen-and-reclose must re-attribute.
                self._terminal_by_task = {}
                self._terminal_fetched = False
        self._evict_oldest(feed, len(self._feed_order) - self.FEED_LIMIT)
        if following and hidden:
            # Out of sight (Work has the column, or the reader is open), the Stream does not
            # scroll: it catches up when it shows again (B2's tester, #695).
            self._catch_up = True
        elif following:
            feed.scroll_end(animate=False)
        else:
            # Scrolled up (or autoscroll off): the view stays where the person put it, and the
            # marker under the feed counts what came in below it (B1 #11) — while it is below
            # it: once laid out, lines a feed that does not overflow shows are not unseen.
            shown = [event for event in fresh if self._shows(event)]
            if shown and not self.unseen:
                self._unseen_since = shown[0].created_at
            self.unseen += len(shown)  # a line the filter hides is not unseen (review of #573)
            self._show_unseen()
            self.call_after_refresh(self._recount_unseen)

    def _feed_scrolled(self, _old: float, new: float) -> None:
        """The feed moved: following again once it is back at its last line, which clears the
        ``↓ N new`` marker."""
        feed = self.query_one("#feed", OptionList)
        self._following = new >= feed.max_scroll_y
        if self._following and self.unseen:
            self.unseen = 0
            self._show_unseen()

    def _recount_unseen(self) -> None:
        """No ``↓ N new`` while the feed's last line is in view: a feed that does not overflow,
        or one the view still ends at. Only a scroll used to clear it, which a short feed with
        autoscroll off never has (review of #554)."""
        feed = self.query_one("#feed", OptionList)
        if self.unseen and feed.scroll_y >= feed.max_scroll_y:
            self.unseen = 0
            self._show_unseen()

    def _show_unseen(self) -> None:
        feed = self.query_one("#feed", OptionList)
        since = (
            f" since {watch.clock(self._unseen_since)}" if self._unseen_since is not None else ""
        )
        feed.border_subtitle = f"↓ {self.unseen} new{since} — End shows them" if self.unseen else ""

    def start_over(self) -> None:
        """The board was cleared under the panel (Start fresh): forget what the panel read
        of it — the feed's lines and the events behind them, the cursor, the close
        attributions, the detail shown — and read it again from its start.

        :meth:`refresh_data` reads only past the cursor and the feed only grows, so after a
        reset the cleared notes, signals and the stop's own ``agent_exited`` stayed listed,
        and selectable through ``_events_by_id``, until asq restarted: on a panel open
        through the reset, on a hidden one shown later and after ``r`` alike
        (tester-fresh-ui at 8b66e535). A frozen select-mode copy is left first, so it shows
        nothing that went."""
        if self._select_mode:
            self._select_mode = False
            self.query_one("#feed", OptionList).remove_class("hidden")
            self.query_one("#feedtext", VerticalScroll).remove_class("active")
        self.query_one("#feed", OptionList).clear_options()
        self._events_by_id.clear()
        self._feed_order.clear()
        self._last_seq = 0
        self._terminal_by_task = {}
        self._terminal_fetched = False
        self._following = True
        self.unseen = 0
        self._show_unseen()
        self.detail_moment = None
        self._show_detail(Text(""))
        self.refresh_data()

    def _evict_oldest(self, feed: OptionList, count: int) -> None:
        """Drop the oldest ``count`` feed lines from the list AND the caches.

        All three together: ``_feed_order`` is what select mode replays through
        ``_events_by_id``, so an id may never outlive its event, and the option
        holds the row on screen. A close attribution does NOT come from here
        (``_done_event_for`` asks the store), so an evicted line costs only
        history the user can no longer scroll to.
        """
        if count <= 0:
            return
        for event_id in self._feed_order[:count]:
            self._events_by_id.pop(event_id, None)
            with contextlib.suppress(OptionDoesNotExist):  # already gone
                feed.remove_option(event_id)
        del self._feed_order[:count]

    # --- the detail pane -----------------------------------------------------------

    def _show_detail(self, content: Text) -> None:
        self.detail_text = content.plain
        self.query_one("#detail", Static).update(content)
        self.query_one("#detailwrap", VerticalScroll).scroll_home(animate=False)

    def _moment_transcript(self) -> tuple[Path, datetime] | None:
        """(transcript path, timestamp) for the selected item, if known."""
        if self.detail_moment is None:
            return None
        session_id, when = self.detail_moment
        session = self._sessions.get(session_id or "")
        if session is None or not session.transcript_path:
            return None
        transcript = Path(session.transcript_path)
        if not transcript.exists():
            return None
        return transcript, when

    def _show_task_detail(self, key: str | None) -> None:
        if key is None:
            return
        task = self._tasks_by_id.get(key)
        if task is None:
            return
        # Read on the pick, never on a tick: one indexed query, and it fails open (B1 #3).
        why = team_service.stopped_because(task) if task.status in watch.STOPPED_BECAUSE else None
        detail = watch._task_detail(task, self._statuses, self._names, why=why)
        done_event = self._done_event_for(task.id) if task.status in CLOSED_STATUSES else None
        if done_event is not None:
            who = (
                watch._named(done_event.session_id, self._names)
                if done_event.session_id
                else team_service.PERSON
            )
            verb = "done" if done_event.kind == "task_done" else "dropped"
            detail.append(
                f"\n{verb} by {who} at {watch.clock(done_event.created_at, seconds=True)}",
                style="green" if verb == "done" else "yellow",
            )
            self.detail_moment = (done_event.session_id, done_event.created_at)
        else:
            self.detail_moment = (task.claimed_by, task.updated_at)
        if self._moment_transcript() is not None:
            detail.append(TRANSCRIPT_HINT, style="dim")
        self._show_detail(detail)

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        # Enter (and click-when-cursor-already-there) post RowSelected. A
        # rebuild's clear/add_row/move_cursor posts RowHighlighted but never
        # RowSelected, so no artifact can touch the detail pane. Enter on a task opens
        # the reader (B2); a heading folds, as a click on it does.
        key = event.row_key.value if event.row_key else None
        self._picked_row(str(key) if key is not None else None)

    def on_pickable_table_row_picked(self, event: PickableTable.RowPicked) -> None:
        # A real mouse click on a data row (posted from inside the widget's click
        # path, where DataTable stops the event before it can bubble).
        # Header/blank clicks never post this, so they can't clobber a selected
        # feed event — and rebuilds can't reach it at all.
        key = event.row_key.value if event.row_key else None
        self._picked_row(str(key) if key is not None else None)

    def _picked_row(self, key: str | None) -> None:
        """A Work row picked, by Enter or a click: a heading folds its group (``f``'s), a task
        opens in the reader (B2: a click and its key do one thing)."""
        if key is not None and key.startswith(GROUP_ROW):
            self._fold(key.removeprefix(GROUP_ROW))
            return
        self._show_task_detail(key)
        if key is not None and key in self._tasks_by_id:
            self.open_reader(key)

    def on_option_list_option_selected(self, event: OptionList.OptionSelected) -> None:
        # OptionSelected (click / Enter) only, mirroring the task table: an
        # append/scroll never posts it, so a feed selection is stable.
        if event.option is None or event.option.id is None:
            return
        stored = self._events_by_id.get(event.option.id)
        if stored is None:
            return
        self._show_event_detail(stored)

    def _show_event_detail(self, stored: TeamEvent) -> None:
        self.detail_moment = (stored.session_id, stored.created_at)
        detail = watch._event_detail(stored, self._roles, self._names)
        if self._moment_transcript() is not None:
            detail.append(TRANSCRIPT_HINT, style="dim")
        self._show_detail(detail)

    # --- the reader (B2) ----------------------------------------------------------------

    READER_HIDES: ClassVar[frozenset[str]] = frozenset(
        {
            "toggle_board",
            "toggle_done",
            "fold_group",
            "toggle_autoscroll",
            "toggle_select",
            "search",
            "lane_item",
            "resize",
            "reset_sizes",
        }
    )
    """The keys that act on what the reader covers: off, and out of the footer, while it is
    open. ``o`` (the transcript at that moment) and Esc stay."""

    def check_action(self, action: str, parameters: tuple[object, ...]) -> bool | None:
        return self._reading is None or action not in self.READER_HIDES

    def open_reader(self, task_id: str) -> None:
        """Show ``task_id`` full height: the task in full, then its history — every event that
        names it, oldest first, read off the UI thread (``team.task_history``). Esc closes."""
        task = self._tasks_by_id.get(task_id)
        if task is None:
            return
        self._reading = task_id
        self.add_class("reading")
        self.refresh_bindings()
        self._apply_layout()
        self._render_reader(task, None)
        reader = self.query_one("#reader", VerticalScroll)
        reader.border_title = f"#{task.number} — Esc goes back" if task.number else "Esc goes back"
        reader.scroll_home(animate=False)
        reader.focus()
        self.run_worker(
            functools.partial(team_service.task_history, task),
            name=HISTORY_WORKER,
            group=HISTORY_WORKER,
            exclusive=True,
            thread=True,
            exit_on_error=False,
        )

    def _render_reader(self, task: TeamTask, history: Sequence[TeamEvent] | None) -> None:
        why = team_service.stopped_because(task) if task.status in watch.STOPPED_BECAUSE else None
        text = watch._task_detail(task, self._statuses, self._names, why=why)
        text.append("\n\nhistory", style="bold")
        if history is None:
            text.append("\nreading…", style="dim")
        elif not history:
            text.append("\nnothing on the board names it yet", style="dim")
        for event in history or ():
            emoji, name, style = watch._who(event, self._roles, self._names)
            verb_emoji, verb = watch._KIND_VERB.get(event.kind, ("•", event.kind))
            text.append(f"\n{watch.clock(event.created_at)}  ", style="dim")
            text.append(f"{emoji} {name} ", style=style)
            text.append(f"{verb_emoji} {verb}: ", style="bold")
            first, *rest = watch.shown_text(event).split("\n")
            text.append(first)
            for line in rest:
                text.append(f"\n    {line}")
        self.reader_text = text.plain
        self.query_one("#reader-text", Static).update(text)

    @on(Worker.StateChanged)
    def _history_answered(self, event: Worker.StateChanged) -> None:
        if event.worker.group != HISTORY_WORKER or event.state is not WorkerState.SUCCESS:
            return
        task = self._tasks_by_id.get(self._reading or "")
        if task is not None and isinstance(event.worker.result, list):
            self._render_reader(task, event.worker.result)

    def action_back(self) -> None:
        """Esc: out of the reader, else off the Stream's filter (B2)."""
        if self._reading is not None:
            self.action_close_reader()
        elif self.stream_filter or self.query_one("#feedpane").has_class("searching"):
            self._end_search(keep=False)

    def action_search(self) -> None:
        """``/``: filter the Stream — the Stream shown first if Work had the column."""
        if self._reading is not None:
            return
        if not self._wide():
            self.pane = STREAM
            self._apply_layout()
        self.query_one("#feedpane").add_class("searching")
        search = self.query_one("#stream-search", Input)
        search.value = self.stream_filter
        search.can_focus = True
        search.focus()

    @on(Input.Changed, "#stream-search")
    def _search_typed(self, event: Input.Changed) -> None:
        event.stop()
        self.stream_filter = event.value.strip()
        self._refilter()
        self._remember_view()

    @on(Input.Submitted, "#stream-search")
    def _search_kept(self, event: Input.Submitted) -> None:
        event.stop()
        self._end_search(keep=True)

    def _end_search(self, *, keep: bool) -> None:
        """Close the filter's field; ``keep`` the filter it holds, or clear it."""
        self.query_one("#feedpane").remove_class("searching")
        self.query_one("#stream-search", Input).can_focus = False
        if not keep and self.stream_filter:
            self.stream_filter = ""
            self._refilter()
            self._remember_view()
        self._retitle_feed()
        self.query_one("#feed").focus()

    def action_close_reader(self) -> None:
        """Back from the reader to the board, the task still under the cursor."""
        if self._reading is None:
            return
        self._reading = None
        self.reader_text = ""
        self.remove_class("reading")
        self.refresh_bindings()
        self._apply_layout()
        self.query_one("#tasks").focus()

    # --- actions -------------------------------------------------------------------

    def action_toggle_done(self) -> None:
        """``d``: unfold Done, or fold it again — Work shown first if it was out of sight, or
        the key changed a table nobody could see."""
        self._show_work()
        self._fold("done")

    def action_open_transcript(self) -> None:
        """Jump into the author session's transcript around the selected moment."""
        target = self._moment_transcript()
        if target is None:
            self.notify(
                "no transcript for this item (session predates transcript capture, "
                "or it was a CLI/remote action)",
                timeout=5,
                markup=False,
            )
            return
        transcript, when = target
        line = watch.transcript_line_near(transcript, when)
        error = watch.action_open_transcript(self.app, watch._transcript_command(transcript, line))
        if error is not None:
            self.notify(
                f"could not open transcript: {error}", severity="error", timeout=6, markup=False
            )

    def _feed_title(self) -> str:
        title = "stream"  # the switch's word for it (B2)
        if self._select_mode:
            return f"{title} — SELECT MODE (drag to select, c copies, v resumes)"
        said = []
        if not self.autoscroll:
            said.append("autoscroll off")
        if self.is_mounted and self._wide() and self._work_hidden:
            said.append(TASKS_HIDDEN)
        if self.stream_filter:
            shown = self.query_one("#feed", OptionList).option_count if self.is_mounted else 0
            said.append(
                f"filter “{self.stream_filter}”: {shown} of {len(self._feed_order)} · Esc clears"
            )
        return " — ".join([title, *said])

    def _retitle_feed(self) -> None:
        """The feed's title as it reads now, on whichever of the two shows the feed."""
        feed = self.query_one("#feed", OptionList)
        shown = self.query_one("#feedtext") if feed.has_class("hidden") else feed
        shown.border_title = self._feed_title()

    def action_toggle_autoscroll(self) -> None:
        self.autoscroll = not self.autoscroll
        feed = self.query_one("#feed", OptionList)
        feed.border_title = self._feed_title()
        if self.autoscroll:
            feed.scroll_end(animate=False)
            self.call_after_refresh(self._recount_unseen)  # at its end, nothing is unseen

    def action_toggle_select(self) -> None:
        """Swap the live feed for a frozen, mouse-selectable text view."""
        self._select_mode = not self._select_mode
        feed = self.query_one("#feed", OptionList)
        wrap = self.query_one("#feedtext", VerticalScroll)
        if self._select_mode:
            snapshot = Text()
            for event_id in self._feed_order:
                snapshot.append(
                    watch.feed_line(self._events_by_id[event_id], self._roles, self._names)
                )
                snapshot.append("\n")
            self.query_one("#feedstatic", Static).update(snapshot)
            feed.add_class("hidden")
            wrap.add_class("active")
            wrap.border_title = self._feed_title()
            wrap.scroll_end(animate=False)
        else:
            feed.remove_class("hidden")
            wrap.remove_class("active")
            feed.border_title = self._feed_title()
            self.refresh_data()  # apply the backlog accumulated while frozen
            if self.autoscroll:
                feed.scroll_end(animate=False)

    def action_copy_selection(self) -> None:
        """Copy the mouse-selected text (select mode) to the clipboard."""
        getter = getattr(self.screen, "get_selected_text", None)
        selected = getter() if callable(getter) else None
        if not selected:
            self.notify(
                "nothing selected — v enters select mode, then drag", timeout=4, markup=False
            )
            return
        copy_and_tell(self.app, selected, f"copied {len(selected)} chars", timeout=3)

    def action_toggle_board(self) -> None:
        """``b``: below :data:`WIDE_FROM`, Work or the Stream (B2); wide, the Work column off
        or on (B1). The focus goes with what comes into sight."""
        if self._wide():
            self._work_hidden = not self._work_hidden
        else:
            self.pane = STREAM if self.pane == WORK else WORK
        self._apply_layout()
        if not self.query_one("#board-side").has_class("collapsed"):
            self.query_one("#tasks").focus()
        else:
            self._feed_shown().focus()

    def on_board_switch_picked(self, event: BoardSwitch.Picked) -> None:
        if event.pane != self.pane:
            self.action_toggle_board()

    def action_refresh_now(self) -> None:
        self.refresh_data()

    def action_resize(self, line: str, sign: int) -> None:
        """``[ ]`` Work's width, ``( )`` the team block's height, ``{ }`` the detail pane's: a
        line's drag, by key, remembered as the drag is."""
        divider = self.query_one(f"#{line}", Divider)
        if divider.display:
            divider.step(sign * DIVIDER_STEP)

    def action_reset_sizes(self) -> None:
        """``=``: every line back to the stylesheet's size, as a double-click puts one back."""
        for line in DIVIDER_KEYS:
            self.query_one(f"#{line}", Divider).reset()

    def _wide(self) -> bool:
        return self.size.width >= WIDE_FROM

    def _show_work(self) -> None:
        """Work into sight, whatever hid it: a card it is asked to go to is in it."""
        self._work_hidden = False
        self.pane = WORK
        self._apply_layout()

    def _apply_layout(self) -> None:
        """Lay the halves out for the panel's width: side by side from :data:`WIDE_FROM`, one at
        a time below it, under the switch that says which.

        Focus inside a half going out of sight moves to the half that shows. Left on a hidden
        table it went to nothing, and with nothing focused no key reaches this panel: the next
        ``b`` could not bring it back.
        """
        wide = self._wide()
        work = not self._work_hidden if wide else self.pane == WORK
        stream = wide or self.pane == STREAM
        side = self.query_one("#board-side")
        side.set_class(not work, "collapsed")
        self.query_one("#feedpane").set_class(not stream, "collapsed")
        self.query_one("#side-divider").set_class(not (wide and work), "collapsed")
        switch = self.query_one(BoardSwitch)
        switch.display = not wide and self._reading is None
        switch.show(self.pane)
        alone = work and not stream
        if alone != side.has_class("alone"):
            if alone:
                self._side_width = side.styles.inline.width  # the line's, kept for the way back
            side.set_class(alone, "alone")
            side.styles.width = None if alone else self._side_width
        self._keep_alone()
        if self.size.width > 0 and self.size.height > 0:
            # The ceilings for the layout just set, not the last resize's: a cap from the
            # side-by-side layout left Work at 67 of 100 alone (B2's tester, #695).
            self._fit(self.size.width, self.size.height)
        if stream and self._reading is None and self._catch_up:
            self._catch_up = False
            feed = self.query_one("#feed", OptionList)
            self.call_after_refresh(feed.scroll_end, animate=False)
        self._retitle_feed()
        self._remember_view()  # the half shown, when b or a pick moved it
        focused = self.app.focused
        if focused is None or self not in focused.ancestors:
            return
        if not work and side in focused.ancestors_with_self:
            self._feed_shown().focus()
        elif not stream and self.query_one("#feedpane") in focused.ancestors_with_self:
            self.query_one("#tasks").focus()

    def _keep_alone(self) -> None:
        """Work's width, kept: beside the Stream, what its line set is noted; with the column to
        itself, a width its line writes (the remembered one, restored a refresh after mount) is
        noted and cleared, so Work fills the column (the ``alone`` rule) and gets it back when
        the Stream returns. Called on every resize of the column."""
        side = self.query_one("#board-side")
        width = side.styles.inline.width
        if not side.has_class("alone"):
            self._side_width = width
        elif width is not None:
            self._side_width = width
            side.styles.width = None

    def on_resize(self, event: events.Resize) -> None:
        self._apply_layout()
        if event.size.width > 0 and event.size.height > 0:
            self._fit(event.size.width, event.size.height)
            self.call_after_refresh(self._recount_unseen)  # a taller feed may show them now

    def _fit(self, width: int, height: int) -> None:
        """Write the column's and the detail pane's ceilings from the panel's size.

        Each is the panel less its line and what the other side must keep —
        the feed's ``min-width``, the board's ``min-height`` above the detail
        pane — so Textual clamps the dragged part on every layout pass and a
        shrinking terminal re-clamps it with no handler. Too small for both
        minimums, the dragged part's floor wins.
        """
        side = self.query_one("#board-side")
        side.styles.max_width = (
            None
            if side.has_class("alone")
            else max(floor_of(side), width - 1 - floor_of(self.query_one("#feedpane")))
        )
        detail = self.query_one("#detailwrap")
        strip = self.query_one(NeedsYouLane)
        detail.styles.max_height = max(
            floor_of(detail, "height"),
            height
            - 1
            - floor_of(strip, "height")
            - (0 if self._wide() else 1)  # the switch's line
            - floor_of(self.query_one("#board-main"), "height"),
        )
