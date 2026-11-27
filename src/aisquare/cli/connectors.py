"""``rlx connectors``: the user's Claude Code connectors, as rlxd lists them (handoff
0.10.3, 2.4 and 7.7).

``list`` asks this machine's rlxd (``rlxd call connectors.list``): the connectors of the
latest run's ``system/init``, each with its status and tools. Under ``--json`` it prints
rlxd's ``data`` as it is, the bare payload. ``add``, ``remove`` and ``status`` are the
roadmap's, still stubs, and hidden.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Annotated, Any

import typer

from aisquare.cli.common import fail
from aisquare.core.console import stdout_console
from aisquare.core.program import words as program_words
from aisquare.core.state import get_state
from aisquare.services import connectors as connectors_service

app = typer.Typer(help="Your Claude Code connectors, as rlxd lists them.", no_args_is_help=True)

ConnectorName = Annotated[str, typer.Argument(help="Connector name, e.g. 'notion'.")]

#: Names the rlxd to ask, by absolute path, over every other place.
RLXD_BIN_ENV = "RLX_RLXD_BIN"

#: How long ``rlxd call`` may take.
CALL_TIMEOUT_S = 30


def rlxd_program() -> Path | None:
    """The rlxd to ask: ``RLX_RLXD_BIN``; else the one beside this rlx; else in uv's tool
    bin directory, where ``rlx daemon install`` puts it; else one on ``PATH``, by its
    absolute path. ``None`` when there is none."""
    name = "rlxd.exe" if sys.platform == "win32" else "rlxd"
    named = os.environ.get(RLXD_BIN_ENV, "")
    if named:
        return Path(named) if os.path.isabs(named) else None
    from aisquare.services.rlxd_install import uv_tool_bin_dir

    for folder in (Path(sys.argv[0]).resolve().parent, uv_tool_bin_dir()):
        candidate = folder / name
        if candidate.is_file():
            return candidate
    found = shutil.which("rlxd")
    return Path(found) if found and os.path.isabs(found) else None


def run_rlxd(argv: list[str]) -> subprocess.CompletedProcess[str]:
    """Runs rlxd's ``argv``, its output captured as text."""
    return subprocess.run(argv, capture_output=True, text=True, timeout=CALL_TIMEOUT_S, check=False)


def call_rlxd(method: str) -> dict[str, Any]:
    """``rlxd call <method>``'s reply envelope. Exits, saying why, when there is no rlxd,
    it is not running, or its answer can't be read."""
    program = rlxd_program()
    if program is None:
        fail(
            f"rlxd was not found: run `{program_words('daemon', 'install')}`, or name it in "
            f"{RLXD_BIN_ENV}",
            error="rlxd_not_found",
        )
    try:
        done = run_rlxd([str(program), "call", method])
    except (OSError, subprocess.TimeoutExpired) as exc:
        fail(f"rlxd ({program}) did not answer {method}: {exc}", error="rlxd_failed")
    try:
        reply = json.loads(done.stdout)
    except json.JSONDecodeError:
        said = (done.stderr or done.stdout).strip()
        fail(
            f"rlxd did not answer {method}: {said or f'it exited {done.returncode}'}",
            error="rlxd_not_running" if done.returncode == 3 else "rlxd_failed",
            detail=said or None,
        )
    if not isinstance(reply, dict):
        fail(f"rlxd's answer to {method} is not an envelope", error="rlxd_failed")
    return reply


@app.command("list")
def list_() -> None:
    """List your Claude Code connectors: each one's status and tools, from the latest run."""
    reply = call_rlxd("connectors.list")
    if not reply.get("ok"):
        error = reply.get("error") or {}
        fail(
            str(error.get("message") or "rlxd refused connectors.list"),
            error=str(error.get("code") or "rlxd_failed"),
            hint=error.get("hint"),
        )
    data = reply.get("data") or {}
    if get_state().json_output:
        typer.echo(json.dumps(data))
        return
    console = stdout_console()
    connectors = data.get("connectors") or []
    if not connectors:
        console.print("No connectors yet: rlxd lists those of its latest run.", markup=False)
        return
    for connector in connectors:
        tools = connector.get("tools") or []
        console.print(
            f"{connector.get('id')}  {connector.get('status')}  "
            f"{len(tools)} tool{'' if len(tools) == 1 else 's'}",
            markup=False,
        )
    console.print(f"as of {data.get('as_of')}", markup=False)


@app.command("add", hidden=True)
def add(name: ConnectorName) -> None:
    """Add and configure a connector."""
    connectors_service.add(name)


@app.command("remove", hidden=True)
def remove(name: ConnectorName) -> None:
    """Remove a configured connector."""
    connectors_service.remove(name)


@app.command("status", hidden=True)
def status() -> None:
    """Show connector health and last sync times."""
    connectors_service.status()
