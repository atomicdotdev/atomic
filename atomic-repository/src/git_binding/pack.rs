//! Bounded, verified binding change packs — the optional `changes.pack`
//! fallback transport (RFC §8.6, CB-6B).
//!
//! A binding commit's tree may carry one extra blob, `changes.pack`, holding
//! the V3 change files of the bound closure for receivers that cannot reach
//! an Atomic remote. The Atomic remote is always preferred; the pack is the
//! verified fallback, and it is *bounded*: every resource budget below is
//! enforced **before** allocation, decompression, extraction, or graph use.
//!
//! # Quarantine order (RFC §5.2, §6.2, §12)
//!
//! 1. Encoded size gate (before any allocation).
//! 2. Streaming decompression with a hard output ceiling (zip-bomb bound).
//! 3. Envelope version gate (unknown versions fail closed).
//! 4. Object-count gate.
//! 5. Per-record family allowlist: only hash-verified V3 change files may
//!    enter or leave Git (the CB-6A privacy boundary,
//!    [`super::privacy::binding_pack_records`]).
//! 6. Per-record content addressing: `blake3(bytes) == key`, key shape.
//! 7. Per-change quarantine: canonical decode (format negotiation fails
//!    closed on unsupported versions), prompt privacy, and unhashed-body
//!    refusal — transcripts and prompts never enter Git objects (§12.13).
//! 8. Duplicate object ids are refused.
//!
//! Nothing here mutates authoritative state: these are pure verification
//! functions over bytes. Caching verified objects for retry is the caller's
//! decision and never implies a committed operation.

use std::collections::{HashMap, HashSet};

use atomic_core::change::{Change, PromptContent};
use atomic_core::types::Base32;
use atomic_core::Hash;

use atomic_objects::{decode_with_limit, encode, ObjectFamily, ObjectRecord};

use super::privacy::binding_pack_records;

/// Resource budgets for binding-pack transport (RFC §8.6 bounded fallback).
///
/// These are the concrete default budgets for this build. Callers handling
/// tighter-trust input may tighten them; nothing may loosen them per call —
/// a different budget is a different transport contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingPackLimits {
    /// Maximum encoded `changes.pack` blob size, checked before decode.
    pub max_pack_bytes: usize,
    /// Maximum decompressed envelope size (zip-bomb ceiling).
    pub max_decompressed_bytes: usize,
    /// Maximum number of change records in one pack.
    pub max_objects: usize,
    /// Maximum byte size of one change record.
    pub max_object_bytes: usize,
    /// Maximum number of entries in a binding's `ordered_changes` closure.
    pub max_closure_entries: usize,
    /// Maximum dependency-chain depth of a closure (cycle/depth bound).
    pub max_closure_depth: usize,
}

impl BindingPackLimits {
    /// The default transport budgets: 64 MiB packs, 256 MiB decompression
    /// ceiling, 100k changes, 32 MiB per change, 100k closure entries, 10k
    /// dependency depth.
    pub const fn default_limits() -> Self {
        BindingPackLimits {
            max_pack_bytes: 64 * 1024 * 1024,
            max_decompressed_bytes: 256 * 1024 * 1024,
            max_objects: 100_000,
            max_object_bytes: 32 * 1024 * 1024,
            max_closure_entries: 100_000,
            max_closure_depth: 10_000,
        }
    }

    /// A deliberately tiny budget for adversarial tests.
    #[cfg(test)]
    pub const fn tiny() -> Self {
        BindingPackLimits {
            max_pack_bytes: 1024,
            max_decompressed_bytes: 2048,
            max_objects: 4,
            max_object_bytes: 512,
            max_closure_entries: 4,
            max_closure_depth: 3,
        }
    }
}

impl Default for BindingPackLimits {
    fn default() -> Self {
        Self::default_limits()
    }
}

/// The versioned `changes.pack` envelope. Unknown versions fail closed at
/// decode (postcard refuses unknown enum variants).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChangesPack {
    /// Version 1: a bounded list of hash-verified change records.
    V1(ChangesPackV1),
}

/// The V1 pack body: exactly the records selected by the audited CB-6A Git
/// boundary ([`super::privacy::binding_pack_records`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChangesPackV1 {
    /// Change records, each with `blake3(bytes) == key`.
    pub records: Vec<ObjectRecord>,
}

