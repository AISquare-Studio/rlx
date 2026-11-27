"""rlx's daemons around ``rlx upgrade`` (handoff 0.10.3, 4.2, and 7.8's Upgrade part).

An upgrade replaces the code every long-lived rlx process runs. rlxd, a detached ``watch``,
``serve`` and the rest (the daemon registry, ``core.daemons``) keep the code they started
with: on macOS and Linux a mix of releases until they restart, on Windows files in use that
fail the reinstall. So the upgrade counts them, and with ``--restart-daemons`` stops them
before the swap and starts them again after it.

- **The census** (:func:`census`), which ``upgrade --check`` adds to its fields: ``daemons``
  (the live records), ``runs_in_flight`` (rlxd's queued and running runs) and ``ptys``
  (rlxd's live PTYs, which a restart of rlxd ends).
- **Refusals** (:func:`refusal`), before anything stops: on Windows any registered daemon
  (``daemon_running``) unless ``--restart-daemons``; with ``--restart-daemons``, a live PTY
  (``ptys_live``) unless ``--end-ptys``; and runs in flight (``runs_in_flight``) unless
  ``--wait-runs N``, which waits up to N minutes for them to end (:func:`wait_for_runs`).
  On macOS and Linux, daemons left running are only warned of.
- **Around the install** (:class:`Around`, which ``services.lifecycle.upgrade`` calls): the
  stop of each restartable daemon of this host (``stop_daemons``), the install, the swap
  of rlxd's binaries by the NEW install's ``daemon install`` (``rlxd``: #593's verified,
  atomic install), then, once the hooks are refreshed, each stopped daemon started again
  from its recorded argv (``start_daemons``). Whatever fails, the daemons it stopped are
  started again from what is in place, the old binaries included: an upgrade never leaves
  no rlxd.
- **Events** (:class:`Events`): with ``--json-lines``, each step is ``upgrade.step {step,
  status, detail?}``, and the end ``upgrade.done {from, to, restarted}`` or ``upgrade.error
  {code, message, detail}``, 7.3 envelopes with a local ``seq``, on stdout and nothing else.

rlxd is reached through its own command line beside rlx (``rlxd status --json``, ``rlxd call
…``, ``rlxd stop``): no client of its protocol lives here.

Everything this module uses is imported at its top: it runs on both sides of the moment the
installer replaces rlx's files, after which nothing may be imported (``services.lifecycle``).
"""

from __future__ import annotations

import contextlib
import json
import os
import signal
import subprocess
import sys
import time
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass, field, replace
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any, TextIO

from aisquare.core import daemons as daemons_core
from aisquare.core import registry
from aisquare.core.spawn import untraced_env
from aisquare.core.version import DISTRIBUTION
from aisquare.services import install_route, rlxd_install

if TYPE_CHECKING:
    from aisquare.services.lifecycle import HookRefresh, UpgradePlan, UpgradeReport

STEPS = ("check", "stop_daemons", "install", "rlxd", "refresh_hooks", "start_daemons")
"""7.8's ``upgrade.step`` names, in the order the upgrade takes them."""

STOP_TIMEOUT_SECONDS = 30.0
"""How long a daemon has to stop, its lock let go, before the upgrade gives up on it."""

START_TIMEOUT_SECONDS = 30.0
"""How long a daemon started again has to come up: rlxd answering, another registered."""

RLXD_CALL_TIMEOUT_SECONDS = 30.0
"""How long one ``rlxd`` command may take."""

INSTALL_TIMEOUT_SECONDS = 300.0
"""How long the new install's ``daemon install`` may take: a download and a check."""

WAIT_POLL_SECONDS = 1.0
"""How often ``--wait-runs`` asks rlxd for its runs."""

PACKAGE_ENV = "AISQUARE_INSTALL_PACKAGE"
"""A wheel to install in place of PyPI's release, as install.sh takes it (4.2's test hook)."""

RUN_STATES = ("queued", "running")
"""The runs an upgrade waits for: still to start, or running."""

DETACHED_FLAGS = 0x00000008 | 0x00000200
"""Windows' ``DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP``: a daemon started again outlives
the upgrade, with no console of its own."""


