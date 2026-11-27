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


def test_the_value_on_disk_put_back_takes_the_refusal_line_away() -> None:
    """Tester of #24, F1: refused, then the value on disk typed back, Enter or leaving the field
    posts no change (nothing to keep), and the line saying why the other value was refused
    goes, with nothing written."""

    async def go(pilot: Any, app: Host, page: Page) -> tuple[str, str, str, list[str]]:
        field = page.query_one(CommitInput)
        field.focus()
        field.value = "bad"
        await pilot.press("enter")
        await pilot.pause()
        refused = page.problem(field)
        field.value = "old"  # what is on disk
        await pilot.press("enter")
        await pilot.pause()
        on_enter = page.problem(field)
        field.value = "bad"
        await pilot.press("enter")
        await pilot.pause()
        field.value = "old"
        app.query_one("#elsewhere").focus()  # leaving it, this time
        await pilot.pause()
        return refused, on_enter, page.problem(field), page.writes

    refused, on_enter, on_leaving, writes = drive(go)
    assert refused.startswith("not saved")
    assert (on_enter, on_leaving) == ("", ""), "the line goes on Enter and on leaving the field"
    assert writes == [], "nothing to write: the value on disk"


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


# --- no Save, Apply or Cancel on a settings page (M5 acceptance 2) -------------------------

SETTINGS_MODULES = (
    # The pages under ⚙ Settings, and the dialogs they open.
    "views/settings_hub.py",
    "views/app_settings.py",
    "views/settings_captain.py",
    "views/settings_remote.py",
    "views/remote.py",
    "views/accounts.py",
    "views/personas_tab.py",
    "views/usage_sharing.py",
    "views/settings.py",  # a project's Settings tab
    "views/fleet_bulk.py",  # the hub's Fleet buttons' box
    "fresh.py",  # a project's Settings tab: Start fresh…'s boxes
    "spawn.py",  # Personas' Attach to new
    "adopt_picker.py",  # Accounts' adopt flow
    "persona_dialogs.py",
    "attach.py",
)
"""Where a settings page or one of its dialogs is built (under ``cli/ui``)."""

EXCEPTIONS = {
    # A dialog that creates, imports, exports or picks something keeps its buttons:
    ("attach.py", "NewBindScreen", "Save"): "App settings' + New binding: creates a binding",
    ("attach.py", "NewBindScreen", "Cancel"): "the same dialog: nothing created",
    ("attach.py", "AttachTargetScreen", "Cancel"): "Personas' attach: picks where it goes",
    ("persona_dialogs.py", "NewPersonaScreen", "Cancel"): "Personas' + New: creates a persona",
    ("persona_dialogs.py", "ImportPersonaScreen", "Cancel"): "Personas' + Import: creates one",
    ("persona_dialogs.py", "ConfirmDraftScreen", "Save"): "the import's draft, kept as a new one",
    ("persona_dialogs.py", "ExportPersonaScreen", "Cancel"): "Personas' Export: writes a copy",
    # A SKILL.md is one document, held to the persona test whole (a half-typed one is
    # refused), so it is written as an editor writes a file; ctrl+z on the page takes it back.
    ("persona_dialogs.py", "EditPersonaScreen", "Save"): "the SKILL.md editor",
    ("persona_dialogs.py", "EditPersonaScreen", "Save as…"): "a bundled one, copied into yours",
    ("persona_dialogs.py", "EditPersonaScreen", "Cancel"): "the editor, closed unsaved",
    ("persona_dialogs.py", "EditorPaneScreen", "Cancel"): "the $EDITOR pane, closed",
    # A box that asks before an action that cannot be undone:
    ("persona_dialogs.py", "ConfirmRemoveScreen", "Cancel"): "Remove deletes the folder",
    ("views/remote.py", "RemoteConfirm", "Cancel"): "a new link or password breaks the old",
    ("views/app_settings.py", "UnsetKeyScreen", "Cancel"): "a binding's key, unset",
    ("views/fleet_bulk.py", "ConfirmBulk", "Cancel"): "the hub's Stop/Restart/Pause/Resume all",
    ("fresh.py", "ConfirmFresh", "Cancel"): "Start fresh…: clears a project's board and fleet",
    ("fresh.py", "ConfirmDiscard", "Cancel"): "Start fresh's second box: work it would discard",
    ("spawn.py", "SpawnDialog", "Cancel"): "Personas' Attach to new: creates an agent",
    # Stopping a sign-in that is under way: nothing on the page is being edited.
    ("views/accounts.py", "AccountsView", "Cancel"): "stops the AISquare sign-in",
    ("views/accounts.py", "AccountsView", "Cancel sign-in"): "stops a coding agent's sign-in",
}
"""Each Save, Apply or Cancel button left in :data:`SETTINGS_MODULES`, and why it stays."""

