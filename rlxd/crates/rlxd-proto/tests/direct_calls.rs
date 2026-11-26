//! Each wire type's `deserialize` and `serialize`, called by path from outside the crate,
//! as rlxd and rlxd-client would call them: `Event::deserialize(…)` rather than through
//! `serde_json::from_value`.
//!
//! Both must run the type's `validate()`. serde's `remote = "Self"` would make unchecked
//! inherent `deserialize` and `serialize` public on the type, and an inherent function
//! wins a `Type::f` path over the trait's. These tests fail if one comes back.

use rlxd_proto::{
    DaemonRecord, Data, Envelope, ErrorBody, Event, HelloData, HelloParams, Paths, Request,
    RequestId, Role, RunScope, StatusData,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const TIME: &str = "2026-11-25T12:00:00.000Z";

/// What `value` gives `T::serialize`, called by path.
fn printed<T: Serialize>(value: &T) -> Result<Value, serde_json::Error> {
    T::serialize(value, serde_json::value::Serializer)
}

#[test]
fn deserialize_called_by_path_refuses_what_the_schema_refuses() {
    let unscoped = json!({"schema_version": 1, "seq": 1, "time": TIME, "topic": "run",
        "type": "run.text", "data": {}});
    assert!(Event::deserialize(unscoped).is_err());
    let wrong_type = json!({"schema_version": 1, "seq": 1, "time": TIME, "topic": "daemon",
        "type": "run.text", "data": {}});
    assert!(Event::deserialize(wrong_type).is_err());
    let leap = r#"{"schema_version": 1, "seq": 1, "time": "2026-11-25T12:00:60Z",
        "topic": "daemon", "type": "daemon.started", "data": {}}"#;
    assert!(Event::deserialize(&mut serde_json::Deserializer::from_str(leap)).is_err());

    assert!(ErrorBody::deserialize(json!({"code": "", "message": "m"})).is_err());
    assert!(
        ErrorBody::deserialize(json!({"code": "incompatible_protocol", "message": "m"})).is_err()
    );
    let empty_token = json!({"token": "", "protocol_version": 1, "client": "c",
        "client_version": "1", "role": "cli", "capabilities": []});
    assert!(HelloParams::deserialize(empty_token).is_err());
    let no_versions = json!({"protocol_version": 1, "protocol_versions": [],
        "rlxd_version": "0.10.1", "capabilities": []});
    assert!(HelloData::deserialize(no_versions).is_err());
    assert!(Request::deserialize(json!({"id": 1, "method": ""})).is_err());
    let pid_zero = json!({"id": "0123456789ab", "kind": "watch", "pid": 0, "host": "h",
        "version": "v", "started_at": TIME, "heartbeat_at": TIME, "argv": ["rlx"],
        "restartable": false});
    assert!(DaemonRecord::deserialize(pid_zero).is_err());
    let empty_code = json!({"schema_version": 1, "ok": false,
        "error": {"code": "", "message": "m"}});
    assert!(Envelope::deserialize(empty_code).is_err());
    let paths = json!({"home": "h", "socket": "s", "lock": "l", "token": "t", "logs": "g"});
    let status = json!({"version": "", "pid": 1, "platform": "linux", "protocol_version": 1,
        "capabilities": [], "seq": 0, "clients": {"app": 0, "tui": 0, "remote": 0, "cli": 0},
        "gate": "g", "brain": {"restarts": 0, "handled": 0},
        "claude": {"installed": false, "signed_in": false}, "runs": {"live": 0, "queued": 0},
        "jobs": {"count": 0}, "paths": paths});
    assert!(StatusData::deserialize(status).is_err());

    // And they still read what is valid.
    assert!(Request::deserialize(json!({"id": 1, "method": "status"})).is_ok());
}

#[test]
fn serialize_called_by_path_does_not_print_an_invalid_value() {
    let mut event = Event::new(1, TIME, "daemon", "daemon.started", Data::new()).unwrap();
    assert!(printed(&event).is_ok());
    event.r#type = "run.text".to_owned();
    assert!(printed(&event).is_err());

    let scope = RunScope::new("r", 1, "task", 1);
    let mut run = Event::for_run(2, TIME, "run.text", scope, Data::new()).unwrap();
    run.data.remove("attempt");
    assert!(printed(&run).is_err());

    let mut error = ErrorBody::new("c", "m").unwrap();
    assert!(printed(&error).is_ok());
    error.message.clear();
    assert!(printed(&error).is_err());
    assert!(printed(&Envelope::err(error)).is_err());

    let mut hello = HelloParams::new("t", "c", "1", Role::Cli).unwrap();
    assert!(printed(&hello).is_ok());
    hello.protocol_version = 0;
    assert!(printed(&hello).is_err());

    let mut reply = HelloData::new(1, "0.10.1", Vec::new()).unwrap();
    assert!(printed(&reply).is_ok());
    reply.rlxd_version.clear();
    assert!(printed(&reply).is_err());

    let mut request = Request::new(RequestId::from(1u32), "status", Data::new()).unwrap();
    assert!(printed(&request).is_ok());
    request.method.clear();
    assert!(printed(&request).is_err());

    let argv = vec!["rlx".to_owned()];
    let mut record = DaemonRecord::new(
        "0123456789ab",
        "watch",
        1,
        "h",
        "v",
        TIME,
        TIME,
        argv,
        false,
    )
    .unwrap();
    assert!(printed(&record).is_ok());
    record.started_at = "2026-11-25T12:00:60Z".to_owned();
    assert!(printed(&record).is_err());

    let paths = Paths::new("h", "s", "l", "t", "g");
    let mut status = StatusData::new("0.10.1", 1, "linux", paths).unwrap();
    assert!(printed(&status).is_ok());
    status.pid = 0;
    assert!(printed(&status).is_err());
}
