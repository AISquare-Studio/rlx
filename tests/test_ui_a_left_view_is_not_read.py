"""``asq``'s tick reads a collapsed group's member while the right pane shows its view, and not
once that view is left behind another.

A collapsed group's members are not read on the tick (#83), except the project the shown
Project or Agent view is of. Views are kept once opened; going back to one reads its project at
once, before the next tick.
"""

from __future__ import annotations

from collections.abc import Collection, Sequence
from datetime import UTC, datetime
from pathlib import Path

import pytest
from textual.pilot import Pilot

from aisquare.cli.ui import app as app_mod
from aisquare.cli.ui import needs_you_bar
from aisquare.cli.ui.needs_you_bar import NeedsYouBar
from aisquare.cli.ui.sidebar import AccountsSelected, ProjectSelected
from aisquare.cli.ui.views.agent import AgentView
from aisquare.cli.ui.views.project import ProjectView
from aisquare.core.store import ContextStore, store_session
from aisquare.models import ProjectInfo, TeamTask
from aisquare.services import fleet as fleet_service
from aisquare.services import project_groups as groups_service
from tests.test_ui_shell import Script, drive, fleet_app, refreshed, seed, settle
from tests.test_ui_shell import no_real_tmux as _no_real_tmux
from tests.test_ui_shell import script as _script
from tests.ui_workers import settle_page

#: Re-exported so pytest collects them here (the guard is autouse there, and here).
no_real_tmux = _no_real_tmux
script = _script


def test_a_left_views_folded_project_is_not_read_and_going_back_reads_it_at_once(
    tmp_path: Path, script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``prj_b`` is the one member of a collapsed group. This test reads the projects each fleet
    read lists: one tick after ``prj_b``'s Project view is opened and then the Accounts page is
    shown, and every read after ``prj_b``'s view is shown again, with no tick."""
    seed(tmp_path, ("prj_a", "alpha", None), ("prj_b", "bravo", None))
    with store_session() as store:
        tools, _ = groups_service.create_group(store, "tools", ["prj_b"])
        groups_service.set_collapsed(store, tools.id, True)
    lists: list[set[str]] = []
    read = fleet_service.read_listings

    def noted(
        store: ContextStore, projects: Sequence[ProjectInfo], *, skip: Collection[str] = ()
    ) -> fleet_service.Listings:
        listings = read(store, projects, skip=skip)
        lists.append(set(listings.listings))
        return listings

    monkeypatch.setattr(fleet_service, "read_listings", noted)

    async def go(pilot: Pilot[None]) -> tuple[set[str], list[set[str]]]:
        app = fleet_app(pilot)
        await app.on_project_selected(ProjectSelected("prj_b"))
        await settle(app)
        await app.on_accounts_selected(AccountsSelected())
        await settle(app)
        await refreshed(app)
        left = lists[-1]
        before = len(lists)
        await app.on_project_selected(ProjectSelected("prj_b"))
        await settle(app)
        return left, lists[before:]

    left, back = drive(go)
    assert left == {"prj_a"}
    assert back == [{"prj_a", "prj_b"}]


# --- #51 (the takeover of rlx#557): the manager's condition, and the board's ------------------


def _folded_bravo(tmp_path: Path) -> None:
    seed(tmp_path, ("prj_a", "alpha", None), ("prj_b", "bravo", None))
    with store_session() as store:
        tools, _ = groups_service.create_group(store, "tools", ["prj_b"])
        groups_service.set_collapsed(store, tools.id, True)


def _listed(monkeypatch: pytest.MonkeyPatch) -> list[set[str]]:
    """The projects each fleet read lists, by read."""
    lists: list[set[str]] = []
    read = fleet_service.read_listings

    def noted(
        store: ContextStore, projects: Sequence[ProjectInfo], *, skip: Collection[str] = ()
    ) -> fleet_service.Listings:
        listings = read(store, projects, skip=skip)
        lists.append(set(listings.listings))
        return listings

    monkeypatch.setattr(fleet_service, "read_listings", noted)
    return lists


def test_a_folded_project_is_listed_once_the_sweep_is_due_and_its_frame_shows_none_of_it(
    tmp_path: Path, script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The manager's condition on the skip (#3091): a folded project's dead panes are still ended
    within a bounded time. ``list_agents_many`` ends the rows of panes that died in every project
    it is handed, so a fleet read lists the folded ones too once
    :data:`~aisquare.cli.ui.app.FOLDED_SWEEP_SECONDS` have passed since one last did, and the
    frame it paints still holds no row of theirs: the cards and views show what they did."""
    _folded_bravo(tmp_path)
    lists = _listed(monkeypatch)

    async def go(pilot: Pilot[None]) -> tuple[list[set[str]], list[bool]]:
        app = fleet_app(pilot)
        await app.on_accounts_selected(AccountsSelected())  # no project's view on screen
        await settle(app)
        before = len(lists)
        shown: list[bool] = []
        for due in (False, True, False):
            if due:
                app._folded_swept_at -= app_mod.FOLDED_SWEEP_SECONDS
            await refreshed(app)
            assert app.snapshot is not None
            shown.append("prj_b" in app.snapshot.agents)
        return lists[before:], shown

    reads, shown = drive(go)
    assert reads == [{"prj_a"}, {"prj_a", "prj_b"}, {"prj_a"}], (len(reads), reads)
    assert shown == [False, False, False], "swept, never shown"


def test_the_needs_bar_reads_and_shows_the_same_with_the_skip_and_without(
    tmp_path: Path, script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The board, the needs-you bar and done tracking read the team store, never the fleet frame
    the skip trims: with ``prj_b`` folded and its view left behind, the bar lists the same
    projects (``snapshot.projects``) and the same needs, a blocked card on ``prj_b``'s board
    among them, whether the left view keeps its project read (as before rlx#557) or not."""
    _folded_bravo(tmp_path)
    with store_session() as store:
        task, _ = store.upsert_task(
            TeamTask(
                id="tsk_bravo", project_id="prj_b", key="stuck", title="waits on a key",
                created_at=datetime.now(UTC), updated_at=datetime.now(UTC),
            )
        )  # fmt: skip
        store.set_task_status(task.id, "blocked")
    lists = _listed(monkeypatch)
    every_open_view = app_mod.FleetApp._viewed_projects

    def as_before(app: app_mod.FleetApp) -> frozenset[str]:
        content = app.content
        return frozenset(
            [view.project.id for view in content.query_children(ProjectView)]
            + [view.status.agent.project_id for view in content.query_children(AgentView)]
        )

    async def go(pilot: Pilot[None]) -> tuple[set[str], list[str], list[tuple[str, str]]]:
        app = fleet_app(pilot)
        await app.on_project_selected(ProjectSelected("prj_b"))
        await settle(app)
        await app.on_accounts_selected(AccountsSelected())
        await settle(app)
        await refreshed(app)
        listed = lists[-1]
        assert app.snapshot is not None
        bar = app.query_one(NeedsYouBar)
        bar.read_now()
        await settle_page(app, group=needs_you_bar.BAR_WORKER)
        projects = [project.id for project in app.snapshot.projects]
        return listed, projects, [(need.project.id, need.kind) for need in bar.needs]

    skipped = drive(go)
    monkeypatch.setattr(app_mod.FleetApp, "_viewed_projects", as_before)
    before = drive(go)
    monkeypatch.setattr(app_mod.FleetApp, "_viewed_projects", every_open_view)

    assert skipped[0] == {"prj_a"} and before[0] == {"prj_a", "prj_b"}, "the skip took"
    assert skipped[1:] == before[1:]
    assert ("prj_b", "blocked") in skipped[2]
