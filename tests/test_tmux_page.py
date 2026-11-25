"""Settings ▸ tmux (handoff E; the owner's decisions, Q2): every window on rlx's own tmux
server and whose each one is; Stop and Adopt, on a window no agent owns; and the servers rlx
did not start, under a fold, read and never written.

Driven as the fleet's tests drive the fleet: the real store in an isolated home, a real spawn's
row and launch record, and the fleet suite's in-memory ``FakeTmux`` (:class:`PagedTmux` adds the
one listing the page asks that it lacks, every pane's pid). What ``ps`` lists is the test's. The
page runs in a Textual app with that same fake behind it; the adopt service is spied on.
"""

from __future__ import annotations

import asyncio
import os
import socket
import sys
from collections.abc import Awaitable, Callable, Iterator, Sequence
from dataclasses import dataclass, replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any, TypeVar

import pytest
from textual.app import App, ComposeResult
from textual.geometry import Region
from textual.pilot import Pilot
from textual.widgets import Button, Collapsible, ContentSwitcher, DataTable, Static

from aisquare.cli.ui import persona_dialogs
from aisquare.cli.ui.sidebar import AgentSelected
from aisquare.cli.ui.terminal import TerminalPane
from aisquare.cli.ui.views import tmux_page
from aisquare.cli.ui.views.tmux_page import (
    ACT_WORKER,
    ConfirmPaneScreen,
    PaneScreen,
    TmuxView,
)
from aisquare.core import claude_accounts as accounts_core
from aisquare.core import tmux as tmux_core
from aisquare.core.tmux import Completed, ForeignServer, PanePids, TmuxError, WindowInfo
from aisquare.models import AdoptableConversation, FleetAgent, ProjectInfo
from aisquare.services import adopt as adopt_service
from aisquare.services import fleet as fleet_service
from aisquare.services import project as project_service
from aisquare.services import tmux_panes
from aisquare.services.claude_accounts import SIGN_IN_SESSION
from aisquare.services.tmux_panes import Pane, PaneRefused, Process, TmuxReport
from tests import pane_harness
from tests import test_fleet_service as fleet_suite
from tests.captain_screens import INPUT_BOX, MARK, REAL_CHOOSER, REAL_IDLE, RULE, TOP_RULE
from tests.test_fleet_adopt import SESSION, _transcript
from tests.test_fleet_service import FakeTmux, _codename, _facts
from tests.ui_workers import settle_page, settle_until

# The fleet suite's fixtures, bound here so pytest finds them for this module's tests.
claude_on_path = fleet_suite.claude_on_path
repo = fleet_suite.repo
project = fleet_suite.project

T = TypeVar("T")
SIZE = (140, 40)
CODEX = "/opt/codex/vendor/aarch64-apple-darwin/codex/codex"


class PagedTmux(FakeTmux):
    """The fleet suite's fake, with every pane's pid in one listing (``pane_pids``), and
    windows no row names: what a hand-run ``tmux -L asq new-window`` leaves. Its kills are
    tmux's own: ``kill-window`` ends every pane of the window (the suite's fake drops the one
    pane), ``kill-pane`` that pane alone; ``kills`` says which was asked of which pane."""

    def __init__(self) -> None:
        super().__init__()
        self.kills: list[tuple[str, str]] = []

    def _window_of(self, pane_id: str) -> list[str]:
        """Every pane of the window holding ``pane_id``, ``pane_id`` first."""
        window = next(w for ws in self.sessions.values() for w in ws if w.pane_id == pane_id)
        others = [
            w.pane_id
            for w in self.sessions[window.session]
            if w.window_id == window.window_id and w.pane_id != pane_id
        ]
        return [pane_id, *others]

    def _kill(self, verb: str, pane_id: str, panes: Callable[[str], list[str]]) -> None:
        self.binary()
        self._require_server()
        if self.refuse_kills:
            raise TmuxError(f"kill-{verb} failed (fake)")
        if pane_id not in self.facts:
            raise TmuxError(f"can't find pane: {pane_id}")
        self.kills.append((verb, pane_id))
        for each in panes(pane_id):
            self.vanish(each)
            self.killed.append(each)
        self._forget_empty_sessions()
        if not self.sessions:
            self.running = False

    def kill_window(self, pane_id: str) -> None:
        self._kill("window", pane_id, self._window_of)

    def kill_pane(self, pane_id: str) -> None:
        self._kill("pane", pane_id, lambda pane: [pane])

    def pane_pids(self) -> PanePids | None:
        if not self._read():
            return PanePids({})
        pids: dict[str, int | None] = {}
        for windows in self.sessions.values():
            for window in windows:
                facts = self.facts.get(window.pane_id)
                dead = facts is None or facts.dead
                pids[window.pane_id] = None if dead else self.pids.get(window.pane_id)
        return PanePids(pids, self.started)

    def window(self, session: str, name: str, command: str, *, pid: int | None = None) -> str:
        """A window of no row's in ``session``, running ``command`` (its foreground name)."""
        self._counter += 1
        pane_id = f"%{self._counter}"
        if session not in self.session_ids:
            self.session_ids[session] = f"${self._sessions_made}"
            self._sessions_made += 1
        self.sessions.setdefault(session, []).append(
            WindowInfo(
                session=session,
                window_id=f"@{self._counter}",
                name=name,
                pane_id=pane_id,
                dead=False,
                dead_status=None,
                current_command=command,
                activity=False,
                session_id=self.session_ids[session],
            )
        )
        self.facts[pane_id] = _facts(pane_id, current_command=command)
        if pid is not None:
            self.pids[pane_id] = pid
        return pane_id

    def split(self, of: str, command: str, *, pid: int | None = None) -> str:
        """A second pane in ``of``'s window, as ``split-window`` makes one."""
        window = next(w for ws in self.sessions.values() for w in ws if w.pane_id == of)
        self._counter += 1
        pane_id = f"%{self._counter}"
        self.sessions[window.session].append(
            replace(window, pane_id=pane_id, current_command=command, name=window.name)
        )
        self.facts[pane_id] = _facts(pane_id, current_command=command)
        if pid is not None:
            self.pids[pane_id] = pid
        return pane_id


