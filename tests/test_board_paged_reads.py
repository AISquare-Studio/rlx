"""The board's per-tick reads, paged (#44; the owner's TUI RAM brief, decision 4): a visible
board reads its project's open tasks and the sessions it shows at each tick, the done archive
when it is unfolded, and an older session when something on the board names it, never every
task and every session the project ever had. What it shows is unchanged ("the board is holy").

The proof is a board read both ways at once: two panels side by side in one app, over one
store, one of them reading as before (every task and session, :func:`_unpaged`), compared
after every step (:func:`_shown`) — their feeds, Work, the team block, the needs-you lane and
strip, a task's detail and the reader — through ticks that move a task, end a session and start
one that renames others, the archive unfolded and folded, and a pick of a task an old session
closed. Then what a tick reads, row by row, and the store's new reads one by one.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import Callable, Iterator
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from textual.app import App, ComposeResult
from textual.containers import Horizontal
from textual.coordinate import Coordinate
from textual.widgets import DataTable, OptionList, Static

from aisquare.cli import watch
from aisquare.cli.ui import board as board_mod
from aisquare.cli.ui.board import BoardPanel
from aisquare.cli.ui.board_parts import NeedsYouLane
from aisquare.cli.ui.needs_you_bar import read_needs
from aisquare.core import event_time
from aisquare.core import store as store_mod
from aisquare.core.store import FLEET_HELD, store_session
from aisquare.models import FleetAgent, ProjectInfo, TeamSession, TeamTask
from aisquare.services import team as team_service
from tests.ui_workers import settle_page

NAMES = ("coder-1", "coder-2", "reviewer-1", "tester-1", "coder-3")
"""The labels the old sessions take in turn: every one of them shared, as restarts leave them."""


# --- the board: many done tasks, many old sessions, every way a name is told apart ------------


def _session(
    store: Any,
    project: ProjectInfo,
    sid: str,
    *,
    role: str = "coder",
    label: str | None = None,
    started: datetime,
    seen: datetime,
    ended: datetime | None,
    state: str = "working",
    transcript: str | None = None,
) -> None:
    """A session row as the hooks leave it, its times and state written as given."""
    store.upsert_session(
        TeamSession(
            id=sid, project_id=project.id, role=role, label=label, started_at=started,
            last_seen_at=seen,
        )
    )  # fmt: skip
    store._conn.execute(
        "UPDATE team_session SET started_at = ?, last_seen_at = ?, ended_at = ?, state = ?, "
        "transcript_path = ? WHERE id = ?",
        (
            started.isoformat(),
            seen.isoformat(),
            ended.isoformat() if ended else None,
            state,
            transcript,
            sid,
        ),
    )
    store._conn.commit()


def _row(
    store: Any, project: ProjectInfo, agent_id: str, label: str, sid: str | None, at: datetime
) -> None:
    """An ended fleet row: the name the board gives the session it records."""
    store.upsert_fleet_agent(
        FleetAgent(
            id=agent_id, project_id=project.id, label=label, role="coder", pane_id=f"%{agent_id}",
            cwd=project.root, session_id=sid, created_at=at,
        )
    )  # fmt: skip
    store.end_fleet_agent(agent_id, ended_by="stopped")


def _task(store: Any, project: ProjectInfo, n: int, **fields: Any) -> TeamTask:
    at = datetime(2026, 11, 1, tzinfo=UTC) + timedelta(minutes=n)
    task: TeamTask
    task, _ = store.upsert_task(
        TeamTask(
            id=f"tsk_{n:05d}", project_id=project.id, key=f"k{n}", title=f"task {n}",
            created_at=at, updated_at=at,
        )
    )  # fmt: skip
    if fields:
        store._conn.execute(
            f"UPDATE team_task SET {', '.join(f'{k} = ?' for k in fields)} WHERE id = ?",
            (*(json.dumps(v) if isinstance(v, list) else v for v in fields.values()), task.id),
        )
        store._conn.commit()
    return task


def _say(
    store: Any, project: ProjectInfo, sid: str | None, kind: str, text: str, task: str | None = None
) -> None:
    team_service._emit(store, project.id, kind, text, session_id=sid, task_id=task)


def build_board(root: Path, *, done: int, old: int, now: datetime) -> ProjectInfo:
    """``done`` closed tasks and ``old`` ended sessions under a few live ones, with the store's
    every way of naming a session: shared labels started in one minute, an earlier round's
    agent nothing on the board names that still decides its namesakes' names, a relabel, a
    retired label, a session a ``/clear`` left and one a launch bound."""
    project = ProjectInfo(id="prj_paged", root=root)
    root.mkdir(parents=True, exist_ok=True)
    transcript = root / "old-closer.jsonl"
    transcript.write_text("{}\n", encoding="utf-8")
    base = (now - timedelta(days=3)).replace(second=0, microsecond=0)  # its minutes whole
    with store_session() as store:
        store.onboard_project(project)
        olds: list[str] = []
        for n in range(old):
            sid = f"ses-old-{n:04d}"
            started = base + timedelta(hours=n % 7, minutes=(n * 13) % 60)
            seen = started + timedelta(minutes=30)
            if n % 10 == 0:
                seen = base + timedelta(hours=9)  # several seen at one moment: the tie order
            label = None if n % 11 == 0 else NAMES[n % len(NAMES)]
            _session(store, project, sid, label=label, started=started, seen=seen, ended=seen)
            olds.append(sid)
        # Who closed the archive's first cards, and nothing since: named by no line the
        # feed's first read holds, with a transcript ``o`` opens.
        _session(
            store, project, "ses-archivist", label="coder-3", started=base - timedelta(days=1),
            seen=base - timedelta(days=1), ended=base - timedelta(days=1),
            transcript=str(transcript),
        )  # fmt: skip
        # An earlier round named by nothing on the board, the newest of its name: it keeps
        # "coder-2" and every other coder-2 that has ended is told apart from it.
        _session(
            store, project, "ses-quiet", label="coder-2", started=now - timedelta(hours=2),
            seen=now - timedelta(hours=2), ended=now - timedelta(hours=2),
        )  # fmt: skip
        # Two rounds of tester-1 started in one minute, one of them silent: the one that
        # speaks is told apart by its short id, as the minute is shared.
        twins = base + timedelta(hours=20, minutes=7)
        for sid, offset in (("ses-twin-a", 5), ("ses-twin-b", 40)):
            started = twins + timedelta(seconds=offset)
            _session(
                store, project, sid, label="tester-1", started=started,
                seen=started + timedelta(minutes=5), ended=started + timedelta(minutes=5),
            )  # fmt: skip
        # Two rounds of one name started, seen and ended at one moment: the newer row keeps
        # the name (team_sessions' order), and the one that speaks is told apart.
        pair = base + timedelta(hours=21)
        for sid in ("ses-pair-a", "ses-pair-b"):
            _session(store, project, sid, label="pair", started=pair, seen=pair, ended=pair)
        # A claimer only its claim names on the board (no line of its own), and a newer round
        # of its name nothing names at all, which keeps the name: the claim reads
        # "coder-7 (HH:MM)" (review of #44, m1).
        for sid, hours in (("ses-claimer", 1), ("ses-claim-newer", 2)):
            started = base + timedelta(hours=hours, minutes=3)
            _session(
                store, project, sid, label="coder-7", started=started,
                seen=started + timedelta(minutes=9), ended=started + timedelta(minutes=9),
            )  # fmt: skip
        # A live session that says nothing and claims nothing: the team block lists it.
        _session(
            store, project, "ses-silent", label="tester-2", started=now - timedelta(hours=4),
            seen=now - timedelta(hours=3), ended=None,
        )  # fmt: skip
        # Live: the manager, a coder waiting on the person, one on a usage limit for hours, a
        # reviewer at work.
        live = now - timedelta(seconds=30)
        _session(store, project, "ses-man", role="manager", started=base, seen=live, ended=None)
        _session(
            store, project, "ses-c1", label="coder-1", started=now - timedelta(hours=1), seen=live,
            ended=None, state="attention",
        )  # fmt: skip
        _session(
            store, project, "ses-lim", label="tester-1", started=now - timedelta(hours=6),
            seen=now - timedelta(hours=5), ended=None, state="limited",
        )  # fmt: skip
        _session(
            store, project, "ses-rev", role="reviewer", label="reviewer-1",
            started=now - timedelta(minutes=40), seen=live, ended=None,
        )  # fmt: skip
        # The fleet's names: a relabel, a retired label, a /clear's earlier session, a launch's.
        _row(store, project, "agt_relabel", "coder-renamed", olds[3], base)
        _row(store, project, "agt_retired", "~coder-1", olds[7], base)
        _row(store, project, "agt_held", "tester-1", "ses-somewhere-else", base)
        store.set_meta(f"{FLEET_HELD}{olds[9]}", "agt_held")
        _row(store, project, "agt_bound", "reviewer-1", None, base)
        store.set_meta("fleet-session:agt_bound", olds[12])
        # Who spoke of an archived card once, long ago, and never since: only the reader's
        # history of that card names it.
        _session(
            store, project, "ses-historian", role="reviewer", label="reviewer-1",
            started=base - timedelta(days=2), seen=base - timedelta(days=2),
            ended=base - timedelta(days=2),
        )  # fmt: skip
        _task(store, project, 3)
        _say(store, project, "ses-historian", "note", "this one is risky", "tsk_00003")
        # The done archive, each closed by an old session: most of those lines are long out
        # of the feed's first read.
        for n in range(done):
            task = _task(
                store, project, n, status="done" if n % 5 else "dropped",
                updated_at=(base + timedelta(minutes=n)).isoformat(),
            )  # fmt: skip
            closer = "ses-archivist" if n < 5 else olds[(n * 7) % 30]
            _say(store, project, closer, "task_done" if n % 5 else "task_dropped", "x", task.id)
        # Work: waiting on the archive and on a card in flight, claimed by the live and by an
        # ended session, a review with its gate and one without, and the blocked.
        first = done
        doing = _task(store, project, first, status="doing", claimed_by="ses-c1")
        _task(store, project, first + 1, needs=["tsk_00001", "tsk_00002"])
        _task(store, project, first + 2, needs=["tsk_00003", doing.id])
        _task(store, project, first + 3, status="doing", claimed_by=olds[5])
        review = _task(store, project, first + 4, status="review", claimed_by="ses-rev")
        gated = _task(store, project, first + 5, status="review", claimed_by=olds[6])
        _task(store, project, first + 6, status="blocked", detail="no key")
        _task(store, project, first + 7, status="doing", claimed_by="ses-claimer")
        _say(store, project, "ses-rev", "task_review", "please look", review.id)
        _say(store, project, olds[6], "task_review", "ready", gated.id)
        _say(store, project, "ses-man", "result", "ok", gated.id)
        for n in range(0, 45, 3):
            _say(store, project, olds[n], "note", f"from the old round {n}")
        _say(store, project, "ses-twin-a", "note", "one of two")
        _say(store, project, "ses-pair-a", "note", "the older row")
        _say(store, project, "ses-lim", "handover_failed", "no account left")
        _say(store, project, "ses-c1", "question", "which key do I use?")
    return project


# --- one app, two boards: one reads as before ------------------------------------------------


def _unpaged(read: Callable[..., team_service.BoardRead]) -> Callable[..., team_service.BoardRead]:
    """``board_read`` as before #44: every task and session, whatever the board asks for."""

    def every(**kwargs: Any) -> team_service.BoardRead:
        for paging in ("closed", "sessions", "task_ids"):
            kwargs.pop(paging, None)
        return read(**kwargs)

    return every


