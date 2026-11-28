//! Where rlx and rlxd are installed: the folders to look in for them, in order. This is
//! the one list: `rlxd here` looks for rlx in it ([`find_rlx`]), as rlxd does for the rlx
//! it runs as its brain and rlxd-hook for the one it falls back to ([`rlx_to_run`]), and
//! `rlxd here` gives the rlxd it starts these folders on its `PATH`. rlxapp looks in it for a local rlxd, and the README spells it
//! out for a remote one, where a non-login SSH session has not read the user's profile.
//!
//! 1. **uv's tool bin folder**, where `uv tool install` puts `rlx`, found as uv finds it:
//!    `UV_TOOL_BIN_DIR`, else `XDG_BIN_HOME`, else `XDG_DATA_HOME/../bin`, else
//!    `~/.local/bin`. A variable counts only when it holds an absolute path. rlxd goes
//!    beside rlx here: it ships with 0.10.3's release binaries, which `rlx daemon install`
//!    (#21) puts in place, and the installer and `rlx upgrade` must put it here too.
//! 2. **`~/.local/bin`**, uv's default, again: a variable that moved uv's folder is often
//!    set only in the profile a non-login session skips.
//! 3. **Homebrew's** `bin` folders: `/opt/homebrew/bin` (Apple silicon) and
//!    `/home/linuxbrew/.linuxbrew/bin`.
//! 4. **`/usr/local/bin`**.
//!
//! On Windows only the first two apply.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The folders rlx and rlxd install to, in the order to look in them, each once. Folders
/// that do not exist are listed too.
pub fn dirs() -> Vec<PathBuf> {
    dirs_from(|name| env::var_os(name), env::home_dir(), cfg!(windows))
}

/// The variable naming the program rlxd and rlxd-hook run as `rlx`, in place of looking
/// for it: for tests and development, such as a checkout's `.venv/bin/rlx`. It must be an
/// absolute path.
pub const RLX_BIN_ENV: &str = "RLXD_RLX_BIN";

/// rlx's command, as an absolute path: in `beside` first, the folder of the rlxd or
/// rlxd-hook asking, since they are installed beside rlx; then in [`dirs`]; then in the
/// absolute folders on `PATH`. This is `rlxd here`'s lookup.
pub fn find_rlx(beside: Option<&Path>) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = beside.into_iter().map(Path::to_path_buf).collect();
    dirs.extend(self::dirs());
    dirs.extend(
        env::var_os("PATH")
            .map(|path| env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|dir| dir.is_absolute()),
    );
    find_in("rlx", &dirs)
}

/// The `rlx` to run, rlxd's brain (`rlx brain --stdio`) or rlxd-hook's fallback (`rlx
/// hook <event>`): what [`RLX_BIN_ENV`] names when it is set and not empty, and nothing
/// else when that is not an absolute path to a file; otherwise [`find_rlx`]. Never a bare
/// name for the OS to look up when it starts the program.
pub fn rlx_to_run(beside: Option<&Path>) -> Option<PathBuf> {
    match env::var_os(RLX_BIN_ENV).filter(|value| !value.is_empty()) {
        Some(named) => {
            let path = PathBuf::from(named);
            (path.is_absolute() && path.is_file()).then_some(path)
        }
        None => find_rlx(beside),
    }
}

/// `program` in the first of `dirs` that holds it as an executable file: `program.exe`
/// on Windows.
pub fn find_in(program: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let name = format!("{program}{}", env::consts::EXE_SUFFIX);
    dirs.iter()
        .map(|dir| dir.join(&name))
        .find(|path| is_executable(path))
}

