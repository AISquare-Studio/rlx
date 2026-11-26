//! The checked-in schemas (`rlxd/schemas/v1/`): up to date with the types, valid draft
//! 2020-12 schemas, and in agreement with the types (handoff 0.10.3 rev 5, 7.2, 7.3 and
//! 4.1). Every document either both accept or both refuse, and every builder prints
//! what its schema accepts.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rlxd_proto::{
    Auth, BRAIN_PROTOCOL, BRAIN_PROTOCOLS, BrainInput, BrainOutput, DaemonRecord, Data, Detail,
    Envelope, ErrorBody, Event, HOOK_ENV_NAMES, HOOK_ENV_PREFIXES, HOOK_ENV_SECRET_WORDS,
    HOOK_EVENTS, HelloData, HelloParams, HereData, HookEventData, HookParams, HookReply,
    InstalledRlx, InstalledRlxd, Invalid, PRE_TOOL_USE, PROTOCOL_VERSION, PROTOCOL_VERSIONS, Paths,
    Request, RequestId, Role, RunScope, SCHEMA_VERSION, Setting, SettingsChanged,
    SettingsDeleteParams, SettingsGetParams, SettingsList, SettingsListParams, SettingsSetParams,
    StatusData, SubscribeParams, b64, brain_codes, codes, daemon_kinds, deny_output, event_types,
    hook_codes, hook_env_forwarded, hook_event_type, methods, protocol_version_of,
    schema_version_of, schemas, send_safety_lowers, send_safety_modes, setting_is_protected,
    setting_keys, topics,
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
        "events-subscribe.json",
        "daemon-record.json",
        "here-reply.json",
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
    let dev = with(&status, "dev_features", json!(["gate-dev-mode"]));
    assert_eq!(agrees::<StatusData>("status-reply.json", &dev), dev);
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
    assert_eq!(
        printed["brain"],
        json!({"restarts": 0, "handled": 0, "queued": 0})
    );
    assert_eq!(
        printed["claude"],
        json!({"installed": false, "signed_in": false})
    );
    assert_eq!(printed["runs"], json!({"live": 0, "queued": 0}));
    assert_eq!(printed["jobs"], json!({"count": 0}));
    assert_eq!(printed.get("pipe_acl"), None);
    assert_eq!(
        printed.get("dev_features"),
        None,
        "a release build has none"
    );

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
        ("dev_features", json!([""])),
        ("dev_features", json!("gate-dev-mode")),
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
    let here = example("here-reply.json");
    agrees::<HereData>("here-reply.json", &with(&here, "pid", max.clone()));
    for pid in [over.clone(), json!(0)] {
        let error = both_refuse::<HereData>("here-reply.json", with(&here, "pid", pid));
        assert!(error.contains("pid"), "{error}");
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
        ("here-reply.json", "/properties/pid/maximum"),
    ] {
        let text = fs::read_to_string(schemas_dir().join(schema)).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value.pointer(pointer), Some(&max), "{schema}{pointer}");
    }
}

// --- events.subscribe -----------------------------------------------------------------------

const SUBSCRIBE: &str = "events-subscribe.json";

#[test]
fn subscribe_params_validate_and_print_back_as_themselves() {
    let whole = json!({"topics": ["run", "needs_you"], "since": 41, "follow": false});
    assert_eq!(agrees::<SubscribeParams>(SUBSCRIBE, &whole), whole);
    // Every field may be left out: every topic, live events only, until rlxd stops.
    let printed = agrees::<SubscribeParams>(SUBSCRIBE, &json!({}));
    assert_eq!(printed, json!({"follow": true}));
    let empty: SubscribeParams = serde_json::from_value(json!({})).unwrap();
    assert_eq!(empty, SubscribeParams::new());
    assert_eq!(empty, SubscribeParams::default());
    for since in [0, 1, u64::MAX] {
        agrees::<SubscribeParams>(SUBSCRIBE, &json!({ "since": since }));
    }
    for topic in topics::ALL {
        agrees::<SubscribeParams>(SUBSCRIBE, &json!({ "topics": [topic] }));
    }
    // A topic this crate doesn't list is rlxd's to refuse, with unknown_topic, so that a
    // later rlxd's topic needs no new types; an unknown field is ignored.
    agrees::<SubscribeParams>(SUBSCRIBE, &json!({"topics": ["later"], "later": 1}));
}

#[test]
fn subscribe_params_refuse_their_wrong_shapes() {
    let u64_over: Value = serde_json::from_str("18446744073709551616").unwrap();
    for wrong in [
        json!({"topics": []}),
        json!({"topics": null}),
        json!({"topics": "run"}),
        json!({"topics": ["Run"]}),
        json!({"topics": ["run.text"]}),
        json!({"topics": [""]}),
        json!({"topics": [1]}),
        json!({"since": -1}),
        json!({"since": 1.5}),
        json!({"since": "1"}),
        json!({"since": null}),
        json!({ "since": u64_over }),
        json!({"follow": "yes"}),
        json!({"follow": null}),
        json!([]),
        json!([["run"], 41]),
        json!("run"),
    ] {
        both_refuse::<SubscribeParams>(SUBSCRIBE, wrong);
    }
}