ON_WINDOWS = sys.platform == "win32"
NO_TMUX_HERE = (
    "Windows has no tmux: the page reads none there (test_windows_has_no_tmux_and_says_so), "
    "so a fake tmux is never asked"
)


@pytest.fixture
def tmux(monkeypatch: pytest.MonkeyPatch) -> PagedTmux:
    """The fake every socket answers with, as the fleet suite's ``tmux`` fixture wires it.
    Skipped on Windows, where :func:`tmux_panes.read` asks no tmux at all."""
    if ON_WINDOWS:
        pytest.skip(NO_TMUX_HERE)
    fake = PagedTmux()
    monkeypatch.setattr(fleet_service, "server", lambda config=None: fake)
    monkeypatch.setattr(tmux_core, "desktop_environment", lambda environ=None: {})
    monkeypatch.setattr(fleet_service, "_started_since", lambda session_id, launched: True)
    monkeypatch.setattr(tmux_panes, "_sleep", lambda seconds: None)
    return fake


@pytest.fixture
def ps(monkeypatch: pytest.MonkeyPatch) -> list[Process]:
    """What ``ps`` lists: the processes a test says run, none else."""
    running: list[Process] = []
    monkeypatch.setattr(tmux_panes, "processes", lambda: list(running))
    return running


@dataclass
class World:
    """rlx's server as each test meets it: one agent, and five windows no row names."""

    project: ProjectInfo
    agent: FleetAgent
    agent_pane: str
    dead: str
    shell: str
    claude: str
    codex: str
    renamed: str
    """A shell whose window was renamed to the agent's label."""


@pytest.fixture
def world(tmux: PagedTmux, claude_on_path: Path, project: ProjectInfo, ps: list[Process]) -> World:
    agent = fleet_service.spawn(project, "coder", worktree=False).agent
    tmux.set_command(agent.pane_id, "claude")
    tmux.pids[agent.pane_id] = 6000
    ps.append(Process(6000, 1, 30, "claude --session-id x"))
    session = fleet_service.session_name(_codename(project))
    dead = tmux.window(session, "old-run", "sh")
    tmux.die(dead, 3)
    shell = tmux.window(session, "scratch", "zsh", pid=7100)
    ps.append(Process(7100, 1, 75, "-zsh"))
    # Claude Code started from a shell, by hand: its native binary names itself by its version.
    claude = tmux.window(session, "by-hand", "2.1.300", pid=7200)
    ps += [Process(7200, 1, 600, "-zsh"), Process(7201, 7200, 590, "claude --model opus")]
    tmux.screens[claude] = list(REAL_IDLE)  # at its empty prompt: what Adopt types into
    codex = tmux.window(session, "codex-run", "codex", pid=7300)
    ps.append(Process(7300, 1, 40, f"{CODEX} --yolo"))
    renamed = tmux.window(session, agent.label, "zsh", pid=7400)
    ps.append(Process(7400, 1, 20, "-zsh"))
    return World(project, agent, agent.pane_id, dead, shell, claude, codex, renamed)


def _by_pane(report: TmuxReport) -> dict[str, Pane]:
    return {pane.pane_id: pane for pane in report.panes}


# --- the rows -------------------------------------------------------------------------------


def test_each_window_reads_its_project_its_agent_or_none_its_command_age_and_state(
    world: World,
) -> None:
    report = tmux_panes.read()
    panes = _by_pane(report)
    name = project_service.display_name(world.project)

    assert list(panes) == [
        world.agent_pane,
        world.dead,
        world.shell,
        world.claude,
        world.codex,
        world.renamed,
    ], "every window on the server, an agent's or not"
    assert {pane.project_name for pane in report.panes} == {name}, "the session's codename"
    seen = {
        pane_id: (pane.label, pane.command, pane.state)
        for pane_id, pane in panes.items()
    }  # fmt: skip
    assert seen == {
        world.agent_pane: (world.agent.label, "claude", "alive"),
        world.dead: ("no agent", "shell", "dead (exit 3)"),
        world.shell: ("no agent", "shell", "alive"),
        world.claude: ("no agent", "claude", "alive"),
        world.codex: ("no agent", "codex", "alive"),
        world.renamed: ("no agent", "shell", "alive"),
    }
    assert panes[world.renamed].window_name == world.agent.label, (
        "named for the agent, and still nobody's: the launch record says whose a pane is"
    )
    assert panes[world.agent_pane].agent is not None
    assert panes[world.agent_pane].agent.id == world.agent.id  # type: ignore[union-attr]
    assert tmux_page.age_text(panes[world.shell], report) == "1m", "75 s by ps"
    assert tmux_page.age_text(panes[world.claude], report) == "10m"
    assert tmux_page.age_text(panes[world.dead], report) == "—", "a dead pane has no process"
    assert panes[world.agent_pane].age(report.read_at) is not None, "the row's own start"
    assert panes[world.claude].tree == (7200, 7201)


def test_a_window_of_rlxs_own_sessions_says_what_it_is_for(
    tmux: PagedTmux, ps: list[Process]
) -> None:
    sign_in = tmux.window(SIGN_IN_SESSION, "account-3", "claude", pid=8100)
    ps.append(Process(8100, 1, 5, "claude"))
    editor = tmux.window(tmux_panes.PERSONA_EDITOR_SESSION, "persona-x", "vim", pid=8200)
    ps.append(Process(8200, 1, 5, "vim draft.md"))

    panes = _by_pane(tmux_panes.read())

    assert panes[sign_in].project_name == "(sign-in)"
    assert panes[editor].project_name == "(persona editor)" and panes[editor].command == "vim"
    assert tmux_panes.PERSONA_EDITOR_SESSION == persona_dialogs.EDITOR_SESSION


def test_no_tmux_says_so_plainly(tmux: PagedTmux, monkeypatch: pytest.MonkeyPatch) -> None:
    tmux.installed = False
    assert tmux_panes.read().unavailable == tmux_panes.NO_TMUX
    monkeypatch.setattr(sys, "platform", "win32")
    assert tmux_panes.read().unavailable == tmux_panes.NO_TMUX_ON_WINDOWS
    assert "tmux" in tmux_panes.NO_TMUX_ON_WINDOWS and "`" not in tmux_panes.NO_TMUX_ON_WINDOWS


