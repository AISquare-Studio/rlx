"""The app-wide "needs you" bar (UX audit M7): one line over asq, on every view, while any
project has something that needs the person.

A project's board says what needs the person on that board (``board.needs_you``, its lane);
this bar says it across every project the sidebar lists, whichever view is open. It names how
many things need the person and the first of them with its project, in the board's own order
(:data:`KIND_ORDER`) — and :data:`OPEN_KEY` or a click opens that project's Board tab on it.
With nothing, the bar takes no space.

Its read is its own and off the UI thread: every :data:`BAR_SECONDS`, each listed project's
board, as the board's own tick reads it (``team.board_read``), counted by the board's own rule
(``board.needs_you``), so "needs you" means one thing on the board and over the app.
"""

from __future__ import annotations

import contextlib
import functools
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from typing import ClassVar

from rich.text import Text
from textual import on
from textual.css.query import NoMatches
from textual.timer import Timer
from textual.widget import Widget
from textual.widgets import ContentSwitcher, DataTable, Static, TabPane
from textual.worker import Worker, WorkerState

from aisquare.cli import watch
from aisquare.cli.ui.board import NEEDS_YOU, BoardPanel, needs_you
from aisquare.cli.ui.board_parts import NeedsYouLane
from aisquare.cli.ui.sidebar import ProjectSelected, project_name
from aisquare.cli.ui.ticks import while_running
from aisquare.cli.ui.views.project import ProjectView
from aisquare.models import ProjectInfo, TeamEvent, TeamSession, TeamTask
from aisquare.services import team as team_service

BAR_SECONDS = 10.0
"""How often the bar reads every listed project's board: a board's own tick is 2 s, for the one
board on screen; the bar's is for all of them, so it is slower."""
BAR_WORKER = "needs-you-bar"
OPEN_KEY = "f6"
"""The key that opens the bar's first item: a function key, as a letter would be typed into a
form's field. A terminal pane keeps every key but the escape key, so it works outside one."""
HANDOVER = "handover"
"""An agent on a usage limit whose automatic hand-over to another account failed (#277)."""
KIND_ORDER: tuple[str, ...] = tuple(
    kind
    for board_kind, *_rest in NEEDS_YOU
    for kind in ((board_kind, HANDOVER) if board_kind == "waiting" else (board_kind,))
)
"""Which comes first across projects: the board's lane's order (``board.NEEDS_YOU``: an agent
waiting, a question, a card in review, a blocked card), the hand-over the lane has not, beside
the waiting agent. One order: the bar's first item is its project's first lane row, so it is
on the lane the key opens (tester of #11: blocked before review here, review before blocked
there, and the card the bar named was past the rows the lane draws)."""
GLYPHS: dict[str, str] = {kind: glyph for kind, glyph, _one, _many in NEEDS_YOU} | {HANDOVER: "⏳"}
BOARD_TAB = "--content-tab-tab-board"
"""The Board tab's id in a project view's tab row (Textual's ``ContentTab`` prefix on the pane's
id): there, once the row is filled."""
LAND_TRIES = 40
"""How many short waits the bar gives a project's view to be built before it gives up."""


@dataclass(frozen=True)
class Need:
    """One thing that needs the person: its project, its kind, what it names (a task's id or
    an agent's label) and how the bar says it."""

    project: ProjectInfo
    kind: str
    target: str
    text: str


def project_needs(
    project: ProjectInfo,
    sessions: Sequence[TeamSession],
    tasks: Sequence[TeamTask],
    events: Sequence[TeamEvent],
    *,
    seq: int,
    fleet_labels: Mapping[str, str] | None = None,
) -> list[Need]:
    """What needs the person on one board: ``board.needs_you``'s items, and each limited agent
    whose last word on the board is a failed hand-over."""
    found = needs_you(project, sessions, tasks, events, seq=seq, fleet_labels=fleet_labels)
    by_id = {task.id: task for task in tasks}
    named = {s.id: team_service.board_name(s, fleet_labels) for s in sessions}
    needs: list[Need] = []
    for label in found.agents.get("question", []):
        asked = next(
            (
                e
                for e in reversed(events)
                if e.kind == "question" and named.get(e.session_id or "") == label
            ),
            None,
        )
        said = watch.one_row(asked.text) if asked is not None else "a question"
        needs.append(Need(project, "question", label, f"{label} asks: {said}"))
    for session in sessions:
        if session.ended_at is not None or session.state != "limited":
            continue
        last = next((e for e in reversed(events) if e.session_id == session.id), None)
        if last is not None and last.kind == "handover_failed":
            label = named.get(session.id, session.role)
            text = f"{label} is on a usage limit and its hand-over failed"
            needs.append(Need(project, HANDOVER, label, text))
    for label in found.agents.get("waiting", []):
        needs.append(Need(project, "waiting", label, f"{label} waits for you in its pane"))
    for kind, said in (("blocked", "blocked"), ("review", "in review")):
        for task_id in found.tasks.get(kind, []):
            task = by_id.get(task_id)
            title = f"#{task.number} {task.title}" if task and task.number else task_id
            needs.append(Need(project, kind, task_id, f"{title} — {said}"))
    return needs


