// Ported from tmux 3.8's input.c, whose notice follows.
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

//! The input parser: tmux's state machine over the bytes a program writes, and what each
//! control, escape and control sequence does to the screen.
//!
//! This is tmux's own machine, not a generic one, because tmux's frame depends on its
//! details: a missing parameter is not a zero (`CSI 1;r` sets the region to the bottom),
//! at most 23 parameters in 63 bytes are read, a bad UTF-8 sequence is one U+FFFD, and
//! only the sequences tmux knows change anything. Replies to queries go back to the
//! program, as tmux sends them, except where tmux asks an attached terminal: with none
//! attached, as here, tmux gives no colour replies either.

use super::grid::{
    ATTR_ALL_UNDERSCORE, ATTR_BLINK, ATTR_BRIGHT, ATTR_CHARSET, ATTR_DIM, ATTR_HIDDEN,
    ATTR_ITALICS, ATTR_OVERLINE, ATTR_REVERSE, ATTR_STRIKETHROUGH, ATTR_UNDERSCORE,
    ATTR_UNDERSCORE_2, ATTR_UNDERSCORE_3, ATTR_UNDERSCORE_4, ATTR_UNDERSCORE_5, COLOUR_DEFAULT,
    COLOUR_FLAG_256, Cell, Utf8, colour_rgb,
};
use super::screen::{
    ALL_MOUSE_MODES, EXTENDED_KEY_MODES, EXTENDED_KEYS, MODE_BRACKETPASTE, MODE_CRLF, MODE_CURSOR,
    MODE_CURSOR_BLINKING, MODE_CURSOR_BLINKING_SET, MODE_CURSOR_VERY_VISIBLE, MODE_FOCUSON,
    MODE_INSERT, MODE_KCURSOR, MODE_KEYS_EXTENDED, MODE_KEYS_EXTENDED_2, MODE_KKEYPAD,
    MODE_MOUSE_ALL, MODE_MOUSE_BUTTON, MODE_MOUSE_SGR, MODE_MOUSE_STANDARD, MODE_MOUSE_UTF8,
    MODE_ORIGIN, MODE_SYNC, MODE_THEME_UPDATES, MODE_WRAP, Screen,
};
use super::utf8::{Decoder, Step};

use std::time::{Duration, Instant};

/// The most bytes an OSC, DCS or APC string holds, as tmux's `input-buffer-size`.
const INPUT_BUFFER_SIZE: usize = 1 << 20;

/// How long a string (DCS, OSC, APC, a rename) may stay open before the parser gives up
/// on it and goes back to ground: tmux's ground timer.
pub const GROUND_TIMER: Duration = Duration::from_secs(5);
/// The parameter bytes kept: tmux's 64-byte buffer, less its NUL.
const PARAM_BUF: usize = 63;
/// The intermediate bytes kept: tmux's 4-byte buffer, less its NUL.
const INTERM_BUF: usize = 3;
/// The parameter slots: tmux's 24, of which a sequence may fill 23.
const PARAM_LIST: usize = 24;
/// tmux's pixel size for a cell, when no client says.
const XPIXEL: u32 = 16;
const YPIXEL: u32 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    EscEnter,
    EscIntermediate,
    CsiEnter,
    CsiParameter,
    CsiIntermediate,
    CsiIgnore,
    DcsEnter,
    DcsParameter,
    DcsIntermediate,
    DcsHandler,
    DcsEscape,
    DcsIgnore,
    OscString,
    ApcString,
    RenameString,
    ConsumeSt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handler {
    None,
    C0,
    Print,
    TopBit,
    Intermediate,
    Parameter,
    Input,
    Esc,
    Csi,
    Dcs,
    EndBel,
}

/// The bytes every state but the DCS string's handles alike: CAN, SUB and ESC.
fn anywhere(ch: u8) -> Option<(Handler, Option<State>)> {
    match ch {
        0x18 | 0x1a => Some((Handler::C0, Some(State::Ground))),
        0x1b => Some((Handler::None, Some(State::EscEnter))),
        _ => None,
    }
}

/// The C0 controls a state passes to `on`: all but CAN, SUB and ESC.
fn c0(ch: u8) -> bool {
    matches!(ch, 0x00..=0x17 | 0x19 | 0x1c..=0x1f)
}

