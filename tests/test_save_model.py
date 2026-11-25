"""One save model for the settings pages (UX audit M5): ``cli.ui.save_model`` on its own.

Each page's own Pilot test (change one control, the disk changes with no Save; ctrl+z puts
the old value back) lives beside that page's other tests: tests/test_ui_project.py (a
project's nickname), tests/test_ui_usage_share.py (Usage & stats' two switches) and
tests/test_ui_accounts.py (an account's name).
"""

from __future__ import annotations

import asyncio
from typing import Any, ClassVar

from textual.app import App, ComposeResult
from textual.containers import Vertical
from textual.widgets import Input

from aisquare.cli.ui.save_model import UNDO_BINDING, CommitInput, SavesAsYouGo


class Page(SavesAsYouGo, Vertical):
    """A settings page in miniature: one text field, kept on commit unless it is ``bad``."""

    BINDINGS: ClassVar = [UNDO_BINDING]

    def __init__(self) -> None:
        super().__init__(id="page")
        self.disk = "old"
        self.writes: list[str] = []

    def compose(self) -> ComposeResult:
        yield CommitInput(self.disk, id="field")

    def on_commit_input_committed(self, event: CommitInput.Committed) -> None:
        field = event.field
        if event.value == "bad":
            self.refuse(field, "not saved: bad is not a value")
            return
        self.accept(field)
        previous, self.disk = self.disk, event.value
        self.writes.append(event.value)
        field.keep(event.value)

        def undo() -> None:
            self.disk = previous
            self.writes.append(previous)
            field.keep(previous)

        self.kept(f"the field ({event.value})", undo)


class Host(App[None]):
    def compose(self) -> ComposeResult:
        yield Page()
        yield Input(id="elsewhere")


def drive(fn: Any) -> Any:
    async def run() -> Any:
        app = Host()
        async with app.run_test() as pilot:
            await pilot.pause()
            return await fn(pilot, app, app.query_one(Page))

    return asyncio.run(run())


def test_a_field_is_kept_on_enter_or_on_leaving_it_once_per_change() -> None:
    async def go(pilot: Any, app: Host, page: Page) -> list[str]:
        field = page.query_one(CommitInput)
        field.focus()
        await pilot.pause()
        field.value = "new"
        await pilot.press("enter")
        await pilot.pause()
        app.query_one("#elsewhere").focus()  # leaving it: the same value, not kept again
        await pilot.pause()
        field.focus()
        await pilot.pause()
        field.value = "newer"
        app.query_one("#elsewhere").focus()  # leaving it with a new value keeps it
        await pilot.pause()
        field.focus()
        await pilot.pause()
        app.query_one("#elsewhere").focus()  # unchanged: nothing written
        await pilot.pause()
        return page.writes

    assert drive(go) == ["new", "newer"]


def test_a_refused_value_is_said_beside_its_field_and_nothing_is_written() -> None:
    async def go(pilot: Any, app: Host, page: Page) -> tuple[str, str, str, list[str]]:
        field = page.query_one(CommitInput)
        field.focus()
        field.value = "bad"
        await pilot.press("enter")
        await pilot.pause()
        refused = page.problem(field)
        kept_value = field.value
        field.value = "good"
        await pilot.press("enter")
        await pilot.pause()
        return refused, kept_value, page.problem(field), page.writes

    refused, typed, after, writes = drive(go)
    assert refused == "not saved: bad is not a value"
    assert typed == "bad", "the field keeps what was typed, to correct"
    assert after == "", "a good value clears the line"
    assert writes == ["good"], "the refused value was never written"


def test_ctrl_z_takes_the_page_s_changes_back_last_first() -> None:
    async def go(pilot: Any, app: Host, page: Page) -> dict[str, Any]:
        field = page.query_one(CommitInput)
        field.focus()
        for value in ("one", "two"):
            field.value = value
            await pilot.press("enter")
            await pilot.pause()
        seen: dict[str, Any] = {"kept": page.disk}
        await pilot.press("ctrl+z")
        await pilot.pause()
        seen["once"] = (page.disk, field.value)
        await pilot.press("ctrl+z")
        await pilot.pause()
        seen["twice"] = (page.disk, field.value)
        await pilot.press("ctrl+z")  # nothing left
        await pilot.pause()
        seen["toasts"] = [n.message for n in app._notifications]
        return seen

    seen = drive(go)
    assert seen["kept"] == "two"
    assert seen["once"] == ("one", "one") and seen["twice"] == ("old", "old")
    assert seen["toasts"] == [
        "Saved the field (one) · Undo: ctrl+z",
        "Saved the field (two) · Undo: ctrl+z",
        "Undone: the field (two)",
        "Undone: the field (one)",
        "Nothing to undo here",
    ]


def test_keep_shows_a_value_put_back_without_keeping_it_again() -> None:
    """Undo (or a value saved elsewhere) puts a value in the field: shown, never committed."""

    async def go(pilot: Any, app: Host, page: Page) -> list[str]:
        field = page.query_one(CommitInput)
        field.keep("from elsewhere")
        field.focus()
        await pilot.pause()
        app.query_one("#elsewhere").focus()
        await pilot.pause()
        return page.writes

    assert drive(go) == []
