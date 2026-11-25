"""0.10.3 §4.1 (4.1a): the registry of long-lived non-UI rlx processes, ``<home>/daemons/``.

An upgrade counted the asq UIs alone. Every daemon (``watch``, ``serve``, later rlxd) now
keeps a record and a held lock in ``<home>/daemons/`` for as long as it runs, and the next
reader prunes the record of one that is gone. These pin the module's rules (``core.daemons``):
the lock decides, not the pid; a record this release cannot read is reported, never crashed
on, and kept while its lock is held; a file is a record only by its name. And that the two
daemons of 4.1a, ``watch --detach``'s watcher and ``serve``, register as what starts them
again and leave as they exit, on stand-ins: no tmux and no MCP transport.
"""

from __future__ import annotations

import json
import os
import signal
import stat
import subprocess
import sys
import time
from collections.abc import Iterator
from datetime import UTC, datetime, timedelta
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli import serve as serve_cli
from aisquare.cli.app import app
from aisquare.core import daemons, locking, orchestrator, selfcli, workspace
from aisquare.core.registry import host_name
from aisquare.core.version import __version__
from aisquare.services import fleet as fleet_service
from aisquare.services import mcp_server
from aisquare.services import team as team_service
from aisquare.services import watch as watch_service

ARGV = ["/opt/rlx/bin/python", "-P", "-m", "aisquare", "watch", "--detach"]


@pytest.fixture
def registry() -> Path:
    folder = daemons.registry_dir()
    folder.mkdir(parents=True, exist_ok=True)
    return folder


@pytest.fixture(autouse=True)
def keep_sigterm() -> Iterator[None]:
    """``watch_service.run`` takes SIGTERM for its loop and never gives it back: given back
    here, so the suite's own SIGTERM still ends it."""
    before = signal.getsignal(signal.SIGTERM)
    yield
    signal.signal(signal.SIGTERM, before)


def _files() -> list[str]:
    try:
        return sorted(os.listdir(daemons.registry_dir()))
    except FileNotFoundError:
        return []


def _record(**overrides: Any) -> dict[str, Any]:
    now = datetime.now(UTC).isoformat()
    body: dict[str, Any] = {
        "kind": "serve",
        "pid": os.getpid(),
        "since": now,
        "argv": ["rlx", "serve"],
        "restartable": True,
        "host": host_name(),
        "version": "0.10.3",
        "heartbeat": now,
        "cwd": "/somewhere",
    }
    body.update(overrides)
    return body


def _write(folder: Path, daemon_id: str, body: object, *, lock: bool = True) -> Path:
    path = folder / f"{daemon_id}.json"
    path.write_text(body if isinstance(body, str) else json.dumps(body), encoding="utf-8")
    if lock:
        (folder / f"{daemon_id}.lock").touch()
    return path


def _hold(path: Path) -> int:
    """``path``'s lock, taken as a running daemon would hold it: from another open file, so
    this process's own probes see it held."""
    fd = locking.open_lock_file(path, mode=0o600)
    locking.acquire(fd, wait_s=0)
    return fd


def _age(path: Path, seconds: float) -> None:
    then = time.time() - seconds
    os.utime(path, (then, then))


# --- register, list, deregister ----------------------------------------------------------------


def test_a_registered_daemon_is_listed_live_until_it_deregisters() -> None:
    registration = daemons.register("watch", argv=ARGV, restartable=True)
    try:
        (daemon,) = daemons.list_live()
        assert daemon == registration.daemon
        assert (daemon.kind, daemon.pid, daemon.argv, daemon.restartable) == (
            "watch",
            os.getpid(),
            tuple(ARGV),
            True,
        )
        assert (daemon.host, daemon.version, daemon.cwd) == (host_name(), __version__, os.getcwd())
        assert daemons.valid_id(daemon.id)
        assert _files() == [f"{daemon.id}.json", f"{daemon.id}.lock"]
        # The record on disk is the module's table, and carries no id: the file name is it.
        on_disk = json.loads((daemons.registry_dir() / f"{daemon.id}.json").read_text("utf-8"))
        assert set(on_disk) == {
            "kind",
            "pid",
            "since",
            "argv",
            "restartable",
            "host",
            "version",
            "heartbeat",
            "cwd",
        }
        assert on_disk["argv"] == ARGV and on_disk["pid"] == os.getpid()
        assert datetime.fromisoformat(on_disk["since"]).utcoffset() == timedelta(0)
        assert daemons.list_live(kind="serve") == [] and daemons.list_live(kind="watch")
    finally:
        registration.deregister()
    assert daemons.list_live() == []
    assert _files() == [], "the record and its lock leave with the daemon"
    registration.deregister()  # once: a second call is nothing


