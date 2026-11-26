// Ported from tmux 3.8's grid.c and grid-view.c, whose notice follows.
//
// Copyright (c) 2008 Nicholas Marriott <nicholas.marriott@gmail.com>
//
// Permission to use, copy, modify, and distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF MIND, USE, DATA OR PROFITS, WHETHER
// IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING
// OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! The grid: rows of cells, history above the screen, as tmux keeps them.
//!
//! Lines are allocated only as cells are written, and grow in tmux's steps (a quarter of
//! the width, a half, then all of it): `capture-pane` reads a line to its allocated
//! width, so those steps show in its bytes. Coordinates here are absolute, history
//! first, from row 0 to `hsize - 1`, then the screen; the `view_` functions take
//! screen coordinates. A cell is five bytes unless it needs more: a character that
//! isn't one byte of ASCII, an RGB colour, an underline colour, a hyperlink or a tab.

use std::collections::HashMap;

/// Bold (tmux's "bright").
pub const ATTR_BRIGHT: u16 = 0x1;
/// Dim.
pub const ATTR_DIM: u16 = 0x2;
/// Underline.
pub const ATTR_UNDERSCORE: u16 = 0x4;
/// Blink.
pub const ATTR_BLINK: u16 = 0x8;
/// Reverse video.
pub const ATTR_REVERSE: u16 = 0x10;
/// Hidden.
pub const ATTR_HIDDEN: u16 = 0x20;
/// Italics.
pub const ATTR_ITALICS: u16 = 0x40;
/// The line-drawing character set.
pub const ATTR_CHARSET: u16 = 0x80;
/// Strikethrough.
pub const ATTR_STRIKETHROUGH: u16 = 0x100;
/// Double underline.
pub const ATTR_UNDERSCORE_2: u16 = 0x200;
/// Curly underline.
pub const ATTR_UNDERSCORE_3: u16 = 0x400;
/// Dotted underline.
pub const ATTR_UNDERSCORE_4: u16 = 0x800;
/// Dashed underline.
pub const ATTR_UNDERSCORE_5: u16 = 0x1000;
/// Overline.
pub const ATTR_OVERLINE: u16 = 0x2000;
/// Every underline style.
pub const ATTR_ALL_UNDERSCORE: u16 =
    ATTR_UNDERSCORE | ATTR_UNDERSCORE_2 | ATTR_UNDERSCORE_3 | ATTR_UNDERSCORE_4 | ATTR_UNDERSCORE_5;

/// A compact cell's foreground is from the 256-colour palette.
pub const FLAG_FG256: u8 = 0x1;
/// A compact cell's background is from the 256-colour palette.
pub const FLAG_BG256: u8 = 0x2;
/// The cell is the second half (or more) of a wide character.
pub const FLAG_PADDING: u8 = 0x4;
/// The cell's entry points at an extended cell.
pub const FLAG_EXTENDED: u8 = 0x8;
/// The cell was cleared, not written.
pub const FLAG_CLEARED: u8 = 0x40;
/// The cell is a tab, as wide as its stop.
pub const FLAG_TAB: u8 = 0x80;

/// The line goes on in the next one.
pub const LINE_WRAPPED: u16 = 0x1;
/// The line holds extended cells.
pub const LINE_EXTENDED: u16 = 0x2;
/// The line was consumed by a reflow.
pub const LINE_DEAD: u16 = 0x4;
/// The line holds a hyperlink.
pub const LINE_HYPERLINK: u16 = 0x100;

/// A colour from the 256-colour palette, in its low byte.
pub const COLOUR_FLAG_256: u32 = 0x0100_0000;
/// An RGB colour, in its low three bytes.
pub const COLOUR_FLAG_RGB: u32 = 0x0200_0000;
/// The default colour.
pub const COLOUR_DEFAULT: u32 = 8;

/// Whether `colour` is a default one: tmux's 8, and 9, the terminal's.
pub fn colour_is_default(colour: u32) -> bool {
    colour == 8 || colour == 9
}

/// An RGB colour.
pub fn colour_rgb(r: u8, g: u8, b: u8) -> u32 {
    COLOUR_FLAG_RGB | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

/// The most bytes one cell's character holds, combining marks included.
pub const UTF8_SIZE: usize = 32;

/// One cell's character: its UTF-8 bytes and its width.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Utf8 {
    bytes: Vec<u8>,
    /// Cells it takes: 0, 1 or 2.
    pub width: u8,
}

impl Utf8 {
    /// The one-byte character `byte`, one cell wide.
    pub fn one(byte: u8) -> Self {
        Self {
            bytes: vec![byte],
            width: 1,
        }
    }

    /// `bytes`, `width` cells wide.
    pub fn new(bytes: &[u8], width: u8) -> Self {
        Self {
            bytes: bytes.to_vec(),
            width,
        }
    }

    /// The character's bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// How many bytes it is.
    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// `more` appended, as a combining character is.
    pub fn push(&mut self, more: &[u8]) {
        self.bytes.extend_from_slice(more);
    }
}

/// A cell, as it is read and written: tmux's `struct grid_cell`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The character.
    pub data: Utf8,
    /// `ATTR_*` bits.
    pub attr: u16,
    /// `FLAG_*` bits.
    pub flags: u8,
    /// Foreground: 0 to 7, 8 for the default, 90 to 97, or a flagged colour.
    pub fg: u32,
    /// Background, alike.
    pub bg: u32,
    /// Underline colour: 8 for none, or a flagged colour.
    pub us: u32,
    /// Hyperlink: 0 for none, else the screen's number for it.
    pub link: u32,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            data: Utf8::one(b' '),
            attr: 0,
            flags: 0,
            fg: COLOUR_DEFAULT,
            bg: COLOUR_DEFAULT,
            us: COLOUR_DEFAULT,
            link: 0,
        }
    }
}

impl Cell {
    /// A cell that was cleared: a space, `FLAG_CLEARED`.
    pub fn cleared() -> Self {
        Self {
            flags: FLAG_CLEARED,
            ..Self::default()
        }
    }

    /// The padding after a wide character.
    pub fn padding() -> Self {
        Self {
            data: Utf8::new(b"!", 0),
            flags: FLAG_PADDING,
            ..Self::default()
        }
    }

    /// Whether this looks like `other`: colours, attributes, flags but `CLEARED`, link.
    pub fn looks_equal(&self, other: &Self) -> bool {
        self.fg == other.fg
            && self.bg == other.bg
            && self.attr == other.attr
            && (self.flags & !FLAG_CLEARED) == (other.flags & !FLAG_CLEARED)
            && self.link == other.link
    }

