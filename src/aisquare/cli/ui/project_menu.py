"""A project card's menu — ⋯ on the card, ``.`` or a right-click — and its Remove, which asks
in place; and a group header's menu, the same box (#39).

The menu answers what was chosen and does nothing itself: the shell (``FleetApp``)
renames, pins, opens the Settings tab, pauses or resumes the project's agents
(``views.fleet_bulk``), starts the project fresh (``ui.fresh``) or removes, as it does for
every gesture the sidebar posts. Every name in it is a ``rich.text.Text``: a nickname is
free text, and ``[wip]`` is not markup.

*Remove from fleet* is its last row, a :class:`~aisquare.cli.ui.confirm_button.ConfirmButton`:
its first press arms it ("Remove? click again") and says under it what leaves the fleet and
what is kept; the second press on it answers :data:`REMOVE` (user, 2026-10-30: a double click
in place, not a dialog's button). The shell removes on that answer at once.

A group header's menu (:class:`GroupMenu`) is the card's box with the card's keys
(:class:`RowMenu`): Rename…, Pin / Unpin, Collapse / Expand, Move up and Move down, and
*Delete group* last, which asks in place as Remove does and names what it moves.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import ClassVar

from rich.text import Text
from textual import events, on
from textual.app import ComposeResult
from textual.binding import Binding, BindingType
from textual.containers import Vertical
from textual.geometry import Region
from textual.screen import ModalScreen
from textual.widgets import Button, OptionList, Static
from textual.widgets.option_list import Option

from aisquare.cli.ui.confirm_button import ConfirmButton
from aisquare.cli.ui.sidebar import SETTINGS_ICON
from aisquare.models import ProjectGroup, ProjectInfo
from aisquare.services.project import display_name

RENAME = "rename"
PIN = "pin"
SETTINGS = "settings"
PAUSE = "pause"
RESUME = "resume"
FRESH = "fresh"
REMOVE = "remove"
COLLAPSE = "collapse"
MOVE_UP = "move-up"
MOVE_DOWN = "move-down"
DELETE = "delete"

MENU_WIDTH = 26
"""The menu's columns, its border included: it fits the narrowest sidebar beside it. The one
source of it: the box is sized from it on mount, as it is placed."""

NOTE_WIDTH = MENU_WIDTH - 4
"""The columns of the line under Remove: the menu's, less its border and the line's padding."""


def remove_note(project: ProjectInfo) -> Text:
    """What an armed *Remove from fleet* says under it: what leaves the fleet, and what stays
    (the toast after the removal names the command that brings it back)."""
    text = Text(display_name(project), style="bold")
    text.append(" leaves the fleet; its folder and history are kept.")
    return text


def delete_note(group: ProjectGroup, count: int) -> Text:
    """What an armed *Delete group* says under it: the group, and what becomes of its
    ``count`` projects (``project_groups.delete_group`` moves them back to the top level)."""
    text = Text(group.name, style="bold")
    if count == 0:
        text.append(" is empty. No project is deleted.")
    else:
        moves = "project moves" if count == 1 else "projects move"
        text.append(f": its {count} {moves} back to the top level. No project is deleted.")
    return text


class MenuOptions(OptionList):
    """The menu's options above Remove: ↓ from the last goes on to it, and ↑ from the first
    wraps round to it, as they would to one more option."""

    def action_cursor_down(self) -> None:
        if self.highlighted is not None and self.highlighted >= self.option_count - 1:
            self.screen.query_one(RemoveRow).focus()
        else:
            super().action_cursor_down()

    def action_cursor_up(self) -> None:
        if not self.highlighted:
            self.screen.query_one(RemoveRow).focus()
        else:
            super().action_cursor_up()


class RemoveRow(ConfirmButton):
    """*Remove from fleet*, the menu's last row: ↑ and ↓ go back to the options above."""

    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("up", "to_options(-1)", show=False),
        Binding("down", "to_options(0)", show=False),
    ]

    def action_to_options(self, index: int) -> None:
        options = self.screen.query_one(MenuOptions)
        options.highlighted = options.option_count - 1 if index < 0 else 0
        options.focus()


