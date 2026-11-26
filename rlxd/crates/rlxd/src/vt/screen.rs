// Ported from tmux 3.8's screen.c and screen-write.c, whose notice follows.
//
// Copyright (c) 2007 Nicholas Marriott <nicholas.marriott@gmail.com>
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

//! The screen: a grid with a cursor, a scroll region, modes, tab stops and an alternate
//! screen, and the writes tmux's input parser makes to it.
//!
//! This is tmux's screen-write.c without what it does for attached terminals (drawing,
//! the redraw lists, timers): only what lands in the grid, in the same order, since
//! that order decides what `capture-pane` prints. Plain ASCII collects into a run that
//! is written as one, as tmux's does; [`Screen::collect_end`] writes it, and the parser
//! calls that before every byte that isn't printable ASCII.

use super::grid::{
    ATTR_CHARSET, COLOUR_DEFAULT, Cell, FLAG_PADDING, FLAG_TAB, Grid, Hyperlinks, LINE_EXTENDED,
    UTF8_SIZE, Utf8,
};
use super::utf8;

/// The cursor is shown.
pub const MODE_CURSOR: u32 = 0x1;
/// Insert mode (IRM).
pub const MODE_INSERT: u32 = 0x2;
/// Application cursor keys (DECCKM).
pub const MODE_KCURSOR: u32 = 0x4;
/// Application keypad (DECKPAM).
pub const MODE_KKEYPAD: u32 = 0x8;
/// Autowrap (DECAWM).
pub const MODE_WRAP: u32 = 0x10;
/// Mouse: normal tracking.
pub const MODE_MOUSE_STANDARD: u32 = 0x20;
/// Mouse: button-event tracking.
pub const MODE_MOUSE_BUTTON: u32 = 0x40;
/// The cursor blinks.
pub const MODE_CURSOR_BLINKING: u32 = 0x80;
/// Mouse: UTF-8 coordinates.
pub const MODE_MOUSE_UTF8: u32 = 0x100;
/// Mouse: SGR coordinates.
pub const MODE_MOUSE_SGR: u32 = 0x200;
/// Bracketed paste.
pub const MODE_BRACKETPASTE: u32 = 0x400;
/// Focus events.
pub const MODE_FOCUSON: u32 = 0x800;
/// Mouse: any-event tracking.
pub const MODE_MOUSE_ALL: u32 = 0x1000;
/// Origin mode (DECOM).
pub const MODE_ORIGIN: u32 = 0x2000;
/// A line feed is a new line too.
pub const MODE_CRLF: u32 = 0x4000;
/// Extended keys, modifyOtherKeys 1.
pub const MODE_KEYS_EXTENDED: u32 = 0x8000;
/// The cursor is very visible.
pub const MODE_CURSOR_VERY_VISIBLE: u32 = 0x10000;
/// The program set whether the cursor blinks.
pub const MODE_CURSOR_BLINKING_SET: u32 = 0x20000;
/// Extended keys, modifyOtherKeys 2.
pub const MODE_KEYS_EXTENDED_2: u32 = 0x40000;
/// Theme update notifications.
pub const MODE_THEME_UPDATES: u32 = 0x80000;
/// Synchronized output.
pub const MODE_SYNC: u32 = 0x100000;

/// Every mouse mode.
pub const ALL_MOUSE_MODES: u32 = MODE_MOUSE_STANDARD | MODE_MOUSE_BUTTON | MODE_MOUSE_ALL;
/// Both extended-key modes.
pub const EXTENDED_KEY_MODES: u32 = MODE_KEYS_EXTENDED | MODE_KEYS_EXTENDED_2;

/// How tmux's `extended-keys` option is set for the screen: `on`, as the fleet's tmux
/// sets it, so a program that asks for extended keys gets them.
pub const EXTENDED_KEYS: u8 = 1;

/// The run of plain ASCII being collected.
#[derive(Clone, Debug, Default)]
struct Collect {
    cell: Cell,
    data: Vec<u8>,
}

