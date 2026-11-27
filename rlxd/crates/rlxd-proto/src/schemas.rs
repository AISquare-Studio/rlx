//! The JSON Schemas of the wire types, as `rlxd/schemas/v1/` holds them.
//!
//! [`all`] is the whole of that directory: the `gen-schemas` binary writes it, and
//! `gen-schemas --check` (and this crate's tests) fail when the checked-in files differ.

use schemars::generate::SchemaSettings;
use schemars::transform::RecursiveTransform;
use schemars::{JsonSchema, Schema};
use serde_json::Value;

use crate::{
    DaemonRecord, EnvValues, Envelope, Event, HelloData, HelloParams, HereData, JobEvent,
    JobListData, JobListParams, JobPutParams, JobRef, JobRunNowData, PtyExited, PtyKillParams,
    PtyList, PtyOutput, PtyRef, PtyResizeParams, PtyScreen, PtyScreenParams, PtySpawnParams,
    PtySubscribeParams, PtyWriteParams, Request, RunCancelData, RunEvent, RunEventsData,
    RunEventsParams, RunGetData, RunListData, RunListParams, RunRef, RunResumeData,
    RunResumeParams, RunStartParams, Setting, SettingsChanged, SettingsDeleteParams,
    SettingsGetParams, SettingsList, SettingsListParams, SettingsSetParams, SpaceEnsureData,
    SpaceEraseData, SpaceParams, StatusData, SubscribeParams,
};

/// One schema file: its name under `schemas/v1/`, and its contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaFile {
    /// The file's name, such as `envelope.json`.
    pub name: &'static str,
    /// The file's exact contents: pretty-printed JSON and a final newline.
    pub contents: String,
}

/// Every schema file of this schema version, in name order.
pub fn all() -> Vec<SchemaFile> {
    let mut files = vec![
        file::<DaemonRecord>("daemon-record.json"),
        file::<EnvValues>("env.json"),
        file::<Envelope>("envelope.json"),
        file::<Event>("event.json"),
        file::<SubscribeParams>("events-subscribe.json"),
        list_of::<Event>(
            "event-list.json",
            "EventList",
            "Events as a JSON array, such as `jq -s .` makes of `rlxd events`' lines.",
        ),
        file::<HelloParams>("hello.json"),
        file::<HelloData>("hello-reply.json"),
        file::<HereData>("here-reply.json"),
        file::<JobEvent>("job-event.json"),
        file::<JobListParams>("job-list.json"),
        file::<JobListData>("job-list-reply.json"),
        file::<JobPutParams>("job-put.json"),
        file::<JobRef>("job-ref.json"),
        file::<JobRunNowData>("job-run-now-reply.json"),
        file::<PtyExited>("pty-exited.json"),
        file::<PtyKillParams>("pty-kill.json"),
        file::<PtyList>("pty-list-reply.json"),
        file::<PtyOutput>("pty-output.json"),
        file::<PtyRef>("pty-ref.json"),
        file::<PtyResizeParams>("pty-resize.json"),
        file::<PtyScreenParams>("pty-screen.json"),
        file::<PtyScreen>("pty-screen-reply.json"),
        file::<PtySpawnParams>("pty-spawn.json"),
        file::<PtySubscribeParams>("pty-subscribe.json"),
        file::<PtyWriteParams>("pty-write.json"),
        file::<Request>("request.json"),
        file::<RunCancelData>("run-cancel-reply.json"),
        file::<RunEvent>("run-event.json"),
        list_of::<RunEvent>(
            "run-event-list.json",
            "RunEventList",
            "A run's events as a JSON array, such as `jq -s .` makes of a capture of the \
             run and needs_you topics, or of `run.events`.",
        ),
        file::<RunEventsParams>("run-events.json"),
        file::<RunEventsData>("run-events-reply.json"),
        file::<RunGetData>("run-get-reply.json"),
        file::<RunListParams>("run-list.json"),
        file::<RunListData>("run-list-reply.json"),
        file::<RunRef>("run-ref.json"),
        file::<RunResumeParams>("run-resume.json"),
        file::<RunResumeData>("run-resume-reply.json"),
        file::<RunStartParams>("run-start.json"),
        file::<StatusData>("status-reply.json"),
        file::<Setting>("setting.json"),
        file::<SettingsChanged>("settings-changed.json"),
        file::<SettingsDeleteParams>("settings-delete.json"),
        file::<SettingsGetParams>("settings-get.json"),
        file::<SettingsListParams>("settings-list.json"),
        file::<SettingsList>("settings-list-reply.json"),
        file::<SettingsSetParams>("settings-set.json"),
        file::<SpaceEnsureData>("space-ensure-reply.json"),
        file::<SpaceEraseData>("space-erase-reply.json"),
        file::<SpaceParams>("space.json"),
    ];
    files.sort_by_key(|file| file.name);
    files
}

/// The schema of `T`, as the files hold it: draft 2020-12, and every unsigned integer
/// bounded by its Rust type's maximum.
pub fn schema_for<T: JsonSchema>() -> Schema {
    SchemaSettings::draft2020_12()
        .with_transform(RecursiveTransform(integer_maxima))
        .into_generator()
        .into_root_schema_for::<T>()
}

/// Gives a `u8`, `u16`, `u32` or `u64` its type's `maximum`. schemars writes only
/// `format: uint32` and the like, which validators ignore, so without this a schema
/// would accept a number its type refuses.
fn integer_maxima(schema: &mut Schema) {
    let maximum = match schema.get("format").and_then(Value::as_str) {
        Some("uint8") => u64::from(u8::MAX),
        Some("uint16") => u64::from(u16::MAX),
        Some("uint32") => u64::from(u32::MAX),
        Some("uint64") => u64::MAX,
        _ => return,
    };
    if schema.get("maximum").is_none() {
        schema.insert("maximum".to_owned(), maximum.into());
    }
}

fn file<T: JsonSchema>(name: &'static str) -> SchemaFile {
    render(name, &schema_for::<T>())
}

/// The schema of a JSON array of `T`, under its own title.
fn list_of<T: JsonSchema>(name: &'static str, title: &str, description: &str) -> SchemaFile {
    let mut schema = schema_for::<Vec<T>>();
    schema.insert("title".to_owned(), title.into());
    schema.insert("description".to_owned(), description.into());
    render(name, &schema)
}

fn render(name: &'static str, schema: &Schema) -> SchemaFile {
    let mut contents =
        serde_json::to_string_pretty(schema).expect("a schema is a JSON value, and prints");
    contents.push('\n');
    SchemaFile { name, contents }
}
