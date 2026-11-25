"""L2 (the UX audit): one pane at a time below 70 columns, for SSH and phones.

Below ``Panes.ONE_PANE_BELOW`` columns the sidebar and the content never share the row: the
side shown is the one the keyboard is in, the escape key (``F12``) and the bar on top switch
sides, and a view asked for (a row's Enter or click, home) is the side shown. From 70 up
nothing changes. Crossing the line keeps the selection and the focus.

Driven as ``test_ui_shell.py`` drives the shell: the real store in the isolated home,
``list_agents`` scripted, and every tmux command held to the test's own socket (its
``no_real_tmux``, the in-memory tmux the shell tests run on).
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from textual.pilot import Pilot
from textual.widget import Widget

from aisquare.cli.ui import app as ui_app
from aisquare.cli.ui.app import FleetApp, Panes, PaneSwitch
from aisquare.cli.ui.divider import Divider
from aisquare.cli.ui.views.agent import AgentView
from tests.test_ui_shell import (
    Script,
    _needs_tmux,
    _state,
    _write_state,
    drive,
    fleet_app,
    row_for,
    seed,
    settle,
    status,
)
from tests.test_ui_shell import no_real_tmux as _no_real_tmux
from tests.test_ui_shell import script as _script

no_real_tmux = _no_real_tmux
script = _script

NARROW = (60, 24)
WIDE = (80, 24)
AGENT = "agt_a_coder-auth"


def _one_project(tmp_path: Path, script: Script) -> None:
    seed(tmp_path, ("prj_a", "alpha", None))
    script["prj_a"] = [
        status("prj_a", "manager", "manager", "working"),
        status("prj_a", "coder-auth", "coder", "working"),
    ]


def _on_screen(widget: Widget) -> bool:
    """Drawn, with cells of its own: shown, and laid out wider than nothing."""
    return widget.display and widget.region.width > 0


def sides(app: FleetApp) -> dict[str, Any]:
    """What is on screen of the two sides, the divider and the bar, and where the keys go."""
    bar = app.query_one(PaneSwitch)
    focused = app.focused
    in_content = focused is not None and app.content in focused.ancestors
    in_sidebar = focused is not None and (
        focused is app.sidebar or app.sidebar in focused.ancestors
    )
    return {
        "sidebar": _on_screen(app.sidebar),
        "content": _on_screen(app.content),
        "divider": _on_screen(app.query_one(Divider)),
        "bar": str(bar.render()) if _on_screen(bar) else None,
        "keys": "content" if in_content else "sidebar" if in_sidebar else None,
    }


async def until(pilot: Pilot[None], done: Any, *, what: str) -> None:
    """Pause, bounded, until ``done()`` holds: a pane takes the keyboard after the frame that
    shows it is drawn (``FleetApp._focus_pane``)."""
    app = fleet_app(pilot)
    for _ in range(250):
        await settle(app)
        if done():
            return
        await pilot.pause(0.02)
    raise AssertionError(f"never: {what} ({sides(app)})")


def _pane_has_the_keyboard(app: FleetApp) -> bool:
    """The open view is the agent's, and its pane has the keyboard."""
    view = app.current_view()
    return isinstance(view, AgentView) and app.focused is view.pane


SIDEBAR_SHOWN = {
    "sidebar": True,
    "content": False,
    "divider": False,
    "bar": "▸ open view · F12",
    "keys": "sidebar",
}
CONTENT_SHOWN = {
    "sidebar": False,
    "content": True,
    "divider": False,
    "bar": "◂ sidebar · F12",
    "keys": "content",
}


def test_at_60_columns_one_side_shows_and_the_escape_key_or_the_bar_switches_it(
    tmp_path: Path, script: Script
) -> None:
    """Start lands home, the Manager tab, the content alone on screen and the keyboard in it.
    F12 shows the sidebar, alone and the whole row wide; F12 again, and a click on the bar,
    switch back. The view stays the one open: switching sides opens nothing."""
    _one_project(tmp_path, script)

    async def go(pilot: Pilot[None]) -> list[tuple[dict[str, Any], str | None, int]]:
        app = fleet_app(pilot)
        seen = []

        async def look() -> None:
            await settle(app)
            seen.append((sides(app), app.content.current, app.sidebar.region.width))

        await look()  # start
        await pilot.press("f12")
        await look()
        await pilot.press("f12")
        await look()
        await pilot.click(PaneSwitch)
        await look()
        await pilot.click(PaneSwitch)
        await look()
        return seen

    seen = drive(go, size=NARROW)
    home = "project-prj_a"
    assert seen == [
        (CONTENT_SHOWN, home, 0),
        (SIDEBAR_SHOWN, home, NARROW[0]),  # F12: the sidebar, the whole row
        (CONTENT_SHOWN, home, 0),  # F12 again
        (SIDEBAR_SHOWN, home, NARROW[0]),  # the bar
        (CONTENT_SHOWN, home, 0),  # the bar again
    ]


