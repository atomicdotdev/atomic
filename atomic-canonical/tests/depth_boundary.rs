//! The depth cap, tested on the cap rather than only past it.
//!
//! `jcs::admit_document` refuses nesting past `jcs::MAX_DEPTH`, and that is
//! meant to be the only depth bound on the path. The parse that follows the
//! admission used to carry a second one, `serde_json`'s recursion limit, which
//! refuses at 128 while the admission admits 128. A document exactly at the
//! declared cap was admitted, and then refused by the parse as one that "does
//! not deserialize"; `encode_for_transport` wrote it into the
//! `Atomic-Delegation` header and `decode_from_transport` would not read it
//! back. Every test here sits on the cap or one container either side of it, so
//! a second bound at any other value shows up as a failure rather than as a
//! document nobody happened to send.
//!
//! The three fixtures under `tests/vectors/ingest/depth/` are wire bytes: `[`
//! repeated *d* times, `null`, `]` repeated *d* times, no trailing newline. Each
//! test asserts the file's length before reading it, because an editor that
//! appends a newline changes the document under test. `PROVENANCE.md` next to
//! them says they were generated here rather than copied from the suite.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use atomic_canonical::delegation;
use atomic_canonical::jcs::{self, MAX_DEPTH};
use atomic_canonical::CanonicalError;
use jcs_admit::Error as Admission;
use serde_json::Value;

/// The wire bytes of `depth/<d>.json`, length-checked before anything reads them.
fn wire(d: usize) -> Vec<u8> {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/vectors/ingest/depth")
        .join(format!("{d}.json"));
    let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    assert_eq!(
        bytes.len(),
        2 * d + 4,
        "{path:?} must be {d} open brackets, null, {d} close brackets and nothing else"
    );
    bytes
}

/// The value `depth/<d>.json` denotes, built by a loop rather than by a parse.
fn nested(d: usize) -> Value {
    (0..d).fold(Value::Null, |inner, _| Value::Array(vec![inner]))
}

/// One under the cap: admitted, and it is the value the loop builds.
#[test]
fn one_under_the_cap_is_admitted() {
    let bytes = wire(MAX_DEPTH - 1);
    let admitted = jcs::admit_document(&bytes).expect("127 containers are under the cap");
    assert_eq!(admitted, nested(MAX_DEPTH - 1));
}

/// At the cap: admitted. This is the document the second bound refused. It is
/// the value the loop builds, and canonicalizing that value writes the wire
/// bytes back, so the fixture is its own canonical form.
#[test]
fn at_the_cap_is_admitted_and_canonicalizes_to_the_same_bytes() {
    let bytes = wire(MAX_DEPTH);
    let admitted = jcs::admit_document(&bytes).expect("128 containers are at the cap, not past it");
    assert_eq!(admitted, nested(MAX_DEPTH));
    assert_eq!(
        jcs::canonicalize(&admitted).as_bytes(),
        &bytes[..],
        "the fixture is already canonical, so canonicalize must reproduce it byte for byte"
    );
}

/// At the cap, through the header: what `encode_for_transport` writes,
/// `decode_from_transport` reads back, and encoding the result again gives the
/// same header. Before the declared cap became the only cap this was the
/// failing direction: encoded without complaint, refused on the way back in.
#[test]
fn at_the_cap_the_transport_round_trip_is_the_identity() {
    let value = nested(MAX_DEPTH);
    let encoded = delegation::encode_for_transport(&value);
    let decoded = delegation::decode_from_transport(&encoded)
        .expect("a document this crate encoded is one it reads back");
    assert_eq!(decoded, value);
    assert_eq!(delegation::encode_for_transport(&decoded), encoded);
}

/// One past the cap: refused as `TooDeep` naming `MAX_DEPTH`, at the byte where
/// the crossing container opens. Every bracket is one byte, so the 129th `[`
/// sits at byte 128.
#[test]
fn one_past_the_cap_is_refused_naming_the_declared_cap() {
    let bytes = wire(MAX_DEPTH + 1);
    match jcs::admit_document(&bytes) {
        Err(CanonicalError::Admission(Admission::TooDeep { limit, offset })) => {
            assert_eq!(limit, MAX_DEPTH);
            assert_eq!(offset, MAX_DEPTH);
        }
        Err(other) => panic!("expected a depth refusal naming the declared cap, got {other}"),
        Ok(_) => panic!("129 containers must be refused, the document was admitted"),
    }
}

/// The ordering guard. The admission bounds the depth before the parse runs
/// with `serde_json`'s recursion limit off, so a document 100,000 containers
/// deep is refused at the 129th and never reaches the unbounded parse. Run on
/// a thread with a 512 KiB stack: with the order reversed, that parse recurses
/// once per container, overflows the stack and aborts the whole test process,
/// which is a failure nobody can miss.
#[test]
fn far_past_the_cap_is_an_error_on_a_small_stack_and_never_an_abort() {
    const DEPTH: usize = 100_000;
    let mut bytes = vec![b'['; DEPTH];
    bytes.extend_from_slice(b"null");
    bytes.resize(2 * DEPTH + 4, b']');

    let outcome = thread::Builder::new()
        .name("depth-guard".into())
        .stack_size(512 * 1024)
        .spawn(move || jcs::admit_document(&bytes).map(|_| ()))
        .expect("spawn the guard thread")
        .join()
        .expect("the guard thread returns rather than panicking");
    match outcome {
        Err(CanonicalError::Admission(Admission::TooDeep { limit, .. })) => {
            assert_eq!(limit, MAX_DEPTH);
        }
        Err(other) => panic!("expected a depth refusal, got {other}"),
        Ok(()) => panic!("100,000 containers must be refused, the document was admitted"),
    }
}
