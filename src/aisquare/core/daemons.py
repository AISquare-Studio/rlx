"""Every long-lived non-UI rlx process on this home, one record each (0.10.3 §4.1, 4.1a).

An upgrade counted only the asq UIs (``core.instances``), yet a detached ``watch``, a
``serve`` or rlxd keeps running the code it started with: on macOS and Linux a mix of old
and new until it restarts, on Windows a file in use that fails the reinstall. So each such
process registers here while it runs, with what an upgrade needs to count it, stop it and
start it again (§4.2). rlxd, written in Rust, registers in this same folder, so what follows
is the format, precisely, for any language; this module is its Python side. The record's
JSON Schema is ``rlxd/schemas/v1/daemon-record.json``.

**The folder**, ``<home>/daemons/``, a sibling of ``instances/``: a registry by the rules of
``core.registry`` (its module docstring is the rest of this format), which says how the
folder, ``<id>.json``, ``<id>.lock`` and ``.gate.lock`` are made, locked, written, read,
deleted and pruned. What this registry adds:

- **Live is the lock.** A record is live while its ``<id>.lock`` is held, whatever host it
  names; a record whose lock is free, or which has no lock file, is a dead process's,
  whatever has its pid since. Only where the lock cannot be probed at all (an error other
  than "held", ``ENOLCK`` on an NFS mount with no lock manager) does the record speak: this
  host's is live while its pid runs, another host's while its ``heartbeat_at`` is under
  :data:`STALE_SECONDS` (60 s) old.
- **Another host's fresh record is kept.** Some shared homes carry no lock across machines
  (NFS with ``local_lock=all`` or ``nolock``, macOS ``locallocks``, a container's bind
  mount), so a reader reads another host's held lock as free. A record naming another host
  whose ``heartbeat_at`` is under :data:`STALE_SECONDS` old is therefore not live to this
  reader, and never pruned by it either: neither the record nor its lock file is deleted.
  Once its heartbeat is stale, any reader prunes it.
- **Registering** is ``core.registry``'s order, under ``.gate.lock``: the others read (the
  dead pruned), then this one's lock taken and its record written.
- **The heartbeat.** A daemon rewrites its record every :data:`HEARTBEAT_SECONDS` (5 s) with
  a new ``heartbeat_at``, and before each rewrite checks that ``<id>.lock`` is still the file
  it holds, taking it again when another process deleted or replaced it
  (``core.registry.Registry.keep_lock``), so it never goes unseen.
- **Leaving.** Each daemon leaves on its exits that run code: ``watch`` on SIGTERM, SIGHUP
  and SIGINT; ``serve`` over HTTP on SIGTERM and Ctrl-C; ``serve --stdio`` at EOF and just
  before its idle deadline's ``os._exit``. A record left behind by any other end (a kill,
  a crash) has its lock free, and the next reader prunes it anyway.
- **Counting.** :func:`list_live` is the live records. A daemon that runs but whose record
  read failed twice (a rewrite on Windows) is in :func:`roll`'s ``unreadable`` with
  ``held=True``: a count of running daemons (an upgrade's, §4.2) counts those too.

**The record**: one JSON object. Keys this release does not know are ignored, as a later
release may say more; a record without one of the required keys, or with one of the wrong
type, or whose ``id`` is not its file's, is one this release cannot read (reported, never
crashed on: ``core.registry``).

- ``id`` (string): its file's name without ``.json``, twelve lower-case hex digits. Another
  id makes the record unreadable; no path is ever built from it.
- ``kind`` (string): what the daemon is, one of :data:`KINDS` (``rlxd``, ``brain``,
  ``watch``, ``serve``, ``remote``, ``board_watch``). A kind this release does not know is
  read and kept as said: a later release's daemon is still a running rlx process.
- ``pid`` (integer): its process id, ``1`` to ``2**31 - 1``. Outside that range the record is
  unreadable (no OS hands out such a pid, and ``kill``/``OpenProcess`` take a C ``int``).
- ``host`` (string): the host name it runs on (``socket.gethostname()``).
- ``version`` (string): the rlx release it runs (``aisquare-cli``'s version, or rlxd's).
- ``started_at`` (string): when it registered, RFC 3339 in UTC, written with an upper-case
  ``T`` and ``Z`` or a numeric offset (``2026-11-25T12:03:00.123456+00:00``,
  ``2026-11-25T12:03:00Z``), with up to nine fractional digits. A lower-case ``t`` or ``z``,
  which RFC 3339 allows, makes the record unreadable here (Python's ``fromisoformat``
  refuses them); a time without an offset is read as UTC.
- ``heartbeat_at`` (string): when the record was last written, as ``started_at``.
- ``argv`` (array of strings, never empty): the command that starts it again. ``argv[0]`` is
  the absolute path of an executable: Python writes ``sys.executable``, rlxd
  ``std::env::current_exe()``. Never a bare name nor, on Windows, a ``.cmd``/``.bat`` shim,
  as neither ``CreateProcess`` nor Rust's ``Command`` resolves ``PATHEXT``. To restart a
  daemon: run ``argv`` as a new process, detached from any terminal (no
  stdin; output to a null device or a log), in ``cwd`` when the record has one, with
  ``AISQUARE_HOME`` naming this home, and do not wait for it to exit: it may BE the daemon
  (``serve``), or start it in a host and return (``watch --detach``, which opens its tmux
  window again). Options the daemon took from its environment are written out, so the
  command means what it meant. Recorded whether or not ``restartable``, for a person to read.
- ``restartable`` (boolean): whether an upgrade may stop the daemon and start ``argv`` in its
  place. ``false`` for one a client owns (``serve --stdio``, whose client holds its stdin and
  stdout) or a person's terminal does (a ``watch`` run in the foreground).
- ``cwd`` (string or null), OPTIONAL: the directory it runs in, absolute, where its restart
  must run (``serve`` serves the project it was started in); ``null`` or absent where none
  could be named.

Every key but ``cwd`` is required.
"""