/// Errors from the bounded binding-pack codec.
#[derive(Debug, thiserror::Error)]
pub enum BindingPackError {
    #[error("binding pack is oversized: {found} bytes exceeds the {limit} byte transport budget")]
    OversizedPack { limit: usize, found: usize },
    #[error(
        "binding pack inflates past the {limit} byte decompression ceiling (probable zip bomb)"
    )]
    DecompressedTooLarge { limit: usize },
    #[error("binding pack envelope is unsupported or malformed: {reason}")]
    UnsupportedVersion { reason: String },
    #[error("binding pack carries {found} objects; the transport budget is {limit}")]
    TooManyObjects { limit: usize, found: usize },
    #[error("binding pack object {key} is {found} bytes; the per-object budget is {limit}")]
    ObjectTooLarge {
        key: String,
        limit: usize,
        found: usize,
    },
    #[error("binding pack object key '{key}' is not a 64-character lowercase hex blake3 key")]
    InvalidKey { key: String },
    #[error("binding pack object {key} does not match blake3 of its bytes")]
    HashMismatch { key: String },
    #[error("{detail}")]
    Refused { detail: String },
    #[error("binding pack lists object {key} more than once; duplicate ids are refused")]
    DuplicateObject { key: String },
    #[error("binding pack object {key} does not decode as a V3 change file: {reason}")]
    ChangeDecode { key: String, reason: String },
    #[error(
        "binding pack object {key} carries private material that never enters Git ({detail}); \
         private evidence travels only via an Atomic remote (RFC 12.13)"
    )]
    PrivateMaterial { key: String, detail: String },
    #[error("cannot encode binding pack: {0}")]
    Encode(String),
}

/// Assemble the optional `changes.pack` blob from candidate records.
///
/// Applies the audited CB-6A Git boundary (family allowlist + content
/// addressing), then the per-change quarantine, then the transport budgets.
/// The result is the exact blob bytes for the binding tree.
pub fn assemble_changes_pack(
    candidates: &[ObjectRecord],
    limits: &BindingPackLimits,
) -> Result<Vec<u8>, BindingPackError> {
    if candidates.len() > limits.max_objects {
        return Err(BindingPackError::TooManyObjects {
            limit: limits.max_objects,
            found: candidates.len(),
        });
    }

    // Audited family + content-addressing gates (CB-6A privacy boundary).
    let allowed = binding_pack_records(candidates).map_err(|error| match error {
        super::privacy::BindingTreeError::HashMismatch { key } => {
            BindingPackError::HashMismatch { key }
        }
        other => BindingPackError::Refused {
            detail: other.to_string(),
        },
    })?;

    // Per-change quarantine before anything may enter Git (§12.13).
    let total: usize = allowed.iter().map(|record| record.bytes.len()).sum();
    if total > limits.max_decompressed_bytes {
        return Err(BindingPackError::ObjectTooLarge {
            key: "<pack total>".to_string(),
            limit: limits.max_decompressed_bytes,
            found: total,
        });
    }
    for record in &allowed {
        quarantine_change_record(record)?;
        if record.bytes.len() > limits.max_object_bytes {
            return Err(BindingPackError::ObjectTooLarge {
                key: record.key.clone(),
                limit: limits.max_object_bytes,
                found: record.bytes.len(),
            });
        }
    }

    // Duplicate ids make the closure ambiguous even when byte-identical.
    let mut seen: HashSet<&str> = HashSet::with_capacity(allowed.len());
    for record in &allowed {
        if !seen.insert(record.key.as_str()) {
            return Err(BindingPackError::DuplicateObject {
                key: record.key.clone(),
            });
        }
    }

    let envelope = ChangesPack::V1(ChangesPackV1 { records: allowed });
    encode(&envelope).map_err(|error| BindingPackError::Encode(error.to_string()))
}

/// Decode a `changes.pack` blob under the transport budgets.
///
/// Every gate runs **before** the corresponding allocation: the encoded size
/// is checked before decompression begins and decompression is streaming
/// with a hard ceiling, so a bomb is stopped without materializing.
pub fn decode_changes_pack(
    bytes: &[u8],
    limits: &BindingPackLimits,
) -> Result<Vec<ObjectRecord>, BindingPackError> {
    // 1. Encoded size gate — before any allocation or decompression.
    if bytes.len() > limits.max_pack_bytes {
        return Err(BindingPackError::OversizedPack {
            limit: limits.max_pack_bytes,
            found: bytes.len(),
        });
    }

    // 2. Streaming decompression with a hard output ceiling; 3. version gate.
    let envelope = decode_with_limit::<ChangesPack>(bytes, limits.max_decompressed_bytes).map_err(
        |error| match error {
            atomic_objects::SyncError::TooLarge { limit } => {
                BindingPackError::DecompressedTooLarge { limit }
            }
            other => BindingPackError::UnsupportedVersion {
                reason: other.to_string(),
            },
        },
    )?;
    let ChangesPack::V1(v1) = envelope;

    // 4. Object-count gate.
    if v1.records.len() > limits.max_objects {
        return Err(BindingPackError::TooManyObjects {
            limit: limits.max_objects,
            found: v1.records.len(),
        });
    }

    // 5-8. Per-record gates in quarantine order.
    let mut seen: HashSet<&str> = HashSet::with_capacity(v1.records.len());
    for record in &v1.records {
        if !matches!(record.family, ObjectFamily::Change) {
            return Err(BindingPackError::Refused {
                detail: format!(
                    "binding pack refused {:?} object {}: only V3 change files may enter Git",
                    record.family, record.key
                ),
            });
        }
        if !is_blake3_key(&record.key) {
            return Err(BindingPackError::InvalidKey {
                key: record.key.clone(),
            });
        }
        if Hash::of(&record.bytes).to_hex() != record.key {
            return Err(BindingPackError::HashMismatch {
                key: record.key.clone(),
            });
        }
        if record.bytes.len() > limits.max_object_bytes {
            return Err(BindingPackError::ObjectTooLarge {
                key: record.key.clone(),
                limit: limits.max_object_bytes,
                found: record.bytes.len(),
            });
        }
        if !seen.insert(record.key.as_str()) {
            return Err(BindingPackError::DuplicateObject {
                key: record.key.clone(),
            });
        }
    }
    Ok(v1.records)
}

