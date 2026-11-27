//! What the daemon's tests share: a fresh home each, the `rlxd` binary run in it with a
//! time limit, and the checked-in schemas.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use rlxd_client::Home;
use rlxd_proto::daemon_kinds;
use serde_json::Value;

/// How long one `rlxd` command may take before the test fails.
const COMMAND_LIMIT: Duration = Duration::from_secs(60);

/// How long a command's stdout and stderr may stay open after it exits. Only a process it
/// started and left running can hold them, and none may: one that did would keep the test
/// waiting for as long as it ran.
const OUTPUT_LIMIT: Duration = Duration::from_secs(20);

/// Held while a test starts a command. To start one, Rust makes its pipes, and on macOS
/// marks them close-on-exec only after it has made them: a command another thread starts
/// in between inherits them, and the rlxd it starts with them, holding the first
/// command's output open. Starting one command at a time, none inherits another's pipes.
static SPAWNING: Mutex<()> = Mutex::new(());

/// Starts `command`, as no other thread of the test starts one ([`SPAWNING`]).
pub fn launch(command: &mut Command) -> std::io::Result<Child> {
    let _alone = SPAWNING.lock().unwrap_or_else(PoisonError::into_inner);
    command.spawn()
}

/// A new pipe, made as no command starts ([`SPAWNING`]), so none inherits it.
pub fn pipe() -> (std::io::PipeReader, std::io::PipeWriter) {
    let _alone = SPAWNING.lock().unwrap_or_else(PoisonError::into_inner);
    std::io::pipe().expect("a pipe")
}

/// A fresh home, under the system's temporary folder with a short name so the socket's
/// path fits the OS's limit. Dropping it stops its rlxd and removes it.
pub struct TestHome {
    root: PathBuf,
    claude: Option<PathBuf>,
    rlx: Option<PathBuf>,
    envs: Vec<(String, String)>,
    binary: Option<PathBuf>,
}

/// What one `rlxd` command did.
#[derive(Debug)]
pub struct Run {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }

    /// stdout, one JSON document.
    pub fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {self:?}"))
    }

    /// The error code of the envelope on stdout.
    pub fn error_code(&self) -> String {
        let envelope = self.json();
        assert_eq!(envelope["ok"], false, "{self:?}");
        envelope["error"]["code"]
            .as_str()
            .unwrap_or_else(|| panic!("no error code: {self:?}"))
            .to_owned()
    }

    /// The `data` of the `ok: true` envelope on stdout.
    pub fn data(&self) -> Value {
        let envelope = self.json();
        assert_eq!(envelope["ok"], true, "{self:?}");
        envelope["data"].clone()
    }

    /// stdout, one JSON document per line: `rlxd events`' events.
    pub fn lines(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("a line is not JSON ({e}): {line:?} in {self:?}"))
            })
            .collect()
    }

    /// The error code of the envelope on stderr, `rlxd events`' one line of JSON.
    pub fn stderr_code(&self) -> String {
        let envelope: Value = serde_json::from_str(self.stderr.trim())
            .unwrap_or_else(|e| panic!("stderr is not one JSON document ({e}): {self:?}"));
        assert_eq!(envelope["ok"], false, "{self:?}");
        envelope["error"]["code"].as_str().unwrap().to_owned()
    }
}

static NEXT: AtomicU32 = AtomicU32::new(0);

