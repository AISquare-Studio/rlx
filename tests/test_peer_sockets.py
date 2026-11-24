"""The messaging folder every launched Claude Code session shares, whatever its account
(``core.peer_sockets``): what a launch sets, what it keeps, what nothing forwards onward, and
what the doctor says."""

from __future__ import annotations

import os
import shutil
import stat
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli import fleet as fleet_cli
from aisquare.cli import launch as launch_cli
from aisquare.cli.app import app
from aisquare.core import claude_accounts as core
from aisquare.core import paths, peer_sockets
from aisquare.core import tmux as tmux_core
from aisquare.core.agent_adapters import get_adapter
from aisquare.core.spawn import tmux_client_env
from aisquare.models import CheckStatus, DoctorCheck
from aisquare.services import claude_accounts as service
from aisquare.services import delivery, diagnostics
from tests import private_fleets

posix_only = pytest.mark.skipif(
    sys.platform == "win32", reason="no unix sockets on Windows: Claude Code uses named pipes"
)

_SOCKETS = get_adapter(core.AGENT).capabilities.peer_sockets
assert _SOCKETS is not None
SOCKETS = _SOCKETS
VAR = SOCKETS.folder_env
ROW = f"{core.AGENT} messaging"
#: The runner the suite stubs (``conftest.no_token_refresh``), kept as
#: tests/test_usage_token_refresh.py keeps it.
REAL_RUN = service._run_token_refresh


