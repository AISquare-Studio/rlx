"""L1 (the UX audit): the Manager tab is home.

Starting rlx with a project lands on the last-used project's Manager tab, its manager's view;
the logo (``r\\x``, the sidebar's top) and the ``home`` key go there too. Welcome is what start
shows with no project, and what ``w`` opens whenever asked.

Driven as ``test_ui_shell.py`` drives the shell: the real store in the isolated home,
``list_agents`` scripted, and every tmux command held to the test's own socket (its
``no_real_tmux``, the in-memory tmux the shell tests run on).
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest
from textual.pilot import Pilot

from aisquare.cli.ui import app as ui_app
from aisquare.cli.ui.views.project import ProjectView
from tests.test_ui_shell import Script, drive, fleet_app, seed, settle, status
from tests.test_ui_shell import no_real_tmux as _no_real_tmux
from tests.test_ui_shell import script as _script

no_real_tmux = _no_real_tmux
script = _script


def _two_projects(tmp_path: Path, script: Script) -> None:
    seed(tmp_path, ("prj_a", "alpha", None), ("prj_b", "beta", None))
    for project in ("prj_a", "prj_b"):
        script[project] = [status(project, "manager", "manager", "working")]


def _shown(pilot: Pilot[None]) -> tuple[str | None, str | None]:
    """The view on screen, and the tab a project's view has open (``None`` for another page)."""
    app = fleet_app(pilot)
    view = app.current_view()
    return app.content.current, view.active if isinstance(view, ProjectView) else None


async def _home_shown(pilot: Pilot[None]) -> tuple[str | None, str | None]:
    await settle(fleet_app(pilot))
    return _shown(pilot)


def test_start_lands_on_the_last_used_projects_manager_tab(tmp_path: Path, script: Script) -> None:
    """Two projects, beta last used: start opens beta's Manager tab, not Welcome nor alpha's."""
    _two_projects(tmp_path, script)
    ui_app._remember_ui_state(ui_app.LAST_PROJECT_KEY, "prj_b")

    assert drive(_home_shown) == ("project-prj_b", "tab-manager")


def test_the_last_used_project_is_the_one_whose_agent_was_open_last(
    tmp_path: Path, script: Script
) -> None:
    """A Settings page opened after a project does not make home Welcome: the last project an
    agent's or project's view was open on stays home, whatever page was open last."""
    _two_projects(tmp_path, script)
    ui_app._remember_ui_state(ui_app.LAST_PROJECT_KEY, "prj_b")
    ui_app._remember_ui_state(ui_app.SELECTED_KEY, "accounts")

    assert drive(_home_shown) == ("project-prj_b", "tab-manager")


def test_with_nothing_remembered_home_is_the_first_listed_project(
    tmp_path: Path, script: Script
) -> None:
    _two_projects(tmp_path, script)

    assert drive(_home_shown) == ("project-prj_a", "tab-manager")


def test_with_no_project_start_shows_welcome(tmp_path: Path, script: Script) -> None:
    assert drive(_home_shown) == ("welcome", None)


def test_the_logo_and_the_home_key_land_on_the_manager_tab_and_w_still_opens_welcome(
    tmp_path: Path, script: Script
) -> None:
    """From another tab and another page: the logo click goes home, the ``home`` key goes home,
    and ``w`` opens Welcome as ever."""
    _two_projects(tmp_path, script)
    ui_app._remember_ui_state(ui_app.LAST_PROJECT_KEY, "prj_b")

    async def go(pilot: Pilot[None]) -> list[tuple[str | None, str | None]]:
        app = fleet_app(pilot)
        await settle(app)
        seen = [_shown(pilot)]
        view = app.current_view()
        assert isinstance(view, ProjectView)
        view.active = "tab-board"  # away from the Manager tab
        await settle(app)
        seen.append(_shown(pilot))
        await pilot.click("#fleet-title")
        await settle(app)
        seen.append(_shown(pilot))
        await pilot.press("w")
        await settle(app)
        seen.append(_shown(pilot))
        app.sidebar.focus()
        await pilot.press("home")
        await settle(app)
        seen.append(_shown(pilot))
        return seen

    assert drive(go) == [
        ("project-prj_b", "tab-manager"),  # start
        ("project-prj_b", "tab-board"),
        ("project-prj_b", "tab-manager"),  # the logo
        ("welcome", None),  # w
        ("project-prj_b", "tab-manager"),  # the home key
    ]


def test_the_first_listed_project_is_the_sidebars_first_card(
    tmp_path: Path, script: Script
) -> None:
    """With nothing remembered, home is the first project as the sidebar lists it (its groups,
    pins and order), not the first in the store: beta pinned above alpha is home."""
    from aisquare.core.store import store_session
    from aisquare.services import project_groups

    _two_projects(tmp_path, script)
    with store_session() as store:
        project_groups.pin(store, "prj_b")

    assert drive(_home_shown) == ("project-prj_b", "tab-manager")


