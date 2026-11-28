"""Each agent's own temporary directory (#57; the owner's disk-tidy brief, item 1).

Every fleet agent, and every launch by hand, runs with ``TMPDIR`` (``TEMP`` and ``TMP`` on
Windows) at ``<home>/tmp/<key>``, made ``0700`` at launch and removed once the agent has
ended: whatever its tools leave in the temporary directory, in any language, goes with it.

- A fleet agent's key is minted at its first spawn and kept in its launch spec, which every
  replacement replays: a restart and a switch keep the directory and its files. A fork, or
  another agent, gets its own.
- A launch from inside an agent is a launch by hand: its own key, never its parent's.
- A ``TMPDIR`` the binding or ``--env`` names is the operator's and wins.
- A home that would make the path longer than macOS's own temporary directory keeps the
  inherited one; a socket bound in pytest's ``tmp_path`` under a real one fits.
- :func:`agent_tmp.ended_dirs` is the one selection, read-only; ``sweep_ended`` removes it. Live,
  paused and recently ended agents keep theirs; so does any whose process still runs.

Driven as ``tests/test_fleet_service.py`` drives the fleet (the real store in an isolated
home, that suite's in-memory ``FakeTmux``) and ``tests/test_launch.py`` drives a launch (its
exec intercepted). No real tmux server, and no real agent, is started.
"""

from __future__ import annotations

import contextlib
import json
import os
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import textwrap
import tomllib
from collections.abc import Iterator
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.core import paths, procs
from aisquare.core.agent_adapters import get_adapter
from aisquare.core.agent_adapters.codex import PERMISSION_PROFILE
from aisquare.core.agent_adapters.types import store_dir
from aisquare.core.spawn import OUTER_TMPDIR_ENV, TMP_KEY_ENV, tmux_client_env
from aisquare.core.store import store_session
from aisquare.models import FleetAgent, LaunchSpec, ProjectInfo
from aisquare.services import agent_tmp, store_broker, usage_stats
from aisquare.services import fleet as fleet_service
from tests import test_fleet_service as fleet_suite
from tests import test_launch as launch_suite
from tests.test_fleet_service import FakeTmux

tmux = fleet_suite.tmux
clock = fleet_suite.clock
claude_on_path = fleet_suite.claude_on_path
repo = fleet_suite.repo
project = fleet_suite.project
work_dir = launch_suite.work_dir
spy = launch_suite.spy

posix = pytest.mark.skipif(sys.platform == "win32", reason="TMPDIR and unix sockets: POSIX")


@pytest.fixture
def roomy(monkeypatch: pytest.MonkeyPatch) -> None:
    """A test home under ``tmp_path`` is longer than any real one: the length rule is pinned
    on its own (``test_a_home_too_long_for_a_socket_keeps_the_inherited_temp_dir``)."""
    monkeypatch.setattr(agent_tmp, "MAX_BYTES", 10_000)


def _key_of(agent: FleetAgent) -> str:
    assert agent.launch_spec is not None and agent.launch_spec.tmp_key is not None
    return agent.launch_spec.tmp_key


def _window_key(tmux: FakeTmux, index: int = -1) -> object:
    env = tmux.spawned[index]["env"]
    assert isinstance(env, dict)
    return env.get(TMP_KEY_ENV)


# --- the key: minted once, replayed by every replacement -----------------------------


def test_a_spawn_mints_a_key_that_its_window_carries_and_a_restart_keeps(
    tmux: FakeTmux, claude_on_path: Path, project: ProjectInfo
) -> None:
    agent = fleet_service.spawn(project, "manager").agent
    key = _key_of(agent)
    assert agent_tmp.is_key(key) and _window_key(tmux) == key

    tmux.die(agent.pane_id, 0)
    fleet_service.list_agents(project)
    started = fleet_service.restart(project, "manager").started

    assert started.id != agent.id, "a restart is a new row"
    assert _key_of(started) == key and _window_key(tmux) == key, "with the same directory"


