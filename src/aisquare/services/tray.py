"""The rlx service's menu bar / tray icon (#34): what it shows and what its menu does.

It runs in a process of its own, ``rlx service tray``, which the service starts and stops
(``services.service_tray.TrayPart``): never in the service, whose idle budget an AppKit or a
session bus would break, and never in the CLI or a hook (``tests/test_tray_imports.py``).
This module is the platform's half that needs no platform: the menu built from what the
service last wrote (``status.json``), the question *Quit rlx* asks, the terminal *Open rlx*
starts, and the two commands *Restart* and *Quit* hand to a detached rlx. Each OS's half
draws it: ``services.tray_macos`` (an ``NSStatusItem``, pyobjc) and ``services.tray_linux``
(a StatusNotifierItem and its dbusmenu, jeepney), both in the ``tray`` extra and imported by
:func:`run_tray` alone.

The menu is built when it opens, from the file: the tray polls nothing while it is idle.

- macOS: a status item with a menu opens it on any click, and the menu bar has no
  double-click: *Open rlx* is the menu's first item there (the brief's double-click, as macOS
  allows it; the ruling on #34, Q2; docs/service.md says so). *Quit rlx* asks in an alert.
- Linux: the host's activation (a double-click, or a click, as the desktop has it) opens rlx;
  the menu is the context menu. A StatusNotifierItem has no dialog, so *Quit rlx* opens a
  submenu that says what quitting costs and holds *Quit rlx now*.
"""

from __future__ import annotations

import json
import logging
import os
import shlex
import shutil
import subprocess
import sys
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Protocol

from aisquare.core import selfcli
from aisquare.core.version import PRODUCT_NAME
from aisquare.services.service_tray import (
    EXIT_NO_EXTRA,
    EXIT_NOT_HERE,
    NO_TRAY_EXITS,
)

_log = logging.getLogger(__name__)

OPEN = "open"
RESTART = "restart"
QUIT = "quit"
"""Ask first: macOS's alert. On Linux *Quit rlx* is the submenu that holds :data:`QUIT_NOW`."""
QUIT_NOW = "quit-now"

NAMED_AT_MOST = 3
"""Agents the question names before it counts the rest (``coder-1, coder-2, reviewer and 4
more``): a menu or an alert line, not a list."""

TERMINALS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("x-terminal-emulator", ("-e",)),
    ("gnome-terminal", ("--",)),
    ("konsole", ("-e",)),
    ("xfce4-terminal", ("-x",)),
    ("kitty", ()),
    ("alacritty", ("-e",)),
    ("foot", ()),
    ("wezterm", ("start", "--")),
    ("xterm", ("-e",)),
)
"""The terminals Linux falls back on, in order, after ``xdg-terminal-exec`` and
``$TERMINAL``, each with what goes before the command it runs. A ``[tray] terminal`` naming one
of them is run that way; any other is given ``-e``."""

COMMAND_FILE = "open-rlx.command"
"""On macOS, the script *Open rlx* opens (in the service's folder, ``0700``): the app the Mac
opens a ``.command`` file with, Terminal or the person's own, runs it; no AppleScript, so no
Automation prompt."""


class TrayError(Exception):
    """What an action could not do; the message is the line the menu shows next."""


# --- what the service last said -----------------------------------------------------------------


@dataclass(frozen=True)
class Summary:
    """The menu's status lines, from the service's ``status.json``."""

    running: bool
    agents: tuple[str, ...] = ()
    """The running agents' names, as many as the service keeps (``service_tray.AGENTS_KEPT``)."""
    room: int | None = None
    accounts: int | None = None
    running_count: int | None = None
    """How many run, all of them (``agents_running``): more than :attr:`agents` names."""

    @property
    def count(self) -> int:
        return max(self.running_count or 0, len(self.agents))