#[test]
fn subscribe_params_builders_print_what_the_schema_accepts_and_refuse_the_rest() {
    let built = SubscribeParams::new()
        .with_topics(["daemon", "run"])
        .unwrap()
        .with_since(7)
        .with_follow(false);
    let printed = serde_json::to_value(&built).unwrap();
    assert_eq!(
        printed,
        json!({"topics": ["daemon", "run"], "since": 7, "follow": false})
    );
    agrees::<SubscribeParams>(SUBSCRIBE, &printed);
    let no_topics: [&str; 0] = [];
    assert_eq!(
        refused(SubscribeParams::new().with_topics(no_topics)),
        "topics"
    );
    assert_eq!(
        refused(SubscribeParams::new().with_topics(["Run"])),
        "topics"
    );
    let mut broken = built;
    broken.topics = Some(Vec::new());
    assert!(serde_json::to_value(&broken).is_err());
}

#[test]
fn the_stream_names_its_method_and_the_daemon_types_their_topic() {
    assert_eq!(methods::EVENTS_SUBSCRIBE, "events.subscribe");
    assert_eq!(methods::DEBUG_EMIT, "debug.emit");
    for kind in [
        event_types::DAEMON_STARTED,
        event_types::DAEMON_STOPPING,
        event_types::DAEMON_GAP,
    ] {
        let event = Event::new(1, TIME, topics::DAEMON, kind, Data::new()).unwrap();
        agrees::<Event>("event.json", &serde_json::to_value(&event).unwrap());
    }
    for code in [
        codes::INVALID_PARAMS,
        codes::UNKNOWN_TOPIC,
        codes::TIMED_OUT,
        codes::STREAM_ENDED,
    ] {
        let reply = Envelope::err(ErrorBody::new(code, "m").unwrap()).with_id(id(1));
        agrees::<Envelope>("envelope.json", &serde_json::to_value(&reply).unwrap());
    }
}

#[test]
fn since_ahead_always_says_the_last_seq() {
    let reply = Envelope::err(ErrorBody::since_ahead(99, 4)).with_id(id(2));
    let printed = serde_json::to_value(&reply).unwrap();
    agrees::<Envelope>("envelope.json", &printed);
    assert_eq!(printed["error"]["code"], "since_ahead");
    assert_eq!(printed["error"]["detail"], json!({"since": 99, "head": 4}));
    assert_eq!(
        refused(ErrorBody::new(codes::SINCE_AHEAD, "m")),
        "detail.head"
    );
    let base = with_error(&error_base(), "code", json!(codes::SINCE_AHEAD));
    agrees::<Envelope>(
        "envelope.json",
        &with_error(&base, "detail", json!({"head": 4})),
    );
    both_refuse::<Envelope>("envelope.json", base.clone());
    for detail in [
        json!("text"),
        json!({}),
        json!({"since": 9}),
        json!({"head": "4"}),
        json!({"head": -1}),
        json!({"head": 4, "since": "9"}),
    ] {
        both_refuse::<Envelope>("envelope.json", with_error(&base, "detail", detail));
    }
    // incompatible_protocol's rule still holds beside it.
    let refused_version = example("envelope-incompatible-protocol.json");
    both_refuse::<Envelope>(
        "envelope.json",
        with_error(&refused_version, "detail", json!({"head": 4})),
    );
}

// --- here -----------------------------------------------------------------------------------

fn here_built() -> HereData {
    HereData::new(
        InstalledRlxd::new("0.10.3", "/u/.local/bin/rlxd"),
        InstalledRlx::found("/u/.local/bin/rlx", Some("0.10.3".to_owned())),
        "/u/.aisquare",
        "/u/.aisquare/run/rlxd.sock",
        "macos",
        Auth::new("/u/.aisquare/rlxd/token"),
    )
    .unwrap()
}

#[test]
fn a_here_reply_validates_and_prints_back_as_itself() {
    let running = example("here-reply.json");
    assert_eq!(agrees::<HereData>("here-reply.json", &running), running);
    // Not running: no pid. Running but silent: no version either.
    let stopped = with(&without(&running, "pid"), "running", json!(false));
    assert_eq!(agrees::<HereData>("here-reply.json", &stopped), stopped);
    let silent = without(&running, "version");
    assert_eq!(agrees::<HereData>("here-reply.json", &silent), silent);
    let mut shown = running.clone();
    shown["auth"]["token"] = json!("0123abcd");
    assert_eq!(agrees::<HereData>("here-reply.json", &shown), shown);
    let unsupported = with(&running, "supported", json!(false));
    let unsupported = with(&unsupported, "protocol_versions", json!([2, 3]));
    assert_eq!(
        agrees::<HereData>("here-reply.json", &unsupported),
        unsupported
    );
    let mut no_rlx = running.clone();
    no_rlx["rlx"] = json!({"installed": false});
    assert_eq!(agrees::<HereData>("here-reply.json", &no_rlx), no_rlx);
    // What a later rlxd adds, at the top or inside, is ignored.
    let mut later = with(&running, "later", json!({"x": 1}));
    later["auth"]["pair"] = json!("rlxd gate pair");
    later["rlxd"]["signed"] = json!(true);
    assert_eq!(agrees::<HereData>("here-reply.json", &later), running);
}

