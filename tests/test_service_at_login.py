"""The rlx service at login (S2, #33; design #1875, decisions #1877 and #1879): the LaunchAgent
and the systemd user unit, word for word; adding and taking them away through a stand-in
launchctl/systemctl, every call asserted; a removal in the OS's own settings that nothing but an
explicit install undoes; the two ways to turn it off, App settings' switch and ``rlx service
uninstall``; and who registers by default: the default home's init and installers, never a
named home's, and no init under CI.

No test runs the real ``launchctl``, ``systemctl`` or ``loginctl`` or writes the real
``~/Library/LaunchAgents``: ``tests/conftest.py::no_real_login_item`` stands in for them in
every test, on macOS's terms (uid 501) unless a test says Linux.
"""

from __future__ import annotations

import json
import os
import plistlib
import sys
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest
from textual.pilot import Pilot
from textual.widgets import Static, Switch, TabbedContent
from typer.testing import CliRunner, Result

from aisquare.cli.app import app
from aisquare.cli.ui.views import start_at_login
from aisquare.cli.ui.views.app_settings import PREFERENCES_TAB
from aisquare.core import paths
from aisquare.core.config import load_config
from aisquare.services import install_route, lifecycle, service_host, service_login
from tests.conftest import LoginOs
from tests.test_ui_app_settings import AppHost, drive

REAL_ENTRY_POINT = service_login.entry_point
"""The real one, which the suite's stub replaces in every test."""
REAL_NAMED_HOME = service_login._named_home
"""The real one, which :func:`default_home` replaces in every test here."""
DOMAIN = "gui/501"
TARGET = f"{DOMAIN}/{service_login.LABEL}"
NOT_LOADED = "Could not find service in domain for user gui: 501"
DISABLED = (
    "disabled services = {\n"
    '\t"com.openssh.ssh-agent" => enabled\n'
    f'\t"{service_login.LABEL}" => disabled\n'
    "}"
)


@pytest.fixture(autouse=True)
def default_home(monkeypatch: pytest.MonkeyPatch) -> None:
    """The default home (no variable names it, as for a person), and no CI: what registers by
    default. The suite's own home is a named one, which never does; the tests of that say so."""
    monkeypatch.setattr(service_login, "_named_home", lambda: None)
    monkeypatch.delenv("CI", raising=False)


@pytest.fixture
def login_os(no_real_login_item: LoginOs) -> LoginOs:
    return no_real_login_item


@pytest.fixture
def linux(monkeypatch: pytest.MonkeyPatch) -> None:
    """Linux with systemd as its init, as on a desktop, a server or a WSL with it turned on."""
    monkeypatch.setattr(service_login, "_platform", lambda: "linux")
    monkeypatch.setattr(service_login, "_systemd_runs", lambda: True)


def _rlx(*args: str) -> Result:
    return CliRunner().invoke(app, list(args))


def _plist() -> Path:
    return service_login.item_path("launchd")


def _unit() -> Path:
    return service_login.item_path("systemd")


def _init(project: Path, *more: str) -> Result:
    project.mkdir(exist_ok=True)
    return _rlx("init", "--local", "--yes", "--no-onboard", *more, str(project))


def _choice() -> bool | None:
    return load_config().service.start_at_login


# --- what the OS is given, word for word ------------------------------------------------------


def test_the_launch_agent_loads_at_an_aqua_login_and_is_kept_alive_only_on_a_crash() -> None:
    argv = ["/Users/u/.local/bin/rlx", "service", "run", "--at-login"]
    env = {"PATH": "/opt/homebrew/bin:/usr/bin:/bin"}
    stderr = Path("/Users/u/Library/Logs/rlx/studio.aisquare.rlx.service.log")

    body = service_login.plist_bytes(argv, env, stderr=stderr)

    assert body.decode() == (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" '
        '"http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
        '<plist version="1.0">\n'
        "<dict>\n"
        "\t<key>AbandonProcessGroup</key>\n"
        "\t<true/>\n"
        "\t<key>EnvironmentVariables</key>\n"
        "\t<dict>\n"
        "\t\t<key>PATH</key>\n"
        "\t\t<string>/opt/homebrew/bin:/usr/bin:/bin</string>\n"
        "\t</dict>\n"
        "\t<key>KeepAlive</key>\n"
        "\t<dict>\n"
        "\t\t<key>SuccessfulExit</key>\n"
        "\t\t<false/>\n"
        "\t</dict>\n"
        "\t<key>Label</key>\n"
        "\t<string>studio.aisquare.rlx.service</string>\n"
        "\t<key>LimitLoadToSessionType</key>\n"
        "\t<string>Aqua</string>\n"
        "\t<key>ProcessType</key>\n"
        "\t<string>Background</string>\n"
        "\t<key>ProgramArguments</key>\n"
        "\t<array>\n"
        "\t\t<string>/Users/u/.local/bin/rlx</string>\n"
        "\t\t<string>service</string>\n"
        "\t\t<string>run</string>\n"
        "\t\t<string>--at-login</string>\n"
        "\t</array>\n"
        "\t<key>RunAtLoad</key>\n"
        "\t<true/>\n"
        "\t<key>StandardErrorPath</key>\n"
        f"\t<string>{stderr}</string>\n"
        "</dict>\n"
        "</plist>\n"
    )


def test_the_user_unit_starts_with_the_users_manager_and_again_only_on_a_failure() -> None:
    argv = ["/home/u/.local/bin/rlx", "service", "run", "--at-login"]
    env = {"PATH": "/home/u/.local/bin:/usr/bin:/bin"}

    assert service_login.unit_text(argv, env) == (
        "[Unit]\n"
        "Description=rlx service: Remote, the usage reads and the fleet's nudges with no rlx "
        "window open\n"
        "Documentation=https://github.com/AISquare-Studio/rlx/blob/main/docs/service.md\n"
        "\n"
        "[Service]\n"
        "Type=simple\n"
        "ExecStart=/home/u/.local/bin/rlx service run --at-login\n"
        "Environment=PATH=/home/u/.local/bin:/usr/bin:/bin\n"
        "Restart=on-failure\n"
        "RestartSec=5\n"
        "KillMode=process\n"
        "\n"
        "[Install]\n"
        "WantedBy=default.target\n"
    )


