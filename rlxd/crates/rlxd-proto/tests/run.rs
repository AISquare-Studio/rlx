//! The runner's and spaces' wire types (handoff 0.10.3, 2.5 and 7.5): the types and their
//! checked-in schemas accept and refuse the same documents.

use std::path::Path;

use rlxd_proto::{
    RunCancelData, RunEvent, RunId, RunKind, RunRef, RunStartParams, RunState, SpaceParams, schemas,
};
use serde::Deserialize;
use serde_json::{Value, json};

fn validator(name: &str) -> jsonschema::Validator {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas/v1")
        .join(name);
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    jsonschema::options()
        .should_validate_formats(true)
        .build(&schema)
        .unwrap()
}

fn valid(name: &str, instance: &Value) -> bool {
    validator(name).is_valid(instance)
}

const ID: &str = "0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f";

/// 7.5's own example of run.start's params, with a lower-case UUID.
fn contract_start() -> Value {
    json!({"run_id": ID, "kind": "task", "prompt": "…",
        "mount": {"kind": "connectors", "connectors": ["claude.ai Slack"]},
        "needs_connectors": ["claude.ai Slack"], "output_schema": {},
        "persist_session": true, "space": "default", "append_system_prompt": "…",
        "partial": true, "timeout_s": 900, "deadline_ms": 120000,
        "pinned_call": null,
        "needs_you_when": {"pointer": "/status", "equals": "needs_you",
            "question_pointer": "/question"}})
}

#[test]
fn the_contracts_run_start_reads_and_prints_back_as_itself() {
    let example = contract_start();
    assert!(valid("run-start.json", &example));
    let params = RunStartParams::deserialize(example.clone()).unwrap();
    assert_eq!(params.kind, RunKind::Task);
    assert!(params.persists());
    assert_eq!(params.pinned_call, None);
    let printed = serde_json::to_value(&params).unwrap();
    assert!(valid("run-start.json", &printed), "{printed:#}");
    assert_eq!(printed["run_id"], ID);
    assert_eq!(printed["needs_you_when"], example["needs_you_when"]);
}

#[test]
fn a_minimal_run_start_takes_the_defaults() {
    let minimal = json!({"run_id": ID, "kind": "widget_refresh", "mount": {"kind": "no_tools"}});
    assert!(valid("run-start.json", &minimal));
    let params = RunStartParams::deserialize(minimal).unwrap();
    assert_eq!(params.space, "default");
    assert_eq!(params.deadline_ms, rlxd_proto::DEFAULT_DEADLINE_MS);
    assert!(!params.persists(), "only a task persists by default");
    assert!(!params.partial);
}

#[test]
fn types_and_schemas_refuse_the_same_run_starts() {
    let base = contract_start();
    let broken = |field: &str, value: Value| {
        let mut params = base.clone();
        params[field] = value;
        params
    };
    for (what, params) in [
        (
            "an upper-case run_id",
            broken("run_id", json!(ID.to_uppercase())),
        ),
        (
            "a run_id that is no UUID",
            broken("run_id", json!("../etc")),
        ),
        ("an unknown kind", broken("kind", json!("shell"))),
        (
            "an unknown mount",
            broken("mount", json!({"kind": "everything"})),
        ),
        ("a space with a separator", broken("space", json!("a/b"))),
        ("a space with a way up", broken("space", json!(".."))),
        ("a hidden space", broken("space", json!(".x"))),
        (
            "a space past 64 bytes",
            broken("space", json!("a".repeat(65))),
        ),
        ("a zero timeout", broken("timeout_s", json!(0))),
        ("a zero deadline", broken("deadline_ms", json!(0))),
        (
            "a null persist_session",
            broken("persist_session", Value::Null),
        ),
        (
            "a pinned call whose hash is not hex",
            broken("pinned_call", json!({"tool": "t", "input_sha256": "xyz"})),
        ),
        (
            "a pointer that is not one",
            broken("needs_you_when", json!({"pointer": "status", "equals": 1})),
        ),
    ] {
        assert!(!valid("run-start.json", &params), "the schema takes {what}");
        assert!(
            RunStartParams::deserialize(params).is_err(),
            "the type takes {what}"
        );
    }
    // The schema can't name Windows' device names; the type refuses them.
    assert!(RunStartParams::deserialize(broken("space", json!("nul"))).is_err());
}

