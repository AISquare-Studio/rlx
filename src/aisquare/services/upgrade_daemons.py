"""rlx's daemons around ``rlx upgrade`` (handoff 0.10.3, 4.2, and 7.8's Upgrade part).

An upgrade replaces the code every long-lived rlx process runs. rlxd, a detached ``watch``,
``serve`` and the rest (the daemon registry, ``core.daemons``) keep the code they started
with: on macOS and Linux a mix of releases until they restart, on Windows files in use that
fail the reinstall. So the upgrade counts them, and with ``--restart-daemons`` stops them
before the swap and starts them again after it.

- **The census** (:func:`census`), which ``upgrade --check`` adds to its fields: ``daemons``
  (the live records, and the held ones this release can't read), ``runs_in_flight`` (rlxd's
  queued and running runs) and ``ptys`` (rlxd's live PTYs, which a restart of rlxd ends).
  It fails closed: when rlxd runs but its runs or PTYs can't be read, they are unknown, and
  an upgrade refuses on unknown as it does on some.
- **Refusals** (:func:`refusal`), only for an upgrade that will run (the route, up-to-date,
  asq and confirm checks come first), and before anything stops: on Windows any daemon
  (``daemon_running``) unless ``--restart-daemons``; with ``--restart-daemons``, a live PTY
  (``ptys_live``) unless ``--end-ptys``; and runs in flight (``runs_in_flight``) unless
  ``--wait-runs N``, which waits up to N minutes for them to end (:func:`wait_for_runs`).
  Counted again right before the first stop (:meth:`Around.before_install`): a PTY or run
  that appeared meanwhile refuses as before. On macOS and Linux, daemons left running are
  only warned of.
- **Around the install** (:class:`Around`, which ``services.lifecycle.upgrade`` calls): the
  stop of each daemon of this host that is restartable and whose recorded command is rlx's
  own (:func:`restart_refusal`), rlxd last (``stop_daemons``); the install; the swap of
  rlxd's binaries by the NEW install's ``daemon install`` (``rlxd``: #593's verified, atomic
  install); then, once the hooks are refreshed, each stopped daemon started again from its
  recorded argv (``start_daemons``), rlxd waited for until it reports the expected version
  from a new pid. Whatever ends the upgrade (a failure, an exception, Ctrl-C, SIGTERM:
  :func:`guard_daemons`), what it stopped is started again from what is in place, the old
  binaries included. A daemon asked to stop that still runs when the upgrade gives up on it
  keeps running, and a detached helper starts it again once it has stopped
  (:func:`restart_when_gone`): an upgrade never leaves no rlxd.
- **Events** (:class:`Events`): with ``--json-lines``, each step is ``upgrade.step {step,
  status, detail?}``, and the end ``upgrade.done {from, to, restarted}`` or ``upgrade.error
  {code, message, detail}``, 7.3 envelopes with a local ``seq``, on stdout and nothing else.

rlxd is reached through the command line installed beside rlx (``rlxd status --json``,
``rlxd call …``, ``rlxd stop``), never a program a record names: no client of its protocol
lives here.

Everything this module uses is imported at its top: it runs on both sides of the moment the
installer replaces rlx's files, after which nothing may be imported (``services.lifecycle``).
"""

from __future__ import annotations

import contextlib
import json
import math
import os
import re
import signal
import stat
import subprocess
import sys
import threading
import time
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass, field, replace
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any, TextIO

from aisquare.core import daemons as daemons_core
from aisquare.core import locking, procs, registry
from aisquare.core import program as prog
from aisquare.core.spawn import untraced_env
from aisquare.core.state import FLAGS, RuntimeState
from aisquare.core.version import DISTRIBUTION
from aisquare.services import install_route, rlxd_install

if TYPE_CHECKING:
    from aisquare.services.lifecycle import HookRefresh, UpgradePlan, UpgradeReport

STEPS = ("check", "stop_daemons", "install", "rlxd", "refresh_hooks", "start_daemons")
"""7.8's ``upgrade.step`` names, in the order the upgrade takes them."""

STOP_TIMEOUT_SECONDS = 30.0
"""How long a daemon asked to stop has to be gone, its lock let go and its process ended,
before the upgrade gives up on it. Longer than ``rlxd stop``'s own 10 s: an rlxd that took
the request finishes it after its command line has given up."""

START_TIMEOUT_SECONDS = 30.0
"""How long a daemon started again has to come up: registered again (rlxd: answering)."""

RESTART_WHEN_GONE_SECONDS = 3600.0
"""How long :func:`restart_when_gone`'s helper waits for a daemon still stopping to end."""

RESTART_GRACE_SECONDS = 10.0
"""How long the helper gives a daemon whose lock is let go to end its process too, before
it starts the daemon again anyway: a process that let its lock go is ending (one whose
parent never waits for it stays a zombie)."""

RLXD_CALL_TIMEOUT_SECONDS = 30.0
"""How long one ``rlxd`` command may take."""

CHECK_CALL_TIMEOUT_SECONDS = 3.0
"""How long one ``rlxd`` command may take for ``upgrade --check``, which the app reads: a
frozen rlxd makes it say its runs and PTYs are unknown, not wait."""

INSTALL_TIMEOUT_SECONDS = 300.0
"""How long the new install's ``daemon install`` may take: a download and a check."""

WAIT_POLL_SECONDS = 1.0
"""How often ``--wait-runs`` asks rlxd for its runs."""

WAIT_RUNS_MAX_MINUTES = 1440.0
"""The longest ``--wait-runs`` takes: a day."""

PACKAGE_ENV = "AISQUARE_INSTALL_PACKAGE"
"""A wheel file to install in place of PyPI's release, as install.sh takes it (4.2's test
hook, set by CI only)."""

RUN_STATES = ("queued", "running")
"""The runs an upgrade waits for: still to start, or running."""

DETACHED_FLAGS = 0x00000008 | 0x00000200
"""Windows' ``DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP``: a daemon started again outlives
the upgrade, with no console of its own."""

_RESTART_WHEN_GONE = "restart-when-gone"
"""The helper's command (``python -m aisquare.services.upgrade_daemons restart-when-gone``)."""

RESTART_LOG = "restart.log"
"""The helpers' log, in :func:`restarts_dir`."""


class Refused(Exception):
    """Why the upgrade stops: a 7.8 ``upgrade.error`` code, a message and a detail."""

    def __init__(self, code: str, message: str, detail: str | None = None) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.detail = detail


class Interrupted(BaseException):
    """SIGTERM, raised where the upgrade runs (:func:`terminating`), so that it unwinds and
    starts again what it stopped. A ``BaseException``, as ``KeyboardInterrupt`` is."""