@pytest.fixture
def short_home(monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    """An aisquare home short enough for a socket path under it, made, in a private folder
    of ``/tmp``: a test's own ``tmp_path`` is too long for one on macOS."""
    private = private_fleets.private_folder()
    home = private / "h"
    home.mkdir()
    monkeypatch.setenv(paths.HOME_ENV_VAR, str(home))
    yield home
    for folder in (home, home / "runtime"):
        if folder.is_dir() and not folder.is_symlink():
            folder.chmod(0o700)
    shutil.rmtree(private, ignore_errors=True)


def _shared() -> str:
    return str(peer_sockets.shared())


def _launches(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> dict[str, Any]:
    """What ``aisquare launch`` would exec, recorded instead of run."""
    work = tmp_path / "repo"
    work.mkdir()
    monkeypatch.chdir(work)
    captured: dict[str, Any] = {}

    def fake_exec(binary: str, argv: list[str], env: dict[str, str]) -> None:
        captured.update(binary=binary, argv=argv, env=env)

    monkeypatch.setattr(launch_cli, "_exec", fake_exec)
    monkeypatch.setattr(shutil, "which", lambda cmd, *a, **k: f"/usr/local/bin/{cmd}")
    return captured


def _doctor_row() -> DoctorCheck:
    """The messaging row as `doctor` itself lists it."""
    (row,) = [row for row in diagnostics.doctor() if row.name == ROW]
    return row


# --------------------------------------------------------------------------- the launch


@posix_only
def test_a_launch_on_slot_2_sets_the_shared_folder_beside_its_own_scratch_dir(
    short_home: Path,
    fake_home: Path,
    runner: CliRunner,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    captured = _launches(monkeypatch, tmp_path)
    second = core.create_account()

    result = runner.invoke(app, ["launch", "coder", "--account", "2"])

    assert result.exit_code == 0, result.output
    assert captured["env"][VAR] == _shared()
    assert captured["env"][core.TMPDIR_VAR] == str(second.tmp_dir)


@posix_only
@pytest.mark.parametrize("flags", [["--account", "1"], []], ids=["slot-1", "no-flag"])
def test_a_launch_on_slot_1_or_with_no_account_flag_sets_the_shared_folder(
    flags: list[str],
    short_home: Path,
    fake_home: Path,
    runner: CliRunner,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    captured = _launches(monkeypatch, tmp_path)

    result = runner.invoke(app, ["launch", "coder", *flags])

    assert result.exit_code == 0, result.output
    assert captured["env"][VAR] == _shared()
    assert core.TMPDIR_VAR not in captured["env"]


@posix_only
def test_session_env_of_slot_2_and_of_slot_1_sets_the_shared_folder(
    short_home: Path, fake_home: Path
) -> None:
    """``accounts run`` and the sign-in window launch with :func:`service.session_env`."""
    second = core.create_account()

    assert service.session_env(second, {"PATH": "/bin"})[VAR] == _shared()
    assert service.session_env(core.default_account(), {"PATH": "/bin"})[VAR] == _shared()


@posix_only
def test_the_token_refresh_runs_in_the_account_environment_and_makes_no_folder(
    short_home: Path, fake_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    second = core.create_account()
    ran: dict[str, Any] = {}

    def fake_run(argv: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
        ran.update(kwargs)
        return subprocess.CompletedProcess(argv, 0)

    monkeypatch.setattr(subprocess, "run", fake_run)

    assert REAL_RUN(second, ["claude", "mcp", "get", "x"])
    assert VAR not in ran["env"] and peer_sockets.SET_ENV not in ran["env"]
    assert ran["env"][core.TMPDIR_VAR] == str(second.tmp_dir)
    assert not peer_sockets.shared().exists()


@posix_only
def test_a_launch_and_session_env_keep_the_folder_their_environment_sets_and_make_none(
    short_home: Path,
    fake_home: Path,
    runner: CliRunner,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    captured = _launches(monkeypatch, tmp_path)
    second = core.create_account()
    own = short_home.parent / "xdg"
    monkeypatch.setenv(VAR, str(own))

    result = runner.invoke(app, ["launch", "coder", "--account", "2"])

    assert result.exit_code == 0, result.output
    assert captured["env"][VAR] == str(own)
    assert peer_sockets.SET_ENV not in captured["env"]
    assert service.session_env(second, {VAR: str(own)})[VAR] == str(own)
    assert not own.exists()
    assert not peer_sockets.shared().exists()


@posix_only
def test_a_blank_folder_variable_counts_as_unset(short_home: Path) -> None:
    """Blank is unset, as for the desktop variables a window carries
    (``core.tmux.desktop_environment``)."""
    assert peer_sockets.apply({VAR: " "}, SOCKETS)[VAR] == _shared()
    assert peer_sockets.named({VAR: " "}, SOCKETS) is None
    assert peer_sockets.named({VAR: "/run/user/1000 "}, SOCKETS) == "/run/user/1000 "


@posix_only
def test_the_shared_folder_is_made_0700_and_owned_by_this_user_under_umask_0(
    short_home: Path,
) -> None:
    previous = os.umask(0)
    try:
        peer_sockets.apply({}, SOCKETS)
    finally:
        os.umask(previous)

    facts = peer_sockets.shared().lstat()
    assert stat.S_IMODE(facts.st_mode) == 0o700
    assert facts.st_uid == private_fleets.uid()


@posix_only
def test_a_home_whose_prompts_folder_exists_with_the_umasks_mode_still_gets_the_shared_folder(
    short_home: Path,
) -> None:
    """A machine that has spawned an agent has ``services.delivery.prompts_dir``, its parent
    made with the umask's mode, before any launch made the shared folder."""
    previous = os.umask(0o022)
    try:
        delivery.prompts_dir().mkdir(parents=True)
    finally:
        os.umask(previous)

    assert peer_sockets.apply({}, SOCKETS)[VAR] == _shared()


def test_on_windows_apply_names_no_folder_and_the_messaging_checks_have_no_row(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(sys, "platform", "win32")

    assert peer_sockets.apply({}, SOCKETS) == {}
    assert not peer_sockets.shared().exists()
    assert diagnostics._peer_sockets_checks() == []


# --------------------------------------------------------------------------- nothing forwards it


@posix_only
def test_a_non_claude_window_from_a_claude_launchs_environment_gets_mains_environment(
    short_home: Path,
    fake_home: Path,
    runner: CliRunner,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    """A Codex window a Claude session spawns takes the spawner's desktop variables
    (``desktop_environment``, its window's ``-e``) and, on a server it starts, the client's
    environment (``tmux_client_env``); a Codex launch from it copies its environment. The
    shell had no runtime dir, so none of them gets one."""
    captured = _launches(monkeypatch, tmp_path)
    core.create_account()
    assert runner.invoke(app, ["launch", "coder", "--account", "2"]).exit_code == 0
    child = captured["env"]

    assert VAR not in tmux_core.desktop_environment(child)
    assert VAR not in tmux_client_env(child) and peer_sockets.SET_ENV not in tmux_client_env(child)
    codex = peer_sockets.apply(dict(child), get_adapter("codex").capabilities.peer_sockets)
    assert VAR not in codex and peer_sockets.SET_ENV not in codex
    assert child[VAR] == _shared()


@posix_only
def test_a_desktop_runtime_dir_still_reaches_a_window_from_a_claude_launch_as_on_main(
    short_home: Path,
    fake_home: Path,
    runner: CliRunner,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    captured = _launches(monkeypatch, tmp_path)
    monkeypatch.setenv(VAR, "/run/user/1000")
    assert runner.invoke(app, ["launch", "coder"]).exit_code == 0
    child = captured["env"]

    assert tmux_core.desktop_environment(child)[VAR] == "/run/user/1000"
    assert tmux_client_env(child)[VAR] == "/run/user/1000"
    assert peer_sockets.apply(dict(child), None)[VAR] == "/run/user/1000"


def test_a_runtime_dir_changed_after_the_launch_is_no_longer_its_and_passes() -> None:
    env = {VAR: "/run/user/1000", peer_sockets.SET_ENV: f"{VAR}=/somewhere/runtime"}

    assert tmux_core.desktop_environment(env) == {VAR: "/run/user/1000"}


def test_attach_execs_tmux_without_the_runtime_dir_a_launch_set(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    execed: dict[str, Any] = {}
    monkeypatch.setattr(os, "execvpe", lambda file, argv, env: execed.update(env=env))
    monkeypatch.setenv(VAR, "/h/runtime")
    monkeypatch.setenv(peer_sockets.SET_ENV, f"{VAR}=/h/runtime")

    fleet_cli._exec_attach(["tmux", "attach-session"])

    assert VAR not in execed["env"] and peer_sockets.SET_ENV not in execed["env"]


def test_the_captains_server_drops_the_runtime_dir_a_launch_set_before_it_speaks(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from aisquare.services import mcp_server
    from aisquare.services.captain import actions, speaker

    seen: dict[str, str | None] = {}
    monkeypatch.setattr(
        speaker, "start_drainer", lambda voice: seen.update(at_start=os.environ.get(VAR))
    )
    monkeypatch.setattr(speaker, "server_voice", lambda: None)
    monkeypatch.setattr(actions, "build_server", lambda: None)
    monkeypatch.setattr(mcp_server, "run_stdio", lambda **kwargs: None)
    monkeypatch.setenv(VAR, "/h/runtime")
    monkeypatch.setenv(peer_sockets.SET_ENV, f"{VAR}=/h/runtime")

    actions.run_stdio(close_after=0)

    assert seen == {"at_start": None}


# --------------------------------------------------------------------------- what is refused


@posix_only
def test_a_folder_owned_by_another_uid_is_left_out_of_the_launch(
    short_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    shared = peer_sockets.shared()
    shared.mkdir(mode=0o700)
    someone_else = private_fleets.uid() + 1
    monkeypatch.setattr(os, "getuid", lambda: someone_else, raising=False)

    assert VAR not in peer_sockets.apply({}, SOCKETS)
    assert (
        peer_sockets.unusable(shared)
        == f"{shared} is owned by uid {os.stat(shared).st_uid}, not by this user"
    )


@posix_only
@pytest.mark.parametrize("mode", [0o755, 0o500], ids=["open-to-others", "not-writable"])
def test_a_folder_of_any_mode_but_0700_is_left_out_of_the_launch_and_the_doctor_warns(
    mode: int, short_home: Path
) -> None:
    shared = peer_sockets.shared()
    shared.mkdir(mode=0o700)
    shared.chmod(mode)

    assert VAR not in peer_sockets.apply({}, SOCKETS)
    row = _doctor_row()
    assert row.status == CheckStatus.warn
    assert f"{shared} has mode {mode:04o}, not 0700" in row.detail


@posix_only
def test_a_file_where_the_folder_goes_is_left_out_of_the_launch_and_the_doctor_warns(
    short_home: Path,
) -> None:
    shared = peer_sockets.shared()
    shared.write_text("", encoding="utf-8")

    assert VAR not in peer_sockets.apply({}, SOCKETS)
    row = _doctor_row()
    assert row.status == CheckStatus.warn
    assert f"{shared} is not a directory" in row.detail


@posix_only
def test_a_folder_gone_after_its_making_is_left_out_of_the_launch(
    short_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(Path, "mkdir", lambda self, *args, **kwargs: None)

    assert VAR not in peer_sockets.apply({}, SOCKETS)


@posix_only
def test_a_linked_home_gets_the_folder_made_under_its_target_and_named_with_no_link(
    short_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A linked aisquare home is a layout the operator may choose (tests/test_config_symlink.py)."""
    link = short_home.parent / "l"
    link.symlink_to(short_home, target_is_directory=True)
    monkeypatch.setenv(paths.HOME_ENV_VAR, str(link))
    target = short_home.resolve() / "runtime"

    named = peer_sockets.apply({}, SOCKETS)[VAR]

    assert named == str(target)
    assert Path(named).resolve() == Path(named)
    assert target.is_dir() and not target.is_symlink()


# --------------------------------------------------------------------------- the path's length


def test_a_socket_path_of_exactly_max_bytes_fits_and_one_byte_more_does_not() -> None:
    tail = f"/{SOCKETS.subfolder}/{peer_sockets.WIDEST_PID}.sock"
    fits = Path("/" + "f" * (SOCKETS.max_bytes - len(tail) - 1))
    over = Path(f"{fits}g")
    assert len(os.fsencode(f"{fits}{tail}")) == SOCKETS.max_bytes

    assert peer_sockets.too_long(fits, SOCKETS) is None
    reason = peer_sockets.too_long(over, SOCKETS)
    assert reason == (
        f"{over}{tail} is {SOCKETS.max_bytes + 1} bytes, over the {SOCKETS.max_bytes} a "
        "socket path may take"
    )


@posix_only
def test_a_home_too_long_for_a_socket_path_gets_no_folder_and_an_ok_row_naming_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A launch then leaves the variable unset, as before the shared folder: no warning, which
    the installer's summary and asq's Doctor would list as something to act on."""
    home = tmp_path / ("h" * SOCKETS.max_bytes)
    home.mkdir()
    monkeypatch.setenv(paths.HOME_ENV_VAR, str(home))

    assert VAR not in peer_sockets.apply({}, SOCKETS)
    assert not peer_sockets.shared().exists()
    row = _doctor_row()
    assert row.status == CheckStatus.ok
    assert str(peer_sockets.shared()) in row.detail
    assert f"over the {SOCKETS.max_bytes} a socket path may take" in row.detail
    assert row.detail.endswith("so launches set nothing")


# --------------------------------------------------------------------------- the doctor's row


@posix_only
def test_the_doctors_row_says_rlxs_own_folder_is_not_made_yet_and_makes_none(
    short_home: Path,
) -> None:
    row = _doctor_row()

    assert row.status == CheckStatus.ok
    assert row.detail == (
        f"{_shared()} (rlx's own, which launches from this shell set as {VAR}) is not made "
        "yet: the next launch from this shell makes it"
    )
    assert not peer_sockets.shared().exists()


@posix_only
def test_the_doctors_row_says_rlxs_own_folder_cannot_be_made_under_a_parent_it_cannot_write(
    short_home: Path,
) -> None:
    short_home.chmod(0o500)

    row = _doctor_row()

    assert row.status == CheckStatus.ok
    assert row.detail.endswith(
        f"cannot be made: {short_home.resolve()} is not writable by this user, so launches set "
        "nothing"
    )


@posix_only
def test_the_doctors_row_names_the_shared_folder_once_made(short_home: Path) -> None:
    peer_sockets.apply({}, SOCKETS)

    row = _doctor_row()

    assert row.status == CheckStatus.ok
    assert row.detail.startswith(f"{_shared()} (rlx's own, which launches from this shell")
    assert row.detail.endswith(f"put their sockets in its {SOCKETS.subfolder}")


@posix_only
def test_the_doctors_row_names_the_folder_this_shell_sets(
    short_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    own = short_home.parent / "xdg"
    own.mkdir(mode=0o700)
    monkeypatch.setenv(VAR, str(own))

    row = _doctor_row()

    assert row.status == CheckStatus.ok
    assert row.detail.startswith(f"{own} ({VAR} in this shell, which launches from it keep)")


@posix_only
def test_the_doctor_warns_of_a_folder_this_shell_names_that_does_not_exist(
    short_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    missing = short_home.parent / "gone"
    monkeypatch.setenv(VAR, str(missing))

    row = _doctor_row()

    assert row.status == CheckStatus.warn
    assert f"{missing} does not exist" in row.detail
    assert not missing.exists()


@pytest.mark.parametrize(
    ("shell", "kept"),
    [({peer_sockets.SET_ENV: f"{VAR}=/h/runtime"}, None), ({}, "/h/runtime")],
    ids=["a-launch-set-it", "the-desktop-set-it"],
)
def test_team_spawn_exec_drops_the_runtime_dir_a_launch_set_and_keeps_a_desktops(
    shell: dict[str, str],
    kept: str | None,
    runner: CliRunner,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    """``team spawn --exec`` starts the role itself, naming no shared folder of its own."""
    (tmp_path / "repo").mkdir()
    monkeypatch.chdir(tmp_path / "repo")
    monkeypatch.setattr(shutil, "which", lambda cmd, *a, **k: f"/usr/local/bin/{cmd}")
    execed: dict[str, Any] = {}
    monkeypatch.setattr(os, "execvpe", lambda file, argv, env: execed.update(env=env))
    monkeypatch.setenv(VAR, "/h/runtime")
    for name, value in shell.items():
        monkeypatch.setenv(name, value)

    result = runner.invoke(app, ["team", "spawn", "coder", "--exec", "--no-probe"])

    assert result.exit_code == 0, result.output
    assert execed["env"].get(VAR) == kept
    assert peer_sockets.SET_ENV not in execed["env"]
