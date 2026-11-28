//! `rlxd here`, the probe rlx.app runs over SSH (the owner's remote brief, §3; task #12's
//! acceptance), each against a fresh home, on every OS rlxd.yml runs. Its answer over a
//! real `ssh localhost` is rlxd.yml's `ssh` job's to check.

mod common;

use std::path::{Path, PathBuf};

use common::{Run, TestHome, assert_valid};
use rlxd_client::Home;
use serde_json::{Value, json};

/// The version every crate of the workspace carries, which is rlx's.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `run`'s `rlxd here --json` envelope, which must be `ok` and valid against both
/// schemas, and its exit code.
fn answer(run: &Run) -> (Option<i32>, Value) {
    let envelope = run.json();
    assert_valid("envelope.json", &envelope);
    assert_eq!(envelope["ok"], true, "{run:?}");
    assert_valid("here-reply.json", &envelope["data"]);
    (run.code(), envelope["data"].clone())
}

/// `rlxd here --json args…` in `home`.
fn here(home: &TestHome, args: &[&str]) -> (Option<i32>, Value) {
    let mut all = vec!["here", "--json"];
    all.extend_from_slice(args);
    answer(&home.rlxd(&all))
}

/// `path`, resolved, so two spellings of one file compare equal.
fn real(path: impl AsRef<Path>) -> PathBuf {
    std::fs::canonicalize(path.as_ref())
        .unwrap_or_else(|e| panic!("{}: {e}", path.as_ref().display()))
}

#[test]
fn stopped_then_started_then_running() {
    // Acceptance 1.
    let home = TestHome::new();
    let endpoint = Home::at(home.root()).endpoint();
    let (code, stopped) = here(&home, &[]);
    assert_eq!(code, Some(3), "installed, not running: {stopped}");
    assert_eq!(stopped["installed"], true);
    assert_eq!(stopped["running"], false);
    assert_eq!(stopped["started"], false);
    assert_eq!(stopped["supported"], true);
    assert_eq!(stopped.get("pid"), None);
    assert_eq!(stopped["version"], VERSION);
    assert_eq!(stopped["protocol_versions"], json!([1]));
    assert_eq!(stopped["socket"], endpoint);
    assert_eq!(stopped["home"], home.root().display().to_string());
    assert_eq!(stopped["platform"], std::env::consts::OS);
    let token = home.root().join("rlxd").join("token");
    assert_eq!(
        stopped["auth"],
        json!({"token_file": token.display().to_string()})
    );
    assert_eq!(stopped["rlxd"]["version"], VERSION);
    assert_eq!(stopped["rlxd"]["protocol_versions"], json!([1]));
    let path = stopped["rlxd"]["path"].as_str().unwrap();
    assert_eq!(real(path), real(env!("CARGO_BIN_EXE_rlxd")));
    assert!(stopped["rlx"]["installed"].is_boolean());

    let (code, started) = here(&home, &["--start"]);
    assert_eq!(code, Some(0), "{started}");
    assert_eq!(started["running"], true);
    assert_eq!(started["started"], true);
    assert!(
        started["pid"].as_u64().is_some_and(|pid| pid > 0),
        "{started}"
    );

    let (code, again) = here(&home, &[]);
    assert_eq!(code, Some(0), "{again}");
    assert_eq!(again["running"], true);
    assert_eq!(again["started"], false);
    assert_eq!(again["pid"], started["pid"]);
    assert_eq!(again["socket"], endpoint);
    assert_eq!(again["protocol_versions"], json!([1]));
    assert_eq!(again["version"], VERSION);
    assert_eq!(
        home.status()["pid"],
        again["pid"],
        "status names the same rlxd"
    );
}

#[test]
fn start_on_a_running_rlxd_starts_nothing() {
    let home = TestHome::new();
    let first = home.start();
    let (code, data) = here(&home, &["--start"]);
    assert_eq!(code, Some(0), "{data}");
    assert_eq!(data["running"], true);
    assert_eq!(data["started"], false);
    assert_eq!(data["pid"], first["pid"]);
}

