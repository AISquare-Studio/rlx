"""The rlx service (S1, #32): one headless rlx per home that keeps alive what the TUI took down
with it when the last one quit.

The owner kept a TUI open on the Mac only so that rlx stayed live, and so that the usage reads
through the login Keychain kept working (the brief, rlx-service-and-tray.md). What lived in
the TUI process and died with it (the inventory, board #1265):

- Remote: its server, its ngrok and the watchdog that revives it, and its auto-off. Only a
  TUI start brought it back, and only the TUI that served it kept it.
- The 60 s usage read: the readings on record, and every agent's status line
  (``cache/statusline/fleet.json``), which the before-limit hand-over and the status lines
  read. Five minutes after the last TUI quit, the fleet figure went from every status line.
- The retry of a manager nudge that a draft or a dialog held back (``fleet.
  retry_deferred_nudges``): with no TUI, only the next board write sent it.
- The usage sweep and the usage shipper.

``rlx service run`` hosts them (:class:`ServiceHost`), one process per home. A TUI that finds
it live leaves them to it, and takes them back when it dies (``cli.ui.app``).

Not here: the daily active ping (a service up every day would count days on which nobody
opened rlx), the release check (its answer is a TUI's to show), the captain's ui socket (its
actions are on screen), the limit hand-overs (an agent's own hooks run them, in a detached
process of their own), login registration (S2) and the tray (S3).

The service is one more instance in the registry (``core.instances``, kind ``service``): the
version gate counts it, a quit request stops it, and :func:`stop` and :func:`start` are what
the home migration and ``rlx upgrade`` call. Beside its record it keeps ``<home>/service/``:
``service.lock``, held while it runs (one service per home), and ``status.json``, what
``rlx service status`` reads, rewritten every :data:`STATUS_SECONDS`. Its log is
``<logs>/service.log``.

POSIX only in 0.10.2 (macOS, Linux, and Windows through WSL, where the fleet runs); rlxd
brings native Windows in 0.10.3.
"""

from __future__ import annotations

import contextlib
import json
import logging
import logging.handlers
import os
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from types import FrameType
from typing import IO, TYPE_CHECKING, Any, Literal, Protocol

from aisquare.core import atomic, instances, locking, paths, selfcli
from aisquare.core.version import PRODUCT_NAME

if TYPE_CHECKING:
    from aisquare.cli.ui.remote_control import RemoteController
    from aisquare.models import HeadroomSummary

_log = logging.getLogger(__name__)

KIND: instances.Kind = "service"
"""The service's kind in the instance registry."""
SERVICE = f"{PRODUCT_NAME} service"
"""What it is called in what it says: the product's name and the word (never a command)."""
FOLDER = "service"
"""``<home>/service/``: the lock and the status."""
LOCK_NAME = "service.lock"
STATUS_NAME = "status.json"
LOG_NAME = "service.log"

STATUS_SECONDS = 5.0
"""How often ``status.json`` is rewritten: as often as the registry's heartbeat."""
REMOTE_FOLLOW_SECONDS = 2.0
"""How often Remote follows its saved switch, which a TUI flips (:class:`RemotePart`)."""
REMOTE_RETRY_SECONDS: tuple[float, ...] = (2.0, 4.0, 8.0, 16.0, 32.0)
"""The waits before each try again of a Remote the switch asks for that would not start: the
first try, one more after each wait, about a minute in all; then the service stops trying,
saves the switch off and says why (#1717). It tried every follow, for ever, a WARNING each
time, and the switch stayed on with no way off from a TUI."""
REMOTE_TRIES = len(REMOTE_RETRY_SECONDS) + 1
LOG_BYTES = 1_000_000
"""``service.log`` is rolled over at this size, one old file kept: a service runs for weeks."""
REMOTE_WATCH_SECONDS = 30.0
"""How often ngrok is revived if it died, and Remote's auto-off looked at, as a TUI does."""
USAGE_SECONDS = 60.0
"""How often the accounts' usage is read, as a TUI's headroom read does."""
NUDGE_SECONDS = 5.0
"""How often a manager nudge held back is retried (a TUI's frame retries every 2 s)."""
SWEEP_FIRST_SECONDS = 45.0
SHIP_FIRST_SECONDS = 60.0
SHIP_CHECK_SECONDS = 300.0
"""The usage sweep and shipper, at a TUI's housekeeping times (``cli.ui.app``)."""
START_WAIT_SECONDS = 10.0
"""How long :func:`start` waits for the service it started to be live."""
STOP_WAIT_SECONDS = 15.0
"""How long :func:`stop` waits for it to be gone: the instance gate's wait for an asq."""
PART_STOP_SECONDS = 30.0
"""How long a part's stop may take: Remote's server and ngrok wind down in seconds."""

