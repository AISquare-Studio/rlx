"""``rlx daemon``: rlxd, the engine, on this machine (handoff 0.10.3, 1.2).

``install`` puts the rlxd and rlxd-hook of this rlx's own release in uv's tool bin
directory, beside ``rlx``: for an rlx installed with plain ``uv tool install``, which
brings no rlxd. The installers and ``rlx upgrade`` do the same themselves.
``services.rlxd_install`` does the work.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Annotated

import typer

from aisquare.cli.common import fail
from aisquare.core.console import stdout_console
from aisquare.core.state import get_state

app = typer.Typer(help="rlxd, the rlx engine, on this machine.", no_args_is_help=True)


@app.command("install")
def install(
    bin_dir: Annotated[
        Path | None,
        typer.Option(
            "--bin-dir",
            help="Install here instead of uv's tool bin directory (`uv tool dir --bin`).",
        ),
    ] = None,
) -> None:
    """Download the rlxd of this rlx's version, check it against rlxd-SHA256SUMS, and
    install rlxd and rlxd-hook beside rlx."""
    from aisquare.services import rlxd_install

    try:
        done = rlxd_install.install(bin_dir=bin_dir)
    except rlxd_install.InstallError as exc:
        fail(exc.message, error=exc.code, detail=exc.detail)
    if get_state().json_output:
        typer.echo(json.dumps(done.to_json()))
        return
    stdout_console().print(
        f"✓ rlxd {done.version} installed in {done.bin_dir} "
        f"({', '.join(path.name for path in done.binaries)}; "
        f"{done.archive} matched rlxd-SHA256SUMS)",
        markup=False,
        highlight=False,
    )
