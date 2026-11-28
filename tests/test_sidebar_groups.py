"""The sidebar's project groups, finished (#39, the owner's request of 2026-11-27).

A group header has the card's menu: ⋯ while it is hovered or highlighted, a click there, a
right-click on it, or ``.``. In it: Rename… (in place), Pin / Unpin, Collapse / Expand, Move
up / Move down, and *Delete group*, which asks in place and names the group and its projects.
Every one goes through ``services.project_groups`` onto the layout's one undo stack, which
``u`` and ctrl+z take back. An expanded group with no project says so, quietly, under its
header. And every arranging command the CLI has is reachable from the TUI (the parity test).

Driven through the whole app with real keys and the terminal's own mouse events: what is
asserted is what the sidebar shows and what the store holds.
"""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import pytest
from textual import events
from textual.app import App, ComposeResult
from textual.geometry import Region
from textual.pilot import Pilot
from textual.widget import Widget
from textual.widgets import Input, OptionList, Static

from aisquare.cli import project as project_cli
from aisquare.cli.ui.app import FleetApp
from aisquare.cli.ui.groups import NEW_GROUP, UNGROUP, GroupPicker
from aisquare.cli.ui.project_menu import (
    COLLAPSE,
    DELETE,
    MOVE_DOWN,
    MOVE_UP,
    PIN,
    RENAME,
    GroupMenu,
    ProjectMenu,
    RemoveRow,
)
from aisquare.cli.ui.save_model import CommitInput
from aisquare.cli.ui.sidebar import (
    GROUP_EMPTY,
    MENU_GLYPH,
    GroupEmptyLine,
    GroupHeader,
    ProjectTitle,
    Sidebar,
)
from aisquare.cli.ui.views.settings import SettingsView
from aisquare.core.store import store_session
from aisquare.models import ProjectGroup
from aisquare.services import project_groups as groups_service
from tests import toast_log
from tests.fleet_fakes import tmux_sessions_as
from tests.test_ui_shell import (
    Script,
    _as_the_terminal_sends,
    _cards,
    _click,
    _drag_onto,
    card_for,
    drive,
    fleet_app,
    seed,
    settle,
    shown,
    status,
)
from tests.test_ui_shell import no_real_tmux as _no_real_tmux
from tests.test_ui_shell import script as _script
from tests.ui_workers import settle_until

#: Re-exported so pytest collects them here (the guard is autouse there, and here).
no_real_tmux = _no_real_tmux
script = _script

THREE = (("prj_a", "api", None), ("prj_b", "cli", None), ("prj_c", "docs", None))


def gid(name: str) -> str:
    with store_session() as store:
        return groups_service.resolve_group(store, name).id


def stored_group(name: str) -> ProjectGroup | None:
    with store_session() as store:
        return store.find_project_group(name)


def stored_names() -> set[str]:
    with store_session() as store:
        return {g.name for g in store.project_groups()}


def drawn(widget: Widget) -> str:
    """Every row ``widget`` composites, padding and all, joined: what the eye gets.
    (``composited`` reads one row at the content's width, which cuts a padded line's end.)"""
    lines = widget.render_lines(Region(0, 0, widget.region.width, widget.region.height))
    return "\n".join(strip.text.rstrip() for strip in lines)


def header(app: FleetApp, name: str) -> GroupHeader:
    return next(h for h in app.sidebar.query(GroupHeader) if h.group.name == name)


def listed(app: FleetApp) -> list[str]:
    """The projects list as shown: cards by id, headers by name, the Pinned label, and a
    group's empty line as ``empty:<group name>``."""
    names = {h.group.id: h.group.name for h in app.sidebar.query(GroupHeader)}
    out: list[str] = []
    for child in app.sidebar.query_one("#projects").children:
        if not child.display:
            continue
        if isinstance(child, GroupEmptyLine):
            out.append(f"empty:{names[child.group_id]}")
        elif isinstance(child, GroupHeader):
            out.append(f"group:{child.group.name}")
        elif child.id == "pinned-label":
            out.append("pinned")
        elif child.id and child.id.startswith("card-"):
            out.append(child.id.removeprefix("card-"))
    return out


async def highlight(pilot: Pilot[None], app: FleetApp, name: str) -> GroupHeader:
    """Put the sidebar's one highlight on ``name``'s header, the keys in the sidebar."""
    app.sidebar.focus()
    app.sidebar.select(f"group:{gid(name)}")
    await pilot.pause()
    return header(app, name)


