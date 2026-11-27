//! Which Claude Code rlxd runs, and whether it is signed in: `status`'s `claude` (handoff
//! 0.10.3, 7.4).
//!
//! - **Which program.** `RLXD_CLAUDE_BIN` when it is set, read from rlxd's own
//!   environment only: a stand-in in tests (`rlxd/tests/fixtures/fake-claude/<scenario>`),
//!   and nothing else when it names no program. Otherwise the first `claude` on the app's
//!   `PATH` (4.3, [`resolve_on`]), else on rlxd's own, in order, skipping empty and
//!   relative entries. The path is kept as found, an absolute path with its links
//!   unresolved. On Windows a name without an extension is tried with each of `PATHEXT`'s,
//!   as `cmd` and `where` do, since Windows runs no file without one. A batch file found
//!   first, such as npm's `claude.cmd` shim, is not run ([`Resolved::NotRunnable`]): Rust
//!   starts one through `cmd`, whose rules no argument escaping makes safe for a prompt.
//! - **What it says.** `claude --version` names the version. `claude auth status --json`
//!   says whether it is signed in (`loggedIn`), how (`authMethod`), as whom (`email`) and
//!   on which plan (`subscriptionType`).
//!
//! rlx reads a sign-in from Claude Code's files and the macOS Keychain
//! (`aisquare.core.claude_accounts.signed_in`). rlxd reads no credential store itself
//! (4.3): it asks Claude Code, which reads its own.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use std::sync::Arc;

use rlxd_proto::{Claude, EnvValues};

use crate::clock;
use crate::env::Env;
use serde_json::Value;
use tokio::process::Command;

/// The variable naming the program rlxd runs as `claude`.
pub const BIN_ENV: &str = "RLXD_CLAUDE_BIN";

/// How long each of the two commands may take.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// Where `claude` was looked for: `status.claude.path_source`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The app's `PATH` (4.3).
    App,
    /// rlxd's own `PATH`, as the app set none.
    Rlxd,
    /// `RLXD_CLAUDE_BIN`, in rlxd's own environment.
    RlxdClaudeBin,
}

impl Source {
    /// Its name in `status`.
    pub fn name(self) -> &'static str {
        match self {
            Self::App => Claude::PATH_SOURCE_APP,
            Self::Rlxd => Claude::PATH_SOURCE_RLXD,
            Self::RlxdClaudeBin => Claude::PATH_SOURCE_RLXD_CLAUDE_BIN,
        }
    }
}

/// Where `claude` resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The program: an absolute path, as it was found.
    Found(PathBuf),
    /// No `claude` on the `PATH` searched, and `RLXD_CLAUDE_BIN` names none.
    NotFound,
    /// Windows: a batch file, such as npm's `claude.cmd` shim, which rlxd doesn't run.
    NotRunnable(PathBuf),
}

impl Resolved {
    /// The program, when one was found that rlxd runs.
    pub fn program(&self) -> Option<&Path> {
        match self {
            Self::Found(path) => Some(path),
            _ => None,
        }
    }
}

/// Resolves `claude` now: `RLXD_CLAUDE_BIN` from rlxd's own environment when it is set,
/// taken as it names, else the first `claude` on `path` (the app's `PATH`), or on rlxd's
/// own `PATH` when the app set none. Entries that are empty or relative are skipped: a
/// relative one would depend on rlxd's working directory. The path is kept as found,
/// never canonicalized. A batch file found on a `PATH`, such as npm's `claude.cmd` shim,
/// is [`Resolved::NotRunnable`].
pub fn resolve_on(path: Option<&OsStr>) -> (Resolved, Source) {
    let pathext = env::var_os("PATHEXT");
    if let Some(named) = env::var_os(BIN_ENV).filter(|value| !value.is_empty()) {
        // The tests' override, taken as it names: a fixture may be a batch file on Windows.
        let found = program_at(Path::new(&named), pathext.as_deref())
            .and_then(|program| std::path::absolute(program).ok());
        let resolved = found.map_or(Resolved::NotFound, Resolved::Found);
        return (resolved, Source::RlxdClaudeBin);
    }
    let source = if path.is_some() {
        Source::App
    } else {
        Source::Rlxd
    };
    let own = env::var_os("PATH");
    let found = path
        .or(own.as_deref())
        .and_then(|path| find_on(path, pathext.as_deref()));
    let resolved = match found {
        Some(program) if is_batch_file(&program) => Resolved::NotRunnable(program),
        Some(program) => Resolved::Found(program),
        None => Resolved::NotFound,
    };
    (resolved, source)
}