/// [`dirs`], given what the environment holds.
fn dirs_from(
    var: impl Fn(&str) -> Option<OsString>,
    user_home: Option<PathBuf>,
    windows: bool,
) -> Vec<PathBuf> {
    let absolute = |name: &str| {
        var(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let user_home = user_home.filter(|dir| !dir.as_os_str().is_empty());
    let local_bin = user_home.map(|home| home.join(".local").join("bin"));
    let uv_bin = absolute("UV_TOOL_BIN_DIR")
        .or_else(|| absolute("XDG_BIN_HOME"))
        .or_else(|| absolute("XDG_DATA_HOME").map(|data| data.join("..").join("bin")))
        .or_else(|| local_bin.clone());
    let mut found: Vec<PathBuf> = Vec::new();
    let mut add = |dir: Option<PathBuf>| {
        if let Some(dir) = dir
            && !found.contains(&dir)
        {
            found.push(dir);
        }
    };
    add(uv_bin);
    add(local_bin);
    if !windows {
        for dir in [
            "/opt/homebrew/bin",
            "/home/linuxbrew/.linuxbrew/bin",
            "/usr/local/bin",
        ] {
            add(Some(PathBuf::from(dir)));
        }
    }
    found
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let pairs: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[cfg(unix)]
    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[cfg(unix)]
    #[test]
    fn with_nothing_set_uvs_default_then_homebrew_then_usr_local() {
        let dirs = dirs_from(vars(&[]), Some(PathBuf::from("/u")), false);
        assert_eq!(
            dirs,
            paths(&[
                "/u/.local/bin",
                "/opt/homebrew/bin",
                "/home/linuxbrew/.linuxbrew/bin",
                "/usr/local/bin",
            ])
        );
    }

    #[cfg(unix)]
    #[test]
    fn uvs_folder_is_found_as_uv_finds_it_and_local_bin_follows() {
        let home = || Some(PathBuf::from("/u"));
        let all = vars(&[
            ("UV_TOOL_BIN_DIR", "/t"),
            ("XDG_BIN_HOME", "/x"),
            ("XDG_DATA_HOME", "/d/share"),
        ]);
        let dirs = dirs_from(all, home(), false);
        assert_eq!(dirs[..2], paths(&["/t", "/u/.local/bin"]));
        let xdg = vars(&[("XDG_BIN_HOME", "/x"), ("XDG_DATA_HOME", "/d/share")]);
        assert_eq!(dirs_from(xdg, home(), false)[0], PathBuf::from("/x"));
        let data = vars(&[("XDG_DATA_HOME", "/d/share")]);
        assert_eq!(
            dirs_from(data, home(), false)[0],
            PathBuf::from("/d/share/../bin")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_or_empty_variable_does_not_count() {
        let home = || Some(PathBuf::from("/u"));
        for value in ["rel/bin", ""] {
            let set = vars(&[("UV_TOOL_BIN_DIR", value), ("XDG_BIN_HOME", value)]);
            assert_eq!(
                dirs_from(set, home(), false)[0],
                PathBuf::from("/u/.local/bin")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn with_no_users_home_only_what_is_named_and_the_system_folders() {
        assert_eq!(
            dirs_from(vars(&[]), None, false)[0],
            PathBuf::from("/opt/homebrew/bin")
        );
        let set = vars(&[("UV_TOOL_BIN_DIR", "/t")]);
        assert_eq!(dirs_from(set, None, false)[0], PathBuf::from("/t"));
    }

    #[test]
    fn windows_has_no_homebrew_or_usr_local() {
        let home = env::temp_dir();
        let dirs = dirs_from(vars(&[]), Some(home.clone()), true);
        assert_eq!(dirs, vec![home.join(".local").join("bin")]);
    }

    #[test]
    fn find_in_takes_the_first_folder_holding_the_program() {
        let root = env::temp_dir().join(format!("rlxd-install-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let name = format!("rlx{}", env::consts::EXE_SUFFIX);
        let dirs = vec![root.join("missing"), a.clone(), b.clone()];
        assert_eq!(find_in("rlx", &dirs), None);
        for dir in [&a, &b] {
            std::fs::write(dir.join(&name), "").unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(find_in("rlx", &dirs), None, "not executable");
            let mode = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(b.join(&name), mode.clone()).unwrap();
            assert_eq!(find_in("rlx", &dirs), Some(b.join(&name)));
            std::fs::set_permissions(a.join(&name), mode).unwrap();
        }
        assert_eq!(find_in("rlx", &dirs), Some(a.join(&name)));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
