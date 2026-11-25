"""Every long-lived non-UI rlx process on this home, one record each (0.10.3 §4.1, 4.1a).

An upgrade counted only the asq UIs (``core.instances``), yet a detached ``watch``, a
``serve`` or rlxd keeps running the code it started with: on macOS and Linux a mix of old
and new until it restarts, on Windows a file in use that fails the reinstall. So each such
process registers here while it runs, with what an upgrade needs to count it, stop it and
start it again (§4.2). rlxd, written in Rust, registers in this same folder, so what follows
is the format, precisely, for any language; this module is its Python side.

**The folder.** ``<home>/daemons/`` (``<home>`` is ``AISQUARE_HOME``, else ``~/.aisquare``),
a sibling of ``instances/`` with the same rules. It is ``0700`` and every file in it
``0600`` on POSIX, so only this user reads or writes them; the first process to register
makes it, a reader never does. Per daemon, ``<id>`` being twelve lower-case hex digits
(``[0-9a-f]{12}``, random, new for every process):

- ``<id>.json`` — the record, UTF-8 JSON, replaced whole: written to a temp file in the
  folder whose name is not ``<id>.json`` (this module writes ``.<id>.json.<pid>.<hex>.tmp``)
  and renamed over it, so a reader sees the old record or the new one, never a part.
- ``<id>.lock`` — empty, locked by the daemon for as long as it runs. A pid is recycled; a
  lock is not: the OS drops it with the process, however the process ended, so a record
  whose lock is free is a crashed daemon's, whatever has its pid now.

**The lock** (``core.locking``), exclusive and never waited on by a reader:

- POSIX: ``flock(fd, LOCK_EX | LOCK_NB)`` on the lock file opened read-write, created
  ``0600``. ``flock``, not ``fcntl``/``F_SETLK`` record locks, which on Linux do not
  conflict with it (Rust: ``libc::flock``, or ``std::fs::File::try_lock``, which is
  ``flock`` on Unix).
- Windows: ``msvcrt.locking(fd, LK_NBLCK, 1)``, a byte-range lock of ONE byte at offset 0,
  past the end of the empty file. Any lock whose range covers byte 0 conflicts with it
  (Rust: ``LockFileEx(LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY)`` of byte 0, or
  ``File::try_lock``, whose range is the whole file). Open the file sharing read and write
  (Rust's default share mode does), as readers open it too, to probe it.
- Both: the descriptor or handle is not inherited (``O_CLOEXEC``; a non-inheritable handle;
  Python's ``os.open`` and Rust's ``std::fs`` do so by default), or a child the daemon
  starts would hold its lock past it, and a crashed daemon would read as running.

A reader probes a lock by taking it without waiting and letting it go at once: taken, the
lock was free; refused (``EWOULDBLOCK``/``EAGAIN``/``EACCES``), it is held. So a daemon
takes its own lock with a short bounded retry (:data:`LOCK_WAIT_SECONDS`), as a reader may
hold it for that instant.

**Registering**, in this order: make the folder; create and lock ``<id>.lock``; write
``<id>.json``; rewrite it every :data:`HEARTBEAT_SECONDS` with a new ``heartbeat``.
**Deregistering**, on a clean exit: delete ``<id>.json``; close the lock file (Windows
deletes no open file); delete ``<id>.lock``. A daemon that cannot finish is a crashed one,
which the next reader prunes.

**The record**: one JSON object. Keys this release does not know are ignored, as a later
release may say more; a record without one of the required keys, or with one of the wrong
type, is one this release cannot read.

=============== ========== ===========================================================
key             type       what
=============== ========== ===========================================================
``kind``        string     what the daemon is: one of :data:`KINDS` (``rlxd``,
                           ``brain``, ``watch``, ``serve``, ``remote``,
                           ``board_watch``). A kind this release does not know is
                           read and kept as said: a later release's daemon is still a
                           running rlx process.
``pid``         integer    its process id, ``> 0``.
``since``       string     when it registered: ISO 8601 / RFC 3339 with an offset, in
                           UTC (``2026-11-25T12:03:00.123456+00:00``; ``Z`` is read
                           too; a time without an offset is read as UTC).
``argv``        [string]   the command that starts it again, ``argv[0]`` the program.
                           To restart a daemon: run ``argv`` as a new process, detached
                           from any terminal (no stdin; output to a null device or a
                           log), in ``cwd``, with ``AISQUARE_HOME`` naming this home,
                           and do not wait for it to exit: it may BE the daemon
                           (``serve``), or start it in a host and return (``watch
                           --detach``, which opens its tmux window again). Options the
                           daemon took from its environment are written out, so the
                           command means what it meant. Recorded whether or not
                           ``restartable``, for a person to read.
``restartable`` boolean    whether an upgrade may stop the daemon and start ``argv``
                           in its place. ``false`` for one a client owns (``serve
                           --stdio``, whose client holds its stdin and stdout) or a
                           person's terminal does (a ``watch`` run in the foreground).
``host``        string     the host name it runs on (``socket.gethostname()``).
``version``     string     the rlx release it runs (``aisquare-cli``'s version, or
                           rlxd's).
``heartbeat``   string     when the record was last written, as ``since``.
``cwd``         string or  the directory it runs in, absolute; optional, ``null`` or
                null       absent where none could be named.
=============== ========== ===========================================================

A record's id is its FILE NAME's (:func:`valid_id`), never anything inside it: every path
is built from a name the folder listing gave, so no record names a file outside the folder,
and a file whose name is not ``<id>.json`` is no record, whatever it holds.

**Live** (:func:`is_live`): a record whose lock is held is live, whatever host it names.
With its lock free, or its lock file gone, a record naming THIS host is a crashed daemon's,
not live. One naming another host sharing the home (NFS) is live while its heartbeat is
under :data:`STALE_SECONDS` old: a lock taken on another machine may not show here. Where
the lock cannot be probed at all (an error other than "held", ``ENOLCK`` on an NFS mount
with no lock manager), this host's record is live while its pid runs, another host's while
its heartbeat is fresh.

**Pruning**: every reader (:func:`roll`) deletes a record that is not live, with its lock;
a record whose lock is held never is. A record this release cannot read is reported
(:class:`Unreadable`), never crashed on: kept while its lock is held or its file is under
:data:`STALE_SECONDS` old (a later release's daemon, most likely), deleted with its lock
once neither holds. Beside the records, a reader deletes only a file of a record that is
gone (``<id>.lock`` whose lock is free, a temp ``.<id>.…``) once it is as old as a stale
heartbeat; any other name is left alone, for a later release to give a meaning.
"""

