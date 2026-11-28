"""rlx's glyph for the tray (#34): the backslash of ``docs/assets/rlx-logo.svg``, or its whole
``r\\x`` mark, drawn at a menu bar's sizes in rlx's blue or as a template.

The paths are the logo's own, byte for byte (``tests/test_tray_glyph.py`` holds them to the
file): glyphs outlined from JetBrains Mono Medium, absolute ``M``, ``L``, ``H``, ``V``, ``Q`` and
``Z`` commands only. They are drawn here, in pure Python, so that the tray needs no imaging
library and draws the same pixels on every OS: the outline flattened to edges, filled by the
non-zero rule a scanline at a time, 4 by 4 samples a pixel for its edges' coverage. An image is
a PNG (macOS's ``NSImage`` reads one) or ARGB32 rows (a StatusNotifierItem's ``IconPixmap``).

Imported by the tray's own process only (``services.tray``), never by the CLI, a hook or the
service: ``tests/test_tray_imports.py``.
"""

from __future__ import annotations

import itertools
import re
import struct
import zlib
from collections.abc import Iterator, Sequence
from dataclasses import dataclass
from typing import Literal

R_PATH = (
    "M0 830V280H98V386H99Q107 332 145.5 301Q184 270 248 270Q333 270 380 322.5Q427 375 427"
    " 470V518H328V471Q328 356 215 356Q158 356 128.5 389Q99 422 99 483V830Z"
)
"""The logo's ``r``."""
BACKSLASH_PATH = "M897 940 543 0H647L1001 940Z"
"""The logo's backslash: rlx's glyph, the tray's icon by default."""
X_PATH = (
    "M1098 830 1293 547 1111 280H1226L1329 441Q1336 453 1342.5 466Q1349 479 1353 487Q1356"
    " 479 1362.5 466Q1369 453 1376 441L1479 280H1594L1412 548L1606 830H1490L1378 657Q1371"
    " 646 1364 631.5Q1357 617 1353 608Q1348 617 1341 631.5Q1334 646 1326 657L1214 830Z"
)
"""The logo's ``x``."""
MARK_PATH = f"{R_PATH} {BACKSLASH_PATH} {X_PATH}"
"""The whole ``r\\x`` mark, as the logo's one ``<path>`` holds it."""
VIEW_BOX = (1606.0, 940.0)
"""The logo's ``viewBox`` (width, height): the mark's canvas, whose height the glyphs share."""

BLUE = (0x2F, 0x74, 0xF0)
"""rlx's blue, ``#2F74F0``: the logo's fill."""
BLACK = (0, 0, 0)
"""A template image's colour: the OS paints it to match the menu bar, by its alpha alone."""

Icon = Literal["backslash", "mark", "template"]
"""What the tray shows (``[tray] icon``): the backslash in blue (the default), the whole mark
in blue, or the backslash as a template (macOS paints it as its other menu bar icons)."""
ICONS: tuple[Icon, ...] = ("backslash", "mark", "template")

SAMPLES = 4
"""Samples a pixel along each axis: 16 a pixel, so an edge's coverage is in 16ths."""
CURVE_STEPS = 8
"""Segments a quadratic is flattened into: at 48 px a glyph's curve stays within a sample."""

Point = tuple[float, float]


@dataclass(frozen=True)
class Image:
    """A drawn glyph: ``alpha`` row by row from the top, 0-255, and the colour it is in."""

    width: int
    height: int
    alpha: tuple[bytes, ...]
    colour: tuple[int, int, int]

    def rgba(self) -> bytes:
        """Non-premultiplied RGBA, row by row from the top."""
        red, green, blue = self.colour
        return b"".join(bytes(v for a in row for v in (red, green, blue, a)) for row in self.alpha)

    def argb(self) -> bytes:
        """ARGB32 in network byte order, row by row: a StatusNotifierItem's ``IconPixmap``."""
        red, green, blue = self.colour
        return b"".join(bytes(v for a in row for v in (a, red, green, blue)) for row in self.alpha)

    def png(self) -> bytes:
        """The image as a PNG: 8-bit RGBA, no filter, what ``NSImage`` reads."""
        stride = self.width * 4
        raw = self.rgba()
        rows = b"".join(b"\x00" + raw[y * stride : (y + 1) * stride] for y in range(self.height))
        header = struct.pack(">IIBBBBB", self.width, self.height, 8, 6, 0, 0, 0)
        return (
            b"\x89PNG\r\n\x1a\n"
            + _chunk(b"IHDR", header)
            + _chunk(b"IDAT", zlib.compress(rows, 9))
            + _chunk(b"IEND", b"")
        )


def _chunk(kind: bytes, body: bytes) -> bytes:
    return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))


_TOKEN = re.compile(r"[MLHVQZ]|-?(?:\d+\.?\d*|\.\d+)")


class PathError(ValueError):
    """A path this drawing does not read: a command other than ``M L H V Q Z``."""


