"""A folder of records, each held by a lock while its process runs: the machinery the home's
process registries share (0.10.3 §4.1a).

``core.daemons`` (``<home>/daemons/``) is the first built on it. ``core.instances`` and
``core.launchers`` keep their own copies of the same rules until 6.2 moves them onto this
module, so their quirks are parameters here (:meth:`Registry.lock_held`'s ``missing``), not
forks. rlxd, written in Rust, writes in such a folder too, so what follows is precise enough
to implement in any language; the record's content is each registry's own.

**The folder**, ``<home>/<name>/`` (``<home>`` is ``AISQUARE_HOME``, else ``~/.aisquare``):
``0700``, every file in it ``0600`` on POSIX, so only this user reads or writes them. The
first writer makes it (and narrows a wider one to ``0700``); a reader never does. Per
record, ``<id>`` being twelve lower-case hex digits (``[0-9a-f]{12}``, random, new for every
process):

- ``<id>.json`` — the record, UTF-8 JSON, replaced whole: written to a temp file in the
  folder whose name is not ``<id>.json`` and renamed over it, so a reader sees the old record
  or the new one, never a part. This module writes ``.<id>.json.<pid>.<hex>.tmp``; rlxd
  writes ``<id>.json.tmp`` and fsyncs it first. A reader takes either for a file of ``<id>``.
- ``<id>.lock`` — empty, locked by the process for as long as it runs. A pid is recycled; a
  lock is not: the OS drops it with the process, however the process ended, so a record
  whose lock is free is a crashed process's, whatever has its pid now.

and ``.gate.lock`` (:data:`GATE_LOCK`), locked with the same primitive, created ``0600``, while
a process reads the others and writes its own record (:meth:`Registry.gate`), waited on for
at most :data:`GATE_WAIT_SECONDS`, so two processes starting at once each see the other. A
reader never deletes it.

A record's id is its FILE NAME's (:func:`valid_id`), never anything inside it: every path is
built from a name the folder listing gave, so no record names a file outside the folder, and
a file whose name is not ``<id>.json`` is no record, whatever it holds.

**The lock** (``core.locking``), exclusive, never waited on by a reader:

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
  Python's ``os.open`` and Rust's ``std::fs`` do so by default), or a child the process
  starts would hold its lock past it, and a crashed process would read as running.

A reader probes a lock by taking it without waiting and letting it go at once: taken, it was
free; refused (``EWOULDBLOCK``/``EAGAIN``/``EACCES``), it is held. So a writer takes its own
new lock with a short bounded retry (:data:`LOCK_WAIT_SECONDS`), as a reader may hold it for
that instant.

**Writing**, in this order: make the folder; take ``.gate.lock``; read the others (pruning
what is dead); create and lock ``<id>.lock``; write ``<id>.json``; let ``.gate.lock`` go.
**Leaving**, on a clean exit: delete ``<id>.json``; close the lock file
(Windows deletes no open file); delete ``<id>.lock``. On Windows a delete is refused while
any other process has the file open (``ERROR_SHARING_VIOLATION``, 32), and every reader opens
each record for a moment: a delete refused so is tried again for up to
:data:`DELETE_RETRY_SECONDS` (:func:`unlink`; the rule ``core.instances`` took in #574, #28).
A process that cannot finish leaving is a crashed one, which the next reader prunes.

**Reading** (:meth:`Registry.roll`): each ``<id>.json`` is read once more, a moment later
(:data:`READ_RETRY_SECONDS`), when the read fails (a sharing violation during a rewrite on
Windows, an NFS ``ESTALE``); one gone since the listing is no record. The registry's own rule
says whether a record it can read is live; with pruning, one that is not is deleted with its
lock, and one whose lock is held never is. A record it cannot read (not JSON, or not what the
registry's parser takes) is reported (:class:`Unreadable`), never crashed on: kept while its
lock is held or its file is younger than the registry's stale age (a later release's process,
most likely), deleted with its lock once neither holds. Beside the records, a reader deletes
only a file of a record that is gone (``<id>.<anything>`` whose lock is free, a temp
``.<id>.…``) once it is as old as a stale record; any other name is left alone, for a later
release to give a meaning.

**Live by lock or heartbeat** (:func:`live_by_lock_or_heartbeat`), the rule the home's
registries share: a record whose lock is held is live, whatever host it names. With its lock
free, a record naming THIS host is a crashed process's. One naming another host sharing the
home (NFS) is live while its heartbeat is younger than the stale age: a lock taken on another
machine may not show here. Where the lock cannot be told (an error other than "held",
``ENOLCK`` on an NFS mount with no lock manager), this host's record is live while its pid
runs, another host's while its heartbeat is fresh.
"""

from __future__ import annotations