#[test]
fn space_names_follow_one_rule() {
    for good in [
        "default",
        "a",
        "work-2",
        "my_space",
        "v1.2",
        &"a".repeat(64),
    ] {
        assert!(SpaceParams::new(good).is_ok(), "{good}");
        assert!(valid("space.json", &json!({"name": good})), "{good}");
    }
    for bad in [
        "",
        ".",
        "..",
        ".x",
        "x.",
        "a..b",
        "a/b",
        r"a\b",
        "A",
        "-x",
        "x y",
        "con",
        "com1.txt",
        "lpt9",
        &"a".repeat(65),
    ] {
        let error = SpaceParams::new(bad).unwrap_err().to_string();
        assert!(error.contains("space name"), "{bad}: {error}");
        assert!(
            SpaceParams::deserialize(json!({"name": bad})).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn run_ids_are_lower_case_uuids() {
    assert!(RunId::new(ID).is_ok());
    for bad in [
        "",
        "x",
        &ID.to_uppercase(),
        &ID.replace('-', ""),
        "0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6",
        "../../etc/passwd",
    ] {
        assert!(RunId::new(bad).is_err(), "{bad}");
    }
    let reply = serde_json::to_value(RunRef::new(RunId::new(ID).unwrap())).unwrap();
    assert!(valid("run-ref.json", &reply));
    let cancelled = serde_json::to_value(RunCancelData::new(
        RunId::new(ID).unwrap(),
        true,
        RunState::Failed,
    ))
    .unwrap();
    assert!(valid("run-cancel-reply.json", &cancelled));
    assert_eq!(cancelled["state"], "failed");
}

fn event(r#type: &str, run_seq: u64, data: Value) -> Value {
    let mut data = data;
    data["run_id"] = json!(ID);
    data["run_seq"] = json!(run_seq);
    data["kind"] = json!("task");
    data["attempt"] = json!(1);
    json!({"schema_version": 1, "seq": run_seq + 10, "time": "2026-11-26T12:00:00.000Z",
        "topic": "run", "type": r#type, "data": data})
}

#[test]
fn a_run_event_list_holds_each_type_to_its_data() {
    let list = json!([
        event("run.queued", 1, json!({"position": 1})),
        event("run.started", 2, json!({"cwd": "/s", "model": "m", "tools": [],
            "mcp_servers": [{"name": "claude.ai Slack", "status": "connected"}],
            "claude_version": "2.1.314", "resumed": false})),
        event("run.text", 3, json!({"text": "hi", "partial": false})),
        event("run.tool_result", 4, json!({"call_id": "toolu_1", "tool": "t",
            "is_error": false, "content": [{"type": "text", "text": "x"}], "truncated": false})),
        event("run.rate_limited", 5, json!({"resets_at": "2026-11-26T13:00:00.000Z"})),
        event("run.some_later_type", 6, json!({"anything": true})),
        event("run.done", 7, json!({"session": null, "result": "ok",
            "usage": {"input_tokens": 1, "output_tokens": 2}, "duration_ms": 3,
            "num_turns": 1})),
        {"schema_version": 1, "seq": 30, "time": "2026-11-26T12:00:00.000Z",
            "topic": "needs_you", "type": "needs_you.cleared", "data": {"id": "n1"}},
    ]);
    assert!(valid("run-event-list.json", &list));
    for event in list.as_array().unwrap() {
        assert!(valid("run-event.json", event), "{event:#}");
    }
    for (what, broken) in [
        (
            "a done without usage",
            event(
                "run.done",
                8,
                json!({"session": null, "result": "x",
            "duration_ms": 1, "num_turns": 1}),
            ),
        ),
        (
            "a text without partial",
            event("run.text", 8, json!({"text": "x"})),
        ),
        (
            "a failure with an unknown reason",
            event(
                "run.failed",
                8,
                json!({"reason": "bored",
            "message": "m", "partial_text": ""}),
            ),
        ),
        (
            "a queue place of 0",
            event("run.queued", 8, json!({"position": 0})),
        ),
        (
            "a reset that is no time",
            event("run.rate_limited", 8, json!({"resets_at": "soon"})),
        ),
    ] {
        assert!(!valid("run-event.json", &broken), "the schema takes {what}");
        assert!(
            !valid("run-event-list.json", &json!([broken])),
            "the list takes {what}"
        );
    }
    // The run fields are event.json's rule, which a run event keeps too.
    let mut unscoped = event("run.text", 9, json!({"text": "x", "partial": false}));
    unscoped["data"].as_object_mut().unwrap().remove("run_seq");
    assert!(!valid("run-event.json", &unscoped));
}

#[test]
fn every_new_schema_is_generated() {
    let names: Vec<&str> = schemas::all().iter().map(|f| f.name).collect();
    for name in [
        "run-start.json",
        "run-ref.json",
        "run-cancel-reply.json",
        "run-event.json",
        "run-event-list.json",
        "space.json",
        "space-ensure-reply.json",
        "space-erase-reply.json",
    ] {
        assert!(names.contains(&name), "{name}: {names:?}");
    }
    let _: Option<RunEvent> = None;
}
