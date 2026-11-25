"""What needs you, over the board (#286): the strip, the empty state, and what they read.

The strip counts with the captain's attention queue (``queue.observe``) over what the
board's own tick read, so these pin both halves: the counts it shows for a board, and
that showing them asks nothing of tmux or the store beyond the tick.
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable, Iterator
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any, TypeVar

import pytest
from textual.app import App, ComposeResult
from textual.widgets import Button, DataTable

from aisquare.cli import watch
from aisquare.cli.ui import board as board_mod
from aisquare.cli.ui import board_parts
from aisquare.cli.ui.board import EMPTY_ROW, NOTHING_NEEDS_YOU, BoardPanel, NeedsYou, needs_you
from aisquare.core import tmux as tmux_core
from aisquare.models import ProjectInfo, TaskStatus, TeamEvent, TeamSession, TeamTask
from aisquare.services import fleet as fleet_service
from aisquare.services import team as team_service
from aisquare.services.captain import queue
from tests.ui_workers import settle_page

T = TypeVar("T")
#: The panel asks ``needs_you`` with the real clock, and a question counts for
#: ``queue.QUESTION_HORIZON`` (a day): a fixed date here stopped counting the panel's
#: question a day after it was written.
NOW = datetime.now(UTC).replace(microsecond=0)
PROJECT = ProjectInfo(id="prj_board", root=Path("/tmp/board-project"))


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


def _task(number: int, title: str, status: TaskStatus) -> TeamTask:
    return TeamTask(
        id=f"tsk_{number:04d}",
        project_id=PROJECT.id,
        number=number,
        key=f"task-{number}",
        title=title,
        status=status,
        created_at=NOW - timedelta(minutes=30),
        updated_at=NOW - timedelta(minutes=5),
    )


def _event(seq: int, session_id: str, kind: str, text: str, **extra: Any) -> TeamEvent:
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
BOARD_TASKS = [
    _task(1, "wire auth", "doing"),
    _task(2, "fix the login race", "blocked"),
    _task(3, "add the retry", "review"),
    _task(4, "write the docs", "todo"),
]


def test_a_board_counts_what_waits_on_you_and_nothing_else() -> None:
    """1 blocked, 1 in review and 1 pane on a permission prompt: those three, by the
    captain queue's own rules — a doing or todo card, and a working manager, are not."""
    found = needs_you(PROJECT, [MANAGER, CODER], BOARD_TASKS, [], seq=0, now=NOW)

    assert found.tasks == {"blocked": ["tsk_0002"], "review": ["tsk_0003"]}
    assert found.agents == {"waiting": ["coder-auth"]}
    assert found.line() == (
        "needs you: 🔔 1 waiting · 👀 1 in review · ⛔ 1 blocked — click one to go to it"
    )


def test_a_card_a_reviewer_gated_and_a_quiet_board_need_nothing() -> None:
    entered = {"created_at": NOW - timedelta(hours=2), "updated_at": NOW - timedelta(hours=1)}
    tasks = [_task(3, "add the retry", "review").model_copy(update=entered)]
    gated = [_event(1, "ses_reviewer", "result", "PASS", task_id="tsk_0003")]

    found = needs_you(PROJECT, [MANAGER], tasks, gated, seq=1, now=NOW)

    assert found == NeedsYou() and found.line() == NOTHING_NEEDS_YOU


def test_a_question_counts_until_its_asker_speaks_again() -> None:
    """The queue's question rows are resolved by the owner, which a board cannot see; the
    strip counts one while it is still the asker's last word and the asker is live."""
    asked = [_event(1, "ses_manager", "question", "staging or prod first?")]
    open_q = needs_you(PROJECT, [MANAGER], [], asked, seq=1, now=NOW)
    moved_on = [*asked, _event(2, "ses_manager", "note", "going with staging")]
    answered = needs_you(PROJECT, [MANAGER], [], moved_on, seq=2, now=NOW)
    gone = [MANAGER.model_copy(update={"ended_at": NOW})]
    ended = needs_you(PROJECT, gone, [], asked, seq=1, now=NOW)
    to_the_manager = [_event(1, "ses_coder", "question", "which branch?", to_role="manager")]
    not_mine = needs_you(PROJECT, [MANAGER, CODER], [], to_the_manager, seq=1, now=NOW)

    assert open_q.agents == {"question": ["manager"]} and open_q.count("question") == 1
    assert answered.count("question") == 0
    assert ended.count("question") == 0
    assert not_mine.count("question") == 0, "a coder's question to a live manager is its"