class Refused(Exception):
    """Why the upgrade stops: a 7.8 ``upgrade.error`` code, a message and a detail."""

    def __init__(self, code: str, message: str, detail: str | None = None) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.detail = detail


@dataclass(frozen=True)
class Options:
    """The upgrade's daemon flags."""

    restart_daemons: bool = False
    end_ptys: bool = False
    wait_runs: float | None = None
    """``--wait-runs``: minutes to wait for runs in flight to end; ``None`` refuses at once."""


# --- events ------------------------------------------------------------------------------


class Events:
    """The upgrade's steps, said: with ``json_lines`` as 7.3 envelopes on ``out`` (stdout,
    nothing else written there), otherwise as lines for a person on stderr."""

    def __init__(self, *, json_lines: bool, out: TextIO | None = None, err: TextIO | None = None):
        self.json_lines = json_lines
        self._out = out
        self._err = err
        self._seq = 0

    def emit(self, type_: str, data: dict[str, Any]) -> None:
        """One event of topic ``upgrade``."""
        if not self.json_lines:
            return
        self._seq += 1
        envelope = {
            "schema_version": 1,
            "seq": self._seq,
            "time": datetime.now(UTC).isoformat(timespec="milliseconds").replace("+00:00", "Z"),
            "topic": "upgrade",
            "type": type_,
            "data": data,
        }
        out = self._out if self._out is not None else sys.stdout
        out.write(json.dumps(envelope, separators=(",", ":")) + "\n")
        out.flush()

    def say_line(self, line: str) -> None:
        """A line for a person: stderr, never stdout."""
        err = self._err if self._err is not None else sys.stderr
        err.write(line + "\n")
        err.flush()

    def step(self, step: str, status: str, detail: str | None = None) -> None:
        """``upgrade.step {step, status, detail?}``; ``status`` is ``started``, ``done``,
        ``skipped`` or ``failed``."""
        data: dict[str, Any] = {"step": step, "status": status}
        if detail is not None:
            data["detail"] = detail
        self.emit("upgrade.step", data)
        if not self.json_lines and detail is not None and status in ("done", "failed"):
            self.say_line(f"{'✗' if status == 'failed' else '·'} {step}: {detail}")

    def done(self, from_: str, to: str, restarted: Sequence[str]) -> None:
        """``upgrade.done {from, to, restarted}``."""
        self.emit("upgrade.done", {"from": from_, "to": to, "restarted": list(restarted)})

    def error(self, refused: Refused) -> None:
        """``upgrade.error {code, message, detail}``."""
        self.emit(
            "upgrade.error",
            {"code": refused.code, "message": refused.message, "detail": refused.detail},
        )


# --- rlxd, through its command line ------------------------------------------------------