def test_the_heartbeat_rewrites_the_record() -> None:
    registration = daemons.register("serve", argv=["rlx", "serve"], restartable=True)
    try:
        before = registration.daemon.heartbeat
        time.sleep(0.01)
        registration.beat()
        (daemon,) = daemons.list_live()
        assert daemon.heartbeat > before and daemon.since == registration.daemon.since
    finally:
        registration.deregister()


def test_register_takes_only_a_known_kind_and_a_named_program() -> None:
    with pytest.raises(ValueError, match="kind"):
        daemons.register("scheduler", argv=["x"], restartable=False)  # type: ignore[arg-type]
    with pytest.raises(ValueError, match="argv"):
        daemons.register("watch", argv=[], restartable=False)
    assert _files() == []


def test_registered_deregisters_however_its_block_ends() -> None:
    with pytest.raises(RuntimeError), daemons.registered("serve", argv=["x"], restartable=False):
        assert len(daemons.list_live()) == 1
        raise RuntimeError("the server failed")
    assert daemons.list_live() == [] and _files() == []


def test_a_registry_that_cannot_be_written_never_stops_the_daemon(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    home = tmp_path / "a-file-not-a-home"
    home.write_text("", encoding="utf-8")
    monkeypatch.setenv("AISQUARE_HOME", str(home))
    told: list[OSError] = []
    ran = False
    with daemons.registered("watch", argv=["x"], restartable=False, on_error=told.append) as mine:
        ran = True
        assert mine is None
    assert ran and len(told) == 1


# --- liveness: the lock decides, not the pid ----------------------------------------------------


def test_a_killed_daemon_is_pruned_by_the_next_reader() -> None:
    script = (
        "import sys, time\n"
        "from aisquare.core import daemons\n"
        "r = daemons.register('serve', argv=['rlx', 'serve'], restartable=True)\n"
        "print(r.daemon.id, flush=True)\n"
        "time.sleep(600)\n"
    )
    child = subprocess.Popen(
        [sys.executable, "-c", script], stdout=subprocess.PIPE, text=True, env=dict(os.environ)
    )
    try:
        assert child.stdout is not None
        daemon_id = child.stdout.readline().strip()
        assert daemons.valid_id(daemon_id), daemon_id
        (daemon,) = daemons.list_live()
        assert (daemon.id, daemon.pid, daemon.kind) == (daemon_id, child.pid, "serve")
    finally:
        child.kill()  # SIGKILL; TerminateProcess on Windows: no finally of its runs
        child.wait(30)
        if child.stdout is not None:
            child.stdout.close()
    assert _files() == [f"{daemon_id}.json", f"{daemon_id}.lock"], "a crash leaves both"
    found = daemons.roll()
    assert found.live == [] and found.pruned == [daemon_id]
    assert _files() == [], "the next reader pruned the record and its lock"


def test_a_recycled_pid_with_a_free_lock_is_not_live(registry: Path) -> None:
    # The pid runs (it is this test's), on this host: only the free lock says the daemon died.
    _write(registry, "0123456789ab", _record(pid=os.getpid()))
    loaded = daemons.REGISTRY.load("0123456789ab", daemons.Daemon.from_record)
    assert isinstance(loaded, daemons.Daemon) and loaded.pid == os.getpid()
    assert daemons.is_live(loaded) is False
    assert daemons.list_live(prune=False) == []
    assert _files() == ["0123456789ab.json", "0123456789ab.lock"], "prune=False writes nothing"
    assert daemons.prune() == ["0123456789ab"]
    assert _files() == []


def test_with_its_lock_held_a_record_is_live_whatever_its_pid(registry: Path) -> None:
    # No process has this pid, and the host is this one: the held lock alone says it runs (a
    # pid namespace, a record a moment from its first beat). Another host's too, stale or not.
    stale = (datetime.now(UTC) - timedelta(days=1)).isoformat()
    _write(registry, "0123456789ab", _record(pid=2**22 + 12345))
    _write(registry, "aaaaaaaaaaaa", _record(host="elsewhere", heartbeat=stale))
    held = [_hold(registry / f"{name}.lock") for name in ("0123456789ab", "aaaaaaaaaaaa")]
    try:
        assert [d.id for d in daemons.list_live()] == ["0123456789ab", "aaaaaaaaaaaa"]
    finally:
        for fd in held:
            os.close(fd)
    assert daemons.list_live() == [] and _files() == []


def test_a_record_whose_lock_file_is_gone_is_not_live(registry: Path) -> None:
    _write(registry, "0123456789ab", _record(), lock=False)
    assert daemons.roll().pruned == ["0123456789ab"] and _files() == []


def test_another_hosts_record_lives_by_its_heartbeat(registry: Path) -> None:
    fresh = datetime.now(UTC)
    stale = fresh - timedelta(seconds=daemons.STALE_SECONDS + 5)
    _write(registry, "aaaaaaaaaaaa", _record(host="elsewhere", heartbeat=fresh.isoformat()))
    _write(registry, "bbbbbbbbbbbb", _record(host="elsewhere", heartbeat=stale.isoformat()))
    found = daemons.roll()
    assert [d.id for d in found.live] == ["aaaaaaaaaaaa"]
    assert found.pruned == ["bbbbbbbbbbbb"]


def test_live_daemons_come_oldest_first_and_a_later_releases_kind_is_kept(
    registry: Path,
) -> None:
    older = (datetime.now(UTC) - timedelta(hours=1)).isoformat()
    later = _record(kind="scheduler", something_new={"a": 1})
    _write(registry, "bbbbbbbbbbbb", later)
    _write(registry, "aaaaaaaaaaaa", _record(since=older, heartbeat="2026-11-25T12:00:00Z"))
    held = [_hold(registry / f"{name}.lock") for name in ("aaaaaaaaaaaa", "bbbbbbbbbbbb")]
    try:
        live = daemons.list_live()
    finally:
        for fd in held:
            os.close(fd)
    assert [(d.id, d.kind) for d in live] == [
        ("aaaaaaaaaaaa", "serve"),
        ("bbbbbbbbbbbb", "scheduler"),
    ]
    assert live[0].heartbeat == datetime(2026, 11, 25, 12, tzinfo=UTC), "Z is read as UTC"


# --- records this release cannot read ----------------------------------------------------------


@pytest.mark.parametrize(
    "body",
    [
        "{not json",
        json.dumps([1, 2]),
        json.dumps(_record(kind=7)),
        json.dumps(_record(pid=True)),
        json.dumps(_record(argv="rlx serve")),
        json.dumps(_record(restartable="yes")),
        json.dumps({k: v for k, v in _record().items() if k != "heartbeat"}),
    ],
    ids=["not-json", "not-an-object", "kind", "pid-bool", "argv-str", "restartable", "no-beat"],
)
def test_an_unreadable_record_is_reported_and_kept_while_its_lock_is_held(
    registry: Path, body: str
) -> None:
    path = _write(registry, "0123456789ab", body)
    _age(path, daemons.STALE_SECONDS * 10)  # old: only the held lock keeps it
    fd = _hold(registry / "0123456789ab.lock")
    try:
        found = daemons.roll()
        assert found.live == [] and found.pruned == []
        (unreadable,) = found.unreadable
        assert unreadable.id == "0123456789ab" and unreadable.held is True
        assert unreadable.path == path and path.exists()
    finally:
        os.close(fd)
    found = daemons.roll()
    assert found.unreadable == [] and found.pruned == ["0123456789ab"]
    assert _files() == []


def test_a_fresh_unreadable_record_is_reported_with_what_it_still_says(registry: Path) -> None:
    path = _write(registry, "0123456789ab", json.dumps(_record(pid=4242, since="soon")))
    found = daemons.roll()
    (unreadable,) = found.unreadable
    assert (unreadable.held, unreadable.pid, unreadable.kind) == (False, 4242, "serve")
    assert path.exists(), "fresh: most likely a later release's daemon"


# --- the folder ----------------------------------------------------------------------------------


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX modes")
def test_the_folder_is_0700_and_its_files_0600() -> None:
    old = os.umask(0o022)  # a usual umask: the modes are the registry's, not the umask's
    try:
        registration = daemons.register("serve", argv=["rlx", "serve"], restartable=True)
    finally:
        os.umask(old)
    try:
        folder = daemons.registry_dir()
        assert stat.S_IMODE(folder.stat().st_mode) == 0o700
        registration.beat()  # a rewritten record keeps the mode
        for name in _files():
            assert stat.S_IMODE((folder / name).stat().st_mode) == 0o600, name
    finally:
        registration.deregister()


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX modes")
def test_a_wider_folder_is_narrowed_as_a_daemon_registers() -> None:
    folder = daemons.registry_dir()
    folder.mkdir(parents=True)
    folder.chmod(0o755)
    with daemons.registered("serve", argv=["x"], restartable=False):
        assert stat.S_IMODE(folder.stat().st_mode) == 0o700


def test_a_reader_makes_no_folder() -> None:
    assert daemons.roll() == daemons.Roll([], [], [])
    assert not daemons.registry_dir().exists()


@pytest.mark.parametrize(
    "name", ["NOTANID.json", "abc.json", "0123456789AB.json", "0123456789abc.json", "x y.json"]
)
def test_an_id_that_is_not_a_valid_file_name_is_ignored(registry: Path, name: str) -> None:
    path = registry / name
    path.write_text(json.dumps(_record()), encoding="utf-8")
    stem = name.removesuffix(".json")
    lock = registry / f"{stem}.lock"
    fd = _hold(lock)
    try:
        _age(path, daemons.STALE_SECONDS * 10)
        found = daemons.roll()
        assert found == daemons.Roll([], [], []), "no record: its name is no id"
        assert path.exists(), "and a name this release gives no meaning is left alone"
    finally:
        os.close(fd)
    assert not daemons.valid_id(stem)
    with pytest.raises(ValueError, match="not an rlx daemons id"):
        daemons.REGISTRY.record(stem)


def test_the_id_is_the_file_names_never_the_contents(registry: Path) -> None:
    _write(registry, "0123456789ab", _record(id="../../evil"))
    fd = _hold(registry / "0123456789ab.lock")
    try:
        (daemon,) = daemons.list_live()
    finally:
        os.close(fd)
    assert daemon.id == "0123456789ab"


def test_a_gone_records_leftovers_are_deleted_once_old_and_nothing_else(registry: Path) -> None:
    leftovers = ["0123456789ab.lock", ".0123456789ab.json.42.deadbeef.tmp"]
    kept = ["README", ".later-release.lock", "fedcba987654.lock"]
    for name in [*leftovers, *kept]:
        (registry / name).touch()
        _age(registry / name, daemons.STALE_SECONDS * 2)
    (registry / "abcdefabcdef.lock").touch()  # fresh: maybe a daemon between lock and record
    fd = _hold(registry / "fedcba987654.lock")  # held: a running daemon's, record or not
    try:
        daemons.roll()
    finally:
        os.close(fd)
    assert _files() == sorted([*kept, "abcdefabcdef.lock"])


# --- the daemons of 4.1a ---------------------------------------------------------------------


def _stand_in_tmux(monkeypatch: pytest.MonkeyPatch) -> dict[str, Any]:
    """``fleet.open_window``, recorded instead of opened: the window ``detach`` asks for."""
    opened: dict[str, Any] = {}

    def open_window(srv: object, session: str, **kwargs: Any) -> None:
        opened.update(session=session, **kwargs)

    monkeypatch.setattr(fleet_service, "settings", lambda: None)
    monkeypatch.setattr(fleet_service, "server", lambda config: None)
    monkeypatch.setattr(fleet_service, "open_window", open_window)
    monkeypatch.setattr(watch_service, "_sleep", lambda seconds: None)
    return opened


def _run_watcher_once(monkeypatch: pytest.MonkeyPatch, *, interval: float | None) -> list[Any]:
    """``watch_service.run`` for one tick, which reads the registry and sends the watcher the
    SIGTERM ``watch --stop`` sends (its handler, called: a real one would end the suite on
    Windows); what the tick found."""
    seen: list[Any] = []

    def tick(self: watch_service.Watcher, now: datetime | None = None) -> Any:
        seen.append(daemons.list_live())
        stopping = signal.getsignal(signal.SIGTERM)
        assert callable(stopping)
        stopping(signal.SIGTERM, None)
        return None

    monkeypatch.setattr(watch_service.Watcher, "tick", tick)
    monkeypatch.setattr(watch_service, "_sleep", lambda seconds: None)
    watch_service.run(interval=interval, on_tick=lambda report: None)
    return seen


def test_watch_detachs_watcher_registers_as_watch_detach_and_leaves_on_exit(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    opened = _stand_in_tmux(monkeypatch)
    assert watch_service.detach() == -1, "the stand-in window runs nothing"
    assert opened["session"] == watch_service.DETACHED_SESSION
    assert opened["command"] == selfcli.argv_for(["watch"])
    # The window's process, in this one: its command with the window's environment.
    for name, value in opened["env"].items():
        monkeypatch.setenv(name, value)
    (seen,) = _run_watcher_once(monkeypatch, interval=None)
    (daemon,) = seen
    assert daemon.kind == "watch" and daemon.pid == os.getpid()
    assert list(daemon.argv) == selfcli.argv_for(["watch", "--detach"])
    assert daemon.restartable is True, "watch --detach opens its window again"
    assert daemons.list_live() == [] and _files() == [], "deregistered on exit"
    assert watch_service.running_pid() is None


def test_a_foreground_watcher_registers_as_its_terminal_ran_it_and_not_restartable(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv(watch_service.DETACHED_ENV, raising=False)
    (seen,) = _run_watcher_once(monkeypatch, interval=5.0)
    (daemon,) = seen
    assert daemon.kind == "watch"
    assert list(daemon.argv) == selfcli.argv_for(["watch", "--interval", "5.0"])
    assert daemon.restartable is False
    assert _files() == []


def test_a_watcher_that_ends_on_an_error_deregisters_too(monkeypatch: pytest.MonkeyPatch) -> None:
    class Stop(BaseException):
        pass

    def tick(self: watch_service.Watcher, now: datetime | None = None) -> Any:
        assert [d.kind for d in daemons.list_live()] == ["watch"]
        raise Stop

    monkeypatch.setattr(watch_service.Watcher, "tick", tick)
    with pytest.raises(Stop):
        watch_service.run(interval=1, on_tick=lambda report: None)
    assert _files() == []


@pytest.fixture
def stand_in_serve(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> list[Any]:
    """``serve`` with its dependency there and its transports and project stand-ins: each
    transport records what the registry holds while it serves, and returns."""
    seen: list[Any] = []
    project = SimpleNamespace(root=tmp_path / "repo", id="prj_test")
    monkeypatch.setattr(serve_cli, "_dependency_error", lambda: None)
    monkeypatch.setattr(team_service, "activate", lambda cwd=None: project)
    monkeypatch.setattr(orchestrator, "team_project", lambda cwd=None: project)
    monkeypatch.setattr(workspace, "marks_project_root", lambda root: True)

    def run_http(*, bind: str, port: int) -> None:
        seen.append(("http", daemons.list_live()))

    def run_stdio(*, close_after: int = 300, **_: Any) -> None:
        seen.append(("stdio", daemons.list_live()))

    monkeypatch.setattr(mcp_server, "run_http", run_http)
    monkeypatch.setattr(mcp_server, "run_stdio", run_stdio)
    return seen


def test_serve_over_http_registers_as_serve_with_its_options_and_leaves_on_exit(
    stand_in_serve: list[Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("AISQUARE_SERVE_PORT", "9555")  # an option from the environment
    result = CliRunner().invoke(app, ["serve", "--bind", "::1"])
    assert result.exit_code == 0, result.output
    ((transport, live),) = stand_in_serve
    (daemon,) = live
    assert transport == "http" and daemon.kind == "serve" and daemon.pid == os.getpid()
    assert list(daemon.argv) == selfcli.argv_for(["serve", "--port", "9555", "--bind", "::1"])
    assert daemon.restartable is True
    assert daemons.list_live() == [] and _files() == [], "deregistered on exit"


def test_a_sigterm_to_serve_over_http_leaves_the_registry_with_its_exit_status(
    stand_in_serve: list[Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    # uvicorn hands a SIGTERM back to the handler it found once it has shut down: called
    # here as it would be (a real one would end the suite on Windows).
    def run_http(*, bind: str, port: int) -> None:
        stand_in_serve.append(("http", daemons.list_live()))
        handler = signal.getsignal(signal.SIGTERM)
        assert callable(handler)
        handler(signal.SIGTERM, None)

    monkeypatch.setattr(mcp_server, "run_http", run_http)
    before = signal.getsignal(signal.SIGTERM)
    result = CliRunner().invoke(app, ["serve"])
    assert result.exit_code == 128 + signal.SIGTERM
    ((_, live),) = stand_in_serve
    assert [d.kind for d in live] == ["serve"]
    assert _files() == [], "deregistered on the way out"
    assert signal.getsignal(signal.SIGTERM) == before, "the handler it found is given back"


@pytest.mark.skipif(sys.platform == "win32", reason="a SIGTERM is TerminateProcess there")
def test_a_real_serve_over_http_registers_and_a_sigterm_deregisters_it(tmp_path: Path) -> None:
    pytest.importorskip("mcp", reason="the [serve] extra is not installed")
    pytest.importorskip("uvicorn", reason="the [serve] extra is not installed")
    repo = tmp_path / "repo"
    (repo / ".git").mkdir(parents=True)
    server = subprocess.Popen(
        [*selfcli.argv_for(["serve", "--port", "0"])],
        cwd=repo,
        env=dict(os.environ),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    try:
        deadline = time.monotonic() + 120
        live: list[daemons.Daemon] = []
        while not live and server.poll() is None and time.monotonic() < deadline:
            live = daemons.list_live(prune=False)
            time.sleep(0.1)
        assert [(d.kind, d.pid, d.restartable) for d in live] == [("serve", server.pid, True)], (
            server.stderr.read() if server.poll() is not None and server.stderr else b""
        )
        assert live[0].cwd == str(repo.resolve()) or live[0].cwd == str(repo)
        server.send_signal(signal.SIGTERM)
        assert server.wait(60) == 128 + signal.SIGTERM
    finally:
        if server.poll() is None:
            server.kill()
            server.wait(30)
        if server.stderr is not None:
            server.stderr.close()
    assert _files() == [], "a stopped server leaves no record for a reader to prune"


def test_serve_over_stdio_registers_and_is_not_restartable(stand_in_serve: list[Any]) -> None:
    result = CliRunner().invoke(app, ["serve", "--stdio", "--close-after", "0"])
    assert result.exit_code == 0, result.output
    ((transport, live),) = stand_in_serve
    (daemon,) = live
    assert transport == "stdio" and daemon.kind == "serve"
    assert list(daemon.argv) == selfcli.argv_for(["serve", "--stdio", "--close-after", "0"])
    assert daemon.restartable is False, "its client holds its stdin and stdout"
    assert _files() == []


def test_serve_show_token_registers_nothing(
    stand_in_serve: list[Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(mcp_server, "serve_token", lambda: "t0ken")
    result = CliRunner().invoke(app, ["serve", "--show-token"])
    assert result.exit_code == 0, result.output
    assert stand_in_serve == [] and not daemons.registry_dir().exists()