/// A screen.
#[derive(Clone, Debug)]
pub struct Screen {
    /// The grid, history and screen.
    pub grid: Grid,
    saved_grid: Option<Grid>,
    saved_cursor: Option<(u32, u32)>,
    saved_cell: Cell,
    saved_history: bool,
    /// The cursor's column: `sx` while a line's last cell is written and the next
    /// character would wrap.
    pub cx: u32,
    /// The cursor's row.
    pub cy: u32,
    /// The scroll region's top row.
    pub rupper: u32,
    /// The scroll region's bottom row.
    pub rlower: u32,
    /// `MODE_*` bits.
    pub mode: u32,
    tabs: Vec<bool>,
    /// The cursor's style, DECSCUSR's: 0 for the default, then block, underline, bar.
    pub cstyle: u8,
    /// The title the program set.
    pub title: String,
    titles: Vec<String>,
    /// The hyperlinks cells carry.
    pub hyperlinks: Hyperlinks,
    collect: Collect,
}

impl Screen {
    /// The bytes it holds: both grids' cells, the history and the hyperlinks.
    pub fn bytes(&self) -> usize {
        self.grid.bytes()
            + self.saved_grid.as_ref().map_or(0, Grid::bytes)
            + self.hyperlinks.bytes()
    }

    /// A screen `sx` by `sy`, keeping `hlimit` lines of history.
    pub fn new(sx: u32, sy: u32, hlimit: u32) -> Self {
        let mut screen = Self {
            grid: Grid::new(sx, sy, hlimit),
            saved_grid: None,
            saved_cursor: None,
            saved_cell: Cell::default(),
            saved_history: false,
            cx: 0,
            cy: 0,
            rupper: 0,
            rlower: sy - 1,
            mode: MODE_CURSOR,
            tabs: Vec::new(),
            cstyle: 0,
            title: String::new(),
            titles: Vec::new(),
            hyperlinks: Hyperlinks::default(),
            collect: Collect::default(),
        };
        screen.reinit();
        screen
    }

    fn reinit(&mut self) {
        self.cx = 0;
        self.cy = 0;
        self.rupper = 0;
        self.rlower = self.sy() - 1;
        self.mode = MODE_CURSOR | MODE_WRAP | (self.mode & MODE_CRLF);
        if self.is_alternate() {
            self.alternate_off(None, false);
        }
        self.saved_cursor = None;
        self.reset_tabs();
        let (hsize, sy) = (self.grid.hsize, self.grid.sy);
        self.grid.clear_lines(hsize, sy, COLOUR_DEFAULT);
        self.titles.clear();
        self.hyperlinks.reset();
    }

    /// The width.
    pub fn sx(&self) -> u32 {
        self.grid.sx
    }

    /// The height.
    pub fn sy(&self) -> u32 {
        self.grid.sy
    }

    /// Whether the alternate screen is on.
    pub fn is_alternate(&self) -> bool {
        self.saved_grid.is_some()
    }

    fn reset_tabs(&mut self) {
        let sx = self.sx() as usize;
        self.tabs = (0..sx).map(|i| i != 0 && i % 8 == 0).collect();
    }

    /// Whether column `x` is a tab stop.
    pub fn is_tab(&self, x: u32) -> bool {
        self.tabs.get(x as usize).copied().unwrap_or(false)
    }

    /// Sets a tab stop at column `x`.
    pub fn set_tab(&mut self, x: u32) {
        if let Some(stop) = self.tabs.get_mut(x as usize) {
            *stop = true;
        }
    }

    /// Clears the tab stop at column `x`.
    pub fn clear_tab(&mut self, x: u32) {
        if let Some(stop) = self.tabs.get_mut(x as usize) {
            *stop = false;
        }
    }

    /// Clears every tab stop.
    pub fn clear_tabs(&mut self) {
        self.tabs.iter_mut().for_each(|stop| *stop = false);
    }

    /// Sets the title, as OSC 0 and 2 do.
    pub fn set_title(&mut self, title: &str) {
        if utf8::is_valid_title(title) {
            self.title = title.to_owned();
        }
    }

    /// Pushes the title, as XTWINOPS 22 does: at most ten are kept.
    pub fn push_title(&mut self) {
        self.titles.push(self.title.clone());
        if self.titles.len() > 10 {
            self.titles.remove(0);
        }
    }

