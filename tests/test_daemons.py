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

import errno
import json
import os
import re
import signal
import stat
import subprocess
import sys
import time
from collections.abc import Callable, Iterator
from datetime import UTC, datetime, timedelta
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import jsonschema
import pytest
from typer.testing import CliRunner

from aisquare.cli import serve as serve_cli
from aisquare.cli.app import app
from aisquare.core import daemons, locking, orchestrator, selfcli, workspace
from aisquare.core.registry import GATE_LOCK, Registry, Unloaded, host_name
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
    """``watch_service.run`` takes SIGTERM and SIGHUP for its loop and never gives them back:
    given back here, so the suite's own signals still end it."""
    names = ["SIGTERM"] if sys.platform == "win32" else ["SIGTERM", "SIGHUP"]
    before = {name: signal.getsignal(getattr(signal, name)) for name in names}
    yield
    for name, handler in before.items():
        signal.signal(getattr(signal, name), handler)


REPO = Path(__file__).resolve().parents[1]
SCHEMA_PATH = REPO / "rlxd" / "schemas" / "v1" / "daemon-record.json"
RECORD_SCHEMA: dict[str, Any] = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
"""The record's published JSON Schema (handoff §4.1), generated from rlxd-proto (#575): what
every record Python writes is checked against, as rlxd's are."""

_DATE_TIME = re.compile(
    r"([0-9]{4})-([0-9]{2})-([0-9]{2})[Tt]([0-9]{2}):([0-9]{2}):([0-9]{2})(?:\.[0-9]+)?"
    r"(?:[Zz]|[+-]([0-9]{2}):([0-9]{2}))"
)
FORMATS = jsonschema.FormatChecker()
"""jsonschema's own ``date-time`` check needs ``rfc3339-validator``, which the dev environment
does not install, and passes anything without it: this one is rlxd-proto's rule
(``rlxd/crates/rlxd-proto/src/check.rs``, ``is_date_time``), the one rlxd reads by."""


@FORMATS.checks("date-time", raises=ValueError)
def _date_time(value: object) -> bool:
    """rlxd-proto's ``is_date_time``: ``YYYY-MM-DD``, ``T`` or ``t``, ``HH:MM:SS``, a fraction of
    one digit or more, then ``Z``, ``z`` or ``±HH:MM``; any year (``0000`` too, leap by the
    Gregorian rule), a day its month has, hours to 23, minutes and seconds to 59, the offset's
    hours to 23 and minutes to 59. ASCII digits only."""
    found = _DATE_TIME.fullmatch(value) if isinstance(value, str) else None
    if found is None:
        raise ValueError(f"not an RFC 3339 date-time: {value!r}")
    year, month, day, hour, minute, second = (int(found.group(n)) for n in range(1, 7))
    leap = year % 4 == 0 and (year % 100 != 0 or year % 400 == 0)
    days = {1: 31, 3: 31, 5: 31, 7: 31, 8: 31, 10: 31, 12: 31, 4: 30, 6: 30, 9: 30, 11: 30}
    days[2] = 29 if leap else 28
    if month not in days or not 1 <= day <= days[month]:
        raise ValueError(f"no such day: {value!r}")
    if hour > 23 or minute > 59 or second > 59:
        raise ValueError(f"no such time: {value!r}")
    if found.group(7) is not None and (int(found.group(7)) > 23 or int(found.group(8)) > 59):
        raise ValueError(f"no such offset: {value!r}")
    return True


def _conforms(record: object) -> None:
    """``record`` validates against :data:`RECORD_SCHEMA`, formats included."""
    jsonschema.validate(record, RECORD_SCHEMA, format_checker=FORMATS)


def _records_on_disk() -> list[dict[str, Any]]:
    folder = daemons.registry_dir()
    return [json.loads(path.read_text(encoding="utf-8")) for path in sorted(folder.glob("*.json"))]


def _files() -> list[str]:
    """The folder's files but its gate, which stays (``core.GATE_LOCK``)."""
    try:
        names = os.listdir(daemons.registry_dir())
    except FileNotFoundError:
        return []
    return sorted(name for name in names if name != GATE_LOCK)


def _record(extra: dict[str, Any] | None = None, **overrides: Any) -> dict[str, Any]:
    """A record by the module's list of keys; its ``id`` is the file's :func:`_write` gives it,
    unless ``overrides`` says one. ``overrides`` takes only the record's own keys, so a typo
    (``heartbeat=`` for ``heartbeat_at=``) fails here rather than passing silently; a key
    this release does not know goes in ``extra``."""
    unknown = set(overrides) - set(RECORD_SCHEMA["properties"])
    assert not unknown, f"not a record key: {sorted(unknown)} (a later release's: extra=)"
    now = datetime.now(UTC).isoformat()
    body: dict[str, Any] = {
        "kind": "serve",
        "pid": os.getpid(),
        "host": host_name(),
        "version": "0.10.3",
        "started_at": now,
        "heartbeat_at": now,
        "argv": ["rlx", "serve"],
        "restartable": True,
        "cwd": "/somewhere",
    }
    body.update(overrides)
    body.update(extra or {})
    return body


