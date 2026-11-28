"""Each agent's own temporary directory: its ``TMPDIR`` (``TEMP`` and ``TMP`` on Windows), #57.

Whatever an agent's tools leave in the temporary directory (pytest's basetemp, cargo's and
Go's test files, a ``mktemp``) lands in one directory rlx owns and removes when the agent
ends, whatever the language. The system's ``$TMPDIR`` is never written to or cleaned.

**Where.** ``<home>/tmp/<key>`` (:func:`aisquare.core.paths.agent_tmp_dir`), made ``0700`` by
the agent's ``aisquare launch`` (:func:`prepare`), before the sandbox and broker steps, which
model a sandboxed agent's writable roots from the launcher's own ``TMPDIR``. A ``TMPDIR`` the
role's binding or ``launch --env`` sets is the operator's and wins. ``TMUX_TMPDIR`` is never
read or set: tmux keeps its sockets there or in ``/tmp``, never under ``TMPDIR``.

**Short.** macOS caps a unix socket's path at 104 bytes, and a test that binds one in pytest's
``tmp_path`` adds about 66 to its ``TMPDIR``. ``/Users/<name>/.rlx/tmp/<key>`` is about 28
bytes; macOS's own default (``/var/folders/…/T/``) is 49. A home that would make it longer than
:data:`MAX_BYTES` keeps the inherited ``TMPDIR``, and the launch says so in one line.

**The key.** A fleet agent's is minted at its first spawn and kept in its launch spec
(``LaunchSpec.tmp_key``), which every replacement replays: a restart, a switch, a hand-over
and a resume keep the directory and the files in it. A relabel keeps it too; a fork mints
its own. A launch by hand gets a fresh one. Beside each directory (never in it) is its owner
record, ``<key>.owner``: the process it was made for, and whether a fleet spec carries it.

**Removal.** :func:`ended_dirs` is the one selection, read-only; :func:`sweep_ended` removes it
(the fleet's end and reap paths, the watcher's tick), and ``rlx tidy`` plans and removes the
same list. A directory is ended when the process it was made for no longer runs, and:

- a fleet key's newest row is ended, not paused, and ended at least :data:`GRACE` ago (the
  gap a hand-over leaves between the old row's end and the new row's insert); or no row
  carries the key any more and the record is older than :data:`GRACE`;
- a launch by hand's process is gone (or its pid is another process's now);
- a directory with no owner record is older than :data:`ORPHAN_AGE`.

Liveness decides, never age alone; a paused agent's directory is never touched. Nothing
outside ``<home>/tmp`` is removed, and a link is never followed.
"""

from __future__ import annotations

import contextlib
import json
import os
import secrets
import shutil
import stat
import sys
import tempfile
import time
from collections.abc import Collection, Mapping, MutableMapping
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Literal

from aisquare.core import atomic, paths, procs
from aisquare.core.spawn import OUTER_TMPDIR_ENV, TMP_KEY_ENV
from aisquare.core.store import ContextStore, store_session
from aisquare.models import FleetAgent, LaunchSpec

VARS: tuple[str, ...] = ("TEMP", "TMP") if sys.platform == "win32" else ("TMPDIR",)
"""The variables that name the temporary directory here."""

MAX_BYTES = 49
"""The longest agent directory a launch uses: macOS's own ``TMPDIR`` length, so nothing that
binds a socket under it today stops fitting."""

GRACE = timedelta(minutes=10)
"""How long an ended agent's directory waits: the hand-over's gap, and a restart soon after."""

ORPHAN_AGE = timedelta(days=1)
"""How old a directory with no owner record must be to go: a launch that died between the two."""

SWEEP_EVERY = 60.0
"""Seconds between two sweeps of :func:`maybe_sweep_ended` in one process (a listing's)."""

OWNER = ".owner"
_ALPHABET = "abcdefghijklmnopqrstuvwxyz234567"
KEY_LENGTH = 6


@dataclass(frozen=True)
class Owner:
    """What ``<key>.owner`` says: the process the directory was made for."""

    kind: Literal["fleet", "launch"]
    """``fleet``: a fleet spec carries the key; ``launch``: a launch by hand's alone."""
    pid: int
    started: str | None
    """``core.procs`` start token, or ``None`` when it could not be read."""
    at: datetime


def is_key(name: str) -> bool:
    """Whether ``name`` is one of ours: :data:`KEY_LENGTH` letters of :data:`_ALPHABET`."""
    return len(name) == KEY_LENGTH and all(ch in _ALPHABET for ch in name)


def new_key(taken: Collection[str] = ()) -> str:
    """A key no directory or owner record under the root has, nor ``taken``."""
    root = paths.agent_tmp_root()
    while True:
        key = "".join(secrets.choice(_ALPHABET) for _ in range(KEY_LENGTH))
        if key in taken:
            continue
        if not (root / key).exists() and not _owner_path(key).exists():
            return key