    /// Pops the title, as XTWINOPS 23 does.
    pub fn pop_title(&mut self) {
        if let Some(title) = self.titles.pop() {
            self.title = title;
        }
    }

    /// Moves the cursor: `None` keeps that coordinate. A column past the width becomes
    /// the last one; the width itself is allowed.
    pub fn set_cursor(&mut self, cx: Option<u32>, cy: Option<u32>) {
        if let Some(cx) = cx {
            self.cx = if cx > self.sx() { self.sx() - 1 } else { cx };
        }
        if let Some(cy) = cy {
            self.cy = cy.min(self.sy() - 1);
        }
    }

    /// RIS: tab stops, the region and the modes back to their defaults, the screen
    /// cleared into the history, the cursor home.
    pub fn reset(&mut self) {
        self.reset_tabs();
        let sy = self.sy();
        self.scroll_region(0, sy - 1);
        self.mode = MODE_CURSOR | MODE_WRAP;
        if EXTENDED_KEYS == 2 {
            self.mode = (self.mode & !EXTENDED_KEY_MODES) | MODE_KEYS_EXTENDED;
        }
        self.clear_screen(COLOUR_DEFAULT);
        self.set_cursor(Some(0), Some(0));
    }

    /// CUU.
    pub fn cursor_up(&mut self, ny: u32) {
        let (mut cx, mut cy) = (self.cx, self.cy);
        let mut ny = ny.max(1);
        if cy < self.rupper {
            ny = ny.min(cy);
        } else {
            ny = ny.min(cy - self.rupper);
        }
        if cx == self.sx() {
            cx -= 1;
        }
        cy -= ny;
        self.set_cursor(Some(cx), Some(cy));
    }

    /// CUD.
    pub fn cursor_down(&mut self, ny: u32) {
        let (mut cx, mut cy) = (self.cx, self.cy);
        let mut ny = ny.max(1);
        if cy > self.rlower {
            ny = ny.min(self.sy() - 1 - cy);
        } else {
            ny = ny.min(self.rlower - cy);
        }
        if cx == self.sx() {
            cx -= 1;
        } else if ny == 0 {
            return;
        }
        cy += ny;
        self.set_cursor(Some(cx), Some(cy));
    }

    /// CUF.
    pub fn cursor_right(&mut self, nx: u32) {
        // tmux's arithmetic is unsigned: at the pending-wrap column, the room left wraps
        // to a huge number, so the cursor moves, and lands on the last column.
        let nx = nx.max(1).min((self.sx() - 1).wrapping_sub(self.cx));
        if nx == 0 {
            return;
        }
        self.set_cursor(Some(self.cx.saturating_add(nx)), Some(self.cy));
    }

    /// CUB.
    pub fn cursor_left(&mut self, nx: u32) {
        let nx = nx.max(1).min(self.cx);
        if nx == 0 {
            return;
        }
        self.set_cursor(Some(self.cx - nx), Some(self.cy));
    }

    /// BS: left one, or to the end of the line above when this one wrapped from it.
    pub fn backspace(&mut self) {
        let (mut cx, mut cy) = (self.cx, self.cy);
        if cx == 0 {
            if cy == 0 {
                return;
            }
            let line = self.grid.line(self.grid.hsize + cy - 1);
            if line.flags & super::grid::LINE_WRAPPED != 0 {
                cy -= 1;
                cx = self.sx() - 1;
            }
        } else {
            cx -= 1;
        }
        self.set_cursor(Some(cx), Some(cy));
    }

    /// DECALN: the screen full of `E`.
    pub fn alignment_test(&mut self) {
        let cell = Cell {
            data: Utf8::one(b'E'),
            ..Cell::default()
        };
        for yy in 0..self.sy() {
            for xx in 0..self.sx() {
                self.grid.view_set_cell(xx, yy, &cell);
            }
        }
        self.set_cursor(Some(0), Some(0));
        self.rupper = 0;
        self.rlower = self.sy() - 1;
    }

