//! A record Python's registry writes, held through `aisquare.core.locking`, read by rlxd
//! (handoff 0.10.3, 4.1): live while the Python process holds its lock, and dead once that
//! process is gone, whatever its record still says.
//!
//! It runs `uv run python` from the repository's root. It skips unless
//! `RLXD_INTEROP_REQUIRED` or `RLXD_BIN` is set, so a checkout without uv passes; rlxd.yml
//! sets `RLXD_INTEROP_REQUIRED=1`, and there a missing uv fails.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use common::TestHome;
use rlxd::registry;
use rlxd_client::Home;

/// Registers as a `watch` through `core.daemons`, prints the record's id, and holds its
/// lock until stdin closes. Then it exits with no cleanup, as a killed process would.
const PYTHON: &str = r#"
import os, sys
from aisquare.core import daemons
registration = daemons.register("watch", argv=[sys.executable, "-c", "pass"], restartable=False)
print(registration.daemon.id, flush=True)
sys.stdin.read()
os._exit(0)
"#;

fn required() -> bool {
    ["RLXD_INTEROP_REQUIRED", "RLXD_BIN"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
}

#[test]
fn a_record_python_holds_reads_as_live_and_then_as_dead() {
    if !required() {
        eprintln!("skipped: set RLXD_INTEROP_REQUIRED=1 to run Python's side (rlxd.yml does)");
        return;
    }
    let home = TestHome::new();
    let repo = common::workspace().parent().unwrap().to_path_buf();
    let mut python = Command::new("uv")
        .args(["run", "--quiet", "python", "-c", PYTHON])
        .current_dir(&repo)
        .env("AISQUARE_HOME", home.root())
        .env_remove("RLX_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("uv must run here (RLXD_INTEROP_REQUIRED is set): {e}"));
    let mut id = String::new();
    BufReader::new(python.stdout.take().unwrap())
        .read_line(&mut id)
        .unwrap();
    let id = id.trim().to_owned();
    assert_eq!(id.len(), 12, "Python printed its record's id: {id:?}");

    let rlxd = Home::at(home.root());
    let roll = registry::read(&rlxd).unwrap();
    let live: Vec<_> = roll
        .live
        .iter()
        .map(|r| (r.id.as_str(), r.kind.as_str()))
        .collect();
    assert_eq!(live, [(id.as_str(), "watch")], "{roll:?}");

    drop(python.stdin.take());
    assert!(python.wait().unwrap().success());
    let roll = registry::read(&rlxd).unwrap();
    assert!(roll.live.is_empty(), "{roll:?}");
    let dead: Vec<_> = roll.dead.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(dead, [id.as_str()], "its record stays, with its lock free");
}
