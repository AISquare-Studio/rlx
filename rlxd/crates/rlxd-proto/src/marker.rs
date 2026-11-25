//! `true` and `false` as types, for the envelope's `ok`.
//!
//! serde's tagged enums take string tags only, and the envelope's discriminator is the
//! boolean `ok`. These two types serialize as the literal, refuse the other one when
//! deserializing, and have the schemas `{"const": true}` and `{"const": false}`, so the
//! derived schema for [`Outcome`](crate::Outcome) says exactly which fields go with which.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Deserializer, Unexpected};
use serde::{Deserialize, Serialize, Serializer};

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
