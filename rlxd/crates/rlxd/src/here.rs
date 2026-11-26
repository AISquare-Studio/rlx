//! `rlxd here [--json] [--start] [--show-token] [--protocols VERSIONS]`: what rlx is
//! installed on this machine, whether its rlxd runs for the home, which protocol versions
//! it serves, where clients connect and how they get in. rlx.app runs it over SSH, by its
//! absolute path, before it connects (the owner's remote brief, §3). `--start` starts
//! rlxd when it is installed but not running.
//!
//! Its answer is one envelope whose `data` is [`HereData`] (`schemas/v1/here-reply.json`),
//! and its exit code says which case this is:
//!
//! - **0:** rlxd runs, and serves a protocol version the caller speaks.
//! - **3:** rlxd is installed but not running. The envelope is `ok`, with `running: false`.
//! - **4:** unsupported: rlxd serves none of the caller's protocol versions
//!   (`--protocols`, else this rlxd's). The envelope is `ok`, with `supported: false`.
//!   `--start` does not start an rlxd the caller could not speak to.
//! - **1:** the probe failed, such as an rlxd that holds the home's lock but does not
//!   answer, or a start that failed. The envelope is `ok: false`.
//! - **2:** a usage error.
//!
//! Not installed is the caller's to see: the shell cannot find the command (127).
//!
//! A non-login SSH session has not read the user's profile, so its `PATH` may lack the
//! folders rlx and Claude Code are installed in. The rlxd `--start` starts gets them on
//! its `PATH`, after the session's own: this rlxd's folder, [`install::dirs`], and the
//! system's.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rlxd_client::{ConnectError, Connection, Home, install};
use rlxd_proto::{
    Auth, Data, Detail, HelloParams, HereData, InstalledRlx, InstalledRlxd, PID_MAX,
    PROTOCOL_VERSIONS, Role, VERSION, codes, daemon_kinds, methods,
};
use serde_json::Value;

use crate::serve::{self, StartError};
use crate::{lock, out, registry};

/// The `client` the probe's `hello` names.
const CLIENT: &str = "rlxd-here";

/// How long the probe waits for a running rlxd to answer, and for one that holds the
/// home's lock to take connections.
const ANSWER_WAIT: Duration = Duration::from_secs(5);

/// How long `rlx --version` may take.
const RLX_WAIT: Duration = Duration::from_secs(10);

/// The folders every system has, last on the `PATH` of an rlxd `--start` starts.
const SYSTEM_DIRS: &[&str] = if cfg!(windows) {
    &[]
} else {
    &["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
};

/// `rlxd here`'s options, as the command line gave them.
#[derive(Debug, Default)]
pub struct HereArgs {
    /// Print the envelope, not a description.
    pub json: bool,
    /// Start rlxd when it is installed but not running.
    pub start: bool,
    /// Put the client token in `auth.token`.
    pub show_token: bool,
    /// The protocol versions the caller speaks, as written: `1`, `1-3` or `1,2`.
    pub protocols: Option<String>,
}

/// What the probe could not do: an error code and a message.
struct Failure {
    code: String,
    message: String,
    hint: Option<String>,
}

impl Failure {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            hint: None,
        }
    }
}

/// What a running rlxd said of itself. Each field is `None` when it would not say.
#[derive(Debug, Default)]
struct Running {
    version: Option<String>,
    protocol_versions: Vec<u32>,
    pid: Option<u32>,
    socket: Option<String>,
}

/// What one connection found.
enum Asked {
    /// An rlxd answered.
    Running(Running),
    /// Nothing listens: no socket, or one nobody serves.
    Absent,
    /// Something answered, or failed, in a way the probe cannot report as an answer.
    Failed(Failure),
}

/// `rlxd here`.
pub fn here(args: HereArgs) -> ExitCode {
    let accepted = match args.protocols.as_deref().map(parse_versions) {
        None => PROTOCOL_VERSIONS.to_vec(),
        Some(Ok(versions)) => versions,
        Some(Err(message)) => return out::usage(&message),
    };
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => return out::fail(codes::INTERNAL, &e.to_string(), None),
    };
    let exe = env::current_exe().ok();
    let beside = exe.as_deref().and_then(Path::parent).map(Path::to_path_buf);
    let rlxd_path = exe.as_deref().map_or_else(|| "rlxd".to_owned(), shown);
    let here = HereData::new(
        InstalledRlxd::new(VERSION, rlxd_path),
        find_rlx(beside.as_deref()),
        shown(home.root()),
        home.endpoint(),
        env::consts::OS,
        Auth::new(shown(&home.token())),
    );
    let mut here = match here {
        Ok(here) => here,
        Err(e) => {
            return out::fail(
                codes::INTERNAL,
                &format!("the answer is not valid: {e}"),
                None,
            );
        }
    };
    if let Err(failure) = answer(&home, &args, &accepted, beside.as_deref(), &mut here) {
        return out::fail(&failure.code, &failure.message, failure.hint.as_deref());
    }
    let status = if !here.supported {
        out::UNSUPPORTED
    } else if here.running {
        0
    } else {
        out::NOT_RUNNING
    };
    let data = match serde_json::to_value(&here) {
        Ok(Value::Object(data)) => data,
        Ok(_) | Err(_) => {
            return out::fail(
                codes::INTERNAL,
                "the answer did not print as an object",
                None,
            );
        }
    };
    if args.json {
        out::succeed_with(status, data)
    } else {
        describe(&here, &accepted, status)
    }
}