def summary_of(record: Mapping[str, Any] | None) -> Summary:
    """What ``record`` (``service_host.read_status``'s) says for the menu; a missing or odd
    one is a service that is not running, said as such, never an error."""
    if not isinstance(record, Mapping):
        return Summary(running=False)
    parts = record.get("parts")
    parts = parts if isinstance(parts, Mapping) else {}
    tray = parts.get("tray") if isinstance(parts.get("tray"), Mapping) else {}
    usage = parts.get("usage") if isinstance(parts.get("usage"), Mapping) else {}
    agents = tray.get("agents") if isinstance(tray, Mapping) else None
    names = tuple(str(name) for name in agents) if isinstance(agents, list) else ()
    room = usage.get("room") if isinstance(usage, Mapping) else None
    accounts = usage.get("accounts") if isinstance(usage, Mapping) else None
    total = tray.get("agents_running") if isinstance(tray, Mapping) else None
    return Summary(
        running=True,
        agents=names,
        room=_whole(room),
        accounts=_whole(accounts),
        running_count=_whole(total),
    )


def _whole(value: object) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def named(agents: Sequence[str], count: int | None = None) -> str:
    """``agents`` as one phrase: ``coder-1``, ``coder-1 and reviewer``, ``a, b, c and 2 more``;
    ``count`` the whole number when it is more than the names (the service keeps 20)."""
    if not agents:
        return ""
    shown = list(agents[:NAMED_AT_MOST])
    rest = max(count or 0, len(agents)) - len(shown)
    if rest:
        return f"{', '.join(shown)} and {rest} more"
    if len(shown) == 1:
        return shown[0]
    return f"{', '.join(shown[:-1])} and {shown[-1]}"


def status_lines(summary: Summary) -> list[str]:
    """The menu's top lines, which do nothing when clicked."""
    if not summary.running:
        return [f"The {PRODUCT_NAME} service is not running"]
    count = summary.count
    lines = [
        "No agents running" if count == 0 else f"{count} agent{'s' if count != 1 else ''} running"
    ]
    if summary.accounts:
        lines.append(f"Accounts with room: {summary.room or 0} of {summary.accounts}")
    return lines


def quit_question(agents: Sequence[str], count: int | None = None) -> tuple[str, str]:
    """*Quit rlx*'s question: its title and what quitting costs. Quitting stops the service
    only (the ruling on #34, Q1): agents run on in tmux, without what it did for them.
    ``count``: how many run, when more than ``agents`` names."""
    title = f"Quit {PRODUCT_NAME}?"
    if not agents:
        return title, "Remote and the usage reads stop until rlx starts again."
    verb = "keeps" if max(count or 0, len(agents)) == 1 else "keep"
    return title, (
        f"{named(agents, count)} {verb} running in tmux, without rlx: no handovers at their "
        "limits, and no Remote."
    )


# --- the menu -------------------------------------------------------------------------------------


@dataclass(frozen=True)
class MenuItem:
    """One row: a ``label``, the ``action`` a click takes (``None``: a line, which does
    nothing), whether it is the menu's default, and a submenu's rows."""

    label: str
    action: str | None = None
    default: bool = False
    children: tuple[MenuItem, ...] = ()
    separator: bool = False


SEPARATOR = MenuItem("", separator=True)


def menu_items(summary: Summary, *, error: str | None = None, ask_in_menu: bool) -> list[MenuItem]:
    """The menu as it opens, in the brief's order: *Open rlx* (first, the default), *Restart
    rlx*, *Quit rlx…*, then the status lines and what went wrong last, which do nothing when
    clicked. ``ask_in_menu``: *Quit rlx…* is a submenu that asks (Linux), not an item whose
    alert asks (macOS)."""
    rows = [
        MenuItem(f"Open {PRODUCT_NAME}", OPEN, default=True),
        MenuItem(f"Restart {PRODUCT_NAME}", RESTART),
    ]
    if ask_in_menu:
        _, detail = quit_question(summary.agents, summary.count)
        rows.append(
            MenuItem(
                f"Quit {PRODUCT_NAME}…",
                children=(MenuItem(detail), MenuItem(f"Quit {PRODUCT_NAME} now", QUIT_NOW)),
            )
        )
    else:
        rows.append(MenuItem(f"Quit {PRODUCT_NAME}…", QUIT))
    rows.append(SEPARATOR)
    rows += [MenuItem(line) for line in status_lines(summary)]
    if error:
        rows.append(MenuItem(error))
    return rows


# --- Open rlx -----------------------------------------------------------------------------------