/// tmux's transition tables: what `ch` does in `state`, and the state after it.
fn transition(state: State, ch: u8) -> (Handler, Option<State>) {
    use Handler as H;
    use State as S;
    if !matches!(state, S::DcsHandler | S::DcsEscape)
        && let Some(found) = anywhere(ch)
    {
        return found;
    }
    match state {
        S::Ground => match ch {
            _ if c0(ch) => (H::C0, None),
            0x20..=0x7e => (H::Print, None),
            0x7f => (H::None, None),
            _ => (H::TopBit, None),
        },
        S::EscEnter => match ch {
            _ if c0(ch) => (H::C0, None),
            0x20..=0x2f => (H::Intermediate, Some(S::EscIntermediate)),
            0x50 => (H::None, Some(S::DcsEnter)),
            0x58 | 0x5e => (H::None, Some(S::ConsumeSt)),
            0x5b => (H::None, Some(S::CsiEnter)),
            0x5d => (H::None, Some(S::OscString)),
            0x5f => (H::None, Some(S::ApcString)),
            0x6b => (H::None, Some(S::RenameString)),
            0x30..=0x7e => (H::Esc, Some(S::Ground)),
            _ => (H::None, None),
        },
        S::EscIntermediate => match ch {
            _ if c0(ch) => (H::C0, None),
            0x20..=0x2f => (H::Intermediate, None),
            0x30..=0x7e => (H::Esc, Some(S::Ground)),
            _ => (H::None, None),
        },
        S::CsiEnter => match ch {
            _ if c0(ch) => (H::C0, None),
            0x20..=0x2f => (H::Intermediate, Some(S::CsiIntermediate)),
            0x30..=0x3b => (H::Parameter, Some(S::CsiParameter)),
            0x3c..=0x3f => (H::Intermediate, Some(S::CsiParameter)),
            0x40..=0x7e => (H::Csi, Some(S::Ground)),
            _ => (H::None, None),
        },
        S::CsiParameter => match ch {
            _ if c0(ch) => (H::C0, None),
            0x20..=0x2f => (H::Intermediate, Some(S::CsiIntermediate)),
            0x30..=0x3b => (H::Parameter, None),
            0x3c..=0x3f => (H::None, Some(S::CsiIgnore)),
            0x40..=0x7e => (H::Csi, Some(S::Ground)),
            _ => (H::None, None),
        },
        S::CsiIntermediate => match ch {
            _ if c0(ch) => (H::C0, None),
            0x20..=0x2f => (H::Intermediate, None),
            0x30..=0x3f => (H::None, Some(S::CsiIgnore)),
            0x40..=0x7e => (H::Csi, Some(S::Ground)),
            _ => (H::None, None),
        },
        S::CsiIgnore => match ch {
            _ if c0(ch) => (H::C0, None),
            0x40..=0x7e => (H::None, Some(S::Ground)),
            _ => (H::None, None),
        },
        S::DcsEnter => match ch {
            _ if c0(ch) => (H::None, None),
            0x20..=0x2f => (H::Intermediate, Some(S::DcsIntermediate)),
            0x30..=0x39 | 0x3b => (H::Parameter, Some(S::DcsParameter)),
            0x3a => (H::None, Some(S::DcsIgnore)),
            0x3c..=0x3f => (H::Intermediate, Some(S::DcsParameter)),
            0x40..=0x7e => (H::Input, Some(S::DcsHandler)),
            _ => (H::None, None),
        },
        S::DcsParameter => match ch {
            _ if c0(ch) => (H::None, None),
            0x20..=0x2f => (H::Intermediate, Some(S::DcsIntermediate)),
            0x30..=0x39 | 0x3b => (H::Parameter, None),
            0x3a | 0x3c..=0x3f => (H::None, Some(S::DcsIgnore)),
            0x40..=0x7e => (H::Input, Some(S::DcsHandler)),
            _ => (H::None, None),
        },
        S::DcsIntermediate => match ch {
            _ if c0(ch) => (H::None, None),
            0x20..=0x2f => (H::Intermediate, None),
            0x30..=0x3f => (H::None, Some(S::DcsIgnore)),
            0x40..=0x7e => (H::Input, Some(S::DcsHandler)),
            _ => (H::None, None),
        },
        S::DcsHandler => match ch {
            0x1b => (H::None, Some(S::DcsEscape)),
            _ => (H::Input, None),
        },
        S::DcsEscape => match ch {
            0x5c => (H::Dcs, Some(S::Ground)),
            _ => (H::Input, Some(S::DcsHandler)),
        },
        S::DcsIgnore | S::ConsumeSt => (H::None, None),
        S::OscString => match ch {
            0x07 => (H::EndBel, Some(S::Ground)),
            0x20..=0xff => (H::Input, None),
            _ => (H::None, None),
        },
        S::ApcString | S::RenameString => match ch {
            0x20..=0xff => (H::Input, None),
            _ => (H::None, None),
        },
    }
}

/// One parameter of a control sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Param {
    Missing,
    Number(i32),
    /// One with `:` sub-parameters, as its text.
    Text(String),
}

/// The pen: the cell characters are written with, and the character sets.
#[derive(Clone, Debug, Default)]
struct Pen {
    cell: Cell,
    /// 0 for G0, 1 for G1.
    set: u8,
    g0_charset: bool,
    g1_charset: bool,
}

/// How a string ended: ST, or BEL, which a colour reply answers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    St,
    Bel,
}

/// The parser's state across writes.
#[derive(Clone, Debug)]
pub struct Input {
    state: State,
    ch: u8,
    interm: Vec<u8>,
    params: Vec<u8>,
    list: Vec<Param>,
    string: Vec<u8>,
    end: End,
    discard: bool,
    last_printed: bool,
    pen: Pen,
    saved_pen: Pen,
    saved_cursor: (u32, u32),
    saved_mode: u32,
    last: Utf8,
    utf8_started: bool,
    utf8: Decoder,
    version: String,
    /// When the open string began, for [`GROUND_TIMER`]; `None` when none is open.
    string_since: Option<Instant>,
    /// When the bytes being parsed came.
    now: Instant,
}

impl Input {
    /// The bytes its buffers hold: a sequence's parameters, and a string's, up to
    /// [`INPUT_BUFFER_SIZE`].
    pub fn bytes(&self) -> usize {
        let texts: usize = self
            .list
            .iter()
            .map(|param| match param {
                Param::Text(text) => text.capacity(),
                _ => 0,
            })
            .sum();
        self.string.capacity()
            + self.params.capacity()
            + self.interm.capacity()
            + self.list.capacity() * std::mem::size_of::<Param>()
            + texts
    }

    /// A parser in the ground state, with the default pen. `version` answers XTVERSION.
    pub fn new(version: &str) -> Self {
        Self {
            state: State::Ground,
            ch: 0,
            interm: Vec::new(),
            params: Vec::new(),
            list: Vec::new(),
            string: Vec::new(),
            end: End::St,
            discard: false,
            last_printed: false,
            pen: Pen::default(),
            saved_pen: Pen::default(),
            saved_cursor: (0, 0),
            saved_mode: 0,
            last: Utf8::one(b' '),
            utf8_started: false,
            utf8: Decoder::default(),
            version: version.to_owned(),
            string_since: None,
            now: Instant::now(),
        }
    }