#[test]
fn the_here_builder_prints_what_its_schema_accepts() {
    let built = here_built();
    let printed = agrees::<HereData>("here-reply.json", &serde_json::to_value(&built).unwrap());
    assert_eq!(printed["installed"], true);
    assert_eq!(printed["running"], false);
    assert_eq!(printed["started"], false);
    assert_eq!(printed["supported"], true);
    assert_eq!(printed["version"], "0.10.3");
    assert_eq!(printed["protocol_versions"], json!(PROTOCOL_VERSIONS));
    assert_eq!(
        printed["rlxd"]["protocol_versions"],
        json!(PROTOCOL_VERSIONS)
    );
    assert_eq!(printed.get("pid"), None);
    assert_eq!(
        printed["auth"],
        json!({"token_file": "/u/.aisquare/rlxd/token"})
    );
    assert_eq!(
        serde_json::to_value(InstalledRlx::missing()).unwrap(),
        json!({"installed": false})
    );

    let new = |rlxd: InstalledRlxd, home: &str, socket: &str, platform: &str, auth: Auth| {
        HereData::new(rlxd, InstalledRlx::missing(), home, socket, platform, auth)
    };
    let rlxd = || InstalledRlxd::new("0.10.3", "/b/rlxd");
    let auth = || Auth::new("/h/rlxd/token");
    assert_eq!(refused(new(rlxd(), "", "/s", "linux", auth())), "home");
    assert_eq!(refused(new(rlxd(), "/h", "", "linux", auth())), "socket");
    assert_eq!(refused(new(rlxd(), "/h", "/s", "", auth())), "platform");
    assert_eq!(
        refused(new(rlxd(), "/h", "/s", "linux", Auth::new(""))),
        "auth.token_file"
    );
    assert_eq!(
        refused(new(
            InstalledRlxd::new("", "/b/rlxd"),
            "/h",
            "/s",
            "linux",
            auth()
        )),
        "version"
    );
    assert_eq!(
        refused(new(
            InstalledRlxd::new("1", ""),
            "/h",
            "/s",
            "linux",
            auth()
        )),
        "rlxd.path"
    );

    type Break = fn(&mut HereData);
    let breaks: [(Break, &str); 4] = [
        (|h| h.pid = Some(0), "pid"),
        (|h| h.protocol_versions.clear(), "protocol_versions"),
        (|h| h.protocol_versions = vec![0], "protocol_versions"),
        (|h| h.version = Some(String::new()), "version"),
    ];
    for (break_it, field) in breaks {
        let mut broken = here_built();
        break_it(&mut broken);
        let error = serde_json::to_value(&broken).unwrap_err().to_string();
        assert!(error.contains(field), "{field}: {error}");
    }
    let mut broken = here_built();
    broken.auth.token = Some(String::new());
    assert!(
        serde_json::to_value(&broken).is_err(),
        "an empty token does not print"
    );
    let mut broken = here_built();
    broken.rlx.version = Some(String::new());
    assert!(
        serde_json::to_value(&broken).is_err(),
        "an empty rlx version does not print"
    );
}

#[test]
fn a_here_reply_refuses_its_wrong_shapes() {
    let here = example("here-reply.json");
    for field in [
        "installed",
        "running",
        "started",
        "supported",
        "protocol_versions",
        "socket",
        "home",
        "platform",
        "auth",
        "rlxd",
        "rlx",
    ] {
        both_refuse::<HereData>("here-reply.json", without(&here, field));
    }
    let u32_over = json!(u64::from(u32::MAX) + 1);
    for (field, value) in [
        ("installed", json!(false)),
        ("running", json!("yes")),
        ("version", json!("")),
        ("version", Value::Null),
        ("pid", Value::Null),
        ("protocol_versions", json!([])),
        ("protocol_versions", json!([0])),
        ("protocol_versions", json!([u32_over])),
        ("socket", json!("")),
        ("home", json!("")),
        ("platform", json!("")),
    ] {
        both_refuse::<HereData>("here-reply.json", with(&here, field, value));
    }
    for (inner, field, value) in [
        ("auth", "token_file", json!("")),
        ("auth", "token", json!("")),
        ("auth", "token", Value::Null),
        ("rlxd", "version", json!("")),
        ("rlxd", "path", json!("")),
        ("rlxd", "protocol_versions", json!([])),
        ("rlx", "installed", json!("yes")),
        ("rlx", "version", json!("")),
        ("rlx", "version", Value::Null),
        ("rlx", "path", Value::Null),
    ] {
        let mut broken = here.clone();
        broken[inner][field] = value;
        both_refuse::<HereData>("here-reply.json", broken);
    }
}

// --- settings -----------------------------------------------------------------------------