async def menu_by_dot(pilot: Pilot[None], app: FleetApp, name: str) -> GroupMenu:
    await highlight(pilot, app, name)
    await pilot.press("full_stop")
    await settle(app)
    menu = app.screen
    assert isinstance(menu, GroupMenu), type(menu).__name__
    return menu


def entries(menu: GroupMenu) -> list[str]:
    options = menu.query_one(OptionList)
    return [str(options.get_option_at_index(i).id) for i in range(options.option_count)]


async def choose(pilot: Pilot[None], app: FleetApp, menu: GroupMenu, answer: str) -> None:
    """Arrow down to ``answer`` and press Enter, as a person does."""
    await pilot.press(*["down"] * entries(menu).index(answer))
    await pilot.press("enter")
    await settle(app)


async def delete_from_menu(pilot: Pilot[None], app: FleetApp, menu: GroupMenu) -> str:
    """↑ from the first option wraps to *Delete group*: Enter arms it, and its note is read;
    Enter again deletes. The note, as shown."""
    await pilot.press("up")
    await pilot.pause()
    assert isinstance(app.focused, RemoveRow), type(app.focused).__name__
    await pilot.press("enter")
    await pilot.pause()
    note = shown(menu.query_one("#project-menu-note", Static))
    await pilot.press("enter")
    await settle(app)
    return note


# --- the ⋯, and the ways the menu opens -----------------------------------------------------


def test_a_group_header_draws_its_menu_dots_only_while_hovered_or_highlighted(
    tmp_path: Path, script: Script
) -> None:
    """The calm rule: ⋯ only on hover or highlight, as an agent's row (Q-F2). A plain header
    gives up no cell."""
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "tools", ["prj_b"])
        groups_service.create_group(store, "web", ["prj_c"])

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        dots = MENU_GLYPH.strip()
        seen: dict[str, object] = {
            "plain": [dots in drawn(h) for h in app.sidebar.query(GroupHeader)]
        }
        tools = await highlight(pilot, app, "tools")
        seen["highlighted"] = (
            drawn(tools).rstrip().endswith(dots),
            dots in drawn(header(app, "web")),
        )
        await pilot.hover(header(app, "web"))
        await pilot.pause()
        seen["hovered"] = drawn(header(app, "web")).rstrip().endswith(dots)
        await pilot.hover(card_for(app, "prj_a"))
        await pilot.pause()
        seen["left"] = dots in drawn(header(app, "web"))
        return seen

    seen = drive(go)
    assert seen["plain"] == [False, False]
    assert seen["highlighted"] == (True, False)
    assert seen["hovered"] is True
    assert seen["left"] is False


def test_dot_a_right_click_and_a_click_on_the_dots_open_the_group_menu_and_fold_nothing(
    tmp_path: Path, script: Script
) -> None:
    """``.`` on the highlighted header, a right-click anywhere on it, and a click on its ⋯
    each open the group's menu, under it: the card menu's box, titled with the group. None
    of them folds the group, as a plain click does."""
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "tools", ["prj_b", "prj_c"])

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        seen: dict[str, object] = {}
        menu = await menu_by_dot(pilot, app, "tools")
        seen["dot"] = shown(menu.query_one("#project-menu-title", Static))
        seen["border"] = menu.query_one("#project-menu").styles.border
        await pilot.press("escape")
        await settle(app)
        app.sidebar.open_menu("prj_b")  # the card menu, in the same app and theme
        await settle(app)
        assert isinstance(app.screen, ProjectMenu), type(app.screen).__name__
        seen["card border"] = app.screen.query_one("#project-menu").styles.border
        await pilot.press("escape")
        await settle(app)
        tools = header(app, "tools")
        await _as_the_terminal_sends(pilot, events.MouseDown, tools, (4, 0), button=3)
        await _as_the_terminal_sends(pilot, events.MouseUp, tools, (4, 0), button=3)
        await settle(app)
        seen["right-click"] = type(app.screen).__name__
        await pilot.press("escape")
        await settle(app)
        tools = header(app, "tools")  # highlighted still: its ⋯ is drawn
        await _click(pilot, tools)  # a plain click folds, as ever
        seen["plain click folds"] = stored_group("tools").collapsed  # type: ignore[union-attr]
        await _click(pilot, header(app, "tools"))
        tools = header(app, "tools")
        dots = (tools.region.width - 2, 0)  # the last cell inside its padding
        await _as_the_terminal_sends(pilot, events.MouseDown, tools, dots)
        await _as_the_terminal_sends(pilot, events.MouseUp, tools, dots)
        await settle(app)
        seen["dots"] = type(app.screen).__name__
        await pilot.press("escape")
        await settle(app)
        seen["folded after"] = stored_group("tools").collapsed  # type: ignore[union-attr]
        return seen

    seen = drive(go)
    assert seen["dot"] == "📁 tools"
    card_border: Any = seen["card border"]
    assert card_border.top[0] == "round", card_border  # styled: the box's own border
    assert seen["border"] == card_border, "one border: the card menu's, unchanged"
    assert seen["right-click"] == "GroupMenu"
    assert seen["plain click folds"] is True
    assert seen["dots"] == "GroupMenu"
    assert seen["folded after"] is False, "the click on its ⋯ opened the menu and folded nothing"


