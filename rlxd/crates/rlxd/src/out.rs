//! What rlxd's commands print, and their exit codes (handoff 0.10.3, "How to read the
//! checks" and 7.2): one envelope on stdout, and 0 when it is `ok`, 1 when it is not, 2
//! on a usage error and 3 when rlxd is not running. `rlxd here` adds 4, unsupported.

use std::io::{self, Write};
use std::process::ExitCode;

use rlxd_client::Reply;
use rlxd_proto::{Data, Envelope, ErrorBody};

/// The envelope is `ok: false`.
pub const FAILED: u8 = 1;
/// The command line is wrong.
pub const USAGE: u8 = 2;
/// No rlxd runs for this home.
pub const NOT_RUNNING: u8 = 3;
/// `rlxd here`: rlxd serves none of the protocol versions the caller speaks.
pub const UNSUPPORTED: u8 = 4;

/// Prints `line` and a newline on stdout. False when stdout is closed.
pub fn print_line(line: &str) -> bool {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(line.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .is_ok()
}

/// Prints a reply as it came, exiting 0 when it is `ok` and 1 when it is not.
pub fn print_reply(reply: &Reply) -> ExitCode {
    exit(print_line(&reply.line), reply.is_ok())
}

/// Prints an `ok: true` envelope carrying `data`, exiting 0.
pub fn succeed(data: Data) -> ExitCode {
    print_envelope(&Envelope::ok(data), 0)
}

/// Prints an `ok: true` envelope carrying `data`, exiting `status`.
pub fn succeed_with(status: u8, data: Data) -> ExitCode {
    print_envelope(&Envelope::ok(data), status)
}

/// Prints an `ok: false` envelope, exiting 1.
pub fn fail(code: &str, message: &str, hint: Option<&str>) -> ExitCode {
    print_envelope(&failure(code, message, hint), FAILED)
}

/// Prints an `ok: false` envelope, exiting `status`.
pub fn fail_with(status: u8, code: &str, message: &str, hint: Option<&str>) -> ExitCode {
    print_envelope(&failure(code, message, hint), status)
}

/// An `ok: false` envelope. `code` and `message` are never empty.
pub fn failure(code: &str, message: &str, hint: Option<&str>) -> Envelope {
    let mut error = ErrorBody::new(code, message).expect("a code and a message");
    if let Some(hint) = hint {
        error = error.with_hint(hint);
    }
    Envelope::err(error)
}

/// A usage error: `message` on stderr, exit 2.
pub fn usage(message: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "rlxd: {message}");
    ExitCode::from(USAGE)
}

fn print_envelope(envelope: &Envelope, status: u8) -> ExitCode {
    match serde_json::to_string(envelope) {
        Ok(line) if print_line(&line) => ExitCode::from(status),
        _ => ExitCode::FAILURE,
    }
}

fn exit(printed: bool, ok: bool) -> ExitCode {
    match (printed, ok) {
        (true, true) => ExitCode::SUCCESS,
        _ => ExitCode::from(FAILED),
    }
}
