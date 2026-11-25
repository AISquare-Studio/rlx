"""The board's layout (B2, the board revamp): what needs you, the work, and the stream.

Each spec line is driven at the two sizes the contract names, 140x40 (Work and the Stream side
by side) and 100x30 (one at a time, under the switch), on a scripted board: the tick's one read
and a task's history are handed in, so nothing here reads the store or tmux.
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable, Iterator
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any, TypeVar

import pytest
from textual.app import App, ComposeResult
from textual.widget import Widget
from textual.widgets import Button, DataTable, Input, OptionList

from aisquare.cli import watch
from aisquare.cli.ui import board as board_mod
from aisquare.cli.ui.board import (
    GROUP_ROW,
    TASK_COLUMNS,
    BoardPanel,
    click_key,
)
from aisquare.cli.ui.board_parts import LaneRow
from aisquare.cli.ui.divider import Divider
from aisquare.models import ProjectInfo, TaskStatus, TeamEvent, TeamSession, TeamTask
from aisquare.services import team as team_service
from tests.ui_workers import settle_page

T = TypeVar("T")
NOW = datetime.now(UTC).replace(microsecond=0)
PROJECT = ProjectInfo(id="prj_layout", root=Path("/tmp/layout-project"))
SIZES = pytest.mark.parametrize("size", [(140, 40), (100, 30)], ids=["140x40", "100x30"])


def _session(sid: str, role: str, label: str, state: str = "working") -> TeamSession:
    return TeamSession(
        id=sid,
        project_id=PROJECT.id,
        role=role,
        label=label,
        started_at=NOW - timedelta(minutes=20),
        last_seen_at=NOW - timedelta(minutes=1),
        state=state,
    )


def _task(number: int, title: str, status: TaskStatus, holder: str | None = None) -> TeamTask:
    return TeamTask(
        id=f"tsk_{number:04d}",
        project_id=PROJECT.id,
        number=number,
        key=f"task-{number}",
        title=title,
        status=status,
        claimed_by=holder,
        created_at=NOW - timedelta(hours=1),
        updated_at=NOW - timedelta(minutes=number),
    )


def _event(seq: int, session_id: str | None, kind: str, text: str, **extra: Any) -> TeamEvent:
    return TeamEvent(
        seq=seq,
        id=f"evt_{seq}",
        project_id=PROJECT.id,
        session_id=session_id,
        kind=kind,
        text=text,
        created_at=NOW - timedelta(minutes=10) + timedelta(seconds=seq),
        **extra,
    )


MANAGER = _session("ses_manager", "manager", "manager")
CODER = _session("ses_coder", "coder", "coder-auth", state="attention")
TASKS = [
    _task(1, "wire auth", "doing", CODER.id),
    _task(2, "fix the login race", "blocked", CODER.id),
    _task(3, "add the retry", "review"),
    _task(4, "write the docs", "todo"),
    _task(5, "ship the hotfix", "done"),
]
EVENTS = [
    _event(1, MANAGER.id, "task_added", "fix the login race", task_id="tsk_0002"),
    _event(2, CODER.id, "task_claimed", "fix the login race", task_id="tsk_0002"),
    _event(3, CODER.id, "task_blocked", "fix the login race — no API key", task_id="tsk_0002"),
    _event(4, MANAGER.id, "question", "staging or prod first?"),
]


@pytest.fixture
def board(monkeypatch: pytest.MonkeyPatch) -> Iterator[dict[str, Any]]:
    """The tick's read and a task's history, scripted."""
    frame: dict[str, Any] = {"sessions": [MANAGER, CODER], "tasks": TASKS, "events": EVENTS}

    def board_read(**_kwargs: Any) -> team_service.BoardRead:
        return team_service.BoardRead(
            PROJECT, frame["sessions"], frame["tasks"], frame["events"], {}
        )

    def history(task: TeamTask, **_kwargs: Any) -> list[TeamEvent]:
        return [event for event in frame["events"] if event.task_id == task.id]

    monkeypatch.setattr(team_service, "board_read", board_read)
    monkeypatch.setattr(team_service, "task_history", history)
    monkeypatch.setattr(team_service, "stopped_because", lambda task: None)
    yield frame


class Host(App[None]):
    def __init__(self, *, standalone: bool = False) -> None:
        super().__init__()
        self.wanted: list[str] = []
        self._standalone = standalone

    def compose(self) -> ComposeResult:
        yield BoardPanel(PROJECT, interval=3600, id="board", standalone=self._standalone)

    def on_board_panel_agent_wanted(self, event: BoardPanel.AgentWanted) -> None:
        self.wanted.append(event.label)


def drive(size: tuple[int, int], fn: Callable[[Any, Host, BoardPanel], Awaitable[T]]) -> T:
    async def run() -> T:
        app = Host()
        async with app.run_test(size=size) as pilot:
            await pilot.pause()
            await settle_page(app)
            panel = app.query_one(BoardPanel)
            panel.query_one("#tasks").focus()
            await pilot.pause()
            return await fn(pilot, app, panel)

    return asyncio.run(run())


async def _stream_in_view(pilot: Any, app: Host, panel: BoardPanel) -> None:
    """Below 120 columns the board opens on Work: ``b`` brings the Stream into view."""
    if not panel.query_one("#feedpane").display:
        await pilot.press("b")
        await settle_page(app)


def _row_keys(panel: BoardPanel) -> list[str]:
    table = panel.query_one("#tasks", DataTable)
    return [str(key.value) for key in table.rows]


# --- the needs-you lane -------------------------------------------------------------------------


@SIZES
def test_the_lane_lists_each_thing_that_needs_you_with_its_text_and_its_action(
    board: dict[str, Any], size: tuple[int, int]
) -> None:
    async def go(pilot: Any, app: Host, panel: BoardPanel) -> tuple[list[str], list[str], str]:
        rows = [str(row.render()) for row in panel.query(LaneRow) if row.display]
        await pilot.click("#lane-0")  # the waiting agent: selected in asq's left pane
        await settle_page(app)
        await pilot.press("4")  # the blocked card: opened in the reader
        await settle_page(app)
        return rows, app.wanted, panel.reader_text

    rows, wanted, reader = drive(size, go)
    assert len(rows) == 4
    assert (
        rows[0].startswith("1  🔔 coder-auth waits for you in its pane") and "selects it" in rows[0]
    )
    assert rows[1].startswith("2  ❓ manager asks: staging or prod first?")
    assert rows[2].startswith("3  👀 #3 add the retry — in review") and "opens it" in rows[2]
    assert rows[3].startswith("4  ⛔ #2 fix the login race — no API key")
    assert wanted == ["coder-auth"]
    assert reader.startswith("fix the login race\n#2 · blocked")


# --- Work ---------------------------------------------------------------------------------------


@SIZES
def test_work_groups_doing_blocked_review_to_do_and_folds_done(
    board: dict[str, Any], size: tuple[int, int]
) -> None:
    async def go(pilot: Any, app: Host, panel: BoardPanel) -> dict[str, Any]:
        table = panel.query_one("#tasks", DataTable)
        seen: dict[str, Any] = {
            "columns": [column.label.plain for column in table.columns.values()],
            "rows": _row_keys(panel),
            "doing": [cell.plain for cell in table.get_row("tsk_0001")],
        }
        await pilot.press("d")
        await settle_page(app)
        seen["unfolded"] = _row_keys(panel)
        return seen

    seen = drive(size, go)
    assert seen["columns"] == list(TASK_COLUMNS) == ["#", "st", "title", "owner", "age"]
    assert seen["rows"] == [
        f"{GROUP_ROW}doing",
        "tsk_0001",
        f"{GROUP_ROW}blocked",
        "tsk_0002",
        f"{GROUP_ROW}review",
        "tsk_0003",
        f"{GROUP_ROW}todo",
        "tsk_0004",
        f"{GROUP_ROW}done",
    ], "Done is folded at first"
    number, _icon, title, owner, age = seen["doing"]
    assert (number, title, owner, age) == ("1", "wire auth", "coder-auth", "1m")
    assert seen["unfolded"][-2:] == [f"{GROUP_ROW}done", "tsk_0005"]


# --- the Stream ---------------------------------------------------------------------------------


@SIZES
def test_the_stream_is_one_row_a_line_with_chips_for_agent_kind_and_status(
    board: dict[str, Any], size: tuple[int, int]
) -> None:
    async def go(pilot: Any, app: Host, panel: BoardPanel) -> tuple[int, int, str]:
        await pilot.press("b") if size[0] < board_mod.WIDE_FROM else None
        await settle_page(app)
        feed = panel.query_one("#feed", OptionList)
        return feed.option_count, feed.virtual_size.height, str(feed.get_option("evt_3").prompt)

    options, rows, blocked = drive(size, go)
    assert rows == options == len(EVENTS)
    assert " 🔨 coder-auth " in blocked and " 🧱 hit a wall on " in blocked
    assert " ⛔ blocked " in blocked and "fix the login race — no API key" in blocked


@SIZES
def test_slash_filters_the_stream_to_the_lines_that_match(
    board: dict[str, Any], size: tuple[int, int]
) -> None:
    async def go(pilot: Any, app: Host, panel: BoardPanel) -> tuple[list[str], str, int]:
        await pilot.press("slash", *"LOGIN")
        await settle_page(app)
        feed = panel.query_one("#feed", OptionList)
        shown = [str(feed.get_option_at_index(i).id) for i in range(feed.option_count)]
        title = str(feed.border_title)
        await pilot.press("escape")
        await settle_page(app)
        return shown, title, feed.option_count

    shown, title, after = drive(size, go)
    assert shown == ["evt_1", "evt_2", "evt_3"], "case-insensitive, over the line as shown"
    assert "filter “LOGIN”: 3 of 4" in title
    assert after == len(EVENTS), "Esc clears the filter"


@SIZES
def test_scrolled_up_the_stream_says_how_many_came_in_and_since_when(
    board: dict[str, Any], size: tuple[int, int]
) -> None:
    board["events"] = [_event(seq, CODER.id, "note", f"line {seq}") for seq in range(1, 61)]

    async def go(pilot: Any, app: Host, panel: BoardPanel) -> str:
        await _stream_in_view(pilot, app, panel)
        feed = panel.query_one("#feed", OptionList)
        feed.scroll_to(y=0, animate=False)
        await settle_page(app)
        board["events"] = [_event(61, CODER.id, "note", "line 61")]
        panel.refresh_data()
        await settle_page(app)
        return str(feed.border_subtitle)

    since = watch.clock(_event(61, None, "note", "").created_at)
    assert drive(size, go) == f"↓ 1 new since {since} — End shows them"


# --- the reader ---------------------------------------------------------------------------------


@SIZES
def test_enter_on_a_task_opens_the_reader_full_height_with_its_history(
    board: dict[str, Any], size: tuple[int, int]
) -> None:
    async def go(pilot: Any, app: Host, panel: BoardPanel) -> dict[str, Any]:
        table = panel.query_one("#tasks", DataTable)
        table.move_cursor(row=table.get_row_index("tsk_0002"))
        await pilot.press("enter")
        await settle_page(app)
        seen = {
            "text": panel.reader_text,
            "height": panel.query_one("#reader").outer_size.height,
            "main": panel.query_one("#board-main").display,
        }
        await pilot.press("escape")
        await settle_page(app)
        seen["back"] = (panel.query_one("#board-main").display, app.focused and app.focused.id)
        return seen

    seen = drive(size, go)
    history = seen["text"].split("\nhistory\n", 1)[1].splitlines()
    assert seen["text"].startswith("fix the login race\n#2 · blocked")
    assert [line.split("  ", 1)[1] for line in history] == [
        "🧭 manager 📌 added: fix the login race",
        "🔨 coder-auth 🤝 claimed: fix the login race",
        "🔨 coder-auth 🧱 hit a wall on: fix the login race — no API key",
    ]
    assert seen["main"] is False and seen["height"] >= size[1] - 10, "it takes the board's height"
    assert seen["back"] == (True, "tasks")


# --- below 120 columns --------------------------------------------------------------------------


def test_below_120_columns_work_and_the_stream_take_turns_and_b_switches_them(
    board: dict[str, Any],
) -> None:
    async def go(pilot: Any, app: Host, panel: BoardPanel) -> list[tuple[bool, bool, bool]]:
        def shown() -> tuple[bool, bool, bool]:
            return (
                panel.query_one("#board-side").display,
                panel.query_one("#feedpane").display,
                panel.query_one("#board-switch").display,
            )

        seen = [shown()]
        await pilot.press("b")
        await settle_page(app)
        seen.append(shown())
        await pilot.resize_terminal(140, 40)
        await settle_page(app)
        seen.append(shown())
        return seen

    narrow, flipped, wide = drive((100, 30), go)
    assert narrow == (True, False, True), "Work first, under the switch"
    assert flipped == (False, True, True), "b: the Stream"
    assert wide == (True, True, False), "from 120 columns, side by side, no switch"


# --- every click has a key ----------------------------------------------------------------------


def _clickable(widget: Widget) -> bool:
    """A part a click acts on: a button, a row table, a list, a field, a line, a lane row."""
    return isinstance(widget, Button | DataTable | OptionList | Input | Divider | LaneRow)


def _bound(panel: BoardPanel, widget: Widget, key: str) -> bool:
    """Whether ``key`` reaches an action from ``widget``: its own bindings, or the panel's."""
    return any(key in node._bindings.key_to_bindings for node in (widget, panel))