NOT_HERE = f"the {SERVICE} runs on macOS and Linux, and on Windows inside WSL, where the fleet runs"
"""Why ``rlx service`` refuses on native Windows."""

PartState = Literal["ok", "off", "waiting", "error"]


class ServiceError(Exception):
    """Something the service, or a command about it, could not do; the message is the line
    to say."""


@dataclass
class PartHealth:
    """How one hosted part is: ``ok``, ``off`` (not wanted), ``waiting`` (wanted, held by
    something else) or ``error``, with a line saying how, and when it last ran."""

    state: PartState
    detail: str | None = None
    last_run: datetime | None = None
    extra: dict[str, Any] = field(default_factory=dict)

    def to_json(self) -> dict[str, Any]:
        out: dict[str, Any] = {"state": self.state, "detail": self.detail}
        out["last_run"] = None if self.last_run is None else self.last_run.isoformat()
        out.update(self.extra)
        return out


class Part(Protocol):
    """One thing the service hosts: started once, ticked every :attr:`every` seconds (the
    first :attr:`first` seconds after the start), stopped once."""

    name: str
    every: float
    first: float

    def start_part(self) -> None: ...

    def tick_part(self) -> PartHealth: ...

    def stop_part(self) -> None: ...


# --- the parts ---------------------------------------------------------------------------------


