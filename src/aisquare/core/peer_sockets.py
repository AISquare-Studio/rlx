"""The one folder the sessions rlx launches put their messaging sockets under, on every account.

An agent whose sessions message each other over sockets says where it puts them
(``capabilities.peer_sockets``): Claude Code under ``$XDG_RUNTIME_DIR``, else under its
account's scratch dir. Each managed account has a scratch dir of its own
(``core.claude_accounts``, "Both variables, always"), so where ``XDG_RUNTIME_DIR`` is unset, as
on macOS, each account's sessions would keep their sockets in a folder of that account's own.
A launch whose environment names no folder is given this one, under the aisquare home
(:func:`shared`), made ``0700``; a launch whose environment names one (most Linux desktops set
``XDG_RUNTIME_DIR``) keeps it, a blank value counting as none, as the desktop variables a
window carries do (``core.tmux.desktop_environment``). The scratch dirs stay each account's own.

The value a launch sets is rlx's, not the desktop's, and the launch records it
(:data:`SET_ENV`): everything rlx forwards onward from that session (a window's desktop
variables, a tmux client's environment, a launch of an agent without peer sockets, the
captain's Actions server) drops it (:func:`drop_set`), so those see what they did before the
shared folder.
"""

from __future__ import annotations

import os
import stat
import sys
from collections.abc import Mapping, MutableMapping
from pathlib import Path
from typing import TYPE_CHECKING

from aisquare.core import paths

if TYPE_CHECKING:
    from aisquare.core.agent_adapters.types import PeerSockets

WIDEST_PID = 4_194_304
"""The widest pid a socket is named for: Linux's ceiling for ``pid_max``; macOS's pids are
narrower."""

SET_ENV = "AISQUARE_PEER_SOCKETS_SET"
"""Where a launch that named the shared folder records it, as ``<variable>=<folder>``."""


def shared() -> Path:
    """The shared folder: ``paths.messaging_dir`` with its parent's links resolved. A linked
    aisquare home is the operator's own layout; resolved, the folder is made under it without
    following a link (:func:`apply`), and the variable names a path with none in it."""
    under = paths.messaging_dir()
    return under.parent.resolve() / under.name


def set_by_rlx(env: Mapping[str, str]) -> str | None:
    """The variable whose value in ``env`` is the one a launch set (:data:`SET_ENV`), or
    ``None``: a value the desktop or the operator set, or one changed since, is not."""
    name, separator, value = env.get(SET_ENV, "").partition("=")
    return name if separator and name and env.get(name) == value else None


def drop_set(env: MutableMapping[str, str]) -> None:
    """Take the value a launch set (:func:`set_by_rlx`), and its record, out of ``env``, in
    place."""
    name = set_by_rlx(env)
    env.pop(SET_ENV, None)
    if name is not None:
        env.pop(name, None)


def named(env: Mapping[str, str], sockets: PeerSockets) -> str | None:
    """The folder ``env`` names of its own, as written, or ``None``: blank counts as unset, and
    a value a launch set is the shared folder's."""
    value = env.get(sockets.folder_env, "")
    if not value.strip() or set_by_rlx(env) == sockets.folder_env:
        return None
    return value


def too_long(where: Path, sockets: PeerSockets) -> str | None:
    """Why ``where`` is too long for the sessions' sockets, or ``None``: its longest socket
    path (:data:`WIDEST_PID`) must fit ``sockets.max_bytes``."""
    longest = where / sockets.subfolder / f"{WIDEST_PID}.sock"
    size = len(os.fsencode(str(longest)))
    if size > sockets.max_bytes:
        return f"{longest} is {size} bytes, over the {sockets.max_bytes} a socket path may take"
    return None


def unusable(where: Path) -> str | None:
    """Why ``where`` cannot hold the sessions' sockets, or ``None``.

    It must exist, as a directory (not a link to one) of this user's with mode ``0700``
    exactly: its owner makes the sockets' subfolder in it, and the XDG spec asks a runtime dir
    to be closed to everyone else, as other programs in the session read the variable too.
    """
    try:
        facts = where.lstat()
    except FileNotFoundError:
        return f"{where} does not exist"
    except OSError as exc:
        return f"{where} cannot be read ({exc.strerror})"
    if not stat.S_ISDIR(facts.st_mode):
        return f"{where} is not a directory"
    getuid = getattr(os, "getuid", None)
    if getuid is not None and facts.st_uid != getuid():
        return f"{where} is owned by uid {facts.st_uid}, not by this user"
    if stat.S_IMODE(facts.st_mode) != 0o700:
        return f"{where} has mode {stat.S_IMODE(facts.st_mode):04o}, not 0700"
    return None


def cannot_make(where: Path) -> str | None:
    """Why a launch cannot make ``where``, or ``None``: it makes the folder alone, never its
    parent, so the parent must be a directory this user may write."""
    parent = where.parent
    if not parent.is_dir():
        return f"{parent} is not a directory"
    if not os.access(parent, os.W_OK | os.X_OK):
        return f"{parent} is not writable by this user"
    return None


def apply(env: dict[str, str], sockets: PeerSockets | None) -> dict[str, str]:
    """Make ``env`` launch an agent with ``sockets``, in place, and return it.

    An agent without peer sockets gets ``env`` without the value a launch set
    (:func:`drop_set`). One with them gets the shared folder only where ``env`` names no
    folder of its own (:func:`named`), never on Windows (no unix sockets: Claude Code's are
    named pipes there), and only a folder a session can use: short enough (:func:`too_long`),
    made ``0700`` under an existing parent, and left out while :func:`unusable` has a reason.
    The doctor's row names the reason.
    """
    if sockets is None:
        drop_set(env)
        return env
    if sys.platform == "win32" or env.get(sockets.folder_env, "").strip():
        return env
    where = shared()
    if too_long(where, sockets) is not None:
        return env
    try:
        where.mkdir(mode=0o700, exist_ok=True)
    except OSError:
        return env  # the doctor's row says why
    if unusable(where) is None:
        env[sockets.folder_env] = str(where)
        env[SET_ENV] = f"{sockets.folder_env}={where}"
    return env