# --- the panel ------------------------------------------------------------------------


class Host(App[None]):
    def __init__(self) -> None:
        super().__init__()
        self.wanted: list[tuple[str, str]] = []

    def compose(self) -> ComposeResult:
        yield BoardPanel(PROJECT, interval=3600, id="board")

    def on_board_panel_agent_wanted(self, event: BoardPanel.AgentWanted) -> None:
        self.wanted.append((event.project_id, event.label))


@pytest.fixture
def board(monkeypatch: pytest.MonkeyPatch) -> Iterator[dict[str, Any]]:
    """The board's one read scripted; any other read recorded and refused: a store
    session opened by ``services.team`` or the captain's queue, the fleet's agents (which
    ask tmux), or tmux itself. The strip may ask for nothing beyond the tick."""
    frame: dict[str, Any] = {"sessions": [MANAGER, CODER], "tasks": BOARD_TASKS, "events": []}
    reads: list[str] = []

    def board_read(**_kwargs: Any) -> team_service.BoardRead:
        reads.append("board_read")
        return team_service.BoardRead(
            PROJECT, frame["sessions"], frame["tasks"], frame["events"], {}
        )

    def refused(name: str) -> Callable[..., Any]:
        def read(*_a: Any, **_k: Any) -> Any:
            reads.append(name)
            raise AssertionError(f"the board read {name}")

        return read

    monkeypatch.setattr(team_service, "board_read", board_read)
    monkeypatch.setattr(team_service, "store_session", refused("team.store_session"))
    monkeypatch.setattr(queue, "store_session", refused("queue.store_session"))
    monkeypatch.setattr(fleet_service, "list_agents", refused("fleet.list_agents"))
    monkeypatch.setattr(tmux_core, "_tmux", refused("tmux"))
    frame["reads"] = reads
    yield frame


def drive(fn: Callable[[Host, BoardPanel], Awaitable[T]]) -> T:
    async def run() -> T:
        app = Host()
        async with app.run_test(size=(140, 40)) as pilot:
            await pilot.pause()
            await settle_page(app)
            return await fn(app, app.query_one(BoardPanel))

    return asyncio.run(run())


class StripHost(App[None]):
    """``NeedsYouStrip`` alone, shown :func:`needs_you`'s counts for the scripted board: how the
    app-wide strip (M7) hosts it. Inside the board, the lane lists each thing instead (B2)."""

    def __init__(self, needs: NeedsYou) -> None:
        super().__init__()
        self.needs = needs
        self.picked: list[str] = []

    def compose(self) -> ComposeResult:
        yield board_mod.NeedsYouStrip(id="needs-you")

    def on_mount(self) -> None:
        self.query_one(board_mod.NeedsYouStrip).show(self.needs)

    def on_needs_you_strip_picked(self, event: board_mod.NeedsYouStrip.Picked) -> None:
        self.picked.append(event.kind)


def strip_line(strip: board_mod.NeedsYouStrip) -> str:
    """The strip as it shows: its lead, each button shown, its tail."""
    parts: list[str] = []
    for widget in strip.children:
        if not widget.display:
            continue
        if isinstance(widget, Button):
            parts.append(str(widget.label))
        else:
            text = getattr(getattr(widget, "visual", None), "plain", "")
            if text.strip():
                parts.append(text.strip())
    return " ".join(parts)


def drive_strip(
    width: int, needs: NeedsYou, fn: Callable[[StripHost, board_mod.NeedsYouStrip], Awaitable[T]]
) -> T:
    async def run() -> T:
        app = StripHost(needs)
        async with app.run_test(size=(width, 30)) as pilot:
            await pilot.pause()
            await settle_page(app)
            await pilot.pause()
            return await fn(app, app.query_one(board_mod.NeedsYouStrip))

    return asyncio.run(run())


def _needs(frame: dict[str, Any]) -> NeedsYou:
    return needs_you(
        PROJECT, frame["sessions"], frame["tasks"], frame["events"], seq=len(frame["events"])
    )


def lane_rows(panel: BoardPanel) -> list[str]:
    """The lane as it shows: its lead, then each row shown."""
    lead = str(panel.query_one("#lane-lead").render())
    rows = [str(row.render()) for row in panel.query(board_parts.LaneRow) if row.display]
    return [lead, *rows]


