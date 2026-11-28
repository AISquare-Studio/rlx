"""The app's own way to move a home from before 0.10.2 (#31; the manager's decision #1985).

The ⬆ row offers *Move rlx's home* even with no newer release; its dialog says the move, and its
press hands off to the same one-click upgrade (``upgrade --one-click``), which moves the home and
reopens rlx. Driven through the real row and dialog with the upgrade stubbed. The daily check's
offer, the one-click run itself and the Doctor's row are tests/test_home_migration.py's, with no
UI fake of tmux in the way of the quiet check.
"""

from __future__ import annotations

from collections.abc import Collection
from typing import Any

import pytest
from textual.pilot import Pilot
from textual.widgets import Button, Static

from aisquare.cli.ui import app as ui_app
from aisquare.cli.ui.app import FleetApp
from aisquare.cli.ui.sidebar import HOME_MOVE_ROW, UpdateRow
from aisquare.cli.ui.update import UpdateScreen
from aisquare.services import install_route, lifecycle, updates
from aisquare.services.install_route import InstallRoute, LatestRelease
from aisquare.services.lifecycle import UpgradePlan
from tests.installer_seams import no_real_installer  # noqa: F401 — autouse, applied by import
from tests.test_ui_shell import Script, no_real_tmux, script, shown  # noqa: F401
from tests.test_ui_update_row import (  # noqa: F401
    _FACTS,
    Fresh,
    _drive,
    _open_by_keyboard,
    _toasts,
    alive_now,
    fresh,
)
from tests.ui_workers import settle_page

MOVE = "~/.aisquare → ~/.rlx"
HOME_OFFER = updates.Offer("0.9.0", "0.9.0", runnable=True, announce=False, home_move=MOVE)


def _home_plan(latest: str = "0.9.0") -> UpgradePlan:
    return UpgradePlan(
        route=InstallRoute(install_route.UV_TOOL, _FACTS),
        current="0.9.0",
        target=None,
        latest=LatestRelease(latest),
        argv=("uv", "tool", "install", "--force", "aisquare-cli>=0.9.0"),
        env={},
        reason=None,
        home_move=MOVE,
    )


@pytest.fixture
def home_planned(monkeypatch: pytest.MonkeyPatch) -> list[dict[str, Any]]:
    """``upgrade_plan`` as the dialog asks it, for a home from before 0.10.2 to move."""
    asked: list[dict[str, Any]] = []

    def plan(target: str | None = None, **kwargs: Any) -> UpgradePlan:
        asked.append({"target": target, **kwargs})
        known = kwargs.get("known_latest")
        return _home_plan(known.version if known is not None else "0.9.0")

    monkeypatch.setattr(lifecycle, "upgrade_plan", plan)
    return asked


# --- the row ---------------------------------------------------------------------------------


def test_an_old_home_with_nothing_newer_has_the_row_offer_to_move_it() -> None:
    async def body(pilot: Pilot[None], app: FleetApp) -> tuple[bool, str, list[str]]:
        row = app.query_one(UpdateRow)
        return row.display, shown(row), await _toasts(app)

    displayed, text, toasts = _drive(body, offer=HOME_OFFER)

    assert displayed and text.endswith(HOME_MOVE_ROW), text
    assert "restart to update" not in text and "0.9.0" not in text
    assert not any("is out" in toast for toast in toasts), "no release to announce"


def test_the_dialog_says_the_move_stays_on_nothing_newer_and_hands_off_to_one_click(
    home_planned: list[dict[str, Any]],
    fresh: Fresh,  # noqa: F811
    alive_now: list[Collection[str]],  # noqa: F811
) -> None:
    """PyPI's answer that nothing is newer closed the dialog with "nothing to update": with a
    home to move it stays, as the move alone, and its press is the one-click upgrade."""
    fresh.answer = "0.9.0"

    async def body(pilot: Pilot[None], app: FleetApp) -> dict[str, Any]:
        screen = await _open_by_keyboard(pilot, app)
        await settle_page(app)
        seen: dict[str, Any] = {
            "open": isinstance(app.screen, UpdateScreen),
            "heading": str(screen.heading()),
            "body": shown(screen.query_one("#update-text", Static)),
            "button": str(screen.query_one("#update-confirm", Button).label),
            "toasts": await _toasts(app),
            "row": shown(app.query_one(UpdateRow)),
        }
        await pilot.click("#update-confirm")
        await settle_page(app)
        seen["hand_off"] = app.hand_off
        return seen

    seen = _drive(body, offer=HOME_OFFER)

    assert seen["open"], "the dialog stays open on PyPI's answer"
    assert seen["heading"] == "Move rlx's home"
    assert f"moves its home ({MOVE})" in seen["body"] and "No agent is running." in seen["body"]
    assert seen["button"] == "Restart & move"
    assert not any("nothing to update" in toast for toast in seen["toasts"])
    assert seen["row"].endswith(HOME_MOVE_ROW)
    assert seen["hand_off"] == ui_app.UPDATE_HAND_OFF == ("upgrade", "--one-click")
    assert "rlx " not in seen["body"].replace("rlx closes", "").replace("rlx reopens", "")


def test_a_newer_release_makes_it_an_update_that_also_moves_the_home(
    home_planned: list[dict[str, Any]],
    fresh: Fresh,  # noqa: F811
) -> None:
    fresh.answer = "0.9.1"

    async def body(pilot: Pilot[None], app: FleetApp) -> dict[str, Any]:
        screen = await _open_by_keyboard(pilot, app)
        await settle_page(app)
        return {
            "heading": str(screen.heading()),
            "body": shown(screen.query_one("#update-text", Static)),
            "button": str(screen.query_one("#update-confirm", Button).label),
            "row": shown(app.query_one(UpdateRow)),
        }

    seen = _drive(body, offer=HOME_OFFER)

    assert seen["heading"] == "Update rlx 0.9.0 → 0.9.1"
    assert "installs 0.9.1" in seen["body"] and f"also moves rlx's home ({MOVE})" in seen["body"]
    assert seen["button"] == "Restart & update"
    assert "0.9.1 · restart to update" in seen["row"]


def test_live_agents_are_named_as_what_the_move_waits_for() -> None:
    screen = UpdateScreen(_home_plan(), 2, "0.9.0", checking=False)

    said = screen.body().plain

    assert "2 agents are running: the home moves only once nothing runs on it" in said
    assert "keep running" not in said
