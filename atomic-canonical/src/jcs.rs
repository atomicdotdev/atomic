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
//! # Admission is a separate step, and it happens earlier
//!
//! The delegated entry point still takes a value someone has already parsed, so
//! faults that only exist in the wire bytes cannot be decided here: a repeated
//! object member is gone before `canonicalize` is called, a document nested past
//! any bound has already been built by the time anything could decline to build
//! it, and RFC 8785 itself admits an integer past 2^53 by rounding it to its
//! double. [`admit_document`] is the entry point for a document that arrives as
//! *bytes* — off a request header, off disk, off stdin — and it decides those
//! questions where they are still answerable, under the RFC 7493 I-JSON profile
//! the signed fields here need. Every path in the workspace that receives a
//! certificate as bytes and later hashes, signs or verifies it goes through that
//! function, so the decision happens before the value exists. The cases it
//! closes are named in `tests/vectors/INGEST-BOUNDARY.md`.

use serde::Deserialize;
use serde_json::Value;

use crate::error::{CanonicalError, Result};

/// The deepest nesting a document arriving as bytes is admitted at, counted in
/// open containers: the 129th nested array or object is refused as
/// [`jcs_admit::Error::TooDeep`] naming this value. It is the only depth bound
/// on the admission path, because [`admit_document`] parses with `serde_json`'s
/// own recursion limit switched off, so the cap declared here is the cap a
/// caller can rely on rather than one a parser default happens to apply.
pub const MAX_DEPTH: usize = 128;

/// What a document arriving as bytes is admitted under.
///
/// RFC 8785 as written, plus the RFC 7493 I-JSON profile, plus integers only,
/// under [`MAX_DEPTH`]. The profile is not decoration:
///
/// - **Safe integers.** RFC 8785 section 3.2.2.3 defers number formatting to
///   ECMAScript, which has one numeric type, so a conforming implementation
///   canonicalizes `9007199254740993` to `9007199254740992` — a different
///   integer, with no error. A signed field whose value a re-checker is entitled
///   to rewrite is a field two parties can disagree about.
/// - **Integers only.** Nothing in the vocabulary needs a fractional number:
///   the one numeric field a certificate carries is `maxChanges`, a count. Two
///   implementations that never format a float can never disagree about one.
/// - **One depth cap.** [`MAX_DEPTH`] is set here rather than left to the
///   admission crate's default, because the parse that follows admission has no
///   depth bound of its own; whatever the constant says is the whole rule.
const ADMISSION: jcs_admit::Options = jcs_admit::Options::ijson()
    .integers_only(true)
    .max_depth(MAX_DEPTH);

/// Admit a JSON document that arrived as bytes, and return the value it denotes.
///
/// The refusals are the point. A repeated member name gives one document two
/// readings: two parties take the same bytes for two different documents and a
/// signature over either reading verifies, and by the time a `serde_json::Value`
/// exists the repeat is gone. Nesting past [`MAX_DEPTH`] containers is refused
/// here rather than walked, and that is the only depth bound on the path: the
/// parse that follows runs with `serde_json`'s own recursion limit switched off,
/// so a document the admission accepts is a document this crate reads back. A
/// number outside the profile above is refused rather than rounded.
///
/// The accepted document is parsed from the bytes it arrived in, not from the
/// admission's canonical output, so nothing about an accepted document changes:
/// the value, its content hash and its proof are exactly what they were. This
/// function only adds refusals.
///
/// # Errors
///
/// [`crate::CanonicalError::Admission`] when the bytes are refused, carrying the
/// named fault and its byte offset; [`crate::CanonicalError::Proof`] if the
/// admitted bytes then fail to deserialize, which is a disagreement between the
/// admission layer and `serde_json` rather than a statement about the input.
pub fn admit_document(bytes: &[u8]) -> Result<Value> {
    jcs_admit::admit_with(bytes, &ADMISSION)?;

    // Sound only because the admission above has already walked these bytes and
    // refused anything nested past MAX_DEPTH. serde_json's own recursion limit
    // is switched off so the declared cap is the only cap: left on, it refuses
    // at 128 while the admission admits 128, and a document this crate encodes
    // is one it will not read back. This parse MUST stay ordered after
    // `admit_with`. Parsed first, an input nested far enough would overflow
    // the stack instead of returning an error.
    let not_deserializable = |e: serde_json::Error| {
        CanonicalError::Proof(format!("admitted document does not deserialize: {e}"))
    };
    let mut de = serde_json::Deserializer::from_slice(bytes);
    de.disable_recursion_limit();
    let value = Value::deserialize(&mut de).map_err(not_deserializable)?;
    de.end().map_err(not_deserializable)?;
    Ok(value)
}

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

    /// `canonicalize` expects the delegate never to fail on a `Value`. That
    /// holds because a `Value` cannot carry a number the delegate refuses to
    /// format, and it holds only while `serde_json`'s `arbitrary_precision`
    /// feature is off: with it on, a `Value` keeps `1e400` as text, the delegate
    /// parses that text to infinity and returns an error, and the `expect`
    /// panics on input anyone can supply. Features unify across the workspace,
    /// so any dependency could turn it on. This fails the build the day one does.
    #[test]
    fn a_value_cannot_carry_a_number_the_delegate_refuses() {
        assert!(
            serde_json::from_str::<Value>("1e400").is_err(),
            "serde_json's arbitrary_precision feature is on, so canonicalize can panic"
        );
    }
}
