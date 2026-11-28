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
import logging
import subprocess
import sys
import textwrap
import threading
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest
from textual.app import App
from textual.pilot import Pilot
from textual.widgets import Checkbox, TabbedContent
from typer.testing import CliRunner

from aisquare.cli.app import app as cli
from aisquare.cli.ui import app as ui_app
from aisquare.cli.ui import remote_control, remote_workers
from aisquare.cli.ui.remote_control import RemoteController
from aisquare.cli.ui.sidebar import SettingsRow
from aisquare.cli.ui.views.app_settings import REMOTE_TAB
from aisquare.cli.ui.views.settings_captain import (
    CaptainToggle,
    ExperimentalSection,
    RemoteSwitched,
    RemoteToggle,
)
from aisquare.core import experimental, paths
from aisquare.core import program as prog
from aisquare.core.config import load_config, salvaged_remote, save_config
from aisquare.core.state_file import update_state
from aisquare.services import diagnostics, remote_server, service_host
from aisquare.services.service_host import RemotePart
from tests import test_ui_shell as ui_suite
from tests.test_captain_view import drive
from tests.test_remote_control import FakeServer, fake_server, fake_tunnel_factory
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


# --- Reset all and `init --reinit` keep a decided switch (the manager's Q3 on #46) ----------


#: (what the person chose, whether the home had Remote set up, how it reads after): each a
#: case where losing the choice would flip how Remote reads. Undecided stays undecided.
KEPT = [(False, True, False), (True, False, True), (None, True, True)]
KEPT_IDS = ["off-on-a-set-up-home", "on-on-a-fresh-home", "undecided-on-a-set-up-home"]


def _choose(chosen: bool | None, set_up: bool) -> None:
    if set_up:
        _set_up("token")
    config = load_config()
    config.experimental.remote = chosen
    save_config(config)


@pytest.mark.parametrize(("chosen", "set_up", "on"), KEPT, ids=KEPT_IDS)
def test_reset_all_keeps_the_switch_and_a_file_with_only_it_holds_nothing_to_reset(
    decides: None, chosen: bool | None, set_up: bool, on: bool
) -> None:
    """As S2's Reset all keeps ``[service]``: forgotten, an off on a home set up before the
    switch read as on again, and an on of a fresh home read as off."""
    from aisquare.services import settings as settings_service

    _choose(chosen, set_up)
    settings_service.set_value("fleet.escape_key", "f11")

    backup = settings_service.reset_settings()

    assert backup is not None and "f11" in backup.read_text(encoding="utf-8")
    assert load_config().fleet.escape_key != "f11"
    assert load_config().experimental.remote is chosen
    assert experimental.remote_enabled() is on
    assert settings_service.anything_to_reset() is False
    assert settings_service.reset_settings() is None


@pytest.mark.parametrize(("chosen", "set_up", "on"), KEPT, ids=KEPT_IDS)
def test_init_reinit_keeps_the_switch(
    decides: None, tmp_path: Path, chosen: bool | None, set_up: bool, on: bool
) -> None:
    project = tmp_path / "project"
    project.mkdir()
    first = CliRunner().invoke(cli, ["init", "--local", "--yes", "--no-onboard", str(project)])
    assert first.exit_code == 0, first.output
    _choose(chosen, set_up)

    again = CliRunner().invoke(
        cli, ["init", "--reinit", "--yes", "--local", "--no-onboard", str(project)]
    )

    assert again.exit_code == 0, again.output
    assert load_config().experimental.remote is chosen
    assert experimental.remote_enabled() is on