@dataclass(frozen=True)
class Options:
    """The upgrade's daemon flags."""

    restart_daemons: bool = False
    end_ptys: bool = False
    wait_runs: float | None = None
    """``--wait-runs``: minutes to wait for runs in flight to end; ``None`` refuses at once."""

    def __post_init__(self) -> None:
        wait = self.wait_runs
        if wait is not None and not (math.isfinite(wait) and 0 <= wait <= WAIT_RUNS_MAX_MINUTES):
            raise ValueError(f"--wait-runs takes 0 to {WAIT_RUNS_MAX_MINUTES:.0f} minutes")


# --- events ------------------------------------------------------------------------------

_SAID_TO_A_PERSON = frozenset({"stop_daemons", "rlxd", "start_daemons"})
"""The steps a run without ``--json-lines`` says on stderr: what its report does not."""


class Events:
    """The upgrade's steps, said: with ``json_lines`` as 7.3 envelopes on ``out`` (stdout,
    nothing else written there), otherwise as lines for a person on stderr."""

    def __init__(self, *, json_lines: bool, out: TextIO | None = None, err: TextIO | None = None):
        self.json_lines = json_lines
        self._out = out
        self._err = err
        self._seq = 0
        self._open: str | None = None

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
        ``skipped`` or ``failed``. The step started last is open until it ends."""
        data: dict[str, Any] = {"step": step, "status": status}
        if detail is not None:
            data["detail"] = detail
        if status == "started":
            self._open = step
        elif self._open == step:
            self._open = None
        self.emit("upgrade.step", data)
        if (
            not self.json_lines
            and detail is not None
            and status in ("done", "failed")
            and step in _SAID_TO_A_PERSON
        ):
            self.say_line(f"{'✗' if status == 'failed' else '·'} {step}: {detail}")

    def end_open_step(self, code: str) -> None:
        """The step still open, ended ``failed`` with ``code``: what an error ended."""
        if self._open is not None:
            self.step(self._open, "failed", code)

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
    timeout: float = RLXD_CALL_TIMEOUT_SECONDS

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
            timeout=self.timeout,
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
        """``rlxd stop``, which returns once rlxd has stopped, or gives up after 10 s while
        rlxd goes on stopping (``stop_timed_out``)."""
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


def find_rlxd(timeout: float = RLXD_CALL_TIMEOUT_SECONDS) -> Rlxd | None:
    """The rlxd command line to ask: the one installed beside rlx, which talks to this home's
    running rlxd whatever program that runs; ``None`` when there is none. Never a program a
    record names: ``upgrade --check`` reads, and runs nothing a JSON file says to."""
    installed = installed_rlxd()
    return None if installed is None else Rlxd((str(installed),), timeout)


# --- the census ----------------------------------------------------------------------------


def _unreadable_kind(record: registry.Unreadable) -> str:
    kind = (record.said or {}).get("kind")
    return kind if isinstance(kind, str) and kind else "unknown"


@dataclass(frozen=True)
class Census:
    """The daemons, runs in flight and live PTYs an upgrade must reckon with."""

    daemons: tuple[daemons_core.Daemon, ...] = ()
    unreadable: tuple[registry.Unreadable, ...] = ()
    """Held records this release can't read: running daemons it can neither stop nor start
    again, counted as this host's (4.1: reported, never silently dropped)."""
    runs: tuple[dict[str, Any], ...] = ()
    ptys: tuple[dict[str, Any], ...] = ()
    rlxd_error: str | None = None
    """Why rlxd's runs and PTYs are not known though an rlxd is registered: unknown, which
    refuses as some would."""
    pending: tuple[dict[str, Any], ...] = ()
    """Helpers waiting to start a daemon again once it has ended (:func:`pending_restarts`)."""
    rlxd_version: str | None = None
    """The version this host's rlxd says it is (``status``), when it answers."""

    @property
    def here(self) -> tuple[daemons_core.Daemon, ...]:
        """The daemons of this host: the ones this upgrade can stop and start."""
        host = registry.host_name()
        return tuple(daemon for daemon in self.daemons if daemon.host == host)

    @property
    def held(self) -> list[str]:
        """What runs on this host, named: its daemons, and the held records not read."""
        unread = [f"{_unreadable_kind(u)} (record {u.id}, not readable)" for u in self.unreadable]
        return [daemon.label for daemon in self.here] + unread

    @property
    def rlxd_runs(self) -> bool:
        """Whether this host's rlxd is registered as running."""
        return any(daemon.kind == "rlxd" for daemon in self.here)

    def as_json(self) -> dict[str, Any]:
        """``upgrade --check``'s three fields (and why rlxd's two are unknown, if they are)."""
        daemons: list[dict[str, Any]] = [
            {
                "id": daemon.id,
                "kind": daemon.kind,
                "pid": daemon.pid,
                "host": daemon.host,
                "version": daemon.version,
                "restartable": daemon.restartable,
                "argv": list(daemon.argv),
                "started_at": daemon.started_at.isoformat(),
                "readable": True,
            }
            for daemon in self.daemons
        ]
        for record in self.unreadable:
            said = record.said or {}
            pid = said.get("pid")
            daemons.append(
                {
                    "id": record.id,
                    "kind": _unreadable_kind(record),
                    "pid": pid if isinstance(pid, int) and not isinstance(pid, bool) else None,
                    "host": None,
                    "version": None,
                    "restartable": False,
                    "argv": [],
                    "started_at": None,
                    "readable": False,
                }
            )
        return {
            "daemons": daemons,
            "runs_in_flight": [
                {key: run.get(key) for key in ("run_id", "kind", "state", "job_id")}
                for run in self.runs
            ],
            "ptys": [
                {key: pty.get(key) for key in ("pty_id", "label", "pid", "argv")}
                for pty in self.ptys
            ],
            "rlxd_error": self.rlxd_error,
            "rlxd_version": self.rlxd_version,
            "restarts_pending": list(self.pending),
        }


NOT_BESIDE = "rlxd runs, but no rlxd is installed beside rlx to ask"
"""Why rlxd's runs and PTYs are unknown when the rlxd that runs is not the one beside rlx (a
build of its own): waiting can't help, only stopping it."""


def _unknown_advice(error: str, waiting: str) -> str:
    """What to do about runs or PTYs rlxd could not say: ``waiting``, unless no rlxd beside rlx
    can ask, when only stopping the rlxd that runs helps."""
    if error == NOT_BESIDE:
        install = prog.shell_advice(
            f", or install it beside rlx with {prog.words('daemon', 'install')}"
        )
        return f"stop the rlxd that runs (rlxd stop) and upgrade{install}"
    return waiting