/// Fills `here` in: probes, starts rlxd when asked to and it may, and probes again.
fn answer(
    home: &Home,
    args: &HereArgs,
    accepted: &[u32],
    beside: Option<&Path>,
    here: &mut HereData,
) -> Result<(), Failure> {
    if let Some(running) = probe(home)? {
        fill(here, running);
    }
    here.supported = shares(accepted, &here.protocol_versions);
    if args.start && !here.running && here.supported {
        match serve::start(home, daemon_path(beside).as_deref()) {
            Ok(_) => here.started = true,
            Err(StartError::AlreadyRunning) => {}
            Err(StartError::Failed { message, hint }) => {
                return Err(Failure {
                    hint,
                    ..Failure::new(codes::START_FAILED, message)
                });
            }
        }
        let Some(running) = probe(home)? else {
            let message = "rlxd started, then stopped again";
            let hint = format!("Its log is {}.", home.log_file().display());
            return Err(Failure {
                hint: Some(hint),
                ..Failure::new(codes::START_FAILED, message)
            });
        };
        fill(here, running);
        here.supported = shares(accepted, &here.protocol_versions);
    }
    if args.show_token {
        here.auth.token = fs::read_to_string(home.token())
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty());
    }
    Ok(())
}

/// What the running rlxd said, put into `here`.
fn fill(here: &mut HereData, running: Running) {
    here.running = true;
    here.version = running.version;
    if !running.protocol_versions.is_empty() {
        here.protocol_versions = running.protocol_versions;
    }
    here.pid = running.pid;
    if let Some(socket) = running.socket {
        here.socket = socket;
    }
}

/// Whether rlxd runs for `home`, and what it says of itself. An rlxd that holds the lock
/// but takes no connections yet is starting or stopping: asked again until
/// [`ANSWER_WAIT`] has passed.
fn probe(home: &Home) -> Result<Option<Running>, Failure> {
    let deadline = Instant::now() + ANSWER_WAIT;
    loop {
        match ask(home) {
            Asked::Running(running) => return Ok(Some(running)),
            Asked::Failed(failure) => return Err(failure),
            Asked::Absent => match lock::is_held(&home.lock()) {
                Ok(false) => return Ok(None),
                Ok(true) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Ok(true) => {
                    let message = format!(
                        "rlxd holds {} but takes no connections on {}",
                        home.lock().display(),
                        home.endpoint()
                    );
                    return Err(Failure {
                        hint: Some(format!("Its log is {}.", home.log_file().display())),
                        ..Failure::new(codes::DISCONNECTED, message)
                    });
                }
                Err(e) => {
                    let message = format!("cannot read {}: {e}", home.lock().display());
                    return Err(Failure::new(codes::INTERNAL, message));
                }
            },
        }
    }
}

/// [`ask_now`], given at most [`ANSWER_WAIT`]: a connection rlxd takes but never answers
/// must not hang the probe.
fn ask(home: &Home) -> Asked {
    let (answered, answer) = mpsc::channel();
    let asked = home.clone();
    thread::spawn(move || {
        let _ = answered.send(ask_now(&asked));
    });
    answer.recv_timeout(ANSWER_WAIT).unwrap_or_else(|_| {
        let message = format!(
            "rlxd took a connection on {} but did not answer within {} s",
            home.endpoint(),
            ANSWER_WAIT.as_secs()
        );
        Asked::Failed(Failure::new(codes::DISCONNECTED, message))
    })
}