#[test]
fn a_caller_rlxd_cannot_serve_gets_exit_4_and_nothing_started() {
    let home = TestHome::new();
    let (code, data) = here(&home, &["--protocols", "2-3", "--start"]);
    assert_eq!(code, Some(4), "{data}");
    assert_eq!(data["supported"], false);
    assert_eq!(data["running"], false);
    assert_eq!(data["started"], false);
    assert_eq!(
        home.rlxd(&["status", "--json"]).code(),
        Some(3),
        "not started"
    );

    let pid = home.start()["pid"].clone();
    let (code, data) = here(&home, &["--protocols", "2"]);
    assert_eq!(code, Some(4), "{data}");
    assert_eq!(data["running"], true);
    assert_eq!(data["supported"], false);
    assert_eq!(data["pid"], pid);
    for speaks in ["1", "1-3", "3,1"] {
        let (code, data) = here(&home, &["--protocols", speaks]);
        assert_eq!(code, Some(0), "{speaks}: {data}");
        assert_eq!(data["supported"], true);
    }
}

#[test]
fn the_token_is_shown_only_when_asked() {
    let home = TestHome::new();
    let (_, data) = here(&home, &["--show-token"]);
    assert_eq!(data["auth"].get("token"), None, "no token file yet");
    home.start();
    let (_, data) = here(&home, &[]);
    assert_eq!(data["auth"].get("token"), None);
    let (_, data) = here(&home, &["--show-token"]);
    let token = std::fs::read_to_string(home.root().join("rlxd").join("token")).unwrap();
    assert_eq!(data["auth"]["token"], token.trim());
}

#[test]
fn without_json_it_describes_and_exits_the_same() {
    let home = TestHome::new();
    let stopped = home.rlxd(&["here"]);
    assert_eq!(stopped.code(), Some(3), "{stopped:?}");
    assert!(stopped.stdout.contains("not running"), "{stopped:?}");
    home.start();
    let running = home.rlxd(&["here"]);
    assert_eq!(running.code(), Some(0), "{running:?}");
    assert!(running.stdout.contains("runs for"), "{running:?}");
    let endpoint = Home::at(home.root()).endpoint();
    assert!(running.stdout.contains(&endpoint), "{running:?}");
    let unsupported = home.rlxd(&["here", "--protocols", "2"]);
    assert_eq!(unsupported.code(), Some(4), "{unsupported:?}");
    assert!(
        unsupported.stdout.contains("unsupported"),
        "{unsupported:?}"
    );
}

/// Stops the rlxd of the home `env -i HOME=<dir>` names, `<dir>/.aisquare`, when
/// dropped, for good ([`common::stop_for_good`]).
#[cfg(unix)]
struct StopsWithNoEnvironment<'a>(&'a Path);

#[cfg(unix)]
impl StopsWithNoEnvironment<'_> {
    /// `env -i HOME=<dir> rlxd args…`: no `PATH`, no home variable, nothing else.
    fn command(&self, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_rlxd"));
        command.env_clear().env("HOME", self.0).args(args);
        command
    }

    fn rlxd(&self, args: &[&str]) -> Run {
        common::run(self.command(args), None)
    }
}

#[cfg(unix)]
impl Drop for StopsWithNoEnvironment<'_> {
    fn drop(&mut self) {
        let home = Home::at(self.0.join(".aisquare"));
        common::stop_for_good(self.command(&["stop"]), &home);
    }
}

#[cfg(unix)]
#[test]
fn with_only_home_set_it_finds_the_default_home_and_starts_rlxd() {
    // Acceptance 2: `env -i HOME=$HOME <absolute path> here --json`, with this test's
    // folder as $HOME.
    let user = TestHome::new();
    let session = StopsWithNoEnvironment(user.root());
    let default_home = user.root().join(".aisquare").display().to_string();
    let (code, stopped) = answer(&session.rlxd(&["here", "--json"]));
    assert_eq!(code, Some(3), "{stopped}");
    assert_eq!(stopped["home"], default_home);
    assert_eq!(stopped["running"], false);
    let (code, started) = answer(&session.rlxd(&["here", "--json", "--start"]));
    assert_eq!(code, Some(0), "{started}");
    assert_eq!(started["started"], true);
    let (code, running) = answer(&session.rlxd(&["here", "--json"]));
    assert_eq!(code, Some(0), "{running}");
    assert_eq!(running["running"], true);
    assert_eq!(running["pid"], started["pid"]);
    assert_eq!(running["home"], default_home);
    assert_eq!(session.rlxd(&["stop"]).code(), Some(0));
}

