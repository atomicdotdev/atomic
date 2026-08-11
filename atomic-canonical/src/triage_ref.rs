//! The `urn:atomic:triage:<blake3>` reference scheme.
//!
//! A triage reference is a content address for a *triage observation*: the set
//! of pins that fix what was reviewed — the feature/target views, the view
//! Merkle the report is a fact about, the candidate change-set, and each
//! intent's substance hash at review time. Two reviews that pin exactly the same
//! facts get the same reference; any change to a pin yields a different one.
//!
//! Kept deliberately **string-based** (base32/hex strings, not typed hashes) so
//! it needs no `atomic-core` dependency — the caller supplies already-encoded
//! identifiers. Hashing goes through the one [`crate::jcs`] + BLAKE3 path the
//! rest of the crate uses, so the reference can never drift from a content hash.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::jcs;

/// The pinned facts a triage reference addresses. Field ordering is irrelevant
/// to the hash: the [`BTreeMap`] canonicalizes the substance-hash map, and
/// `candidate_changes` is sorted before hashing, so only the *set* of changes
/// matters, not their order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriagePins {
    /// The feature (source) view name/id being triaged.
    pub feature: String,
    /// The target view name/id the feature would be promoted into.
    pub target: String,
    /// Base32 of the target/feature view Merkle — the materialized state pinned.
    pub view_merkle: String,
    /// Base32 change hashes in the candidate set. Order is not significant
    /// (sorted before hashing).
    pub candidate_changes: Vec<String>,
    /// Intent id → `intentSubstanceHash` at review time.
    pub intent_substance_hashes: BTreeMap<String, String>,
}

/// The `urn:atomic:triage:<blake3-hex>` reference for a set of pins.
///
/// `candidate_changes` is sorted before hashing so a triage over the same set of
/// changes in a different order produces the same reference. The map is already
/// canonical (BTreeMap), and JCS sorts object keys, so the reference is a pure
/// function of the pinned facts.
pub fn triage_reference(pins: &TriagePins) -> String {
    let mut canonical_pins = pins.clone();
    canonical_pins.candidate_changes.sort();
    let value =
        serde_json::to_value(&canonical_pins).expect("TriagePins serialization is infallible");
    let canonical = jcs::canonicalize(&value);
    let digest = blake3::hash(canonical.as_bytes());
    format!("urn:atomic:triage:{}", digest.to_hex())
}

/// Recompute the reference for `pins` and compare it to `reference`. True iff the
/// pins hash to exactly this reference (order-insensitive for candidate changes).
pub fn verify_triage_reference(pins: &TriagePins, reference: &str) -> bool {
    triage_reference(pins) == reference
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pins() -> TriagePins {
        let mut substance = BTreeMap::new();
        substance.insert("urn:atomic:intent:a".to_string(), "blake3:aaa".to_string());
        substance.insert("urn:atomic:intent:b".to_string(), "blake3:bbb".to_string());
        TriagePins {
            feature: "feature-login".to_string(),
            target: "dev".to_string(),
            view_merkle: "MERKLEBASE32".to_string(),
            candidate_changes: vec!["CH2".to_string(), "CH1".to_string(), "CH3".to_string()],
            intent_substance_hashes: substance,
        }
    }

    #[test]
    fn reference_is_urn_prefixed_and_deterministic() {
        let p = pins();
        let r = triage_reference(&p);
        assert!(r.starts_with("urn:atomic:triage:"));
        assert_eq!(r, triage_reference(&p));
    }

    #[test]
    fn candidate_change_order_is_not_significant() {
        let mut a = pins();
        a.candidate_changes = vec!["CH1".to_string(), "CH2".to_string(), "CH3".to_string()];
        let mut b = pins();
        b.candidate_changes = vec!["CH3".to_string(), "CH1".to_string(), "CH2".to_string()];
        assert_eq!(triage_reference(&a), triage_reference(&b));
    }

    #[test]
    fn reference_changes_when_any_pin_changes() {
        let base = triage_reference(&pins());

        let mut feature = pins();
        feature.feature = "feature-logout".to_string();
        assert_ne!(triage_reference(&feature), base);

        let mut target = pins();
        target.target = "release".to_string();
        assert_ne!(triage_reference(&target), base);

        let mut merkle = pins();
        merkle.view_merkle = "OTHER".to_string();
        assert_ne!(triage_reference(&merkle), base);

        let mut changes = pins();
        changes.candidate_changes.push("CH4".to_string());
        assert_ne!(triage_reference(&changes), base);

        let mut substance = pins();
        substance
            .intent_substance_hashes
            .insert("urn:atomic:intent:a".to_string(), "blake3:zzz".to_string());
        assert_ne!(triage_reference(&substance), base);
    }

    #[test]
    fn verify_round_trips_and_rejects_mutation() {
        let p = pins();
        let r = triage_reference(&p);
        assert!(verify_triage_reference(&p, &r));

        // Order-insensitive: a reshuffled candidate set still verifies.
        let mut reshuffled = p.clone();
        reshuffled.candidate_changes.reverse();
        assert!(verify_triage_reference(&reshuffled, &r));

        // A mutated pin no longer verifies against the original reference.
        let mut mutated = p.clone();
        mutated.view_merkle = "TAMPERED".to_string();
        assert!(!verify_triage_reference(&mutated, &r));
    }
}