def outlines(d: str) -> list[list[Point]]:
    """The closed outlines of the path ``d``, each a list of points, curves flattened."""
    if re.search(r"[^MLHVQZ0-9.\s-]", d):
        raise PathError(f"only absolute M, L, H, V, Q and Z are drawn: {d[:40]!r}")
    tokens = _TOKEN.findall(d)
    shapes: list[list[Point]] = []
    shape: list[Point] = []
    command = ""
    at: Point = (0.0, 0.0)
    i = 0

    def number() -> float:
        nonlocal i
        value = float(tokens[i])
        i += 1
        return value

    while i < len(tokens):
        if tokens[i] in "MLHVQZ":
            command = tokens[i]
            i += 1
            if command == "Z":
                if len(shape) > 2:
                    shapes.append(shape)
                shape = []
                continue
        elif command in ("", "Z"):
            raise PathError(f"a number with no command before it in {d[:40]!r}")
        if command == "M":
            if len(shape) > 2:
                shapes.append(shape)
            at = (number(), number())
            shape = [at]
            command = "L"  # what repeats after a moveto is a lineto
        elif command == "L":
            at = (number(), number())
            shape.append(at)
        elif command == "H":
            at = (number(), at[1])
            shape.append(at)
        elif command == "V":
            at = (at[0], number())
            shape.append(at)
        elif command == "Q":
            control = (number(), number())
            end = (number(), number())
            shape.extend(_quadratic(at, control, end))
            at = end
    if len(shape) > 2:
        shapes.append(shape)
    return shapes


def _quadratic(start: Point, control: Point, end: Point) -> Iterator[Point]:
    for step in range(1, CURVE_STEPS + 1):
        t = step / CURVE_STEPS
        u = 1 - t
        yield (
            u * u * start[0] + 2 * u * t * control[0] + t * t * end[0],
            u * u * start[1] + 2 * u * t * control[1] + t * t * end[1],
        )


def _bounds(shapes: Sequence[Sequence[Point]]) -> tuple[float, float, float, float]:
    xs = [x for shape in shapes for x, _ in shape]
    ys = [y for shape in shapes for _, y in shape]
    return min(xs), min(ys), max(xs), max(ys)


def draw(
    d: str,
    height: int,
    *,
    width: int | None = None,
    colour: tuple[int, int, int] = BLUE,
    margin: float = 1 / 9,
) -> Image:
    """The path ``d`` drawn ``height`` pixels high (and ``width`` wide, square by default),
    its outline scaled to the canvas less ``margin`` of the height on each side, centred."""
    width = height if width is None else width
    if height < 1 or width < 1:
        raise ValueError(f"an image is at least 1 x 1 px, not {width} x {height}")
    shapes = outlines(d)
    left, top, right, bottom = _bounds(shapes)
    inner_w, inner_h = width - 2 * margin * height, height * (1 - 2 * margin)
    scale = min(inner_w / (right - left), inner_h / (bottom - top))
    x0 = (width - (right - left) * scale) / 2 - left * scale
    y0 = (height - (bottom - top) * scale) / 2 - top * scale
    edges = [
        (x0 + a[0] * scale, y0 + a[1] * scale, x0 + b[0] * scale, y0 + b[1] * scale)
        for shape in shapes
        for a, b in zip(shape, [*shape[1:], shape[0]], strict=True)
        if a[1] != b[1]
    ]
    full = SAMPLES * SAMPLES
    rows: list[bytes] = []
    for py in range(height):
        counts = [0] * width
        for sy in range(SAMPLES):
            y = py + (sy + 0.5) / SAMPLES
            crossings = sorted(
                (ax + (y - ay) * (bx - ax) / (by - ay), 1 if by > ay else -1)
                for ax, ay, bx, by in edges
                if min(ay, by) <= y < max(ay, by)
            )
            winding = 0
            for (xa, turn), (xb, _) in itertools.pairwise(crossings):
                winding += turn
                if winding == 0:
                    continue
                first = max(0, _ceil(xa * SAMPLES - 0.5))
                last = min(width * SAMPLES, _ceil(xb * SAMPLES - 0.5))
                for k in range(first, last):
                    counts[k // SAMPLES] += 1
        rows.append(bytes(round(255 * count / full) for count in counts))
    return Image(width, height, tuple(rows), colour)


def _ceil(value: float) -> int:
    whole = int(value)
    return whole + 1 if value > whole else whole


def icon_named(value: str) -> Icon:
    """``value`` as one of :data:`ICONS`; the backslash for anything else."""
    for known in ICONS:
        if value == known:
            return known
    return "backslash"


def icon(choice: Icon, height: int) -> Image:
    """What ``[tray] icon`` names, ``height`` pixels high: the mark as wide as it is."""
    if choice == "mark":
        width = _ceil(height * VIEW_BOX[0] / VIEW_BOX[1] - 1e-9)
        return draw(MARK_PATH, height, width=width, margin=1 / 9)
    colour = BLACK if choice == "template" else BLUE
    return draw(BACKSLASH_PATH, height, colour=colour)