from __future__ import annotations

import contextlib
import json
import logging
import os
import re
import sys
import threading
import time
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass, replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any, Literal, NamedTuple
from uuid import uuid4

from aisquare.core import atomic, locking, paths, procs
from aisquare.core.instances import host_name

Kind = Literal["rlxd", "brain", "watch", "serve", "remote", "board_watch"]
"""What a daemon is."""

KINDS: tuple[Kind, ...] = ("rlxd", "brain", "watch", "serve", "remote", "board_watch")
"""Every kind this release knows. 4.1a registers ``watch`` and ``serve``; ``remote`` and
``board_watch`` come with 4.1b, ``rlxd`` and ``brain`` with rlxd."""

HEARTBEAT_SECONDS = 15.0
"""How often a registered daemon rewrites its record: a quarter of :data:`STALE_SECONDS`, so
a daemon on another host sharing the home reads as live through three missed beats."""

STALE_SECONDS = 60.0
"""How old the heartbeat of another host's record with no held lock may be before it is
stale; and how old a file must be before a reader deletes it without a record to say why."""

LOCK_WAIT_SECONDS = 1.0
"""How long :func:`register` retries its own new lock, which a reader probing the folder may
hold for an instant."""

FOLDER = "daemons"

_ID = re.compile(r"[0-9a-f]{12}")