    /// Feeds `bytes` to the screen, adding any replies to `replies`, then writes what
    /// was collected, as tmux does at the end of each read.
    ///
    /// `now` is when they came. tmux gives up on a string open [`GROUND_TIMER`] or more
    /// when its timer fires; rlxd does when the next bytes come, which a capture can't
    /// tell apart.
    pub fn parse(
        &mut self,
        screen: &mut Screen,
        bytes: &[u8],
        replies: &mut Vec<u8>,
        now: Instant,
    ) {
        self.now = now;
        if self
            .string_since
            .is_some_and(|since| now.saturating_duration_since(since) >= GROUND_TIMER)
        {
            // input_ground_timer_callback: input_reset(ictx, 0).
            self.reset_pen();
            self.clear();
            self.state = State::Ground;
            self.last_printed = false;
        }
        for &ch in bytes {
            self.ch = ch;
            let (handler, next) = transition(self.state, ch);
            if handler != Handler::Print {
                screen.collect_end();
            }
            self.run(handler, screen, replies);
            if let Some(next) = next {
                self.set_state(next, screen, replies);
            }
        }
        screen.collect_end();
    }

    fn set_state(&mut self, next: State, screen: &mut Screen, replies: &mut Vec<u8>) {
        match self.state {
            State::OscString => self.exit_osc(screen),
            State::ApcString => self.exit_apc(screen),
            _ => {}
        }
        let _ = replies;
        self.state = next;
        match next {
            State::EscEnter | State::CsiEnter => self.clear(),
            State::DcsEnter
            | State::OscString
            | State::ApcString
            | State::RenameString
            | State::ConsumeSt => {
                self.clear();
                self.string_since = Some(self.now);
                self.last_printed = false;
            }
            State::Ground => self.string_since = None,
            _ => {}
        }
    }

    fn clear(&mut self) {
        self.string_since = None;
        self.interm.clear();
        self.params.clear();
        self.string.clear();
        self.end = End::St;
        self.discard = false;
    }

    fn run(&mut self, handler: Handler, screen: &mut Screen, replies: &mut Vec<u8>) {
        match handler {
            Handler::None => {}
            Handler::C0 => self.c0_dispatch(screen),
            Handler::Print => self.print(screen),
            Handler::TopBit => self.top_bit_set(screen),
            Handler::Intermediate => {
                if self.interm.len() == INTERM_BUF {
                    self.discard = true;
                } else {
                    self.interm.push(self.ch);
                }
            }
            Handler::Parameter => {
                if self.params.len() == PARAM_BUF {
                    self.discard = true;
                } else {
                    self.params.push(self.ch);
                }
            }
            Handler::Input => self.input(),
            Handler::Esc => self.esc_dispatch(screen),
            Handler::Csi => self.csi_dispatch(screen, replies),
            Handler::Dcs => self.dcs_dispatch(screen, replies),
            Handler::EndBel => self.end = End::Bel,
        }
    }

    fn input(&mut self) {
        // tmux doubles its buffer up to the limit, so a string stops one byte short of it.
        if self.string.len() + 1 >= INPUT_BUFFER_SIZE {
            self.discard = true;
            return;
        }
        self.string.push(self.ch);
    }

    /// Ends a UTF-8 character cut short: one U+FFFD in its place.
    fn stop_utf8(&mut self, screen: &mut Screen) {
        if self.utf8_started {
            self.pen.cell.data = Utf8::new("\u{fffd}".as_bytes(), 1);
            screen.collect_add(&self.pen.cell);
        }
        self.utf8_started = false;
    }

    fn charset_attr(&mut self) {
        let set = if self.pen.set == 0 {
            self.pen.g0_charset
        } else {
            self.pen.g1_charset
        };
        if set {
            self.pen.cell.attr |= ATTR_CHARSET;
        } else {
            self.pen.cell.attr &= !ATTR_CHARSET;
        }
    }

    fn print(&mut self, screen: &mut Screen) {
        self.stop_utf8(screen);
        self.charset_attr();
        self.pen.cell.data = Utf8::one(self.ch);
        screen.collect_add(&self.pen.cell);
        self.last = self.pen.cell.data;
        self.last_printed = true;
        self.pen.cell.attr &= !ATTR_CHARSET;
    }

    fn top_bit_set(&mut self, screen: &mut Screen) {
        self.last_printed = false;
        if !self.utf8_started {
            self.utf8_started = true;
            if self.utf8.open(self.ch) != Step::More {
                self.stop_utf8(screen);
            }
            return;
        }
        match self.utf8.append(self.ch) {
            Step::More => return,
            Step::Error => {
                self.stop_utf8(screen);
                return;
            }
            Step::Done => {}
        }
        self.utf8_started = false;
        self.pen.cell.data = Utf8::new(self.utf8.bytes(), self.utf8.width());
        screen.collect_add(&self.pen.cell);
        self.last = self.pen.cell.data;
        self.last_printed = true;
    }

    fn c0_dispatch(&mut self, screen: &mut Screen) {
        self.stop_utf8(screen);
        match self.ch {
            0x08 => screen.backspace(),
            0x09 => self.tab(screen),
            0x0a..=0x0c => {
                screen.linefeed(false, self.pen.cell.bg);
                if screen.mode & MODE_CRLF != 0 {
                    screen.carriage_return();
                }
            }
            0x0d => screen.carriage_return(),
            0x0e => self.pen.set = 1,
            0x0f => self.pen.set = 0,
            _ => {}
        }
        self.last_printed = false;
    }