def terminal_argv(
    platform: str,
    *,
    rlx: Sequence[str],
    choice: str = "",
    env: Mapping[str, str] | None = None,
    which: Callable[[str], str | None] | None = None,
    command_file: Path | None = None,
) -> list[str]:
    """The command that opens a terminal running ``rlx`` (an argv) on ``platform``.

    macOS: ``open`` on ``command_file`` (the app the Mac opens a ``.command`` with), or ``open
    -a <choice>`` on it. Linux: ``choice`` if set, else ``xdg-terminal-exec``, ``$TERMINAL``,
    then the first of :data:`TERMINALS` found. :class:`TrayError` when there is none."""
    source = os.environ if env is None else env
    which = which or shutil.which
    if platform == "darwin":
        if command_file is None:
            raise TrayError("no script to open rlx with")
        if choice:
            return ["open", "-a", choice, str(command_file)]
        return ["open", str(command_file)]
    if choice:
        words = shlex.split(choice)
        known = dict(TERMINALS)
        lead = known.get(Path(words[0]).name, ("-e",)) if len(words) == 1 else ()
        return [*words, *lead, *rlx]
    if which("xdg-terminal-exec"):
        return ["xdg-terminal-exec", *rlx]
    terminal = source.get("TERMINAL", "").strip()
    if terminal:
        return [*shlex.split(terminal), "-e", *rlx]
    for name, lead in TERMINALS:
        if which(name):
            return [name, *lead, *rlx]
    raise TrayError(
        "no terminal app found to open rlx in: name one in App settings ▸ User preferences ▸ Tray"
    )


def command_script(rlx: Sequence[str], env: Mapping[str, str]) -> str:
    """The ``.command`` file's text: the home this service runs on, then rlx."""
    lines = ["#!/bin/sh"]
    for name in ("RLX_HOME", "AISQUARE_HOME"):
        value = env.get(name)
        if value:
            lines.append(f"export {name}={shlex.quote(value)}")
    lines.append(f"exec {shlex.join(rlx)}")
    return "\n".join(lines) + "\n"


def _spawn_terminal(argv: Sequence[str]) -> None:
    """The one place the tray starts a terminal (``core.spawn.SEAMS``): on its own, so it
    outlives the tray, and without any agent's tracing identity. The suite replaces this
    (``tests/conftest.py::no_real_tray``)."""
    from aisquare.core import spawn

    subprocess.Popen(
        list(argv),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
        close_fds=True,
        env=spawn.untraced_env(),
    )


def _spawn_rlx(args: Sequence[str]) -> None:
    """The one place the tray hands a command to rlx (``core.spawn.SEAMS``): ``rlx service
    restart`` or ``stop``, detached, since either stops the service and the service stops
    this tray. The suite replaces this (``tests/conftest.py::no_real_tray``)."""
    from aisquare.core import spawn

    subprocess.Popen(
        selfcli.argv_for(list(args)),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
        close_fds=True,
        env=spawn.untraced_env(),
    )


# --- the tray -----------------------------------------------------------------------------------


