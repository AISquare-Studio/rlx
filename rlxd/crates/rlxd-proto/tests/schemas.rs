//! The checked-in schemas (`rlxd/schemas/v1/`): up to date with the types, valid draft
//! 2020-12 schemas, and in agreement with the types on the contract's examples (handoff
//! 0.10.3, 7.2 and 7.3, with the handshake and per-run sequence of rlxapp's Engine
//! change spec) and on what each must refuse.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rlxd_proto::{
    DaemonRecord, Data, Envelope, ErrorBody, Event, HelloData, HelloParams, PROTOCOL_VERSION,
    PROTOCOL_VERSIONS, Request, Role, RunScope, SCHEMA_VERSION, codes, daemon_kinds, schemas,
    topics,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn schemas_dir() -> PathBuf {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = crate_dir.parent().and_then(Path::parent).unwrap();
    workspace.join("schemas").join("v1")
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A validator for a checked-in schema, with `format` asserted (`date-time`, above all).
fn validator(schema: &str) -> jsonschema::Validator {
    jsonschema::options()
        .should_validate_formats(true)
        .build(&read_json(&schemas_dir().join(schema)))
        .unwrap_or_else(|e| panic!("{schema}: {e}"))
}

fn example(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("examples")
        .join(name);
    read_json(&path)
}

fn assert_valid(schema: &str, instance: &Value) {
    if let Err(error) = validator(schema).validate(instance) {
        panic!("{schema} refuses {instance}: {error}");
    }
}

/// `instance` validates against `schema`, parses as `T`, and `T` prints a document that
/// validates too and parses back to the same `T`. Returns what `T` prints.
fn agrees<T>(schema: &str, instance: &Value) -> Value
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_valid(schema, instance);
    let parsed: T = serde_json::from_value(instance.clone())
        .unwrap_or_else(|e| panic!("{instance} does not parse as {schema}'s type: {e}"));
    let printed = serde_json::to_value(&parsed).unwrap();
    assert_valid(schema, &printed);
    assert_eq!(
        serde_json::from_value::<T>(printed.clone()).unwrap(),
        parsed
    );
    printed
}

/// Both the schema and the type refuse `instance`.
fn both_refuse<T: DeserializeOwned + std::fmt::Debug>(schema: &str, instance: Value) {
    assert!(
        !validator(schema).is_valid(&instance),
        "{schema} accepts {instance}"
    );
    if let Ok(parsed) = serde_json::from_value::<T>(instance.clone()) {
        panic!("{instance} parses as {schema}'s type: {parsed:?}");
    }
}

/// The schema refuses `instance`. The type may parse it, so a client can read a newer
/// or a malformed document and say what is wrong with it.
fn schema_refuses(schema: &str, instance: Value) {
    assert!(
        !validator(schema).is_valid(&instance),
        "{schema} accepts {instance}"
    );
}

