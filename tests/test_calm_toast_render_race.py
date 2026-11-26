"""A calm toast drawn while it has no component styles (#36).

``CalmToast.render_lines`` draws its dismiss glyph (``✕``, or a copy's cue) in the toast's
own component style. Textual's ``Widget._remove`` empties a widget's component styles, and a
``refresh()`` (or a style update) empties its cached ones, so a toast just taken from its rack
and refreshed had no style to give the glyph. The next frame that drew it, from the screen's
last map, raised ``KeyError: "No 'calm-toast--dismiss' key in COMPONENT_CLASSES"`` mid-frame.
That KeyError reddened L2's full matrix once (run 45717280636, py3.12 shard 4/4, in
``test_ui_pause_resume.py``). This forces that state rather than waiting for the race: the
toast is drawn once, taken from its rack, refreshed, and drawn again, as that frame does.
"""

from __future__ import annotations

import asyncio

import pytest
from rich.style import Style
from textual.geometry import Region
from textual.pilot import Pilot
from textual.widget import Widget

from aisquare.cli.ui.toasts import CUES, CalmToast
from aisquare.core import clipboard
from tests.test_calm_toasts import Host, settle, shown


def glyph_style(toast: CalmToast, glyph: str) -> Style | None:
    """The style ``glyph`` is drawn in on the toast's first row, as a frame draws it."""
    size = toast.outer_size
    (row, *_) = toast.render_lines(Region(0, 0, size.width, size.height))
    drawn = [segment for segment in row if segment.text == glyph]
    assert drawn, ("no glyph drawn", glyph, "".join(segment.text for segment in row))
    return drawn[-1].style


async def one_toast(app: Host, pilot: Pilot[None], cue: str | None) -> CalmToast:
    app.notify("the agent's pane is gone", timeout=30)
    await settle(pilot)
    (toast,) = shown(app)
    if cue is not None:
        toast.cue(clipboard.CopyResult(cue, "osc52"))  # type: ignore[arg-type]
    return toast


@pytest.mark.parametrize("cue", [None, "copied", "failed"])
def test_a_toast_taken_from_its_rack_and_refreshed_still_draws_its_glyph(cue: str | None) -> None:
    """The race, forced: the frame that draws a toast just taken from its rack (its component
    styles emptied by the removal, its cached ones by a refresh) draws the glyph in the cell's
    own style. Before #36 it raised KeyError for each of the three parts."""
    glyph = CUES[cue] if cue else "✕"  # as render_lines picks it

    async def run() -> tuple[int, int, bool]:
        app = Host()
        async with app.run_test(size=(90, 30), notifications=True) as pilot:
            await pilot.pause()
            toast = await one_toast(app, pilot, cue)
            glyph_style(toast, glyph)  # drawn once: its style is cached
            holder = toast.parent
            assert isinstance(holder, Widget)
            await holder.remove()
            toast.refresh()
            styles, cached = len(toast._component_styles), len(toast._rich_style_cache)
            return styles, cached, glyph_style(toast, glyph) is not None

    styles, cached, drawn = asyncio.run(run())
    assert (styles, cached) == (0, 0), "the state the frame met: no component style at all"
    assert drawn


def test_the_frame_after_draws_the_glyph_in_its_own_colour_again() -> None:
    """The fallback is for that one frame: Textual does not cache a ``default``, so once the
    stylesheet fills the component styles again the glyph has its own colour back."""

    async def run() -> tuple[Style | None, Style | None, Style | None]:
        app = Host()
        async with app.run_test(size=(90, 30), notifications=True) as pilot:
            await pilot.pause()
            toast = await one_toast(app, pilot, None)
            own = glyph_style(toast, "✕")
            # What the removal leaves (``Widget._remove`` clears this very dict), on a toast
            # still on screen, then a refresh: the frame between has no component style.
            toast._component_styles.clear()
            toast.refresh()
            fallback = glyph_style(toast, "✕")
            app.stylesheet.apply(toast)  # the component pass, as a mount or a CSS refresh runs it
            return own, fallback, glyph_style(toast, "✕")

    own, fallback, after = asyncio.run(run())
    assert own is not None and own.color is not None
    assert fallback is not None and fallback.color != own.color, "the cell's style that frame"
    assert after == own, "its own colour once the styles are back"
