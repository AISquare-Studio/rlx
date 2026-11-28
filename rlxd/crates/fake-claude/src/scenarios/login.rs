//! `claude mcp login <name>`, as rlxd's `connectors.login` runs it in a PTY (handoff
//! 0.10.3, 2.4 and 7.7). Each prints what a login prints: a line naming the connector,
//! the URL to open, whose query holds secrets that rlxd's log must never carry, and a
//! wait for the browser. The URLs and their secrets are made up.
//!
//! - `mcp-login` signs in: after the wait, it says so and exits 0.
//! - `mcp-login-fails` says the sign-in failed after the wait, and exits 1.
//! - `mcp-login-http` prints an `http://` URL, then waits a minute, as for a browser.
//! - `mcp-login-silent` prints no URL, and exits 0.
//!
//! Each start is in the fixture log, as a run's are, with its `argv` and `exit`.

use std::io::Write;
use std::thread;
use std::time::Duration;

use serde_json::Value;

use crate::Log;

/// One login scenario: given the command line and the connector, it plays the login and
/// returns its exit code.
pub type Login = fn(&[String], &str) -> i32;

/// The login scenarios, by the name the binary is started as.
pub const ALL: &[(&str, Login)] = &[
    ("mcp-login", |argv, id| {
        play(argv, id, Some(HTTPS), Ending::SignedIn)
    }),
    ("mcp-login-fails", |argv, id| {
        play(argv, id, Some(HTTPS), Ending::Fails)
    }),
    ("mcp-login-http", |argv, id| {
        play(argv, id, Some(HTTP), Ending::Waits)
    }),
    ("mcp-login-silent", |argv, id| {
        play(argv, id, None, Ending::SignedIn)
    }),
];

/// The URL the logins print, its query made up: `state` and `code_challenge` stand for
/// the secrets a real one holds.
pub const HTTPS: &str = "https://claude.ai/oauth/mcp/authorize?client_id=fake-client\
    &state=fake-state-5e3c7a&code_challenge=fake-challenge-91d2";

/// `mcp-login-http`'s URL.
pub const HTTP: &str = "http://claude.ai/oauth/mcp/authorize?client_id=fake-client\
    &state=fake-state-5e3c7a&code_challenge=fake-challenge-91d2";

/// The query's secret values, which rlxd's log must not hold.
pub const SECRETS: &[&str] = &["fake-state-5e3c7a", "fake-challenge-91d2"];

/// The login scenario called `name`.
pub fn find(name: &str) -> Option<Login> {
    ALL.iter()
        .find(|(each, _)| *each == name)
        .map(|(_, login)| *login)
}

/// How a login ends.
#[derive(Clone, Copy)]
enum Ending {
    /// Signed in: exit 0.
    SignedIn,
    /// Failed: exit 1.
    Fails,
    /// Waits a minute, as for a browser that never comes.
    Waits,
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    // A PTY that closed ends the login anyway.
    let _ = write!(out, "{line}\r\n").and_then(|()| out.flush());
}

fn play(argv: &[String], id: &str, url: Option<&str>, ending: Ending) -> i32 {
    let log = Log::open(argv);
    say(&format!("Authenticating with {id}…"));
    thread::sleep(Duration::from_millis(200));
    let code = match url {
        Some(url) => {
            say(&format!(
                "If your browser doesn't open, visit this URL to authenticate:\r\n\r\n  {url}\r\n"
            ));
            say("Waiting for authentication…");
            match ending {
                Ending::SignedIn => {
                    thread::sleep(Duration::from_millis(800));
                    say(&format!("✔ Authenticated with {id}."));
                    0
                }
                Ending::Fails => {
                    thread::sleep(Duration::from_millis(800));
                    say(&format!("✘ Authentication with {id} failed."));
                    1
                }
                Ending::Waits => {
                    thread::sleep(Duration::from_secs(60));
                    0
                }
            }
        }
        None => {
            thread::sleep(Duration::from_millis(500));
            say("Done.");
            0
        }
    };
    if let Some(log) = log {
        log.update(|start| start["exit"] = Value::from(code));
    }
    code
}
