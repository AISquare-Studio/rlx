"""The rlx service's tray part (#34): the tray icon in a process of its own, kept running.

The service imports this, and nothing of the tray itself: ``services.tray``, its backends and
their libraries (pyobjc, jeepney) load in ``rlx service tray``, the child this part starts,
never in the service, the CLI or a hook (``tests/test_tray_imports.py``). An AppKit in the
service would take it past its idle budget (docs/service.md), and a tray's crash would take
Remote and the handovers with it.

The part, every :data:`TRAY_SECONDS`:

- starts the child when the tray is wanted (``[tray] enabled``, an OS with a tray in this
  release, the ``tray`` extra installed, a session with a place for an icon) and none runs;
- says, in the child's health, which agents run, for the menu's line and *Quit rlx*'s
  question: the one count upgrade and uninstall act on (``lifecycle.running_fleet``);
- starts it again after a crash, but starts it at most :data:`TRAY_TRIES` times per service
  start, the first start included; and never again after an exit that means there is no tray
  here (``tray.NO_TRAY_EXITS``);
- follows App settings: a change to ``[tray]`` stops the child, and the next tick starts one
  that reads it, or none when it was switched off;
- stops it with the service.
"""

from __future__ import annotations

import importlib.util
import logging
import os
import subprocess
import sys
from collections.abc import Callable, Mapping, Sequence
from typing import TYPE_CHECKING

from aisquare.core import selfcli
from aisquare.core.version import PRODUCT_NAME

if TYPE_CHECKING:
    from aisquare.services.service_host import PartHealth

_log = logging.getLogger(__name__)

TRAY_MODULE = "aisquare.services.tray_process"
"""What the child runs, ``python -m`` (``tray.main``): not the CLI, which imports every
command (about 68 MB measured, against about 27 MB for this before the OS's half)."""
TRAY_SECONDS = 30.0
"""How often the part looks at its child and the agents: two wakeups a minute."""
TRAY_FIRST_SECONDS = 2.0
"""The child's start after the service's: Remote and the usage reads first."""
TRAY_TRIES = 5
"""How many times the part starts the tray per service start, the first start included: at the
fifth crash it says so (``error``) and leaves it, until the service starts again."""
STOP_WAIT_SECONDS = 3.0
AGENTS_KEPT = 20
"""Agents' names the status holds for the menu, which names three and counts the rest."""

EXTRA_MODULES = {"darwin": "objc", "linux": "jeepney"}
"""The module whose presence says the ``tray`` extra is installed, by OS: looked for, never
imported (``importlib.util.find_spec``)."""
EXIT_NO_EXTRA = 3
"""``rlx service tray``'s exit when the ``tray`` extra is not installed."""
EXIT_NO_HOST = 4
"""... when there is no place for an icon here: no menu bar in this session (macOS over SSH),
or no StatusNotifierItem host on the session bus (GNOME without the AppIndicator extension)."""
EXIT_NOT_HERE = 5
"""... on an OS with no tray in this release (Windows, native or WSL: 0.10.3's)."""
NO_TRAY_EXITS = {
    EXIT_NO_EXTRA: f"the tray needs {PRODUCT_NAME}'s tray extra, which is not installed",
    EXIT_NO_HOST: "this session has no place for a tray icon",
    EXIT_NOT_HERE: "no tray on this OS in this release",
}
"""What each exit that means *no tray here* says: the part never starts the tray again after
one of them, until the service starts again itself. Here, not in ``services.tray``, which the
service never imports (tests/test_tray_imports.py)."""


def why_no_tray(
    *,
    platform: str = sys.platform,
    enabled: bool | None = None,
    env: dict[str, str] | None = None,
    remote_session: Callable[[], bool] | None = None,
    has_module: Callable[[str], bool] | None = None,
) -> str | None:
    """Why the service shows no tray, or ``None`` when it does: checked before any process
    is started for it, so a server with no desktop pays nothing."""
    source = os.environ if env is None else env
    if enabled is None:
        enabled = _enabled()
    if not enabled:
        return "off in App settings"
    if platform == "darwin":
        out_of_reach = remote_session or _remote_session
        if out_of_reach():
            return "the menu bar is in the Mac's login session; this service was started over SSH"
    elif platform.startswith("linux") and not _in_wsl(source):
        if not (source.get("WAYLAND_DISPLAY") or source.get("DISPLAY")):
            return "no desktop session here"
        if not source.get("DBUS_SESSION_BUS_ADDRESS") and not source.get("XDG_RUNTIME_DIR"):
            return "no session bus here"
    else:
        return NO_TRAY_EXITS[EXIT_NOT_HERE]
    module = EXTRA_MODULES["darwin" if platform == "darwin" else "linux"]
    found = has_module or _has_module
    if not found(module):
        return NO_TRAY_EXITS[EXIT_NO_EXTRA]
    return None