class RlxdError(Exception):
    """An rlxd command that did not answer ``ok``: its error code, or ``unreachable``."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


@dataclass(frozen=True)
class Rlxd:
    """The rlxd command line: ``command`` is the program and any arguments before the
    subcommand (the binary beside rlx, or what a test stands in for it)."""

    command: tuple[str, ...]

    def _run(self, *args: str) -> subprocess.CompletedProcess[str]:
        """A registered spawn seam (``core.spawn.SEAMS``): rlxd's own command line, which
        talks to the running rlxd of this home. Not a model process."""
        return subprocess.run(
            [*self.command, *args],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdin=subprocess.DEVNULL,
            env=untraced_env(),
            timeout=RLXD_CALL_TIMEOUT_SECONDS,
            check=False,
        )

    def _envelope(self, *args: str) -> dict[str, Any]:
        try:
            done = self._run(*args)
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise RlxdError("unreachable", f"rlxd {' '.join(args)}: {exc}") from exc
        try:
            envelope = json.loads(done.stdout)
        except ValueError as exc:
            said = (done.stderr or done.stdout).strip()
            raise RlxdError("unreachable", f"rlxd {' '.join(args)}: {said}") from exc
        if not isinstance(envelope, dict):
            raise RlxdError("unreachable", f"rlxd {' '.join(args)}: {done.stdout.strip()}")
        if not envelope.get("ok"):
            error = envelope.get("error") or {}
            code = str(error.get("code") or "error")
            raise RlxdError(code, str(error.get("message") or code))
        data = envelope.get("data")
        return data if isinstance(data, dict) else {}

    def daemon_status(self) -> dict[str, Any] | None:
        """``rlxd status --json``'s data; ``None`` when no rlxd answers."""
        try:
            return self._envelope("status", "--json")
        except RlxdError:
            return None

    def call(self, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        """``rlxd call <method> <params>``'s data. :class:`RlxdError` when it is refused."""
        args = ["call", method]
        if params is not None:
            args.append(json.dumps(params, separators=(",", ":")))
        return self._envelope(*args)

    def stop(self) -> None:
        """``rlxd stop``, which returns once rlxd has stopped."""
        try:
            done = self._run("stop")
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise RlxdError("unreachable", f"rlxd stop: {exc}") from exc
        if done.returncode != 0:
            said = (done.stderr or done.stdout).strip()
            raise RlxdError("stop_failed", said or "rlxd stop failed")


def installed_rlxd() -> Path | None:
    """rlxd in uv's tool bin directory beside rlx, where 1.2 puts it; ``None`` when absent."""
    names = rlxd_install.binary_names(rlxd_install.host_target())
    try:
        path = rlxd_install.uv_tool_bin_dir() / names[0]
    except (OSError, ValueError, rlxd_install.InstallError):
        return None
    return path if path.is_file() else None


def find_rlxd(live: Iterable[daemons_core.Daemon]) -> Rlxd | None:
    """The rlxd to ask: the one this home's rlxd record says runs (its argv's program), else
    the one installed beside rlx; ``None`` when there is neither."""
    here = registry.host_name()
    for daemon in live:
        if daemon.kind == "rlxd" and daemon.host == here and daemon.argv:
            return Rlxd((daemon.argv[0],))
    installed = installed_rlxd()
    return None if installed is None else Rlxd((str(installed),))


# --- the census ----------------------------------------------------------------------------


@dataclass(frozen=True)
class Census:
    """The daemons, runs in flight and live PTYs an upgrade must reckon with."""

    daemons: tuple[daemons_core.Daemon, ...] = ()
    runs: tuple[dict[str, Any], ...] = ()
    ptys: tuple[dict[str, Any], ...] = ()
    rlxd_error: str | None = None
    """Why rlxd's runs and PTYs are not known though an rlxd is registered."""

    @property
    def here(self) -> tuple[daemons_core.Daemon, ...]:
        """The daemons of this host: the ones this upgrade can stop and start."""
        host = registry.host_name()
        return tuple(daemon for daemon in self.daemons if daemon.host == host)

    @property
    def rlxd_runs(self) -> bool:
        """Whether this host's rlxd is registered as running."""
        return any(daemon.kind == "rlxd" for daemon in self.here)

    def as_json(self) -> dict[str, Any]:
        """``upgrade --check``'s three fields (and why rlxd's two are unknown, if they are)."""
        return {
            "daemons": [
                {
                    "id": daemon.id,
                    "kind": daemon.kind,
                    "pid": daemon.pid,
                    "host": daemon.host,
                    "version": daemon.version,
                    "restartable": daemon.restartable,
                    "argv": list(daemon.argv),
                    "started_at": daemon.started_at.isoformat(),
                }
                for daemon in self.daemons
            ],
            "runs_in_flight": [
                {key: run.get(key) for key in ("run_id", "kind", "state", "job_id")}
                for run in self.runs
            ],
            "ptys": [
                {key: pty.get(key) for key in ("pty_id", "label", "pid", "argv")}
                for pty in self.ptys
            ],
            "rlxd_error": self.rlxd_error,
        }


def census(rlxd: Rlxd | None = None) -> Census:
    """What runs now: the live daemons of this home (read only: nothing pruned), and, when
    this host's rlxd runs, its queued and running runs and its live PTYs."""
    live = tuple(daemons_core.list_live(prune=False))
    found = Census(daemons=live)
    if not found.rlxd_runs:
        return found
    rlxd = rlxd if rlxd is not None else find_rlxd(live)
    if rlxd is None:
        return replace(found, rlxd_error="rlxd is registered, but its program is not known")
    try:
        runs = rlxd.call("run.list", {"states": list(RUN_STATES)}).get("runs") or []
        ptys = rlxd.call("pty.list").get("ptys") or []
    except RlxdError as exc:
        return replace(found, rlxd_error=str(exc))
    return replace(
        found,
        runs=tuple(run for run in runs if isinstance(run, dict)),
        ptys=tuple(pty for pty in ptys if isinstance(pty, dict) and not pty.get("exited")),
    )


# --- refusals -------------------------------------------------------------------------------


def refusal(found: Census, options: Options, *, windows: bool) -> Refused | None:
    """Why the upgrade must not go on as asked, before anything stops; ``None`` when it may.
    Runs in flight are refused only when ``--wait-runs`` was not given (it waits instead)."""
    here = found.here
    if windows and here and not options.restart_daemons:
        return Refused(
            "daemon_running",
            "rlx daemons are running, and Windows can't replace the files of a running "
            f"program: {', '.join(daemon.label for daemon in here)}. Add --restart-daemons "
            "to stop them for the upgrade and start them again after it.",
            ", ".join(daemon.kind for daemon in here),
        )
    if options.restart_daemons and found.ptys and not options.end_ptys:
        return Refused(
            "ptys_live",
            f"{len(found.ptys)} PTY{'s are' if len(found.ptys) != 1 else ' is'} live in rlxd, "
            "and restarting rlxd ends "
            f"{'them' if len(found.ptys) != 1 else 'it'}: add --end-ptys to end "
            f"{'them' if len(found.ptys) != 1 else 'it'}, or upgrade once "
            f"{'they have' if len(found.ptys) != 1 else 'it has'} exited.",
            ", ".join(str(pty.get("pty_id")) for pty in found.ptys),
        )
    if found.runs and options.wait_runs is None:
        return _runs_refused(found.runs)
    return None


def _runs_refused(runs: Sequence[dict[str, Any]]) -> Refused:
    many = len(runs) != 1
    return Refused(
        "runs_in_flight",
        f"{len(runs)} rlxd run{'s are' if many else ' is'} in flight, and the upgrade "
        "replaces rlxd-hook under "
        f"{'them' if many else 'it'}: add --wait-runs <minutes> to wait for "
        f"{'them' if many else 'it'} to end, or cancel {'them' if many else 'it'}.",
        ", ".join(str(run.get("run_id")) for run in runs),
    )


def mixed_releases(found: Census, options: Options) -> str | None:
    """On macOS and Linux, the warning for daemons left running across the upgrade."""
    here = found.here
    if not here or options.restart_daemons:
        return None
    return (
        f"{', '.join(daemon.label for daemon in here)} keep{'s' if len(here) == 1 else ''} "
        "running the release it started with until restarted (add --restart-daemons to "
        "restart them with the upgrade)"
    )


def wait_for_runs(found: Census, options: Options, rlxd: Rlxd | None, events: Events) -> Census:
    """With ``--wait-runs``, waits up to its minutes for the runs in flight to end: the
    census then. :class:`Refused` ``runs_in_flight`` when they have not."""
    if not found.runs or options.wait_runs is None:
        return found
    if rlxd is None:
        raise _runs_refused(found.runs)
    deadline = time.monotonic() + options.wait_runs * 60
    events.step("check", "started", f"waiting for {len(found.runs)} run(s) in flight to end")
    runs: Sequence[dict[str, Any]] = found.runs
    while runs:
        if time.monotonic() >= deadline:
            raise _runs_refused(runs)
        time.sleep(WAIT_POLL_SECONDS)
        try:
            runs = rlxd.call("run.list", {"states": list(RUN_STATES)}).get("runs") or []
        except RlxdError as exc:
            raise Refused("runs_in_flight", f"rlxd's runs could not be read: {exc}") from exc
    return replace(found, runs=())


# --- the package to install ---------------------------------------------------------------


def wheel_version(package: str) -> str | None:
    """The version a wheel's file name gives (``name-version-…whl``, PEP 427)."""
    name = Path(package).name
    if not name.endswith(".whl"):
        return None
    parts = name[: -len(".whl")].split("-")
    return parts[1] if len(parts) >= 5 else None


def with_package(plan: UpgradePlan, lifecycle: Any) -> UpgradePlan:
    """``plan`` to install :data:`PACKAGE_ENV`'s wheel instead of PyPI's release, when it is
    set, as install.sh installs it: the version its file name gives is the target, which the
    new process must report, and the reinstall keeps the receipt's Python, ``--with``s and
    options, as every reinstall does. ``plan`` as it is when the variable is unset, the
    install is no uv tool, or on Windows (no self-replace there, whatever the package).

    ``lifecycle`` is ``services.lifecycle``, passed in: the plan it made is remade with the
    reason it refused a local source dropped (the package is named explicitly), and the hook
    sites and the running fleet it reads only for a plan it runs."""
    package = os.environ.get(PACKAGE_ENV, "").strip()
    route = plan.route
    if not package or route.receipt is None or route.facts.platform == "win32":
        return plan
    target = wheel_version(package)
    if target is None:
        raise Refused(
            "invalid_version",
            f"{PACKAGE_ENV} names {package!r}, which is not a wheel: its version can't be told",
        )
    receipt = route.receipt
    extras = f"[{','.join(receipt.extras)}]" if receipt.extras else ""
    where = Path(package).expanduser()
    spec = (
        f"{DISTRIBUTION}{extras} @ {where.resolve().as_uri()}"
        if where.exists()
        else f"{DISTRIBUTION}{extras} @ {package}"
    )
    # install_route._uv_install_argv's prefix, the package spec its own.
    argv = ["uv", "tool", "install", "--force", "--python"]
    argv.append(receipt.python or route.facts.python_version)
    for requirement in receipt.withs:
        argv.extend(["--with", requirement])
    argv.extend(receipt.options)
    argv.append(spec)
    refresh, left = lifecycle.refresh_sites(route.facts)
    live, _unlistened, fleet_error = lifecycle.running_fleet()
    return replace(
        plan,
        target=target,
        argv=tuple(argv),
        reason=None,
        pin_refused=False,
        pin_unmet=None,
        refresh=refresh,
        left=left,
        live_agents=live,
        fleet_error=fleet_error,
        remote=lifecycle.running_remote(),
        asqs=lifecycle.running_asqs(),
    )


# --- stopping and starting -------------------------------------------------------------------


def _live_ids() -> set[str]:
    return {daemon.id for daemon in daemons_core.list_live(prune=False)}


def _wait_gone(daemon: daemons_core.Daemon, timeout: float) -> bool:
    """Whether ``daemon``'s record stops being live (its lock let go) within ``timeout``."""
    deadline = time.monotonic() + timeout
    while daemon.id in _live_ids():
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.2)
    return True