/// The first `claude` in `path`'s entries, in order, skipping empty and relative ones.
fn find_on(path: &OsStr, pathext: Option<&OsStr>) -> Option<PathBuf> {
    env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty() && dir.is_absolute())
        .find_map(|dir| program_at(&dir.join("claude"), pathext))
}

/// Whether Windows would start `program` through `cmd`: a `.cmd` or `.bat` file.
fn is_batch_file(program: &Path) -> bool {
    cfg!(windows)
        && program
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
}

/// How long a probe's answer is reused: a client polling `status` doesn't start Claude
/// Code each time. An env change or rlxd's start probes again at once.
pub const FRESH: Duration = Duration::from_secs(30);

/// How long `status` waits for a probe in flight before it answers with what it has: a
/// quick probe's first answer is then fresh, and a hanging one never holds `status` up.
pub const WAIT: Duration = Duration::from_millis(500);

/// `status.claude`, probed in the background (7.4): `status` gets the last answer and
/// never waits on Claude Code for more than [`WAIT`]. Each probe has a generation, and
/// one started for an older environment never overwrites a newer one's answer.
pub struct Probe {
    state: std::sync::Mutex<ProbeState>,
    /// The generation of the last probe that ended.
    done: tokio::sync::watch::Sender<u64>,
}

#[derive(Default)]
struct ProbeState {
    /// The last answer, for the environment now; none while the first is running.
    answer: Option<Claude>,
    /// When it ended.
    at: Option<std::time::Instant>,
    /// The latest probe started.
    generation: u64,
    /// The probe running, by generation.
    running: Option<u64>,
}

