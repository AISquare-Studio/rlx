"""The Agent view's header names the task the agent is on from the board its ROW reads (#14, M2).

``views.agent.agent_task`` read ``store.team_tasks(agent.project_id)``, the fleet project's own
board. An agent's sessions claim on the board its spawn recorded (``FleetAgent.board``,
``fleet.board_of_row``), and that is another board in three cases: under a hub
(``AISQUARE_TEAM_HUB``), for a project rooted in a sub-folder of its repository, and for one
rooted at a linked worktree (both read their principal checkout's board). In all three the
header named no task (review of #579, #1133, major).

And the listing gave those rows no session (round 2's tester, #1306): ``fleet._statuses`` read
the sessions of the fleet project's own board, while the agent's hook registers its session on
the row's board, so a CLAIMED task, matched by session, still showed nothing. These pins run
the whole path: a real spawn (the fleet suite's fake tmux), the session registered by the
REAL ``hook_session_start`` in the window's environment, the claim by ``claim_task``, the
status as ``fleet.list_agents`` lists it, and the REAL ``agent_task``; for a task the session
claimed and for the task the agent was spawned for.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

from aisquare.cli.ui.views.agent import agent_task, task_words
from aisquare.core import orchestrator
from aisquare.core.store import store_session
from aisquare.core.workspace import project_id_for
from aisquare.models import FleetAgent, FleetAgentStatus, ProjectInfo
from aisquare.services import fleet as fleet_service
from aisquare.services import team as team_service
from tests import test_fleet_service as fleet_suite
from tests.test_fleet_service import FakeTmux, _add_task, _git

# The fleet suite's fixtures, bound here so pytest finds them for this module's tests.
tmux = fleet_suite.tmux
claude_on_path = fleet_suite.claude_on_path
repo = fleet_suite.repo
project = fleet_suite.project

WHERE = ["hub", "sub-folder", "linked-worktree"]


def _fleet_project(where: str, project: ProjectInfo, tmp_path: Path) -> ProjectInfo:
    """The fleet project the agent is spawned into, ``where`` its board is not its own."""
    if where == "sub-folder":
        return fleet_suite._project_below(project)
    if where == "linked-worktree":
        tree = tmp_path / "linked"
        _git("worktree", "add", "-q", "-b", "side", str(tree), cwd=project.root)
        with store_session() as store:
            return store.onboard_project(ProjectInfo(id=project_id_for(tree), root=tree))
    return project


def _board_of_a_spawn(spawned_in: ProjectInfo) -> ProjectInfo:
    """The board a spawn into ``spawned_in`` gives its row, in the store: the hub's under
    ``AISQUARE_TEAM_HUB`` (set by the test), else the principal checkout's."""
    board = orchestrator.team_project(spawned_in.root)
    with store_session() as store:
        # Added on purpose, as a hub is (`team on` in it): with `[projects] capture` off, the
        # default, a hook registers nothing on a board nobody added.
        return store.onboard_project(board)


def _session_started(
    agent: FleetAgent, monkeypatch: pytest.MonkeyPatch, *, hub: Path | None = None
) -> None:
    """What the agent's SessionStart hook does in its window: the REAL ``hook_session_start``,
    in the window's environment (its role, its row, and the hub when it was given one), which
    registers the session on the board the window resolves: the row's."""
    assert agent.session_id is not None
    with monkeypatch.context() as window:
        window.setenv("AISQUARE_ROLE", agent.role)
        window.setenv("AISQUARE_FLEET_AGENT", agent.id)
        if hub is not None:
            window.setenv("AISQUARE_TEAM_HUB", str(hub))
        team_service.hook_session_start(agent.session_id, agent.cwd, "startup")


def _listed(spawned_in: ProjectInfo, agent: FleetAgent) -> FleetAgentStatus:
    """The row as ``fleet ls`` and the fleet UI list it (``list_agents``)."""
    (status,) = [s for s in fleet_service.list_agents(spawned_in) if s.agent.id == agent.id]
    return status


@pytest.mark.parametrize("where", WHERE)
@pytest.mark.parametrize("how", ["claimed", "spawned-for"])
def test_the_header_names_the_task_on_the_board_the_row_reads(
    tmux: FakeTmux,
    claude_on_path: Path,
    project: ProjectInfo,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    where: str,
    how: str,
) -> None:
    spawned_in = _fleet_project(where, project, tmp_path)
    hub = None
    if where == "hub":
        hub = tmp_path / "hub"
        hub.mkdir()
        monkeypatch.setenv("AISQUARE_TEAM_HUB", str(hub))
    board = _board_of_a_spawn(spawned_in)
    task = _add_task(board, "Agent view: move a limited agent")
    decoy = _add_task(spawned_in, "a task of the fleet project's own board")
    if how == "spawned-for":
        agent = fleet_suite._coder(spawned_in, task_id=task.id)
    else:
        agent = fleet_suite._coder(spawned_in)
    monkeypatch.delenv("AISQUARE_TEAM_HUB", raising=False)
    _session_started(agent, monkeypatch, hub=hub)
    with store_session() as store:
        registered = store.get_session(agent.session_id or "")
    assert registered is not None and registered.project_id == board.id, "on the row's board"
    if how == "claimed":
        assert agent.session_id is not None
        with store_session() as store:
            assert store.claim_task(
                decoy.id, agent.session_id, datetime.now(tz=UTC) + timedelta(hours=1)
            )
            assert store.claim_task(
                task.id, agent.session_id, datetime.now(tz=UTC) + timedelta(hours=1)
            )
    status = _listed(spawned_in, agent)
    assert fleet_service.board_of_row(status.agent) == board.id != status.agent.project_id, (
        "the case under test: the row's board is not its fleet project's"
    )
    assert status.session is not None and status.session.id == agent.session_id, (
        "the listing gives the row its session, from the row's board"
    )

    shown = agent_task(status)

    assert shown is not None and shown.id == task.id, shown
    assert task_words(shown) == f"#{task.number} Agent view: move a limited agent"


def test_on_its_own_projects_board_the_header_names_the_task_as_before(
    tmux: FakeTmux, claude_on_path: Path, project: ProjectInfo, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The control: a row whose board is its fleet project's own reads that one."""
    task = _add_task(project, "Agent view")
    agent = fleet_suite._coder(project, task_id=task.id)
    _session_started(agent, monkeypatch)
    status = _listed(project, agent)
    assert fleet_service.board_of_row(status.agent) == status.agent.project_id == project.id

    shown = agent_task(status)

    assert shown is not None and shown.id == task.id
