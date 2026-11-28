"""``rlx tidy [--dry-run]``: free the disk rlx's ended agents and sessions left (#58).

Every rule is ``services.tidy``'s; this prints its plan (``--dry-run``) or carries it out, as a
table a person reads or, with the global ``--json``, as one object the watcher and the doctor
read. A dry run lists every path with its size and the rule that picked it; a real run frees
exactly that list, and says what it did not free and why.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import typer
from rich.table import Table

from aisquare.core import paths
from aisquare.core import program as prog
from aisquare.core.console import stdout_console
from aisquare.core.state import get_state
from aisquare.services import tidy as tidy_service


def size(count: int) -> str:
    """``count`` bytes as a person reads a disk: decimal units, one decimal place past KB."""
    value = float(count)
    for unit in ("B", "KB", "MB", "GB"):
        if value < 1000:
            return f"{value:.0f} {unit}" if unit in ("B", "KB") else f"{value:.1f} {unit}"
        value /= 1000
    return f"{value:.1f} TB"


def _item(item: tidy_service.Item) -> dict[str, Any]:
    return {"rule": item.rule, "path": str(item.path), "bytes": item.bytes}


def _kept(kept: tidy_service.Kept) -> dict[str, Any]:
    return {"rule": kept.rule, "path": str(kept.path), "why": kept.why, "bytes": kept.bytes}


def plan_json(plan: tidy_service.Plan) -> dict[str, Any]:
    """A dry run as JSON: what would go, what stays and why, and the counts per rule."""
    return {
        "dry_run": True,
        "bytes": plan.bytes,
        "paths": [_item(item) for item in plan.items],
        "kept": [_kept(kept) for kept in plan.kept],
        "counts": plan.counts(),
        "branches": [{"repo": str(repo), "branch": branch} for repo, branch in plan.branches],
        "ignored_tags": [str(path) for path in plan.ignored],
    }


def report_json(report: tidy_service.Report) -> dict[str, Any]:
    """A real run as JSON: what went, what did not and why, and the branches reap deleted."""
    return {
        "dry_run": False,
        "freed_bytes": report.bytes,
        "freed": [_item(item) for item in report.freed],
        "not_freed": [{**_item(item), "why": why} for item, why in report.not_freed],
        "kept": [_kept(kept) for kept in report.kept],
        "branches_deleted": [
            {"repo": str(repo), "branch": branch} for repo, branch in report.branches_deleted
        ],
    }


def _paths_table(rows: list[tuple[str, str, str]], header: tuple[str, str, str]) -> Table:
    table = Table(box=None, pad_edge=False, show_edge=False, header_style="bold")
    for column in header:
        table.add_column(column, no_wrap=column != header[-1], overflow="fold")
    for row in rows:
        table.add_row(*row, style=None)
    return table


def _print_reasons(title: str, rows: list[tuple[str, str, str]]) -> None:
    """``rows`` of (rule, path, why) under ``title``, each as two lines: the rule and the path,
    then why, whole. A table at 80 columns dropped the why (tester #3507, F3), the line that says
    what to push; printed soft-wrapped, it is never cut, the terminal wraps it."""
    if not rows:
        return
    console = stdout_console()
    console.print(f"\n{title}", style="bold")
    for rule, where, why in rows:
        console.print(f"  {rule}  {where}", markup=False)
        console.print(f"      {why}", markup=False)


def _print_kept(kept: list[tidy_service.Kept]) -> None:
    rows = [
        (k.rule, paths.shown(k.path) + (f"  ({size(k.bytes)})" if k.bytes else ""), k.why)
        for k in kept
    ]
    _print_reasons("kept:", rows)


def _print_ignored(ignored: list[Path]) -> None:
    """The roots whose CACHEDIR.TAG authorised nothing, apart from ``kept`` (a freed tree is
    never said kept: tester #3577, m1)."""
    if not ignored:
        return
    console = stdout_console()
    console.print(f"\nignored, a CACHEDIR.TAG at a root: {tidy_service.ROOT_TAG}", markup=False)
    for path in ignored:
        console.print(f"  {paths.shown(path)}", markup=False)


def tidy(
    dry_run: bool = typer.Option(
        False, "--dry-run", help="List what would be freed, with sizes and rules; delete nothing."
    ),
) -> None:
    """Free the disk rlx's ended agents and sessions left: their trees, temp files and build
    output. Nothing a live or paused agent uses, no tracked change, and no commit found nowhere
    else is ever removed."""
    plan = tidy_service.plan()
    console = stdout_console()
    if dry_run:
        if get_state().json_output:
            typer.echo(json.dumps(plan_json(plan)))
            return
        count = len(plan.items)
        console.print(
            f"{prog.word()} tidy --dry-run: would free {size(plan.bytes)} in {count} "
            f"{'path' if count == 1 else 'paths'}",
            markup=False,
        )
        if plan.items:
            rows = [(i.rule, size(i.bytes), paths.shown(i.path)) for i in plan.items]
            console.print(_paths_table(rows, ("RULE", "SIZE", "PATH")), markup=False)
        for repo, branch in plan.branches:
            console.print(f"branch {branch} in {paths.shown(repo)} would go", markup=False)
        _print_kept(plan.kept)
        _print_ignored(plan.ignored)
        return
    report = tidy_service.apply(plan)
    if get_state().json_output:
        typer.echo(json.dumps(report_json(report)))
        return
    count = len(report.freed)
    console.print(
        f"{prog.word()} tidy: freed {size(report.bytes)} in {count} "
        f"{'path' if count == 1 else 'paths'}",
        markup=False,
    )
    if report.freed:
        rows = [(i.rule, size(i.bytes), paths.shown(i.path)) for i in report.freed]
        console.print(_paths_table(rows, ("RULE", "SIZE", "PATH")), markup=False)
    _print_reasons(
        "not freed:", [(i.rule, paths.shown(i.path), why) for i, why in report.not_freed]
    )
    for repo, branch in report.branches_deleted:
        console.print(f"deleted branch {branch} in {paths.shown(repo)}", markup=False)
    _print_kept(report.kept)