def _runs_now(rlxd: Rlxd) -> tuple[dict[str, Any], ...]:
    runs = rlxd.call("run.list", {"states": list(RUN_STATES)}).get("runs") or []
    return tuple(run for run in runs if isinstance(run, dict))


def census(rlxd: Rlxd | None = None, *, ask_rlxd: bool = True) -> Census:
    """What runs now: the live daemons of this home and its held unreadable records (read
    only: nothing pruned), and, when this host's rlxd runs and ``ask_rlxd``, its queued and
    running runs and its live PTYs, or why they are unknown."""
    found_roll = daemons_core.roll(prune=False)
    found = Census(
        daemons=tuple(found_roll.live),
        unreadable=tuple(record for record in found_roll.unreadable if record.held),
        pending=pending_restarts(),
    )
    if not ask_rlxd or not found.rlxd_runs:
        return found
    rlxd = rlxd if rlxd is not None else find_rlxd()
    if rlxd is None:
        return replace(found, rlxd_error=NOT_BESIDE)
    try:
        runs = _runs_now(rlxd)
        ptys = rlxd.call("pty.list").get("ptys") or []
    except RlxdError as exc:
        return replace(found, rlxd_error=str(exc))
    version = (rlxd.daemon_status() or {}).get("version")
    return replace(
        found,
        runs=runs,
        ptys=tuple(pty for pty in ptys if isinstance(pty, dict) and not pty.get("exited")),
        rlxd_version=version if isinstance(version, str) else None,
    )


def rlxd_lags(current: str) -> str | None:
    """:func:`rlxd_behind` for an upgrade with nothing to install: only rlxd's ``status``,
    asked with the check's short timeout, and only when this host's rlxd is registered."""
    if not census(ask_rlxd=False).rlxd_runs:
        return None
    rlxd = find_rlxd(timeout=CHECK_CALL_TIMEOUT_SECONDS)
    version = (rlxd.daemon_status() or {}).get("version") if rlxd is not None else None
    return rlxd_behind(Census(rlxd_version=version if isinstance(version, str) else None), current)


def rlxd_behind(found: Census, current: str) -> str | None:
    """What to do when this host's rlxd runs another version than rlx's ``current`` (after
    a swap that failed, say: rlx moved on and rlxd did not), or ``None`` when it doesn't,
    or doesn't say."""
    version = found.rlxd_version
    if version is None or install_route.same_version(version, current):
        return None
    install = prog.shell_advice(f": install it with {prog.words('daemon', 'install')}")
    return f"rlxd runs {version}, not rlx's {current}{install}"


# --- refusals -------------------------------------------------------------------------------


def refusal(found: Census, options: Options, *, windows: bool) -> Refused | None:
    """Why the upgrade must not go on as asked, before anything stops; ``None`` when it may.
    Unknown runs or PTYs refuse as some would. Runs in flight are refused only when
    ``--wait-runs`` was not given (it waits instead)."""
    held = found.held
    if windows and held and not options.restart_daemons:
        return Refused(
            "daemon_running",
            "rlx daemons are running, and Windows can't replace the files of a running "
            f"program: {', '.join(held)}. Add --restart-daemons to stop them for the "
            "upgrade and start them again after it.",
            ", ".join(held),
        )
    if options.restart_daemons:
        unrecognized = [
            f"{daemon.label}: {why}"
            for daemon in found.here
            if daemon.restartable and (why := restart_refusal(daemon)) is not None
        ] + [f"{_unreadable_kind(u)} (record {u.id}): not readable" for u in found.unreadable]
        if unrecognized:
            return Refused(
                "daemon_unrecognized",
                "--restart-daemons stops and starts again only the daemons whose record is the "
                f"one rlx itself writes, and these are not: {'; '.join(unrecognized)}. Stop "
                "them yourself, then upgrade.",
                "; ".join(unrecognized),
            )
    if options.restart_daemons and not options.end_ptys:
        if found.ptys:
            many = len(found.ptys) != 1
            them = "them" if many else "it"
            return Refused(
                "ptys_live",
                f"{len(found.ptys)} PTY{'s are' if many else ' is'} live in rlxd, and "
                f"restarting rlxd ends {them}: add --end-ptys to end {them}, or upgrade once "
                f"{'they have' if many else 'it has'} exited.",
                ", ".join(str(pty.get("pty_id")) for pty in found.ptys),
            )
        if found.rlxd_error is not None:
            advice = _unknown_advice(
                found.rlxd_error, "upgrade once rlxd answers, or add --end-ptys"
            )
            return Refused(
                "ptys_live",
                f"rlxd's PTYs can't be read ({found.rlxd_error}), and restarting rlxd ends any "
                f"that are live: {advice}.",
                found.rlxd_error,
            )
    if options.wait_runs is None:
        if found.runs:
            return _runs_refused(found.runs)
        if found.rlxd_error is not None:
            return _runs_unknown(found.rlxd_error)
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


def _runs_unknown(error: str) -> Refused:
    advice = _unknown_advice(error, "upgrade once rlxd answers, or add --wait-runs <minutes>")
    return Refused(
        "runs_in_flight",
        f"rlxd's runs can't be read ({error}), and the upgrade replaces rlxd-hook under any "
        f"in flight: {advice}.",
        error,
    )


def windows_refusal(options: Options, command: str | None) -> Refused | None:
    """On Windows, where rlx can't replace itself (its route refuses), the daemons that run
    named as well, without --restart-daemons: they hold rlx's files as rlx does, so the
    command run by hand fails until they stop (4.2's ``daemon_running``). Read from the
    registry only; ``None`` with --restart-daemons or none running."""
    if options.restart_daemons:
        return None
    held = census(ask_rlxd=False).held
    if not held:
        return None
    then = f", then run: {command}" if command else ", then reinstall it"
    return Refused(
        "daemon_running",
        "rlx can't upgrade itself on Windows, and rlx daemons are running, whose files "
        f"Windows can't replace either: {', '.join(held)}. Stop them (`rlxd stop` for rlxd) "
        f"and quit rlx{then}",
        ", ".join(held),
    )


def network_refusal(plan: UpgradePlan) -> Refused | None:
    """7.8's ``network``: PyPI could not say what is newest, and nothing names the release
    (no ``--version``, no package), so the install would fail offline after the daemons
    stopped. ``None`` otherwise."""
    latest = plan.latest
    if plan.target is not None or latest is None or latest.error is None:
        return None
    return Refused(
        "network",
        f"PyPI could not be asked for rlx's latest release ({latest.error}): nothing was "
        "stopped or installed. Try again once it can be reached, or name one with --version.",
        latest.error,
    )


