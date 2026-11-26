"""The rlx service's tray (#34): its menu, *Quit rlx*'s question, *Open rlx*'s terminal per OS,
the service part that keeps the tray's process running, the Linux StatusNotifierItem's answers,
and the switches in App settings.

No real tray runs (``tests/conftest.py::no_real_tray``): the service part's spawn, the tray's
commands and terminals, and the backends are stubs here; the Linux item is driven with
jeepney's own messages against no bus, and the macOS one is never imported.
"""

from __future__ import annotations

import json
import os
import shlex
import shutil
import stat
import sys
from collections.abc import Mapping
from pathlib import Path
from typing import Any

import pytest
from jeepney import DBusAddress, HeaderFields, MessageType, new_method_call
from textual.pilot import Pilot
from textual.widgets import Select, Switch, TabbedContent

from aisquare.cli.ui.sidebar import AppSettingsSelected
from aisquare.cli.ui.views.app_settings import PREFERENCES_TAB
from aisquare.cli.ui.views.settings_tray import TrayToggle, terminal_options
from aisquare.core import selfcli
from aisquare.core.config import AppConfig, TraySettings, load_config, save_config
from aisquare.services import service_host, service_tray, tray, tray_linux
from aisquare.services.service_tray import TrayPart, why_no_tray
from aisquare.services.tray import (
    OPEN,
    QUIT,
    QUIT_NOW,
    RESTART,
    Summary,
    Tray,
    TrayError,
    menu_items,
    quit_question,
    status_lines,
    summary_of,
    terminal_argv,
)
from tests.test_ui_shell import drive, fleet_app, settle
from tests.test_ui_shell import no_real_tmux as _no_real_tmux
from tests.test_ui_shell import script as _script

no_real_tmux = _no_real_tmux
script = _script

AGENTS = ["coder-1 (repo)", "reviewer (repo)", "tester (repo)", "manager (repo)", "coder-2 (api)"]


def _status(agents: list[str] | None = None, room: int = 2, accounts: int = 3) -> dict[str, Any]:
    return {
        "pid": 4242,
        "parts": {
            "usage": {
                "state": "ok",
                "detail": "claude-code: ok",
                "room": room,
                "accounts": accounts,
            },
            "tray": {"state": "ok", "agents": agents if agents is not None else AGENTS[:2]},
        },
    }


# --- what the menu says ---------------------------------------------------------------------------


def test_the_status_lines_count_the_agents_and_the_accounts_with_room() -> None:
    summary = summary_of(_status(AGENTS))
    assert status_lines(summary) == ["5 agents running", "Accounts with room: 2 of 3"]
    assert status_lines(summary_of(_status([]))) == [
        "No agents running",
        "Accounts with room: 2 of 3",
    ]
    assert status_lines(summary_of(_status(["coder-1"]))) == [
        "1 agent running",
        "Accounts with room: 2 of 3",
    ]
    assert status_lines(summary_of(None)) == ["The rlx service is not running"]


def test_an_odd_status_file_is_read_as_far_as_it_goes() -> None:
    odd = {"parts": {"usage": {"room": True, "accounts": "3"}, "tray": {"agents": "coder"}}}
    assert summary_of(odd) == Summary(running=True)
    assert summary_of({"parts": []}) == Summary(running=True)
    assert summary_of([1, 2]) == Summary(running=False)  # type: ignore[arg-type]


@pytest.mark.parametrize(
    ("agents", "said"),
    [
        ([], "Remote and the usage reads stop until rlx starts again."),
        (
            ["coder-1"],
            "coder-1 keeps running in tmux, without rlx: no handovers at their limits, "
            "and no Remote.",
        ),
        (
            AGENTS[:2],
            "coder-1 (repo) and reviewer (repo) keep running in tmux, without rlx: no handovers "
            "at their limits, and no Remote.",
        ),
        (
            AGENTS,
            "coder-1 (repo), reviewer (repo), tester (repo) and 2 more keep running in tmux, "
            "without rlx: no handovers at their limits, and no Remote.",
        ),
    ],
    ids=["none", "one", "two", "five"],
)
def test_quit_asks_first_and_names_the_agents_it_leaves(agents: list[str], said: str) -> None:
    """The ruling on #34, Q1: Quit stops the service only; the agents run on in tmux, and the
    question says what they lose."""
    assert quit_question(agents) == ("Quit rlx?", said)