class Unpaged(BoardPanel):
    """A board whose every read is :func:`_unpaged`: the board as it was."""

    def refresh_data(self) -> None:
        paged = team_service.board_read
        team_service.board_read = _unpaged(paged)
        try:
            super().refresh_data()
        finally:
            team_service.board_read = paged


class TwoBoards(App[None]):
    CSS = "BoardPanel { width: 1fr; }"

    def __init__(self, project: ProjectInfo) -> None:
        super().__init__()
        self.project = project

    def compose(self) -> ComposeResult:
        with Horizontal():
            yield BoardPanel(self.project, interval=3600, id="paged")
            yield Unpaged(self.project, interval=3600, id="before")


def _markup(visual: Any) -> str:
    return str(getattr(visual, "markup", visual))


def _shown(panel: BoardPanel) -> dict[str, Any]:
    """Everything the panel shows, styles and all."""
    table = panel.query_one("#tasks", DataTable)
    rows = []
    for index in range(table.row_count):
        key = table.coordinate_to_cell_key(Coordinate(index, 0)).row_key.value
        rows.append((key, [_markup(cell) for cell in table.get_row_at(index)]))
    feed = panel.query_one("#feed", OptionList)
    lines = [
        (feed.get_option_at_index(i).id, _markup(feed.get_option_at_index(i).prompt))
        for i in range(feed.option_count)
    ]
    lane = panel.query_one(NeedsYouLane)
    return {
        "work": rows,
        "work title": str(table.border_title),
        "stream": lines,
        "team": _markup(panel.query_one("#sessions", Static).visual),
        "lane": [_markup(row.visual) for row in lane.query(Static) if row.display],
        "needs": panel.needs,
        "strip": panel.needs.line(),
        "detail": panel.detail_text,
        "reader": panel.reader_text,
        "moment": panel._moment_transcript(),
    }


