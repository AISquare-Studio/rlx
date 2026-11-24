"""``asq``'s tick reads a collapsed group's member while the right pane shows its view, and not
once that view is left behind another.

A collapsed group's members are not read on the tick (#83), except the project the shown
Project or Agent view is of. Views are kept once opened; going back to one reads its project at
once, before the next tick.
"""

from __future__ import annotations

from collections.abc import Collection, Sequence
from pathlib import Path

import pytest
from textual.pilot import Pilot

from aisquare.cli.ui.sidebar import AccountsSelected, ProjectSelected
from aisquare.core.store import ContextStore, store_session
from aisquare.models import ProjectInfo
from aisquare.services import fleet as fleet_service
from aisquare.services import project_groups as groups_service
from tests.test_ui_shell import Script, drive, fleet_app, refreshed, seed, settle
from tests.test_ui_shell import no_real_tmux as _no_real_tmux
from tests.test_ui_shell import script as _script

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