#[test]
fn settings_documents_validate_and_print_back_as_themselves() {
    for key in [
        "theme",
        "rlxapp.sidebar",
        "rlxapp.sidebar.width",
        "later_known_key",
    ] {
        let get = json!({ "key": key });
        assert_eq!(agrees::<SettingsGetParams>("settings-get.json", &get), get);
    }
    let set = json!({"key": "theme", "value": "rlx_light", "expected_version": 3});
    assert_eq!(agrees::<SettingsSetParams>("settings-set.json", &set), set);
    let long_theme = "é".repeat(64);
    let long_sign_off = "—".repeat(512);
    for (key, value) in [
        ("theme", json!(long_theme)),
        (
            "layout",
            json!({"panes": [1, 2], "sidebar": {"open": true}}),
        ),
        ("agent_sign_off", json!("— Ava, Sam's AI Agent")),
        ("agent_sign_off", json!("")),
        ("agent_sign_off", json!(long_sign_off)),
        ("send_safety_mode", json!({"default": "draft"})),
        (
            "send_safety_mode",
            json!({"default": "standing_approval", "connectors": {"claude.ai Slack": "auto"}}),
        ),
        ("rlxapp.sidebar", json!(240)),
        ("rlxapp.flags", json!([true, "x", {"y": null}])),
        ("rlxapp.note", json!("")),
        // A bare key a later rlxd may know: its shape is that rlxd's to check.
        ("later_known_key", json!("anything")),
    ] {
        agrees::<SettingsSetParams>("settings-set.json", &json!({"key": key, "value": value}));
    }
    for list in [
        json!({}),
        json!({"prefix": "rlxapp."}),
        json!({"prefix": ""}),
    ] {
        agrees::<SettingsListParams>("settings-list.json", &list);
    }

    let set_one = json!({"key": "theme", "version": 4, "value": "rlx_light",
        "updated_at": TIME, "by_device": "Sam's MacBook"});
    assert_eq!(agrees::<Setting>("setting.json", &set_one), set_one);
    let unset = json!({"key": "send_safety_mode", "version": 0});
    assert_eq!(agrees::<Setting>("setting.json", &unset), unset);
    let list = json!({"settings": [set_one, without(&set_one, "by_device")]});
    assert_eq!(
        agrees::<SettingsList>("settings-list-reply.json", &list),
        list
    );
    agrees::<SettingsList>("settings-list-reply.json", &json!({"settings": []}));
    let changed = json!({"key": "theme", "version": 4, "by_device": "Sam's MacBook"});
    assert_eq!(
        agrees::<SettingsChanged>("settings-changed.json", &changed),
        changed
    );
    agrees::<SettingsChanged>("settings-changed.json", &without(&changed, "by_device"));
    // What a client does not know is ignored.
    agrees::<Setting>("setting.json", &with(&set_one, "later", json!(1)));
    agrees::<SettingsChanged>("settings-changed.json", &with(&changed, "later", json!(1)));
}

#[test]
fn settings_documents_refuse_their_wrong_shapes() {
    for key in [
        json!(""),
        json!("Theme"),
        json!("a..b"),
        json!(".a"),
        json!("a."),
        json!("x".repeat(129)),
        json!(1),
        json!(null),
    ] {
        both_refuse::<SettingsGetParams>("settings-get.json", json!({ "key": key }));
    }
    both_refuse::<SettingsGetParams>("settings-get.json", json!({}));
    both_refuse::<SettingsGetParams>("settings-get.json", json!(["theme"]));

    let long_theme = "x".repeat(65);
    let long_sign_off = "x".repeat(513);
    for (key, value) in [
        ("theme", json!("")),
        ("theme", json!(long_theme)),
        ("theme", json!(1)),
        ("layout", json!("wide")),
        ("layout", json!([1])),
        ("agent_sign_off", json!(long_sign_off)),
        ("agent_sign_off", json!(null)),
        ("send_safety_mode", json!("draft")),
        ("send_safety_mode", json!({"default": "yolo"})),
        ("send_safety_mode", json!({"connectors": {"s": "auto"}})),
        (
            "send_safety_mode",
            json!({"default": "draft", "connectors": {"": "auto"}}),
        ),
        (
            "send_safety_mode",
            json!({"default": "draft", "connectors": {"s": "nope"}}),
        ),
        (
            "send_safety_mode",
            json!({"default": "draft", "connectors": ["auto"]}),
        ),
        // A policy is strict: a field it doesn't know is refused, not ignored.
        ("send_safety_mode", json!({"default": "auto", "extra": 1})),
        (
            "send_safety_mode",
            json!({"default": "draft", "connectors": {}, "later": true}),
        ),
        ("rlxapp.sidebar", json!(null)),
    ] {
        both_refuse::<SettingsSetParams>("settings-set.json", json!({"key": key, "value": value}));
    }
    for wrong in [
        json!({"key": "theme"}),
        json!({"key": "theme", "value": "x", "expected_version": -1}),
        json!({"key": "theme", "value": "x", "expected_version": null}),
        json!({"value": "x"}),
        json!([]),
    ] {
        both_refuse::<SettingsSetParams>("settings-set.json", wrong);
    }
    both_refuse::<SettingsListParams>("settings-list.json", json!({ "prefix": "x".repeat(129) }));
    both_refuse::<SettingsListParams>("settings-list.json", json!({"prefix": null}));
    both_refuse::<SettingsListParams>("settings-list.json", json!([]));

    let set_one = json!({"key": "theme", "version": 4, "value": "rlx_light",
        "updated_at": TIME});
    for wrong in [
        with(
            &json!({"key": "theme", "version": 0}),
            "value",
            json!("rlx"),
        ),
        with(
            &json!({"key": "theme", "version": 0}),
            "updated_at",
            json!(TIME),
        ),
        with(
            &json!({"key": "theme", "version": 0}),
            "by_device",
            json!("d"),
        ),
        without(&set_one, "value"),
        without(&set_one, "updated_at"),
        with(&set_one, "updated_at", json!("yesterday")),
        with(&set_one, "value", json!(7)),
        with(&set_one, "value", json!(null)),
        with(&set_one, "by_device", json!("")),
        with(&set_one, "version", json!(-1)),
    ] {
        both_refuse::<Setting>("setting.json", wrong);
    }
    both_refuse::<SettingsList>(
        "settings-list-reply.json",
        json!({"settings": [without(&set_one, "value")]}),
    );
    for wrong in [
        json!({"key": "theme", "version": 0}),
        json!({"key": "theme", "version": 1, "by_device": ""}),
        json!({"key": "Theme", "version": 1}),
    ] {
        both_refuse::<SettingsChanged>("settings-changed.json", wrong);
    }
}

