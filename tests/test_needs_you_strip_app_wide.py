"""The app-wide "needs you" bar (UX audit M7), in the real shell.

Two projects in the store; each board's read is scripted (``team.board_read``) and the fleet has
no agents to ask tmux for. What needs the person in project A shows over asq while project B's
Settings are open, and F6 opens it on A's board; with nothing, the bar takes no space.
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any, TypeVar

import pytest
from textual.widgets import TabbedContent

from aisquare.cli.ui import needs_you_bar
from aisquare.cli.ui.board import BoardPanel
from aisquare.cli.ui.needs_you_bar import NeedsYouBar
from aisquare.models import ProjectInfo, TaskStatus, TeamEvent, TeamSession, TeamTask
from aisquare.services import fleet as fleet_service
from aisquare.services import team as team_service
from tests.ui_workers import settle_page

T = TypeVar("T")
NOW = datetime.now(UTC).replace(microsecond=0)


def _session(project: ProjectInfo, sid: str, role: str, label: str, **extra: Any) -> TeamSession:
    fields: dict[str, Any] = {
        "started_at": NOW - timedelta(minutes=20),
        "last_seen_at": NOW - timedelta(minutes=1),
    }
    fields.update(extra)
    return TeamSession(id=sid, project_id=project.id, role=role, label=label, **fields)


def _task(project: ProjectInfo, number: int, title: str, status: TaskStatus) -> TeamTask:
    return TeamTask(
        id=f"tsk_{project.id[-4:]}{number:04d}",
        project_id=project.id,
        number=number,
        key=f"task-{number}",
        title=title,
        status=status,
        created_at=NOW - timedelta(hours=1),
        updated_at=NOW - timedelta(minutes=5),
    )


def _event(project: ProjectInfo, seq: int, session_id: str, kind: str, text: str) -> TeamEvent:
    return TeamEvent(
        seq=seq,
        id=f"evt_{project.id[-4:]}{seq}",
        project_id=project.id,
        session_id=session_id,
        kind=kind,
        text=text,
        created_at=NOW - timedelta(minutes=10) + timedelta(seconds=seq),
    )


@pytest.fixture
def projects(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> dict[str, Any]:
    """Alpha and beta, listed in the sidebar; each board's read scripted by ``frames``."""
    from tests.test_ui_spawn import register

    alpha = register(tmp_path / "alpha", project_id="prj_alpha", codename="amber-otter")
    beta = register(tmp_path / "beta", project_id="prj_beta", codename="brisk-heron")
    frames: dict[str, tuple[list[TeamSession], list[TeamTask], list[TeamEvent]]] = {
        alpha.id: ([], [], []),
        beta.id: ([], [], []),
    }

    def board_read(*, project: ProjectInfo | None = None, **_k: Any) -> team_service.BoardRead:
        assert project is not None, "the bar and the board each read the board they show"
        sessions, tasks, events = frames.get(project.id, ([], [], []))
        return team_service.BoardRead(project, sessions, tasks, events, {})

    monkeypatch.setattr(team_service, "board_read", board_read)
    monkeypatch.setattr(fleet_service, "list_agents", lambda project, live_only=True: [])
    return {"alpha": alpha, "beta": beta, "frames": frames}


def drive(size: tuple[int, int], fn: Callable[[Any, Any], Awaitable[T]]) -> T:
    from aisquare.cli.ui.app import FleetApp

    async def run() -> T:
        app = FleetApp(refresh_seconds=3600, doctor=lambda: [], accounts=None)
        async with app.run_test(size=size) as pilot:
            await pilot.pause()
            await settle_page(app)
            return await fn(pilot, app)

    return asyncio.run(run())


async def _until(pilot: Any, holds: Callable[[], bool], *, what: str = "the view") -> None:
    """Pause until ``holds()`` (the bar lands on a project's view once it is built), bounded,
    and fail at the bound: a poll that runs out and returns read a stale screen (#797)."""
    for _ in range(500):  # 10 s at most
        if holds():
            return
        await pilot.pause(0.02)
    pytest.fail(f"never came: {what}")