@_needs_tmux
def test_at_60_columns_an_agent_chosen_in_the_sidebar_shows_its_pane_and_typing_reaches_it(
    tmp_path: Path, script: Script, no_real_tmux: list[tuple[str, ...]]
) -> None:
    """From the sidebar, a click on an agent's row shows its view alone, its pane holding the
    keyboard: a ``q`` typed there goes to the agent (``send-keys``), not to the app, which would
    quit. F12 from the pane is the way back, to the sidebar alone."""
    _one_project(tmp_path, script)

    async def go(pilot: Pilot[None]) -> tuple[dict[str, Any], int | None, dict[str, Any]]:
        app = fleet_app(pilot)
        await settle(app)
        app.query_one(Panes).show_sidebar()
        await settle(app)
        await pilot.click(row_for(app, AGENT))
        await until(pilot, lambda: _pane_has_the_keyboard(app), what="the agent's pane has it")
        chosen = sides(app)
        await pilot.press("q")
        await settle(app)
        alive = app.return_code
        await pilot.press("f12")
        await settle(app)
        return chosen, alive, sides(app)

    chosen, alive, back = drive(go, size=NARROW)
    assert chosen == CONTENT_SHOWN
    assert alive is None, "q went to the agent, not to the app"
    # Typed text goes as hex (``send-keys -H``): ``q`` is ``71``.
    typed = [argv for argv in no_real_tmux if "send-keys" in argv and argv[-2:] == ("-H", "71")]
    assert typed, f"q was forwarded to tmux: {no_real_tmux[-3:]}"
    assert back == SIDEBAR_SHOWN, "F12 from the pane: the sidebar"


def test_at_80_columns_both_sides_show_and_the_escape_key_switches_nothing(
    tmp_path: Path, script: Script
) -> None:
    """From 70 columns up nothing changes: the sidebar, the divider and the content share the
    row, there is no bar, and F12 from the sidebar leaves the keyboard there."""
    _one_project(tmp_path, script)

    async def go(pilot: Pilot[None]) -> tuple[dict[str, Any], dict[str, Any]]:
        app = fleet_app(pilot)
        await settle(app)
        app.sidebar.focus()
        await settle(app)
        before = sides(app)
        await pilot.press("f12")
        await settle(app)
        return before, sides(app)

    both = {"sidebar": True, "content": True, "divider": True, "bar": None, "keys": "sidebar"}
    assert drive(go, size=WIDE) == (both, both)


def test_resizing_80_to_60_to_80_keeps_the_selected_agent_and_the_keyboard(
    tmp_path: Path, script: Script
) -> None:
    """The agent's pane holds the keyboard at 80: at 60 its view is the side shown, the pane
    still holding it, and at 80 again both sides show as before. With the keyboard in the
    sidebar instead, 60 shows the sidebar. The selected row and the open view never change."""
    _one_project(tmp_path, script)

    async def go(pilot: Pilot[None]) -> list[tuple[dict[str, Any], str | None, str | None]]:
        app = fleet_app(pilot)
        seen = []

        async def at(size: tuple[int, int]) -> None:
            await pilot.resize_terminal(*size)
            await settle(app)
            seen.append((sides(app), app.sidebar.selected_key, app.content.current))

        await pilot.click(row_for(app, AGENT))
        await until(pilot, lambda: _pane_has_the_keyboard(app), what="the agent's pane has it")
        await at(NARROW)
        await at(WIDE)
        app.sidebar.focus()
        await settle(app)
        await at(NARROW)
        await at(WIDE)
        return seen

    selected, view = f"agent:{AGENT}", f"agent-{AGENT}"
    both = {"sidebar": True, "content": True, "divider": True, "bar": None}
    assert drive(go, size=WIDE) == [
        (CONTENT_SHOWN, selected, view),
        ({**both, "keys": "content"}, selected, view),
        (SIDEBAR_SHOWN, selected, view),
        ({**both, "keys": "sidebar"}, selected, view),
    ]