    /// Whether this is `other`, character included.
    pub fn equals(&self, other: &Self) -> bool {
        self.looks_equal(other)
            && self.data.width == other.data.width
            && self.data.bytes == other.data.bytes
    }

    /// This cell made a tab `width` cells wide: `width` spaces, `FLAG_TAB`.
    pub fn set_tab(&mut self, width: u8) {
        self.flags |= FLAG_TAB;
        self.flags &= !FLAG_PADDING;
        self.data = Utf8::new(&vec![b' '; usize::from(width)], width);
    }

    fn needs_extended(&self) -> bool {
        self.attr > 0xff
            || self.data.size() > 1
            || self.data.width > 1
            || self.fg & COLOUR_FLAG_RGB != 0
            || self.bg & COLOUR_FLAG_RGB != 0
            || self.us != COLOUR_DEFAULT
            || self.link != 0
            || self.flags & FLAG_TAB != 0
    }
}

/// One cell of a line: five bytes, the cell itself or where its extended cell is.
#[derive(Clone, Copy, Debug, Default)]
struct Entry {
    flags: u8,
    /// Compact: attr, fg, bg, the character. Extended: the offset, little-endian.
    data: [u8; 4],
}

impl Entry {
    const CLEARED: Self = Self {
        flags: FLAG_CLEARED,
        data: [0, 8, 8, b' '],
    };

    fn offset(&self) -> usize {
        u32::from_le_bytes(self.data) as usize
    }

    fn set_offset(&mut self, offset: usize) {
        self.data = u32::try_from(offset).unwrap_or(u32::MAX).to_le_bytes();
    }

    fn store(&mut self, cell: &Cell, byte: u8) {
        self.flags = cell.flags & !FLAG_CLEARED;
        if cell.fg & COLOUR_FLAG_256 != 0 {
            self.flags |= FLAG_FG256;
        }
        if cell.bg & COLOUR_FLAG_256 != 0 {
            self.flags |= FLAG_BG256;
        }
        self.data = [
            (cell.attr & 0xff) as u8,
            (cell.fg & 0xff) as u8,
            (cell.bg & 0xff) as u8,
            byte,
        ];
    }
}

/// A line's extended cell.
#[derive(Clone, Debug)]
struct Extended {
    data: Utf8,
    attr: u16,
    flags: u8,
    fg: u32,
    bg: u32,
    us: u32,
    link: u32,
}

/// One line: its cells as allocated, how many were used, and its flags.
#[derive(Clone, Debug, Default)]
pub struct Line {
    cells: Vec<Entry>,
    extended: Vec<Extended>,
    /// The cells written to, from the left.
    pub cellused: u32,
    /// `LINE_*` bits.
    pub flags: u16,
}

impl Line {
    /// How many cells the line has allocated.
    pub fn cellsize(&self) -> u32 {
        self.cells.len() as u32
    }

    fn get(&self, px: usize) -> Cell {
        let entry = &self.cells[px];
        if entry.flags & FLAG_EXTENDED != 0 {
            let Some(extended) = self.extended.get(entry.offset()) else {
                return Cell::default();
            };
            let mut cell = Cell {
                data: extended.data.clone(),
                attr: extended.attr,
                flags: extended.flags,
                fg: extended.fg,
                bg: extended.bg,
                us: extended.us,
                link: extended.link,
            };
            if cell.flags & FLAG_TAB != 0 {
                let width = cell.data.width;
                cell.set_tab(width);
            }
            return cell;
        }
        let [attr, fg, bg, byte] = entry.data;
        let mut cell = Cell {
            data: Utf8::one(byte),
            attr: u16::from(attr),
            flags: entry.flags & !(FLAG_FG256 | FLAG_BG256),
            fg: u32::from(fg),
            bg: u32::from(bg),
            us: COLOUR_DEFAULT,
            link: 0,
        };
        if entry.flags & FLAG_FG256 != 0 {
            cell.fg |= COLOUR_FLAG_256;
        }
        if entry.flags & FLAG_BG256 != 0 {
            cell.bg |= COLOUR_FLAG_256;
        }
        cell
    }

    /// Points `px` at a new extended cell, keeping `flags`.
    fn new_extended(&mut self, px: usize, flags: u8) {
        self.extended.push(Extended {
            data: Utf8::one(b' '),
            attr: 0,
            flags: 0,
            fg: COLOUR_DEFAULT,
            bg: COLOUR_DEFAULT,
            us: COLOUR_DEFAULT,
            link: 0,
        });
        let entry = &mut self.cells[px];
        entry.set_offset(self.extended.len() - 1);
        entry.flags = flags | FLAG_EXTENDED;
    }

    /// Writes `cell` to `px`'s extended cell, making one if it has none.
    fn set_extended(&mut self, px: usize, cell: &Cell) -> usize {
        let flags = cell.flags & !FLAG_CLEARED;
        if self.cells[px].flags & FLAG_EXTENDED == 0 {
            self.new_extended(px, flags);
        }
        self.flags |= LINE_EXTENDED;
        if cell.link != 0 {
            self.flags |= LINE_HYPERLINK;
        }
        let offset = self.cells[px].offset();
        self.extended[offset] = Extended {
            data: cell.data.clone(),
            attr: cell.attr,
            flags,
            fg: cell.fg,
            bg: cell.bg,
            us: cell.us,
            link: cell.link,
        };
        offset
    }

    fn set(&mut self, px: usize, cell: &Cell) {
        if self.cells[px].flags & FLAG_EXTENDED != 0 || cell.needs_extended() {
            self.set_extended(px, cell);
        } else {
            let byte = cell.data.bytes().first().copied().unwrap_or(b' ');
            self.cells[px].store(cell, byte);
        }
    }

    /// Frees the extended cells no cell points at any more.
    fn compact(&mut self) {
        if self.extended.is_empty() {
            return;
        }
        let mut kept = Vec::new();
        for entry in &mut self.cells {
            if entry.flags & FLAG_EXTENDED != 0 {
                if let Some(extended) = self.extended.get(entry.offset()) {
                    kept.push(extended.clone());
                }
                entry.set_offset(kept.len().saturating_sub(1));
            }
        }
        self.extended = kept;
    }

