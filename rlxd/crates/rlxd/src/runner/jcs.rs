//! RFC 8785, the JSON Canonicalization Scheme: a tool call's input as one exact text, whose
//! SHA-256 is its `input_sha256` (handoff 0.10.3, 5.1), so a pinned call matches an input
//! however the model spelled it.
//!
//! - **Objects:** members sorted by their names' UTF-16 code units, no whitespace.
//! - **Strings:** as ECMAScript's `JSON.stringify` writes them: `"` and `\` escaped, the
//!   control characters below U+0020 as `\b \t \n \f \r` or `\u00xx` (lower-case hex),
//!   everything else as itself, in UTF-8.
//! - **Numbers:** IEEE 754 doubles, written as ECMAScript's `Number.prototype.toString`
//!   does: the shortest digits that read back to the same double, `-0` as `0`, plain from
//!   1e-6 up to below 1e21, else with an exponent (`1e+21`, `1e-7`).

use std::fmt::Write as _;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// `value` as RFC 8785 canonical JSON.
pub fn canonical(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

/// The SHA-256, in lower-case hex, of `value`'s canonical JSON: a call's `input_sha256`.
pub fn sha256(value: &Value) -> String {
    Sha256::digest(canonical(value).as_bytes()).iter().fold(
        String::with_capacity(64),
        |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        },
    )
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        // serde_json holds no NaN or infinity; every number is read as a double.
        Value::Number(number) => out.push_str(&es_number(number.as_f64().unwrap_or(0.0))),
        Value::String(text) => string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        Value::Object(members) => {
            let mut names: Vec<&String> = members.keys().collect();
            names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (n, name) in names.into_iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                string(name, out);
                out.push(':');
                write(&members[name.as_str()], out);
            }
            out.push('}');
        }
    }
}

/// `text` as a JSON string, as `JSON.stringify` writes it.
fn string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if c < ' ' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `x` as ECMAScript's `Number.prototype.toString` writes it (ECMA-262, Number::toString).
fn es_number(x: f64) -> String {
    if x == 0.0 {
        // Both zeros.
        return "0".to_owned();
    }
    // Rust's shortest round-trip digits, as `d.ddd` and an exponent: x = d.ddd × 10^e.
    let shortest = format!("{:e}", x.abs());
    let (mantissa, exponent) = shortest.split_once('e').unwrap_or((&shortest, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let e: i64 = exponent.parse().unwrap_or(0);
    // ECMA-262's k (the digits' count) and n: x = digits × 10^(n - k).
    let k = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    let n = e + 1;
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n(
            '0',
            usize::try_from(n - k).unwrap_or(0),
        ));
    } else if 0 < n && n <= 21 {
        let at = usize::try_from(n).unwrap_or(0);
        out.push_str(&digits[..at]);
        out.push('.');
        out.push_str(&digits[at..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', usize::try_from(-n).unwrap_or(0)));
        out.push_str(&digits);
    } else {
        let exponent = n - 1;
        let sign = if exponent < 0 { '-' } else { '+' };
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{sign}{}", exponent.unsigned_abs());
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_handoffs_example_hashes_as_written() {
        let input = json!({"b": 1, "a": "x"});
        assert_eq!(canonical(&input), r#"{"a":"x","b":1}"#);
        assert_eq!(
            sha256(&input),
            "cdab067e9f3beb32d1252cfd63e492592fecbf591b0d08cadb24bb17f3864246"
        );
    }

    #[test]
    fn numbers_are_written_as_ecmascript_writes_them() {
        for (number, written) in [
            (json!(0), "0"),
            (json!(-0.0), "0"),
            (json!(1.0), "1"),
            (json!(-1), "-1"),
            (json!(4.5), "4.5"),
            (json!(0.002), "0.002"),
            (json!(0.1), "0.1"),
            (json!(1e-6), "0.000001"),
            (json!(1e-7), "1e-7"),
            (json!(1.5e-7), "1.5e-7"),
            (json!(1e20), "100000000000000000000"),
            (json!(1e21), "1e+21"),
            (json!(1.2345678901234568e20), "123456789012345680000"),
            (json!(333_333_333.333_333_3), "333333333.3333333"),
            (json!(5e-324), "5e-324"),
            (
                json!(1.797_693_134_862_315_7e308),
                "1.7976931348623157e+308",
            ),
            (json!(9_007_199_254_740_993_u64), "9007199254740992"),
        ] {
            assert_eq!(canonical(&number), written, "{number}");
        }
    }

    #[test]
    fn names_sort_by_utf16_units_and_strings_escape_as_json_stringify_does() {
        // RFC 8785's own example of member order (3.2.3).
        let input = json!({
            "\u{20ac}": "Euro Sign",
            "\r": "Carriage Return",
            "\u{fb33}": "Hebrew Letter Dalet With Dagesh",
            "1": "One",
            "\u{1f600}": "Emoji: Grinning Face",
            "\u{80}": "Control",
            "\u{f6}": "Latin Small Letter O With Diaeresis"
        });
        // Read the order in the text itself: serde_json's own map orders by name.
        let text = canonical(&input);
        let order: Vec<usize> = [
            "\"\\r\"",
            "\"1\"",
            "\"\u{80}\"",
            "\"\u{f6}\"",
            "\"\u{20ac}\"",
            "\"\u{1f600}\"",
            "\"\u{fb33}\"",
        ]
        .iter()
        .map(|name| {
            text.find(name)
                .unwrap_or_else(|| panic!("{name} in {text}"))
        })
        .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        assert_eq!(
            canonical(&json!("a\"b\\c\u{8}\t\n\u{c}\r\u{f}\u{1f} é/")),
            "\"a\\\"b\\\\c\\b\\t\\n\\f\\r\\u000f\\u001f é/\""
        );
    }

    #[test]
    fn nested_values_and_spacing_canonicalize() {
        let spelled = serde_json::from_str::<Value>(
            "{ \"z\" : [ 3 , { \"b\" : null , \"a\" : true } , \"x\" ] , \"a\" : { } }",
        )
        .unwrap();
        assert_eq!(
            canonical(&spelled),
            r#"{"a":{},"z":[3,{"a":true,"b":null},"x"]}"#
        );
        let again = json!({"a": {}, "z": [3.0, {"a": true, "b": null}, "x"]});
        assert_eq!(sha256(&spelled), sha256(&again), "3 and 3.0 are one number");
    }
}