#[cfg(unix)]
#[test]
fn rlx_beside_rlxd_is_found_and_asked_its_version() {
    rlx_beside_rlxd();
}

/// [`rlx_beside_rlxd_is_found_and_asked_its_version`] 50 times, 4 at once, as two more
/// threads start commands without pause, alongside this binary's other tests. Each run
/// copies rlxd and puts its stand-in rlx, then starts them, while other threads start
/// processes. On Linux a program that any process still has open for writing can't start
/// ("Text file busy", ETXTBSY), which failed that test in runs 45843894810 and
/// 45872911004 ([`common::copy_executable`]).
#[cfg(unix)]
#[test]
fn rlx_beside_rlxd_holds_fifty_times_as_other_threads_start_commands() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    const RUNS: usize = 50;
    const AT_ONCE: usize = 4;
    let next = AtomicUsize::new(0);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let starters: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let mut started = 0_u64;
                    while !done.load(Ordering::Relaxed) {
                        let mut command = std::process::Command::new("/bin/sh");
                        command
                            .args(["-c", "exit 0"])
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null());
                        let mut child = common::launch(&mut command).expect("sh starts");
                        child.wait().expect("sh ends");
                        started += 1;
                    }
                    started
                })
            })
            .collect();
        let runners: Vec<_> = (0..AT_ONCE)
            .map(|_| {
                scope.spawn(|| {
                    while next.fetch_add(1, Ordering::Relaxed) < RUNS {
                        rlx_beside_rlxd();
                    }
                })
            })
            .collect();
        // Every runner ends, a failed one too, before the starters are told to stop.
        let ran: Vec<_> = runners.into_iter().map(|runner| runner.join()).collect();
        done.store(true, Ordering::Relaxed);
        let started: u64 = starters
            .into_iter()
            .map(|starter| starter.join().expect("a starter"))
            .sum();
        for outcome in ran {
            if let Err(panic) = outcome {
                std::panic::resume_unwind(panic);
            }
        }
        assert!(started > 0, "no command started alongside the runs");
        eprintln!("{RUNS} runs, {AT_ONCE} at once, alongside {started} other commands");
    });
}

/// rlxd, copied to a folder of a fresh home with no PATH, finds the rlx beside it before
/// the install folders' and asks its version: of a stand-in rlx that says one, one that
/// fails, and one that leaves a process holding its output open.
#[cfg(unix)]
fn rlx_beside_rlxd() {
    let home = TestHome::new();
    let bin = home.root().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    common::copy_executable(Path::new(env!("CARGO_BIN_EXE_rlxd")), &bin.join("rlxd"));
    let rlx = bin.join("rlx");
    // The copy, in this home, with no PATH: only the folder beside it holds an rlx
    // before the install folders do.
    let probe = || {
        let mut command = std::process::Command::new(bin.join("rlxd"));
        command
            .args(["here", "--json"])
            .env("AISQUARE_HOME", home.root())
            .env_remove("RLX_HOME")
            .env_remove("PATH");
        answer(&common::run(command, None)).1
    };
    for (script, version) in [
        ("#!/bin/sh\necho 'rlx 9.9.9'\n", json!("9.9.9")),
        ("#!/bin/sh\nexit 1\n", Value::Null),
        // A process it started keeps its output open for 20 s: the first line is enough.
        ("#!/bin/sh\necho 'rlx 9.9.8'\nsleep 20 &\n", json!("9.9.8")),
    ] {
        let began = std::time::Instant::now();
        common::put_executable(&rlx, script);
        let data = probe();
        assert_eq!(data["rlx"]["installed"], true, "{data}");
        assert_eq!(real(data["rlx"]["path"].as_str().unwrap()), real(&rlx));
        let said = data["rlx"].get("version").cloned().unwrap_or(Value::Null);
        assert_eq!(said, version, "{data}");
        assert!(
            began.elapsed() < std::time::Duration::from_secs(8),
            "{script:?} took {:?}",
            began.elapsed()
        );
        assert_eq!(
            real(data["rlxd"]["path"].as_str().unwrap()),
            real(bin.join("rlxd"))
        );
    }
}