    /// ICH.
    pub fn insert_character(&mut self, nx: u32, bg: u32) {
        let nx = nx.max(1).min(self.sx().saturating_sub(self.cx));
        if nx == 0 || self.cx > self.sx() - 1 {
            return;
        }
        self.grid.view_insert_cells(self.cx, self.cy, nx, bg);
    }

    /// DCH.
    pub fn delete_character(&mut self, nx: u32, bg: u32) {
        let nx = nx.max(1).min(self.sx().saturating_sub(self.cx));
        if nx == 0 || self.cx > self.sx() - 1 {
            return;
        }
        self.grid.view_delete_cells(self.cx, self.cy, nx, bg);
    }

    /// ECH.
    pub fn clear_character(&mut self, nx: u32, bg: u32) {
        let nx = nx.max(1).min(self.sx().saturating_sub(self.cx));
        if nx == 0 || self.cx > self.sx() - 1 {
            return;
        }
        self.grid.view_clear(self.cx, self.cy, nx, 1, bg);
    }

    /// IL.
    pub fn insert_line(&mut self, ny: u32, bg: u32) {
        let sy = self.sy();
        let mut ny = ny.max(1);
        if self.cy < self.rupper || self.cy > self.rlower {
            ny = ny.min(sy - self.cy);
            if ny == 0 {
                return;
            }
            self.grid.view_insert_lines(self.cy, ny, bg);
            return;
        }
        ny = ny.min(self.rlower + 1 - self.cy);
        if ny == 0 {
            return;
        }
        self.grid
            .view_insert_lines_region(self.rlower, self.cy, ny, bg);
    }

    /// DL.
    pub fn delete_line(&mut self, ny: u32, bg: u32) {
        let sy = self.sy();
        let mut ny = ny.max(1);
        if self.cy < self.rupper || self.cy > self.rlower {
            ny = ny.min(sy - self.cy);
            if ny == 0 {
                return;
            }
            self.grid.view_delete_lines(self.cy, ny, bg);
            return;
        }
        ny = ny.min(self.rlower + 1 - self.cy);
        if ny == 0 {
            return;
        }
        self.grid
            .view_delete_lines_region(self.rlower, self.cy, ny, bg);
    }

    /// EL 2.
    pub fn clear_line(&mut self, bg: u32) {
        let line = self.grid.line(self.grid.hsize + self.cy);
        if line.cellsize() == 0 && super::grid::colour_is_default(bg) {
            return;
        }
        let sx = self.sx();
        self.grid.view_clear(0, self.cy, sx, 1, bg);
    }

    /// EL 0.
    pub fn clear_end_of_line(&mut self, bg: u32) {
        if self.cx == 0 {
            self.clear_line(bg);
            return;
        }
        let sx = self.sx();
        let line = self.grid.line(self.grid.hsize + self.cy);
        if self.cx > sx - 1 || (self.cx >= line.cellsize() && super::grid::colour_is_default(bg)) {
            return;
        }
        self.grid.view_clear(self.cx, self.cy, sx - self.cx, 1, bg);
    }

    /// EL 1.
    pub fn clear_start_of_line(&mut self, bg: u32) {
        let sx = self.sx();
        if self.cx >= sx - 1 {
            self.clear_line(bg);
            return;
        }
        self.grid.view_clear(0, self.cy, self.cx + 1, 1, bg);
    }

    /// CUP and its kin: `None` keeps that coordinate; `origin` counts rows from the
    /// region's top in origin mode.
    pub fn cursor_move(&mut self, px: Option<u32>, py: Option<u32>, origin: bool) {
        let mut py = py;
        if origin
            && self.mode & MODE_ORIGIN != 0
            && let Some(y) = py
        {
            py = Some(if y > self.rlower - self.rupper {
                self.rlower
            } else {
                y + self.rupper
            });
        }
        let px = px.map(|x| x.min(self.sx() - 1));
        let py = py.map(|y| y.min(self.sy() - 1));
        self.set_cursor(px, py);
    }

    /// RI.
    pub fn reverse_index(&mut self, bg: u32) {
        if self.cy != self.rupper {
            if self.cy > 0 {
                self.set_cursor(None, Some(self.cy - 1));
            }
            return;
        }
        self.grid
            .view_scroll_region_down(self.rupper, self.rlower, bg);
    }