@pytest.fixture
def now(monkeypatch: pytest.MonkeyPatch) -> datetime:
    """One clock for both boards: a card's age and a line's time read it, never the wall's."""
    fixed = datetime.now(UTC).replace(second=0, microsecond=0)  # whole: the fixture's minutes
    monkeypatch.setattr(board_mod, "_now", lambda: fixed)
    monkeypatch.setattr(event_time, "_now", lambda: fixed)
    return fixed


@pytest.fixture
def big(tmp_path: Path, now: datetime) -> ProjectInfo:
    return build_board(tmp_path / "board", done=450, old=120, now=now)


def _both(app: TwoBoards) -> tuple[BoardPanel, BoardPanel]:
    return app.query_one("#paged", BoardPanel), app.query_one("#before", BoardPanel)


def test_a_paged_board_shows_what_the_board_showed_through_ticks_and_the_archive(
    big: ProjectInfo, now: datetime
) -> None:
    """Acceptance 1: the feed, Work and its counts, the team, the needs-you lane and strip, a
    done task's closer and the reader's history, step by step, the same as a board that reads
    everything."""
    project = big
    steps: list[tuple[str, dict[str, Any], dict[str, Any]]] = []
    held: list[bool] = []

    async def go() -> None:
        app = TwoBoards(project)
        async with app.run_test(size=(300, 60)) as pilot:
            await settle_page(app)
            paged, before = _both(app)

            async def step(name: str, act: Callable[[BoardPanel], object] | None = None) -> None:
                for panel in (paged, before):
                    if act is not None:
                        act(panel)
                await settle_page(app)
                await pilot.pause()
                steps.append((name, _shown(paged), _shown(before)))

            await step("first read")
            with store_session() as store:
                # A tick's worth: a card done, an old round speaking, the reviewer gone, and a
                # coder-1 started since and ended unseen, which renames the earlier coder-1s.
                store.set_task_status("tsk_00450", "done")
                _say(store, project, "ses-old-0040", "note", "back from the archive")
                store.end_session("ses-rev")
                _session(
                    store, project, "ses-newest", label="coder-1",
                    started=now - timedelta(minutes=10), seen=now - timedelta(minutes=9),
                    ended=now - timedelta(minutes=9),
                )  # fmt: skip
            await step("a tick later", lambda panel: panel.refresh_data())
            await step("the archive unfolded", lambda panel: panel.action_toggle_done())
            await step("another tick", lambda panel: panel.refresh_data())
            # A task an old session closed, its line long out of the feed: who closed it, and
            # the transcript ``o`` opens.
            held.append("ses-archivist" in paged._sessions)
            await step(
                "a closer from the archive", lambda panel: panel._show_task_detail("tsk_00001")
            )
            held.append("ses-archivist" in paged._sessions)
            held.append("ses-historian" in paged._sessions)
            await step("a history", lambda panel: panel.open_reader("tsk_00003"))
            held.append("ses-historian" in paged._sessions)
            await step("a tick with the reader open", lambda panel: panel.refresh_data())
            await step("back", lambda panel: panel.action_close_reader())
            await step("the archive folded", lambda panel: panel.action_toggle_done())
            await step("a last tick", lambda panel: panel.refresh_data())

    asyncio.run(go())

    for name, paged, before in steps:
        assert paged == before, f"{name}: the paged board shows otherwise"
    first = steps[0][2]
    assert len(first["stream"]) == 400, "the first read fills the feed"
    assert ("group:done", ["", "[bold]▸[/bold]", "[bold]Done (450)[/bold]", "", ""]) in first[
        "work"
    ]
    picked = steps[4][2]
    assert "done by coder-3 (" in picked["detail"] and picked["moment"] is not None, picked
    assert held == [False, True, False, True], "each read for what shows it, and only then"
    assert "reviewer-1 (" in steps[5][2]["reader"], "the history names it as the board does"
    assert "coder-1 (" in str(steps[1][2]["stream"]), "the newest coder-1 renamed the rest"
    twin = [line for _id, line in first["stream"] if "one of two" in line]
    assert twin and "tester-1 (ses-twin" in twin[0], twin
    older = [line for _id, line in first["stream"] if "the older row" in line]
    assert older and " pair (" in older[0], "a tie: the newer row keeps the name"
    claimed = [cells for key, cells in first["work"] if key == "tsk_00457"]
    assert claimed and "coder-7 (" in claimed[0][3], "the claim names it as the board does"
    assert "tester-2" in first["team"], "the silent live session is in the team block"