#[cfg(unix)]
#[test]
fn a_running_rlxd_of_other_protocols_is_running_and_unsupported() {
    // A stand-in for a later rlxd that serves protocols 2 and 3 only: it refuses this
    // probe's hello as rlxd refuses one it does not serve.
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    let home = TestHome::new();
    let layout = Home::at(home.root());
    std::fs::create_dir_all(layout.run_dir()).unwrap();
    std::fs::create_dir_all(layout.rlxd_dir()).unwrap();
    std::fs::write(layout.token(), "t").unwrap();
    let listener = UnixListener::bind(layout.socket()).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            let id = serde_json::from_str::<Value>(&line).map_or(json!(1), |r| r["id"].clone());
            let reply = json!({"schema_version": 1, "id": id, "ok": false, "error": {
                "code": "incompatible_protocol", "message": "protocol 1 is not served",
                "detail": {"supported": [3, 2]}}});
            let _ = writeln!(stream, "{reply}");
        }
    });
    let (code, data) = here(&home, &[]);
    assert_eq!(code, Some(4), "{data}");
    assert_eq!(data["running"], true);
    assert_eq!(data["supported"], false);
    assert_eq!(data["protocol_versions"], json!([2, 3]));
    assert_eq!(data.get("pid"), None, "no registry record to read it from");
    assert_eq!(data.get("version"), None);
    assert_eq!(data["rlxd"]["protocol_versions"], json!([1]));
    let (code, data) = here(&home, &["--protocols", "2-3"]);
    assert_eq!(code, Some(0), "{data}");
    let (code, data) = here(&home, &["--start"]);
    assert_eq!(code, Some(4), "{data}");
    assert_eq!(data["started"], false);
}

#[cfg(unix)]
#[test]
fn an_rlxd_that_holds_the_lock_and_never_listens_fails_the_probe() {
    let home = TestHome::new();
    let layout = Home::at(home.root());
    std::fs::create_dir_all(layout.run_dir()).unwrap();
    let lock = std::fs::File::create(layout.lock()).unwrap();
    lock.try_lock().unwrap();
    for args in [&["here", "--json"][..], &["here", "--json", "--start"]] {
        let run = home.rlxd(args);
        assert_eq!(run.code(), Some(1), "{args:?}: {run:?}");
        assert_eq!(run.error_code(), "disconnected", "{args:?}");
        assert_valid("envelope.json", &run.json());
    }
    drop(lock);
}

/// The environment of the process `pid`, as `NAME=value` text: `/proc/<pid>/environ` on
/// Linux, `ps eww` elsewhere.
#[cfg(unix)]
fn environment_of(pid: u64) -> String {
    if let Ok(bytes) = std::fs::read(format!("/proc/{pid}/environ")) {
        return String::from_utf8_lossy(&bytes).replace('\0', " ");
    }
    let mut ps = std::process::Command::new("ps");
    ps.args(["eww", "-p", &pid.to_string()]);
    common::run(ps, None).stdout
}

/// What the process `pid`'s stdin, stdout and stderr are open on: `/proc/<pid>/fd/<n>`
/// on Linux, `lsof` elsewhere.
#[cfg(unix)]
fn stdio_of(pid: u64) -> Vec<String> {
    let fds = PathBuf::from(format!("/proc/{pid}/fd"));
    if fds.is_dir() {
        return ["0", "1", "2"]
            .map(|fd| {
                std::fs::read_link(fds.join(fd))
                    .map_or_else(|e| format!("{fd}: {e}"), |p| p.display().to_string())
            })
            .to_vec();
    }
    let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .into_iter()
        .find(|path| Path::new(path).exists())
        .unwrap_or("lsof");
    let mut command = std::process::Command::new(lsof);
    // One field per line: `p<pid>`, then `f<fd>` and `n<name>` for each descriptor.
    command.args(["-a", "-p", &pid.to_string(), "-d", "0,1,2", "-F", "n"]);
    let run = common::run(command, None);
    run.stdout
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .map(str::to_owned)
        .collect()
}

/// `command`, its process to inherit `fd` as it is: a descriptor passed on by mistake, as
/// a pipe a thread has just made is to a command another thread starts in that moment
/// (on macOS, Rust makes a pipe close-on-exec only after it has made it).
#[cfg(unix)]
#[allow(unsafe_code)]
fn inheriting(mut command: std::process::Command, fd: std::os::fd::RawFd) -> std::process::Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs in the child, between fork and exec, and makes one call,
    // which is async-signal-safe and changes only the child's own descriptor.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