    /// DECSTBM, rows from 0. The cursor goes home.
    pub fn scroll_region(&mut self, rupper: u32, rlower: u32) {
        let last = self.sy() - 1;
        let (rupper, rlower) = (rupper.min(last), rlower.min(last));
        if rupper >= rlower {
            return;
        }
        self.set_cursor(Some(0), Some(0));
        self.rupper = rupper;
        self.rlower = rlower;
    }

    /// LF: down a row, scrolling the region at its bottom. `wrapped` marks the line as
    /// going on in the next.
    pub fn linefeed(&mut self, wrapped: bool, bg: u32) {
        let hsize = self.grid.hsize;
        if wrapped {
            self.grid.line_mut(hsize + self.cy).flags |= super::grid::LINE_WRAPPED;
        }
        if self.cy != self.rlower {
            if self.cy < self.sy() - 1 {
                self.set_cursor(None, Some(self.cy + 1));
            }
            return;
        }
        self.grid
            .view_scroll_region_up(self.rupper, self.rlower, bg);
    }

    /// SU.
    pub fn scroll_up(&mut self, lines: u32, bg: u32) {
        let region = self.rlower - self.rupper + 1;
        let lines = if lines == 0 { 1 } else { lines.min(region) };
        for _ in 0..lines {
            self.grid
                .view_scroll_region_up(self.rupper, self.rlower, bg);
        }
    }

    /// SD.
    pub fn scroll_down(&mut self, lines: u32, bg: u32) {
        let region = self.rlower - self.rupper + 1;
        let lines = if lines == 0 { 1 } else { lines.min(region) };
        for _ in 0..lines {
            self.grid
                .view_scroll_region_down(self.rupper, self.rlower, bg);
        }
    }

    /// CR.
    pub fn carriage_return(&mut self) {
        self.set_cursor(Some(0), None);
    }

    /// ED 0. From the top left, the screen is cleared into the history, as tmux's
    /// `scroll-on-clear` does.
    pub fn clear_end_of_screen(&mut self, bg: u32) {
        let (sx, sy) = (self.sx(), self.sy());
        if self.cx == 0 && self.cy == 0 && self.grid.history {
            self.grid.view_clear_history(bg);
        } else {
            if self.cx < sx {
                self.grid.view_clear(self.cx, self.cy, sx - self.cx, 1, bg);
            }
            self.grid
                .view_clear(0, self.cy + 1, sx, sy - (self.cy + 1), bg);
        }
    }

    /// ED 1.
    pub fn clear_start_of_screen(&mut self, bg: u32) {
        let sx = self.sx();
        if self.cy > 0 {
            self.grid.view_clear(0, 0, sx, self.cy, bg);
        }
        if self.cx > sx - 1 {
            self.grid.view_clear(0, self.cy, sx, 1, bg);
        } else {
            self.grid.view_clear(0, self.cy, self.cx + 1, 1, bg);
        }
    }

    /// ED 2: into the history, as tmux's `scroll-on-clear` does.
    pub fn clear_screen(&mut self, bg: u32) {
        let (sx, sy) = (self.sx(), self.sy());
        if self.grid.history {
            self.grid.view_clear_history(bg);
        } else {
            self.grid.view_clear(0, 0, sx, sy, bg);
        }
    }

    /// ED 3: the history emptied.
    pub fn clear_history(&mut self) {
        self.grid.clear_history();
    }

    /// Writes a cell as tmux's `screen_write_clear_cell` does: the default cell, keeping
    /// the background.
    fn clear_cell(&mut self, px: u32, py: u32) {
        let bg = self.grid.view_get_cell(px, py).bg;
        let cell = Cell {
            bg,
            ..Cell::default()
        };
        self.grid.view_set_cell(px, py, &cell);
    }

