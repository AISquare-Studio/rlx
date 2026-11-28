"""Settings ▸ tmux: every window on rlx's own tmux server, whose each one is, and the two things
that can be done with one nobody's (handoff E; the owner's decisions, Q2).

**rlx's server** is the fleet's private one (``tmux -L asq``, ``[fleet] tmux_socket``). Every
window on it is listed, an agent's or not: a project's session is ``asq-<codename>``, and rlx's
own sessions of no project are the sign-in windows (``asq-accounts``), the persona editor's
(``asq-persona-edit``) and ``aisquare watch --detach``'s (``asq-watch``). A row's project is
its session's codename; the agent is the fleet row whose LAUNCH RECORD places it in that pane
(``fleet.resolve_place``), on this server (``PlacedPane.outlived``: a server numbers its panes
from ``%0`` again after a restart), never the window's name, which anything can rename. A
window no record places is nobody's: an orphan.

**Two acts, each on an orphan only**, each asked again of tmux and the records at the moment
it acts (a read on screen is seconds old):

- :func:`stop` closes a dead window (the bundled config's ``remain-on-exit`` keeps an exited
  agent's), or a live one no row names. Never a row's live pane: that is its agent's Stop.
  An agent's own exited window is stopped through the fleet, so its row ends ``stopped``.
- :func:`adopt` brings a live Claude Code or Codex that runs in an orphan window into the
  fleet: ``fleet adopt`` resumes a conversation, not a pane, and refuses one a live process
  holds, so the agent in the pane is quit at its own exit command first and its conversation
  then resumed in an rlx window. Every refusal of adopt's is asked before anything is quit
  (``adopt.plan_pane``). Never a plain shell, which holds no conversation. The pane it leaves
  dead is closed alone, never the window around it.

**The servers rlx did not start** (the user's own ``default``, another tool's: every other
socket in the user's ``tmux-<uid>`` folder) are read, never written: ``core.tmux.ForeignServer``
has no method but its ``list-panes``.

Windows has no tmux, and the page says so (:data:`NO_TMUX_ON_WINDOWS`).
"""

from __future__ import annotations

import contextlib
import os
import sys
import time
from collections.abc import Callable, Collection, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from pathlib import Path

from aisquare.core import process_table, procs
from aisquare.core import tmux as tmux_core
from aisquare.core.agent_adapters import adapters, claude_screen, get_adapter, recorded_adapter
from aisquare.core.agent_adapters.types import (
    AgentAdapter,
    executable_name,
    process_executable,
    screen_of,
)
from aisquare.core.config import FleetSettings
from aisquare.core.store import store_session
from aisquare.core.tmux import ForeignPane, ForeignServer, TmuxError, TmuxServer, TmuxUnavailable
from aisquare.models import FleetAgent, ProjectInfo
from aisquare.services import adopt as adopt_service
from aisquare.services import delivery
from aisquare.services import fleet as fleet_service
from aisquare.services import project as project_service
from aisquare.services.captain import screen as pane_screen
from aisquare.services.claude_accounts import SIGN_IN_SESSION
from aisquare.services.watch import DETACHED_SESSION

PERSONA_EDITOR_SESSION = "asq-persona-edit"
"""The persona editor's session (``cli.ui.persona_dialogs.EDITOR_SESSION``; a test keeps the two
one): named here, as a service imports no view."""

PURPOSES: Mapping[str, str] = {
    SIGN_IN_SESSION: "sign-in",
    PERSONA_EDITOR_SESSION: "persona editor",
    DETACHED_SESSION: "watch",
}
"""rlx's own sessions of no project, and what each one's windows are for."""