@SIZES
def test_every_part_of_the_board_a_click_acts_on_has_a_key(
    board: dict[str, Any], size: tuple[int, int]
) -> None:
    async def go(pilot: Any, app: Host, panel: BoardPanel) -> list[tuple[str, str | None, bool]]:
        found = []
        for widget in panel.walk_children(Widget):
            if _clickable(widget):
                key = click_key(widget)
                bound = key is not None and _bound(panel, widget, key)
                found.append((f"{type(widget).__name__}#{widget.id}", key, bound))
        return found

    found = drive(size, go)
    assert {name.split("#")[0] for name, _key, _bound in found} >= {
        "PickableTable",
        "StreamList",
        "SearchField",
        "Divider",
        "LaneRow",
        "SwitchButton",
    }
    assert [entry for entry in found if not entry[2]] == [], "a click with no key"


# --- remembered per project ---------------------------------------------------------------------


@SIZES
def test_a_fold_and_a_filter_survive_a_restart_and_stay_with_their_project(
    tmp_path: Path, size: tuple[int, int]
) -> None:
    from aisquare.cli import watch as watch_mod

    first_dir, other_dir = tmp_path / "first", tmp_path / "other"
    first_dir.mkdir()
    other_dir.mkdir()
    first = team_service.activate(first_dir)
    other = team_service.activate(other_dir)

    async def session(project: ProjectInfo, act: bool) -> tuple[list[str], str]:
        app = watch_mod._build_app_class(interval=3600.0, project=project)()
        async with app.run_test(size=size) as pilot:
            await pilot.pause()
            if act:
                app.board.query_one("#tasks").focus()
                await pilot.press("d", "slash", *"auth", "enter")
                await pilot.pause()
            return sorted(app.board.folds), app.board.stream_filter

    changed = asyncio.run(session(first, True))
    again = asyncio.run(session(first, False))
    elsewhere = asyncio.run(session(other, False))

    assert changed == ([], "auth"), "the premise: Done unfolded, the Stream filtered"
    assert again == ([], "auth"), "the same project after a restart"
    assert elsewhere == (["done"], ""), "another project keeps its own (here, the defaults)"