@dataclass
class Tray:
    """What a backend draws and calls: the menu as it opens, and each row's action. The
    backend is the OS's half; this is the same on every OS. ``status`` and ``folder`` are the
    service's ``status.json`` and its folder, as the service passes them (``TrayPart``)."""

    platform: str = sys.platform
    read_status: Callable[[], Mapping[str, Any] | None] | None = None
    terminal: str = ""
    status: Path | None = None
    folder: Path | None = None
    service_pid: int | None = None
    """The service's pid, as it passed it (``--service-pid``): the parent the tray ends with.
    Passed, not read with ``getppid`` once the OS's half has loaded: a service that died during
    those imports would leave a tray whose parent is already another process."""
    error: str | None = field(default=None)

    def service_gone(self) -> bool:
        """Whether the service that started this tray has ended (its child's parent changed)."""
        return self.service_pid is not None and os.getppid() != self.service_pid

    def summary(self) -> Summary:
        try:
            return summary_of(self._read())
        except Exception:  # an unreadable status is a menu that says so, never a dead tray
            _log.warning("the tray could not read the service's status", exc_info=True)
            return Summary(running=False)

    def _read(self) -> Mapping[str, Any] | None:
        if self.read_status is not None:
            return self.read_status()
        if self.status is not None:
            try:
                data = json.loads(self.status.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                return None
            return data if isinstance(data, dict) else None
        from aisquare.services import service_host

        return service_host.read_status()

    def items(self) -> list[MenuItem]:
        return menu_items(self.summary(), error=self.error, ask_in_menu=self.platform != "darwin")

    def question(self) -> tuple[str, str]:
        """What macOS's alert asks before :data:`QUIT_NOW`."""
        summary = self.summary()
        return quit_question(summary.agents, summary.count)

    def act(self, action: str) -> None:
        """Do what a row's ``action`` names; what went wrong is the menu's next line."""
        self.error = None
        try:
            if action == OPEN:
                self.open_rlx()
            elif action == RESTART:
                _spawn_rlx(["service", "restart"])
            elif action == QUIT_NOW:
                _spawn_rlx(["service", "stop"])
            elif action != QUIT:  # QUIT is the backend's to ask
                raise TrayError(f"nothing to do for {action!r}")
        except (OSError, TrayError, ValueError) as exc:  # ValueError: a quote left open
            _log.warning("the tray's %s failed: %s", action, exc)
            self.error = f"Could not {action.replace('-', ' ')}: {exc}"

    def open_rlx(self) -> None:
        rlx = selfcli.argv_for([])
        script: Path | None = None
        if self.platform == "darwin":
            folder = self.folder
            if folder is None:
                from aisquare.services import service_host

                folder = service_host.service_dir()
            script = folder / COMMAND_FILE
            script.write_text(command_script(rlx, os.environ), encoding="utf-8")
            script.chmod(0o700)
        argv = terminal_argv(self.platform, rlx=rlx, choice=self.terminal, command_file=script)
        _spawn_terminal(argv)


class Backend(Protocol):
    """An OS's half: draws :class:`Tray` until the service stops it; its exit status."""

    def run(self, tray: Tray, *, icon: str) -> int: ...


def _in_wsl() -> bool:
    from aisquare.core.clipboard import in_wsl

    return in_wsl(os.environ)


def backend_for(platform: str = sys.platform) -> Backend | int:
    """The OS's half, or the exit that says why there is none (:data:`NO_TRAY_EXITS`). The
    extra's modules are imported here, the only place: never by the CLI, a hook or the
    service (``tests/test_tray_imports.py``)."""
    if platform == "darwin":
        try:
            from aisquare.services import tray_macos
        except ImportError:
            return EXIT_NO_EXTRA
        return tray_macos.MacTray()
    if platform.startswith("linux") and not _in_wsl():
        try:
            from aisquare.services import tray_linux
        except ImportError:
            return EXIT_NO_EXTRA
        return tray_linux.LinuxTray()
    return EXIT_NOT_HERE


def run_tray(
    *,
    icon: str = "backslash",
    terminal: str = "",
    status: Path | None = None,
    folder: Path | None = None,
    service_pid: int | None = None,
    backend: Backend | int | None = None,
) -> int:
    """Draw the tray until the service stops it; the exit status, one of
    :data:`NO_TRAY_EXITS` when there is no tray here."""
    chosen = backend_for() if backend is None else backend
    if isinstance(chosen, int):
        _log.info("no tray: %s", NO_TRAY_EXITS.get(chosen, chosen))
        return chosen
    shown = Tray(terminal=terminal, status=status, folder=folder, service_pid=service_pid)
    if shown.service_gone():  # the service ended while the OS's half loaded: nothing to show
        _log.info("no tray: the service that started it has ended")
        return 0
    return chosen.run(shown, icon=icon)


def main(argv: Sequence[str] | None = None) -> int:
    """``python -m aisquare.services.tray_process``: the process the service starts
    (``TrayPart``).

    Not ``rlx service tray``: the CLI imports every command (about 68 MB measured), where this
    module and what it needs is about 27 MB, before the OS's half. So the service hands it
    ``[tray]`` and its own paths on the command line, and it reads no config."""
    import argparse

    parser = argparse.ArgumentParser(prog="rlx-tray")
    parser.add_argument("--icon", default="backslash")
    parser.add_argument("--terminal", default="")
    parser.add_argument("--status", type=Path)
    parser.add_argument("--folder", type=Path)
    parser.add_argument("--service-pid", type=int)
    args = parser.parse_args(argv)
    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s"
    )
    return run_tray(
        icon=args.icon,
        terminal=args.terminal,
        status=args.status,
        folder=args.folder,
        service_pid=args.service_pid,
    )
