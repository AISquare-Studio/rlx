"""Each agent's mark reads at 3:1 or more on the surface it is drawn on, theme by theme (R16-C).

The R16-C verify (seq 11914) measured the marks as drawn and found the picker's below 3:1 on
nine built-in themes: the pin then measured each colour against the theme's ``background``,
while the picker draws its rows on the list's own surface, and the page blends a Rich colour
into its background at the theme's text alpha. One colour per agent cannot read on every
theme's surfaces (catppuccin-frappe's picker needs a lighter ink than textual-light's page
allows), so each mark has a colour for a dark theme and one for a light theme
(``AgentMark.on_dark``, ``on_light``), of one hue.

Here every built-in theme and our own ``rlx`` (#139), but the two ``ansi`` ones (they paint the
terminal's own background, which no test can know), is put on screen, after the page has
painted, so the repaint a theme change brings is the one measured: the Accounts page and the
sidebar rows of a fleet that mixes agents, then the account picker with each agent's row off the
highlight.
Every mark cell's ink is measured against the background drawn under it (WCAG's ratio).
"""

from __future__ import annotations

import asyncio
import shutil
from collections.abc import Iterable
from pathlib import Path
from typing import Any

import pytest
from rich.style import Style
from textual.app import App
from textual.theme import BUILTIN_THEMES
from textual.widgets import OptionList

from aisquare.cli.ui.app import FleetApp
from aisquare.cli.ui.attach import AttachTargetScreen
from aisquare.cli.ui.sidebar import CUSTOM_ROLE_ICON, ROLE_ICON, STATE_CHIP
from aisquare.cli.ui.theme import RLX_THEME, register_themes
from aisquare.core.orchestrator import team_project
from aisquare.models import FleetAgent, FleetAgentStatus
from aisquare.services import claude_accounts as accounts_service
from aisquare.services import fleet as fleet_service
from tests.test_ui_accounts import NOW, _mixed_frame, _table_usage, open_accounts, painted
from tests.test_ui_target_picker_agents import mixed as picker_frame
from tests.ui_workers import settle_page, settle_until, settle_until_held

#: The themes whose background is the terminal's own: not measured, and the only ones not.
TERMINALS = frozenset({"ansi-dark", "ansi-light"})
THEMES = sorted((set(BUILTIN_THEMES) | {RLX_THEME.name}) - TERMINALS)
"""Every built-in theme and our own, ``rlx`` (the default since #139), but the terminals'."""
FLOOR = 3.0
"""WCAG 2's ratio for a graphical object that must be told apart (1.4.11)."""


def test_every_built_in_theme_is_measured_but_the_terminals_own() -> None:
    assert set(BUILTIN_THEMES) >= TERMINALS and len(THEMES) >= 20
    assert {"rlx", "textual-dark", "textual-light", "catppuccin-frappe", "nord"} <= set(THEMES)


def _luminance(rgb: tuple[int, int, int]) -> float:
    def channel(value: int) -> float:
        share = value / 255
        return share / 12.92 if share <= 0.03928 else ((share + 0.055) / 1.055) ** 2.4

    red, green, blue = rgb
    return 0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)


def contrast(one: tuple[int, int, int], other: tuple[int, int, int]) -> float:
    light, dark = sorted((_luminance(one), _luminance(other)), reverse=True)
    return (light + 0.05) / (dark + 0.05)


Seen = list[tuple[str, int, float]]
"""Each mark cell drawn: its glyph, its x and its ink's ratio to the background under it."""


def _rgb(style: Style, attribute: str) -> tuple[int, int, int]:
    colour = getattr(style, attribute)
    assert colour is not None, f"a mark drawn with no {attribute}: {style}"
    triplet = colour.get_truecolor()
    return (triplet.red, triplet.green, triplet.blue)


def marks_drawn(app: App[Any]) -> Seen:
    """Every ``✻`` and ``>_`` on screen in a colour of its own: a row under a highlight bar
    draws its mark in the bar's colours (bold), which is the bar's to read, not the mark's."""
    seen: Seen = []
    for (strip,) in app.screen._compositor.render_full_update(simplify=True).strips:
        cells = [(char, segment.style) for segment in strip for char in segment.text]
        for x, (char, style) in enumerate(cells):
            glyph = "✻" if char == "✻" else ">_" if cells[x : x + 2] and char == ">" else None
            if glyph == ">_" and (x + 1 >= len(cells) or cells[x + 1][0] != "_"):
                glyph = None
            if glyph is None or style is None or style.bold:
                continue
            seen.append((glyph, x, contrast(_rgb(style, "color"), _rgb(style, "bgcolor"))))
    return seen