def test_the_unit_quotes_what_systemd_would_split_or_expand() -> None:
    text = service_login.unit_text(
        ["/home/u/my tools/rlx", "service", "run", "--at-login"],
        {"PATH": "/home/u/100%/bin:$HOME/bin"},
    )

    assert 'ExecStart="/home/u/my tools/rlx" service run --at-login\n' in text, text
    # A specifier's % is doubled everywhere; $ only on the command line, where it expands.
    assert "Environment=PATH=/home/u/100%%/bin:$HOME/bin\n" in text, text


def test_the_item_carries_path_and_the_homes_variable_and_nothing_else() -> None:
    env = service_login.service_env(
        {
            "PATH": "/opt/homebrew/bin:/usr/bin",
            "AISQUARE_HOME": "/Users/u/scratch-home",
            "RLX_HOME": "",
            "TMUX_TMPDIR": "/tmp/fleet",
            "CLAUDE_CONFIG_DIR": "/Users/u/.claude-accounts/2",
            "HOME": "/Users/u",
            "AISQUARE_AGENT_NAME": "coder-1",
        }
    )

    assert env == {"PATH": "/opt/homebrew/bin:/usr/bin", "AISQUARE_HOME": "/Users/u/scratch-home"}


def _route(kind: str, tmp_path: Path, *shims: Path) -> install_route.InstallRoute:
    found = install_route.Facts(
        prefix=tmp_path / "venv",
        base_prefix=tmp_path / "python",
        executable=tmp_path / "venv" / "bin" / "python",
        platform=sys.platform,
        python_version="3.13",
    )
    receipt = install_route.UvReceipt(entrypoints=shims) if shims else None
    return install_route.InstallRoute(kind, found, receipt=receipt, source=str(tmp_path / "src"))


def test_the_os_runs_the_uv_tools_rlx_and_never_a_checkouts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(service_login, "entry_point", REAL_ENTRY_POINT)
    local = tmp_path / ".local" / "bin"
    tool = _route(install_route.UV_TOOL, tmp_path, local / "aisquare", local / "rlx")
    monkeypatch.setattr(install_route, "detect", lambda: tool)
    assert service_login.entry_point() == local / "rlx"

    for kind in (install_route.EDITABLE, install_route.VENV, install_route.UVX):
        route = _route(kind, tmp_path)
        monkeypatch.setattr(install_route, "detect", lambda route=route: route)
        with pytest.raises(service_login.LoginError, match="which a login item should not run"):
            service_login.entry_point()


