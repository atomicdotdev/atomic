//! JSON Canonicalization Scheme (RFC 8785), the subset RtW needs.
//!
//! We hash and sign over canonical bytes, not over the authored markdown, so
//! the content hash attests the *typed node* rather than an unstructured
//! surface. Canonicalization must be deterministic: object keys sorted, no
//! insignificant whitespace, stable string/number encoding.
//!
//! This is the **single canonicalization entry point** — both the content
//! hash (`hash.rs`) and the Data Integrity proof (`proof.rs`) go through
//! `canonicalize`, so the two can never drift.
//!
//! Scope note (M0): object-key sorting is by UTF-8 byte order. RFC 8785
//! specifies sorting by UTF-16 code units; for the ASCII property names in
//! our vocabulary the two orderings are identical. Numbers are emitted via
//! `serde_json`'s formatter, which is exact for the integer/simple values we
//! use. Both are revisited if the vocabulary ever admits non-ASCII keys or
//! floating-point payloads.

use serde_json::Value;

/// Canonicalize a JSON value into its RFC-8785 string form.
pub fn canonicalize(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_json_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(out, key);
                out.push(':');
                write_value(out, &map[*key]);
            }
            out.push('}');
        }
    }
}

/// Emit a JSON string with standard escaping. `serde_json` produces a valid,
/// minimally-escaped JSON string literal (quotes included), which matches JCS
/// for the ASCII content in our records.
fn write_json_string(out: &mut String, s: &str) {
    out.push_str(&serde_json::to_string(s).expect("string serialization is infallible"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_object_keys() {
        let v = json!({ "b": 1, "a": 2, "c": 3 });
        assert_eq!(canonicalize(&v), r#"{"a":2,"b":1,"c":3}"#);
    }

    #[test]
    fn no_insignificant_whitespace_and_nesting() {
        let v = json!({ "z": [1, 2, { "y": "x" }], "a": true });
        assert_eq!(canonicalize(&v), r#"{"a":true,"z":[1,2,{"y":"x"}]}"#);
    }

    #[test]
    fn key_order_is_input_independent() {
        let a = json!({ "one": 1, "two": 2 });
        let b = json!({ "two": 2, "one": 1 });
        assert_eq!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn escapes_strings() {
        let v = json!({ "k": "a\"b\nc" });
        assert_eq!(canonicalize(&v), r#"{"k":"a\"b\nc"}"#);
    }
}