#[test]
fn settings_builders_print_what_their_schemas_accept() {
    let set = SettingsSetParams::new(setting_keys::THEME, json!("rlx"))
        .unwrap()
        .expecting(0);
    let printed = serde_json::to_value(&set).unwrap();
    assert_eq!(
        printed,
        json!({"key": "theme", "value": "rlx", "expected_version": 0})
    );
    agrees::<SettingsSetParams>("settings-set.json", &printed);
    assert_eq!(refused(SettingsSetParams::new("theme", json!(1))), "value");
    assert_eq!(refused(SettingsSetParams::new("a..b", json!(1))), "key");
    assert_eq!(refused(SettingsGetParams::new("")), "key");
    let list = SettingsListParams::new().with_prefix("rlxapp.");
    agrees::<SettingsListParams>("settings-list.json", &serde_json::to_value(&list).unwrap());

    let unset = Setting::unset(setting_keys::SEND_SAFETY_MODE).unwrap();
    agrees::<Setting>("setting.json", &serde_json::to_value(&unset).unwrap());
    let mode = json!({"default": send_safety_modes::DRAFT});
    let set_one = Setting::new("send_safety_mode", 2, mode, TIME, Some("d".to_owned())).unwrap();
    agrees::<Setting>("setting.json", &serde_json::to_value(&set_one).unwrap());
    assert_eq!(
        refused(Setting::new("theme", 0, json!("x"), TIME, None)),
        "version"
    );
    assert_eq!(
        refused(Setting::new("theme", 1, json!("x"), "now", None)),
        "updated_at"
    );
    let listed = SettingsList::new(vec![set_one.clone(), unset]).unwrap();
    agrees::<SettingsList>(
        "settings-list-reply.json",
        &serde_json::to_value(&listed).unwrap(),
    );

    let changed = SettingsChanged::new("theme", 3, None).unwrap();
    let data = changed.to_data().unwrap();
    assert_eq!(
        Value::Object(data.clone()),
        json!({"key": "theme", "version": 3})
    );
    let event = Event::new(
        9,
        TIME,
        topics::SETTINGS,
        event_types::SETTINGS_CHANGED,
        data,
    )
    .unwrap();
    agrees::<Event>("event.json", &serde_json::to_value(&event).unwrap());
    assert_eq!(refused(SettingsChanged::new("theme", 0, None)), "version");
    // The settings topic's types start with it, as every topic's must.
    let wrong = json!({"schema_version": 1, "seq": 1, "time": TIME, "topic": "settings",
        "type": "daemon.started", "data": {}});
    both_refuse::<Event>("event.json", wrong);
}

#[test]
fn the_settings_api_names_its_methods_codes_and_gated_keys() {
    assert_eq!(methods::SETTINGS_GET, "settings.get");
    assert_eq!(methods::SETTINGS_SET, "settings.set");
    assert_eq!(methods::SETTINGS_LIST, "settings.list");
    assert!(topics::ALL.contains(&topics::SETTINGS));
    assert_eq!(methods::SETTINGS_DELETE, "settings.delete");
    assert_eq!(event_types::SETTINGS_DELETED, "settings.deleted");
    for key in [
        "send_safety_mode",
        "policy.app_absent",
        "policy.connectors.slack",
    ] {
        assert!(setting_is_protected(key), "{key}");
    }
    for key in [
        "theme",
        "layout",
        "agents.ava",
        "rlxapp.policy",
        "policyx.y",
        "policy",
    ] {
        assert!(!setting_is_protected(key), "{key}");
    }
    for key in setting_keys::KNOWN {
        assert!(
            !key.contains('.'),
            "known keys are bare; dotted ones are free"
        );
    }
    for code in [codes::CONFLICT, codes::UNKNOWN_SETTING] {
        let reply = Envelope::err(ErrorBody::new(code, "m").unwrap()).with_id(id(1));
        agrees::<Envelope>("envelope.json", &serde_json::to_value(&reply).unwrap());
    }
}

#[test]
fn send_safety_lowers_compares_every_connectors_mode() {
    let lowers = |before: Option<Value>, after: Value| send_safety_lowers(before.as_ref(), &after);
    assert!(
        lowers(None, json!({"default": "auto"})),
        "never set reads as draft"
    );
    assert!(!lowers(None, json!({"default": "draft"})));
    assert!(!lowers(
        Some(json!({"default": "auto"})),
        json!({"default": "draft"})
    ));
    // The floor stays auto, but gmail goes from draft (the default) to auto.
    assert!(lowers(
        Some(json!({"default": "draft", "connectors": {"slack": "auto"}})),
        json!({"default": "draft", "connectors": {"slack": "auto", "gmail": "auto"}})
    ));
    // A connector's own mode removed: it falls back to a less safe default.
    assert!(lowers(
        Some(json!({"default": "auto", "connectors": {"slack": "draft"}})),
        json!({"default": "auto"})
    ));
    // Raised for one, the same for the rest.
    assert!(!lowers(
        Some(json!({"default": "standing_approval", "connectors": {"slack": "auto"}})),
        json!({"default": "standing_approval", "connectors": {"slack": "draft"}})
    ));
    assert!(!lowers(
        Some(json!({"default": "draft"})),
        json!("not a mode")
    ));
    assert_eq!(
        send_safety_modes::ALL,
        ["draft", "standing_approval", "auto"]
    );
}

