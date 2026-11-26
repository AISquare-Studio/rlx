//! The event bus (handoff 0.10.3, 1.7 and 7.3), through `rlxd events`, `rlxd call
//! events.subscribe` and rlxd-client's `Subscription`, against a real rlxd in a fresh
//! home. The checks are task #22's acceptance, by number. `debug.emit` is here because
//! cargo builds the tests' rlxd with the `debug-methods` feature.

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use common::{TestHome, assert_valid, eventually, finish, kill_hard};
use rlxd_client::proto::{HelloParams, Role, SubscribeParams};
use rlxd_client::{Connection, Home, Incoming, Next};
use serde_json::{Value, json};

fn seqs(events: &[Value]) -> Vec<u64> {
    events.iter().map(|e| e["seq"].as_u64().unwrap()).collect()
}

/// What `jq -s -e '[.[].seq] as $s | $s == ($s|sort|unique)'` checks: no seq repeats,
/// and they come in order.
fn strictly_increasing(seqs: &[u64]) -> bool {
    seqs.windows(2).all(|pair| pair[0] < pair[1])
}

fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

/// A connection to `home`'s rlxd that has said hello as `cli`.
fn connect(home: &TestHome) -> Connection {
    let home = Home::at(home.root());
    let token = std::fs::read_to_string(home.token()).unwrap();
    let mut connection = Connection::open(&home).unwrap();
    let hello = HelloParams::new(token.trim(), "events-test", "0", Role::Cli).unwrap();
    assert!(connection.hello(&hello).unwrap().is_ok());
    connection
}

/// Waits until `status` counts `n` cli connections, its own included.
fn wait_for_cli_clients(home: &TestHome, n: u64) {
    eventually(&format!("{n} cli connections"), || {
        home.status()["clients"]["cli"].as_u64() >= Some(n)
    });
}

