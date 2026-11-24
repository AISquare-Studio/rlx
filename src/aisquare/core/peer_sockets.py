"""The one folder the sessions rlx launches put their messaging sockets under, on every account.

An agent whose sessions message each other over sockets says where it puts them
(``capabilities.peer_sockets``): Claude Code under ``$XDG_RUNTIME_DIR``, else under its
account's scratch dir. Each managed account has a scratch dir of its own
(``core.claude_accounts``, "Both variables, always"), so where ``XDG_RUNTIME_DIR`` is unset, as
on macOS, each account's sessions would keep their sockets in a folder of that account's own.
A launch whose environment names no folder is given this one, under the aisquare home
(``paths.messaging_dir``), made ``0700``; a launch whose environment names one (most Linux
desktops set ``XDG_RUNTIME_DIR``) keeps it. The scratch dirs stay each account's own.
"""

from __future__ import annotations

import os
import stat
import sys
from collections.abc import Mapping
from pathlib import Path
from typing import TYPE_CHECKING

from aisquare.core import paths

if TYPE_CHECKING:
    from aisquare.core.agent_adapters.types import PeerSockets

WIDEST_PID = 4_194_304
"""The widest pid a socket is named for: Linux's ceiling for ``pid_max``; macOS's pids are
narrower."""


def folder(env: Mapping[str, str], sockets: PeerSockets) -> Path:
    """The folder a session launched with ``env`` puts its socket under: the one ``env``
    names, else the shared one, as :func:`apply` would name it."""
    named = env.get(sockets.folder_env, "").strip()
    return Path(named) if named else paths.messaging_dir().absolute()


def problem(where: Path, sockets: PeerSockets) -> str | None:
    """Why a session cannot put its socket under ``where``, or ``None`` when it can.

    Its longest socket path (:data:`WIDEST_PID`) must fit ``sockets.max_bytes``, and a folder
    that exists must be a directory of this user's, closed to everyone else (``0700``), as the
    XDG spec asks of a runtime dir: other programs in the session read the variable too. A
    folder not made yet has only the length to fail.
    """
    longest = where / sockets.subfolder / f"{WIDEST_PID}.sock"
    size = len(os.fsencode(str(longest)))
    if size > sockets.max_bytes:
        return f"{longest} is {size} bytes, over the {sockets.max_bytes} a socket path may take"
    try:
        facts = where.lstat()
    except FileNotFoundError:
        return None
    except OSError as exc:
        return f"{where} cannot be read ({exc.strerror})"
    if not stat.S_ISDIR(facts.st_mode):
        return f"{where} is not a directory"
    getuid = getattr(os, "getuid", None)
    if getuid is not None and facts.st_uid != getuid():
        return f"{where} is owned by uid {facts.st_uid}, not by this user"
    if facts.st_mode & 0o077:
        return f"{where} has mode {stat.S_IMODE(facts.st_mode):04o}, not 0700"
    return None


def apply(env: dict[str, str], sockets: PeerSockets | None) -> dict[str, str]:
    """Name the shared folder in ``env``, in place, for a launch of an agent with ``sockets``,
    and return ``env``.

    Only where ``env`` names no folder of its own, never on Windows (no unix sockets: Claude
    Code's are named pipes there), and only a folder a session can use: made ``0700`` here, and
    left out of ``env`` while :func:`problem` has a reason, which the doctor's row names.
    """
    if sockets is None or sys.platform == "win32" or env.get(sockets.folder_env, "").strip():
        return env
    shared = folder(env, sockets)
    try:
        shared.mkdir(mode=0o700, parents=True, exist_ok=True)
    except OSError:
        return env  # the doctor's row says why
    if problem(shared, sockets) is None:
        env[sockets.folder_env] = str(shared)
    return env