def test_the_readers_keys_are_its_own_while_it_is_open(board: dict[str, Any]) -> None:
    """The reader covers Work and the Stream: their keys are off (and out of the footer) while
    it is open, so ``d`` does not fold a table nobody can see; Esc brings them back."""

    async def go(pilot: Any, app: Host, panel: BoardPanel) -> tuple[bool, bool, bool, bool]:
        panel.open_reader("tsk_0002")
        await settle_page(app)
        offered = bool(panel.check_action("toggle_done", ()))
        await pilot.press("d")
        await settle_page(app)
        folded_while_reading = "done" in panel.folds
        await pilot.press("escape")
        await settle_page(app)
        await pilot.press("d")
        await settle_page(app)
        return (
            offered,
            folded_while_reading,
            bool(panel.check_action("toggle_done", ())),
            ("done" in panel.folds),
        )

    offered, still_folded, back, folded_after = drive((140, 40), go)
    assert (offered, still_folded) == (False, True), "d is off while the reader is open"
    assert (back, folded_after) == (True, False), "back on the board, d unfolds Done"


# --- review of #573 -----------------------------------------------------------------------------


@SIZES
def test_only_the_lanes_drawn_rows_act_and_more_opens_the_rest(
    board: dict[str, Any], size: tuple[int, int]
) -> None:
    """Major (1): with more items than rows, the keys past the drawn rows and a click on
    ``+ N more`` opened cards that were not on screen. Now only a drawn row acts, and ``+ N
    more`` (its key or a click) opens the lane to its nine rows."""
    board["tasks"] = [_task(n, f"stuck {n}", "blocked", CODER.id) for n in range(1, 7)]

    async def go(pilot: Any, app: Host, panel: BoardPanel) -> dict[str, Any]:
        def drawn() -> list[str]:
            return [str(row.render()) for row in panel.query(LaneRow) if row.display]

        seen: dict[str, Any] = {"collapsed": drawn()}
        await pilot.press("6")  # past the drawn rows
        await settle_page(app)
        seen["after_6"] = panel._reading
        await pilot.click("#lane-4")  # "+ N more"
        await settle_page(app)
        seen["expanded"] = drawn()
        seen["after_more"] = panel._reading
        await pilot.press("6")
        await settle_page(app)
        seen["opened"] = panel._reading
        return seen

    seen = drive(size, go)
    lane = [f"tsk_{n:04d}" for n in range(1, 7)]
    assert len(seen["collapsed"]) == 5 and seen["collapsed"][4].startswith("5     + 4 more")
    assert seen["after_6"] is None, "a key past the drawn rows opens nothing"
    assert seen["after_more"] is None, "+ N more opens the lane, not a card"
    assert len(seen["expanded"]) == 8, "the waiting agent, the question and the six cards"
    assert seen["opened"] == lane[3], "6: the sixth item, drawn now (the fourth card)"


