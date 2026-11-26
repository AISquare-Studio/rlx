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
        let mut replies = Vec::new();
        #[cfg(feature = "debug-methods")]
        if self.test_panic == Some(TestPanic::Output) {
            // Half the bytes in, so the screen is left half-changed, as by a real bug.
            let half = bytes.len() / 2;
            self.input
                .parse(&mut self.screen, &bytes[..half], &mut replies);
            panic!("debug.pty_panic: the screen model panics on the program's output");
        }
        self.input.parse(&mut self.screen, bytes, &mut replies);
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
        match rng.below(12) {
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
            3 => format!("\x1b[38:2::{0}:{0}:{0}:{0}:{0}:{0}:{0}m", rng.pick(&huge)).into_bytes(),
            4 => {
                let mut osc = b"\x1b]".to_vec();
                osc.extend(std::iter::repeat_n(b'x', rng.below(3 << 20) as usize));
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
            8 => format!(
                "\x1b[?{}h\x1b[?{}l",
                rng.pick(&["1049", "47", "1047", "6", "7", "2004", "1"]),
                rng.pick(&["1049", "47", "6", "7"])
            )
            .into_bytes(),
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
            _ => format!("x\x1b[{}b\x1b#8\x1bc\x1b[3J\x1b[2J", rng.pick(&huge)).into_bytes(),
        }
    }

    #[test]
    fn hostile_output_never_panics_and_memory_stays_bounded() {
        for seed in 1..=40u64 {
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed);
            let (mut cols, mut rows) = (80u16, 24u16);
            let (mut widest, mut tallest) = (cols, rows);
            let mut terminal = Terminal::new(cols, rows, 500, "test");
            for step in 0..60 {
                let bytes = hostile(&mut rng);
                // Cut anywhere, so a sequence spans writes, and a resize lands inside one.
                let cut = rng.below(bytes.len() as u64 + 1) as usize;
                terminal.feed(&bytes[..cut]);
                if rng.below(4) == 0 {
                    cols = 1 + rng.below(300) as u16;
                    rows = 1 + rng.below(100) as u16;
                    (widest, tallest) = (widest.max(cols), tallest.max(rows));
                    terminal.resize(cols, rows);
                }
                terminal.feed(&bytes[cut..]);
                let (_, y, _) = terminal.cursor();
                assert!(y < rows, "seed {seed} step {step}: row {y} of {rows}");
                // A narrower screen keeps the column as it was, as tmux's does when it
                // doesn't rewrap (the alternate screen), and with autowrap off a character
                // past the edge is dropped: only a move brings it back. A carriage return
                // always does.
                terminal.feed(b"\x1b\\\r");
                let (x, y, _) = terminal.cursor();
                assert!(
                    x == 0 && y < rows,
                    "seed {seed} step {step}: cursor {x},{y} in {cols}x{rows}"
                );
                assert_eq!(terminal.size(), (cols, rows));
                // Every line as wide as the screen has been (a line keeps its cells), on
                // both screens; then the parser's string, up to 1 MiB and its doubling, and
                // the hyperlinks, at most 5000 of a 1 KiB URI and id each, held twice.
                let lines = 500 + 2 * usize::from(tallest);
                let bound = lines * (usize::from(widest) * 16 + 256) + (2 << 20) + (21 << 20);
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