    /// Clears cell `px` to `bg`; `moved` when the cell's old contents went elsewhere.
    fn clear_cell(&mut self, px: u32, bg: u32, moved: bool) {
        let px = px as usize;
        let entry = self.cells[px];
        let had_extended = entry.flags & FLAG_EXTENDED != 0;
        self.cells[px] = Entry::CLEARED;
        if !moved && had_extended && entry.offset() < self.extended.len() {
            self.cells[px].flags |= FLAG_EXTENDED;
            self.cells[px].set_offset(entry.offset());
            let mut cell = Cell::cleared();
            if bg != COLOUR_DEFAULT {
                cell.bg = bg;
            }
            self.set_extended(px, &cell);
        } else if bg != COLOUR_DEFAULT {
            if bg & COLOUR_FLAG_RGB != 0 {
                self.new_extended(px, self.cells[px].flags);
                let cell = Cell {
                    bg,
                    ..Cell::cleared()
                };
                self.set_extended(px, &cell);
            } else {
                if bg & COLOUR_FLAG_256 != 0 {
                    self.cells[px].flags |= FLAG_BG256;
                }
                self.cells[px].data[2] = (bg & 0xff) as u8;
            }
        }
    }

    /// Grows the line to hold `sx` cells, in tmux's steps for a grid `grid_sx` wide, the
    /// new ones cleared to `bg`.
    fn expand(&mut self, sx: u32, grid_sx: u32, bg: u32) {
        let cellsize = self.cellsize();
        if sx <= cellsize {
            return;
        }
        let sx = if sx < grid_sx / 4 {
            grid_sx / 4
        } else if sx < grid_sx / 2 {
            grid_sx / 2
        } else if grid_sx > sx {
            grid_sx
        } else {
            sx
        };
        self.cells.resize(sx as usize, Entry::default());
        for xx in cellsize..sx {
            self.clear_cell(xx, bg, false);
        }
    }

    /// Writes `cell` at `px`, as tmux's `grid_set_cell` does, for a grid `grid_sx` wide.
    fn set_cell(&mut self, px: u32, cell: &Cell, grid_sx: u32) {
        self.expand(px + 1, grid_sx, COLOUR_DEFAULT);
        if px + 1 > self.cellused {
            self.cellused = px + 1;
        }
        self.set(px as usize, cell);
    }

    /// Whether writing `cell` at `px` would change nothing, by tmux's test in
    /// `screen_write_cell`: past the cells, a default cell; else a compact entry that
    /// holds exactly this, its raw flags and colour bytes included.
    pub fn unchanged_by(&self, px: u32, cell: &Cell) -> bool {
        let Some(entry) = self.cells.get(px as usize) else {
            return cell.equals(&Cell::default());
        };
        let [attr, fg, bg, byte] = entry.data;
        entry.flags & FLAG_EXTENDED == 0
            && cell.flags == entry.flags
            && cell.attr == u16::from(attr)
            && cell.fg == u32::from(fg)
            && cell.bg == u32::from(bg)
            && cell.data.width == 1
            && cell.data.size() == 1
            && cell.data.bytes()[0] == byte
    }

    /// The bytes this line holds, for the memory bound.
    pub fn bytes(&self) -> usize {
        self.cells.capacity() * std::mem::size_of::<Entry>()
            + self.extended.capacity() * std::mem::size_of::<Extended>()
            + self
                .extended
                .iter()
                .map(|e| e.data.bytes.capacity())
                .sum::<usize>()
    }
}

/// The grid: history, then the screen.
#[derive(Clone, Debug)]
pub struct Grid {
    /// The width.
    pub sx: u32,
    /// The screen's height.
    pub sy: u32,
    /// The lines of history above the screen.
    pub hsize: u32,
    /// The most lines of history kept.
    pub hlimit: u32,
    /// The history lines scrolled off, not cleared, that a taller screen takes back.
    pub hscrolled: u32,
    /// Whether lines scrolled off the top go into the history.
    pub history: bool,
    lines: Vec<Line>,
}

impl Grid {
    /// A grid `sx` by `sy`, keeping `hlimit` lines of history (none when 0).
    pub fn new(sx: u32, sy: u32, hlimit: u32) -> Self {
        Self {
            sx,
            sy,
            hsize: 0,
            hlimit,
            hscrolled: 0,
            history: hlimit != 0,
            lines: vec![Line::default(); sy as usize],
        }
    }

    fn check_y(&self, py: u32) -> bool {
        py < self.hsize + self.sy
    }

    /// Line `py`, absolute.
    pub fn line(&self, py: u32) -> &Line {
        &self.lines[py as usize]
    }

    /// Line `py`, absolute, to change.
    pub fn line_mut(&mut self, py: u32) -> &mut Line {
        &mut self.lines[py as usize]
    }

    /// Line `py`, when there is one.
    pub fn peek_line(&self, py: u32) -> Option<&Line> {
        self.lines.get(py as usize).filter(|_| self.check_y(py))
    }

    /// The bytes every line holds, for the memory bound.
    pub fn bytes(&self) -> usize {
        self.lines.capacity() * std::mem::size_of::<Line>()
            + self.lines.iter().map(Line::bytes).sum::<usize>()
    }

    fn clear_cell(&mut self, px: u32, py: u32, bg: u32, moved: bool) {
        self.lines[py as usize].clear_cell(px, bg, moved);
    }

    /// Grows line `py` to hold `sx` cells, in tmux's steps, the new ones cleared to `bg`.
    fn expand_line(&mut self, py: u32, sx: u32, bg: u32) {
        let grid_sx = self.sx;
        self.lines[py as usize].expand(sx, grid_sx, bg);
    }

    /// Empties line `py`, filled to the width with `bg` when that isn't the default.
    pub fn empty_line(&mut self, py: u32, bg: u32) {
        self.lines[py as usize] = Line::default();
        if !colour_is_default(bg) {
            let sx = self.sx;
            self.expand_line(py, sx, bg);
        }
    }

    /// The cell at `px`, `py`: a default one beyond the line's cells.
    pub fn get_cell(&self, px: u32, py: u32) -> Cell {
        if !self.check_y(py) || px >= self.lines[py as usize].cellsize() {
            return Cell::default();
        }
        self.lines[py as usize].get(px as usize)
    }

    /// Writes `cell` at `px`, `py`.
    pub fn set_cell(&mut self, px: u32, py: u32, cell: &Cell) {
        if !self.check_y(py) {
            return;
        }
        let grid_sx = self.sx;
        self.lines[py as usize].set_cell(px, cell, grid_sx);
    }