    /// Writes the collected run of ASCII, clearing the wide characters it cuts.
    pub fn collect_end(&mut self) {
        if self.collect.data.is_empty() {
            return;
        }
        let (cx, cy) = (self.cx, self.cy);
        if cx != 0 {
            let mut xx = cx;
            let mut cell = Cell::default();
            while xx > 0 {
                cell = self.grid.view_get_cell(xx, cy);
                if cell.flags & FLAG_PADDING == 0 {
                    break;
                }
                self.clear_cell(xx, cy);
                xx -= 1;
            }
            if xx != cx {
                if xx == 0 {
                    cell = self.grid.view_get_cell(0, cy);
                }
                if cell.data.width > 1 || cell.flags & FLAG_PADDING != 0 {
                    self.clear_cell(xx, cy);
                }
            }
        }
        let collect = std::mem::take(&mut self.collect);
        self.grid
            .view_set_cells(cx, cy, &collect.cell, &collect.data);
        self.set_cursor(Some(cx + collect.data.len() as u32), None);
        let sx = self.sx();
        let mut xx = self.cx;
        while xx < sx {
            let cell = self.grid.view_get_cell(xx, self.cy);
            if cell.flags & FLAG_PADDING == 0 {
                break;
            }
            self.clear_cell(xx, self.cy);
            xx += 1;
        }
    }

    /// Writes `cell`: collected with the run when it is plain ASCII, else at once.
    pub fn collect_add(&mut self, cell: &Cell) {
        let sx = self.sx();
        let collect = !(cell.data.width != 1
            || cell.data.size() != 1
            || cell.data.bytes()[0] >= 0x7f
            || cell.flags & FLAG_TAB != 0
            || cell.attr & ATTR_CHARSET != 0
            || self.mode & MODE_WRAP == 0
            || self.mode & MODE_INSERT != 0);
        if !collect {
            self.collect_end();
            self.cell(cell);
            return;
        }
        let used = self.collect.data.len() as u32;
        if self.cx > sx - 1 || used > sx - 1 - self.cx {
            self.collect_end();
        }
        if self.cx > sx - 1 {
            self.linefeed(true, COLOUR_DEFAULT);
            self.set_cursor(Some(0), None);
        }
        if self.collect.data.is_empty() {
            self.collect.cell = cell.clone();
        }
        self.collect.data.push(cell.data.bytes()[0]);
    }

    /// Writes one cell at the cursor, as tmux's `screen_write_cell` does.
    pub fn cell(&mut self, cell: &Cell) {
        if cell.flags & FLAG_PADDING != 0 {
            return;
        }
        if self.combine(cell) {
            return;
        }
        let (sx, sy) = (self.sx(), self.sy());
        let width = u32::from(cell.data.width);
        if self.mode & MODE_WRAP == 0
            && width > 1
            && (width > sx || (self.cx != sx && self.cx > sx.wrapping_sub(width)))
        {
            return;
        }
        let mut skip = true;
        if self.mode & MODE_INSERT != 0 {
            self.grid
                .view_insert_cells(self.cx, self.cy, width, COLOUR_DEFAULT);
            skip = false;
        }
        if self.mode & MODE_WRAP != 0 && self.cx > sx.wrapping_sub(width) {
            self.linefeed(true, COLOUR_DEFAULT);
            self.set_cursor(Some(0), None);
        }
        if self.cx > sx.wrapping_sub(width) || self.cy > sy - 1 {
            return;
        }
        let hsize = self.grid.hsize;
        if self.grid.line(hsize + self.cy).flags & LINE_EXTENDED != 0 {
            let now = self.grid.view_get_cell(self.cx, self.cy);
            if self.overwrite(&now, width) {
                skip = false;
            }
        }
        for xx in self.cx + 1..self.cx + width {
            self.grid.view_set_padding(xx, self.cy, cell.bg);
            skip = false;
        }
        if skip {
            skip = self.grid.line(hsize + self.cy).unchanged_by(self.cx, cell);
        }
        if !skip {
            self.grid.view_set_cell(self.cx, self.cy, cell);
        }
        let not_wrap = u32::from(self.mode & MODE_WRAP == 0);
        if self.cx <= sx.wrapping_sub(not_wrap).wrapping_sub(width) {
            self.set_cursor(Some(self.cx + width), None);
        } else {
            self.set_cursor(Some(sx - not_wrap), None);
        }
    }