def test_windows_has_no_tmux_and_says_so(monkeypatch: pytest.MonkeyPatch) -> None:
    """With no fake tmux, so it runs on Windows itself (where every test above is skipped):
    the page's read says there is no tmux, in plain words, and asks nothing of tmux."""
    if not ON_WINDOWS:
        monkeypatch.setattr(sys, "platform", "win32")

    def asked(*args: object, **kwargs: object) -> object:
        raise AssertionError("tmux was asked on Windows")

    monkeypatch.setattr(tmux_panes, "rlx_server", asked)
    monkeypatch.setattr(fleet_service, "settings", asked)
    report = tmux_panes.read()
    assert report.unavailable == tmux_panes.NO_TMUX_ON_WINDOWS
    assert report.panes == [] and report.others == [] and report.error is None
    assert "`" not in tmux_panes.NO_TMUX_ON_WINDOWS


# --- Stop: a dead window, or a live one nobody's ----------------------------------------------


def test_stop_is_offered_for_a_dead_window_and_a_live_orphan_and_never_an_agents_live_pane(
    world: World, tmux: PagedTmux
) -> None:
    panes = _by_pane(tmux_panes.read())

    stoppable = {pane_id for pane_id, pane in panes.items() if pane.stoppable}
    assert stoppable == {world.dead, world.shell, world.claude, world.codex, world.renamed}
    assert not tmux_page.offers(panes[world.agent_pane])["stop"]
    with pytest.raises(PaneRefused, match="stop it from its agent view"):
        tmux_panes.stop(panes[world.agent_pane])
    assert tmux.killed == [], "an agent's live pane is its own Stop's"


def test_stop_refuses_a_window_that_is_an_agents_by_the_time_it_acts(
    world: World, tmux: PagedTmux
) -> None:
    """Asked again as it acts: the read on screen is seconds old."""
    shell = _by_pane(tmux_panes.read())[world.shell]
    tmux.vanish(world.shell)
    with pytest.raises(PaneRefused, match="is gone"):
        tmux_panes.stop(shell)
    assert tmux.killed == []


# --- Adopt: a live Claude Code or Codex nobody's ---------------------------------------------


def test_adopt_is_offered_for_the_orphan_claude_and_codex_only_never_a_shell(
    world: World,
) -> None:
    panes = _by_pane(tmux_panes.read())

    adoptable = {pane_id for pane_id, pane in panes.items() if pane.adoptable}
    assert adoptable == {world.claude, world.codex}
    assert panes[world.claude].runs == "claude-code" and panes[world.codex].runs == "codex"
    with pytest.raises(PaneRefused, match="not Claude Code or Codex"):
        tmux_panes.plan_adopt(panes[world.shell])
    with pytest.raises(PaneRefused, match="in the fleet"):
        tmux_panes.plan_adopt(panes[world.agent_pane])


def test_the_plan_finds_the_conversation_the_panes_claude_holds_before_anything_is_quit(
    fake_home: Path, tmux: PagedTmux, claude_on_path: Path, project: ProjectInfo, tmp_path: Path
) -> None:
    """Claude Code's registry names the session the pane's process runs; every refusal of
    adopt's is asked then, before anything is quit: here, a folder no project holds."""
    slot = accounts_core.create_account()
    path = _transcript(slot, SESSION, project.root)
    me = os.getpid()  # a live pid, standing for the pane's claude
    registry = slot.config_dir / "sessions" / f"{me}.json"
    registry.parent.mkdir(parents=True)
    registry.write_text(f'{{"pid": {me}, "sessionId": "{SESSION}"}}', encoding="utf-8")

    plan = adopt_service.plan_pane({me}, processes=[])

    assert plan.conversation.session_id == SESSION and plan.transcript == path
    assert plan.project.id == project.id and plan.holder == me
    assert (plan.agent_label, plan.exit_command) == ("Claude Code", "/exit")
    with pytest.raises(adopt_service.AdoptRefused) as none:
        adopt_service.plan_pane({me + 1}, processes=[])
    assert none.value.code == "no_conversation"
    elsewhere = tmp_path / "not-a-project"
    elsewhere.mkdir()
    _transcript(slot, SESSION, elsewhere)
    with pytest.raises(adopt_service.AdoptRefused) as refused:
        adopt_service.plan_pane({me}, processes=[])
    assert refused.value.code == "not_onboarded"
    assert tmux.spawned == [], "nothing adopted, nothing quit"


def _plan(world: World, pane: str) -> adopt_service.PanePlan:
    return adopt_service.PanePlan(
        conversation=AdoptableConversation(
            session_id=SESSION,
            agent="claude-code",
            transcript_path=f"/home/me/.claude/projects/x/{SESSION}.jsonl",
            slot=1,
            account="main",
            cwd=str(world.project.root),
            project_id=world.project.id,
            last_activity=datetime(2026, 11, 25, 9, 0, tzinfo=UTC),
            first_prompt="fix the login bug",
            held_by=7201,
        ),
        project=world.project,
        transcript=Path(f"/home/me/.claude/projects/x/{SESSION}.jsonl"),
        holder=7201,
        agent_label="Claude Code",
        exit_command="/exit",
    )


@dataclass
class AdoptSpy:
    planned: list[tuple[int, ...]]
    adopted: list[tuple[str, str]]


@pytest.fixture
def adopt_spy(world: World, tmux: PagedTmux, monkeypatch: pytest.MonkeyPatch) -> AdoptSpy:
    """The adopt service, spied on: its plan for the claude pane, and the adopt itself. Its
    holder is alive until ``/exit`` was typed into that pane."""
    spy = AdoptSpy([], [])

    def plan_pane(pids: Sequence[int], *, processes: object = None) -> adopt_service.PanePlan:
        spy.planned.append(tuple(pids))
        return _plan(world, world.claude)

    def adopt(ref: str, *, role: str = "coder", label: str | None = None) -> Any:
        spy.adopted.append((ref, role))
        adopted = world.agent.model_copy(
            update={"id": "agt_adopted", "label": "coder-2", "session_id": SESSION}
        )
        return fleet_service.SpawnReceipt(agent=adopted, asked_label=None, tmux_session="x")

    def alive(pid: int) -> bool:  # until /exit was typed, into whichever pane it adopts
        return not any(entry[1:] == ("literal", "/exit") for entry in tmux.typed)

    monkeypatch.setattr(adopt_service, "plan_pane", plan_pane)
    monkeypatch.setattr(adopt_service, "adopt", adopt)
    monkeypatch.setattr(tmux_panes, "_alive", alive)
    return spy


