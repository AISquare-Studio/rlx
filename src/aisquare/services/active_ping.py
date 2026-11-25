"""The anonymous daily active ping: one small POST on each local day that rlx is used, so the
people who build it can count how many installs are active, those not sharing usage included.

It is **separate from usage sharing** (``services.usage_share``). It is sent whether sharing is
on or off, and it has its own switch, ``[sharing] active_ping`` (on unless turned off; Settings ▸
Usage & stats). ``DO_NOT_TRACK``, ``AISQUARE_NO_TELEMETRY`` and ``CI`` stop it, as they stop
sharing (``core.sharing.forced_off_reason``).

**What it sends** is the endpoint's whole contract (AISquare-Studio-BE's
``docs/handoffs/rlx-active-installs.md``): ``POST {api_url}/api/v1/rlx/active/`` with
``{"date", "version", "os", "arch", "install_method"}``, each from a closed set or a pattern
(:func:`body`), and nothing else. It sends no install id or other identifier, no account and no
bearer (:data:`Transport` takes no token), and nothing about repositories or usage. The server
keeps a counter per day and those four traits.

**Once a day.** A ping settles its local day when the server answers 2xx (delivered), 404 (the
endpoint is not deployed yet: silent, and tried again on a later day) or another 4xx (refused:
not sent again that day). A throttle (429), a 503 and every other failure wait, for the longer
of the backoff (5 minutes, doubling) and the server's ``Retry-After`` on a 429 or 503 (at most a
day), and are tried again while the local day lasts. The body always carries the day it is sent
on, so a missed day is never sent later.

**Never on a hot path.** Every command that is a feature, once it is done, and a bare ``rlx``
opening the app, call :func:`start_background`: an environment look, the config's switch and
one read of ``state.json``. Once a day it also starts one detached
``aisquare metrics ping`` (:func:`send`), so the network is never touched by the process the
person typed, and nothing is ever printed. The hooks never get here: they are no feature
(``cli.app._NOT_FEATURES``), and this module is imported only by what runs it. Every function
here fails open, except that a switch that cannot be read reads as off.
"""

from __future__ import annotations

import platform
import re
import sys
from collections.abc import Callable
from dataclasses import dataclass
from datetime import UTC, date, datetime, timedelta, tzinfo
from pathlib import Path
from typing import TYPE_CHECKING, Literal

from pydantic import BaseModel

from aisquare.core import paths, sharing, state_file

if TYPE_CHECKING:
    from aisquare.services.iam import HttpResult

ENDPOINT_PATH = "/api/v1/rlx/active/"
STATE_KEY = "active_ping"
TIMEOUT_S = 5.0
SPAWN_EVERY = timedelta(minutes=30)
"""Between two detached pingers started by commands while one may still be running: a pinger
clears its mark when it finishes, so this only holds back the commands that run meanwhile."""
RETRY_AFTER_CAP = timedelta(hours=24)
"""The longest ``Retry-After`` a ping waits for."""

OSES = frozenset({"darwin", "linux", "windows"})
"""The OS names sent as they are; any other is ``other``."""
ARCHES = {"x86_64": "x86_64", "amd64": "x86_64", "arm64": "arm64", "aarch64": "arm64"}
"""``platform.machine()`` (lower case) to the architecture sent; any other is ``other``."""
INSTALL_METHODS = frozenset({"brew", "pipx", "uv", "script", "pip", "unknown"})
_VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+[0-9a-z.+]{0,20}$")
MAX_VERSION = 32

Outcome = Literal["sent", "absent", "refused"]
"""How a day was settled: delivered (2xx), the endpoint not there yet (404), refused (4xx)."""

Transport = Callable[[str, dict[str, str], float], "HttpResult"]
"""``(url, body, timeout) -> result``: how the ping reaches the server. It has no place for a
token: the ping is anonymous by construction."""


def default_transport(url: str, body: dict[str, str], timeout: float) -> HttpResult:
    from aisquare.services import iam  # lazy: only the detached pinger sends

    return iam.post_json(url, body, token=None, timeout=timeout)


class PingState(BaseModel):
    """What ``state.json`` keeps of the ping (:data:`STATE_KEY`): when, never what or who."""

    day: str | None = None
    """The local day the ping is settled for: nothing more is sent on it."""
    outcome: Outcome | None = None
    not_before: str | None = None
    """After a failure: no ping before this moment."""
    failures: int = 0
    """Failed attempts in a row (the backoff's exponent); a settled day clears it."""
    spawned_at: str | None = None
    """When a command last started the detached pinger; the pinger clears it when it is done."""
    last_error: str | None = None