/// Quarantine one candidate change record and recover its identity.
///
/// Two distinct, deliberate key namespaces meet here — this is the resolved
/// CB-6A/CB-6B ambiguity (RFC §8.6):
///
/// - **Pack addressing** (the record `key`): lowercase-hex
///   `blake3(record bytes)` — the audited [`binding_pack_records`] contract.
///   The change *file* is the addressed object; its trailer is excluded from
///   this digest by the V3 format, so this is NOT the change hash.
/// - **Change identity**: the V3 trailer content hash — what
///   `ordered_changes`, the change store, and the Atomic sync plane use.
///
/// Quarantine verifies both: the record must be blake3-addressed by its key,
/// and decoding must succeed (format negotiation fails closed on unsupported
/// versions). The returned `Hash` is the *identity* hash callers index by.
/// The privacy boundary (§12.13) is enforced before anything is returned.
///
/// [`binding_pack_records`]: super::privacy::binding_pack_records
pub fn quarantine_change_record(record: &ObjectRecord) -> Result<(Hash, Change), BindingPackError> {
    if !matches!(record.family, ObjectFamily::Change) {
        return Err(BindingPackError::Refused {
            detail: format!(
                "binding pack refused {:?} object {}: only V3 change files may enter Git",
                record.family, record.key
            ),
        });
    }
    if !is_blake3_key(&record.key) {
        return Err(BindingPackError::InvalidKey {
            key: record.key.clone(),
        });
    }
    if Hash::of(&record.bytes).to_hex() != record.key {
        return Err(BindingPackError::HashMismatch {
            key: record.key.clone(),
        });
    }
    let (change, identity) = Change::deserialize(&mut std::io::Cursor::new(&record.bytes))
        .map_err(|error| BindingPackError::ChangeDecode {
            key: record.key.clone(),
            reason: error.to_string(),
        })?;
    if let Some(detail) = private_material_in_change(&change) {
        return Err(BindingPackError::PrivateMaterial {
            key: record.key.clone(),
            detail,
        });
    }
    Ok((identity, change))
}

/// The private material a change carries for Git transport, if any (§12.13).
///
/// This resolves the CB-6A-format ambiguity for CB-6B: a change file's key
/// pins its bytes, but hash authority is not privacy authorization. Provenance
/// with unhashed prompt bodies (`Full`/`Compressed`) and any `unhashed`
/// section (free-form transcripts, review comments) never enters Git; such
/// changes travel only via an Atomic remote.
pub fn private_material_in_change(change: &Change) -> Option<String> {
    for provenance in change.provenance() {
        match &provenance.prompt {
            PromptContent::Full(_) => {
                return Some("provenance carries a full prompt body".to_string())
            }
            PromptContent::Compressed(_) => {
                return Some("provenance carries a compressed prompt body".to_string())
            }
            PromptContent::Hashed(_) | PromptContent::None => {}
        }
    }
    if change.unhashed.is_some() {
        return Some("the change carries an unhashed metadata section".to_string());
    }
    None
}

/// Whether `key` is a 64-character lowercase-hex blake3 key.
fn is_blake3_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A hash-verified, privacy-quarantined view of one pack: decoded changes
/// keyed by content hash.
#[derive(Debug, Clone, Default)]
pub struct QuarantinedPack {
    changes: HashMap<Hash, Change>,
}

impl QuarantinedPack {
    /// Quarantine every record of a decoded pack, enforcing the object-count
    /// budget before decoding anything. The pack indexes changes by their
    /// decoded **identity** hash (see [`quarantine_change_record`]).
    pub fn from_records(
        records: &[ObjectRecord],
        limits: &BindingPackLimits,
    ) -> Result<Self, BindingPackError> {
        if records.len() > limits.max_objects {
            return Err(BindingPackError::TooManyObjects {
                limit: limits.max_objects,
                found: records.len(),
            });
        }
        let mut changes = HashMap::with_capacity(records.len());
        for record in records {
            let (identity, change) = quarantine_change_record(record)?;
            changes.insert(identity, change);
        }
        Ok(QuarantinedPack { changes })
    }

    /// The quarantined change for `hash`, if the pack carries it.
    pub fn get(&self, hash: &Hash) -> Option<&Change> {
        self.changes.get(hash)
    }

