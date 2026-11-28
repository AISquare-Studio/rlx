"""Remote from Settings (#33, Q17): the one Remote page, its ngrok token and every way in.

On a fresh install the owner could not find Remote: it lived in the ``R`` modal alone, so
Settings got a Remote section of its own (#33), then App settings a *Remote* tab (#41): two
pages over one controller. Remote is one page again (0.10.2, Q17), the ``R`` panel, and the
Settings hub's *Remote control*, App settings' *Remote* tab, the sidebar's chip and ``R`` all
open it. What only the Settings page had, the ngrok token's slot and the missing-ngrok line,
is the panel's now, and is driven here as a person who looks in Settings reaches it. Driven
as ``test_ui_remote`` drives the panel: the REAL ``services.remote_server`` on a free port, a
scripted tunnel, and every claim read from what a widget shows.
"""

from __future__ import annotations

import os
import sys
import threading
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest
from textual.pilot import Pilot
from textual.widgets import Button, Input, Static, Switch, TabbedContent

from aisquare.cli.ui.app import FleetApp
from aisquare.cli.ui.sidebar import AppSettingsSelected, SettingsRow
from aisquare.cli.ui.views import settings_remote
from aisquare.cli.ui.views.app_settings import REMOTE_TAB
from aisquare.cli.ui.views.remote import RemotePanel
from aisquare.cli.ui.views.settings_remote import (
    AUTHTOKEN_FROM,
    ENV_TOKEN,
    NGROK_MISSING,
    OPEN_LABEL,
    REPLACE_TITLE,
    TOKEN_TITLE,
    RemoteChip,
    RemoteLink,
    has_authtoken,
)
from aisquare.services import remote_server
from aisquare.services.ngrok_sdk import (
    BAD_TOKEN,
    REFUSED_TOKEN,
    bundled_ngrok,
    save_token,
    saved_token,
)
from aisquare.services.ngrok_tunnel import AUTHTOKEN_HINT, NgrokTunnel
from tests.fake_clipboard import NEEDS_POSIX
from tests.fake_ngrok_sdk import Script, fake_sdk
from tests.test_remote_control import FakeTunnel
from tests.test_ui_remote import (  # noqa: F401 — the autouse fixtures, applied by import
    PUBLIC,
    drive,
    fresh_remote_server,
    installed_page,
    missing_ngrok,
    no_agents,
    no_real_tmux,
    panel,
    shown,
    written,
)
from tests.toast_log import record
from tests.ui_workers import pause_until, settle_page


def path_with(tmp_path: Path, *tools: str) -> None:
    """PATH holding only stand-ins for ``tools``: what ``shutil.which`` finds, and all."""
    folder = tmp_path / "path"
    folder.mkdir(exist_ok=True)
    for tool in tools:
        stand_in = folder / (f"{tool}.bat" if sys.platform == "win32" else tool)
        stand_in.write_text("@echo off\n" if sys.platform == "win32" else "#!/bin/sh\n")
        stand_in.chmod(0o755)
    os.environ["PATH"] = str(folder)


