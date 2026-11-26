//! What the daemon's tests share: a fresh home each, the `rlxd` binary run in it with a
//! time limit, and the checked-in schemas.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long one `rlxd` command may take before the test fails.
const COMMAND_LIMIT: Duration = Duration::from_secs(60);

/// A fresh home, under the system's temporary folder with a short name so the socket's
/// path fits the OS's limit. Dropping it stops its rlxd and removes it.
pub struct TestHome {
    root: PathBuf,
    claude: Option<PathBuf>,
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
    /// names one.
    pub fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("rlxd-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self {
            root,
            claude: None,
            binary: None,
        }
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

    /// `rlxd args…` in this home, started and left running, with its stdout and stderr
    /// piped: [`finish`] waits for it.
    pub fn spawn(&self, args: &[&str]) -> Child {
        let mut command = self.command(args);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().expect("rlxd starts")
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
    /// unset, so neither a developer's home nor CI's is touched.
    pub fn command(&self, args: &[&str]) -> Command {
        let binary = self
            .binary
            .clone()
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_rlxd")));
        let mut command = Command::new(binary);
        command
            .args(args)
            .env("AISQUARE_HOME", &self.root)
            .env_remove("RLX_HOME")
            .env(
                "RLXD_CLAUDE_BIN",
                self.claude
                    .clone()
                    .unwrap_or_else(|| self.root.join("no-claude")),
            );
        command
    }

    /// `rlxd serve --detach`, which must succeed.
    pub fn start(&self) -> Value {
        let run = self.rlxd(&["serve", "--detach"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        run.data()
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
        let _ = self.rlxd(&["stop"]);
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
/// [`COMMAND_LIMIT`].
pub fn run(mut command: Command, stdin: Option<&str>) -> Run {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("rlxd starts");
    if let Some(text) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(text.as_bytes()).unwrap();
    }
    finish(child)
}

/// Waits for `child`, started with its stdout and stderr piped, to end, failing the test if
/// it takes longer than [`COMMAND_LIMIT`].
pub fn finish(mut child: Child) -> Run {
    let stdout = read_all(child.stdout.take().unwrap());
    let stderr = read_all(child.stderr.take().unwrap());
    let status = wait(&mut child);
    Run {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

fn read_all(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        let _ = pipe.read_to_string(&mut text);
        text
    })
}

fn wait(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + COMMAND_LIMIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("rlxd ran longer than {} s", COMMAND_LIMIT.as_secs());
        }
        thread::sleep(Duration::from_millis(10));
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
    let status = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .status()
    } else {
        Command::new("kill").args(["-9", &pid.to_string()]).status()
    };
    assert!(status.unwrap().success(), "kill {pid}");
}