def test_the_remembered_sidebar_width_comes_back_and_narrow_keys_do_not_overwrite_it(
    tmp_path: Path, script: Script, isolated_home: Path
) -> None:
    """The sidebar fills a narrow row through its floor, not its width: the divider's width,
    remembered in state.json, is the sidebar's again at 80. The sidebar's ``>`` ``<`` ``=``
    change nothing while no divider is on screen, so a narrow terminal never saves its row."""
    _one_project(tmp_path, script)
    _write_state(isolated_home, {ui_app.SIDEBAR_WIDTH_KEY: 34})

    async def go(pilot: Pilot[None]) -> tuple[int, int, int, Any]:
        app = fleet_app(pilot)
        await settle(app)
        wide = app.sidebar.outer_size.width
        await pilot.resize_terminal(*NARROW)
        await settle(app)
        app.query_one(Panes).show_sidebar()
        await settle(app)
        narrow = app.sidebar.outer_size.width
        await pilot.press("greater_than_sign", "less_than_sign", "less_than_sign", "equals_sign")
        await settle(app)
        await pilot.resize_terminal(*WIDE)
        await settle(app)
        return (
            wide,
            narrow,
            app.sidebar.outer_size.width,
            _state(isolated_home).get(ui_app.SIDEBAR_WIDTH_KEY),
        )

    assert drive(go, size=WIDE) == (34, NARROW[0], 34, 34)


def test_at_60_columns_with_no_project_start_shows_welcome_not_the_sidebar(
    tmp_path: Path, script: Script
) -> None:
    """L1 starts on Welcome while no project is listed, and at 60 columns the narrow layout
    showed the sidebar ('No projects yet…'), Welcome hidden: the focus sat there, and only a
    view a person asked for showed the content side (review of #585, #1247). Start shows the
    content side when it lands on Welcome; 80 columns still shows both."""
    seen: dict[str, Any] = {}

    async def go(pilot: Pilot[None]) -> None:
        app = fleet_app(pilot)
        await settle(app)
        seen.update(sides(app), current=app.content.current)

    drive(go, size=NARROW)
    assert seen["current"] == "welcome"
    assert (seen["content"], seen["sidebar"]) == (True, False), seen

    drive(go, size=WIDE)
    assert (seen["content"], seen["sidebar"]) == (True, True), "80 columns: both, as ever"


def _inside(widget: Widget, width: int) -> bool:
    region = widget.region
    return region.x >= 0 and region.right <= width and region.width > 0


def test_at_60_columns_the_help_fits_the_screen(tmp_path: Path, script: Script) -> None:
    """The ? help's box is 78 wide, and at 60 columns its every line was cut on the right
    (#10's tester, #1248): it is the screen's width at most."""
    from aisquare.cli.ui.app import HelpScreen

    _one_project(tmp_path, script)

    async def go(pilot: Pilot[None]) -> tuple[bool, bool]:
        app = fleet_app(pilot)
        await settle(app)
        app.sidebar.focus()
        app.push_screen(HelpScreen("f12"))
        await settle(app)
        box = app.screen.query_one("#helpbox")
        return isinstance(app.screen, HelpScreen), _inside(box, NARROW[0])

    assert drive(go, size=NARROW) == (True, True)


def test_at_60_columns_the_sharing_question_fits_and_no_thanks_shows(
    tmp_path: Path, script: Script
) -> None:
    """The usage-sharing question is 80 wide: at 60 columns its text was cut and No thanks was
    off the screen (#10's tester, #1248). The box is the screen's width, and the three answers
    stack, each whole on the screen; at 80 columns they share one row, as before."""
    from textual.widgets import Button

    from aisquare.cli.ui.views.usage_sharing import ConsentDialog
    from tests.test_ui_usage_share import FakeSharing

    _one_project(tmp_path, script)
    seen: dict[str, Any] = {}

    async def go(pilot: Pilot[None]) -> None:
        app = fleet_app(pilot)
        await settle(app)
        app.push_screen(ConsentDialog(FakeSharing()))  # type: ignore[arg-type]
        await settle(app)
        dialog = app.screen
        assert isinstance(dialog, ConsentDialog)
        width = app.size.width
        buttons = [
            dialog.query_one(f"#{name}", Button)
            for name in ("consent-see", "consent-share", "consent-no")
        ]
        seen.update(
            box=_inside(dialog.query_one("#consent-box"), width),
            buttons=[_inside(button, width) for button in buttons],
            rows=len({button.region.y for button in buttons}),
        )

    drive(go, size=NARROW)
    assert seen["box"] and seen["buttons"] == [True] * 3, seen
    assert seen["rows"] == 3, "stacked, one answer per row"
    drive(go, size=(90, 30))
    assert seen["box"] and seen["buttons"] == [True] * 3 and seen["rows"] == 1, seen