def test_dot_and_f2_on_a_highlighted_header_reach_the_group_and_nothing_else(
    tmp_path: Path, script: Script
) -> None:
    """Manager's ask on #39: ``.`` and F2 must not collide with another binding on a header.
    ``.`` opens the group's menu (not a card's), F2 its name field (not a card's); neither
    folds it, moves it, nor changes what the store holds."""
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "tools", ["prj_b"])

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        before = _cards(app)
        await highlight(pilot, app, "tools")
        await pilot.press("full_stop")
        await settle(app)
        seen: dict[str, object] = {"dot": type(app.screen).__name__}
        await pilot.press("escape")
        await settle(app)
        await pilot.press("f2")
        await settle(app)
        tools = header(app, "tools")
        seen["f2"] = (tools.renaming, isinstance(app.focused, Input))
        app.sidebar.open_rename("prj_a")  # a card's field: one name field at a time
        await settle(app)
        seen["one at a time"] = (header(app, "tools").renaming, card_for(app, "prj_a").renaming)
        await pilot.press("escape")
        await settle(app)
        await highlight(pilot, app, "tools")
        await pilot.press("f2")
        await settle(app)
        seen["cards renaming"] = [c.project.id for c in app.query("ProjectCard") if c.renaming]  # type: ignore[attr-defined]
        await pilot.press("escape")
        await settle(app)
        seen["after"] = (_cards(app), stored_group("tools").collapsed)  # type: ignore[union-attr]
        return seen | {"before": before}

    seen = drive(go)
    assert seen["dot"] == GroupMenu.__name__
    assert seen["f2"] == (True, True)
    assert seen["one at a time"] == (False, True)
    assert seen["cards renaming"] == []
    assert seen["after"] == (seen["before"], False)
    assert stored_names() == {"tools"}


# --- Rename…, in place ------------------------------------------------------------------------


def test_rename_edits_the_header_in_place_enter_saves_and_esc_cancels(
    tmp_path: Path, script: Script
) -> None:
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "tools", ["prj_b"])

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        seen: dict[str, object] = {}
        menu = await menu_by_dot(pilot, app, "tools")
        await choose(pilot, app, menu, RENAME)
        tools = header(app, "tools")
        field = tools.field
        assert field is not None
        seen["opened"] = (field.value, field.selected_text, app.focused is field)
        await pilot.press(*"zzz", "escape")
        await settle(app)
        seen["esc"] = (header(app, "tools").renaming, stored_names(), app.focused is app.sidebar)
        await pilot.press("f2")
        await settle(app)
        await pilot.press(*"backend", "enter")
        await settle(app)
        seen["enter"] = (stored_names(), _cards(app)[0], app.focused is app.sidebar)
        await pilot.press("ctrl+z")
        await settle(app)
        seen["undone"] = stored_names()
        return seen

    seen = drive(go)
    assert seen["opened"] == ("tools", "tools", True), "prefilled, all selected, focused"
    assert seen["esc"] == (False, {"tools"}, True), "Esc: closed, nothing written"
    assert seen["enter"] == ({"backend"}, "group:backend", True)
    assert seen["undone"] == {"tools"}, "ctrl+z takes the rename back"


def test_a_press_and_a_move_in_the_open_name_field_drag_nothing_and_fold_nothing(
    tmp_path: Path, script: Script
) -> None:
    """The field is inside the header, which is a drag handle: a press in it, moved onto
    the header above and released — a drag of the group would move it there — is the
    field's: the groups keep their order, the group stays unfolded, and the field stays open
    with what was typed."""
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "web", ["prj_c"])
        groups_service.create_group(store, "tools", ["prj_b"])

    async def go(pilot: Pilot[None]) -> tuple[list[str], list[str], bool, object]:
        app = fleet_app(pilot)
        before = _cards(app)
        await highlight(pilot, app, "tools")
        await pilot.press("f2")
        await settle(app)
        await pilot.press(*"infra")
        tools = header(app, "tools")
        await _drag_onto(pilot, tools, header(app, "web"), offset=(3, 0))
        field = header(app, "tools").field
        folded = stored_group("tools").collapsed  # type: ignore[union-attr]
        return before, _cards(app), folded, field is not None and field.value

    before, after, folded, value = drive(go)
    assert after == before
    assert folded is False
    assert value == "infra"