    /// Writes padding, on `bg`, at `px`, `py`.
    pub fn set_padding(&mut self, px: u32, py: u32, bg: u32) {
        let cell = Cell {
            bg,
            ..Cell::padding()
        };
        self.set_cell(px, py, &cell);
    }

    /// Writes the one-byte characters `bytes` from `px`, each as `cell`.
    pub fn set_cells(&mut self, px: u32, py: u32, cell: &Cell, bytes: &[u8]) {
        if !self.check_y(py) {
            return;
        }
        let n = bytes.len() as u32;
        self.expand_line(py, px + n, COLOUR_DEFAULT);
        let line = &mut self.lines[py as usize];
        if px + n > line.cellused {
            line.cellused = px + n;
        }
        for (i, &byte) in bytes.iter().enumerate() {
            let at = px as usize + i;
            if line.cells[at].flags & FLAG_EXTENDED != 0 || cell.needs_extended() {
                let mut one = cell.clone();
                one.data = Utf8::one(byte);
                line.set_extended(at, &one);
            } else {
                line.cells[at].store(cell, byte);
            }
        }
    }

    /// Clears `nx` by `ny` cells from `px`, `py` to `bg`.
    pub fn clear(&mut self, px: u32, py: u32, nx: u32, ny: u32, bg: u32) {
        if nx == 0 || ny == 0 {
            return;
        }
        if px == 0 && nx == self.sx {
            self.clear_lines(py, ny, bg);
            return;
        }
        // tmux's arithmetic is unsigned and wraps, as here: a count that wrapped past
        // the end clears nothing.
        let end = py.wrapping_add(ny);
        if !self.check_y(py) || !self.check_y(end.wrapping_sub(1)) {
            return;
        }
        for yy in py..end {
            let sx = self.sx.min(self.lines[yy as usize].cellsize());
            let mut ox = nx;
            if colour_is_default(bg) {
                if px > sx {
                    continue;
                }
                if px + nx > sx {
                    ox = sx - px;
                }
            }
            self.expand_line(yy, px + ox, COLOUR_DEFAULT);
            for xx in px..px + ox {
                self.clear_cell(xx, yy, bg, false);
            }
        }
    }

    /// Empties `ny` lines from `py`, to `bg`.
    pub fn clear_lines(&mut self, py: u32, ny: u32, bg: u32) {
        let end = py.wrapping_add(ny);
        if ny == 0 || !self.check_y(py) || !self.check_y(end.wrapping_sub(1)) {
            return;
        }
        for yy in py..end {
            self.empty_line(yy, bg);
        }
        if py != 0 {
            self.lines[py as usize - 1].flags &= !LINE_WRAPPED;
        }
    }

    /// Moves `ny` lines from `py` to `dy`, emptying to `bg` those left behind.
    pub fn move_lines(&mut self, dy: u32, py: u32, ny: u32, bg: u32) {
        if ny == 0 || py == dy {
            return;
        }
        if !self.check_y(py)
            || !self.check_y(py + ny - 1)
            || !self.check_y(dy)
            || !self.check_y(dy + ny - 1)
        {
            return;
        }
        if dy != 0 {
            self.lines[dy as usize - 1].flags &= !LINE_WRAPPED;
        }
        let moved: Vec<Line> = (py..py + ny)
            .map(|yy| std::mem::take(&mut self.lines[yy as usize]))
            .collect();
        for (i, line) in moved.into_iter().enumerate() {
            self.lines[dy as usize + i] = line;
        }
        for yy in py..py + ny {
            if yy < dy || yy >= dy + ny {
                self.empty_line(yy, bg);
            }
        }
        if py != 0 && (py < dy || py >= dy + ny) {
            self.lines[py as usize - 1].flags &= !LINE_WRAPPED;
        }
    }

    /// Moves `nx` cells of line `py` from `px` to `dx`, clearing to `bg` those left
    /// behind.
    pub fn move_cells(&mut self, dx: u32, px: u32, py: u32, nx: u32, bg: u32) {
        if nx == 0 || px == dx || !self.check_y(py) {
            return;
        }
        self.expand_line(py, px + nx, COLOUR_DEFAULT);
        self.expand_line(py, dx + nx, COLOUR_DEFAULT);
        let line = &mut self.lines[py as usize];
        line.cells
            .copy_within(px as usize..(px + nx) as usize, dx as usize);
        if dx + nx > line.cellused {
            line.cellused = dx + nx;
        }
        for xx in px..px + nx {
            if xx >= dx && xx < dx + nx {
                continue;
            }
            self.clear_cell(xx, py, bg, true);
        }
    }

    /// Frees the oldest lines when the history is full: a tenth of the limit.
    pub fn collect_history(&mut self) {
        if self.hsize == 0 || self.hsize < self.hlimit {
            return;
        }
        let ny = (self.hlimit / 10).clamp(1, self.hsize);
        self.lines.drain(0..ny as usize);
        self.hsize -= ny;
        if self.hscrolled > self.hsize {
            self.hscrolled = self.hsize;
        }
    }

    /// Scrolls the whole screen up one line into the history, a new line at the bottom.
    pub fn scroll_history(&mut self, bg: u32) {
        let yy = self.hsize + self.sy;
        self.lines.push(Line::default());
        self.empty_line(yy, bg);
        self.hscrolled += 1;
        self.lines[self.hsize as usize].compact();
        self.hsize += 1;
    }

    /// Empties the history.
    pub fn clear_history(&mut self) {
        self.lines.drain(0..self.hsize as usize);
        self.hscrolled = 0;
        self.hsize = 0;
    }

    /// Scrolls the region `upper` to `lower` (absolute) up one line, its top line into
    /// the history.
    pub fn scroll_history_region(&mut self, upper: u32, lower: u32, bg: u32) {
        let history = self.hsize as usize;
        // The region's top line goes into the history, above every line of the screen;
        // the lines above the region stay as they were, and the rest of the region
        // moves up over it.
        let top = self.lines.remove(upper as usize);
        self.lines.insert(history, top);
        // A new line at the region's bottom: the lines below it move down one, onto
        // the line the history took from the screen's count.
        let lower = lower + 1;
        self.lines.insert(lower as usize, Line::default());
        self.empty_line(lower, bg);
        self.hscrolled += 1;
        self.hsize += 1;
    }