@pytest.fixture
def ready(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Everything Remote needs from the owner: an ngrok to start and its token."""
    own_home(tmp_path, monkeypatch)
    monkeypatch.setenv("PATH", os.environ.get("PATH", ""))  # restored after the test
    path_with(tmp_path, "ngrok")
    monkeypatch.setenv("NGROK_AUTHTOKEN", "a-token")


def late(tunnels: list[FakeTunnel]) -> Any:
    """A tunnel factory whose ngrok announces nothing until the test says so."""

    def make(port: int) -> NgrokTunnel:
        tunnels.append(FakeTunnel(port, url=None, failure=None))
        return tunnels[-1]

    return make


async def opened(pilot: Pilot[None]) -> RemotePanel:
    """Open Settings as a click on *⚙ Settings* does, then its *Remote control* link: the
    Remote page, where it lands (Q17).

    Activated and pressed, each once its target is there, not clicked: under load a click
    came before the hub could be hit-tested, and ``#hub-remote`` was not found (the local
    gate at cc8d5e0c, and 2 in 3 alone beside it)."""
    app = pilot.app
    assert isinstance(app, FleetApp)
    app.query_one(SettingsRow).activate()  # what a click on the row calls
    await until(pilot, lambda: bool(app.screen.query("#hub-remote")), what="the settings hub")
    app.screen.query_one("#hub-remote", Button).press()
    await until(pilot, lambda: isinstance(app.screen, RemotePanel), what="the Remote page")
    await settle_page(app)  # its look at what Remote needs, answered
    return panel(pilot)


async def turn(pilot: Pilot[None], section: RemotePanel, on: bool) -> None:
    """Remote on or off with the page's switch, and its writes landed."""
    switch = section.query_one("#remote-on", Switch)
    if switch.value is not on:
        switch.toggle()
    await written(pilot)


def displayed(section: RemotePanel, selector: str) -> bool:
    widget = section.query_one(selector)
    return all(node.display for node in (widget, *widget.ancestors) if hasattr(node, "display"))


# --- Off: the token, and nothing to install --------------------------------------------------

NOT_ON_THE_NORMAL_PATH = ("pip install", "uv ", "[remote]", "not installed", "ngrok config", "brew")
"""What Settings' Remote never tells the owner to do (the owner, 2026-10-23, #34)."""


def said_in(section: RemotePanel) -> str:
    """Every sentence the page shows now, one per line."""
    return "\n".join(
        shown(static)
        for static in section.query(Static).results(Static)
        if not isinstance(static, Button) and all(node.display for node in _chain(static))
    )


def _chain(widget: Any) -> list[Any]:
    return [widget, *(node for node in widget.ancestors if hasattr(node, "display"))]


def own_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A home of the test's own, where ngrok's config is looked for and written: the suite
    isolates the aisquare home, not ``~``, which holds the developer's real ngrok config."""
    home = tmp_path / "user"
    home.mkdir(exist_ok=True)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("USERPROFILE", str(home))
    monkeypatch.setenv("LOCALAPPDATA", str(home / "AppData" / "Local"))
    monkeypatch.delenv("XDG_CONFIG_HOME", raising=False)
    monkeypatch.delenv("NGROK_AUTHTOKEN", raising=False)


def bundled_ngrok_with(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, *, refuse: str | None = None
) -> Script:
    """The bundled ngrok (#34), answering as ngrok's service would: every sign-in taken, or,
    with ``refuse``, refused with that code; in the test's own home, with no token anywhere."""
    own_home(tmp_path, monkeypatch)
    script = Script(url=PUBLIC, refuse=refuse)
    monkeypatch.setitem(sys.modules, "ngrok", fake_sdk(script))
    return script


async def paste(pilot: Pilot[None], section: RemotePanel, token: str) -> None:
    """Paste ``token`` into the slot and press Enter (M5: there is no *Save*), and let the save
    answer."""
    field = section.query_one("#settings-remote-token-field", Input)
    field.focus()
    await pilot.pause()
    field.value = token
    await pilot.press("enter")
    await pilot.pause()
    await settle_page(pilot.app)
    await pilot.pause()