    /// HT: to the next stop, as one tab cell over blank cells that look alike, else as
    /// a cursor move.
    fn tab(&mut self, screen: &mut Screen) {
        let sx = screen.sx();
        let mut cx = screen.cx;
        if cx >= sx - 1 {
            return;
        }
        let line = screen.cy + screen.grid.hsize;
        let first = screen.grid.get_cell(cx, line);
        let mut has_content = false;
        loop {
            if !has_content {
                let cell = screen.grid.get_cell(cx, line);
                if cell.data.bytes() != b" " || !cell.looks_equal(&first) {
                    has_content = true;
                }
            }
            cx += 1;
            if screen.is_tab(cx) || cx >= sx - 1 {
                break;
            }
        }
        let width = cx - screen.cx;
        if has_content || width as usize > super::grid::UTF8_SIZE {
            screen.cx = cx;
        } else {
            let mut cell = screen.grid.get_cell(screen.cx, line);
            cell.set_tab(width as u8);
            screen.collect_add(&cell);
        }
    }

    fn esc_dispatch(&mut self, screen: &mut Screen) {
        if self.discard {
            return;
        }
        let bg = self.pen.cell.bg;
        match (self.ch, self.interm.as_slice()) {
            (b'c', b"") => {
                self.reset_pen();
                screen.reset();
            }
            (b'D', b"") => screen.linefeed(false, bg),
            (b'E', b"") => {
                screen.carriage_return();
                screen.linefeed(false, bg);
            }
            (b'H', b"") => {
                if screen.cx < screen.sx() {
                    screen.set_tab(screen.cx);
                }
            }
            (b'M', b"") => screen.reverse_index(bg),
            (b'=', b"") => screen.mode |= MODE_KKEYPAD,
            (b'>', b"") => screen.mode &= !MODE_KKEYPAD,
            (b'7', b"") => self.save_state(screen),
            (b'8', b"") => self.restore_state(screen),
            (b'8', b"#") => screen.alignment_test(),
            (b'0', b"(") => self.pen.g0_charset = true,
            (b'B', b"(") => self.pen.g0_charset = false,
            (b'0', b")") => self.pen.g1_charset = true,
            (b'B', b")") => self.pen.g1_charset = false,
            // tmux returns before its end for a sequence it doesn't know, so REP still
            // repeats the character before it.
            (b'\\', b"") => {}
            _ => return,
        }
        self.last_printed = false;
    }

    fn reset_pen(&mut self) {
        self.pen = Pen::default();
        self.saved_pen = Pen::default();
        self.saved_cursor = (0, 0);
    }

    fn save_state(&mut self, screen: &Screen) {
        self.saved_pen = self.pen.clone();
        self.saved_cursor = (screen.cx, screen.cy);
        self.saved_mode = screen.mode;
    }

    fn restore_state(&mut self, screen: &mut Screen) {
        self.pen = self.saved_pen.clone();
        if self.saved_mode & MODE_ORIGIN != 0 {
            screen.mode |= MODE_ORIGIN;
        } else {
            screen.mode &= !MODE_ORIGIN;
        }
        let (x, y) = self.saved_cursor;
        screen.cursor_move(Some(x), Some(y), false);
    }

    /// Splits the parameters as tmux's `input_split` does. `false` when they can't be
    /// read: a number past `i32::MAX`, or too many.
    fn split(&mut self) -> bool {
        self.list.clear();
        if self.params.is_empty() {
            return true;
        }
        let text = String::from_utf8_lossy(&self.params).into_owned();
        for out in text.split(';') {
            let param = if out.is_empty() {
                Param::Missing
            } else if out.contains(':') {
                Param::Text(out.to_owned())
            } else {
                match out.parse::<i64>() {
                    Ok(n) if (0..=i64::from(i32::MAX)).contains(&n) => Param::Number(n as i32),
                    _ => return false,
                }
            };
            self.list.push(param);
            if self.list.len() == PARAM_LIST {
                return false;
            }
        }
        true
    }

    /// Parameter `index`: `default` when missing, at least `min`, and -1 for one with
    /// sub-parameters.
    fn get(&self, index: usize, min: i32, default: i32) -> i32 {
        match self.list.get(index) {
            None | Some(Param::Missing) => default,
            Some(Param::Text(_)) => -1,
            Some(Param::Number(n)) => (*n).max(min),
        }
    }