_RETRY_SECONDS = 0.05
"""How long :func:`roll` waits before reading again a record whose read failed."""

_log = logging.getLogger(__name__)


@dataclass(frozen=True)
class Daemon:
    """One daemon's record, and the id its file name gives it."""

    id: str
    kind: str
    """One of :data:`KINDS`, or a later release's kind, kept as said."""
    pid: int
    since: datetime
    argv: tuple[str, ...]
    restartable: bool
    host: str
    version: str
    heartbeat: datetime
    cwd: str | None = None

    def to_record(self) -> dict[str, Any]:
        """The record as written: the id is the file's name, never in it."""
        return {
            "kind": self.kind,
            "pid": self.pid,
            "since": self.since.isoformat(),
            "argv": list(self.argv),
            "restartable": self.restartable,
            "host": self.host,
            "version": self.version,
            "heartbeat": self.heartbeat.isoformat(),
            "cwd": self.cwd,
        }

    @classmethod
    def from_record(cls, daemon_id: str, data: object) -> Daemon | None:
        """The record ``data`` holds, as the daemon ``daemon_id``; ``None`` for one this
        release cannot read (the module's table says what it must hold)."""
        if not isinstance(data, dict):
            return None
        try:
            kind, pid, argv = data["kind"], data["pid"], data["argv"]
            restartable, host, version = data["restartable"], data["host"], data["version"]
            cwd = data.get("cwd")
            if not (isinstance(kind, str) and kind):
                return None
            if type(pid) is not int or pid <= 0:  # a bool is an int to isinstance
                return None
            if not (isinstance(argv, list) and all(isinstance(arg, str) for arg in argv)):
                return None
            if type(restartable) is not bool:
                return None
            if not (isinstance(host, str) and isinstance(version, str)):
                return None
            if cwd is not None and not isinstance(cwd, str):
                return None
            return cls(
                id=daemon_id,
                kind=kind,
                pid=pid,
                since=_moment(data["since"]),
                argv=tuple(argv),
                restartable=restartable,
                host=host,
                version=version,
                heartbeat=_moment(data["heartbeat"]),
                cwd=cwd,
            )
        except (KeyError, TypeError, ValueError):
            return None

    @property
    def label(self) -> str:
        """It, named in a line: ``serve (rlx 0.10.3, pid 4242 on mini)``."""
        return f"{self.kind} (rlx {self.version}, pid {self.pid} on {self.host})"


def _moment(text: object) -> datetime:
    """A record's time: ISO 8601, read as UTC when it names no offset. Raises ``TypeError``
    or ``ValueError`` for anything else."""
    if not isinstance(text, str):
        raise TypeError(f"not a time: {text!r}")
    moment = datetime.fromisoformat(text)
    return moment if moment.tzinfo is not None else moment.replace(tzinfo=UTC)


# --- where -----------------------------------------------------------------------------------


def registry_dir() -> Path:
    """``<home>/daemons``; made by the first daemon to register, never by a reader."""
    return paths.aisquare_home() / FOLDER


def valid_id(text: str) -> bool:
    """Whether ``text`` is a daemon id: twelve lower-case hex digits, so a path built from it
    stays a file in the folder."""
    return _ID.fullmatch(text) is not None


def _named(daemon_id: str, suffix: str) -> Path:
    if not valid_id(daemon_id):
        raise ValueError(f"not an rlx daemon id: {daemon_id!r}")
    return registry_dir() / f"{daemon_id}{suffix}"


def _record(daemon_id: str) -> Path:
    return _named(daemon_id, ".json")


def _lock(daemon_id: str) -> Path:
    return _named(daemon_id, ".lock")


def _private_dir() -> Path:
    """The registry folder, made ``0700`` (and narrowed to it, if it was made wider)."""
    folder = registry_dir()
    folder.mkdir(mode=0o700, parents=True, exist_ok=True)
    if sys.platform != "win32":
        os.chmod(folder, 0o700)
    return folder


