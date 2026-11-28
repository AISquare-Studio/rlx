//! The resident Python brain rlxd supervises (handoff 0.10.3, 1.6): `status.brain`, its
//! restart after a kill, its backoff when it keeps dying, a brain of another release, its
//! record in the registry, and that no brain outlives rlxd's stop.
//!
//! The checks of a brain that works run the checkout's own `rlx brain --stdio`
//! ([`common::real_rlx`]); they skip where there is none, unless `RLXD_INTEROP_REQUIRED`
//! is set, as rlxd.yml sets it. The others run `tests/fixtures/fake-brain`.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use common::{TestHome, alive, assert_valid, eventually, fake_brain, kill_hard, real_rlx};
use rlxd_client::Home;
use rlxd_proto::daemon_kinds;
use serde_json::Value;

/// `status.brain`.
fn brain(home: &TestHome) -> Value {
    home.status()["brain"].clone()
}

/// The brain's pid, once one runs.
fn brain_pid(home: &TestHome) -> u64 {
    let mut pid = 0;
    eventually("a brain runs", || {
        pid = brain(home)["pid"].as_u64().unwrap_or(0);
        pid > 0
    });
    pid
}

/// Every pid the log says a brain ran as: the program rlxd started, and the Python it
/// said its hello from (the same on POSIX; on Windows `rlx.exe` starts Python).
fn logged_brain_pids(home: &TestHome) -> Vec<u64> {
    home.log()
        .lines()
        .filter_map(|line| line.split_once("brain: pid ").map(|(_, rest)| rest))
        .flat_map(|rest| {
            let words: Vec<&str> = rest.split([' ', '(', ')', ',']).collect();
            let started = words.first().and_then(|w| w.parse().ok());
            let python = words
                .iter()
                .position(|w| *w == "python")
                .and_then(|at| words.get(at + 1))
                .and_then(|w| w.parse().ok());
            [started, python].into_iter().flatten()
        })
        .collect()
}

#[test]
fn status_names_a_running_brain_and_it_answers_through_its_queue() {
    let Some(rlx) = real_rlx() else { return };
    let home = TestHome::new().with_rlx(rlx);
    // `serve --detach` returns once the brain runs, so the next status names it (1.6).
    let started = home.start();
    let pid = started["brain"]["pid"].as_u64().unwrap_or(0);
    assert!(pid > 0, "{started}");
    let status = home.status();
    assert_valid("status-reply.json", &status);
    assert_eq!(status["brain"]["pid"].as_u64(), Some(pid), "{status}");
    assert_eq!(status["brain"]["restarts"], 0, "{status}");
    assert!(status["brain"].get("error").is_none(), "{status}");
    for _ in 0..20 {
        let ping = home.rlxd(&["call", "hook.ping"]);
        assert_eq!(ping.code(), Some(0), "{ping:?}");
    }
    let after = brain(&home);
    assert_eq!(after["pid"].as_u64(), Some(pid), "the same brain: {after}");
    assert!(after["handled"].as_u64().unwrap() >= 20, "{after}");
    assert_eq!(after["restarts"], 0, "{after}");
}

