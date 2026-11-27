//! `rlxd attach <pty_id>` (handoff 0.10.3, 1.4): a PTY in this terminal, as `tmux -L asq
//! attach` puts a fleet pane in it. The escape hatch.
//!
//! - **The screen** comes from the PTY's stream: its snapshot drawn first, then its
//!   output as the program writes it. A client too slow to keep up is redrawn from a new
//!   snapshot.
//! - **Input** goes as tmux's goes: the terminal's keys are read as keys and sent by
//!   name (`pty.write {keys}`), so rlxd sends each one as the program's modes need, as
//!   tmux's server does for its clients; a character with no Ctrl or Alt goes as text,
//!   which is what `send-keys` sends for it; a paste goes as a paste. Input rlxd
//!   refuses rings the bell, and attach goes on. With `--read-only`, nothing is sent.
//! - **The size** follows this terminal, as tmux's `window-size latest` does: the PTY is
//!   resized to it at the start and whenever it changes, unless `--read-only`.
//! - **Ctrl-]** detaches, as in telnet. Attach ends too when the PTY's process ends, or
//!   the PTY is killed.
//!
//! A snapshot is the screen's text; the modes a program set before attach (mouse
//! reporting, application keys) are rlxd's to honour for input, not replayed to this
//! terminal.

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use rlxd_client::{Connection, Home, Next, Reply};
use rlxd_proto::{Data, PtySubscribeParams, check_pty_id, codes, event_types, methods};
use serde_json::{Value, json};

use crate::cli::{self, AskError};
use crate::out;
use crate::pty::unbase64;

/// `rlxd attach`'s options.
#[derive(Debug)]
pub struct AttachArgs {
    /// The PTY.
    pub pty_id: String,
    /// Watch only: send no input, and leave the size alone.
    pub read_only: bool,
}

/// How attach ended.
enum Ended {
    Detached,
    Exited(u64),
    Gone,
    Lost(String),
}

/// `rlxd attach`.
pub fn attach(args: AttachArgs) -> ExitCode {
    if let Err(e) = check_pty_id("pty_id", &args.pty_id) {
        return out::usage(&format!("{}: {e}", args.pty_id));
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return out::usage("rlxd attach needs a terminal: run it in one");
    }
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => return cli::report_on_stderr(AskError::NotRunning(e.to_string())),
    };
    let mut input = match cli::open_as(&home, "tui", &cli::Target::default()) {
        Ok(connection) => connection,
        Err(e) => return cli::report_on_stderr(e),
    };
    if !args.read_only
        && let Ok((cols, rows)) = terminal::size()
        && let Err(code) = resize(&mut input, &args.pty_id, cols, rows, true)
    {
        return code;
    }
    let stream = match cli::open_as(&home, "tui", &cli::Target::default()) {
        Ok(connection) => connection,
        Err(e) => return cli::report_on_stderr(e),
    };
    let params = match PtySubscribeParams::new(&args.pty_id, args.read_only) {
        Ok(params) => params,
        Err(e) => return out::usage(&e.to_string()),
    };
    let mut subscription = match stream.subscribe_pty(&params) {
        Ok(subscription) => subscription,
        Err(e) => return cli::report_on_stderr(AskError::Failed(e.to_string())),
    };
    let first = match subscription.next_frame() {
        Ok(Next::Event(event)) => event,
        Ok(Next::End(reply)) => return out::print_reply_on_stderr(&reply),
        Err(e) => return cli::report_on_stderr(AskError::Failed(e.to_string())),
    };
    let screen = Screen::take(args.read_only);
    let mut stdout = std::io::stdout();
    let first: Value = serde_json::from_str(&first.line).unwrap_or_default();
    draw(&mut stdout, &first["data"]);
    if first["data"]["exited"] == true {
        // Ended before attach came: its pty.exited is older than the snapshot.
        drop(screen);
        let status = first["data"]["status"].as_u64().unwrap_or(0);
        println!("[the process exited, status {status}]");
        return ExitCode::SUCCESS;
    }
    // The stream on a thread of its own; this one reads the keyboard.
    let (ended_tx, ended_rx) = mpsc::channel::<Ended>();
    std::thread::spawn(move || {
        let ended = follow(&mut subscription);
        let _ = ended_tx.send(ended);
    });
    let ended = read_keys(&mut input, &args, &ended_rx);
    drop(screen);
    let said = match &ended {
        Ended::Detached => "[detached]".to_owned(),
        Ended::Exited(status) => format!("[the process exited, status {status}]"),
        Ended::Gone => "[the pty is gone]".to_owned(),
        Ended::Lost(why) => format!("[lost the pty: {why}]"),
    };
    println!("{said}");
    match ended {
        Ended::Lost(_) => ExitCode::from(out::FAILED),
        _ => ExitCode::SUCCESS,
    }
}

