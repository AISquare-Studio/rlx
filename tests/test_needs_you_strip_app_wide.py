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


def test_the_key_opens_a_blocked_card_on_a_board_built_before(projects: dict[str, Any]) -> None:
    """The tester of #11: alpha's board was built (visited) while it had nothing, then a card
    was blocked. Its lane was the one it last read (a hidden board's ticks are skipped), so
    F6 found no card on it and only focused the work list; the reader stayed empty."""
    from aisquare.cli.ui.sidebar import ProjectSelected

    alpha, beta = projects["alpha"], projects["beta"]

    async def go(pilot: Any, app: Any) -> tuple[str, str]:
        app.post_message(ProjectSelected(alpha.id))  # visited while nothing needed anyone
        await settle_page(app)
        assert _alphas_board(app, alpha) is not None, "the premise: alpha's board is built"
        await _on_betas_settings(pilot, app, beta)
        projects["frames"][alpha.id] = ([], [_task(alpha, 2, "fix the login race", "blocked")], [])
        bar = app.query_one(NeedsYouBar)
        bar.read_now()
        await settle_page(app)
        line = bar.line().plain
        await pilot.press(needs_you_bar.OPEN_KEY)
        await _until(
            pilot,
            lambda: bool((board := _alphas_board(app, alpha)) and board.reader_text),
            what="the card in alpha's reader",
        )
        panel = _alphas_board(app, alpha)
        return line, panel.reader_text if panel is not None else ""

    line, reader = drive((100, 30), go)
    assert "#2 fix the login race — blocked" in line
    assert reader.startswith("fix the login race\n#2 · blocked"), "opened in the board's reader"


def _blocked_behind_five_reviews(project: ProjectInfo) -> tuple[list[Any], list[Any], list[Any]]:
    """The tester of #11's lane (#1593): one blocked card, five cards in review."""
    tasks = [_task(project, 2, "fix the login race", "blocked")]
    tasks += [_task(project, 10 + i, f"review thing {i}", "review") for i in range(5)]
    return [], tasks, []


@pytest.mark.parametrize("warm", [False, True], ids=["cold", "warm"])
def test_the_bar_names_the_first_row_of_the_lane_and_the_key_opens_it(
    projects: dict[str, Any], warm: bool
) -> None:
    """The tester of #11 (#1593): the bar put a blocked card before cards in review, the lane
    the other way round and five rows deep, so the card the bar named was past the rows the
    lane draws and F6 opened nothing. One order now (the lane's): the bar names the lane's
    first row, and F6 opens it, whether the board was built before or not."""
    from aisquare.cli.ui.sidebar import ProjectSelected

    alpha, beta = projects["alpha"], projects["beta"]

    async def go(pilot: Any, app: Any) -> tuple[str, list[Any], str]:
        if warm:
            app.post_message(ProjectSelected(alpha.id))
            await settle_page(app)
        await _on_betas_settings(pilot, app, beta)
        projects["frames"][alpha.id] = _blocked_behind_five_reviews(alpha)
        bar = app.query_one(NeedsYouBar)
        bar.read_now()
        await settle_page(app)
        line = bar.line().plain
        await pilot.press(needs_you_bar.OPEN_KEY)
        await _until(
            pilot,
            lambda: bool((board := _alphas_board(app, alpha)) and board.reader_text),
            what="the card in alpha's reader",
        )
        panel = _alphas_board(app, alpha)
        assert panel is not None
        return line, list(panel.lane), panel.reader_text

    line, lane, reader = drive((100, 30), go)
    assert line.startswith("needs you: 6") and "#10 review thing 0 — in review" in line, line
    assert lane[0][0] == "review", "the lane's first row is the card the bar names"
    assert reader.startswith("review thing 0\n#10 · review"), reader


@pytest.mark.parametrize("warm", [False, True], ids=["cold", "warm"])
def test_a_card_past_the_rows_the_lane_draws_still_opens(
    projects: dict[str, Any], warm: bool
) -> None:
    """The bar opens the card it is given wherever the lane has it: the blocked card is the
    sixth item behind five in review, past the rows the lane draws (its row acts only when
    drawn), and still opens in the reader."""
    from aisquare.cli.ui.sidebar import ProjectSelected

    alpha, beta = projects["alpha"], projects["beta"]

    async def go(pilot: Any, app: Any) -> tuple[int, int, str]:
        if warm:
            app.post_message(ProjectSelected(alpha.id))
            await settle_page(app)
        await _on_betas_settings(pilot, app, beta)
        projects["frames"][alpha.id] = _blocked_behind_five_reviews(alpha)
        bar = app.query_one(NeedsYouBar)
        bar.read_now()
        await settle_page(app)
        blocked = next(need for need in bar.needs if need.kind == "blocked")
        bar.open_need(blocked)
        await _until(
            pilot,
            lambda: bool((board := _alphas_board(app, alpha)) and board.reader_text),
            what="the blocked card in alpha's reader",
        )
        panel = _alphas_board(app, alpha)
        assert panel is not None
        from aisquare.cli.ui.board_parts import NeedsYouLane

        index = panel.lane.index(("blocked", blocked.target))
        return index, panel.query_one(NeedsYouLane).drawn, panel.reader_text

    index, drawn, reader = drive((100, 30), go)
    assert index >= drawn, f"the premise: the card is past the lane's drawn rows ({index}, {drawn})"
    assert reader.startswith("fix the login race\n#2 · blocked"), reader