class RemotePart:
    """Remote, through the controller a TUI drives (``cli.ui.remote_control``, which imports
    no Textual), following the switch saved in ``state.json``: on as the service starts if
    the switch is on, and on or off within :data:`REMOTE_FOLLOW_SECONDS` when a TUI flips
    it. Every :data:`REMOTE_WATCH_SECONDS`, ngrok revived if it died and the auto-off
    enforced, as a TUI's housekeeping does. Stopped with the service, the switch kept: the
    next start serves again."""

    name = "remote"
    every = REMOTE_FOLLOW_SECONDS
    first = 0.0

    def __init__(
        self,
        controller: Callable[[], RemoteController] | None = None,
        *,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._make = controller
        self._clock = clock
        self.controller: RemoteController | None = None
        self._watched_at: float | None = None
        self._failures = 0
        """Tries in a row that left Remote off while its switch asks for it on."""
        self._retry_at: float | None = None
        """When the next of those tries is due, on ``clock``; ``None``: at the next follow."""
        self.gave_up: tuple[str, datetime] | None = None
        """Why the service stopped trying, and when: told to a TUI (``status.json``) until the
        switch is turned on again."""
        self._logged: str | None = None

    def start_part(self) -> None:
        from aisquare.cli.ui import remote_control

        self.controller = (self._make or remote_control.RemoteController)()
        self.controller.on_news = self._hear

    def _hear(self, news: str, trouble: bool) -> None:
        """The controller's news, to ``service.log`` once per change: each try of a start resets
        the controller's own once-only, and a Remote that would not start wrote the same
        WARNING every :data:`REMOTE_FOLLOW_SECONDS` (#1717)."""
        if news == self._logged:
            return
        self._logged = news
        _log.log(logging.WARNING if trouble else logging.INFO, "remote: %s", news)

    def tick_part(self) -> PartHealth:
        from aisquare.cli.ui import remote_control

        controller = self.controller
        if controller is None:
            return PartHealth("error", "not started")
        saved = remote_control.load_remote_state()
        minutes = controller.state.auto_off_minutes
        controller.state = saved  # the switches as any rlx on this home saved them last
        if not saved.remote_enabled:
            self._failures, self._retry_at = 0, None  # turned on again: tries afresh
            if controller.remote_serving:
                controller.turn_off(wait=True, persist=False, reason="remote switched off")
        elif not controller.remote_serving:
            self.gave_up = None
            self._try_start(controller)
        elif saved.auto_off_minutes != minutes:
            # A TUI's auto-off picker, as its client saved it: a new deadline from now.
            controller.set_auto_off(saved.auto_off_minutes, wait=True)
        now = self._clock()
        if self._watched_at is None or now - self._watched_at >= REMOTE_WATCH_SECONDS:
            self._watched_at = now
            if controller.remote_serving:
                controller.revive_tunnel_if_dead()
                controller.enforce_auto_off(wait=True)
        return self.health()

    def _try_start(self, controller: RemoteController) -> None:
        """Start the Remote the switch asks for, when a try is due: again after each of
        :data:`REMOTE_RETRY_SECONDS`, then no more, the switch saved off, why told once.
        Another rlx serving this home is waited for, not counted: it takes nothing to fix."""
        now = self._clock()
        if self._retry_at is not None and now < self._retry_at:
            return
        controller.restore(wait=True)
        if controller.remote_serving or not controller.state.remote_enabled:
            self._failures, self._retry_at = 0, None  # up; or its missed auto-off saved it off
            return
        if controller.served_elsewhere_now():
            self._failures, self._retry_at = 0, None
            return
        self._failures += 1
        if self._failures < REMOTE_TRIES:
            self._retry_at = now + REMOTE_RETRY_SECONDS[self._failures - 1]
            return
        why = controller.message or "Remote would not start"
        self._failures, self._retry_at = 0, None
        controller.switch_off_unstarted()
        self.gave_up = (why, datetime.now(UTC))
        self._hear(f"{why}; tried {REMOTE_TRIES} times, so its switch is saved off", True)

    def health(self) -> PartHealth:
        controller = self.controller
        if controller is None:
            return PartHealth("error", "not started")

        facts: dict[str, Any] = {
            "on": controller.remote_serving,
            "public_origin": controller.public_origin,
            "auto_off_at": (
                None if controller.auto_off_at is None else controller.auto_off_at.isoformat()
            ),
        }
        if controller.remote_serving:
            where = controller.public_origin or "this machine only (ngrok not up)"
            return PartHealth("ok", f"serving: {where}", extra=facts)
        if not controller.state.remote_enabled:
            if self.gave_up is None:
                return PartHealth("off", "switched off", extra=facts)
            why, when = self.gave_up
            facts.update(problem=why, problem_at=when.isoformat())
            return PartHealth("off", f"switched off: {why}", extra=facts)
        if controller.served_elsewhere_now():
            return PartHealth("waiting", "served by another rlx on this home", extra=facts)
        detail = controller.message or "not serving"
        if self._failures:
            detail = f"{detail} (try {self._failures} of {REMOTE_TRIES})"
        return PartHealth("error", detail, extra=facts)

    def stop_part(self) -> None:
        controller = self.controller
        if controller is None:
            return
        controller.shutdown_for_exit(wait=True)  # its switch kept: the next start serves again
        controller.wait_until_off(PART_STOP_SECONDS)
        controller.writes_done(PART_STOP_SECONDS)
        pushes = sys.modules.get("aisquare.services.remote_push")  # loaded by all that push
        if pushes is not None:
            with contextlib.suppress(Exception):
                pushes.push_drain()


ReadHeadroom = Callable[..., "HeadroomSummary"]


KEYCHAIN_OUT_OF_REACH = (
    "Keychain locked: this service was started from an SSH login, and macOS keeps the login "
    "Keychain from that session, so Claude Code's usage reads come back empty; start it from "
    "a terminal at the Mac"
)
"""The usage part's health, and ``rlx service start``'s warning, for a service in an SSH
login's session on macOS (design A.3 of #32)."""


def keychain_out_of_reach() -> bool:
    """Whether this process runs in a macOS security session an SSH login established
    (``core.login_session``), where the login Keychain does not answer: every process started
    from it, a detached service included, inherits that session. ``False`` off macOS, or when
    macOS cannot be asked."""
    from aisquare.core import login_session

    return login_session.remote_session() is True


class UsagePart:
    """The accounts' usage, every :data:`USAGE_SECONDS`, through the cache a TUI reads
    (``claude_accounts.read_fleet_headroom``): the network is asked only for a reading over
    five minutes old, the readings are recorded, and ``fleet.json`` is written for every
    agent's status line. One read per agent that has accounts, as a TUI's dots are. In the
    login session only does the Keychain answer: a service started over SSH says so in its
    health, and never writes a figure over the Mac's (``write_statusline_fleet``)."""

    name = "usage"
    every = USAGE_SECONDS
    first = 0.0

    def __init__(
        self,
        read: ReadHeadroom | None = None,
        *,
        out_of_reach: Callable[[], bool] = keychain_out_of_reach,
    ) -> None:
        self._read = read
        self._out_of_reach = out_of_reach
        self._locked = False

    def start_part(self) -> None:
        self._locked = self._out_of_reach()  # a process's session is its own for good

    def tick_part(self) -> PartHealth:
        from aisquare.services import claude_accounts as accounts_service

        read = self._read or accounts_service.read_fleet_headroom
        agents = sorted({account.agent for account in accounts_service.list_accounts()})
        said: list[str] = []
        for agent in agents or [accounts_service.AGENT]:
            summary = read(agent=agent)
            line = f"{agent}: {summary.level}, {summary.usable} usable"
            if summary.needs_login:
                line += f", {summary.needs_login} to sign in"
            said.append(line)
        if self._locked:
            return PartHealth("error", f"{KEYCHAIN_OUT_OF_REACH} ({'; '.join(said)})")
        return PartHealth("ok", "; ".join(said))

    def stop_part(self) -> None:
        return None


class NudgePart:
    """A manager nudge a draft or a dialog held back, sent once its input box is clear, every
    :data:`NUDGE_SECONDS` (``fleet.retry_deferred_nudges``: one indexed read when nothing is
    held back)."""

    name = "nudges"
    every = NUDGE_SECONDS
    first = 0.0

    def __init__(self, retry: Callable[[], int] | None = None) -> None:
        self._retry = retry
        self.sent = 0

    def start_part(self) -> None:
        return None

    def tick_part(self) -> PartHealth:
        from aisquare.services import fleet as fleet_service

        self.sent += (self._retry or fleet_service.retry_deferred_nudges)()
        return PartHealth("ok", f"{self.sent} sent since the service started")

    def stop_part(self) -> None:
        return None


SWEEP_COMMAND = ("service", "sweep")
"""What :class:`SweepPart` runs in a process of its own: a hidden ``rlx service`` verb, which is
no feature (``cli.app._NOT_FEATURES``). It ran ``metrics stats``, a counted command, whose
close sends the day's active ping: every day the service was up sent it with nobody at rlx,
which the ruling on #32 (Q2) keeps out (#1842)."""


class SweepPart:
    """The usage sweep the *Usage & stats* page reads, in a process of its own: ``metrics stats``
    sweeps first (``usage_stats.maybe_sweep``, at most every ten minutes, one process at a
    time). In this process it would keep what it read, about 140 MB, for as long as the
    service runs (measured for #32's idle budget), and hold the other parts up for as long as
    a first sweep takes."""

    name = "sweep"
    first = SWEEP_FIRST_SECONDS

    def __init__(self, sweep: Callable[[], object] | None = None) -> None:
        from aisquare.services import usage_stats

        self.every = usage_stats.SWEEP_EVERY.total_seconds()
        self._sweep = sweep
        self._child: subprocess.Popen[bytes] | None = None

    def start_part(self) -> None:
        return None

    def tick_part(self) -> PartHealth:
        if self._sweep is not None:
            self._sweep()
            return PartHealth("ok", "swept")
        child = self._child
        if child is not None and child.poll() is None:
            return PartHealth("ok", f"a sweep runs (pid {child.pid})")
        self._child = _detached(selfcli.argv_for(list(SWEEP_COMMAND)))
        return PartHealth("ok", f"started a sweep (pid {self._child.pid})")

    def stop_part(self) -> None:
        return None


class ShipPart:
    """The anonymous usage shipment, when sharing is on and one is due, as every feature command
    starts it (``sharing.start_background_ship``: a detached ``metrics ship``, due every 6 h);
    nothing at all while sharing is off."""

    name = "ship"
    every = SHIP_CHECK_SECONDS
    first = SHIP_FIRST_SECONDS

    def __init__(self, ship: Callable[[], object] | None = None) -> None:
        self._ship = ship

    def start_part(self) -> None:
        return None

    def tick_part(self) -> PartHealth:
        from aisquare.core import sharing

        if not sharing.enabled():
            return PartHealth("off", "usage sharing is off")
        if self._ship is not None:
            self._ship()
            return PartHealth("ok", "looked for a shipment due")
        started = sharing.start_background_ship()
        return PartHealth("ok", "started a shipment" if started else "no shipment due")

    def stop_part(self) -> None:
        return None


def _detached(command: Sequence[str]) -> subprocess.Popen[bytes]:
    """``command`` in a session of its own, its output in the service's log, reaped when it ends
    (a thread waits for it), so that it leaves no zombie in a process that lives on."""
    log = log_path()
    log.parent.mkdir(parents=True, exist_ok=True)
    from aisquare.core import spawn

    with open(log, "ab") as out:
        child = subprocess.Popen(
            list(command),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=out,
            start_new_session=True,
            close_fds=True,
            env=spawn.untraced_env(),
        )
    reaper = threading.Thread(target=child.wait, name="rlx-service-child", daemon=True)
    reaper.start()
    return child


def default_parts() -> list[Part]:
    """What the service hosts, in the order they start (and stop in reverse)."""
    return [RemotePart(), UsagePart(), NudgePart(), SweepPart(), ShipPart()]


# --- the host ----------------------------------------------------------------------------------


@dataclass
class _Hosted:
    part: Part
    due: float
    health: PartHealth


class ServiceHost:
    """Runs :class:`Part` s on one thread: each ticks when it is due, and the thread sleeps
    until the next is, so an idle service wakes only for what it does. A part that raises
    is an ``error`` in the status, said in the log, and ticks again next time: never the
    end of the service."""

    def __init__(
        self,
        parts: Sequence[Part],
        *,
        clock: Callable[[], float] = time.monotonic,
        status_path: Path | None = None,
        describe: Callable[[], dict[str, Any]] | None = None,
    ) -> None:
        self._clock = clock
        self._status_path = status_path
        self._describe = describe or dict
        self._hosted = [_Hosted(part, 0.0, PartHealth("waiting", "not started")) for part in parts]
        self._status_due = 0.0
        self.started_at = datetime.now(UTC)

    @property
    def parts(self) -> list[Part]:
        return [hosted.part for hosted in self._hosted]

    def health(self) -> dict[str, PartHealth]:
        return {hosted.part.name: hosted.health for hosted in self._hosted}

    def start_parts(self) -> None:
        now = self._clock()
        for hosted in self._hosted:
            try:
                hosted.part.start_part()
            except Exception as exc:
                _log.exception("the service could not start %s", hosted.part.name)
                hosted.health = PartHealth("error", f"did not start: {_said(exc)}")
            hosted.due = now + hosted.part.first
        self._status_due = now

    def tick_parts(self) -> float:
        """Tick every part that is due, write the status if it is due; how many seconds until
        the next thing is."""
        for hosted in self._hosted:
            if self._clock() < hosted.due:
                continue
            try:
                health = hosted.part.tick_part()
                health.last_run = datetime.now(UTC)
                hosted.health = health
            except Exception as exc:
                _log.exception("the service's %s failed", hosted.part.name)
                hosted.health = PartHealth("error", _said(exc), last_run=datetime.now(UTC))
            hosted.due = self._clock() + hosted.part.every
        if self._clock() >= self._status_due:
            self.write_status()
            self._status_due = self._clock() + STATUS_SECONDS
        soonest = min([self._status_due, *(hosted.due for hosted in self._hosted)])
        return max(0.0, soonest - self._clock())

    def stop_parts(self) -> None:
        for hosted in reversed(self._hosted):
            try:
                hosted.part.stop_part()
            except Exception:
                _log.exception("the service could not stop %s", hosted.part.name)
            hosted.health = PartHealth("off", "the service stopped")

    def run_until(self, stop: threading.Event) -> None:
        """Start the parts, tick them until ``stop`` is set, then stop them."""
        self.start_parts()
        try:
            while not stop.is_set():
                stop.wait(self.tick_parts())
        finally:
            self.stop_parts()

    def status_record(self) -> dict[str, Any]:
        return {
            **self._describe(),
            "started_at": self.started_at.isoformat(),
            "written_at": datetime.now(UTC).isoformat(),
            "parts": {name: health.to_json() for name, health in self.health().items()},
        }

    def write_status(self) -> None:
        if self._status_path is None:
            return
        try:
            self._status_path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            body = json.dumps(self.status_record())
            atomic.write_replacing(self._status_path, body, durable=False, owner_only=True)
        except OSError:
            _log.warning("the service's status could not be written", exc_info=True)
        roll_log_if_due()  # every STATUS_SECONDS: lines on fd 1 and 2 count too


def _said(exc: BaseException) -> str:
    text = str(exc).strip()
    return f"{type(exc).__name__}: {text}" if text else type(exc).__name__


# --- where it keeps things ---------------------------------------------------------------------


def service_dir() -> Path:
    """``<home>/service/``, this user's only (``0700``)."""
    folder = paths.aisquare_home() / FOLDER
    folder.mkdir(mode=0o700, parents=True, exist_ok=True)
    with contextlib.suppress(OSError):
        folder.chmod(0o700)
    return folder


def status_path() -> Path:
    return paths.aisquare_home() / FOLDER / STATUS_NAME


def log_path() -> Path:
    return paths.log_dir() / LOG_NAME


# --- running it: ``rlx service run`` -----------------------------------------------------------


@contextlib.contextmanager
def _one_per_home() -> Iterator[None]:
    """Hold ``service.lock`` for the block: one service per home. :class:`ServiceError` when
    another holds it."""
    fd = locking.open_lock_file(service_dir() / LOCK_NAME, mode=0o600)
    try:
        try:
            locking.lock_exclusive(fd)
        except OSError:
            other = running_service()
            pid = f" (pid {other.pid})" if other is not None else ""
            raise ServiceError(f"the {SERVICE} already runs on this home{pid}") from None
        yield
    finally:
        os.close(fd)


def _register() -> instances.Registration:
    """Register as the service, past the version gate: beside a live rlx of another release
    the service starts nothing (two releases would race on the store's migrations), and
    says which one; it never asks a TUI to quit."""
    me = instances.describe_this_process(kind=KIND)
    with instances.gate() as roll:
        other = next((i for i in roll.live if i.differs_from(me)), None)
        if other is not None:
            raise ServiceError(
                f"not started: {other.label} runs another release on this home; the service "
                f"runs rlx {me.version}. Quit that rlx, or start the service of its release"
            )
        return instances.register(me)


Stopping = Callable[[str], None]


@contextlib.contextmanager
def _stopped_by_signals(stop: Stopping) -> Iterator[None]:
    """SIGTERM, SIGINT and SIGHUP stop the service cleanly, as a quit request does: its
    Remote and ngrok first. Signal handlers are the main thread's; elsewhere, nothing."""
    if threading.current_thread() is not threading.main_thread():
        yield
        return

    def handle(signum: int, frame: FrameType | None) -> None:
        stop(signal.Signals(signum).name)

    names = ("SIGTERM", "SIGINT", "SIGHUP")
    before = {name: signal.getsignal(signal.Signals[name]) for name in names}
    for name in names:
        signal.signal(signal.Signals[name], handle)
    try:
        yield
    finally:
        for name, handler in before.items():
            signal.signal(signal.Signals[name], handler)


def run_service(
    *,
    stop: threading.Event | None = None,
    parts: Sequence[Part] | None = None,
    signals: bool = True,
) -> None:
    """``rlx service run``: host the parts until a quit request, a signal or ``stop``.

    One per home (``service.lock``), registered as the ``service`` instance past the version
    gate, its status kept in ``status.json`` while it runs and removed as it ends. Raises
    :class:`ServiceError` when it cannot start."""
    if sys.platform == "win32":
        raise ServiceError(NOT_HERE)
    stopping = stop if stop is not None else threading.Event()

    def stop_now(why: str) -> None:
        _log.info("the %s stops: %s", SERVICE, why)
        stopping.set()

    with _one_per_home():
        registration = _register()
        try:
            registration.deliver_quits_to(lambda asked: stop_now(f"{asked.who} asked it to quit"))
            registration.start()
            me = registration.instance
            host = ServiceHost(
                default_parts() if parts is None else parts,
                status_path=status_path(),
                describe=lambda: {
                    "pid": me.pid,
                    "version": me.version,
                    "schema": me.schema,
                    "host": me.host,
                    "instance": me.id,
                },
            )
            _log.info("the %s runs: %s", SERVICE, me.label)
            with _stopped_by_signals(stop_now) if signals else contextlib.nullcontext():
                host.run_until(stopping)
        finally:
            with contextlib.suppress(OSError):
                status_path().unlink(missing_ok=True)
            registration.deregister()
            _log.info("the %s stopped", SERVICE)


# --- asking about it, starting and stopping it -------------------------------------------------


@dataclass(frozen=True)
class ServiceRemote:
    """The Remote the service hosts, as its ``status.json`` says it: for a TUI that is the
    service's client (``RemoteController.service``)."""

    on: bool
    public_origin: str | None = None
    auto_off_at: datetime | None = None
    problem: str | None = None
    """Why the service stopped trying to start Remote, its switch saved off (#1717)."""
    problem_at: datetime | None = None


@dataclass(frozen=True)
class Hosting:
    """A live service of this release, which hosts what a TUI would: the TUI is its client
    (``cli.ui.service_client``)."""

    instance: instances.Instance
    remote: ServiceRemote


def hosting(live: Sequence[instances.Instance] | None) -> Hosting | None:
    """The service among ``live`` (a TUI's frame's registry read) that hosts for this rlx: of
    this release and store schema, with its Remote as its status says. ``None`` when none
    runs, or the one that runs is of another release (the gate's to settle)."""
    from aisquare.core.store import SCHEMA_VERSION
    from aisquare.core.version import __version__

    found = next((i for i in live or () if i.kind == KIND), None)
    if found is None or found.version != __version__ or found.schema != SCHEMA_VERSION:
        return None
    status = read_status() or {}
    parts = status.get("parts") if status.get("instance") == found.id else None
    remote = parts.get("remote") if isinstance(parts, dict) else None
    if not isinstance(remote, dict):
        return Hosting(found, ServiceRemote(on=False))
    origin = remote.get("public_origin")
    problem = remote.get("problem")
    return Hosting(
        found,
        ServiceRemote(
            on=remote.get("on") is True,
            public_origin=origin if isinstance(origin, str) else None,
            auto_off_at=_when(remote.get("auto_off_at")),
            problem=problem if isinstance(problem, str) else None,
            problem_at=_when(remote.get("problem_at")),
        ),
    )


def _when(value: object) -> datetime | None:
    """A time ``status.json`` holds, as written; ``None`` for none or a damaged one."""
    try:
        return datetime.fromisoformat(value) if isinstance(value, str) else None
    except ValueError:
        return None


def running_service(*, prune: bool = False) -> instances.Instance | None:
    """The live rlx service on this home, if one runs (the registry's ``service`` record)."""
    return next((i for i in instances.live_instances(prune=prune) if i.kind == KIND), None)


def read_status() -> dict[str, Any] | None:
    """What the running service last wrote in ``status.json``; ``None`` when there is none or
    it cannot be read."""
    try:
        data = json.loads(status_path().read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


@dataclass(frozen=True)
class ServiceStatus:
    """``rlx service status``: whether it runs, and how."""

    instance: instances.Instance | None
    status: dict[str, Any] | None
    this_version: str

    @property
    def live(self) -> bool:
        return self.instance is not None

    @property
    def mismatch(self) -> str | None:
        """``version mismatch: the service runs rlx X, this rlx is Y`` when they differ."""
        if self.instance is None or self.instance.version == self.this_version:
            return None
        return (
            f"version mismatch: the service runs rlx {self.instance.version}, this rlx is "
            f"{self.this_version}"
        )

    def uptime(self, now: datetime | None = None) -> float | None:
        if self.instance is None:
            return None
        return max(0.0, ((now or datetime.now(UTC)) - self.instance.started_at).total_seconds())

    def to_json(self, now: datetime | None = None) -> dict[str, Any]:
        instance = self.instance
        parts = (self.status or {}).get("parts") if instance is not None else None
        return {
            "running": instance is not None,
            "pid": None if instance is None else instance.pid,
            "version": None if instance is None else instance.version,
            "this_version": self.this_version,
            "version_mismatch": self.mismatch,
            "host": None if instance is None else instance.host,
            "started_at": None if instance is None else instance.started_at.isoformat(),
            "uptime_s": None if instance is None else round(self.uptime(now) or 0.0),
            "parts": parts if isinstance(parts, dict) else {},
        }


def service_status() -> ServiceStatus:
    from aisquare.core.version import __version__

    instance = running_service()
    return ServiceStatus(instance, read_status() if instance is not None else None, __version__)


def start_service(
    *, timeout: float = START_WAIT_SECONDS, argv: Sequence[str] | None = None
) -> instances.Instance:
    """Start the service in the background, unless one runs; the running one either way.

    ``rlx service run`` in a session of its own, so the terminal that started it can close,
    its output appended to ``service.log``. Waits at most ``timeout`` seconds for its record
    to be live, then :class:`ServiceError`, with what the log's end says."""
    if sys.platform == "win32":
        raise ServiceError(NOT_HERE)
    already = running_service()
    if already is not None:
        return already
    log = log_path()
    log.parent.mkdir(parents=True, exist_ok=True)
    command = list(argv) if argv is not None else selfcli.argv_for(["service", "run"])
    with open(log, "ab") as out:
        said_from = out.tell()  # what the log held before: an earlier run's lines, not this one's
        child = _spawn_service(command, out)
    # Reaped once it ends, so that a caller that lives on (a TUI's restart) leaves no zombie,
    # which reads as alive to :func:`stop`'s wait; a CLI caller exits first and hands it on.
    reaper = threading.Thread(target=child.wait, name="rlx-service-reaper", daemon=True)
    reaper.start()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        found = running_service()
        if found is not None:
            return found
        if child.poll() is not None:
            raise ServiceError(f"the {SERVICE} did not start: {_log_tail(log, since=said_from)}")
        time.sleep(0.1)
    said = _log_tail(log, since=said_from)
    raise ServiceError(
        f"the {SERVICE} did not start within {timeout:.0f} s (pid {child.pid}): {said}"
    )


def _spawn_service(command: Sequence[str], out: IO[bytes]) -> subprocess.Popen[bytes]:
    """The one place an rlx service process is started (``core.spawn.SEAMS``): ``command`` in
    a session of its own, so the terminal that started it can close, its output appended to
    ``out`` (``service.log``), and without any agent's tracing identity: a service started from
    an agent's pane hosts the home's work, not that agent's. The suite replaces this
    (``tests/conftest.py::no_real_service``): a real one outlives the test that started it."""
    from aisquare.core import spawn

    return subprocess.Popen(
        list(command),
        stdin=subprocess.DEVNULL,
        stdout=out,
        stderr=subprocess.STDOUT,
        start_new_session=True,
        close_fds=True,
        env=spawn.untraced_env(),
    )


def stop_service(*, timeout: float = STOP_WAIT_SECONDS) -> bool:
    """Stop the running service as a quit request does: its parts wound down cleanly, its
    Remote's switch kept. Whether one ran. :class:`ServiceError` when it is still there after
    ``timeout`` seconds; it is never killed."""
    service = running_service()
    if service is None:
        return False
    me = instances.this_instance() or instances.describe_this_process()
    try:
        instances.request_quit(service, by=me)
    except OSError as exc:
        raise ServiceError(f"the {SERVICE} could not be asked to stop: {exc}") from exc
    if not instances.wait_gone(service, timeout):
        raise ServiceError(
            f"the {SERVICE} (pid {service.pid}) did not stop within {timeout:.0f} s; it may "
            "still be finishing Remote's writes"
        )
    return True


def restart_service(*, timeout: float = STOP_WAIT_SECONDS) -> instances.Instance:
    stop_service(timeout=timeout)
    return start_service()


def _log_tail(path: Path, lines: int = 3, *, since: int = 0) -> str:
    """The last ``lines`` of ``path`` from byte ``since`` on: what one run wrote, not the lines
    an earlier run left above it, which a failed start quoted before its own reason (#1743)."""
    try:
        with path.open("rb") as log:
            log.seek(since)
            text = log.read().decode("utf-8", errors="replace")
    except OSError:
        return "its log cannot be read"
    tail = text.strip().splitlines()[-lines:]
    return " / ".join(tail) if tail else "it wrote nothing in its log"


class _ServiceLog(logging.handlers.RotatingFileHandler):
    """``service.log``, rolled over at :data:`LOG_BYTES` with one old file kept, and this
    process's fd 1 and 2 moved with it while they are that file. :func:`start_service` hands the
    service its log as stdout and stderr: what is written there (uvicorn's own lines, a
    traceback) went on into the rolled-over file, and after a second roll into an unlinked
    one, growing with no bound (#1842)."""

    def __init__(self, path: Path, *, follow: tuple[int, ...]) -> None:
        super().__init__(path, maxBytes=LOG_BYTES, backupCount=1, encoding="utf-8")
        self._follow = follow
        self._point_fds()

    def doRollover(self) -> None:
        super().doRollover()
        self._point_fds()

    def _point_fds(self) -> None:
        stream = self.stream
        if not self._follow or stream is None:
            return
        for pending in (sys.stdout, sys.stderr):  # what they hold goes where it was written
            with contextlib.suppress(Exception):
                pending.flush()
        for fd in self._follow:
            os.dup2(stream.fileno(), fd)

    def roll_if_due(self) -> None:
        """Roll over once the file is :data:`LOG_BYTES` long, whoever wrote it: lines on fd 1
        and 2 pass no handler, which looks at the size only as it writes a record."""
        try:
            due = Path(self.baseFilename).stat().st_size >= LOG_BYTES
        except OSError:
            return
        if due:
            with self.lock or contextlib.nullcontext():
                self.doRollover()


_service_log: _ServiceLog | None = None


def roll_log_if_due() -> None:
    """:meth:`_ServiceLog.roll_if_due` for this process's ``service.log``, when it has one."""
    handler = _service_log
    if handler is not None:
        handler.roll_if_due()


def _is_the_file(fd: int, path: Path) -> bool:
    try:
        return os.path.samestat(os.fstat(fd), path.stat())
    except OSError:
        return False


def log_to_file() -> None:
    """Send this process's log lines to ``service.log`` (``rlx service run``'s), each with its
    time; INFO and up. Its fd 1 and 2, when :func:`start_service` made them that file, follow
    each rollover; on a terminal (a ``run`` in the foreground) they stay there."""
    global _service_log
    log = log_path()
    log.parent.mkdir(parents=True, exist_ok=True)
    log.touch(exist_ok=True)
    follow = tuple(fd for fd in (1, 2) if _is_the_file(fd, log))
    handler = _ServiceLog(log, follow=follow)
    handler.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(name)s: %(message)s"))
    root = logging.getLogger()
    root.addHandler(handler)
    _service_log = handler
    if root.level > logging.INFO or root.level == logging.NOTSET:
        root.setLevel(logging.INFO)


__all__ = [
    "KEYCHAIN_OUT_OF_REACH",
    "KIND",
    "NOT_HERE",
    "SWEEP_COMMAND",
    "Hosting",
    "NudgePart",
    "Part",
    "PartHealth",
    "RemotePart",
    "ServiceError",
    "ServiceHost",
    "ServiceRemote",
    "ServiceStatus",
    "ShipPart",
    "SweepPart",
    "UsagePart",
    "default_parts",
    "hosting",
    "keychain_out_of_reach",
    "read_status",
    "restart_service",
    "roll_log_if_due",
    "run_service",
    "running_service",
    "service_status",
    "start_service",
    "stop_service",
]