def test_an_empty_or_taken_name_is_refused_in_our_words_and_the_field_stays_open(
    tmp_path: Path, script: Script
) -> None:
    """A group needs a name, and names are unique with any case (the store's rule): each is
    said in the sidebar's words, nothing is written, and the field keeps what was typed until
    Esc closes it with nothing changed — even after a click away left it open."""
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "tools", ["prj_b"])
        groups_service.create_group(store, "web", ["prj_c"])

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        told = toast_log.record(app)
        seen: dict[str, object] = {}
        await highlight(pilot, app, "tools")
        await pilot.press("f2")
        await settle(app)
        await pilot.press("backspace", "space", "enter")  # opened all selected: cleared
        await settle(app)
        field = header(app, "tools").field
        seen["empty"] = (field is not None and field.value, stored_names(), list(told))
        told.clear()
        await pilot.press(*"WEB", "enter")  # refused, it is all selected again
        await settle(app)
        field = header(app, "tools").field
        seen["taken"] = (field is not None and field.value, stored_names(), list(told))
        told.clear()
        # A click away: the field is left, refused again, and stays open, unsaved.
        await _click(pilot, card_for(app, "prj_a").query_one(ProjectTitle))
        field = header(app, "tools").field
        seen["left"] = (field is not None and field.value, stored_names(), list(told))
        app.sidebar.focus()
        await pilot.press("escape")
        await settle(app)
        seen["esc"] = (header(app, "tools").renaming, stored_names())
        # Left open again by a refusal, then a card's name field opens: one field at a time,
        # the group's closing unsaved.
        await highlight(pilot, app, "tools")
        await pilot.press("f2")
        await settle(app)
        await pilot.press(*"WEB")
        await _click(pilot, card_for(app, "prj_a").query_one(ProjectTitle))
        app.sidebar.open_rename("prj_a")
        await settle(app)
        seen["card's field"] = (
            header(app, "tools").renaming,
            card_for(app, "prj_a").renaming,
            stored_names(),
        )
        return seen

    seen = drive(go)
    assert seen["empty"] == (" ", {"tools", "web"}, ["not renamed: a group needs a name"])
    assert seen["taken"] == ("WEB", {"tools", "web"}, ["not renamed: another group is called web"])
    assert seen["left"] == ("WEB", {"tools", "web"}, ["not renamed: another group is called web"])
    assert seen["esc"] == (False, {"tools", "web"}), "Esc always leaves with nothing changed"
    assert seen["card's field"] == (False, True, {"tools", "web"})


def test_a_rename_the_store_refuses_is_said_and_the_field_opens_again_with_it(
    tmp_path: Path, script: Script
) -> None:
    """Another shell took the name after the sidebar's frame was read: the shell refuses it in
    the sidebar's words, as the field itself would have (tester of #39, #3042), and the field
    opens again with what was typed, to correct."""
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "tools", ["prj_b"])

    async def go(pilot: Pilot[None]) -> tuple[object, list[str]]:
        app = fleet_app(pilot)
        told = toast_log.record(app)
        await highlight(pilot, app, "tools")
        await pilot.press("f2")
        await settle(app)
        with store_session() as store:
            groups_service.create_group(store, "infra")  # from a shell, unseen by the frame
        await pilot.press(*"infra", "enter")  # opened all selected: replaced
        await settle(app)
        field = header(app, "tools").field
        return (field is not None and field.value), told

    value, told = drive(go)
    assert value == "infra"
    assert told == ["not renamed: another group is called infra"], told
    assert stored_names() == {"tools", "infra"}


# --- Delete group…, and the owner's empty group -------------------------------------------