def test_the_menu_opens_rlx_first_then_restarts_quits_and_says_how_things_are() -> None:
    """The brief's order, with *Open rlx* first and the default: on macOS, where the menu bar
    has no double-click, it is how rlx opens (the ruling on #34, Q2)."""
    rows = menu_items(summary_of(_status()), ask_in_menu=False)
    assert [(row.label, row.action) for row in rows] == [
        ("Open rlx", OPEN),
        ("Restart rlx", RESTART),
        ("Quit rlx…", QUIT),
        ("", None),
        ("2 agents running", None),
        ("Accounts with room: 2 of 3", None),
    ]
    assert rows[0].default and not any(row.default for row in rows[1:])
    assert rows[3].separator


def test_on_linux_quit_is_a_submenu_that_asks() -> None:
    """A StatusNotifierItem has no dialog: *Quit rlx…* opens the question and *Quit rlx now*."""
    rows = menu_items(summary_of(_status(AGENTS[:1])), ask_in_menu=True, error="Could not open: x")
    quit_row = rows[2]
    assert quit_row.label == "Quit rlx…" and quit_row.action is None
    assert [(child.label, child.action) for child in quit_row.children] == [
        (quit_question(AGENTS[:1])[1], None),
        ("Quit rlx now", QUIT_NOW),
    ]
    assert rows[-1].label == "Could not open: x", "what went wrong last, the menu's last line"


# --- Open rlx: a terminal per OS ------------------------------------------------------------------

RLX = ["/home/me/.local/bin/rlx"]


def _which(*found: str) -> Any:
    return lambda name: f"/usr/bin/{name}" if name in found else None


def test_on_macos_rlx_opens_through_the_command_file() -> None:
    script_file = Path("/h/.rlx/service/open-rlx.command")
    assert terminal_argv("darwin", rlx=RLX, command_file=script_file) == ["open", str(script_file)]
    assert terminal_argv("darwin", rlx=RLX, choice="iTerm", command_file=script_file) == [
        "open",
        "-a",
        "iTerm",
        str(script_file),
    ]
    with pytest.raises(TrayError):
        terminal_argv("darwin", rlx=RLX)


@pytest.mark.parametrize(
    ("env", "found", "argv"),
    [
        ({}, ("xdg-terminal-exec", "xterm"), ["xdg-terminal-exec", *RLX]),
        ({"TERMINAL": "foot --app-id=x"}, ("xterm",), ["foot", "--app-id=x", "-e", *RLX]),
        ({}, ("konsole", "xterm"), ["konsole", "-e", *RLX]),
        ({}, ("gnome-terminal",), ["gnome-terminal", "--", *RLX]),
        ({}, ("kitty",), ["kitty", *RLX]),
        ({}, ("xterm",), ["xterm", "-e", *RLX]),
    ],
    ids=["xdg-terminal-exec", "TERMINAL", "konsole", "gnome-terminal", "kitty", "xterm"],
)
def test_on_linux_rlx_opens_in_the_persons_terminal_then_a_common_one(
    env: dict[str, str], found: tuple[str, ...], argv: list[str]
) -> None:
    assert terminal_argv("linux", rlx=RLX, env=env, which=_which(*found)) == argv


def test_on_linux_a_named_terminal_wins_and_none_found_is_said() -> None:
    which = _which("xdg-terminal-exec")
    assert terminal_argv("linux", rlx=RLX, choice="wezterm", which=which) == [
        "wezterm",
        "start",
        "--",
        *RLX,
    ]
    assert terminal_argv("linux", rlx=RLX, choice="tilix", which=which) == ["tilix", "-e", *RLX]
    assert terminal_argv("linux", rlx=RLX, choice="tilix --new-process -e", which=which) == [
        "tilix",
        "--new-process",
        "-e",
        *RLX,
    ]
    with pytest.raises(TrayError, match="App settings"):
        terminal_argv("linux", rlx=RLX, env={}, which=_which())


def test_the_command_file_runs_rlx_on_this_services_home() -> None:
    text = tray.command_script(["/a b/rlx"], {"RLX_HOME": "/h/my home", "PATH": "/x"})
    assert text == "#!/bin/sh\nexport RLX_HOME='/h/my home'\nexec '/a b/rlx'\n"