import contextlib
import enum
import json
import os
import re
import socket
import sys
import time
from collections.abc import Callable, Iterator, Mapping
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any, Generic, TypeVar
from uuid import uuid4

from aisquare.core import atomic, locking, paths, procs

T = TypeVar("T")

LOCK_WAIT_SECONDS = 1.0
"""How long :meth:`Registry.take` retries a new lock, which a reader probing the folder may
hold for an instant."""

DELETE_RETRY_SECONDS = 1.0
"""How long a delete refused with ``PermissionError`` is tried again (:func:`unlink`): on
Windows a file another process has open cannot be deleted, and readers open each record for a
moment. Refused once and given up, ``core.instances``'s records stayed behind and, naming
another host name, read as live by their heartbeat (#574, #28)."""

READ_RETRY_SECONDS = 0.05
"""How long a reader waits before reading again a record whose read failed, and between two
tries of a refused delete."""

GATE_LOCK = ".gate.lock"
"""The folder's gate (:meth:`Registry.gate`): held while a process reads the others and writes
its own record. A reader never deletes it."""

GATE_WAIT_SECONDS = 2.0
"""How long :meth:`Registry.gate` waits for another process's read-and-write to end."""

_ID = re.compile(r"[0-9a-f]{12}")


def valid_id(text: str) -> bool:
    """Whether ``text`` is a record id: twelve lower-case hex digits, so a path built from it
    stays a file in the folder."""
    return _ID.fullmatch(text) is not None


def new_id() -> str:
    """A fresh random id (:func:`valid_id`)."""
    return uuid4().hex[:12]


def host_name() -> str:
    """This host's name as a record says it (``socket.gethostname()``)."""
    return socket.gethostname() or "localhost"


def zoned(moment: datetime) -> datetime:
    """``moment`` with a zone: a record's times are UTC, and one written without says so."""
    return moment if moment.tzinfo is not None else moment.replace(tzinfo=UTC)


def moment(text: object) -> datetime:
    """A record's time: ISO 8601 / RFC 3339 (``Z`` too), read as UTC when it names no
    offset. Raises ``TypeError`` or ``ValueError`` for anything else."""
    if not isinstance(text, str):
        raise TypeError(f"not a time: {text!r}")
    return zoned(datetime.fromisoformat(text))


def unlink(path: Path) -> bool:
    """Delete ``path`` (gone already is fine), trying again for :data:`DELETE_RETRY_SECONDS`
    while it is refused with ``PermissionError``; whether it is gone. Never raises."""
    deadline = time.monotonic() + DELETE_RETRY_SECONDS
    while True:
        try:
            path.unlink(missing_ok=True)
        except PermissionError:
            if time.monotonic() >= deadline:
                return False
            time.sleep(READ_RETRY_SECONDS)
        except OSError:
            return False
        else:
            return True


def live_by_lock_or_heartbeat(
    held: bool | None,
    *,
    host: str,
    pid: int,
    heartbeat: datetime,
    stale_seconds: float,
    here: str | None = None,
    now: datetime | None = None,
) -> bool:
    """The module's rule: ``held`` (the record's lock, :meth:`Registry.lock_held`) at once;
    else this host's record (``host == here``) is live only where the lock could not be told
    and its pid runs; another host's while its heartbeat is under ``stale_seconds`` old."""
    if held:
        return True
    if host == (host_name() if here is None else here):
        return held is None and procs.alive(pid)
    age = (datetime.now(UTC) if now is None else now) - heartbeat
    return age < timedelta(seconds=stale_seconds)


@dataclass(frozen=True)
class Unreadable:
    """A record this release cannot read whose lock is held or whose file is fresh: a process
    of an unknown release, most likely a later one; or one whose file would not be read twice
    running."""

    id: str
    path: Path
    written_at: datetime
    held: bool
    """Whether its lock is held: a running process's, for sure."""
    said: Mapping[str, Any] | None = None
    """Its content, where it is at least a JSON object; ``None`` otherwise."""

    @property
    def pid(self) -> int | None:
        """The ``pid`` it still says, as an integer; ``None``."""
        pid = (self.said or {}).get("pid")
        return pid if type(pid) is int else None

    @property
    def kind(self) -> str | None:
        """The ``kind`` it still says, as a string; ``None``."""
        kind = (self.said or {}).get("kind")
        return kind if isinstance(kind, str) else None


@dataclass(frozen=True)
class Roll(Generic[T]):
    """A registry as one read found it."""

    live: list[T]
    """Every live record, in the folder's order (the registry sorts them as it says)."""
    unreadable: list[Unreadable]
    """Records this release cannot read, with their lock held or fresh."""
    pruned: list[str]
    """The ids of the records this read deleted."""
    held: frozenset[str] = field(default_factory=frozenset)
    """The ids of the live records whose lock is held: a running process's, whatever host the
    record names."""