#[test]
fn a_setting_value_nests_at_most_32_deep_and_the_types_say_so() {
    let nested = |depth: usize| -> Value {
        serde_json::from_str(&format!("{}1{}", "[".repeat(depth), "]".repeat(depth))).unwrap()
    };
    let ok = SettingsSetParams::new("rlxapp.deep", nested(32));
    assert!(ok.is_ok(), "{ok:?}");
    assert_eq!(
        refused(SettingsSetParams::new("rlxapp.deep", nested(33))),
        "value"
    );
    let objects = |depth: usize| (0..depth).fold(json!(1), |inner, _| json!({ "a": inner }));
    assert!(SettingsSetParams::new("layout", objects(32)).is_ok());
    assert_eq!(
        refused(SettingsSetParams::new("layout", objects(33))),
        "value"
    );
    // The schema can't count depth: it takes what the type refuses, as documented.
    let deep = json!({"key": "rlxapp.deep", "value": nested(40)});
    assert_valid("settings-set.json", &deep);
    assert!(serde_json::from_value::<SettingsSetParams>(deep).is_err());
    let stored = json!({"key": "rlxapp.deep", "version": 1, "value": nested(40),
        "updated_at": TIME});
    assert!(serde_json::from_value::<Setting>(stored).is_err());
}

#[test]
fn settings_delete_params_validate_and_refuse_their_wrong_shapes() {
    let whole = json!({"key": "rlxapp.sidebar", "expected_version": 3});
    assert_eq!(
        agrees::<SettingsDeleteParams>("settings-delete.json", &whole),
        whole
    );
    agrees::<SettingsDeleteParams>("settings-delete.json", &json!({"key": "policy.app_absent"}));
    for wrong in [
        json!({}),
        json!({"key": ""}),
        json!({"key": "A"}),
        json!({"key": "theme", "expected_version": -1}),
        json!({"key": "theme", "expected_version": null}),
        json!([]),
    ] {
        both_refuse::<SettingsDeleteParams>("settings-delete.json", wrong);
    }
    let built = SettingsDeleteParams::new("theme").unwrap().expecting(2);
    assert_eq!(
        serde_json::to_value(&built).unwrap(),
        json!({"key": "theme", "expected_version": 2})
    );
    assert_eq!(refused(SettingsDeleteParams::new("a..b")), "key");
}

#[test]
fn send_safety_mode_is_strict_for_a_writer_and_additive_for_a_reader() {
    // A later rlxd's field, in what a client reads: taken, and printed back with it, so a
    // new field never costs an older client its settings.list.
    let later = json!({"default": "draft", "connectors": {"slack": "auto"}, "later": 1});
    let setting = json!({"key": "send_safety_mode", "version": 7, "value": later,
                         "updated_at": "2026-11-26T12:00:00.000Z", "by_device": "Sam MacBook"});
    assert_eq!(agrees::<Setting>("setting.json", &setting), setting);
    let list = json!({ "settings": [setting] });
    assert_eq!(
        agrees::<SettingsList>("settings-list-reply.json", &list),
        list
    );
    assert!(rlxd_proto::check_setting_value("value", "send_safety_mode", &later).is_ok());
    assert!(!send_safety_lowers(
        Some(&json!({"default": "draft"})),
        &json!({"default": "draft", "later": 1})
    ));

    // What a writer sends: refused, by the schema and the type alike.
    let error = both_refuse::<SettingsSetParams>(
        "settings-set.json",
        json!({"key": "send_safety_mode", "value": later}),
    );
    assert!(error.contains("no other field"), "{error}");
    assert!(rlxd_proto::check_setting_value_to_write("value", "send_safety_mode", &later).is_err());
    // A reader still refuses a value that is not a policy at all.
    both_refuse::<Setting>(
        "setting.json",
        json!({"key": "send_safety_mode", "version": 7, "value": {"default": "often"},
               "updated_at": "2026-11-26T12:00:00.000Z"}),
    );
}

// --- the brain protocol (1.6) -----------------------------------------------------------------

#[test]
fn brain_frames_validate_and_print_back_as_themselves() {
    for frame in [
        json!({"type": "hello", "protocol": 1, "rlxd_version": "0.10.3",
               "env_allowlist": ["AISQUARE_ROLE", "PATH"], "env_prefixes": ["CLAUDE"],
               "env_secret_words": ["TOKEN"]}),
        json!({"type": "request", "id": 7, "method": "ping", "params": {}}),
        json!({"type": "request", "id": 8, "method": "hook", "params": {"event": "stop"}}),
    ] {
        assert_eq!(agrees::<BrainInput>("brain-input.json", &frame), frame);
    }
    for frame in [
        json!({"type": "hello", "protocol": 1, "pid": 4243, "rlx_version": "0.10.3"}),
        json!({"type": "refused", "code": "incompatible_protocol", "message": "m",
               "supported": [1]}),
        json!({"type": "accepted", "id": 7}),
        json!({"type": "reply", "id": 7, "ok": true, "result": {"pid": 4243}}),
        json!({"type": "reply", "id": 8, "ok": false,
               "error": {"code": "unknown_method", "message": "m"}}),
    ] {
        assert_eq!(agrees::<BrainOutput>("brain-output.json", &frame), frame);
    }
    // Absent params mean none; what a later release adds is ignored.
    let bare = json!({"type": "request", "id": 1, "method": "ping"});
    assert_eq!(
        agrees::<BrainInput>("brain-input.json", &bare),
        json!({"type": "request", "id": 1, "method": "ping", "params": {}})
    );
    let later = json!({"type": "hello", "protocol": 1, "pid": 2, "rlx_version": "v",
                       "later": {"names": ["PATH"]}});
    agrees::<BrainOutput>("brain-output.json", &later);
}

