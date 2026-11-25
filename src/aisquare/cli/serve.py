"""``aisquare serve`` — expose the orchestrator to remote agents over MCP."""

from __future__ import annotations

import contextlib
import json
import os
import signal
from collections.abc import Iterator
from pathlib import Path
from typing import Annotated

import typer

from aisquare.cli.common import fail
from aisquare.core import daemons, selfcli
from aisquare.core import program as prog
from aisquare.core.console import stderr_console, stdout_console
from aisquare.core.state import get_state

_INSTALL_HINT = "pip install 'aisquare-cli[serve]' (or: pipx inject aisquare-cli mcp)"

#: The module ``services.mcp_server.build_server`` actually imports. Probing the
#: DISTRIBUTION instead (``find_spec("mcp")``) is not enough: mcp 1.x ships a
#: package called ``mcp`` that does not contain this module (it had
#: ``mcp.server.fastmcp``, which 2.0.0 renamed to this), so the distribution
#: check passes and the user gets a raw ModuleNotFoundError from deep inside
#: the server instead of the CLI's error contract. A test pins this name
#: against the import in mcp_server.py so the two cannot drift.
REQUIRED_MODULE = "mcp.server.mcpserver"


def _find_spec(name: str) -> object | None:
    """Indirection so the dependency state can be exercised in tests.

    Patching ``importlib.util.find_spec`` itself would sabotage every other
    import for the duration of the test, including pytest's own.
    """
    import importlib.util

    return importlib.util.find_spec(name)


def _dependency_error() -> str | None:
    """Why the MCP server cannot start here, or ``None`` when it can.

    Two failures that need two different fixes: the extra is missing (install
    it), or the installed ``mcp`` is a major without what we import (move it
    into range — today that means a 1.x, which predates the rename). Telling
    someone to install a package they already have is worse than saying
    nothing, so they are reported separately.
    """
    try:
        if _find_spec(REQUIRED_MODULE) is not None:
            return None
    except (ImportError, ValueError) as exc:
        # find_spec on a dotted name imports its parents first, so an absent
        # `mcp` raises here rather than returning None. That IS the
        # extra-not-installed case, and it falls through to the check below.
        # A parent that exists but will not import raises too — mcp present,
        # one of ITS dependencies missing or broken — and that is a third
        # case: neither "install the extra" nor "move mcp into range" fixes
        # it, and the second would send someone to install the mcp they have.
        # The exception names the module that failed; a bare re-raise from a
        # test stub does not, and takes the absent-mcp path as before.
        #
        # Checked ahead of the out-of-range verdict, which an mcp 1.x with a
        # broken transitive dependency would otherwise get. Deliberate: the
        # hint below installs the extra, which pins `mcp>=2.1,<3`, so it fixes
        # the range and the broken import together, while "pin mcp" alone
        # leaves the import broken.
        if getattr(exc, "name", None) not in (None, "mcp"):
            return (
                f"the installed mcp{_installed_mcp()} cannot be imported — {exc}. "
                f"Reinstall the serve extra: {_INSTALL_HINT}"
            )
    if _find_spec("mcp") is None:
        return f"the serve extra is not installed — {_INSTALL_HINT}"
    return (
        f"the installed mcp package has no {REQUIRED_MODULE}{_installed_mcp()} — "
        "rlx needs mcp>=2.1,<3, which is where that module lives. "
        "Move it into range: pip install 'mcp>=2.1,<3'"
    )


def dependency_error() -> str | None:
    """Why an MCP server cannot start here, or ``None`` — for every command that serves one
    (``serve``, ``captain serve``)."""
    return _dependency_error()


def _installed_mcp() -> str:
    """``" (mcp 1.29.1)"`` when the version is knowable, else nothing.

    Today only a 1.x reaches the out-of-range message — every 2.x release
    ships the probed module — but a 3.x would, and a 2.x that will not import
    reaches the reinstall message, which this decorates too.
    """
    from importlib.metadata import PackageNotFoundError, version

    try:
        return f" (mcp {version('mcp')})"
    except PackageNotFoundError:  # importable but not an installed distribution
        return ""