@pytest.mark.parametrize("steps", range(24))
def test_landing_on_a_cold_projects_view_at_any_moment_of_its_mount(
    tmp_path: Path, projects: dict[str, Any], steps: int
) -> None:
    """The tester of #11 (#1636): the bar landed on a project's view while it was still
    mounting. ``steps`` event-loop turns after the view first appears: 1 to 4 raised out of
    the landing (its tab row not filled: asq exited), 5 raised on the board's parts, 6 to 23
    opened the reader in a hidden pane, the keys going to it. At every moment now: no error,
    and the card the bar named, in a reader on screen, focused."""
    from aisquare.cli.ui.sidebar import ProjectSelected
    from tests.test_ui_spawn import register

    gamma = register(tmp_path / "gamma", project_id="prj_gamma", codename="calm-gecko")
    projects["frames"][gamma.id] = _blocked_behind_five_reviews(gamma)

    def gammas_board(app: Any) -> BoardPanel | None:
        panels = app.query(f"#project-{gamma.id} BoardPanel")  # none while it mounts
        return panels.first() if panels else None

    async def go(pilot: Any, app: Any) -> tuple[str, bool, bool]:
        await _on_betas_settings(pilot, app, projects["beta"])
        bar = app.query_one(NeedsYouBar)
        bar.read_now()
        await settle_page(app)
        named = bar.needs[0]
        assert named.project.id == gamma.id, "the premise: the bar names gamma's card"
        app.post_message(ProjectSelected(gamma.id))
        for _ in range(5000):
            if app.query(f"#project-{gamma.id}"):
                break
            await asyncio.sleep(0)
        else:
            pytest.fail("gamma's view never appeared")
        for _ in range(steps):
            await asyncio.sleep(0)
        bar._landing = (named, 0)  # what open_need leaves for the landing, at this moment
        bar._land()
        await _until(
            pilot,
            lambda: bool(
                (board := gammas_board(app))
                and board.reader_text
                and board.is_on_screen
                and app.focused is not None
                and app.focused.is_on_screen
            ),
            what="gamma's card in a reader on screen, focused",
        )
        board = gammas_board(app)
        assert board is not None
        focused = app.focused
        return board.reader_text, board.is_on_screen, bool(focused and focused.is_on_screen)

    reader, shown, focused = drive((100, 30), go)
    assert reader.startswith("review thing 0\n#10 · review"), reader
    assert shown and focused


def _reader_focused(app: Any, board: BoardPanel | None) -> bool:
    focused = app.focused
    return bool(
        board is not None
        and board.reader_text
        and focused is not None
        and focused.id == "reader"
        and focused.is_on_screen
    )


@pytest.mark.parametrize("size", [(60, 24), (100, 30)], ids=["60x24", "100x30"])
def test_the_keys_stay_on_the_card_when_the_board_shows_after_it_opened(
    projects: dict[str, Any], monkeypatch: pytest.MonkeyPatch, size: tuple[int, int]
) -> None:
    """The tester of #11 (#1672): the card opened before the board handled its own Show, which
    then handed the keys to its work list; below the wide layout the reader covers that list,
    and the focus fell to nothing: the card showed and no key reached it. Here the board's
    Show is made to come last, every time."""
    from textual import events

    alpha, beta = projects["alpha"], projects["beta"]
    opened = NeedsYouBar._open_item

    def board_shows_after(self: NeedsYouBar, panel: BoardPanel, need: Any) -> None:
        opened(self, panel, need)
        panel.post_message(events.Show())  # the board's own Show, handled after the card

    monkeypatch.setattr(NeedsYouBar, "_open_item", board_shows_after)

    async def go(pilot: Any, app: Any) -> str:
        await _on_betas_settings(pilot, app, beta)
        projects["frames"][alpha.id] = _blocked_behind_five_reviews(alpha)
        bar = app.query_one(NeedsYouBar)
        bar.read_now()
        await settle_page(app)
        await pilot.click("#needs-you-bar")
        await _until(
            pilot,
            lambda: bar._landing is None and bar._keeping is None,
            what="the landing and its focus checks done",
        )
        await settle_page(app)
        assert _reader_focused(app, _alphas_board(app, alpha)), (
            f"the keys are not on the card: {app.focused!r}"
        )
        await pilot.press("escape")  # and the board's keys reach it: Esc closes the reader
        await settle_page(app)
        board = _alphas_board(app, alpha)
        return board.reader_text if board is not None else "?"

    assert drive(size, go) == "", "Esc closed the reader: the keys were on it"


def test_a_warm_board_at_60_columns_takes_the_keys_on_every_click(
    projects: dict[str, Any],
) -> None:
    """#1672's own repro, as it happens: alpha's board built full, beta's Settings, a click
    on the bar at 60x24, six times; each time the card is in the reader and the keys on it."""
    from aisquare.cli.ui.sidebar import ProjectSelected

    alpha, beta = projects["alpha"], projects["beta"]
    projects["frames"][alpha.id] = _blocked_behind_five_reviews(alpha)

    async def go(pilot: Any, app: Any) -> list[bool]:
        app.post_message(ProjectSelected(alpha.id))
        await settle_page(app)
        seen: list[bool] = []
        bar = app.query_one(NeedsYouBar)
        for _ in range(6):
            await _on_betas_settings(pilot, app, beta)
            bar.read_now()
            await settle_page(app)
            await pilot.click("#needs-you-bar")
            await _until(
                pilot,
                lambda: bar._landing is None and bar._keeping is None,
                what="the landing and its focus checks done",
            )
            await settle_page(app)
            seen.append(_reader_focused(app, _alphas_board(app, alpha)))
            await pilot.press("escape")
            await settle_page(app)
        return seen

    assert drive((60, 24), go) == [True] * 6