def test_from_a_checkout_install_refuses_and_names_it(
    tmp_path: Path, login_os: LoginOs, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(service_login, "entry_point", REAL_ENTRY_POINT)
    route = _route(install_route.EDITABLE, tmp_path)
    monkeypatch.setattr(install_route, "detect", lambda: route)

    result = _rlx("service", "install")

    assert result.exit_code != 0
    assert "an editable install of" in result.output, result.output
    assert not _plist().exists() and login_os.changes() == [] and _choice() is None


# --- macOS: adding it and taking it away ----------------------------------------------------------


def test_install_writes_the_agent_takes_it_off_launchds_disabled_list_and_loads_it(
    login_os: LoginOs,
) -> None:
    login_os.reply("launchctl", "print", TARGET, code=113, said=NOT_LOADED)

    result = _rlx("service", "install")

    assert result.exit_code == 0, result.output
    assert _plist().read_bytes() == service_login.item_body("launchd")
    assert login_os.calls == [
        ("launchctl", "print", TARGET),
        ("launchctl", "enable", TARGET),
        ("launchctl", "bootstrap", DOMAIN, str(_plist())),
        ("launchctl", "print-disabled", DOMAIN),
    ]
    assert "starts at login: yes" in result.output, result.output
    assert _choice() is True


def test_install_over_a_loaded_agent_starts_it_and_leaves_a_running_one(login_os: LoginOs) -> None:
    result = _rlx("service", "install")

    assert result.exit_code == 0, result.output
    assert login_os.changes() == [
        ("launchctl", "enable", TARGET),
        ("launchctl", "kickstart", TARGET),
    ]
    assert _plist().is_file()


def test_install_away_from_the_mac_keeps_the_agent_for_the_next_login(login_os: LoginOs) -> None:
    login_os.reply("launchctl", "print", TARGET, code=113, said=NOT_LOADED)
    login_os.reply(
        "launchctl", "bootstrap", code=125, said="Bootstrap failed: 125: Domain does not support"
    )

    result = _rlx("service", "install")

    assert result.exit_code == 0, result.output
    assert "starts at login: yes" in result.output, result.output
    assert "it starts at your next login at the Mac" in result.output, result.output
    assert _plist().is_file()


def test_uninstall_boots_the_agent_out_and_takes_it_away(login_os: LoginOs) -> None:
    assert _rlx("service", "install").exit_code == 0
    login_os.calls.clear()

    result = _rlx("--json", "service", "uninstall")

    assert result.exit_code == 0, result.output
    assert not _plist().exists()
    assert login_os.changes() == [("launchctl", "bootout", TARGET)]
    data = json.loads(result.stdout)
    assert data["at_login"]["on"] is False
    assert data["at_login"]["chosen"] == "off"
    assert data["at_login"]["why_off"] == service_login.OFF_IN_RLX
    assert _choice() is False


def test_status_says_whether_it_starts_at_login(login_os: LoginOs) -> None:
    before = _rlx("--json", "service", "status")
    assert before.exit_code == 3, before.output
    assert json.loads(before.stdout)["at_login"] == {
        "on": False,
        "how": "launchd",
        "item": str(_plist()),
        "chosen": None,
        "why_off": service_login.NEVER_ON,
    }

    _rlx("service", "install")
    after = _rlx("service", "status")

    assert after.exit_code == 3, after.output
    assert "the rlx service is not running" in after.output
    assert "starts at login: yes (" in after.output, after.output


def test_an_install_macos_keeps_off_in_login_items_says_where_to_turn_it_on(
    login_os: LoginOs,
) -> None:
    login_os.reply("launchctl", "print-disabled", DOMAIN, said=DISABLED)

    result = _rlx("service", "install")

    assert result.exit_code == 0, result.output
    assert f"starts at login: no — {service_login.OFF_IN_LOGIN_ITEMS}" in result.output
    assert service_login.TURN_IT_ON_THERE in result.output, result.output


# --- Linux (and WSL): the user unit ---------------------------------------------------------------


@pytest.mark.usefixtures("linux")
def test_install_on_linux_writes_the_unit_and_enables_it_now(login_os: LoginOs) -> None:
    result = _rlx("service", "install")

    assert result.exit_code == 0, result.output
    assert _unit().read_bytes() == service_login.item_body("systemd")
    assert login_os.calls == [
        ("systemctl", "--user", "daemon-reload"),
        ("systemctl", "--user", "enable", "--now", "rlx.service"),
        ("systemctl", "--user", "is-enabled", "rlx.service"),
    ]
    assert not any(call[0] == "loginctl" for call in login_os.calls)  # never unasked


@pytest.mark.usefixtures("linux")
def test_uninstall_on_linux_disables_and_stops_the_unit(login_os: LoginOs) -> None:
    _rlx("service", "install")
    login_os.calls.clear()

    result = _rlx("service", "uninstall")

    assert result.exit_code == 0, result.output
    assert not _unit().exists()
    assert login_os.changes() == [
        ("systemctl", "--user", "disable", "--now", "rlx.service"),
        ("systemctl", "--user", "daemon-reload"),
    ]


@pytest.mark.usefixtures("linux")
def test_lingering_is_turned_on_only_when_asked_for(login_os: LoginOs) -> None:
    _rlx("service", "install")
    assert not any(call[:2] == ("loginctl", "enable-linger") for call in login_os.calls)

    result = _rlx("service", "install", "--linger")

    assert result.exit_code == 0, result.output
    assert [call[:2] for call in login_os.calls if call[0] == "loginctl"] == [
        ("loginctl", "enable-linger")
    ]
    assert "it runs with nobody logged in too" in result.output, result.output


@pytest.mark.parametrize(
    ("in_wsl", "why"),
    [(False, service_login.NO_SYSTEMD), (True, service_login.NO_SYSTEMD_IN_WSL)],
    ids=["linux", "wsl"],
)
def test_without_systemd_install_says_why_and_adds_nothing(
    login_os: LoginOs, monkeypatch: pytest.MonkeyPatch, in_wsl: bool, why: str
) -> None:
    monkeypatch.setattr(service_login, "_platform", lambda: "linux")
    monkeypatch.setattr(service_login, "_systemd_runs", lambda: False)
    monkeypatch.setattr(service_login, "_in_wsl", lambda: in_wsl)

    result = _rlx("service", "install")

    assert result.exit_code != 0
    assert why in result.output, result.output
    assert login_os.calls == [] and _choice() is None


# --- native Windows: a clean no-op, nothing recorded (#1879) --------------------------------------


def test_native_windows_does_nothing_and_records_no_choice(
    tmp_path: Path, login_os: LoginOs, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(service_login, "_platform", lambda: "win32")

    by_installer = _rlx("service", "install", "--if-undecided")
    explicit = _rlx("service", "install")
    off = _rlx("service", "uninstall")
    init = _init(tmp_path / "project")

    assert by_installer.exit_code == 0, by_installer.output
    assert service_login.NOT_HERE in by_installer.output
    assert explicit.exit_code != 0 and service_login.NOT_HERE in explicit.output
    assert off.exit_code == 0 and service_login.NOT_HERE in off.output
    assert init.exit_code == 0 and service_login.NOT_HERE in init.output, init.output
    assert login_os.calls == [] and _choice() is None


# --- who registers by default ---------------------------------------------------------------------


def test_the_first_init_of_the_default_home_adds_it(tmp_path: Path, login_os: LoginOs) -> None:
    first = _init(tmp_path / "project")

    assert first.exit_code == 0, first.output
    assert _plist().is_file()
    assert "the rlx service starts at login" in first.output, first.output
    assert _choice() is True


def test_no_service_is_kept_so_no_later_init_or_installer_adds_it(
    tmp_path: Path, login_os: LoginOs
) -> None:
    """The manager's ruling on #33 (tester m4): --no-service records the no. A later plain init,
    the installer's step or another --no-service adds nothing and calls no OS tool; only an
    explicit install turns it on."""
    left_off = _init(tmp_path / "left-off", "--no-service")

    assert left_off.exit_code == 0, left_off.output
    assert "--no-service, kept for later runs" in left_off.output, left_off.output
    assert _choice() is False

    later = _init(tmp_path / "project")
    by_installer = _rlx("service", "install", "--if-undecided")
    again = _init(tmp_path / "project", "--no-service")

    for result in (later, by_installer, again):
        assert result.exit_code == 0, result.output
    assert f"does not start at login: {service_login.OFF_IN_RLX}" in later.output, later.output
    assert not _plist().exists() and login_os.calls == []
    assert _choice() is False

    assert _rlx("service", "install").exit_code == 0
    assert _plist().is_file() and _choice() is True


def test_no_service_leaves_a_choice_already_made(tmp_path: Path, login_os: LoginOs) -> None:
    """A login item already on stays on: --no-service is about not adding one; the switch and
    `rlx service uninstall` turn one off."""
    _init(tmp_path / "project")
    login_os.calls.clear()

    again = _init(tmp_path / "project", "--no-service")

    assert again.exit_code == 0, again.output
    assert _plist().is_file() and _choice() is True
    assert login_os.changes() == []
    assert "the rlx service starts at login" in again.output, again.output


def test_a_named_homes_init_never_adds_one_only_an_explicit_install_does(
    tmp_path: Path, login_os: LoginOs, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A scratch home's ``rlx init`` on a real Mac can never add a login item (#1877)."""
    home = paths.aisquare_home()
    monkeypatch.setattr(service_login, "_named_home", lambda: home)

    init = _init(tmp_path / "project")
    by_installer = _rlx("service", "install", "--if-undecided")

    assert init.exit_code == 0 and by_installer.exit_code == 0, init.output + by_installer.output
    assert "this home is named by AISQUARE_HOME" in init.output, init.output
    assert "this home is named by AISQUARE_HOME" in by_installer.output
    assert login_os.calls == [] and _choice() is None

    result = _rlx("service", "install")

    assert result.exit_code == 0, result.output
    tag = service_login.home_tag()
    assert tag is not None and len(tag) == 8
    plist = service_login.launch_agents_dir() / f"{service_login.LABEL}.{tag}.plist"
    agent = plistlib.loads(plist.read_bytes())
    assert agent["Label"] == f"{service_login.LABEL}.{tag}"
    assert agent["EnvironmentVariables"]["AISQUARE_HOME"] == os.environ["AISQUARE_HOME"]
    assert not (service_login.launch_agents_dir() / f"{service_login.LABEL}.plist").exists()


def test_a_named_home_gets_a_unit_of_its_own(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(service_login, "_named_home", lambda: Path("/srv/rlx-home"))

    assert service_login.unit_name() == f"rlx-{service_login.home_tag()}.service"
    assert service_login.unit_name() != "rlx.service"


def test_init_under_ci_adds_none_and_says_so_and_an_installer_still_does(
    tmp_path: Path, login_os: LoginOs, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("CI", "true")

    init = _init(tmp_path / "project")

    assert init.exit_code == 0, init.output
    assert "CI is set, and a CI run adds no login item" in init.output, init.output
    assert not _plist().exists() and _choice() is None

    by_installer = _rlx("service", "install", "--if-undecided")

    assert by_installer.exit_code == 0, by_installer.output
    assert _plist().is_file() and _choice() is True


# --- a removal respected (acceptance 2) -----------------------------------------------------------


def _removed_in_the_os(how: str, login_os: LoginOs) -> None:
    if how == "file":
        _plist().unlink()
    elif how == "login items":
        login_os.reply("launchctl", "print-disabled", DOMAIN, said=DISABLED)
    else:
        login_os.reply("systemctl", "--user", "is-enabled", "rlx.service", code=1, said="disabled")


class _AnInstance:
    pid = 4242
    version = "0.10.2"


@pytest.mark.parametrize(
    ("how", "why"),
    [
        ("file", service_login.REMOVED),
        ("login items", service_login.OFF_IN_LOGIN_ITEMS),
        ("systemctl", service_login.UNIT_DISABLED),
    ],
    ids=["its-file-deleted", "off-in-login-items", "unit-disabled"],
)
def test_a_removal_in_the_os_is_respected_until_an_explicit_install(
    tmp_path: Path, login_os: LoginOs, monkeypatch: pytest.MonkeyPatch, how: str, why: str
) -> None:
    if how == "systemctl":
        monkeypatch.setattr(service_login, "_platform", lambda: "linux")
        monkeypatch.setattr(service_login, "_systemd_runs", lambda: True)
    assert _init(tmp_path / "project").exit_code == 0
    item = service_login.item_path(service_login.login_how())
    written = item.read_bytes()
    _removed_in_the_os(how, login_os)
    login_os.calls.clear()

    # The next starts of rlx: an init, the installer's step, and the service started by hand.
    again = _init(tmp_path / "project")
    by_installer = _rlx("service", "install", "--if-undecided")
    monkeypatch.setattr(service_host, "running_service", lambda **_: None)
    monkeypatch.setattr(service_host, "keychain_out_of_reach", lambda: False)
    monkeypatch.setattr(service_host, "start_service", lambda **_: _AnInstance())
    started = _rlx("service", "start")

    assert again.exit_code == 0 and by_installer.exit_code == 0 and started.exit_code == 0
    assert login_os.changes() == [], "nothing re-added it"
    assert (item.read_bytes() == written) if item.exists() else how == "file"
    assert f"does not start at login: {why}" in again.output, again.output
    assert service_login.at_login_state().why_off == why
    assert _choice() is True

    result = _rlx("service", "install")

    assert result.exit_code == 0, result.output
    assert item.read_bytes() == written
    changed = login_os.changes()
    if how == "systemctl":
        assert ("systemctl", "--user", "enable", "--now", "rlx.service") in changed
    else:
        assert ("launchctl", "enable", TARGET) in changed


def test_an_init_after_turning_it_off_leaves_it_off(tmp_path: Path, login_os: LoginOs) -> None:
    _init(tmp_path / "project")
    _rlx("service", "uninstall")
    login_os.calls.clear()

    again = _init(tmp_path / "project")

    assert again.exit_code == 0, again.output
    assert not _plist().exists() and login_os.changes() == []
    assert f"does not start at login: {service_login.OFF_IN_RLX}" in again.output


def test_every_other_path_refreshes_an_item_that_is_ours_and_makes_none(
    tmp_path: Path, login_os: LoginOs, monkeypatch: pytest.MonkeyPatch
) -> None:
    _rlx("service", "install")
    new_path = os.pathsep.join((str(tmp_path / "new-bin"), "/usr/bin"))
    monkeypatch.setenv("PATH", new_path)

    again = _init(tmp_path / "project")

    assert again.exit_code == 0, again.output
    refreshed = plistlib.loads(_plist().read_bytes())
    assert refreshed["EnvironmentVariables"]["PATH"] == new_path

    # Not ours (another label at our file's name): left as it is.
    theirs = plistlib.dumps({"Label": "com.example.other", "ProgramArguments": ["/bin/true"]})
    _plist().write_bytes(theirs)
    assert service_login.refresh_the_item() is False
    assert _plist().read_bytes() == theirs

    # None there: none made.
    _plist().unlink()
    assert service_login.refresh_the_item() is False
    assert not _plist().exists()


def test_rlx_uninstall_takes_the_agent_away_and_forgets_the_choice(
    login_os: LoginOs, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Or the OS would start a program that is gone at every login; and an install after it is
    a new one, on by default again."""
    _rlx("service", "install")
    monkeypatch.setattr(service_host, "stop_service", lambda **_: False)

    lifecycle._stop_the_service_for_good()

    assert not _plist().exists()
    assert ("launchctl", "bootout", TARGET) in login_os.calls
    assert _choice() is None


# --- starting it: through the OS when it is set up ------------------------------------------------


@pytest.mark.skipif(
    sys.platform == "win32", reason="service_host.start_service refuses on native Windows"
)
def test_start_goes_through_the_os_when_the_item_is_set_up(
    login_os: LoginOs, monkeypatch: pytest.MonkeyPatch
) -> None:
    """So the service that holds S1's lock is the one launchd keeps alive, not a copy it does
    not know (``no_real_service`` refuses any detached spawn here)."""
    _rlx("service", "install")
    login_os.calls.clear()
    answers: list[Any] = [None, None]

    def running(**_: Any) -> Any:
        return answers.pop(0) if answers else _AnInstance()

    monkeypatch.setattr(service_host, "running_service", running)

    found = service_host.start_service(timeout=5.0)

    assert found.pid == _AnInstance.pid
    assert ("launchctl", "kickstart", TARGET) in login_os.calls


def test_start_spawns_it_itself_when_no_item_is_set_up_or_it_is_turned_off(
    login_os: LoginOs,
) -> None:
    assert service_login.start_through_the_os() is False  # none there
    _rlx("service", "install")
    login_os.reply("launchctl", "print-disabled", DOMAIN, said=DISABLED)
    login_os.calls.clear()

    assert service_login.start_through_the_os() is False
    assert not any(call[:2] == ("launchctl", "kickstart") for call in login_os.calls)


# --- the login item's own start -------------------------------------------------------------------


@pytest.mark.parametrize(
    ("flags", "code"), [((), 1), (("--at-login",), 0)], ids=["by-hand", "at-login"]
)
def test_a_refusal_to_start_at_login_is_no_crash_for_the_os_to_restart(
    monkeypatch: pytest.MonkeyPatch, flags: tuple[str, ...], code: int
) -> None:
    def refuse(**_: Any) -> None:
        raise service_host.ServiceError("the rlx service already runs on this home (pid 77)")

    monkeypatch.setattr(service_host, "log_to_file", lambda: None)
    monkeypatch.setattr(service_host, "run_service", refuse)

    result = _rlx("service", "run", *flags)

    assert result.exit_code == code, result.output


def test_print_disabled_is_read_in_both_of_its_formats() -> None:
    label = service_login.LABEL
    assert service_login._disabled_in(DISABLED, label)
    assert service_login._disabled_in(f'disabled services = {{\n\t"{label}" => true\n}}', label)
    assert not service_login._disabled_in(f'\t"{label}" => enabled\n', label)
    assert not service_login._disabled_in(f'\t"{label}.0a1b2c3d" => disabled\n', label)


# --- App settings ▸ User preferences ▸ Start rlx at login (acceptance 3) --------------------------


def _switch(host: AppHost) -> Switch:
    return host.query_one(f"#{start_at_login.SWITCH}", Switch)


def _about(host: AppHost) -> str:
    return str(host.query_one(f"#{start_at_login.ABOUT}", Static).render())


async def _until(pilot: Pilot[None], holds: Callable[[], bool], what: str) -> None:
    for _ in range(500):
        if holds():
            return
        await pilot.pause(0.02)
    pytest.fail(f"never came, in 10 s: {what}")


async def _on_preferences(pilot: Pilot[None], host: AppHost) -> None:
    host.query_one("#app-settings-tabs", TabbedContent).active = PREFERENCES_TAB
    await _until(pilot, lambda: not _switch(host).disabled, "the switch read")


def test_the_switch_turns_it_on_and_off_and_ctrl_z_takes_it_back(login_os: LoginOs) -> None:
    async def scenario(pilot: Pilot[None], host: AppHost) -> list[str]:
        seen: list[str] = []
        await _on_preferences(pilot, host)
        assert _switch(host).value is False
        seen.append(_about(host))

        _switch(host).value = True
        await _until(pilot, _plist().is_file, "the agent written")
        await _until(pilot, lambda: _about(host) == start_at_login.ON, "the line saying on")

        _switch(host).value = False
        await _until(pilot, lambda: not _plist().exists(), "the agent taken away")
        await _until(pilot, lambda: _about(host).startswith("Off:"), "the line saying off")
        seen.append(_about(host))

        await pilot.press("ctrl+z")
        await _until(pilot, _plist().is_file, "the agent back")
        await _until(pilot, lambda: _switch(host).value, "the switch back on")
        seen.extend(message for message, _ in host.notices)
        return seen

    seen = drive(scenario)

    assert seen[0] == f"Off: {service_login.NEVER_ON}."
    assert seen[1] == f"Off: {service_login.OFF_IN_RLX}."
    assert "rlx starts at login · Undo: ctrl+z" in seen
    # Its words, not the constant: bootout stops the service the OS started (#2103 m3).
    assert "rlx no longer starts at login, and the service it started stops · Undo: ctrl+z" in seen
    assert ("launchctl", "bootout", TARGET) in login_os.calls
    assert _choice() is True


def test_the_switch_reads_a_removal_in_login_items_as_off(login_os: LoginOs) -> None:
    _rlx("service", "install")
    login_os.reply("launchctl", "print-disabled", DOMAIN, said=DISABLED)

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[bool, str]:
        await _on_preferences(pilot, host)
        return _switch(host).value, _about(host)

    on, line = drive(scenario)

    assert on is False
    assert line == f"Off: {service_login.OFF_IN_LOGIN_ITEMS}."


def test_a_switch_the_os_refuses_says_why_and_stays_off(
    login_os: LoginOs, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(service_login, "_platform", lambda: "linux")
    monkeypatch.setattr(service_login, "_systemd_runs", lambda: True)
    login_os.reply("systemctl", "--user", "daemon-reload", code=1, said="Failed to connect to bus")

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[bool, str]:
        await _on_preferences(pilot, host)
        _switch(host).value = True
        page: Any = host.query_one("#app-settings")
        await _until(pilot, lambda: bool(page.problem(_switch(host))), "the refusal")
        return _switch(host).value, page.problem(_switch(host))

    on, problem = drive(scenario)

    assert on is False
    assert problem == "Not changed: systemd's user manager did not answer: Failed to connect to bus"
    assert _choice() is None


def test_where_it_cannot_start_at_login_the_switch_is_off_and_says_why(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(service_login, "_platform", lambda: "linux")
    monkeypatch.setattr(service_login, "_systemd_runs", lambda: False)
    monkeypatch.setattr(service_login, "_in_wsl", lambda: True)

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[bool, bool, str]:
        host.query_one("#app-settings-tabs", TabbedContent).active = PREFERENCES_TAB
        await _until(pilot, lambda: _about(host) != start_at_login.READING, "the switch read")
        return _switch(host).value, _switch(host).disabled, _about(host)

    on, disabled, line = drive(scenario)

    assert (on, disabled) == (False, True)
    assert line == f"Off: {service_login.NO_SYSTEMD_IN_WSL}."


# --- review of #33 (#2103): m1, m2, and Reset all ---------------------------------------------


@pytest.mark.usefixtures("linux")
@pytest.mark.parametrize("refused", ["daemon-reload", "enable"], ids=["reload", "enable"])
def test_a_unit_systemd_refuses_is_taken_away_and_never_reads_as_on(
    login_os: LoginOs, refused: str
) -> None:
    """m1: a failed install left the unit file, and status and the switch said "yes"."""
    login_os.reply("systemctl", "--user", refused, code=1, said="Failed to connect to bus")

    result = _rlx("service", "install")

    assert result.exit_code != 0
    assert "Failed to connect to bus" in result.output, result.output
    assert not _unit().exists()
    assert _choice() is None
    state = service_login.at_login_state()
    assert state.on is False and state.why_off == service_login.NEVER_ON


def test_a_variable_that_names_the_default_homes_own_path_is_the_default_home(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """m2: the captain's windows are given the home by its variable (``services.fleet``): the
    default home's own path there is the default home, with its one item and label, so an
    uninstall from a plain shell takes away the item it started from."""
    monkeypatch.setattr(service_login, "_named_home", REAL_NAMED_HOME)
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("USERPROFILE", str(tmp_path))
    monkeypatch.delenv("RLX_HOME", raising=False)

    monkeypatch.setenv("AISQUARE_HOME", str(tmp_path / ".rlx"))
    assert service_login.home_tag() is None
    assert service_login.label() == service_login.LABEL
    assert service_login.unit_name() == "rlx.service"

    monkeypatch.setenv("AISQUARE_HOME", str(tmp_path / "scratch-home"))
    assert service_login.home_tag() is not None
    assert service_login.label() != service_login.LABEL


def test_the_default_homes_item_carries_no_home_variable(
    login_os: LoginOs, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Whichever shell wrote it, the default home's item reads the same: PATH alone."""
    monkeypatch.setenv("AISQUARE_HOME", str(paths.aisquare_home()))

    agent = plistlib.loads(service_login.item_body("launchd"))

    assert set(agent["EnvironmentVariables"]) == {"PATH"}


def test_reset_all_keeps_the_choice_and_a_file_with_only_it_holds_nothing_to_reset(
    login_os: LoginOs,
) -> None:
    """The manager's decision on #33: a settings reset must not re-enable a login item the
    person turned off, as the next init would once [service] was forgotten."""
    from aisquare.services import settings as settings_service

    _rlx("service", "install")
    _rlx("service", "uninstall")
    settings_service.set_value("fleet.escape_key", "f11")

    backup = settings_service.reset_settings()

    assert backup is not None and "f11" in backup.read_text(encoding="utf-8")
    assert _choice() is False
    assert load_config().fleet.escape_key != "f11"
    assert settings_service.anything_to_reset() is False
    assert settings_service.reset_settings() is None


# --- tester of #33 (#2110): B1, B2, m2b, and a job loaded from a file deleted since -----------


def _broken_config() -> None:
    path = paths.config_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    before = path.read_text(encoding="utf-8") if path.exists() else ""
    path.write_text(before + "\n[ui]\nthis is = not [toml\n", encoding="utf-8")


def test_an_unreadable_config_touches_no_login_item_and_says_so(
    tmp_path: Path, login_os: LoginOs
) -> None:
    """B1: "cannot read" read as undecided, so the installer's step and init added back an item
    the person had turned off, then died in a traceback. Now nothing is added, refreshed or
    taken away, one line says why, and the installer's step still exits 0."""
    _rlx("service", "install")
    _rlx("service", "uninstall")
    _broken_config()
    login_os.calls.clear()

    by_installer = _rlx("service", "install", "--if-undecided")
    init = _init(tmp_path / "project")
    explicit = _rlx("service", "install")

    assert by_installer.exit_code == 0, by_installer.output
    assert init.exit_code == 0, init.output
    for said in (by_installer.output, init.output):
        assert "login item was left as it is: config.toml cannot be read" in said, said
        assert "Traceback" not in said, said
    assert explicit.exit_code != 0 and "Traceback" not in explicit.output, explicit.output
    assert "config.toml cannot be read" in explicit.output, explicit.output
    assert not _plist().exists()
    assert login_os.calls == []


def test_init_reinit_keeps_the_choice_and_adds_back_no_item(
    tmp_path: Path, login_os: LoginOs
) -> None:
    """B2 and the manager's ruling: --reinit saved a fresh config, [service] with it, and the
    same init then found nothing chosen and added the item back. The choice is carried now."""
    _init(tmp_path / "project")
    _rlx("service", "uninstall")
    login_os.calls.clear()

    again = _rlx("init", "--reinit", "--yes", "--local", "--no-onboard", str(tmp_path / "project"))

    assert again.exit_code == 0, again.output
    assert _choice() is False
    assert not _plist().exists() and login_os.changes() == []
    assert f"does not start at login: {service_login.OFF_IN_RLX}" in again.output


def test_init_reinit_keeps_a_removal_in_the_os(tmp_path: Path, login_os: LoginOs) -> None:
    _init(tmp_path / "project")
    _plist().unlink()
    login_os.calls.clear()

    again = _rlx("init", "--reinit", "--yes", "--local", "--no-onboard", str(tmp_path / "project"))

    assert again.exit_code == 0, again.output
    assert _choice() is True and not _plist().exists() and login_os.changes() == []


def test_uninstall_boots_out_a_job_whose_file_was_deleted_since(login_os: LoginOs) -> None:
    """Loaded from a file deleted since, the job runs until logout unless booted out."""
    _rlx("service", "install")
    _plist().unlink()
    login_os.calls.clear()

    result = _rlx("service", "uninstall")

    assert result.exit_code == 0, result.output
    assert ("launchctl", "bootout", TARGET) in login_os.changes()


def test_a_switch_macos_keeps_off_says_where_to_turn_it_on_and_offers_no_undo(
    login_os: LoginOs,
) -> None:
    """m2b: turned on while Login Items keeps it off, it said "rlx starts at login · Undo", and
    that ctrl+z would have taken the item away."""
    login_os.reply("launchctl", "print-disabled", DOMAIN, said=DISABLED)

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[bool, str, list[str]]:
        await _on_preferences(pilot, host)
        _switch(host).value = True
        await _until(pilot, _plist().is_file, "the agent written")
        await _until(
            pilot,
            lambda: any(message == service_login.TURN_IT_ON_THERE for message, _ in host.notices),
            "where to turn it on",
        )
        await pilot.press("ctrl+z")
        await pilot.pause()
        return _switch(host).value, _about(host), [message for message, _ in host.notices]

    on, line, notices = drive(scenario)

    assert on is False
    assert line == f"Off: {service_login.OFF_IN_LOGIN_ITEMS}."
    assert not any("Undo" in notice for notice in notices), notices
    assert ("launchctl", "bootout", TARGET) not in login_os.calls
    assert _plist().is_file()


# --- reviewer of #33, round 2 (#2140) -------------------------------------------------------------


def test_a_move_back_keeps_an_off_choice_and_says_nothing_of_an_item_never_there(
    tmp_path: Path, login_os: LoginOs
) -> None:
    """The MAJOR: a move back made an OFF choice undecided, so the next init of a release with
    the service added the item. An off is kept now, and nothing is said of an item that was
    never there."""
    _init(tmp_path / "project", "--no-service")
    login_os.calls.clear()

    said, gone = lifecycle._no_login_item_for(service_login, "0.10.1")

    assert (said, gone) == (None, False)
    assert _choice() is False
    again = _init(tmp_path / "project")
    assert again.exit_code == 0, again.output
    assert not _plist().exists() and login_os.changes() == [("launchctl", "bootout", TARGET)]


def test_a_move_back_forgets_an_on_choice_with_its_item(login_os: LoginOs) -> None:
    _rlx("service", "install")

    said, gone = lifecycle._no_login_item_for(service_login, "0.10.1")

    assert gone is True and not _plist().exists() and _choice() is None
    assert said == "the rlx service no longer starts at login: 0.10.1 has no rlx service"


def test_init_reinit_over_an_unreadable_config_touches_no_login_item(
    tmp_path: Path, login_os: LoginOs
) -> None:
    """--reinit --yes replaced a file it could not read, and the init then found nothing chosen
    and added the item back (reviewer, live: 4 launchctl calls)."""
    _rlx("service", "install")
    _rlx("service", "uninstall")
    _broken_config()
    login_os.calls.clear()

    (tmp_path / "p").mkdir()
    again = _rlx("init", "--reinit", "--yes", "--local", "--no-onboard", str(tmp_path / "p"))

    assert again.exit_code == 0, again.output
    assert "login item was left as it is: config.toml cannot be read" in again.output
    assert not _plist().exists() and login_os.calls == []


def test_status_says_an_unreadable_config_is_why_it_cannot_say(login_os: LoginOs) -> None:
    _broken_config()

    result = _rlx("--json", "service", "status")

    assert result.exit_code == 3, result.output
    why = json.loads(result.stdout)["at_login"]["why_off"]
    assert why.startswith("config.toml cannot be read"), why


def test_service_refresh_rewrites_an_item_that_is_there_and_makes_none(
    tmp_path: Path, login_os: LoginOs, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The upgrade's step, run by the install it put in place (``lifecycle``)."""
    none_there = _rlx("--json", "service", "refresh")
    assert json.loads(none_there.stdout) == {"refreshed": False}
    assert not _plist().exists()

    _rlx("service", "install")
    new_path = os.pathsep.join((str(tmp_path / "new-bin"), "/usr/bin"))
    monkeypatch.setenv("PATH", new_path)

    refreshed = _rlx("--json", "service", "refresh")

    assert json.loads(refreshed.stdout) == {"refreshed": True}
    assert plistlib.loads(_plist().read_bytes())["EnvironmentVariables"]["PATH"] == new_path


# --- tester of #33, round 2 (#2230): MAJOR B, m1, m3 ---------------------------------------------


def _garbage_config() -> None:
    """A config.toml with no line left to read a choice from."""
    path = paths.config_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("this is = not [toml\n", encoding="utf-8")


def test_a_reinit_of_a_broken_file_keeps_an_off_read_from_its_text(
    tmp_path: Path, login_os: LoginOs
) -> None:
    """MAJOR B (a): the reset rlx recommends for a broken file lost the off, and the next plain
    init added the item. Read from the file's text, the off is carried."""
    _init(tmp_path / "project", "--no-service")
    _broken_config()
    login_os.calls.clear()

    again = _rlx("init", "--reinit", "--yes", "--local", "--no-onboard", str(tmp_path / "project"))
    later = _init(tmp_path / "project")

    assert again.exit_code == 0 and later.exit_code == 0, again.output + later.output
    assert _choice() is False
    assert not _plist().exists() and login_os.calls == []


@pytest.mark.parametrize("on", [True, False], ids=["item-there", "no-item"])
def test_a_reinit_of_a_file_with_nothing_to_read_records_what_the_os_has(
    tmp_path: Path, login_os: LoginOs, on: bool
) -> None:
    """MAJOR B (a), nothing in the text: the choice is recorded as the OS has it (the item there
    is on, none is off), adding and taking away nothing."""
    _rlx("service", "install")
    if not on:
        _rlx("service", "uninstall")
    _garbage_config()
    login_os.calls.clear()
    (tmp_path / "p").mkdir()

    again = _rlx("init", "--reinit", "--yes", "--local", "--no-onboard", str(tmp_path / "p"))
    later = _init(tmp_path / "p")

    assert again.exit_code == 0 and later.exit_code == 0, again.output + later.output
    assert _choice() is on and _plist().is_file() is on
    assert login_os.changes() == []


def test_a_config_reset_of_a_broken_file_keeps_an_off_read_from_its_text(
    tmp_path: Path, login_os: LoginOs
) -> None:
    """MAJOR B (b): ``rlx config reset --yes`` kept nothing of a file that did not parse."""
    _init(tmp_path / "project", "--no-service")
    _broken_config()
    login_os.calls.clear()

    reset = _rlx("config", "reset", "--yes")
    later = _init(tmp_path / "project")

    assert reset.exit_code == 0 and later.exit_code == 0, reset.output + later.output
    assert _choice() is False
    assert not _plist().exists() and login_os.calls == []


def test_a_move_back_takes_away_every_homes_item(
    login_os: LoginOs, monkeypatch: pytest.MonkeyPatch
) -> None:
    """m1: the install every home runs loses its service, so every home's item would loop at
    every login: a named home's goes too. Another program's file beside them stays."""
    monkeypatch.setattr(service_login, "_named_home", lambda: Path("/srv/scratch-home"))
    service_login.register_at_login()
    named = service_login.label()
    monkeypatch.setattr(service_login, "_named_home", lambda: None)
    service_login.register_at_login()
    theirs = service_login.launch_agents_dir() / f"{service_login.LABEL}.helper.plist"
    theirs.write_bytes(plistlib.dumps({"Label": "studio.aisquare.rlx.service.helper"}))
    login_os.calls.clear()

    said, gone = lifecycle._no_login_item_for(service_login, "0.10.1")

    assert gone is True and said is not None
    assert sorted(path.name for path in service_login.launch_agents_dir().iterdir()) == [
        theirs.name
    ]
    assert ("launchctl", "bootout", TARGET) in login_os.calls
    assert ("launchctl", "bootout", f"{DOMAIN}/{named}") in login_os.calls


def test_an_unreadable_configs_reason_is_one_line() -> None:
    """m3: pydantic's ValidationError text is four lines; a note is one."""
    path = paths.config_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text('[updates]\ncheck = "maybe"\n', encoding="utf-8")

    why = service_login.config_unreadable()

    assert why is not None and why.startswith("config.toml cannot be read (ValidationError")
    assert "\n" not in why


def test_a_reinit_of_a_broken_file_keeps_an_on_whose_item_was_removed_outside(
    tmp_path: Path, login_os: LoginOs
) -> None:
    """Read from the text, an on is kept as it was (the item removed in the OS, so "turned off
    outside rlx"), not rewritten as the OS has it (off in rlx); and no item is added back."""
    _init(tmp_path / "project")
    _plist().unlink()
    _broken_config()
    login_os.calls.clear()

    again = _rlx("init", "--reinit", "--yes", "--local", "--no-onboard", str(tmp_path / "project"))

    assert again.exit_code == 0, again.output
    assert _choice() is True and not _plist().exists() and login_os.changes() == []
    assert service_login.at_login_state().why_off == service_login.REMOVED