def test_a_remembered_column_width_leaves_the_narrow_column_whole(board: dict[str, Any]) -> None:
    """Major (2): Work's remembered width (70) was restored a refresh after the single column
    took the whole width, so Work was 70 of 100 beside a blank band; and widening showed the
    stylesheet's 58, the remembered 70 lost. Now it fills the column, and comes back wide."""
    from aisquare.core.state_file import update_state

    update_state(board_mod.COLUMN_WIDTH_KEY, 70)

    async def go(pilot: Any, app: Host, panel: BoardPanel) -> tuple[int, int, int]:
        await settle_page(app)
        await pilot.pause()
        narrow = (panel.query_one("#board-side").outer_size.width, panel.size.width)
        await pilot.resize_terminal(140, 40)
        await settle_page(app)
        await pilot.pause()
        return *narrow, panel.query_one("#board-side").outer_size.width

    side, room, wide = drive((100, 30), go)
    assert side == room == 100, "Work has the whole column"
    assert wide == 70, "the remembered width, beside the Stream again"


def test_a_click_on_the_heading_under_the_cursor_folds_it_once(board: dict[str, Any]) -> None:
    """Minor (3): DataTable selects a click on the cell under its cursor itself; the table's own
    pick on top folded the heading twice, so not at all."""

    async def go(pilot: Any, app: Host, panel: BoardPanel) -> tuple[str, bool]:
        table = panel.query_one("#tasks", DataTable)
        table.move_cursor(row=0, column=0)
        await settle_page(app)
        under = str(table.coordinate_to_cell_key(table.cursor_coordinate).row_key.value)
        await pilot.click("#tasks", offset=(2, 2))  # row 0, the # column
        await settle_page(app)
        return under, "doing" in panel.folds

    under, folded = drive((140, 40), go)
    assert under == f"{GROUP_ROW}doing", "the premise: the cursor is on Doing's heading"
    assert folded, "one click, one fold"


