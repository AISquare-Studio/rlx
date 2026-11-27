// `Terminal::capture` is ported from the capture loop of tmux 3.8's cmd-capture-pane.c,
// whose notice follows. The rest of this file is rlxd's own.
//
// Copyright (c) 2009 Jonathan Alvarado <radobobo@users.sourceforge.net>
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

//! A VT screen model that prints its screen as tmux does (handoff 0.10.3, 1.4).
//!
//! `pty.screen`'s text must be byte for byte what `tmux capture-pane -e -p` prints for
//! the same screen, so the TUI's renderer reads it unchanged. tmux's output depends on
//! details no generic emulator keeps: how far each line was allocated, which form a
//! colour was given in, the order its codes come in. So this is tmux 3.8's own model,
//! ported: the grid (`grid`), the screen and its writes (`screen`), the input parser
//! (`input`) and UTF-8 (`utf8`). Each of those files carries tmux's ISC notice.
//!
//! What is left out is what tmux does for attached terminals: drawing, the redraw lists,
//! timers, palettes, the clipboard, images. The model holds what the frame needs, the
//! cursor, the size, the modes that decide how keys and pastes are sent, and the title.

mod grid;
mod input;
mod screen;
mod utf8;

use input::Input;
use screen::{
    MODE_BRACKETPASTE, MODE_CURSOR, MODE_KCURSOR, MODE_KEYS_EXTENDED, MODE_KEYS_EXTENDED_2,
    MODE_KKEYPAD, Screen,
};

/// The modes a program set that decide how keys and pastes reach it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct InputModes {
    /// Application cursor keys (DECCKM).
    pub cursor_keys: bool,
    /// Application keypad (DECKPAM).
    pub keypad: bool,
    /// Bracketed paste (mode 2004).
    pub bracketed_paste: bool,
    /// Extended keys: 0 off, 1 or 2 for modifyOtherKeys 1 or 2.
    pub extended_keys: u8,
}

/// One terminal: its screen, and the parser the program's output goes through.
#[derive(Clone, Debug)]
pub struct Terminal {
    screen: Screen,
    input: Input,
    #[cfg(feature = "debug-methods")]
    test_panic: Option<TestPanic>,
}

/// Where a test build's screen model panics on purpose, as a bug in it would, so a test
/// can show the panic ends that PTY alone (`debug.pty_panic`). Test builds only: the
/// `debug-methods` feature.
#[cfg(feature = "debug-methods")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestPanic {
    /// In the parser, halfway through the program's next output.
    Output,
    /// As it prints the screen.
    Screen,
    /// As it resizes.
    Resize,
}

impl Terminal {
    /// A terminal `cols` by `rows`, keeping `history` lines above the screen. `version`
    /// answers a program's XTVERSION query.
    pub fn new(cols: u16, rows: u16, history: u32, version: &str) -> Self {
        Self {
            screen: Screen::new(u32::from(cols.max(1)), u32::from(rows.max(1)), history),
            input: Input::new(version),
            #[cfg(feature = "debug-methods")]
            test_panic: None,
        }
    }

    /// Test builds only: panics the next time the model is called `at`.
    #[cfg(feature = "debug-methods")]
    pub fn panic_next(&mut self, at: TestPanic) {
        self.test_panic = Some(at);
    }

