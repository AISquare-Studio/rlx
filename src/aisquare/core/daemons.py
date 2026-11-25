"""Every long-lived non-UI rlx process on this home, one record each (0.10.3 §4.1, 4.1a).

An upgrade counted only the asq UIs (``core.instances``), yet a detached ``watch``, a
``serve`` or rlxd keeps running the code it started with: on macOS and Linux a mix of old
and new until it restarts, on Windows a file in use that fails the reinstall. So each such
process registers here while it runs, with what an upgrade needs to count it, stop it and
start it again (§4.2). rlxd, written in Rust, registers in this same folder, so what follows
is the format, precisely, for any language; this module is its Python side.

**The folder**, ``<home>/daemons/``, a sibling of ``instances/``: a registry by the rules of
``core.registry`` (its module docstring is the rest of this format), which says how the
folder, ``<id>.json`` and ``<id>.lock`` are made, locked, written, read, deleted and pruned.
What this registry adds:

- **Every daemon keeps a lock.** A record with no ``<id>.lock`` beside it is not live.
- **The heartbeat.** A daemon rewrites its record every :data:`HEARTBEAT_SECONDS` with a new
  ``heartbeat``; :data:`STALE_SECONDS` is the stale age.
- **Live** is ``core.registry``'s rule by lock or heartbeat: its lock held, at once; with its
  lock free, this host's record is a crashed daemon's, whatever has its pid now; another
  host's (an NFS home) lives while its heartbeat is fresh.

**The record**: one JSON object. Keys this release does not know are ignored, as a later
release may say more; a record without one of the required keys, or with one of the wrong
type, is one this release cannot read (reported, never crashed on: ``core.registry``).

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
``argv``        [string]   the command that starts it again, ``argv[0]`` the program,
                           never empty. To restart a daemon: run ``argv`` as a new
                           process, detached from any terminal (no stdin; output to a
                           null device or a log), in ``cwd``, with ``AISQUARE_HOME``
                           naming this home, and do not wait for it to exit: it may BE
                           the daemon (``serve``), or start it in a host and return
                           (``watch --detach``, which opens its tmux window again).
                           Options the daemon took from its environment are written
                           out, so the command means what it meant. Recorded whether
                           or not ``restartable``, for a person to read.
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

There is no id in the record: the id is the file's name (``core.registry``).
"""

from __future__ import annotations

import contextlib
import logging
import os
import threading
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass, replace
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Literal

from aisquare.core import registry
from aisquare.core.registry import Unreadable

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
stale; and how old an unreadable record or a gone record's leftover must be before a reader
deletes it (``core.registry``)."""

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
                since=registry.moment(data["since"]),
                argv=tuple(argv),
                restartable=restartable,
                host=host,
                version=version,
                heartbeat=registry.moment(data["heartbeat"]),
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
    return registry.live_by_lock_or_heartbeat(
        held,
        host=daemon.host,
        pid=daemon.pid,
        heartbeat=daemon.heartbeat,
        stale_seconds=STALE_SECONDS,
        here=here,
        now=now,
    )


def roll(*, prune: bool = True) -> registry.Roll[Daemon]:
    """The live daemons on this home, oldest first, and the records this release cannot read
    that have their lock held or are fresh. With ``prune``, deletes what is not live
    (``core.registry``); without, writes nothing. Reads only: no folder is made."""
    here, now = registry.host_name(), datetime.now(UTC)
    found = REGISTRY.roll(
        Daemon.from_record,
        lambda daemon, held: _live(daemon, held, here=here, now=now),
        prune=prune,
    )
    return replace(found, live=sorted(found.live, key=lambda daemon: daemon.since))


def list_live(*, kind: str | None = None, prune: bool = True) -> list[Daemon]:
    """Every live daemon on this home (of ``kind``, when given), oldest first (:func:`roll`)."""
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

    def beat(self) -> None:
        """Rewrite the record with a heartbeat of now."""
        with self._guard:
            if self._closed:
                return
            self._daemon = replace(self._daemon, heartbeat=datetime.now(UTC))
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
        since=now,
        argv=tuple(argv),
        restartable=restartable,
        host=registry.host_name(),
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
