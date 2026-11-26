//! The checked-in schemas (`rlxd/schemas/v1/`): up to date with the types, valid draft
//! 2020-12 schemas, and in agreement with the types (handoff 0.10.3 rev 5, 7.2, 7.3 and
//! 4.1). Every document either both accept or both refuse, and every builder prints
//! what its schema accepts.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rlxd_proto::{
    DaemonRecord, Data, Detail, Envelope, ErrorBody, Event, HelloData, HelloParams, Invalid,
    PROTOCOL_VERSION, PROTOCOL_VERSIONS, Paths, Request, RequestId, Role, RunScope, SCHEMA_VERSION,
    StatusData, codes, daemon_kinds, protocol_version_of, schema_version_of, schemas, topics,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const TIME: &str = "2026-11-25T12:00:00.000Z";

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn schemas_dir() -> PathBuf {
    let workspace = crate_dir().parent().and_then(Path::parent).unwrap();
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
    read_json(&crate_dir().join("tests").join("examples").join(name))
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

/// Both the schema and the type refuse `instance`. Returns the type's error.
fn both_refuse<T: DeserializeOwned + std::fmt::Debug>(schema: &str, instance: Value) -> String {
    assert!(
        !validator(schema).is_valid(&instance),
        "{schema} accepts {instance}"
    );
    match serde_json::from_value::<T>(instance.clone()) {
        Ok(parsed) => panic!("{instance} parses as {schema}'s type: {parsed:?}"),
        Err(error) => error.to_string(),
    }
}

/// The field a builder's refusal names.
fn refused<T>(result: Result<T, Invalid>) -> String {
    match result {
        Ok(_) => panic!("the builder accepted it"),
        Err(error) => error.field().to_owned(),
    }
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

/// `base` with `error.<field>` set to `value`.
fn with_error(base: &Value, field: &str, value: Value) -> Value {
    let mut instance = base.clone();
    instance["error"][field] = value;
    instance
}

fn error_base() -> Value {
    json!({"schema_version": 1, "ok": false, "error": {"code": "c", "message": "m"}})
}

fn hello_base() -> Value {
    json!({"token": "t", "protocol_version": 1, "client": "rlxapp", "client_version": "0.1.0",
        "role": "app", "capabilities": []})
}

fn reply_base() -> Value {
    json!({"protocol_version": 1, "protocol_versions": [1], "rlxd_version": "0.10.1",
        "capabilities": []})
}

fn id(n: u32) -> RequestId {
    RequestId::from(n)
}

// --- the schemas themselves ---------------------------------------------------------------

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

/// The body of every `#[<name>(…)]` attribute in `text`: what lies between its
/// parentheses, balanced, with a parenthesis inside a string literal (`"("`, `"\")"`)
/// taken as text.
fn attribute_bodies<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let opener = format!("#[{name}(");
    let mut bodies = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(&opener) {
        let body = &rest[at + opener.len()..];
        let (mut depth, mut in_string, mut escaped) = (1, false, false);
        let end = body
            .char_indices()
            .find_map(|(i, c)| {
                if in_string {
                    match c {
                        _ if escaped => escaped = false,
                        '\\' => escaped = true,
                        '"' => in_string = false,
                        _ => {}
                    }
                    return None;
                }
                match c {
                    '"' => in_string = true,
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                (depth == 0).then_some(i)
            })
            .unwrap_or(body.len());
        bodies.push(&body[..end]);
        rest = &body[end..];
    }
    bodies
}

/// Whether an attribute body sets one of `options`, outside a string literal.
fn sets_an_option(body: &str, options: &[&str]) -> bool {
    let mut outside = String::new();
    let (mut in_string, mut escaped) = (false, false);
    for c in body.chars() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
            outside.push(' ');
        } else {
            outside.push(c);
        }
    }
    let words: Vec<&str> = outside
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '='))
        .filter(|w| !w.is_empty())
        .collect();
    options.iter().any(|name| {
        words.iter().enumerate().any(|(i, w)| {
            *w == *name && words.get(i + 1).is_some_and(|next| next.starts_with('='))
                || w.strip_prefix(name)
                    .is_some_and(|tail| tail.starts_with('='))
        })
    })
}

/// Whether `source` replaces a field's schema with another type's or a function's: the
/// options schemars reads to do it are `with` and `schema_with` in `#[schemars(…)]`, and
/// `with` in `#[serde(…)]` (serde's `deserialize_with` and `serialize_with` it leaves
/// alone).
fn overrides_a_schema(source: &str) -> Option<String> {
    let schemars = attribute_bodies(source, "schemars")
        .into_iter()
        .find(|body| sets_an_option(body, &["with", "schema_with"]))
        .map(|body| format!("#[schemars({body})]"));
    let serde = || {
        attribute_bodies(source, "serde")
            .into_iter()
            .find(|body| sets_an_option(body, &["with"]))
            .map(|body| format!("#[serde({body})]"))
    };
    schemars.or_else(serde)
}

