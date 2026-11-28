//! Each wire type's `deserialize` and `serialize`, called by path from outside the crate,
//! as rlxd and rlxd-client would call them: `Event::deserialize(…)` rather than through
//! `serde_json::from_value`.
//!
//! Both must run the type's `validate()`. serde's `remote = "Self"` would make unchecked
//! inherent `deserialize` and `serialize` public on the type, and an inherent function
//! wins a `Type::f` path over the trait's. These tests fail if one comes back.

use rlxd_proto::{
    Auth, BrainInput, BrainOutput, DaemonRecord, Data, Envelope, ErrorBody, Event, HelloData,
    HelloParams, HereData, HookEventData, HookParams, InstalledRlx, InstalledRlxd, Paths,
    PtyCursor, PtyExited, PtyInfo, PtyKillParams, PtyList, PtyOutput, PtyRef, PtyResizeParams,
    PtyScreen, PtyScreenParams, PtySpawnParams, PtySubscribeParams, PtyWriteParams, Request,
    RequestId, Role, RunScope, Setting, SettingsChanged, SettingsDeleteParams, SettingsGetParams,
    SettingsList, SettingsListParams, SettingsSetParams, StatusData, SubscribeParams,
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
    let here = json!({"installed": true, "running": false, "started": false,
        "supported": true, "protocol_versions": [], "socket": "s", "home": "h",
        "platform": "linux", "auth": {"token_file": "t"},
        "rlxd": {"version": "v", "path": "p", "protocol_versions": [1]},
        "rlx": {"installed": false}});
    assert!(HereData::deserialize(here).is_err());

    assert!(SubscribeParams::deserialize(json!({"topics": []})).is_err());
    assert!(SettingsGetParams::deserialize(json!({"key": "a..b"})).is_err());
    assert!(SettingsSetParams::deserialize(json!({"key": "theme", "value": 1})).is_err());
    assert!(SettingsListParams::deserialize(json!({"prefix": null})).is_err());
    assert!(Setting::deserialize(json!({"key": "theme", "version": 0, "value": "x"})).is_err());
    assert!(SettingsList::deserialize(json!({"settings": [{"key": "", "version": 0}]})).is_err());
    assert!(SettingsChanged::deserialize(json!({"key": "theme", "version": 0})).is_err());
    assert!(SettingsDeleteParams::deserialize(json!({"key": ""})).is_err());
    assert!(ErrorBody::deserialize(json!({"code": "since_ahead", "message": "m"})).is_err());

    assert!(BrainInput::deserialize(json!({"type": "request", "id": 0, "method": "m"})).is_err());
    assert!(BrainOutput::deserialize(json!({"type": "reply", "id": 1, "ok": true})).is_err());
    assert!(HookParams::deserialize(json!({"event": "codex", "stdin_b64": ""})).is_err());
    let no_run = json!({"session_id": "t1", "cwd": "/p", "run_id": ""});
    assert!(HookEventData::deserialize(no_run).is_err());
    let pty = "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b";
    let spawn = json!({"argv": [], "cols": 80, "rows": 24});
    assert!(PtySpawnParams::deserialize(spawn).is_err());
    assert!(PtyRef::deserialize(json!({"pty_id": "x"})).is_err());
    let both = json!({"pty_id": pty, "text": "a", "paste": "b"});
    assert!(PtyWriteParams::deserialize(both).is_err());
    let resize = json!({"pty_id": pty, "cols": 0, "rows": 24});
    assert!(PtyResizeParams::deserialize(resize).is_err());
    let screen = json!({"pty_id": pty, "scrollback": 50_001});
    assert!(PtyScreenParams::deserialize(screen).is_err());
    assert!(PtyKillParams::deserialize(json!({"pty_id": ""})).is_err());
    assert!(PtySubscribeParams::deserialize(json!({"pty_id": pty})).is_err());
    let unexited = json!({"pty_id": pty, "text": "", "cursor": {"x": 0, "y": 0,
        "visible": true}, "cols": 80, "rows": 24, "alternate": false, "history": 0,
        "exited": false, "status": 1, "seq": 0});
    assert!(PtyScreen::deserialize(unexited).is_err());
    let no_pid = json!({"ptys": [{"pty_id": pty, "argv": ["sh"], "cwd": "/", "pid": 0,
        "cols": 80, "rows": 24, "started_at": TIME, "exited": false}]});
    assert!(PtyList::deserialize(no_pid).is_err());
    assert!(PtyOutput::deserialize(json!({"pty_id": pty, "bytes_b64": "abc"})).is_err());
    assert!(PtyExited::deserialize(json!({"pty_id": pty, "status": 1, "signal": 0})).is_err());

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

    let mut set = SettingsSetParams::new("theme", json!("rlx")).unwrap();
    assert!(printed(&set).is_ok());
    set.value = json!(1);
    assert!(printed(&set).is_err());
    let mut get = SettingsGetParams::new("theme").unwrap();
    assert!(printed(&get).is_ok());
    get.key.clear();
    assert!(printed(&get).is_err());
    let mut list = SettingsListParams::new();
    assert!(printed(&list).is_ok());
    list.prefix = Some("x".repeat(129));
    assert!(printed(&list).is_err());
    let mut setting = Setting::unset("theme").unwrap();
    assert!(printed(&setting).is_ok());
    setting.value = Some(json!("rlx"));
    assert!(printed(&setting).is_err());
    let mut settings = SettingsList::new(Vec::new()).unwrap();
    assert!(printed(&settings).is_ok());
    settings.settings.push(setting);
    assert!(printed(&settings).is_err());
    let mut delete = SettingsDeleteParams::new("theme").unwrap();
    assert!(printed(&delete).is_ok());
    delete.key.clear();
    assert!(printed(&delete).is_err());
    let mut changed = SettingsChanged::new("theme", 1, None).unwrap();
    assert!(printed(&changed).is_ok());
    changed.version = 0;
    assert!(printed(&changed).is_err());

    let mut hello = BrainInput::hello("0.10.3").unwrap();
    assert!(printed(&hello).is_ok());
    if let BrainInput::Hello { protocol, .. } = &mut hello {
        *protocol = 0;
    }
    assert!(printed(&hello).is_err());
    let mut reply: BrainOutput =
        serde_json::from_value(json!({"type": "reply", "id": 1, "ok": true, "result": {}}))
            .unwrap();
    assert!(printed(&reply).is_ok());
    if let BrainOutput::Reply { ok, .. } = &mut reply {
        *ok = false;
    }
    assert!(printed(&reply).is_err());
    let mut hook = HookParams::new("stop", "", None, Default::default()).unwrap();
    assert!(printed(&hook).is_ok());
    hook.event = "codex".to_owned();
    assert!(printed(&hook).is_err());
    let mut event = HookEventData::new("t1", "/p");
    assert!(printed(&event).is_ok());
    event.run_id = Some(String::new());
    assert!(printed(&event).is_err());

    let mut subscribe = SubscribeParams::new().with_topics(["run"]).unwrap();
    assert!(printed(&subscribe).is_ok());
    subscribe.topics = Some(vec!["Run".to_owned()]);
    assert!(printed(&subscribe).is_err());

    let pty = "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b";
    let mut spawn = PtySpawnParams::new(vec!["sh".to_owned()], 80, 24).unwrap();
    assert!(printed(&spawn).is_ok());
    spawn.argv.clear();
    assert!(printed(&spawn).is_err());
    let mut one = PtyRef::new(pty).unwrap();
    assert!(printed(&one).is_ok());
    one.pty_id.push('0');
    assert!(printed(&one).is_err());
    let mut write = PtyWriteParams::text(pty, "a").unwrap();
    assert!(printed(&write).is_ok());
    write.paste = Some("b".to_owned());
    assert!(printed(&write).is_err());
    let mut resize = PtyResizeParams::new(pty, 80, 24).unwrap();
    assert!(printed(&resize).is_ok());
    resize.rows = 0;
    assert!(printed(&resize).is_err());
    let mut screen_params = PtyScreenParams::new(pty).unwrap();
    assert!(printed(&screen_params).is_ok());
    screen_params.scrollback = Some(50_001);
    assert!(printed(&screen_params).is_err());
    let mut kill = PtyKillParams::new(pty).unwrap();
    assert!(printed(&kill).is_ok());
    kill.pty_id.clear();
    assert!(printed(&kill).is_err());
    let mut subscribe_pty = PtySubscribeParams::new(pty, true).unwrap();
    assert!(printed(&subscribe_pty).is_ok());
    subscribe_pty.pty_id = "X".repeat(32);
    assert!(printed(&subscribe_pty).is_err());
    let mut screen = PtyScreen::new(pty, "", PtyCursor::new(0, 0, true), 80, 24, 0).unwrap();
    assert!(printed(&screen).is_ok());
    screen.exited = true;
    assert!(printed(&screen).is_err());
    let info = PtyInfo::new(pty, vec!["sh".to_owned()], "/", 1, 80, 24, TIME).unwrap();
    let mut ptys = PtyList::new(vec![info]).unwrap();
    assert!(printed(&ptys).is_ok());
    ptys.ptys[0].pid = 0;
    assert!(printed(&ptys).is_err());
    let mut output = PtyOutput::new(pty, "AA==").unwrap();
    assert!(printed(&output).is_ok());
    output.bytes_b64 = "A".to_owned();
    assert!(printed(&output).is_err());
    let mut exited = PtyExited::new(pty, 3, None).unwrap();
    assert!(printed(&exited).is_ok());
    exited.signal = Some(0);
    assert!(printed(&exited).is_err());

    let rlxd = InstalledRlxd::new("0.10.3", "/b/rlxd");
    let auth = Auth::new("/h/rlxd/token");
    let mut here = HereData::new(rlxd, InstalledRlx::missing(), "/h", "/s", "linux", auth).unwrap();
    assert!(printed(&here).is_ok());
    here.pid = Some(0);
    assert!(printed(&here).is_err());
}