def test_adopt_quits_the_agent_in_its_pane_then_resumes_its_conversation(
    world: World, tmux: PagedTmux, adopt_spy: AdoptSpy
) -> None:
    claude = _by_pane(tmux_panes.read())[world.claude]
    plan = tmux_panes.plan_adopt(claude)

    receipt = tmux_panes.adopt(claude, plan)

    assert adopt_spy.planned == [(7200, 7201), (7200, 7201)], "planned, and asked again"
    typed = [entry for entry in tmux.typed if entry[0] == world.claude]
    assert typed == [(world.claude, "literal", "/exit"), (world.claude, "key", "Enter")]
    assert adopt_spy.adopted == [(str(plan.transcript), "coder")], "by its transcript"
    assert receipt.closed and tmux.kills == [("pane", world.claude)], "its dead pane, alone"
    assert tmux.killed == [world.claude] and receipt.kept == ()
    assert all(entry[0] == world.claude for entry in tmux.typed), "no other pane typed into"


def test_adopt_refuses_when_the_pane_holds_another_conversation_now_and_types_nothing(
    world: World, tmux: PagedTmux, adopt_spy: AdoptSpy
) -> None:
    claude = _by_pane(tmux_panes.read())[world.claude]
    shown = replace(_plan(world, world.claude), transcript=Path("/elsewhere/other.jsonl"))
    with pytest.raises(PaneRefused, match="another conversation"):
        tmux_panes.adopt(claude, shown)
    assert tmux.typed == [] and adopt_spy.adopted == [] and tmux.killed == []


def test_an_agent_that_does_not_quit_is_not_adopted(
    world: World, tmux: PagedTmux, adopt_spy: AdoptSpy, monkeypatch: pytest.MonkeyPatch
) -> None:
    clock = fleet_suite.FakeClock()
    monkeypatch.setattr(tmux_panes, "_sleep", clock.sleep)
    monkeypatch.setattr(tmux_panes, "_monotonic", clock.monotonic)
    monkeypatch.setattr(tmux_panes, "_alive", lambda pid: True)
    claude = _by_pane(tmux_panes.read())[world.claude]
    with pytest.raises(PaneRefused, match="did not quit"):
        tmux_panes.adopt(claude, tmux_panes.plan_adopt(claude))
    assert adopt_spy.adopted == [] and tmux.killed == []


# --- the servers rlx did not start ------------------------------------------------------------


@pytest.fixture
def their_socket() -> Iterator[Path]:
    """A tmux server's socket file in a folder of the user's: a unix socket, listening (short
    path, under the test's private ``TMUX_TMPDIR``: an AF_UNIX path is capped near 100 bytes)."""
    if sys.platform == "win32":
        pytest.skip("no unix sockets for tmux on Windows")
    folder = Path(os.environ["TMUX_TMPDIR"]) / "theirs"
    folder.mkdir()
    listening = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listening.bind(str(folder / "default"))
    listening.listen()
    for mine in ("asq", "asq-check"):  # rlx's own, by today's config: never listed
        own = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        own.bind(str(folder / mine))
        own.close()
    (folder / "notes.txt").write_text("no socket", encoding="utf-8")
    try:
        yield folder / "default"
    finally:
        listening.close()


class Logged:
    """A ``Runner`` standing for the user's own tmux server: one session, two windows; every
    command it is handed is logged."""

    def __init__(self) -> None:
        self.log: list[list[str]] = []

    def __call__(self, argv: Sequence[str], stdin: bytes | None) -> Completed:
        self.log.append(list(argv))
        sep = "|~|"
        lines = [
            sep.join(("main", "0", "%0", "0", "", "vim", "notes")),
            sep.join(("main", "1", "%1", "1", "130", "bash", "build")),
        ]
        return Completed(0, "\n".join(lines) + "\n", "")


def _reader(folder: Path | None, runner: Logged | None) -> Callable[[], TmuxReport]:
    def other(path: Path) -> ForeignServer:
        return ForeignServer(path, binary=sys.executable, runner=runner)

    return lambda: tmux_panes.read(folder=folder, other=other)


def test_the_other_servers_are_read_with_list_panes_alone_and_rlxs_own_sockets_are_not_theirs(
    world: World, tmux: PagedTmux, their_socket: Path
) -> None:
    runner = Logged()

    report = _reader(their_socket.parent, runner)()

    [theirs] = report.others
    assert theirs.name == "default" and len(theirs.panes) == 2
    assert [(p.session, p.window_name, p.dead, p.dead_status) for p in theirs.panes] == [
        ("main", "notes", False, None),
        ("main", "build", True, 130),
    ]
    assert runner.log == [[sys.executable, "-S", str(their_socket), *runner.log[0][3:]]]
    assert runner.log[0][3:5] == ["list-panes", "-a"] and "-f" not in runner.log[0]


# --- the page -------------------------------------------------------------------------------


class Host(App[None]):
    """The page as the app holds it: one of the pages of a ``ContentSwitcher``."""

    def __init__(self, reader: Callable[[], TmuxReport], listed: set[str] | None = None):
        super().__init__()
        self.reader = reader
        self.selected: list[tuple[str, str]] = []
        self.snapshot = _Snapshot(listed or set())

    def compose(self) -> ComposeResult:
        with ContentSwitcher(initial="tmux", id="content"):
            yield TmuxView(reader=self.reader, id="tmux")

    def on_agent_selected(self, event: AgentSelected) -> None:
        self.selected.append((event.project_id, event.agent_id))


class _Snapshot:
    """What the app lists: ``agent()`` answers for the agents ids named."""

    def __init__(self, listed: set[str]) -> None:
        self.listed = listed

    def agent(self, project_id: str, agent_id: str) -> object | None:
        return object() if agent_id in self.listed else None