def stop_daemon(daemon: daemons_core.Daemon, rlxd: Rlxd | None) -> None:
    """Stops ``daemon``: rlxd by ``rlxd stop``, any other by SIGTERM (Windows: terminated),
    then waits for its lock to be let go. :class:`Refused` ``daemon_stop_failed`` when it
    is not, within :data:`STOP_TIMEOUT_SECONDS`."""
    try:
        if daemon.kind == "rlxd" and rlxd is not None:
            rlxd.stop()
        else:
            os.kill(daemon.pid, signal.SIGTERM)
    except (OSError, RlxdError) as exc:
        if daemon.id not in _live_ids():
            return
        raise Refused(
            "daemon_stop_failed", f"{daemon.label} could not be stopped: {exc}", daemon.kind
        ) from exc
    if not _wait_gone(daemon, STOP_TIMEOUT_SECONDS):
        raise Refused(
            "daemon_stop_failed",
            f"{daemon.label} did not stop within {STOP_TIMEOUT_SECONDS:.0f} s; nothing was "
            "installed",
            daemon.kind,
        )


def spawn_daemon(daemon: daemons_core.Daemon) -> None:
    """Starts ``daemon`` again from its recorded argv, detached: it outlives the upgrade.

    A registered spawn seam (``core.spawn.SEAMS``): one of rlx's own daemons (rlxd, ``watch``,
    ``serve``), not a model process; the tracing identity is stripped."""
    options: dict[str, Any] = {}
    if sys.platform == "win32":
        options["creationflags"] = DETACHED_FLAGS
    else:
        options["start_new_session"] = True
    cwd = daemon.cwd if daemon.cwd and Path(daemon.cwd).is_dir() else None
    subprocess.Popen(
        list(daemon.argv),
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=untraced_env(),
        close_fds=True,
        **options,
    )


