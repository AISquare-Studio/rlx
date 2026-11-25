//! The shared client token, `<home>/rlxd/token` (handoff 0.10.3, 1.3): random, made
//! once, mode 0600. `hello` requires it, and anything that can read it already runs as the
//! user.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// The token's random bytes: 32, written as 64 hex digits.
const BYTES: usize = 32;

/// The token in `path`, made first when the file is missing or empty. The file is left
/// 0600 on macOS and Linux, tightened if it was looser.
pub fn ensure(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => {
            restrict(path)?;
            return Ok(text.trim().to_owned());
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let token = random_hex()?;
    write_private(path, &token)?;
    Ok(token)
}

/// Whether `given` is `token`, in time that does not depend on where they first differ.
pub fn matches(token: &str, given: &str) -> bool {
    let (a, b) = (token.as_bytes(), given.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn random_hex() -> io::Result<String> {
    let mut bytes = [0u8; BYTES];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(format!("no random bytes: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Writes `text` to `path` whole: a 0600 sibling, flushed to disk, renamed over it.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    restrict(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)
}

#[cfg(unix)]
fn restrict(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode() & 0o777;
    if mode != 0o600 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// On Windows the file takes its folder's ACL, which is the user's profile's.
#[cfg(not(unix))]
fn restrict(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_made_once_and_then_read_back() {
        let dir = std::env::temp_dir().join(format!("rlxd-token-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        let made = ensure(&path).unwrap();
        assert_eq!(made.len(), 2 * BYTES);
        assert!(made.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(ensure(&path).unwrap(), made);
        fs::write(&path, "").unwrap();
        let remade = ensure(&path).unwrap();
        assert_ne!(remade, made);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(ensure(&path).unwrap(), remade);
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_the_same_token_matches() {
        assert!(matches("abc", "abc"));
        assert!(!matches("abc", "abd"));
        assert!(!matches("abc", "ab"));
        assert!(!matches("abc", ""));
    }
}
