"""``rlx brain --stdio``: the resident Python brain rlxd starts and supervises (handoff
0.10.3, 1.6). Hidden: rlxd runs it, never a person. ``services.brain`` is the protocol."""

from __future__ import annotations

from typing import Annotated

import typer

app = typer.Typer(help="The resident brain rlxd runs (internal).", hidden=True)


@app.callback(invoke_without_command=True)
def brain(
    stdio: Annotated[
        bool, typer.Option("--stdio", help="Speak the brain protocol on stdin and stdout.")
    ] = False,
) -> None:
    """Answer rlxd's requests, as JSON lines on stdin and stdout, until stdin closes."""
    if not stdio:
        typer.echo("This command is rlxd's: rlxd starts it, with --stdio.", err=True)
        raise typer.Exit(2)
    from aisquare.services import brain as brain_service

    raise typer.Exit(brain_service.serve_stdio())