# --- what a tick reads -----------------------------------------------------------------------


@pytest.fixture
def rows_read(monkeypatch: pytest.MonkeyPatch) -> Iterator[dict[str, int]]:
    """Each task and session row the store turns into a model, counted."""
    counts = {"tasks": 0, "sessions": 0}
    task, session = store_mod._row_to_task, store_mod._row_to_session

    def a_task(row: Any) -> TeamTask:
        counts["tasks"] += 1
        return task(row)

    def a_session(row: Any) -> TeamSession:
        counts["sessions"] += 1
        return session(row)

    monkeypatch.setattr(store_mod, "_row_to_task", a_task)
    monkeypatch.setattr(store_mod, "_row_to_session", a_session)
    yield counts


def test_a_tick_reads_the_open_tasks_and_the_sessions_it_shows(
    big: ProjectInfo, rows_read: dict[str, int]
) -> None:
    """Acceptance 2, rows read: with Done folded a tick reads the 8 open tasks, never the 450
    done; and the live sessions, those its feed names and their namesakes, never all 133 when
    nothing the board shows names them. The archive is read while it is unfolded."""
    project = big
    shown: dict[str, int] = {}

    async def go() -> None:
        app = TwoBoards(project)
        async with app.run_test(size=(300, 60)):
            await settle_page(app)
            paged, _before = _both(app)
            for kind in rows_read:
                rows_read[kind] = 0
            paged.refresh_data()
            shown.update(rows_read)
            for kind in rows_read:
                rows_read[kind] = 0
            paged.action_toggle_done()
            shown["unfolded tasks"] = rows_read["tasks"]

    asyncio.run(go())

    assert shown["tasks"] == 8, shown
    with store_session() as store:
        every = len(store.team_sessions(project.id))
        live = sum(1 for s in store.team_sessions(project.id) if s.ended_at is None)
    assert live <= shown["sessions"] < every, shown
    assert shown["unfolded tasks"] == 458, "unfolded, the archive is read with the open"