def test_delete_group_names_the_group_and_its_projects_and_keeps_every_project(
    tmp_path: Path, script: Script
) -> None:
    """The confirm is the card's Remove: a first press arms it and says what happens, the
    second deletes. Its N counts the pinned member too: it is the group's, and moves back too."""
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "tools", ["prj_b", "prj_c"])
        groups_service.pin(store, "prj_c")

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        told = toast_log.record(app)
        menu = await menu_by_dot(pilot, app, "tools")
        note = await delete_from_menu(pilot, app, menu)
        seen: dict[str, object] = {"note": note, "after": _cards(app), "told": list(told)}
        with store_session() as store:
            seen["projects"] = sorted(p.id for p in store.list_projects())
        await pilot.press("ctrl+z")
        await settle(app)
        seen["undone"] = _cards(app)
        return seen

    seen = drive(go)
    assert seen["note"] == (
        "tools: its 2 projects move back to the top level. No project is deleted."
    )
    assert seen["after"] == ["pinned", "prj_c", "prj_a", "prj_b"], "both at the top level"
    assert seen["projects"] == ["prj_a", "prj_b", "prj_c"], "no project is gone"
    assert seen["told"] == ["Deleted group tools · Undo: ctrl+z"]
    assert seen["undone"] == ["pinned", "prj_c", "group:tools", "prj_b", "prj_a"]
    assert stored_names() == {"tools"}


def test_an_empty_group_says_so_quietly_and_its_menu_deletes_it(
    tmp_path: Path, script: Script
) -> None:
    """The owner's case: a group left with no project looked broken and could not be removed
    from the TUI. It says ``Empty. Drag a project here, or ⋯ → Delete`` under its header —
    a line the arrows pass over, gone while it is folded — and its menu deletes it."""
    seed(tmp_path, *THREE[:2])
    with store_session() as store:
        groups_service.create_group(store, "tools", ["prj_b"])
        groups_service.create_group(store, "old")

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        seen: dict[str, object] = {"listed": listed(app)}
        line = app.sidebar.query_one(GroupEmptyLine)
        seen["says"] = " ".join(drawn(line).split())  # two lines at the sidebar's 30
        await highlight(pilot, app, "old")
        await pilot.press("down")
        await pilot.pause()
        seen["arrow past it"] = app.sidebar.selected_key
        await highlight(pilot, app, "old")
        await pilot.press("space")
        await settle(app)
        seen["folded"] = listed(app)
        await pilot.press("space")
        await settle(app)
        menu = await menu_by_dot(pilot, app, "old")
        seen["note"] = await delete_from_menu(pilot, app, menu)
        seen["deleted"] = listed(app)
        seen["lines"] = len(app.sidebar.query(GroupEmptyLine))
        return seen

    seen = drive(go)
    assert seen["listed"] == ["group:tools", "prj_b", "group:old", "empty:old", "prj_a"]
    assert seen["says"] == GROUP_EMPTY == "Empty. Drag a project here, or ⋯ → Delete"
    assert seen["arrow past it"] == "project:prj_a", "the empty line is no row"
    assert seen["folded"] == ["group:tools", "prj_b", "group:old", "prj_a"]
    assert seen["note"] == "old is empty. No project is deleted."
    assert seen["deleted"] == ["group:tools", "prj_b", "prj_a"]
    assert seen["lines"] == 0
    assert stored_names() == {"tools"}


def test_a_card_dropped_on_the_empty_line_joins_the_group_and_a_header_dropped_there_does_not(
    tmp_path: Path, script: Script
) -> None:
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "tools", ["prj_b"])
        groups_service.create_group(store, "old")

    async def go(pilot: Pilot[None]) -> tuple[list[str], list[str]]:
        app = fleet_app(pilot)
        line = app.sidebar.query_one(GroupEmptyLine)
        await _drag_onto(pilot, header(app, "tools"), line, offset=(5, 0))
        await pilot.pause()
        header_dropped = listed(app)
        title = card_for(app, "prj_c").query_one(ProjectTitle)
        await _drag_onto(pilot, title, app.sidebar.query_one(GroupEmptyLine), offset=(5, 0))
        await pilot.pause()
        return header_dropped, listed(app)

    header_dropped, card_dropped = drive(go)
    assert header_dropped == [
        "group:tools",
        "prj_b",
        "group:old",
        "empty:old",
        "prj_a",
        "prj_c",
    ], "a group is never dropped on an empty line: it snapped back"
    assert card_dropped == ["group:tools", "prj_b", "group:old", "prj_c", "prj_a"]


# --- Pin, Collapse and Move, each undone ---------------------------------------------------