def test_a_switch_keeps_the_key_and_a_fork_gets_its_own(
    tmux: FakeTmux, claude_on_path: Path, project: ProjectInfo, monkeypatch: pytest.MonkeyPatch
) -> None:
    fleet_suite._two_slots_with_usage(monkeypatch, work=95, personal=10)
    agent = fleet_service.spawn(project, "coder", worktree=False, account="2").agent
    key = _key_of(agent)

    switched = fleet_service.switch(project, agent.label, reason="session limit").started
    assert switched.account_slot == 3 and _key_of(switched) == key

    tmux.die(switched.pane_id, 0)
    fleet_service.list_agents(project)
    fork = fleet_service.hand_off(project, switched.label).started.agent
    assert _key_of(fork) != key and agent_tmp.is_key(_key_of(fork))
    assert _window_key(tmux) == _key_of(fork)


def test_a_relabel_keeps_the_key_with_the_row(
    tmux: FakeTmux, claude_on_path: Path, project: ProjectInfo
) -> None:
    """Why the key is the spec's and not the label's: a relabel renames a live agent, and a
    label-keyed directory would then match no live row and be swept from under it."""
    agent = fleet_suite._coder(project)
    fleet_service.relabel(project, agent.label, "login fix")
    with store_session() as store:
        row = store.get_fleet_agent(agent.id)
    assert row is not None and row.label != agent.label and _key_of(row) == _key_of(agent)


# --- the launch: where TMPDIR is set -------------------------------------------------


@posix
def test_a_launch_by_hand_runs_its_agent_in_a_private_dir_of_its_own(
    runner: CliRunner, work_dir: Path, spy: dict[str, Any], roomy: None
) -> None:
    inherited = os.environ.get("TMPDIR", "")
    result = runner.invoke(app, ["launch", "coder"])

    assert result.exit_code == 0, result.output
    env = spy["env"]
    directory = Path(env["TMPDIR"])
    assert directory.parent == paths.agent_tmp_root() and agent_tmp.is_key(directory.name)
    assert stat.S_IMODE(directory.stat().st_mode) == 0o700
    assert env[OUTER_TMPDIR_ENV] == inherited and TMP_KEY_ENV not in env
    owner = agent_tmp.read_owner(directory.name)
    assert owner is not None and owner.kind == "launch" and owner.pid == os.getpid()
    # The launcher's own TMPDIR is the agent's too: its sandbox and broker steps model the
    # agent's writable roots from it, and its own temporary files land there.
    assert os.environ["TMPDIR"] == str(directory)
    assert Path(tempfile.gettempdir()) == directory