def _write_private(path: Path, body: str) -> None:
    """``body`` into ``path`` in one step, ``0600``. Not fsynced: a record lost to a crash is
    one that would be stale anyway."""
    atomic.write_replacing(path, body, durable=False, owner_only=sys.platform != "win32")


# --- reading -----------------------------------------------------------------------------------


def _lock_held(daemon_id: str) -> bool | None:
    """Whether a running process holds ``<id>.lock``: ``False`` too when there is no lock file
    (every daemon keeps one); ``None`` when it cannot be told (a probe that failed otherwise
    than on a holder)."""
    try:
        fd = locking.open_lock_file(_lock(daemon_id), create=False)
    except FileNotFoundError:
        return False
    except OSError:
        return None
    try:
        locking.acquire(fd, wait_s=0)
    except TimeoutError:
        return True
    except OSError:
        return None
    else:
        with contextlib.suppress(OSError):
            locking.unlock(fd)
        return False
    finally:
        os.close(fd)


def is_live(daemon: Daemon, *, host: str | None = None, now: datetime | None = None) -> bool:
    """Whether ``daemon`` runs, by the module's rule: its lock held, at once; else this host's
    record is a crashed daemon's, and another host's lives while its heartbeat is fresh."""
    return _live(daemon, _lock_held(daemon.id), host=host, now=now)


def _live(daemon: Daemon, held: bool | None, *, host: str | None, now: datetime | None) -> bool:
    """:func:`is_live`, its lock already probed (``held``)."""
    if held:
        return True
    if daemon.host == (host_name() if host is None else host):
        return held is None and procs.alive(daemon.pid)
    age = (datetime.now(UTC) if now is None else now) - daemon.heartbeat
    return age < timedelta(seconds=STALE_SECONDS)


def _forget(daemon_id: str) -> None:
    """Delete the record, then its lock: no reader finds a record whose lock is gone."""
    for path in (_record(daemon_id), _lock(daemon_id)):
        with contextlib.suppress(OSError):
            path.unlink(missing_ok=True)


Unloaded = Literal["gone", "failed", "unparsable"]
"""Why a record was not read (:func:`_load`)."""


def _load(path: Path, daemon_id: str) -> Daemon | Unloaded:
    """The record at ``path``, or why not: ``gone``, its file no longer there (a daemon that
    left between the folder's listing and this read); ``failed``, an ``OSError`` reading it
    (a sharing violation during a beat on Windows, an NFS ``ESTALE``); ``unparsable``, its
    content, which a release this one cannot read wrote."""
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return "gone"
    except UnicodeDecodeError:
        return "unparsable"
    except OSError:
        return "failed"
    try:
        daemon = Daemon.from_record(daemon_id, json.loads(text))
    except (ValueError, RecursionError):
        return "unparsable"
    return "unparsable" if daemon is None else daemon


@dataclass(frozen=True)
class Unreadable:
    """A record this release cannot read whose lock is held or whose file is fresh: a daemon
    of an unknown release, most likely a later one; or one whose file would not be read twice
    running."""

    id: str
    written_at: datetime
    held: bool
    """Whether its lock is held: a running process's, for sure."""
    pid: int | None = None
    """Its pid, where the record still says one as an integer."""
    kind: str | None = None
    """Its kind, where the record still says one as a string."""

    @property
    def path(self) -> Path:
        return _record(self.id)


class Roll(NamedTuple):
    """The registry as one read found it."""

    live: list[Daemon]
    """Every live daemon, oldest first."""
    unreadable: list[Unreadable]
    """Records this release cannot read, with their lock held or fresh."""
    pruned: list[str]
    """The ids of the records this read deleted."""