def test_pin_collapse_and_move_from_the_menu_each_work_and_u_or_ctrl_z_undoes_each(
    tmp_path: Path, script: Script
) -> None:
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "a", ["prj_a"])
        groups_service.create_group(store, "b", ["prj_b"])
        groups_service.create_group(store, "c", ["prj_c"])

    async def go(pilot: Pilot[None]) -> list[tuple[str, list[str], list[str]]]:
        app = fleet_app(pilot)
        steps: list[tuple[str, list[str], list[str]]] = []
        for answer, undo in (
            (MOVE_UP, "ctrl+z"),
            (MOVE_DOWN, "u"),
            (PIN, "ctrl+z"),
            (COLLAPSE, "u"),
        ):
            menu = await menu_by_dot(pilot, app, "b")
            await choose(pilot, app, menu, answer)
            done = _cards(app)
            app.sidebar.focus()
            await pilot.press(undo)
            await settle(app)
            steps.append((answer, done, _cards(app)))
        return steps

    before = ["group:a", "prj_a", "group:b", "prj_b", "group:c", "prj_c"]
    assert drive(go) == [
        (MOVE_UP, ["group:b", "prj_b", "group:a", "prj_a", "group:c", "prj_c"], before),
        (MOVE_DOWN, ["group:a", "prj_a", "group:c", "prj_c", "group:b", "prj_b"], before),
        (PIN, ["pinned", "group:b", "prj_b", "group:a", "prj_a", "group:c", "prj_c"], before),
        (COLLAPSE, ["group:a", "prj_a", "group:b", "group:c", "prj_c"], before),
    ]


def test_the_menu_offers_only_the_moves_that_would_move_the_group(
    tmp_path: Path, script: Script
) -> None:
    """As a card's menu lists only a pause that would act: the first group has no Move up,
    the last no Move down, and a pinned group, which keeps its pin order, neither. Its Pin
    and Collapse rows say what they would do."""
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "a", ["prj_a"])
        groups_service.create_group(store, "b", ["prj_b"])
        groups_service.create_group(store, "c", ["prj_c"])
        groups_service.pin_group(store, groups_service.resolve_group(store, "c").id)
        groups_service.set_collapsed(store, groups_service.resolve_group(store, "b").id, True)

    async def go(pilot: Pilot[None]) -> dict[str, tuple[list[str], list[str]]]:
        app = fleet_app(pilot)
        seen: dict[str, tuple[list[str], list[str]]] = {}
        for name in ("a", "b", "c"):
            menu = await menu_by_dot(pilot, app, name)
            options = menu.query_one(OptionList)
            prompts = [
                str(options.get_option_at_index(i).prompt) for i in range(options.option_count)
            ]
            seen[name] = (entries(menu), prompts)
            await pilot.press("escape")
            await settle(app)
        return seen

    seen = drive(go)
    assert seen["a"][0] == [RENAME, PIN, COLLAPSE, MOVE_DOWN]
    assert seen["b"][0] == [RENAME, PIN, COLLAPSE, MOVE_UP]
    assert seen["c"][0] == [RENAME, PIN, COLLAPSE]
    assert seen["a"][1][1:3] == ["📌 Pin", "▸ Collapse"]
    assert seen["b"][1][2] == "▾ Expand"
    assert seen["c"][1][1] == "📌 Unpin"