_WILDCARD_BINDS = frozenset({"0.0.0.0", "::", ""})


def _client_url(bind: str, port: int) -> str:
    """A URL for ``bind``, since a listen address is not always addressable.

    An IPv6 literal needs brackets: ``http://::1:8747/mcp`` is not a URL at
    all, and ``::1`` is one of the spellings that keeps the transport's Host
    validation, so it has to print. That half is unambiguous.

    A wildcard bind names every interface and no destination, so this
    machine's name stands in for it. Whether that name resolves, and to
    something a client can reach, is the operator's network to know: it may be
    absent from DNS, or map straight back to loopback. It is a better starting
    point than ``0.0.0.0``, which is never dialable, and the caller prints the
    bind alongside so nothing is hidden.
    """
    import socket

    host = socket.gethostname() if bind in _WILDCARD_BINDS else bind
    if ":" in host:
        host = f"[{host}]"
    return f"http://{host}:{port}/mcp"


def _announce_open_bind(bind: str) -> None:
    """Say what a non-loopback bind gives up, or say nothing.

    On stderr, so ``--json`` stdout stays machine-readable, and from every
    path that hands this bind to the transport — serving it, and
    ``--show-token``, which is the command an operator reads while wiring a
    client up and is often the only one they read at all.

    Keyed on ``LOOPBACK_BINDS``, which mirrors the literal the SDK matches on;
    ``tests/test_serve.py`` drives all three of its spellings against the real
    transport, so a mirror that stopped matching fails there rather than
    turning this notice into a lie.
    """
    from aisquare.services import mcp_server

    if bind in mcp_server.LOOPBACK_BINDS:
        return
    shown = bind or '""'
    stderr_console().print(
        f"--bind {shown} is not one of {', '.join(mcp_server.LOOPBACK_BINDS)}: the MCP "
        "transport's Host/Origin validation is off for it, and the bearer token is the "
        "only gate — a long-lived credential sent in clear over plain HTTP on every "
        "request. Keep this on a trusted network, or behind a TLS-terminating proxy."
    )


def _registered(
    args: list[str], *, restartable: bool
) -> contextlib.AbstractContextManager[daemons.Registration | None]:
    """This server in the daemon registry (``core.daemons``) while it serves, as a ``serve``
    that ``serve <args>`` starts again: its options written out, an environment's
    ``AISQUARE_SERVE_PORT`` or ``AISQUARE_SERVE_CLOSE_AFTER`` included, so the command means
    what it meant. Unregistered it still serves, and says so on stderr (stdout is stdio's
    protocol channel)."""

    def unregistered(exc: OSError) -> None:
        stderr_console().print(
            f"{prog.word()} serve: not in the daemon registry, so an upgrade will not count "
            f"this server: {exc}",
            markup=False,
        )

    return daemons.registered(
        "serve",
        argv=selfcli.argv_for([*selfcli.global_flags(), "serve", *args]),
        restartable=restartable,
        on_error=unregistered,
    )


@contextlib.contextmanager
def _terminated_as_an_exit() -> Iterator[None]:
    """SIGTERM as ``SystemExit(143)`` for the block, so a stopped HTTP server unwinds through
    its registration and leaves the registry, with the exit status a SIGTERM gives.

    uvicorn takes SIGTERM while it serves, shuts down gracefully, gives back the handler it
    found and raises the signal again: to the default handler, which ended the process before
    any ``finally``, and left its record for the next reader to prune. Not for stdio, which
    ends by ``os._exit`` on purpose (``mcp_server._serve_stdio_until_idle``)."""

    def terminated(signum: int, frame: object) -> None:
        raise SystemExit(128 + signum)

    try:
        before = signal.signal(signal.SIGTERM, terminated)
    except ValueError:  # not the main thread: nothing to take
        yield
        return
    try:
        yield
    finally:
        signal.signal(signal.SIGTERM, before)


