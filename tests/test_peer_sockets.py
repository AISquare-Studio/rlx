"""The messaging folder every launched Claude Code session shares, whatever its account
(``core.peer_sockets``): what a launch sets, what it keeps, and what the doctor says."""

from __future__ import annotations

import os
import shutil
import stat
import sys
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli import launch as launch_cli
from aisquare.cli.app import app
from aisquare.core import claude_accounts as core
from aisquare.core import paths, peer_sockets
from aisquare.core.agent_adapters import get_adapter
from aisquare.models import CheckStatus
from aisquare.services import claude_accounts as service
from aisquare.services import diagnostics
from tests import private_fleets

posix_only = pytest.mark.skipif(
    sys.platform == "win32", reason="no unix sockets on Windows: Claude Code uses named pipes"
)

_SOCKETS = get_adapter(core.AGENT).capabilities.peer_sockets
assert _SOCKETS is not None
SOCKETS = _SOCKETS
VAR = SOCKETS.folder_env


@pytest.fixture
def short_home(monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    """An aisquare home short enough for a socket path under it, in a private folder of
    ``/tmp``: a test's own ``tmp_path`` is too long for one on macOS."""
    private = private_fleets.private_folder()
    home = private / "h"
    monkeypatch.setenv(paths.HOME_ENV_VAR, str(home))
    yield home
    shutil.rmtree(private, ignore_errors=True)


def _shared() -> str:
    return str(paths.messaging_dir().absolute())


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


def _doctor_row() -> Any:
    (row,) = diagnostics._peer_sockets_checks()
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
    assert service.session_env(second, {VAR: str(own)})[VAR] == str(own)
    assert not own.exists()
    assert not paths.messaging_dir().exists()


@posix_only
def test_a_blank_folder_variable_reads_as_unset(short_home: Path) -> None:
    assert peer_sockets.apply({VAR: " "}, SOCKETS)[VAR] == _shared()


@posix_only
def test_the_shared_folder_is_made_0700_and_owned_by_this_user_under_umask_0(
    short_home: Path,
) -> None:
    previous = os.umask(0)
    try:
        peer_sockets.apply({}, SOCKETS)
    finally:
        os.umask(previous)

    facts = paths.messaging_dir().lstat()
    assert stat.S_IMODE(facts.st_mode) == 0o700
    assert facts.st_uid == private_fleets.uid()


def test_on_windows_a_launch_sets_no_folder_and_the_doctor_has_no_row(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(sys, "platform", "win32")

    assert peer_sockets.apply({}, SOCKETS) == {}
    assert not paths.messaging_dir().exists()
    assert diagnostics._peer_sockets_checks() == []


# --------------------------------------------------------------------------- what is refused


@posix_only
def test_a_folder_owned_by_another_uid_is_left_out_of_the_launch_and_named_by_the_doctor(
    short_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    shared = paths.messaging_dir()
    shared.mkdir(parents=True, mode=0o700)
    someone_else = private_fleets.uid() + 1
    monkeypatch.setattr(os, "getuid", lambda: someone_else, raising=False)

    assert VAR not in peer_sockets.apply({}, SOCKETS)
    row = _doctor_row()
    assert row.status == CheckStatus.warn
    assert str(shared) in row.detail and f"owned by uid {os.stat(shared).st_uid}" in row.detail


@posix_only
def test_a_folder_open_to_others_is_left_out_of_the_launch_and_named_by_the_doctor(
    short_home: Path,
) -> None:
    shared = paths.messaging_dir()
    shared.mkdir(parents=True, mode=0o700)
    shared.chmod(0o755)

    assert VAR not in peer_sockets.apply({}, SOCKETS)
    row = _doctor_row()
    assert row.status == CheckStatus.warn
    assert str(shared) in row.detail and "mode 0755, not 0700" in row.detail


@posix_only
def test_a_file_where_the_folder_goes_is_left_out_of_the_launch_and_named_by_the_doctor(
    short_home: Path,
) -> None:
    shared = paths.messaging_dir()
    shared.parent.mkdir(parents=True)
    shared.write_text("", encoding="utf-8")

    assert VAR not in peer_sockets.apply({}, SOCKETS)
    row = _doctor_row()
    assert row.status == CheckStatus.warn
    assert f"{shared} is not a directory" in row.detail


# --------------------------------------------------------------------------- the path's length


def test_a_socket_path_of_exactly_max_bytes_fits_and_one_byte_more_does_not() -> None:
    tail = f"/{SOCKETS.subfolder}/{peer_sockets.WIDEST_PID}.sock"
    fits = Path("/" + "f" * (SOCKETS.max_bytes - len(tail) - 1))
    over = Path(f"{fits}g")
    assert len(os.fsencode(f"{fits}{tail}")) == SOCKETS.max_bytes

    assert peer_sockets.problem(fits, SOCKETS) is None
    reason = peer_sockets.problem(over, SOCKETS)
    assert reason == (
        f"{over}{tail} is {SOCKETS.max_bytes + 1} bytes, over the {SOCKETS.max_bytes} a "
        "socket path may take"
    )


@posix_only
def test_a_home_too_long_for_a_socket_path_is_left_out_of_the_launch_and_named_by_the_doctor(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    home = tmp_path / ("h" * SOCKETS.max_bytes)
    monkeypatch.setenv(paths.HOME_ENV_VAR, str(home))

    assert VAR not in peer_sockets.apply({}, SOCKETS)
    row = _doctor_row()
    assert row.status == CheckStatus.warn
    assert str(paths.messaging_dir().absolute()) in row.detail
    assert f"over the {SOCKETS.max_bytes} a socket path may take" in row.detail


# --------------------------------------------------------------------------- the doctor's row


@posix_only
def test_the_doctors_row_names_the_shared_folder_and_makes_none(short_home: Path) -> None:
    row = _doctor_row()

    assert row.status == CheckStatus.ok
    assert row.detail.startswith(f"{_shared()} (rlx's own, which launches set as {VAR})")
    assert row.detail.endswith(f"in its {SOCKETS.subfolder}; the first launch makes it")
    assert not paths.messaging_dir().exists()


@posix_only
def test_the_doctors_row_names_the_folder_this_shell_sets(
    short_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    own = short_home.parent / "xdg"
    own.mkdir(mode=0o700)
    monkeypatch.setenv(VAR, str(own))

    row = _doctor_row()

    assert row.status == CheckStatus.ok
    assert row.detail.startswith(f"{own} ({VAR} as this shell sets it, which launches keep)")
    assert not row.detail.endswith("the first launch makes it")
