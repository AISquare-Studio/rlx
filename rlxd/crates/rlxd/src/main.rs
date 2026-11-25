//! rlxd, the rlx engine: one local daemon per home that hosts processes and pushes events
//! to its clients (handoff 0.10.3, section 1).
//!
//! For now it answers `--version` and `--help` only (1.1); `serve`, `status`, `stop` and
//! the rest arrive with 1.3.

use std::io::{self, Write};
use std::process::ExitCode;

const USAGE: &str = "usage: rlxd [--version | --help]

The rlx engine. This build answers --version only; the daemon arrives with
rlx 0.10.3.
";

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [flag] if flag == "--version" || flag == "-V" => {
            print(&format!("rlxd {}\n", env!("CARGO_PKG_VERSION")))
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