@dataclass(frozen=True)
class PingStatus:
    """Where the ping stands: what the Settings switch shows."""

    switch: bool
    forced_off: str | None
    last_sent: str | None
    """The local day of the last delivered ping, if any."""

    @property
    def enabled(self) -> bool:
        return self.switch and self.forced_off is None


@dataclass(frozen=True)
class PingResult:
    """What one run of the detached pinger did (``aisquare metrics ping --json``)."""

    sent: bool = False
    status: int | None = None
    outcome: Outcome | None = None
    retry_at: str | None = None
    skipped: str | None = None
    error: str | None = None


# --- the clock (a seam for the tests) ----------


def _now() -> datetime:
    return datetime.now(UTC)


def _tz() -> tzinfo | None:
    """The zone of the local day: ``None`` is the machine's own."""
    return None


def local_day(moment: datetime) -> date:
    return moment.astimezone(_tz()).date()


# --- the switch ----------


def switch_on() -> bool:
    """``[sharing] active_ping``; ``False`` for a config that cannot be read (fail closed: a
    switch someone may have turned off and that cannot be read sends nothing)."""
    try:
        from aisquare.core.config import load_config  # lazy, as core.sharing does

        return load_config(copy=False).sharing.active_ping
    except Exception:
        return False


def enabled() -> bool:
    """Whether the ping is sent: nothing forces it off and its switch is on."""
    return sharing.forced_off_reason() is None and switch_on()


def status() -> PingStatus:
    state = read()
    return PingStatus(
        switch=switch_on(),
        forced_off=sharing.forced_off_reason(),
        last_sent=state.day if state.outcome == "sent" else None,
    )


def set_enabled(on: bool) -> PingStatus:
    """Turn the ping on or off (``[sharing] active_ping`` in ``config.toml``)."""
    from aisquare.services import settings as settings_service

    settings_service.set_value("sharing.active_ping", "true" if on else "false")
    return status()


# --- the state ----------


def read() -> PingState:
    """The ping's state; a fresh one for a file with none or one that cannot be read."""
    try:
        raw = state_file.read_state().get(STATE_KEY)
        return PingState.model_validate(raw) if isinstance(raw, dict) else PingState()
    except Exception:
        return PingState()


def update(change: Callable[[PingState], None]) -> PingState:
    """Apply ``change(state)`` under the state file's lock and write it back. Raises what the
    state file raises."""
    holder: dict[str, PingState] = {}

    def apply(old: object | None) -> object | None:
        current = PingState.model_validate(old) if isinstance(old, dict) else PingState()
        change(current)
        holder["state"] = current
        return current.model_dump(mode="json", exclude_none=True)

    state_file.modify_state(STATE_KEY, apply)
    return holder["state"]


def due(state: PingState, now: datetime) -> bool:
    """Whether a ping is due: today is not settled, and no failure's wait is still running."""
    if state.day == local_day(now).isoformat():
        return False
    wait = sharing.parse_time(state.not_before)
    return wait is None or now >= wait


def lock_path() -> Path:
    return paths.aisquare_home() / "active-ping.lock"


# --- the body ----------


def _version() -> str:
    from aisquare.core.version import __version__

    text = __version__.strip().lower()
    return text if len(text) <= MAX_VERSION and _VERSION.fullmatch(text) else "unknown"


def _os() -> str:
    name = platform.system().lower()
    return name if name in OSES else "other"


def _arch() -> str:
    return ARCHES.get(platform.machine().lower(), "other")


def _install_method() -> str:
    try:
        from aisquare.services.usage_share import install_method

        method = install_method()
    except Exception:
        return "unknown"
    return method if method in INSTALL_METHODS else "unknown"


def body(now: datetime | None = None) -> dict[str, str]:
    """The ping: these five keys, and nothing else."""
    moment = now or _now()
    return {
        "date": local_day(moment).isoformat(),
        "version": _version(),
        "os": _os(),
        "arch": _arch(),
        "install_method": _install_method(),
    }


def endpoint() -> str:
    from aisquare.services import iam

    return f"{iam.resolve_api_url()}{ENDPOINT_PATH}"


# --- starting it ----------