def test_every_running_agent_is_counted_though_twenty_are_named() -> None:
    """The service keeps 20 names (``AGENTS_KEPT``) and the count of all: with 25 agents the
    menu said "20 agents running" and Quit "and 17 more" (review of #602, m1)."""
    names = [f"coder-{n}" for n in range(1, 21)]
    record = {"parts": {"tray": {"agents": names, "agents_running": 25}}}
    summary = summary_of(record)
    assert status_lines(summary)[0] == "25 agents running"
    assert quit_question(summary.agents, summary.count)[1].startswith(
        "coder-1, coder-2, coder-3 and 22 more keep running"
    )
    rows = menu_items(summary, ask_in_menu=True)
    assert rows[2].children[0].label == quit_question(summary.agents, summary.count)[1]


# --- the actions ----------------------------------------------------------------------------------


@pytest.fixture
def spawned(tray_seams: None, monkeypatch: pytest.MonkeyPatch) -> dict[str, list[list[str]]]:
    """The tray's two spawn seams, recorded instead of run."""
    seen: dict[str, list[list[str]]] = {"rlx": [], "terminal": []}
    monkeypatch.setattr(tray, "_spawn_rlx", lambda args: seen["rlx"].append(list(args)))
    monkeypatch.setattr(tray, "_spawn_terminal", lambda argv: seen["terminal"].append(list(argv)))
    monkeypatch.setattr(selfcli, "argv_for", lambda args: [*RLX, *args])
    return seen


def test_restart_and_quit_hand_their_command_to_a_detached_rlx(
    spawned: dict[str, list[list[str]]],
) -> None:
    item = Tray(platform="linux", read_status=lambda: _status())
    item.act(RESTART)
    item.act(QUIT)  # the backend's to ask: nothing yet
    item.act(QUIT_NOW)
    assert spawned["rlx"] == [["service", "restart"], ["service", "stop"]]
    assert item.error is None