class RowMenu(ModalScreen[str | None]):
    """A sidebar row's menu: the box a card's and a group header's share, their keys, and
    their place, under the row's line (``anchor``).

    Its options are a :class:`MenuOptions` and answer their ids; its last row is a
    :class:`RemoveRow`, which asks in place and answers :attr:`LAST` once pressed twice. Esc
    (with that row armed, the first Esc disarms it), or a click anywhere off the menu,
    answers ``None``.
    """

    CSS = """
    RowMenu { align: left top; background: $background 20%; }
    RowMenu #project-menu { height: auto; border: round $accent; background: $surface; }
    RowMenu #project-menu-title {
        height: 1; padding: 0 1; text-style: bold; text-wrap: nowrap; text-overflow: ellipsis;
    }
    RowMenu OptionList { height: auto; border: none; padding: 0; }
    RowMenu #project-menu-remove {
        width: 100%; min-width: 0; height: 1; padding: 0; background: $surface;
        content-align: left middle; text-align: left; text-style: none;
        text-wrap: nowrap; text-overflow: ellipsis;
    }
    RowMenu #project-menu-remove:focus {
        background: $block-cursor-background; color: $block-cursor-foreground;
    }
    RowMenu #project-menu-remove.-armed { background: $error; color: $text; }
    RowMenu #project-menu-note { display: none; height: auto; padding: 0 1; }
    """
    SCOPED_CSS: ClassVar[bool] = False
    """Each rule above starts with ``RowMenu``, so it reaches the card's menu and the group's
    and nothing else. Scoped, Textual prefixes a screen's ``CSS`` with the screen's OWN type
    (``ProjectMenu RowMenu …``), and no rule matched either menu."""

    BINDINGS: ClassVar = [("escape", "close_menu", "close")]

    LAST: ClassVar[str] = REMOVE
    """What the last row answers once it was pressed twice."""

    SEPARATORS = 0
    """The lines between the menu's groups (its ``compose``), counted into its height."""

    def __init__(self, anchor: Region) -> None:
        super().__init__()
        self.near = anchor
        """Where the row is on screen: the menu opens under its line."""

    def last_note(self) -> Text:  # pragma: no cover - every subclass overrides
        """What the last row says under it once armed."""
        raise NotImplementedError

    def on_mount(self) -> None:
        box = self.query_one("#project-menu", Vertical)
        options = self.query_one(OptionList)
        # The title, the last row and the border; and room for the note that row shows when
        # armed, so that showing it moves nothing and is not cut by the screen's edge.
        note = self.last_note().wrap(self.app.console, NOTE_WIDTH)
        height = options.option_count + self.SEPARATORS + 4 + len(note)
        width, rows = self.app.size
        x = max(0, min(self.near.x + 2, width - MENU_WIDTH))
        y = max(0, min(self.near.y + 1, rows - height))
        box.styles.width = MENU_WIDTH
        box.styles.offset = (x, y)
        options.highlighted = 0
        options.focus()

    @on(OptionList.OptionSelected, "#project-menu-options")
    def _chosen(self, event: OptionList.OptionSelected) -> None:
        event.stop()
        self.dismiss(event.option.id)

    @on(Button.Pressed, "#project-menu-remove")
    def _remove(self, event: Button.Pressed) -> None:
        event.stop()
        self.dismiss(self.LAST)  # the second press: the first armed it

    def on_click(self, event: events.Click) -> None:
        # A click that reaches the screen itself fell outside the menu: close, as Esc does.
        if event.widget is self:
            self.dismiss(None)

    def action_close_menu(self) -> None:
        self.dismiss(None)