#[test]
fn brain_frames_refuse_their_wrong_shapes() {
    for frame in [
        json!({"type": "hello", "protocol": 0, "rlxd_version": "v"}),
        json!({"type": "hello", "protocol": 1, "rlxd_version": ""}),
        json!({"type": "hello", "protocol": 1}),
        json!({"type": "request", "id": 0, "method": "ping"}),
        json!({"type": "request", "id": 1, "method": ""}),
        json!({"type": "request", "id": 1, "method": "ping", "params": []}),
        json!({"type": "accepted", "id": 1}),
        json!({"id": 1, "method": "ping"}),
    ] {
        both_refuse::<BrainInput>("brain-input.json", frame);
    }
    for frame in [
        json!({"type": "hello", "protocol": 1, "pid": 0, "rlx_version": "v"}),
        json!({"type": "hello", "protocol": 1, "pid": 2147483648u64, "rlx_version": "v"}),
        json!({"type": "refused", "code": "", "message": "m"}),
        json!({"type": "accepted", "id": 0}),
        json!({"type": "reply", "id": 1}),
        json!({"type": "reply", "id": 1, "ok": true, "result": null}),
        json!({"type": "reply", "id": 1, "ok": false, "error": null}),
        json!({"type": "request", "id": 1, "method": "ping"}),
        json!({"type": "something", "id": 1}),
    ] {
        both_refuse::<BrainOutput>("brain-output.json", frame);
    }
    // A reply says its result or its error, as `ok` says: the type holds it to that.
    for frame in [
        json!({"type": "reply", "id": 1, "ok": true}),
        json!({"type": "reply", "id": 1, "ok": false, "result": {}}),
        json!({"type": "reply", "id": 1, "ok": true, "result": {},
               "error": {"code": "c", "message": "m"}}),
    ] {
        assert!(
            serde_json::from_value::<BrainOutput>(frame.clone()).is_err(),
            "{frame}"
        );
    }
}

#[test]
fn the_brain_builders_print_what_the_schema_accepts() {
    let hello = BrainInput::hello("0.10.3").unwrap();
    let printed = agrees::<BrainInput>("brain-input.json", &serde_json::to_value(&hello).unwrap());
    assert_eq!(
        printed,
        json!({"type": "hello", "protocol": BRAIN_PROTOCOL, "rlxd_version": "0.10.3",
               "env_allowlist": HOOK_ENV_NAMES, "env_prefixes": HOOK_ENV_PREFIXES,
               "env_secret_words": HOOK_ENV_SECRET_WORDS})
    );
    assert!(BRAIN_PROTOCOLS.contains(&BRAIN_PROTOCOL));
    let request = BrainInput::request(3, "ping", Data::new()).unwrap();
    agrees::<BrainInput>("brain-input.json", &serde_json::to_value(&request).unwrap());
    assert_eq!(refused(BrainInput::request(0, "ping", Data::new())), "id");
    assert_eq!(refused(BrainInput::request(1, "", Data::new())), "method");
    assert_eq!(refused(BrainInput::hello("")), "rlxd_version");
}

#[test]
fn a_status_says_why_no_brain_runs_and_how_long_until_the_next_start() {
    let mut status = example("status-reply.json");
    status["brain"] = json!({"restarts": 3, "handled": 41, "queued": 2, "backoff_ms": 2000,
        "error": {"code": "brain_exited", "message": "it ended (exit status: 1)"}});
    assert_eq!(agrees::<StatusData>("status-reply.json", &status), status);
    status["brain"]["error"] = json!({"code": "brain_incompatible", "message": "m",
        "detail": {"rlxd": [1]}});
    assert_eq!(agrees::<StatusData>("status-reply.json", &status), status);
    for brain in [
        json!({"restarts": 0, "handled": 0, "error": null}),
        json!({"restarts": 0, "handled": 0, "backoff_ms": null}),
        json!({"restarts": 0, "handled": 0, "error": {"code": "", "message": "m"}}),
        json!({"restarts": 0, "handled": 0, "backoff_ms": -1}),
        json!({"restarts": 0, "handled": 0, "queued": -1}),
    ] {
        let wrong = with(&status, "brain", brain);
        both_refuse::<StatusData>("status-reply.json", wrong);
    }
    for code in [
        brain_codes::BRAIN_NOT_FOUND,
        brain_codes::BRAIN_START_FAILED,
        brain_codes::BRAIN_INCOMPATIBLE,
        brain_codes::BRAIN_EXITED,
        brain_codes::BRAIN_UNRESPONSIVE,
        brain_codes::BRAIN_FAILED,
    ] {
        assert!(code.starts_with("brain_"), "{code}");
    }
    assert_eq!(methods::HOOK_PING, "hook.ping");
    assert_eq!(codes::ROLE_NOT_ALLOWED, "role_not_allowed");
}

// --- hooks over the socket (1.5, 7.3) ----------------------------------------------------------