def _drive(
    reader: Callable[[], TmuxReport],
    fn: Callable[[Pilot[None], TmuxView], Awaitable[T]],
    *,
    listed: set[str] | None = None,
    size: tuple[int, int] = SIZE,
) -> T:
    async def run() -> T:
        app = Host(reader, listed)
        async with app.run_test(size=size) as pilot:
            view = app.query_one(TmuxView)
            await settle_until(app, lambda: view.report is not None)
            await settle_page(app)
            return await fn(pilot, view)

    return asyncio.run(run())


def _text(widget: Static) -> str:
    plain = getattr(widget.visual, "plain", None)
    if isinstance(plain, str):
        return plain.rstrip()
    lines = widget.render_lines(Region(0, 0, max(widget.size.width, 1), max(widget.size.height, 1)))
    return "\n".join(line.text.rstrip() for line in lines).rstrip()


async def _highlight(pilot: Pilot[None], view: TmuxView, pane_id: str) -> None:
    table = view.query_one(DataTable)
    row = next(i for i, pane in enumerate(view.panes) if pane.pane_id == pane_id)
    table.focus()
    table.move_cursor(row=row)
    await settle_page(view.app)
    assert view.highlighted is not None and view.highlighted.pane_id == pane_id


def _enabled(view: TmuxView) -> dict[str, bool]:
    return {
        act: not view.query_one(f"#tmux-{act}", Button).disabled for act in tmux_page.offers(None)
    }


def test_the_page_lists_every_window_and_offers_each_rows_acts(world: World) -> None:
    async def go(pilot: Pilot[None], view: TmuxView) -> dict[str, Any]:
        seen: dict[str, Any] = {"rows": view.query_one(DataTable).row_count}
        seen["status"] = _text(view.query_one("#tmux-status", Static))
        for pane_id in (world.agent_pane, world.dead, world.shell, world.claude):
            await _highlight(pilot, view, pane_id)
            seen[pane_id] = _enabled(view)
        seen["fold"] = view.query_one("#tmux-others", Collapsible).display
        return seen

    seen = _drive(tmux_panes.read, go)
    assert seen["rows"] == 6
    assert seen["status"].startswith("6 windows on rlx's tmux server · 5 with no agent")
    assert seen[world.agent_pane] == {"open": True, "stop": False, "adopt": False}
    assert seen[world.dead] == {"open": True, "stop": True, "adopt": False}
    assert seen[world.shell] == {"open": True, "stop": True, "adopt": False}
    assert seen[world.claude] == {"open": True, "stop": True, "adopt": True}
    assert seen["fold"] is False, "no other server: no fold at all"


def test_the_stop_confirm_names_the_window_and_closes_that_window_alone(
    world: World, tmux: PagedTmux
) -> None:
    async def go(pilot: Pilot[None], view: TmuxView) -> tuple[str, list[str]]:
        await _highlight(pilot, view, world.agent_pane)
        await pilot.press("s")
        await settle_page(view.app)
        assert not isinstance(view.app.screen, ConfirmPaneScreen), "an agent's: no Stop"
        await _highlight(pilot, view, world.shell)
        await pilot.press("s")
        await settle_until(view.app, lambda: isinstance(view.app.screen, ConfirmPaneScreen))
        question = _text(view.app.screen.query_one("#tmux-confirm-question", Static))
        await pilot.click("#tmux-confirm-yes")
        await settle_until(view.app, lambda: world.shell in tmux.killed, group=ACT_WORKER)
        await settle_until(view.app, lambda: world.shell not in {p.pane_id for p in view.panes})
        return question, [pane.pane_id for pane in view.panes]

    question, left = _drive(tmux_panes.read, go)
    shell = f"asq-{_codename(world.project)} ▸ scratch ({world.shell})"
    assert shell in question, "the confirm names the window"
    assert tmux.killed == [world.shell], "that window alone"
    assert world.shell not in left and world.agent_pane in left


def test_adopt_asks_first_naming_the_window_and_its_project_then_quits_and_resumes(
    world: World, tmux: PagedTmux, adopt_spy: AdoptSpy
) -> None:
    async def go(pilot: Pilot[None], view: TmuxView) -> str:
        await _highlight(pilot, view, world.shell)
        await pilot.press("a")
        await settle_page(view.app)
        assert adopt_spy.planned == [], "a shell: no Adopt"
        await _highlight(pilot, view, world.claude)
        await pilot.press("a")
        await settle_until(view.app, lambda: isinstance(view.app.screen, ConfirmPaneScreen))
        question = _text(view.app.screen.query_one("#tmux-confirm-question", Static))
        assert tmux.typed == [], "nothing typed before the confirm"
        await pilot.click("#tmux-confirm-yes")
        await settle_until(view.app, lambda: adopt_spy.adopted, group=ACT_WORKER)
        await settle_page(view.app)
        return question

    question = _drive(tmux_panes.read, go)
    assert f"({world.claude})" in question and "by-hand" in question
    assert project_service.display_name(world.project) in question
    assert "fix the login bug" in question and "/exit" in question
    assert (world.claude, "literal", "/exit") in tmux.typed
    assert adopt_spy.adopted == [(str(_plan(world, world.claude).transcript), "coder")]