impl TestHome {
    /// A new, empty home. Its rlxd finds no Claude Code unless [`TestHome::with_claude`]
    /// names one, and no rlx to run as its brain unless [`TestHome::with_rlx`] does.
    pub fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("rlxd-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self {
            root,
            claude: None,
            rlx: None,
            envs: Vec::new(),
            binary: None,
        }
    }

    /// This home, its rlxd running `program` as `rlx`: its brain is `program brain --stdio`.
    pub fn with_rlx(mut self, program: PathBuf) -> Self {
        self.rlx = Some(program);
        self
    }

    /// This home, every `rlxd` in it run with `name` set to `value`.
    pub fn with_env(mut self, name: &str, value: &str) -> Self {
        self.envs.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Sets `name` to `value` for every `rlxd` run in this home from now on, replacing
    /// what it was: for a restart with another environment.
    pub fn set_env(&mut self, name: &str, value: &str) {
        self.envs.retain(|(each, _)| each != name);
        self.envs.push((name.to_owned(), value.to_owned()));
    }

    /// This home, run by the rlxd at `binary` rather than this test build's.
    pub fn with_binary(mut self, binary: PathBuf) -> Self {
        self.binary = Some(binary);
        self
    }

    /// This home, its rlxd running `program` as Claude Code.
    pub fn with_claude(mut self, program: PathBuf) -> Self {
        self.claude = Some(program);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `rlxd args…` in this home.
    pub fn rlxd(&self, args: &[&str]) -> Run {
        self.rlxd_with_stdin(args, None)
    }

    /// `program args…` in this home, run to its end.
    pub fn run_of(&self, program: &Path, args: &[&str]) -> Run {
        let mut command = self.command_of(program, args);
        command.stdin(Stdio::null());
        run(command, None)
    }

    /// `rlxd args…` in this home, started and left running, with its stdout and stderr
    /// piped: [`finish`] waits for it.
    pub fn spawn(&self, args: &[&str]) -> Child {
        let mut command = self.command(args);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        launch(&mut command).expect("rlxd starts")
    }

    /// `rlxd call debug.emit {count}`, which must succeed: the seqs of the first and the
    /// last event it published.
    pub fn emit(&self, count: u64) -> (u64, u64) {
        let params = format!(r#"{{"count":{count}}}"#);
        let run = self.rlxd(&["call", "debug.emit", &params]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        let data = run.data();
        (
            data["first"].as_u64().unwrap(),
            data["last"].as_u64().unwrap(),
        )
    }

    /// `program args…` in this home, from `cwd`, with `stdin` on its stdin and `envs` set:
    /// stdout as bytes, for a hook's, which must match byte for byte.
    pub fn run_bytes(
        &self,
        program: &Path,
        args: &[&str],
        cwd: &Path,
        stdin: &[u8],
        envs: &[(&str, &str)],
    ) -> (Option<i32>, Vec<u8>, String) {
        let mut command = self.command_of(program, args);
        command
            .current_dir(cwd)
            .envs(envs.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = launch(&mut command).expect("the program starts");
        let mut input = child.stdin.take().unwrap();
        let bytes = stdin.to_vec();
        let writer = thread::spawn(move || {
            let _ = input.write_all(&bytes);
        });
        let output = wait_output(child);
        let _ = writer.join();
        (
            output.status.code(),
            output.stdout,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// `rlxd args…` in this home, with `stdin` on its stdin.
    pub fn rlxd_with_stdin(&self, args: &[&str], stdin: Option<&str>) -> Run {
        let mut command = self.command(args);
        command.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        run(command, stdin)
    }

    /// The `rlxd` command, in this home: `AISQUARE_HOME` names it and `RLX_HOME` is
    /// unset, so neither a developer's home nor CI's is touched, and no agent's variables
    /// are passed on.
    pub fn command(&self, args: &[&str]) -> Command {
        let binary = self
            .binary
            .clone()
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_rlxd")));
        self.command_of(&binary, args)
    }

    /// `program args…` in this home, as [`TestHome::command`] runs `rlxd`: for a copy of
    /// rlxd, such as a signed helper, or a wrapper that runs it.
    pub fn command_of(&self, program: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(program);
        // Not the identity of an agent the tests run under (its role, fleet agent, config
        // dir, tmux pane): rlx's hooks would act on it.
        for (name, _) in std::env::vars_os() {
            let name = name.to_string_lossy();
            if name.starts_with("AISQUARE_")
                || name.starts_with("CLAUDE")
                || name.starts_with("CODEX_")
                || name.starts_with("TMUX")
            {
                command.env_remove(name.as_ref());
            }
        }
        command
            .args(args)
            .envs(self.envs.iter().map(|(k, v)| (k, v)))
            .env("AISQUARE_HOME", &self.root)
            .env_remove("RLX_HOME")
            .env(
                "RLXD_CLAUDE_BIN",
                self.claude
                    .clone()
                    .unwrap_or_else(|| self.root.join("no-claude")),
            )
            .env(
                "RLXD_RLX_BIN",
                self.rlx.clone().unwrap_or_else(|| self.root.join("no-rlx")),
            );
        command
    }

    /// `rlxd serve --detach`, which must succeed.
    pub fn start(&self) -> Value {
        self.start_with(&[])
    }

    /// `rlxd serve --detach --gate-dev-mode`, which must succeed.
    pub fn start_dev(&self) -> Value {
        self.start_with(&["--gate-dev-mode"])
    }

    /// `rlxd serve --detach flags…`, which must succeed.
    pub fn start_with(&self, flags: &[&str]) -> Value {
        let mut args = vec!["serve", "--detach"];
        args.extend_from_slice(flags);
        let run = self.rlxd(&args);
        assert_eq!(run.code(), Some(0), "{run:?}");
        run.data()
    }

    /// `rlxd stop`, which must succeed.
    pub fn stop(&self) {
        let run = self.rlxd(&["stop"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
    }

    /// rlxd's log, as it is now.
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("rlxd").join("logs").join("rlxd.log"))
            .unwrap_or_default()
    }

    /// Writes a gate.json holding one device, "planted", of `route`, whose credential is
    /// `credential`: as any process of the user's could.
    pub fn plant(&self, route: &str, credential: &str) {
        use sha2::{Digest, Sha256};
        let sha256: String = Sha256::digest(credential.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let gate = serde_json::json!({"version": 1, "devices": [{
            "device": "planted", "route": route, "sha256": sha256,
            "created": "2026-11-26T00:00:00.000Z",
        }]});
        std::fs::create_dir_all(self.root.join("rlxd")).unwrap();
        std::fs::write(self.gate_json(), gate.to_string()).unwrap();
    }

    /// `<home>/rlxd/gate.json`.
    pub fn gate_json(&self) -> PathBuf {
        self.root.join("rlxd").join("gate.json")
    }

    /// The test client's kept credential, `<home>/test-gate-credential`.
    pub fn test_credential(&self) -> Option<String> {
        std::fs::read_to_string(self.root.join("test-gate-credential"))
            .ok()
            .map(|text| text.trim().to_owned())
    }

    /// The shared client token.
    pub fn token(&self) -> String {
        std::fs::read_to_string(self.root.join("rlxd").join("token"))
            .unwrap()
            .trim()
            .to_owned()
    }

    /// `rlxd call status`'s `clients.app`.
    pub fn app_clients(&self) -> u64 {
        self.status()["clients"]["app"].as_u64().unwrap()
    }

    /// `rlxd call status`'s data, which must be `ok`.
    pub fn status(&self) -> Value {
        let run = self.rlxd(&["call", "status"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        run.data()
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        stop_for_good(self.command(&["stop"]), &Home::at(&self.root));
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::remove_dir_all(&self.root).is_err() && self.root.exists() {
            if Instant::now() > deadline {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Runs `command` to its end, failing the test if it takes longer than
/// [`COMMAND_LIMIT`], or its output stays open [`OUTPUT_LIMIT`] after it ends.
pub fn run(command: Command, stdin: Option<&str>) -> Run {
    run_or_say(command, stdin).unwrap_or_else(|failure| panic!("{failure}"))
}

/// [`run`], what goes wrong said rather than failing the test: for a drop guard, which
/// must not panic.
fn run_or_say(mut command: Command, stdin: Option<&str>) -> Result<Run, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = launch(&mut command).map_err(|e| format!("{command:?} did not start: {e}"))?;
    if let Some(text) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(text.as_bytes())
            .map_err(|e| format!("cannot write to {command:?}'s stdin: {e}"))?;
    }
    end(child)
}

/// Waits for `child`, started with its stdout and stderr piped, to end, failing the test if
/// it takes longer than [`COMMAND_LIMIT`], or its output stays open [`OUTPUT_LIMIT`]
/// after it ends.
pub fn finish(child: Child) -> Run {
    end(child).unwrap_or_else(|failure| panic!("{failure}"))
}

/// [`finish`], for a command that may run up to `limit`, such as an `rlxd events
/// --follow` with a `--timeout` past [`COMMAND_LIMIT`].
#[allow(dead_code)]
pub fn finish_within(child: Child, limit: Duration) -> Run {
    end_within(child, limit).unwrap_or_else(|failure| panic!("{failure}"))
}

fn end(child: Child) -> Result<Run, String> {
    end_within(child, COMMAND_LIMIT)
}

fn end_within(mut child: Child, limit: Duration) -> Result<Run, String> {
    let stdout = read_all(child.stdout.take().unwrap());
    let stderr = read_all(child.stderr.take().unwrap());
    let status = wait(&mut child, limit)?;
    let deadline = Instant::now() + OUTPUT_LIMIT;
    let rest = |output: mpsc::Receiver<String>, name: &str| {
        output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| {
                format!(
                    "the command exited ({status}), but its {name} was still open {} s \
                     later: a process it started holds it",
                    OUTPUT_LIMIT.as_secs()
                )
            })
    };
    Ok(Run {
        status,
        stdout: rest(stdout, "stdout")?,
        stderr: rest(stderr, "stderr")?,
    })
}

/// `child`'s end and all its output, failing the test past [`COMMAND_LIMIT`].
fn wait_output(child: Child) -> std::process::Output {
    let (done, output) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    output
        .recv_timeout(COMMAND_LIMIT)
        .expect("the program ended in time")
        .expect("its output was read")
}

/// All of `pipe`, once every process holding it open has let it go.
fn read_all(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (read, text) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let _ = pipe.read_to_string(&mut text);
        let _ = read.send(text);
    });
    text
}

fn wait(child: &mut Child, limit: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() <= deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("rlxd ran longer than {} s", limit.as_secs()));
            }
            Err(e) => return Err(format!("cannot wait for rlxd: {e}")),
        }
    }
}

/// Stops the rlxd of `home` with `stop`, an `rlxd stop` in it, then kills hard any rlxd
/// still running for it: one a stop could not stop still holds its registry record's
/// lock. So a test's rlxd never outlives the test, however the test ended. For drop
/// guards: it fails no test, and prints what went wrong.
pub fn stop_for_good(stop: Command, home: &Home) {
    if let Err(failure) = run_or_say(stop, None) {
        eprintln!("rlxd stop for {}: {failure}", home.root().display());
    }
    let live = rlxd::registry::read(home)
        .map(|roll| roll.live)
        .unwrap_or_default();
    for record in live.iter().filter(|r| r.kind == daemon_kinds::RLXD) {
        let killed = kill(u64::from(record.pid));
        eprintln!(
            "rlxd {} still ran for {} after a stop: killed: {killed}",
            record.pid,
            home.root().display()
        );
    }
}

/// Waits up to 10 s for `done`.
pub fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        thread::sleep(Duration::from_millis(25));
    }
}

/// `rlxd/`, the workspace, without `..` in the path: cmd.exe runs a batch file named
/// by such a path badly.
pub fn workspace() -> PathBuf {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

/// `rlxd/schemas/v1/<name>`.
pub fn schema(name: &str) -> jsonschema::Validator {
    let path = workspace().join("schemas").join("v1").join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let schema: Value = serde_json::from_str(&text).unwrap();
    jsonschema::options()
        .should_validate_formats(true)
        .build(&schema)
        .unwrap()
}

/// Fails unless `instance` is valid against the schema `name`.
pub fn assert_valid(name: &str, instance: &Value) {
    let validator = schema(name);
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{name}: {errors:?}\n{instance}");
}

/// `rlxd/tests/fixtures/fake-brain/<scenario>`, as this OS runs it: the batch file on
/// Windows.
pub fn fake_brain(scenario: &str) -> PathBuf {
    let name = if cfg!(windows) {
        format!("{scenario}.cmd")
    } else {
        scenario.to_owned()
    };
    workspace()
        .join("tests")
        .join("fixtures")
        .join("fake-brain")
        .join(name)
}

/// The checkout's own `rlx`, for a real brain: `RLXD_RLX_BIN` when the test runs with it
/// set, else the `rlx` that `uv run` finds in the repository's environment. `None`, and
/// the test skips, when neither is there and the run does not require it:
/// `RLXD_INTEROP_REQUIRED`, which rlxd.yml sets, makes a missing one fail.
pub fn real_rlx() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("RLXD_RLX_BIN").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(named));
    }
    let required = std::env::var_os("RLXD_INTEROP_REQUIRED").is_some_and(|v| !v.is_empty());
    let found = Command::new("uv")
        .args([
            "run",
            "--quiet",
            "python",
            "-c",
            "import shutil, sys; sys.stdout.write(shutil.which('rlx') or '')",
        ])
        .current_dir(workspace().parent().unwrap())
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    if found.is_none() {
        assert!(
            !required,
            "no rlx: RLXD_INTEROP_REQUIRED is set, so `uv run` must find the checkout's rlx"
        );
        eprintln!("skipped: no rlx (set RLXD_RLX_BIN, or run where `uv run` works)");
    }
    found
}