@posix
def test_a_fleet_window_runs_in_the_dir_its_spec_names_and_a_nested_launch_in_its_own(
    runner: CliRunner,
    work_dir: Path,
    spy: dict[str, Any],
    roomy: None,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(TMP_KEY_ENV, "abc234")
    monkeypatch.setenv("AISQUARE_LAUNCH_ID", "")  # a fleet window's: no launch inherited
    assert runner.invoke(app, ["launch", "coder"]).exit_code == 0
    window = spy["env"]
    assert window["TMPDIR"] == str(paths.agent_tmp_dir("abc234"))
    owner = agent_tmp.read_owner("abc234")
    assert owner is not None and owner.kind == "fleet"

    # The same variables, inherited by a launch from inside that agent's tool shell: it is
    # a launch by hand, with a directory of its own, and the true outer TMPDIR kept.
    for name in ("TMPDIR", OUTER_TMPDIR_ENV, "AISQUARE_LAUNCH_ID"):
        monkeypatch.setenv(name, window[name])
    monkeypatch.setenv(TMP_KEY_ENV, "abc234")
    assert runner.invoke(app, ["launch", "coder"]).exit_code == 0
    nested = spy["env"]
    assert nested["TMPDIR"] != window["TMPDIR"]
    assert Path(nested["TMPDIR"]).parent == paths.agent_tmp_root()
    assert nested[OUTER_TMPDIR_ENV] == window[OUTER_TMPDIR_ENV]


@posix
def test_a_tmpdir_the_binding_names_is_the_operators(
    runner: CliRunner, work_dir: Path, spy: dict[str, Any], roomy: None, tmp_path: Path
) -> None:
    chosen = tmp_path / "mine"
    result = runner.invoke(app, ["launch", "coder", "--env", f"TMPDIR={chosen}"])

    assert result.exit_code == 0, result.output
    assert spy["env"]["TMPDIR"] == str(chosen)
    assert not paths.agent_tmp_root().exists(), "nothing made"


def test_windows_sets_temp_and_tmp_with_no_socket_cap(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(sys, "platform", "win32")
    monkeypatch.setattr(agent_tmp, "VARS", ("TEMP", "TMP"))
    monkeypatch.setattr(agent_tmp, "MAX_BYTES", 1)  # no cap there: Windows binds no unix socket
    env: dict[str, str] = {}
    environ: dict[str, str] = {}

    assert agent_tmp.prepare(env, environ=environ) is None

    directory = env["TEMP"]
    assert env["TMP"] == directory == environ["TEMP"] == environ["TMP"]
    assert Path(directory).parent == paths.agent_tmp_root() and "TMPDIR" not in env


@posix
def test_a_home_too_long_for_a_socket_keeps_the_inherited_temp_dir() -> None:
    """The test home is under ``tmp_path``, far past the 49 bytes macOS's own TMPDIR takes."""
    assert len(os.fsencode(paths.agent_tmp_dir("abc234"))) > agent_tmp.MAX_BYTES
    env = {"TMPDIR": "/inherited"}
    environ: dict[str, str] = {}

    said = agent_tmp.prepare(env, environ=environ)

    assert said is not None and "keeps the inherited one" in said and "49" in said
    assert env == {"TMPDIR": "/inherited"} and environ == {}
    assert not paths.agent_tmp_root().exists()


@posix
def test_a_launch_that_keeps_the_inherited_temp_dir_says_so_once_it_goes_ahead(
    runner: CliRunner, work_dir: Path, spy: dict[str, Any]
) -> None:
    """The test home is too long: the launch says so in one line, before its banner. A launch
    refused before that (an account no one holds) says nothing of it."""
    result = runner.invoke(app, ["launch", "coder"])
    assert result.exit_code == 0, result.output
    said = [line for line in result.output.splitlines() if line.startswith("agent temp: ")]
    assert len(said) == 1 and "keeps the inherited one" in said[0]
    assert result.output.index("agent temp: ") < result.output.index("Launching ")

    refused = runner.invoke(app, ["launch", "coder", "--account", "9"])
    assert refused.exit_code != 0 and "agent temp: " not in refused.output


@posix
def test_a_link_in_place_of_the_dir_is_refused(roomy: None, tmp_path: Path) -> None:
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    paths.agent_tmp_root().mkdir(parents=True)
    paths.agent_tmp_dir("abc234").symlink_to(elsewhere)
    env = {TMP_KEY_ENV: "abc234", "TMPDIR": "/inherited"}
    environ: dict[str, str] = {}

    said = agent_tmp.prepare(env, environ=environ)

    assert said is not None and "could not be made" in said
    assert env["TMPDIR"] == "/inherited" and environ == {}


# --- removal: ended_dirs is the one selection, sweep removes it -----------------------


NOW = datetime(2026, 11, 27, 12, 0, tzinfo=UTC)


@pytest.fixture
def processes(monkeypatch: pytest.MonkeyPatch) -> dict[int, str]:
    """The processes that run: pid -> start token. Nothing else does."""
    running: dict[int, str] = {}
    monkeypatch.setattr(procs, "alive", lambda pid: pid in running)
    monkeypatch.setattr(
        procs,
        "inspect",
        lambda pid: procs.ProcInfo(1, running[pid], "") if pid in running else None,
    )
    return running


def _dir(key: str, *, kind: str | None = "fleet", pid: int = 4242, at: datetime = NOW) -> Path:
    directory = paths.agent_tmp_dir(key)
    directory.mkdir(parents=True)
    (directory / "pytest-of-someone").mkdir()
    if kind is not None:
        record = {"kind": kind, "pid": pid, "started": "t0", "at": at.isoformat()}
        (paths.agent_tmp_root() / f"{key}.owner").write_text(json.dumps(record))
    return directory


def _row(
    project: ProjectInfo,
    key: str,
    *,
    ended: datetime | None,
    paused: datetime | None = None,
    label: str = "coder-1",
) -> None:
    with store_session() as store:
        store.upsert_fleet_agent(
            FleetAgent(
                id=f"agt_{key}_{label}",
                project_id=project.id,
                label=label,
                role="coder",
                pane_id="%9",
                cwd=project.root,
                created_at=NOW - timedelta(hours=1),
                ended_at=ended,
                paused_at=paused,
                launch_spec=LaunchSpec(binary="claude", tmp_key=key),
            )
        )


def test_a_fleet_agents_dir_goes_only_once_it_ended_past_the_grace(
    project: ProjectInfo, processes: dict[int, str]
) -> None:
    live, recent, ended, paused = (_dir(k) for k in ("aaaaaa", "bbbbbb", "cccccc", "dddddd"))
    _row(project, "aaaaaa", ended=None, label="coder-1")
    _row(project, "bbbbbb", ended=NOW - timedelta(minutes=9), label="coder-2")
    _row(project, "cccccc", ended=NOW - timedelta(minutes=10), label="coder-3")
    _row(project, "dddddd", ended=NOW - timedelta(days=3), paused=NOW, label="coder-4")

    assert agent_tmp.ended_dirs(now=NOW) == [ended]
    assert all(d.exists() for d in (live, recent, ended, paused)), "the selection reads only"

    assert agent_tmp.sweep_ended(now=NOW) == [ended]
    assert not ended.exists() and not (paths.agent_tmp_root() / "cccccc.owner").exists()
    assert live.exists() and recent.exists() and paused.exists()


def test_a_dir_whose_process_still_runs_is_kept_whatever_its_row_says(
    project: ProjectInfo, processes: dict[int, str]
) -> None:
    held = _dir("aaaaaa", pid=4242)
    processes[4242] = "t0"
    _row(project, "aaaaaa", ended=NOW - timedelta(days=1))

    assert agent_tmp.ended_dirs(now=NOW) == []

    processes[4242] = "t1"  # the pid is another process's now: the agent is gone
    assert agent_tmp.ended_dirs(now=NOW) == [held]


def test_a_hand_launchs_dir_goes_with_its_process(processes: dict[int, str]) -> None:
    running = _dir("aaaaaa", kind="launch", pid=11)
    gone = _dir("bbbbbb", kind="launch", pid=12)
    processes[11] = "t0"

    assert agent_tmp.ended_dirs(now=NOW) == [gone]
    assert agent_tmp.sweep_ended(now=NOW) == [gone] and running.exists()


def test_a_fleet_dir_no_row_carries_goes_after_the_grace(processes: dict[int, str]) -> None:
    """A spawn whose window never wrote its row: kept while its process runs, then once its
    record is older than the grace."""
    young = _dir("aaaaaa", at=NOW - timedelta(minutes=5))
    old = _dir("bbbbbb", at=NOW - timedelta(minutes=11))

    assert agent_tmp.ended_dirs(now=NOW) == [old]
    assert young.exists()


def test_a_dir_with_no_record_goes_after_a_day_and_nothing_else_is_touched(
    processes: dict[int, str], tmp_path: Path
) -> None:
    orphan = _dir("aaaaaa", kind=None)
    old = (NOW - timedelta(days=2)).timestamp()
    os.utime(orphan, (old, old))
    fresh = _dir("bbbbbb", kind=None)
    os.utime(fresh, (NOW.timestamp(), NOW.timestamp()))
    root = paths.agent_tmp_root()
    (root / "notakey").mkdir()
    (root / "abcdef").write_text("a file, not a dir")
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    if sys.platform != "win32":
        (root / "zzzzzz").symlink_to(elsewhere)

    assert agent_tmp.sweep_ended(now=NOW) == [orphan]
    assert fresh.exists() and (root / "notakey").exists() and (root / "abcdef").exists()
    assert elsewhere.exists()
    assert not agent_tmp.remove_dir(elsewhere), "nothing outside <home>/tmp"


def test_reap_removes_the_ended_dirs_and_says_so(
    tmux: FakeTmux, project: ProjectInfo, processes: dict[int, str]
) -> None:
    ended = _dir("cccccc")
    _row(project, "cccccc", ended=datetime.now(UTC) - timedelta(hours=1))

    report = fleet_service.reap(project)

    assert report.temp_dirs_removed == [ended] and not ended.exists()


def test_every_listing_sweeps_at_most_once_a_minute(
    tmux: FakeTmux, claude_on_path: Path, project: ProjectInfo, monkeypatch: pytest.MonkeyPatch
) -> None:
    fleet_suite._coder(project)
    swept: list[int] = []

    def sweep_ended() -> list[Path]:
        swept.append(1)
        return []

    monkeypatch.setattr(agent_tmp, "sweep_ended", sweep_ended)
    monkeypatch.setattr(agent_tmp, "_last_sweep", 0.0)

    fleet_service.list_agents(project)
    fleet_service.list_agents(project)

    assert swept == [1]


@posix
def test_removal_clears_what_the_agent_left_read_only_and_never_follows_a_link(
    processes: dict[int, str], tmp_path: Path
) -> None:
    directory = _dir("aaaaaa", kind="launch", pid=12)
    locked = directory / "locked"
    (locked / "deeper").mkdir(parents=True)
    (locked / "deeper" / "file").write_text("x")
    (locked / "deeper").chmod(0o500)
    locked.chmod(0o000)
    outside = tmp_path / "outside"
    outside.mkdir()
    (outside / "keep").write_text("mine")
    outside.chmod(0o500)
    (directory / "link").symlink_to(outside)
    try:
        assert agent_tmp.sweep_ended(now=NOW) == [directory] and not directory.exists()
        assert stat.S_IMODE(outside.stat().st_mode) == 0o500, "a link's target is never changed"
        assert (outside / "keep").read_text() == "mine"
    finally:  # what a failure leaves, pytest can still clean
        for each in (outside, locked, locked / "deeper"):
            if os.path.lexists(each):
                with contextlib.suppress(OSError):
                    each.chmod(0o700)


# --- a sandboxed Codex may write its own TMPDIR, inside the home it holds read-only -----


def _table(
    env: dict[str, str], argv: list[str], monkeypatch: pytest.MonkeyPatch
) -> dict[str, str] | None:
    """What ``start_for_launch`` hands Codex as its filesystem table; ``None`` when it hands
    none (no store profile: the operator's sandbox). Its broker is never started."""

    def not_started(directory: Path, env: object) -> None:
        raise RuntimeError("not started in this test")

    monkeypatch.setattr(store_broker, "run_broker", not_started)
    launched, _ = store_broker.start_for_launch(env, get_adapter("codex"), argv)
    key = f"permissions.{PERMISSION_PROFILE}.filesystem="
    found = [arg for arg in launched if arg.startswith(key)]
    if not found:
        return None
    table: dict[str, str] = tomllib.loads(f"t = {found[-1].partition('=')[2]}")["t"]
    return table


@pytest.mark.parametrize(
    ("case", "seat", "own", "writes"),
    [
        ("the store's profile, a coder", "workspace", True, True),
        ("the store's profile, a read-only reviewer", "read_only", True, True),
        ("a TMPDIR outside the home: Codex's own rule", "workspace", False, False),
        ("the operator's sandbox, no store profile", None, True, None),
    ],
)
def test_a_sandboxed_codex_may_write_its_own_tmpdir_in_the_home_it_holds_read_only(
    case: str,
    seat: str | None,
    own: bool,
    writes: bool | None,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Review of #643, the MAJOR: under the store's profile a sandboxed Codex holds the whole
    rlx home read-only, and its TMPDIR is in it. Its own directory is writable within (the
    more specific path winning, as its broker's inbox is), to every seat; its owner record
    stays read-only; another TMPDIR adds nothing.

    The env names it as a launch does here (:func:`agent_tmp.prepare`): ``TMPDIR`` on POSIX,
    ``TEMP`` and ``TMP`` on Windows, where Codex runs under the same profile (CI on 3e689512
    set ``TMPDIR`` alone, which no Windows launch does)."""
    monkeypatch.chdir(tmp_path)
    mine = paths.agent_tmp_dir("abc234").resolve() if own else tmp_path / "T"
    env = {"AISQUARE_LAUNCH_ID": "launch-57", **dict.fromkeys(agent_tmp.VARS, str(mine))}
    codex = get_adapter("codex")
    argv = (
        ["codex", *codex.store_access(seat, store_dir())]  # type: ignore[arg-type]
        if seat is not None
        else ["codex", "--sandbox", "workspace-write"]
    )

    table = _table(env, argv, monkeypatch)

    if writes is None:
        assert table is None, case
        return
    assert table is not None and table[str(store_dir())] == "read", case
    assert (table.get(str(mine)) == "write") is writes, case
    owner = paths.agent_tmp_root().resolve() / "abc234.owner"
    assert table.get(str(owner)) != "write", "the record beside it stays the home's"


# --- what stays shared ------------------------------------------------------------------


def test_a_tmux_server_started_from_an_agent_keeps_the_tmpdir_it_inherited() -> None:
    inside = {"TMPDIR": "/home/u/.rlx/tmp/abc234", OUTER_TMPDIR_ENV: "/var/folders/x/T/"}
    assert tmux_client_env(inside)["TMPDIR"] == "/var/folders/x/T/"
    client = tmux_client_env({"TMPDIR": "/home/u/.rlx/tmp/abc234", OUTER_TMPDIR_ENV: ""})
    assert "TMPDIR" not in client and OUTER_TMPDIR_ENV not in client
    assert tmux_client_env({"TMPDIR": "/tmp", TMP_KEY_ENV: "abc234"}) == {"TMPDIR": "/tmp"}


def test_a_repository_made_in_an_agents_dir_is_a_throwaway() -> None:
    assert paths.agent_tmp_root() in usage_stats._ephemeral_roots()


# --- macOS: a socket bound in pytest's tmp_path under a real-length TMPDIR fits ---------


@pytest.fixture
def short_home() -> Iterator[Path]:
    """A home as short as a person's: under ``/tmp``, resolved as pytest resolves its own
    basetemp (``/private/tmp`` on macOS), removed after."""
    if sys.platform == "win32":
        pytest.skip("no /tmp and no unix sockets")
    made = Path(tempfile.mkdtemp(prefix="r57.", dir="/tmp")).resolve()
    try:
        yield made
    finally:
        shutil.rmtree(made, ignore_errors=True)


@pytest.mark.skipif(sys.platform != "darwin", reason="macOS's 104-byte sun_path")
def test_a_socket_in_tmp_path_binds_under_a_real_length_agent_dir(
    short_home: Path, tmp_path: Path
) -> None:
    """The brief's proof. The agent directory a real home gives (``~/.rlx/tmp/<key>``) is
    padded to this machine's length; a child pytest run with it as TMPDIR binds a socket in
    ``tmp_path`` from a test whose name is at pytest's 30-character cap."""
    real = len(os.fsencode(Path.home() / ".rlx" / "tmp" / "abc234"))
    assert real <= agent_tmp.MAX_BYTES, f"the real path is {real} bytes"
    directory = short_home / ("d" * (real - len(os.fsencode(short_home)) - 1))
    directory.mkdir(mode=0o700)
    assert len(os.fsencode(directory)) == real
    suite = tmp_path / "suite"
    suite.mkdir()
    (suite / "test_bind.py").write_text(
        textwrap.dedent(
            """
            import socket

            def test_a_name_long_enough_to_reach_the_cap_of_pytest(tmp_path):
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                    s.bind(str(tmp_path / "s.sock"))
            """
        )
    )
    env = {
        "PATH": os.environ.get("PATH", ""),
        "HOME": str(short_home),
        "TMPDIR": str(directory),
        "USER": os.environ.get("USER", "runner"),
        "PYTEST_DISABLE_PLUGIN_AUTOLOAD": "1",
    }
    done = subprocess.run(
        [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", "-c", os.devnull,
         "--rootdir", str(suite), str(suite / "test_bind.py")],
        cwd=suite, env=env, capture_output=True, text=True, timeout=120,
    )  # fmt: skip
    assert done.returncode == 0, done.stdout + done.stderr
    assert "1 passed" in done.stdout


def test_the_socket_bind_itself_needs_no_room_beyond_sun_path(short_home: Path) -> None:
    """The child's check can fail: the same bind under a path past sun_path is refused."""
    if not hasattr(socket, "AF_UNIX"):
        pytest.skip("no unix sockets here")
    deep = short_home / ("x" * 120)
    deep.mkdir()
    with (
        socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock,
        pytest.raises(OSError),
    ):
        sock.bind(str(deep / "s.sock"))