from __future__ import annotations

import contextlib
import logging
import os
import threading
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass, replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any, Literal

from aisquare.core import registry
from aisquare.core.registry import Unreadable

Kind = Literal["rlxd", "brain", "watch", "serve", "remote", "board_watch"]
"""What a daemon is."""

KINDS: tuple[Kind, ...] = ("rlxd", "brain", "watch", "serve", "remote", "board_watch")
"""Every kind this release knows. 4.1a registers ``watch`` and ``serve``; ``remote`` and
``board_watch`` come with 4.1b, ``rlxd`` and ``brain`` with rlxd."""

HEARTBEAT_SECONDS = 5.0
"""How often a registered daemon rewrites its record (handoff §4.1)."""

STALE_SECONDS = 60.0
"""How old an unreadable record or a gone record's leftover must be before a reader deletes it
(``core.registry``); and, where a lock cannot be probed at all, how old another host's
heartbeat may be before its record is dead."""

PID_MAX = 2**31 - 1
"""The largest pid a record may say: ``os.kill`` (and so ``core.procs.alive``) raises
``OverflowError``, not ``OSError``, past a C ``int``, which no OS hands out anyway."""

FOLDER = "daemons"

REGISTRY = registry.Registry(FOLDER, stale_seconds=STALE_SECONDS)
"""``<home>/daemons/``."""

_log = logging.getLogger(__name__)

__all__ = [
    "HEARTBEAT_SECONDS",
    "KINDS",
    "REGISTRY",
    "STALE_SECONDS",
    "Daemon",
    "Kind",
    "Registration",
    "Roll",
    "Unreadable",
    "describe_this_process",
    "is_live",
    "list_live",
    "prune",
    "register",
    "registered",
    "registry_dir",
    "roll",
    "valid_id",
]