def mixed_releases(found: Census, options: Options) -> str | None:
    """On macOS and Linux, the warning for daemons left running across the upgrade."""
    held = found.held
    if not held or options.restart_daemons:
        return None
    return (
        f"{', '.join(held)} keep{'s' if len(held) == 1 else ''} running the release "
        "it started with until restarted (add --restart-daemons to restart them with the "
        "upgrade)"
    )


def wait_for_runs(
    found: Census, options: Options, rlxd: Rlxd | None, events: Events
) -> tuple[Census, str | None]:
    """With ``--wait-runs``, waits up to its minutes for the runs in flight (or unknown) to
    end: the census then, and what was waited for. :class:`Refused` ``runs_in_flight`` when
    they have not, or could not be read by then."""
    wait = options.wait_runs
    if wait is None or (not found.runs and found.rlxd_error is None):
        return found, None
    if rlxd is None:
        raise _runs_refused(found.runs) if found.runs else _runs_unknown(str(found.rlxd_error))
    began = time.monotonic()
    deadline = began + wait * 60
    events.say_line(f"· waiting up to {wait:g} min for rlxd's runs in flight to end")
    runs, error = found.runs, found.rlxd_error
    while runs or error is not None:
        if time.monotonic() >= deadline:
            raise _runs_refused(runs) if runs else _runs_unknown(str(error))
        time.sleep(WAIT_POLL_SECONDS)
        try:
            runs, error = _runs_now(rlxd), None
        except RlxdError as exc:
            error = str(exc)
    waited = f"waited {time.monotonic() - began:.0f} s for rlxd's runs in flight to end"
    return replace(found, runs=(), rlxd_error=None), waited


# --- the package to install ---------------------------------------------------------------


def wheel_version(package: str) -> str | None:
    """The version a wheel's file name gives (``name-version-…whl``, PEP 427)."""
    name = Path(package).name
    if not name.endswith(".whl"):
        return None
    parts = name[: -len(".whl")].split("-")
    return parts[1] if len(parts) >= 5 else None


def package_wheel() -> Path | None:
    """:data:`PACKAGE_ENV`'s wheel, a file on this machine; ``None`` when it is unset.
    :class:`Refused` for anything else: a URL, a missing file, a name that gives no
    version."""
    value = os.environ.get(PACKAGE_ENV, "").strip()
    if not value:
        return None
    path = Path(value).expanduser()
    if "://" in value or not path.is_file() or wheel_version(path.name) is None:
        raise Refused(
            "invalid_version",
            f"{PACKAGE_ENV} names {value!r}: it takes a wheel file on this machine "
            "(name-version-….whl), nothing else",
        )
    return path.resolve()


def with_package(
    plan: UpgradePlan, lifecycle: Any, *, pinned: bool
) -> tuple[UpgradePlan, Path | None]:
    """``plan`` to install :data:`PACKAGE_ENV`'s wheel instead of PyPI's release, when it is
    set, as install.sh installs it, and that wheel; ``(plan, None)`` when it is unset. The
    version the wheel's name gives is the target, which the new process must report, and
    the reinstall keeps the receipt's Python, ``--with``s and options, as every reinstall
    does. The route is judged as the uv tool the reinstall makes: every reason it gives
    (Windows, an unreadable receipt, another tool environment, no uv) still refuses; only
    the ones about where the running install came from and about a pin are dropped.

    :class:`Refused` for a package that is no local wheel, with ``--version`` (``pinned``),
    or for an install that is no uv tool. ``lifecycle`` is ``services.lifecycle``, passed
    in: the hook sites and the running fleet it reads only for a plan it runs."""
    if not os.environ.get(PACKAGE_ENV, "").strip():
        return plan, None
    if pinned:
        raise Refused(
            "invalid_version",
            f"{PACKAGE_ENV} and --version both name what to install: give one of them",
        )
    wheel = package_wheel()
    route = plan.route
    if (
        wheel is None
        or route.receipt is None
        or route.kind not in (install_route.UV_TOOL, install_route.LOCAL_SOURCE)
    ):
        raise Refused(
            "upgrade_unsupported_route",
            f"{PACKAGE_ENV} is set, but this install was not made by uv as a tool, the only "
            f"install it can reinstall ({route.describe()})",
            route.describe(),
        )
    receipt = route.receipt
    extras = f"[{','.join(receipt.extras)}]" if receipt.extras else ""
    # install_route._uv_install_argv's prefix (a test holds the two together), the spec ours.
    argv = ["uv", "tool", "install", "--force", "--python"]
    argv.append(receipt.python or route.facts.python_version)
    for requirement in receipt.withs:
        argv.extend(["--with", requirement])
    argv.extend(receipt.options)
    argv.append(f"{DISTRIBUTION}{extras} @ {wheel.as_uri()}")
    refresh, left = lifecycle.refresh_sites(route.facts)
    live, _unlistened, fleet_error = lifecycle.running_fleet()
    planned = replace(
        plan,
        target=wheel_version(wheel.name),
        argv=tuple(argv),
        reason=install_route.not_automated(replace(route, kind=install_route.UV_TOOL)),
        pin_refused=False,
        pin_unmet=None,
        refresh=refresh,
        left=left,
        live_agents=live,
        fleet_error=fleet_error,
        remote=lifecycle.running_remote(),
        asqs=lifecycle.running_asqs(),
    )
    return planned, wheel


# --- what may be stopped and started --------------------------------------------------------


RLXD_FLAGS: tuple[tuple[str, ...], ...] = ((), ("--gate-dev-mode",))
"""What rlxd writes after ``serve --detach`` in its record (rlxd's ``serve::dev_flags``):
nothing, or gate dev mode's flag."""


def rlxd_command() -> tuple[str, ...] | None:
    """The command an rlxd record starts with, as rlxd writes it: the rlxd installed beside
    rlx, ``serve --detach``. ``None`` when no rlxd is installed beside rlx."""
    installed = installed_rlxd()
    return None if installed is None else (str(installed), "serve", "--detach")


def python_command() -> tuple[str, ...]:
    """The command a record of rlx's own Python daemons starts with, as
    ``core.selfcli.argv_for`` writes it: this install's interpreter, ``-P -m aisquare``."""
    return (sys.executable, "-P", "-m", "aisquare")


def _global_flags(rest: Sequence[str]) -> int:
    """How many of ``rest``'s first items are global flags as ``core.selfcli.global_flags``
    writes them (``core.state.FLAGS``): a boolean one bare, a valued one as
    ``--flag=VALUE``, ``--json`` never."""
    unset = RuntimeState()
    named = [(flag, getattr(unset, field)) for field, flag in FLAGS.items()]
    bare = {flag for flag, default in named if isinstance(default, bool) and flag != "--json"}
    valued = [f"{flag}=" for flag, default in named if not isinstance(default, bool)]
    count = 0
    for arg in rest:
        if arg in bare or any(arg.startswith(at) and len(arg) > len(at) for at in valued):
            count += 1
            continue
        break
    return count