def test_a_board_s_plain_frame_and_the_needs_you_bar_read_the_same_paged(
    big: ProjectInfo, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``board -w`` without Textual and the app-wide needs-you bar read paged too, and show and
    count what they did."""
    project = big
    paged_frame = watch.board_frame(60, 200, project).plain.split("\n", 1)[1]
    paged_needs = read_needs([project])
    monkeypatch.setattr(team_service, "board_read", _unpaged(team_service.board_read))
    before_frame = watch.board_frame(60, 200, project).plain.split("\n", 1)[1]
    before_needs = read_needs([project])

    assert paged_frame == before_frame
    assert paged_needs == before_needs
    assert {need.kind for need in paged_needs} >= {"question", "handover", "blocked", "review"}


# --- the store's reads --------------------------------------------------------------------------


def test_board_sessions_keeps_team_sessions_order_ties_and_all(big: ProjectInfo) -> None:
    """The order the team block lists and ``board_names`` keeps a name by: ``team_sessions``',
    sessions seen at one moment newest row first, past a chunk of 500 ids too."""
    with store_session() as store:
        every = store.team_sessions(big.id)
        ids = [s.id for s in every] + [f"ses-none-{n}" for n in range(600)]
        read = store.board_sessions(big.id, ids)
        live = store.board_sessions(big.id, ())

    assert [s.id for s in read] == [s.id for s in every]
    assert [s.id for s in live] == [s.id for s in every if s.ended_at is None]


def test_session_ids_called_finds_every_way_a_session_is_named(big: ProjectInfo) -> None:
    """Its own label, its role, a fleet row's label, a retired one, a held one, a bound one."""
    with store_session() as store:
        every = store.team_sessions(big.id)
        labels = team_service.fleet_labels(store, every)
        for name in {team_service.board_name(s, labels) for s in every}:
            called = store.session_ids_called(big.id, [name])
            named = {s.id for s in every if team_service.board_name(s, labels) == name}
            assert named <= called, f"{name}: {sorted(named - called)} not found"


def test_task_reads_count_and_name_without_reading_the_archive(big: ProjectInfo) -> None:
    with store_session() as store:
        every = store.team_tasks(big.id)
        open_ = store.team_tasks_in(big.id, team_service.OPEN_STATUSES)
        counts = store.team_task_counts(big.id, ("done", "dropped"))
        named = [t.id for t in every] + [f"tsk_none_{n}" for n in range(600)]
        statuses = store.team_task_statuses(big.id, named)

    assert [t.id for t in open_] == [t.id for t in every if t.status not in ("done", "dropped")]
    assert counts == {"done": 360, "dropped": 90}
    assert statuses == {t.id: t.status for t in every}


# --- review of #44 (#3026): n1, n2 -------------------------------------------------------------


def _reads(monkeypatch: pytest.MonkeyPatch) -> list[dict[str, Any]]:
    """Every ``board_read`` the panels make, by its arguments."""
    asked: list[dict[str, Any]] = []
    read = team_service.board_read

    def counted(**kwargs: Any) -> team_service.BoardRead:
        asked.append(kwargs)
        return read(**kwargs)

    monkeypatch.setattr(team_service, "board_read", counted)
    return asked


def test_a_pick_reads_no_closer_for_a_card_the_person_closed_and_asks_once_for_one_elsewhere(
    big: ProjectInfo, monkeypatch: pytest.MonkeyPatch
) -> None:
    """n1: a pick of a card whose closer the board had not read read the board again, and so
    did every pick of one the person closed (no session) or one closed by a session the board
    does not have: each pick, a read. Now the person's reads nothing, and the other is asked
    for once."""
    with store_session() as store:
        _task(store, big, 900, status="done")
        _say(store, big, None, "task_done", "by hand", "tsk_00900")
        _task(store, big, 901, status="done")
        _say(store, big, "ses-on-another-board", "task_done", "elsewhere", "tsk_00901")
    picks: dict[str, int] = {}

    async def go() -> None:
        app = TwoBoards(big)
        async with app.run_test(size=(300, 60)):
            await settle_page(app)
            paged, _before = _both(app)
            paged.action_toggle_done()
            await settle_page(app)
            asked = _reads(monkeypatch)
            for _ in range(3):
                paged._show_task_detail("tsk_00900")
            picks["by the person"] = len(asked)
            for _ in range(3):
                paged._show_task_detail("tsk_00901")
            picks["elsewhere"] = len(asked) - picks["by the person"]

    asyncio.run(go())

    assert picks == {"by the person": 0, "elsewhere": 1}


def test_a_failed_read_as_done_unfolds_leaves_it_folded(
    big: ProjectInfo, monkeypatch: pytest.MonkeyPatch
) -> None:
    """n2: with the store busy as Done unfolded, it showed open with no rows until a tick read
    them. It stays folded, as the board keeps its last frame, and opens at the next ``d``."""
    shown: list[tuple[bool, str]] = []

    async def go() -> None:
        app = TwoBoards(big)
        async with app.run_test(size=(300, 60)):
            await settle_page(app)
            paged, _before = _both(app)
            read = team_service.board_read

            def busy(**_kwargs: Any) -> team_service.BoardRead:
                raise RuntimeError("database is locked")

            monkeypatch.setattr(team_service, "board_read", busy)
            paged.action_toggle_done()
            await settle_page(app)
            heading = dict(_shown(paged)["work"])["group:done"][1]
            shown.append(("done" in paged.folds, heading))
            monkeypatch.setattr(team_service, "board_read", read)
            paged.action_toggle_done()
            await settle_page(app)
            heading = dict(_shown(paged)["work"])["group:done"][1]
            shown.append(("done" in paged.folds, heading))

    asyncio.run(go())

    assert shown == [(True, "[bold]▸[/bold]"), (False, "[bold]▾[/bold]")]
