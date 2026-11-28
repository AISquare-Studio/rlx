//! rlxd's registry reader on records rlxd wrote: live while its rlxd runs, dead after a
//! kill, and a file that is not a record reported, not dropped. `registry_interop.rs`
//! does the same with records Python writes and locks.

mod common;

use common::{TestHome, kill_hard};
use rlxd::registry;
use rlxd_client::Home;

#[test]
fn a_running_rlxd_reads_as_live_and_a_killed_one_as_dead() {
    let home = TestHome::new();
    let pid = home.start()["pid"].as_u64().unwrap();
    let rlxd = Home::at(home.root());
    let roll = registry::read(&rlxd).unwrap();
    assert_eq!(roll.live.len(), 1, "{roll:?}");
    assert_eq!(u64::from(roll.live[0].pid), pid);
    assert!(
        roll.dead.is_empty() && roll.unreadable.is_empty(),
        "{roll:?}"
    );

    kill_hard(pid);
    common::eventually("the killed rlxd reads as dead", || {
        let roll = registry::read(&rlxd).unwrap();
        roll.live.is_empty() && roll.dead.len() == 1
    });
}

#[test]
fn a_file_that_is_not_a_record_is_reported() {
    let home = TestHome::new();
    let rlxd = Home::at(home.root());
    assert!(
        registry::read(&rlxd).unwrap().live.is_empty(),
        "no folder, no records"
    );
    std::fs::create_dir_all(rlxd.daemons()).unwrap();
    let examples = common::workspace().join("crates/rlxd-proto/tests/examples");
    let record = std::fs::read_to_string(examples.join("daemon-record.json")).unwrap();
    let id = serde_json::from_str::<serde_json::Value>(&record).unwrap()["id"].clone();
    assert_ne!(id, "ba9876543210");
    for (name, text) in [
        ("0123456789ab.json", "not json"),
        ("ba9876543210.json", record.as_str()),
        ("cafe00000000.json", r#"{"id": "cafe00000000"}"#),
        ("not-an-id.json", "{}"),
        ("ABCDEF012345.json", "{}"),
    ] {
        std::fs::write(rlxd.daemons().join(name), text).unwrap();
    }
    let roll = registry::read(&rlxd).unwrap();
    let mut unreadable: Vec<String> = roll
        .unreadable
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    unreadable.sort();
    // Not JSON; a valid record under another id's name; a field missing. The two names
    // that are no record id are not records at all.
    assert_eq!(
        unreadable,
        [
            "0123456789ab.json",
            "ba9876543210.json",
            "cafe00000000.json"
        ]
    );
    assert!(roll.live.is_empty() && roll.dead.is_empty());
}

/// The record is rewritten at each heartbeat through a sibling of a random name made new:
/// a FIFO, or a link, put at a name it might have used (the old fixed `<id>.json.tmp`) is
/// never opened or written through, and the heartbeat goes on.
#[cfg(unix)]
#[test]
fn nothing_planted_beside_the_record_is_opened_or_written_through() {
    let home = TestHome::new();
    home.start();
    let dir = home.root().join("daemons");
    let record = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|e| e == "json"))
        .expect("rlxd's record");
    let planted = record.with_extension("json.tmp");
    let heartbeat = || -> String {
        let text = std::fs::read_to_string(&record).unwrap_or_default();
        serde_json::from_str::<serde_json::Value>(&text)
            .map(|r| r["heartbeat_at"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_default()
    };
    let beats_past = |before: &str| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let now = heartbeat();
            if !now.is_empty() && now.as_str() > before {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the heartbeat stopped at {before}"
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    };

    // A FIFO: opened for writing, it would wedge the heartbeat with no reader.
    let made = std::process::Command::new("mkfifo")
        .arg(&planted)
        .status()
        .unwrap();
    assert!(made.success());
    beats_past(&heartbeat());
    beats_past(&heartbeat());
    use std::os::unix::fs::FileTypeExt;
    assert!(
        std::fs::symlink_metadata(&planted)
            .unwrap()
            .file_type()
            .is_fifo()
    );

    // A link to a file elsewhere: followed, the record would be written over it.
    std::fs::remove_file(&planted).unwrap();
    let victim = home.root().join("victim.txt");
    std::fs::write(&victim, "untouched").unwrap();
    std::os::unix::fs::symlink(&victim, &planted).unwrap();
    beats_past(&heartbeat());
    beats_past(&heartbeat());
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
    assert!(
        std::fs::symlink_metadata(&planted)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // No temporary is left beside the record but what was planted.
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert_eq!(
        left,
        [planted.file_name().unwrap().to_string_lossy().into_owned()]
    );
}