class ProjectMenu(RowMenu):
    """Under the card it is about, in three groups (Q11 of the UX audit): the project (Rename,
    Pin / Unpin, Open settings), its agents (Pause agents… and Resume agents…), and starting
    over (Start fresh…, then Remove from fleet, a row of its own).

    Answers :data:`RENAME`, :data:`PIN`, :data:`SETTINGS`, :data:`PAUSE`, :data:`RESUME`,
    :data:`FRESH`, or :data:`REMOVE` once Remove was pressed twice; Esc (with Remove armed, the
    first Esc disarms it), or a click anywhere off the menu, answers ``None``.
    ``can`` says whether a pause and a resume would act on any of its agents
    (``fleet_bulk.applicable``). Each that would act is listed, and only those: both when both
    would, enabled; one when one would. When neither would, a greyed Pause holds the agents'
    group, so it is never empty. (Both used to be listed always, the idle one greyed.)
    """

    SEPARATORS = 2

    def __init__(
        self, project: ProjectInfo, anchor: Region, can: Mapping[str, bool] | None = None
    ) -> None:
        super().__init__(anchor)
        self.project = project
        self.can = can if can is not None else {PAUSE: True, RESUME: True}

    def agent_options(self) -> list[Option]:
        """The agents' group: Pause agents… and Resume agents…, each where it would act on one
        of the project's agents; with neither, a greyed Pause agents… holds the group's row."""
        pause = self.can.get(PAUSE, True)
        resume = self.can.get(RESUME, True)
        options = []
        if pause or not resume:
            options.append(Option("⏸ Pause agents…", id=PAUSE, disabled=not pause))
        if resume:
            options.append(Option("▶ Resume agents…", id=RESUME))
        return options

    def compose(self) -> ComposeResult:
        pin = "Unpin" if self.project.pinned_at is not None else "Pin"
        with Vertical(id="project-menu"):
            yield Static(Text(display_name(self.project)), id="project-menu-title")
            yield MenuOptions(
                Option("✎ Rename", id=RENAME),
                Option(f"📌 {pin}", id=PIN),
                Option(f"{SETTINGS_ICON} Open settings", id=SETTINGS),
                None,
                *self.agent_options(),
                None,
                Option("↺ Start fresh…", id=FRESH),
                id="project-menu-options",
            )
            yield RemoveRow(
                "✗ Remove from fleet",
                armed_label="✗ Remove? click again",
                note=remove_note(self.project),
                note_in="#project-menu-note",
                id="project-menu-remove",
                compact=True,
            )
            yield Static(id="project-menu-note")

    def last_note(self) -> Text:
        return remove_note(self.project)


class GroupMenu(RowMenu):
    """Under the group header it is about (#39), the card menu's box and keys: the group
    (Rename…, Pin / Unpin, Collapse / Expand), its place (Move up, Move down), then *Delete
    group*, a row of its own that asks in place, as a card's Remove does.

    Answers :data:`RENAME`, :data:`PIN`, :data:`COLLAPSE`, :data:`MOVE_UP`, :data:`MOVE_DOWN`,
    or :data:`DELETE` once Delete was pressed twice; Esc or a click off it answers ``None``.
    ``count`` is how many projects are the group's, the pinned ones too: the armed Delete
    says they move back to the top level (:func:`delete_note`). A move is listed only where it
    would move the group (``up``, ``down``): a pinned group keeps its pin order, and an end
    has nowhere further to go, as a card's menu lists only a pause that would act.
    """

    LAST = DELETE

    def __init__(
        self, group: ProjectGroup, anchor: Region, *, count: int, up: bool, down: bool
    ) -> None:
        super().__init__(anchor)
        self.group = group
        self.count = count
        self.up = up
        self.down = down
        self.SEPARATORS = 1 if up or down else 0

    def move_options(self) -> list[Option]:
        options = []
        if self.up:
            options.append(Option("↑ Move up", id=MOVE_UP))
        if self.down:
            options.append(Option("↓ Move down", id=MOVE_DOWN))
        return options

    def compose(self) -> ComposeResult:
        pin = "Unpin" if self.group.pinned_at is not None else "Pin"
        fold = "▾ Expand" if self.group.collapsed else "▸ Collapse"
        moves = self.move_options()
        with Vertical(id="project-menu"):
            yield Static(Text(f"📁 {self.group.name}"), id="project-menu-title")
            yield MenuOptions(
                Option("✎ Rename…", id=RENAME),
                Option(f"📌 {pin}", id=PIN),
                Option(fold, id=COLLAPSE),
                *([None, *moves] if moves else []),
                id="project-menu-options",
            )
            yield RemoveRow(
                "✗ Delete group",
                armed_label="✗ Delete? click again",
                note=self.last_note(),
                note_in="#project-menu-note",
                id="project-menu-remove",
                compact=True,
            )
            yield Static(id="project-menu-note")

    def last_note(self) -> Text:
        return delete_note(self.group, self.count)