    /// Takes in `bytes` the program wrote. What it returns goes back to the program: its
    /// queries' replies, such as the cursor's position.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.feed_at(bytes, std::time::Instant::now())
    }

    /// [`feed`](Self::feed), of bytes that came at `now`: a string the program opened
    /// [`input::GROUND_TIMER`] before and never ended is given up first, as tmux does.
    pub fn feed_at(&mut self, bytes: &[u8], now: std::time::Instant) -> Vec<u8> {
        self.feed_inner(bytes, now, None)
    }

    /// [`feed`](Self::feed), stopping early once `stop` is set: the rest is not taken.
    pub fn feed_unless(&mut self, bytes: &[u8], stop: &std::sync::atomic::AtomicBool) -> Vec<u8> {
        self.feed_inner(bytes, std::time::Instant::now(), Some(stop))
    }

    fn feed_inner(
        &mut self,
        bytes: &[u8],
        now: std::time::Instant,
        stop: Option<&std::sync::atomic::AtomicBool>,
    ) -> Vec<u8> {
        let mut replies = Vec::new();
        #[cfg(feature = "debug-methods")]
        if self.test_panic == Some(TestPanic::Output) {
            // Half the bytes in, so the screen is left half-changed, as by a real bug.
            let half = bytes.len() / 2;
            self.input
                .parse(&mut self.screen, &bytes[..half], &mut replies, now, stop);
            panic!("debug.pty_panic: the screen model panics on the program's output");
        }
        self.input
            .parse(&mut self.screen, bytes, &mut replies, now, stop);
        replies
    }

    /// The screen as `tmux capture-pane -e -p -S -<scrollback>` prints it: `scrollback`
    /// lines of history first, as many as there are, then every row, each with its SGR
    /// codes and a newline.
    pub fn capture(&self, scrollback: u32) -> String {
        #[cfg(feature = "debug-methods")]
        if self.test_panic == Some(TestPanic::Screen) {
            panic!("debug.pty_panic: the screen model panics printing the screen");
        }
        let grid = &self.screen.grid;
        let top = grid.hsize - scrollback.min(grid.hsize);
        let bottom = grid.hsize + grid.sy - 1;
        let mut last = grid::Cell::default();
        let mut out = Vec::new();
        for py in top..=bottom {
            out.extend(grid::string_cells(
                grid,
                py,
                &mut last,
                &self.screen.hyperlinks,
            ));
            out.push(b'\n');
        }
        String::from_utf8(out)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
    }

    /// The cursor: column and row from 0, and whether the program shows it. The column
    /// is the width while the last cell of a line is written and the next would wrap.
    pub fn cursor(&self) -> (u16, u16, bool) {
        let to = |n: u32| u16::try_from(n).unwrap_or(u16::MAX);
        (
            to(self.screen.cx),
            to(self.screen.cy),
            self.screen.mode & MODE_CURSOR != 0,
        )
    }

    /// The size: columns and rows.
    pub fn size(&self) -> (u16, u16) {
        let to = |n: u32| u16::try_from(n).unwrap_or(u16::MAX);
        (to(self.screen.sx()), to(self.screen.sy()))
    }

    /// Whether the program is on the alternate screen.
    pub fn alternate(&self) -> bool {
        self.screen.is_alternate()
    }

    /// The lines of history above the screen.
    pub fn history(&self) -> u32 {
        self.screen.grid.hsize
    }

    /// The modes that decide how keys and pastes reach the program.
    pub fn input_modes(&self) -> InputModes {
        let mode = self.screen.mode;
        InputModes {
            cursor_keys: mode & MODE_KCURSOR != 0,
            keypad: mode & MODE_KKEYPAD != 0,
            bracketed_paste: mode & MODE_BRACKETPASTE != 0,
            extended_keys: if mode & MODE_KEYS_EXTENDED_2 != 0 {
                2
            } else if mode & MODE_KEYS_EXTENDED != 0 {
                1
            } else {
                0
            },
        }
    }

    /// The title the program set, OSC 0 or 2.
    pub fn title(&self) -> &str {
        &self.screen.title
    }

    /// Resizes to `cols` by `rows`, rewrapping lines off the alternate screen, as tmux
    /// resizes a pane.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        #[cfg(feature = "debug-methods")]
        if self.test_panic == Some(TestPanic::Resize) {
            panic!("debug.pty_panic: the screen model panics resizing");
        }
        let reflow = !self.screen.is_alternate();
        self.screen
            .resize(u32::from(cols.max(1)), u32::from(rows.max(1)), reflow);
    }

    /// About the bytes the model holds, for the memory bound: both screens' cells, the
    /// history, the hyperlinks, and the parser's buffers.
    pub fn memory(&self) -> usize {
        self.screen.bytes() + self.input.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(cols: u16, rows: u16, bytes: &[u8]) -> String {
        let mut terminal = Terminal::new(cols, rows, 2000, "test");
        terminal.feed(bytes);
        terminal.capture(0)
    }

    fn rows(rows: usize) -> String {
        "\n".repeat(rows)
    }

    #[test]
    fn the_handoffs_frame_is_tmuxs() {
        let text = capture(80, 24, b"\x1b[31mred\x1b[0m plain");
        assert_eq!(text, format!("\x1b[31mred\x1b[39m plain\n{}", rows(23)));
    }

    #[test]
    fn a_line_is_read_to_its_allocated_width_so_a_reset_shows() {
        // Twenty cells are allocated for the first, so the default cell after the X is
        // read too: its code stays, its space is trimmed.
        let text = capture(80, 2, b"\x1b[41mX\x1b[0m");
        assert_eq!(text, "\x1b[41mX\x1b[49m\n\n");
    }

    #[test]
    fn colours_keep_the_form_they_were_given_in() {
        let text = capture(80, 1, b"\x1b[31ma\x1b[38;5;1mb\x1b[91mc\x1b[38;2;1;2;3md");
        assert_eq!(
            text,
            "\x1b[31ma\x1b[38;5;1mb\x1b[91mc\x1b[38;2;1;2;3md\x1b[39m\n"
        );
    }

    #[test]
    fn a_wide_character_takes_two_cells_and_prints_once() {
        let mut terminal = Terminal::new(10, 1, 0, "test");
        terminal.feed("中x".as_bytes());
        assert_eq!(terminal.capture(0), "中x\n");
        assert_eq!(terminal.cursor(), (3, 0, true));
    }

    #[test]
    fn a_long_line_wraps_and_the_cursor_waits_at_the_width() {
        let mut terminal = Terminal::new(5, 3, 0, "test");
        terminal.feed(b"abcde");
        assert_eq!(terminal.cursor().0, 5, "pending wrap");
        terminal.feed(b"fg");
        assert_eq!(terminal.capture(0), "abcde\nfg\n\n");
    }

    #[test]
    fn a_query_gets_tmuxs_reply() {
        let mut terminal = Terminal::new(80, 24, 0, "0.10.3");
        assert_eq!(terminal.feed(b"\x1b[5;9H\x1b[6n"), b"\x1b[5;9R");
        assert_eq!(terminal.feed(b"\x1b[c"), b"\x1b[?1;2c");
        assert_eq!(terminal.feed(b"\x1b[>q"), b"\x1bP>|rlxd 0.10.3\x1b\\");
        assert_eq!(terminal.feed(b"\x1b[?2004$p"), b"\x1b[?2004;2$y");
    }

    #[test]
    fn a_missing_parameter_is_not_a_zero() {
        let mut terminal = Terminal::new(10, 5, 0, "test");
        // The region from row 1 to the bottom: tmux's default for a missing bottom.
        terminal.feed(b"\x1b[1;r\x1b[5;1Hx\n");
        assert_eq!(terminal.capture(0), "\n\n\nx\n\n");
    }

    #[test]
    fn modes_for_keys_and_pastes_follow_the_program() {
        let mut terminal = Terminal::new(10, 5, 0, "test");
        assert_eq!(terminal.input_modes(), InputModes::default());
        terminal.feed(b"\x1b[?1h\x1b=\x1b[?2004h\x1b[>4;1m");
        assert_eq!(
            terminal.input_modes(),
            InputModes {
                cursor_keys: true,
                keypad: true,
                bracketed_paste: true,
                extended_keys: 1,
            }
        );
    }

    /// The memory a terminal 80 wide takes once its 50,000 lines of history are full of
    /// lines of `width` characters.
    fn full_history(width: usize) -> (usize, usize) {
        let mut terminal = Terminal::new(80, 24, 50_000, "test");
        let line = format!("{}\r\n", "x".repeat(width));
        let chunk = line.repeat(1000);
        for _ in 0..52 {
            terminal.feed(chunk.as_bytes());
        }
        assert!(terminal.history() >= 45_000, "{}", terminal.history());
        (terminal.memory(), terminal.history() as usize + 24)
    }

    #[test]
    fn a_full_scrollback_of_80_column_lines_stays_in_proportion_to_its_cells() {
        let (full, lines) = full_history(80);
        // A compact cell is five bytes, 80 of them a line, plus the line's own record
        // and the slack of the array the lines are in.
        let bound = lines * (80 * 5 + 160);
        assert!(
            full <= bound,
            "{full} bytes for {lines} lines, more than {bound}"
        );
        // A line holds the cells it was written to, rounded up to a quarter of the width:
        // ten characters take 20 cells, so each line costs 60 cells, 300 bytes, less.
        let (short, _) = full_history(10);
        let saved = full - short;
        let expected = lines * 60 * 5;
        assert!(
            saved.abs_diff(expected) * 20 < expected,
            "10-cell lines saved {saved} bytes, not about {expected}"
        );
    }

    /// The seeded test's runs, and the writes in each.
    const SEEDS: u64 = 50;
    const STEPS: u32 = 60;

    /// A small deterministic generator, so a failure names the seed that made it.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len() as u64) as usize]
        }
    }

    /// Bytes no program should send, and some a broken one would: random bytes, sequences
    /// with huge counts and many parameters, strings that never end, bad UTF-8.
    fn hostile(rng: &mut Rng) -> Vec<u8> {
        let huge = [
            "0",
            "1",
            "65535",
            "2147483647",
            "2147483648",
            "99999999999999999999",
        ];
        let finals = b"@ABCDEFGHJKLMPSTXZ`bcdfghlmnpqrstu";
        match rng.below(15) {
            0 => (0..rng.below(4096)).map(|_| rng.next() as u8).collect(),
            1 => {
                let n = rng.pick(&huge);
                let f = *rng.pick(finals) as char;
                format!("\x1b[{n};{n}{f}").into_bytes()
            }
            2 => {
                let params: Vec<String> = (0..rng.below(200))
                    .map(|_| rng.pick(&huge).to_string())
                    .collect();
                format!("\x1b[{}m", params.join(";")).into_bytes()
            }
            // Colours in every form tmux takes, then text in them and an erase in them,
            // so the cells that carry them are written and cleared.
            3 => {
                let (r, g, b) = (rng.below(256), rng.below(256), rng.below(256));
                let colour = match rng.below(6) {
                    0 => format!("38;2;{r};{g};{b}"),
                    1 => format!("48:2::{r}:{g}:{b}"),
                    2 => format!("38:2:{r}:{g}:{b}"),
                    3 => format!("48;5;{r}"),
                    4 => format!("58:2::{r}:{g}:{b};4:3"),
                    _ => format!("9{};10{};1;3;7", r % 8, g % 8),
                };
                let erase = rng.pick(&["", "\x1b[K", "\x1b[2J", "\x1b[J", "\x1b[1K", "\x1b[5X"]);
                let text = rng
                    .pick(&["coloured ", "中文", "e\u{301}"])
                    .repeat(rng.below(40) as usize);
                format!("\x1b[{colour}m{text}{erase}").into_bytes()
            }
            4 => {
                let mut osc = b"\x1b]".to_vec();
                osc.extend(std::iter::repeat_n(b'x', rng.below(3 << 19) as usize));
                if rng.below(2) == 0 {
                    osc.push(0x07);
                }
                osc
            }
            5 => {
                let mut dcs = b"\x1bP$q".to_vec();
                dcs.extend(std::iter::repeat_n(b'q', rng.below(1 << 16) as usize));
                dcs.extend_from_slice(b"\x1b\\");
                dcs
            }
            6 => vec![
                0xe4, 0xff, 0xc3, 0x28, 0xf0, 0x90, 0xed, 0xa0, 0x80, 0xc0, 0x80, 0xf4, 0x90,
            ],
            7 => format!(
                "\x1b]8;id={0};http://{0}\x1b\\link\x1b]8;;\x07",
                "y".repeat(rng.below(5000) as usize)
            )
            .into_bytes(),
            // A move anywhere, even past the screen, then the alternate screen and the
            // saved cursor in any order: the test resizes between such writes.
            8 | 12 | 13 => {
                let ops = [
                    "\x1b[?1049h",
                    "\x1b[?1049l",
                    "\x1b[?47h",
                    "\x1b[?47l",
                    "\x1b[?1047h",
                    "\x1b[?1047l",
                    "\x1b[?1048h",
                    "\x1b[?1048l",
                    "\x1b7",
                    "\x1b8",
                    "\x1b[s",
                    "\x1b[u",
                    "\x1b[?6h",
                    "\x1b[?6l",
                    "\x1b[?7l",
                    "\x1b[?7h",
                ];
                let mut out = format!("\x1b[{};{}H", rng.below(120), rng.below(320));
                for _ in 0..1 + rng.below(3) {
                    out.push_str(rng.pick(&ops));
                }
                out.into_bytes()
            }
            9 => "中e\u{301}👍🏽🇬🇧\u{200d}\u{fe0f}\t"
                .repeat(rng.below(200) as usize)
                .into_bytes(),
            10 => format!(
                "\x1b[{};{}r\x1b[{}L\x1b[{}M\x1bM\x1bD\x1b[{}S\x1b[{}T",
                rng.below(60),
                rng.below(60),
                rng.below(100),
                rng.below(100),
                rng.below(100),
                rng.below(100)
            )
            .into_bytes(),
            11 => format!("x\x1b[{}b\x1b#8\x1bc\x1b[3J\x1b[2J", rng.pick(&huge)).into_bytes(),
            // Small inserts and deletes under an RGB background, many of them: each
            // clears cells it moves (#2641 major 3).
            14 => format!(
                "\x1b[48;2;{};{};{}m{}",
                rng.below(256),
                rng.below(256),
                rng.below(256),
                rng.pick(&["\x1b[3@", "\x1b[2P", "\x1b[40@", "\x1b[40P", "\x1b[X"])
                    .repeat(rng.below(3000) as usize)
            )
            .into_bytes(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn hostile_output_never_panics_and_memory_stays_bounded() {
        // RLXD_VT_SEEDS=N runs N seeds instead, for a long run: `RLXD_VT_SEEDS=20000
        // cargo test --release -p rlxd --lib hostile`.
        // RLXD_VT_SEED=N runs seed N alone, to look into one a long run names.
        let seeds = std::env::var("RLXD_VT_SEEDS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(SEEDS);
        let one: Option<u64> = std::env::var("RLXD_VT_SEED")
            .ok()
            .and_then(|n| n.parse().ok());
        for seed in one.map_or(1..=seeds, |n| n..=n) {
            let run = std::panic::catch_unwind(|| hostile_run(seed));
            if let Err(panic) = run {
                let why = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_default();
                panic!("seed {seed}: {why}");
            }
        }
    }

    /// One seed's run of the seeded test.
    fn hostile_run(seed: u64) {
        {
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed);
            let (mut cols, mut rows) = (80u16, 24u16);
            let (mut widest, mut tallest) = (cols, rows);
            let mut terminal = Terminal::new(cols, rows, 500, "test");
            for step in 0..STEPS {
                // A few pieces, each cut anywhere, so a sequence spans writes, with a
                // resize inside one or between two.
                for _ in 0..1 + rng.below(3) {
                    let bytes = hostile(&mut rng);
                    let cut = rng.below(bytes.len() as u64 + 1) as usize;
                    terminal.feed(&bytes[..cut]);
                    if rng.below(3) == 0 {
                        cols = 1 + rng.below(300) as u16;
                        rows = 1 + rng.below(100) as u16;
                        (widest, tallest) = (widest.max(cols), tallest.max(rows));
                        terminal.resize(cols, rows);
                    }
                    terminal.feed(&bytes[cut..]);
                }
                let (_, y, _) = terminal.cursor();
                assert!(y < rows, "seed {seed} step {step}: row {y} of {rows}");
                // A narrower screen keeps the column as it was, as tmux's does when it
                // doesn't rewrap (the alternate screen), and with autowrap off a character
                // past the edge is dropped: only a move brings it back. A carriage return
                // always does, once any string is ended: two STs end one even after an
                // ESC inside a DCS, which takes the next byte as the string's.
                terminal.feed(b"\x1b\\\x1b\\\r");
                let (x, y, _) = terminal.cursor();
                assert!(
                    x == 0 && y < rows,
                    "seed {seed} step {step}: cursor {x},{y} in {cols}x{rows}"
                );
                assert_eq!(terminal.size(), (cols, rows));
                // Every line as wide as the screen has been (a line keeps its cells), on
                // both screens, each cell 5 bytes and 16 more when extended, which a screen
                // line's can take twice of until it goes into the history; then the
                // parser's string, up to 1 MiB and its doubling, and the hyperlinks, at
                // most 5000 of a 1 KiB URI and id each, held twice.
                let lines = 500 + 2 * usize::from(tallest);
                let cell = 5 + 2 * 16;
                let bound = lines * (usize::from(widest) * cell + 256) + (2 << 20) + (21 << 20);
                assert!(
                    terminal.memory() < bound,
                    "seed {seed} step {step}: {} bytes",
                    terminal.memory()
                );
                let text = terminal.capture(1000);
                assert!(text.matches('\n').count() >= usize::from(rows));
            }
        }
    }

    #[test]
    fn a_cursor_saved_on_a_taller_screen_is_restored_inside_the_screen() {
        // #2585's sequence: the cursor ?1049h saves at row 22 of 24 outlives the
        // alternate screen; ?47h after a shorter resize, then a wider one, then ?1049l
        // restores it. tmux reads past its lines there; rlxd clamps it to the screen.
        // And the #28 tester's (#2589): row 20, ?1047h, at each of its sizes.
        let cases = [
            (22, &b"\x1b[?47h"[..], 100, 10),
            (22, b"\x1b[?47h", 80, 10),
            (20, b"\x1b[?1047h", 40, 12),
            (20, b"\x1b[?1047h", 40, 5),
            (20, b"\x1b[?1047h", 100, 5),
            (20, b"\x1b[?1047h", 80, 5),
        ];
        for (row, enter, cols, rows) in cases {
            let mut terminal = Terminal::new(80, 24, 100, "test");
            terminal.feed(format!("\x1b[{row};1H\x1b[?1049h\x1b[?1049l").as_bytes());
            terminal.resize(80, 10);
            terminal.feed(enter);
            terminal.resize(cols, rows);
            terminal.feed(b"\x1b[?1049l");
            let (_, y, _) = terminal.cursor();
            assert!(y < rows, "row {y} of {rows}");
            terminal.feed(b"\x1b[31mafter");
            assert!(terminal.capture(0).contains("after"));
        }
    }

    /// #2585: a full history of coloured or wide text, within 1.5 times what tmux 3.8
    /// took for the same, by the review's figures: 52,000 lines written at 80 columns,
    /// with tmux's 50,000-line history, one process's RSS, in MiB.
    ///
    /// `memory()` counts what the model holds, and the allocator adds about a fifth to
    /// it (#2641's nit), so it is held to 1.25 times tmux's RSS: the model's own peak
    /// RSS, measured in release by a program that runs it alone, is then within 1.5 times
    /// (at #2641's round: ASCII 26.6 MiB, RGB 92.1, wide 58.1, against tmux's 26, 169
    /// and 45). rlxd's RSS has the bus's copy of the output on top, its own budget
    /// (decision 4).
    #[test]
    fn a_full_history_of_colour_or_wide_text_stays_near_tmuxs_memory() {
        // And within 5% of what the layout itself takes: 5 bytes a cell, 16 more for one
        // that is extended, and up to 128 a line for the line and its place in the grid.
        for (what, line, tmux, extended) in [
            ("ASCII", "x".repeat(80), 26, 0),
            (
                "an RGB foreground",
                format!("\x1b[38;2;1;2;3m{}", "x".repeat(80)),
                169,
                80,
            ),
            ("wide characters", "中".repeat(40), 45, 40),
        ] {
            let mut terminal = Terminal::new(80, 24, 50_000, "test");
            let mut bytes = Vec::with_capacity(52_000 * (line.len() + 2));
            for _ in 0..52_000 {
                bytes.extend_from_slice(line.as_bytes());
                bytes.extend_from_slice(b"\r\n");
            }
            terminal.feed(&bytes);
            assert!(
                terminal.history() > 45_000,
                "{what}: {}",
                terminal.history()
            );
            let held = terminal.memory();
            assert!(
                held < (tmux << 20) * 5 / 4,
                "{what}: {} MiB, where tmux takes {tmux}",
                held >> 20
            );
            let lines = terminal.history() as usize + 24;
            let layout = lines * (80 * 5 + extended * 16 + 128);
            assert!(
                held < layout * 21 / 20,
                "{what}: {held} bytes, where its cells take {layout}"
            );
        }
    }

    #[test]
    fn a_string_left_open_is_given_up_after_five_seconds_as_tmux_does() {
        // #2585's repro: a stray DCS, then output six seconds later. tests/vt_tmux.rs
        // checks the same against tmux, in real time.
        let start = std::time::Instant::now();
        let later = |s: u64| start + std::time::Duration::from_secs(s);
        let mut expected = Terminal::new(20, 3, 0, "test");
        expected.feed(b"before\r\n\x1b[32mhello\x1b[0m");
        // The review's own: a stray DCS, which swallows ESC, so only the timer ends it.
        let mut terminal = Terminal::new(20, 3, 0, "test");
        terminal.feed_at(b"before\r\n\x1bPq", start);
        terminal.feed_at(b"\x1b[31mlost", later(4));
        assert_eq!(terminal.capture(0), "before\n\n\n");
        terminal.feed_at(b"\x1b[32mhello\x1b[0m", later(6));
        assert_eq!(terminal.capture(0), expected.capture(0));
        assert_eq!(terminal.cursor(), expected.cursor());
        // Every string state has the timer: plain text is the string's until it fires.
        for open in [
            &b"\x1bPq"[..],
            b"\x1b]0;title",
            b"\x1b_apc",
            b"\x1bkname",
            b"\x1bX",
        ] {
            let mut terminal = Terminal::new(20, 3, 0, "test");
            terminal.feed_at(open, start);
            terminal.feed_at(b"lost", later(4));
            assert_eq!(terminal.capture(0), "\n\n\n", "{open:?}");
            terminal.feed_at(b"hello", later(6));
            assert_eq!(terminal.capture(0), "hello\n\n\n", "{open:?}");
        }
        // A string that ends in time is not given up, however long the output after it.
        let mut terminal = Terminal::new(20, 3, 0, "test");
        terminal.feed_at(b"\x1bPq\x1b\\a", start);
        terminal.feed_at(b"b", later(9));
        assert_eq!(terminal.capture(0), "ab\n\n\n");
    }

    /// #2641 major 2: each terminal numbers its own long characters, so one that is sent
    /// more distinct ones than its quota (the review's 262,154 code points from U+20000)
    /// leaves another's emoji and conjuncts as they are, and holds a bounded table.
    #[test]
    fn one_terminals_flood_of_characters_leaves_anothers_alone() {
        let line = "a🚀b xक्षy";
        let mut flooded = Terminal::new(80, 24, 100, "test");
        let flood: String = (0x2_0000u32..0x2_0000 + 262_154)
            .filter_map(char::from_u32)
            .collect();
        flooded.feed(flood.as_bytes());
        let mut other = Terminal::new(80, 24, 100, "test");
        other.feed(line.as_bytes());
        assert!(
            other.capture(0).starts_with(&format!("{line}\n")),
            "{:?}",
            other.capture(0)
        );
        // The flooded one keeps its quota and no more, and memory() counts it: a few
        // hundred KB are the screen, the rest its table (#2685 (b)).
        let held = flooded.memory();
        assert!(held < 16 << 20, "{held} bytes");
        assert!(held > 4 << 20, "{held} bytes: the table is counted");
        assert!(other.memory() < 1 << 20, "{} bytes", other.memory());
        // Past its quota a new long character is spaces there, and there only.
        flooded.feed(b"\x1b[2J\x1b[H");
        flooded.feed(line.as_bytes());
        assert!(!flooded.capture(0).contains('🚀'));
        assert!(other.capture(0).contains('🚀'));
    }

    /// #2641 major 3: inserts and deletes under an RGB background clear the cells they
    /// move, each with an extended cell of its own: the screen's lines stay bounded.
    #[test]
    fn inserts_and_deletes_under_an_rgb_background_stay_bounded() {
        for each in [&b"\x1b[40@"[..], b"\x1b[40P", b"\x1b[@", b"\x1b[3@\x1b[2P"] {
            let mut terminal = Terminal::new(80, 24, 100, "test");
            terminal.feed(b"\x1b[48;2;1;2;3m");
            for _ in 0..4 {
                terminal.feed(&each.repeat(50_000));
            }
            // A screen of 80 by 24, each cell 5 bytes and at most two extended ones.
            let held = terminal.memory();
            assert!(
                held < 24 * 80 * (5 + 2 * 16) + (64 << 10),
                "{each:?}: {held} bytes"
            );
        }
    }

    #[test]
    fn a_string_that_never_ends_holds_at_most_a_mebibyte() {
        let mut terminal = Terminal::new(80, 24, 100, "test");
        terminal.feed(b"\x1b]0;");
        let chunk = vec![b'x'; 1 << 20];
        for _ in 0..8 {
            terminal.feed(&chunk);
        }
        let held = terminal.memory();
        assert!(held < 3 << 20, "{held} bytes");
        // Once it ends, the screen takes what follows.
        terminal.feed(b"\x07after");
        assert!(terminal.capture(0).starts_with("after\n"));
    }

    #[test]
    fn a_flood_of_hyperlinks_stays_bounded() {
        let mut terminal = Terminal::new(80, 24, 100, "test");
        let uri = format!("http://{}", "u".repeat(1000));
        for i in 0..5200 {
            // Named links, each with an id as long as a link may keep, and one longer,
            // which is dropped.
            let kept = format!("{i:0>1000}");
            let dropped = format!("{i:0>4000}");
            terminal.feed(
                format!(
                    "\x1b]8;id={kept};{uri}\x1b\\a\x1b]8;id={dropped};{uri}\x1b\\b\x1b]8;;\x1b\\"
                )
                .as_bytes(),
            );
        }
        // At most 5000 links of a 1 KiB URI and id, each held twice: about 20 MB.
        let held = terminal.memory();
        assert!(held < 21 << 20, "{held} bytes");
        assert!(held > 19 << 20, "{held} bytes: the links are counted");
    }

    #[test]
    fn history_is_kept_and_captured_on_request() {
        let mut terminal = Terminal::new(10, 2, 100, "test");
        terminal.feed(b"one\r\ntwo\r\nthree");
        assert_eq!(terminal.history(), 1);
        assert_eq!(terminal.capture(0), "two\nthree\n");
        assert_eq!(terminal.capture(5), "one\ntwo\nthree\n");
    }
}