/// The terminal in raw mode on the alternate screen, put back as it was when dropped.
struct Screen {
    paste: bool,
}

impl Screen {
    fn take(read_only: bool) -> Self {
        let _ = terminal::enable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, EnterAlternateScreen);
        if !read_only {
            let _ = execute!(stdout, EnableBracketedPaste);
        }
        Self { paste: !read_only }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        if self.paste {
            let _ = execute!(stdout, DisableBracketedPaste);
        }
        let _ = stdout.write_all(b"\x1b[0m\x1b[?25h");
        let _ = execute!(stdout, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

/// Draws a snapshot's screen: its rows from the top, then the cursor where it is.
fn draw(stdout: &mut impl Write, screen: &Value) {
    let text = screen["text"].as_str().unwrap_or_default();
    let text = text.strip_suffix('\n').unwrap_or(text);
    let mut out = Vec::with_capacity(text.len() + 64);
    out.extend_from_slice(b"\x1b[?25l\x1b[0m\x1b[H\x1b[2J");
    out.extend_from_slice(text.replace('\n', "\r\n").as_bytes());
    out.extend_from_slice(b"\x1b[0m");
    let x = screen["cursor"]["x"].as_u64().unwrap_or(0);
    let y = screen["cursor"]["y"].as_u64().unwrap_or(0);
    out.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
    if screen["cursor"]["visible"] != false {
        out.extend_from_slice(b"\x1b[?25h");
    }
    let _ = stdout.write_all(&out);
    let _ = stdout.flush();
}

/// Writes the stream to the terminal until it ends.
fn follow(subscription: &mut rlxd_client::Subscription) -> Ended {
    let mut stdout = std::io::stdout();
    loop {
        let event = match subscription.next_frame() {
            Ok(Next::Event(event)) => event,
            Ok(Next::End(_)) => return Ended::Gone,
            Err(e) => return Ended::Lost(e.to_string()),
        };
        let Ok(event) = serde_json::from_str::<Value>(&event.line) else {
            continue;
        };
        match event["type"].as_str() {
            Some(event_types::PTY_SNAPSHOT) => draw(&mut stdout, &event["data"]),
            Some(event_types::PTY_OUTPUT) => {
                let bytes = event["data"]["bytes_b64"]
                    .as_str()
                    .and_then(unbase64)
                    .unwrap_or_default();
                let _ = stdout.write_all(&bytes);
                let _ = stdout.flush();
            }
            Some(event_types::PTY_EXITED) => {
                return Ended::Exited(event["data"]["status"].as_u64().unwrap_or(0));
            }
            _ => {}
        }
    }
}

/// Reads the keyboard until Ctrl-] or the stream ends, sending what it reads.
fn read_keys(input: &mut Connection, args: &AttachArgs, ended: &mpsc::Receiver<Ended>) -> Ended {
    loop {
        if let Ok(ended) = ended.try_recv() {
            return ended;
        }
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => return Ended::Lost(e.to_string()),
        }
        let event = match event::read() {
            Ok(event) => event,
            Err(e) => return Ended::Lost(e.to_string()),
        };
        if let Event::Key(key) = &event
            && is_detach(key)
        {
            return Ended::Detached;
        }
        if args.read_only {
            continue;
        }
        let sent = match event {
            Event::Key(key) => match input_of(&key) {
                Some(params) => write(input, &args.pty_id, params),
                None => Sent::Yes,
            },
            Event::Paste(text) => write(input, &args.pty_id, json!({ "paste": text })),
            Event::Resize(cols, rows) => match resize(input, &args.pty_id, cols, rows, false) {
                Ok(()) => Sent::Yes,
                Err(_) => Sent::Ended,
            },
            _ => Sent::Yes,
        };
        match sent {
            Sent::Yes => {}
            // rlxd took none of it (too long a paste, a program that reads nothing):
            // the bell says so, and this goes on.
            Sent::Refused => {
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(b"\x07");
                let _ = stdout.flush();
            }
            Sent::Ended => {
                // The stream says why, an exit or the PTY's end: let it.
                if let Ok(ended) = ended.recv_timeout(Duration::from_secs(2)) {
                    return ended;
                }
                return Ended::Lost("rlxd took no more input".to_owned());
            }
        }
    }
}

/// What `pty.write` sends for `key`: a character with no Ctrl or Alt as text, which is
/// what `send-keys` sends for it and is never refused (a no-break space, an ideographic
/// space), any other key by its tmux name.
fn input_of(key: &KeyEvent) -> Option<Value> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    if let KeyCode::Char(c) = key.code
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return Some(json!({ "text": c.to_string() }));
    }
    key_name(key).map(|name| json!({ "keys": [name] }))
}

/// Ctrl-], which a terminal sends as GS (0x1d) and crossterm reads as Ctrl-5.
fn is_detach(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(']' | '5'))
}