    /// Copies `ny` lines from `src`'s `sy` to this grid's `dy`.
    pub fn duplicate_lines(&mut self, dy: u32, src: &Grid, sy: u32, ny: u32) {
        let mut ny = ny;
        if dy + ny > self.hsize + self.sy {
            ny = self.hsize + self.sy - dy;
        }
        if sy + ny > src.hsize + src.sy {
            ny = src.hsize + src.sy - sy;
        }
        for yy in 0..ny {
            self.lines[(dy + yy) as usize] = src.lines[(sy + yy) as usize].clone();
        }
    }

    /// Sets the number of lines to `n`, history and screen: new ones empty.
    pub fn adjust_lines(&mut self, n: u32) {
        self.lines.resize(n as usize, Line::default());
    }

    /// Where the cell at `px`, `py` (absolute) is in its unwrapped line: the offset into
    /// it, or `u32::MAX` past its end, and which unwrapped line.
    pub fn wrap_position(&self, px: u32, py: u32) -> (u32, u32) {
        let (mut ax, mut ay) = (0u32, 0u32);
        for yy in 0..py {
            let line = &self.lines[yy as usize];
            if line.flags & LINE_WRAPPED != 0 {
                ax += line.cellused;
            } else {
                ax = 0;
                ay += 1;
            }
        }
        let used = self.lines.get(py as usize).map_or(0, |line| line.cellused);
        let ax = if px >= used { u32::MAX } else { ax + px };
        (ax, ay)
    }

    /// The cell `wrap_position` gave, back to a column and an absolute row.
    pub fn unwrap_position(&self, wx: u32, wy: u32) -> (u32, u32) {
        let ey = self.hsize + self.sy - 1;
        let mut ay = 0;
        let mut yy = 0;
        while yy < ey {
            if ay == wy {
                break;
            }
            if self.lines[yy as usize].flags & LINE_WRAPPED == 0 {
                ay += 1;
            }
            yy += 1;
        }
        let mut wx = wx;
        if wx == u32::MAX {
            while yy < ey && self.lines[yy as usize].flags & LINE_WRAPPED != 0 {
                yy += 1;
            }
            wx = self.lines[yy as usize].cellused;
        } else {
            while self.lines[yy as usize].flags & LINE_WRAPPED != 0 {
                if wx < self.lines[yy as usize].cellused {
                    break;
                }
                wx -= self.lines[yy as usize].cellused;
                yy += 1;
            }
        }
        (wx, yy)
    }

    /// Rewraps every line to the grid's width, which the caller has set to `sx`, as
    /// tmux's `grid_reflow` does: a line too long splits, a wrapped line takes as much of
    /// the next ones as fits.
    pub fn reflow(&mut self, sx: u32) {
        let mut target: Vec<Line> = Vec::new();
        let total = self.hsize + self.sy;
        for yy in 0..total {
            let line = &self.lines[yy as usize];
            if line.flags & LINE_DEAD != 0 {
                continue;
            }
            let (mut at, mut width) = (0u32, 0u32);
            if line.flags & LINE_EXTENDED == 0 {
                width = line.cellused;
                at = width.min(sx);
            } else {
                for i in 0..line.cellused {
                    let w = u32::from(line.get(i as usize).data.width);
                    if at == 0 && width + w > sx {
                        at = i;
                    }
                    width += w;
                }
            }
            if width == sx {
                self.reflow_move(&mut target, yy);
            } else if width > sx {
                self.reflow_split(&mut target, sx, yy, at);
            } else if line.flags & LINE_WRAPPED != 0 {
                self.reflow_join(&mut target, sx, yy, width, false);
            } else {
                self.reflow_move(&mut target, yy);
            }
        }
        if (target.len() as u32) < self.sy {
            target.resize(self.sy as usize, Line::default());
        }
        self.hsize = target.len() as u32 - self.sy;
        if self.hscrolled > self.hsize {
            self.hscrolled = self.hsize;
        }
        self.lines = target;
    }

    fn reflow_move(&mut self, target: &mut Vec<Line>, yy: u32) {
        let line = std::mem::replace(
            &mut self.lines[yy as usize],
            Line {
                flags: LINE_DEAD,
                ..Line::default()
            },
        );
        target.push(line);
    }

    fn reflow_join(&mut self, target: &mut Vec<Line>, sx: u32, yy: u32, width: u32, already: bool) {
        let to = if already {
            target.len() - 1
        } else {
            self.reflow_move(target, yy);
            target.len() - 1
        };
        let mut width = width;
        let mut at = target[to].cellused;
        let mut lines = 0u32;
        let mut wrapped = true;
        let mut from: Option<u32> = None;
        let mut want = 0u32;
        let total = self.hsize + self.sy;
        loop {
            if yy + 1 + lines == total {
                break;
            }
            let line = yy + 1 + lines;
            let next = &self.lines[line as usize];
            if next.flags & LINE_WRAPPED == 0 {
                wrapped = false;
            }
            if next.cellused == 0 {
                if !wrapped {
                    break;
                }
                lines += 1;
                continue;
            }
            let first = next.get(0);
            if width + u32::from(first.data.width) > sx {
                break;
            }
            width += u32::from(first.data.width);
            target[to].set_cell(at, &first, sx);
            at += 1;
            from = Some(line);
            want = 1;
            while want < next.cellused {
                let cell = next.get(want as usize);
                if width + u32::from(cell.data.width) > sx {
                    break;
                }
                width += u32::from(cell.data.width);
                target[to].set_cell(at, &cell, sx);
                at += 1;
                want += 1;
            }
            lines += 1;
            if !wrapped || want != next.cellused || width == sx {
                break;
            }
        }
        let Some(from) = from else {
            return;
        };
        if lines == 0 {
            return;
        }
        let left = self.lines[from as usize].cellused - want;
        if left != 0 {
            self.move_cells(0, want, yy + lines, left, COLOUR_DEFAULT);
            let line = &mut self.lines[from as usize];
            line.cells.truncate(left as usize);
            line.cellused = left;
            lines -= 1;
        } else if !wrapped {
            target[to].flags &= !LINE_WRAPPED;
        }
        for i in yy + 1..yy + 1 + lines {
            self.lines[i as usize] = Line {
                flags: LINE_DEAD,
                ..Line::default()
            };
        }
        let to = to as u32;
        if self.hscrolled > to + lines {
            self.hscrolled -= lines;
        } else if self.hscrolled > to {
            self.hscrolled = to;
        }
    }

