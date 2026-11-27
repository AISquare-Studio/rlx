"""Remote's switch (#46): ``[experimental] remote``, beside the captain's in Settings'
*Experimental*, both off as they ship.

Off, nothing of Remote loads: no tab, worker, chip, key or server, and no module of it in
``sys.modules`` (read in a fresh interpreter: this one has imported everything the suite has,
``conftest``'s ``no_real_ngrok`` the ngrok modules for every test). A home where Remote was set
up before it had a switch reads as on, and is written so once, so the upgrade cuts no one off.
The suite runs with Remote on (``conftest.remote_on``); each test here sets the variable, or
takes it away so the config and the home decide.
"""

from __future__ import annotations

import json
import subprocess
import sys
import textwrap

import pytest
from textual.pilot import Pilot
from textual.widgets import Checkbox, TabbedContent
from typer.testing import CliRunner

from aisquare.cli.app import app as cli
from aisquare.cli.ui.sidebar import SettingsRow
from aisquare.cli.ui.views.app_settings import REMOTE_TAB
from aisquare.cli.ui.views.settings_captain import CaptainToggle, ExperimentalSection, RemoteToggle
from aisquare.core import experimental, paths
from aisquare.core import program as prog
from aisquare.core.config import load_config, save_config
from aisquare.services import diagnostics
from tests import test_ui_shell as ui_suite
from tests.test_captain_view import drive
from tests.test_ui_shell import Script, fleet_app
from tests.ui_workers import settle_page

no_real_tmux = ui_suite.no_real_tmux
script = ui_suite.script

#: Remote's modules, as the brief names them: what must not load while it is off.
REMOTE_MODULES = (
    "aisquare.cli.ui.remote_control",
    "aisquare.cli.ui.views.remote",
    "aisquare.cli.ui.views.settings_remote",
    "aisquare.services.remote_server",
    "aisquare.services.ngrok_sdk",
    "aisquare.services.ngrok_tunnel",
)


@pytest.fixture
def decides(monkeypatch: pytest.MonkeyPatch) -> None:
    """No variable: the config, and the home while it is undecided, decide."""
    monkeypatch.delenv(experimental.REMOTE_ENV, raising=False)


def _set_up(signal: str) -> None:
    """This home with Remote set up before it had a switch, by one of the three signals."""
    home = paths.aisquare_home()
    home.mkdir(parents=True, exist_ok=True)
    if signal == "token":
        path = paths.ngrok_token_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("2abc_token\n", encoding="utf-8")
    elif signal == "device":
        path = paths.remote_state_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({"version": 2, "devices": [{"id": "d1"}]}), encoding="utf-8")
    elif signal in ("push", "needs", "audit"):
        path = {
            "push": paths.remote_push_path(),
            "needs": paths.remote_needs_path(),
            "audit": paths.remote_audit_path(),
        }[signal]
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("{}" if signal != "audit" else "a write\n", encoding="utf-8")
    elif signal == "switch":
        paths.state_path().write_text(json.dumps({"remote_enabled": False}), encoding="utf-8")
    else:  # pragma: no cover - the parametrize names every signal
        raise AssertionError(signal)


# --- the flag and the upgrade rule -----------------------------------------------------------


def test_a_fresh_home_reads_remote_off_and_writes_nothing(decides: None) -> None:
    assert experimental.remote_enabled() is False
    assert experimental.settle_remote() is False
    assert not paths.config_path().exists() or "remote" not in paths.config_path().read_text()


@pytest.mark.parametrize("signal", ["token", "device", "push", "needs", "audit", "switch"])
def test_a_home_set_up_before_the_switch_reads_on_and_is_written_so_once(
    decides: None, signal: str
) -> None:
    """The manager's rule (#46): a saved token, a paired device or a phone's trace, or
    Remote's own on/off in state.json. Read on while undecided, then written once."""
    _set_up(signal)

    assert experimental.remote_enabled() is True
    assert experimental.settle_remote() is True
    assert experimental.settle_remote() is False, "decided now: never written again"

    assert load_config().experimental.remote is True
    assert paths.config_path().read_text(encoding="utf-8").count("remote = true") == 1


def test_a_bare_remote_json_is_no_set_up_since_a_first_look_makes_one(decides: None) -> None:
    """Any first look at Remote (its page, ``rlx remote status``) makes remote.json, with a
    fresh link and password and no device: that is no Remote of the person's."""
    path = paths.remote_state_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps({"version": 2, "token": "t", "password": "p", "devices": []}), encoding="utf-8"
    )

    assert experimental.remote_enabled() is False
    assert experimental.settle_remote() is False