#[test]
fn check_1_every_line_is_an_event_the_first_daemon_started_and_status_has_the_last_seq() {
    let home = TestHome::new();
    let started = home.start();
    let run = home.rlxd(&[
        "events", "--topics", "daemon", "--since", "0", "--limit", "1",
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let events = run.lines();
    assert_eq!(events.len(), 1, "{run:?}");
    for event in &events {
        assert_valid("event.json", event);
    }
    assert_eq!(events[0]["type"], "daemon.started");
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[0]["data"]["pid"], started["pid"]);
    assert_eq!(events[0]["data"]["version"], started["version"]);

    let (_, last) = home.emit(5);
    let run = home.rlxd(&["events", "--since", "0"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let events = run.lines();
    assert_valid("event-list.json", &Value::Array(events.clone()));
    assert_eq!(seqs(&events), [1, 2, 3, 4, 5, 6]);
    assert_eq!(
        home.status()["seq"],
        last,
        "status.data.seq is the latest seq"
    );
    assert_eq!(*seqs(&events).last().unwrap(), last);

    // --limit stops it there, replaying or following.
    let three = home.rlxd(&["events", "--since", "0", "--limit", "3"]);
    assert_eq!(three.code(), Some(0), "{three:?}");
    assert_eq!(seqs(&three.lines()), [1, 2, 3]);
    let two = home.rlxd(&[
        "events",
        "--since",
        "0",
        "--follow",
        "--limit",
        "2",
        "--timeout",
        "20",
    ]);
    assert_eq!(two.code(), Some(0), "{two:?}");
    assert_eq!(seqs(&two.lines()), [1, 2]);

    // `rlxd call` prints a stream's events the same way, and exits 0 when it ends.
    let call = home.rlxd(&["call", "events.subscribe", r#"{"since":0,"follow":false}"#]);
    assert_eq!(call.code(), Some(0), "{call:?}");
    assert_eq!(call.lines(), events);
}

#[test]
fn check_2_a_follower_gets_all_50_events_published_while_it_runs_in_order() {
    let home = TestHome::new();
    home.start();
    let since = home.status()["seq"].as_u64().unwrap().to_string();
    let follower = home.spawn(&[
        "events",
        "--topics",
        "daemon",
        "--since",
        &since,
        "--follow",
        "--limit",
        "50",
        "--timeout",
        "30",
    ]);
    wait_for_cli_clients(&home, 2);
    let (first, last) = home.emit(50);
    let run = finish(follower);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let events = run.lines();
    assert_eq!(events.len(), 50, "{run:?}");
    let got = seqs(&events);
    assert!(strictly_increasing(&got), "{got:?}");
    assert_eq!(got, (first..=last).collect::<Vec<_>>());
    let indexes: Vec<u64> = events
        .iter()
        .map(|e| e["data"]["index"].as_u64().unwrap())
        .collect();
    assert_eq!(
        indexes,
        (1..=50).collect::<Vec<_>>(),
        "in the order published"
    );
    assert_valid("event-list.json", &Value::Array(events));
}

#[test]
fn check_3_reconnecting_from_the_last_seq_repeats_no_event() {
    let home = TestHome::new();
    home.start();
    home.emit(5);
    let all = home.rlxd(&["events", "--since", "0"]).lines();
    let last = *seqs(&all).last().unwrap();
    let second_to_last = seqs(&all)[all.len() - 2];

    let again = home.rlxd(&["events", "--since", &last.to_string()]);
    assert_eq!(again.code(), Some(0), "{again:?}");
    assert!(
        again.lines().is_empty(),
        "nothing after the last: {again:?}"
    );
    // The handoff's form: from the second-to-last seq, exactly the last event.
    let one = home
        .rlxd(&["events", "--since", &second_to_last.to_string()])
        .lines();
    assert_eq!(seqs(&one), [last]);

    let (first, newest) = home.emit(3);
    let resumed = home.rlxd(&["events", "--since", &last.to_string()]).lines();
    assert_eq!(seqs(&resumed), (first..=newest).collect::<Vec<_>>());
    assert!(seqs(&resumed).iter().all(|seq| *seq > last));
}

#[test]
fn check_4_after_10001_events_since_0_starts_with_daemon_gap() {
    let home = TestHome::new();
    home.start();
    let (_, last) = home.emit(10_001);
    assert_eq!(last, 10_002, "daemon.started, then the 10,001");
    let run = home.rlxd(&["events", "--since", "0", "--limit", "1"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(run.lines().len(), 1);
    let first = &run.lines()[0];
    assert_valid("event.json", first);
    assert_eq!(first["type"], "daemon.gap");
    assert_eq!(first["data"], json!({"from": 1, "to": 2}));
    assert_eq!(first["seq"], 2, "a gap is numbered its to");

    let all = home.rlxd(&["events", "--since", "0"]).lines();
    assert_eq!(
        all.len(),
        1 + 10_000,
        "the gap, then the 10,000 the buffer holds"
    );
    assert!(strictly_increasing(&seqs(&all)));
    assert_eq!(all[1]["seq"], 3);
    assert_eq!(all.last().unwrap()["seq"], last);
    // From inside the buffer, no gap.
    let inside = home
        .rlxd(&["events", "--since", "2", "--limit", "1"])
        .lines();
    assert_eq!(inside[0]["type"], "daemon.debug");
}

#[test]
fn check_5_a_subscriber_that_stops_reading_does_not_delay_another() {
    let home = TestHome::new();
    home.start();
    let subscribed = home.status()["seq"].as_u64().unwrap();
    let mut stalled = connect(&home)
        .subscribe(&SubscribeParams::new().with_since(subscribed))
        .unwrap();

    // Far more than any socket or pipe buffer holds, and twice what the bus does: the
    // stalled subscriber's writes block, and the buffer drops what it has not read.
    let started = Instant::now();
    let (_, last) = home.emit(20_000);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "publishing waited on the stalled subscriber: {:?}",
        started.elapsed()
    );

    let since = home.status()["seq"].as_u64().unwrap().to_string();
    let follower = home.spawn(&[
        "events",
        "--since",
        &since,
        "--follow",
        "--limit",
        "1",
        "--timeout",
        "20",
    ]);
    wait_for_cli_clients(&home, 3);
    let (seq, _) = home.emit(1);
    let published = Instant::now();
    let run = finish(follower);
    let waited = published.elapsed();
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(seqs(&run.lines()), [seq]);
    assert!(
        waited < Duration::from_secs(1),
        "the second subscriber waited {waited:?}"
    );

    // The stalled one, reading at last: what was in flight, then a gap for what the
    // buffer dropped meanwhile, then the rest, every seq in order.
    let mut got = Vec::new();
    let mut gaps = Vec::new();
    while got.last() != Some(&seq) {
        match stalled.next_frame().unwrap() {
            Next::Event(event) => {
                if event.event.r#type == "daemon.gap" {
                    gaps.push(event.event.data.clone());
                }
                got.push(event.event.seq);
            }
            Next::End(reply) => panic!("the stream ended: {}", reply.line),
        }
    }
    assert!(strictly_increasing(&got), "{got:?}");
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    let (from, to) = (gaps[0]["from"].as_u64(), gaps[0]["to"].as_u64());
    let (from, to) = (from.unwrap(), to.unwrap());
    assert!(subscribed < from && from <= to && to < last, "{gaps:?}");
    // Every seq after the subscribe's but the dropped ones, plus the gap, each once.
    let dropped = to - from + 1;
    assert_eq!(got.len() as u64, seq - subscribed - dropped + 1);
}

#[test]
fn check_6_until_with_no_such_event_exits_4_at_the_timeout_and_with_rlxd_stopped_3() {
    let home = TestHome::new();
    home.start();
    let started = Instant::now();
    let run = home.rlxd(&["events", "--until", "daemon.never", "--timeout", "2"]);
    let took = started.elapsed();
    assert_eq!(run.code(), Some(4), "{run:?}");
    assert_eq!(run.stderr_code(), "timed_out");
    assert!(run.stdout.is_empty(), "stdout carries events only: {run:?}");
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(20),
        "{took:?}"
    );

    // And once the type comes, 0.
    let since = home.status()["seq"].as_u64().unwrap().to_string();
    let waiting = home.spawn(&[
        "events",
        "--since",
        &since,
        "--until",
        "daemon.debug",
        "--timeout",
        "20",
    ]);
    home.emit(1);
    let run = finish(waiting);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(types(&run.lines()), ["daemon.debug"]);

    assert_eq!(home.rlxd(&["stop"]).code(), Some(0));
    for args in [&["events"][..], &["events", "--follow", "--since", "0"]] {
        let run = home.rlxd(args);
        assert_eq!(run.code(), Some(3), "{run:?}");
        assert_eq!(run.stderr_code(), "not_running");
        assert!(run.stdout.is_empty(), "{run:?}");
    }
}

#[test]
fn a_follower_sees_daemon_stopping_then_its_stream_ends() {
    let home = TestHome::new();
    home.start();
    let since = home.status()["seq"].as_u64().unwrap().to_string();
    let until = home.spawn(&[
        "events",
        "--topics",
        "daemon",
        "--since",
        &since,
        "--until",
        "daemon.stopping",
        "--timeout",
        "30",
    ]);
    let call = home.spawn(&["call", "events.subscribe", r#"{"topics":["daemon"]}"#]);
    wait_for_cli_clients(&home, 3);
    assert_eq!(home.rlxd(&["stop"]).code(), Some(0));

    let run = finish(until);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let stopping = run.lines().pop().unwrap();
    assert_eq!(stopping["type"], "daemon.stopping");
    assert_eq!(stopping["data"]["reason"], "stop");
    // `rlxd call` prints the stream to its end, the reply, and exits 0.
    let run = finish(call);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(types(&run.lines()), ["daemon.stopping"]);
}

#[test]
fn the_numbering_goes_on_across_a_restart_and_daemon_started_is_there() {
    let home = TestHome::new();
    home.start();
    home.emit(2);
    assert_eq!(home.rlxd(&["stop"]).code(), Some(0));
    let restarted = home.start();
    // The handoff's check: after `rlxd stop && rlxd serve --detach`.
    let run = home.rlxd(&["events", "--topics", "daemon", "--since", "0"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let events = run.lines();
    let started: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "daemon.started")
        .collect();
    assert!(!started.is_empty(), "{events:?}");
    assert!(started.last().unwrap()["data"]["pid"].is_number());
    assert_eq!(started.last().unwrap()["data"]["pid"], restarted["pid"]);
    // 1 started, 2 and 3 debug, 4 stopping; the new rlxd numbers on from 5, and holds
    // none of the last one's.
    assert_eq!(types(&events), ["daemon.gap", "daemon.started"]);
    assert_eq!(events[0]["data"], json!({"from": 1, "to": 4}));
    assert_eq!(seqs(&events), [4, 5]);
    assert_eq!(home.status()["seq"], 5);
    // A client that saw the last one's every event resumes with no gap.
    let resumed = home.rlxd(&["events", "--since", "4"]).lines();
    assert_eq!(types(&resumed), ["daemon.started"]);
}

#[test]
fn after_a_kill_the_next_rlxd_numbers_past_what_the_last_reserved() {
    let home = TestHome::new();
    let pid = home.start()["pid"].as_u64().unwrap();
    // Past the first reservation, so the killed rlxd had to reserve again.
    let (_, last) = home.emit(rlxd::bus::RESERVE + 500);
    kill_hard(pid);
    eventually("the killed rlxd's lock is free", || {
        home.rlxd(&["status", "--json"]).code() == Some(3)
    });
    home.start();
    let seq = home.status()["seq"].as_u64().unwrap();
    assert!(seq > last, "{seq} repeats a seq the killed rlxd gave out");
    let resumed = home.rlxd(&["events", "--since", &last.to_string()]).lines();
    assert_eq!(types(&resumed), ["daemon.gap", "daemon.started"]);
    assert_eq!(resumed[0]["data"]["from"], last + 1);
    assert_eq!(resumed[1]["seq"], seq);
}

#[test]
fn subscribe_refuses_unknown_topics_and_params_it_cannot_take() {
    let home = TestHome::new();
    home.start();
    let run = home.rlxd(&["events", "--topics", "run,nope"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.stderr_code(), "unknown_topic");
    assert!(run.stdout.is_empty());
    for (params, code) in [
        (r#"{"topics":["nope"]}"#, "unknown_topic"),
        (r#"{"topics":["Run"]}"#, "unknown_topic"),
        (r#"{"topics":[]}"#, "invalid_params"),
        (r#"{"topics":"daemon"}"#, "invalid_params"),
        (r#"{"since":-1}"#, "invalid_params"),
        (r#"{"since":99}"#, "since_ahead"),
        (r#"{"follow":"no"}"#, "invalid_params"),
    ] {
        let run = home.rlxd(&["call", "events.subscribe", params]);
        assert_eq!(run.code(), Some(1), "{params}: {run:?}");
        assert_eq!(run.error_code(), code, "{params}");
        assert_valid("envelope.json", &run.json());
        assert!(!run.json()["error"]["message"].as_str().unwrap().is_empty());
        if code == "since_ahead" {
            assert_eq!(
                run.json()["error"]["detail"],
                json!({"since": 99, "head": 1})
            );
        }
    }
    for params in [r#"{}"#, r#"{"count":0}"#, r#"{"count":"1"}"#] {
        let run = home.rlxd(&["call", "debug.emit", params]);
        assert_eq!(run.error_code(), "invalid_params", "{params}");
    }
    for args in [
        &["events", "--topics", "run,"][..],
        &["events", "--timeout", "0"],
    ] {
        assert_eq!(home.rlxd(args).code(), Some(2), "{args:?}");
    }
}

#[test]
fn as_app_streams_with_the_test_clients_gate_credential_and_is_refused_without() {
    let home = TestHome::new();
    home.start();
    let run = home.rlxd(&["events", "--as-app", "--since", "0"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.stderr_code(), "not_gate");
    assert!(run.stdout.is_empty());
    home.stop();

    // `rlxd events --as-app` never pairs: it presents the credential that `rlxd call
    // --as-app` kept when it paired, in gate dev mode.
    let started = home.start_dev();
    let run = home.rlxd(&["events", "--as-app", "--since", "0"]);
    assert_eq!(run.stderr_code(), "not_gate", "{run:?}");
    let run = home.rlxd(&["call", "--as-app", "status"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let run = home.rlxd(&["events", "--as-app", "--since", "0"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let events = run.lines();
    assert_valid("event-list.json", &Value::Array(events.clone()));
    let last = events.last().expect("this rlxd's daemon.started");
    assert_eq!(last["type"], "daemon.started", "{events:?}");
    assert_eq!(last["data"]["pid"], started["pid"]);
}

#[test]
fn a_connection_answers_requests_while_it_streams_and_takes_one_stream_only() {
    let home = TestHome::new();
    home.start();
    let mut connection = connect(&home);
    let subscribe = connection
        .send(
            "events.subscribe",
            json!({"topics": ["daemon"]}).as_object().unwrap().clone(),
        )
        .unwrap();
    let status = connection.send("status", Default::default()).unwrap();
    let second = connection
        .send("events.subscribe", Default::default())
        .unwrap();
    // Refused for being a second stream, before its params are read.
    let bad_second = connection
        .send(
            "events.subscribe",
            json!({"topics": ["nope"]}).as_object().unwrap().clone(),
        )
        .unwrap();
    let mut replies = Vec::new();
    while replies.len() < 3 {
        match connection.read_incoming().unwrap() {
            Incoming::Reply(reply) => replies.push(reply),
            Incoming::Event(event) => panic!("no event yet: {}", event.line),
        }
    }
    assert_eq!(replies[0].envelope.id, Some(status));
    assert!(replies[0].is_ok(), "{}", replies[0].line);
    assert_eq!(replies[1].envelope.id, Some(second));
    assert_eq!(replies[1].error().unwrap().code, "invalid_request");
    assert_eq!(replies[2].envelope.id, Some(bad_second));
    assert_eq!(replies[2].error().unwrap().code, "invalid_request");

    let (seq, _) = home.emit(1);
    match connection.read_incoming().unwrap() {
        Incoming::Event(event) => assert_eq!(event.event.seq, seq),
        Incoming::Reply(reply) => panic!("an event, not {}", reply.line),
    }
    assert_ne!(subscribe, status);
}

#[test]
fn after_a_stream_without_follow_the_connection_takes_requests_again() {
    let home = TestHome::new();
    home.start();
    home.emit(3);
    let params = SubscribeParams::new().with_since(0).with_follow(false);
    let mut stream = connect(&home).subscribe(&params).unwrap();
    let mut got = Vec::new();
    let end = loop {
        match stream.next_frame().unwrap() {
            Next::Event(event) => got.push(event.event.seq),
            Next::End(reply) => break reply,
        }
    };
    assert_eq!(got, [1, 2, 3, 4]);
    assert!(end.is_ok(), "{}", end.line);
    assert_eq!(end.data().unwrap()["seq"], 4, "where to resume from");
    assert_valid("envelope.json", &serde_json::from_str(&end.line).unwrap());
    let mut connection = stream.into_connection();
    let status = connection.request("status", Default::default()).unwrap();
    assert!(status.is_ok(), "{}", status.line);
}

#[test]
fn a_stream_with_some_topics_carries_those_only() {
    let home = TestHome::new();
    home.start();
    home.emit(2);
    let run = home.rlxd(&["events", "--topics", "run,pty", "--since", "0"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert!(run.lines().is_empty(), "no run or pty events yet: {run:?}");
    let daemon: BTreeSet<String> = home
        .rlxd(&["events", "--topics", "daemon,run", "--since", "0"])
        .lines()
        .iter()
        .map(|e| e["topic"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(daemon, BTreeSet::from(["daemon".to_owned()]));
}

#[test]
fn with_no_since_and_no_follow_rlxd_events_prints_nothing_and_exits_0() {
    let home = TestHome::new();
    home.start();
    home.emit(2);
    let run = home.rlxd(&["events"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert!(run.stdout.is_empty(), "{run:?}");
}

#[test]
fn a_stream_without_follow_ends_where_the_stream_was_when_it_was_asked_for() {
    let home = TestHome::new();
    home.start();
    // More than any socket or pipe buffer holds, so rlxd is still writing the replay
    // when more events come.
    let (_, head) = home.emit(9_000);
    let params = SubscribeParams::new().with_since(0).with_follow(false);
    let mut stream = connect(&home).subscribe(&params).unwrap();
    let first = match stream.next_frame().unwrap() {
        Next::Event(event) => event.event.seq,
        Next::End(reply) => panic!("an event first, not {}", reply.line),
    };
    assert_eq!(first, 1, "rlxd took the subscribe");
    home.emit(100);
    let mut last = first;
    let end = loop {
        match stream.next_frame().unwrap() {
            Next::Event(event) => {
                assert!(event.event.seq > last, "in order");
                last = event.event.seq;
            }
            Next::End(reply) => break reply,
        }
    };
    assert_eq!(
        last, head,
        "the replay ends at the head when it was asked for"
    );
    assert_eq!(end.data().unwrap()["seq"], head);
}

/// A connection written byte by byte, for frames rlxd-client would not send. It is
/// tokio's, whose socket and pipe client can read and write at once (a synchronous
/// Windows pipe cannot), and every read has a time limit, so a line that never comes
/// fails the test rather than hang it.
struct Raw {
    runtime: tokio::runtime::Runtime,
    reader: tokio::io::BufReader<tokio::io::ReadHalf<RawStream>>,
    writer: tokio::io::WriteHalf<RawStream>,
}

#[cfg(unix)]
type RawStream = tokio::net::UnixStream;
#[cfg(windows)]
type RawStream = tokio::net::windows::named_pipe::NamedPipeClient;

impl Raw {
    fn open(home: &TestHome) -> Self {
        let home = Home::at(home.root());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        #[cfg(unix)]
        let stream = runtime.block_on(RawStream::connect(home.socket())).unwrap();
        #[cfg(windows)]
        let stream = runtime
            .block_on(async {
                tokio::net::windows::named_pipe::ClientOptions::new().open(home.pipe_name())
            })
            .unwrap();
        let (reader, writer) = tokio::io::split(stream);
        let mut raw = Self {
            runtime,
            reader: tokio::io::BufReader::new(reader),
            writer,
        };
        let token = std::fs::read_to_string(home.token()).unwrap();
        let hello = json!({"id": 1, "method": "hello", "params": {"token": token.trim(),
            "protocol_version": 1, "client": "raw", "client_version": "0", "role": "cli",
            "capabilities": []}});
        raw.write(&format!("{hello}\n"));
        assert_eq!(raw.next()["ok"], true);
        raw
    }

    fn write(&mut self, text: &str) {
        use tokio::io::AsyncWriteExt;
        let Self {
            runtime, writer, ..
        } = self;
        runtime.block_on(async {
            writer.write_all(text.as_bytes()).await.unwrap();
            writer.flush().await.unwrap();
        });
    }

    /// The next line rlxd sends, within 10 s.
    fn next(&mut self) -> Value {
        use tokio::io::AsyncBufReadExt;
        let Self {
            runtime, reader, ..
        } = self;
        let mut line = String::new();
        let read = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line)).await
        });
        let length = read.expect("rlxd sent a line within 10 s").unwrap();
        assert!(length > 0, "rlxd closed the connection");
        serde_json::from_str(&line).unwrap()
    }
}

#[test]
fn a_partial_frame_holds_up_no_event_on_its_connection() {
    let home = TestHome::new();
    home.start();
    let mut raw = Raw::open(&home);
    raw.write("{\"id\":2,\"method\":\"events.subscribe\",\"params\":{\"topics\":[\"daemon\"]}}\n");
    raw.write("{\"id\":3,\"method\":\"sta");
    let (seq, _) = home.emit(1);
    let event = raw.next();
    assert_eq!(
        event["seq"], seq,
        "the event came while the frame was half sent"
    );
    raw.write("tus\"}\n");
    let reply = raw.next();
    assert_eq!(reply["id"], 3);
    assert_eq!(reply["ok"], true, "{reply}");
    let (seq, _) = home.emit(1);
    assert_eq!(raw.next()["seq"], seq, "and the stream goes on");
}
