"""The app-wide "needs you" bar (UX audit M7): one line over asq, on every view, while any
project has something that needs the person.

A project's board says what needs the person on that board (``board.needs_you``, its lane);
this bar says it across every project the sidebar lists, whichever view is open. It names how
many things need the person and the first of them with its project — a question first, then an
agent whose usage-limit hand-over failed, an agent waiting in its pane, a blocked card, a card
in review — and :data:`OPEN_KEY` or a click opens that project's Board tab on it. With nothing,
the bar takes no space.

Its read is its own and off the UI thread: every :data:`BAR_SECONDS`, each listed project's
board, as the board's own tick reads it (``team.board_read``), counted by the board's own rule
(``board.needs_you``), so "needs you" means one thing on the board and over the app.
"""

from __future__ import annotations

import functools
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import ClassVar

from rich.text import Text
from textual import on
from textual.css.query import NoMatches
from textual.widget import Widget
from textual.widgets import Static
from textual.worker import Worker, WorkerState

from aisquare.cli import watch
from aisquare.cli.ui.board import NEEDS_YOU, BoardPanel, needs_you
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
KIND_ORDER: tuple[str, ...] = ("question", HANDOVER, "waiting", "blocked", "review")
"""Which comes first across projects: what only the person can answer, then what is stuck."""
GLYPHS: dict[str, str] = {kind: glyph for kind, glyph, _one, _many in NEEDS_YOU} | {HANDOVER: "⏳"}
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
            read = team_service.board_read(
                events=200, project=project, exclude_kinds=team_service.CAPTAIN_AUDIT_KINDS
            )
        except Exception:
            continue
        shown, sessions, tasks, events = read[:4]
        seq = max((event.seq for event in events), default=0)
        needs += project_needs(
            shown, sessions, tasks, events, seq=seq, fleet_labels=read.fleet_labels
        )
    return sorted(needs, key=lambda need: KIND_ORDER.index(need.kind))


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
        if not self.needs:
            return
        need = self.needs[0]
        self.app.post_message(ProjectSelected(need.project.id))
        self._landing = (need, 0)
        self.call_after_refresh(self._land)

    def _land(self) -> None:
        """Once the project's view is built: its Board tab, on the item."""
        if self._landing is None:
            return
        need, tries = self._landing
        views = [
            node
            for node in self.app.query(f"#project-{need.project.id}")
            if isinstance(node, ProjectView)
        ]
        try:
            if not views:
                raise NoMatches("the project's view is not built yet")
            view = views[0]
            view.active = "tab-board"  # raises until its tabs are composed
        except NoMatches:
            if tries < LAND_TRIES:
                self._landing = (need, tries + 1)
                self.set_timer(0.05, self._land)
            else:
                self._landing = None
            return
        self._landing = None
        panel = view.query_one(BoardPanel)
        if need.kind in ("blocked", "review") and (need.kind, need.target) in panel.lane:
            panel.action_lane_item(panel.lane.index((need.kind, need.target)))
            return
        target: Widget = panel.query_one("#tasks")
        if target.display:
            target.focus()