def test_an_off_the_person_chose_stays_off_on_a_home_set_up(decides: None) -> None:
    _set_up("token")
    config = load_config()
    config.experimental.remote = False
    save_config(config)

    assert experimental.remote_enabled() is False
    assert experimental.settle_remote() is False
    assert load_config().experimental.remote is False, "kept: an explicit false"


@pytest.mark.parametrize(("value", "set_up", "on"), [("1", False, True), ("0", True, False)])
def test_the_variable_wins_both_ways(
    monkeypatch: pytest.MonkeyPatch, value: str, set_up: bool, on: bool
) -> None:
    if set_up:
        _set_up("device")
    monkeypatch.setenv(experimental.REMOTE_ENV, value)

    assert experimental.remote_enabled() is on
    assert experimental.remote_forced() is on


# --- Doctor and `rlx config` ------------------------------------------------------------------


@pytest.mark.parametrize("on", [False, True])
def test_doctor_shows_both_switches_in_one_ok_row(
    monkeypatch: pytest.MonkeyPatch, captain_off: None, on: bool
) -> None:
    monkeypatch.setenv(experimental.REMOTE_ENV, "1" if on else "0")

    row = diagnostics._experimental_check()  # doctor's one row: tests/test_captain_flag.py

    assert row.name == "experimental" and row.status.value == "ok", row
    detail = row.detail
    assert detail.startswith("captain off, remote ") and f"remote {'on' if on else 'off'}" in detail
    assert f"({experimental.REMOTE_ENV})" in detail, "the variable decides: said"
    assert "config set experimental.captain true" in detail


def test_doctor_in_rlx_says_where_in_settings_and_names_no_command(
    monkeypatch: pytest.MonkeyPatch, captain_off: None, decides: None
) -> None:
    monkeypatch.setenv(prog.IN_APP_ENV, "1")

    row = diagnostics._experimental_check()

    assert "Settings > App settings > Experimental" in row.detail, row.detail
    assert "config set" not in row.detail


def test_doctor_says_a_remote_set_up_before_the_switch_reads_on(
    captain_off: None, decides: None
) -> None:
    _set_up("token")

    row = diagnostics._experimental_check()

    assert "remote on (set up before it had a switch)" in row.detail, row.detail


def _got(runner: CliRunner) -> str:
    """``rlx config get experimental.remote``'s value."""
    return (
        runner.invoke(cli, ["config", "get", "experimental.remote"]).output.split("=")[-1].strip()
    )


def test_config_list_and_get_show_both_flags_remote_as_it_reads(decides: None) -> None:
    runner = CliRunner()
    assert _got(runner) == "false"
    listed = runner.invoke(cli, ["config", "list"]).output
    assert "[experimental]" in listed and "captain = " in listed and "remote = false" in listed

    _set_up("device")
    assert _got(runner) == "true"
    assert runner.invoke(cli, ["config", "set", "experimental.remote", "false"]).exit_code == 0
    assert _got(runner) == "false"
    assert load_config().experimental.remote is False, "written: an explicit off"


# --- `rlx remote` -----------------------------------------------------------------------------


def test_rlx_remote_exits_2_while_off_and_its_help_still_prints(remote_off: None) -> None:
    runner = CliRunner()

    refused = runner.invoke(cli, ["remote", "status"])
    helped = runner.invoke(cli, ["remote", "--help"])

    assert refused.exit_code == 2 and "Remote is experimental and off" in refused.output
    assert helped.exit_code == 0 and "status" in helped.output


# --- nothing of Remote loads while it is off ---------------------------------------------------

_STARTED_TUI = textwrap.dedent(
    """
    import asyncio, sys
    from aisquare.core import claude_accounts as accounts
    accounts.KEYCHAIN_TOOL = "/nonexistent/security"  # no login Keychain is ever asked
    accounts.keychain_platform = lambda: False
    import aisquare.cli.app
    from aisquare.cli.ui.app import FleetApp
    from aisquare.cli.ui.sidebar import AppSettingsSelected

    async def main():
        app = FleetApp(refresh_seconds=3600, doctor=lambda: [], accounts=None, headroom=None)
        async with app.run_test(headless=True, size=(120, 40)) as pilot:
            await pilot.pause()
            key = app.check_action("remote_panel", ())  # the sidebar has the focus at start
            await pilot.press("R")
            app.post_message(AppSettingsSelected())
            for _ in range(5):
                await pilot.pause()
            print("ON", app.remote_on, getattr(app, "remote", None) is not None, key)
            print("TABS", len(app.query("#tab-remote")), len(app.query("#remote-chip")))
            print("LOADED", " ".join(sorted(m for m in sys.modules if m in {modules!r})))
        app.exit()

    asyncio.run(main())
    """
)


