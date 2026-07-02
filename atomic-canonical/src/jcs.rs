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
//! Object keys are sorted by UTF-16 code units as RFC 8785 §3.2.3 specifies
//! (not by UTF-8 bytes — the two differ once keys leave the BMP, e.g. an
//! emoji key sorts after `\u{ff61}` in UTF-8 but before it in UTF-16).
//!
//! Scope note: numbers are emitted via `serde_json`'s formatter, which
//! matches RFC 8785 for the integer values our vocabulary admits; the full
//! ECMAScript number-to-string algorithm is only needed if floating-point
//! payloads are ever admitted.

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
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
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

    /// RFC 8785 §3.2.3: keys sort by UTF-16 code units. A supplementary-plane
    /// key (😀, surrogate pair D83D DE00) must sort BEFORE U+FF61 (｡) — the
    /// opposite of UTF-8 byte order, where 😀 (F0 9F …) follows ｡ (EF BD A1).
    #[test]
    fn keys_sort_by_utf16_code_units_not_utf8_bytes() {
        let v = json!({ "\u{ff61}": 1, "😀": 2 });
        assert_eq!(
            canonicalize(&v),
            "{\"😀\":2,\"\u{ff61}\":1}",
            "supplementary-plane keys must sort by UTF-16 code units"
        );
    }
}