class Unloaded(enum.Enum):
    """Why a record was not read (:meth:`Registry.load`)."""

    GONE = "gone"
    """Its file is no longer there: its process left between the listing and the read."""
    FAILED = "failed"
    """An ``OSError`` reading it: a sharing violation during a rewrite on Windows, an NFS
    ``ESTALE``."""
    UNPARSABLE = "unparsable"
    """Not JSON, or not what the registry's parser takes: a release this one cannot read
    wrote it."""


class Registry:
    """``<home>/<name>/``, by the module's rules; the record's content is the caller's."""

    def __init__(self, name: str, *, stale_seconds: float) -> None:
        self.name = name
        self.stale_seconds = stale_seconds
        """How old an unreadable record's file, or a gone record's leftover, may be before a
        reader deletes it; and the heartbeat age past which another host's record with no held
        lock is stale (:func:`live_by_lock_or_heartbeat`)."""

    # --- where ---------------------------------------------------------------------------------

    @property
    def folder(self) -> Path:
        """``<home>/<name>``; made by the first writer, never by a reader."""
        return paths.aisquare_home() / self.name

    def path(self, record_id: str, suffix: str) -> Path:
        """``<id><suffix>`` in the folder; ``ValueError`` for an id that is not one."""
        if not valid_id(record_id):
            raise ValueError(f"not an rlx {self.name} id: {record_id!r}")
        return self.folder / f"{record_id}{suffix}"

    def record(self, record_id: str) -> Path:
        return self.path(record_id, ".json")

    def lock(self, record_id: str) -> Path:
        return self.path(record_id, ".lock")

    def private_folder(self) -> Path:
        """The folder, made ``0700`` (and narrowed to it, if it was made wider)."""
        folder = self.folder
        folder.mkdir(mode=0o700, parents=True, exist_ok=True)
        if sys.platform != "win32":
            os.chmod(folder, 0o700)
        return folder

    # --- writing -------------------------------------------------------------------------------

    def take(self, record_id: str, *, wait_s: float = LOCK_WAIT_SECONDS) -> int:
        """Make the folder, then create ``<id>.lock`` (``0600``) and lock it: the open
        descriptor, held until :meth:`leave` closes it. ``OSError`` when the folder or the lock
        cannot be had, one held past ``wait_s`` included."""
        self.private_folder()
        lock = self.lock(record_id)
        fd = locking.open_lock_file(lock, mode=0o600)
        try:
            locking.acquire(fd, wait_s=wait_s)
        except TimeoutError as held:
            os.close(fd)
            raise OSError(f"the lock {lock} is held by another process") from held
        except BaseException:
            os.close(fd)
            raise
        return fd

    @contextlib.contextmanager
    def gate(self, *, wait_s: float = GATE_WAIT_SECONDS) -> Iterator[None]:
        """Hold the folder's ``.gate.lock`` (:data:`GATE_LOCK`) for the block, made ``0600``,
        so a process that reads the others and writes its own record inside it is read by, or
        reads, another doing the same at the same moment. Makes the folder; raises
        :class:`TimeoutError` (an ``OSError``) when another holds it past ``wait_s``, and
        ``OSError`` when the folder or the lock cannot be had."""
        folder = self.private_folder()
        with locking.holding(folder / GATE_LOCK, wait_s=wait_s, mode=0o600):
            yield

    def write(self, record_id: str, record: Mapping[str, Any]) -> None:
        """``record`` into ``<id>.json`` in one step, ``0600``. Not fsynced: a record lost to a
        crash is one that would be stale anyway."""
        body = json.dumps(dict(record))
        owner_only = sys.platform != "win32"
        atomic.write_replacing(self.record(record_id), body, durable=False, owner_only=owner_only)

    def leave(self, record_id: str, fd: int | None, *extra: str) -> None:
        """Leave the registry, in the module's order: the record, then the lock file once
        ``fd`` (its descriptor) is closed, and the record's ``extra`` files (suffixes) with it.
        Never raises."""
        unlink(self.record(record_id))
        if fd is not None:
            with contextlib.suppress(OSError):
                os.close(fd)
        self.forget(record_id, *extra)

    def forget(self, record_id: str, *extra: str) -> None:
        """Delete a record, its ``extra`` files (suffixes) and then its lock: no reader finds a
        record whose lock is gone. Never raises."""
        for suffix in (".json", *extra, ".lock"):
            unlink(self.path(record_id, suffix))

    # --- reading -------------------------------------------------------------------------------

    def lock_held(self, record_id: str, *, missing: bool | None = False) -> bool | None:
        """Whether a running process holds ``<id>.lock``; ``None`` when it cannot be told (a
        probe that failed otherwise than on a holder). ``missing`` is the answer when there is
        no lock file: ``False`` where every writer keeps one, ``None`` for a registry an older
        release of which kept none."""
        try:
            fd = locking.open_lock_file(self.lock(record_id), create=False)
        except FileNotFoundError:
            return missing
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

    def load(self, record_id: str, parse: Callable[[str, object], T | None]) -> T | Unloaded:
        """The record ``<id>.json`` as ``parse`` takes it (``parse(id, data)``, ``None`` for
        content it cannot read), or why not (:class:`Unloaded`)."""
        try:
            text = self.record(record_id).read_text(encoding="utf-8")
        except FileNotFoundError:
            return Unloaded.GONE
        except UnicodeDecodeError:
            return Unloaded.UNPARSABLE
        except OSError:
            return Unloaded.FAILED
        try:
            parsed = parse(record_id, json.loads(text))
        except (ValueError, RecursionError):
            return Unloaded.UNPARSABLE
        return Unloaded.UNPARSABLE if parsed is None else parsed

    def roll(
        self,
        parse: Callable[[str, object], T | None],
        live: Callable[[T, bool | None], bool],
        *,
        prune: bool = True,
        missing: bool | None = False,
        reserved: frozenset[str] = frozenset(),
    ) -> Roll[T]:
        """The live records (``live(record, held)``, ``held`` being :meth:`lock_held`'s
        answer with ``missing``), and the ones this release cannot read that have their lock
        held or are fresh. With ``prune``, deletes what is not live, as the module says, but
        never :data:`GATE_LOCK` nor a name in ``reserved`` (a registry's own folder-level
        files); without, writes nothing. Reads only: no folder is made."""
        folder = self.folder
        try:
            names = sorted(os.listdir(folder))
        except OSError:
            return Roll([], [], [])
        now = datetime.now(UTC)
        found: list[T] = []
        unreadable: list[Unreadable] = []
        pruned: list[str] = []
        held: set[str] = set()
        for name in names:
            path = folder / name
            stem = name.removesuffix(".json")
            if stem == name or not valid_id(stem):
                if (
                    prune
                    and name != GATE_LOCK
                    and name not in reserved
                    and self._leftover(name, names)
                ):
                    unlink(path)
                continue  # no record: not <id>.json
            loaded = self.load(stem, parse)
            if loaded is Unloaded.FAILED:  # once more, a moment later: the failures are brief
                time.sleep(READ_RETRY_SECONDS)
                loaded = self.load(stem, parse)
            if loaded is Unloaded.GONE:
                continue  # its process left since the listing
            if isinstance(loaded, Unloaded):  # unparsable, or a read that failed twice running
                written = _written(path)
                if written is None:
                    continue  # gone since
                locked = self.lock_held(stem) is True
                if locked or now - written < timedelta(seconds=self.stale_seconds):
                    unreadable.append(Unreadable(stem, path, written, locked, _object_in(path)))
                elif prune:
                    self.forget(stem)
                    pruned.append(stem)
                continue
            lock = self.lock_held(stem, missing=missing)
            if live(loaded, lock):
                found.append(loaded)
                if lock:
                    held.add(stem)
            elif prune:
                self.forget(stem)  # not live: its lock is not held
                pruned.append(stem)
        return Roll(found, unreadable, pruned, frozenset(held))

    def _leftover(self, name: str, names: list[str]) -> bool:
        """Whether ``name`` (not a record) is a file of a record that is gone, as old as a stale
        record and, when it is the lock, not held: one a reader deletes."""
        owner = _owner(name)
        if owner is None or f"{owner}.json" in names or not self._old(self.folder / name):
            return False
        return not (name == f"{owner}.lock" and self.lock_held(owner) is True)

    def _old(self, path: Path) -> bool:
        """Whether ``path`` was last written as long ago as a stale record."""
        try:
            return time.time() - path.stat().st_mtime >= self.stale_seconds
        except OSError:
            return False


def _owner(name: str) -> str | None:
    """The id whose file ``name`` is, beside its record (``<id>.lock``, a temp ``.<id>.…``);
    ``None`` for a name this release gives no meaning."""
    bare = name.removeprefix(".")
    owner = bare.split(".", 1)[0]
    return owner if owner != bare and valid_id(owner) else None


def _object_in(path: Path) -> Mapping[str, Any] | None:
    """The JSON object a record holds, however else it fails to read; ``None``."""
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, RecursionError):
        return None
    return data if isinstance(data, dict) else None


def _written(path: Path) -> datetime | None:
    try:
        return datetime.fromtimestamp(path.stat().st_mtime, UTC)
    except OSError:
        return None