@dataclass(frozen=True)
class Daemon:
    """One daemon's record, and the id its file name gives it."""

    id: str
    kind: str
    """One of :data:`KINDS`, or a later release's kind, kept as said."""
    pid: int
    host: str
    version: str
    started_at: datetime
    heartbeat_at: datetime
    argv: tuple[str, ...]
    restartable: bool
    cwd: str | None = None

    def to_record(self) -> dict[str, Any]:
        """The record as written (the module's list of keys)."""
        return {
            "id": self.id,
            "kind": self.kind,
            "pid": self.pid,
            "host": self.host,
            "version": self.version,
            "started_at": self.started_at.isoformat(),
            "heartbeat_at": self.heartbeat_at.isoformat(),
            "argv": list(self.argv),
            "restartable": self.restartable,
            "cwd": self.cwd,
        }

    @classmethod
    def from_record(cls, daemon_id: str, data: object) -> Daemon | None:
        """The record ``data`` holds, as the daemon ``daemon_id`` (its file's name); ``None``
        for one this release cannot read (the module's list of keys says what it must hold), one
        whose ``id`` is another than its file's included."""
        if not isinstance(data, dict):
            return None
        try:
            if data["id"] != daemon_id:
                return None
            kind, pid, argv = data["kind"], data["pid"], data["argv"]
            restartable, host, version = data["restartable"], data["host"], data["version"]
            cwd = data.get("cwd")
            if not (isinstance(kind, str) and kind):
                return None
            if type(pid) is not int or not 0 < pid <= PID_MAX:  # a bool is an int to isinstance
                return None
            if not (isinstance(argv, list) and argv and all(isinstance(a, str) for a in argv)):
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
                host=host,
                version=version,
                started_at=registry.moment(data["started_at"]),
                heartbeat_at=registry.moment(data["heartbeat_at"]),
                argv=tuple(argv),
                restartable=restartable,
                cwd=cwd,
            )
        except (KeyError, TypeError, ValueError):
            return None

    @property
    def label(self) -> str:
        """It, named in a line: ``serve (rlx 0.10.3, pid 4242 on mini)``."""
        return f"{self.kind} (rlx {self.version}, pid {self.pid} on {self.host})"


Roll = registry.Roll
"""The registry as one read found it (``core.registry.Roll`` of :class:`Daemon`)."""


def registry_dir() -> Path:
    """``<home>/daemons``; made by the first daemon to register, never by a reader."""
    return REGISTRY.folder


def valid_id(text: str) -> bool:
    """Whether ``text`` is a daemon id (``core.registry.valid_id``)."""
    return registry.valid_id(text)


# --- reading -----------------------------------------------------------------------------------


def is_live(daemon: Daemon, *, host: str | None = None, now: datetime | None = None) -> bool:
    """Whether ``daemon`` runs, by the module's rule (its lock probed now)."""
    return _live(daemon, REGISTRY.lock_held(daemon.id), here=host, now=now)


def _live(
    daemon: Daemon, held: bool | None, *, here: str | None = None, now: datetime | None = None
) -> bool:
    """The module's rule, the lock probed already (``held``): the lock alone, and the record's
    pid or heartbeat only where the lock could not be told (``None``)."""
    if held is not None:
        return held
    return registry.live_by_lock_or_heartbeat(
        None,
        host=daemon.host,
        pid=daemon.pid,
        heartbeat=daemon.heartbeat_at,
        stale_seconds=STALE_SECONDS,
        here=here,
        now=now,
    )


def _kept(daemon: Daemon, *, here: str, now: datetime) -> bool:
    """Whether a reader keeps a record that is not live to it: another host's whose heartbeat
    is fresh, as that host's held lock may not show here (the module's rule)."""
    return daemon.host != here and now - daemon.heartbeat_at < timedelta(seconds=STALE_SECONDS)