@NEEDS_POSIX
def test_off_with_no_ngrok_token_shows_only_the_token_field_and_its_link(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No token yet: *Paste your ngrok token*, where a free one comes from, and *Save*, on the
    Remote page; no *Change* for a token there is not; nothing to install and no command to
    run."""
    bundled_ngrok_with(tmp_path, monkeypatch)

    async def go(pilot: Pilot[None]) -> tuple[bool, bool, bool, str]:
        section = await opened(pilot)
        return (
            displayed(section, "#settings-remote-token-field"),
            displayed(section, "#remote-token-row"),
            displayed(section, "#settings-remote-ngrok-missing"),
            said_in(section),
        )

    field, change, missing, said = drive(go, tunnel=missing_ngrok)
    assert field and not change and not missing
    assert TOKEN_TITLE in said and AUTHTOKEN_FROM in said, said
    assert not [hint for hint in NOT_ON_THE_NORMAL_PATH if hint in said], said


@NEEDS_POSIX
def test_a_saved_token_hides_the_slot_and_turning_remote_on_reaches_the_tunnel(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Save checks the token with ngrok and keeps it, owner-only; the slot goes, *Change* is
    there for it, and Remote turned on starts the bundled tunnel, signed in with that token
    (#34). The token is in no toast."""
    script = bundled_ngrok_with(tmp_path, monkeypatch)
    token = "2abcDEF_0123456789abcdefghijklmnopqrstuvwxyzAB"

    async def go(pilot: Pilot[None]) -> tuple[bool, bool, list[str]]:
        app = pilot.app
        assert isinstance(app, FleetApp)
        section = await opened(pilot)
        told = record(app)
        await paste(pilot, section, token)
        slot, change = (
            displayed(section, "#settings-remote-token"),
            displayed(section, "#remote-token-row"),
        )
        await turn(pilot, section, True)
        await settle_page(app)
        return slot, change, told

    slot, change, told = drive(go, tunnel=bundled_ngrok)
    assert saved_token() == token
    assert not slot and change
    assert "✓ ngrok token saved" in told and not [line for line in told if token in line]
    assert script.calls.tokens == [token, token], "checked on Save, then signed in at the on"
    assert len(script.calls.forwards) == 1, "Remote on started the tunnel"


def test_a_token_ngrok_refuses_is_said_beside_the_field_without_the_token(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    bundled_ngrok_with(tmp_path, monkeypatch, refuse="ERR_NGROK_105")
    token = "2abc-not-a-real-token"

    async def go(pilot: Pilot[None]) -> tuple[str, bool, str]:
        section = await opened(pilot)
        await paste(pilot, section, token)
        problem = section.query_one("#settings-remote-token-problem", Static)
        first = shown(problem) if problem.display else ""
        await paste(pilot, section, "two words")
        return first, displayed(section, "#settings-remote-token"), shown(problem)

    first, slot, spaced = drive(go, tunnel=missing_ngrok)
    assert first == REFUSED_TOKEN, first
    assert token not in first and slot
    assert saved_token() is None, "a token ngrok refuses is not kept"
    assert spaced == "that is not one token: it has spaces in it"


@NEEDS_POSIX
def test_a_refused_token_entered_again_is_checked_again(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """#53: a token ngrok refused stays in the field to try again, and Enter with it unchanged
    asks ngrok again: the refusal answered the value in flight."""
    bundled_ngrok_with(tmp_path, monkeypatch, refuse="ERR_NGROK_105")
    token = "2abc-not-a-real-token"
    real_store = settings_remote.store_ngrok_token
    asked: list[str] = []

    def counted(value: str) -> None:
        asked.append(value)
        real_store(value)

    monkeypatch.setattr(settings_remote, "store_ngrok_token", counted)

    async def go(pilot: Pilot[None]) -> list[str]:
        section = await opened(pilot)
        await paste(pilot, section, token)
        await paste(pilot, section, token)  # the same token, Enter again
        return list(asked)

    assert drive(go, tunnel=missing_ngrok) == [token, token]


OLD_TOKEN = "2oldTOKEN_0123456789abcdefghijklmnopqrstuvwxy"
NEW_TOKEN = "2newTOKEN_9876543210zyxwvutsrqponmlkjihgfedcb"


async def until(pilot: Pilot[None], done: Callable[[], bool], *, what: str) -> None:
    """Pause until ``done()`` holds: ngrok's answer comes on a thread of its own. Bounded by
    the wall clock, not a count of pauses (``pause_until``, #67)."""
    await pause_until(pilot, done, what=what)


def slot_seen(section: RemotePanel) -> tuple[bool, str, bool]:
    """The token field shown, its title, and the ``NGROK_AUTHTOKEN`` line shown."""
    return (
        displayed(section, "#settings-remote-token-field"),
        shown(section.query_one("#settings-remote-token-title", Static)),
        displayed(section, "#settings-remote-token-env"),
    )


@NEEDS_POSIX
def test_a_saved_token_ngrok_refuses_can_be_replaced_on_and_off(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Review of #274, F-1: ngrok refused the saved token, and Remote said to paste a new one
    in Settings → Remote, whose field showed only while there was no token at all: on, it
    was not there, and off, the line saying why was gone too. It shows on and off as *Replace
    your ngrok token*, and a new one saved and turned on signs in with it."""
    script = bundled_ngrok_with(tmp_path, monkeypatch)
    save_token(OLD_TOKEN)

    async def go(pilot: Pilot[None]) -> dict[str, Any]:
        app = pilot.app
        assert isinstance(app, FleetApp)
        section = await opened(pilot)
        seen: dict[str, Any] = {"before": slot_seen(section)[0]}
        script.refuse = "ERR_NGROK_107"  # reset on ngrok's dashboard since it was saved
        await turn(pilot, section, True)
        await until(pilot, lambda: app.remote.status_line() == BAD_TOKEN, what="ngrok refused")
        section.repaint()
        seen["on"] = slot_seen(section)
        section.query_one("#remote-on", Switch).toggle()
        await pilot.pause()
        assert app.remote.wait_until_off(10)
        section.repaint()
        await settle_page(app)  # the look again, back at off
        seen["off"] = (*slot_seen(section), displayed(section, "#remote-token-row"))
        script.refuse = None
        await paste(pilot, section, NEW_TOKEN)
        seen["saved"] = slot_seen(section)[0]
        await turn(pilot, section, True)
        await until(pilot, lambda: app.remote.public_origin is not None, what="signed in")
        return seen

    seen = drive(go, tunnel=bundled_ngrok)
    assert seen["before"] is False, "a token ngrok took asks for nothing"
    assert seen["on"] == (True, REPLACE_TITLE, False)
    assert seen["off"] == (True, REPLACE_TITLE, False, True), "Replace, and Change"
    assert seen["saved"] is False
    assert saved_token() == NEW_TOKEN
    assert script.calls.tokens == [OLD_TOKEN, NEW_TOKEN, NEW_TOKEN], "on, Save's check, on"


@NEEDS_POSIX
def test_a_remote_turned_on_with_no_token_shows_the_field_and_a_save_brings_ngrok_up(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Turned on with ``R`` and no token anywhere, Remote said to paste one in Settings →
    Remote, which showed its On view with no field. The field is there on the Remote page
    Settings opens, and the token saved starts the tunnel at once, with no off and on
    (``RemoteController.token_saved``)."""
    script = bundled_ngrok_with(tmp_path, monkeypatch)

    async def go(pilot: Pilot[None]) -> tuple[str, bool, tuple[bool, str, bool], bool]:
        app = pilot.app
        assert isinstance(app, FleetApp)
        app.remote.turn_on(wait=False)  # what R's switch does
        await written(pilot)
        said = app.remote.status_line()
        section = await opened(pilot)
        on, slot = section.query_one("#remote-on", Switch).value, slot_seen(section)
        await paste(pilot, section, NEW_TOKEN)
        await until(pilot, lambda: app.remote.public_origin is not None, what="ngrok up")
        section.repaint()
        return said, on, slot, slot_seen(section)[0]

    said, on, slot, after = drive(go, tunnel=bundled_ngrok)
    assert said == AUTHTOKEN_HINT
    assert on and slot == (True, TOKEN_TITLE, False)
    assert script.calls.tokens == [NEW_TOKEN, NEW_TOKEN], "Save's check, then the sign-in"
    assert not after


@NEEDS_POSIX
@pytest.mark.parametrize("environment", [False, True], ids=["saved", "NGROK_AUTHTOKEN"])
def test_change_beside_the_ngrok_token_opens_the_field_and_says_when_the_env_wins(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, environment: bool
) -> None:
    bundled_ngrok_with(tmp_path, monkeypatch)
    save_token(OLD_TOKEN)
    if environment:
        monkeypatch.setenv("NGROK_AUTHTOKEN", "2envTOKEN_0123456789")

    async def go(pilot: Pilot[None]) -> tuple[bool, tuple[bool, str, bool], str, bool]:
        section = await opened(pilot)
        before = slot_seen(section)[0]
        section.query_one("#remote-change-token", Button).press()
        await pilot.pause()
        opened_up = slot_seen(section)
        env_line = shown(section.query_one("#settings-remote-token-env", Static))
        await paste(pilot, section, NEW_TOKEN)
        return before, opened_up, env_line, slot_seen(section)[0]

    before, opened_up, env_line, after = drive(go, tunnel=missing_ngrok)
    assert not before
    assert opened_up == (True, REPLACE_TITLE, environment)
    assert env_line == ENV_TOKEN
    assert saved_token() == NEW_TOKEN and not after


def test_what_remote_needs_is_looked_at_off_the_ui_thread(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Review of #274, m6: Settings' first open, and each turn-off, imported ngrok's native
    library and parsed ngrok's configs on Textual's thread. A thread worker looks now, on the
    Remote page as it did in Settings."""
    bundled_ngrok_with(tmp_path, monkeypatch)
    looked_on: list[int] = []
    found, token = settings_remote.ngrok_found, settings_remote.has_authtoken

    def ngrok_found() -> bool:
        looked_on.append(threading.get_ident())
        return found()

    def has_authtoken() -> bool:
        looked_on.append(threading.get_ident())
        return token()

    monkeypatch.setattr(settings_remote, "ngrok_found", ngrok_found)
    monkeypatch.setattr(settings_remote, "has_authtoken", has_authtoken)

    async def go(pilot: Pilot[None]) -> tuple[int, bool]:
        section = await opened(pilot)
        return threading.get_ident(), displayed(section, "#settings-remote-token-field")

    ui, field = drive(go, tunnel=missing_ngrok)
    assert len(looked_on) == 2 and ui not in looked_on, (ui, looked_on)
    assert field, "its answer painted: no token, so the field"


@pytest.mark.parametrize("look", ["as it is", "slow"])
def test_a_missing_ngrok_is_one_line_with_a_fix_in_the_app_never_a_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, look: str
) -> None:
    """Only a damaged install, whose bundled ngrok cannot be imported, with none on PATH.

    ``slow``: the look again answers as late as Windows CI's runner answered it (#51), where
    the line was read before the answer came."""
    own_home(tmp_path, monkeypatch)
    monkeypatch.setitem(sys.modules, "ngrok", None)
    monkeypatch.setenv("PATH", os.environ.get("PATH", ""))
    path_with(tmp_path)
    if look == "slow":
        needs = settings_remote.remote_needs

        def slow() -> tuple[bool, bool, bool]:
            time.sleep(0.3)
            return needs()

        monkeypatch.setattr(settings_remote, "remote_needs", slow)
    monkeypatch.setenv("NGROK_AUTHTOKEN", "a-token")

    async def go(pilot: Pilot[None]) -> tuple[bool, str, bool]:
        section = await opened(pilot)
        missing = displayed(section, "#settings-remote-ngrok-missing")
        said = said_in(section)
        path_with(tmp_path, "ngrok")  # what the fix brings back
        section.query_one("#settings-remote-ngrok-fix", Button).press()
        # The look again is on a worker (#274 m6): one pause was enough here, not on
        # Windows CI's runner, which read the line before the answer came (#51).
        await until(
            pilot,
            lambda: not displayed(section, "#settings-remote-ngrok-missing"),
            what="the line gone once the fix brought ngrok back",
        )
        return missing, said, displayed(section, "#settings-remote-ngrok-missing")

    missing, said, after = drive(go, tunnel=missing_ngrok)
    assert missing and NGROK_MISSING in said
    assert not [hint for hint in NOT_ON_THE_NORMAL_PATH if hint in said], said
    assert not after


# --- one Remote page, every way in (Q17) -----------------------------------------------------


async def back_out(pilot: Pilot[None]) -> None:
    """Esc out of the Remote page, back to what was under it."""
    await pilot.press("escape")
    await until(pilot, lambda: not isinstance(pilot.app.screen, RemotePanel), what="closed")


def test_r_the_hub_row_app_settings_and_the_chip_open_the_one_remote_page(ready: None) -> None:
    """Q17: Remote lived in three places, the ``R`` modal, App settings' *Remote* tab and the
    Settings hub's *Remote control* row (which led to that tab), two pages over one
    controller. ``R``, the hub's row, App settings' tab and the sidebar's chip all open one
    page, the same screen over the app's one controller, and never a second over the first."""

    async def go(pilot: Pilot[None]) -> dict[str, Any]:
        app = pilot.app
        assert isinstance(app, FleetApp)
        seen: dict[str, Any] = {}
        app.sidebar.focus()
        await pilot.press("R")
        seen["R"] = panel(pilot)
        app.action_remote_panel()  # R again, or the palette's Remote control, over it
        await pilot.pause()
        seen["stacked"] = sum(isinstance(screen, RemotePanel) for screen in app.screen_stack)
        await back_out(pilot)
        seen["hub"] = await opened(pilot)
        await back_out(pilot)
        app.post_message(AppSettingsSelected(REMOTE_TAB))
        await until(pilot, lambda: bool(app.query(RemoteLink)), what="App settings' Remote tab")
        await settle_page(app)
        tab = app.query_one(RemoteLink)
        seen["tab"] = (
            app.query_one("#app-settings-tabs", TabbedContent).active,
            str(tab.query_one("#settings-remote-open", Button).label),
            [type(child).__name__ for child in tab.query("*")],
        )
        tab.query_one("#settings-remote-open", Button).press()
        await until(pilot, lambda: isinstance(app.screen, RemotePanel), what="the Remote page")
        seen["App settings"] = panel(pilot)
        await back_out(pilot)
        app.remote.turn_on(wait=False)
        await written(pilot)
        chip = app.query_one(RemoteChip)
        chip.remote_tick()
        await pilot.pause()
        await pilot.click(chip)
        await until(pilot, lambda: isinstance(app.screen, RemotePanel), what="the Remote page")
        seen["chip"] = panel(pilot)
        seen["controller"] = app.remote
        return seen

    seen = drive(go, tunnel=late([]))
    pages = [seen[way] for way in ("R", "hub", "App settings", "chip")]
    assert all(type(page) is RemotePanel for page in pages), "one page, the same screen"
    assert all(page.controller is seen["controller"] for page in pages), "over one controller"
    assert seen["stacked"] == 1, "never a second page over the first"
    active, label, kinds = seen["tab"]
    assert (active, label) == (REMOTE_TAB, OPEN_LABEL)
    assert "Switch" not in kinds and "TokenSlot" not in kinds, "no second Remote in the tab"


def test_app_settings_remote_tab_says_whether_remote_is_on(ready: None) -> None:
    """The tab is a way in, not a page of its own: what Remote is, whether it is on and how
    many devices are in, as the sidebar's chip counts them."""

    async def go(pilot: Pilot[None]) -> tuple[str, str]:
        app = pilot.app
        assert isinstance(app, FleetApp)
        app.post_message(AppSettingsSelected(REMOTE_TAB))
        await until(pilot, lambda: bool(app.query(RemoteLink)), what="App settings' Remote tab")
        await settle_page(app)
        tab = app.query_one(RemoteLink)
        off = shown(tab.query_one("#settings-remote-state", Static))
        app.remote.turn_on(wait=False)
        await written(pilot)
        runtime = remote_server.runtime()
        assert runtime.unlock_device(runtime.password, "iPhone Safari") is not None
        tab.remote_link_paint()
        await pilot.pause()
        return off, shown(tab.query_one("#settings-remote-state", Static))

    off, on = drive(go, tunnel=late([]))
    assert off == "Remote is off"
    assert on == "● on  · 1 device signed in"


# --- the authtoken check --------------------------------------------------------------------


@pytest.mark.parametrize(
    ("config", "found"),
    [
        pytest.param('version: "3"\nagent:\n  authtoken: 2abc\n', True, id="v3, under agent"),
        pytest.param("authtoken: 2abc\nregion: eu\n", True, id="v2, top level"),
        pytest.param('version: "3"\nagent:\n  authtoken: ""\n', False, id="an empty one"),
        pytest.param('version: "3"\n# authtoken: 2abc\n', False, id="commented out"),
        pytest.param(None, False, id="no config"),
    ],
)
def test_the_authtoken_is_found_where_ngrok_config_add_authtoken_writes_it(
    tmp_path: Path, config: str | None, found: bool
) -> None:
    path = tmp_path / "ngrok.yml"
    if config is not None:
        path.write_text(config, encoding="utf-8")
    assert has_authtoken({}, [path]) is found


def test_the_authtoken_in_the_environment_signs_ngrok_in_too(tmp_path: Path) -> None:
    assert has_authtoken({"NGROK_AUTHTOKEN": "2abc"}, [tmp_path / "none.yml"])
    assert not has_authtoken({"NGROK_AUTHTOKEN": "  "}, [tmp_path / "none.yml"])


# --- M5: a token typed and not entered, as the page goes ----------------------------------


@NEEDS_POSIX
def test_a_token_typed_and_not_entered_is_saved_when_rlx_quits(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Reviewer of #24: quitting before the field was left lost what it held. It is checked
    and saved as Enter would, before rlx goes."""
    bundled_ngrok_with(tmp_path, monkeypatch)
    token = "2abcDEF_0123456789abcdefghijklmnopqrstuvwxyzAB"

    async def go(pilot: Pilot[None]) -> None:
        section = await opened(pilot)
        field = section.query_one("#settings-remote-token-field", Input)
        field.focus()
        await pilot.pause()
        field.value = token
        pilot.app.exit()  # no Enter, the field never left

    drive(go, tunnel=missing_ngrok)
    assert saved_token() == token


@NEEDS_POSIX
def test_a_token_typed_and_not_entered_is_saved_when_the_page_is_closed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Esc closes the Remote page with the token typed: saved, off the UI thread, and said."""
    bundled_ngrok_with(tmp_path, monkeypatch)
    token = "2abcDEF_0123456789abcdefghijklmnopqrstuvwxyzAB"

    async def go(pilot: Pilot[None]) -> list[str]:
        app = pilot.app
        assert isinstance(app, FleetApp)
        section = await opened(pilot)
        told = record(app)
        field = section.query_one("#settings-remote-token-field", Input)
        field.focus()
        await pilot.pause()
        field.value = token
        await pilot.press("escape")
        await until(pilot, lambda: not isinstance(app.screen, RemotePanel), what="the page shut")
        await until(pilot, lambda: saved_token() == token, what="the token saved")
        await until(pilot, lambda: "✓ ngrok token saved" in told, what="the toast")
        return told

    told = drive(go, tunnel=missing_ngrok)
    assert saved_token() == token
    assert not any(token in said for said in told), "the token is said nowhere"
