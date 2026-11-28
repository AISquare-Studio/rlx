//! Literals as types: `true` and `false` for the envelope's `ok`, and [`V1`] for every
//! document's `schema_version`.
//!
//! Each serializes as its literal, refuses any other value when deserializing, and has
//! the schema `{"const": <literal>}`. So the derived schema for
//! [`Outcome`](crate::Outcome) says which fields go with which `ok`, and a document of
//! another schema version fails to parse rather than misparse.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Deserializer, Unexpected};
use serde::{Deserialize, Serialize, Serializer};

use crate::SCHEMA_VERSION;

macro_rules! literal {
    ($(#[$doc:meta])* $name:ident = $value:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
        pub struct $name;

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_bool($value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Expected;
                impl de::Expected for Expected {
                    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(formatter, "the boolean {}", $value)
                    }
                }
                let value = bool::deserialize(deserializer)?;
                if value == $value {
                    Ok($name)
                } else {
                    Err(de::Error::invalid_value(Unexpected::Bool(value), &Expected))
                }
            }
        }

        impl JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed(stringify!($name))
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({ "const": $value })
            }
        }
    };
}

literal! {
    /// The JSON literal `true`, and nothing else.
    True = true
}

literal! {
    /// The JSON literal `false`, and nothing else.
    False = false
}

/// Schema version 1, the `schema_version` of every document this crate reads and
/// writes, and nothing else.
///
/// A document of another version, `{"schema_version": 2, …}`, fails to parse, because
/// under 7.2's versioning a new version renames or removes fields that a v1 reader would
/// otherwise misread. To report such a document, read `schema_version` from a
/// `serde_json::Value` first ([`schema_version_of`](crate::schema_version_of)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct V1;

impl V1 {
    /// The version, as a number.
    pub const fn get(self) -> u32 {
        SCHEMA_VERSION
    }
}

impl Serialize for V1 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(SCHEMA_VERSION)
    }
}

impl<'de> Deserialize<'de> for V1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let version = u64::deserialize(deserializer)?;
        if version == u64::from(SCHEMA_VERSION) {
            Ok(V1)
        } else {
            Err(de::Error::custom(format_args!(
                "schema_version {version} is not {SCHEMA_VERSION}: this document is for \
                 another protocol version"
            )))
        }
    }
}

impl JsonSchema for V1 {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("V1")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "integer", "const": SCHEMA_VERSION })
    }
}