impl Probe {
    /// No answer yet: [`Probe::start`] the first.
    pub fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(ProbeState::default()),
            done: tokio::sync::watch::Sender::new(0),
        }
    }

    /// Starts a probe in the background, with `env`'s values and the `claude` they
    /// resolve to. With `fresh_env` (rlxd's start, an env change) always, and the answer
    /// so far is dropped, as it was for other values. Otherwise only when none is running
    /// and the last answer is older than [`FRESH`]. Needs the runtime.
    pub fn start(self: &Arc<Self>, env: &Arc<Env>, fresh_env: bool) {
        let generation = {
            let mut state = self.state();
            let stale = state.at.is_none_or(|at| at.elapsed() >= FRESH);
            if !fresh_env && (state.running.is_some() || !stale) {
                return;
            }
            if fresh_env {
                state.answer = None;
                state.at = None;
            }
            state.generation += 1;
            state.running = Some(state.generation);
            state.generation
        };
        let (probe, env) = (Arc::clone(self), Arc::clone(env));
        tokio::spawn(async move {
            let (resolved, source) = env.claude();
            let mut claude = probe_at(&resolved, source, &env.values()).await;
            claude.as_of = Some(clock::now());
            let mut state = probe.state();
            if state.generation == generation {
                state.answer = Some(claude);
                state.at = Some(std::time::Instant::now());
                state.running = None;
                drop(state);
                probe.done.send_replace(generation);
            }
        });
    }

    /// The answer for `status`: the last probe's, a running one's when it ends within
    /// [`WAIT`], else, before any has ended, what the resolution alone says, with no
    /// `as_of`. Starts a probe when the answer is stale.
    pub async fn answer(self: &Arc<Self>, env: &Arc<Env>) -> Claude {
        self.start(env, false);
        let running = self.state().running;
        if let Some(generation) = running {
            let mut done = self.done.subscribe();
            let _ = tokio::time::timeout(WAIT, done.wait_for(|ended| *ended >= generation)).await;
        }
        if let Some(answer) = self.state().answer.clone() {
            return answer;
        }
        let (resolved, source) = env.claude();
        let mut claude = Claude::default();
        claude.path_source = Some(source.name().to_owned());
        if let Some(program) = resolved.program() {
            claude.installed = true;
            claude.path = Some(program.display().to_string());
        }
        claude
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ProbeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What the Claude Code `resolved` names says about itself now, and where it was looked
/// for: `claude --version` and `claude auth status --json`, run at once with the app's
/// environment `env` (4.3), each given [`TIMEOUT`]. A command that fails says why in
/// `error`, never what it printed; the sign-in then reads as signed out.
pub async fn probe_at(resolved: &Resolved, source: Source, env: &EnvValues) -> Claude {
    probe_within(resolved, source, env, TIMEOUT).await
}

/// [`probe_at`], each command given `timeout`.
async fn probe_within(
    resolved: &Resolved,
    source: Source,
    env: &EnvValues,
    timeout: Duration,
) -> Claude {
    let mut claude = Claude::default();
    claude.path_source = Some(source.name().to_owned());
    let Some(program) = resolved.program() else {
        return claude;
    };
    claude.installed = true;
    claude.path = Some(program.display().to_string());
    let (version, auth) = tokio::join!(
        output(program, &["--version"], env, timeout),
        output(program, &["auth", "status", "--json"], env, timeout),
    );
    let mut errors = Vec::new();
    match version {
        Ok(text) => claude.version = parse_version(&text),
        Err(why) => errors.push(format!("claude --version {why}")),
    }
    match auth.map(|text| serde_json::from_str::<Value>(&text)) {
        Ok(Ok(status)) if status.is_object() => read_auth_status(&mut claude, &status),
        Ok(_) => errors.push("claude auth status --json printed no JSON object".to_owned()),
        Err(why) => errors.push(format!("claude auth status --json {why}")),
    }
    if !errors.is_empty() {
        claude.error = Some(errors.join("; "));
    }
    claude
}

/// The program `path` names, if it names one this OS can run.
fn program_at(path: &Path, pathext: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if cfg!(windows) {
        windows_program_at(path, pathext)
    } else {
        unix_program_at(path)
    }
}

/// A file with an execute bit.
#[cfg(unix)]
fn unix_program_at(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).ok()?;
    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then(|| path.to_owned())
}

#[cfg(not(unix))]
fn unix_program_at(_path: &Path) -> Option<PathBuf> {
    None
}

/// `path` when its extension is one of `PATHEXT`'s, else `path` with the first of them
/// that names a file.
fn windows_program_at(path: &Path, pathext: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let extensions = executable_extensions(pathext);
    let has_one = path.extension().is_some_and(|ext| {
        let ext = format!(".{}", ext.to_string_lossy()).to_ascii_lowercase();
        extensions.contains(&ext)
    });
    if has_one {
        return path.is_file().then(|| path.to_owned());
    }
    extensions.iter().find_map(|ext| {
        let mut name: OsString = path.as_os_str().to_owned();
        name.push(ext);
        let candidate = PathBuf::from(name);
        candidate.is_file().then_some(candidate)
    })
}

/// `PATHEXT`'s extensions, lower-case, or Windows' default list.
fn executable_extensions(pathext: Option<&std::ffi::OsStr>) -> Vec<String> {
    let list = pathext
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_owned());
    list.split(';')
        .map(str::trim)
        .filter(|ext| ext.starts_with('.') && ext.len() > 1)
        .map(str::to_ascii_lowercase)
        .collect()
}