def test_ctrl_z_in_the_sidebar_never_reaches_a_settings_pages_undo(
    tmp_path: Path, script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Manager's ask on #39: ctrl+z is the settings pages' undo too (M5). Bound on the
    sidebar, it is the sidebar's while the sidebar has the keys: it takes the layout back
    and leaves the page's kept change alone; on the page, it is still the page's."""
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "tools", ["prj_b"])
    tmux_sessions_as(monkeypatch)  # a nickname renames the project's sessions: none, faked

    def nickname() -> str | None:
        with store_session() as store:
            project = store.get_project("prj_a")
            assert project is not None
            return project.nickname

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        await app._open_settings("prj_a")
        await settle(app)
        page = app.content.get_child_by_id("project-prj_a").query_one(SettingsView)
        field = page.query_one("#nickname", CommitInput)
        field.focus()
        await pilot.pause()
        seen: dict[str, object] = {"focused": app.focused is field}
        await pilot.press(*"Auth", "enter")  # kept on Enter (M5), in the page's worker
        # Kept is the PAGE having heard the save (the field's ``kept``), not the store written:
        # the worker's thread writes the store first, and on windows-latest (run 46048849599)
        # focus left the field before the page heard, so leaving it committed "Auth" again and
        # the page's last change put back "Auth". A bounded wait on what the page holds.
        await settle_until(app, lambda: field.kept == "Auth")
        seen["kept"] = (nickname(), field.kept, len(page.changes))
        await highlight(pilot, app, "tools")
        await pilot.press("space")
        await settle(app)
        seen["folded"] = stored_group("tools").collapsed  # type: ignore[union-attr]
        app.sidebar.focus()
        await pilot.press("ctrl+z")
        await settle(app)
        seen["sidebar ctrl+z"] = (stored_group("tools").collapsed, nickname())  # type: ignore[union-attr]
        page.focus()
        await pilot.press("ctrl+z")
        await settle(app)
        seen["page ctrl+z"] = nickname()
        return seen

    seen = drive(go)
    assert seen["focused"] is True
    assert seen["kept"] == ("Auth", "Auth", 1), "one change on the page to take back"
    assert seen["folded"] is True
    assert seen["sidebar ctrl+z"] == (False, "Auth"), "the layout's undo, and only it"
    assert seen["page ctrl+z"] is None, "the page's own undo still takes its change back"


@pytest.mark.parametrize("field", ["card nickname", "agent label", "group name"])
def test_ctrl_z_typed_in_an_open_name_field_undoes_no_arrangement(
    tmp_path: Path, script: Script, field: str
) -> None:
    """Reviewer of #39 (#2993): an ``Input`` lets ctrl+z through to the sidebar's binding, and
    a group just deleted came back while a name was being typed. In a card's, an agent's or a
    group's name field, ctrl+z undoes no arrangement: the field stays open with what was
    typed, and the arrangement waits for ctrl+z from the sidebar itself."""
    seed(tmp_path, *THREE)
    script["prj_a"] = [status("prj_a", "coder-1", "coder", "working")]
    with store_session() as store:
        groups_service.create_group(store, "tools", ["prj_b"])
        groups_service.create_group(store, "old")

    async def go(pilot: Pilot[None]) -> tuple[set[str], object, set[str]]:
        app = fleet_app(pilot)
        menu = await menu_by_dot(pilot, app, "old")
        await delete_from_menu(pilot, app, menu)  # the arrangement ctrl+z would take back
        if field == "card nickname":
            app.sidebar.open_rename("prj_c")
        elif field == "agent label":
            assert app.sidebar.open_agent_rename("agt_a_coder-1") is None
        else:
            await highlight(pilot, app, "tools")
            await pilot.press("f2")
        await settle(app)
        focused = app.focused
        assert isinstance(focused, Input), type(focused).__name__
        await pilot.press(*"xy", "ctrl+z")
        await settle(app)
        typed = (app.focused is focused, focused.value.endswith("xy"))
        during = stored_names()
        await pilot.press("escape")
        await settle(app)
        app.sidebar.focus()
        await pilot.press("ctrl+z")
        await settle(app)
        return during, typed, stored_names()

    during, typed, after = drive(go)
    assert during == {"tools"}, "ctrl+z in the field brought no group back"
    assert typed == (True, True), "the field is still open, with what was typed"
    assert after == {"tools", "old"}, "ctrl+z from the sidebar still takes the delete back"


@pytest.mark.parametrize("field", ["group name", "card nickname"])
def test_ctrl_z_while_typing_a_name_leaves_a_fold_folded(
    tmp_path: Path, script: Script, field: str
) -> None:
    """The tester's repro of #39 (#3042, B1): web folded, then a name typed and ctrl+z
    pressed in the field. web stays folded, no 'undid' toast is raised, and the field still
    holds what was typed."""
    seed(tmp_path, *THREE)
    with store_session() as store:
        groups_service.create_group(store, "tools", ["prj_b"])
        groups_service.create_group(store, "web", ["prj_c"])

    async def go(pilot: Pilot[None]) -> tuple[bool | None, list[str], str]:
        app = fleet_app(pilot)
        told = toast_log.record(app)
        await highlight(pilot, app, "web")
        await pilot.press("space")
        await settle(app)
        assert stored_group("web").collapsed is True  # type: ignore[union-attr]
        if field == "group name":
            await highlight(pilot, app, "tools")
            await pilot.press("f2")
            typed = "infra"
        else:
            app.sidebar.open_rename("prj_a")
            typed = "Auth"
        await settle(app)
        focused = app.focused
        assert isinstance(focused, Input), type(focused).__name__
        await pilot.press(*typed, "ctrl+z")
        await settle(app)
        return stored_group("web").collapsed, told, focused.value  # type: ignore[union-attr]

    folded, told, value = drive(go)
    assert folded is True
    assert not [t for t in told if t.startswith("undid")], told
    assert value.endswith("infra" if field == "group name" else "Auth")


# --- parity with the CLI ---------------------------------------------------------------------

TUI_REACHES = {
    "project group create": "the group picker's + New group… (g on a card, shift+g on marks)",
    "project group rename": "the group menu's Rename…, or F2 on a group header",
    "project group delete": "the group menu's Delete group",
    "project group add": "the group picker's groups, or a card dragged onto a header or its "
    "empty line",
    "project group remove": "the group picker's Ungroup, or a card dragged below the list",
    "project group move": "the group menu's Move up / Move down, shift+↑ ↓ on a header, or a "
    "header dragged",
    "project pin": "p on a card, or the card menu's Pin",
    "project unpin": "p on a pinned card, or the card menu's Unpin",
    "project move": "shift+↑ ↓ on a card, a card dragged, or the group picker",
}
"""Every arranging command the CLI has, and the TUI's way to it (#39, the owner's (3))."""

SHOWN_BY_THE_SIDEBAR = {"project group list": "the sidebar itself lists the groups"}

CLI_ONLY = {"project group parent", "project group route", "project group cap"}
"""A group's parent, its routing and its cap: settings, not arrangement; CLI-only by the
owner's word (#39)."""