def _wait_started(
    daemon: daemons_core.Daemon, rlxd: Rlxd | None, timeout: float
) -> daemons_core.Daemon | None:
    """The record of ``daemon`` started again: a live one of its kind on this host with
    another pid (rlxd: and answering ``status``), within ``timeout``; ``None``."""
    host = registry.host_name()
    deadline = time.monotonic() + timeout
    while True:
        for record in daemons_core.list_live(kind=daemon.kind, prune=False):
            answers = daemon.kind != "rlxd" or rlxd is None or rlxd.daemon_status() is not None
            if record.host == host and record.pid != daemon.pid and answers:
                return record
        if time.monotonic() >= deadline:
            return None
        time.sleep(0.2)


@dataclass
class Around:
    """The daemons' part of one upgrade, which ``services.lifecycle.upgrade`` calls around
    its installer (:meth:`before_install`, :meth:`installed`), and the command after it
    (:meth:`finish`). It carries the steps' events, and what it stopped, so that whatever
    fails, what it stopped is started again."""

    found: Census
    options: Options
    events: Events
    rlxd: Rlxd | None
    to_version: str | None = None
    stopped: list[daemons_core.Daemon] = field(default_factory=list)
    restarted: list[str] = field(default_factory=list)
    failure: Refused | None = None
    started_again: bool = False
    rlxd_installed: bool = field(default_factory=lambda: installed_rlxd() is not None)
    new_install: Callable[[], Sequence[str]] | None = None
    """The new install's ``python -P -m aisquare`` (its interpreter), for ``daemon install``."""

    def before_install(self) -> None:
        """``stop_daemons``: with ``--restart-daemons``, ends the live PTYs (``--end-ptys``)
        and stops each restartable daemon of this host, rlxd last. A stop that fails starts
        again what was stopped, and raises: nothing is installed."""
        if not self.options.restart_daemons:
            self.events.step("stop_daemons", "skipped")
            self.events.step("install", "started")
            return
        self.events.step("stop_daemons", "started")
        if self.options.end_ptys and self.rlxd is not None:
            for pty in self.found.ptys:
                with contextlib.suppress(RlxdError):  # ending rlxd ends it anyway
                    self.rlxd.call("pty.kill", {"pty_id": pty.get("pty_id")})
        targets = [daemon for daemon in self.found.here if daemon.restartable]
        targets.sort(key=lambda daemon: daemon.kind == "rlxd")
        for daemon in targets:
            try:
                stop_daemon(daemon, self.rlxd)
            except Refused as refused:
                self.events.step("stop_daemons", "failed", refused.message)
                self.start_again()
                raise
            self.stopped.append(daemon)
        kinds = ", ".join(daemon.kind for daemon in self.stopped)
        self.events.step("stop_daemons", "done", f"stopped: {kinds}" if kinds else None)
        self.events.step("install", "started")

    def installed(self, version: str | None, problem: str | None) -> None:
        """The install's end: on a problem, what was stopped starts again from what is in
        place; else ``rlxd``, the new install's own ``daemon install``, which swaps rlxd and
        rlxd-hook, both or neither, after checking them against the release's sums."""
        if problem is not None:
            self.events.step("install", "failed", problem)
            self.start_again()
            return
        self.to_version = version
        self.events.step("install", "done", version)
        self.swap_rlxd()

    def swap_rlxd(self) -> None:
        """The ``rlxd`` step (the class's doc)."""
        if not self.rlxd_installed and not any(d.kind == "rlxd" for d in self.found.here):
            self.events.step("rlxd", "skipped", "rlxd is not installed beside rlx")
            return
        if self.new_install is None:
            self.events.step("rlxd", "skipped", "the new install could not be asked")
            return
        self.events.step("rlxd", "started")
        argv = [*self.new_install(), "--json", "daemon", "install"]
        done = install_route.run_captured(argv, timeout=INSTALL_TIMEOUT_SECONDS)
        if done.error is not None:
            self._rlxd_failed("install_failed", done.error)
            return
        if done.returncode == 0:
            self.events.step("rlxd", "done", self.to_version)
            return
        said = _json_object(done.stdout)
        code = str(said.get("error") or "install_failed")
        if "No such command" in (done.stderr or "") + (done.stdout or ""):
            self.events.step(
                "rlxd", "skipped", f"{self.to_version} has no `daemon install`: rlxd is kept"
            )
            return
        self._rlxd_failed(code, str(said.get("detail") or done.stderr.strip() or code))

    def _rlxd_failed(self, code: str, detail: str) -> None:
        self.events.step("rlxd", "failed", f"{code}: {detail}")
        self.failure = Refused(
            "rlxd_install_failed",
            f"rlxd {self.to_version} could not be installed ({code}): the rlxd in place is "
            "kept, and runs again",
            code,
        )

    def finish(self, hooks: Sequence[HookRefresh] | None) -> None:
        """After the install: ``refresh_hooks`` (what the install's hook refresh did, when it
        ran), then ``start_daemons``."""
        if hooks is not None:
            failed = [str(hook.config_dir) for hook in hooks if not hook.ok]
            if failed:
                self.events.step("refresh_hooks", "failed", ", ".join(failed))
            else:
                self.events.step("refresh_hooks", "done")
        self.start_again()

    def start_again(self) -> None:
        """``start_daemons``: each daemon stopped, rlxd first, from its recorded argv; rlxd
        waited for until it answers. Once only."""
        if self.started_again:
            return
        self.started_again = True
        if not self.stopped:
            self.events.step("start_daemons", "skipped")
            return
        self.events.step("start_daemons", "started")
        failed: list[str] = []
        for daemon in sorted(self.stopped, key=lambda d: d.kind != "rlxd"):
            try:
                spawn_daemon(daemon)
            except OSError as exc:
                failed.append(f"{daemon.kind}: {exc}")
                continue
            if _wait_started(daemon, self.rlxd, START_TIMEOUT_SECONDS) is None:
                failed.append(f"{daemon.kind}: did not come up in {START_TIMEOUT_SECONDS:.0f} s")
                continue
            self.restarted.append(daemon.kind)
        if failed:
            self.events.step("start_daemons", "failed", "; ".join(failed))
            if self.failure is None:
                self.failure = Refused(
                    "daemon_start_failed",
                    f"not every daemon started again: {'; '.join(failed)}. Start them by hand "
                    "(rlxd serve --detach for rlxd).",
                    "; ".join(failed),
                )
            return
        self.events.step("start_daemons", "done", f"started: {', '.join(self.restarted)}")


def _json_object(text: str) -> dict[str, Any]:
    try:
        value = json.loads(text)
    except ValueError:
        return {}
    return value if isinstance(value, dict) else {}


def new_install_command(plan: UpgradePlan) -> Callable[[], Sequence[str]]:
    """The new install's ``python -P -m aisquare``: on the interpreter the version check asks,
    never this process's, whose files the installer replaced."""
    return lambda: [str(plan.route.facts.executable), "-P", "-m", "aisquare"]


def done_or_error(
    around: Around, report: UpgradeReport | None, events: Events, *, from_version: str
) -> Refused | None:
    """The upgrade's end, said: ``upgrade.done`` with what restarted, or the failure that
    stopped it (also said, as ``upgrade.error``)."""
    if around.failure is not None:
        events.error(around.failure)
        return around.failure
    to = report.version if report is not None and report.version else from_version
    events.done(from_version, to, around.restarted)
    return None