    fn reflow_split(&mut self, target: &mut Vec<Line>, sx: u32, yy: u32, at: u32) {
        let line = &self.lines[yy as usize];
        let used = line.cellused;
        let flags = line.flags;
        let lines = if line.flags & LINE_EXTENDED == 0 {
            1 + (used - 1) / sx
        } else {
            let (mut lines, mut width) = (2u32, 0u32);
            for i in at..used {
                let w = u32::from(line.get(i as usize).data.width);
                if width + w > sx {
                    lines += 1;
                    width = 0;
                }
                width += w;
            }
            lines
        };
        let first = target.len();
        let mut row = first + 1;
        target.resize(first + lines as usize, Line::default());
        let (mut width, mut xx) = (0u32, 0u32);
        for i in at..used {
            let cell = self.lines[yy as usize].get(i as usize);
            let w = u32::from(cell.data.width);
            if width + w > sx {
                target[row].flags |= LINE_WRAPPED;
                row += 1;
                width = 0;
                xx = 0;
            }
            width += w;
            target[row].set_cell(xx, &cell, sx);
            xx += 1;
        }
        if flags & LINE_WRAPPED != 0 {
            target[row].flags |= LINE_WRAPPED;
        }
        let mut line = std::mem::replace(
            &mut self.lines[yy as usize],
            Line {
                flags: LINE_DEAD,
                ..Line::default()
            },
        );
        line.cells.truncate(at as usize);
        line.cellused = at;
        line.flags |= LINE_WRAPPED;
        target[first] = line;
        if yy <= self.hscrolled {
            self.hscrolled += lines - 1;
        }
        if width < sx && flags & LINE_WRAPPED != 0 {
            self.reflow_join(target, sx, yy, width, true);
        }
    }

    // The view: screen coordinates.

    /// Screen row `py` as an absolute row.
    pub fn view_y(&self, py: u32) -> u32 {
        self.hsize + py
    }

    /// The cell at screen `px`, `py`.
    pub fn view_get_cell(&self, px: u32, py: u32) -> Cell {
        self.get_cell(px, self.view_y(py))
    }

    /// Writes `cell` at screen `px`, `py`.
    pub fn view_set_cell(&mut self, px: u32, py: u32, cell: &Cell) {
        let py = self.view_y(py);
        self.set_cell(px, py, cell);
    }

    /// Writes padding at screen `px`, `py`.
    pub fn view_set_padding(&mut self, px: u32, py: u32, bg: u32) {
        let py = self.view_y(py);
        self.set_padding(px, py, bg);
    }

    /// Writes one-byte characters at screen `px`, `py`.
    pub fn view_set_cells(&mut self, px: u32, py: u32, cell: &Cell, bytes: &[u8]) {
        let py = self.view_y(py);
        self.set_cells(px, py, cell, bytes);
    }

    /// Scrolls the screen's lines down to the last used one into the history, then
    /// clears the rest: tmux's clear with `scroll-on-clear`.
    pub fn view_clear_history(&mut self, bg: u32) {
        let last = (0..self.sy)
            .filter(|&yy| self.lines[self.view_y(yy) as usize].cellused != 0)
            .map(|yy| yy + 1)
            .max()
            .unwrap_or(0);
        if last == 0 {
            let (sx, sy) = (self.sx, self.sy);
            self.view_clear(0, 0, sx, sy, bg);
            return;
        }
        for _ in 0..last {
            self.collect_history();
            self.scroll_history(bg);
        }
        if last < self.sy {
            let (sx, sy) = (self.sx, self.sy);
            self.view_clear(0, 0, sx, sy - last, bg);
        }
        self.hscrolled = 0;
    }

    /// Clears an area of the screen.
    pub fn view_clear(&mut self, px: u32, py: u32, nx: u32, ny: u32, bg: u32) {
        let py = self.view_y(py);
        self.clear(px, py, nx, ny, bg);
    }

    /// Scrolls the screen region `rupper` to `rlower` up one line.
    pub fn view_scroll_region_up(&mut self, rupper: u32, rlower: u32, bg: u32) {
        if self.history {
            self.collect_history();
            if rupper == 0 && rlower == self.sy - 1 {
                self.scroll_history(bg);
            } else {
                let (upper, lower) = (self.view_y(rupper), self.view_y(rlower));
                self.scroll_history_region(upper, lower, bg);
            }
        } else {
            let (upper, lower) = (self.view_y(rupper), self.view_y(rlower));
            self.move_lines(upper, upper + 1, lower - upper, bg);
        }
    }

    /// Scrolls the screen region `rupper` to `rlower` down one line.
    pub fn view_scroll_region_down(&mut self, rupper: u32, rlower: u32, bg: u32) {
        let (upper, lower) = (self.view_y(rupper), self.view_y(rlower));
        self.move_lines(upper + 1, upper, lower - upper, bg);
    }

    /// Inserts `ny` lines at screen row `py`.
    pub fn view_insert_lines(&mut self, py: u32, ny: u32, bg: u32) {
        let (py, sy) = (self.view_y(py), self.view_y(self.sy));
        self.move_lines(py + ny, py, sy - py - ny, bg);
    }

    /// Inserts `ny` lines at screen row `py`, in a region ending at `rlower`.
    pub fn view_insert_lines_region(&mut self, rlower: u32, py: u32, ny: u32, bg: u32) {
        let (rlower, py) = (self.view_y(rlower), self.view_y(py));
        let ny2 = rlower + 1 - py - ny;
        self.move_lines(rlower + 1 - ny2, py, ny2, bg);
        let sx = self.sx;
        self.clear(0, py + ny2, sx, ny.wrapping_sub(ny2), bg);
    }

    /// Deletes `ny` lines at screen row `py`.
    pub fn view_delete_lines(&mut self, py: u32, ny: u32, bg: u32) {
        let (py, sy) = (self.view_y(py), self.view_y(self.sy));
        self.move_lines(py, py + ny, sy - py - ny, bg);
        let sx = self.sx;
        self.clear(0, sy - ny, sx, ny, bg);
    }

    /// Deletes `ny` lines at screen row `py`, in a region ending at `rlower`.
    pub fn view_delete_lines_region(&mut self, rlower: u32, py: u32, ny: u32, bg: u32) {
        let (rlower, py) = (self.view_y(rlower), self.view_y(py));
        let ny2 = rlower + 1 - py - ny;
        self.move_lines(py, py + ny, ny2, bg);
        let sx = self.sx;
        self.clear(0, py + ny2, sx, ny.wrapping_sub(ny2), bg);
    }