def test_the_lane_lists_what_needs_you_and_reads_nothing_beyond_the_tick(
    board: dict[str, Any],
) -> None:
    async def go(app: Host, panel: BoardPanel) -> tuple[list[str], list[str]]:
        before = len(board["reads"])
        panel.refresh_data()
        await settle_page(app)
        return lane_rows(panel), board["reads"][before:]

    rows, reads = drive(go)
    assert [row.split("   ")[0] for row in rows[1:]] == [
        "1  🔔 coder-auth waits for you in its pane",
        "2  👀 #3 add the retry — in review",
        "3  ⛔ #2 fix the login race — blocked",
    ]
    assert reads == ["board_read"], "one tick, one read: the lane asked for nothing more"


def test_the_strip_shows_the_counts(board: dict[str, Any]) -> None:
    async def go(app: StripHost, strip: board_mod.NeedsYouStrip) -> str:
        return strip_line(strip)

    line = drive_strip(140, _needs(board), go)
    assert line == "needs you: 🔔 1 waiting 👀 1 in review ⛔ 1 blocked — click one to go to it"


def test_nothing_needs_you_and_no_open_tasks_say_so(board: dict[str, Any]) -> None:
    board["sessions"] = [MANAGER]
    board["tasks"] = [_task(1, "shipped", "done")]

    async def go(app: Host, panel: BoardPanel) -> tuple[list[str], list[Any], str]:
        table = panel.query_one("#tasks", DataTable)
        rows = [key.value for key in table.rows]
        body = table.get_row(EMPTY_ROW)[2].plain
        return lane_rows(panel), rows, body

    lane, rows, body = drive(go)
    assert lane == [NOTHING_NEEDS_YOU]
    assert rows == [EMPTY_ROW, f"{board_mod.GROUP_ROW}done"], "the empty state, over Done (B2)"
    assert " ".join(body.split()) == board_mod.EMPTY_TASKS


def test_a_lane_row_goes_to_what_it_names(board: dict[str, Any]) -> None:
    async def go(app: Host, panel: BoardPanel) -> tuple[Any, str, str, list[tuple[str, str]]]:
        panel.query_one("#tasks").focus()
        panel.action_lane_item(2)  # ⛔ the blocked card
        await settle_page(app)
        table = panel.query_one("#tasks", DataTable)
        cursor = table.coordinate_to_cell_key(table.cursor_coordinate).row_key.value
        reading = panel.reader_text
        panel.action_close_reader()
        await settle_page(app)
        panel.action_lane_item(0)  # 🔔 the waiting agent
        await settle_page(app)
        return cursor, panel.detail_text, reading, app.wanted

    cursor, detail, reading, wanted = drive(go)
    assert cursor == "tsk_0002" and "fix the login race" in detail
    assert reading.startswith("fix the login race"), "the card opens in the reader"
    assert wanted == [(PROJECT.id, "coder-auth")], "the pane's agent, for the shell to select"


def test_a_count_on_the_strip_says_which_kind_it_goes_to(board: dict[str, Any]) -> None:
    async def go(app: StripHost, strip: board_mod.NeedsYouStrip) -> list[str]:
        strip.query_one("#needs-you-blocked", Button).press()
        await settle_page(app)
        return app.picked

    assert drive_strip(140, _needs(board), go) == ["blocked"]


WORDED = "needs you: 🔔 1 waiting ❓ 1 question 👀 1 in review ⛔ 1 blocked"


@pytest.mark.parametrize(
    ("width", "line"),
    [
        (140, f"{WORDED} — click one to go to it"),
        (80, WORDED),
        (50, "needs you: 🔔 1 ❓ 1 👀 1 ⛔ 1"),
    ],
)
def test_the_strip_fits_the_width_it_has(board: dict[str, Any], width: int, line: str) -> None:
    """Inside `asq` at 80 columns the board is about 50 wide: the whole line ran off it,
    two counts with it. It drops its tail, then its words, before anything is cut."""
    board["events"] = [_event(1, "ses_manager", "question", "staging or prod first?")]

    async def go(app: StripHost, strip: board_mod.NeedsYouStrip) -> tuple[str, list[str]]:
        cut = [
            str(child.id)
            for child in strip.children
            if child.display and child.region.right > strip.region.right
        ]
        return strip_line(strip), cut

    shown, cut = drive_strip(width, _needs(board), go)
    assert shown == line
    assert cut == [], f"run past the strip at {width} columns: {cut}"


