"""``rlx home migrate``: move a home from before 0.10.2 to ``~/.rlx``, in the new layout (#31).

Hidden: ``rlx upgrade`` (and the app's Update, which runs it) is the way a person moves the home,
and runs this in the new install. The work is ``services.home_migration``; this module parses,
prints and honours ``--json``.
"""

from __future__ import annotations

import json
from typing import Annotated, Any

import typer

from aisquare.cli.common import fail
from aisquare.core import paths
from aisquare.core.console import stdout_console
from aisquare.core.state import get_state
from aisquare.services import home_migration

app = typer.Typer(help="rlx's home.", no_args_is_help=True)


def _echo_home_json(payload: dict[str, Any]) -> None:
    typer.echo(json.dumps(payload, separators=(",", ":")))


def _say_home(line: str) -> None:
    stdout_console().print(line, markup=False, highlight=False)


@app.command("migrate")
def migrate_home(
    dry_run: Annotated[
        bool, typer.Option("--dry-run", help="Say what would move, and move nothing.")
    ] = False,
    rollback: Annotated[
        bool,
        typer.Option("--rollback", help="Put the home back as it was before an unfinished move."),
    ] = False,
    drop_cache: Annotated[
        bool,
        typer.Option("--drop-cache", help="Drop the regenerable cache once the home has moved."),
    ] = False,
) -> None:
    """Move a home from before 0.10.2 (~/.aisquare) to ~/.rlx in the new layout.

    Only when nothing runs on it: no rlx, no fleet agent, no live pane on rlx's tmux server,
    no watcher, no Remote served elsewhere. A crash at any step is resumed by the next run, or
    rolled back with --rollback, until the old Keychain logins are deleted.
    """
    json_output = get_state().json_output
    if rollback:
        try:
            undone = home_migration.roll_back_move()
        except home_migration.MigrationFailed as exc:
            fail(exc.reason, error="home_move_forward_only")
        if json_output:
            _echo_home_json({"rolled_back": undone})
        else:
            _say_home("the home is as it was before the move" if undone else "no move to roll back")
        return
    try:
        found = home_migration.planned_move()
    except home_migration.MigrationRefused as exc:
        fail(f"the home was not moved: {exc.reason}", error="home_move_refused")
    if found is None:
        if json_output:
            _echo_home_json({"moved": False, "home": str(paths.aisquare_home())})
        else:
            _say_home(
                f"the home is laid out as 0.10.2 lays it out: {paths.shown(paths.aisquare_home())}"
            )
        return
    if dry_run:
        running = home_migration.running_on_home()
        if json_output:
            _echo_home_json(
                {
                    "dry_run": True,
                    "old_home": str(found.old_root),
                    "home": str(found.new_root),
                    "in_place": found.named,
                    "resumes": found.journal is not None,
                    "running": running,
                }
            )
            return
        _say_home(f"the home would move: {found.shown}")
        for line in running:
            _say_home(f"  but first: {line}")
        return
    try:
        moved = home_migration.move_home(
            drop_cache=drop_cache, say=_say_home if not json_output else _says_nothing
        )
    except home_migration.MigrationRefused as exc:
        named = "; ".join(exc.running)
        fail(
            f"the home was not moved: {exc.reason}" + (f": {named}" if named else ""),
            error="home_move_refused",
            detail=named or None,
        )
    except home_migration.MigrationFailed as exc:
        then = (
            "it was rolled back, and the home is as it was"
            if exc.rolled_back
            else "run it again to finish it"
        )
        fail(f"the home's move stopped: {exc.reason} ({then})", error="home_move_failed")
    if moved is None:
        if json_output:
            _echo_home_json({"moved": False, "home": str(paths.aisquare_home())})
        return
    if json_output:
        _echo_home_json(moved.as_json())
        return
    for line in home_migration.said_of_move(moved):
        _say_home(line)


def _says_nothing(_line: str) -> None:
    """``--json`` prints one object and nothing beside it."""