/// Every `.rs` file under `dir`, in its subdirectories too.
fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|e| e == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn every_schema_comes_from_the_fields_real_types() {
    // A `with` or `schema_with` override hides a change to its field's Rust type from
    // gen-schemas --check, so there are none: optional fields drop `null` with a
    // transform on the schema derived from their own type instead.
    for probe in [
        "#[schemars(with = \"String\")]",
        "#[schemars(length(min = 1), with = \"String\")]",
        "#[schemars(title = \"x\", with=\"Value\")]",
        "#[schemars(schema_with = \"f\")]",
        "#[schemars(transform = drop_null, schema_with = \"f\")]",
        "#[schemars(regex(pattern = \"^(a\"), with = \"String\")]",
        "#[schemars(description = \"a \\\")\", with = \"String\")]",
        "#[serde(with = \"module\")]",
        "#[serde(default, with = \"module\")]",
    ] {
        assert!(overrides_a_schema(probe).is_some(), "{probe}");
    }
    for fine in [
        "#[schemars(transform = drop_null)]",
        "#[schemars(length(min = 1))]",
        "#[schemars(title = \"Error\")]",
        "#[schemars(description = \"with = x\")]",
        "#[schemars(regex(pattern = \"^(with=)$\"))]",
        "#[serde(default, deserialize_with = \"present\")]",
        "#[serde(serialize_with = \"f\", rename = \"with\")]",
    ] {
        assert_eq!(overrides_a_schema(fine), None, "{fine}");
    }
    let mut files = Vec::new();
    rust_files(&crate_dir().join("src"), &mut files);
    let mut seen = 0;
    for path in &files {
        let text = fs::read_to_string(path).unwrap();
        seen += attribute_bodies(&text, "schemars").len();
        if let Some(attribute) = overrides_a_schema(&text) {
            panic!("{} overrides a field's schema: {attribute}", path.display());
        }
    }
    assert!(seen > 10, "found only {seen} schemars attributes");
    assert!(
        files.iter().any(|p| p.ends_with("bin/gen-schemas.rs")),
        "the scan reaches src/bin: {files:?}"
    );
}

// --- the contract's examples ----------------------------------------------------------------

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
    let mut later = with(&event, "seq", json!(43));
    later["data"]["run_seq"] = json!(4);
    let list = json!([example("event-daemon.json"), event.clone(), later]);
    assert_eq!(agrees::<Vec<Event>>("event-list.json", &list), list);
    assert_valid("event-list.json", &json!([]));

    let unseq = without(&event, "seq");
    both_refuse::<Vec<Event>>("event-list.json", json!([event.clone(), unseq]));
    both_refuse::<Vec<Event>>("event-list.json", event.clone());
    let unscoped = with(&event, "data", json!({}));
    both_refuse::<Vec<Event>>("event-list.json", json!([event, unscoped]));
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
    let cli = with(&hello_base(), "role", json!("cli"));
    assert_eq!(agrees::<HelloParams>("hello.json", &cli), cli);
}

#[test]
fn a_daemon_record_reads_as_python_writes_it() {
    // core/daemons.py always writes `cwd`, a string or null, and `+00:00` offsets.
    let record = example("daemon-record.json");
    assert_eq!(
        agrees::<DaemonRecord>("daemon-record.json", &record),
        record
    );

    // A null cwd reads as absent, and `Z` as an offset.
    let no_cwd = example("daemon-record-no-cwd.json");
    let printed = agrees::<DaemonRecord>("daemon-record.json", &no_cwd);
    assert_eq!(printed, without(&no_cwd, "cwd"));

    // A later release's kind or field is kept, not refused (4.1).
    agrees::<DaemonRecord>("daemon-record.json", &with(&record, "kind", json!("later")));
    agrees::<DaemonRecord>("daemon-record.json", &with(&record, "later", json!(1)));
    for kind in daemon_kinds::ALL {
        agrees::<DaemonRecord>("daemon-record.json", &with(&record, "kind", json!(kind)));
    }
}

#[test]
fn what_a_client_does_not_know_is_ignored_not_refused() {
    // New fields keep the schema version (7.2), so neither side may refuse one.
    let later = |base: Value| with(&base, "later", json!(1));
    agrees::<Envelope>("envelope.json", &later(example("envelope-ok.json")));
    agrees::<Envelope>(
        "envelope.json",
        &with_error(&error_base(), "later", json!(1)),
    );
    agrees::<Event>("event.json", &later(example("event.json")));
    let other_topic = json!({"schema_version": 1, "seq": 1, "time": TIME, "topic": "later",
        "type": "later.thing", "data": {}});
    agrees::<Event>("event.json", &other_topic);
    agrees::<Request>("request.json", &later(example("request.json")));
    agrees::<HelloParams>("hello.json", &later(hello_base()));
    agrees::<HelloData>("hello-reply.json", &later(reply_base()));
}

// --- the builders ---------------------------------------------------------------------------

