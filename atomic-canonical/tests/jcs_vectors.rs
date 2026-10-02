//! Canonicalization vectors, one fixture per divergence.
//!
//! Each file in `tests/vectors/` carries an input document, the divergence it
//! pins, and the canonical form RFC 8785 requires. The number cases are Appendix
//! B rows: the canonical text of a double is one string and no other, so a
//! canonicalizer that writes `1e-6` where the algorithm writes `0.000001`
//! produces a different hash for the same value, and a second implementation
//! then rejects a proof this one accepts.
//!
//! `tests/vectors/INGEST-BOUNDARY.md` lists the conformance cases this entry
//! point cannot decide, because it receives an already-parsed value rather than
//! the bytes.

use std::fs;
use std::path::{Path, PathBuf};

use atomic_canonical::jcs;
use serde_json::Value;

fn vector_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors")
}

fn fixture(name: &str) -> Value {
    let path = vector_dir().join(format!("{name}.json"));
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path:?}: {e}"))
}

/// Parse the fixture's input the way any caller receiving JSON does, canonicalize
/// it, and require the exact bytes.
fn assert_canonical(name: &str) {
    let f = fixture(name);
    let text = f["input"]
        .as_str()
        .expect("fixture carries an input string");
    let value: Value = serde_json::from_str(text).expect("fixture input is valid JSON");
    let expected = f["expect"]["canonical"]
        .as_str()
        .expect("fixture expects a canonical form");
    assert_eq!(
        jcs::canonicalize(&value),
        expected,
        "{name} ({}): {}",
        f["vector"].as_str().unwrap_or("?"),
        f["divergence"].as_str().unwrap_or("")
    );
}

#[test]
fn number_decimal_below_exponent_threshold() {
    assert_canonical("number-decimal-below-exponent-threshold");
}

#[test]
fn number_decimal_2pow68() {
    assert_canonical("number-decimal-2pow68");
}

#[test]
fn number_decimal_999999999999999700000() {
    assert_canonical("number-decimal-999999999999999700000");
}

#[test]
fn number_decimal_999999999999999900000() {
    assert_canonical("number-decimal-999999999999999900000");
}

#[test]
fn number_negative_small_decimal() {
    assert_canonical("number-negative-small-decimal");
}

#[test]
fn number_negative_zero() {
    assert_canonical("number-negative-zero");
}

#[test]
fn number_exponent_9_999999999999997e22() {
    assert_canonical("number-exponent-9.999999999999997e22");
}

#[test]
fn number_exponent_1_0000000000000001e23() {
    assert_canonical("number-exponent-1.0000000000000001e23");
}

#[test]
fn number_exponent_9_999999999999997e_minus_7() {
    assert_canonical("number-exponent-9.999999999999997e-7");
}

#[test]
fn number_rounded_to_its_double() {
    assert_canonical("number-rounded-to-its-double");
}

/// The accept half of the depth condition: a document at 128 containers
/// canonicalizes, and the delegate walks it on the heap rather than the call
/// stack. The refusal half needs a fallible boundary -- see
/// `tests/vectors/INGEST-BOUNDARY.md`.
#[test]
fn depth_at_the_cap_is_canonicalized() {
    let f = fixture("depth-at-the-cap-is-canonicalized");
    assert!(f["input_generated"].as_str().is_some());

    let mut value = Value::Null;
    for _ in 0..128 {
        value = Value::Array(vec![value]);
    }
    let canonical = jcs::canonicalize(&value);
    assert_eq!(
        canonical,
        format!("{}null{}", "[".repeat(128), "]".repeat(128))
    );
}