def _rlx_verb(kind: str, verb: Sequence[str]) -> bool:
    """Whether ``verb``, what follows the global flags, is what rlx writes for a restartable
    daemon of ``kind``: ``watch --detach`` (``services.watch``, the one restartable watch)
    and ``serve --port N --bind HOST`` (``cli.serve``, the one restartable serve)."""
    if kind == "watch":
        return list(verb) == ["watch", "--detach"]
    return (
        kind == "serve"
        and len(verb) == 5
        and (verb[0], verb[1], verb[3]) == ("serve", "--port", "--bind")
        and re.fullmatch(r"[0-9]{1,5}", verb[2]) is not None
        and 0 < int(verb[2]) < 65536
        and bool(verb[4])
        and not verb[4].startswith("-")
    )


def _same_program(given: str, expected: str) -> str | None:
    """Why ``given``, a record's ``argv[0]``, is not the program ``expected``, or ``None``."""
    program = Path(given)
    if not program.is_absolute() or not program.is_file():
        return f"its program {given} is not a file"
    try:
        same = os.path.samefile(program, expected)
    except OSError:
        same = False
    return None if same else f"it runs {given}, not {expected}"


def _registry_untrusted() -> str | None:
    """Why the registry's folder can't be trusted to name programs to run (POSIX: it must be
    0700 and this user's), or ``None``."""
    if sys.platform == "win32":
        return None
    try:
        found = daemons_core.registry_dir().stat()
    except OSError as exc:
        return f"its folder can't be read ({exc})"
    if found.st_uid != os.getuid():
        return "its folder is not this user's"
    if found.st_mode & 0o077:
        return f"its folder is open to others ({stat.filemode(found.st_mode)})"
    return None


def restart_refusal(daemon: daemons_core.Daemon) -> str | None:
    """Why the upgrade won't stop ``daemon`` and run its recorded argv again; ``None`` when it
    will: restartable, its argv exactly what rlx itself writes for its kind, its program the
    same file, the registry private.

    - **rlxd:** :func:`rlxd_command`, then nothing or ``--gate-dev-mode`` (:data:`RLXD_FLAGS`).
    - **watch, serve:** :func:`python_command`, rlx's global flags, then the verb rlx writes
      (:func:`_rlx_verb`). Nothing else: not ``python -c``, not another module.
    - **Any other kind** (the brain, rlxd's child; a later release's): not restarted.

    The argv is run as a list, never through a shell."""
    if not daemon.restartable:
        return "not restartable"
    argv = list(daemon.argv)
    if daemon.kind == "rlxd":
        command = rlxd_command()
        if command is None:
            return "no rlxd is installed beside rlx to start it again"
        if tuple(argv[len(command) :]) not in RLXD_FLAGS:
            return "its command is not the one rlxd writes"
    elif daemon.kind in ("watch", "serve"):
        command = python_command()
        rest = argv[len(command) :]
        if not _rlx_verb(daemon.kind, rest[_global_flags(rest) :]):
            return f"its command is not the one rlx writes for a {daemon.kind} (it runs {argv[0]})"
    else:
        return f"rlx doesn't restart a {daemon.kind}"
    if argv[1 : len(command)] != list(command[1:]):
        return "its command is not rlx's"
    wrong = _same_program(argv[0], command[0])
    if wrong is not None:
        return wrong
    untrusted = _registry_untrusted()
    if untrusted is not None:
        return f"the daemon registry can't be trusted: {untrusted}"
    return None


# --- stopping and starting -------------------------------------------------------------------


def _live_ids() -> set[str]:
    return {daemon.id for daemon in daemons_core.list_live(prune=False)}


def _gone(daemon: daemons_core.Daemon) -> bool:
    """Whether ``daemon`` has ended: its lock let go and its process gone. A process of ours
    (one this process started) that ended is a zombie, alive to ``kill(pid, 0)``, until it
    is waited for: it is, here."""
    if daemon.id in _live_ids():
        return False
    if sys.platform != "win32":
        with contextlib.suppress(OSError):  # ChildProcessError: not ours, nothing to reap
            os.waitpid(daemon.pid, os.WNOHANG)
    return not procs.alive(daemon.pid)


def _wait_gone(daemon: daemons_core.Daemon, timeout: float) -> bool:
    """Whether ``daemon`` ends (:func:`_gone`) within ``timeout``."""
    deadline = time.monotonic() + timeout
    while not _gone(daemon):
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.2)
    return True


def stop_daemon(daemon: daemons_core.Daemon, rlxd: Rlxd | None) -> None:
    """Asks ``daemon`` to stop (rlxd: ``rlxd stop``; any other: SIGTERM, on Windows
    terminated), then waits up to :data:`STOP_TIMEOUT_SECONDS` for it to be gone. An rlxd
    that took the request finishes it even after its command line gave up, so a failed
    ``rlxd stop`` is waited out like any other request. :class:`Refused`
    ``daemon_stop_failed`` when it still runs then."""
    said = None
    try:
        if daemon.kind == "rlxd" and rlxd is not None:
            rlxd.stop()
        else:
            os.kill(daemon.pid, signal.SIGTERM)
    except (OSError, RlxdError) as exc:
        said = str(exc)
    if _wait_gone(daemon, STOP_TIMEOUT_SECONDS):
        return
    raise Refused(
        "daemon_stop_failed",
        f"{daemon.label} was asked to stop and still runs after {STOP_TIMEOUT_SECONDS:.0f} s"
        f"{f' ({said})' if said else ''}: nothing was installed. It keeps running, and is "
        "started again if it ends.",
        daemon.kind,
    )


def spawn_detached(argv: Sequence[str], cwd: str | None) -> None:
    """Runs ``argv`` detached, as the registry says a daemon is started again: it outlives
    the upgrade, with no stdin, its output to a null device, in ``cwd`` when that is a
    folder. Never through a shell.

    A registered spawn seam (``core.spawn.SEAMS``): one of rlx's own daemons (rlxd,
    ``watch``, ``serve``), or :func:`restart_when_gone`'s helper; not a model process, so
    the tracing identity is stripped."""
    options: dict[str, Any] = {}
    if sys.platform == "win32":
        options["creationflags"] = DETACHED_FLAGS
    else:
        options["start_new_session"] = True
    subprocess.Popen(
        list(argv),
        cwd=cwd if cwd and Path(cwd).is_dir() else None,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=untraced_env(),
        close_fds=True,
        **options,
    )