def read_needs(projects: Sequence[ProjectInfo]) -> list[Need]:
    """Every listed project's needs, the most pressing kind first, in the sidebar's order within
    a kind. A board that cannot be read now is left out until the next read.

    The one function the bar reads through (the manager's #1074): a durable "pending on you"
    list (0.10.3) points it at its store, and the bar is unchanged."""
    needs: list[Need] = []
    for project in projects:
        try:
            # Paged (#44): the open tasks and the sessions these events and tasks name, all
            # ``project_needs`` reads, not every task and session the board ever had.
            read = team_service.board_read(
                events=200,
                project=project,
                exclude_kinds=team_service.CAPTAIN_AUDIT_KINDS,
                closed=False,
                sessions=(),
            )
        except Exception:
            continue
        shown, sessions, tasks, events = read[:4]
        seq = max((event.seq for event in events), default=0)
        needs += project_needs(
            shown, sessions, tasks, events, seq=seq, fleet_labels=read.fleet_labels
        )
    return sorted(needs, key=lambda need: KIND_ORDER.index(need.kind))


def _board_ready(view: ProjectView) -> bool:
    """Whether ``view`` can show its Board tab: the tab in its tab row, and the tab's pane in its
    switcher with the board in it composed. Before all of that, setting its active tab raises,
    or hides one pane and shows none.

    And the view has taken its first tab. Its tab row activates that tab as the view composes,
    and the view handles it only after its own mount, so a view just mounted has no active tab
    yet: the Board tab set then was put back to the first tab (Manager) a moment later, and the
    card never opened (#40)."""
    if not view.is_mounted or not view.active:
        return False
    if not view.query(f"ContentTabs #tabs-list > #{BOARD_TAB}"):
        return False
    pane = view.get_child_by_type(ContentSwitcher).get_child_by_id("tab-board")
    panels = pane.query(BoardPanel)
    if not panels:
        return False
    panel = panels.first()
    return panel.is_mounted and bool(panel.query("#sessions")) and bool(panel.query("#tasks"))