def _write(folder: Path, daemon_id: str, body: object, *, lock: bool = True) -> Path:
    path = folder / f"{daemon_id}.json"
    if isinstance(body, dict):
        body = json.dumps({"id": daemon_id, **body})
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
        # The record on disk is the module's list of keys (handoff §4.1), its id the file's name.
        on_disk = json.loads((daemons.registry_dir() / f"{daemon.id}.json").read_text("utf-8"))
        assert set(on_disk) == {*RECORD_SCHEMA["required"], "cwd"}
        _conforms(on_disk)
        assert on_disk["id"] == daemon.id and on_disk["argv"] == ARGV
        assert on_disk["pid"] == os.getpid()
        assert datetime.fromisoformat(on_disk["started_at"]).utcoffset() == timedelta(0)
        assert daemons.list_live(kind="serve") == [] and daemons.list_live(kind="watch")
    finally:
        registration.deregister()
    assert daemons.list_live() == []
    assert _files() == [], "the record and its lock leave with the daemon"
    registration.deregister()  # once: a second call is nothing


def test_the_schema_check_can_fail() -> None:
    """The checks below are only as good as this one: a record off the schema is refused."""
    registration = daemons.register("serve", argv=["rlx", "serve"], restartable=True)
    try:
        (good,) = _records_on_disk()
    finally:
        registration.deregister()
    _conforms(good)
    for bad in (
        {**good, "pid": 0},
        {**good, "id": "NOT-AN-ID"},
        {**good, "argv": []},
        {**good, "started_at": "yesterday"},
        {**good, "heartbeat_at": "2026-13-45T25:61:00Z"},
        {k: v for k, v in good.items() if k != "restartable"},
    ):
        with pytest.raises(jsonschema.ValidationError):
            _conforms(bad)


@pytest.mark.parametrize(
    ("value", "takes"),
    [
        ("2026-11-25T12:03:00.123456+00:00", True),  # what Python writes
        ("2026-11-25T12:03:00Z", True),
        ("2026-11-25t12:03:00z", True),
        ("2026-11-25T12:03:00.123456789-05:30", True),
        ("2024-02-29T00:00:00Z", True),
        ("0000-02-29T00:00:00Z", True),  # year 0, a leap year, as check.rs reads it
        ("2026-11-25T12:03:00+23:59", True),
        ("2026-11-25T12:03:00+00:60", False),
        ("2026-11-25T12:03:00+05:99", False),
        ("2026-11-25T12:03:00+24:00", False),
        ("2026-11-25T24:00:00Z", False),
        ("2026-11-25T23:60:00Z", False),
        ("2026-11-25T23:59:60Z", False),
        ("2025-02-29T00:00:00Z", False),
        ("1900-02-29T00:00:00Z", False),
        ("2026-04-31T00:00:00Z", False),
        ("2026-13-01T00:00:00Z", False),
        ("2026-11-25T12:03:00.Z", False),
        ("2026-11-25T12:03:00", False),
        ("2026-11-25 12:03:00Z", False),
        ("2026-11-25T12:03:00+0000", False),
        ("\u0662026-11-25T12:03:00Z", False),  # a non-ASCII digit
    ],
)
def test_the_schema_checks_times_by_rlxd_protos_rule(value: str, takes: bool) -> None:
    record = {**_record(), "id": "0123456789ab", "started_at": value}
    if takes:
        _conforms(record)
    else:
        with pytest.raises(jsonschema.ValidationError):
            _conforms(record)


@pytest.mark.parametrize("kind", daemons.KINDS)
def test_every_kind_python_registers_writes_a_record_rlxds_schema_takes(kind: daemons.Kind) -> None:
    registration = daemons.register(kind, argv=selfcli.argv_for([kind]), restartable=False)
    try:
        registration.beat()  # a rewritten record too
        (record,) = _records_on_disk()
        _conforms(record)
        assert record["kind"] == kind
    finally:
        registration.deregister()