    /// Inserts `nx` cells at screen `px`, `py`.
    pub fn view_insert_cells(&mut self, px: u32, py: u32, nx: u32, bg: u32) {
        let (py, sx) = (self.view_y(py), self.sx);
        if px >= sx - 1 {
            self.clear(px, py, 1, 1, bg);
        } else {
            self.move_cells(px + nx, px, py, sx - px - nx, bg);
        }
    }

    /// Deletes `nx` cells at screen `px`, `py`.
    pub fn view_delete_cells(&mut self, px: u32, py: u32, nx: u32, bg: u32) {
        let (py, sx) = (self.view_y(py), self.sx);
        self.move_cells(px, px + nx, py, sx - px - nx, bg);
        self.clear(sx - nx, py, nx, 1, bg);
    }
}

/// The screen's hyperlinks: each URI with its id, by the number cells carry.
#[derive(Clone, Debug, Default)]
pub struct Hyperlinks {
    by_number: HashMap<u32, (String, String)>,
    by_uri: HashMap<(String, String), u32>,
    order: std::collections::VecDeque<u32>,
    next: u32,
}

/// The most hyperlinks a screen keeps; the oldest go first.
const MAX_HYPERLINKS: usize = 5000;

/// The longest URI kept, once escaped.
const MAX_HYPERLINK_URI: usize = 1024;

impl Hyperlinks {
    /// The number for `uri` with `id`: the same one again for a named link already
    /// held, a new one for an anonymous one. 0 when the URI is too long.
    pub fn put(&mut self, uri: &str, id: Option<&str>) -> u32 {
        let uri = vis(uri);
        if uri.len() > MAX_HYPERLINK_URI {
            return 0;
        }
        let id = vis(id.unwrap_or(""));
        if !id.is_empty()
            && let Some(&number) = self.by_uri.get(&(uri.clone(), id.clone()))
        {
            return number;
        }
        self.next += 1;
        let number = self.next;
        if !id.is_empty() {
            self.by_uri.insert((uri.clone(), id.clone()), number);
        }
        self.by_number.insert(number, (uri, id));
        self.order.push_back(number);
        if self.order.len() >= MAX_HYPERLINKS
            && let Some(oldest) = self.order.pop_front()
            && let Some(key) = self.by_number.remove(&oldest)
        {
            self.by_uri.remove(&key);
        }
        number
    }

    /// `number`'s URI and id.
    pub fn get(&self, number: u32) -> Option<(&str, &str)> {
        self.by_number
            .get(&number)
            .map(|(uri, id)| (uri.as_str(), id.as_str()))
    }

    /// Forgets every link.
    pub fn reset(&mut self) {
        *self = Self {
            next: self.next,
            ..Self::default()
        };
    }
}