def _alphas_board(app: Any, alpha: ProjectInfo) -> BoardPanel | None:
    views = app.query(f"#project-{alpha.id}")
    return views.first().query_one(BoardPanel) if views else None


async def _on_betas_settings(pilot: Any, app: Any, beta: ProjectInfo) -> None:
    from aisquare.cli.ui.sidebar import ProjectSelected

    app.post_message(ProjectSelected(beta.id))
    await settle_page(app)
    app.query_one(f"#project-{beta.id}", TabbedContent).active = "tab-settings"
    await settle_page(app)


def test_a_question_in_one_project_shows_over_another_projects_settings(
    projects: dict[str, Any],
) -> None:
    alpha, beta = projects["alpha"], projects["beta"]
    manager = _session(alpha, "ses_alpha_manager", "manager", "manager")
    projects["frames"][alpha.id] = (
        [manager],
        [],
        [_event(alpha, 1, manager.id, "question", "staging or prod first?")],
    )

    async def go(pilot: Any, app: Any) -> dict[str, Any]:
        await _on_betas_settings(pilot, app, beta)
        bar = app.query_one(NeedsYouBar)
        bar.read_now()
        await settle_page(app)
        seen: dict[str, Any] = {
            "height": bar.outer_size.height,
            "line": bar.line().plain,
            "beta_settings": app.query_one(f"#project-{beta.id}", TabbedContent).active,
        }
        await pilot.press(needs_you_bar.OPEN_KEY)
        await _until(
            pilot,
            lambda: (
                app.query(f"#project-{alpha.id}")
                and app.query_one(f"#project-{alpha.id}", TabbedContent).active == "tab-board"
            ),
        )
        await settle_page(app)
        view = app.query_one(f"#project-{alpha.id}", TabbedContent)
        panel = view.query_one(BoardPanel)
        seen["alpha_tab"] = view.active
        seen["on_screen"] = panel.is_on_screen
        seen["lane"] = list(panel.lane)
        return seen

    seen = drive((120, 40), go)
    assert seen["beta_settings"] == "tab-settings", "the premise: beta's Settings are open"
    assert seen["height"] == 1
    assert "needs you: 1" in seen["line"] and "manager asks: staging or prod first?" in seen["line"]
    assert "amber-otter" in seen["line"] or "alpha" in seen["line"], seen["line"]
    assert seen["alpha_tab"] == "tab-board" and seen["on_screen"], "F6: alpha's board, shown"
    assert ("question", "manager") in seen["lane"], "the question is on the board it opened"


def test_the_key_opens_a_blocked_card_in_its_boards_reader(projects: dict[str, Any]) -> None:
    alpha, beta = projects["alpha"], projects["beta"]
    stuck = _task(alpha, 2, "fix the login race", "blocked")
    projects["frames"][alpha.id] = ([], [stuck], [])

    async def go(pilot: Any, app: Any) -> tuple[str, str]:
        await _on_betas_settings(pilot, app, beta)
        bar = app.query_one(NeedsYouBar)
        bar.read_now()
        await settle_page(app)
        line = bar.line().plain
        await pilot.press(needs_you_bar.OPEN_KEY)
        await _until(
            pilot, lambda: bool((board := _alphas_board(app, alpha)) and board.reader_text)
        )
        panel = _alphas_board(app, alpha)
        return line, panel.reader_text if panel is not None else ""

    line, reader = drive((120, 40), go)
    assert "#2 fix the login race — blocked" in line
    assert reader.startswith("fix the login race\n#2 · blocked"), "opened in the board's reader"