def key_for(spec: LaunchSpec | None, *, fork: bool) -> str:
    """The key a spawn gives its agent: a replay's own (``spec``), else a new one; a fork's is
    always new, as it is another agent."""
    if spec is not None and not fork and spec.tmp_key is not None and is_key(spec.tmp_key):
        return spec.tmp_key
    return new_key()


def _owner_path(key: str) -> Path:
    return paths.agent_tmp_root() / f"{key}{OWNER}"


def read_owner(key: str) -> Owner | None:
    """``key``'s owner record, or ``None`` when it is missing or unreadable."""
    try:
        raw = json.loads(_owner_path(key).read_text(encoding="utf-8"))
        kind = raw["kind"]
        if kind not in ("fleet", "launch"):
            return None
        started = raw.get("started")
        return Owner(
            kind=kind,
            pid=int(raw["pid"]),
            started=started if isinstance(started, str) else None,
            at=datetime.fromisoformat(raw["at"]),
        )
    except (OSError, ValueError, KeyError, TypeError):
        return None


def prepare(
    env: MutableMapping[str, str],
    *,
    explicit: Collection[str] = (),
    environ: MutableMapping[str, str] | None = None,
) -> str | None:
    """Make this launch's directory and point ``env`` (the agent's) and ``environ`` (this
    process's, ``os.environ`` by default) at it. Returns a line to say when it keeps the
    inherited directory instead: the home is too long for a socket under it
    (:data:`MAX_BYTES`), or the directory could not be made (a link or a file in its place,
    someone else's, a disk that refuses). ``None`` when it set the agent's own, or when
    ``explicit`` (the binding's and ``--env``'s variables) names one: the operator's.

    A fleet window's key is its spec's (``AISQUARE_TMP_KEY``, honoured only where no launch
    id is inherited: a launch from inside an agent is a launch by hand, with its own)."""
    environ = os.environ if environ is None else environ
    carried = env.pop(TMP_KEY_ENV, "")
    if any(var in explicit for var in VARS):
        return None
    fleet = not env.get("AISQUARE_LAUNCH_ID") and is_key(carried)
    key = carried if fleet else new_key()
    directory = paths.agent_tmp_dir(key)
    size = len(os.fsencode(directory))
    if sys.platform != "win32" and size > MAX_BYTES:
        return (
            f"agent temp: {directory} would be {size} bytes, over the {MAX_BYTES} that leave "
            "room for a socket under it; it keeps the inherited one"
        )
    try:
        _make(directory)
        _write_owner(key, "fleet" if fleet else "launch")
    except OSError as exc:
        why = exc.strerror or str(exc)
        return f"agent temp: {directory} could not be made ({why}); it keeps the inherited one"
    if sys.platform != "win32":
        # The true outer one: a launch from inside an agent inherited that agent's own.
        outer = env.get(OUTER_TMPDIR_ENV, env.get("TMPDIR", ""))
        env[OUTER_TMPDIR_ENV] = environ[OUTER_TMPDIR_ENV] = outer
    for var in VARS:
        env[var] = environ[var] = str(directory)
    tempfile.tempdir = None  # this process asks again: the launcher's own temp files go there too
    return None


def _make(directory: Path) -> None:
    """``directory`` and the root above it, each a real directory of this user's, ``0700``.
    A link, a file or someone else's directory in either place is refused (``OSError``)."""
    directory.parent.parent.mkdir(parents=True, exist_ok=True)  # the home
    for each in (directory.parent, directory):
        with contextlib.suppress(FileExistsError):
            each.mkdir(mode=0o700)
        info = each.lstat()
        if not stat.S_ISDIR(info.st_mode):
            raise OSError(f"{each} is not a directory of its own (a link or a file)")
        getuid = getattr(os, "getuid", None)
        if getuid is not None and info.st_uid != getuid():
            raise OSError(f"{each} belongs to another user")
        if sys.platform != "win32" and stat.S_IMODE(info.st_mode) != 0o700:
            each.chmod(0o700)


def _write_owner(key: str, kind: Literal["fleet", "launch"]) -> None:
    pid = os.getpid()
    found = procs.inspect(pid)
    record = {
        "kind": kind,
        "pid": pid,
        "started": found.started if found is not None else None,
        "at": datetime.now(UTC).isoformat(timespec="seconds"),
    }
    atomic.write_replacing(_owner_path(key), json.dumps(record), durable=False)