def restarts_dir() -> Path:
    """Where a pending :func:`restart_when_gone` helper says what it waits for
    (``<id>.json``, its claim) and logs what it does (:data:`RESTART_LOG`): beside the
    daemon registry."""
    return daemons_core.registry_dir().parent / "upgrade-restarts"


def pending_restarts() -> tuple[dict[str, Any], ...]:
    """The helpers still waiting to start a daemon again (``upgrade --check``'s
    ``restarts_pending``): each claim whose lock its helper holds (a lock, not a pid, which
    the OS can hand to another process once the helper ends)."""
    try:
        claims = sorted(restarts_dir().glob("*.json"))
    except OSError:
        return ()
    found = []
    for claim in claims:
        with contextlib.suppress(OSError):
            if _helper_holds(claim.with_suffix(".lock")):
                said = _json_object(claim.read_text(encoding="utf-8"))
                found.append({key: said.get(key) for key in _CLAIM_KEYS})
    return tuple(found)


def _helper_holds(lock: Path) -> bool:
    """Whether a helper holds ``lock``, its claim on a record id."""
    try:
        handle = locking.open_lock_file(lock, create=False)
    except FileNotFoundError:
        return False
    try:
        locking.acquire(handle, wait_s=0)
    except TimeoutError:
        return True
    finally:
        os.close(handle)
    return False


_CLAIM_KEYS = ("id", "kind", "pid", "helper_pid", "since", "log")


def restart_when_gone(daemon: daemons_core.Daemon) -> Path:
    """Starts a detached helper that waits (up to :data:`RESTART_WHEN_GONE_SECONDS`) for
    ``daemon``, asked to stop and still stopping, to end, then runs its argv: a daemon the
    upgrade gave up on is started again whenever it does end. Waits a moment for the helper
    to claim it, so the upgrade's end already shows it pending; the helper's log."""
    payload = {
        "id": daemon.id,
        "kind": daemon.kind,
        "pid": daemon.pid,
        "argv": list(daemon.argv),
        "cwd": daemon.cwd,
        "wait": RESTART_WHEN_GONE_SECONDS,
        "grace": RESTART_GRACE_SECONDS,
    }
    helper = [sys.executable, "-P", "-m", __name__, _RESTART_WHEN_GONE, json.dumps(payload)]
    spawn_detached(helper, None)
    claim = restarts_dir() / f"{daemon.id}.json"
    deadline = time.monotonic() + 5.0
    while not claim.exists() and time.monotonic() < deadline:
        time.sleep(0.05)
    return restarts_dir() / RESTART_LOG


def _say_in_log(log: Path, line: str) -> None:
    """One line of the helper's log: when, which helper, what."""
    moment = datetime.now(UTC).isoformat(timespec="seconds").replace("+00:00", "Z")
    no_follow = getattr(os, "O_NOFOLLOW", 0)  # a link planted at its name is not written
    with contextlib.suppress(OSError):
        handle = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_APPEND | no_follow, 0o600)
        with os.fdopen(handle, "a", encoding="utf-8") as out:
            out.write(f"{moment} helper {os.getpid()}: {line}\n")


def _claim(lock: Path) -> int | None:
    """This helper's claim on a record id: ``lock`` held for as long as it runs (the OS lets
    it go when the helper ends, however it ends). ``None`` while another helper holds it."""
    handle = locking.open_lock_file(lock, mode=0o600)
    try:
        locking.acquire(handle, wait_s=0)
    except TimeoutError:
        os.close(handle)
        return None
    return handle


def _running_already(kind: str, argv: Sequence[str]) -> daemons_core.Daemon | None:
    """A live daemon of this host that stands for the one to start again: any rlxd (one runs
    per home), or one of ``kind`` with the same argv."""
    host = registry.host_name()
    for record in daemons_core.list_live(kind=kind, prune=False):
        if record.host == host and (kind == "rlxd" or list(record.argv) == list(argv)):
            return record
    return None


def _helper(argv: Sequence[str]) -> int:
    """The helper :func:`restart_when_gone` starts: ``restart-when-gone <payload>``. It holds
    a claim on the daemon's record id (a second helper for it does nothing), logs each step
    to :data:`RESTART_LOG`, and starts the daemon only if nothing stands for it by then (a
    restart by hand, another helper's)."""
    if len(argv) != 2 or argv[0] != _RESTART_WHEN_GONE:
        return 2
    payload = json.loads(argv[1])
    record_id, pid, kind = str(payload["id"]), int(payload["pid"]), str(payload["kind"])
    command = [str(part) for part in payload["argv"]]
    folder = restarts_dir()
    folder.mkdir(parents=True, exist_ok=True, mode=0o700)
    if sys.platform != "win32":
        if folder.stat().st_uid == os.getuid():
            folder.chmod(0o700)  # each time, not only when made
    log, claim = folder / RESTART_LOG, folder / f"{record_id}.json"
    said = {
        "id": record_id,
        "kind": kind,
        "pid": pid,
        "helper_pid": os.getpid(),
        "since": datetime.now(UTC).isoformat(timespec="seconds").replace("+00:00", "Z"),
        "log": str(log),
    }
    held = _claim(claim.with_suffix(".lock"))
    if held is None:
        _say_in_log(log, f"another helper waits for {kind} (record {record_id}): nothing to do")
        return 0
    no_follow = getattr(os, "O_NOFOLLOW", 0)
    with os.fdopen(
        os.open(claim, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | no_follow, 0o600),
        "w",
        encoding="utf-8",
    ) as out:
        json.dump(said, out)
    try:
        _say_in_log(log, f"waiting for {kind} (record {record_id}, pid {pid}) to end")
        deadline = time.monotonic() + float(payload.get("wait", RESTART_WHEN_GONE_SECONDS))
        while record_id in _live_ids():
            if time.monotonic() >= deadline:
                _say_in_log(log, f"gave up: {kind} (pid {pid}) still runs")
                return 1
            time.sleep(0.5)
        ended = time.monotonic() + float(payload.get("grace", RESTART_GRACE_SECONDS))
        while procs.alive(pid) and time.monotonic() < ended:
            time.sleep(0.2)
        running = _running_already(kind, command)
        if running is not None:
            _say_in_log(log, f"{kind} ended; another runs already (pid {running.pid}): kept")
            return 0
        spawn_detached(command, payload.get("cwd"))
        _say_in_log(log, f"{kind} ended; started again: {' '.join(command)}")
        return 0
    finally:
        claim.unlink(missing_ok=True)
        os.close(held)