def test_nothing_needs_you_shortens_rather_than_runs_off(board: dict[str, Any]) -> None:
    async def go(app: StripHost, strip: board_mod.NeedsYouStrip) -> str:
        return strip_line(strip)

    assert drive_strip(50, NeedsYou(), go) == board_mod.NOTHING_SHORT
    assert drive_strip(140, NeedsYou(), go) == NOTHING_NEEDS_YOU


def test_in_the_shell_every_lane_row_ends_inside_the_board_at_80_columns(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Review of #324, round 2 (the tmux walk), for the lane (B2): the real shell, its Board tab
    shown, the terminal shrunk to 80 columns: every row the lane shows ends inside it."""
    from textual.widgets import TabbedContent

    from aisquare.cli.ui.app import FleetApp
    from aisquare.cli.ui.sidebar import ProjectSelected
    from tests.test_ui_spawn import register

    project = register(tmp_path / "alpha", project_id="prj_alpha")
    events = [_event(1, "ses_manager", "question", "staging or prod first? " * 4)]
    monkeypatch.setattr(
        team_service,
        "board_read",
        lambda **_k: team_service.BoardRead(project, [MANAGER, CODER], BOARD_TASKS, events, {}),
    )
    monkeypatch.setattr(fleet_service, "list_agents", lambda project, live_only=True: [])

    async def run() -> tuple[int, list[tuple[str, int]]]:
        app = FleetApp(refresh_seconds=3600, doctor=lambda: [], accounts=None)
        async with app.run_test(size=(160, 40)) as pilot:
            await pilot.pause()
            await settle_page(app)
            app.post_message(ProjectSelected(project.id))
            await settle_page(app)
            app.query_one(f"#project-{project.id}", TabbedContent).active = "tab-board"
            await settle_page(app)
            await pilot.resize_terminal(80, 40)
            await settle_page(app)
            await pilot.pause()
            lane = app.query_one(board_parts.NeedsYouLane)
            shown = [
                (str(row.id), row.region.right)
                for row in lane.query(board_parts.LaneRow)
                if row.display
            ]
            return lane.region.right, shown

    right, shown = asyncio.run(run())
    assert [name for name, _ in shown] == ["lane-0", "lane-1", "lane-2", "lane-3"]
    assert all(end <= right for _, end in shown), f"past the lane's edge {right}: {shown}"


def test_a_restart_tells_the_earlier_session_s_feed_lines_apart(board: dict[str, Any]) -> None:
    """#140: the feed names a line's author as the left pane names its agent. A restart gives
    the label to a new session; the earlier one's lines, drawn when the name was its own, are
    redrawn with the time it started, and the new one's keep the plain name."""
    from textual.widgets import OptionList

    earlier = CODER.model_copy(update={"id": "ses_earlier", "state": "working"})
    board["sessions"] = [MANAGER, earlier]
    board["events"] = [_event(1, earlier.id, "note", "first try")]
    restarted = CODER.model_copy(
        update={"id": "ses_restarted", "state": "working", "started_at": NOW}
    )

    async def go(app: Host, panel: BoardPanel) -> tuple[str, str, str]:
        feed = panel.query_one("#feed", OptionList)
        before = str(feed.get_option("evt_1").prompt)
        board["sessions"] = [
            MANAGER,
            earlier.model_copy(update={"ended_at": NOW}),
            restarted,
        ]
        board["events"] = [_event(2, restarted.id, "note", "second try")]
        panel.refresh_data()
        await settle_page(app)
        return before, str(feed.get_option("evt_1").prompt), str(feed.get_option("evt_2").prompt)

    before, redrawn, new = drive(go)
    stamp = f"{earlier.started_at.astimezone():%H:%M}"
    # A Stream line's chips (B2): the agent's name, then the kind's, then the text.
    assert " coder-auth   💬 noted  first try" in before
    assert f" coder-auth ({stamp})   💬 noted  first try" in redrawn
    assert " coder-auth   💬 noted  second try" in new


def test_a_long_name_never_wraps_the_sessions_block(board: dict[str, Any]) -> None:
    """#140: names are longer than ``role·<id>``. The sessions block is one row a session, its
    end cut with ``…`` at the border, so it never grows and pushes the task table down."""
    from textual.widgets import Static

    long = CODER.model_copy(
        update={"label": "reviewer-board-audit-of-the-whole-release", "state": "waiting"}
    )
    board["sessions"] = [MANAGER, long]

    async def go(app: Host, panel: BoardPanel) -> int:
        panel.refresh_data()
        await settle_page(app)
        return panel.query_one("#sessions", Static).region.height

    # Two sessions, a row each, the block's closing newline, and the border's two.
    assert drive(go) == 2 + 1 + 2, "a long name wrapped the sessions block"


# --- B1: the board audit's quick bundle and its "Next" list ------------------------------------


class Standalone(Host):
    """``board -w``'s panel: no shell around it to select an agent in."""

    def compose(self) -> ComposeResult:
        yield BoardPanel(PROJECT, interval=3600, id="board", standalone=True)


def test_in_board_w_a_question_count_shows_the_question(board: dict[str, Any]) -> None:
    """B1 #10: in ``board -w`` a ❓ count asked for an agent nobody could select, and nothing
    happened. It shows the question now, picked in the feed; 🔔 says where the agent waits."""
    from textual.widgets import OptionList

    board["events"] = [
        _event(1, "ses_manager", "note", "starting"),
        _event(2, "ses_manager", "question", "staging or prod first?"),
    ]

    async def run() -> tuple[str, str | None, str, list[tuple[str, str]]]:
        app = Standalone()
        async with app.run_test(size=(140, 40)) as pilot:
            await pilot.pause()
            await settle_page(app)
            panel = app.query_one(BoardPanel)
            await pilot.click("#lane-1")  # ❓ the manager's question (the lane, B2)
            await settle_page(app)
            feed = panel.query_one("#feed", OptionList)
            picked = feed.highlighted
            asked = panel.detail_text
            await pilot.click("#lane-0")  # 🔔 the waiting agent
            await settle_page(app)
            option = feed.get_option_at_index(picked).id if picked is not None else None
            return asked, option, panel.detail_text, app.wanted

    asked, option, waiting, wanted = asyncio.run(run())
    assert asked.startswith("question by manager") and "staging or prod first?" in asked
    assert option == "evt_2", "the question's own line is picked in the feed"
    assert "coder-auth waits for you in its pane" in waiting
    assert wanted == [], "nothing asked of a shell that is not there"


def test_scrolled_up_the_feed_stays_put_and_counts_what_came_in(board: dict[str, Any]) -> None:
    """B1 #11: every new line scrolled the feed to its end, under the person reading further
    up. Scrolled up, the view stays and the feed's foot counts ``↓ N new``; back at the end
    (End, or scrolling down), the count goes and the feed follows again."""
    from textual.widgets import OptionList

    board["events"] = [_event(seq, "ses_coder", "note", f"line {seq}") for seq in range(1, 61)]

    async def go(app: Host, panel: BoardPanel) -> dict[str, Any]:
        feed = panel.query_one("#feed", OptionList)
        seen: dict[str, Any] = {"followed": feed.scroll_y == feed.max_scroll_y > 0}
        feed.scroll_to(y=3, animate=False)
        await settle_page(app)
        board["events"] = [_event(seq, "ses_coder", "note", f"line {seq}") for seq in (61, 62, 63)]
        panel.refresh_data()
        await settle_page(app)
        seen["stayed"] = feed.scroll_y
        seen["marker"] = (panel.unseen, str(feed.border_subtitle))
        feed.focus()
        feed.action_last()  # End
        await settle_page(app)
        seen["cleared"] = (panel.unseen, str(feed.border_subtitle))
        board["events"] = [_event(64, "ses_coder", "note", "line 64")]
        panel.refresh_data()
        await settle_page(app)
        seen["follows"] = feed.scroll_y == feed.max_scroll_y
        return seen

    seen = drive(go)
    since = watch.clock(_event(61, "ses_coder", "note", "").created_at)
    assert seen["followed"], "the premise: at the end, the feed followed"
    assert seen["stayed"] == 3
    assert seen["marker"] == (3, f"↓ 3 new since {since} — End shows them")
    assert seen["cleared"] == (0, "")
    assert seen["follows"], "back at the end, it follows again"


def test_the_team_block_is_one_row_per_agent(board: dict[str, Any]) -> None:
    """B1 #12: a focus took a second row under its agent; it ends the agent's row now."""
    from textual.widgets import Static

    focused = CODER.model_copy(update={"focus": "wire the login\nthen the tests"})
    board["sessions"] = [MANAGER.model_copy(update={"focus": "plan the release"}), focused]

    async def go(app: Host, panel: BoardPanel) -> tuple[int, list[str]]:
        panel.refresh_data()
        await settle_page(app)
        block = panel.query_one("#sessions", Static)
        plain = getattr(block.visual, "plain", "")
        return block.region.height, [line for line in plain.splitlines() if line]

    height, rows = drive(go)
    assert height == 2 + 1 + 2, "two agents, a row each, the closing newline, the border"
    assert rows[0].startswith("🧭 manager") and rows[0].endswith("🎯 plan the release")
    assert rows[1].startswith("🔨 coder-auth") and "🎯 wire the login ⏎ then the tests" in rows[1]


def test_in_the_shell_at_90_columns_the_board_tab_answers_at_once_and_its_footer_fits(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """B1 #1, #2 and #9 in asq itself, at the screen the tester walks (90x30): a click on the
    Board tab gives the board the keys, so ``d`` answers with no click inside it; at that width
    Work and the Stream take turns under the switch (B2), and ``d`` unfolds Done in Work; and
    with the board focused, every key in the footer ends inside 90 columns."""
    from textual.widgets import Footer, TabbedContent

    from aisquare.cli.ui.app import FleetApp
    from aisquare.cli.ui.sidebar import ProjectSelected
    from tests.test_ui_spawn import register

    project = register(tmp_path / "alpha", project_id="prj_alpha")
    monkeypatch.setattr(
        team_service,
        "board_read",
        lambda **_k: team_service.BoardRead(project, [MANAGER, CODER], BOARD_TASKS, [], {}),
    )
    monkeypatch.setattr(fleet_service, "list_agents", lambda project, live_only=True: [])

    async def run() -> dict[str, Any]:
        app = FleetApp(refresh_seconds=3600, doctor=lambda: [], accounts=None)
        async with app.run_test(size=(90, 30)) as pilot:
            await pilot.pause()
            await settle_page(app)
            app.post_message(ProjectSelected(project.id))
            await settle_page(app)
            tabs = app.query_one(f"#project-{project.id}", TabbedContent)
            await pilot.click(tabs.query_one("#--content-tab-tab-board"))
            await settle_page(app)
            panel = app.query_one(BoardPanel)
            seen: dict[str, Any] = {"active": tabs.active}
            seen["focus"] = app.focused is not None and panel in app.focused.ancestors_with_self
            seen["switch"] = panel.query_one("#board-switch").display
            footer = app.screen.query_one(Footer)
            seen["keys"] = [
                (str(getattr(key, "key_display", "")), key.region.right)
                for key in footer.query("FooterKey")
            ]
            await pilot.press("d")
            await settle_page(app)
            seen["shown"] = panel.query_one("#board-side").display
            seen["done"] = "done" in panel.folds
            return seen

    seen = asyncio.run(run())
    assert seen["active"] == "tab-board"
    assert seen["focus"], "the tab's click left the keys outside the board"
    assert seen["switch"], "at 90 columns Work and the Stream take turns, under the switch (B2)"
    keys = {key for key, _ in seen["keys"]}
    assert {"b", "d", "a", "o", "/", "?"} <= keys, seen["keys"]
    assert all(right <= 90 for _, right in seen["keys"]), seen["keys"]
    assert seen["shown"] and not seen["done"], "d answered: Work shows, Done unfolded"


def test_no_new_lines_marker_while_the_feed_shows_its_last_line(board: dict[str, Any]) -> None:
    """Review of #554: with autoscroll off, a feed short enough to show every line counted the
    lines that came in as unseen, and only a scroll cleared the count, which a feed that does
    not overflow never has."""
    from textual.widgets import OptionList

    board["events"] = [_event(1, "ses_coder", "note", "line 1")]

    async def go(app: Host, panel: BoardPanel) -> tuple[float, int, str]:
        panel.action_toggle_autoscroll()  # off
        board["events"] = [_event(seq, "ses_coder", "note", f"line {seq}") for seq in (2, 3, 4)]
        panel.refresh_data()
        await settle_page(app)
        feed = panel.query_one("#feed", OptionList)
        return feed.max_scroll_y, panel.unseen, str(feed.border_subtitle)

    overflow, unseen, marker = drive(go)
    assert overflow == 0, "the premise: four lines fit the feed"
    assert (unseen, marker) == (0, "")