def roll(*, prune: bool = True) -> Roll:
    """The live daemons on this home, and the records this release cannot read that have their
    lock held or are fresh (the module's rules). With ``prune``, deletes what is not live, as
    the module says; without, writes nothing. Reads only: no folder is made."""
    folder = registry_dir()
    try:
        names = sorted(os.listdir(folder))
    except OSError:
        return Roll([], [], [])
    here = host_name()
    now = datetime.now(UTC)
    live: list[Daemon] = []
    unreadable: list[Unreadable] = []
    pruned: list[str] = []
    for name in names:
        path = folder / name
        stem = name.removesuffix(".json")
        if stem == name or not valid_id(stem):
            if prune and _owner(name) is not None and _old(path) and not _explained(name, names):
                with contextlib.suppress(OSError):
                    path.unlink()
            continue  # no record: not <id>.json
        loaded = _load(path, stem)
        if loaded == "failed":  # once more, a moment later: the failures it means are brief
            time.sleep(_RETRY_SECONDS)
            loaded = _load(path, stem)
        if loaded == "gone":
            continue  # its daemon left since the listing
        if isinstance(loaded, str):  # unparsable, or a read that failed twice running
            written = _written(path)
            if written is None:
                continue  # gone since
            locked = _lock_held(stem) is True
            if locked or now - written < timedelta(seconds=STALE_SECONDS):
                pid, kind = _said_in(path)
                unreadable.append(Unreadable(stem, written, locked, pid, kind))
            elif prune:
                _forget(stem)
                pruned.append(stem)
            continue
        lock = _lock_held(stem)
        if _live(loaded, lock, host=here, now=now):
            live.append(loaded)
        elif prune:
            _forget(stem)  # not live: its lock is not held
            pruned.append(stem)
    return Roll(sorted(live, key=lambda daemon: daemon.since), unreadable, pruned)


def list_live(*, kind: str | None = None, prune: bool = True) -> list[Daemon]:
    """Every live daemon on this home (of ``kind``, when given), oldest first (:func:`roll`)."""
    return [d for d in roll(prune=prune).live if kind is None or d.kind == kind]


def prune() -> list[str]:
    """Delete every record that is not live, and what belonged to records gone; the ids of the
    records deleted (:func:`roll`)."""
    return roll(prune=True).pruned


def _owner(name: str) -> str | None:
    """The id whose file ``name`` is, beside its record (``<id>.lock``, a temp ``.<id>.…``);
    ``None`` for a name this release gives no meaning."""
    owner = name.removeprefix(".").split(".", 1)[0]
    return owner if valid_id(owner) and owner != name.removeprefix(".") else None


def _explained(name: str, names: list[str]) -> bool:
    """Whether ``name`` (not a record) belongs to a record still there, or is a lock still held."""
    owner = _owner(name)
    if owner is None:
        return False
    return f"{owner}.json" in names or (name == f"{owner}.lock" and _lock_held(owner) is True)


def _said_in(path: Path) -> tuple[int | None, str | None]:
    """The ``pid`` (an integer) and ``kind`` (a string) a record holds, however else it fails to
    read; ``None`` for each it does not."""
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, RecursionError):
        return None, None
    if not isinstance(data, dict):
        return None, None
    pid, kind = data.get("pid"), data.get("kind")
    return (pid if type(pid) is int else None), (kind if isinstance(kind, str) else None)


def _written(path: Path) -> datetime | None:
    try:
        return datetime.fromtimestamp(path.stat().st_mtime, UTC)
    except OSError:
        return None


def _old(path: Path) -> bool:
    """Whether ``path`` was last written as long ago as a stale heartbeat."""
    try:
        return time.time() - path.stat().st_mtime >= STALE_SECONDS
    except OSError:
        return False


# --- this process's own record ---------------------------------------------------------------


