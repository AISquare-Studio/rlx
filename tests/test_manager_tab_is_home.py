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

    async def go(pilot: Pilot[None]) -> tuple[tuple[str | None, str | None], list[str]]:
        app = fleet_app(pilot)
        await settle(app)
        strips = app.screen._compositor.render_full_update(simplify=True).strips
        return _shown(pilot), [strip.text.rstrip() for (strip,) in strips]

    shown, rows = drive(go, size=(100, 30))
    assert shown == ("project-prj_b", "tab-manager"), shown
    tab_row = next(row for row in rows if "Manager" in row and "Board" in row)
    assert tab_row.index("Manager") < tab_row.index("Board"), tab_row


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
