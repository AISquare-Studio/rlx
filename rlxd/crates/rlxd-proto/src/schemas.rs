//! The JSON Schemas of the wire types, as `rlxd/schemas/v1/` holds them.
//!
//! [`all`] is the whole of that directory: the `gen-schemas` binary writes it, and
//! `gen-schemas --check` (and this crate's tests) fail when the checked-in files differ.

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema};

use crate::{Envelope, Event, HelloData, HelloParams, Request};

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
        file::<Envelope>("envelope.json"),
        file::<Event>("event.json"),
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
    let schema = schema_for::<T>();
    let mut contents =
        serde_json::to_string_pretty(&schema).expect("a schema is a JSON value, and prints");
    contents.push('\n');
    SchemaFile { name, contents }
}
