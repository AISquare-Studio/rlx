"""``rlx service``: the headless rlx that keeps Remote, the usage reads and the nudge retries
alive with no TUI open (S1, #32). The work is ``services.service_host``; this parses, prints and
honours ``--json``.

``run`` is the process itself, in the foreground: what ``start`` starts in the background, and
what the OS starts at login (``run --at-login``). ``install`` and ``uninstall`` add and take away
that login item (S2, #33: ``services.service_login``). The tray is not here yet.
"""

from __future__ import annotations

import json
import logging
import sys
from datetime import UTC, datetime
from typing import TYPE_CHECKING, Annotated, Any

import typer

from aisquare.cli.common import expected_config_write_errors, fail
from aisquare.core import paths
from aisquare.core.console import stdout_console
from aisquare.core.state import get_state
from aisquare.core.version import PRODUCT_NAME

if TYPE_CHECKING:
    from aisquare.services.service_host import ServiceStatus
    from aisquare.services.service_login import AtLogin

app = typer.Typer(
    help="The rlx service: keeps Remote, the usage reads and the fleet's nudges going with no "
    "rlx window open.",
    no_args_is_help=True,
)

SERVICE = f"{PRODUCT_NAME} service"
"""``services.service_host.SERVICE``, built here too: that module is imported only when a
command runs, never with the CLI (every hook imports the CLI)."""

NOT_RUNNING = 3
"""``rlx service status``'s exit code while no service runs."""


def _say(data: dict[str, Any], line: str) -> None:
    if get_state().json_output:
        typer.echo(json.dumps(data))
    else:
        stdout_console().print(line, markup=False, highlight=False)


@app.command("run")
def service_run(
    at_login: Annotated[
        bool,
        typer.Option(
            "--at-login",
            hidden=True,
            help="Started by the OS at login: a refusal to start (one already runs, say) "
            "exits 0, so the OS does not take it for a crash and start it again.",
        ),
    ] = False,
) -> None:
    """Run the service in this terminal until it is stopped (Ctrl-C, `rlx service stop`)."""
    from aisquare.services import service_host

    service_host.log_to_file()
    try:
        service_host.run_service()
    except service_host.ServiceError as exc:
        if at_login:
            # A service started by hand runs already, or another release's rlx does: no crash,
            # so launchd's KeepAlive and systemd's Restart= leave it (they restart a failure).
            logging.getLogger(__name__).info("the %s at login did not start: %s", SERVICE, exc)
            return
        fail(str(exc), error="service_not_started", detail=str(exc))
    except Exception:
        if at_login:  # what launchd and systemd do not keep: its traceback, in the log
            logging.getLogger(__name__).exception("the %s at login ended", SERVICE)
        raise


@app.command("start")
def service_start() -> None:
    """Start the service in the background, unless it already runs."""
    from aisquare.services import service_host, service_login

    # Only ever refreshed here, never made: a login item removed outside rlx stays removed.
    service_login.refresh_the_item()
    before = service_host.running_service()
    try:
        service = service_host.start_service()
    except service_host.ServiceError as exc:
        fail(str(exc), error="service_not_started", detail=str(exc))
    started = before is None
    line = (
        f"started the {SERVICE} (pid {service.pid})"
        if started
        else f"the {SERVICE} already runs (pid {service.pid})"
    )
    data: dict[str, Any] = {"started": started, "pid": service.pid, "version": service.version}
    if started and service_host.keychain_out_of_reach():
        # Design A.3 of #32: it starts, as asked, but in this login's session for good.
        data["warning"] = service_host.KEYCHAIN_OUT_OF_REACH
        line = f"{line}\nwarning: {service_host.KEYCHAIN_OUT_OF_REACH}"
    _say(data, line)


@app.command("sweep", hidden=True)
def service_sweep() -> None:
    """The usage sweep, in a process of the service's own (``SweepPart``): what ``metrics
    stats`` reads first, as a command nobody counts. ``service`` is no feature, so no daily
    active ping and no shipment goes with it (the ruling on #32, Q2; #1842)."""
    from aisquare.services import usage_stats

    report = usage_stats.maybe_sweep()
    _say(
        {"transcripts": report.transcripts, "sessions_new": report.sessions_new},
        f"swept: {report.sessions_new} new session(s) of {report.transcripts} transcript(s)",
    )


@app.command("tray", hidden=True)
def service_tray() -> None:
    """The menu bar / tray icon in this terminal, until Ctrl-C (#34): the manual check's way
    to see it. The service starts it its own way, ``python -m aisquare.services.tray_process``
    (``TrayPart``), which imports no CLI. Its exit says when there is no tray here
    (``service_tray.NO_TRAY_EXITS``)."""
    from aisquare.core.config import TraySettings, load_config
    from aisquare.services import tray
    from aisquare.services.service_tray import NO_TRAY_EXITS

    try:
        settings = load_config().tray
    except Exception:  # a broken config.toml: the tray in its defaults
        settings = TraySettings()
    code = tray.run_tray(icon=settings.icon, terminal=settings.terminal)
    if code in NO_TRAY_EXITS:  # said, not only logged: the CLI has no log handler
        typer.echo(f"No tray: {NO_TRAY_EXITS[code]}.", err=True)
    raise typer.Exit(code)


@app.command("stop")
def service_stop() -> None:
    """Stop the service: Remote and ngrok wound down, Remote's switch kept for its next start."""
    from aisquare.services import service_host

    try:
        stopped = service_host.stop_service()
    except service_host.ServiceError as exc:
        fail(str(exc), error="service_not_stopped", detail=str(exc))
    _say({"stopped": stopped}, f"stopped the {SERVICE}" if stopped else f"no {SERVICE} runs")