    /// How many changes the pack carries.
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Whether the pack is empty.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// A source of hash-verified changes for closure validation: the pack, the
/// local store, or a chain of both (mixed-have).
pub trait ClosureChangeSource {
    /// The decoded change for `hash`, or `None` when unavailable.
    ///
    /// Corruption is an error (fail closed), never a silent `None`.
    fn get(&mut self, hash: &Hash) -> Result<Option<Change>, BindingClosureError>;
}

impl<F> ClosureChangeSource for F
where
    F: FnMut(&Hash) -> Result<Option<Change>, BindingClosureError>,
{
    fn get(&mut self, hash: &Hash) -> Result<Option<Change>, BindingClosureError> {
        self(hash)
    }
}

/// The outcome of closure validation over an available object source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureValidation {
    /// Ordered hashes verified through the source.
    pub verified: usize,
    /// Ordered hashes (or closure dependencies) that no available source
    /// could provide. Empty means the closure is complete.
    pub missing: Vec<Hash>,
    /// Closure-internal dependency edges that were validated.
    pub internal_edges: usize,
    /// Dependencies that resolve outside the binding's ordered closure
    /// (e.g. snapshot changes available locally). Legitimate, but counted
    /// so callers can distinguish exact-closure transport.
    pub external_deps: usize,
    /// The validated dependency depth of the closure.
    pub depth: usize,
}

impl ClosureValidation {
    /// Whether every ordered change and dependency resolved.
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Validate a binding's change closure against the available objects.
///
/// Checks, in order:
/// 1. structural payload validation (version, ordering, OIDs, signer);
/// 2. `closure_root` recomputation over the ordered changes;
/// 3. the closure entry budget;
/// 4. per-change quarantine through `source` (corruption fails closed);
/// 5. dependency completeness: every dependency is inside the closure or
///    resolvable through `source`;
/// 6. acyclicity and depth of the closure-internal dependency graph.
///
/// Missing objects are *reported* (`missing`), never synthesized, so callers
/// can drive remote/pack fallback and report explicit incompleteness.
pub fn validate_binding_closure(
    payload: &super::codec::GitStateBindingPayload,
    source: &mut dyn ClosureChangeSource,
    limits: &BindingPackLimits,
) -> Result<ClosureValidation, BindingClosureError> {
    payload
        .validate()
        .map_err(|error| BindingClosureError::InvalidBinding(error.to_string()))?;

    if payload.ordered_changes.len() > limits.max_closure_entries {
        return Err(BindingClosureError::ClosureTooLarge {
            limit: limits.max_closure_entries,
            found: payload.ordered_changes.len(),
        });
    }
    if payload.closure_root != payload.compute_closure_root() {
        return Err(BindingClosureError::ClosureRootMismatch);
    }

    // Decode every ordered change through the source.
    let mut changes: HashMap<Hash, Change> = HashMap::with_capacity(payload.ordered_changes.len());
    let mut missing: Vec<Hash> = Vec::new();
    for hash in &payload.ordered_changes {
        match source.get(hash)? {
            Some(change) => {
                changes.insert(*hash, change);
            }
            None => missing.push(*hash),
        }
    }

    // Dependency completeness + cycle/depth over the closure-internal graph.
    let mut external_deps = 0usize;
    let mut internal_edges: HashMap<Hash, Vec<Hash>> = HashMap::with_capacity(changes.len());
    for (hash, change) in &changes {
        let mut edges = Vec::new();
        for dep in change.dependencies() {
            if changes.contains_key(dep) {
                edges.push(*dep);
            } else if !payload.ordered_changes.contains(dep) {
                // Outside the binding's closure: legitimate only when some
                // source (typically the local store) still provides it.
                match source.get(dep)? {
                    Some(_) => external_deps += 1,
                    None => {
                        if !missing.contains(dep) {
                            missing.push(*dep);
                        }
                    }
                }
            }
        }
        internal_edges.insert(*hash, edges);
    }

    let depth = check_depth_and_cycles(&internal_edges, limits.max_closure_depth)?;

    Ok(ClosureValidation {
        verified: changes.len(),
        missing,
        internal_edges: internal_edges.values().map(|edges| edges.len()).sum(),
        external_deps,
        depth,
    })
}

/// Iterative DFS over the closure-internal dependency graph: rejects cycles
/// and enforces the depth budget without recursing (deep adversarial chains
/// must not exhaust the stack).
fn check_depth_and_cycles(
    edges: &HashMap<Hash, Vec<Hash>>,
    max_depth: usize,
) -> Result<usize, BindingClosureError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Grey,
        Black,
    }
    let mut color: HashMap<Hash, Color> = edges.keys().map(|h| (*h, Color::White)).collect();
    let mut depth: HashMap<Hash, usize> = HashMap::with_capacity(edges.len());
    let mut overall = 0usize;

    let roots: Vec<Hash> = edges.keys().copied().collect();
    for root in roots {
        if color[&root] != Color::White {
            continue;
        }
        // Stack holds (node, phase): phase 0 = enter, 1 = leave.
        let mut stack: Vec<(Hash, u8)> = vec![(root, 0)];
        while let Some((node, phase)) = stack.pop() {
            if phase == 1 {
                let child_max = edges[&node]
                    .iter()
                    .filter_map(|dep| depth.get(dep))
                    .copied()
                    .max()
                    .unwrap_or(0);
                let here = child_max + 1;
                depth.insert(node, here);
                if here > max_depth {
                    return Err(BindingClosureError::ClosureTooDeep {
                        limit: max_depth,
                        found: here,
                    });
                }
                overall = overall.max(here);
                color.insert(node, Color::Black);
                continue;
            }
            match color[&node] {
                Color::Black => continue,
                Color::Grey => {
                    return Err(BindingClosureError::DependencyCycle {
                        at: node.to_base32(),
                    })
                }
                Color::White => {}
            }
            color.insert(node, Color::Grey);
            stack.push((node, 1));
            for dep in edges[&node].iter().rev() {
                if color[dep] == Color::Grey {
                    return Err(BindingClosureError::DependencyCycle {
                        at: dep.to_base32(),
                    });
                }
                if color[dep] == Color::White {
                    stack.push((*dep, 0));
                }
            }
        }
    }
    Ok(overall)
}

