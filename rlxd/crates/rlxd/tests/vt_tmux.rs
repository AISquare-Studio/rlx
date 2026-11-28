//! rlxd's screen model against tmux itself (handoff 0.10.3, 1.4): the same bytes go to a
//! tmux pane and to `rlxd::vt::Terminal`, and `capture-pane -e -p` must equal the model's
//! capture byte for byte, the cursor too.
//!
//! The bytes reach tmux through a pseudo-terminal, which turns each `\n` into `\r\n`
//! (ONLCR), so the model gets them turned the same way. Without tmux on `PATH` the test
//! says so and passes, unless `RLXD_TMUX_REQUIRED=1`, as rlxd.yml sets on macOS and
//! Linux, where it fails instead.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use rlxd::pty::{keys, paste};
use rlxd::vt::Terminal;

/// One screen: a name, its size, and the bytes a program writes.
struct Fixture {
    name: &'static str,
    cols: u16,
    rows: u16,
    bytes: Vec<u8>,
    /// Lines of history to capture too, as `-S -n`.
    scrollback: u32,
    /// A size to resize to once the bytes are in, as `resize-window` does.
    resize: Option<(u16, u16)>,
    /// The oldest tmux whose frame this matches: older ones print it otherwise.
    since: Option<(u32, u32)>,
}

fn fixture(name: &'static str, bytes: impl Into<Vec<u8>>) -> Fixture {
    Fixture {
        name,
        cols: 80,
        rows: 24,
        bytes: bytes.into(),
        scrollback: 0,
        resize: None,
        since: None,
    }
}

/// A paragraph that wraps, for the resize fixtures.
fn paragraph() -> String {
    format!(
        "{}\n\x1b[1mshort bold\x1b[0m\n{}中文{}\nend",
        "word ".repeat(30),
        "x".repeat(37),
        "y".repeat(50)
    )
}