@pytest.mark.parametrize("command", [["config", "reset", "--yes"], ["init", "--reinit", "--yes"]])
def test_a_reset_of_a_broken_file_keeps_an_off_read_from_its_text(
    decides: None, tmp_path: Path, command: list[str]
) -> None:
    """The reset rlx recommends for a file that no longer parses must not lose the off, as
    S2's does not lose ``[service]``'s (tester of #33, round 2)."""
    _choose(False, set_up=True)
    path = paths.config_path()
    path.write_text(
        path.read_text(encoding="utf-8") + "\n[ui]\nthis is = not [toml\n", encoding="utf-8"
    )
    project = tmp_path / "project"
    project.mkdir()
    more = ["--local", "--no-onboard", str(project)] if command[0] == "init" else []

    reset = CliRunner().invoke(cli, [*command, *more])

    assert reset.exit_code == 0, reset.output
    assert load_config().experimental.remote is False
    assert experimental.remote_enabled() is False


def test_the_switch_is_salvaged_from_its_own_table_alone() -> None:
    assert salvaged_remote(b"[ui]\nremote = true\n[experimental]\nremote = false # off\n") is False
    assert salvaged_remote(b"[experimental]\ncaptain = true\n[ui]\nremote = true\n") is None
    assert salvaged_remote(b"[ experimental ]\n  remote = true\n\xff[oops\n") is True


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
            # A tick, the housekeeping tick (Remote's auto-off check), then the quit's own path.
            app.refresh_data()
            app._housekeeping()
            for _ in range(5):
                await pilot.pause()
            await app.action_quit()
        print("QUIT", " ".join(sorted(m for m in sys.modules if m in {modules!r})))

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
    return {key: lines.get(key, "") for key in ("ON", "TABS", "LOADED", "QUIT")}


@pytest.mark.skipif(sys.platform == "win32", reason="the headless start is POSIX's here")
def test_with_remote_off_a_started_tui_loads_and_shows_nothing_of_it() -> None:
    """Acceptance 1: off, no module of Remote in sys.modules after a start, the R key, the
    Settings it opens and all: no tab, no chip; nor after a tick, the housekeeping tick and the
    quit (review of #46, #3780). On, the same start loads them (the control: the check can
    fail)."""
    off = _started("0")
    on = _started("1")

    assert off["ON"] == "False False False", off
    assert off["TABS"] == "0 0", off
    assert off["LOADED"] == "", off
    assert off["QUIT"] == "", off
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


class _HeldWrites(FakeServer):
    """Remote's server and ``remote.json``, one for every controller as the real runtime is.
    The write switch's write waits for the test's word (a write in flight), and starts are
    recorded beside the stops."""

    def __init__(self) -> None:
        super().__init__()
        self.held = threading.Event()

    def set_allow_write(self, enabled: bool) -> None:
        assert self.held.wait(30), "the test never let the write land"
        super().set_allow_write(enabled)

    def start_remote_server(
        self, dist_dir: Path | None, port: int = 8750
    ) -> remote_server.RemoteInfo:
        self.calls.append("start_remote_server")
        return super().start_remote_server(dist_dir, port)


async def _until(pilot: Pilot[None], holds: Callable[[], object], what: str) -> None:
    for _ in range(600):  # 30 s at most
        if holds():
            return
        await pilot.pause(0.05)
    raise AssertionError(f"never: {what}")


def _one_server(monkeypatch: pytest.MonkeyPatch) -> tuple[_HeldWrites, list[RemoteController]]:
    """One server for every controller the app builds, each recorded as it is made, with
    Remote's own switch on, so that each start serves."""
    server = _HeldWrites()
    made: list[RemoteController] = []

    def controller() -> RemoteController:
        made.append(
            RemoteController(server=server, tunnel_factory=fake_tunnel_factory(url="x.ngrok.app"))
        )
        return made[-1]

    monkeypatch.setattr(remote_control, "RemoteController", controller)
    update_state("remote_enabled", True)
    return server, made


async def _a_write_in_flight(pilot: Pilot[None], made: list[RemoteController]) -> RemoteController:
    """The Remote the start brought up, serving, with a write of it held in flight."""
    await _until(pilot, lambda: made and made[0].remote_serving, "the first Remote serving")
    first = made[0]
    first.set_allow_write(True, wait=False)
    return first