def test_with_nothing_needing_you_the_bar_takes_no_space(projects: dict[str, Any]) -> None:
    async def go(pilot: Any, app: Any) -> tuple[int, bool]:
        bar = app.query_one(NeedsYouBar)
        bar.read_now()
        await settle_page(app)
        return bar.outer_size.height, bar.display

    height, shown = drive((120, 40), go)
    assert (height, shown) == (0, False)


def test_a_limited_agent_whose_hand_over_failed_needs_you(projects: dict[str, Any]) -> None:
    """Not on the board's lane (``board.needs_you`` has no such kind): the bar adds it."""
    alpha = projects["alpha"]
    coder = _session(alpha, "ses_alpha_coder", "coder", "coder-auth", state="limited")
    failed = _event(alpha, 1, coder.id, "handover_failed", "no account has room")
    needs = needs_you_bar.project_needs(alpha, [coder], [], [failed], seq=1)
    assert [(need.kind, need.target) for need in needs] == [(needs_you_bar.HANDOVER, "coder-auth")]
    moved_on = _event(alpha, 2, coder.id, "switched", "moved to account 2")
    assert needs_you_bar.project_needs(alpha, [coder], [], [failed, moved_on], seq=2) == []


def _a_question_in_alpha(projects: dict[str, Any]) -> None:
    alpha = projects["alpha"]
    manager = _session(alpha, "ses_alpha_manager", "manager", "manager")
    projects["frames"][alpha.id] = (
        [manager],
        [],
        [_event(alpha, 1, manager.id, "question", "staging or prod first?")],
    )


def _alphas_board_shown(app: Any, alpha: ProjectInfo) -> bool:
    views = app.query(f"#project-{alpha.id}")
    if not views:
        return False
    view = app.query_one(f"#project-{alpha.id}", TabbedContent)
    return bool(view.active == "tab-board" and view.query_one(BoardPanel).is_on_screen)


def test_a_click_on_the_bar_opens_its_first_item_as_the_key_does(
    projects: dict[str, Any],
) -> None:
    alpha, beta = projects["alpha"], projects["beta"]
    _a_question_in_alpha(projects)

    async def go(pilot: Any, app: Any) -> list[tuple[str, str]]:
        await _on_betas_settings(pilot, app, beta)
        bar = app.query_one(NeedsYouBar)
        bar.read_now()
        await settle_page(app)
        await pilot.click("#needs-you-bar")
        await _until(pilot, lambda: _alphas_board_shown(app, alpha), what="alpha's board")
        return list(app.query_one(f"#project-{alpha.id}").query_one(BoardPanel).lane)

    assert ("question", "manager") in drive((120, 40), go)


def test_at_60_columns_the_bar_shows_and_its_key_opens_the_board_in_the_one_pane(
    projects: dict[str, Any],
) -> None:
    """L2 landed: below 70 columns one pane shows at a time. The bar is over it, whole width,
    one line, and its key brings the project's board up as the one pane."""
    alpha, beta = projects["alpha"], projects["beta"]
    _a_question_in_alpha(projects)

    async def go(pilot: Any, app: Any) -> dict[str, Any]:
        await _on_betas_settings(pilot, app, beta)
        bar = app.query_one(NeedsYouBar)
        bar.read_now()
        await settle_page(app)
        seen: dict[str, Any] = {
            "region": (bar.region.x, bar.region.y, bar.region.width, bar.region.height),
            "line": bar.line().plain,
            "one pane": app.screen.size.width < 70,
        }
        await pilot.press(needs_you_bar.OPEN_KEY)
        await _until(pilot, lambda: _alphas_board_shown(app, alpha), what="alpha's board")
        seen["sidebar shown"] = app.query_one("#sidebar").is_on_screen
        return seen

    seen = drive((60, 24), go)
    assert seen["one pane"], "the premise: L2's one-pane width"
    assert seen["region"] == (0, 0, 60, 1), "the top line, the whole width, one row"
    assert seen["line"].startswith("needs you: 1"), seen["line"]
    assert not seen["sidebar shown"], "the board is the one pane shown"