@contextlib.contextmanager
def _signals_held_off() -> Iterator[None]:
    """Ctrl-C and SIGTERM held off inside (main thread only): what starts the daemons again
    is not cut short by the same signal twice. Caught by a handler that does nothing, not
    ignored: an ignored signal stays ignored across exec, so every daemon and helper started
    in here would begin deaf to SIGTERM, while a caught one is reset to its default."""
    if threading.current_thread() is not threading.main_thread():
        yield
        return

    def held_off(signum: int, frame: object) -> None:
        return None

    names = [signal.SIGINT] + ([signal.SIGTERM] if hasattr(signal, "SIGTERM") else [])
    before = {signum: signal.signal(signum, held_off) for signum in names}
    try:
        yield
    finally:
        for signum, handler in before.items():
            signal.signal(signum, handler)


@contextlib.contextmanager
def terminating() -> Iterator[None]:
    """Inside, SIGTERM raises :class:`Interrupted` (once; another is ignored until the block
    ends), so an upgrade the app ends unwinds and starts again what it stopped. Only in the
    main thread, where Python delivers signals."""
    if threading.current_thread() is not threading.main_thread() or not hasattr(signal, "SIGTERM"):
        yield
        return

    def raised(signum: int, frame: object) -> None:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        raise Interrupted("SIGTERM")

    before = signal.signal(signal.SIGTERM, raised)
    try:
        yield
    finally:
        signal.signal(signal.SIGTERM, before)


def ended_by(exc: BaseException) -> str:
    """The 7.8 code an exception ends the upgrade with."""
    if isinstance(exc, Refused):
        return exc.code
    if isinstance(exc, KeyboardInterrupt | Interrupted):
        return "interrupted"
    return "internal"


@contextlib.contextmanager
def guard_daemons(around: Around | None, *, sigterm: bool) -> Iterator[None]:
    """Whatever ends the block, an exception, Ctrl-C or (with ``sigterm``) SIGTERM, what
    ``around`` stopped is started again before it goes on, with both signals held off while
    that runs; the step that was open is said to have failed first. Every caller of
    ``services.lifecycle.upgrade`` with an ``around`` runs it, and the command's end, in
    here."""
    with terminating() if sigterm else contextlib.nullcontext():
        try:
            yield
        except BaseException as exc:
            if around is not None:
                around.events.end_open_step(ended_by(exc))
                with _signals_held_off():
                    around.start_again()
            raise


@dataclass
class Around:
    """The daemons' part of one upgrade, which ``services.lifecycle.upgrade`` calls around
    its installer (:meth:`before_install`, :meth:`installed`), and the command after it
    (:meth:`finish`). It carries the steps' events, and what it asked to stop, so that
    whatever ends the upgrade, that is started again (:func:`guard_daemons`)."""

    found: Census
    options: Options
    events: Events
    rlxd: Rlxd | None
    windows: bool = False
    check_detail: str | None = None
    """What the ``check`` step ends with: the package installed, a wait, the warning."""
    mixed: str | None = None
    """The mixed-releases warning (macOS and Linux, without ``--restart-daemons``)."""
    to_version: str | None = None
    asked: list[daemons_core.Daemon] = field(default_factory=list)
    """Every daemon asked to stop, whether or not it has."""
    stopped: list[daemons_core.Daemon] = field(default_factory=list)
    left: list[str] = field(default_factory=list)
    restarted: list[str] = field(default_factory=list)
    later: list[str] = field(default_factory=list)
    """Daemons still stopping when the upgrade gave up on them: a helper starts them."""
    failure: Refused | None = None
    rlxd_before: str | None = None
    """The version rlxd reported before it was stopped: what a rollback must come back at."""
    rlxd_swapped: bool = False
    rlxd_installed: bool = field(default_factory=lambda: installed_rlxd() is not None)
    new_install: Callable[[], Sequence[str]] | None = None
    """The new install's ``python -P -m aisquare`` (its interpreter), for ``daemon install``."""
    _checked: bool = False
    _ended_ptys: set[Any] = field(default_factory=set)
    _handled: set[str] = field(default_factory=set)
    _start_failed: list[str] = field(default_factory=list)
    _start_said: bool = False
    _start_done: bool = False

    def before_install(self) -> None:
        """The end of ``check``: rlxd's PTYs and runs counted again, now, after any wait and
        the question; one that appeared meanwhile refuses as before, nothing stopped. Then
        ``stop_daemons``: with ``--restart-daemons``, the live PTYs ended (``--end-ptys``)
        and each daemon of this host the upgrade may restart stopped, rlxd last. A stop that
        fails starts again what was asked to stop, and raises: nothing is installed."""
        fresh = self.counted_again()
        refused = refusal(fresh, replace(self.options, wait_runs=None), windows=self.windows)
        if refused is not None:
            self.events.step("check", "failed", refused.code)
            raise refused
        self.found = fresh
        self._checked = True
        self.events.step("check", "done", self.check_detail)
        if not self.options.restart_daemons:
            self.events.step("stop_daemons", "skipped")
            self.events.step("install", "started")
            return
        self.events.step("stop_daemons", "started")
        ended = self._end_ptys(fresh)
        # What refusal() let through: rlx's own records, or ones the upgrade does not stop.
        targets = [daemon for daemon in fresh.here if restart_refusal(daemon) is None]
        # rlxd's brain is its child: it stops with rlxd, and rlxd starts it again.
        self.left = [
            f"{d.kind} (not restartable)"
            for d in fresh.here
            if not d.restartable and d.kind != "brain"
        ]
        targets.sort(key=lambda daemon: daemon.kind == "rlxd")
        for daemon in targets:
            if daemon.kind == "rlxd":
                # The others took their time to stop: a PTY or run that opened meanwhile
                # refuses now, before rlxd's stop would end it without a word (round 2, m2).
                late = refusal(
                    self.counted_again(),
                    replace(self.options, wait_runs=None),
                    windows=self.windows,
                )
                if late is not None:
                    self.events.step("stop_daemons", "failed", late.code)
                    self.start_again()
                    raise late
                ended += self._end_ptys(self.found)
            if daemon.kind == "rlxd" and self.rlxd is not None:
                status = self.rlxd.daemon_status() or {}
                version = status.get("version")
                self.rlxd_before = version if isinstance(version, str) else None
            self.asked.append(daemon)
            try:
                stop_daemon(daemon, self.rlxd)
            except Refused as stuck:
                self.events.step("stop_daemons", "failed", stuck.message)
                self.start_again()
                raise
            self.stopped.append(daemon)
        said = [f"ended {ended} PTY{'' if ended == 1 else 's'}"] if ended else []
        if self.stopped:
            said.append(f"stopped: {', '.join(daemon.kind for daemon in self.stopped)}")
        if self.left:
            said.append(f"left running: {', '.join(self.left)}")
        self.events.step("stop_daemons", "done", "; ".join(said) or None)
        self.events.step("install", "started")

    def counted_again(self) -> Census:
        """The census now, kept as :attr:`found`."""
        self.found = census(self.rlxd)
        return self.found

    def _end_ptys(self, found: Census) -> int:
        """With ``--end-ptys``, ends each live PTY of ``found`` not ended already: how many."""
        if not self.options.end_ptys or self.rlxd is None:
            return 0
        ended = 0
        for pty in found.ptys:
            pty_id = pty.get("pty_id")
            if pty_id in self._ended_ptys:
                continue
            with contextlib.suppress(RlxdError):  # ending rlxd ends it anyway
                self.rlxd.call("pty.kill", {"pty_id": pty_id})
                self._ended_ptys.add(pty_id)
                ended += 1
        return ended

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
            self.rlxd_swapped = True
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
        """A swap that failed: the rlxd in place is kept. Without ``--restart-daemons`` the
        upgrade of rlx stands, and this is a warning; with it, the upgrade fails."""
        self.events.step("rlxd", "failed", f"{code}: {detail}")
        message = (
            f"rlx is upgraded to {self.to_version}, but rlxd is not ({code}): the rlxd in "
            "place is kept"
        )
        # rlx is the new release now, so running the upgrade again finds nothing to do:
        # what installs rlxd is said here (tester #3107).
        advice = prog.shell_advice(f" Install it with: {prog.words('daemon', 'install')}")
        if not self.options.restart_daemons:
            self.events.say_line(f"! {message}.{advice}")
            return
        self.failure = Refused("rlxd_install_failed", f"{message}, and runs again.{advice}", code)

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

    def _expected(self, daemon: daemons_core.Daemon) -> str | None:
        """The version ``daemon`` must report once started again: rlxd's, the new one when
        swapped, else the one it had; ``None`` for a daemon whose version isn't asked."""
        if daemon.kind != "rlxd":
            return None
        return self.to_version if self.rlxd_swapped else self.rlxd_before

    def start_again(self) -> None:
        """``start_daemons``: each daemon asked to stop, rlxd first, from its recorded argv;
        rlxd waited for until it reports the version it must from a new pid. One still
        stopping is left to :func:`restart_when_gone`. Resumes where an interruption left
        it, and ends once."""
        if self._start_done:
            return
        if not self._checked:
            self._start_done = True
            return
        if not self.asked:
            self._start_done = True
            self.events.step("start_daemons", "skipped")
            return
        if not self._start_said:
            self._start_said = True
            self.events.step("start_daemons", "started")
        for daemon in sorted(self.asked, key=lambda d: d.kind != "rlxd"):
            if daemon.id in self._handled:
                continue
            if daemon not in self.stopped and not _gone(daemon):
                self._handled.add(daemon.id)
                try:
                    log = restart_when_gone(daemon)
                except OSError as exc:
                    self._start_failed.append(f"{daemon.kind}: {exc}")
                    continue
                self.later.append(f"{daemon.kind} (log: {log})")
                continue
            before = _live_ids()
            try:
                spawn_daemon(daemon)
            except OSError as exc:
                self._handled.add(daemon.id)
                self._start_failed.append(f"{daemon.kind}: {exc}")
                continue
            self._handled.add(daemon.id)
            problem = _wait_started(
                daemon, before, self.rlxd, self._expected(daemon), START_TIMEOUT_SECONDS
            )
            if problem is not None:
                self._start_failed.append(f"{daemon.kind}: {problem}")
                continue
            self.restarted.append(daemon.kind)
        self._start_done = True
        said = [f"started: {', '.join(self.restarted)}"] if self.restarted else []
        if self.later:
            said.append(f"still stopping, started again once ended: {', '.join(self.later)}")
        if self._start_failed:
            failed = "; ".join(self._start_failed)
            self.events.step("start_daemons", "failed", "; ".join([*said, failed]))
            if self.failure is None:
                self.failure = Refused(
                    "daemon_start_failed",
                    f"not every daemon started again: {failed}. Start them by hand "
                    "(rlxd serve --detach for rlxd).",
                    failed,
                )
            return
        self.events.step("start_daemons", "done", "; ".join(said) or None)