/// `text` escaped as tmux's `utf8_stravis(VIS_OCTAL | VIS_CSTYLE)` escapes it: valid
/// UTF-8 and printable ASCII as they are, a backslash doubled, C escapes for the usual
/// controls and octal for any other byte.
fn vis(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{7}' => out.push_str("\\a"),
            '\u{b}' => out.push_str("\\v"),
            '\u{c}' => out.push_str("\\f"),
            '\0' => out.push_str("\\000"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:03o}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

// The capture: tmux's grid_string_cells and its helpers, as `capture-pane -e` uses them.

/// The SGR codes for the attributes tmux writes, in its order.
const SGR_ATTRS: [(u16, u32); 13] = [
    (ATTR_BRIGHT, 1),
    (ATTR_DIM, 2),
    (ATTR_ITALICS, 3),
    (ATTR_UNDERSCORE, 4),
    (ATTR_BLINK, 5),
    (ATTR_REVERSE, 7),
    (ATTR_HIDDEN, 8),
    (ATTR_STRIKETHROUGH, 9),
    (ATTR_UNDERSCORE_2, 42),
    (ATTR_UNDERSCORE_3, 43),
    (ATTR_UNDERSCORE_4, 44),
    (ATTR_UNDERSCORE_5, 45),
    (ATTR_OVERLINE, 53),
];

fn cells_fg(cell: &Cell, values: &mut Vec<u32>) {
    let fg = cell.fg;
    if fg & COLOUR_FLAG_256 != 0 {
        values.extend([38, 5, fg & 0xff]);
    } else if fg & COLOUR_FLAG_RGB != 0 {
        values.extend([38, 2, (fg >> 16) & 0xff, (fg >> 8) & 0xff, fg & 0xff]);
    } else {
        match fg {
            0..=7 => values.push(fg + 30),
            8 => values.push(39),
            90..=97 => values.push(fg),
            _ => {}
        }
    }
}

fn cells_bg(cell: &Cell, values: &mut Vec<u32>) {
    let bg = cell.bg;
    if bg & COLOUR_FLAG_256 != 0 {
        values.extend([48, 5, bg & 0xff]);
    } else if bg & COLOUR_FLAG_RGB != 0 {
        values.extend([48, 2, (bg >> 16) & 0xff, (bg >> 8) & 0xff, bg & 0xff]);
    } else {
        match bg {
            0..=7 => values.push(bg + 40),
            8 => values.push(49),
            90..=97 => values.push(bg + 10),
            _ => {}
        }
    }
}

fn cells_us(cell: &Cell, values: &mut Vec<u32>) {
    let us = cell.us;
    if us & COLOUR_FLAG_256 != 0 {
        values.extend([58, 5, us & 0xff]);
    } else if us & COLOUR_FLAG_RGB != 0 {
        values.extend([58, 2, (us >> 16) & 0xff, (us >> 8) & 0xff, us & 0xff]);
    }
}

fn add_code(out: &mut Vec<u8>, reset: bool, new: &[u32], old: &[u32]) {
    if new.is_empty() {
        return;
    }
    if !reset && new == old {
        return;
    }
    if reset && (new[0] == 49 || new[0] == 39) {
        return;
    }
    out.extend_from_slice(b"\x1b[");
    let codes: Vec<String> = new.iter().map(u32::to_string).collect();
    out.extend_from_slice(codes.join(";").as_bytes());
    out.push(b'm');
}

fn add_hyperlink(out: &mut Vec<u8>, id: &str, uri: &str) -> bool {
    if uri.len() + id.len() + 17 >= 8192 {
        return false;
    }
    out.extend_from_slice(b"\x1b]8;");
    if id.is_empty() {
        out.push(b';');
    } else {
        out.extend_from_slice(format!("id={id};").as_bytes());
    }
    out.extend_from_slice(uri.as_bytes());
    out.extend_from_slice(b"\x1b\\");
    true
}

/// The codes that take a reader from `last`'s look to `cell`'s.
fn cells_code(
    last: &Cell,
    cell: &Cell,
    out: &mut Vec<u8>,
    links: &Hyperlinks,
    has_link: &mut bool,
) {
    let attr = cell.attr;
    let mut lastattr = last.attr;
    let mut codes: Vec<u32> = Vec::new();
    // If any attribute is removed, begin with 0.
    for (mask, _) in SGR_ATTRS {
        if (!attr & mask != 0 && lastattr & mask != 0)
            || (last.us != COLOUR_DEFAULT && cell.us == COLOUR_DEFAULT)
        {
            codes.push(0);
            lastattr &= ATTR_CHARSET;
            break;
        }
    }
    for (mask, code) in SGR_ATTRS {
        if attr & mask != 0 && lastattr & mask == 0 {
            codes.push(code);
        }
    }
    if !codes.is_empty() {
        out.extend_from_slice(b"\x1b[");
        let printed: Vec<String> = codes
            .iter()
            .map(|&code| {
                if code < 10 {
                    code.to_string()
                } else {
                    format!("{}:{}", code / 10, code % 10)
                }
            })
            .collect();
        out.extend_from_slice(printed.join(";").as_bytes());
        out.push(b'm');
    }
    let reset = codes.first() == Some(&0);
    let (mut new, mut old) = (Vec::new(), Vec::new());
    cells_fg(cell, &mut new);
    cells_fg(last, &mut old);
    add_code(out, reset, &new, &old);
    new.clear();
    old.clear();
    cells_bg(cell, &mut new);
    cells_bg(last, &mut old);
    add_code(out, reset, &new, &old);
    new.clear();
    old.clear();
    cells_us(cell, &mut new);
    cells_us(last, &mut old);
    add_code(out, reset, &new, &old);
    if attr & ATTR_CHARSET != 0 && lastattr & ATTR_CHARSET == 0 {
        out.push(0x0e);
    }
    if attr & ATTR_CHARSET == 0 && lastattr & ATTR_CHARSET != 0 {
        out.push(0x0f);
    }
    if last.link != cell.link {
        if let Some((uri, id)) = links.get(cell.link) {
            *has_link = add_hyperlink(out, id, uri);
        } else if *has_link {
            add_hyperlink(out, "", "");
            *has_link = false;
        }
    }
}

/// Line `py`'s cells as `capture-pane -e -p` prints them, after `last`, the last cell of
/// the line before, which this leaves at its own last cell: every allocated cell, padding
/// skipped, codes before each change, trailing spaces trimmed.
pub fn string_cells(grid: &Grid, py: u32, last: &mut Cell, links: &Hyperlinks) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(line) = grid.peek_line(py) else {
        return out;
    };
    let mut has_link = false;
    let end = line.cellsize().min(grid.sx);
    for xx in 0..end {
        let cell = line.get(xx as usize);
        if cell.flags & FLAG_PADDING != 0 {
            continue;
        }
        cells_code(last, &cell, &mut out, links, &mut has_link);
        if cell.flags & FLAG_TAB != 0 {
            out.push(b'\t');
        } else {
            out.extend_from_slice(cell.data.bytes());
        }
        *last = cell;
    }
    if has_link {
        add_hyperlink(&mut out, "", "");
    }
    while out.last() == Some(&b' ') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_grows_in_quarters_halves_then_the_whole_width() {
        let mut grid = Grid::new(80, 24, 100);
        grid.set_cell(0, 0, &Cell::default());
        assert_eq!(grid.line(0).cellsize(), 20);
        grid.set_cell(25, 0, &Cell::default());
        assert_eq!(grid.line(0).cellsize(), 40);
        grid.set_cell(41, 0, &Cell::default());
        assert_eq!(grid.line(0).cellsize(), 80);
        assert_eq!(grid.line(0).cellused, 42);
    }

    #[test]
    fn a_compact_and_an_extended_cell_read_back_as_written() {
        let mut grid = Grid::new(80, 24, 0);
        let plain = Cell {
            data: Utf8::one(b'x'),
            attr: ATTR_BRIGHT,
            fg: 1 | COLOUR_FLAG_256,
            bg: 4,
            ..Cell::default()
        };
        grid.set_cell(1, 0, &plain);
        assert_eq!(grid.get_cell(1, 0), plain);
        let rich = Cell {
            data: Utf8::new("中".as_bytes(), 2),
            attr: ATTR_STRIKETHROUGH | ATTR_OVERLINE,
            fg: colour_rgb(1, 2, 3),
            us: 9 | COLOUR_FLAG_256,
            link: 3,
            ..Cell::default()
        };
        grid.set_cell(2, 0, &rich);
        assert_eq!(grid.get_cell(2, 0), rich);
        assert_eq!(grid.get_cell(70, 0), Cell::default(), "past the cells");
    }

    #[test]
    fn codes_follow_tmux_order_and_reset_only_when_an_attribute_goes() {
        let links = Hyperlinks::default();
        let mut out = Vec::new();
        let mut has_link = false;
        let red = Cell {
            fg: 1,
            ..Cell::default()
        };
        cells_code(&Cell::default(), &red, &mut out, &links, &mut has_link);
        assert_eq!(out, b"\x1b[31m");
        out.clear();
        cells_code(&red, &Cell::default(), &mut out, &links, &mut has_link);
        assert_eq!(out, b"\x1b[39m", "a colour change alone sets the colour");
        out.clear();
        let bold_red = Cell {
            attr: ATTR_BRIGHT,
            ..red.clone()
        };
        cells_code(&bold_red, &red, &mut out, &links, &mut has_link);
        assert_eq!(out, b"\x1b[0m\x1b[31m", "bold went, so 0 first");
        out.clear();
        let over = Cell {
            attr: ATTR_OVERLINE | ATTR_UNDERSCORE_3,
            ..Cell::default()
        };
        cells_code(&Cell::default(), &over, &mut out, &links, &mut has_link);
        assert_eq!(
            out, b"\x1b[4:3;5:3m",
            "tmux prints 43 and 53 as 4:3 and 5:3"
        );
    }
}