#[test]
fn a_killed_brain_is_started_again_within_five_seconds() {
    let Some(rlx) = real_rlx() else { return };
    let home = TestHome::new().with_rlx(rlx);
    home.start();
    let first = brain_pid(&home);
    // `kill -9`, or `taskkill /F` on Windows.
    kill_hard(first);
    let killed = Instant::now();
    let mut now = Value::Null;
    while killed.elapsed() < Duration::from_secs(5) {
        now = brain(&home);
        if now["restarts"] == 1 && now["pid"].as_u64().is_some_and(|pid| pid != first) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(now["restarts"], 1, "{now}");
    let second = now["pid"].as_u64().unwrap_or(0);
    assert!(second > 0 && second != first, "{now}");
    let ping = home.rlxd(&["call", "hook.ping"]);
    assert_eq!(ping.code(), Some(0), "{ping:?}");
    assert!(home.log().contains("brain_exited"), "{}", home.log());
}

#[test]
fn the_brain_is_registered_while_it_runs_and_stop_leaves_no_brain() {
    let Some(rlx) = real_rlx() else { return };
    let home = TestHome::new().with_rlx(rlx.clone());
    home.start();
    let pid = brain_pid(&home);
    let roll = rlxd::registry::read(&Home::at(home.root())).unwrap();
    let brains: Vec<_> = roll
        .live
        .iter()
        .filter(|record| record.kind == daemon_kinds::BRAIN)
        .collect();
    assert_eq!(brains.len(), 1, "{roll:?}");
    assert_eq!(u64::from(brains[0].pid), pid);
    assert_eq!(
        brains[0].argv,
        [rlx.display().to_string(), "brain".into(), "--stdio".into()]
    );
    assert!(
        !brains[0].restartable,
        "rlxd restarts its brain, not an upgrade"
    );
    let record = home
        .root()
        .join("daemons")
        .join(format!("{}.json", brains[0].id));
    let text = std::fs::read_to_string(&record).unwrap();
    assert_valid("daemon-record.json", &serde_json::from_str(&text).unwrap());

    let pids = logged_brain_pids(&home);
    assert!(pids.contains(&pid), "{pids:?} in {}", home.log());
    home.stop();
    for pid in pids {
        assert!(!alive(pid), "brain process {pid} outlived rlxd's stop");
    }
    let roll = rlxd::registry::read(&Home::at(home.root())).unwrap();
    assert!(
        !roll
            .live
            .iter()
            .chain(&roll.dead)
            .any(|record| record.kind == daemon_kinds::BRAIN),
        "{roll:?}"
    );
    assert!(home.log().contains("brain: stopped"), "{}", home.log());
}

#[test]
fn a_brain_that_dies_at_start_is_started_again_with_backoff() {
    let home = TestHome::new().with_rlx(fake_brain("dies"));
    let started = home.start();
    assert_eq!(
        started["brain"]["error"]["code"], "brain_exited",
        "{started}"
    );
    thread::sleep(Duration::from_millis(2500));
    let now = brain(&home);
    assert_valid("status-reply.json", &home.status());
    assert!(now.get("pid").is_none(), "{now}");
    assert_eq!(now["error"]["code"], "brain_exited", "{now}");
    // Waits of 250, 500, 1000 and 2000 ms: about 3 starts again in 2.5 s, never a spin.
    let restarts = now["restarts"].as_u64().unwrap();
    assert!((2..=4).contains(&restarts), "{now}");
    let waiting = now["backoff_ms"].as_u64();
    assert!(waiting.is_some_and(|ms| ms >= 500), "{now}");
    // A request meanwhile is refused as never delivered, not left to hang.
    let ping = home.rlxd(&["call", "hook.ping"]);
    assert_eq!(ping.code(), Some(1), "{ping:?}");
    assert!(ping.error_code().starts_with("brain_"), "{ping:?}");
    let log = std::fs::read_to_string(home.root().join("rlxd/logs/brain.log")).unwrap();
    assert!(log.contains("fake-brain/dies: dying at start"), "{log}");
}

#[test]
fn a_brain_of_another_release_is_named_not_misread() {
    let home = TestHome::new().with_rlx(fake_brain("skewed"));
    let started = home.start();
    let brain = &started["brain"];
    assert_eq!(brain["error"]["code"], "brain_incompatible", "{started}");
    assert!(brain.get("pid").is_none(), "{started}");
    let message = brain["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("protocol 99") && message.contains("9.9.9"),
        "{message}"
    );
    assert_eq!(brain["error"]["detail"]["rlxd"], serde_json::json!([1]));
    let ping = home.rlxd(&["call", "hook.ping"]);
    assert_eq!(ping.error_code(), "brain_incompatible", "{ping:?}");
}

#[test]
fn with_no_rlx_status_says_so_and_requests_are_refused_at_once() {
    let home = TestHome::new();
    let started = home.start();
    assert_eq!(
        started["brain"]["error"]["code"], "brain_not_found",
        "{started}"
    );
    assert_eq!(started["brain"]["restarts"], 0);
    let asked = Instant::now();
    let ping = home.rlxd(&["call", "hook.ping"]);
    assert_eq!(ping.error_code(), "brain_not_found", "{ping:?}");
    assert!(asked.elapsed() < Duration::from_secs(3), "it did not wait");
}

#[test]
fn a_request_the_brain_never_accepted_was_never_delivered() {
    let home = TestHome::new().with_rlx(fake_brain("mute"));
    home.start();
    let first = brain_pid(&home);
    let asked = Instant::now();
    let ping = home.rlxd(&["call", "hook.ping"]);
    // 2 s for the brain to accept, then killed and reaped before the answer.
    assert!(asked.elapsed() >= Duration::from_secs(2), "{ping:?}");
    assert_eq!(ping.error_code(), "brain_unresponsive", "{ping:?}");
    let message = ping.json()["error"]["message"].as_str().unwrap().to_owned();
    assert!(message.contains("did not accept"), "{message}");
    assert!(!alive(first), "the mute brain was killed before the answer");
    eventually("the brain is started again", || {
        let now = brain(&home);
        now["restarts"] == 1 && now["pid"].as_u64().is_some_and(|pid| pid != first)
    });
}

/// The other side of the accept wait: an `accepted` the brain wrote, still unread in the
/// pipe when the wait ends, is read once the brain is killed, so the request timed out and
/// was never "not delivered" (review #2555 M1, tester #2645). The fake brain writes it
/// without the line's end and stalls, which holds it there until the kill.
#[test]
fn an_accepted_left_in_the_pipe_at_the_accept_wait_is_read_after_the_kill() {
    let home = TestHome::new().with_rlx(fake_brain("accepts-unterminated"));
    home.start();
    let first = brain_pid(&home);
    let asked = Instant::now();
    let ping = home.rlxd(&["call", "hook.ping"]);
    assert!(asked.elapsed() >= Duration::from_secs(2), "{ping:?}");
    assert_eq!(
        ping.error_code(),
        "timed_out",
        "it may have run, so it was delivered: {ping:?}"
    );
    assert!(
        !alive(first),
        "the stalled brain was killed before the answer"
    );
}

#[test]
fn a_request_the_brain_accepted_then_died_on_failed_rather_than_never_ran() {
    let home = TestHome::new().with_rlx(fake_brain("accepts-then-dies"));
    home.start();
    brain_pid(&home);
    let ping = home.rlxd(&["call", "hook.ping"]);
    assert_eq!(ping.error_code(), "brain_exited", "{ping:?}");
    let message = ping.json()["error"]["message"].as_str().unwrap().to_owned();
    assert!(message.contains("ended running it"), "{message}");
    assert_eq!(brain(&home)["handled"], 0, "no reply came");
}

#[test]
fn hook_ping_is_for_command_line_clients_only() {
    let home = TestHome::new();
    home.start();
    for role in ["tui", "remote"] {
        let refused = home.rlxd(&["call", "--role", role, "--client", "probe", "hook.ping"]);
        assert_eq!(refused.error_code(), "role_not_allowed", "{refused:?}");
        let detail = &refused.json()["error"]["detail"];
        assert_eq!(detail["role"], role, "{refused:?}");
        assert_eq!(detail["method"], "hook.ping", "{refused:?}");
    }
    // A cli client gets through to the brain, which this home has none of.
    let cli = home.rlxd(&["call", "hook.ping"]);
    assert_eq!(cli.error_code(), "brain_not_found", "{cli:?}");
}

#[test]
fn the_brains_queue_is_bounded_and_status_shows_its_depth() {
    let home = std::sync::Arc::new(TestHome::new().with_rlx(fake_brain("mute")));
    home.start();
    brain_pid(&home);
    // The mute brain takes one ping and never accepts it: the rest wait, 64 at most.
    let callers: Vec<_> = (0..100)
        .map(|_| {
            let home = std::sync::Arc::clone(&home);
            thread::spawn(move || {
                let asked = Instant::now();
                let ping = home.rlxd(&["call", "hook.ping"]);
                (asked.elapsed(), ping)
            })
        })
        .collect();
    let mut deepest = 0;
    let watched = Instant::now();
    while watched.elapsed() < Duration::from_millis(1500) {
        deepest = deepest.max(brain(&home)["queued"].as_u64().unwrap_or(0));
        thread::sleep(Duration::from_millis(50));
    }
    let answers: Vec<_> = callers.into_iter().map(|c| c.join().unwrap()).collect();
    assert!(
        (16..=64).contains(&deepest),
        "status said {deepest} waiting"
    );
    let full: Vec<_> = answers
        .iter()
        .filter(|(_, ping)| ping.stdout.contains("queue already holds 64"))
        .collect();
    assert!(!full.is_empty(), "none refused for a full queue");
    for (took, ping) in &full {
        assert_eq!(ping.error_code(), "brain_unresponsive", "{ping:?}");
        assert!(
            *took < Duration::from_secs(2),
            "refused at once, not after {took:?}"
        );
    }
    assert!(answers.iter().all(|(_, ping)| ping.code() == Some(1)));
    eventually("the queue empties", || brain(&home)["queued"] == 0);
}

#[test]
fn stop_ends_what_the_brain_started_too() {
    let home = TestHome::new().with_rlx(fake_brain("spawns-child"));
    home.start();
    brain_pid(&home);
    let file = home.root().join("fake-brain-child.pid");
    let mut child = 0;
    eventually("the brain's child is started", || {
        child = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0);
        child > 0
    });
    assert!(alive(child), "the child {child} runs");
    home.stop();
    eventually("the brain's child is gone with it", || !alive(child));
}