#[test]
fn hook_documents_validate_and_print_back_as_themselves() {
    let params = json!({"event": "session-start", "stdin_b64": "eyJzZXNzaW9uX2lkIjoidDEifQ==",
        "cwd": "/p", "env": {"AISQUARE_ROLE": "coder"}, "accept_ms": 5000, "deadline_ms": 113000});
    assert_eq!(agrees::<HookParams>("hook.json", &params), params);
    let run = json!({"event": "pre-tool-use", "stdin_b64": "", "env": {}, "run_id": "r",
        "attempt": 2});
    assert_eq!(agrees::<HookParams>("hook.json", &run), run);
    let bare = json!({"event": "stop", "stdin_b64": ""});
    assert_eq!(
        agrees::<HookParams>("hook.json", &bare),
        json!({"event": "stop", "stdin_b64": "", "env": {}})
    );
    let reply = json!({"stdout_b64": "c2VlZAo=", "stderr_b64": "", "exit_code": 0});
    assert_eq!(agrees::<HookReply>("hook-reply.json", &reply), reply);
    let data = json!({"session_id": "t1", "cwd": "/p"});
    assert_eq!(agrees::<HookEventData>("hook-event.json", &data), data);
    let with_run = json!({"session_id": "", "cwd": "", "run_id": "r"});
    assert_eq!(
        agrees::<HookEventData>("hook-event.json", &with_run),
        with_run
    );
}

#[test]
fn hook_documents_refuse_their_wrong_shapes() {
    for params in [
        json!({"stdin_b64": ""}),
        json!({"event": "Stop", "stdin_b64": ""}),
        json!({"event": "stop", "stdin_b64": "", "cwd": null}),
        json!({"event": "stop", "stdin_b64": "", "cwd": ""}),
        json!({"event": "stop", "stdin_b64": "", "accept_ms": 0}),
        json!({"event": "stop", "stdin_b64": "", "env": {"A": 1}}),
        json!({"event": "stop", "stdin_b64": "", "run_id": ""}),
        json!({"event": "stop", "stdin_b64": "", "attempt": 0}),
    ] {
        both_refuse::<HookParams>("hook.json", params);
    }
    // What the schema cannot say: the event is one rlxd-hook takes, and the bytes base64.
    for params in [
        json!({"event": "codex", "stdin_b64": ""}),
        json!({"event": "stop", "stdin_b64": "not base64!"}),
    ] {
        assert!(
            serde_json::from_value::<HookParams>(params.clone()).is_err(),
            "{params}"
        );
    }
    for reply in [
        json!({"stdout_b64": "", "stderr_b64": ""}),
        json!({"stdout_b64": "", "stderr_b64": "", "exit_code": "0"}),
    ] {
        both_refuse::<HookReply>("hook-reply.json", reply);
    }
    for data in [
        json!({"cwd": "/p"}),
        json!({"session_id": "t1", "cwd": "/p", "run_id": ""}),
        json!({"session_id": "t1", "cwd": "/p", "run_id": null}),
    ] {
        both_refuse::<HookEventData>("hook-event.json", data);
    }
}

#[test]
fn the_hook_builders_print_what_the_schemas_accept() {
    let env = std::collections::BTreeMap::from([("PATH".to_owned(), "/bin".to_owned())]);
    let params = HookParams::new("stop", b64::encode(b"{}"), Some("/p".into()), env)
        .unwrap()
        .with_times(5000, 53000)
        .unwrap();
    agrees::<HookParams>("hook.json", &serde_json::to_value(&params).unwrap());
    assert_eq!(
        refused(HookParams::new("nope", "", None, Default::default())),
        "event"
    );
    assert_eq!(
        refused(HookParams::new("stop", "%%%", None, Default::default())),
        "stdin_b64"
    );
    let data = HookEventData::new("t1", "/p").with_run("r1").unwrap();
    agrees::<HookEventData>("hook-event.json", &serde_json::to_value(&data).unwrap());
    assert_eq!(
        refused(HookEventData::new("t1", "/p").with_run("")),
        "run_id"
    );
}

#[test]
fn the_hooks_name_their_events_methods_and_codes() {
    assert_eq!(HOOK_EVENTS.len(), 7);
    assert!(HOOK_EVENTS.contains(&PRE_TOOL_USE));
    let types: Vec<String> = HOOK_EVENTS
        .iter()
        .filter_map(|e| hook_event_type(e))
        .collect();
    assert_eq!(
        types,
        [
            event_types::HOOK_SESSION_START,
            event_types::HOOK_USER_PROMPT_SUBMIT,
            event_types::HOOK_SESSION_END,
            event_types::HOOK_STOP,
            event_types::HOOK_STOP_FAILURE,
            event_types::HOOK_NOTIFICATION,
            event_types::HOOK_PRE_TOOL_USE,
        ]
    );
    for r#type in &types {
        let event = Event::new(1, TIME, topics::HOOK, r#type.clone(), data(json!({}))).unwrap();
        assert_valid("event.json", &serde_json::to_value(&event).unwrap());
    }
    assert_eq!(methods::HOOK, "hook");
    assert_eq!(hook_codes::HOOK_NOT_DELIVERED, "hook_not_delivered");
    let deny: Value = serde_json::from_str(&deny_output("r")).unwrap();
    assert_eq!(deny["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(HOOK_ENV_NAMES.contains(&"AISQUARE_ROLE"));
    assert!(hook_env_forwarded("CLAUDE_CONFIG_DIR"));
    assert!(!hook_env_forwarded("CLAUDE_CODE_OAUTH_TOKEN"));
}