/// Says `hello` to `home`'s rlxd as a command-line client, then asks its `status`.
fn ask_now(home: &Home) -> Asked {
    let mut connection = match Connection::open(home) {
        Ok(connection) => connection,
        Err(ConnectError::NotRunning { .. }) => return Asked::Absent,
        Err(e) => return Asked::Failed(Failure::new(codes::DISCONNECTED, e.to_string())),
    };
    let token = match fs::read_to_string(home.token()) {
        Ok(token) => token.trim().to_owned(),
        Err(e) => {
            let message = format!("cannot read the token in {}: {e}", home.token().display());
            return Asked::Failed(Failure::new(codes::UNAUTHORIZED, message));
        }
    };
    let hello = match HelloParams::new(token, CLIENT, VERSION, Role::Cli) {
        Ok(hello) => hello,
        Err(e) => {
            let message = format!("the token in {} is not valid: {e}", home.token().display());
            return Asked::Failed(Failure::new(codes::UNAUTHORIZED, message));
        }
    };
    let reply = match connection.hello(&hello) {
        Ok(reply) => reply,
        Err(e) => return Asked::Failed(Failure::new(codes::DISCONNECTED, e.to_string())),
    };
    if let Some(error) = reply.error() {
        if error.code == codes::INCOMPATIBLE_PROTOCOL {
            return Asked::Running(incompatible(home, error.detail.as_ref()));
        }
        return Asked::Failed(Failure {
            hint: error.hint.clone(),
            ..Failure::new(&error.code, error.message.clone())
        });
    }
    let said = reply.data().cloned().unwrap_or_default();
    let mut running = Running {
        version: text(&said, "rlxd_version"),
        protocol_versions: versions_in(said.get("protocol_versions")),
        ..Running::default()
    };
    if let Ok(status) = connection.request(methods::STATUS, Data::new())
        && let Some(status) = status.data()
    {
        running.pid = status
            .get("pid")
            .and_then(Value::as_u64)
            .filter(|pid| (1..=u64::from(PID_MAX)).contains(pid))
            .and_then(|pid| u32::try_from(pid).ok());
        running.socket = status
            .get("paths")
            .and_then(|paths| paths.as_object())
            .and_then(|paths| text(paths, "socket"));
        running.version = text(status, "version").or(running.version);
    }
    Asked::Running(running)
}

/// A running rlxd that serves none of this rlxd's protocol versions, as its refusal and
/// its record in the daemon registry describe it.
fn incompatible(home: &Home, detail: Option<&Detail>) -> Running {
    let supported = match detail {
        Some(Detail::Object(fields)) => versions_in(fields.get("supported")),
        _ => Vec::new(),
    };
    let record = registry::read(home)
        .ok()
        .and_then(|roll| roll.live.into_iter().find(|r| r.kind == daemon_kinds::RLXD));
    Running {
        version: record
            .as_ref()
            .map(|r| r.version.clone())
            .filter(|v| !v.is_empty()),
        protocol_versions: supported,
        pid: record.map(|r| r.pid),
        socket: None,
    }
}