def test_at_100_by_30_with_two_projects_the_first_screen_is_the_manager_tab(
    tmp_path: Path, script: Script
) -> None:
    """The contract's screen: rlx at 100x30 with two projects shows the Manager tab first, its
    tab the active one on screen."""
    _two_projects(tmp_path, script)
    ui_app._remember_ui_state(ui_app.LAST_PROJECT_KEY, "prj_b")

    async def go(
        pilot: Pilot[None],
    ) -> tuple[tuple[str | None, str | None], list[str], tuple[str, tuple[int, int]]]:
        from textual.widgets import Tabs

        app = fleet_app(pilot)
        await settle(app)
        strips = app.screen._compositor.render_full_update(simplify=True).strips
        tab = app.current_view().query_one(Tabs).active_tab  # type: ignore[union-attr]
        assert tab is not None
        drawn = (str(tab.label.plain), (tab.region.x, tab.region.y))
        return _shown(pilot), [strip.text.rstrip() for (strip,) in strips], drawn

    shown, rows, active = drive(go, size=(100, 30))
    assert shown == ("project-prj_b", "tab-manager"), shown
    tab_row = next(row for row in rows if "Manager" in row and "Board" in row)
    assert tab_row.index("Manager") < tab_row.index("Board"), tab_row
    label, (x, y) = active
    assert label == "Manager", "the active tab on screen is the Manager's"
    assert rows[y][x:].lstrip().startswith("Manager"), (rows[y], x)


def test_a_click_on_welcomes_fleet_card_title_stays_on_welcome(
    tmp_path: Path, script: Script
) -> None:
    """The logo is the sidebar's ``#fleet-title`` alone: Welcome's step-3 card has one of its
    own, and a click on it ('3 Fleet') went home (the tester of #580, #828). It stays on
    Welcome; the logo, clicked after it, still goes home."""
    from aisquare.cli.ui.views.welcome import WelcomeView

    _two_projects(tmp_path, script)

    async def go(pilot: Pilot[None]) -> tuple[str | None, str | None]:
        app = fleet_app(pilot)
        await settle(app)
        await app.action_welcome()  # `w`
        await settle(app)
        await pilot.click(app.query_one(WelcomeView).query_one("#fleet-title"))
        await settle(app)
        on_card = app.content.current
        await pilot.click(app.sidebar.query_one("#fleet-title"))
        await settle(app)
        return on_card, app.content.current

    assert drive(go) == ("welcome", "project-prj_a"), "the card stays; the logo goes home"


@pytest.mark.needs_captain
def test_opening_the_captain_keeps_the_last_used_project(tmp_path: Path, script: Script) -> None:
    """The captain's row is ``agent:<home>/<id>``, and its board is no project: opening it wrote
    the home's id as the last-used project, and home fell back to the first card (the tester of
    #580, #828). Beta opened, then the captain: the Home key and a relaunch both open beta."""
    from aisquare.cli.ui.sidebar import ProjectTitle
    from aisquare.services.captain import state as captain_state
    from tests.test_ui_shell import card_for, row_for

    _two_projects(tmp_path, script)
    home = captain_state.home_project()
    captain = status(home.id, "captain", "captain", "waiting")
    script[home.id] = [captain]

    async def go(pilot: Pilot[None]) -> tuple[tuple[str | None, str | None], str | None]:
        app = fleet_app(pilot)
        await settle(app)
        await pilot.click(card_for(app, "prj_b").query_one(ProjectTitle))
        await settle(app)
        await pilot.click(row_for(app, captain.agent.id))  # the captain's view
        await settle(app)
        app.sidebar.focus()
        await pilot.press("home")
        await settle(app)
        return _shown(pilot), ui_app._ui_state(ui_app.LAST_PROJECT_KEY)

    shown, last = drive(go)
    assert shown == ("project-prj_b", "tab-manager"), "the Home key: beta, not the first card"
    assert last == "prj_b", "the captain's board is no project: it is never the last-used one"
    assert drive(_home_shown) == ("project-prj_b", "tab-manager"), "a relaunch: beta too"