@SIZES
def test_the_new_lines_marker_counts_only_what_the_filter_shows(
    board: dict[str, Any], size: tuple[int, int]
) -> None:
    """Minor (6): scrolled up under a filter, lines the filter hides were counted as new."""
    board["events"] = [_event(seq, CODER.id, "note", f"keep {seq}") for seq in range(1, 61)]

    async def go(pilot: Any, app: Host, panel: BoardPanel) -> int:
        await pilot.press("slash", *"keep", "enter")
        await settle_page(app)
        feed = panel.query_one("#feed", OptionList)
        feed.scroll_to(y=0, animate=False)
        await settle_page(app)
        board["events"] = [
            _event(61, CODER.id, "note", "drop this"),
            _event(62, CODER.id, "note", "keep 62"),
            _event(63, CODER.id, "note", "drop that"),
        ]
        panel.refresh_data()
        await settle_page(app)
        return panel.unseen

    assert drive(size, go) == 1, "one line the filter shows came in below"


# --- review of #573, round 2 (the tester's #695) -------------------------------------------------


async def _hide_stream(pilot: Any, app: Host, panel: BoardPanel) -> None:
    """The Stream out of sight: below 120 columns, Work takes the column (``b``); wide, the
    reader covers it."""
    if panel._wide():
        panel.open_reader("tsk_0002")
    else:
        await pilot.press("b")
    await settle_page(app)