def test_the_daemons_of_4_1a_write_records_rlxds_schema_takes(
    monkeypatch: pytest.MonkeyPatch, stand_in_serve: list[Any]
) -> None:
    """``watch --detach``'s watcher, a foreground watcher and ``serve`` over HTTP and stdio, each
    as it registers in production; every record read off the disk while it runs."""
    written: list[dict[str, Any]] = []

    def watcher() -> None:
        written.extend(_records_on_disk())
        stopping = signal.getsignal(signal.SIGTERM)
        assert callable(stopping)
        stopping(signal.SIGTERM, None)

    monkeypatch.setattr(watch_service, "_sleep", lambda seconds: None)
    for detached in (True, False):
        if detached:
            monkeypatch.setenv(watch_service.DETACHED_ENV, "1")
        else:
            monkeypatch.delenv(watch_service.DETACHED_ENV, raising=False)
        monkeypatch.setattr(watch_service.Watcher, "tick", _one_tick(watcher))
        watch_service.run(interval=5.0, on_tick=lambda report: None)

    def transport(**_: Any) -> None:
        written.extend(_records_on_disk())

    monkeypatch.setattr(mcp_server, "run_http", transport)
    monkeypatch.setattr(mcp_server, "run_stdio", transport)
    for argv in (["serve", "--port", "9555"], ["serve", "--stdio", "--close-after", "0"]):
        result = CliRunner().invoke(app, argv)
        assert result.exit_code == 0, result.output
    assert [(r["kind"], r["restartable"]) for r in written] == [
        ("watch", True),
        ("watch", False),
        ("serve", True),
        ("serve", False),
    ]
    for record in written:
        _conforms(record)
    assert _files() == []


def test_the_heartbeat_rewrites_the_record() -> None:
    registration = daemons.register("serve", argv=["rlx", "serve"], restartable=True)
    try:
        before = registration.daemon.heartbeat_at
        time.sleep(0.01)
        registration.beat()
        (daemon,) = daemons.list_live()
        assert daemon.heartbeat_at > before
        assert daemon.started_at == registration.daemon.started_at
        assert daemons.HEARTBEAT_SECONDS == 5.0, "handoff §4.1: a heartbeat every 5 s"
    finally:
        registration.deregister()