/// What `program args` printed, when it ran to an end within [`TIMEOUT`]. Its exit status
/// does not matter: `auth status` exits 1 when signed out, and says so.
async fn output(
    program: &Path,
    args: &[&str],
    env: &EnvValues,
    timeout: Duration,
) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.pairs())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: a detached rlxd has no console, and a console program it
        // starts would open a window of its own.
        command.creation_flags(0x0800_0000);
    }
    // Started as no other program of rlxd's starts (`crate::spawn`): the brain must not
    // inherit this probe's stdout, nor this probe the brain's stdin. Only the start is
    // held, never the wait.
    let child = {
        let _alone = crate::spawn::alone();
        command
            .spawn()
            .map_err(|e| format!("could not start: {e}"))?
    };
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("did not answer within {} ms", timeout.as_millis()))?
        .map_err(|e| format!("could not be read: {e}"))?;
    String::from_utf8(output.stdout).map_err(|_| "printed text that is not UTF-8".to_owned())
}

/// `2.1.314` from `2.1.314 (Claude Code)`: the first word of the first line, when it
/// starts with a digit.
fn parse_version(text: &str) -> Option<String> {
    let word = text
        .lines()
        .find(|l| !l.trim().is_empty())?
        .split_whitespace()
        .next()?;
    word.starts_with(|c: char| c.is_ascii_digit())
        .then(|| word.to_owned())
}