/// The non-empty string at `key`.
fn text(object: &Data, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// The protocol versions in a JSON array, ascending, each once, 0 and non-numbers left
/// out.
fn versions_in(value: Option<&Value>) -> Vec<u32> {
    let mut versions: Vec<u32> = value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .filter_map(|v| u32::try_from(v).ok())
        .filter(|v| *v >= 1)
        .collect();
    versions.sort_unstable();
    versions.dedup();
    versions
}

/// Whether the two lists share a version.
fn shares(accepted: &[u32], served: &[u32]) -> bool {
    accepted.iter().any(|v| served.contains(v))
}

/// `--protocols`: versions such as `1`, `1-3` or `1,2,4`, each at least 1.
fn parse_versions(text: &str) -> Result<Vec<u32>, String> {
    let bad = || format!("--protocols {text:?} is not a list of versions, such as 1, 1-3 or 1,2");
    let number = |part: &str| part.trim().parse::<u32>().ok().filter(|v| *v >= 1);
    let mut versions = Vec::new();
    for part in text.split(',') {
        match part.split_once('-') {
            Some((low, high)) => {
                let (low, high) = number(low).zip(number(high)).ok_or_else(bad)?;
                if low > high || high - low > 1000 {
                    return Err(bad());
                }
                versions.extend(low..=high);
            }
            None => versions.push(number(part).ok_or_else(bad)?),
        }
    }
    versions.sort_unstable();
    versions.dedup();
    Ok(versions)
}

/// rlx's command: beside this rlxd first, where `rlx daemon install` puts the pair, then
/// in the install folders, then on `PATH`.
fn find_rlx(beside: Option<&Path>) -> InstalledRlx {
    let mut dirs: Vec<PathBuf> = beside.into_iter().map(Path::to_path_buf).collect();
    dirs.extend(install::dirs());
    dirs.extend(path_dirs());
    match install::find_in("rlx", &dirs) {
        Some(path) => InstalledRlx::found(shown(&path), rlx_version(&path)),
        None => InstalledRlx::missing(),
    }
}

/// What `<rlx> --version` says, `rlx 0.10.3`, as its last word.
fn rlx_version(rlx: &Path) -> Option<String> {
    let mut child = Command::new(rlx)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + RLX_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(Some(_)) => return None,
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let said = reader.join().ok()?;
    let word = said.lines().next()?.split_whitespace().last()?;
    Some(word.to_owned())
}

/// The `PATH` for the rlxd `--start` starts: this session's, then this rlxd's folder,
/// the install folders and the system's, each once, and only those that exist. `None`
/// when it cannot be written as a `PATH`, and the session's is kept.
fn daemon_path(beside: Option<&Path>) -> Option<OsString> {
    let mut dirs = path_dirs();
    let extra = beside
        .map(Path::to_path_buf)
        .into_iter()
        .chain(install::dirs())
        .chain(SYSTEM_DIRS.iter().map(PathBuf::from));
    for dir in extra {
        if dir.is_dir() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    env::join_paths(dirs).ok()
}

/// The absolute folders on this session's `PATH`.
fn path_dirs() -> Vec<PathBuf> {
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|dir| dir.is_absolute())
        .collect()
}

fn shown(path: &Path) -> String {
    path.display().to_string()
}

/// The answer, written for a person, exiting `status`.
fn describe(here: &HereData, accepted: &[u32], status: u8) -> ExitCode {
    let list = |versions: &[u32]| {
        versions
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let version = here.version.as_deref().unwrap_or("of an unknown version");
    let mut lines = vec![if here.running {
        let pid = here
            .pid
            .map(|pid| format!(", pid {pid}"))
            .unwrap_or_default();
        let started = if here.started { ", started now" } else { "" };
        format!("rlxd {version} runs for {}{pid}{started}", here.home)
    } else {
        format!(
            "rlxd {version} is installed, and not running for {}. `rlxd here --start` starts it.",
            here.home
        )
    }];
    lines.push(format!(
        "protocol versions: {}",
        list(&here.protocol_versions)
    ));
    if !here.supported {
        lines.push(format!(
            "unsupported: the caller speaks {}. Upgrade rlx with `rlx upgrade`.",
            list(accepted)
        ));
    }
    lines.push(format!("socket: {}", here.socket));
    lines.push(format!("token: {}", here.auth.token_file));
    lines.push(format!("rlxd: {} ({})", here.rlxd.path, here.rlxd.version));
    lines.push(match (&here.rlx.path, &here.rlx.version) {
        (Some(path), Some(version)) => format!("rlx: {path} ({version})"),
        (Some(path), None) => format!("rlx: {path} (it did not say its version)"),
        (None, _) => "rlx: not found".to_owned(),
    });
    if out::print_line(&lines.join("\n")) {
        ExitCode::from(status)
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn protocols_are_a_version_a_range_or_a_list() {
        assert_eq!(parse_versions("1"), Ok(vec![1]));
        assert_eq!(parse_versions("1-3"), Ok(vec![1, 2, 3]));
        assert_eq!(parse_versions("4,1-2, 2"), Ok(vec![1, 2, 4]));
        for bad in ["", "0", "x", "3-1", "1-", "-2", "1,,2", "1-99999", "-1"] {
            assert!(parse_versions(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn versions_are_read_ascending_once_and_without_what_is_not_one() {
        let value = json!([3, 1, 3, 0, "2", -1, 2.5, 4_294_967_296_u64, 2]);
        assert_eq!(versions_in(Some(&value)), vec![1, 2, 3]);
        assert_eq!(versions_in(Some(&json!("1"))), Vec::<u32>::new());
        assert_eq!(versions_in(None), Vec::<u32>::new());
    }

    #[test]
    fn supported_means_one_version_in_common() {
        assert!(shares(&[1, 2], &[2, 3]));
        assert!(!shares(&[1], &[2, 3]));
        assert!(!shares(&[], &[1]));
    }

    #[test]
    fn the_daemons_path_keeps_the_sessions_first_and_adds_what_exists() {
        let path = daemon_path(None).expect("a PATH");
        let dirs: Vec<PathBuf> = env::split_paths(&path).collect();
        let session = path_dirs();
        assert_eq!(dirs[..session.len()], session[..]);
        for (n, dir) in dirs.iter().enumerate().skip(session.len()) {
            assert!(dir.is_dir(), "{}", dir.display());
            assert!(!dirs[..n].contains(dir), "{} twice", dir.display());
        }
    }
}
