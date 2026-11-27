"""``rlx service``: the headless rlx that keeps Remote, the usage reads and the nudge retries
alive with no TUI open (S1, #32). The work is ``services.service_host``; this parses, prints and
honours ``--json``.

``run`` is the process itself, in the foreground: what ``start`` starts in the background, and
what the OS will start at login (S2). Starting it at login, and the tray, are not here yet.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime
from typing import TYPE_CHECKING, Any

import typer

from aisquare.cli.common import fail
from aisquare.core.console import stdout_console
from aisquare.core.state import get_state
from aisquare.core.version import PRODUCT_NAME

if TYPE_CHECKING:
    from aisquare.services.service_host import ServiceStatus

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
def service_run() -> None:
    """Run the service in this terminal until it is stopped (Ctrl-C, `rlx service stop`)."""
    from aisquare.services import service_host

    service_host.log_to_file()
    try:
        service_host.run_service()
    except service_host.ServiceError as exc:
        fail(str(exc), error="service_not_started", detail=str(exc))


@app.command("start")
def service_start() -> None:
    """Start the service in the background, unless it already runs."""
    from aisquare.services import service_host

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
    """Whether the service runs: its pid, version, uptime, and how each part it hosts is.
    Exits 3 when none runs."""
    from aisquare.services import service_host

    found = service_host.service_status()
    if get_state().json_output:
        typer.echo(json.dumps(found.to_json()))
    else:
        for line in status_lines(found):
            stdout_console().print(line, markup=False, highlight=False)
    if not found.live:
        raise typer.Exit(NOT_RUNNING)


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