    /// Combines a zero-width character, a modifier or a joined one onto the cell before
    /// the cursor. Whether it was taken (combined, or dropped as meaningless alone).
    fn combine(&mut self, cell: &Cell) -> bool {
        let data = &cell.data;
        if utf8::is_hangul_filler(data.bytes()) {
            return true;
        }
        let mut force_wide = false;
        let zero_width = if utf8::is_zwj(data.bytes()) {
            true
        } else if utf8::is_vs(data.bytes()) {
            force_wide = utf8::VARIATION_SELECTOR_ALWAYS_WIDE;
            true
        } else {
            data.width == 0
        };
        let (mut cx, cy) = (self.cx, self.cy);
        if data.size() < 2 || cx == 0 {
            return zero_width;
        }
        let mut n = 1;
        let mut last = self.grid.view_get_cell(cx - n, cy);
        if cx != 1 && last.flags & FLAG_PADDING != 0 {
            n = 2;
            last = self.grid.view_get_cell(cx - n, cy);
        }
        if n != u32::from(last.data.width) || last.flags & FLAG_PADDING != 0 {
            return zero_width;
        }
        if !zero_width {
            if utf8::should_combine(last.data.bytes(), data.bytes())
                || utf8::should_combine(data.bytes(), last.data.bytes())
            {
                force_wide = true;
            } else if !utf8::has_zwj(last.data.bytes()) {
                return false;
            }
        }
        if last.data.size() + data.size() > UTF8_SIZE {
            return zero_width;
        }
        last.data.push(data.bytes());
        if last.data.width == 1 && force_wide {
            last.data.width = 2;
            n = 2;
            cx += 1;
        } else {
            force_wide = false;
        }
        self.grid.view_set_cell(cx - n, cy, &last);
        if force_wide {
            self.grid.view_set_padding(cx - 1, cy, last.bg);
        }
        self.set_cursor(Some(cx), Some(cy));
        true
    }

    /// Clears what a character of `width` written at the cursor over `now` would leave
    /// half drawn. Whether it cleared anything.
    fn overwrite(&mut self, now: &Cell, width: u32) -> bool {
        let (sx, cy) = (self.sx(), self.cy);
        let mut done = false;
        if now.flags & FLAG_PADDING != 0 {
            let mut xx = self.cx;
            while xx > 0 {
                let cell = self.grid.view_get_cell(xx, cy);
                if cell.flags & FLAG_PADDING == 0 {
                    break;
                }
                self.clear_cell(xx, cy);
                xx -= 1;
            }
            // tmux's loop is `xx = cx + 1; while (--xx > 0)`, so it stops at the first
            // non-padding cell or at 0, and clears that one too.
            self.clear_cell(xx, cy);
            done = true;
        }
        if width != 1 || now.data.width != 1 || now.flags & FLAG_PADDING != 0 {
            let mut xx = self.cx + width;
            while xx < sx {
                let cell = self.grid.view_get_cell(xx, cy);
                if cell.flags & FLAG_PADDING == 0 {
                    break;
                }
                self.clear_cell(xx, cy);
                done = true;
                xx += 1;
            }
        }
        done
    }

    /// The alternate screen on: the screen saved, then cleared; with `cursor`, the
    /// cursor and `cell` saved too.
    pub fn alternate_on(&mut self, cell: &Cell, cursor: bool) {
        if self.is_alternate() {
            return;
        }
        let (sx, sy) = (self.sx(), self.sy());
        let mut saved = Grid::new(sx, sy, 0);
        saved.duplicate_lines(0, &self.grid, self.grid.hsize, sy);
        self.saved_grid = Some(saved);
        if cursor {
            self.saved_cursor = Some((self.cx, self.cy));
        }
        self.saved_cell = cell.clone();
        self.grid.view_clear(0, 0, sx, sy, COLOUR_DEFAULT);
        self.saved_history = self.grid.history;
        self.grid.history = false;
    }