#[cfg(unix)]
#[test]
fn the_rlxd_start_starts_holds_none_of_its_callers_descriptors() {
    // Its caller reads `here`'s output to the end, and may have passed it other pipes by
    // mistake: an rlxd that held one would keep its reader waiting as long as rlxd ran.
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let home = TestHome::new();
    let (mut reader, writer) = common::pipe();
    let command = inheriting(
        home.command(&["here", "--json", "--start"]),
        writer.as_raw_fd(),
    );
    let (code, started) = answer(&common::run(command, None));
    drop(writer);
    assert_eq!(code, Some(0), "{started}");
    assert_eq!(started["started"], true, "{started}");
    let pid = started["pid"].as_u64().unwrap();
    assert_eq!(
        stdio_of(pid),
        ["/dev/null"; 3],
        "rlxd {pid}'s stdin, stdout and stderr"
    );
    // `here` has exited, so with no rlxd holding a copy, the pipe has ended.
    let (read, ended) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = read.send(reader.read_to_end(&mut Vec::new()));
    });
    assert!(
        ended
            .recv_timeout(std::time::Duration::from_secs(10))
            .is_ok(),
        "rlxd {pid} holds the pipe its starter inherited"
    );
}

#[cfg(unix)]
#[test]
fn the_rlxd_start_starts_keeps_no_ssh_session_variable() {
    const SESSION: [&str; 5] = [
        "SSH_AUTH_SOCK",
        "SSH_CONNECTION",
        "SSH_CLIENT",
        "SSH_TTY",
        "SSH_ORIGINAL_COMMAND",
    ];
    let home = TestHome::new();
    let mut command = home.command(&["here", "--json", "--start"]);
    for name in SESSION {
        command.env(name, format!("/session/{name}"));
    }
    // A variable that is kept, to show the environment is really read.
    command.env("RLXD_HERE_TEST_KEPT", "kept");
    let (code, started) = answer(&common::run(command, None));
    assert_eq!(code, Some(0), "{started}");
    let pid = started["pid"].as_u64().unwrap();
    let environment = environment_of(pid);
    // Whole names: `_P9K_SSH_TTY=` is not `SSH_TTY=`.
    let has = |name: &str| {
        let prefix = format!("{name}=");
        environment
            .split_whitespace()
            .any(|w| w.starts_with(&prefix))
    };
    assert!(
        has("RLXD_HERE_TEST_KEPT"),
        "the daemon's environment was not read: {environment}"
    );
    assert!(has("PATH"), "{environment}");
    for name in SESSION {
        assert!(!has(name), "{name} kept: {environment}");
    }
}

/// The command rlx.app sends over SSH, as the README gives it.
#[cfg(unix)]
fn readme_command() -> String {
    let readme = common::workspace().join("README.md");
    let text = std::fs::read_to_string(&readme).unwrap();
    text.lines()
        .find(|line| line.starts_with("sh -c '") && line.ends_with("exit 127'"))
        .unwrap_or_else(|| panic!("no `sh -c '…exit 127'` line in {}", readme.display()))
        .to_owned()
}

#[cfg(unix)]
#[test]
fn the_readme_command_runs_rlxd_else_says_rlx_else_nothing() {
    // Folders every user shares: an rlx or rlxd there would answer instead of this test's.
    let shared = [
        "/opt/homebrew/bin",
        "/home/linuxbrew/.linuxbrew/bin",
        "/usr/local/bin",
    ];
    if let Some(found) = shared
        .iter()
        .flat_map(|dir| ["rlx", "rlxd"].map(|p| Path::new(dir).join(p)))
        .find(|p| p.exists())
    {
        eprintln!(
            "skipped: {} answers the README's command first",
            found.display()
        );
        return;
    }
    let user = TestHome::new();
    let session = StopsWithNoEnvironment(user.root());
    let bin = user.root().join(".local").join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let sent = readme_command();
    // A non-login session: HOME and a system PATH, nothing from a profile.
    let send = || {
        let mut command = std::process::Command::new("/bin/sh");
        command
            .env_clear()
            .env("HOME", user.root())
            .env("PATH", "/usr/bin:/bin")
            .args(["-c", &sent]);
        common::run(command, None)
    };

    let nothing = send();
    assert_eq!(nothing.code(), Some(127), "{nothing:?}");

    let rlx = bin.join("rlx");
    common::put_executable(&rlx, "#!/bin/sh\necho 'rlx 0.10.1'\n");
    let old = send();
    assert_eq!(old.code(), Some(5), "{old:?}");
    assert_eq!(old.stdout.trim(), "rlx 0.10.1");

    common::copy_executable(Path::new(env!("CARGO_BIN_EXE_rlxd")), &bin.join("rlxd"));
    let (code, started) = answer(&send());
    assert_eq!(code, Some(0), "{started}");
    assert_eq!(started["started"], true);
    assert_eq!(
        real(started["rlxd"]["path"].as_str().unwrap()),
        real(bin.join("rlxd"))
    );
    assert_eq!(real(started["rlx"]["path"].as_str().unwrap()), real(&rlx));
    assert_eq!(started["rlx"]["version"], "0.10.1");
    assert_eq!(session.rlxd(&["stop"]).code(), Some(0));
}