/// Fills `claude`'s sign-in from what `auth status --json` printed.
fn read_auth_status(claude: &mut Claude, status: &Value) {
    let text = |key: &str| {
        status
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    claude.signed_in = status.get("loggedIn").and_then(Value::as_bool) == Some(true);
    if claude.signed_in {
        claude.auth_method = auth_method(text("authMethod"), text("apiProvider"));
        claude.account = text("email");
        claude.plan = text("subscriptionType");
    }
}

/// `auth_method` from what `claude auth status` reports: its `authMethod` (`claude.ai`,
/// `api_key`, `oauth_token`), but for `third_party` the provider it names (`bedrock`,
/// `vertex`, `foundry`, …), and nothing for `none`.
fn auth_method(method: Option<String>, provider: Option<String>) -> Option<String> {
    match method.as_deref() {
        None | Some("none") => None,
        Some("third_party") => provider.or(method),
        Some(_) => method,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn path_is_walked_in_order_skipping_empty_and_relative_entries_and_kept_as_found() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("rlxd-find-on-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (rel, first, second) = (base.join("rel"), base.join("first"), base.join("second"));
        let target = base.join("program");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&target, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        for dir in [&rel, &second] {
            std::fs::create_dir_all(dir).unwrap();
            std::os::unix::fs::symlink(&target, dir.join("claude")).unwrap();
        }
        std::fs::create_dir_all(&first).unwrap();
        // `rel`, spelled relative to the working directory: it holds a claude, and is
        // skipped all the same.
        let cwd = std::env::current_dir().unwrap();
        let up = "../".repeat(cwd.components().count() - 1);
        let relative = format!("{up}{}", rel.strip_prefix("/").unwrap().display());
        assert!(Path::new(&relative).join("claude").exists(), "{relative}");
        let path = format!(":{relative}:{}:{}", first.display(), second.display());
        let found = find_on(OsStr::new(&path), None).unwrap();
        assert_eq!(found, second.join("claude"), "the link, as found");
        assert_eq!(find_on(OsStr::new(&relative), None), None);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_batch_file_is_one_only_on_windows() {
        assert_eq!(
            is_batch_file(Path::new(r"C:\npm\claude.cmd")),
            cfg!(windows)
        );
        assert_eq!(
            is_batch_file(Path::new(r"C:\npm\CLAUDE.BAT")),
            cfg!(windows)
        );
        assert!(!is_batch_file(Path::new(r"C:\bin\claude.exe")));
    }

    #[test]
    fn the_version_is_the_first_word_when_it_is_a_number() {
        assert_eq!(
            parse_version("2.1.314 (Claude Code)\n").as_deref(),
            Some("2.1.314")
        );
        assert_eq!(parse_version("\n  2.1.400\n").as_deref(), Some("2.1.400"));
        assert_eq!(parse_version("error: no such flag\n"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn status_carries_only_its_own_fields_of_claude_codes_answer() {
        // Manager #2554: nothing of auth status's org, folders or expiry is passed on.
        let mut claude = Claude::default();
        let status = json!({"loggedIn": true, "authMethod": "claude.ai",
            "apiProvider": "firstParty", "analyticsDisabled": false,
            "projectsDirectory": "/p", "configDirectory": "/c",
            "loginExpiresAt": "2026-12-01T00:00:00.000Z", "email": "probe@example.com",
            "orgId": "00000000-0000-0000-0000-000000000000", "orgName": "Probe Org",
            "subscriptionType": "pro", "storedClaudeAiLogin": true, "planTier": "pro_1"});
        read_auth_status(&mut claude, &status);
        let printed = serde_json::to_value(&claude).unwrap();
        let keys: Vec<&str> = printed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["account", "auth_method", "installed", "plan", "signed_in"],
            "{printed}"
        );
        let text = printed.to_string();
        for kept_out in ["Probe Org", "00000000", "/c", "/p", "2026-12-01", "pro_1"] {
            assert!(!text.contains(kept_out), "{kept_out}: {text}");
        }
    }

    #[test]
    fn auth_method_is_claude_codes_or_the_provider_for_a_third_party() {
        let method =
            |m: &str, p: Option<&str>| auth_method(Some(m.to_owned()), p.map(str::to_owned));
        assert_eq!(
            method("claude.ai", Some("firstParty")).as_deref(),
            Some("claude.ai")
        );
        assert_eq!(
            method("api_key", Some("firstParty")).as_deref(),
            Some("api_key")
        );
        assert_eq!(method("oauth_token", None).as_deref(), Some("oauth_token"));
        assert_eq!(
            method("third_party", Some("bedrock")).as_deref(),
            Some("bedrock")
        );
        assert_eq!(
            method("third_party", Some("vertex")).as_deref(),
            Some("vertex")
        );
        assert_eq!(method("third_party", None).as_deref(), Some("third_party"));
        assert_eq!(method("none", None), None);
        assert_eq!(auth_method(None, Some("bedrock".to_owned())), None);
        let mut claude = Claude::default();
        read_auth_status(
            &mut claude,
            &json!({"loggedIn": true, "authMethod": "third_party", "apiProvider": "vertex"}),
        );
        assert_eq!(claude.auth_method.as_deref(), Some("vertex"));
        assert_eq!((claude.account, claude.plan), (None, None));
    }

    /// A program in a fresh folder: a shell script of `body`.
    #[cfg(unix)]
    fn script(name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rlxd-probe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("claude");
        std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_probe_runs_claude_with_the_apps_environment() {
        let program = script(
            "env",
            r#"case "$1" in
  --version) echo "2.1.332 (Claude Code)";;
  auth) printf '{"loggedIn":true,"authMethod":"claude.ai","email":"probe+%s@example.com","subscriptionType":"pro"}' "$CLAUDE_CONFIG_DIR";;
esac"#,
        );
        let mut env = EnvValues::default();
        env.claude_config_dir = Some("cc-test".to_owned());
        let resolved = Resolved::Found(program.clone());
        let claude = probe_within(&resolved, Source::App, &env, TIMEOUT).await;
        assert!(claude.installed && claude.signed_in, "{claude:?}");
        assert_eq!(claude.version.as_deref(), Some("2.1.332"));
        assert_eq!(claude.account.as_deref(), Some("probe+cc-test@example.com"));
        assert_eq!(claude.plan.as_deref(), Some("pro"));
        assert_eq!(claude.path_source.as_deref(), Some("app"));
        assert_eq!(claude.error, None);
        std::fs::remove_dir_all(program.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_probe_that_hangs_or_prints_no_json_says_why_and_reads_as_signed_out() {
        let program = script("hangs", "exec sleep 30");
        let resolved = Resolved::Found(program.clone());
        let started = std::time::Instant::now();
        let claude = probe_within(
            &resolved,
            Source::Rlxd,
            &EnvValues::default(),
            Duration::from_millis(300),
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(claude.installed && !claude.signed_in, "{claude:?}");
        let error = claude.error.unwrap();
        assert!(
            error.contains("claude auth status --json did not answer within 300 ms"),
            "{error}"
        );
        std::fs::remove_dir_all(program.parent().unwrap()).unwrap();

        let program = script("not-json", "echo 'secret-looking text'");
        let resolved = Resolved::Found(program.clone());
        let claude = probe_within(&resolved, Source::Rlxd, &EnvValues::default(), TIMEOUT).await;
        assert!(!claude.signed_in);
        let error = claude.error.unwrap();
        assert!(error.contains("printed no JSON object"), "{error}");
        assert!(!error.contains("secret"), "never what it printed: {error}");
        std::fs::remove_dir_all(program.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn not_installed_is_installed_false_and_nothing_else_but_where_it_looked() {
        let claude = probe_at(&Resolved::NotFound, Source::App, &EnvValues::default()).await;
        assert!(!claude.installed && !claude.signed_in);
        assert_eq!(
            (claude.version, claude.path, claude.error),
            (None, None, None)
        );
        assert_eq!(claude.path_source.as_deref(), Some("app"));
    }

    #[test]
    fn auth_status_gives_the_sign_in_and_nothing_when_signed_out() {
        let mut claude = Claude::default();
        let status = json!({"loggedIn": true, "authMethod": "claude.ai",
            "email": "probe@example.com", "subscriptionType": "pro", "orgName": "x"});
        read_auth_status(&mut claude, &status);
        assert!(claude.signed_in);
        assert_eq!(claude.auth_method.as_deref(), Some("claude.ai"));
        assert_eq!(claude.account.as_deref(), Some("probe@example.com"));
        assert_eq!(claude.plan.as_deref(), Some("pro"));

        let mut claude = Claude::default();
        read_auth_status(
            &mut claude,
            &json!({"loggedIn": false, "authMethod": "none"}),
        );
        assert!(!claude.signed_in);
        assert_eq!(claude.auth_method, None);
        let mut claude = Claude::default();
        read_auth_status(&mut claude, &json!({"loggedIn": "yes"}));
        assert!(!claude.signed_in);
    }

    #[test]
    fn on_windows_a_name_is_tried_with_each_executable_extension() {
        let dir = env::temp_dir().join(format!("rlxd-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fake"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("fake.cmd"), "@echo off\n").unwrap();
        let pathext = Some(std::ffi::OsStr::new(".COM;.EXE;.BAT;.CMD"));
        assert_eq!(
            windows_program_at(&dir.join("fake"), pathext),
            Some(dir.join("fake.cmd")),
            "the extensionless file is not one Windows runs"
        );
        assert_eq!(
            windows_program_at(&dir.join("fake.cmd"), pathext),
            Some(dir.join("fake.cmd"))
        );
        assert_eq!(windows_program_at(&dir.join("gone"), pathext), None);
        assert_eq!(
            windows_program_at(&dir.join("fake"), Some(".EXE".as_ref())),
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pathext_is_read_as_windows_reads_it() {
        assert_eq!(
            executable_extensions(Some(".exe; .CMD;;bad;.".as_ref())),
            [".exe", ".cmd"]
        );
        assert_eq!(executable_extensions(Some("".as_ref())).len(), 4);
        assert_eq!(
            executable_extensions(None),
            [".com", ".exe", ".bat", ".cmd"]
        );
    }
}