/// Closure validation failures: structural violations that can never be
/// adopted, distinct from mere incompleteness (reported as `missing`).
#[derive(Debug, thiserror::Error)]
pub enum BindingClosureError {
    #[error("binding is invalid: {0}")]
    InvalidBinding(String),
    #[error("binding closure lists {found} changes; the transport budget is {limit}")]
    ClosureTooLarge { limit: usize, found: usize },
    #[error("closure_root does not match recomputation over the binding's ordered changes")]
    ClosureRootMismatch,
    #[error("closure dependency graph contains a cycle at {at}")]
    DependencyCycle { at: String },
    #[error("closure dependency depth {found} exceeds the transport budget of {limit}")]
    ClosureTooDeep { limit: usize, found: usize },
    #[error("closure change {hash} failed quarantine: {source}")]
    Quarantine {
        hash: String,
        #[source]
        source: BindingPackError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::{Author, ChangeHeader};

    fn record_with_deps(
        message: &str,
        deps: Vec<Hash>,
        unhashed: Option<serde_json::Value>,
    ) -> (ObjectRecord, Change) {
        let header = ChangeHeader::builder()
            .message(message)
            .author(Author::new("Test", Some("test@example.com")))
            .build();
        let mut change = Change::new(
            header,
            Vec::new(),
            format!("{message}\n").into_bytes(),
            deps,
        );
        change.unhashed = unhashed;
        let mut bytes = Vec::new();
        change
            .serialize(&mut std::io::Cursor::new(&mut bytes))
            .expect("serialize");
        // Pack records are addressed by blake3 of the exact file bytes (the
        // CB-6A pack contract), not by the change identity hash.
        (
            ObjectRecord::new(ObjectFamily::Change, Hash::of(&bytes).to_hex(), bytes),
            change,
        )
    }

    fn plain_change(message: &str) -> (ObjectRecord, Change) {
        record_with_deps(message, Vec::new(), None)
    }

    #[test]
    fn pack_round_trips_through_the_bounded_envelope() {
        let (a, change_a) = plain_change("alpha");
        let (b, change_b) = plain_change("beta");
        let bytes =
            assemble_changes_pack(&[a, b], &BindingPackLimits::default_limits()).expect("assemble");
        let records =
            decode_changes_pack(&bytes, &BindingPackLimits::default_limits()).expect("decode");
        let pack = QuarantinedPack::from_records(&records, &BindingPackLimits::default_limits())
            .expect("quarantine");
        assert_eq!(pack.len(), 2);
        let hash_a = change_a.hash().unwrap();
        let hash_b = change_b.hash().unwrap();
        assert_eq!(
            pack.get(&hash_a).unwrap().message(),
            change_a.message(),
            "the exact decoded change rides the pack"
        );
        assert_eq!(pack.get(&hash_b).unwrap().message(), change_b.message());
    }

    #[test]
    fn oversized_encoded_pack_is_refused_before_decode() {
        let (a, _) = plain_change("alpha");
        let bytes = assemble_changes_pack(&[a], &BindingPackLimits::default_limits()).unwrap();
        let mut limits = BindingPackLimits::default_limits();
        limits.max_pack_bytes = bytes.len() - 1;
        let error = decode_changes_pack(&bytes, &limits).unwrap_err();
        assert!(
            matches!(error, BindingPackError::OversizedPack { .. }),
            "got {error}"
        );
    }

    #[test]
    fn decompression_bomb_is_refused_at_the_ceiling() {
        // Highly compressible body: the encoded pack is small but the
        // decompressed envelope is huge. The streaming ceiling stops it
        // before the full output is materialized.
        let data = vec![b'x'; 4096];
        let huge = ObjectRecord::new(ObjectFamily::Change, Hash::of(&data).to_hex(), data);
        let envelope = ChangesPack::V1(ChangesPackV1 {
            records: vec![huge],
        });
        let bytes = encode(&envelope).unwrap();
        let mut limits = BindingPackLimits::default_limits();
        limits.max_decompressed_bytes = 1024;
        let error = decode_changes_pack(&bytes, &limits).unwrap_err();
        assert!(
            matches!(error, BindingPackError::DecompressedTooLarge { .. }),
            "got {error}"
        );
    }

    #[test]
    fn corrupted_and_garbage_envelopes_fail_closed() {
        let (a, _) = plain_change("alpha");
        let bytes = assemble_changes_pack(&[a], &BindingPackLimits::default_limits()).unwrap();
        let mut tampered = bytes.clone();
        tampered[0] ^= 0x01;
        assert!(
            decode_changes_pack(&tampered, &BindingPackLimits::default_limits()).is_err(),
            "a corrupted envelope must fail closed"
        );
        assert!(
            decode_changes_pack(b"not a zstd frame", &BindingPackLimits::default_limits()).is_err(),
            "garbage must fail closed"
        );
    }

    #[test]
    fn object_and_size_budgets_are_enforced() {
        let (a, _) = plain_change("one");
        let (b, _) = plain_change("two");
        let (c, _) = plain_change("three");
        let mut limits = BindingPackLimits::default_limits();
        limits.max_objects = 2;
        let error = assemble_changes_pack(&[a, b, c], &limits).unwrap_err();
        assert!(
            matches!(error, BindingPackError::TooManyObjects { .. }),
            "{error}"
        );

        let mut limits = BindingPackLimits::default_limits();
        limits.max_object_bytes = 8;
        let (big, _) = plain_change("this change body is longer than eight bytes");
        let error = assemble_changes_pack(&[big], &limits).unwrap_err();
        assert!(
            matches!(error, BindingPackError::ObjectTooLarge { .. }),
            "{error}"
        );
    }

    #[test]
    fn hash_mismatch_bad_keys_and_duplicates_are_refused() {
        let (a, _) = plain_change("alpha");
        let mut forged = a.clone();
        forged.key = Hash::of(b"other").to_hex();
        let error =
            assemble_changes_pack(&[forged], &BindingPackLimits::default_limits()).unwrap_err();
        assert!(
            matches!(error, BindingPackError::HashMismatch { .. }),
            "{error}"
        );

        let (b, _) = plain_change("beta");
        let mut bad_key = b.clone();
        bad_key.key = "ZZZ-not-hex".to_string();
        // The transport decode checks key shape before hashing, so a non-hex
        // key is an InvalidKey there; the assembler delegates to the audited
        // CB-6A gate first and reports the mismatch either way — both refuse.
        let error = decode_changes_pack(
            &encode(&ChangesPack::V1(ChangesPackV1 {
                records: vec![bad_key.clone()],
            }))
            .unwrap(),
            &BindingPackLimits::default_limits(),
        )
        .unwrap_err();
        assert!(
            matches!(error, BindingPackError::InvalidKey { .. }),
            "{error}"
        );
        assert!(assemble_changes_pack(&[bad_key], &BindingPackLimits::default_limits()).is_err());

        let (c, _) = plain_change("gamma");
        let duplicate = c.clone();
        let error = assemble_changes_pack(&[c, duplicate], &BindingPackLimits::default_limits())
            .unwrap_err();
        assert!(
            matches!(error, BindingPackError::DuplicateObject { .. }),
            "{error}"
        );
    }

    #[test]
    fn private_sidecar_families_never_enter_a_pack() {
        let provenance = ObjectRecord::new(
            ObjectFamily::Provenance,
            Hash::of(b"p").to_hex(),
            b"p".to_vec(),
        );
        let error =
            assemble_changes_pack(&[provenance], &BindingPackLimits::default_limits()).unwrap_err();
        assert!(
            error.to_string().contains("only V3 change files"),
            "{error}"
        );
    }

    #[test]
    fn changes_with_full_prompts_never_enter_a_pack() {
        let header = ChangeHeader::builder()
            .message("with prompt")
            .author(Author::new("Test", Some("t@e")))
            .build();
        let mut change = Change::new(header, Vec::new(), b"body\n".to_vec(), Vec::new());
        let provenance = atomic_core::change::Provenance {
            prompt: PromptContent::Full("PROMPT-SENTINEL-CB6B".to_string()),
            ..Default::default()
        };
        change.add_provenance(provenance);
        let mut bytes = Vec::new();
        change
            .serialize(&mut std::io::Cursor::new(&mut bytes))
            .unwrap();
        // Pack records are addressed by blake3 of the exact file bytes.
        let record = ObjectRecord::new(ObjectFamily::Change, Hash::of(&bytes).to_hex(), bytes);

        let error = assemble_changes_pack(
            std::slice::from_ref(&record),
            &BindingPackLimits::default_limits(),
        )
        .unwrap_err();
        assert!(
            matches!(error, BindingPackError::PrivateMaterial { .. }),
            "{error}"
        );

        // A pack that already carries it is refused at quarantine, even
        // though the transport-level decode succeeds.
        let envelope = ChangesPack::V1(ChangesPackV1 {
            records: vec![record],
        });
        let bytes = encode(&envelope).unwrap();
        let records = decode_changes_pack(&bytes, &BindingPackLimits::default_limits())
            .expect("transport-level gates pass");
        let error = QuarantinedPack::from_records(&records, &BindingPackLimits::default_limits())
            .unwrap_err();
        assert!(
            matches!(error, BindingPackError::PrivateMaterial { .. }),
            "{error}"
        );
    }

    #[test]
    fn changes_with_unhashed_sections_never_enter_a_pack() {
        let (record, _) = record_with_deps(
            "with transcript",
            Vec::new(),
            Some(serde_json::json!({ "transcript": "TRANSCRIPT-SENTINEL-CB6B" })),
        );
        let error =
            assemble_changes_pack(&[record], &BindingPackLimits::default_limits()).unwrap_err();
        assert!(
            matches!(error, BindingPackError::PrivateMaterial { .. }),
            "{error}"
        );
    }

    #[test]
    fn tampered_change_bytes_are_refused() {
        let (mut record, _) = plain_change("canonical");
        record.bytes[0] ^= 0x01;
        let error = quarantine_change_record(&record).unwrap_err();
        assert!(
            matches!(error, BindingPackError::HashMismatch { .. }),
            "{error}"
        );
    }

    #[test]
    fn unsupported_change_file_versions_fail_closed_in_packs() {
        let (record, _) = plain_change("alpha");

        // A V3 file with a future schema version: bytes 4..8 hold the u32 LE
        // version after the b"ATOM" magic. Version 99 must be refused, never
        // best-effort decoded.
        let mut future = record.bytes.clone();
        future[4..8].copy_from_slice(&99u32.to_le_bytes());
        let forged = ObjectRecord::new(ObjectFamily::Change, Hash::of(&future).to_hex(), future);
        let error = quarantine_change_record(&forged).unwrap_err();
        assert!(
            matches!(error, BindingPackError::ChangeDecode { .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains("version"),
            "the refusal names the format version: {error}"
        );

        // Legacy pre-V3 files (they start with a small LE integer instead of
        // the magic) are refused with an explicit re-record instruction.
        let mut legacy = record.bytes.clone();
        legacy[0..4].copy_from_slice(&2u32.to_le_bytes());
        let forged = ObjectRecord::new(ObjectFamily::Change, Hash::of(&legacy).to_hex(), legacy);
        let error = quarantine_change_record(&forged).unwrap_err();
        assert!(
            matches!(error, BindingPackError::ChangeDecode { .. }),
            "{error}"
        );
    }

    #[test]
    fn closure_validation_reports_completeness_and_depth() {
        let (_, change_a) = plain_change("a");
        let (_, change_b) = record_with_deps("b", vec![change_a.hash().unwrap()], None);
        let hash_a = change_a.hash().unwrap();
        let hash_b = change_b.hash().unwrap();

        let payload = closure_payload(vec![hash_a, hash_b]);
        let pack: HashMap<Hash, Change> =
            HashMap::from([(hash_a, change_a.clone()), (hash_b, change_b.clone())]);
        let mut source = |hash: &Hash| Ok(pack.get(hash).cloned());
        let report =
            validate_binding_closure(&payload, &mut source, &BindingPackLimits::default_limits())
                .expect("closure validates");
        assert!(report.is_complete());
        assert_eq!(report.verified, 2);
        assert_eq!(report.internal_edges, 1);
        assert_eq!(report.depth, 2);
    }

    #[test]
    fn closure_validation_lists_missing_objects() {
        let (_, change_a) = plain_change("a");
        let hash_a = change_a.hash().unwrap();
        let payload = closure_payload(vec![hash_a]);
        let mut source = |_hash: &Hash| -> Result<Option<Change>, BindingClosureError> { Ok(None) };
        let report =
            validate_binding_closure(&payload, &mut source, &BindingPackLimits::default_limits())
                .expect("incompleteness is an outcome, not an error");
        assert!(!report.is_complete());
        assert_eq!(report.missing, vec![hash_a]);
    }

    #[test]
    fn dependency_outside_the_closure_must_resolve_somewhere() {
        let (_, change_a) = plain_change("a");
        let hash_a = change_a.hash().unwrap();
        let (_, change_b) = record_with_deps("b", vec![hash_a], None);
        let hash_b = change_b.hash().unwrap();
        // The binding's closure names only B; A is a dependency outside it.
        let payload = closure_payload(vec![hash_b]);
        let pack: HashMap<Hash, Change> = HashMap::from([(hash_b, change_b)]);

        // Without A available anywhere: explicit incompleteness naming A.
        let mut source = |hash: &Hash| Ok(pack.get(hash).cloned());
        let report =
            validate_binding_closure(&payload, &mut source, &BindingPackLimits::default_limits())
                .expect("incompleteness is an outcome");
        assert_eq!(report.missing, vec![hash_a]);

        // With A resolvable (e.g. the local store) alongside the packed B:
        // complete, and the external dependency is counted.
        let cached_b = pack.values().next().cloned().expect("packed b");
        let cached_a = change_a.clone();
        let mut source = move |hash: &Hash| {
            Ok(if *hash == hash_a {
                Some(cached_a.clone())
            } else if *hash == hash_b {
                Some(cached_b.clone())
            } else {
                None
            })
        };
        let report =
            validate_binding_closure(&payload, &mut source, &BindingPackLimits::default_limits())
                .expect("external dep available");
        assert!(report.is_complete());
        assert_eq!(report.external_deps, 1);
        let _ = change_b;
    }

    #[test]
    fn cyclic_closures_are_rejected() {
        // A change's hash covers its dependencies, so a real B↔C cycle cannot
        // be minted with the public serializer (each side would need the
        // other's final hash). The validator's cycle detector is therefore
        // exercised directly on a hand-built dependency-edge map — the same
        // map `validate_binding_closure` builds from decoded pack changes —
        // which is the code path adversarial packs hit.
        let h1 = Hash::from_bytes([1u8; 32]);
        let h2 = Hash::from_bytes([2u8; 32]);
        let edges = HashMap::from([(h1, vec![h2]), (h2, vec![h1])]);
        let error = check_depth_and_cycles(&edges, 10).unwrap_err();
        assert!(matches!(error, BindingClosureError::DependencyCycle { .. }));
    }

    #[test]
    fn depth_budget_is_enforced() {
        let h1 = Hash::from_bytes([1u8; 32]);
        let h2 = Hash::from_bytes([2u8; 32]);
        let h3 = Hash::from_bytes([3u8; 32]);
        let h4 = Hash::from_bytes([4u8; 32]);
        // Chain of depth 4 against a budget of 3.
        let edges = HashMap::from([(h1, vec![h2]), (h2, vec![h3]), (h3, vec![h4]), (h4, vec![])]);
        let error = check_depth_and_cycles(&edges, 3).unwrap_err();
        assert!(
            matches!(error, BindingClosureError::ClosureTooDeep { .. }),
            "{error}"
        );
        // Within budget it passes.
        assert_eq!(check_depth_and_cycles(&edges, 4).unwrap(), 4);
    }

    #[test]
    fn closure_root_mismatch_is_refused() {
        let (_, change_a) = plain_change("a");
        let hash_a = change_a.hash().unwrap();
        let mut payload = closure_payload(vec![hash_a]);
        payload.closure_root = Hash::from_bytes([7u8; 32]);
        let mut absent = |_hash: &Hash| -> Result<Option<Change>, BindingClosureError> { Ok(None) };
        let error =
            validate_binding_closure(&payload, &mut absent, &BindingPackLimits::default_limits())
                .unwrap_err();
        // The encoding-level payload contract rejects a mismatched
        // closure_root first; the validator's independent recomputation is
        // the second gate behind it. Either way the closure is refused.
        assert!(
            error.to_string().contains("closure_root does not match"),
            "{error}"
        );
    }

    #[test]
    fn oversized_closures_are_refused() {
        let (_, change_a) = plain_change("a");
        let (_, change_b) = plain_change("b");
        let hash_a = change_a.hash().unwrap();
        let hash_b = change_b.hash().unwrap();
        // Entry budget: two distinct changes against a one-entry budget.
        let payload = closure_payload(vec![hash_a, hash_b]);
        let mut limits = BindingPackLimits::default_limits();
        limits.max_closure_entries = 1;
        let mut absent = |_hash: &Hash| -> Result<Option<Change>, BindingClosureError> { Ok(None) };
        let error = validate_binding_closure(&payload, &mut absent, &limits).unwrap_err();
        assert!(
            matches!(error, BindingClosureError::ClosureTooLarge { .. }),
            "{error}"
        );

        // Duplicate ordered entries are structurally refused by the payload
        // contract before the budget is even consulted.
        let duplicated = closure_payload(vec![hash_a; 2]);
        let mut absent = |_hash: &Hash| -> Result<Option<Change>, BindingClosureError> { Ok(None) };
        let error = validate_binding_closure(
            &duplicated,
            &mut absent,
            &BindingPackLimits::default_limits(),
        )
        .unwrap_err();
        assert!(
            matches!(error, BindingClosureError::InvalidBinding(_)),
            "{error}"
        );
    }

    fn closure_payload(ordered: Vec<Hash>) -> super::super::codec::GitStateBindingPayload {
        use super::super::codec::{
            BindingSigner, CausalOrigin, GitObjectFormat, GitOid, GitStateBindingPayload,
            BINDING_VERSION,
        };
        use atomic_core::types::{Merkle, OperationId, SetId};
        use atomic_identity::keypair::{KeyPair, SecretKey};

        let mut secret = [0u8; 32];
        for (index, byte) in secret.iter_mut().enumerate() {
            *byte = (index * 17 + 3) as u8;
        }
        let keypair = KeyPair::from_secret_key(SecretKey::from_bytes(&secret));
        let commit_hex: String = (0..40).map(|i| format!("{:x}", i % 16)).collect();
        let mut payload = GitStateBindingPayload {
            version: BINDING_VERSION,
            git_object_format: GitObjectFormat::Sha1,
            git_commit: GitOid::from_hex(&commit_hex).unwrap(),
            git_tree: GitOid::from_hex(&commit_hex).unwrap(),
            git_parents: Vec::new(),
            raw_commit_object: None,
            set_id: SetId::from_bytes([1u8; 32]),
            merkle_state: Merkle::from_bytes([2u8; 32]),
            view_hint: None,
            ordered_changes: ordered,
            closure_root: Hash::from_bytes([0u8; 32]),
            operation: OperationId::from_bytes([3u8; 32]),
            origin: CausalOrigin::ExactAtomicResurrection,
            loss: Vec::new(),
            provenance_roots: Vec::new(),
            attestation_roots: Vec::new(),
            signer: BindingSigner::for_keypair(&keypair),
        };
        payload.closure_root = payload.compute_closure_root();
        payload
    }
}