fn data(value: Value) -> Data {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

/// `base` with `field` removed.
fn without(base: &Value, field: &str) -> Value {
    let mut instance = base.clone();
    instance.as_object_mut().unwrap().remove(field);
    instance
}

/// `base` with `field` set to `value`.
fn with(base: &Value, field: &str, value: Value) -> Value {
    let mut instance = base.clone();
    instance[field] = value;
    instance
}

fn hello_base() -> Value {
    json!({"token": "t", "protocol_version": 1, "client": "rlxapp", "client_version": "0.1.0",
        "role": "app", "capabilities": []})
}

fn reply_base() -> Value {
    json!({"protocol_version": 1, "protocol_versions": [1], "rlxd_version": "0.10.1",
        "capabilities": []})
}

fn run_event_base() -> Value {
    example("event.json")
}

#[test]
fn the_checked_in_schemas_are_the_generated_ones() {
    let dir = schemas_dir();
    for file in schemas::all() {
        let on_disk = fs::read_to_string(dir.join(file.name))
            .unwrap_or_else(|e| panic!("{}: {e}", file.name));
        assert!(
            on_disk == file.contents,
            "{} drifted from rlxd-proto: run `cargo run -p rlxd-proto --bin gen-schemas`",
            file.name
        );
    }
}

#[test]
fn every_schema_is_a_valid_draft_2020_12_schema() {
    let files = schemas::all();
    let names: Vec<_> = files.iter().map(|file| file.name).collect();
    for name in [
        "envelope.json",
        "event.json",
        "event-list.json",
        "daemon-record.json",
    ] {
        assert!(names.contains(&name), "{name} is not generated");
    }
    for file in files {
        let schema: Value = serde_json::from_str(&file.contents).unwrap();
        assert_eq!(
            schema["$schema"], "https://json-schema.org/draft/2020-12/schema",
            "{}",
            file.name
        );
        if let Err(error) = jsonschema::meta::validate(&schema) {
            panic!("{} is not a valid schema: {error}", file.name);
        }
    }
}

#[test]
fn the_contracts_envelopes_validate_and_print_back_as_themselves() {
    for name in [
        "envelope-ok.json",
        "envelope-error.json",
        "envelope-socket.json",
        "envelope-unauthorized.json",
        "envelope-incompatible-protocol.json",
    ] {
        let instance = example(name);
        assert_eq!(agrees::<Envelope>("envelope.json", &instance), instance);
    }
}

#[test]
fn the_contracts_events_validate_and_print_back_as_themselves() {
    for name in ["event.json", "event-daemon.json"] {
        let instance = example(name);
        assert_eq!(agrees::<Event>("event.json", &instance), instance);
    }
}

#[test]
fn a_list_of_events_validates_as_event_list() {
    // 7.3's checks validate `jq -s .` of `rlxd events`' lines against event-list.json.
    let event = example("event.json");
    let later = with(&with(&event, "seq", json!(43)), "data", {
        let mut data = event["data"].clone();
        data["run_seq"] = json!(4);
        data
    });
    let list = json!([example("event-daemon.json"), event.clone(), later]);
    assert_eq!(agrees::<Vec<Event>>("event-list.json", &list), list);
    assert_valid("event-list.json", &json!([]));

    let unseq = without(&event, "seq");
    both_refuse::<Vec<Event>>("event-list.json", json!([event.clone(), unseq]));
    both_refuse::<Vec<Event>>("event-list.json", event.clone());
    let unscoped = with(&event, "data", json!({}));
    schema_refuses("event-list.json", json!([event, unscoped]));
}

#[test]
fn request_frames_validate_and_absent_params_mean_empty_params() {
    let instance = example("request.json");
    assert_eq!(agrees::<Request>("request.json", &instance), instance);

    let bare = example("request-no-params.json");
    let printed = agrees::<Request>("request.json", &bare);
    assert_eq!(printed, json!({"id": 1, "method": "status", "params": {}}));
}

#[test]
fn the_handshake_validates_and_prints_back_as_itself() {
    let hello = example("hello.json");
    assert_eq!(agrees::<HelloParams>("hello.json", &hello), hello);
    let reply = example("hello-reply.json");
    assert_eq!(agrees::<HelloData>("hello-reply.json", &reply), reply);

    let without_env = with(&hello_base(), "role", json!("cli"));
    assert_eq!(
        agrees::<HelloParams>("hello.json", &without_env),
        without_env
    );

    for (role, name) in [
        (Role::App, "app"),
        (Role::Tui, "tui"),
        (Role::Remote, "remote"),
        (Role::Cli, "cli"),
    ] {
        let printed = serde_json::to_value(HelloParams::new("t", "c", "1", role)).unwrap();
        assert_eq!(printed["role"], name);
        assert_eq!(printed["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(agrees::<HelloParams>("hello.json", &printed), printed);
    }
}

#[test]
fn the_builders_print_the_documented_shapes() {
    let ok = Envelope::ok(Data::new());
    assert_eq!(
        serde_json::to_value(&ok).unwrap(),
        example("envelope-ok.json")
    );
    assert!(ok.is_ok());

    let error = ErrorBody::new("not_a_tty", "…")
        .with_hint("…")
        .with_detail("…");
    let refused = Envelope::err(error.clone());
    assert_eq!(
        serde_json::to_value(&refused).unwrap(),
        example("envelope-error.json")
    );
    assert!(!refused.is_ok());
    assert_eq!(refused.into_result(), Err(error));

    let answer = Envelope::ok(data(json!({"run_id": "run_1"}))).with_id(7);
    assert_eq!(
        serde_json::to_value(&answer).unwrap(),
        example("envelope-socket.json")
    );
    assert_eq!(
        Envelope::from(answer.into_result()).with_id(7),
        serde_json::from_value(example("envelope-socket.json")).unwrap()
    );

    let unauthorized = Envelope::err(ErrorBody::new(
        codes::UNAUTHORIZED,
        "the first frame must be hello",
    ))
    .with_id(1);
    assert_eq!(
        serde_json::to_value(&unauthorized).unwrap(),
        example("envelope-unauthorized.json")
    );

    let incompatible = Envelope::err(ErrorBody::incompatible_protocol(99)).with_id(1);
    assert_eq!(
        serde_json::to_value(&incompatible).unwrap(),
        example("envelope-incompatible-protocol.json")
    );

    let event = Event::for_run(
        42,
        "2026-11-25T12:00:00.000Z",
        "run.text",
        RunScope::new("…", 3),
        data(json!({"kind": "task"})),
    );
    assert_eq!(event.schema_version, SCHEMA_VERSION);
    assert_eq!(event.topic, topics::RUN);
    assert_eq!(serde_json::to_value(&event).unwrap(), example("event.json"));
    assert_eq!(event.run_scope(), Some(RunScope::new("…", 3)));

    let daemon = Event::new(
        41,
        "2026-11-25T12:00:00.000Z",
        topics::DAEMON,
        "daemon.started",
        data(json!({"version": "0.10.1", "pid": 4242})),
    );
    assert_eq!(
        serde_json::to_value(&daemon).unwrap(),
        example("event-daemon.json")
    );
    assert_eq!(daemon.run_scope(), None);

    let request = Request::new(7, "run.start", Data::new());
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        example("request.json")
    );

    let env = BTreeMap::from([("CLAUDE_CONFIG_DIR".to_owned(), "cc-test".to_owned())]);
    let hello = HelloParams::new("0123abcd", "rlxapp", "0.1.0", Role::App).with_env(env);
    assert_eq!(serde_json::to_value(&hello).unwrap(), example("hello.json"));
    let able = HelloParams::new("t", "rlxapp", "0.1.0", Role::App)
        .with_capabilities(vec!["tool_gate".to_owned()]);
    assert_eq!(
        serde_json::to_value(&able).unwrap(),
        with(&hello_base(), "capabilities", json!(["tool_gate"]))
    );

    let reply = HelloData::new(PROTOCOL_VERSION, "0.10.1", Vec::new());
    assert_eq!(reply.protocol_versions, PROTOCOL_VERSIONS);
    assert_eq!(
        serde_json::to_value(&reply).unwrap(),
        example("hello-reply.json")
    );
}

#[test]
fn what_a_client_does_not_know_is_ignored_not_refused() {
    // New fields keep the schema version (7.2), so neither side may refuse one.
    let envelope = json!({"schema_version": 1, "ok": true, "data": {}, "later": 1});
    agrees::<Envelope>("envelope.json", &envelope);
    let error = json!({"schema_version": 1, "ok": false,
        "error": {"code": "c", "message": "m", "later": 1}});
    agrees::<Envelope>("envelope.json", &error);
    let event = json!({"schema_version": 1, "seq": 1, "time": "2026-11-25T12:00:00.000Z",
        "topic": "later", "type": "later.thing", "data": {}, "later": 1});
    agrees::<Event>("event.json", &event);
    agrees::<Event>("event.json", &with(&run_event_base(), "later", json!(1)));
    let request = json!({"id": 1, "method": "status", "params": {}, "later": 1});
    agrees::<Request>("request.json", &request);
    agrees::<HelloParams>("hello.json", &with(&hello_base(), "later", json!(1)));
    agrees::<HelloData>("hello-reply.json", &with(&reply_base(), "later", json!(1)));
}

#[test]
fn an_envelope_is_ok_with_data_or_not_ok_with_an_error_and_nothing_else() {
    for instance in [
        json!({"schema_version": 1, "ok": true}),
        json!({"schema_version": 1, "ok": false}),
        json!({"schema_version": 1, "ok": true, "error": {"code": "c", "message": "m"}}),
        json!({"schema_version": 1, "ok": false, "data": {}}),
        json!({"schema_version": 1, "ok": "true", "data": {}}),
        json!({"schema_version": 1, "data": {}}),
        json!({"ok": true, "data": {}}),
        json!({"schema_version": "1", "ok": true, "data": {}}),
        json!({"schema_version": 1, "ok": true, "data": []}),
        json!({"schema_version": 1, "ok": true, "data": null}),
        json!({"schema_version": 1, "ok": false, "error": {"code": "c"}}),
        json!({"schema_version": 1, "ok": false, "error": {"message": "m"}}),
        json!({"schema_version": 1, "ok": false, "error": "not_a_tty"}),
        json!({"schema_version": 1, "id": "7", "ok": true, "data": {}}),
        json!({"schema_version": 1, "id": -1, "ok": true, "data": {}}),
    ] {
        both_refuse::<Envelope>("envelope.json", instance);
    }
}

#[test]
fn incompatible_protocol_always_says_which_versions_are_served() {
    let base = example("envelope-incompatible-protocol.json");
    let error = |detail: Option<Value>| {
        let mut instance = base.clone();
        match detail {
            Some(detail) => instance["error"]["detail"] = detail,
            None => {
                instance["error"].as_object_mut().unwrap().remove("detail");
            }
        }
        instance
    };
    for detail in [
        None,
        Some(json!("1")),
        Some(json!({})),
        Some(json!({"supported": []})),
        Some(json!({"supported": [0]})),
        Some(json!({"supported": ["1"]})),
    ] {
        schema_refuses("envelope.json", error(detail));
    }
    assert_valid("envelope.json", &error(Some(json!({"supported": [1, 2]}))));
    // Any other code's detail may be a string or an object.
    let other = json!({"schema_version": 1, "ok": false,
        "error": {"code": "c", "message": "m", "detail": {"anything": true}}});
    agrees::<Envelope>("envelope.json", &other);
}

#[test]
fn an_event_has_every_field() {
    let whole = example("event-daemon.json");
    for field in ["schema_version", "seq", "time", "topic", "type", "data"] {
        both_refuse::<Event>("event.json", without(&whole, field));
    }
    for (field, value) in [
        ("seq", json!(-1)),
        ("seq", json!("42")),
        ("data", json!([])),
        ("data", json!(null)),
        ("topic", json!(1)),
    ] {
        both_refuse::<Event>("event.json", with(&whole, field, value));
    }
}

#[test]
fn a_run_event_carries_its_run_and_its_place_in_it() {
    let event = run_event_base();
    let parsed: Event = serde_json::from_value(event.clone()).unwrap();
    assert_eq!(parsed.run_scope(), Some(RunScope::new("…", 3)));

    // The handoff's rev 2 example, before the per-run sequence: valid no more.
    schema_refuses("event.json", with(&event, "data", json!({})));
    for field in ["run_id", "run_seq"] {
        let mut instance = event.clone();
        instance["data"].as_object_mut().unwrap().remove(field);
        schema_refuses("event.json", instance.clone());
        let parsed: Event = serde_json::from_value(instance).unwrap();
        assert_eq!(parsed.run_scope(), None, "without {field}");
    }
    for (field, value) in [
        ("run_seq", json!(0)),
        ("run_seq", json!(-1)),
        ("run_seq", json!("3")),
        ("run_id", json!("")),
        ("run_id", json!(7)),
    ] {
        let mut instance = event.clone();
        instance["data"][field] = value;
        schema_refuses("event.json", instance);
    }
    // Other topics need neither.
    assert_valid("event.json", &example("event-daemon.json"));
}

#[test]
fn requests_and_the_handshake_refuse_their_wrong_shapes() {
    for instance in [
        json!({"method": "status"}),
        json!({"id": 1}),
        json!({"id": "1", "method": "status"}),
        json!({"id": 1, "method": "status", "params": []}),
    ] {
        both_refuse::<Request>("request.json", instance);
    }

    let hello = hello_base();
    for field in [
        "token",
        "protocol_version",
        "client",
        "client_version",
        "role",
        "capabilities",
    ] {
        both_refuse::<HelloParams>("hello.json", without(&hello, field));
    }
    for (field, value) in [
        ("protocol_version", json!("1")),
        ("protocol_version", json!(-1)),
        ("role", json!("robot")),
        ("role", json!("App")),
        ("role", json!(null)),
        ("capabilities", json!("tool_gate")),
        ("env", json!({"A": 1})),
    ] {
        both_refuse::<HelloParams>("hello.json", with(&hello, field, value));
    }

    let reply = reply_base();
    for field in [
        "protocol_version",
        "protocol_versions",
        "rlxd_version",
        "capabilities",
    ] {
        both_refuse::<HelloData>("hello-reply.json", without(&reply, field));
    }
    both_refuse::<HelloData>(
        "hello-reply.json",
        with(&reply, "protocol_versions", json!(["1"])),
    );
}

#[test]
fn the_schemas_also_hold_what_the_types_leave_to_the_reader() {
    // The types parse these, so a client can read a newer or a malformed document and
    // say what is wrong with it (and an optional field's `null` reads as absent); the v1
    // schemas refuse them, and rlxd never prints them.
    let envelope = |error: Value| json!({"schema_version": 1, "ok": false, "error": error});
    schema_refuses(
        "envelope.json",
        json!({"schema_version": 2, "ok": true, "data": {}}),
    );
    schema_refuses(
        "envelope.json",
        envelope(json!({"code": "", "message": "m"})),
    );
    schema_refuses(
        "envelope.json",
        envelope(json!({"code": "c", "message": ""})),
    );
    schema_refuses(
        "envelope.json",
        envelope(json!({"code": "c", "message": "m", "hint": null})),
    );

    let event = example("event-daemon.json");
    schema_refuses("event.json", with(&event, "seq", json!(0)));
    schema_refuses("event.json", with(&event, "time", json!("yesterday")));
    schema_refuses("event.json", with(&event, "topic", json!("")));
    schema_refuses("request.json", json!({"id": 1, "method": ""}));

    let hello = hello_base();
    schema_refuses("hello.json", with(&hello, "token", json!("")));
    schema_refuses("hello.json", with(&hello, "protocol_version", json!(0)));
    schema_refuses("hello.json", with(&hello, "env", json!(null)));

    let reply = reply_base();
    schema_refuses(
        "hello-reply.json",
        with(&reply, "protocol_versions", json!([])),
    );
    schema_refuses(
        "hello-reply.json",
        with(&reply, "protocol_versions", json!([0])),
    );
    schema_refuses(
        "hello-reply.json",
        with(&reply, "protocol_version", json!(0)),
    );
    schema_refuses("hello-reply.json", with(&reply, "rlxd_version", json!("")));
}

#[test]
fn a_daemon_record_reads_as_python_writes_it() {
    // core/daemons.py always writes `cwd`, a string or null, and `+00:00` offsets.
    let record = example("daemon-record.json");
    assert_eq!(
        agrees::<DaemonRecord>("daemon-record.json", &record),
        record
    );
    let built = DaemonRecord::new(
        "0123456789ab",
        daemon_kinds::WATCH,
        4242,
        "mac.local",
        "0.10.1",
        "2026-11-25T18:00:00.123456+00:00",
        "2026-11-25T18:00:05.123456+00:00",
        vec!["rlx".to_owned(), "watch".to_owned()],
        true,
    )
    .with_cwd("/Users/me/project");
    assert_eq!(serde_json::to_value(&built).unwrap(), record);

    // A null cwd reads as absent, and `Z` as an offset.
    let no_cwd = example("daemon-record-no-cwd.json");
    let printed = agrees::<DaemonRecord>("daemon-record.json", &no_cwd);
    assert_eq!(printed, without(&no_cwd, "cwd"));
    assert_eq!(
        serde_json::from_value::<DaemonRecord>(no_cwd).unwrap().cwd,
        None
    );

    // A later release's kind or field is kept, not refused (4.1).
    agrees::<DaemonRecord>("daemon-record.json", &with(&record, "kind", json!("later")));
    agrees::<DaemonRecord>("daemon-record.json", &with(&record, "later", json!(1)));
    for kind in daemon_kinds::ALL {
        assert_valid("daemon-record.json", &with(&record, "kind", json!(kind)));
    }
}

#[test]
fn a_daemon_record_refuses_its_wrong_shapes() {
    let record = example("daemon-record.json");
    for field in [
        "id",
        "kind",
        "pid",
        "host",
        "version",
        "started_at",
        "heartbeat_at",
        "argv",
        "restartable",
    ] {
        both_refuse::<DaemonRecord>("daemon-record.json", without(&record, field));
    }
    for (field, value) in [
        ("pid", json!(-1)),
        ("pid", json!("4242")),
        ("argv", json!("rlx watch")),
        ("restartable", json!("yes")),
        ("cwd", json!(7)),
    ] {
        both_refuse::<DaemonRecord>("daemon-record.json", with(&record, field, value));
    }
    for (field, value) in [
        ("id", json!("0123456789AB")),
        ("id", json!("0123456789a")),
        ("id", json!("0123456789abc")),
        ("kind", json!("")),
        ("pid", json!(0)),
        ("argv", json!([])),
        ("started_at", json!("yesterday")),
        ("heartbeat_at", json!("2026-11-25 18:00")),
    ] {
        schema_refuses("daemon-record.json", with(&record, field, value));
    }
}