def _started(variable: str) -> dict[str, str]:
    """What a TUI started headless in a fresh interpreter reports, Remote set ``variable``."""
    code = _STARTED_TUI.replace("{modules!r}", repr(set(REMOTE_MODULES)))
    env = {**__import__("os").environ, experimental.REMOTE_ENV: variable}
    result = subprocess.run(
        [sys.executable, "-c", code], capture_output=True, text=True, env=env, timeout=120
    )
    assert result.returncode == 0, result.stderr[-2000:]
    lines = dict(line.split(" ", 1) for line in result.stdout.splitlines() if " " in line)
    return {key: lines.get(key, "") for key in ("ON", "TABS", "LOADED")}


@pytest.mark.skipif(sys.platform == "win32", reason="the headless start is POSIX's here")
def test_with_remote_off_a_started_tui_loads_and_shows_nothing_of_it() -> None:
    """Acceptance 1: off, no module of Remote in sys.modules after a start, the R key, the
    Settings it opens and all: no tab, no chip. On, the same start loads them (the control:
    the check can fail)."""
    off = _started("0")
    on = _started("1")

    assert off["ON"] == "False False False", off
    assert off["TABS"] == "0 0", off
    assert off["LOADED"] == "", off
    assert on["ON"] == "True True True", on
    assert on["TABS"] == "1 1", on
    assert set(on["LOADED"].split()) == set(REMOTE_MODULES), on


# --- the section, and the switch live ----------------------------------------------------------


def _palette_has_remote(app: object) -> bool:
    """Whether the command palette offers *Remote control* (it does while Remote is on)."""
    from aisquare.cli.ui.app import FleetApp

    assert isinstance(app, FleetApp)
    return any(command.title == "Remote control" for command in app.get_system_commands(app.screen))


async def _experimental(pilot: Pilot[None]) -> ExperimentalSection:
    """Settings, then its *Experimental* link: App settings' last section."""
    app = fleet_app(pilot)
    await pilot.click(app.query_one(SettingsRow))
    await settle_page(app)
    await pilot.click("#hub-experimental")
    await settle_page(app)
    return app.query_one(ExperimentalSection)


def test_a_fresh_home_shows_experimental_with_captain_and_remote_both_off(
    script: Script, captain_off: None, decides: None
) -> None:
    async def body(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        section = await _experimental(pilot)
        tabs = app.query_one("#app-settings-tabs", TabbedContent)
        return {
            "captain box": section.query_one(CaptainToggle).query_one(Checkbox).value,
            "remote box": section.query_one(RemoteToggle).query_one(Checkbox).value,
            "remote tab": len(tabs.query(f"#{REMOTE_TAB}")),
            "hub link": app.query_one("#hub-remote-row").display,
            "controller": getattr(app, "remote", None),
            "palette": _palette_has_remote(app),
            "chip": len(app.query("#remote-chip")),
        }

    assert drive(body) == {
        "captain box": False,
        "remote box": False,
        "remote tab": 0,
        "hub link": False,
        "controller": None,
        "palette": False,
        "chip": 0,
    }


def test_the_remote_box_brings_remote_up_and_down_at_once_and_ctrl_z_takes_it_back(
    script: Script, decides: None
) -> None:
    async def body(pilot: Pilot[None]) -> list[tuple[object, ...]]:
        app = fleet_app(pilot)
        section = await _experimental(pilot)
        box = section.query_one("#remote-toggle", Checkbox)
        tabs = app.query_one("#app-settings-tabs", TabbedContent)

        def state() -> tuple[object, ...]:
            return (
                box.value,
                getattr(app, "remote", None) is not None,
                len(tabs.query(f"#{REMOTE_TAB}")),
                app.query_one("#hub-remote-row").display,
                _palette_has_remote(app),
                len(app.query("#remote-chip")),
                load_config().experimental.remote,
            )

        seen = [state()]
        box.focus()
        box.toggle()
        await settle_page(app)
        seen.append(state())
        controller = getattr(app, "remote", None)
        await pilot.press("ctrl+z")
        await settle_page(app)
        seen.append(state())
        seen.append((controller in app.remotes_stopping,))
        return seen

    before, on, off, stopping = drive(body)
    assert before == (False, False, 0, False, False, 0, None)
    assert on == (True, True, 1, True, True, 1, True)
    assert off == (False, False, 0, False, False, 0, False), "ctrl+z: off again, and kept"
    assert stopping == (True,), "stopped on its own thread; run_ui waits for it at the quit"
