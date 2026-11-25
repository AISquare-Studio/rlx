//! Which Claude Code rlxd runs, and whether it is signed in: `status`'s `claude` (handoff
//! 0.10.3, 7.4).
//!
//! - **Which program.** `RLXD_CLAUDE_BIN` when it is set: a stand-in in tests
//!   (`rlxd/tests/fixtures/fake-claude/<scenario>`), and nothing else when it names no
//!   program. Otherwise `claude` on `PATH`. On Windows a name without an extension is
//!   tried with each of `PATHEXT`'s, as `cmd` and `where` do, since Windows runs no
//!   file without one.
//! - **What it says.** `claude --version` names the version. `claude auth status --json`
//!   says whether it is signed in (`loggedIn`), how (`authMethod`), as whom (`email`) and
//!   on which plan (`subscriptionType`).
//!
//! rlx reads a sign-in from Claude Code's files and the macOS Keychain
//! (`aisquare.core.claude_accounts.signed_in`). rlxd reads no credential store itself
//! (4.3): it asks Claude Code, which reads its own.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rlxd_proto::Claude;
use serde_json::Value;
use tokio::process::Command;

/// The variable naming the program rlxd runs as `claude`.
pub const BIN_ENV: &str = "RLXD_CLAUDE_BIN";

/// How long each of the two commands may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// What Claude Code says about itself now.
pub async fn probe() -> Claude {
    let mut claude = Claude::default();
    let Some(program) = resolve() else {
        return claude;
    };
    claude.installed = true;
    let (version, auth) = tokio::join!(
        output(&program, &["--version"]),
        output(&program, &["auth", "status", "--json"]),
    );
    claude.version = version.as_deref().and_then(parse_version);
    if let Some(status) = auth
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
    {
        read_auth_status(&mut claude, &status);
    }
    claude
}

/// The program to run as `claude`, if there is one.
pub fn resolve() -> Option<PathBuf> {
    let pathext = env::var_os("PATHEXT");
    if let Some(named) = env::var_os(BIN_ENV).filter(|value| !value.is_empty()) {
        return program_at(Path::new(&named), pathext.as_deref());
    }
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| program_at(&dir.join("claude"), pathext.as_deref()))
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
async fn output(program: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new(program);
    command
        .args(args)
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
    let output = tokio::time::timeout(TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    String::from_utf8(output.stdout).ok()
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
        claude.auth_method = text("authMethod");
        claude.account = text("email");
        claude.plan = text("subscriptionType");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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
