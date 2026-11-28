"""The tray's glyph (#34): rlx's backslash from the logo's own path, drawn at a menu bar's sizes
in its blue and as a template, and the whole r\\x mark (``core.glyph``)."""

from __future__ import annotations

import re
import struct
import zlib
from pathlib import Path

import pytest

from aisquare.core import glyph

LOGO = Path(__file__).resolve().parents[1] / "docs" / "assets" / "rlx-logo.svg"


def _logo_path() -> str:
    found = re.search(r'<path fill="#2F74F0" d="([^"]+)"', LOGO.read_text(encoding="utf-8"))
    assert found, "the logo holds one blue <path>"
    return found.group(1)


def test_the_paths_are_the_logos_own() -> None:
    """Pinned byte for byte: a redrawn logo moves the tray's icon with it, or fails here."""
    assert _logo_path() == glyph.MARK_PATH
    assert f" {glyph.BACKSLASH_PATH} " in f" {_logo_path()} "
    view_box = re.search(r'viewBox="0 0 ([\d.]+) ([\d.]+)"', LOGO.read_text(encoding="utf-8"))
    assert view_box and tuple(map(float, view_box.groups())) == glyph.VIEW_BOX


def test_the_blue_is_the_logos() -> None:
    assert "#{:02X}{:02X}{:02X}".format(*glyph.BLUE) == "#2F74F0"
    assert 'fill="#2F74F0"' in LOGO.read_text(encoding="utf-8")


def _ink(image: glyph.Image) -> list[tuple[int, int]]:
    return [(x, y) for y, row in enumerate(image.alpha) for x, a in enumerate(row) if a > 127]


@pytest.mark.parametrize("height", [16, 18, 22, 24, 32, 36, 48])
def test_the_backslash_is_drawn_at_each_size_falling_left_to_right(height: int) -> None:
    image = glyph.icon("backslash", height)
    assert (image.width, image.height) == (height, height)
    ink = _ink(image)
    assert ink, "something is drawn"
    top = [x for x, y in ink if y < height / 3]
    bottom = [x for x, y in ink if y > 2 * height / 3]
    assert top and bottom
    assert sum(top) / len(top) < sum(bottom) / len(bottom), "a backslash: \\ not /"
    # Within the margin: nothing on the canvas's edge rows.
    assert all(a == 0 for a in image.alpha[0]) and all(a == 0 for a in image.alpha[-1])
    # A solid stroke somewhere, and soft edges: coverage, not a two-tone bitmap.
    values = {a for row in image.alpha for a in row}
    assert 255 in values and any(0 < a < 255 for a in values)


def test_the_colours_blue_and_template() -> None:
    blue = glyph.icon("backslash", 18)
    template = glyph.icon("template", 18)
    assert blue.colour == glyph.BLUE and template.colour == glyph.BLACK
    assert blue.alpha == template.alpha, "the template is the same glyph, painted by macOS"
    rgba = blue.rgba()
    assert len(rgba) == 18 * 18 * 4
    x, y = _ink(blue)[0]
    offset = (y * 18 + x) * 4
    assert tuple(rgba[offset : offset + 3]) == glyph.BLUE


def test_the_argb_rows_are_what_a_status_notifier_item_reads() -> None:
    image = glyph.icon("backslash", 22)
    argb = image.argb()
    assert len(argb) == 22 * 22 * 4
    x, y = _ink(image)[0]
    offset = (y * 22 + x) * 4
    assert argb[offset] == image.alpha[y][x], "alpha first, in network order"
    assert tuple(argb[offset + 1 : offset + 4]) == glyph.BLUE


def test_the_png_decodes_to_the_same_pixels() -> None:
    image = glyph.icon("backslash", 18)
    data = image.png()
    assert data.startswith(b"\x89PNG\r\n\x1a\n")
    width, height, depth, colour_type = struct.unpack(">IIBB", data[16:26])
    assert (width, height, depth, colour_type) == (18, 18, 8, 6)
    idat_at = data.index(b"IDAT")
    (length,) = struct.unpack(">I", data[idat_at - 4 : idat_at])
    raw = zlib.decompress(data[idat_at + 4 : idat_at + 4 + length])
    rows = [raw[y * (18 * 4 + 1) : (y + 1) * (18 * 4 + 1)] for y in range(18)]
    assert all(row[0] == 0 for row in rows), "no filter"
    assert b"".join(row[1:] for row in rows) == image.rgba()


def test_the_whole_mark_is_as_wide_as_the_logo() -> None:
    image = glyph.icon("mark", 18)
    assert image.height == 18
    assert image.width == round(18 * glyph.VIEW_BOX[0] / glyph.VIEW_BOX[1] + 0.5)
    columns = {x for x, _ in _ink(image)}
    # r, \ and x: three runs of inked columns with gaps between them.
    runs = sum(1 for x in sorted(columns) if x - 1 not in columns)
    assert runs >= 3, sorted(columns)


def test_a_path_with_a_command_it_does_not_draw_is_refused() -> None:
    with pytest.raises(glyph.PathError):
        glyph.outlines("M0 0 C1 1 2 2 3 3Z")
    with pytest.raises(ValueError):
        glyph.draw(glyph.BACKSLASH_PATH, 0)


def test_an_icon_name_it_does_not_know_is_the_backslash() -> None:
    assert glyph.icon_named("mark") == "mark"
    assert glyph.icon_named("sparkles") == "backslash"