def roll(*, prune: bool = True) -> registry.Roll[Daemon]:
    """The live daemons on this home, oldest first, and the records this release cannot read
    that have their lock held or are fresh. With ``prune``, deletes what is not live
    (``core.registry``); without, writes nothing. Reads only: no folder is made."""
    here, now = registry.host_name(), datetime.now(UTC)
    found = REGISTRY.roll(
        Daemon.from_record,
        lambda daemon, held: _live(daemon, held, here=here, now=now),
        prune=prune,
        keep=lambda daemon, held: _kept(daemon, here=here, now=now),
    )
    return replace(found, live=sorted(found.live, key=lambda daemon: daemon.started_at))


def list_live(*, kind: str | None = None, prune: bool = True) -> list[Daemon]:
    """Every live daemon on this home (of ``kind``, when given), oldest first (:func:`roll`).
    A count of running daemons also counts :func:`roll`'s ``unreadable`` with ``held=True``
    (the module's "Counting")."""
    return [d for d in roll(prune=prune).live if kind is None or d.kind == kind]


def prune() -> list[str]:
    """Delete every record that is not live, and what belonged to records gone; the ids of the
    records deleted (:func:`roll`)."""
    return roll(prune=True).pruned


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

    def start(self) -> None:
        """Start the heartbeat thread (:func:`register` does). Once."""
        if not self._thread.is_alive() and not self._closed:
            with contextlib.suppress(RuntimeError):  # started already
                self._thread.start()

    @property
    def beating(self) -> bool:
        """Whether the heartbeat thread runs."""
        return self._thread.is_alive()

    def beat(self) -> None:
        """Rewrite the record with a heartbeat of now; first, take the lock file again if
        another process deleted or replaced it (``core.registry.Registry.keep_lock``)."""
        with self._guard:
            if self._closed:
                return
            if self._lock_fd is not None:
                try:
                    self._lock_fd = REGISTRY.keep_lock(self._daemon.id, self._lock_fd)
                except OSError:  # kept as it was: tried again at the next beat
                    _log.debug("the rlx daemon lock could not be taken again", exc_info=True)
            self._daemon = replace(self._daemon, heartbeat_at=datetime.now(UTC))
            REGISTRY.write(self._daemon.id, self._daemon.to_record())

    def deregister(self) -> None:
        """Stop the heartbeat and leave the registry: the record, then the lock
        (``core.registry``). Once; never raises."""
        with self._guard:
            if self._closed:
                return
            self._closed = True
        self._stop.set()
        if self._thread.is_alive() and self._thread is not threading.current_thread():
            self._thread.join(2.0)
        fd, self._lock_fd = self._lock_fd, None
        REGISTRY.leave(self._daemon.id, fd)

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
        id=registry.new_id(),
        kind=kind,
        pid=os.getpid(),
        host=registry.host_name(),
        version=__version__,
        started_at=now,
        heartbeat_at=now,
        argv=tuple(argv),
        restartable=restartable,
        cwd=_cwd(),
    )


def register(kind: Kind, *, argv: Sequence[str], restartable: bool) -> Registration:
    """Register this process as a daemon of ``kind``, with the command that starts it again
    (``argv``, the module's list of keys says how it is run) and whether an upgrade may
    (``restartable``): under ``.gate.lock``, the others read (the dead pruned), its lock taken
    and held and its record written; then its heartbeat started. The caller calls
    :meth:`Registration.deregister` as it exits (or uses :func:`registered`). Raises
    ``OSError`` when the folder, the gate, the lock or the record cannot be had (another
    holding the gate past ``core.registry.GATE_WAIT_SECONDS`` included)."""
    if kind not in KINDS:
        raise ValueError(f"not a daemon kind: {kind!r}")
    if not argv:
        raise ValueError("a daemon's argv names its program")
    with REGISTRY.gate():
        roll(prune=True)
        me = describe_this_process(kind, argv=argv, restartable=restartable)
        fd = REGISTRY.take(me.id)
        try:
            REGISTRY.write(me.id, me.to_record())
        except BaseException:
            REGISTRY.leave(me.id, fd)
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