def _waiting(app: App[Any]) -> bool:
    """Whether a Remote turned on again still waits for the one turning off."""
    return any(
        worker.name == remote_workers.RESTORE_WORKER and not worker.is_finished
        for worker in app.workers
    )


def _starts_after_the_last_stop(server: _HeldWrites) -> int:
    calls = list(server.calls)
    last_stop = len(calls) - 1 - calls[::-1].index("stop_remote_server")
    return calls[last_stop:].count("start_remote_server")


def _still_turning_off(app: App[Any]) -> list[str]:
    """Every toast saying Remote is still turning off from now on, with its severity, as it is
    raised (a toast's own timeout can end before a slow runner reads the screen's)."""
    said: list[str] = []
    notify = app.notify

    def spy(message: str, **kwargs: Any) -> None:
        if message == remote_control.STILL_TURNING_OFF:
            said.append(str(kwargs.get("severity", "information")))
        notify(message, **kwargs)

    app.notify = spy  # type: ignore[method-assign]
    return said


def test_remote_off_then_on_at_once_waits_for_the_one_still_stopping(
    script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Review of #46 (#3780): a quick off then on built a new controller and brought it up at
    once, beside the one still stopping with a write in flight. The two share ``remote.json``
    and the server's runtime, and their locks are each their own. The new one now comes up only
    once the old one is off and its write has landed: one controller left, serving, and the
    write switch as a restore leaves it. The old one's write landed first and the restore then
    turned writes off (``_restored_read_only``). Without the wait, the held write landed after
    the restore: writes on behind a Remote that said it came back read-only."""
    server, made = _one_server(monkeypatch)

    async def body(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        old = await _a_write_in_flight(pilot, made)
        app.post_message(RemoteSwitched(False))
        app.post_message(RemoteSwitched(True))
        await _until(pilot, lambda: len(made) == 2, "a second controller")
        new = made[1]
        for _ in range(10):
            await pilot.pause(0.02)
        held = new.remote_serving
        server.held.set()
        await _until(pilot, lambda: new.remote_serving, "the new Remote serving")
        await settle_page(app)
        return {
            "new serving while the old one's write was in flight": held,
            "the app's controller is the new one": getattr(app, "remote", None) is new,
            "the old one is off": old.wait_until_off(5) and not old.remote_serving,
            "server running": server.running,
            "write switch": (server.allow_write, new.write_actions_allowed()),
            "its writes, in order": list(server.allow_write_calls),
            "starts after the last stop": _starts_after_the_last_stop(server),
        }

    assert drive(body) == {
        "new serving while the old one's write was in flight": False,
        "the app's controller is the new one": True,
        "the old one is off": True,
        "server running": True,
        "write switch": (False, False),
        "its writes, in order": [True, False],
        "starts after the last stop": 1,
    }


def test_remote_off_on_and_off_again_ends_the_wait_and_on_once_more_comes_up_once(
    script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Review of #46 (#3839), a third toggle: Remote off, on and off again while the first
    one's write is still in flight. The second is never brought up, and its wait ends at its
    next look, not with the write: there is nothing left for it to bring up. Turned on a
    fourth time, the third comes up once the first is off: one start after the last stop."""
    server, made = _one_server(monkeypatch)

    async def body(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        said = _still_turning_off(app)
        first = await _a_write_in_flight(pilot, made)
        for on in (False, True, False):
            app.post_message(RemoteSwitched(on))
        await _until(pilot, lambda: len(made) == 2 and app._remote is None, "off once more")
        second = made[1]
        await _until(pilot, lambda: not _waiting(app), "the second one's wait over")
        ended_in_flight = not first.writes_done(0)
        app.post_message(RemoteSwitched(True))
        await _until(pilot, lambda: len(made) == 3, "a third controller")
        third = made[2]
        for _ in range(10):
            await pilot.pause(0.02)
        held = third.remote_serving
        server.held.set()
        await _until(pilot, lambda: third.remote_serving, "the third Remote serving")
        await settle_page(app)
        return {
            "the second one's wait ended with the write in flight": ended_in_flight,
            "the third serving while the write was in flight": held,
            "the app's controller is the third": app._remote is third,
            "the second, never up, among those stopping": (
                not second.remote_serving and second in app.remotes_stopping
            ),
            "starts, in all": server.calls.count("start_remote_server"),
            "starts after the last stop": _starts_after_the_last_stop(server),
            "said still turning off": said,
        }

    assert drive(body) == {
        "the second one's wait ended with the write in flight": True,
        "the third serving while the write was in flight": False,
        "the app's controller is the third": True,
        "the second, never up, among those stopping": True,
        "starts, in all": 2,
        "starts after the last stop": 1,
        "said still turning off": [],
    }


def test_a_wait_past_its_patience_gives_up_says_so_and_brings_nothing_up(
    script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Review of #46 (#3839): the wait for the one still turning off was unbounded, so a stop
    that never ended parked its worker for good. Past its patience it ends with the write
    still in flight, Remote stays down, and a warning says what Remote's own switch says to a
    start it refuses. The write landing later brings nothing up by itself."""
    server, made = _one_server(monkeypatch)
    monkeypatch.setattr(ui_app, "_RESTORE_PATIENCE", 0.5)

    async def body(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        said = _still_turning_off(app)
        first = await _a_write_in_flight(pilot, made)
        app.post_message(RemoteSwitched(False))
        app.post_message(RemoteSwitched(True))
        await _until(pilot, lambda: len(made) == 2, "a second controller")
        second = made[1]
        await _until(pilot, lambda: said and not _waiting(app), "the wait given up")
        gave_up_in_flight = not first.writes_done(0)
        server.held.set()
        await _until(pilot, lambda: first.writes_done(0) and first.wait_until_off(0), "off")
        for _ in range(10):
            await pilot.pause(0.02)
        await settle_page(app)
        return {
            "gave up with the write in flight": gave_up_in_flight,
            "said still turning off": said,
            "the second serving": second.remote_serving,
            "the app's controller is the second": app._remote is second,
            "starts after the last stop": _starts_after_the_last_stop(server),
        }

    assert drive(body) == {
        "gave up with the write in flight": True,
        "said still turning off": ["warning"],
        "the second serving": False,
        "the app's controller is the second": True,
        "starts after the last stop": 0,
    }


def test_a_quit_while_remote_waits_for_the_one_turning_off_is_not_held_by_the_wait(
    script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Review of #46 (#3839): rlx quit while a Remote turned on again waits for the one still
    turning off. Textual cancels the wait's worker as it exits, and the wait ends at its next
    look, so the run's end, whose executor joins the worker's thread, is not held until the
    write lands (held 30 s here). run_ui waits for the one turning off itself, as before."""
    server, made = _one_server(monkeypatch)
    left: list[float] = []

    async def body(pilot: Pilot[None]) -> None:
        app = fleet_app(pilot)
        await _a_write_in_flight(pilot, made)
        app.post_message(RemoteSwitched(False))
        app.post_message(RemoteSwitched(True))
        await _until(pilot, lambda: len(made) == 2 and _waiting(app), "the wait running")
        left.append(time.monotonic())

    try:
        ui_suite.drive(body)
        took = time.monotonic() - left[0]
        in_flight = not made[0].writes_done(0)
    finally:
        server.held.set()
    assert made[0].writes_done(10) and made[0].wait_until_off(10)
    assert in_flight, "the write landed before the run ended: nothing was held"
    assert took < 10, f"the run's end waited {took:.1f} s for the wait"
    assert not made[1].remote_serving
    assert server.calls.count("start_remote_server") == 1


# --- the rlx service ---------------------------------------------------------------------------

_SERVICE_PART = textwrap.dedent(
    """
    import sys
    from aisquare.core.state_file import update_state
    from aisquare.services.service_host import RemotePart
    # Remote's own switch on while it is off in Experimental (nothing of it may load even so),
    # and off for the control, so that no real server or ngrok ever starts here.
    update_state("remote_enabled", sys.argv[1] == "0")
    part = RemotePart()
    part.start_part()
    health = part.tick_part()
    print("PART", health.state, "|", health.detail)
    print("LOADED", " ".join(sorted(m for m in sys.modules if m in {modules!r})))
    """
)


def _service_part(variable: str) -> dict[str, str]:
    """What the rlx service's Remote part reports, started and ticked once in a fresh
    interpreter with Remote set ``variable``, and which of Remote's modules it loaded."""
    code = _SERVICE_PART.replace("{modules!r}", repr(set(REMOTE_MODULES)))
    env = {**__import__("os").environ, experimental.REMOTE_ENV: variable}
    result = subprocess.run(
        [sys.executable, "-c", code, variable], capture_output=True, text=True, env=env, timeout=120
    )
    assert result.returncode == 0, result.stderr[-2000:]
    lines = dict(line.split(" ", 1) for line in result.stdout.splitlines() if " " in line)
    return {key: lines.get(key, "") for key in ("PART", "LOADED")}


@pytest.mark.skipif(sys.platform == "win32", reason=service_host.NOT_HERE)
def test_with_remote_off_the_service_loads_nothing_of_it() -> None:
    """Off, the service's Remote part imports nothing of Remote and says why, its own switch
    on or not. On, the same start loads its controller (the control: the check can fail)."""
    off = _service_part("0")
    on = _service_part("1")

    assert off["PART"] == f"off | {service_host.EXPERIMENTAL_OFF}", off
    assert off["LOADED"] == "", off
    assert on["PART"] == "off | switched off", on
    assert {"aisquare.cli.ui.remote_control", "aisquare.services.remote_server"} <= set(
        on["LOADED"].split()
    ), on


def _choose_remote(on: bool) -> None:
    """Remote's box in Settings' *Experimental*, as it saves."""
    config = load_config()
    config.experimental.remote = on
    save_config(config)


@pytest.mark.skipif(sys.platform == "win32", reason=service_host.NOT_HERE)
def test_the_service_starts_and_stops_remote_as_experimental_turns_it_on_and_off(
    decides: None, caplog: pytest.LogCaptureFixture
) -> None:
    """Off in Experimental, the service makes no controller and serves nothing, though
    Remote's own switch is on. Turned on, it serves at the next follow; off, it stops the
    Remote it serves and says why, the switch kept; on again, it serves again."""
    update_state("remote_enabled", True)  # Remote's own switch, as a TUI saved it
    _choose_remote(False)
    server = fake_server()
    made: list[RemoteController] = []

    def controller() -> RemoteController:
        tunnel = fake_tunnel_factory(url="https://quiet-otter.ngrok-free.app")
        made.append(RemoteController(server=server, tunnel_factory=tunnel))
        return made[-1]

    part = RemotePart(controller)
    part.start_part()
    first = part.tick_part()

    assert (first.state, first.detail, first.extra["on"]) == (
        "off",
        service_host.EXPERIMENTAL_OFF,
        False,
    )
    assert (made, part.controller, server.running) == ([], None, False)

    _choose_remote(True)
    assert part.tick_part().state == "ok" and server.running and len(made) == 1

    _choose_remote(False)
    with caplog.at_level(logging.INFO, logger=service_host.__name__):
        stopped = part.tick_part()
    assert (stopped.state, stopped.detail) == ("off", service_host.EXPERIMENTAL_OFF)
    assert not server.running
    assert made[0].message == service_host.EXPERIMENTAL_OFF_REASON
    assert f"remote: {service_host.EXPERIMENTAL_OFF_REASON}" in caplog.messages
    assert remote_control.load_remote_state().remote_enabled, "its own switch kept"

    _choose_remote(True)
    assert part.tick_part().state == "ok" and server.running and len(made) == 1
    part.stop_part()
    assert not server.running