    fn csi_dispatch(&mut self, screen: &mut Screen, replies: &mut Vec<u8>) {
        if self.discard || !self.split() {
            return;
        }
        let bg = self.pen.cell.bg;
        let n1 = |input: &Self| input.get(0, 1, 1);
        let positive = |n: i32| u32::try_from(n).ok();
        match (self.ch, self.interm.as_slice()) {
            (b'Z', b"") => {
                let mut cx = screen.cx.min(screen.sx() - 1);
                let mut n = n1(self);
                if n == -1 {
                    return self.done();
                }
                while cx > 0 && n > 0 {
                    n -= 1;
                    loop {
                        cx -= 1;
                        if cx == 0 || screen.is_tab(cx) {
                            break;
                        }
                    }
                }
                screen.cx = cx;
            }
            (b'D', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.cursor_left(n);
                }
            }
            (b'B', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.cursor_down(n);
                }
            }
            (b'C', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.cursor_right(n);
                }
            }
            (b'H' | b'f', b"") => {
                let (n, m) = (self.get(0, 1, 1), self.get(1, 1, 1));
                if n != -1 && m != -1 {
                    screen.cursor_move(Some(m as u32 - 1), Some(n as u32 - 1), true);
                }
            }
            (b'm', b">") => {
                if self.get(0, 0, 0) != 4 {
                    return self.done();
                }
                let m = self.get(1, 0, 0);
                if EXTENDED_KEYS == 0 {
                    return self.done();
                }
                screen.mode &= !EXTENDED_KEY_MODES;
                if m == 2 {
                    screen.mode |= MODE_KEYS_EXTENDED_2;
                } else if m == 1 || EXTENDED_KEYS == 2 {
                    screen.mode |= MODE_KEYS_EXTENDED;
                }
            }
            (b'n', b">") => {
                if self.get(0, 0, 0) != 4 {
                    return self.done();
                }
                screen.mode &= !(MODE_KEYS_EXTENDED | MODE_KEYS_EXTENDED_2);
                if EXTENDED_KEYS == 2 {
                    screen.mode |= MODE_KEYS_EXTENDED;
                }
            }
            (b't', b"") => self.winops(screen, replies),
            (b'A', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.cursor_up(n);
                }
            }
            (b'E', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.carriage_return();
                    screen.cursor_down(n);
                }
            }
            (b'F', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.carriage_return();
                    screen.cursor_up(n);
                }
            }
            (b'c', b"") => {
                if self.get(0, 0, 0) == 0 {
                    replies.extend_from_slice(b"\x1b[?1;2c");
                }
            }
            (b'c', b">") => {
                if self.get(0, 0, 0) == 0 {
                    replies.extend_from_slice(b"\x1b[>84;0;0c");
                }
            }
            (b'X', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.clear_character(n, bg);
                }
            }
            (b'P', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.delete_character(n, bg);
                }
            }
            (b'r', b"") => {
                let n = self.get(0, 1, 1);
                let m = self.get(1, 1, screen.sy() as i32);
                if n != -1 && m != -1 {
                    screen.scroll_region(n as u32 - 1, m as u32 - 1);
                }
            }
            (b'M', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.delete_line(n, bg);
                }
            }
            (b'p', b"$") => {
                let m = self.get(0, 0, 0);
                let n = if m == 4 {
                    if screen.mode & MODE_INSERT != 0 { 1 } else { 2 }
                } else {
                    0
                };
                if m > 0 {
                    replies.extend_from_slice(format!("\x1b[{m};{n}$y").as_bytes());
                }
            }
            (b'p', b"?$") => {
                let m = self.get(0, 0, 0);
                let on = |mask: u32| if screen.mode & mask != 0 { 1 } else { 2 };
                let n = match m {
                    1 => on(MODE_KCURSOR),
                    3 => 4,
                    6 => on(MODE_ORIGIN),
                    7 => on(MODE_WRAP),
                    12 => {
                        if screen.cstyle != 0 || screen.mode & MODE_CURSOR_BLINKING_SET != 0 {
                            on(MODE_CURSOR_BLINKING)
                        } else {
                            2
                        }
                    }
                    25 => on(MODE_CURSOR),
                    47 | 1047 | 1049 => {
                        if screen.is_alternate() {
                            1
                        } else {
                            2
                        }
                    }
                    1000 => on(MODE_MOUSE_STANDARD),
                    1002 => on(MODE_MOUSE_BUTTON),
                    1003 => on(MODE_MOUSE_ALL),
                    1004 => on(MODE_FOCUSON),
                    1005 => on(MODE_MOUSE_UTF8),
                    1006 => on(MODE_MOUSE_SGR),
                    2004 => on(MODE_BRACKETPASTE),
                    2026 => on(MODE_SYNC),
                    2031 => on(MODE_THEME_UPDATES),
                    _ => 0,
                };
                if m > 0 {
                    replies.extend_from_slice(format!("\x1b[?{m};{n}$y").as_bytes());
                }
            }
            (b'n', b"") => match self.get(0, 0, 0) {
                5 => replies.extend_from_slice(b"\x1b[0n"),
                6 => replies.extend_from_slice(
                    format!("\x1b[{};{}R", screen.cy + 1, screen.cx + 1).as_bytes(),
                ),
                _ => {}
            },
            (b'J', b"") => match self.get(0, 0, 0) {
                0 => screen.clear_end_of_screen(bg),
                1 => screen.clear_start_of_screen(bg),
                2 => screen.clear_screen(bg),
                3 if self.get(1, 0, 0) == 0 => screen.clear_history(),
                _ => {}
            },
            (b'K', b"") => match self.get(0, 0, 0) {
                0 => screen.clear_end_of_line(bg),
                1 => screen.clear_start_of_line(bg),
                2 => screen.clear_line(bg),
                _ => {}
            },
            (b'G' | b'`', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.cursor_move(Some(n - 1), None, true);
                }
            }
            (b'@', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.insert_character(n, bg);
                }
            }
            (b'L', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.insert_line(n, bg);
                }
            }
            (b'b', b"") => {
                let n = n1(self);
                if n == -1 || !self.last_printed {
                    return self.done();
                }
                let room = screen.sx().saturating_sub(screen.cx);
                let n = (n as u32).min(room);
                self.charset_attr();
                self.pen.cell.data = self.last;
                for _ in 0..n {
                    screen.collect_add(&self.pen.cell);
                }
            }
            (b'u', b"") => self.restore_state(screen),
            (b'l', b"") => self.rm(screen),
            (b'l', b"?") => self.rm_private(screen),
            (b's', b"") => self.save_state(screen),
            (b'm', b"") => self.sgr(),
            (b'h', b"") => self.sm(screen),
            (b'h', b"?") => self.sm_private(screen),
            (b'S', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.scroll_up(n, bg);
                }
            }
            (b'T', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.scroll_down(n, bg);
                }
            }
            (b'g', b"") => match self.get(0, 0, 0) {
                0 => {
                    if screen.cx < screen.sx() {
                        screen.clear_tab(screen.cx);
                    }
                }
                3 => screen.clear_tabs(),
                _ => {}
            },
            (b'd', b"") => {
                if let Some(n) = positive(n1(self)) {
                    screen.cursor_move(None, Some(n - 1), true);
                }
            }
            (b'q', b" ") => {
                let n = self.get(0, 0, 0);
                if n == -1 {
                    return self.done();
                }
                set_cursor_style(screen, n);
                if n == 0 {
                    screen.mode &= !MODE_CURSOR_BLINKING_SET;
                }
            }
            (b'q', b">") => {
                if self.get(0, 0, 0) == 0 {
                    replies.extend_from_slice(
                        format!("\x1bP>|rlxd {}\x1b\\", self.version).as_bytes(),
                    );
                }
            }
            // DSR's private form: 996 asks for the colour theme, which tmux reports from
            // the terminal of the client it is attached to. rlxd has none, as tmux has
            // none with no client, so neither replies.
            (b'n', b"?") => {}
            // XTSMGRAPHICS: tmux answers it only when built with sixel, which rlxd
            // doesn't draw.
            (b'S', b"?") => {}
            // Unknown: tmux returns before its end, keeping REP's last character.
            _ => return,
        }
        self.done();
    }

    fn done(&mut self) {
        self.last_printed = false;
    }

    fn rm(&mut self, screen: &mut Screen) {
        for i in 0..self.list.len() {
            match self.get(i, 0, -1) {
                4 => screen.mode &= !MODE_INSERT,
                34 => screen.mode |= MODE_CURSOR_VERY_VISIBLE,
                _ => {}
            }
        }
    }

    fn sm(&mut self, screen: &mut Screen) {
        for i in 0..self.list.len() {
            match self.get(i, 0, -1) {
                4 => screen.mode |= MODE_INSERT,
                34 => screen.mode &= !MODE_CURSOR_VERY_VISIBLE,
                _ => {}
            }
        }
    }

    fn rm_private(&mut self, screen: &mut Screen) {
        for i in 0..self.list.len() {
            match self.get(i, 0, -1) {
                1 => screen.mode &= !MODE_KCURSOR,
                3 => {
                    screen.cursor_move(Some(0), Some(0), true);
                    screen.clear_screen(self.pen.cell.bg);
                }
                6 => {
                    screen.mode &= !MODE_ORIGIN;
                    screen.cursor_move(Some(0), Some(0), true);
                }
                7 => screen.mode &= !MODE_WRAP,
                12 => {
                    screen.mode &= !MODE_CURSOR_BLINKING;
                    screen.mode |= MODE_CURSOR_BLINKING_SET;
                }
                25 => screen.mode &= !MODE_CURSOR,
                1000..=1003 => screen.mode &= !ALL_MOUSE_MODES,
                1004 => screen.mode &= !MODE_FOCUSON,
                1005 => screen.mode &= !MODE_MOUSE_UTF8,
                1006 => screen.mode &= !MODE_MOUSE_SGR,
                47 | 1047 => screen.alternate_off(Some(&mut self.pen.cell), false),
                1049 => screen.alternate_off(Some(&mut self.pen.cell), true),
                2004 => screen.mode &= !MODE_BRACKETPASTE,
                2026 => screen.mode &= !MODE_SYNC,
                2031 => screen.mode &= !MODE_THEME_UPDATES,
                _ => {}
            }
        }
    }

    fn sm_private(&mut self, screen: &mut Screen) {
        for i in 0..self.list.len() {
            match self.get(i, 0, -1) {
                1 => screen.mode |= MODE_KCURSOR,
                3 => {
                    screen.cursor_move(Some(0), Some(0), true);
                    screen.clear_screen(self.pen.cell.bg);
                }
                6 => {
                    screen.mode |= MODE_ORIGIN;
                    screen.cursor_move(Some(0), Some(0), true);
                }
                7 => screen.mode |= MODE_WRAP,
                12 => screen.mode |= MODE_CURSOR_BLINKING | MODE_CURSOR_BLINKING_SET,
                25 => screen.mode |= MODE_CURSOR,
                1000 => {
                    screen.mode &= !ALL_MOUSE_MODES;
                    screen.mode |= MODE_MOUSE_STANDARD;
                }
                1002 => {
                    screen.mode &= !ALL_MOUSE_MODES;
                    screen.mode |= MODE_MOUSE_BUTTON;
                }
                1003 => {
                    screen.mode &= !ALL_MOUSE_MODES;
                    screen.mode |= MODE_MOUSE_ALL;
                }
                1004 => screen.mode |= MODE_FOCUSON,
                1005 => screen.mode |= MODE_MOUSE_UTF8,
                1006 => screen.mode |= MODE_MOUSE_SGR,
                47 | 1047 => {
                    let cell = self.pen.cell.clone();
                    screen.alternate_on(&cell, false);
                }
                1049 => {
                    let cell = self.pen.cell.clone();
                    screen.alternate_on(&cell, true);
                }
                2004 => screen.mode |= MODE_BRACKETPASTE,
                2031 => screen.mode |= MODE_THEME_UPDATES,
                2026 => screen.mode |= MODE_SYNC,
                _ => {}
            }
        }
    }

    fn winops(&mut self, screen: &mut Screen, replies: &mut Vec<u8>) {
        let (x, y) = (screen.sx(), screen.sy());
        let mut m = 0;
        loop {
            let n = self.get(m, 0, -1);
            if n == -1 {
                return;
            }
            match n {
                3 | 4 | 8 => {
                    m += 1;
                    if self.get(m, 0, -1) == -1 {
                        return;
                    }
                    m += 1;
                    if self.get(m, 0, -1) == -1 {
                        return;
                    }
                }
                9 | 10 => {
                    m += 1;
                    if self.get(m, 0, -1) == -1 {
                        return;
                    }
                }
                14 => replies
                    .extend_from_slice(format!("\x1b[4;{};{}t", y * YPIXEL, x * XPIXEL).as_bytes()),
                15 => replies
                    .extend_from_slice(format!("\x1b[5;{};{}t", y * YPIXEL, x * XPIXEL).as_bytes()),
                16 => replies.extend_from_slice(format!("\x1b[6;{YPIXEL};{XPIXEL}t").as_bytes()),
                18 => replies.extend_from_slice(format!("\x1b[8;{y};{x}t").as_bytes()),
                19 => replies.extend_from_slice(format!("\x1b[9;{y};{x}t").as_bytes()),
                22 => {
                    m += 1;
                    match self.get(m, 0, -1) {
                        -1 => return,
                        0 | 2 => screen.push_title(),
                        _ => {}
                    }
                }
                23 => {
                    m += 1;
                    match self.get(m, 0, -1) {
                        -1 => return,
                        0 | 2 => screen.pop_title(),
                        _ => {}
                    }
                }
                _ => {}
            }
            m += 1;
        }
    }

    fn sgr_256(&mut self, fgbg: i32, c: i32) {
        let cell = &mut self.pen.cell;
        if c == -1 || c > 255 {
            match fgbg {
                38 => cell.fg = COLOUR_DEFAULT,
                48 => cell.bg = COLOUR_DEFAULT,
                _ => {}
            }
        } else {
            let c = c as u32 | COLOUR_FLAG_256;
            match fgbg {
                38 => cell.fg = c,
                48 => cell.bg = c,
                58 => cell.us = c,
                _ => {}
            }
        }
    }

    fn sgr_rgb(&mut self, fgbg: i32, r: i32, g: i32, b: i32) -> bool {
        let channel = |v: i32| u8::try_from(v).ok();
        let (Some(r), Some(g), Some(b)) = (channel(r), channel(g), channel(b)) else {
            return false;
        };
        let colour = colour_rgb(r, g, b);
        let cell = &mut self.pen.cell;
        match fgbg {
            38 => cell.fg = colour,
            48 => cell.bg = colour,
            58 => cell.us = colour,
            _ => {}
        }
        true
    }

    fn sgr_colon(&mut self, text: &str) {
        let mut p = [-1i32; 8];
        let mut n = 0usize;
        for out in text.split(':') {
            if !out.is_empty() {
                match out.parse::<i64>() {
                    Ok(v) if (0..=i64::from(i32::MAX)).contains(&v) => p[n] = v as i32,
                    _ => return,
                }
            }
            n += 1;
            if n == p.len() {
                return;
            }
        }
        if n == 0 {
            return;
        }
        let cell = &mut self.pen.cell;
        if p[0] == 4 {
            if n != 2 {
                return;
            }
            let style = match p[1] {
                0 => 0,
                1 => ATTR_UNDERSCORE,
                2 => ATTR_UNDERSCORE_2,
                3 => ATTR_UNDERSCORE_3,
                4 => ATTR_UNDERSCORE_4,
                5 => ATTR_UNDERSCORE_5,
                _ => return,
            };
            cell.attr &= !ATTR_ALL_UNDERSCORE;
            cell.attr |= style;
            return;
        }
        if n < 2 || !matches!(p[0], 38 | 48 | 58) {
            return;
        }
        match p[1] {
            2 => {
                if n < 3 {
                    return;
                }
                let i = if n == 5 { 2 } else { 3 };
                if n < i + 3 {
                    return;
                }
                self.sgr_rgb(p[0], p[i], p[i + 1], p[i + 2]);
            }
            5 => {
                if n < 3 {
                    return;
                }
                self.sgr_256(p[0], p[2]);
            }
            _ => {}
        }
    }

    fn sgr(&mut self) {
        if self.list.is_empty() {
            self.pen.cell = Cell::default();
            return;
        }
        let mut i = 0;
        while i < self.list.len() {
            if let Param::Text(text) = &self.list[i] {
                let text = text.clone();
                self.sgr_colon(&text);
                i += 1;
                continue;
            }
            let n = self.get(i, 0, 0);
            if n == -1 {
                i += 1;
                continue;
            }
            if matches!(n, 38 | 48 | 58) {
                i += 1;
                match self.get(i, 0, -1) {
                    2 => {
                        let (r, g, b) = (
                            self.get(i + 1, 0, -1),
                            self.get(i + 2, 0, -1),
                            self.get(i + 3, 0, -1),
                        );
                        if self.sgr_rgb(n, r, g, b) {
                            i += 3;
                        }
                    }
                    5 => {
                        let c = self.get(i + 1, 0, -1);
                        self.sgr_256(n, c);
                        i += 1;
                    }
                    _ => {}
                }
                i += 1;
                continue;
            }
            let cell = &mut self.pen.cell;
            match n {
                0 => {
                    let link = cell.link;
                    *cell = Cell::default();
                    cell.link = link;
                }
                1 => cell.attr |= ATTR_BRIGHT,
                2 => cell.attr |= ATTR_DIM,
                3 => cell.attr |= ATTR_ITALICS,
                4 => {
                    cell.attr &= !ATTR_ALL_UNDERSCORE;
                    cell.attr |= ATTR_UNDERSCORE;
                }
                5 | 6 => cell.attr |= ATTR_BLINK,
                7 => cell.attr |= ATTR_REVERSE,
                8 => cell.attr |= ATTR_HIDDEN,
                9 => cell.attr |= ATTR_STRIKETHROUGH,
                21 => {
                    cell.attr &= !ATTR_ALL_UNDERSCORE;
                    cell.attr |= ATTR_UNDERSCORE_2;
                }
                22 => cell.attr &= !(ATTR_BRIGHT | ATTR_DIM),
                23 => cell.attr &= !ATTR_ITALICS,
                24 => cell.attr &= !ATTR_ALL_UNDERSCORE,
                25 => cell.attr &= !ATTR_BLINK,
                27 => cell.attr &= !ATTR_REVERSE,
                28 => cell.attr &= !ATTR_HIDDEN,
                29 => cell.attr &= !ATTR_STRIKETHROUGH,
                30..=37 => cell.fg = (n - 30) as u32,
                39 => cell.fg = COLOUR_DEFAULT,
                40..=47 => cell.bg = (n - 40) as u32,
                49 => cell.bg = COLOUR_DEFAULT,
                53 => cell.attr |= ATTR_OVERLINE,
                55 => cell.attr &= !ATTR_OVERLINE,
                59 => cell.us = COLOUR_DEFAULT,
                90..=97 => cell.fg = n as u32,
                100..=107 => cell.bg = (n - 10) as u32,
                _ => {}
            }
            i += 1;
        }
    }

    fn dcs_dispatch(&mut self, screen: &mut Screen, replies: &mut Vec<u8>) {
        if self.discard {
            return;
        }
        // DECRQSS: DCS $ q Pt ST. Only the cursor style is answered.
        if self.interm.as_slice() == b"$" && self.string.first() == Some(&b'q') {
            if self.string.len() < 3 || self.string[1] != b' ' || self.string[2] != b'q' {
                replies.extend_from_slice(b"\x1bP0$r\x1b\\");
                return;
            }
            let blinking = screen.mode & MODE_CURSOR_BLINKING != 0;
            let ps = match screen.cstyle {
                1 => {
                    if blinking {
                        1
                    } else {
                        2
                    }
                }
                2 => {
                    if blinking {
                        3
                    } else {
                        4
                    }
                }
                3 => {
                    if blinking {
                        5
                    } else {
                        6
                    }
                }
                _ => 0,
            };
            replies.extend_from_slice(format!("\x1bP1$r q{ps} q\x1b\\").as_bytes());
        }
        // Passthrough (`tmux;`) is tmux's `allow-passthrough`, off by default: ignored.
    }

    fn exit_osc(&mut self, screen: &mut Screen) {
        if self.discard {
            return;
        }
        let bytes = std::mem::take(&mut self.string);
        let Some(first) = bytes.first() else {
            return;
        };
        if !first.is_ascii_digit() {
            return;
        }
        let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
        let mut option = 0u32;
        for &b in &bytes[..digits] {
            option = option.wrapping_mul(10).wrapping_add(u32::from(b - b'0'));
        }
        let rest = &bytes[digits..];
        let value = match rest.first() {
            None => &rest[..0],
            Some(b';') => &rest[1..],
            Some(_) => return,
        };
        match option {
            0 | 2 => {
                if let Ok(title) = std::str::from_utf8(value) {
                    screen.set_title(title);
                }
            }
            8 => self.osc_8(screen, value),
            _ => {}
        }
    }

    /// OSC 8: a hyperlink for the cells written from now on, or none with an empty URI.
    ///
    /// On bytes, as tmux's `input_osc_8` is: a URI or id that isn't UTF-8 is kept, its
    /// bytes escaped as the hyperlinks hold them.
    fn osc_8(&mut self, screen: &mut Screen, value: &[u8]) {
        let mut id: Option<&[u8]> = None;
        let mut start = 0;
        let uri_at = loop {
            let Some(found) = value[start..].iter().position(|&b| b == b':' || b == b';') else {
                return;
            };
            let end = start + found;
            let part = &value[start..end];
            if part.len() >= 4 && part.starts_with(b"id=") {
                if id.is_some() {
                    return;
                }
                id = Some(&part[3..]);
            }
            if value[end] == b';' {
                break end + 1;
            }
            start = end + 1;
        };
        let uri = &value[uri_at..];
        if uri.is_empty() {
            self.pen.cell.link = 0;
            return;
        }
        self.pen.cell.link = screen.hyperlinks.put(uri, id);
    }

    fn exit_apc(&mut self, screen: &mut Screen) {
        if self.discard {
            return;
        }
        if let Ok(title) = std::str::from_utf8(&self.string) {
            let title = title.to_owned();
            screen.set_title(&title);
        }
    }
}

/// DECSCUSR's style, as tmux's `screen_set_cursor_style` keeps it: 0 the default, then
/// block, underline and bar, each blinking or not.
fn set_cursor_style(screen: &mut Screen, style: i32) {
    let (cstyle, blinking) = match style {
        0 => (0, None),
        1 => (1, Some(true)),
        2 => (1, Some(false)),
        3 => (2, Some(true)),
        4 => (2, Some(false)),
        5 => (3, Some(true)),
        6 => (3, Some(false)),
        _ => return,
    };
    screen.cstyle = cstyle;
    match blinking {
        Some(true) => screen.mode |= MODE_CURSOR_BLINKING,
        Some(false) => screen.mode &= !MODE_CURSOR_BLINKING,
        None => {}
    }
}