def test_the_heartbeat_thread_beats_on_its_own_and_ends_with_the_registration(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(daemons, "HEARTBEAT_SECONDS", 0.05)
    writes: list[str] = []
    real = daemons.REGISTRY.write

    def write(record_id: str, record: Any) -> None:
        writes.append(record["heartbeat_at"])
        real(record_id, record)

    monkeypatch.setattr(daemons.REGISTRY, "write", write)
    registration = daemons.register("serve", argv=["rlx", "serve"], restartable=True)
    try:
        assert registration.beating, "register starts the thread"
        deadline = time.monotonic() + 30
        while len(writes) < 3 and time.monotonic() < deadline:  # the record, then 2 beats
            time.sleep(0.01)
        assert len(writes) >= 3 and len(set(writes)) == len(writes), writes
        (daemon,) = daemons.list_live()
        assert daemon.heartbeat_at > registration.daemon.started_at
    finally:
        registration.deregister()
    assert not registration.beating, "deregister stops it"
    after = len(writes)
    time.sleep(0.2)  # four intervals
    assert len(writes) == after and _files() == [], "and nothing is written after it leaves"


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
    _write(registry, "aaaaaaaaaaaa", _record(host="elsewhere", heartbeat_at=stale))
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


def test_a_free_lock_is_a_dead_daemon_whatever_its_host_or_heartbeat(registry: Path) -> None:
    fresh = datetime.now(UTC).isoformat()
    stale = (datetime.now(UTC) - timedelta(seconds=daemons.STALE_SECONDS + 5)).isoformat()
    _write(registry, "aaaaaaaaaaaa", _record(host="elsewhere", heartbeat_at=fresh))
    _write(registry, "bbbbbbbbbbbb", _record(heartbeat_at=fresh))
    _write(registry, "cccccccccccc", _record(host="elsewhere", heartbeat_at=stale))
    found = daemons.roll()
    assert found.live == [], "a free lock is a dead daemon, whatever host or heartbeat"
    assert found.pruned == ["bbbbbbbbbbbb", "cccccccccccc"]


def test_another_hosts_fresh_record_and_its_lock_file_are_never_deleted_here(
    registry: Path,
) -> None:
    # Its host's held lock may not show on this one (locks that do not cross machines): this
    # reader neither counts it nor deletes it, its lock file least of all, until it is stale.
    fresh = datetime.now(UTC).isoformat()
    _write(registry, "aaaaaaaaaaaa", _record(host="elsewhere", heartbeat_at=fresh))
    for _ in range(2):
        found = daemons.roll()
        assert found.live == [] and found.pruned == []
        assert _files() == ["aaaaaaaaaaaa.json", "aaaaaaaaaaaa.lock"]
    stale = (datetime.now(UTC) - timedelta(seconds=daemons.STALE_SECONDS + 5)).isoformat()
    _write(registry, "aaaaaaaaaaaa", _record(host="elsewhere", heartbeat_at=stale))
    assert daemons.prune() == ["aaaaaaaaaaaa"] and _files() == []


def test_only_a_lock_that_cannot_be_probed_lets_the_record_speak(
    registry: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fresh = datetime.now(UTC)
    stale = (fresh - timedelta(seconds=daemons.STALE_SECONDS + 5)).isoformat()
    _write(registry, "aaaaaaaaaaaa", _record(host="elsewhere", heartbeat_at=fresh.isoformat()))
    _write(registry, "bbbbbbbbbbbb", _record(host="elsewhere", heartbeat_at=stale))
    _write(registry, "cccccccccccc", _record(pid=os.getpid()))  # this host, its pid runs
    monkeypatch.setattr(daemons.REGISTRY, "lock_held", lambda record_id, missing=False: None)
    assert [d.id for d in daemons.list_live(prune=False)] == ["aaaaaaaaaaaa", "cccccccccccc"]


def test_live_daemons_come_oldest_first_and_a_later_releases_kind_is_kept(
    registry: Path,
) -> None:
    older = (datetime.now(UTC) - timedelta(hours=1)).isoformat()
    later = _record(kind="scheduler", extra={"something_new": {"a": 1}})
    _write(registry, "bbbbbbbbbbbb", later)
    _write(registry, "aaaaaaaaaaaa", _record(started_at=older, heartbeat_at="2026-11-25T12:00:00Z"))
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
    assert live[0].heartbeat_at == datetime(2026, 11, 25, 12, tzinfo=UTC), "Z is read as UTC"


@pytest.mark.skipif(sys.platform == "win32", reason="Windows deletes no file its daemon holds")
@pytest.mark.parametrize("how", ["deleted", "replaced"])
def test_a_daemon_whose_lock_file_was_taken_away_takes_it_again_at_its_next_beat(
    how: str,
) -> None:
    registration = daemons.register("serve", argv=["rlx", "serve"], restartable=True)
    try:
        daemon_id = registration.daemon.id
        lock = daemons.REGISTRY.lock(daemon_id)
        daemons.REGISTRY.forget(daemon_id)  # another reader's prune, or a person's rm
        if how == "replaced":
            lock.touch()  # a new, free file of the same name
        found = daemons.roll()
        assert found.live == [], "unseen while its lock is not the one it holds"
        registration.beat()
        assert daemons.REGISTRY.lock_held(daemon_id) is True
        assert [d.id for d in daemons.roll().live] == [daemon_id]
        assert _files() == [f"{daemon_id}.json", f"{daemon_id}.lock"]
    finally:
        registration.deregister()
    assert _files() == []


def test_a_pid_past_a_c_int_is_unreadable_and_never_stops_a_daemon(
    registry: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _write(registry, "0123456789ab", _record(pid=2**31))
    _write(registry, "aaaaaaaaaaaa", _record(pid=daemons.PID_MAX))
    # Where the lock cannot be told, the record's pid is asked: os.kill(2**31, 0) overflows.
    monkeypatch.setattr(daemons.REGISTRY, "lock_held", lambda record_id, missing=False: None)
    found = daemons.roll(prune=False)
    assert [(u.id, u.pid) for u in found.unreadable] == [("0123456789ab", None)]
    assert all(d.id != "0123456789ab" for d in found.live)
    with daemons.registered("watch", argv=["x"], restartable=False) as mine:
        assert mine is not None, "a daemon starts beside it"


@pytest.mark.parametrize(
    ("said", "read"),
    [
        ("2026-11-25T12:03:00Z", datetime(2026, 11, 25, 12, 3, tzinfo=UTC)),
        ("2026-11-25T12:03:00.123456789Z", datetime(2026, 11, 25, 12, 3, 0, 123456, tzinfo=UTC)),
        ("2026-11-25T07:03:00-05:00", datetime(2026, 11, 25, 12, 3, tzinfo=UTC)),
        ("2026-11-25t12:03:00Z", datetime(2026, 11, 25, 12, 3, tzinfo=UTC)),
        ("2026-11-25t12:03:00z", None),
        ("2026-11-25T12:03:00z", None),
    ],
)
def test_times_are_read_as_the_contract_writes_them(
    registry: Path, said: str, read: datetime | None
) -> None:
    _write(registry, "0123456789ab", _record(started_at=said, heartbeat_at=said))
    loaded = daemons.REGISTRY.load("0123456789ab", daemons.Daemon.from_record)
    if read is None:
        assert loaded is Unloaded.UNPARSABLE, "a lower-case z: unreadable"
    else:
        assert isinstance(loaded, daemons.Daemon) and loaded.started_at == read


def test_an_unreadable_records_pid_outside_a_pids_range_is_no_pid(registry: Path) -> None:
    # The records an upgrade's count reaches (held, unreadable): a stop by any of these pids
    # would be kill(-1), kill(0) or an OverflowError.
    says = {"aaaaaaaaaaaa": -1, "bbbbbbbbbbbb": 0, "cccccccccccc": 2**31, "dddddddddddd": True}
    says["eeeeeeeeeeee"] = 4242
    for daemon_id, pid in says.items():
        _write(registry, daemon_id, json.dumps({"id": daemon_id, "pid": pid}))
    held = [_hold(registry / f"{daemon_id}.lock") for daemon_id in says]
    try:
        found = daemons.roll()
    finally:
        for fd in held:
            os.close(fd)
    assert [(u.id, u.held, u.pid) for u in found.unreadable] == [
        ("aaaaaaaaaaaa", True, None),
        ("bbbbbbbbbbbb", True, None),
        ("cccccccccccc", True, None),
        ("dddddddddddd", True, None),
        ("eeeeeeeeeeee", True, 4242),
    ]


@pytest.mark.parametrize(
    ("ahead", "kept"),
    [(50.0, True), (daemons.CLOCK_SKEW_SECONDS + 5, False)],
    ids=["within-the-skew-bound", "past-it"],
)
def test_a_heartbeat_from_the_future_is_fresh_only_within_the_clock_skew_bound(
    registry: Path, ahead: float, kept: bool
) -> None:
    future = (datetime.now(UTC) + timedelta(seconds=ahead)).isoformat()
    _write(registry, "aaaaaaaaaaaa", _record(host="elsewhere", heartbeat_at=future))
    found = daemons.roll()
    assert found.live == []
    assert found.pruned == ([] if kept else ["aaaaaaaaaaaa"])
    assert (_files() == ["aaaaaaaaaaaa.json", "aaaaaaaaaaaa.lock"]) is kept


def test_a_future_heartbeat_past_the_bound_is_not_live_where_the_lock_cannot_be_told(
    registry: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    future = datetime.now(UTC) + timedelta(seconds=daemons.CLOCK_SKEW_SECONDS + 5)
    _write(registry, "aaaaaaaaaaaa", _record(host="elsewhere", heartbeat_at=future.isoformat()))
    monkeypatch.setattr(daemons.REGISTRY, "lock_held", lambda record_id, missing=False: None)
    assert daemons.list_live(prune=False) == []


@pytest.mark.skipif(sys.platform == "win32", reason="Windows deletes no file its daemon holds")
def test_a_prune_never_deletes_a_lock_a_beat_took_again_since_the_read(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    registration = daemons.register("serve", argv=["rlx", "serve"], restartable=True)
    daemon_id = registration.daemon.id
    daemons.REGISTRY.lock(daemon_id).unlink()  # lost: the read below takes it for dead
    real_gate = daemons.REGISTRY.gate
    fired: list[bool] = []

    def gate(**kwargs: Any) -> Any:
        if not fired:  # the beat lands between the reader's read and its deletion
            fired.append(True)
            registration.beat()  # takes the lock again, under the gate itself
        return real_gate(**kwargs)

    monkeypatch.setattr(daemons.REGISTRY, "gate", gate)
    try:
        found = daemons.roll()
        assert fired and found.pruned == [], "the second probe, under the gate, found it held"
        assert daemons.REGISTRY.lock_held(daemon_id) is True
        assert [d.id for d in daemons.list_live()] == [daemon_id]
    finally:
        registration.deregister()


@pytest.mark.skipif(sys.platform == "win32", reason="Windows deletes no file its daemon holds")
def test_a_beat_takes_its_lost_lock_again_only_under_the_gate(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    registration = daemons.register("serve", argv=["rlx", "serve"], restartable=True)
    daemon_id = registration.daemon.id
    try:
        daemons.REGISTRY.lock(daemon_id).unlink()
        fd = _hold(daemons.registry_dir() / GATE_LOCK)  # a pruning reader mid-deletion
        monkeypatch.setattr(daemons.REGISTRY, "gate", _gate_with(0.05))
        try:
            registration.beat()
            assert not daemons.REGISTRY.lock(daemon_id).exists(), "waits for the gate"
        finally:
            os.close(fd)
        registration.beat()  # the gate free: taken again
        assert daemons.REGISTRY.lock_held(daemon_id) is True
    finally:
        registration.deregister()


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX modes")
@pytest.mark.skipif(sys.platform != "win32" and os.geteuid() == 0, reason="root opens mode 000")
def test_a_dead_record_whose_lock_cannot_be_probed_is_pruned_once(
    registry: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # This host's record of a pid that is gone, its lock file one this user cannot open (left
    # by a `sudo rlx …`): the read judges it dead by its pid, and the prune keeps that verdict.
    ended = subprocess.Popen([sys.executable, "-c", "pass"])
    ended.wait(30)
    _write(registry, "0123456789ab", _record(pid=ended.pid))
    (registry / "0123456789ab.lock").chmod(0o000)
    assert daemons.REGISTRY.lock_held("0123456789ab") is None, "cannot be told"
    real_gate, gates = daemons.REGISTRY.gate, []

    def gate(**kwargs: Any) -> Any:
        gates.append(True)
        return real_gate(**kwargs)

    monkeypatch.setattr(daemons.REGISTRY, "gate", gate)
    reads = [daemons.roll() for _ in range(3)]
    assert [r.pruned for r in reads] == [["0123456789ab"], [], []]
    assert len(gates) == 1, "only the read that prunes takes the gate"
    assert _files() == []


def test_a_reader_that_cannot_have_the_gate_prunes_nothing_that_read(
    registry: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _write(registry, "0123456789ab", _record())  # dead: its lock free
    fd = _hold(registry / GATE_LOCK)  # a registration in progress, held long
    monkeypatch.setattr(daemons.REGISTRY, "gate", _gate_with(0.05))
    try:
        found = daemons.roll()
        assert found.pruned == [] and _files() == ["0123456789ab.json", "0123456789ab.lock"]
    finally:
        os.close(fd)
    assert daemons.prune() == ["0123456789ab"], "a later read prunes it"


@pytest.mark.skipif(sys.platform == "win32", reason="Windows deletes no file its daemon holds")
def test_a_beat_whose_lock_descriptor_went_stale_takes_the_lock_again() -> None:
    # NFSv3: once another client deleted the file this one holds, fstat on it fails ESTALE.
    registration = daemons.register("serve", argv=["rlx", "serve"], restartable=True)
    daemon_id = registration.daemon.id
    try:
        daemons.REGISTRY.forget(daemon_id)
        stale_fd = registration._lock_fd
        real_fstat = os.fstat

        def fstat(fd: int) -> os.stat_result:
            if fd == stale_fd:
                raise OSError(errno.ESTALE, "Stale NFS file handle")
            return real_fstat(fd)

        with pytest.MonkeyPatch.context() as stale:  # scoped: never undoes the test's home
            stale.setattr(os, "fstat", fstat)
            registration.beat()
        assert daemons.REGISTRY.lock_held(daemon_id) is True
        assert [d.id for d in daemons.roll().live] == [daemon_id]
    finally:
        registration.deregister()
    assert _files() == []


# --- records this release cannot read ----------------------------------------------------------


@pytest.mark.parametrize(
    "body",
    [
        "{not json",
        json.dumps([1, 2]),
        # Each dict differs from a readable record by its one defect: _write gives it its id.
        _record(kind=7),
        _record(kind=""),
        _record(pid=True),
        _record(argv="rlx serve"),
        _record(argv=[]),
        _record(argv=["rlx", 7]),
        _record(restartable="yes"),
        _record(host=7),
        _record(version=None),
        _record(cwd=7),
        {k: v for k, v in _record().items() if k != "heartbeat_at"},
        {k: v for k, v in _record().items() if k != "started_at"},
        json.dumps(_record()),  # a string, so no id is added: no id
        _record(id="fedcba987654"),  # another record's id
    ],
    ids=[
        "not-json",
        "not-an-object",
        "kind-number",
        "kind-empty",
        "pid-bool",
        "argv-str",
        "argv-empty",
        "argv-not-all-str",
        "restartable",
        "host",
        "version",
        "cwd",
        "no-beat",
        "no-start",
        "no-id",
        "another-id",
    ],
)
def test_an_unreadable_record_is_reported_and_kept_while_its_lock_is_held(
    registry: Path, body: object
) -> None:
    good = {"id": "0123456789ab", **_record()}
    assert daemons.Daemon.from_record("0123456789ab", good) is not None, "the one defect decides"
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
    path = _write(registry, "0123456789ab", json.dumps(_record(pid=4242, started_at="soon")))
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


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX modes")
def test_the_gate_stays_0600_and_no_reader_deletes_it() -> None:
    with daemons.registered("serve", argv=["x"], restartable=False):
        pass
    gate = daemons.registry_dir() / GATE_LOCK
    assert stat.S_IMODE(gate.stat().st_mode) == 0o600
    _age(gate, daemons.STALE_SECONDS * 10)
    daemons.prune()
    assert gate.exists()


def test_a_daemon_registers_only_through_the_gate(monkeypatch: pytest.MonkeyPatch) -> None:
    folder = daemons.REGISTRY.private_folder()
    fd = _hold(folder / GATE_LOCK)  # another daemon reading and writing, held long
    monkeypatch.setattr(daemons.REGISTRY, "gate", _gate_with(0.05))
    told: list[OSError] = []
    try:
        with pytest.raises(TimeoutError):
            daemons.register("watch", argv=["x"], restartable=False)
        with daemons.registered("watch", argv=["x"], restartable=False, on_error=told.append):
            pass
    finally:
        os.close(fd)
    assert len(told) == 1 and isinstance(told[0], TimeoutError)
    assert _files() == [], "refused at the gate: no lock, no record"


def _gate_with(wait_s: float) -> Any:
    real = Registry.gate

    def gate(**_: Any) -> Any:
        return real(daemons.REGISTRY, wait_s=wait_s)

    return gate


def _gate_held() -> bool:
    """Whether another holder has ``.gate.lock``: taken from an open file of its own."""
    fd = locking.open_lock_file(daemons.registry_dir() / GATE_LOCK, mode=0o600)
    try:
        locking.acquire(fd, wait_s=0)
    except TimeoutError:
        return True
    finally:
        os.close(fd)
    return False


def test_registering_prunes_the_dead_under_the_gate(
    registry: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _write(registry, "0123456789ab", _record())  # its lock free: dead
    real, under_gate = daemons.roll, []

    def roll(*, prune: bool = True, gated: bool = False) -> Any:
        under_gate.append((prune, gated, _gate_held()))
        return real(prune=prune, gated=gated)

    monkeypatch.setattr(daemons, "roll", roll)
    with daemons.registered("serve", argv=["x"], restartable=False) as mine:
        assert mine is not None
        assert _files() == [f"{mine.daemon.id}.json", f"{mine.daemon.id}.lock"]
    assert under_gate == [(True, True, True)], "the one pruning read, with the gate held"
    assert not _gate_held(), "and the gate let go after"


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


def test_a_record_naming_another_id_is_unreadable_and_names_no_path(registry: Path) -> None:
    _write(registry, "0123456789ab", _record(id="../../evil"))
    fd = _hold(registry / "0123456789ab.lock")
    try:
        found = daemons.roll()
    finally:
        os.close(fd)
    assert found.live == [] and [u.id for u in found.unreadable] == ["0123456789ab"]
    assert found.unreadable[0].path == registry / "0123456789ab.json"


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


class TickFailed(BaseException):
    """A stand-in tick's failure, raised past ``run``'s ``except Exception`` (one bad tick never
    ends the watcher), so that a test fails instead of looping for ever."""


def _one_tick(body: Callable[[], None]) -> Callable[..., None]:
    """A stand-in for ``Watcher.tick`` that runs ``body`` once: a failure in it, or a second
    tick (the watcher did not stop), ends ``run`` with :class:`TickFailed`."""
    calls: list[None] = []

    def tick(self: watch_service.Watcher, now: datetime | None = None) -> None:
        calls.append(None)
        if len(calls) > 1:
            raise TickFailed("the watcher ticked again: it did not stop")
        try:
            body()
        except Exception as exc:
            raise TickFailed(f"{type(exc).__name__}: {exc}") from exc

    return tick


def _run_watcher_once(monkeypatch: pytest.MonkeyPatch, *, interval: float | None) -> list[Any]:
    """``watch_service.run`` for one tick, which reads the registry and sends the watcher the
    SIGTERM ``watch --stop`` sends (its handler, called: a real one would end the suite on
    Windows); what the tick found."""
    seen: list[Any] = []

    def body() -> None:
        seen.append(daemons.list_live())
        stopping = signal.getsignal(signal.SIGTERM)
        assert callable(stopping), "run() takes SIGTERM"
        stopping(signal.SIGTERM, None)

    monkeypatch.setattr(watch_service.Watcher, "tick", _one_tick(body))
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
    assert os.path.isabs(daemon.argv[0]), "argv[0] is an executable's absolute path"
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

    def body() -> None:
        assert [d.kind for d in daemons.list_live()] == ["watch"]
        raise Stop

    def tick(self: watch_service.Watcher, now: datetime | None = None) -> None:
        try:
            body()
        except Exception as exc:  # past run()'s except Exception: a failure, not a loop
            raise TickFailed(f"{type(exc).__name__}: {exc}") from exc

    monkeypatch.setattr(watch_service.Watcher, "tick", tick)
    with pytest.raises(Stop):
        watch_service.run(interval=1, on_tick=lambda report: None)
    assert _files() == []


@pytest.mark.skipif(sys.platform == "win32", reason="no SIGHUP there")
def test_a_watcher_whose_window_closes_leaves_the_registry(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # watch --stop kills the tmux session, which sends the watcher SIGHUP: its handler (called,
    # as a real one would end the suite) ends the loop the way SIGTERM does.
    def body() -> None:
        assert [d.kind for d in daemons.list_live()] == ["watch"]
        if sys.platform == "win32":  # skipped there; and mypy --platform win32 reads this
            raise AssertionError("no SIGHUP on Windows")
        hangup = signal.getsignal(signal.SIGHUP)
        assert callable(hangup), "run() takes SIGHUP"
        hangup(signal.SIGHUP, None)

    monkeypatch.setattr(watch_service.Watcher, "tick", _one_tick(body))
    monkeypatch.setattr(watch_service, "_sleep", lambda seconds: None)
    watch_service.run(interval=1, on_tick=lambda report: None)
    assert _files() == []


@pytest.mark.parametrize("name", ["SIGTERM"] if sys.platform == "win32" else ["SIGTERM", "SIGHUP"])
def test_a_watcher_started_with_a_signal_ignored_keeps_it_ignored(
    monkeypatch: pytest.MonkeyPatch, name: str
) -> None:
    # nohup rlx watch &: SIGHUP is SIG_IGN, and the watcher must outlive the logout.
    signum = getattr(signal, name)
    signal.signal(signum, signal.SIG_IGN)  # given back by keep_sigterm
    seen: list[object] = []

    class Stop(BaseException):
        pass

    def tick(self: watch_service.Watcher, now: datetime | None = None) -> None:
        seen.append(signal.getsignal(signum))
        raise Stop

    monkeypatch.setattr(watch_service.Watcher, "tick", tick)
    with pytest.raises(Stop):
        watch_service.run(interval=1, on_tick=lambda report: None)
    assert seen == [signal.SIG_IGN] and _files() == []


def test_watch_stop_says_stopped_only_once_the_watcher_has_exited(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(watch_service, "stop_detached", lambda: 4242)
    monkeypatch.setattr(watch_service, "running_pid", lambda: 4242)  # still in its look
    still = CliRunner().invoke(app, ["watch", "--stop"])
    assert still.exit_code == 0 and "still stopping" in still.output
    assert "stopped the watcher" not in still.output
    said = CliRunner().invoke(app, ["--json", "watch", "--stop"])
    assert json.loads(said.output) == {"stopped": 4242, "stopping": True}
    monkeypatch.setattr(watch_service, "running_pid", lambda: None)
    gone = CliRunner().invoke(app, ["watch", "--stop"])
    assert "stopped the watcher (pid 4242)" in gone.output


def test_watch_stop_kills_the_window_only_once_the_watcher_has_left(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    said: list[str] = []
    pids = iter([4242, 4242, 4242, None])  # the watcher, then still leaving, then gone

    def running_pid() -> int | None:
        pid = next(pids, None)
        said.append(f"running? {pid}")
        return pid

    class Server:
        def kill_session(self, name: str) -> None:
            said.append(f"kill {name}")

    monkeypatch.setattr(watch_service, "running_pid", running_pid)
    monkeypatch.setattr(os, "kill", lambda pid, sig: said.append(f"signal {pid} {sig}"))
    monkeypatch.setattr(watch_service, "_sleep", lambda seconds: None)
    monkeypatch.setattr(fleet_service, "settings", lambda: None)
    monkeypatch.setattr(fleet_service, "server", lambda config: Server())
    assert watch_service.stop_detached() == 4242
    assert said == [
        "running? 4242",
        f"signal 4242 {signal.SIGTERM}",
        "running? 4242",
        "running? 4242",
        "running? None",
        f"kill {watch_service.DETACHED_SESSION}",
    ]


def test_watch_stop_kills_the_window_after_its_wait_all_the_same(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    killed: list[str] = []
    slept: list[float] = []

    class Server:
        def kill_session(self, name: str) -> None:
            killed.append(name)

    monkeypatch.setattr(watch_service, "running_pid", lambda: 4242)  # never leaves
    monkeypatch.setattr(os, "kill", lambda pid, sig: None)
    monkeypatch.setattr(watch_service, "_sleep", slept.append)
    monkeypatch.setattr(fleet_service, "settings", lambda: None)
    monkeypatch.setattr(fleet_service, "server", lambda config: Server())
    watch_service.stop_detached()
    assert killed == [watch_service.DETACHED_SESSION]
    assert (
        watch_service.STOP_WAIT_SECONDS - 0.2 <= sum(slept) <= watch_service.STOP_WAIT_SECONDS + 0.2
    )


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
    assert os.path.isabs(daemon.argv[0]), "argv[0] is an executable's absolute path"
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


def test_a_real_serve_over_stdio_leaves_the_registry_at_its_idle_deadline(
    tmp_path: Path,
) -> None:
    pytest.importorskip("mcp", reason="the [serve] extra is not installed")
    repo = tmp_path / "repo"
    (repo / ".git").mkdir(parents=True)
    env = {k: v for k, v in os.environ.items() if k != "AISQUARE_SERVE_CLOSE_AFTER"}
    server = subprocess.Popen(  # stdin held open and never written: idle from the start
        selfcli.argv_for(["serve", "--stdio", "--close-after", "1"]),
        cwd=repo,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    seen: list[daemons.Daemon] = []
    try:
        deadline = time.monotonic() + 120
        while server.poll() is None and time.monotonic() < deadline:
            seen = seen or daemons.list_live(prune=False)
            time.sleep(0.05)
        assert server.poll() == 0, "it left by its idle deadline"
    finally:
        if server.poll() is None:
            server.kill()
            server.wait(30)
        for stream in (server.stdin, server.stderr):
            if stream is not None:
                stream.close()
    assert [(d.kind, d.pid, d.restartable) for d in seen] == [("serve", server.pid, False)]
    assert _files() == [], "deregistered before os._exit, not left for the next reader"


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