def test_open_on_linux_starts_the_terminal(
    spawned: dict[str, list[list[str]]], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(shutil, "which", _which("konsole"))
    monkeypatch.delenv("TERMINAL", raising=False)
    Tray(platform="linux", read_status=dict).act(OPEN)
    assert spawned["terminal"] == [["konsole", "-e", *RLX]]


def test_open_on_macos_writes_an_owner_only_script_for_this_home(
    spawned: dict[str, list[list[str]]], tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    home = tmp_path / "home"
    monkeypatch.setenv("AISQUARE_HOME", str(home))
    Tray(platform="darwin", read_status=dict, terminal="Ghostty").act(OPEN)
    script_file = service_host.service_dir() / tray.COMMAND_FILE
    assert spawned["terminal"] == [["open", "-a", "Ghostty", str(script_file)]]
    assert script_file.read_text(encoding="utf-8").endswith(f"exec {RLX[0]}\n")
    export = f"export AISQUARE_HOME={shlex.quote(str(home))}"  # quoted where a path needs it
    assert export in script_file.read_text(encoding="utf-8")
    if sys.platform != "win32":
        assert stat.S_IMODE(script_file.stat().st_mode) == 0o700


def test_a_failed_action_is_the_menus_next_line(
    tray_seams: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    def fails(_args: Any) -> None:
        raise OSError("no such file")

    monkeypatch.setattr(tray, "_spawn_rlx", fails)
    item = Tray(platform="linux", read_status=lambda: _status())
    item.act(RESTART)
    assert item.error == "Could not restart: no such file"
    assert item.items()[-1].label == "Could not restart: no such file"
    item.act("nonsense")
    assert item.error == "Could not nonsense: nothing to do for 'nonsense'"


def test_a_terminal_setting_that_will_not_split_is_said_not_raised(
    spawned: dict[str, list[list[str]]],
) -> None:
    """``[tray] terminal`` with a quote left open: ``shlex.split`` raises ValueError, which the
    menu says (review of #602, m4)."""
    item = Tray(platform="linux", read_status=dict, terminal='konsole --title "rlx')
    item.act(OPEN)
    assert item.error == "Could not open: No closing quotation"
    assert spawned["terminal"] == []


def test_an_unreadable_status_is_a_menu_that_says_so() -> None:
    def broken() -> Mapping[str, Any]:
        raise PermissionError("status.json")

    assert Tray(platform="linux", read_status=broken).summary() == Summary(running=False)


def test_the_suite_never_starts_a_real_tray_terminal_or_command() -> None:
    """``tests/conftest.py::no_real_tray``: what a test leaves unstubbed refuses."""
    with pytest.raises(RuntimeError, match="no_real_tray"):
        tray._spawn_rlx(["service", "stop"])
    with pytest.raises(RuntimeError, match="no_real_tray"):
        tray.backend_for("linux")
    assert (
        why_no_tray(platform="linux", enabled=True, env={"DISPLAY": ":0", "XDG_RUNTIME_DIR": "/r"})
        == (service_tray.NO_TRAY_EXITS[service_tray.EXIT_NO_EXTRA])
    ), "the extra reads as missing: a test's service wants no tray"


# --- the tray's process ---------------------------------------------------------------------------


class Backend:
    def __init__(self) -> None:
        self.ran: list[tuple[Tray, str]] = []

    def run(self, shown: Tray, *, icon: str) -> int:
        self.ran.append((shown, icon))
        return 0


def test_the_tray_process_draws_with_what_the_service_hands_it(
    tray_seams: None, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``python -m aisquare.services.tray``: the settings and the service's paths on its
    command line (it reads no config: ``TrayPart``), drawn by the OS's half."""
    backend = Backend()
    monkeypatch.setattr(tray, "backend_for", lambda platform=None: backend)
    status = tmp_path / "status.json"
    status.write_text(json.dumps(_status(AGENTS)), encoding="utf-8")
    argv = ["--icon", "mark", "--terminal", "kitty", "--status", str(status), "--folder", "/f"]
    assert tray.main(argv) == 0
    ((shown, icon),) = backend.ran
    assert (icon, shown.terminal, shown.folder) == ("mark", "kitty", Path("/f"))
    assert shown.summary().agents == tuple(AGENTS), "the status file the service named"
    monkeypatch.setattr(tray, "backend_for", lambda platform=None: service_tray.EXIT_NO_HOST)
    assert tray.main([]) == service_tray.EXIT_NO_HOST


def test_rlx_service_tray_draws_it_here_with_the_settings(
    tray_seams: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The manual check's way to see it: ``rlx service tray`` reads ``[tray]`` itself."""
    from typer.testing import CliRunner

    from aisquare.cli.app import app

    config = load_config()
    config.tray = TraySettings(icon="template", terminal="foot")
    save_config(config)
    backend = Backend()
    monkeypatch.setattr(tray, "backend_for", lambda platform=None: backend)
    result = CliRunner().invoke(app, ["service", "tray"])
    assert result.exit_code == 0, result.output
    ((shown, icon),) = backend.ran
    assert (icon, shown.terminal) == ("template", "foot")


def test_a_tray_whose_service_has_ended_shows_nothing(
    tray_seams: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The service passes its pid (``--service-pid``): a service that ended while the OS's half
    loaded leaves no tray behind, whose parent would be another process (review of #602, m2)."""
    backend = Backend()
    monkeypatch.setattr(tray, "backend_for", lambda platform=None: backend)
    assert tray.main(["--service-pid", str(os.getppid() + 1)]) == 0
    assert backend.ran == [], "never drawn"
    assert tray.main(["--service-pid", str(os.getppid())]) == 0
    ((shown, _icon),) = backend.ran
    assert shown.service_pid == os.getppid() and not shown.service_gone()


def test_each_os_gets_its_half_or_the_exit_that_says_why_not(
    tray_seams: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("WSL_DISTRO_NAME", raising=False)
    assert isinstance(tray.backend_for("linux"), tray_linux.LinuxTray)
    assert tray.backend_for("win32") == service_tray.EXIT_NOT_HERE
    monkeypatch.setenv("WSL_DISTRO_NAME", "Ubuntu")
    assert tray.backend_for("linux") == service_tray.EXIT_NOT_HERE
    # macOS without the extra: pyobjc's import fails, and the tray says so by its exit.
    monkeypatch.setitem(sys.modules, "aisquare.services.tray_macos", None)
    assert tray.backend_for("darwin") == service_tray.EXIT_NO_EXTRA


# --- the service's part ---------------------------------------------------------------------------


class Child:
    """A stand-in for the tray's process."""

    count = 0

    def __init__(self) -> None:
        Child.count += 1
        self.pid = 1000 + Child.count
        self.returncode: int | None = None
        self.terminated = False

    def poll(self) -> int | None:
        return self.returncode

    def terminate(self) -> None:
        self.terminated = True
        self.returncode = -15

    def wait(self, _timeout: float | None = None) -> int | None:
        return self.returncode

    def kill(self) -> None:
        self.returncode = -9


class Spawner:
    def __init__(self) -> None:
        self.children: list[Child] = []
        self.argv: list[list[str]] = []

    def __call__(self, argv: Any) -> Any:
        self.argv.append(list(argv))
        child = Child()
        self.children.append(child)
        return child


def _part(spawner: Spawner, **kwargs: Any) -> TrayPart:
    settings = kwargs.pop("settings", lambda: (True, "backslash", ""))
    return TrayPart(
        why_not=kwargs.pop("why_not", lambda: None),
        spawn=spawner,
        agents=kwargs.pop("agents", lambda: list(AGENTS)),
        settings=settings,
    )


def test_the_part_starts_the_tray_and_says_which_agents_run() -> None:
    spawner = Spawner()
    part = _part(spawner)
    health = part.tick_part()
    argv = spawner.argv[0]
    assert argv[argv.index("-m") + 1] == service_tray.TRAY_MODULE, "not the CLI: a lean process"
    assert argv[argv.index("--icon") + 1] == "backslash"
    assert argv[argv.index("--status") + 1] == str(service_host.status_path())
    assert argv[argv.index("--service-pid") + 1] == str(os.getpid()), "its parent, passed"
    assert health.state == "ok"
    assert health.extra == {"agents": AGENTS, "agents_running": 5}
    part.tick_part()
    assert len(spawner.children) == 1, "one tray, kept"
    part.stop_part()
    assert spawner.children[0].terminated


def test_an_exit_that_means_no_tray_here_is_never_retried() -> None:
    spawner = Spawner()
    part = _part(spawner)
    part.tick_part()
    spawner.children[0].returncode = service_tray.EXIT_NO_HOST
    health = part.tick_part()
    assert (health.state, health.detail) == ("off", "this session has no place for a tray icon")
    part.tick_part()
    assert len(spawner.children) == 1


def test_a_crashed_tray_is_started_again_up_to_its_limit() -> None:
    spawner = Spawner()
    part = _part(spawner)
    part.tick_part()
    for _ in range(service_tray.TRAY_TRIES - 1):
        spawner.children[-1].returncode = 1
        assert part.tick_part().state == "ok"
    spawner.children[-1].returncode = 1
    health = part.tick_part()
    assert health.state == "error" and "stopped 5 times" in (health.detail or "")
    assert len(spawner.children) == service_tray.TRAY_TRIES
    part.tick_part()
    assert len(spawner.children) == service_tray.TRAY_TRIES


def test_a_change_in_app_settings_restarts_or_stops_the_tray() -> None:
    """Within a tick (30 s): a new icon, a child that draws it; switched off, none."""
    spawner = Spawner()
    now = {"settings": (True, "backslash", "")}
    off: dict[str, str | None] = {"why": None}
    part = _part(spawner, settings=lambda: now["settings"], why_not=lambda: off["why"])
    part.tick_part()
    now["settings"] = (True, "mark", "")
    part.tick_part()
    assert spawner.children[0].terminated and len(spawner.children) == 2
    now["settings"], off["why"] = (False, "mark", ""), "off in App settings"
    health = part.tick_part()
    assert spawner.children[1].terminated
    assert (health.state, health.detail) == ("off", "off in App settings")
    assert len(spawner.children) == 2


def test_no_tray_wanted_starts_no_process() -> None:
    spawner = Spawner()
    part = _part(spawner, why_not=lambda: "no desktop session here")
    assert part.tick_part().state == "off"
    assert spawner.children == []


def test_the_fleet_unreadable_is_a_tray_with_no_names() -> None:
    def broken() -> list[str]:
        raise OSError("store busy")

    spawner = Spawner()
    health = _part(spawner, agents=broken).tick_part()
    assert health.state == "ok" and health.extra == {"agents": [], "agents_running": 0}


@pytest.mark.parametrize(
    ("platform", "env", "remote", "module", "why"),
    [
        ("darwin", {}, False, True, None),
        (
            "darwin",
            {},
            True,
            True,
            "the menu bar is in the Mac's login session; this service was started over SSH",
        ),
        ("darwin", {}, False, False, "the tray needs rlx's tray extra, which is not installed"),
        (
            "linux",
            {"WAYLAND_DISPLAY": "wayland-0", "XDG_RUNTIME_DIR": "/run/user/1"},
            False,
            True,
            None,
        ),
        ("linux", {"XDG_RUNTIME_DIR": "/run/user/1"}, False, True, "no desktop session here"),
        ("linux", {"DISPLAY": ":0"}, False, True, "no session bus here"),
        (
            "linux",
            {"DISPLAY": ":0", "WSL_DISTRO_NAME": "Ubuntu"},
            False,
            True,
            "no tray on this OS in this release",
        ),
        ("win32", {}, False, True, "no tray on this OS in this release"),
    ],
    ids=[
        "macos",
        "macos-ssh",
        "macos-no-extra",
        "linux",
        "linux-headless",
        "linux-no-bus",
        "wsl",
        "windows",
    ],
)
def test_why_the_service_shows_no_tray(
    platform: str, env: dict[str, str], remote: bool, module: bool, why: str | None
) -> None:
    said = why_no_tray(
        platform=platform,
        enabled=True,
        env=env,
        remote_session=lambda: remote,
        has_module=lambda _name: module,
    )
    assert said == why


def test_switched_off_it_is_off_before_anything_else_is_asked() -> None:
    def never() -> bool:
        raise AssertionError("not asked")

    assert (
        why_no_tray(platform="darwin", enabled=False, remote_session=never) == "off in App settings"
    )


def test_the_tray_is_the_last_part_and_the_usage_part_counts_the_room(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from aisquare.models import ClaudeAccount, HeadroomSummary
    from aisquare.services import claude_accounts as accounts_service

    assert isinstance(service_host.default_parts()[-1], TrayPart)
    accounts = [
        ClaudeAccount(slot=1, config_dir=Path("/a/1")),
        ClaudeAccount(slot=2, config_dir=Path("/a/2")),
        ClaudeAccount(slot=3, config_dir=Path("/a/3"), disabled=True),
    ]
    monkeypatch.setattr(accounts_service, "list_accounts", lambda: accounts)

    def read(*, agent: str) -> HeadroomSummary:
        return HeadroomSummary(level="good", usable=2, left_by_slot={1: 40.0, 2: 0.0})

    part = service_host.UsagePart(read=read, out_of_reach=lambda: False)
    part.start_part()
    health = part.tick_part()
    assert (health.extra["room"], health.extra["accounts"]) == (1, 2)


# --- Linux: the StatusNotifierItem and its menu ---------------------------------------------------


def _call(
    path: str, interface: str, member: str, signature: str | None = None, body: tuple[Any, ...] = ()
) -> Any:
    msg = new_method_call(
        DBusAddress(path, "org.kde.StatusNotifierItem-1-1", interface), member, signature, body
    )
    msg.header.serial = 5  # as the bus numbers a call: what the reply answers
    msg.header.fields[HeaderFields.sender] = ":1.42"  # the host: the reply's destination
    return msg


class Recorder(Tray):
    """A tray whose actions are recorded."""

    def __init__(self, given: dict[str, Any]) -> None:
        super().__init__(platform="linux", read_status=lambda: self.given)
        self.given = given
        self.acted: list[str] = []

    def act(self, action: str) -> None:
        self.acted.append(action)


def _replies(item: tray_linux.Item, *args: Any) -> list[Any]:
    out = item.handle(_call(*args))
    for message in out:
        message.serialise(serial=7)  # the body is what its signature says, or this raises
    return out


def _labels(layout: Any) -> list[tuple[int, str]]:
    return [(child[0], child[1].get("label", ("s", "-"))[1]) for _, child in layout[2]]


def test_the_item_shows_rlx_s_glyph_and_names_its_menu() -> None:
    item = tray_linux.Item(Recorder(_status()))
    (reply,) = _replies(
        item,
        tray_linux.ITEM_PATH,
        tray_linux.PROPERTIES,
        "GetAll",
        "s",
        (tray_linux.ITEM_INTERFACE,),
    )
    (props,) = reply.body
    sizes = [(w, h) for w, h, _ in props["IconPixmap"][1]]
    assert sizes == [(s, s) for s in tray_linux.ICON_SIZES]
    assert props["Menu"] == ("o", tray_linux.MENU_PATH)
    assert props["ItemIsMenu"] == ("b", False), "a click is Activate: it opens rlx"
    assert props["Id"] == ("s", "rlx") and props["Status"] == ("s", "Active")
    (one,) = _replies(
        item,
        tray_linux.ITEM_PATH,
        tray_linux.PROPERTIES,
        "Get",
        "ss",
        (tray_linux.ITEM_INTERFACE, "Title"),
    )
    assert one.body == (("s", "rlx"),)


def test_activating_the_item_opens_rlx() -> None:
    """The host's activation: Linux's double-click (or click), the brief's double-click."""
    recorder = Recorder(_status())
    item = tray_linux.Item(recorder)
    (reply,) = _replies(
        item, tray_linux.ITEM_PATH, tray_linux.ITEM_INTERFACE, "Activate", "ii", (10, 10)
    )
    assert reply.header.message_type is MessageType.method_return
    assert recorder.acted == [OPEN]


def test_the_menu_is_laid_out_and_its_clicks_act() -> None:
    recorder = Recorder(_status())
    item = tray_linux.Item(recorder)
    (reply,) = _replies(
        item, tray_linux.MENU_PATH, tray_linux.MENU_INTERFACE, "GetLayout", "iias", (0, -1, [])
    )
    revision, layout = reply.body
    assert revision == item.revision
    rows = _labels(layout)
    assert [label for _, label in rows] == [
        "Open rlx",
        "Restart rlx",
        "Quit rlx…",
        "-",
        "2 agents running",
        "Accounts with room: 2 of 3",
    ]
    restart = rows[1][0]
    _replies(
        item,
        tray_linux.MENU_PATH,
        tray_linux.MENU_INTERFACE,
        "Event",
        "isvu",
        (restart, "clicked", ("s", ""), 0),
    )
    _replies(
        item,
        tray_linux.MENU_PATH,
        tray_linux.MENU_INTERFACE,
        "Event",
        "isvu",
        (restart, "hovered", ("s", ""), 0),
    )
    quit_id = rows[2][0]
    (sub,) = _replies(
        item,
        tray_linux.MENU_PATH,
        tray_linux.MENU_INTERFACE,
        "GetLayout",
        "iias",
        (quit_id, -1, []),
    )
    children = _labels(sub.body[1])
    assert children[1][1] == "Quit rlx now"
    _replies(
        item,
        tray_linux.MENU_PATH,
        tray_linux.MENU_INTERFACE,
        "Event",
        "isvu",
        (children[1][0], "clicked", ("s", ""), 0),
    )
    assert recorder.acted == [RESTART, QUIT_NOW]
    status_line = rows[4][0]
    (prop,) = _replies(
        item,
        tray_linux.MENU_PATH,
        tray_linux.MENU_INTERFACE,
        "GetProperty",
        "is",
        (status_line, "enabled"),
    )
    assert prop.body == (("b", False),), "a status line does nothing"


def test_the_menu_is_rebuilt_as_it_is_about_to_show() -> None:
    """Built when it opens, from the service's status: nothing polls while it is closed."""
    recorder = Recorder(_status())
    item = tray_linux.Item(recorder)
    (same,) = _replies(
        item, tray_linux.MENU_PATH, tray_linux.MENU_INTERFACE, "AboutToShow", "i", (0,)
    )
    assert same.body == (False,)
    recorder.given = _status(AGENTS)
    reply, signal = _replies(
        item, tray_linux.MENU_PATH, tray_linux.MENU_INTERFACE, "AboutToShow", "i", (0,)
    )
    assert reply.body == (True,)
    assert signal.header.message_type is MessageType.signal and signal.body == (item.revision, 0)


def test_a_call_it_does_not_know_is_an_error_never_the_end_of_the_tray() -> None:
    item = tray_linux.Item(Recorder(_status()))
    (unknown,) = _replies(item, tray_linux.MENU_PATH, tray_linux.MENU_INTERFACE, "Frobnicate")
    assert unknown.header.message_type is MessageType.error
    (elsewhere,) = _replies(item, "/Elsewhere", tray_linux.ITEM_INTERFACE, "Activate", "ii", (0, 0))
    assert elsewhere.header.message_type is MessageType.error
    (bad,) = _replies(item, tray_linux.MENU_PATH, tray_linux.MENU_INTERFACE, "GetLayout", "i", (0,))
    assert bad.header.message_type is MessageType.error


# --- App settings ▸ User preferences ▸ Tray -------------------------------------------------------


def test_the_terminal_choices_keep_the_one_the_file_names() -> None:
    assert terminal_options("", "darwin")[:2] == [
        ("Your default terminal", ""),
        ("Terminal", "Terminal"),
    ]
    assert terminal_options("tilix", "linux")[-1] == ("tilix", "tilix")


def test_the_tray_switches_are_kept_as_they_change_and_ctrl_z_takes_them_back() -> None:
    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        app.post_message(AppSettingsSelected())
        await settle(app)
        app.query_one("#app-settings-tabs", TabbedContent).active = PREFERENCES_TAB
        await settle(app)
        toggle = app.query_one(TrayToggle)
        switch = toggle.query_one("#tray-enabled", Switch)
        switch.focus()
        switch.toggle()
        await settle(app)
        seen: dict[str, object] = {"off": load_config().tray.enabled}
        toggle.query_one("#tray-icon", Select).value = "template"
        await settle(app)
        seen["icon"] = load_config().tray.icon
        switch.focus()
        await pilot.press("ctrl+z")
        await settle(app)
        seen["undone"] = load_config().tray.icon
        toggle.reread(AppConfig())  # Reset all's re-read
        await settle(app)
        seen["reset"] = (switch.value, toggle.query_one("#tray-icon", Select).value)
        seen["toasts"] = [n.message for n in app._notifications]
        return seen

    seen = drive(go, notifications=True, size=(100, 40))
    assert seen["off"] is False
    assert seen["icon"] == "template"
    assert seen["undone"] == "backslash", "ctrl+z put the icon back"
    assert seen["reset"] == (True, "backslash")
    toasts = seen["toasts"]
    assert isinstance(toasts, list) and any("the tray's icon" in str(t) for t in toasts), toasts


def test_the_part_follows_the_file(tmp_path: Path) -> None:
    config = load_config()
    config.tray = TraySettings(enabled=False, icon="mark", terminal="foot")
    save_config(config)
    assert service_tray.tray_settings() == (False, "mark", "foot")
    assert why_no_tray(platform="linux", env={"DISPLAY": ":0"}) == "off in App settings"


# --- macOS: its half defines, and draws its image, with no status item ----------------------------


def test_the_macos_half_defines_and_draws_the_menu_bar_image() -> None:
    """Imported where pyobjc is (the dev extra on macOS): a method pyobjc took for a selector
    failed only at import. No status item is made: only the image a backend would show."""
    pytest.importorskip("objc")
    from aisquare.services import tray_macos

    template = tray_macos.menu_bar_image("template")
    assert template.isTemplate() and tuple(template.size()) == (18.0, 18.0)
    assert len(template.representations()) == 2, "1x and 2x"
    mark = tray_macos.menu_bar_image("mark")
    assert not mark.isTemplate() and mark.size().width > 18


def test_the_macos_menu_is_built_from_the_rows_with_the_quit_alert_asked_first(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The delegate's menu, as ``menuNeedsUpdate:`` builds it as the menu opens (an ``NSMenu``,
    not shown); a status line does nothing, and Quit asks before it acts."""
    pytest.importorskip("objc")
    from AppKit import NSMenu

    from aisquare.services import tray_macos

    shown = Recorder(_status(AGENTS[:1]))
    shown.platform = "darwin"
    delegate = tray_macos.delegate_for(shown)
    menu = NSMenu.new()
    delegate.menuNeedsUpdate_(menu)
    titles = [str(menu.itemAtIndex_(i).title()) for i in range(menu.numberOfItems())]
    assert titles == [
        "Open rlx",
        "Restart rlx",
        "Quit rlx…",
        "",
        "1 agent running",
        "Accounts with room: 2 of 3",
    ]
    assert not menu.itemAtIndex_(4).isEnabled() and menu.itemAtIndex_(0).isEnabled()
    answers = iter([False, True])
    asked: list[str] = []

    def ask(tray_shown: Tray) -> bool:
        asked.append(tray_shown.question()[0])
        return next(answers)

    monkeypatch.setattr(tray_macos, "asked", ask)
    delegate.clicked_(menu.itemAtIndex_(2))  # Quit, cancelled
    delegate.clicked_(menu.itemAtIndex_(2))  # Quit, confirmed
    delegate.clicked_(menu.itemAtIndex_(0))  # Open
    assert asked == ["Quit rlx?", "Quit rlx?"]
    assert shown.acted == [QUIT_NOW, OPEN]