def spawn_daemon(daemon: daemons_core.Daemon) -> None:
    """Starts ``daemon`` again from its recorded argv, in its recorded folder, detached."""
    spawn_detached(daemon.argv, daemon.cwd)


def _wait_started(
    daemon: daemons_core.Daemon,
    before: set[str],
    rlxd: Rlxd | None,
    expected: str | None,
    timeout: float,
) -> str | None:
    """Why ``daemon``, just run again, did not come up, or ``None`` once it has: a live record
    of this host that was not live before the run, of its kind, argv and folder; rlxd's also
    answering ``status`` from that record's pid, at ``expected`` when that is known."""
    host = registry.host_name()
    deadline = time.monotonic() + timeout
    while True:
        for record in daemons_core.list_live(kind=daemon.kind, prune=False):
            if record.id in before or record.host != host or record.argv != daemon.argv:
                continue
            if daemon.cwd is not None and record.cwd != daemon.cwd:
                continue
            if daemon.kind != "rlxd" or rlxd is None:
                return None
            status = rlxd.daemon_status() or {}
            if status.get("pid") != record.pid:
                continue  # not answering yet, or another rlxd answers
            version = status.get("version")
            if expected is not None and version != expected:
                return f"started from {daemon.argv[0]}, it reports {version}, not {expected}"
            return None
        if time.monotonic() >= deadline:
            return f"did not come up in {timeout:.0f} s"
        time.sleep(0.2)


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


def prepare(
    plan: UpgradePlan,
    options: Options,
    events: Events,
    *,
    windows: bool,
    said: Sequence[str | None] = (),
) -> Around:
    """The daemons' part of an upgrade that will run, after every other check and the
    question: counted (failing closed), refused as 7.8's check step says, ``--wait-runs``
    waited, and the :class:`Around` that stops and starts them. ``said`` is what the check
    step's end adds. :class:`Refused` when the upgrade must not go on."""
    rlxd = find_rlxd()
    found = census(rlxd)
    refused = refusal(found, options, windows=windows)
    if refused is not None:
        raise refused
    found, waited = wait_for_runs(found, options, rlxd, events)
    mixed = mixed_releases(found, options)
    detail = "; ".join(part for part in (*said, waited, mixed) if part) or None
    return Around(
        found,
        options,
        events,
        rlxd,
        windows=windows,
        check_detail=detail,
        mixed=mixed,
        new_install=new_install_command(plan),
    )


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


if __name__ == "__main__":
    raise SystemExit(_helper(sys.argv[1:]))