#[test]
fn every_builder_prints_what_its_schema_accepts() {
    let error = ErrorBody::new("not_a_tty", "…")
        .unwrap()
        .with_hint("…")
        .with_detail("…")
        .unwrap();
    let denied = ErrorBody::new("denied", "the gate denied it")
        .unwrap()
        .with_detail(data(json!({"by": "unreachable"})))
        .unwrap();
    let unauthorized =
        ErrorBody::new(codes::UNAUTHORIZED, "the first frame must be hello").unwrap();
    let not_gate = ErrorBody::new(codes::NOT_GATE, "this connection is not the gate").unwrap();
    let largest = RequestId::new(RequestId::MAX).unwrap();

    let envelopes = [
        (Envelope::ok(Data::new()), Some("envelope-ok.json")),
        (Envelope::err(error), Some("envelope-error.json")),
        (
            Envelope::ok(data(json!({"run_id": "run_1"}))).with_id(id(7)),
            Some("envelope-socket.json"),
        ),
        (
            Envelope::err(unauthorized).with_id(id(1)),
            Some("envelope-unauthorized.json"),
        ),
        (
            Envelope::err(ErrorBody::incompatible_protocol(99)).with_id(id(1)),
            Some("envelope-incompatible-protocol.json"),
        ),
        (Envelope::err(denied), None),
        (Envelope::err(not_gate).with_id(id(2)), None),
        (Envelope::ok(Data::new()).with_id(largest), None),
    ];
    for (envelope, shape) in envelopes {
        let printed = serde_json::to_value(&envelope).unwrap();
        assert_eq!(agrees::<Envelope>("envelope.json", &printed), printed);
        if let Some(shape) = shape {
            assert_eq!(printed, example(shape), "{shape}");
        }
    }

    let run_text = Event::for_run(
        42,
        TIME,
        "run.text",
        RunScope::new("…", 3, "task", 1),
        Data::new(),
    );
    let started = Event::new(
        41,
        TIME,
        topics::DAEMON,
        "daemon.started",
        data(json!({"version": "0.10.1", "pid": 4242})),
    );
    let offset = Event::new(
        1,
        "2026-11-25T12:00:00+05:30",
        "later",
        "later.thing",
        Data::new(),
    );
    let fired = Event::for_run(
        43,
        TIME,
        "run.started",
        RunScope::new("r", 1, "widget_refresh", 2).with_job("job_1"),
        Data::new(),
    );
    let login = Event::new(
        44,
        TIME,
        topics::CONNECTOR,
        "connector.login.open_url",
        data(json!({"login_id": "l", "id": "claude.ai Slack", "url": "https://x"})),
    );
    for (event, shape) in [
        (fired, None),
        (login, None),
        (run_text, Some("event.json")),
        (started, Some("event-daemon.json")),
        (offset, None),
    ] {
        let event = event.unwrap();
        assert_eq!(event.schema_version.get(), SCHEMA_VERSION);
        let printed = serde_json::to_value(&event).unwrap();
        assert_eq!(agrees::<Event>("event.json", &printed), printed);
        if let Some(shape) = shape {
            assert_eq!(printed, example(shape), "{shape}");
        }
    }

    let request = Request::new(id(7), "run.start", Data::new()).unwrap();
    let printed = serde_json::to_value(&request).unwrap();
    assert_eq!(printed, example("request.json"));
    agrees::<Request>("request.json", &printed);

    let env = BTreeMap::from([("CLAUDE_CONFIG_DIR".to_owned(), "cc-test".to_owned())]);
    let app = HelloParams::new("0123abcd", "rlxapp", "0.1.0", Role::App)
        .unwrap()
        .with_gate_credential("gc-secret")
        .unwrap()
        .with_env(env);
    let able = HelloParams::new("t", "rlxapp", "0.1.0", Role::App)
        .unwrap()
        .with_capabilities(vec!["tool_gate".to_owned()]);
    let hellos = [
        (app, Some("hello.json")),
        (able, None),
        (
            HelloParams::new("t", "rlx-tui", "0.10.1", Role::Tui).unwrap(),
            None,
        ),
        (
            HelloParams::new("t", "remote", "0.10.1", Role::Remote).unwrap(),
            None,
        ),
        (
            HelloParams::new("t", "rlxd-hook", "0.10.1", Role::Cli).unwrap(),
            None,
        ),
    ];
    for (hello, shape) in hellos {
        assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
        let printed = serde_json::to_value(&hello).unwrap();
        assert_eq!(agrees::<HelloParams>("hello.json", &printed), printed);
        if let Some(shape) = shape {
            assert_eq!(printed, example(shape), "{shape}");
        }
    }

    let reply = HelloData::new(PROTOCOL_VERSION, "0.10.1", Vec::new()).unwrap();
    assert_eq!(reply.protocol_versions, PROTOCOL_VERSIONS);
    let printed = serde_json::to_value(&reply).unwrap();
    assert_eq!(printed, example("hello-reply.json"));
    agrees::<HelloData>("hello-reply.json", &printed);

    let record = DaemonRecord::new(
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
    .unwrap();
    let printed = serde_json::to_value(&record).unwrap();
    agrees::<DaemonRecord>("daemon-record.json", &printed);
    let printed = serde_json::to_value(record.with_cwd("/Users/me/project")).unwrap();
    assert_eq!(printed, example("daemon-record.json"));
}

#[test]
fn the_builders_refuse_what_the_schemas_refuse() {
    assert_eq!(refused(ErrorBody::new("", "m")), "code");
    assert_eq!(refused(ErrorBody::new("c", "")), "message");
    assert_eq!(
        refused(ErrorBody::new(codes::INCOMPATIBLE_PROTOCOL, "m")),
        "detail.supported"
    );
    assert_eq!(
        refused(ErrorBody::incompatible_protocol(9).with_detail("text")),
        "detail.supported"
    );

    let none = Data::new;
    assert_eq!(refused(Event::new(0, TIME, "d", "d.x", none())), "seq");
    assert_eq!(
        refused(Event::new(1, "yesterday", "d", "d.x", none())),
        "time"
    );
    let feb_30 = "2026-02-30T00:00:00Z";
    assert_eq!(refused(Event::new(1, feb_30, "d", "d.x", none())), "time");
    assert_eq!(refused(Event::new(1, TIME, "", "d.x", none())), "topic");
    assert_eq!(refused(Event::new(1, TIME, "d", "", none())), "type");
    let run = topics::RUN;
    assert_eq!(
        refused(Event::new(1, TIME, run, "run.text", none())),
        "data.run_id"
    );
    let no_id = RunScope::new("", 1, "task", 1);
    assert_eq!(
        refused(Event::for_run(1, TIME, "run.text", no_id, none())),
        "data.run_id"
    );
    let no_seq = RunScope::new("r", 0, "task", 1);
    assert_eq!(
        refused(Event::for_run(1, TIME, "run.text", no_seq, none())),
        "data.run_seq"
    );
    let no_kind = RunScope::new("r", 1, "", 1);
    assert_eq!(
        refused(Event::for_run(1, TIME, "run.text", no_kind, none())),
        "data.kind"
    );
    let no_attempt = RunScope::new("r", 1, "task", 0);
    assert_eq!(
        refused(Event::for_run(1, TIME, "run.text", no_attempt, none())),
        "data.attempt"
    );
    let no_job = RunScope::new("r", 1, "task", 1).with_job("");
    assert_eq!(
        refused(Event::for_run(1, TIME, "run.text", no_job, none())),
        "data.job_id"
    );
    // G3: the type is <topic>.<event>, the event in snake_case.
    let scope = || RunScope::new("r", 1, "task", 1);
    for wrong in [
        "daemon.gap",
        "run",
        "run.",
        "run.Text",
        "run..text",
        "run.text-x",
    ] {
        assert_eq!(
            refused(Event::for_run(1, TIME, wrong, scope(), none())),
            "type",
            "{wrong}"
        );
    }
    assert_eq!(
        refused(Event::new(1, TIME, "Run", "Run.text", none())),
        "topic"
    );
    assert_eq!(
        refused(Event::new(1, TIME, "needs-you", "needs-you.raised", none())),
        "topic"
    );

    assert_eq!(refused(Request::new(id(1), "", none())), "method");
    assert_eq!(refused(HelloParams::new("", "c", "1", Role::Cli)), "token");
    assert_eq!(refused(HelloParams::new("t", "", "1", Role::Cli)), "client");
    let app = HelloParams::new("t", "rlxapp", "1", Role::App).unwrap();
    assert_eq!(refused(app.with_gate_credential("")), "gate_credential");
    assert_eq!(
        refused(HelloData::new(0, "0.10.1", Vec::new())),
        "protocol_version"
    );
    assert_eq!(refused(HelloData::new(1, "", Vec::new())), "rlxd_version");

    let record = |id: &str, pid: u32, argv: Vec<String>| {
        DaemonRecord::new(id, "watch", pid, "h", "v", TIME, TIME, argv, false)
    };
    let argv = || vec!["rlx".to_owned()];
    assert_eq!(refused(record("0123456789AB", 1, argv())), "id");
    assert_eq!(refused(record("0123456789ab", 0, argv())), "pid");
    assert_eq!(refused(record("0123456789ab", 1, Vec::new())), "argv");

    assert_eq!(RequestId::new(RequestId::MAX + 1), None);
    assert_eq!(refused(RequestId::try_from(RequestId::MAX + 1)), "id");
}

#[test]
fn a_value_broken_after_it_was_built_does_not_print() {
    let mut error = ErrorBody::new("c", "m").unwrap();
    error.code.clear();
    assert!(serde_json::to_value(Envelope::err(error)).is_err());

    let scope = RunScope::new("r", 1, "task", 1);
    let mut event = Event::for_run(1, TIME, "run.text", scope, Data::new()).unwrap();
    event.data.remove("run_seq");
    assert!(serde_json::to_value(&event).is_err());

    let mut hello = HelloParams::new("t", "c", "1", Role::Cli).unwrap();
    hello.token.clear();
    assert!(serde_json::to_value(&hello).is_err());
}

// --- what both refuse -----------------------------------------------------------------------

#[test]
fn another_schema_version_fails_to_parse_rather_than_misparse() {
    // 7.2: a new version renames or removes fields. A v1 reader must not read one.
    for version in [json!(0), json!(2), json!(99), json!("1"), json!(null)] {
        let envelope = with(
            &example("envelope-ok.json"),
            "schema_version",
            version.clone(),
        );
        let error = both_refuse::<Envelope>("envelope.json", envelope);
        if version.is_u64() {
            assert!(error.contains("schema_version"), "{error}");
        }
        let event = with(&example("event.json"), "schema_version", version);
        both_refuse::<Event>("event.json", event);
    }
    // A reader can still say which version it was given.
    let newer = json!({"schema_version": 2, "ok": true, "result": {}});
    assert_eq!(schema_version_of(&newer), Some(2));
}

#[test]
fn ok_decides_which_of_data_and_error_an_envelope_carries() {
    let ok = example("envelope-ok.json");
    let refused = error_base();
    let cases = [
        (
            json!({"schema_version": 1, "ok": true}),
            "data: is required when ok is true",
        ),
        (
            json!({"schema_version": 1, "ok": false}),
            "error: is required when ok is false",
        ),
        (
            with(&ok, "error", refused["error"].clone()),
            "error: must be absent when ok is true",
        ),
        (
            with(&refused, "data", json!({})),
            "data: must be absent when ok is false",
        ),
        (
            with(&ok, "error", json!(null)),
            "error: must be absent when ok is true",
        ),
        (
            with(&refused, "data", json!(null)),
            "data: must be absent when ok is false",
        ),
        (with(&ok, "data", json!([])), "data: must be an object"),
        (with(&ok, "data", json!(null)), "data: must be an object"),
        (
            with_error(&refused, "message", json!("")),
            "error: message: must not be empty",
        ),
        (
            json!({"schema_version": 1, "ok": false, "error": {"code": "c"}}),
            "error: missing field `message`",
        ),
        (
            with(&refused, "error", json!("not_a_tty")),
            "error: invalid type",
        ),
        (without(&ok, "ok"), "missing field `ok`"),
        (with(&ok, "ok", json!("true")), "invalid type"),
        (
            without(&ok, "schema_version"),
            "missing field `schema_version`",
        ),
    ];
    for (instance, message) in cases {
        let error = both_refuse::<Envelope>("envelope.json", instance.clone());
        assert!(
            error.contains(message),
            "{instance}: {error:?} lacks {message:?}"
        );
    }
}

#[test]
fn an_errors_detail_is_a_string_or_an_object_and_never_null() {
    let refused = error_base();
    agrees::<Envelope>(
        "envelope.json",
        &with_error(&refused, "detail", json!("text")),
    );
    let by = with_error(&refused, "detail", json!({"by": "app"}));
    agrees::<Envelope>("envelope.json", &by);
    let parsed: Envelope = serde_json::from_value(by).unwrap();
    assert_eq!(
        parsed.into_result().unwrap_err().detail,
        Some(Detail::Object(data(json!({"by": "app"}))))
    );
    for detail in [json!(7), json!([1]), json!(true), json!(null)] {
        both_refuse::<Envelope>("envelope.json", with_error(&refused, "detail", detail));
    }
    for hint in [json!(null), json!(7)] {
        both_refuse::<Envelope>("envelope.json", with_error(&refused, "hint", hint));
    }
}

#[test]
fn incompatible_protocol_always_says_which_versions_are_served() {
    let base = example("envelope-incompatible-protocol.json");
    for detail in [
        json!("1"),
        json!({}),
        json!({"supported": []}),
        json!({"supported": [0]}),
        json!({"supported": ["1"]}),
    ] {
        let instance = with_error(&base, "detail", detail);
        let error = both_refuse::<Envelope>("envelope.json", instance);
        assert!(error.contains("detail.supported"), "{error}");
    }
    let mut bare = base.clone();
    bare["error"].as_object_mut().unwrap().remove("detail");
    both_refuse::<Envelope>("envelope.json", bare);
    let two = with_error(&base, "detail", json!({"supported": [1, 2]}));
    agrees::<Envelope>("envelope.json", &two);
}

#[test]
fn an_id_is_an_integer_a_javascript_client_holds_exactly() {
    let ok = example("envelope-ok.json");
    agrees::<Envelope>("envelope.json", &with(&ok, "id", json!(RequestId::MAX)));
    let largest = json!({"id": RequestId::MAX, "method": "status"});
    agrees::<Request>("request.json", &largest);
    for bad in [
        json!(RequestId::MAX + 1),
        json!(-1),
        json!(1.5),
        json!("7"),
        json!(null),
    ] {
        both_refuse::<Envelope>("envelope.json", with(&ok, "id", bad.clone()));
        both_refuse::<Request>("request.json", json!({"id": bad, "method": "status"}));
    }
    // The one case JSON Schema cannot tell apart: it reads 1.0 as the integer 1, which
    // the type refuses. No producer writes ids that way.
    let float_one = json!({"id": 1.0, "method": "status"});
    assert_valid("request.json", &float_one);
    assert!(serde_json::from_value::<Request>(float_one).is_err());
}

#[test]
fn an_event_has_every_field_and_a_run_event_its_run() {
    let whole = example("event-daemon.json");
    for field in ["schema_version", "seq", "time", "topic", "type", "data"] {
        both_refuse::<Event>("event.json", without(&whole, field));
    }
    for (field, value) in [
        ("seq", json!(0)),
        ("seq", json!(-1)),
        ("seq", json!("42")),
        ("time", json!("yesterday")),
        ("time", json!("2026-11-25 12:00:00Z")),
        ("time", json!("2026-11-25T12:00:00")),
        ("time", json!("2026-13-01T00:00:00Z")),
        ("topic", json!("")),
        ("topic", json!(1)),
        ("type", json!("")),
        ("data", json!([])),
        ("data", json!(null)),
    ] {
        both_refuse::<Event>("event.json", with(&whole, field, value));
    }

    let run = example("event.json");
    let parsed: Event = serde_json::from_value(run.clone()).unwrap();
    assert_eq!(parsed.run_scope(), Some(RunScope::new("…", 3, "task", 1)));
    // The handoff's rev 2 example, before the per-run sequence: valid no more.
    let error = both_refuse::<Event>("event.json", with(&run, "data", json!({})));
    assert!(error.contains("data.run_id"), "{error}");
    // G2: §7.5's run fields, every one required but job_id.
    for (field, value) in [
        ("run_seq", None),
        ("run_id", None),
        ("kind", None),
        ("attempt", None),
        ("run_seq", Some(json!(0))),
        ("run_seq", Some(json!(-1))),
        ("run_seq", Some(json!("3"))),
        ("run_id", Some(json!(""))),
        ("run_id", Some(json!(7))),
        ("kind", Some(json!(""))),
        ("kind", Some(json!(1))),
        ("attempt", Some(json!(0))),
        ("attempt", Some(json!("1"))),
        ("job_id", Some(json!(""))),
        ("job_id", Some(json!(7))),
        ("job_id", Some(json!(null))),
    ] {
        let mut instance = run.clone();
        match value {
            Some(value) => instance["data"][field] = value,
            None => {
                instance["data"].as_object_mut().unwrap().remove(field);
            }
        }
        let error = both_refuse::<Event>("event.json", instance);
        assert!(error.contains(&format!("data.{field}")), "{error}");
    }
    let mut fired = run.clone();
    fired["data"]["job_id"] = json!("job_1");
    let parsed = agrees::<Event>("event.json", &fired);
    let scope = serde_json::from_value::<Event>(parsed).unwrap().run_scope();
    assert_eq!(scope.and_then(|s| s.job_id), Some("job_1".to_owned()));
    // Other topics need none of them.
    let gap = Event::new(1, TIME, "daemon", "daemon.gap", Data::new()).unwrap();
    assert_eq!(gap.run_scope(), None);
}

#[test]
fn an_events_type_is_its_topic_then_a_snake_case_event() {
    let run = example("event.json");
    let daemon = example("event-daemon.json");
    // G3: topic run with type daemon.gap, and every other shape the rule refuses.
    for (base, wrong) in [
        (&run, "daemon.gap"),
        (&daemon, "run.text"),
        (&run, "run"),
        (&run, "run."),
        (&run, "run.Text"),
        (&run, "run..text"),
        (&run, "run.text-x"),
        (&run, "run.text."),
        (&run, ".run.text"),
    ] {
        let error = both_refuse::<Event>("event.json", with(base, "type", json!(wrong)));
        assert!(error.contains("type"), "{wrong}: {error}");
    }
    for topic in ["Run", "needs-you", "1run", ""] {
        let instance = with(
            &with(&daemon, "topic", json!(topic)),
            "type",
            json!(format!("{topic}.x")),
        );
        both_refuse::<Event>("event.json", instance);
    }
    // Rev 5's own names, dotted ones included (7.7).
    for (topic, kind) in [
        ("connector", "connector.login.open_url"),
        ("fleet", "fleet.agent_spawned"),
        ("needs_you", "needs_you.raised"),
        ("hook", "hook.user_prompt_submit"),
    ] {
        let instance = with(&with(&daemon, "topic", json!(topic)), "type", json!(kind));
        agrees::<Event>("event.json", &instance);
    }
    // A topic this version doesn't list keeps the rule's shape in both. JSON Schema
    // can't compare one field with another, so only the type checks that the prefix is
    // the topic there; for the listed topics, both do (above).
    let later = with(
        &with(&daemon, "topic", json!("later")),
        "type",
        json!("later.thing"),
    );
    agrees::<Event>("event.json", &later);
    let mismatched = with(&later, "type", json!("other.thing"));
    assert_valid("event.json", &mismatched);
    assert!(serde_json::from_value::<Event>(mismatched).is_err());
    both_refuse::<Event>("event.json", with(&later, "type", json!("later")));
}

#[test]
fn requests_and_the_handshake_refuse_their_wrong_shapes() {
    for instance in [
        json!({"method": "status"}),
        json!({"id": 1}),
        json!({"id": 1, "method": ""}),
        json!({"id": 1, "method": "status", "params": []}),
        json!({"id": 1, "method": "status", "params": null}),
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
        ("token", json!("")),
        ("protocol_version", json!(0)),
        ("protocol_version", json!("1")),
        ("client", json!("")),
        ("role", json!("robot")),
        ("role", json!("hook")),
        ("role", json!("App")),
        ("role", json!(null)),
        ("capabilities", json!("tool_gate")),
        ("env", json!({"A": 1})),
        ("env", json!(null)),
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
    for (field, value) in [
        ("protocol_version", json!(0)),
        ("protocol_versions", json!([])),
        ("protocol_versions", json!([0])),
        ("protocol_versions", json!(["1"])),
        ("rlxd_version", json!("")),
    ] {
        both_refuse::<HelloData>("hello-reply.json", with(&reply, field, value));
    }
}

#[test]
fn the_gate_credential_is_carried_here_and_checked_by_rlxd() {
    // Rev 5, 7.2 and 5.1: role app needs a valid credential, which rlxd checks; this
    // crate carries it.
    let app = with(&hello_base(), "gate_credential", json!("gc-secret"));
    assert_eq!(agrees::<HelloParams>("hello.json", &app), app);
    // A role-app hello without one is well formed: rlxd answers it with not_gate.
    agrees::<HelloParams>("hello.json", &hello_base());
    for bad in [json!(""), json!(7), json!(null)] {
        both_refuse::<HelloParams>("hello.json", with(&app, "gate_credential", bad));
    }

    // Neither secret shows in a logged hello.
    let hello = HelloParams::new("tok-secret", "rlxapp", "0.1.0", Role::App)
        .unwrap()
        .with_gate_credential("gc-secret")
        .unwrap();
    let shown = format!("{hello:?}");
    assert!(
        !shown.contains("gc-secret") && !shown.contains("tok-secret"),
        "{shown}"
    );
    assert!(
        shown.contains("<redacted>") && shown.contains("rlxapp"),
        "{shown}"
    );
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
        ("id", json!("0123456789AB")),
        ("id", json!("0123456789a")),
        ("id", json!("0123456789abc")),
        ("kind", json!("")),
        ("pid", json!(0)),
        ("pid", json!(-1)),
        ("pid", json!("4242")),
        ("started_at", json!("yesterday")),
        ("heartbeat_at", json!("2026-11-25 18:00")),
        ("argv", json!([])),
        ("argv", json!("rlx watch")),
        ("restartable", json!("yes")),
        ("cwd", json!(7)),
    ] {
        both_refuse::<DaemonRecord>("daemon-record.json", with(&record, field, value));
    }
}

#[test]
fn second_60_is_refused_in_every_time() {
    // RFC 3339 allows a leap second, but the validators disagree on it: check-jsonschema
    // refuses every :60, while the jsonschema crate accepts 23:59:60. So the types refuse
    // all of them, the bound both agree on. (The tester ran these five through
    // check-jsonschema, which refused each.)
    let leaps = [
        "2026-11-25T12:00:60Z",
        "2026-11-30T23:59:60Z",
        "2026-11-25T23:59:60Z",
        "2026-11-25T23:59:60+01:00",
        "2026-11-25T12:00:60.500Z",
    ];
    let event = example("event-daemon.json");
    let record = example("daemon-record.json");
    for time in leaps {
        assert_eq!(
            refused(Event::new(1, time, "daemon", "daemon.started", Data::new())),
            "time"
        );
        let parsed = serde_json::from_value::<Event>(with(&event, "time", json!(time)));
        assert!(parsed.is_err(), "{time}");
        for field in ["started_at", "heartbeat_at"] {
            let parsed = serde_json::from_value::<DaemonRecord>(with(&record, field, json!(time)));
            assert!(parsed.is_err(), "{field} {time}");
        }
        let built = DaemonRecord::new(
            "0123456789ab",
            "watch",
            1,
            "h",
            "v",
            time,
            TIME,
            vec!["rlx".to_owned()],
            false,
        );
        assert_eq!(refused(built), "started_at");
    }
    // Where both validators refuse it, the schema refuses it too.
    both_refuse::<Event>(
        "event.json",
        with(&event, "time", json!("2026-11-25T12:00:60Z")),
    );
    let mut late = with(&record, "heartbeat_at", json!("2026-11-25T12:00:60.500Z"));
    both_refuse::<DaemonRecord>("daemon-record.json", late.clone());
    late["heartbeat_at"] = json!("2026-11-25T12:00:59.999Z");
    agrees::<DaemonRecord>("daemon-record.json", &late);
}

#[test]
fn every_unsigned_integer_is_bounded_in_the_schema_as_in_its_type() {
    let big = |text: &str| -> Value { serde_json::from_str(text).unwrap() };
    let u32_over = json!(u64::from(u32::MAX) + 1);
    let u64_over = big("18446744073709551616");

    let hello = hello_base();
    both_refuse::<HelloParams>(
        "hello.json",
        with(&hello, "protocol_version", u32_over.clone()),
    );
    agrees::<HelloParams>(
        "hello.json",
        &with(&hello, "protocol_version", json!(u32::MAX)),
    );
    let reply = reply_base();
    both_refuse::<HelloData>(
        "hello-reply.json",
        with(&reply, "protocol_version", u32_over.clone()),
    );
    both_refuse::<HelloData>(
        "hello-reply.json",
        with(
            &reply,
            "protocol_versions",
            json!([1, u64::from(u32::MAX) + 1]),
        ),
    );
    let record = example("daemon-record.json");
    both_refuse::<DaemonRecord>("daemon-record.json", with(&record, "pid", u32_over.clone()));
    let refused_version = example("envelope-incompatible-protocol.json");
    both_refuse::<Envelope>(
        "envelope.json",
        with_error(&refused_version, "detail", json!({"supported": [u32_over]})),
    );

    let daemon = example("event-daemon.json");
    both_refuse::<Event>("event.json", with(&daemon, "seq", u64_over.clone()));
    agrees::<Event>("event.json", &with(&daemon, "seq", json!(u64::MAX)));
    for field in ["run_seq", "attempt"] {
        let mut run = example("event.json");
        run["data"][field] = u64_over.clone();
        both_refuse::<Event>("event.json", run.clone());
        run["data"][field] = json!(u64::MAX);
        agrees::<Event>("event.json", &run);
    }
    // Every u32 and u64 the generator wrote has its maximum.
    for file in schemas::all() {
        let schema: Value = serde_json::from_str(&file.contents).unwrap();
        let mut unbounded = Vec::new();
        find_unbounded(&schema, "", &mut unbounded);
        assert!(unbounded.is_empty(), "{}: {unbounded:?}", file.name);
    }
}

fn find_unbounded(value: &Value, at: &str, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            let unsigned = map
                .get("format")
                .and_then(Value::as_str)
                .is_some_and(|f| f.starts_with("uint"));
            if unsigned && !map.contains_key("maximum") {
                found.push(at.to_owned());
            }
            for (key, child) in map {
                find_unbounded(child, &format!("{at}/{key}"), found);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                find_unbounded(child, &format!("{at}/{i}"), found);
            }
        }
        _ => {}
    }
}

