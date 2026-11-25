"""Settings ▸ tmux: every window on rlx's own tmux server, read again while the page shows.

Opened from *⚙ Settings ▸ tmux* (handoff E; the owner's decisions, Q2). What a row says and
whose a window is are ``services.tmux_panes``': project (its session's codename), the agent (the
fleet row whose launch record places it there, else *no agent*), command, age, and alive or
dead with its exit status.

**Actions**, on the highlighted row, each a button and a key:

- *Open* (Enter, ``o``): an agent's pane opens its agent view; any other window opens in a
  terminal pane over the page (:class:`PaneScreen`), as the persona editor's does.
- *Stop…* (``s``): a dead window, or a live one nobody's, closed after a confirm that names it.
  Not offered for an agent's live pane: that is its agent's own Stop.
- *Adopt…* (``a``): a live Claude Code or Codex nobody's. What it would adopt, and into which
  project, is found first (off the UI thread); the confirm says it; then the agent is quit in
  its pane and its conversation resumed in an rlx window. Never a plain shell.

**Panes rlx did not start** sit under a collapsed fold below, read-only: the page has no act
for them. The fold is not there at all when there are none.

**It reads only while it shows**, as Agent resources samples: a timer every
:data:`REFRESH_SECONDS` from its Show to its Hide, one read at a time off the UI thread. ``r``
reads now. No tmux (Windows): one plain line says so, and nothing else shows.

``reader`` is how the page reads (``() -> TmuxReport``); tests hand a fake.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any, ClassVar

from rich.table import Table
from rich.text import Text
from textual import on
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.css.query import NoMatches
from textual.message import Message
from textual.screen import ModalScreen
from textual.timer import Timer
from textual.widgets import Button, Collapsible, ContentSwitcher, DataTable, Static
from textual.worker import Worker, WorkerState

from aisquare.cli.fleet_top_render import duration
from aisquare.cli.ui.sidebar import AgentSelected
from aisquare.cli.ui.terminal import EscapeToSidebar, TerminalPane
from aisquare.cli.ui.ticks import while_running
from aisquare.core.tmux import TmuxServer
from aisquare.services import adopt as adopt_service
from aisquare.services import project as project_service
from aisquare.services import tmux_panes
from aisquare.services.tmux_panes import Pane, TmuxReport

VIEW_ID = "tmux"
"""The page's id among the app's pages (the ``#content`` switcher)."""

REFRESH_SECONDS = 5.0
"""How often the page reads while it shows."""

READ_WORKER = "tmux-page-read"
ACT_WORKER = "tmux-page-act"
"""A stop, an adopt's look first, or an adopt: one at a time, every act button greyed."""

TITLE = "tmux"
ABOUT = "Every window on rlx's own tmux server, an agent's or not."
EMPTY = "No windows on rlx's tmux server: nothing is running there."
OTHERS_TITLE = "Not started by rlx"
OTHERS_ABOUT = "Read-only: rlx never types into, sends keys to or closes these."

COLUMNS = ("Project", "Agent", "Command", "Age", "State")
"""The table's columns; the highlighted window's own name is the hint line's, in full (at
80x24 a sixth column was clipped to "Win")."""

Reader = Callable[[], TmuxReport]


def default_reader() -> Reader:
    return tmux_panes.read


def pane_server() -> TmuxServer:
    """The server a window the page opens is shown from: rlx's own (``tmux_panes``), where
    the page read it."""
    return tmux_panes.rlx_server()


def age_text(pane: Pane, report: TmuxReport) -> str:
    age = pane.age(report.read_at)
    return "—" if age is None else duration(int(age.total_seconds()))


def offers(pane: Pane | None) -> dict[str, bool]:
    """Which acts the highlighted row offers: ``open``, ``stop``, ``adopt``."""
    if pane is None:
        return {"open": False, "stop": False, "adopt": False}
    return {"open": True, "stop": pane.stoppable, "adopt": pane.adoptable}


def show_page(app: App[Any], reader: Reader | None = None) -> None:
    """Show the page among the app's pages, building it the first time; ``⚙ Settings`` stays
    the sidebar's highlighted row, as for Agent resources. The hub's *tmux* link calls this.

    The keyboard goes to the page's table as it shows, as the hub's goes to its first link:
    left on the hub's button, which the page hides, ↑/↓, Enter, ``s`` and ``a`` reached
    nothing of the page's."""
    switcher = app.query_one("#content", ContentSwitcher)
    try:
        page = switcher.get_child_by_id(VIEW_ID, TmuxView)
    except NoMatches:
        page = TmuxView(reader=reader, id=VIEW_ID)
        page.focus_on_show = True
        switcher.add_content(page, set_current=True)
        return
    if switcher.current != VIEW_ID:
        page.focus_on_show = True
        switcher.current = VIEW_ID  # its Show reads at once
    else:
        page.focus_rows()
        page.tick()


class WindowTable(DataTable[Text]):
    """The windows' table: Enter opens the highlighted window (:class:`OpenAsked`); a click
    only highlights. ``DataTable`` selects a row clicked while highlighted, so with one window
    listed a single click opened its pane (the tester's E on #565)."""

    class OpenAsked(Message):
        """Enter on the highlighted row."""

    def action_select_cursor(self) -> None:
        self.post_message(self.OpenAsked())


class ConfirmPaneScreen(ModalScreen[bool]):
    """The question for one window, naming it, its button and Cancel: ``True`` at the
    button; ``False`` at Cancel or Escape. The button has the focus, so Enter confirms."""

    DEFAULT_CSS = """
    ConfirmPaneScreen { align: center middle; }
    ConfirmPaneScreen > Vertical { width: 72; max-width: 94%; height: auto; border: heavy $accent;
                                   background: $surface; padding: 0 1; }
    ConfirmPaneScreen #tmux-confirm-question { height: auto; padding: 1 0; }
    ConfirmPaneScreen .tmux-confirm-buttons { height: auto; align-horizontal: right;
                                              padding-bottom: 1; }
    ConfirmPaneScreen .tmux-confirm-buttons Button { margin-left: 2; min-width: 10; }
    """
    BINDINGS: ClassVar = [Binding("escape", "cancel", "cancel")]
    AUTO_FOCUS = "#tmux-confirm-yes"

    def __init__(self, question: Text, verb: str, *, danger: bool = False) -> None:
        super().__init__()
        self.question = question
        self.verb = verb
        self.danger = danger

    def compose(self) -> ComposeResult:
        with Vertical():
            yield Static(self.question, id="tmux-confirm-question")
            with Horizontal(classes="tmux-confirm-buttons"):
                yield Button(
                    self.verb, id="tmux-confirm-yes", variant="error" if self.danger else "primary"
                )
                yield Button("Cancel", id="tmux-confirm-no")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        event.stop()
        self.dismiss(event.button.id == "tmux-confirm-yes")

    def action_cancel(self) -> None:
        self.dismiss(False)


def stop_question(pane: Pane) -> Text:
    question = Text()
    question.append("Close this window?\n\n", style="bold")
    question.append(pane.where, style="bold")
    question.append(f"\n{pane.project_name} · {pane.label} · {pane.command} · {pane.state}\n\n")
    question.append(
        "Its window closes, with what runs in it."
        if not pane.dead
        else "It has exited: its window closes, and its last screen with it.",
        style="dim",
    )
    if pane.siblings:
        others = ", ".join(pane.siblings)
        many = len(pane.siblings) > 1
        question.append(
            f"\nIts window's other pane{'s' if many else ''} ({others}) close{'' if many else 's'} "
            "with it, and what runs there.",
            style="bold",
        )
    question.append("\nNothing else on the server is touched.", style="dim")
    return question


def adopt_question(pane: Pane, plan: adopt_service.PanePlan) -> Text:
    conversation = plan.conversation
    question = Text()
    question.append(f"Adopt this {plan.agent_label} into {_project(plan)}?\n\n", style="bold")
    question.append(pane.where, style="bold")
    question.append(f"\nits conversation {conversation.session_id[:8]}")
    if conversation.first_prompt:
        question.append(f": “{conversation.first_prompt}”")
    question.append(f"\non {conversation.account}, in {conversation.cwd or 'its folder'}\n\n")
    question.append(
        f"{plan.exit_command} is typed in its pane, so it quits, and its conversation is "
        "resumed in a new rlx window, as a coder. A window it leaves empty is closed.",
        style="dim",
    )
    return question


def _project(plan: adopt_service.PanePlan) -> str:
    return project_service.display_name(plan.project)


class PaneScreen(ModalScreen[None]):
    """A window that is no agent's, live in a terminal pane over the page, as the persona
    editor's pane is: its keys go to it, the escape key hands them back, then Escape or Close
    shuts this. Nothing here closes the window."""

    DEFAULT_CSS = """
    PaneScreen { align: center middle; }
    PaneScreen > Vertical { width: 120; max-width: 96%; height: 90%; border: heavy $accent;
                            background: $surface; padding: 0 1; }
    PaneScreen #tmux-pane-header { height: auto; padding: 1 0 0 0; }
    PaneScreen #tmux-pane-view { height: 1fr; min-height: 6; }
    PaneScreen .tmux-pane-buttons { height: auto; align-horizontal: right; padding-bottom: 1; }
    """
    BINDINGS: ClassVar = [Binding("escape", "close", "close")]

    def __init__(self, pane: Pane) -> None:
        super().__init__()
        self.pane = pane

    def compose(self) -> ComposeResult:
        escape = str(getattr(self.app, "escape_key", "f12"))
        with Vertical():
            header = Text()
            header.append(self.pane.where, style="bold")
            header.append(f"  {self.pane.project_name} · {self.pane.command}", style="dim")
            header.append(
                f"\n{escape.upper()} leaves the pane; Escape or Close shuts this. The window "
                "stays as it is.",
                style="dim",
            )
            yield Static(header, id="tmux-pane-header")
            yield TerminalPane(None, escape_key=escape, label=self.pane.where, id="tmux-pane-view")
            with Horizontal(classes="tmux-pane-buttons"):
                yield Button("Close", id="tmux-pane-close")

    def on_mount(self) -> None:
        view = self.query_one("#tmux-pane-view", TerminalPane)
        view.server = pane_server()
        view.attach(self.pane.pane_id)
        view.focus()

    def on_button_pressed(self, event: Button.Pressed) -> None:
        event.stop()
        self.action_close()

    def on_escape_to_sidebar(self, event: EscapeToSidebar) -> None:
        """The pane's escape key: over the page there is no sidebar to go to, so Close takes
        the keyboard, and Escape then shuts this. Left to the shell, the sidebar behind this
        screen took it, and every key, Escape too, still went into the window (the tester's C
        on #565)."""
        event.stop()
        self.query_one("#tmux-pane-close", Button).focus()

    def action_close(self) -> None:
        self.query_one("#tmux-pane-view", TerminalPane).attach(None)
        self.dismiss(None)


class TmuxView(VerticalScroll):
    """Settings ▸ tmux: a title, a line, the windows' table, the acts, and the other servers'
    fold."""

    DEFAULT_CSS = """
    TmuxView { padding: 1 2; }
    TmuxView #tmux-title { height: 1; text-style: bold; margin-bottom: 1; }
    TmuxView #tmux-about { height: auto; color: $text-muted; margin-bottom: 1; }
    TmuxView #tmux-status { height: auto; color: $text-muted; margin-bottom: 1; }
    TmuxView #tmux-unavailable { height: auto; }
    TmuxView #tmux-panes { height: auto; max-height: 16; }
    TmuxView #tmux-actions { height: auto; margin-top: 1; }
    TmuxView .tmux-act { margin: 0 1 0 0; min-width: 10; }
    TmuxView #tmux-hint { height: auto; color: $text-muted; margin-top: 1; }
    TmuxView #tmux-others { margin-top: 1; }
    TmuxView #tmux-others-about { height: auto; color: $text-muted; }
    TmuxView #tmux-others-table { height: auto; }
    """

    BINDINGS: ClassVar = [
        Binding("o", "open", "open", show=False),
        Binding("s", "stop", "stop", show=False),
        Binding("a", "adopt", "adopt", show=False),
        Binding("r", "read_now", "refresh", show=False),
    ]

    def __init__(self, *, reader: Reader | None = None, id: str | None = None) -> None:
        super().__init__(id=id)
        self._reader = reader or default_reader()
        self.report: TmuxReport | None = None
        """The newest read painted."""
        self._timer: Timer | None = None
        self._acting: Pane | None = None
        """The pane the act running is on (its plan's, while an adopt is asked about)."""
        self.focus_on_show = False
        """Put the keyboard on the table at the next Show (:func:`show_page` sets it)."""

    def compose(self) -> ComposeResult:
        yield Static(Text(TITLE), id="tmux-title")
        yield Static(Text(ABOUT), id="tmux-about")
        yield Static(Text("Reading rlx's tmux server…"), id="tmux-status")
        yield Static("", id="tmux-unavailable")
        yield WindowTable(id="tmux-panes", cursor_type="row", zebra_stripes=True)
        with Horizontal(id="tmux-actions"):
            yield Button("Open", id="tmux-open", classes="tmux-act", compact=True)
            yield Button("Stop…", id="tmux-stop", classes="tmux-act", compact=True, variant="error")
            yield Button("Adopt…", id="tmux-adopt", classes="tmux-act", compact=True)
        yield Static("", id="tmux-hint")
        with Collapsible(title=OTHERS_TITLE, collapsed=True, id="tmux-others"):
            yield Static(Text(OTHERS_ABOUT), id="tmux-others-about")
            yield Static("", id="tmux-others-table")

    def on_mount(self) -> None:
        self.query_one(DataTable).add_columns(*COLUMNS)
        self.query_one("#tmux-unavailable").display = False
        self.query_one("#tmux-others").display = False
        self._paint_acts()

    # --- reading: only while the page shows ---------------------------------------------

    @property
    def reading(self) -> bool:
        """Whether the page's timer runs: it does while the page shows, and only then."""
        return self._timer is not None

    def on_show(self) -> None:
        if self._timer is None:
            self._timer = self.set_interval(REFRESH_SECONDS, self.tick)
        self.tick()
        if self.focus_on_show:
            self.focus_on_show = False
            self.call_after_refresh(self.focus_rows)

    def focus_rows(self) -> None:
        """The keyboard to the table of windows."""
        self.query_one(DataTable).focus()

    def on_hide(self) -> None:
        self._stop_timer()

    def on_unmount(self) -> None:
        self._stop_timer()

    def _stop_timer(self) -> None:
        if self._timer is not None:
            self._timer.stop()
            self._timer = None

    def _worker_runs(self, name: str) -> bool:
        return any(w.name == name and not w.is_finished for w in self.workers)

    @property
    def busy(self) -> bool:
        """Whether a read is running."""
        return self._worker_runs(READ_WORKER)

    @property
    def acting(self) -> bool:
        """Whether a stop or an adopt (or an adopt's first look) is running."""
        return self._worker_runs(ACT_WORKER)

    @while_running
    def tick(self) -> None:
        """Read now, off the UI thread, unless a read still runs: the next tick asks again."""
        if self.busy:
            return
        self.run_worker(
            self._reader, name=READ_WORKER, group=READ_WORKER, thread=True, exit_on_error=False
        )

    def action_read_now(self) -> None:
        self.tick()

    @on(Worker.StateChanged)
    def _worker_changed(self, event: Worker.StateChanged) -> None:
        worker = event.worker
        if worker.name == READ_WORKER:
            self._read_done(worker, event.state)
        elif worker.name == ACT_WORKER:
            self._act_done(worker, event.state)

    def _read_done(self, worker: Worker[Any], state: WorkerState) -> None:
        if state is WorkerState.ERROR:
            self.query_one("#tmux-status", Static).update(
                Text(f"Could not read rlx's tmux server: {worker.error}", style="red")
            )
            return
        if state is WorkerState.SUCCESS and isinstance(worker.result, TmuxReport):
            self.report = worker.result
            self.paint()

    # --- painting ------------------------------------------------------------------------

    @property
    def panes(self) -> list[Pane]:
        return self.report.panes if self.report is not None else []

    @property
    def highlighted(self) -> Pane | None:
        """The pane of the table's highlighted row."""
        table = self.query_one(DataTable)
        panes = self.panes
        if not panes or table.row_count == 0:
            return None
        row = table.cursor_row
        return panes[row] if 0 <= row < len(panes) else None

    def paint(self) -> None:
        """Draw :attr:`report`; the highlight stays on its pane while that pane is listed."""
        report = self.report
        if report is None:
            return
        status = self.query_one("#tmux-status", Static)
        unavailable = self.query_one("#tmux-unavailable", Static)
        table = self.query_one(DataTable)
        if report.unavailable is not None:
            status.display = False
            unavailable.update(Text(report.unavailable))
            unavailable.display = True
            for selector in ("#tmux-about", "#tmux-actions", "#tmux-hint", "#tmux-others"):
                self.query_one(selector).display = False
            table.display = False
            return
        unavailable.display = False
        at = report.read_at.astimezone().strftime("%H:%M:%S")
        if report.error is not None:
            status.update(Text(f"{report.error} · at {at}", style="red"))
        else:
            orphans = len(report.orphans)
            count = len(report.panes)
            status.update(
                Text(
                    f"{count} window{'s' if count != 1 else ''} on rlx's tmux server · "
                    f"{orphans} with no agent · at {at}, every {REFRESH_SECONDS:g} s while open"
                )
            )
        kept = self.highlighted.pane_id if self.highlighted is not None else None
        table.clear()
        for pane in report.panes:
            table.add_row(*self._cells(pane, report), key=pane.pane_id)
        at_row = next((i for i, p in enumerate(report.panes) if p.pane_id == kept), 0)
        if report.panes:
            table.move_cursor(row=at_row)
        table.display = True
        if not report.panes and report.error is None:
            status.update(Text(f"{EMPTY} · at {at}"))
        self._paint_others(report)
        self._paint_acts()

    @staticmethod
    def _cells(pane: Pane, report: TmuxReport) -> list[Text]:
        agent = Text(pane.label, style="" if pane.agent is not None else "yellow")
        state = Text(pane.state, style="" if not pane.dead else "dim")
        return [
            Text(pane.project_name),
            agent,
            Text(pane.command),
            Text(age_text(pane, report)),
            state,
        ]

    def _paint_others(self, report: TmuxReport) -> None:
        fold = self.query_one("#tmux-others", Collapsible)
        if not report.other_panes and not any(o.error for o in report.others):
            fold.display = False
            return
        servers = len(report.others)
        fold.title = (
            f"{OTHERS_TITLE} · {report.other_panes} pane{'s' if report.other_panes != 1 else ''}"
            f" on {servers} server{'s' if servers != 1 else ''}"
        )
        grid = Table(box=None, pad_edge=False, show_edge=False)
        for column in ("Server", "Session", "Window", "Command", "State"):
            grid.add_column(column, style="dim" if column == "Server" else "")
        for other in report.others:
            if other.error is not None:
                grid.add_row(other.name, Text(f"could not be read: {other.error}", style="red"))
                continue
            for each in other.panes:
                state = (
                    "alive"
                    if not each.dead
                    else ("dead" if each.dead_status is None else f"dead (exit {each.dead_status})")
                )
                grid.add_row(
                    other.name,
                    each.session,
                    f"{each.window_index}:{each.window_name}",
                    each.current_command or "—",
                    state,
                )
        self.query_one("#tmux-others-table", Static).update(grid)
        fold.display = True

    def _paint_acts(self) -> None:
        offered = offers(self.highlighted)
        busy = self.acting
        for act, on_offer in offered.items():
            self.query_one(f"#tmux-{act}", Button).disabled = busy or not on_offer
        self.query_one("#tmux-hint", Static).update(Text(self._hint(self.highlighted)))

    @staticmethod
    def _hint(pane: Pane | None) -> str:
        if pane is None:
            return ""
        return f"{pane.where}. {TmuxView._offer(pane)}"

    @staticmethod
    def _offer(pane: Pane) -> str:
        if pane.agent is not None and not pane.dead:
            return f"{pane.label}'s pane: Open shows its agent view, where it is stopped."
        if pane.held is not None and not pane.dead:
            return (
                f"{pane.held.label}'s fleet row names this pane, but "
                f"{pane.held_why or 'its launch record does not place it'}: left as it is."
            )
        if pane.adoptable:
            return f"{pane.command} with no agent: Adopt resumes its conversation in rlx."
        if pane.stoppable:
            return "Stop closes this window." + (" Open shows it." if not pane.dead else "")
        return "Open shows this window."

    @on(DataTable.RowHighlighted, "#tmux-panes")
    def _row_highlighted(self, event: DataTable.RowHighlighted) -> None:
        self._paint_acts()

    @on(WindowTable.OpenAsked)
    def _open_asked(self, event: WindowTable.OpenAsked) -> None:
        event.stop()
        self.action_open()

    @on(Button.Pressed, ".tmux-act")
    def _act_pressed(self, event: Button.Pressed) -> None:
        event.stop()
        act = (event.button.id or "").removeprefix("tmux-")
        {"open": self.action_open, "stop": self.action_stop, "adopt": self.action_adopt}[act]()

    # --- the acts ------------------------------------------------------------------------

    def action_open(self) -> None:
        """An agent's pane: its agent view, while the app lists the agent. Any other window
        (an ended agent's no longer listed too): a terminal pane over the page."""
        pane = self.highlighted
        if pane is None:
            return
        agent = pane.agent
        snapshot = getattr(self.app, "snapshot", None)
        listed = (
            agent is not None
            and snapshot is not None
            and snapshot.agent(agent.project_id, agent.id) is not None
        )
        if agent is not None and listed:
            self.post_message(AgentSelected(agent.project_id, agent.id))
            return
        self.app.push_screen(PaneScreen(pane), lambda _closed: self.focus_rows())

    def action_stop(self) -> None:
        pane = self.highlighted
        if pane is None or not pane.stoppable or self.acting:
            return

        def answered(yes: bool | None) -> None:
            self.focus_rows()  # back to the table, answered either way (the tester's E)
            if yes:
                self._act(pane, lambda: tmux_panes.stop(pane))

        self.app.push_screen(ConfirmPaneScreen(stop_question(pane), "Close", danger=True), answered)

    def action_adopt(self) -> None:
        """Look first (off the UI thread): what it would adopt, into which project; then the
        confirm that says so; then the adopt."""
        pane = self.highlighted
        if pane is None or not pane.adoptable or self.acting:
            return
        self._act(pane, lambda: tmux_panes.plan_adopt(pane))

    def _ask_to_adopt(self, pane: Pane, plan: adopt_service.PanePlan) -> None:
        def answered(yes: bool | None) -> None:
            self.focus_rows()
            if yes:
                self._act(pane, lambda: tmux_panes.adopt(pane, plan))

        self.app.push_screen(ConfirmPaneScreen(adopt_question(pane, plan), "Adopt"), answered)

    def _act(self, pane: Pane, work: Callable[[], object]) -> None:
        self._acting = pane
        self.run_worker(work, name=ACT_WORKER, group=ACT_WORKER, thread=True, exit_on_error=False)
        self._paint_acts()

    def _act_done(self, worker: Worker[Any], state: WorkerState) -> None:
        if state not in (WorkerState.SUCCESS, WorkerState.ERROR, WorkerState.CANCELLED):
            return
        pane, self._acting = self._acting, None
        self._paint_acts()
        if state is WorkerState.ERROR:
            self.notify(str(worker.error), severity="warning", timeout=10, markup=False)
        elif state is WorkerState.SUCCESS:
            result = worker.result
            if isinstance(result, adopt_service.PanePlan) and pane is not None:
                self._ask_to_adopt(pane, result)
                return
            if isinstance(result, tmux_panes.AdoptReceipt):
                self.notify(
                    f"adopted into {_project(result.plan)} as {result.spawned.agent.label}"
                    + (": its old window was closed" if result.closed else ""),
                    timeout=8,
                    markup=False,
                )
                refresh = getattr(self.app, "refresh_data", None)
                if callable(refresh):
                    refresh()
            elif isinstance(result, str):
                self.notify(result, timeout=6, markup=False)
        self.tick()