class NeedsYouBar(Static):
    """One line over asq while anything needs the person; no space with nothing."""

    DEFAULT_CSS = """
    NeedsYouBar { height: 1; padding: 0 1; display: none; background: $warning 20%;
                  text-wrap: nowrap; text-overflow: ellipsis; }
    NeedsYouBar.shown { display: block; }
    NeedsYouBar:hover { background: $warning 35%; }
    """
    board_key: ClassVar[str] = OPEN_KEY
    """A click on the bar does what its key does: open its first item."""

    def __init__(self, *, id: str | None = None) -> None:
        super().__init__("", id=id)
        self.needs: list[Need] = []
        self._landing: tuple[Need, int] | None = None
        self._waiting: Timer | None = None
        """The landing's next short wait, stopped when another item is opened meanwhile."""

    def on_mount(self) -> None:
        self.set_interval(BAR_SECONDS, self._tick)
        self.call_after_refresh(self.read_now)

    @while_running
    def _tick(self) -> None:
        self.read_now()

    def read_now(self) -> None:
        """Read every listed project's board again, off the UI thread."""
        snapshot = getattr(self.app, "snapshot", None)
        projects = list(getattr(snapshot, "projects", None) or [])
        self.run_worker(
            functools.partial(read_needs, projects),
            name=BAR_WORKER,
            group=BAR_WORKER,
            exclusive=True,
            thread=True,
            exit_on_error=False,
        )

    @on(Worker.StateChanged)
    def _read(self, event: Worker.StateChanged) -> None:
        if event.worker.group != BAR_WORKER or event.state is not WorkerState.SUCCESS:
            return
        if isinstance(event.worker.result, list):
            self.show(event.worker.result)

    def show(self, needs: list[Need]) -> None:
        """Say ``needs``: the count and the first, with its project; nothing at all with none."""
        self.needs = needs
        self.set_class(bool(needs), "shown")
        self.update(self.line())

    def line(self) -> Text:
        if not self.needs:
            return Text("")
        first = self.needs[0]
        text = Text()
        text.append(f"needs you: {len(self.needs)}", style="bold")
        text.append(" — ")
        text.append(f"{GLYPHS[first.kind]} {project_name(first.project)}: ", style="bold")
        text.append(first.text)
        text.append(f"   {OPEN_KEY.upper()} opens it", style="dim")
        return text

    def on_click(self) -> None:
        self.open_first()

    def open_first(self) -> None:
        """Open the first item on its project's board: the project's view, its Board tab, and
        a card in the board's reader."""
        if self.needs:
            self.open_need(self.needs[0])

    def open_need(self, need: Need) -> None:
        """Open ``need`` on its project's board, wherever it sits on the lane."""
        self.app.post_message(ProjectSelected(need.project.id))
        if self._waiting is not None:  # the one opening before: this one replaces it
            self._waiting.stop()
            self._waiting = None
        self._landing = (need, 0)
        self.call_after_refresh(self._land)

    @staticmethod
    def _cursor_on(panel: BoardPanel, task_id: str) -> None:
        """The work list's cursor on ``task_id``, when its row is listed (not folded)."""
        table = panel.query_one("#tasks", DataTable)
        with contextlib.suppress(Exception):  # a folded group lists no row for it
            table.move_cursor(row=table.get_row_index(task_id))

    def _land(self) -> None:
        """Once the project's view is ready (:func:`_board_ready`): its Board tab, and once that
        is shown, the item (:meth:`_open_on_board`).

        A view still mounting is waited for, a short wait at a time: set as it mounted, its
        Board tab raised out of here (``ValueError``, the tab row not filled yet: asq exited),
        or showed no pane at all (the board's pane not mounted yet: the reader opened in a
        hidden pane, the keys going to it) (tester of #11, #1636)."""
        if self._landing is None:
            return
        need, tries = self._landing
        view = self._view_of(need)
        try:
            ready = view is not None and _board_ready(view)
            if ready and view is not None:
                view.active = "tab-board"
        except (NoMatches, ValueError):  # mounting still, under the checks
            ready = False
        if not ready:
            self._try_again(need, tries, self._land)
            return
        self._landing = (need, 0)
        self.call_after_refresh(self._open_on_board)

    def _open_on_board(self) -> None:
        """The Board tab shown: the item on it. Not shown yet, a short wait; another tab chosen
        meanwhile, nothing (the person moved on). Nothing is focused that is not on screen.

        Opened on the board's own queue (``panel.call_later``), not here: once the board is on
        screen its Show is queued on it, and the board hands the keys to its work list as it
        handles it. Opened before that, the reader's focus was queued first and the work list's
        after it; below the wide layout the reader covers that list, its blur reset the focus,
        and no key reached the card (tester of #11, #1672). After it, the reader's is last."""
        if self._landing is None:
            return
        need, tries = self._landing
        view = self._view_of(need)
        if view is None or view.active != "tab-board":
            self._landing = None
            return
        try:
            pane = view.query_one("#tab-board", TabPane)
            panel = pane.query_one(BoardPanel)
            shown = pane.display and panel.is_on_screen
            if shown:
                panel.call_later(self._open_item, panel, need)
        except NoMatches:
            shown = False
        if shown:
            self._landing = None
        else:
            self._try_again(need, tries, self._open_on_board)

    def _try_again(self, need: Need, tries: int, step: Callable[[], None]) -> None:
        self._waiting = None
        if tries < LAND_TRIES:
            self._landing = (need, tries + 1)
            self._waiting = self.set_timer(0.05, step)
        else:
            self._landing = None  # given up: the bar stays, and F6 tries again

    def _view_of(self, need: Need) -> ProjectView | None:
        views = [
            node
            for node in self.app.query(f"#project-{need.project.id}")
            if isinstance(node, ProjectView)
        ]
        return views[0] if views else None

    def _open_item(self, panel: BoardPanel, need: Need) -> None:
        """``need`` on ``panel``, shown: a card in the board's reader, else the work list. On the
        board's queue; a board gone meanwhile is nothing to open."""
        with contextlib.suppress(NoMatches):
            self._open_on(panel, need)

    def _open_on(self, panel: BoardPanel, need: Need) -> None:
        card = (need.kind, need.target)
        if need.kind in ("blocked", "review") and card not in panel.lane:
            # A view built earlier holds the lane it last read: its ticks skip a hidden board,
            # and its on_show catches up only after this has run (the tester of #11). Read it
            # now, as its tick does, so the card is there to open.
            panel.refresh_data()
        if need.kind in ("blocked", "review") and card in panel.lane:
            index = panel.lane.index(card)
            if index < panel.query_one(NeedsYouLane).drawn:
                panel.action_lane_item(index)  # the lane's own row: cursor, fold, reader
            else:
                # Past the rows the lane draws (it acts on a drawn row only): the card in the
                # board's reader all the same, the one the bar named.
                self._cursor_on(panel, need.target)
                panel.open_reader(need.target)
            return
        target: Widget = panel.query_one("#tasks")
        if target.display:
            target.focus()
