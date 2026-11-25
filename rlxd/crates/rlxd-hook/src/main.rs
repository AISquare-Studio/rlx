//! rlxd-hook, the tiny client Claude Code runs for each hook. It hands the hook to rlxd
//! over the local socket, so no Python process starts per hook (handoff 0.10.3, 1.5).
//!
//! For now it answers `--version` and `--help` only (1.1).

use std::io::{self, Write};
use std::process::ExitCode;

const USAGE: &str = "usage: rlxd-hook [--version | --help]

Hands a Claude Code hook to rlxd. This build answers --version only; hook
intake arrives with rlx 0.10.3.
";

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [flag] if flag == "--version" || flag == "-V" => {
            print(&format!("rlxd-hook {}\n", env!("CARGO_PKG_VERSION")))
        }
        [flag] if flag == "--help" || flag == "-h" => print(USAGE),
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Prints `text` on stdout, and fails rather than panics when stdout is closed.
fn print(text: &str) -> ExitCode {
    let mut stdout = io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