#[cfg(unix)]
#[test]
fn a_file_at_the_sockets_path_with_the_lock_free_is_not_running_and_start_replaces_it() {
    let home = TestHome::new();
    let layout = Home::at(home.root());
    std::fs::create_dir_all(layout.run_dir()).unwrap();
    std::fs::write(layout.socket(), "not a socket").unwrap();
    let (code, stopped) = here(&home, &[]);
    assert_eq!(code, Some(3), "{stopped}");
    assert_eq!(stopped["running"], false);
    let (code, started) = here(&home, &["--start"]);
    assert_eq!(code, Some(0), "{started}");
    assert_eq!(started["started"], true);
}

#[cfg(unix)]
#[test]
fn a_home_whose_path_is_not_utf8_fails_rather_than_print_another_path() {
    use std::os::unix::ffi::OsStrExt;
    let home = TestHome::new();
    let mut not_utf8 = home.root().as_os_str().as_bytes().to_vec();
    not_utf8.extend_from_slice(b"/h\xff");
    let mut command = home.command(&["here", "--json"]);
    command.env("AISQUARE_HOME", std::ffi::OsStr::from_bytes(&not_utf8));
    let run = common::run(command, None);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_valid("envelope.json", &run.json());
    let message = run.json()["error"]["message"].as_str().unwrap().to_owned();
    assert!(message.contains("not UTF-8"), "{message}");
}

#[test]
fn a_relative_home_is_answered_with_absolute_paths() {
    let home = TestHome::new();
    let mut command = home.command(&["here", "--json"]);
    command.env("AISQUARE_HOME", "rel").current_dir(home.root());
    let (code, data) = answer(&common::run(command, None));
    assert_eq!(code, Some(3), "{data}");
    for field in ["home", "socket"] {
        let path = data[field].as_str().unwrap();
        assert!(Path::new(path).is_absolute(), "{field}: {path}");
    }
    let token_file = data["auth"]["token_file"].as_str().unwrap();
    assert!(Path::new(token_file).is_absolute(), "{token_file}");
    assert!(
        Path::new(data["home"].as_str().unwrap()).ends_with("rel"),
        "{data}"
    );
}

#[cfg(unix)]
#[test]
fn after_a_start_race_no_losing_rlxd_outlives_a_stop() {
    // Every start that lost has given up by the time its `here --start` returns, so a
    // stop right after leaves no rlxd: none was left waiting to take the home.
    let home = TestHome::new();
    let racers: Vec<_> = (0..4)
        .map(|_| {
            let command = home.command(&["here", "--json", "--start"]);
            std::thread::spawn(move || answer(&common::run(command, None)))
        })
        .collect();
    let answers: Vec<_> = racers.into_iter().map(|r| r.join().unwrap()).collect();
    let started = answers.iter().filter(|(_, d)| d["started"] == true).count();
    assert_eq!(started, 1, "{answers:?}");
    for (code, data) in &answers {
        assert_eq!(*code, Some(0), "{data}");
        assert_eq!(data["pid"], answers[0].1["pid"], "one rlxd");
    }
    assert_eq!(home.rlxd(&["stop"]).code(), Some(0));
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert_eq!(
        home.rlxd(&["status", "--json"]).code(),
        Some(3),
        "a losing rlxd took the home after the stop"
    );
}
