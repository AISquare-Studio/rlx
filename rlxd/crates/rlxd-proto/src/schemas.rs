//! The JSON Schemas of the wire types, as `rlxd/schemas/v1/` holds them.
//!
//! [`all`] is the whole of that directory: the `gen-schemas` binary writes it, and
//! `gen-schemas --check` (and this crate's tests) fail when the checked-in files differ.

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema};

use crate::{DaemonRecord, Envelope, Event, HelloData, HelloParams, Request};

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
        file::<Envelope>("envelope.json"),
        file::<Event>("event.json"),
        list_of::<Event>(
            "event-list.json",
            "EventList",
            "Events as a JSON array, such as `jq -s .` makes of `rlxd events`' lines.",
        ),
        file::<HelloParams>("hello.json"),
        file::<HelloData>("hello-reply.json"),
        file::<Request>("request.json"),
    ];
    files.sort_by_key(|file| file.name);
    files
}

/// The schema of `T`, as the files hold it: draft 2020-12.
pub fn schema_for<T: JsonSchema>() -> Schema {
    SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<T>()
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