def serve(
    stdio: Annotated[
        bool,
        typer.Option("--stdio", help="Serve over stdio (for Claude Desktop / wsl.exe launch)."),
    ] = False,
    port: Annotated[
        int, typer.Option("--port", help="HTTP port.", envvar="AISQUARE_SERVE_PORT")
    ] = 8747,
    bind: Annotated[
        str,
        typer.Option(
            "--bind",
            help="HTTP bind address. 127.0.0.1, localhost or ::1 keep the transport's "
            "Host/Origin validation; any other bind runs with the bearer token as the only "
            "gate, sent in clear — trusted networks or a TLS proxy only.",
        ),
    ] = "127.0.0.1",
    show_token: Annotated[
        bool,
        typer.Option("--show-token", help="Print the HTTP connection details and exit."),
    ] = False,
    close_after: Annotated[
        int,
        typer.Option(
            "--close-after",
            min=0,
            envvar="AISQUARE_SERVE_CLOSE_AFTER",
            help="stdio only: exit after this many seconds without a client message or a "
            "running tool call "
            "(default 300; 0 = run forever — set it for persistent clients like "
            "Claude Desktop). HTTP mode ignores it.",
        ),
    ] = 300,
) -> None:
    """Run the orchestrator MCP server so remote Claude clients can join this project."""
    # mcp_server itself imports lazily, so probe the dependency directly —
    # a bare `import` succeeds without the extra and dies later mid-request.
    problem = _dependency_error()
    if problem is not None:
        fail(problem, error="serve_not_installed")
    from aisquare.services import mcp_server

    if show_token:
        token = mcp_server.serve_token()
        url = _client_url(bind, port)
        if get_state().json_output:
            typer.echo(json.dumps({"url": url, "token": token, "bind": bind}))
        else:
            console = stdout_console()
            console.print(f"URL:    {url}")
            if bind in _WILDCARD_BINDS:
                console.print(f"Bind:   {bind} (every interface — the URL names this machine)")
            console.print(f"Header: Authorization: Bearer {token}")
        _announce_open_bind(bind)
        return
    # Starting a server here IS the opt-in for this project: activate it
    # explicitly (and visibly — `team on` semantics, with the pipe event),
    # so remote calls never activate a directory as a side effect.
    from aisquare.cli.team import STORE_ERRORS, _fail_team
    from aisquare.core.orchestrator import team_project
    from aisquare.core.workspace import find_project_root, marks_project_root
    from aisquare.services import team as team_service

    if stdio:
        # Claude Desktop launches stdio servers from wherever it likes
        # ($HOME by default) and find_project_root falls back to the cwd —
        # refuse to silently activate a directory that is not a project.
        target = team_project(None)
        marker_root = find_project_root(Path.cwd())
        if not os.environ.get("AISQUARE_TEAM_HUB", "").strip() and not marks_project_root(
            marker_root
        ):
            fail(
                f"refusing to activate {target.root}: not a project root (no .git/.hg/"
                ".aisquare marker). Launch from the repo, or set AISQUARE_TEAM_HUB.",
                error="not_a_project",
            )
        try:
            project = team_service.activate()
        except STORE_ERRORS as exc:
            # team_disabled, but also a wedged store or a failed activation
            # read-back — serve must exit with the error contract, never a
            # traceback (#20 review).
            _fail_team(exc)
        # stdout is the MCP protocol channel; announce on stderr so the
        # opt-in is never invisible.
        stderr_console().print(f"rlx agent orchestrator activated for {project.root} (stdio serve)")
        # Not restartable: its client holds its stdin and stdout, and starts it again itself.
        # Its idle deadline ends it with os._exit, so it leaves the registry just before.
        with _registered(["--stdio", "--close-after", str(close_after)], restartable=False) as mine:
            mcp_server.run_stdio(
                close_after=close_after,
                before_exit=None if mine is None else mine.deregister,
            )
        return
    try:
        project = team_service.activate()
    except STORE_ERRORS as exc:
        _fail_team(exc)
    stderr_console().print(
        f"Serving the orchestrator for {project.root.name or project.id} at "
        f"{_client_url(bind, port)} "
        f"(bearer token required — see `{prog.word()} serve --show-token`). Ctrl-C stops."
    )
    _announce_open_bind(bind)
    with (
        _registered(["--port", str(port), "--bind", bind], restartable=True),
        _terminated_as_an_exit(),
    ):
        mcp_server.run_http(bind=bind, port=port)