#[test]
fn a_brain_ends_when_rlxd_is_killed() {
    let Some(rlx) = real_rlx() else { return };
    let home = TestHome::new().with_rlx(rlx);
    let started = home.start();
    let rlxd = started["pid"].as_u64().unwrap();
    brain_pid(&home);
    let pids = logged_brain_pids(&home);
    assert!(!pids.is_empty(), "{}", home.log());
    // POSIX: the brain reads its stdin's end and exits. Windows: rlxd's end closes the
    // brain's job.
    kill_hard(rlxd);
    for pid in pids {
        eventually("the brain exits after rlxd is killed", || !alive(pid));
    }
    let roll = rlxd::registry::read(&Home::at(home.root())).unwrap();
    assert!(
        !roll
            .live
            .iter()
            .any(|record| record.kind == daemon_kinds::BRAIN),
        "{roll:?}"
    );
}

/// `hook.ping`'s answer, which must come: from fake-brain/print-env, its pid and the
/// `CLAUDE_CONFIG_DIR` it was started with.
fn ping(home: &TestHome) -> Value {
    let ping = home.rlxd(&["call", "hook.ping"]);
    assert_eq!(ping.code(), Some(0), "{ping:?}");
    ping.data()
}

/// `env.set` from the app.
fn set_env(home: &TestHome, change: &str) {
    let set = home.rlxd(&["call", "--as-app", "env.set", change]);
    assert_eq!(set.code(), Some(0), "{set:?}");
}