def start_background(now: datetime | None = None) -> bool:
    """Start the detached pinger if the ping is on and today's is due. Called by every command
    that is a feature and by a bare ``rlx`` opening the app; costs an environment look, the
    config's switch and one small file read. Never raises; ``True`` when one was started."""
    try:
        if not enabled():
            return False
        moment = now or _now()
        state = read()
        if not due(state, moment):
            return False
        spawned = sharing.parse_time(state.spawned_at)
        if spawned is not None and timedelta(0) <= moment - spawned < SPAWN_EVERY:
            return False
        update(lambda s: setattr(s, "spawned_at", moment.isoformat(timespec="seconds")))
        _spawn()
        return True
    except Exception:
        return False


def _spawn() -> None:
    """The detached pinger: its own session, no terminal, without any agent's tracing identity."""
    import subprocess  # lazy: only a command that finds the ping due gets here

    from aisquare.core import selfcli, spawn

    subprocess.Popen(
        selfcli.argv_for(["metrics", "ping"]),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=sys.platform != "win32",
        env=spawn.untraced_env(),
    )


# --- sending it ----------


def send(*, transport: Transport | None = None) -> PingResult:
    """Send today's ping if it is on and due: what the detached pinger runs. One request with a
    :data:`TIMEOUT_S` timeout; the answer settles the day or sets the next try. Never raises."""
    try:
        if not enabled():
            _clear_spawned()
            return PingResult(skipped="the ping is off")
        from aisquare.core import locking

        lock = lock_path()
        lock.parent.mkdir(parents=True, exist_ok=True)
        try:
            with locking.holding(lock, wait_s=0):
                return _send(transport or default_transport)
        except TimeoutError:
            return PingResult(skipped="another ping is being sent")
    except Exception as exc:  # a state file that cannot be written: nothing to say, nothing lost
        return PingResult(skipped=f"{type(exc).__name__}: {exc}"[:200])


def _send(transport: Transport) -> PingResult:
    from aisquare.services import iam

    moment = _now()
    if not due(read(), moment):
        _clear_spawned()
        return PingResult(skipped="not due")
    ping = body(moment)
    try:
        url = endpoint()
    except iam.IamError as exc:  # an api_url that is not https: no try today will do better
        return _settle(ping["date"], "refused", error=exc.message)
    try:
        result = transport(url, ping, TIMEOUT_S)
    except iam.IamError as exc:  # unreachable, a timeout
        return _failed(moment, error=exc.message)
    except Exception as exc:
        return _failed(moment, error=f"{type(exc).__name__}: {exc}")
    code = result.status
    if 200 <= code < 300:
        return _settle(ping["date"], "sent", status=code)
    if code == 404:
        return _settle(ping["date"], "absent", status=code)
    if 400 <= code < 500 and code not in (408, 425, 429):
        return _settle(ping["date"], "refused", status=code)
    said = result.said_retry_after() if code in (429, 503) else None
    return _failed(moment, status=code, retry_after=said, error=f"HTTP {code}")


def _settle(
    day: str, outcome: Outcome, *, status: int | None = None, error: str | None = None
) -> PingResult:
    def record(state: PingState) -> None:
        state.day = day
        state.outcome = outcome
        state.not_before = None
        state.failures = 0
        state.spawned_at = None
        state.last_error = error[:200] if error else None

    update(record)
    return PingResult(sent=outcome == "sent", status=status, outcome=outcome, error=error)


def _failed(
    moment: datetime,
    *,
    status: int | None = None,
    retry_after: int | None = None,
    error: str | None = None,
) -> PingResult:
    holder: dict[str, str] = {}

    def record(state: PingState) -> None:
        state.failures += 1
        wait = sharing.backoff(state.failures)
        if retry_after is not None:
            wait = max(wait, min(timedelta(seconds=retry_after), RETRY_AFTER_CAP))
        holder["at"] = (moment + wait).isoformat(timespec="seconds")
        state.not_before = holder["at"]
        state.spawned_at = None
        state.last_error = error[:200] if error else None

    update(record)
    return PingResult(status=status, retry_at=holder["at"], error=error)


def _clear_spawned() -> None:
    try:
        if read().spawned_at is not None:
            update(lambda s: setattr(s, "spawned_at", None))
    except Exception:
        return


__all__ = [
    "ENDPOINT_PATH",
    "PingResult",
    "PingState",
    "PingStatus",
    "body",
    "due",
    "enabled",
    "send",
    "set_enabled",
    "start_background",
    "status",
    "switch_on",
]