def test_home_inside_an_agents_pane_is_the_agents_own_key(
    tmp_path: Path, script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The Home key goes home from the sidebar only (as ``w``): in an attached agent's pane it
    is the agent's, handed to the pane's input (a shell's line start), and nothing moves. It
    fails for a binding that takes Home from the pane: a ``priority=True`` one with ``home`` out
    of ``FleetApp.SIDEBAR_ACTIONS``, whose gate is what keeps it the sidebar's (checked by hand).

    Read at the pane's own input (``TerminalPane._forward``), not at tmux's command line: with
    no tmux binary (windows-py3.12, or tmux off PATH) the pane sends nothing to a server, and
    the recorder read ``[]`` though the key was the pane's (#9's Windows failure)."""
    from aisquare.cli.ui.terminal import TerminalPane
    from tests.test_ui_shell import row_for

    _two_projects(tmp_path, script)
    handed: list[tuple[str, object]] = []
    real = TerminalPane._forward

    def forward(pane: TerminalPane, kind: Any, value: str | bytes) -> bool:
        handed.append((str(kind), value))
        return real(pane, kind, value)

    monkeypatch.setattr(TerminalPane, "_forward", forward)

    async def go(pilot: Pilot[None]) -> tuple[str | None, str]:
        app = fleet_app(pilot)
        await settle(app)
        await pilot.click(row_for(app, "agt_a_manager"))  # a click on its row: the pane has keys
        await settle(app)
        assert type(app.focused).__name__ == "TerminalPane", type(app.focused).__name__
        handed.clear()
        await pilot.press("home")
        await settle(app)
        return app.content.current, type(app.focused).__name__

    assert drive(go) == ("agent-agt_a_manager", "TerminalPane"), "the agent's view stays"
    assert ("key", "Home") in handed, handed


@pytest.mark.parametrize("remembered", ["project:prj_b", "agent:prj_b/agt_b_manager"])
def test_an_upgrade_with_only_the_last_view_remembered_opens_its_project(
    tmp_path: Path, script: Script, remembered: str
) -> None:
    """The upgrade path: 0.10.1 remembered the last view (``fleet.selected``) and nothing of
    ``fleet.last_project``. Two projects, the second remembered: home is its project, not the
    first listed."""
    _two_projects(tmp_path, script)
    ui_app._remember_ui_state(ui_app.SELECTED_KEY, remembered)

    assert drive(_home_shown) == ("project-prj_b", "tab-manager")


def test_the_help_names_home_its_key_and_the_logo() -> None:
    """The ? help says what home is and every way there: Home from the sidebar, Esc there when
    nothing is marked, the logo."""
    from aisquare.cli.ui.app import HelpScreen

    (line,) = [row for row in HelpScreen("f12").keys_text().plain.splitlines() if "r\\x" in row]
    assert line.lstrip().startswith("Home  r\\x"), line
    for said in (
        "last-used project's Manager tab",
        "Home from the sidebar",
        "Esc there when nothing is marked",
        "logo",
    ):
        assert said in line, (said, line)


def test_esc_in_the_sidebar_goes_home_once_no_mark_is_left(tmp_path: Path, script: Script) -> None:
    """Seq 441's decision (2): Esc in the sidebar clears the cards' marks, and with none to
    clear, goes home, the last-used project's Manager tab."""
    _two_projects(tmp_path, script)
    ui_app._remember_ui_state(ui_app.LAST_PROJECT_KEY, "prj_b")

    async def go(pilot: Pilot[None]) -> list[tuple[str | None, str | None, int]]:
        app = fleet_app(pilot)
        await settle(app)
        view = app.current_view()
        assert isinstance(view, ProjectView)
        view.active = "tab-board"  # away from the Manager tab
        await app.action_welcome()  # and off the project: Welcome
        await settle(app)
        app.sidebar.focus()
        app.sidebar.select("project:prj_a")
        await pilot.press("m")  # a mark on alpha's card
        await settle(app)
        seen = [(*_shown(pilot), len(app.sidebar.query(".marked")))]
        await pilot.press("escape")  # the mark goes, nothing else
        await settle(app)
        seen.append((*_shown(pilot), len(app.sidebar.query(".marked"))))
        await pilot.press("escape")  # no mark left: home
        await settle(app)
        seen.append((*_shown(pilot), len(app.sidebar.query(".marked"))))
        return seen

    assert drive(go) == [
        ("welcome", None, 1),
        ("welcome", None, 0),
        ("project-prj_b", "tab-manager", 0),
    ]


def test_a_drop_or_a_group_pick_leaves_the_open_view_where_it_is(
    tmp_path: Path, script: Script
) -> None:
    """Esc goes home only through Esc's own action: the shell clears the cards' marks after a
    drop and after a group pick (``Sidebar.action_clear_marks``), and while Esc's go-home lived
    there, a drag of one unmarked card or ``g`` on the highlighted one replaced the open view
    with the Manager tab (review of #580, #981). An agent's view stays open through both."""
    from aisquare.cli.ui.groups import DropProject, GroupProjects
    from tests.test_ui_shell import row_for

    _two_projects(tmp_path, script)

    async def go(pilot: Pilot[None]) -> list[str | None]:
        app = fleet_app(pilot)
        await settle(app)
        await pilot.click(row_for(app, "agt_a_manager"))  # alpha's manager: its view
        await settle(app)
        seen = [app.content.current]
        app.post_message(DropProject(["prj_a"], scope=None, before=None))  # a drag, unmarked
        await settle(app)
        seen.append(app.content.current)
        app.post_message(GroupProjects(["prj_a"]))  # g on the highlighted card
        await settle(app)
        app.screen.dismiss("new:tools")  # the picker's answer: a new group
        await settle(app)
        seen.append(app.content.current)
        return seen

    assert drive(go) == ["agent-agt_a_manager"] * 3, "the open view stays the agent's"