NO_AGENT = "no agent"
SHELL = "shell"
SHELLS = frozenset({"sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "ash", "nu"})

NO_TMUX_ON_WINDOWS = (
    "rlx's agents run in tmux, and Windows has no tmux, so there are no panes to show here."
)
NO_TMUX = "tmux is not installed on this computer, so there are no panes to show here."

QUIT_WAIT = 20.0
"""How long :func:`adopt` waits, after typing its agent's exit command, for the agent to quit."""
_POLL = 0.2
_KEY_GAP = 0.2
"""Between the exit command and its Enter, as the fleet types one (``fleet._KEY_GAP``: Codex
keeps an Enter that arrives in the same burst as text)."""

_sleep = time.sleep
_monotonic = time.monotonic
_alive = procs.alive
_EPOCH = datetime(1970, 1, 1, tzinfo=UTC)


class PaneRefused(fleet_service.FleetError):
    """The page will not stop or adopt this window (any more): the message says why."""


# --- processes: what runs in a pane, and since when -------------------------------------------


@dataclass(frozen=True)
class Process:
    """One process as ``ps`` lists it."""

    pid: int
    parent: int
    elapsed: int | None
    """Seconds since it started (``etime``); ``None`` when ``ps`` did not say."""
    args: str


def processes() -> list[Process]:
    """Every process this user can see, with its arguments and how long it has run; none where
    ``ps`` cannot run (Windows has none to ask, and no tmux). Two reads of the seams already
    ruled for it (``core.spawn.SEAMS``): ``core.process_table`` (pid, parent, ``etime``, the one
    Agent resources samples) and ``adopt.process_args`` (pid, parent, arguments), joined by
    pid; a process in one read and not the other (it started or ended between them) has no
    age, or no arguments."""
    listed = adopt_service.process_args()
    table = process_table.read()
    ages = {pid: proc.elapsed for pid, proc in table.procs.items()} if table is not None else {}
    return [Process(pid, parent, ages.get(pid), args) for pid, parent, args in listed]


def _tree(pid: int | None, listed: Sequence[Process]) -> list[Process]:
    """``pid``'s process and every one below it, the pane's own first."""
    if pid is None:
        return []
    children: dict[int, list[Process]] = {}
    by_pid: dict[int, Process] = {}
    for each in listed:
        children.setdefault(each.parent, []).append(each)
        by_pid[each.pid] = each
    found: list[Process] = [by_pid[pid]] if pid in by_pid else []
    seen = {pid}
    queue = [pid]
    while queue:
        for child in children.get(queue.pop(0), []):
            if child.pid not in seen:
                seen.add(child.pid)
                found.append(child)
                queue.append(child.pid)
    return found


def resumes(agent_id: str | None) -> bool:
    """Whether a conversation of that agent's is one ``fleet adopt`` resumes: it finds one by
    the agent's transcript (``AgentAdapter.transcript_reader``, as ``adopt.plan_pane`` reads
    it), so an agent rlx reads no transcript of has none to adopt. The adapter's to say, not a
    list of names here."""
    if agent_id is None:
        return False
    try:
        return get_adapter(agent_id).transcript_reader is not None
    except ValueError:  # no agent rlx knows
        return False


def _agent_in(tree: Sequence[Process], current: str) -> AgentAdapter | None:
    """The coding agent that runs in a pane, in its foreground: none while a shell is (its
    ``pane_current_command``); else a process of its tree whose executable is the agent's
    binary (``types.process_executable``: an npm shim's script, never a word further
    on, so a shell whose arguments mention ``claude`` runs none), else the pane's foreground
    command when it is one's name. ``None`` for a shell or any other program."""
    if executable_name(current) in SHELLS:
        # A shell in the foreground: a claude beneath it is stopped (Ctrl-Z) or in the
        # background, and whatever is typed goes to the shell (the tester's D on #565).
        return None
    known = {executable_name(each.binary): each for each in adapters()}
    for each in tree:
        image = process_executable(each.args.split())
        if image is not None and (adapter := known.get(executable_name(image))) is not None:
            return adapter
    return known.get(executable_name(current)) if current else None


# --- the listing ------------------------------------------------------------------------------


@dataclass(frozen=True)
class Pane:
    """One window (its pane) on rlx's tmux server, and whose it is."""

    session: str
    window_id: str
    window_name: str
    """The window's name as tmux has it: shown, never read for who the pane is."""
    pane_id: str
    server_started: datetime | None
    """When the server it is on started: its pane ids name its panes only."""
    project: ProjectInfo | None
    """The project whose codename its session is named for (``asq-<codename>``)."""
    purpose: str | None
    """For a window of one of rlx's own sessions of no project (:data:`PURPOSES`)."""
    agent: FleetAgent | None
    """The fleet row whose launch record places it here (``fleet.resolve_place``)."""
    command: str
    """``claude``, ``codex``, ``shell``, or the program's own name."""
    runs: str | None
    """The id of the coding agent that runs in it (``claude-code``, ``codex``), live only."""
    started: datetime | None
    dead: bool
    dead_status: int | None
    pid: int | None
    tree: tuple[int, ...] = ()
    """The pane's process and those below it: where its agent's conversation is held."""
    shares_window: bool = False
    """Another pane of its window is a row's live one (placed, or :attr:`held`): closing the
    window would end it."""
    siblings: tuple[str, ...] = ()
    """The other panes of its window, which a Stop closes with it."""
    held: FleetAgent | None = None
    """A live fleet row whose own columns name this pane on rlx's socket while its launch
    record does not place it (``fleet.Unplaced``: unreadable, from a newer rlx, another
    project's, no window place). The row's word places nothing, so it is never acted on;
    it only keeps Stop and Adopt off the pane, as its agent may be running there."""
    held_why: str | None = None
    """Why :attr:`held`'s record does not place it."""

    @property
    def label(self) -> str:
        if self.agent is not None:
            return self.agent.label
        if self.held is not None:
            return f"{self.held.label} (unplaced)"
        return NO_AGENT

    @property
    def orphan(self) -> bool:
        """No row names it: no launch record places one here, and no live row's own columns
        name it either."""
        return self.agent is None and self.held is None

    @property
    def stoppable(self) -> bool:
        """A dead window, or a live one nobody's: Stop is offered. Never a row's live pane,
        placed or not, nor a window that holds one."""
        return (self.dead or self.orphan) and not self.shares_window

    @property
    def adoptable(self) -> bool:
        """A live coding agent nobody's, whose conversation ``fleet adopt`` resumes
        (:func:`resumes`: Claude Code and Codex, the owner's Q2): Adopt is offered. Never a
        shell."""
        return not self.dead and self.orphan and resumes(self.runs)

    @property
    def where(self) -> str:
        """How a confirm names it: its session, its window's name and its pane."""
        return f"{self.session} ▸ {self.window_name or '(unnamed)'} ({self.pane_id})"

    @property
    def project_name(self) -> str:
        if self.project is not None:
            return project_service.display_name(self.project)
        if self.purpose is not None:
            return f"({self.purpose})"
        return "—"

    @property
    def state(self) -> str:
        if not self.dead:
            return "alive"
        return "dead" if self.dead_status is None else f"dead (exit {self.dead_status})"

    def age(self, now: datetime) -> timedelta | None:
        return None if self.started is None else max(now - self.started, timedelta(0))


@dataclass(frozen=True)
class OtherServer:
    """A tmux server in the user's socket folder that rlx did not start, and its panes."""

    socket: Path
    panes: list[ForeignPane] = field(default_factory=list)
    error: str | None = None
    """Why it could not be read, when it could not."""

    @property
    def name(self) -> str:
        return self.socket.name


@dataclass(frozen=True)
class TmuxReport:
    """One read of the page: rlx's windows, and the other servers' panes."""

    read_at: datetime
    panes: list[Pane] = field(default_factory=list)
    others: list[OtherServer] = field(default_factory=list)
    unavailable: str | None = None
    """Why there is no tmux to read (Windows, none installed): the page's one line."""
    error: str | None = None
    """Why rlx's server could not be read this time."""

    @property
    def orphans(self) -> list[Pane]:
        return [pane for pane in self.panes if pane.orphan]

    @property
    def other_panes(self) -> int:
        return sum(len(other.panes) for other in self.others)


def _now() -> datetime:
    return datetime.now(UTC)


def rlx_server(config: FleetSettings | None = None) -> TmuxServer:
    """rlx's own tmux server: the one the fleet runs on (``[fleet] tmux_socket``)."""
    return fleet_service.server(config)


def _same_server(address: fleet_service._Address, srv: TmuxServer, config: FleetSettings) -> bool:
    """Whether a launch record's address is where ``srv`` (today's socket) is asked."""
    if address.path is None:
        return address.name == config.tmux_socket
    try:
        here = srv.socket_path()
    except TmuxUnavailable:
        return False
    return os.path.realpath(address.path) == os.path.realpath(here)


@dataclass(frozen=True)
class _Owners:
    """Who the panes of rlx's server are, as the fleet's rows and records say (:func:`_owners`)."""

    placed: dict[str, FleetAgent]
    """Pane id → the row whose launch record places it there."""
    held: dict[str, tuple[FleetAgent, str]]
    """Pane id → a LIVE row no record places whose own columns name it, and why it is not
    placed: a word that only refuses (:attr:`Pane.held`)."""
    sockets: set[str]
    """Every socket a record names: rlx's, whatever it is called now."""


def _could_be_here(row: FleetAgent, srv: TmuxServer, config: FleetSettings) -> bool:
    """Whether ``row``'s own socket columns could name ``srv``, today's: its socket's name, or
    the path it says its server listened at. Read only to refuse, never to act."""
    if row.tmux_socket == config.tmux_socket:
        return True
    if row.socket_path is None:
        return False
    try:
        here = srv.socket_path()
    except TmuxUnavailable:
        return False
    return os.path.realpath(row.socket_path) == os.path.realpath(here)


def _owners(srv: TmuxServer, started: datetime | None, config: FleetSettings) -> _Owners:
    """Who each pane of ``srv`` (the server that started at ``started``) is (:class:`_Owners`).

    Only a row live, or ended since that server started, can have a window on it: one ended
    before is of an earlier server's. A row no record places (``Unplaced``) names no pane
    here; but while it is LIVE its agent may run in the pane its own columns name, so that
    pane is held: never stopped or adopted on the page's word (review of #565, finding 1).
    """
    since = started or _EPOCH
    with store_session() as store:
        projects = store.fleet_live_counts(since)
        rows = [
            row
            for project_id in projects
            for row in store.fleet_agents(project_id)
            if row.ended_at is None or row.ended_at >= since
        ]
    owners: dict[str, FleetAgent] = {}
    held: dict[str, tuple[FleetAgent, str]] = {}
    sockets: set[str] = set()
    for row in sorted(rows, key=lambda row: row.created_at):
        placement = fleet_service.resolve_place(row, config=config)
        if not isinstance(placement, fleet_service.Placed):
            if row.ended_at is None and _could_be_here(row, srv, config):
                why = getattr(placement, "why", "its launch record does not place it")
                held[row.pane_id] = (row, why)
            continue
        address = placement.pane.address
        sockets.add(address.name if address.path is None else str(address.path))
        if not _same_server(address, srv, config) or placement.pane.outlived(started):
            continue
        owners[placement.pane.pane_id] = placement.agent
    return _Owners(owners, held, sockets)


def _projects_by_codename() -> dict[str, ProjectInfo]:
    with store_session() as store:
        projects = store.list_projects(all=True)
    return {project.codename: project for project in projects if project.codename}


def _command(current: str, runs: AgentAdapter | None, agent: FleetAgent | None) -> str:
    if runs is not None:
        return executable_name(runs.binary)
    if agent is not None and (adapter := recorded_adapter(agent.agent)) is not None:
        return executable_name(adapter.binary)
    name = executable_name(current) if current else ""
    if name in SHELLS:
        return SHELL
    return name or "—"


def read(
    *,
    config: FleetSettings | None = None,
    listed: Callable[[], list[Process]] | None = None,
    folder: Path | None = None,
    other: Callable[[Path], ForeignServer] | None = None,
    others: bool = True,
) -> TmuxReport:
    """Every window on rlx's tmux server and whose it is, and the panes of every other server
    in the user's socket folder (``folder``, the one tmux keeps rlx's socket in unless given).

    ``listed`` lists the processes (:func:`processes`) and ``other`` makes the handle on a
    server rlx did not start: tests hand fakes. ``others=False`` reads rlx's server alone (an
    act asking again). Raises nothing tmux says: a server that could not be asked is the
    report's :attr:`~TmuxReport.error`."""
    now = _now()
    if sys.platform == "win32":
        return TmuxReport(read_at=now, unavailable=NO_TMUX_ON_WINDOWS)
    config = config or fleet_service.settings()
    srv = rlx_server(config)
    if not srv.available():
        return TmuxReport(read_at=now, unavailable=NO_TMUX)
    try:
        windows, _output = srv.every_window()
        pids = srv.pane_pids()
    except TmuxError as exc:
        return TmuxReport(read_at=now, error=f"rlx's tmux server could not be read: {exc}")
    if pids is None:
        return TmuxReport(read_at=now, error="rlx's tmux server did not answer")
    found = [window for session in windows.values() for window in session]
    started = next((w.server_started for w in found if w.server_started), None)
    started = started or pids.server_started
    known = _owners(srv, started, config) if found else _Owners({}, {}, set())
    owners, sockets = known.placed, known.sockets
    codenames = _projects_by_codename() if found else {}
    table = (listed or processes)() if any(not window.dead for window in found) else []
    by_pid = {each.pid: each for each in table}
    # A live pane a row names, placed or held, by window: closing its window would end it.
    claimed = set(owners) | set(known.held)
    live_claimed: dict[str, set[str]] = {}
    panes_of: dict[str, list[str]] = {}
    for w in found:
        panes_of.setdefault(w.window_id, []).append(w.pane_id)
        if not w.dead and w.pane_id in claimed:
            live_claimed.setdefault(w.window_id, set()).add(w.pane_id)
    panes: list[Pane] = []
    for window in found:
        agent = owners.get(window.pane_id)
        pid = pids.pids.get(window.pane_id)
        tree = _tree(pid, table) if not window.dead else []
        runs = None if window.dead else _agent_in(tree, window.current_command)
        codename = window.session.removeprefix(fleet_service.SESSION_PREFIX)
        project = None
        if window.session.startswith(fleet_service.SESSION_PREFIX):
            project = codenames.get(codename)
        if project is None and agent is not None:  # a codename since renamed: the row's own
            project = next((p for p in codenames.values() if p.id == agent.project_id), None)
        if agent is not None:
            started_at: datetime | None = agent.created_at
        elif pid is not None and pid in by_pid and by_pid[pid].elapsed is not None:
            started_at = now - timedelta(seconds=by_pid[pid].elapsed or 0)
        else:
            started_at = None
        alongside = tuple(p for p in panes_of[window.window_id] if p != window.pane_id)
        shares = bool(live_claimed.get(window.window_id, set()) - {window.pane_id})
        holder = known.held.get(window.pane_id) if agent is None and not window.dead else None
        panes.append(
            Pane(
                session=window.session,
                window_id=window.window_id,
                window_name=window.name,
                pane_id=window.pane_id,
                server_started=window.server_started or started,
                project=project,
                purpose=PURPOSES.get(window.session),
                agent=agent,
                command=_command(window.current_command, runs, agent),
                runs=runs.id if runs is not None else None,
                started=started_at,
                dead=window.dead,
                dead_status=window.dead_status,
                pid=pid,
                tree=tuple(each.pid for each in tree),
                shares_window=shares,
                siblings=alongside,
                held=holder[0] if holder is not None else None,
                held_why=holder[1] if holder is not None else None,
            )
        )
    servers = _others(config, sockets, folder, other) if others else []
    return TmuxReport(read_at=now, panes=panes, others=servers)


def _others(
    config: FleetSettings,
    named: Collection[str],
    folder: Path | None,
    other: Callable[[Path], ForeignServer] | None,
) -> list[OtherServer]:
    """Every other server in the user's socket folder that has panes: read with one
    ``list-panes`` each (``ForeignServer``), never written. rlx's own sockets are none of them:
    today's, its throwaway conf check's, and every one a launch record names. A socket file
    no server listens behind (tmux leaves its file) has none, and is left out."""
    folder = folder if folder is not None else tmux_core.socket_folder(config.tmux_socket)
    mine = {
        config.tmux_socket,
        f"{config.tmux_socket}{tmux_core.CHECK_SOCKET_SUFFIX}",
        *named,
    }
    make = other or ForeignServer
    servers: list[OtherServer] = []
    for path in tmux_core.sockets_in(folder):
        if path.name in mine or str(path) in mine:
            continue
        try:
            panes = make(path).panes()
        except TmuxError as exc:
            servers.append(OtherServer(path, [], str(exc)))
            continue
        if panes:
            servers.append(OtherServer(path, panes))
    return servers


# --- the acts: on an orphan only, asked again as they act --------------------------------------


def _again(
    pane: Pane, config: FleetSettings, listed: Callable[[], list[Process]] | None
) -> tuple[Pane, TmuxServer]:
    """``pane`` as tmux and the records say it is NOW, on the server it was read on; and that
    server. :class:`PaneRefused` when it is gone, or the server has restarted since (its id
    names another pane now)."""
    report = read(config=config, listed=listed, others=False)
    if report.unavailable or report.error:
        raise PaneRefused(report.unavailable or report.error or "tmux could not be read")
    now = next((each for each in report.panes if each.pane_id == pane.pane_id), None)
    if now is None:
        raise PaneRefused(f"{pane.where} is gone")
    if pane.server_started is not None and now.server_started not in (None, pane.server_started):
        raise PaneRefused(f"{pane.where} is gone: rlx's tmux server has restarted since")
    return now, rlx_server(config)


def stop(
    pane: Pane,
    *,
    config: FleetSettings | None = None,
    listed: Callable[[], list[Process]] | None = None,
) -> str:
    """Close ``pane``'s window, a dead one or a live one nobody's; what was closed. Never a
    row's live pane, nor a window another of whose panes is one (:class:`PaneRefused`).

    An agent's own exited window is stopped through the fleet (:func:`_stop_through_the_fleet`),
    never closed alone: closed alone, its row stayed live with no pane, and the sidebar read
    it as lost until a reap (the validator's F9)."""
    config = config or fleet_service.settings()
    now, srv = _again(pane, config, listed)
    if not now.stoppable:
        raise PaneRefused(_kept(now))
    said = f"closed {now.where}"
    if now.agent is not None:
        said, left = _stop_through_the_fleet(now, config, listed)
        if left is None:
            return said
        now = left
    srv.kill_window(now.pane_id)
    return said


def _stop_through_the_fleet(
    pane: Pane, config: FleetSettings, listed: Callable[[], list[Process]] | None
) -> tuple[str, Pane | None]:
    """Stop the agent whose exited window ``pane`` is (:attr:`Pane.agent`, placed by its launch
    record) as its agent view's Stop does: ``fleet.stop`` pinned to that row, on the owner's
    word. Its row ends ``stopped`` (``ended_by``), the claims it held go back to the board and
    its exit is announced, and the fleet's stop closes the window. What to say, and the pane
    :func:`stop` is still to close, if any.

    Should the fleet's stop leave the window standing (it can mark an ended row stopped
    with no kill, where its own listing finds no window of the row's), it is handed back for
    :func:`stop` to close, once tmux still lists it dead and that row's: asked again of tmux
    and the records (:func:`_again`), so a pane gone meanwhile, or a server restarted since,
    is left alone."""
    agent = pane.agent
    assert agent is not None  # the caller's
    with store_session() as store:
        project = store.get_project(agent.project_id, include_forgotten=True)
    if project is None:
        raise PaneRefused(f"{pane.where} is {agent.label}'s, whose project rlx no longer has")
    fleet_service.stop(project, agent.label, agent_id=agent.id, actor="user")
    stopped = f"stopped {agent.label}: closed {pane.where}"
    try:
        after, _srv = _again(pane, config, listed)
    except PaneRefused:
        return stopped, None  # gone with the fleet's stop
    same = after.agent is not None and after.agent.id == agent.id
    if not (after.dead and same and not after.shares_window):
        return f"stopped {agent.label}; {pane.where} is left as it is: {_kept(after)}", None
    return stopped, after


def plan_adopt(
    pane: Pane,
    *,
    config: FleetSettings | None = None,
    listed: Callable[[], list[Process]] | None = None,
) -> adopt_service.PanePlan:
    """What adopting ``pane``'s agent would do (``adopt.plan_pane``), asked before the confirm:
    its conversation and the project it goes into. :class:`PaneRefused` when it is no live
    Claude Code or Codex nobody's; ``adopt.AdoptRefused`` for every refusal of adopt's."""
    config = config or fleet_service.settings()
    now, _srv = _again(pane, config, listed)
    _require_adoptable(now)
    return adopt_service.plan_pane(now.tree)


def _kept(pane: Pane) -> str:
    """Why Stop leaves ``pane`` alone."""
    if pane.agent is not None:
        return f"{pane.where} is {pane.label}'s, alive: stop it from its agent view, not here"
    if pane.held is not None:
        return (
            f"{pane.where} may be {pane.held.label}'s, alive: its fleet row names this pane, "
            f"but {pane.held_why or 'its launch record does not place it'}, so it is left as "
            "it is"
        )
    return f"{pane.where} shares its window with an agent's pane: it is left as it is"


def _require_adoptable(pane: Pane) -> None:
    if pane.adoptable:
        return
    if pane.held is not None and not pane.dead:
        raise PaneRefused(_kept(pane))
    if pane.agent is not None:
        raise PaneRefused(f"{pane.where} is {pane.label}'s already: it is in the fleet")
    if pane.dead:
        raise PaneRefused(f"{pane.where} has exited: there is no agent in it to adopt")
    raise PaneRefused(
        f"{pane.where} runs {pane.command}, not Claude Code or Codex: only an agent's "
        "conversation can be adopted"
    )


@dataclass(frozen=True)
class AdoptReceipt:
    """What :func:`adopt` did."""

    spawned: fleet_service.SpawnReceipt
    plan: adopt_service.PanePlan
    closed: bool
    """Whether the pane it quit was closed: dead once its agent quit (the agent was the pane's
    own process), and nobody's. That pane alone (``kill-pane``): its window goes with it only
    when it held no other. One whose shell runs on is left open."""
    kept: tuple[str, ...] = ()
    """The other panes its window held as it acted, each left as it was."""


def adopt(
    pane: Pane,
    plan: adopt_service.PanePlan,
    *,
    role: str = "coder",
    config: FleetSettings | None = None,
    listed: Callable[[], list[Process]] | None = None,
) -> AdoptReceipt:
    """Adopt ``pane``'s agent as ``plan`` (the one the confirm showed) says: quit it at its
    exit command, wait for it to let its conversation go, and resume that conversation in an
    rlx window (``adopt.adopt``), as ``fleet adopt`` does.

    Everything is asked again first: the pane still a live agent nobody's, holding the same
    conversation, with every refusal of adopt's still clear (``adopt.plan_pane``), and at its
    empty prompt, where the exit command and its Enter answer nothing else (a dialog's
    default, a draft the command would join). :class:`PaneRefused` or ``adopt.AdoptRefused``
    otherwise, with nothing typed."""
    config = config or fleet_service.settings()
    now, srv = _again(pane, config, listed)
    _require_adoptable(now)
    fresh = adopt_service.plan_pane(now.tree)
    if fresh.transcript != plan.transcript:
        raise PaneRefused(
            f"{now.where} holds another conversation now ({fresh.conversation.session_id}): "
            "nothing was quit"
        )
    _refuse_unless_at_its_prompt(now, fresh, srv)
    srv.send_literal(now.pane_id, fresh.exit_command)
    _sleep(_KEY_GAP)
    srv.send_keys(now.pane_id, "Enter")
    deadline = _monotonic() + QUIT_WAIT
    while _alive(fresh.holder):
        if _monotonic() >= deadline:
            raise PaneRefused(
                f"{fresh.agent_label} in {now.where} did not quit within {QUIT_WAIT:g} s of "
                f"its {fresh.exit_command}: nothing was adopted, and it is left running"
            )
        _sleep(_POLL)
    spawned = adopt_service.adopt(str(fresh.transcript), role=role)
    return AdoptReceipt(spawned, fresh, _close_if_dead(now, srv), now.siblings)


def _refuse_unless_at_its_prompt(pane: Pane, plan: adopt_service.PanePlan, srv: TmuxServer) -> None:
    """:class:`PaneRefused` unless the pane's agent sits at its empty prompt, with nothing drawn
    over it: its screen reads ready (``AgentScreen.ready``: its empty prompt, no turn running,
    no draft in its composer) and shows nothing a typed line must not go into
    (``AgentScreen.modal``: a dialog of its own, a permission chooser, a question). Typed
    anywhere else, the exit command and its Enter answer for the human: an Enter at Claude
    Code's permission chooser picks "1. Yes" (review of #565, finding 2), and at Codex's
    ``Switch to …?`` its cheaper model. An agent whose screen reads neither, and a screen
    tmux will not give, are refused too: nothing is typed on a guess. Here it is the human's
    to finish (``fleet._exit_withheld`` kills the window instead)."""
    adapter = get_adapter(plan.conversation.agent)
    reader = screen_of(adapter)
    if reader.ready_on_screen is None and not reader.reads_empty_prompt:
        raise PaneRefused(
            f"{adapter.label}'s screen is not one rlx can read for its prompt, so nothing is "
            f"typed into {pane.where}: quit it there, then adopt its conversation"
        )
    try:
        lines = list(srv.capture(pane.pane_id).lines)  # with its escapes: a dim placeholder
    except TmuxError as exc:
        raise PaneRefused(f"{pane.where}'s screen could not be read: {exc}") from exc
    screen = pane_screen.strip_escapes(lines)
    asking = reader.modal(screen)
    if asking is not None:
        raise PaneRefused(
            f"{pane.where} shows {adapter.label} asking: {asking}. Open it, answer there, "
            "then adopt"
        )
    if not reader.ready(screen):
        raise PaneRefused(
            f"{pane.where} shows {adapter.label} busy, not at its empty prompt: open it, "
            "finish there, then adopt"
        )
    # A draft: Claude Code's input box holding text (its dim placeholder is none), which its
    # readiness reads past; or someone typing into the pane in asq just now. As the fleet's
    # own typing asks (``fleet`` before a delivery, ``delivery.draft_guard``).
    if claude_screen.composer_text(lines) or delivery.human_typing(
        srv, pane.pane_id, quiet=fleet_service.settings().draft_quiet_seconds
    ):
        raise PaneRefused(
            f"{pane.where} shows a draft in {adapter.label}'s input: open it, send or clear "
            "it there, then adopt"
        )


def _close_if_dead(pane: Pane, srv: TmuxServer) -> bool:
    """Close ``pane`` once it is dead on the server it was read on, its agent having quit: an
    orphan's dead pane, which Stop would offer next. The pane alone (``kill-pane``), never its
    window: a window split beside it may hold a live agent's pane or a human's editor, and
    ``kill-window`` would end them with it (the round-3 tester of #565). A window of no other
    pane closes with it, as tmux closes one left empty. Fails open: a pane that could not be
    closed is left as it is."""
    with contextlib.suppress(TmuxError):
        facts = srv.pane_facts_or_raise(pane.pane_id)
        if facts is None or not facts.dead:
            return False
        if pane.server_started is not None and facts.server_started not in (
            None,
            pane.server_started,
        ):
            return False
        srv.kill_pane(pane.pane_id)
        return True
    return False