_BUTTONS = ("Button", "GuardedButton", "ConfirmButton")


def _commit_buttons() -> set[tuple[str, str, str]]:
    import ast
    from pathlib import Path

    import aisquare.cli.ui as ui

    base = Path(ui.__file__).parent
    found: set[tuple[str, str, str]] = set()
    for module in SETTINGS_MODULES:
        tree = ast.parse((base / module).read_text(encoding="utf-8"))
        for cls in (node for node in ast.walk(tree) if isinstance(node, ast.ClassDef)):
            for call in (node for node in ast.walk(cls) if isinstance(node, ast.Call)):
                name = getattr(call.func, "id", getattr(call.func, "attr", ""))
                if name not in _BUTTONS or not call.args:
                    continue
                label = call.args[0]
                if not (isinstance(label, ast.Constant) and isinstance(label.value, str)):
                    continue
                if label.value.strip().lower().startswith(("save", "apply", "cancel")):
                    found.add((module, cls.name, label.value))
    return found


def test_no_settings_page_has_a_save_apply_or_cancel_button_but_the_listed_ones() -> None:
    found = _commit_buttons()
    assert found - EXCEPTIONS.keys() == set(), "a Save, Apply or Cancel button not listed"
    assert EXCEPTIONS.keys() - found == set(), "listed, but gone: take it off the list"


def test_an_undo_whose_change_went_stale_is_dropped_and_the_older_one_follows() -> None:
    """The reviewer of #24 (#1813): an undo whose change was replaced since, by another way
    in, acts on nothing; it is dropped, said, and not left in front of the older ones."""
    from aisquare.cli.ui.save_model import STALE

    async def go(pilot: Any, app: Host, page: Page) -> tuple[list[str], list[str]]:
        undone: list[str] = []
        page.kept("the older", lambda: undone.append("the older"))
        page.kept("the stale", lambda: STALE)
        page.query_one(CommitInput).focus()  # the keys on the page
        await pilot.press("ctrl+z")
        await pilot.pause()
        await pilot.press("ctrl+z")
        await pilot.pause()
        return undone, [n.message for n in app._notifications]

    undone, toasts = drive(go)
    assert undone == ["the older"], "the second ctrl+z reached the older change"
    assert toasts[-2:] == [
        "Not undone: the stale has changed since, elsewhere",
        "Undone: the older",
    ]


def test_forgetting_none_forgets_nothing() -> None:
    """Reviewer of #24, n1: forget(None) dropped every change kept with no key."""

    async def go(pilot: Any, app: Host, page: Page) -> list[str]:
        page.kept("unkeyed", lambda: None)
        page.kept("keyed", lambda: None, key=("user", "pair"))
        page.forget(None)
        kept_before = [change.what for change in page.changes]
        page.forget(("user", "pair"))
        return kept_before + ["|"] + [change.what for change in page.changes]

    assert drive(go) == ["unkeyed", "keyed", "|", "unkeyed"]


# --- #53: a value in flight is committed once -------------------------------------------


class LatePage(Page):
    """A page that answers later, as one saving off the UI thread does: each commit is held in
    :attr:`asked` until the test answers it with :meth:`land`, :meth:`fail` or ``refuse``."""

    def __init__(self) -> None:
        super().__init__()
        self.asked: list[str] = []

    def on_commit_input_committed(self, event: CommitInput.Committed) -> None:
        event.prevent_default()  # not Page's handler too, which keeps at once
        self.asked.append(event.value)

    def land(self) -> None:
        """The save of the last value asked lands: on disk, kept, with its Undo."""
        field = self.query_one(CommitInput)
        previous, self.disk = self.disk, self.asked[-1]
        self.writes.append(self.disk)
        field.keep(self.disk)

        def undo() -> None:
            self.disk = previous
            self.writes.append(previous)
            field.keep(previous)

        self.kept(f"the field ({self.disk})", undo)

    def fail(self) -> None:
        """The save failed: nothing written, the typed value left to try again."""
        self.query_one(CommitInput).release()


class LateHost(Host):
    def compose(self) -> ComposeResult:
        yield LatePage()
        yield Input(id="elsewhere")


def drive_late(fn: Any) -> Any:
    async def run() -> Any:
        app = LateHost()
        async with app.run_test() as pilot:
            await pilot.pause()
            return await fn(pilot, app, app.query_one(LatePage))

    return asyncio.run(run())