/// 4.3: the brain runs with the app's environment, as a run's Claude Code does. An
/// `env.set` that changes it starts the brain again with the new values once it is idle:
/// a request it runs ends first, answered by the brain it started on. One that changes
/// nothing leaves it running, and env.json's values reach the brain of the next rlxd.
#[test]
fn an_env_set_reaches_the_brain_once_no_request_runs() {
    let home = TestHome::new();
    let own = home.root().join("claude-config").display().to_string();
    let home = std::sync::Arc::new(
        home.with_env("CLAUDE_CONFIG_DIR", &own)
            .with_rlx(fake_brain("print-env")),
    );
    home.start_dev();
    let first = ping(&home);
    assert_eq!(
        first["CLAUDE_CONFIG_DIR"],
        own.as_str(),
        "rlxd's own, as the app set none: {first}"
    );

    // The app's environment changes while the brain runs a request.
    let hold = home.root().join("fake-brain-hold");
    let busy = home.root().join("fake-brain-busy");
    std::fs::write(&hold, "").unwrap();
    let _ = std::fs::remove_file(&busy);
    let running = {
        let home = std::sync::Arc::clone(&home);
        thread::spawn(move || ping(&home))
    };
    eventually("the brain runs the request", || busy.exists());
    set_env(&home, r#"{"CLAUDE_CONFIG_DIR":"cc-brain"}"#);
    thread::sleep(Duration::from_millis(500));
    let held = brain(&home);
    assert_eq!(held["restarts"], 0, "not while it runs a request: {held}");
    std::fs::remove_file(&hold).unwrap();
    let ran = running.join().unwrap();
    assert_eq!(ran["pid"], first["pid"], "{ran}");
    assert_eq!(
        ran["CLAUDE_CONFIG_DIR"],
        own.as_str(),
        "the values it started with: {ran}"
    );

    // Then it is started again, with no backoff and no error, and runs with the app's.
    eventually("the brain is started again", || {
        brain(&home)["restarts"] == 1
    });
    let now = brain(&home);
    assert!(
        now.get("error").is_none() && now.get("backoff_ms").is_none(),
        "{now}"
    );
    let second = ping(&home);
    assert_ne!(second["pid"], first["pid"], "{second}");
    assert_eq!(second["CLAUDE_CONFIG_DIR"], "cc-brain", "{second}");
    assert!(
        home.log().contains("the app's environment changed"),
        "{}",
        home.log()
    );

    // The same values again: no change, no restart.
    set_env(&home, r#"{"CLAUDE_CONFIG_DIR":"cc-brain"}"#);
    thread::sleep(Duration::from_millis(500));
    assert_eq!(ping(&home)["pid"], second["pid"]);
    assert_eq!(brain(&home)["restarts"], 1);

    // A later rlxd's brain starts with env.json's values.
    home.stop();
    home.start_dev();
    let third = ping(&home);
    assert_eq!(third["CLAUDE_CONFIG_DIR"], "cc-brain", "{third}");
}