/// A key as tmux names it, such as `C-c`, `M-x`, `S-Up` or `Enter`.
pub fn key_name(key: &KeyEvent) -> Option<String> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let mut name = String::new();
    let mods = key.modifiers;
    if mods.contains(KeyModifiers::CONTROL) {
        name.push_str("C-");
    }
    if mods.contains(KeyModifiers::ALT) {
        name.push_str("M-");
    }
    // A shifted character is its own character; a shifted named key says so.
    let shift = mods.contains(KeyModifiers::SHIFT);
    let named = |base: &str| {
        if shift {
            format!("S-{base}")
        } else {
            base.to_owned()
        }
    };
    let base = match key.code {
        KeyCode::Char(' ') => named("Space"),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => named("Enter"),
        KeyCode::Esc => named("Escape"),
        KeyCode::Tab => named("Tab"),
        KeyCode::BackTab => "BTab".to_owned(),
        KeyCode::Backspace => named("BSpace"),
        KeyCode::Up => named("Up"),
        KeyCode::Down => named("Down"),
        KeyCode::Left => named("Left"),
        KeyCode::Right => named("Right"),
        KeyCode::Home => named("Home"),
        KeyCode::End => named("End"),
        KeyCode::PageUp => named("PageUp"),
        KeyCode::PageDown => named("PageDown"),
        KeyCode::Insert => named("IC"),
        KeyCode::Delete => named("DC"),
        KeyCode::F(n) if (1..=12).contains(&n) => named(&format!("F{n}")),
        _ => return None,
    };
    name.push_str(&base);
    Some(name)
}

/// How a write went.
enum Sent {
    Yes,
    /// rlxd refused it, and the PTY goes on.
    Refused,
    /// The PTY has ended or gone, or the connection with it.
    Ended,
}

fn write(input: &mut Connection, pty_id: &str, mut params: Value) -> Sent {
    params["pty_id"] = json!(pty_id);
    let params: Data = params.as_object().cloned().unwrap_or_default();
    match input.request(methods::PTY_WRITE, params) {
        Ok(reply) if reply.is_ok() => Sent::Yes,
        Ok(reply) => {
            let code = reply.error().map(|e| e.code.as_str()).unwrap_or_default();
            if matches!(code, codes::PTY_EXITED | codes::UNKNOWN_PTY) {
                Sent::Ended
            } else {
                Sent::Refused
            }
        }
        Err(_) => Sent::Ended,
    }
}

/// Sizes the PTY to `cols` by `rows`. `first`, at attach's start: one whose process has
/// ended takes no size, and attach still shows its last screen.
fn resize(
    input: &mut Connection,
    pty_id: &str,
    cols: u16,
    rows: u16,
    first: bool,
) -> Result<(), ExitCode> {
    let params =
        json!({"pty_id": pty_id, "cols": cols.clamp(1, 1000), "rows": rows.clamp(1, 1000)});
    let params: Data = params.as_object().cloned().unwrap_or_default();
    match input.request(methods::PTY_RESIZE, params) {
        Ok(reply) if reply.is_ok() => Ok(()),
        Ok(reply) if first && reply.error().is_some_and(|e| e.code == codes::PTY_EXITED) => Ok(()),
        Ok(reply) => Err(refused(&reply)),
        Err(e) => Err(cli::report_on_stderr(AskError::Failed(e.to_string()))),
    }
}

fn refused(reply: &Reply) -> ExitCode {
    out::print_reply_on_stderr(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn keys_are_named_as_tmux_names_them() {
        let none = KeyModifiers::NONE;
        let cases = [
            (key(KeyCode::Char('c'), KeyModifiers::CONTROL), "C-c"),
            (key(KeyCode::Char('x'), KeyModifiers::ALT), "M-x"),
            (key(KeyCode::Char('A'), KeyModifiers::SHIFT), "A"),
            (key(KeyCode::Char(' '), none), "Space"),
            (key(KeyCode::Enter, KeyModifiers::SHIFT), "S-Enter"),
            (
                key(KeyCode::Up, KeyModifiers::CONTROL | KeyModifiers::ALT),
                "C-M-Up",
            ),
            (key(KeyCode::BackTab, KeyModifiers::SHIFT), "BTab"),
            (key(KeyCode::F(5), none), "F5"),
            (key(KeyCode::Delete, none), "DC"),
            (key(KeyCode::Char('é'), none), "é"),
        ];
        for (event, name) in cases {
            assert_eq!(key_name(&event).as_deref(), Some(name), "{event:?}");
        }
        assert!(is_detach(&key(KeyCode::Char(']'), KeyModifiers::CONTROL)));
        assert!(is_detach(&key(KeyCode::Char('5'), KeyModifiers::CONTROL)));
        assert!(!is_detach(&key(KeyCode::Char(']'), none)));
    }
}
