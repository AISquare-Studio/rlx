//! Spaces (handoff 0.10.3, 2.5): where app runs work, which is not a project folder.
//!
//! `space.ensure {name}` makes `<home>/spaces/<name>/` and returns its path. A space holds
//! the runs' working directory and an empty project `.claude/settings.json`, so that
//! `--setting-sources project` has a project. Nothing run-specific goes there: each run's
//! gate settings are its own file (5.1). A space is not a project: it has no `.git` and
//! is not onboarded, so rlx's project root markers never find one.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rlxd_client::Home;

/// `<home>/spaces/`.
pub fn root(home: &Home) -> PathBuf {
    home.root().join("spaces")
}

/// Makes the space `name` if it is not there, with its empty project settings, and
/// returns its folder, absolute and resolved, as the Claude Code that runs in it reports
/// its `cwd`. `name` has passed the space name rule (`rlxd_proto::check_space_name`), so
/// it is one folder inside `<home>/spaces/`. A space folder, or its `.claude`, that is a
/// link is refused rather than followed.
pub fn ensure(home: &Home, name: &str) -> io::Result<PathBuf> {
    let root = root(home);
    fs::create_dir_all(&root)?;
    let space = root.join(name);
    make_dir(&space)?;
    let project = space.join(".claude");
    make_dir(&project)?;
    let settings = project.join("settings.json");
    match fs::symlink_metadata(&settings) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(linked(&settings));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::write(&settings, "{}\n")?,
        Err(e) => return Err(e),
    }
    resolved(&space)
}

/// Makes `dir` if it is not there; refuses a link in its place.
fn make_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => Err(linked(dir)),
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} is a file, not a folder", dir.display()),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir(dir) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => make_dir(dir),
            other => other,
        },
        Err(e) => Err(e),
    }
}

fn linked(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{} is a link, which rlxd does not follow out of a space",
            path.display()
        ),
    )
}

/// Every link inside `space`, found without following any: a symlink, or on Windows a
/// junction. (Windows' erase lists them before std removes them; elsewhere the erase
/// finds them as it removes.)
#[cfg_attr(not(windows), allow(dead_code))]
pub fn links_in(space: &Path) -> Vec<PathBuf> {
    let mut links = Vec::new();
    let mut pending: Vec<PathBuf> = fs::read_dir(space)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    while let Some(path) = pending.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            links.push(path);
            continue;
        }
        if meta.is_dir()
            && let Ok(entries) = fs::read_dir(&path)
        {
            pending.extend(entries.flatten().map(|entry| entry.path()));
        }
    }
    links
}

/// `path` resolved, without Windows' verbatim prefix (`\\?\C:\x` is `C:\x`).
pub fn resolved(path: &Path) -> io::Result<PathBuf> {
    Ok(plain(&fs::canonicalize(path)?))
}

/// `path` without Windows' verbatim prefix (`\\?\C:\x` is `C:\x`, `\\?\UNC\s\x` is
/// `\\s\x`), as it is shown.
pub fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }
    if let Some(plain) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(plain);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (Home, PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rlxd-spaces-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (Home::at(&dir), dir)
    }

    #[test]
    fn a_space_is_made_once_with_empty_project_settings_and_no_git() {
        let (home, dir) = home();
        let path = ensure(&home, "default").unwrap();
        assert!(path.is_absolute());
        assert_eq!(path, resolved(&dir.join("spaces").join("default")).unwrap());
        assert_eq!(
            fs::read_to_string(path.join(".claude").join("settings.json")).unwrap(),
            "{}\n"
        );
        assert!(!path.join(".git").exists());
        fs::write(path.join("kept"), "x").unwrap();
        assert_eq!(ensure(&home, "default").unwrap(), path);
        assert!(
            path.join("kept").exists(),
            "ensuring again keeps what is there"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_space_that_is_a_link_is_refused() {
        let (home, dir) = home();
        let elsewhere = dir.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::create_dir_all(dir.join("spaces")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("spaces").join("linked")).unwrap();
        let error = ensure(&home, "linked").unwrap_err().to_string();
        assert!(error.contains("is a link"), "{error}");
        assert!(!elsewhere.join(".claude").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