@app.command("restart")
def service_restart() -> None:
    """Stop the service if it runs, and start it again."""
    from aisquare.services import service_host

    try:
        service = service_host.restart_service()
    except service_host.ServiceError as exc:
        fail(str(exc), error="service_not_restarted", detail=str(exc))
    _say(
        {"restarted": True, "pid": service.pid, "version": service.version},
        f"restarted the {SERVICE} (pid {service.pid})",
    )


@app.command("status")
def service_status() -> None:
    """Whether the service runs: its pid, version, uptime, how each part it hosts is, and
    whether it starts at login. Exits 3 when none runs."""
    from aisquare.services import service_host, service_login

    found = service_host.service_status()
    at_login = service_login.at_login_state()
    if get_state().json_output:
        typer.echo(json.dumps({**found.to_json(), "at_login": at_login.to_json()}))
    else:
        for line in [*status_lines(found), at_login_line(at_login)]:
            stdout_console().print(line, markup=False, highlight=False)
    if not found.live:
        raise typer.Exit(NOT_RUNNING)


@app.command("install")
def service_install(
    linger: Annotated[
        bool | None,
        typer.Option(
            "--linger/--no-linger",
            help="Linux: keep it running while you are logged out too (systemd's lingering "
            "for your user, a system-wide setting). Asked at a terminal when not given.",
        ),
    ] = None,
    if_undecided: Annotated[
        bool,
        typer.Option(
            "--if-undecided",
            hidden=True,
            help="The installers' step: add it only while nothing was chosen, and only for the "
            "default home; refresh it otherwise. Never fails.",
        ),
    ] = False,
) -> None:
    """Start the service at each login (on by default): a LaunchAgent on macOS, a systemd user
    unit on Linux and in WSL. Starts it now too, and adds it back after a removal elsewhere."""
    from aisquare.services import service_login

    if if_undecided:
        with expected_config_write_errors():
            said = service_login.register_by_default(by_installer=True)
        _say({"said": said}, said)
        return
    if linger is None:
        linger = _ask_to_linger()
    try:
        with expected_config_write_errors():
            done = service_login.register_at_login(linger=linger)
    except service_login.LoginError as exc:
        fail(str(exc), error="service_not_installed", detail=str(exc))
    line = at_login_line(done.state)
    _say(
        {"at_login": done.state.to_json(), "notes": list(done.notes)},
        "\n".join((line, *(f"note: {note}" for note in done.notes))),
    )


def _ask_to_linger() -> bool:
    """Linux, at a terminal, while lingering is off: ask, defaulting to no (the brief's rule: a
    system-level choice, made only when asked). Anywhere else: no."""
    from aisquare.services import service_login

    try:
        how = service_login.login_how()
    except service_login.LoginError:
        return False
    if how != "systemd" or get_state().json_output or not sys.stdin.isatty():
        return False
    if service_login.lingering() is not False:
        return False
    return typer.confirm(
        f"Keep the {SERVICE} running while you are logged out too? That turns on systemd's "
        "lingering for your user (loginctl enable-linger), which starts your services at boot",
        default=False,
    )


@app.command("refresh", hidden=True)
def service_refresh() -> None:
    """The upgrade's step, run by the install it put in place: this home's login item rewritten
    as this install writes it (its path to rlx, its PATH), when one is there and rlx's. Never
    makes one, and touches nothing while config.toml cannot be read."""
    from aisquare.services import service_login

    refreshed = service_login.refresh_the_item()
    _say(
        {"refreshed": refreshed},
        "the login item was refreshed" if refreshed else "the login item was left as it is",
    )


@app.command("uninstall")
def service_uninstall() -> None:
    """Stop starting the service at login: its login item is taken away, and the service the OS
    started from it stops."""
    from aisquare.services import service_login

    try:
        with expected_config_write_errors():
            state = service_login.unregister_at_login()
    except service_login.NotHere as exc:
        _say({"at_login": None, "said": str(exc)}, str(exc))  # nothing there, nothing recorded
        return
    except service_login.LoginError as exc:
        fail(str(exc), error="service_not_uninstalled", detail=str(exc))
    _say({"at_login": state.to_json()}, f"the {SERVICE} no longer starts at login")


def at_login_line(state: AtLogin) -> str:
    """Whether it starts at login, for a person."""
    if state.on:
        where = f" ({paths.shown(state.item)})" if state.item is not None else ""
        return f"starts at login: yes{where}"
    return f"starts at login: no — {state.why_off}"


def status_lines(found: ServiceStatus, now: datetime | None = None) -> list[str]:
    """``rlx service status``'s lines, for a person."""
    instance = found.instance
    if instance is None:
        return [f"the {SERVICE} is not running"]
    uptime = _duration(found.uptime(now or datetime.now(UTC)) or 0.0)
    lines = [f"the {SERVICE} runs: rlx {instance.version}, pid {instance.pid}, up {uptime}"]
    if found.mismatch is not None:
        lines.append(f"⚠ {found.mismatch}")
    parts = found.to_json(now).get("parts") or {}
    for name, part in parts.items():
        if not isinstance(part, dict):
            continue
        detail = part.get("detail")
        lines.append(f"  {name}: {part.get('state')}" + (f" — {detail}" if detail else ""))
    if not parts:
        lines.append("  (its parts have not said how they are yet)")
    return lines


def _duration(seconds: float) -> str:
    whole = int(seconds)
    days, rest = divmod(whole, 86400)
    hours, rest = divmod(rest, 3600)
    minutes, secs = divmod(rest, 60)
    if days:
        return f"{days}d {hours}h"
    if hours:
        return f"{hours}h {minutes}m"
    if minutes:
        return f"{minutes}m {secs}s"
    return f"{secs}s"