    /// The alternate screen off: the saved screen back; with `cursor`, the cursor and
    /// the cell (into `cell`) back too.
    pub fn alternate_off(&mut self, cell: Option<&mut Cell>, cursor: bool) {
        let (sx, sy) = (self.sx(), self.sy());
        if let Some(saved) = &self.saved_grid {
            let (ox, oy) = (saved.sx, saved.sy);
            self.resize(ox, oy, false);
        }
        if cursor && let Some((x, y)) = self.saved_cursor {
            // rlxd's, not tmux's: the cursor was saved on the screen of its time, which a
            // resize since may have made shorter or narrower (by ?47h, which doesn't save
            // it anew). tmux restores it as it was and reads past its lines.
            self.cx = x.min(self.sx());
            self.cy = y.min(self.sy() - 1);
            if let Some(cell) = cell {
                *cell = self.saved_cell.clone();
            }
        }
        let Some(saved) = self.saved_grid.take() else {
            self.cx = self.cx.min(self.sx() - 1);
            self.cy = self.cy.min(self.sy() - 1);
            return;
        };
        let hsize = self.grid.hsize;
        self.grid.duplicate_lines(hsize, &saved, 0, saved.sy);
        if self.saved_history {
            self.grid.history = true;
        }
        self.resize(sx, sy, true);
        self.cx = self.cx.min(self.sx() - 1);
        self.cy = self.cy.min(self.sy() - 1);
    }

    /// Resizes to `sx` by `sy`, as tmux's `screen_resize` does: rows come off the bottom
    /// down to the cursor, then go into the history; a taller screen takes back scrolled
    /// history first. With `reflow`, wrapped lines rewrap to the new width.
    pub fn resize(&mut self, sx: u32, sy: u32, reflow: bool) {
        let (sx, sy) = (sx.max(1), sy.max(1));
        // The cursor is always on the screen, but resizing counts on it: make sure.
        self.cy = self.cy.min(self.sy() - 1);
        let mut cx = self.cx;
        let mut cy = self.grid.hsize + self.cy;
        let mut reflow = reflow;
        if sx != self.sx() {
            self.grid.sx = sx;
            self.reset_tabs();
        } else {
            reflow = false;
        }
        if sy != self.sy() {
            self.resize_y(sy, true, &mut cy);
        }
        if reflow {
            self.reflow(sx, &mut cx, &mut cy);
        }
        if cy >= self.grid.hsize {
            self.cx = cx;
            self.cy = cy - self.grid.hsize;
        } else {
            self.cx = 0;
            self.cy = 0;
        }
    }

    fn resize_y(&mut self, sy: u32, eat_empty: bool, cy: &mut u32) {
        let oldy = self.sy();
        if sy < oldy {
            let mut needed = oldy - sy;
            if eat_empty {
                let mut available = oldy - 1 - self.cy;
                if available > 0 {
                    available = available.min(needed);
                    self.grid
                        .view_delete_lines(oldy - available, available, COLOUR_DEFAULT);
                }
                needed -= available;
            }
            let available = self.cy;
            if self.grid.history {
                self.grid.hscrolled += needed;
                self.grid.hsize += needed;
            } else if needed > 0 && available > 0 {
                let available = available.min(needed);
                self.grid.view_delete_lines(0, available, COLOUR_DEFAULT);
                *cy -= available;
            }
        }
        let total = self.grid.hsize + sy;
        self.grid.adjust_lines(total);
        if sy > oldy {
            let mut needed = sy - oldy;
            let mut available = self.grid.hscrolled;
            if self.grid.history && available > 0 {
                available = available.min(needed);
                self.grid.hscrolled -= available;
                self.grid.hsize -= available;
            } else {
                available = 0;
            }
            needed -= available;
            for i in self.grid.hsize + sy - needed..self.grid.hsize + sy {
                self.grid.empty_line(i, COLOUR_DEFAULT);
            }
        }
        self.grid.sy = sy;
        self.rupper = 0;
        self.rlower = sy - 1;
    }

    /// Rewraps the lines to `sx`, keeping the cursor where its character went.
    fn reflow(&mut self, sx: u32, cx: &mut u32, cy: &mut u32) {
        let (wx, wy) = self.grid.wrap_position(*cx, *cy);
        self.grid.reflow(sx);
        let (x, y) = self.grid.unwrap_position(wx, wy);
        *cx = x;
        *cy = y;
    }
}