async def _enter_then_leave(pilot: Any, app: App[None], field: CommitInput, value: str) -> None:
    field.focus()
    await pilot.pause()
    field.value = value
    await pilot.press("enter")
    await pilot.pause()
    app.query_one("#elsewhere").focus()  # left before the page has answered
    await pilot.pause()


def test_enter_then_leaving_before_the_save_lands_commits_once_and_undoes_in_one() -> None:
    """#53: Enter, then leaving the field before the page has heard its save, asks the page
    once; the save lands as one write and one Undo, and one ctrl+z puts the old value back."""

    async def go(pilot: Any, app: LateHost, page: LatePage) -> dict[str, Any]:
        field = page.query_one(CommitInput)
        await _enter_then_leave(pilot, app, field, "new")
        seen: dict[str, Any] = {"asked": list(page.asked), "in_flight": field.in_flight}
        page.land()
        seen["landed"] = (page.writes[:], len(page.changes), field.kept, field.in_flight)
        field.focus()
        await pilot.pause()
        app.query_one("#elsewhere").focus()  # the value on disk now: nothing to ask
        await pilot.pause()
        seen["asked_after"] = list(page.asked)
        field.focus()  # ctrl+z reaches the page from a control on it
        await pilot.pause()
        await pilot.press("ctrl+z")
        await pilot.pause()
        seen["undone"] = (page.disk, field.value, len(page.changes))
        return seen

    seen = drive_late(go)
    assert seen["asked"] == ["new"] and seen["in_flight"] == "new", seen
    assert seen["landed"] == (["new"], 1, "new", None), seen
    assert seen["asked_after"] == ["new"], "kept: leaving the field again asks nothing"
    assert seen["undone"] == ("old", "old", 0), "one ctrl+z: the old value, on disk and shown"


def test_a_value_whose_save_failed_or_was_refused_is_committed_again() -> None:
    """The guard lasts until the page answers: a save that failed (``release``) or a value it
    refused (``refuse``) leaves the typed value to try again, and Enter asks again."""

    async def go(pilot: Any, app: LateHost, page: LatePage) -> tuple[list[str], list[str]]:
        field = page.query_one(CommitInput)
        await _enter_then_leave(pilot, app, field, "new")
        page.fail()
        await _enter_then_leave(pilot, app, field, "new")
        after_failure = list(page.asked)
        page.refuse(field, "not saved: try again")
        await _enter_then_leave(pilot, app, field, "new")
        return after_failure, list(page.asked)

    after_failure, after_refusal = drive_late(go)
    assert after_failure == ["new", "new"], "released: asked again"
    assert after_refusal == ["new", "new", "new"], "refused: asked again"


def test_a_page_s_re_read_of_what_is_on_disk_answers_the_value_in_flight() -> None:
    """Setting :attr:`CommitInput.kept` (a page's re-read) answers the value in flight, and a
    new value is asked at once even while another is in flight."""

    async def go(pilot: Any, app: LateHost, page: LatePage) -> tuple[str | None, list[str]]:
        field = page.query_one(CommitInput)
        await _enter_then_leave(pilot, app, field, "new")
        await _enter_then_leave(pilot, app, field, "newer")  # another value: asked
        field.kept = "old"
        cleared = field.in_flight
        await _enter_then_leave(pilot, app, field, "newer")
        return cleared, list(page.asked)

    cleared, asked = drive_late(go)
    assert cleared is None
    assert asked == ["new", "newer", "newer"]


def test_keep_leaves_text_typed_since_and_shows_what_it_answers() -> None:
    """#56: an earlier save's answer keeps its value (``kept``) but does not replace text typed
    in the field since: that is the person's. What it answers (the value in flight, here
    normalised) and a field showing the value kept (Undo's keep) are shown as before."""

    async def go(pilot: Any, app: LateHost, page: LatePage) -> dict[str, Any]:
        field = page.query_one(CommitInput)
        field.focus()
        await pilot.pause()
        field.value = "new"
        await pilot.press("enter")
        await pilot.pause()
        field.value = "typed"  # typed after Enter, not committed
        page.land()
        seen: dict[str, Any] = {"typing": (field.value, field.kept)}
        field.value = " padded "
        await pilot.press("enter")
        await pilot.pause()
        field.keep("padded")  # the store's own spelling of the value it answers
        seen["answered"] = (field.value, field.kept)
        field.keep("old")  # Undo's: the field shows what was kept
        seen["undone"] = (field.value, field.kept)
        return seen

    seen = drive_late(go)
    assert seen["typing"] == ("typed", "new"), "the typed text stays; the value is kept"
    assert seen["answered"] == ("padded", "padded")
    assert seen["undone"] == ("old", "old")