#[test]
fn a_hello_rlxd_cannot_parse_can_still_be_told_its_version_is_incompatible() {
    // 7.2: a version rlxd doesn't serve gets incompatible_protocol, never a parse error.
    // rlxd reads the version from the raw params first.
    let hello = hello_base();
    let later = json!({"protocol_version": 2, "token": "t", "who": {"client": "rlxapp"}});
    for (params, version) in [
        (with(&hello, "protocol_version", json!(0)), Some(0)),
        (
            with(&hello, "protocol_version", json!(u64::from(u32::MAX) + 1)),
            Some(u64::from(u32::MAX) + 1),
        ),
        (later, Some(2)),
        (with(&hello, "protocol_version", json!("1")), None),
        (without(&hello, "protocol_version"), None),
    ] {
        assert!(
            serde_json::from_value::<HelloParams>(params.clone()).is_err(),
            "{params}"
        );
        assert_eq!(protocol_version_of(&params), version, "{params}");
        if let Some(version) = version {
            let reply = Envelope::err(ErrorBody::incompatible_protocol(version)).with_id(id(1));
            let printed = serde_json::to_value(&reply).unwrap();
            agrees::<Envelope>("envelope.json", &printed);
            assert!(
                printed["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(&version.to_string())
            );
        }
    }
    assert_eq!(
        protocol_version_of(&hello),
        Some(u64::from(PROTOCOL_VERSION))
    );
}

#[test]
fn the_cli_json_lines_topics_have_their_prefix_checked_too() {
    // 7.8: install and sign-in print event envelopes from install.sh, install.ps1 and
    // Python, which don't use these types, so the schema's check is theirs.
    let daemon = example("event-daemon.json");
    let on =
        |topic: &str, kind: &str| with(&with(&daemon, "topic", json!(topic)), "type", json!(kind));
    for (topic, kind) in [
        ("install", "install.step"),
        ("install", "install.done"),
        ("signin", "signin.open_url"),
        ("signin", "signin.signed_in"),
        ("upgrade", "upgrade.done"),
    ] {
        agrees::<Event>("event.json", &on(topic, kind));
    }
    for (topic, kind) in [
        ("install", "signin.done"),
        ("signin", "install.step"),
        ("upgrade", "install.done"),
    ] {
        both_refuse::<Event>("event.json", on(topic, kind));
    }
}

// --- status ---------------------------------------------------------------------------------

fn status_paths() -> Paths {
    Paths::new(
        "/h",
        "/h/run/rlxd.sock",
        "/h/run/rlxd.lock",
        "/h/rlxd/token",
        "/h/rlxd/logs",
    )
}

#[test]
fn a_status_validates_and_prints_back_as_itself() {
    let status = example("status-reply.json");
    assert_eq!(agrees::<StatusData>("status-reply.json", &status), status);
    let windows = with(&status, "pipe_acl", json!("current_user"));
    assert_eq!(agrees::<StatusData>("status-reply.json", &windows), windows);
    let mut brain = status.clone();
    brain["brain"]["pid"] = json!(77);
    assert_eq!(agrees::<StatusData>("status-reply.json", &brain), brain);
    let mut signed_out = status.clone();
    signed_out["claude"] = json!({"installed": false, "signed_in": false});
    assert_eq!(
        agrees::<StatusData>("status-reply.json", &signed_out),
        signed_out
    );
    // What a later rlxd adds, at the top or inside, is ignored.
    let mut later = with(&status, "later", json!({"x": 1}));
    later["clients"]["kiosk"] = json!(2);
    later["claude"]["model"] = json!("x");
    assert_eq!(agrees::<StatusData>("status-reply.json", &later), status);
}

#[test]
fn the_status_builder_prints_what_its_schema_accepts() {
    let status = StatusData::new("0.10.1", 42, "linux", status_paths()).unwrap();
    let printed =
        agrees::<StatusData>("status-reply.json", &serde_json::to_value(&status).unwrap());
    assert_eq!(printed["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(printed["gate"], StatusData::GATE_SIGNED_APP_ONLY);
    assert_eq!(printed["seq"], 0);
    assert_eq!(
        printed["clients"],
        json!({"app": 0, "tui": 0, "remote": 0, "cli": 0})
    );
    assert_eq!(printed["brain"], json!({"restarts": 0, "handled": 0}));
    assert_eq!(
        printed["claude"],
        json!({"installed": false, "signed_in": false})
    );
    assert_eq!(printed["runs"], json!({"live": 0, "queued": 0}));
    assert_eq!(printed["jobs"], json!({"count": 0}));
    assert_eq!(printed.get("pipe_acl"), None);

    assert_eq!(
        refused(StatusData::new("", 42, "linux", status_paths())),
        "version"
    );
    assert_eq!(
        refused(StatusData::new("0.10.1", 0, "linux", status_paths())),
        "pid"
    );
    assert_eq!(
        refused(StatusData::new("0.10.1", 42, "", status_paths())),
        "platform"
    );

    let mut broken = status.clone();
    broken.gate.clear();
    assert!(
        serde_json::to_value(&broken).is_err(),
        "an empty gate does not print"
    );
    let mut broken = status;
    broken.pipe_acl = Some(String::new());
    assert!(
        serde_json::to_value(&broken).is_err(),
        "an empty pipe_acl does not print"
    );
}

#[test]
fn a_status_refuses_its_wrong_shapes() {
    let status = example("status-reply.json");
    for field in [
        "version",
        "pid",
        "platform",
        "protocol_version",
        "capabilities",
        "seq",
        "clients",
        "gate",
        "brain",
        "claude",
        "runs",
        "jobs",
        "paths",
    ] {
        both_refuse::<StatusData>("status-reply.json", without(&status, field));
    }
    let u32_over = json!(u64::from(u32::MAX) + 1);
    for (field, value) in [
        ("version", json!("")),
        ("pid", json!(0)),
        ("pid", u32_over.clone()),
        ("platform", json!("")),
        ("protocol_version", json!(0)),
        ("protocol_version", u32_over.clone()),
        ("seq", json!(-1)),
        ("gate", json!("")),
        ("pipe_acl", json!("")),
        ("pipe_acl", Value::Null),
        ("clients", json!({"app": 0, "tui": 0, "remote": 0})),
        (
            "clients",
            json!({"app": -1, "tui": 0, "remote": 0, "cli": 0}),
        ),
    ] {
        both_refuse::<StatusData>("status-reply.json", with(&status, field, value));
    }
    for (inner, field, value) in [
        ("claude", "version", Value::Null),
        ("claude", "account", json!(1)),
        ("claude", "installed", json!("yes")),
        ("brain", "pid", Value::Null),
        ("brain", "pid", u32_over),
        ("runs", "live", json!(-1)),
        ("paths", "home", Value::Null),
    ] {
        let mut broken = status.clone();
        broken[inner][field] = value;
        both_refuse::<StatusData>("status-reply.json", broken);
    }
}

#[test]
fn a_daemon_records_pid_is_one_python_can_signal() {
    // core.registry.PID_MAX: os.kill takes a C int, so Python reads a larger pid as none.
    assert_eq!(DaemonRecord::PID_MAX, 2_147_483_647);
    assert_eq!(DaemonRecord::PID_MAX, i32::MAX as u32);
    let record = example("daemon-record.json");
    let max = json!(DaemonRecord::PID_MAX);
    assert_eq!(
        agrees::<DaemonRecord>("daemon-record.json", &with(&record, "pid", max.clone())),
        with(&record, "pid", max)
    );
    let error = both_refuse::<DaemonRecord>(
        "daemon-record.json",
        with(&record, "pid", json!(u64::from(DaemonRecord::PID_MAX) + 1)),
    );
    assert!(error.contains("pid"), "{error}");
    let argv = vec!["rlx".to_owned()];
    let built = DaemonRecord::new(
        "0123456789ab",
        "watch",
        DaemonRecord::PID_MAX + 1,
        "h",
        "v",
        "2026-11-25T12:00:00Z",
        "2026-11-25T12:00:00Z",
        argv,
        false,
    );
    assert_eq!(refused(built), "pid");
    let schema: Value = serde_json::from_str(
        &fs::read_to_string(schemas_dir().join("daemon-record.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        schema["properties"]["pid"]["maximum"],
        json!(DaemonRecord::PID_MAX)
    );
}

#[test]
fn every_pid_is_one_kill_can_take() {
    // PID_MAX is i32::MAX: a pid goes to kill(), and a larger one wraps negative.
    assert_eq!(rlxd_proto::PID_MAX, i32::MAX as u32);
    let over = json!(u64::from(rlxd_proto::PID_MAX) + 1);
    let max = json!(rlxd_proto::PID_MAX);
    let status = example("status-reply.json");
    agrees::<StatusData>("status-reply.json", &with(&status, "pid", max.clone()));
    both_refuse::<StatusData>("status-reply.json", with(&status, "pid", over.clone()));
    for pid in [max.clone(), over.clone(), json!(0)] {
        let mut brain = status.clone();
        brain["brain"]["pid"] = pid.clone();
        if pid == max {
            agrees::<StatusData>("status-reply.json", &brain);
        } else {
            let error = both_refuse::<StatusData>("status-reply.json", brain);
            assert!(error.contains("brain.pid"), "{error}");
        }
    }
    let paths = || Paths::new("h", "s", "l", "t", "g");
    let built = StatusData::new("0.10.1", rlxd_proto::PID_MAX + 1, "linux", paths());
    assert_eq!(refused(built), "pid");
    let mut status = StatusData::new("0.10.1", rlxd_proto::PID_MAX, "linux", paths()).unwrap();
    status.brain.pid = Some(rlxd_proto::PID_MAX + 1);
    assert!(
        serde_json::to_value(&status).is_err(),
        "a brain pid over PID_MAX does not print"
    );
    for (schema, pointer) in [
        ("status-reply.json", "/properties/pid/maximum"),
        ("status-reply.json", "/$defs/Brain/properties/pid/maximum"),
        ("daemon-record.json", "/properties/pid/maximum"),
    ] {
        let text = fs::read_to_string(schemas_dir().join(schema)).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value.pointer(pointer), Some(&max), "{schema}{pointer}");
    }
}
