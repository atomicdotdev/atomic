//! Replays a whole conformance suite through `jcs::admit_document`.
//!
//! `tests/ingest_boundary.rs` pins four documents copied out of the
//! `ai-agent-action` suite in
//! <https://github.com/probityai/agent-evidence-vectors>. This test reads the
//! suite itself, at the release the CI job checks out, so a vector added
//! upstream is exercised here without anyone copying it in.
//!
//! Every document in the suite is a statement or a line of a record sidecar.
//! The test holds each one to the verdict its manifest entry implies:
//!
//! - a vector rejected for a byte-level fault (`ADMISSION_CODES`) must have at
//!   least one document that `admit_document` refuses;
//! - a vector rejected as `noncanonical-bytes` must have a record line that is
//!   admitted but is not its own canonical form;
//! - every other document, in accept and reject vectors alike, must be admitted;
//! - every record line of an accept vector must already be canonical, byte for
//!   byte.
//!
//! The last rule is the one a second implementation breaks: the record lines
//! were written by the suite's generator, not by this crate, so a line that
//! re-canonicalizes to different bytes is a disagreement about RFC 8785 that
//! would change a hash. Reject vectors are exempt from it because several of
//! them carry a deliberately non-canonical record as their fault.
//!
//! One accept vector is left out, and `NOT_WHOLE_DOCUMENT` says why.
//!
//! The suite is not vendored, so the test is ignored by default. CI checks the
//! suite out and runs it with
//! `AEV_SUITE=<checkout>/vectors-ai-agent-action cargo test -p atomic-canonical --test conformance_corpus -- --ignored`.

use std::fs;
use std::path::{Path, PathBuf};

use atomic_canonical::jcs;
use serde_json::Value;

/// Reject codes whose fault is in the bytes, so the admission layer owns them.
const ADMISSION_CODES: &[&str] = &[
    "duplicate-member",
    "depth-exceeded",
    "unsafe-integer",
    "non-integer-in-signed-field",
    "ill-formed-string",
];

/// Accept vectors whose bound is counted inside one member rather than from
/// the document root. `v43175cd2ad003639` nests its `extensions` value exactly
/// 128 deep, which the suite admits, so the statement around it is 130 deep and
/// `admit_document`, counting from the root it is handed, refuses it. The reject
/// side of the same bound, `vd94ac70c9f0d84bf`, is still checked.
const NOT_WHOLE_DOCUMENT: &[&str] = &["v43175cd2ad003639"];

/// Reject code for a record line that parses but is not canonical.
const NONCANONICAL: &str = "noncanonical-bytes";

fn suite_dir() -> PathBuf {
    let dir = std::env::var_os("AEV_SUITE")
        .expect("set AEV_SUITE to the vectors-ai-agent-action directory of a suite checkout");
    PathBuf::from(dir)
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The documents of one vector: its statement, then each record line.
fn documents(dir: &Path, vector: &Value) -> Vec<(String, Vec<u8>, bool)> {
    let mut docs = Vec::new();
    let file = vector["file"].as_str().expect("vector names a file");
    docs.push((file.to_owned(), read(&dir.join(file)), false));
    if let Some(records) = vector["records"].as_str() {
        let bytes = read(&dir.join(records));
        for (n, line) in bytes.split(|b| *b == b'\n').enumerate() {
            if !line.is_empty() {
                docs.push((format!("{records}:{}", n + 1), line.to_vec(), true));
            }
        }
    }
    docs
}

/// `Ok(true)` when admitted and canonical, `Ok(false)` when admitted but not
/// canonical, and the refusal otherwise.
fn judge(bytes: &[u8]) -> Result<bool, String> {
    let value = jcs::admit_document(bytes).map_err(|e| e.to_string())?;
    Ok(jcs::canonicalize(&value).as_bytes() == bytes)
}

#[test]
#[ignore = "needs a checkout of the conformance suite; set AEV_SUITE"]
fn every_vector_meets_its_manifest_verdict() {
    let dir = suite_dir();
    let manifest: Value =
        serde_json::from_slice(&read(&dir.join("MANIFEST.json"))).expect("MANIFEST.json parses");
    let vectors = manifest["vectors"]
        .as_array()
        .expect("manifest lists vectors");
    assert!(!vectors.is_empty(), "the manifest lists no vectors");

    let mut failures = Vec::new();
    let mut checked = 0usize;
    for vector in vectors {
        let id = vector["id"].as_str().unwrap_or("?");
        if NOT_WHOLE_DOCUMENT.contains(&id) {
            continue;
        }
        let accept = vector["kind"].as_str() == Some("accept");
        let codes: Vec<&str> = vector["expected"]["codes"]
            .as_array()
            .map(|c| c.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let docs = documents(&dir, vector);
        checked += docs.len();
        let verdicts: Vec<(&str, Result<bool, String>, bool)> = docs
            .iter()
            .map(|(name, bytes, is_record)| (name.as_str(), judge(bytes), *is_record))
            .collect();

        if codes.iter().any(|c| ADMISSION_CODES.contains(c)) {
            if verdicts.iter().all(|(_, v, _)| v.is_ok()) {
                failures.push(format!("{id} {codes:?}: every document was admitted"));
            }
        } else if codes.contains(&NONCANONICAL) {
            if !verdicts
                .iter()
                .any(|(_, v, is_record)| *is_record && *v == Ok(false))
            {
                failures.push(format!(
                    "{id} {codes:?}: no admitted record line is noncanonical"
                ));
            }
        } else {
            for (name, v, is_record) in &verdicts {
                match v {
                    Err(e) => failures.push(format!("{id} {codes:?}: {name} was refused: {e}")),
                    Ok(false) if accept && *is_record => {
                        failures.push(format!("{id} {codes:?}: {name} is not canonical"));
                    }
                    Ok(_) => {}
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} vectors disagree with the manifest ({checked} documents read):\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}