fn fixtures() -> Vec<Fixture> {
    let mut all = vec![
        fixture("the handoff's", "\x1b[31mred\x1b[0m plain"),
        fixture(
            "attributes",
            "\x1b[1mbold\x1b[0m \x1b[2mdim\x1b[22m \x1b[3mital\x1b[23m \x1b[4munder\x1b[24m \
             \x1b[7mrev\x1b[27m \x1b[9mstrike\x1b[29m \x1b[5mblink\x1b[25m \x1b[8mhid\x1b[28m \
             \x1b[53mover\x1b[55m \x1b[1;3;4mmany\x1b[22;23mfewer\x1b[0m end",
        ),
        fixture(
            "underline styles and colours",
            "\x1b[4:3mcurly\x1b[4:0m \x1b[21mdouble\x1b[24m \x1b[4;58;5;196mus\x1b[59mplain \
             \x1b[4;58:2::1:2:3mrgb\x1b[0m done",
        ),
        fixture(
            "colours in their forms",
            "\x1b[31ma\x1b[38;5;1mb\x1b[91mc\x1b[38;2;10;20;30md\x1b[42me\x1b[48;5;200mf\
             \x1b[103mg\x1b[48:2::1:2:3mh\x1b[38:5:33mi\x1b[39;49mj\x1b[0mk",
        ),
        fixture("256 colours", {
            let mut text = String::new();
            for row in 0..8 {
                for col in 0..32 {
                    text.push_str(&format!("\x1b[38;5;{}m#", row * 32 + col));
                }
                text.push_str("\x1b[0m\n");
            }
            text
        }),
        fixture("truecolour", {
            let mut text = String::new();
            for i in 0..60u32 {
                text.push_str(&format!(
                    "\x1b[48;2;{};{};{}m ",
                    i * 4,
                    255 - i * 4,
                    (i * 7) % 256
                ));
            }
            text.push_str("\x1b[0m\n");
            text
        }),
        fixture("wide characters", "中文 text ✓ 한국어\nx中y\n全角スペース"),
        fixture(
            "combining marks",
            "e\u{301} a\u{308} n\u{303}o\n\u{301}alone",
        ),
        fixture(
            "cursor moves",
            "\x1b[5;10Hx\x1b[2;3Hy\x1b[10Cz\x1b[3Dw\x1b[Aq\x1b[2Bp\rr\x1b[Gs\x1b[3dt\x1b[8;70Hend",
        ),
        fixture(
            "a line wrap",
            format!("{}\x1b[33m{}", "a".repeat(85), "b".repeat(90)),
        ),
        fixture(
            "a wide character at the edge",
            format!("{}中", "a".repeat(79)),
        ),
        fixture(
            "erase with a background",
            "\x1b[44m\x1b[2J\x1b[Hblue\x1b[K\n\x1b[41mred\x1b[0m\x1b[K tail\x1b[45m\x1b[1K\x1b[0m\
             \n\x1b[46m\x1b[3X\x1b[0mz\n\x1b[42m\x1b[2K\x1b[0m\nwhole line above",
        ),
        fixture(
            "attributes across a line end",
            "\x1b[1;31mbold red\nstill\x1b[0m\n\x1b[4m\nunder",
        ),
        fixture(
            "a reset before spaces",
            "\x1b[31mred\x1b[0m   \n\x1b[41m   \x1b[0m\n\x1b[41mX\x1b[0m",
        ),
        fixture(
            "a scroll region",
            "\x1b[2;4r\x1b[2;1H1\n2\n3\n4\n5\n6\x1b[r\x1b[10;1Hafter",
        ),
        fixture("a clear screen", "junk\nmore\n\x1b[2J\x1b[Hfresh"),
        fixture(
            "the alternate screen",
            "main text\n\x1b[?1049h\x1b[Halt screen\x1b[?1049lback\n\x1b[?47hold\x1b[?47l",
        ),
        Fixture {
            // tmux 3.6 keeps a tab as one cell, which capture prints as a tab.
            since: Some((3, 6)),
            ..fixture(
                "tabs",
                "a\tb\tc\n\x1b[31m\tred tab\x1b[0m\n12345678\t9\n\x1b[3g\tno stops",
            )
        },
        fixture(
            "insert and delete",
            "abcdef\x1b[3G\x1b[2@XY\x1b[2P\n123456\x1b[2G\x1b[4h99\x1b[4l\nline\x1b[2G\x1b[3X",
        ),
        fixture(
            "lines inserted and deleted",
            "one\ntwo\nthree\nfour\nfive\x1b[2;1H\x1b[2L\x1b[5;1H\x1b[1M",
        ),
        fixture(
            "the line-drawing set",
            "\x1b(0lqqk\x1b(B x \x0eaaa\x0f done",
        ),
        fixture("a reset", "stuff\nmore\x1bcafter"),
        fixture("a repeat", "x\x1b[5b-\x1b[3b\x1b[31m=\x1b[2b"),
        fixture(
            "hyperlinks",
            "\x1b]8;;http://example.com\x1b\\link\x1b]8;;\x1b\\ \x1b]8;id=a;http://x\x07two\x1b]8;;\x07",
        ),
        fixture("a reverse index", "\x1b[Htop\x1bMnew\x1b[3;1Hthird\x1bMup"),
        fixture("the alignment test", "\x1b#8\x1b[3;3Hok"),
        fixture(
            "bad UTF-8",
            b"a\xe4b\xff c\xe4\xb8 d\xc3\x28e\n\xed\xa0\x80".to_vec(),
        ),
        fixture(
            "origin mode",
            "\x1b[5;10r\x1b[?6h\x1b[2;3Horigin\x1b[?6l\x1b[rdone",
        ),
        fixture(
            "index and next line",
            "a\x1bEb\x1bDc\x1b[20;1H\x1bD\x1bD\x1bDbottom",
        ),
        fixture("scroll up and down", "1\n2\n3\n4\x1b[2S\x1b[1Tx"),
        fixture(
            "autowrap off",
            format!("\x1b[?7l{}\x1b[?7h\nnext", "x".repeat(85)),
        ),
        fixture(
            "a title",
            "\x1b]2;a title\x07visible \x1b]0;other\x1b\\text",
        ),
        fixture(
            "a backspace over a wrap",
            format!("{}\x08\x08XY", "z".repeat(82)),
        ),
        fixture(
            "save and restore",
            "\x1b[3;5H\x1b[1msaved\x1b7\x1b[10;10Hmoved\x1b8back",
        ),
    ];
    all.push(Fixture {
        scrollback: 10,
        ..fixture("history", {
            let mut text = String::new();
            for i in 0..40 {
                text.push_str(&format!("\x1b[3{}mline {i}\n", i % 8));
            }
            text
        })
    });
    for (name, resize) in [
        ("a resize narrower", (30, 24)),
        ("a resize wider", (120, 24)),
        ("a resize shorter", (80, 5)),
        ("a resize taller after scrolling", (80, 30)),
    ] {
        let mut bytes = paragraph();
        if name.ends_with("scrolling") {
            bytes = format!("{}{bytes}", "filler\n".repeat(30));
        }
        all.push(Fixture {
            resize: Some(resize),
            scrollback: 40,
            ..fixture(name, bytes)
        });
    }
    all.push(Fixture {
        resize: Some((40, 12)),
        ..fixture(
            "a resize on the alternate screen",
            format!(
                "main {}\n\x1b[?1049h\x1b[Halt {}",
                "m".repeat(70),
                "a".repeat(70)
            ),
        )
    });
    // #2585: CSI ? n (996 asks for the colour theme, which no client gives) ends a
    // repeat as any known sequence does. tmux 3.6 added it.
    all.push(Fixture {
        since: Some((3, 6)),
        ..fixture("a private DSR ends a repeat", "a\x1b[?996n\x1b[3b|")
    });
    // #2685 m2: combining marks up to tmux's 32 bytes a cell, and either side of it: tmux
    // keeps a cell of exactly 32, which its packing reads back as empty.
    for (name, first, marks) in [
        ("a cell of 31 bytes", "e", 15),
        ("a cell of 32 bytes", "é", 15),
        ("a cell of 33 bytes", "e", 16),
    ] {
        // tmux 3.6 raised a cell's limit from 21 bytes to 32.
        all.push(Fixture {
            cols: 20,
            rows: 3,
            since: Some((3, 6)),
            ..fixture(name, format!("a{first}{}b|", "\u{301}".repeat(marks)))
        });
    }
    // #2699: a 32-byte cell on a wide base, which tmux reads back 2 wide, and the same
    // rewrapped one column narrower.
    let wide = format!("a中\u{20d0}{}\u{301}z|", "\u{301}".repeat(13));
    all.push(Fixture {
        cols: 20,
        rows: 3,
        since: Some((3, 6)),
        ..fixture("a 32-byte cell on a wide base", wide.clone())
    });
    all.push(Fixture {
        cols: 20,
        rows: 3,
        resize: Some((19, 3)),
        since: Some((3, 6)),
        ..fixture("a 32-byte cell on a wide base, narrower", wide)
    });
    // #2678 minor 2: a link that starts in the last column and wraps. tmux prints the
    // last cell's codes again as it closes the link at the line's end.
    all.push(Fixture {
        cols: 80,
        rows: 5,
        ..fixture(
            "a link from the last column, wrapping",
            format!(
                "{}\x1b]8;;https://example.com/a/long/path\x1b\\https://example.com/a/long/path\x1b]8;;\x1b\\ done",
                "x".repeat(79)
            ),
        )
    });
    all.push(Fixture {
        cols: 20,
        rows: 4,
        ..fixture(
            "a coloured link that ends each line",
            "\x1b[31m\x1b]8;id=a;http://x\x1b\\0123456789abcdefghijKLMNOPQRSTUVWXYZ\x1b]8;;\x1b\\\x1b[m",
        )
    });
    // #2585: a hyperlink that isn't UTF-8 is kept, its bytes escaped.
    all.push(fixture(
        "a hyperlink that isn't UTF-8",
        &b"\x1b]8;;http://e\xe9\xff/\x1b\\link\x1b]8;id=\xc3;http://x\x07two\x1b]8;;\x07"[..],
    ));
    all.push(Fixture {
        cols: 20,
        rows: 6,
        ..fixture(
            "a small screen",
            "a long line that wraps around twice here\nshort\n中文中文中文中文中文x",
        )
    });
    all
}

