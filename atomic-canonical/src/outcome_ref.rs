//! The `urn:atomic:outcome:<blake3>` reference scheme.
//!
//! An outcome reference is a content address for an *outcome observation*: the
//! pins that fix a rollup of N views against a target — the views, the Merkle
//! state each was observed at, and the candidate change-set they contribute.
//! Two outcomes over exactly the same facts get the same reference; any change
//! to a pin yields a different one.
//!
//! This is the same scheme as [`crate::triage_ref`], and deliberately so: a
//! triage reference answers "was *this* change-set reviewed?", an outcome
//! reference answers "what did *this* body of work cost and produce?". Making
//! them the same shape means the two reports can be pinned, stored, and later
//! diffed against each other with one mechanism.
//!
//! Deliberately **string-based** (base32/hex strings, not typed hashes) so it
//! needs no `atomic-core` dependency — the caller supplies already-encoded
//! identifiers. Hashing goes through the one [`crate::jcs`] + BLAKE3 path the
//! rest of the crate uses, so the reference can never drift from a content hash.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::jcs;

/// The pinned facts an outcome reference addresses.
///
/// Field ordering is irrelevant to the hash: the two [`BTreeMap`]s
/// canonicalize, and `candidate_changes` is sorted before hashing, so only the
/// *set* of views and changes matters — not the order the user listed them in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomePins {
    /// The source views being rolled up, in the order the user named them.
    /// Sorted before hashing, so `a b` and `b a` pin identically.
    pub sources: Vec<String>,
    /// The target view the sources would be promoted into.
    pub target: String,
    /// View name → base32 of that view's Merkle at observation time — the
    /// materialized state the rollup is a fact about.
    pub source_merkle: BTreeMap<String, String>,
    /// Base32 Merkle of the target view at observation time.
    pub target_merkle: String,
    /// Base32 change hashes in the union of the sources' deltas. Order is not
    /// significant (sorted before hashing).
    pub candidate_changes: Vec<String>,
}

/// The `urn:atomic:outcome:<blake3-hex>` reference for a set of pins.
///
/// `candidate_changes` and `sources` are sorted before hashing so an outcome
/// over the same set of views and changes in any order gets the same
/// reference.
pub fn outcome_reference(pins: &OutcomePins) -> String {
    let mut canonical_pins = pins.clone();
    canonical_pins.sources.sort();
    canonical_pins.candidate_changes.sort();
    let value =
        serde_json::to_value(&canonical_pins).expect("OutcomePins serialization is infallible");
    let canonical = jcs::canonicalize(&value);
    let digest = blake3::hash(canonical.as_bytes());
    format!("urn:atomic:outcome:{}", digest.to_hex())
}

/// Recompute the reference for `pins` and compare it to `reference`. True iff
/// the pins hash to exactly this reference (order-insensitive for sources and
/// candidate changes).
pub fn verify_outcome_reference(pins: &OutcomePins, reference: &str) -> bool {
    outcome_reference(pins) == reference
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pins() -> OutcomePins {
        let mut merkle = BTreeMap::new();
        merkle.insert("feature-a".to_string(), "MERCLEA".to_string());
        merkle.insert("feature-b".to_string(), "MERCLEB".to_string());
        OutcomePins {
            sources: vec!["feature-a".to_string(), "feature-b".to_string()],
            target: "dev".to_string(),
            source_merkle: merkle,
            target_merkle: "MERKLEDEV".to_string(),
            candidate_changes: vec!["HASHB".to_string(), "HASHA".to_string()],
        }
    }

    #[test]
    fn reference_is_stable_across_input_order() {
        let mut reordered = pins();
        reordered.sources.reverse();
        reordered.candidate_changes.reverse();
        assert_eq!(outcome_reference(&pins()), outcome_reference(&reordered));
    }

    #[test]
    fn reference_round_trips() {
        assert!(verify_outcome_reference(
            &pins(),
            &outcome_reference(&pins())
        ));
    }

    #[test]
    fn changing_a_pin_changes_the_reference() {
        let base = outcome_reference(&pins());

        let mut other_target = pins();
        other_target.target = "release".to_string();
        assert_ne!(base, outcome_reference(&other_target));

        let mut other_merkle = pins();
        other_merkle.target_merkle = "MERKLE2".to_string();
        assert_ne!(base, outcome_reference(&other_merkle));

        let mut added_view = pins();
        added_view.sources.push("feature-c".to_string());
        assert_ne!(base, outcome_reference(&added_view));

        let mut added_change = pins();
        added_change.candidate_changes.push("HASHC".to_string());
        assert_ne!(base, outcome_reference(&added_change));
    }

    #[test]
    fn reference_has_the_documented_shape() {
        let reference = outcome_reference(&pins());
        assert!(reference.starts_with("urn:atomic:outcome:"));
        // blake3 hex is 64 chars.
        assert_eq!(reference.len(), "urn:atomic:outcome:".len() + 64);
    }
}