def _fleet(project: str) -> list[FleetAgentStatus]:
    def row(label: str, agent: str, slot: int) -> FleetAgentStatus:
        return FleetAgentStatus(
            agent=FleetAgent(
                id=f"agt_{label}",
                project_id=project,
                label=label,
                role="coder",
                pane_id="%4",
                cwd=Path("/tmp/proj"),
                created_at=NOW,
                account_slot=slot,
                agent=agent,
            ),
            state="working",
        )

    return [row("coder-claude", "claude-code", 1), row("coder-codex", "codex", 4)]


class PickerHost(App[None]):
    def __init__(self, project: Any) -> None:
        super().__init__()
        register_themes(self)  # rlx, as both of our apps offer it
        self.project = project

    def on_mount(self) -> None:
        self.push_screen(
            AttachTargetScreen(self.project, persona=None, intent="new", accounts=picker_frame)
        )


def _below(seen: Iterable[tuple[str, int, float]], where: str) -> list[str]:
    return [f"{where} {glyph} at x={x}: {ratio:.2f}:1" for glyph, x, ratio in seen if ratio < FLOOR]


@pytest.mark.parametrize("theme", THEMES)
def test_each_mark_reads_at_three_to_one_on_the_surface_it_is_drawn_on(
    theme: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(accounts_service, "usage", lambda account, **_: _table_usage(account))
    monkeypatch.setattr(fleet_service, "list_agents", lambda project, **_: [])
    monkeypatch.setattr(shutil, "which", lambda name, *a, **k: f"/usr/bin/{name}")
    project = team_project(tmp_path)

    async def page_and_sidebar() -> tuple[Seen, int]:
        app = FleetApp(refresh_seconds=3600, doctor=lambda: [], accounts=_mixed_frame)
        async with app.run_test(size=(200, 50)) as pilot:
            await settle_page(app)
            view = await open_accounts(pilot)
            await settle_until(app, lambda: painted(view, 1, 2, 4))
            app.sidebar.show_projects([project], {project.id: _fleet(project.id)})
            await settle_page(app)
            app.theme = theme  # after the paint: the repaint a theme change brings is measured
            await settle_page(app)
            return marks_drawn(app), app.sidebar.outer_size.width

    async def picker() -> tuple[Seen, Seen]:
        host = PickerHost(project)
        async with host.run_test(size=(120, 40)):
            await settle_page(host)
            host.theme = theme
            listing = host.screen.query_one("#picker-list", OptionList)
            await settle_until_held(
                host, lambda: listing.get_option("account:5") is not None, what="the rows"
            )
            first = marks_drawn(host)  # the Claude row under the bar: the Codex row's mark
            listing.highlighted = listing.get_option_index("account:5")
            await settle_page(host)
            return first, marks_drawn(host)  # and now the Claude row's

    (on_page, sidebar_width), (picker_first, picker_moved) = (
        asyncio.run(page_and_sidebar()),
        asyncio.run(picker()),
    )
    sidebar = [mark for mark in on_page if mark[1] < sidebar_width]
    page = [mark for mark in on_page if mark[1] >= sidebar_width]
    picked = picker_first + picker_moved
    # Every surface drew both agents' marks: a measurement of nothing passes nothing.
    for where, seen in (("page", page), ("sidebar", sidebar), ("picker", picked)):
        assert {glyph for glyph, _, _ in seen} == {"✻", ">_"}, (theme, where, seen)
    below = _below(page, "page") + _below(sidebar, "sidebar") + _below(picked, "picker")
    assert not below, f"{theme}: {below}"


TEXT_FLOOR = 4.5
"""WCAG 2's ratio for text (1.4.3): a state's mark, Spawn agent and an agent's buttons (M8)."""


def text_cells(app: App[Any], widget: Any) -> list[tuple[str, float]]:
    """Every character drawn in ``widget``'s region, with its ink's ratio to the ground under it.

    The column moves by each character's cells: an emoji takes two, and counted as one it
    shifted the rest of the line left, the sidebar's divider into a row with two of them."""
    from rich.cells import cell_len

    region = widget.region
    seen = []
    for y in range(region.y, region.bottom):
        (strip,) = app.screen._compositor.render_full_update(simplify=True).strips[y]
        x = 0
        for segment in strip:
            for char in segment.text:
                style = segment.style
                if region.x <= x < region.right and char.strip() and style is not None:
                    ratio = contrast(_rgb(style, "color"), _rgb(style, "bgcolor"))
                    seen.append((char, ratio))
                x += cell_len(char)
    return seen


def _every_state(project: str, *, worded: bool = False) -> list[FleetAgentStatus]:
    """An agent in each state; ``worded``: each on account slot 1 and the exited one with its
    status, so the rows say ``#1``, ``paused`` and ``exited(0)`` beside their marks."""
    from aisquare.cli.ui.sidebar import STATE_CHIP

    return [
        FleetAgentStatus(
            agent=FleetAgent(
                id=f"agt_{state}",
                project_id=project,
                label=f"coder-{place}",
                role="coder",
                pane_id=f"%{place + 1}",
                cwd=Path("/tmp/proj"),
                created_at=NOW,
                tmux_socket="asq-test-mark-contrast",
                account_slot=1 if worded else None,
                exit_status=0 if worded and state == "exited" else None,
            ),
            state=state,
        )
        for place, state in enumerate(STATE_CHIP)
    ]


@pytest.mark.parametrize("theme", THEMES)
def test_each_state_mark_and_spawn_agent_read_at_four_and_a_half_to_one(
    theme: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """M8 (the UX audit) and 0.10.1's exploratory test: every theme the app offers draws each
    state's mark, and *Spawn agent*, at 4.5:1 or more on the row under it. Rich's named colours
    fell to 1.96:1 (yellow on textual-light), and Spawn agent's $accent to 1.49:1 there."""
    from aisquare.cli.ui.sidebar import STATE_CHIP, ActionRow, AgentRow

    monkeypatch.setattr(fleet_service, "list_agents", lambda project, **_: [])
    monkeypatch.setattr(shutil, "which", lambda name, *a, **k: f"/usr/bin/{name}")
    project = team_project(tmp_path)

    async def drawn() -> tuple[dict[str, list[float]], list[float]]:
        app = FleetApp(refresh_seconds=3600, doctor=lambda: [])
        async with app.run_test(size=(160, 50)):
            await settle_page(app)
            app.sidebar.show_projects([project], {project.id: _every_state(project.id)})
            app.theme = theme  # after the paint: the repaint a theme change brings is measured
            await settle_page(app)
            marks: dict[str, list[float]] = {}
            for row in app.sidebar.query(AgentRow):
                glyph = STATE_CHIP[row.status.state][0]
                marks[row.status.state] = [r for c, r in text_cells(app, row) if c == glyph]
            spawn = [r for row in app.sidebar.query(ActionRow) for _, r in text_cells(app, row)]
            return marks, spawn

    marks, spawn = asyncio.run(drawn())
    assert set(marks) == set(STATE_CHIP) and all(marks.values()), (theme, marks)
    assert spawn, f"{theme}: no Spawn agent drawn: a measurement of nothing passes nothing"
    below = {state: min(seen) for state, seen in marks.items() if min(seen) < TEXT_FLOOR}
    assert not below, f"{theme}: state marks under {TEXT_FLOOR}:1: {below}"
    assert min(spawn) >= TEXT_FLOOR, f"{theme}: Spawn agent at {min(spawn):.2f}:1"


@pytest.mark.parametrize("theme", THEMES)
def test_an_agents_bar_reads_at_four_and_a_half_to_one(
    theme: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    """M8: Pause, Resume, Stop and Restart, every label on its fill at 4.5:1 or more, on every
    theme (catppuccin-latte's Pause was 2.32:1, solarized's Restart 3.41:1). Under the shell's
    own CSS (``RLX_CSS``), as ``FleetApp`` adds it."""
    from textual.app import ComposeResult
    from textual.widgets import Button

    from aisquare.cli.ui.theme import RLX_CSS, theme_variable_defaults
    from aisquare.cli.ui.views.agent import AgentView
    from aisquare.core import tmux as tmux_core
    from aisquare.core.tmux import Completed, TmuxServer

    # No server answers: the pane says so and the bar is drawn all the same.
    monkeypatch.setattr(tmux_core, "_tmux", lambda argv, stdin: Completed(1, "", "no server\n"))
    socket = "asq-test-mark-contrast"
    agent = FleetAgent(
        id="agt_bar", project_id="prj_bar", label="coder-1", role="coder", pane_id="%1",
        cwd=Path("/tmp/proj"), created_at=NOW, tmux_socket=socket,
    )  # fmt: skip

    class Host(App[None]):
        CSS = RLX_CSS

        def __init__(self, state: str) -> None:
            super().__init__()
            register_themes(self)
            self.state = state

        def get_theme_variable_defaults(self) -> dict[str, str]:
            return {**super().get_theme_variable_defaults(), **theme_variable_defaults(self)}

        def compose(self) -> ComposeResult:
            status = FleetAgentStatus(agent=agent, state=self.state)
            yield AgentView(status, server=TmuxServer(socket), escape_key="f12")

    async def bar(state: str) -> dict[str, float]:
        host = Host(state)
        async with host.run_test(size=(120, 20)):
            host.theme = theme
            host.screen.set_focus(None)  # at rest: no button focused or hovered
            await settle_page(host)
            return {
                f"{button.id} ({button.variant})": min(r for _, r in text_cells(host, button))
                for button in host.query(Button)
                if button.display and button.region.area
            }

    seen = {**asyncio.run(bar("working")), **asyncio.run(bar("paused"))}
    assert {"agent-pause (warning)", "agent-resume (primary)", "agent-stop (default)"} <= set(
        seen
    ), seen
    below = {name: round(ratio, 2) for name, ratio in seen.items() if ratio < TEXT_FLOOR}
    assert not below, f"{theme}: agent bar labels under {TEXT_FLOOR}:1: {below}"


@pytest.mark.parametrize("theme", ["rlx", "textual-dark", "textual-light", "solarized-light"])
def test_paused_is_the_pause_mark_and_waiting_a_muted_one_not_yellow(
    theme: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Q4 (the UX audit): one meaning per mark. A paused row draws ⏸, as Pause's own button and
    menu entry do; a waiting row, an agent at rest at its prompt, draws ``◦`` in the theme's
    muted tone, where it drew a yellow ⏸ (and paused a ⏯)."""
    from textual.color import Color

    from aisquare.cli.ui.sidebar import AgentRow
    from aisquare.cli.ui.theme import theme_tones

    monkeypatch.setattr(fleet_service, "list_agents", lambda project, **_: [])
    project = team_project(tmp_path)

    async def drawn() -> dict[str, list[tuple[str, tuple[int, int, int]]]]:
        app = FleetApp(refresh_seconds=3600, doctor=lambda: [])
        async with app.run_test(size=(160, 50)):
            await settle_page(app)
            app.sidebar.show_projects([project], {project.id: _every_state(project.id)})
            app.theme = theme
            await settle_page(app)
            tones = theme_tones(app.current_theme)
            seen: dict[str, list[tuple[str, tuple[int, int, int]]]] = {"tones": []}
            seen["tones"] = [(tone, Color.parse(tones[tone]).rgb) for tone in ("muted", "warning")]
            strips = app.screen._compositor.render_full_update(simplify=True).strips
            for row in app.sidebar.query(AgentRow):
                if row.status.state not in ("waiting", "paused"):
                    continue
                (strip,) = strips[row.region.y]
                cells = [(c, seg.style) for seg in strip for c in seg.text]
                seen[row.status.state] = [
                    (char, _rgb(style, "color"))
                    for char, style in cells[row.region.x : row.region.right]
                    if char in "◦⏸⏯" and style is not None
                ]
            return seen

    seen = asyncio.run(drawn())
    (_muted, warning) = (rgb for _, rgb in seen["tones"])
    assert [char for char, _ in seen["paused"]] == ["⏸"], seen["paused"]
    assert [char for char, _ in seen["waiting"]] == ["◦"], seen["waiting"]
    ((_, ink),) = seen["waiting"]
    # A grey (the muted tone, lifted where it must read as well as 0.10.1's yellow did), never
    # the theme's warning colour nor any yellow.
    assert max(ink) - min(ink) <= 16 and ink != warning, (theme, ink, warning)


Ratios = dict[str, float]
"""What was measured, by name, at its worst ratio."""

FADED_TEXT_FLOOR = 3.0
"""Text on the side that does not hold the keyboard, drawn at ``FADE_OPACITY`` (the manager's
call on #6): the fade marks it unfocused, and it stays clickable."""
MARK_FLOOR = 3.0
"""A state's mark anywhere, the selected row's highlight bar and the faded side included
(WCAG 1.4.11, the manager's call on #6). Where the keyboard is, an unselected row's mark is
held to text's 4.5:1 too, as the first test of the marks and Spawn agent measures it."""
TEXT_MARKS = {mark for mark, _ in STATE_CHIP.values()} - {"🔔", "⏳", "💤"}
"""The marks drawn as text, in a colour of ours: the emoji ones keep their own colours."""
EMOJI = {"🔔", "⏳", "💤", *ROLE_ICON.values(), CUSTOM_ROLE_ICON}
"""What a row draws in the emoji's own colours: every other cell is ink of ours."""


@pytest.mark.parametrize("theme", THEMES)
def test_the_sidebar_and_the_agent_bar_read_where_the_keyboard_is_and_where_it_is_not(
    theme: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """M8 under the focus fade, and on the selected row (#6's tester, board #240): with an
    agent's view open, measured with the keyboard on the sidebar and then on the agent's pane.

    - Each text mark, on its row and on the highlight bar with that row selected: 3:1 or more
      on either side (the bar drew catppuccin-latte's ▶ at 1.01:1).
    - Every other cell of a selected row but its emoji, its words beside the mark included
      (``#1``, ``paused``, ``exited(0)``): text's 4.5:1 where the keyboard is, 3:1 faded (the
      bar drew the words muted, 1.02:1 to 1.81:1, #6's tester, round 2).
    - Spawn agent, the ``+`` beside Fleet and the line under ⚙ Settings: 4.5:1 or more where the
      keyboard is, 3:1 on the faded side (the ``+`` was 2.65:1, the line's words 2.80:1).
    - The agent's bar: 4.5:1 or more with the keyboard on its pane, 3:1 faded (Restart was
      3.16:1 there on catppuccin-latte, Spawn agent 3.71:1 faded).
    """
    from textual.widgets import Button

    from aisquare.cli.ui.sidebar import (
        STATE_CHIP,
        ActionRow,
        AddButton,
        AgentRow,
        StatusLine,
    )
    from aisquare.cli.ui.views.agent import AgentView
    from aisquare.core import tmux as tmux_core
    from aisquare.core.store import store_session
    from aisquare.core.tmux import Completed
    from aisquare.models import ProjectInfo
    from tests.fleet_fakes import list_agents_as

    monkeypatch.setattr(tmux_core, "_tmux", lambda argv, stdin: Completed(1, "", "no server\n"))
    monkeypatch.setattr(shutil, "which", lambda name, *a, **k: f"/usr/bin/{name}")
    with store_session() as store:
        project = store.onboard_project(ProjectInfo(id="prj_c", root=tmp_path / "alpha"))
    rows = _every_state(project.id, worded=True)
    list_agents_as(monkeypatch, lambda listed, **_: rows if listed.id == project.id else [])

    async def side(app: FleetApp) -> Ratios:
        """The sidebar as drawn now: each row's mark, then each mark on the bar, the texts."""
        seen: Ratios = {}
        agent_rows = list(app.sidebar.query(AgentRow))
        for row in agent_rows:
            glyph = STATE_CHIP[row.status.state][0]
            if glyph in TEXT_MARKS and not row.has_class("selected"):
                seen[f"mark {glyph}"] = min(r for c, r in text_cells(app, row) if c == glyph)
        for row in agent_rows:
            state = row.status.state
            glyph = STATE_CHIP[state][0]
            app.sidebar.select(row.selection_key)
            await settle_page(app)
            assert row.has_class("selected"), row.selection_key
            cells = text_cells(app, row)
            if glyph in TEXT_MARKS:
                seen[f"selected mark {glyph}"] = min(r for c, r in cells if c == glyph)
            words = [r for c, r in cells if c != glyph and c not in EMOJI]
            assert words, f"the {state} row drew no words: a measurement of nothing"
            seen[f"selected words {state}"] = min(words)
        for name, widgets in (
            ("Spawn agent", app.sidebar.query(ActionRow)),
            ("+", app.sidebar.query(AddButton)),
            ("status line", app.sidebar.query(StatusLine)),
        ):
            # The line's words, figure and ⚠; a band's dot is its band's colour (ΔE 35 apart).
            drawn = [
                r
                for widget in widgets
                for char, r in text_cells(app, widget)
                if not (name == "status line" and char == "●")
            ]
            assert drawn, f"{name} not drawn: a measurement of nothing passes nothing"
            seen[name] = min(drawn)
        return seen

    def bar(app: FleetApp) -> Ratios:
        view = app.query_one(AgentView)
        return {
            f"bar {button.id}": min(r for _, r in text_cells(app, button))
            for button in view.query(Button)
            if button.display and button.region.area
        }

    async def drawn() -> tuple[Ratios, Ratios]:
        app = FleetApp(refresh_seconds=3600, doctor=lambda: [])
        async with app.run_test(size=(160, 50)):
            await settle_page(app)
            app.theme = theme
            await settle_page(app)
            working = next(r for r in app.sidebar.query(AgentRow) if r.status.state == "working")
            working.activate()  # its view, with its bar
            await settle_until(app, lambda: bool(app.query(AgentView)))
            app.sidebar.focus()
            await settle_page(app)
            assert app.content.has_class("-unfocused"), "the keyboard is on the sidebar"
            on_sidebar = {**await side(app), **bar(app)}
            app.query_one(AgentView).pane.focus()
            await settle_page(app)
            assert app.sidebar.has_class("-unfocused"), "the keyboard is on the pane"
            on_pane = {**await side(app), **bar(app)}
            return on_sidebar, on_pane

    on_sidebar, on_pane = asyncio.run(drawn())

    def floor(key: str, keyboard_here: bool) -> float:
        if key.startswith("selected mark") or (key.startswith("mark") and not keyboard_here):
            return MARK_FLOOR
        return TEXT_FLOOR if keyboard_here else FADED_TEXT_FLOOR

    below = [
        f"{where}: {key} {ratio:.2f}:1 < {floor(key, here)}"
        for where, seen, bar_here in (
            ("keyboard on the sidebar", on_sidebar, False),
            ("keyboard on the pane", on_pane, True),
        )
        for key, ratio in seen.items()
        for here in [bar_here if key.startswith("bar") else not bar_here]
        if ratio < floor(key, here)
    ]
    assert any(key.startswith("bar") for key in on_sidebar), on_sidebar
    assert not below, f"{theme}: {below}"


@pytest.mark.parametrize("theme", THEMES)
def test_no_state_mark_reads_worse_than_0_10_1_drew_it(theme: str) -> None:
    """The manager's call on #6: a state's mark never reads worse than base (0.10.1's style,
    ``sidebar.DRAWN_BEFORE``, as Textual draws it through the app's ANSI theme), on either of
    the sidebar's grounds; and at 4.5:1 or more there. Waiting's muted ◦ reads at least as well
    as the yellow ⏸ it replaced (Q4)."""
    from textual.color import Color

    from aisquare.cli.ui.sidebar import DRAWN_BEFORE, STATE_CHIP, state_styles
    from aisquare.cli.ui.theme import GROUNDS, contrast_ratio, drawn_ink

    host = App[None]()
    register_themes(host)
    shown = host.get_theme(theme)
    assert shown is not None
    terminal = host.ansi_theme_dark if shown.dark else host.ansi_theme_light
    colours = shown.to_color_system().generate()
    grounds = [Color.parse(colours[ground]) for ground in GROUNDS]
    foreground = Color.parse(colours["foreground"])
    styles = state_styles(shown, terminal)
    worse = []
    for state in STATE_CHIP:
        if STATE_CHIP[state][0] not in TEXT_MARKS:
            continue  # an emoji keeps its own colours
        for ground in grounds:
            now = contrast_ratio(drawn_ink(styles[state], terminal, foreground, ground), ground)
            then = contrast_ratio(
                drawn_ink(DRAWN_BEFORE[state], terminal, foreground, ground), ground
            )
            if now < max(TEXT_FLOOR, then) - 0.01:
                worse.append(f"{state} on {ground.hex}: {now:.2f} (0.10.1: {then:.2f})")
    assert not worse, f"{theme}: {worse}"