/// What a pseudo-terminal's ONLCR makes of `bytes`.
fn onlcr(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

/// A private tmux server, killed when dropped.
struct Tmux {
    socket: String,
    dir: PathBuf,
}

impl Tmux {
    fn start() -> Option<Self> {
        let found = Command::new("tmux").arg("-V").output().ok()?;
        if !found.status.success() {
            return None;
        }
        // One server each: the tests run in parallel, and each kills its own.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket = format!("rlxd-vt-{}-{n}", std::process::id());
        let dir = std::env::temp_dir().join(&socket);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Some(Self { socket, dir })
    }

    fn version(&self) -> String {
        let out = Command::new("tmux").arg("-V").output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn run(&self, args: &[&str]) -> Vec<u8> {
        let out = Command::new("tmux")
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "tmux {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// One idle session for the test's whole life, so the server, and any server option
    /// set on it, stays up while each case's own session is opened and closed in turn.
    fn hold(&self) {
        self.run(&["new-session", "-d", "-s", "hold", "exec sleep 3600"]);
    }

    /// Ends `session`, and with it its pseudo-terminal. Each case's session is closed once
    /// captured: left open to the test's end, a session per fixture and per input held
    /// about 65 at once with the tests in parallel, and a macOS runner ran out ("fork
    /// failed: Device not configured", rlxd.yml run 46107527723).
    fn close(&self, session: &str) {
        self.run(&["kill-session", "-t", session]);
    }

    /// Waits until the pane's screen stops changing.
    fn settle(&self, session: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = self.run(&["capture-pane", "-p", "-t", session]);
        loop {
            std::thread::sleep(Duration::from_millis(150));
            let now = self.run(&["capture-pane", "-p", "-t", session]);
            if now == last || Instant::now() > deadline {
                return;
            }
            last = now;
        }
    }

    /// The server's version, as (major, minor): 3.8 is (3, 8), 3.6a (3, 6).
    fn numbers(&self) -> (u32, u32) {
        let version = self.version();
        let digits = version.trim_start_matches(|c: char| !c.is_ascii_digit());
        let mut parts = digits.split('.');
        let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts
            .next()
            .map(|p| p.trim_end_matches(|c: char| !c.is_ascii_digit()))
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        (major, minor)
    }

    /// Shows `fixture` in a pane of its own, and once the screen is still, returns its
    /// capture and the cursor, and closes the pane's session.
    fn show(&self, n: usize, fixture: &Fixture) -> (Vec<u8>, String) {
        let file = self.dir.join(format!("{n}.bin"));
        std::fs::write(&file, &fixture.bytes).unwrap();
        let session = format!("f{n}");
        let command = format!("cat '{}'; exec sleep 60", file.display());
        let (cols, rows) = (fixture.cols.to_string(), fixture.rows.to_string());
        let resized = fixture.resize.map(|(x, y)| (x.to_string(), y.to_string()));
        self.run(&[
            "new-session",
            "-d",
            "-s",
            &session,
            "-x",
            &cols,
            "-y",
            &rows,
            &command,
        ]);
        if let Some((x, y)) = &resized {
            // The bytes first, then the resize, as the model takes them.
            self.settle(&session);
            self.run(&["resize-window", "-t", &session, "-x", x, "-y", y]);
        }
        let start = format!("-{}", fixture.scrollback);
        let capture = |s: &str| {
            let mut args = vec!["capture-pane", "-e", "-p", "-t", s];
            if fixture.scrollback > 0 {
                args.extend(["-S", start.as_str()]);
            }
            let text = self.run(&args);
            let cursor = self.run(&[
                "display-message",
                "-p",
                "-t",
                s,
                "#{cursor_x},#{cursor_y} #{pane_width}x#{pane_height}",
            ]);
            (text, String::from_utf8_lossy(&cursor).trim().to_owned())
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = capture(&session);
        let shown = loop {
            std::thread::sleep(Duration::from_millis(150));
            let now = capture(&session);
            if now == last || Instant::now() > deadline {
                break now;
            }
            last = now;
        };
        self.close(&session);
        shown
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn shown(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).escape_debug().to_string()
}

fn first_difference(a: &[u8], b: &[u8]) -> String {
    let at = a
        .iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()));
    let from = at.saturating_sub(24);
    format!(
        "at byte {at}:\n  tmux: {}\n  rlxd: {}",
        shown(&a[from..(at + 40).min(a.len())]),
        shown(&b[from..(at + 40).min(b.len())])
    )
}

fn check(tmux: &Tmux, fixtures: &[Fixture]) -> Vec<String> {
    let mut failures = Vec::new();
    let version = tmux.numbers();
    tmux.hold();
    for (n, fixture) in fixtures.iter().enumerate() {
        if let Some(since) = fixture.since
            && version < since
        {
            eprintln!(
                "{}: skipped, as tmux {}.{} prints it as tmux before {}.{} did",
                fixture.name, version.0, version.1, since.0, since.1
            );
            continue;
        }
        let (expected, cursor) = tmux.show(n, fixture);
        let mut model = Terminal::new(fixture.cols, fixture.rows, 2000, "test");
        model.feed(&onlcr(&fixture.bytes));
        if let Some((x, y)) = fixture.resize {
            model.resize(x, y);
        }
        let text = model.capture(fixture.scrollback);
        let (x, y, _) = model.cursor();
        let (cols, rows) = model.size();
        let ours = format!("{x},{y} {cols}x{rows}");
        if expected != text.as_bytes() {
            failures.push(format!(
                "{}: the frame differs {}",
                fixture.name,
                first_difference(&expected, text.as_bytes())
            ));
        } else if cursor != ours {
            failures.push(format!(
                "{}: tmux's cursor is {cursor}, rlxd's {ours}",
                fixture.name
            ));
        }
    }
    failures
}

fn tmux_or_skip() -> Option<Tmux> {
    match Tmux::start() {
        Some(tmux) => Some(tmux),
        None if std::env::var_os("RLXD_TMUX_REQUIRED").is_some_and(|v| v == "1") => {
            panic!("RLXD_TMUX_REQUIRED=1, and tmux is not on PATH")
        }
        None => {
            eprintln!("tmux is not on PATH: the frames were not compared");
            None
        }
    }
}

/// #2585: a stray DCS swallows everything after it, ESC included, until tmux's ground
/// timer gives it up five seconds on. tmux and the model are given the same writes six
/// seconds apart.
#[test]
fn a_stray_dcs_is_given_up_after_five_seconds_as_tmux_does() {
    let Some(tmux) = tmux_or_skip() else {
        return;
    };
    let (first, second) = (&b"before\r\n\x1bPq"[..], &b"\x1b[32mhello\x1b[0m"[..]);
    let one = tmux.dir.join("first.bin");
    let two = tmux.dir.join("second.bin");
    std::fs::write(&one, first).unwrap();
    std::fs::write(&two, second).unwrap();
    let command = format!(
        "cat '{}'; sleep 6; cat '{}'; exec sleep 60",
        one.display(),
        two.display()
    );
    let start = Instant::now();
    tmux.run(&[
        "new-session",
        "-d",
        "-s",
        "dcs",
        "-x",
        "40",
        "-y",
        "4",
        &command,
    ]);
    std::thread::sleep(Duration::from_secs(7));
    tmux.settle("dcs");
    let expected = tmux.run(&["capture-pane", "-e", "-p", "-t", "dcs"]);
    let cursor = tmux.run(&[
        "display-message",
        "-p",
        "-t",
        "dcs",
        "#{cursor_x},#{cursor_y}",
    ]);
    let mut model = Terminal::new(40, 4, 2000, "test");
    model.feed_at(first, start);
    model.feed_at(second, start + Duration::from_secs(6));
    let text = model.capture(0);
    assert_eq!(
        shown(&expected),
        shown(text.as_bytes()),
        "the frames differ {}",
        first_difference(&expected, text.as_bytes())
    );
    let (x, y, _) = model.cursor();
    assert_eq!(String::from_utf8_lossy(&cursor).trim(), format!("{x},{y}"));
}

/// Bytes to write, then a size to take, if any.
type Stage = (&'static [u8], Option<(u16, u16)>);

/// The #28 tester's (B), and #2585's major 1: a cursor ?1049h saved at row 20 outlives the
/// alternate screen; ?1047h after a shorter resize, another resize, then ?1049l restores
/// it. tmux reads past its lines there and lives; rlxd clamps the cursor to the screen.
/// Both are given the same writes with the same resizes between them.
#[test]
fn a_stale_saved_cursor_comes_back_as_tmux_shows_it() {
    let Some(tmux) = tmux_or_skip() else {
        return;
    };
    let stages: [Stage; 3] = [
        (b"\x1b[20;1H\x1b[?1049h\x1b[?1049l", Some((80, 10))),
        (b"\x1b[?1047h", Some((40, 12))),
        (b"\x1b[?1049lafter\r\n", None),
    ];
    let mut files = Vec::new();
    for (n, (bytes, _)) in stages.iter().enumerate() {
        let file = tmux.dir.join(format!("stage{n}.bin"));
        std::fs::write(&file, bytes).unwrap();
        files.push(format!("cat '{}'", file.display()));
    }
    let command = format!("{}; exec sleep 60", files.join("; sleep 2; "));
    tmux.run(&[
        "new-session",
        "-d",
        "-s",
        "stale",
        "-x",
        "80",
        "-y",
        "24",
        &command,
    ]);
    // Each resize lands between two writes: at 1 s and 3 s, the writes at 0, 2 and 4.
    std::thread::sleep(Duration::from_secs(1));
    tmux.run(&["resize-window", "-t", "stale", "-x", "80", "-y", "10"]);
    std::thread::sleep(Duration::from_secs(2));
    tmux.run(&["resize-window", "-t", "stale", "-x", "40", "-y", "12"]);
    std::thread::sleep(Duration::from_millis(2500));
    tmux.settle("stale");
    let expected = tmux.run(&["capture-pane", "-e", "-p", "-t", "stale"]);
    let cursor = tmux.run(&[
        "display-message",
        "-p",
        "-t",
        "stale",
        "#{cursor_x},#{cursor_y}",
    ]);
    let mut model = Terminal::new(80, 24, 2000, "test");
    for (bytes, resize) in stages {
        model.feed(&onlcr(bytes));
        if let Some((cols, rows)) = resize {
            model.resize(cols, rows);
        }
    }
    let text = model.capture(0);
    assert_eq!(
        shown(&expected),
        shown(text.as_bytes()),
        "the frames differ {}",
        first_difference(&expected, text.as_bytes())
    );
    let (x, y, _) = model.cursor();
    assert_eq!(String::from_utf8_lossy(&cursor).trim(), format!("{x},{y}"));
}

#[test]
fn every_fixture_captures_as_tmux_captures_it() {
    let Some(tmux) = tmux_or_skip() else {
        return;
    };
    let fixtures = fixtures();
    let failures = check(&tmux, &fixtures);
    assert!(
        failures.is_empty(),
        "{} of {} fixtures differ from {}:\n{}",
        failures.len(),
        fixtures.len(),
        tmux.version(),
        failures.join("\n\n")
    );
}

/// A program that sets `argv[1]`'s modes (as hex), says READY, then prints every byte
/// it reads, in hex, in raw mode.
const HEX_ECHO: &str = r#"
import os, sys, tty
tty.setraw(0)
os.write(1, bytes.fromhex(sys.argv[1]))
os.write(1, b"READY\r\n")
while True:
    b = os.read(0, 4096)
    if not b:
        break
    os.write(1, b.hex().encode())
"#;

/// One case of input: the modes a program sets, then keys or a paste.
struct Input {
    name: &'static str,
    setup: &'static str,
    keys: &'static [&'static str],
    paste: Option<&'static str>,
    since: Option<(u32, u32)>,
}

/// The keys the fleet sends today (core/keys.py, remote_server.py, the agent adapters),
/// and their modified forms.
const FLEET_KEYS: &[&str] = &[
    "Enter", "Escape", "Tab", "BTab", "BSpace", "Space", "Up", "Down", "Left", "Right", "Home",
    "End", "PageUp", "PageDown", "PPage", "NPage", "Delete", "DC", "IC", "F1", "F2", "F3", "F4",
    "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "C-c", "C-d", "C-j", "C-l", "C-o", "C-r",
    "C-u", "C-@", "0", "1", "9", "y", "n", "M-x", "M-Enter", "C-M-x", "C-Up", "M-Up", "S-Up",
    "C-M-S-Up", "C-M-S-F5", "S-F1", "C-S-F12", "M-S-F12", "S-Enter", "C-Enter", "C-BSpace",
    "M-BSpace", "C-1", "C-/", "C-?", "C-[", "C-]", "C-^", "C-_", "é", "M-é", "Entr", "KP5",
    "KPEnter", "^c", "C-Space", "S-Space",
];

const MODIFIED: &[&str] = &[
    "S-Enter", "C-Enter", "M-Enter", "C-c", "C-S-a", "C-1", "M-x", "C-M-x", "C-Tab", "S-Tab",
    "BTab", "C-BTab", "S-Space", "C-Space", "C-é", "Up", "C-Up",
];

const PASTE: &str = "line one\nline two\ttab\x1b[31mesc\x7fdel\x07bell\rcr é";

fn inputs() -> Vec<Input> {
    vec![
        Input {
            name: "the fleet's keys",
            setup: "",
            keys: FLEET_KEYS,
            paste: None,
            since: None,
        },
        Input {
            name: "application cursor keys and keypad",
            setup: "\x1b[?1h\x1b=",
            keys: &[
                "Up", "Down", "Left", "Right", "Home", "End", "M-Up", "C-Up", "KP5", "KP/",
                "KPEnter", "KP.", "F1",
            ],
            paste: None,
            since: None,
        },
        Input {
            // tmux before 3.5 encodes extended keys otherwise.
            name: "extended keys, modifyOtherKeys 1",
            setup: "\x1b[>4;1m",
            keys: MODIFIED,
            paste: None,
            since: Some((3, 5)),
        },
        Input {
            name: "extended keys, modifyOtherKeys 2",
            setup: "\x1b[>4;2m",
            keys: MODIFIED,
            paste: None,
            since: Some((3, 5)),
        },
        Input {
            // tmux 3.8 makes each pasted line safe (vis): before, ESC and DEL went raw.
            name: "a paste",
            setup: "",
            keys: &[],
            paste: Some(PASTE),
            since: Some((3, 8)),
        },
        Input {
            name: "a bracketed paste",
            setup: "\x1b[?2004h",
            keys: &[],
            paste: Some(PASTE),
            since: Some((3, 8)),
        },
    ]
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Tmux {
    /// What the program in `fixture`'s pane reads after `input`'s keys or paste, as
    /// hex.
    fn received(&self, n: usize, input: &Input) -> String {
        let script = self.dir.join("hex_echo.py");
        std::fs::write(&script, HEX_ECHO).unwrap();
        let session = format!("k{n}");
        let command = format!(
            "exec python3 '{}' '{}'",
            script.display(),
            hex(input.setup.as_bytes())
        );
        self.run(&[
            "new-session",
            "-d",
            "-s",
            &session,
            "-x",
            "200",
            "-y",
            "50",
            &command,
        ]);
        let ready = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(&self.run(&["capture-pane", "-p", "-t", &session]))
            .contains("READY")
        {
            assert!(
                Instant::now() < ready,
                "{}: the program never got ready",
                input.name
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        if let Some(text) = input.paste {
            self.run(&["set-buffer", "-b", "rlxd-test", "--", text]);
            self.run(&["paste-buffer", "-p", "-b", "rlxd-test", "-t", &session]);
        } else {
            let mut args = vec!["send-keys", "-t", &session, "--"];
            args.extend(input.keys);
            self.run(&args);
        }
        self.settle(&session);
        let text = self.run(&["capture-pane", "-p", "-J", "-S", "-", "-t", &session]);
        self.close(&session);
        let text = String::from_utf8_lossy(&text);
        text.split_once("READY")
            .map(|(_, after)| after.split_whitespace().collect())
            .unwrap_or_default()
    }
}

#[test]
fn keys_and_pastes_send_tmuxs_bytes() {
    let Some(tmux) = tmux_or_skip() else {
        return;
    };
    if Command::new("python3").arg("-V").output().is_err() {
        assert!(
            std::env::var_os("RLXD_TMUX_REQUIRED").is_none_or(|v| v != "1"),
            "RLXD_TMUX_REQUIRED=1, and python3 is not on PATH"
        );
        eprintln!("python3 is not on PATH: the keys were not compared");
        return;
    }
    // Extended keys as the fleet's tmux has them, before any pane asks.
    tmux.hold();
    tmux.run(&["set-option", "-s", "extended-keys", "on"]);
    let version = tmux.numbers();
    let mut failures = Vec::new();
    for (n, input) in inputs().iter().enumerate() {
        if let Some(since) = input.since
            && version < since
        {
            eprintln!(
                "{}: skipped, as tmux {}.{} sends it otherwise",
                input.name, version.0, version.1
            );
            continue;
        }
        let theirs = tmux.received(n, input);
        let mut model = Terminal::new(80, 24, 0, "test");
        model.feed(input.setup.as_bytes());
        let modes = model.input_modes();
        let ours = match input.paste {
            Some(text) => paste(text, modes.bracketed_paste),
            None => {
                let names: Vec<String> = input.keys.iter().map(|k| (*k).to_owned()).collect();
                keys::send_keys(&names, modes)
            }
        };
        if theirs != hex(&ours) {
            failures.push(format!(
                "{}:\n  tmux: {theirs}\n  rlxd: {}",
                input.name,
                hex(&ours)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} differ from {}:\n{}",
        failures.len(),
        inputs().len(),
        tmux.version(),
        failures.join("\n")
    );
}