async def _show_stream(pilot: Any, app: Host, panel: BoardPanel) -> None:
    await pilot.press("escape" if panel._reading is not None else "b")
    await settle_page(app)
    await pilot.pause()


@SIZES
@pytest.mark.parametrize("scrolled_up", [False, True], ids=["following", "scrolled-up"])
def test_lines_that_come_in_while_the_stream_is_out_of_sight_are_shown_or_counted(
    board: dict[str, Any], size: tuple[int, int], scrolled_up: bool
) -> None:
    """(1): lines that came in while Work had the column were below the view when the Stream
    came back, with no "N new" — scroll_end on a hidden list does nothing. Following, the
    Stream catches up as it shows; scrolled up, the marker counts them."""
    board["events"] = [_event(seq, CODER.id, "note", f"line {seq}") for seq in range(1, 61)]

    async def go(pilot: Any, app: Host, panel: BoardPanel) -> tuple[bool, int, str]:
        await _stream_in_view(pilot, app, panel)
        feed = panel.query_one("#feed", OptionList)
        if scrolled_up:
            feed.scroll_to(y=0, animate=False)
            await settle_page(app)
        await _hide_stream(pilot, app, panel)
        board["events"] = [_event(seq, CODER.id, "note", f"line {seq}") for seq in range(61, 66)]
        panel.refresh_data()
        await settle_page(app)
        await _show_stream(pilot, app, panel)
        return feed.scroll_y == feed.max_scroll_y, panel.unseen, str(feed.border_subtitle or "")

    at_end, unseen, marker = drive(size, go)
    if scrolled_up:
        assert not at_end and unseen == 5 and marker.startswith("↓ 5 new since ")
    else:
        assert at_end and unseen == 0 and not marker, "it caught up as it showed"


@pytest.mark.parametrize("how", ["after-a-resize", "after-a-restart"])
def test_work_alone_fills_the_column_however_the_stream_was_left(
    board: dict[str, Any], how: str
) -> None:
    """(2): with the Stream showing, a resize wrote the side-by-side width cap (67 of 100), and
    ``b`` back to Work left it there; a restart on the Stream (remembered) was the same."""
    if how == "after-a-restart":
        from aisquare.core.state_file import update_state

        update_state(
            board_mod.VIEW_KEY + PROJECT.id, {"folds": ["done"], "filter": "", "pane": "stream"}
        )

    async def go(pilot: Any, app: Host, panel: BoardPanel) -> tuple[bool, int, int]:
        if how == "after-a-resize":
            await pilot.press("b")  # the Stream
            await settle_page(app)
            await pilot.resize_terminal(101, 30)
            await settle_page(app)
        on_stream = panel.query_one("#feedpane").display
        await pilot.press("b")  # back to Work
        await settle_page(app)
        await pilot.pause()
        return on_stream, panel.query_one("#board-side").outer_size.width, panel.size.width

    on_stream, side, room = drive((100, 30), go)
    assert on_stream, "the premise: the Stream had the column"
    assert side == room, f"Work {side} of {room}"