/// rlxd-hook, built from this tree: the executable cargo reports, so a test of the rlxd
/// package runs the rlxd-hook of the same sources. Set `CARGO_BUILD_JOBS` to bound it.
pub fn rlxd_hook() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let output = Command::new(cargo)
                .args([
                    "build",
                    "--locked",
                    "--package",
                    "rlxd-hook",
                    "--bin",
                    "rlxd-hook",
                    "--message-format=json-render-diagnostics",
                ])
                .current_dir(workspace())
                .stdin(Stdio::null())
                .stderr(Stdio::inherit())
                .output()
                .expect("cargo runs");
            assert!(output.status.success(), "cargo build -p rlxd-hook failed");
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|message| message["reason"] == "compiler-artifact")
                .filter(|message| message["target"]["name"] == "rlxd-hook")
                .find_map(|message| message["executable"].as_str().map(PathBuf::from))
                .expect("cargo names the rlxd-hook executable it built")
        })
        .clone()
}

/// Whether process `pid` is running.
pub fn alive(pid: u64) -> bool {
    if cfg!(windows) {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .stdin(Stdio::null())
            .output()
            .expect("tasklist runs");
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// `rlxd/tests/fixtures/fake-claude/<scenario>`.
pub fn fake_claude(scenario: &str) -> PathBuf {
    workspace()
        .join("tests")
        .join("fixtures")
        .join("fake-claude")
        .join(scenario)
}

/// Kills the process `pid` as `kill -9` does: no chance to clean up.
pub fn kill_hard(pid: u64) {
    assert!(kill(pid), "kill {pid}");
}

/// [`kill_hard`], and whether it could.
fn kill(pid: u64) -> bool {
    let mut command = if cfg!(windows) {
        let mut taskkill = Command::new("taskkill");
        taskkill
            .args(["/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null());
        taskkill
    } else {
        let mut kill = Command::new("kill");
        kill.args(["-9", &pid.to_string()]);
        kill
    };
    launch(&mut command)
        .and_then(|mut child| child.wait())
        .is_ok_and(|status| status.success())
}

/// rlxd, built as a release is (`cargo build --release`): without the features that only
/// rlxd's dev-dependency on itself turns on for the tests, `debug-methods` and
/// `gate-dev-mode`. The executable cargo reports, in the workspace's own target folder,
/// so rlxd.yml's later `cargo build --release` finds it done. Set `CARGO_BUILD_JOBS` to
/// bound it.
///
/// Under `cargo test --release` that is the path of this test build's rlxd too, which it
/// then replaces: a test calls this before it runs or copies `CARGO_BIN_EXE_rlxd`.
pub fn release_rlxd() -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args([
            "build",
            "--release",
            "--locked",
            "--package",
            "rlxd",
            "--bin",
            "rlxd",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(workspace())
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output()
        .expect("cargo runs");
    assert!(output.status.success(), "cargo build --release failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .filter(|message| message["target"]["name"] == "rlxd")
        .find_map(|message| message["executable"].as_str().map(PathBuf::from))
        .expect("cargo names the rlxd executable it built")
}