TUI_GESTURES = {
    "picker": {NEW_GROUP, UNGROUP},
    "group menu": {RENAME, PIN, COLLAPSE, MOVE_UP, MOVE_DOWN, DELETE},
    "sidebar keys": {
        "group_picker",
        "group_marked",
        "toggle_pin",
        "move_up",
        "move_down",
        "rename",
        "project_menu",
        "undo_layout",
    },
}
"""The TUI gestures the table above names: the picker's answers, the group menu's, and the
sidebar's actions (asserted below to exist)."""


def _commands(typer_app: object, prefix: str) -> set[str]:
    registered = getattr(typer_app, "registered_commands")  # noqa: B009
    return {f"{prefix} {c.name or c.callback.__name__}" for c in registered}


def test_every_arranging_command_the_cli_has_is_reachable_in_the_tui() -> None:
    """Parity: the CLI's ``project group`` commands and its project pin / unpin / move, each
    mapped to the TUI's way to it, or named CLI-only. A new CLI command fails here until it
    is placed."""
    group_commands = _commands(project_cli.group_app, "project group")
    project_commands = {
        name
        for name in _commands(project_cli.app, "project")
        if name in {"project pin", "project unpin", "project move"}
    }
    cli = group_commands | project_commands
    placed = set(TUI_REACHES) | set(SHOWN_BY_THE_SIDEBAR) | CLI_ONLY
    assert cli == placed, (sorted(cli - placed), sorted(placed - cli))
    assert {"create", "rename", "delete", "add", "remove", "move"} <= {
        name.rsplit(" ", 1)[1] for name in TUI_REACHES if name.startswith("project group")
    }
    assert not CLI_ONLY & set(TUI_REACHES)

    actions = {
        binding.action.split("(")[0]
        for binding in Sidebar.BINDINGS
        if not isinstance(binding, tuple)
    }
    assert TUI_GESTURES["sidebar keys"] <= actions, sorted(TUI_GESTURES["sidebar keys"] - actions)
    keys = {b.key for b in Sidebar.BINDINGS if not isinstance(b, tuple)}
    assert "u,ctrl+z" in keys and "full_stop" in keys and "f2" in keys


def test_the_group_menu_and_the_picker_answer_every_gesture_the_parity_table_names(
    tmp_path: Path,
) -> None:
    """The table's TUI side is real: the group menu (both moves offered) answers each of its
    gestures, Delete group by its last row, and the picker offers New group… and Ungroup."""

    class Host(App[None]):
        def compose(self) -> ComposeResult:
            yield Static()

    async def run() -> tuple[set[str], set[str]]:
        host = Host()
        async with host.run_test() as pilot:
            group = ProjectGroup(id="grp_x", name="x", created_at=datetime.now(UTC))
            menu = GroupMenu(group, Region(0, 0, 20, 1), count=0, up=True, down=True)
            await host.push_screen(menu)
            await pilot.pause()
            answers = set(entries(menu)) | {menu.LAST}
            menu.dismiss(None)
            await pilot.pause()
            picker = GroupPicker([group], 1)
            await host.push_screen(picker)
            await pilot.pause()
            options = picker.query_one(OptionList)
            offered = {str(options.get_option_at_index(i).id) for i in range(options.option_count)}
            return answers, offered

    answers, offered = asyncio.run(run())
    assert answers == TUI_GESTURES["group menu"]
    assert TUI_GESTURES["picker"] <= offered and "group:grp_x" in offered