class Registration:
    """This process's record: written by :func:`register`, its heartbeat kept by a thread of
    its own, removed by :meth:`deregister`."""

    def __init__(self, daemon: Daemon, lock_fd: int) -> None:
        self._daemon = daemon
        self._lock_fd: int | None = lock_fd
        self._guard = threading.Lock()
        self._closed = False
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, name="rlx-daemon-beat", daemon=True)

    @property
    def daemon(self) -> Daemon:
        return self._daemon

    def beat(self) -> None:
        """Rewrite the record with a heartbeat of now."""
        with self._guard:
            if self._closed:
                return
            self._daemon = replace(self._daemon, heartbeat=datetime.now(UTC))
            _write_private(_record(self._daemon.id), json.dumps(self._daemon.to_record()))

    def deregister(self) -> None:
        """Stop the heartbeat and remove the record and its lock. Once; never raises."""
        with self._guard:
            if self._closed:
                return
            self._closed = True
        self._stop.set()
        if self._thread.is_alive() and self._thread is not threading.current_thread():
            self._thread.join(2.0)
        # The record first, so no reader finds it once the lock is let go; the lock file
        # after its descriptor is closed, which Windows asks before a delete.
        with contextlib.suppress(OSError):
            _record(self._daemon.id).unlink(missing_ok=True)
        fd, self._lock_fd = self._lock_fd, None
        if fd is not None:
            with contextlib.suppress(OSError):
                os.close(fd)
        _forget(self._daemon.id)

    def start(self) -> None:
        """Start the heartbeat thread (:func:`register` does). Once."""
        if not self._thread.is_alive() and not self._closed:
            with contextlib.suppress(RuntimeError):  # started already
                self._thread.start()

    def _run(self) -> None:
        while not self._stop.wait(HEARTBEAT_SECONDS):
            try:
                self.beat()
            except Exception:  # a beat missed is one more next time, never a dead thread
                _log.debug("the rlx daemon record could not be written", exc_info=True)


def _cwd() -> str | None:
    try:
        return os.getcwd()
    except OSError:  # the directory it runs in was deleted
        return None


def describe_this_process(kind: Kind, *, argv: Sequence[str], restartable: bool) -> Daemon:
    """This process as its record would say it, registered or not."""
    from aisquare.core.version import __version__

    now = datetime.now(UTC)
    return Daemon(
        id=uuid4().hex[:12],
        kind=kind,
        pid=os.getpid(),
        since=now,
        argv=tuple(argv),
        restartable=restartable,
        host=host_name(),
        version=__version__,
        heartbeat=now,
        cwd=_cwd(),
    )


def register(kind: Kind, *, argv: Sequence[str], restartable: bool) -> Registration:
    """Register this process as a daemon of ``kind``, with the command that starts it again
    (``argv``, the module's table says how it is run) and whether an upgrade may
    (``restartable``): its lock taken and held, its record written and its heartbeat started.
    The caller calls :meth:`Registration.deregister` as it exits (or uses :func:`registered`).
    Raises ``OSError`` when the folder, the lock or the record cannot be had."""
    if kind not in KINDS:
        raise ValueError(f"not a daemon kind: {kind!r}")
    if not argv:
        raise ValueError("a daemon's argv names its program")
    me = describe_this_process(kind, argv=argv, restartable=restartable)
    _private_dir()
    fd = locking.open_lock_file(_lock(me.id), mode=0o600)
    try:
        try:
            locking.acquire(fd, wait_s=LOCK_WAIT_SECONDS)
        except TimeoutError as held:  # a new random id's lock, held past a probe: not ours
            raise OSError(f"the daemon lock {_lock(me.id)} is held by another process") from held
        _write_private(_record(me.id), json.dumps(me.to_record()))
    except BaseException:
        os.close(fd)
        _forget(me.id)
        raise
    registration = Registration(me, fd)
    registration.start()
    return registration


@contextlib.contextmanager
def registered(
    kind: Kind,
    *,
    argv: Sequence[str],
    restartable: bool,
    on_error: Callable[[OSError], object] | None = None,
) -> Iterator[Registration | None]:
    """This process registered (:func:`register`) for the block, and deregistered as it ends,
    however it ends. A registry that cannot be written does not stop the daemon: ``on_error``
    is told and the block runs unregistered (``None``), as the registry is what an upgrade
    counts, not what lets a daemon run."""
    try:
        registration: Registration | None = register(kind, argv=argv, restartable=restartable)
    except OSError as exc:
        _log.debug("the rlx daemon could not register", exc_info=True)
        if on_error is not None:
            on_error(exc)
        registration = None
    try:
        yield registration
    finally:
        if registration is not None:
            registration.deregister()