def _in_wsl(env: Mapping[str, str]) -> bool:
    from aisquare.core.clipboard import in_wsl

    return in_wsl(env)


def _enabled() -> bool:
    return tray_settings()[0]


def tray_settings() -> tuple[bool, str, str]:
    """``[tray]`` as the part follows it: ``(enabled, icon, terminal)``, the defaults for a
    config that will not load, as everything else reads one."""
    from aisquare.core.config import TraySettings, load_config

    try:
        settings = load_config().tray
    except Exception:
        settings = TraySettings()
    return settings.enabled, settings.icon, settings.terminal


def _remote_session() -> bool:
    from aisquare.core import login_session

    return login_session.remote_session() is True


def _has_module(name: str) -> bool:
    try:
        return importlib.util.find_spec(name) is not None
    except (ImportError, ValueError):
        return False


def _detached(command: Sequence[str]) -> subprocess.Popen[bytes]:
    """The service's own way to start a worker (``service_host._detached``)."""
    from aisquare.services import service_host

    return service_host._detached(command)


def _running_agents() -> list[str]:
    from aisquare.services import lifecycle

    live, _unlistened, _error = lifecycle.running_fleet()
    return list(live)


class TrayPart:
    """The tray, in its child (``rlx service tray``), kept running while it is wanted."""

    name = "tray"
    every = TRAY_SECONDS
    first = TRAY_FIRST_SECONDS

    def __init__(
        self,
        *,
        why_not: Callable[[], str | None] = why_no_tray,
        spawn: Callable[[Sequence[str]], subprocess.Popen[bytes]] | None = None,
        agents: Callable[[], list[str]] = _running_agents,
        settings: Callable[[], tuple[bool, str, str]] = tray_settings,
    ) -> None:
        self._why_not = why_not
        self._spawn = spawn
        self._agents = agents
        self._settings = settings
        self._started_with: tuple[bool, str, str] | None = None
        """``[tray]`` as the child started: a change to it stops the child, and the next tick
        starts one that reads the new settings (or none, switched off)."""
        self._child: subprocess.Popen[bytes] | None = None
        self._crashes = 0
        self._left: PartHealth | None = None
        """Why the part gave the tray up for this service run, as its health: ``off`` for an
        exit that means no tray here, ``error`` for too many crashes, for the rest of the
        run."""

    def start_part(self) -> None:
        return None

    def tick_part(self) -> PartHealth:
        from aisquare.services.service_host import PartHealth

        if self._left is not None:
            return self._left
        child = self._child
        if child is not None and child.poll() is not None:
            self._child = None
            code = child.returncode
            if code in NO_TRAY_EXITS:
                self._left = PartHealth("off", NO_TRAY_EXITS[code])
                return self._left
            self._crashes += 1
            _log.warning("the tray stopped (exit %s), %s time(s)", code, self._crashes)
            if self._crashes >= TRAY_TRIES:
                gave_up = f"the tray stopped {self._crashes} times; it starts with the service"
                self._left = PartHealth("error", gave_up)
                return self._left
        if self._child is not None and self._settings() != self._started_with:
            self.stop_part()  # App settings changed it: the next child reads it, or none runs
        if self._child is None:
            why = self._why_not()
            if why is not None:
                return PartHealth("off", why)
            self._started_with = self._settings()
            self._child = self._start()
        agents = self._listed()
        return PartHealth(
            "ok",
            f"the tray runs (pid {self._child.pid})",
            extra={"agents": agents[:AGENTS_KEPT], "agents_running": len(agents)},
        )

    def _start(self) -> subprocess.Popen[bytes]:
        from aisquare.services import service_host

        _enabled, icon, terminal = self._started_with or self._settings()
        args = ["--icon", icon, "--terminal", terminal]
        args += ["--status", str(service_host.status_path())]
        args += ["--folder", str(service_host.service_dir())]
        args += ["--service-pid", str(os.getpid())]  # the parent it ends with, passed (m2)
        spawn = self._spawn or _detached
        return spawn(selfcli.argv_for(args, module=TRAY_MODULE))

    def _listed(self) -> list[str]:
        try:
            return self._agents()
        except Exception:  # the fleet unreadable: the menu says no names, the tray runs on
            _log.warning("the tray part could not list the running agents", exc_info=True)
            return []

    def stop_part(self) -> None:
        child, self._child = self._child, None
        if child is None or child.poll() is not None:
            return
        child.terminate()
        try:
            child.wait(STOP_WAIT_SECONDS)
        except subprocess.TimeoutExpired:
            child.kill()
