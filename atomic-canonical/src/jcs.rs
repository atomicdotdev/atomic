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
//! # The algorithm is delegated rather than written here
//!
//! The bytes come from `serde_json_canonicalizer`. Two of the three parts of
//! RFC 8785 are straightforward to write by hand and the third is not:
//!
//! * Object keys sort by UTF-16 code units as §3.2.3 specifies, not by UTF-8
//!   bytes — the two differ once a key leaves the BMP, e.g. an emoji key sorts
//!   after `\u{ff61}` in UTF-8 but before it in UTF-16.
//! * Strings take the short form for the seven named escapes and lowercase
//!   `\u00xx` for the rest of C0 (§3.2.2.2).
//! * **Numbers take the ECMAScript `Number::toString` algorithm** (§3.2.2.3),
//!   which `serde_json`'s formatter is not. `serde_json` writes `-0.0` where the
//!   algorithm writes `0`, keeps a `.0` on an integer-valued double where the
//!   algorithm drops it, and crosses between decimal and exponent notation at
//!   different magnitudes: `1e-6` for `0.000001`, `2.9514790517935283e+20` for
//!   `295147905179352830000`. The delegate formats through `ryu_js`, which is
//!   the ECMAScript variant the section names, and it serializes through an
//!   explicit heap stack rather than the call stack. The cases that used to
//!   diverge are pinned in `tests/vectors/`.
//!
//! Scope note: the entry point takes an already-parsed [`Value`], so faults that
//! only exist in the wire bytes cannot be decided here — a repeated object
//! member is gone before this function is called, and RFC 8785 admits an integer
//! past 2^53 by rounding it to its double. Those belong to a strict decoder at
//! the boundary where the bytes arrive; `tests/vectors/INGEST-BOUNDARY.md` names
//! the cases and what closes them.

use serde_json::Value;

/// Canonicalize a JSON value into its RFC-8785 string form.
pub fn canonicalize(value: &Value) -> String {
    // Infallible for a `Value`: there is no writer to fail against, every
    // member name is already a Rust `String`, and the delegate's only other
    // error path is a non-finite float, which `Value` cannot hold. This mirrors
    // the expectation the hand-written string helper carried before it.
    serde_json_canonicalizer::to_string(value).expect("canonicalizing a Value is infallible")
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