def ended_dirs(store: ContextStore | None = None, *, now: datetime | None = None) -> list[Path]:
    """Every agent directory under ``<home>/tmp`` whose agent has ended (the module's rules),
    oldest name first. Reads only: :func:`sweep_ended` and ``rlx tidy`` remove what it lists."""
    root = paths.agent_tmp_root()
    try:
        names = sorted(entry.name for entry in root.iterdir())
    except OSError:
        return []
    keys = [name for name in names if is_key(name) and _real_dir(root / name)]
    if not keys:
        return []
    if store is None:
        with store_session() as opened:
            heads = opened.fleet_label_heads()
    else:
        heads = store.fleet_label_heads()
    newest: dict[str, FleetAgent] = {}
    for row in heads:
        key = row.launch_spec.tmp_key if row.launch_spec is not None else None
        if key is not None and (key not in newest or row.created_at >= newest[key].created_at):
            newest[key] = row
    moment = now or datetime.now(UTC)
    return [
        root / key
        for key in keys
        if _agent_gone(root / key, read_owner(key), newest.get(key), moment)
    ]


def _real_dir(path: Path) -> bool:
    try:
        return stat.S_ISDIR(path.lstat().st_mode)
    except OSError:
        return False


def _agent_gone(
    directory: Path, owner: Owner | None, row: FleetAgent | None, now: datetime
) -> bool:
    if owner is not None and _owner_runs(owner):
        return False
    if owner is not None and owner.kind == "launch":
        return True
    if row is not None:
        if row.ended_at is None or row.paused_at is not None:
            return False
        return now - row.ended_at >= GRACE
    if owner is not None:
        return now - owner.at >= GRACE
    try:
        made = datetime.fromtimestamp(directory.lstat().st_mtime, UTC)
    except OSError:
        return False
    return now - made >= ORPHAN_AGE


def _owner_runs(owner: Owner) -> bool:
    """Whether the process the directory was made for still runs. A pid another process has
    now is not it; one that cannot be told apart is taken to run (the safe side)."""
    if not procs.alive(owner.pid):
        return False
    if owner.started is None:
        return True
    found = procs.inspect(owner.pid)
    return found is None or found.started == owner.started


def sweep_ended(store: ContextStore | None = None, *, now: datetime | None = None) -> list[Path]:
    """Remove every :func:`ended_dirs` directory and its owner record; the ones removed. A
    directory that cannot be removed is left for the next sweep."""
    removed: list[Path] = []
    for directory in ended_dirs(store, now=now):
        if remove_dir(directory):
            removed.append(directory)
    return removed


def remove_dir(directory: Path) -> bool:
    """Remove one agent directory and its owner record: only a real directory directly under
    ``<home>/tmp`` whose name is a key. What the agent left read-only is made writable first
    (never through a link), so nothing stays behind for good. Whether it is gone."""
    root = paths.agent_tmp_root()
    if directory.parent != root or not is_key(directory.name) or not _real_dir(directory):
        return False
    _writable_within(directory)
    shutil.rmtree(directory, ignore_errors=True)
    if directory.exists():
        return False
    _owner_path(directory.name).unlink(missing_ok=True)
    return True


def _writable_within(top: Path) -> None:
    """Give this user write and search on every directory under ``top``, and write on every
    file on Windows (where a read-only file cannot be deleted), before removal. A link is
    never followed nor changed: ``os.walk`` does not descend into one, and each is skipped."""

    def own(path: str, mode: int) -> None:
        with contextlib.suppress(OSError):
            if not os.path.islink(path):
                os.chmod(path, mode)

    own(str(top), stat.S_IRWXU)
    for here, dirs, files in os.walk(top, followlinks=False):
        for name in dirs:
            own(os.path.join(here, name), stat.S_IRWXU)
        if sys.platform == "win32":
            for name in files:
                own(os.path.join(here, name), stat.S_IREAD | stat.S_IWRITE)


def own_dir(env: Mapping[str, str]) -> Path | None:
    """The agent's own directory ``env`` names as its temporary directory, resolved: a key's
    directory directly under ``<home>/tmp``. ``None`` for any other (the inherited one, the
    operator's): what a sandbox that holds the home read-only must still let it write."""
    named = env.get(VARS[0], "")
    if not named:
        return None
    try:
        root = paths.agent_tmp_root().resolve()
        resolved = Path(named).resolve()
    except (OSError, RuntimeError):
        return None
    if resolved.parent != root or not is_key(resolved.name):
        return None
    return resolved


_last_sweep = 0.0


def maybe_sweep_ended() -> list[Path]:
    """:func:`sweep_ended`, at most every :data:`SWEEP_EVERY` seconds in this process: for a path
    every listing runs. Never raises: a sweep that fails costs nothing but the sweep."""
    global _last_sweep
    moment = time.monotonic()
    if _last_sweep and moment - _last_sweep < SWEEP_EVERY:
        return []
    _last_sweep = moment
    try:
        return sweep_ended()
    except Exception:
        return []