def test_open_shows_an_agents_view_and_any_other_window_in_a_terminal_pane(
    world: World, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    harness = pane_harness.FakeTmux()
    harness.panes[world.shell] = pane_harness.FakePane(screen=["$ ls", "notes.txt", "$ "])
    monkeypatch.setattr(tmux_page, "pane_server", lambda: harness.server(tmp_path))

    async def go(pilot: Pilot[None], view: TmuxView) -> tuple[list[tuple[str, str]], str | None]:
        host = view.app
        assert isinstance(host, Host)
        await _highlight(pilot, view, world.agent_pane)
        await pilot.press("enter")
        await settle_page(host)
        await _highlight(pilot, view, world.shell)
        await pilot.press("o")
        await settle_until(host, lambda: isinstance(host.screen, PaneScreen))
        shown = host.screen.query_one(TerminalPane).pane_id
        await pilot.press("escape")
        return host.selected, shown

    selected, shown = _drive(tmux_panes.read, go, listed={world.agent.id})
    assert selected == [(world.project.id, world.agent.id)], "its agent view"
    assert shown == world.shell, "the window itself, in a terminal pane"


def test_the_fold_of_the_other_servers_is_there_collapsed_and_reads_list_panes_alone(
    world: World, tmux: PagedTmux, their_socket: Path
) -> None:
    runner = Logged()

    async def go(pilot: Pilot[None], view: TmuxView) -> dict[str, Any]:
        fold = view.query_one("#tmux-others", Collapsible)
        seen = {"shown": fold.display, "collapsed": fold.collapsed, "title": fold.title}
        fold.collapsed = False
        table = view.query_one("#tmux-others-table", Static)
        await settle_until(view.app, lambda: table.size.height > 0)
        seen["table"] = _text(table)
        return seen

    seen = _drive(_reader(their_socket.parent, runner), go)
    assert seen["shown"] is True and seen["collapsed"] is True
    assert seen["title"] == "Not started by rlx · 2 panes on 1 server"
    assert "default" in seen["table"] and "notes" in seen["table"] and "exit 130" in seen["table"]
    assert runner.log and all(
        argv[1:3] == ["-S", str(their_socket)] and argv[3] in {"list-panes", "display-message"}
        for argv in runner.log
    ), "only reads, and only at that socket"
    assert tmux.typed == [] and tmux.killed == []


def test_with_no_tmux_the_page_says_so_and_shows_nothing_else() -> None:
    report = TmuxReport(read_at=datetime.now(UTC), unavailable=tmux_panes.NO_TMUX_ON_WINDOWS)

    async def go(pilot: Pilot[None], view: TmuxView) -> tuple[str, list[bool]]:
        shown = [
            view.query_one(selector).display
            for selector in ("#tmux-panes", "#tmux-actions", "#tmux-others", "#tmux-status")
        ]
        return _text(view.query_one("#tmux-unavailable", Static)), shown

    message, shown = _drive(lambda: report, go)
    assert message == tmux_panes.NO_TMUX_ON_WINDOWS
    assert shown == [False, False, False, False]


def test_the_page_reads_only_while_it_shows(world: World, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(tmux_page, "REFRESH_SECONDS", 0.05)
    calls: list[int] = []

    def counted() -> TmuxReport:
        calls.append(1)
        return tmux_panes.read()

    async def go(pilot: Pilot[None], view: TmuxView) -> tuple[bool, bool, int, int, int]:
        switcher = view.app.query_one(ContentSwitcher)
        await settle_until(view.app, lambda: len(calls) >= 3)
        reading = view.reading
        await switcher.mount(Static("elsewhere", id="elsewhere"))
        switcher.current = "elsewhere"
        await settle_page(view.app)
        hidden_at = len(calls)
        await pilot.pause(0.3)
        hidden_later = len(calls)
        switcher.current = "tmux"
        await settle_until(view.app, lambda: len(calls) > hidden_later)
        return reading, view.reading, hidden_at, hidden_later, len(calls)

    reading, again, hidden_at, hidden_later, shown_again = _drive(counted, go)
    assert reading and again, "the timer runs while the page shows"
    assert hidden_later == hidden_at, "nothing is read while it is hidden"
    assert shown_again > hidden_later


def test_the_hub_shows_the_page_in_the_apps_pages(world: World) -> None:
    """``show_page``: built the first time among the ``#content`` pages, shown again later."""

    class Pages(App[None]):
        def compose(self) -> ComposeResult:
            with ContentSwitcher(initial="hub", id="content"):
                yield Static("hub", id="hub")

    async def run() -> tuple[str | None, bool, str | None, bool, int]:
        app = Pages()
        async with app.run_test(size=SIZE):
            tmux_page.show_page(app, tmux_panes.read)
            switcher = app.query_one(ContentSwitcher)
            await settle_until(app, lambda: isinstance(app.focused, DataTable))
            first, keyboard = switcher.current, isinstance(app.focused, DataTable)
            app.set_focus(None)
            switcher.current = "hub"
            await settle_page(app)
            tmux_page.show_page(app)
            await settle_until(app, lambda: isinstance(app.focused, DataTable))
            again = isinstance(app.focused, DataTable)
            return first, keyboard, switcher.current, again, len(app.query(TmuxView))

    first, keyboard, current, again, pages = asyncio.run(run())
    assert (first, current, pages) == ("tmux", "tmux", 1), "built once, shown again"
    assert keyboard and again, "the keyboard on the table, each time it is shown"


@pytest.mark.parametrize("size", [(80, 24), (120, 30)])
def test_the_page_fits_the_small_and_the_usual_terminal(
    world: World, size: tuple[int, int]
) -> None:
    """At 80x24 and 120x30 the title, the line, the rows and the acts all show."""

    async def go(pilot: Pilot[None], view: TmuxView) -> list[bool]:
        return [
            view.query_one(selector).region.height > 0
            for selector in ("#tmux-title", "#tmux-status", "#tmux-panes", "#tmux-actions")
        ]

    assert all(_drive(tmux_panes.read, go, size=size))


# --- the reviews of #565: what the page must never act on -------------------------------------


def test_a_live_row_no_record_places_keeps_its_pane_from_stop_and_adopt(
    world: World, tmux: PagedTmux, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Review of #565, finding 1: a live row whose launch record cannot place it (unreadable,
    from a newer rlx, another project's) placed nothing, so its running agent read "no agent"
    and Stop killed it. Its own row's word names the pane: never to act on, only to refuse."""
    real = fleet_service.resolve_place

    def unplaced(row: FleetAgent, record: object = None, config: object = None) -> object:
        if row.id == world.agent.id:
            return fleet_service.Unplaced(row, "its launch record could not be read")
        return real(row, record, config)  # type: ignore[arg-type]

    monkeypatch.setattr(fleet_service, "resolve_place", unplaced)
    tmux.set_command(world.agent_pane, "claude")
    pane = _by_pane(tmux_panes.read())[world.agent_pane]

    assert pane.agent is None and pane.held is not None and pane.held.id == world.agent.id
    assert pane.label == f"{world.agent.label} (unplaced)"
    assert not pane.orphan and not pane.stoppable and not pane.adoptable
    with pytest.raises(PaneRefused, match="could not be read"):
        tmux_panes.stop(pane)
    with pytest.raises(PaneRefused, match="could not be read"):
        tmux_panes.plan_adopt(pane)
    assert tmux.killed == [] and tmux.typed == []


@pytest.mark.parametrize(
    ("screen", "refusal"),
    [
        (REAL_CHOOSER, "asking: a numbered choice"),  # Enter would pick "1. Yes"
        ([TOP_RULE, f"{MARK} half a thought", RULE, INPUT_BOX[3]], "a draft"),
        ([], "busy, not at its empty prompt"),  # nothing drawn: no prompt to type at
    ],
    ids=["permission-chooser", "draft", "no-prompt"],
)
def test_adopt_types_nothing_into_a_dialog_a_draft_or_a_screen_with_no_prompt(
    world: World, tmux: PagedTmux, adopt_spy: AdoptSpy, screen: list[str], refusal: str
) -> None:
    """Review of #565, finding 2: Claude Code reads no empty prompt off its screen, so Adopt
    typed /exit and its Enter into whatever it showed. Its screen must read ready, with
    nothing a line must not go into and no draft; anything else is the human's to finish."""
    claude = _by_pane(tmux_panes.read())[world.claude]
    plan = tmux_panes.plan_adopt(claude)
    tmux.screens[world.claude] = list(screen)

    with pytest.raises(PaneRefused, match=refusal):
        tmux_panes.adopt(claude, plan)
    assert tmux.typed == [] and adopt_spy.adopted == [] and tmux.killed == []


def test_a_server_restarted_since_a_row_holds_none_of_its_panes(
    world: World, tmux: PagedTmux
) -> None:
    """A server numbers its panes from %0 again: on one that started after the row, the pane
    id its record names is another pane, nobody's (``PlacedPane.outlived``)."""
    before = _by_pane(tmux_panes.read())[world.agent_pane]
    tmux.started = datetime.now(UTC) + timedelta(minutes=1)
    after = _by_pane(tmux_panes.read())[world.agent_pane]

    assert before.agent is not None and not before.stoppable
    assert after.agent is None and after.held is None and after.label == "no agent"


def test_a_pane_that_shares_its_window_with_an_agents_pane_is_never_stopped(
    world: World, tmux: PagedTmux, ps: list[Process]
) -> None:
    """Stop closes a window, every pane in it: a shell split beside a live agent's pane would
    take the agent with it. Its row's pane lists the split as a sibling."""
    beside = tmux.split(world.agent_pane, "zsh", pid=7500)
    ps.append(Process(7500, 1, 9, "-zsh"))
    panes = _by_pane(tmux_panes.read())

    assert panes[beside].orphan and panes[beside].shares_window and not panes[beside].stoppable
    assert panes[world.agent_pane].siblings == (beside,)
    with pytest.raises(PaneRefused, match="shares its window"):
        tmux_panes.stop(panes[beside])
    assert tmux.killed == []


def test_stops_confirm_names_the_other_panes_its_window_takes_with_it(
    world: World, tmux: PagedTmux
) -> None:
    """Review of #565, nit: a window of two orphan panes closes whole; the confirm says so."""
    beside = tmux.split(world.dead, "sh")
    tmux.die(beside, 0)
    dead = _by_pane(tmux_panes.read())[world.dead]

    said = tmux_page.stop_question(dead).plain
    assert dead.stoppable and dead.siblings == (beside,)
    assert f"other pane ({beside}) closes with it" in said
    alone = tmux_page.stop_question(_by_pane(tmux_panes.read())[world.shell]).plain
    assert "other pane" not in alone, "control: a window of one pane"
    tmux_panes.stop(dead)
    assert tmux.kills == [("window", world.dead)] and tmux.killed == [world.dead, beside]


# --- the tester's note on #565 (board #351) -------------------------------------------------


def test_f12_in_an_opened_window_gives_the_keyboard_to_close_and_escape_then_closes_it(
    world: World, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """C: F12 left the keyboard on the pane, the sidebar behind the screen taking it, so Escape
    went into the window, an orphan Claude's turn interrupted. F12 now gives it to Close, and
    Escape shuts the screen; the keyboard is back on the table, and nothing reached the pane."""
    harness = pane_harness.FakeTmux()
    harness.panes[world.shell] = pane_harness.FakePane(screen=["$ "])
    monkeypatch.setattr(tmux_page, "pane_server", lambda: harness.server(tmp_path))

    async def go(pilot: Pilot[None], view: TmuxView) -> dict[str, Any]:
        host = view.app
        await _highlight(pilot, view, world.shell)
        await pilot.press("o")
        await settle_until(host, lambda: isinstance(host.screen, PaneScreen))
        await settle_until(host, lambda: isinstance(host.focused, TerminalPane))
        await pilot.press("f12")
        await settle_page(host)
        seen: dict[str, Any] = {"after_f12": getattr(host.focused, "id", None)}
        await pilot.press("escape")
        await settle_until(host, lambda: not isinstance(host.screen, PaneScreen))
        seen["closed"] = not isinstance(host.screen, PaneScreen)
        seen["keyboard"] = type(host.focused).__name__
        seen["typed"] = [sent for sent in harness.sent() if "Escape" in sent]
        return seen

    seen = _drive(tmux_panes.read, go)
    assert seen["after_f12"] == "tmux-pane-close"
    assert seen["closed"] and seen["keyboard"] == "WindowTable"
    assert seen["typed"] == [], "no Escape typed into the window"


def test_a_shell_in_the_foreground_is_never_adopted_whatever_runs_beneath_it(
    world: World, tmux: PagedTmux, ps: list[Process]
) -> None:
    """D: a zsh whose claude child is stopped (Ctrl-Z) or in the background read as claude, and
    Adopt typed /exit and Enter into the zsh. A shell in the foreground is a plain shell."""
    pane = tmux.window(f"asq-{_codename(world.project)}", "suspended", "zsh", pid=7600)
    ps += [Process(7600, 1, 80, "-zsh"), Process(7601, 7600, 70, "claude --model opus")]
    read = _by_pane(tmux_panes.read())[pane]

    assert read.command == "shell" and read.runs is None and not read.adoptable
    with pytest.raises(PaneRefused, match="not Claude Code or Codex"):
        tmux_panes.plan_adopt(read)
    assert tmux.typed == []


def test_a_click_highlights_a_row_and_enter_opens_it_and_the_keyboard_comes_back(
    world: World, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """E: a click on the highlighted row opened its window (``DataTable`` selects it); after a
    confirm or the opened window, the keyboard was nowhere and o, s and a reached nothing."""
    harness = pane_harness.FakeTmux()
    harness.panes[world.shell] = pane_harness.FakePane(screen=["$ "])
    monkeypatch.setattr(tmux_page, "pane_server", lambda: harness.server(tmp_path))

    async def go(pilot: Pilot[None], view: TmuxView) -> dict[str, Any]:
        host = view.app
        await _highlight(pilot, view, world.shell)
        table = view.query_one(DataTable)
        row = next(i for i, pane in enumerate(view.panes) if pane.pane_id == world.shell)
        await pilot.click(table, offset=(2, row + 1))  # the header is row 0 on screen
        await settle_page(host)
        seen: dict[str, Any] = {"clicked": type(host.screen).__name__}
        await pilot.press("s")
        await settle_until(host, lambda: isinstance(host.screen, ConfirmPaneScreen))
        await pilot.click("#tmux-confirm-no")
        await settle_until(host, lambda: not isinstance(host.screen, ConfirmPaneScreen))
        seen["after_cancel"] = type(host.focused).__name__
        await pilot.press("enter")
        await settle_until(host, lambda: isinstance(host.screen, PaneScreen))
        seen["enter"] = type(host.screen).__name__
        return seen

    seen = _drive(tmux_panes.read, go)
    assert seen["clicked"] != "PaneScreen", "a click only highlights"
    assert seen["after_cancel"] == "WindowTable", "the keyboard is back on the table"
    assert seen["enter"] == "PaneScreen"


def test_stop_refuses_a_window_that_has_become_an_agents_by_the_time_it_acts(
    world: World, tmux: PagedTmux, monkeypatch: pytest.MonkeyPatch
) -> None:
    """F: read while no record placed it (nobody's, Stop offered), the window is the agent's
    by the time the stop acts: asked again, it is refused and nothing is killed."""
    real = tmux_panes._owners
    monkeypatch.setattr(tmux_panes, "_owners", lambda *a: tmux_panes._Owners({}, {}, set()))
    then = _by_pane(tmux_panes.read())[world.agent_pane]
    monkeypatch.setattr(tmux_panes, "_owners", real)

    assert then.orphan and then.stoppable
    with pytest.raises(PaneRefused, match="stop it from its agent view"):
        tmux_panes.stop(then)
    assert tmux.killed == []


def test_the_hint_names_the_highlighted_window_in_full() -> None:
    """F: at 80x24 a Window column was clipped; the window's name is the hint line's."""
    assert "Window" not in tmux_page.COLUMNS


# --- the round-3 tester of #565 (board #1477) -----------------------------------------------


@pytest.mark.parametrize("neighbour", ["placed", "held", "a human's editor"])
def test_adopt_closes_its_dead_pane_alone_and_never_the_window_it_shares(
    world: World,
    tmux: PagedTmux,
    ps: list[Process],
    adopt_spy: AdoptSpy,
    monkeypatch: pytest.MonkeyPatch,
    neighbour: str,
) -> None:
    """MAJOR 1: an orphan Claude Code split beside a live pane was adopted, and its dead pane's
    ``kill-window`` closed the whole window, the agent beside it (placed, or held by a row
    whose record cannot place it) or a human's editor with it. The dead pane alone is closed
    now, and the confirm names the pane that stays."""
    session = fleet_service.session_name(_codename(world.project))
    if neighbour == "a human's editor":
        beside = tmux.window(session, "notes", "vim", pid=7800)
        ps.append(Process(7800, 1, 300, "vim notes.md"))
    else:
        beside = world.agent_pane
    if neighbour == "held":
        real = fleet_service.resolve_place

        def unplaced(row: FleetAgent, record: object = None, config: object = None) -> object:
            if row.id == world.agent.id:
                return fleet_service.Unplaced(row, "its launch record could not be read")
            return real(row, record, config)  # type: ignore[arg-type]

        monkeypatch.setattr(fleet_service, "resolve_place", unplaced)
    split = tmux.split(beside, "2.1.300", pid=7700)
    ps.append(Process(7700, 1, 60, "claude --model opus"))
    tmux.screens[split] = list(REAL_IDLE)
    panes = _by_pane(tmux_panes.read())
    pane = panes[split]
    assert pane.orphan and pane.adoptable and pane.siblings == (beside,)
    assert pane.shares_window == (neighbour != "a human's editor")
    plan = tmux_panes.plan_adopt(pane)
    said = tmux_page.adopt_question(pane, plan).plain
    assert f"other pane ({beside}) stays open" in said and "window it leaves empty" not in said

    receipt = tmux_panes.adopt(pane, plan)

    assert [entry[0] for entry in tmux.typed] == [split, split], "typed into the split alone"
    assert receipt.closed and receipt.kept == (beside,)
    assert tmux.kills == [("pane", split)] and tmux.killed == [split]
    assert beside in tmux.facts and not tmux.facts[beside].dead, "its neighbour runs on"
    assert beside in _by_pane(tmux_panes.read())


def test_adopts_confirm_of_a_window_of_one_pane_names_no_other(world: World) -> None:
    """Control for the confirm above: a pane alone in its window says its window closes."""
    pane = _by_pane(tmux_panes.read())[world.claude]
    said = tmux_page.adopt_question(pane, _plan(world, world.claude)).plain
    assert pane.siblings == ()
    assert "A window it leaves empty is closed." in said and "other pane" not in said


def test_which_agents_adopt_is_offered_for_is_the_adapters_to_say() -> None:
    """MAJOR 2: a list of two agents' names in the service (test_no_agent_literals); now an
    agent's conversation is adoptable when its adapter reads its transcript, as
    ``adopt.plan_pane`` finds it."""
    from aisquare.core.agent_adapters import adapters

    for adapter in adapters():
        assert tmux_panes.resumes(adapter.id) == (adapter.transcript_reader is not None)
    assert tmux_panes.resumes("claude-code") and tmux_panes.resumes("codex")
    assert not tmux_panes.resumes(None) and not tmux_panes.resumes("no-such-agent")
