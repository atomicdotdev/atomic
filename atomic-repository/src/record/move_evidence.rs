//! Typed advisory evidence for file moves observed while recording.
//!
//! Move evidence is stored only in [`Change::unhashed`] and never changes the
//! hashed V3 change schema. Authoritative evidence preserves a known inode
//! identity, while probable evidence and loss notes make heuristic or
//! unresolved rename detection explicit to downstream consumers.

use std::collections::BTreeSet;

use atomic_core::{change::Change, types::Inode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

/// The `Change::unhashed` object key reserved for typed move evidence.
pub const MOVE_EVIDENCE_UNHASHED_KEY: &str = "atomic.move_evidence";

/// The move-evidence payload version emitted by this crate.
pub const MOVE_EVIDENCE_VERSION: u16 = 1;

/// Minimum deterministic similarity score accepted as a probable move.
///
/// Similarity alone is never authoritative. The conservative 80% threshold
/// limits accidental identity reuse while still recognizing focused edits.
pub const PROBABLE_MOVE_THRESHOLD_BPS: u16 = 8_000;

/// Why a move is authoritative rather than heuristic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveAuthority {
    /// The move was explicitly performed through Atomic.
    ExplicitAtomicMove,
    /// Stable inode projection proved that both paths identify the same file.
    StableInodeProjection,
}

/// Evidence used to infer a probable move or unresolved rename candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveBasis {
    /// The source and destination bytes are identical.
    ByteIdentity,
    /// Source and destination contents are similar but not identical.
    ContentSimilarity,
}

/// A move whose old and new paths are known to refer to the same inode.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AuthoritativeMove {
    /// Path before the move.
    pub old_path: String,
    /// Path after the move.
    pub new_path: String,
    /// Stable file identity preserved by the move.
    pub inode: Inode,
    /// Evidence that makes the identity relationship authoritative.
    pub authority: MoveAuthority,
}

impl AuthoritativeMove {
    /// Construct authoritative move evidence.
    pub fn new(
        old_path: impl Into<String>,
        new_path: impl Into<String>,
        inode: Inode,
        authority: MoveAuthority,
    ) -> Self {
        Self {
            old_path: old_path.into(),
            new_path: new_path.into(),
            inode,
            authority,
        }
    }
}

/// A heuristic move candidate that must not be treated as exact identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProbableMove {
    /// Candidate path before the move.
    pub old_path: String,
    /// Candidate path after the move.
    pub new_path: String,
    /// Inode proposed for continuity if policy accepts the candidate.
    pub inode: Inode,
    /// Similarity in basis points (`0..=10_000`).
    ///
    /// Integer basis points avoid platform-dependent floating-point encodings.
    pub score: u16,
    /// Evidence used to calculate the score.
    pub basis: MoveBasis,
}

impl ProbableMove {
    /// Construct probable move evidence.
    pub fn new(
        old_path: impl Into<String>,
        new_path: impl Into<String>,
        inode: Inode,
        score: u16,
        basis: MoveBasis,
    ) -> Self {
        Self {
            old_path: old_path.into(),
            new_path: new_path.into(),
            inode,
            score,
            basis,
        }
    }
}

/// A source/destination pairing considered during rename resolution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RenameCandidate {
    /// Candidate source path.
    pub source_path: String,
    /// Candidate destination path.
    pub destination_path: String,
    /// Similarity in basis points (`0..=10_000`).
    pub score: u16,
    /// Evidence used to calculate the score.
    pub basis: MoveBasis,
}

impl RenameCandidate {
    /// Construct unresolved rename candidate evidence.
    pub fn new(
        source_path: impl Into<String>,
        destination_path: impl Into<String>,
        score: u16,
        basis: MoveBasis,
    ) -> Self {
        Self {
            source_path: source_path.into(),
            destination_path: destination_path.into(),
            score,
            basis,
        }
    }
}

/// Explicit loss recorded when move identity cannot be preserved.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LossNote {
    /// Rename inference was ambiguous, so recording retained delete-plus-add semantics.
    RenameUnresolved {
        /// Deterministically ordered evidence considered during resolution.
        candidates: BTreeSet<RenameCandidate>,
    },
    /// Git tree projection omitted an explicitly tracked empty directory.
    EmptyDirectory {
        /// Repository-relative directory path.
        path: String,
    },
}

impl LossNote {
    /// Construct an unresolved-rename note from candidate evidence.
    pub fn rename_unresolved(candidates: impl IntoIterator<Item = RenameCandidate>) -> Self {
        Self::RenameUnresolved {
            candidates: candidates.into_iter().collect(),
        }
    }

    /// Construct a note for a directory omitted from Git tree projection.
    pub fn empty_directory(path: impl Into<String>) -> Self {
        Self::EmptyDirectory { path: path.into() }
    }
}

/// Versioned advisory move evidence stored with a change.
///
/// Ordered sets make JSON array ordering independent of discovery or insertion
/// order. Duplicate evidence is collapsed without losing distinct candidates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveEvidence {
    /// Payload version. Currently always [`MOVE_EVIDENCE_VERSION`].
    pub version: u16,
    /// Moves backed by authoritative identity evidence.
    #[serde(default)]
    pub authoritative_moves: BTreeSet<AuthoritativeMove>,
    /// Moves inferred from non-authoritative evidence.
    #[serde(default)]
    pub probable_moves: BTreeSet<ProbableMove>,
    /// Explicit losses produced when a move could not be resolved.
    #[serde(default)]
    pub loss_notes: BTreeSet<LossNote>,
}

impl Default for MoveEvidence {
    fn default() -> Self {
        Self {
            version: MOVE_EVIDENCE_VERSION,
            authoritative_moves: BTreeSet::new(),
            probable_moves: BTreeSet::new(),
            loss_notes: BTreeSet::new(),
        }
    }
}

impl MoveEvidence {
    /// Construct an empty V1 move-evidence payload.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the payload contains no move or loss evidence.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.authoritative_moves.is_empty()
            && self.probable_moves.is_empty()
            && self.loss_notes.is_empty()
    }

    /// Add authoritative evidence.
    pub fn insert_authoritative(&mut self, evidence: AuthoritativeMove) -> bool {
        self.authoritative_moves.insert(evidence)
    }

    /// Add probable evidence.
    pub fn insert_probable(&mut self, evidence: ProbableMove) -> bool {
        self.probable_moves.insert(evidence)
    }

    /// Add an explicit loss note.
    pub fn insert_loss(&mut self, loss: LossNote) -> bool {
        self.loss_notes.insert(loss)
    }

    fn validate_version(&self) -> Result<(), MoveEvidenceError> {
        if self.version == MOVE_EVIDENCE_VERSION {
            Ok(())
        } else {
            Err(MoveEvidenceError::UnsupportedVersion(self.version))
        }
    }
}

/// Errors reading or writing typed move evidence.
#[derive(Debug, Error)]
pub enum MoveEvidenceError {
    /// Existing unhashed metadata cannot accept a namespaced object entry.
    #[error("Change.unhashed must be a JSON object to store move evidence")]
    UnhashedMetadataNotObject,
    /// Evidence was attached after hash-stable V3 bytes had already been cached.
    #[error("move evidence must be attached before V3 serialization")]
    V3BytesAlreadyCached,
    /// The payload version is not understood by this crate.
    #[error("unsupported move-evidence version {0}")]
    UnsupportedVersion(u16),
    /// The namespaced payload was not valid move-evidence JSON.
    #[error("invalid move-evidence JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Merge typed evidence into `Change::unhashed` without replacing other keys.
///
/// The reserved move-evidence key is replaced, while all unrelated object
/// entries remain unchanged. A non-object unhashed value is rejected and left
/// untouched because wrapping or replacing it would lose its existing shape.
pub fn merge_move_evidence(
    change: &mut Change,
    evidence: &MoveEvidence,
) -> Result<(), MoveEvidenceError> {
    evidence.validate_version()?;
    let value = serde_json::to_value(evidence)?;

    match change.unhashed.as_mut() {
        Some(Value::Object(object)) => {
            object.insert(MOVE_EVIDENCE_UNHASHED_KEY.to_owned(), value);
        }
        Some(Value::Null) | None => {
            let mut object = Map::new();
            object.insert(MOVE_EVIDENCE_UNHASHED_KEY.to_owned(), value);
            change.unhashed = Some(Value::Object(object));
        }
        Some(_) => return Err(MoveEvidenceError::UnhashedMetadataNotObject),
    }

    Ok(())
}

/// Extract typed evidence from `Change::unhashed`.
///
/// Returns `Ok(None)` when the namespaced key is absent. Existing non-object
/// metadata is rejected rather than silently reinterpreted.
pub fn extract_move_evidence(change: &Change) -> Result<Option<MoveEvidence>, MoveEvidenceError> {
    let Some(unhashed) = change.unhashed.as_ref() else {
        return Ok(None);
    };
    if unhashed.is_null() {
        return Ok(None);
    }
    let object = unhashed
        .as_object()
        .ok_or(MoveEvidenceError::UnhashedMetadataNotObject)?;
    let Some(value) = object.get(MOVE_EVIDENCE_UNHASHED_KEY) else {
        return Ok(None);
    };

    let evidence: MoveEvidence = serde_json::from_value(value.clone())?;
    evidence.validate_version()?;
    Ok(Some(evidence))
}

#[cfg(test)]
mod tests {
    use atomic_core::change::{Change, ChangeHeader};
    use serde_json::json;

    use super::*;

    fn sample_evidence() -> MoveEvidence {
        let mut evidence = MoveEvidence::new();
        evidence.insert_authoritative(AuthoritativeMove::new(
            "src/old.rs",
            "src/new.rs",
            Inode::new(7),
            MoveAuthority::StableInodeProjection,
        ));
        evidence.insert_probable(ProbableMove::new(
            "assets/old.bin",
            "assets/new.bin",
            Inode::new(9),
            9_750,
            MoveBasis::ByteIdentity,
        ));
        evidence.insert_loss(LossNote::rename_unresolved([
            RenameCandidate::new("src/a.rs", "src/c.rs", 8_200, MoveBasis::ContentSimilarity),
            RenameCandidate::new("src/b.rs", "src/c.rs", 8_200, MoveBasis::ContentSimilarity),
        ]));
        evidence
    }

    #[test]
    fn move_evidence_roundtrips_through_change_unhashed() {
        let mut change = Change::empty(ChangeHeader::builder().message("move").build());
        let evidence = sample_evidence();

        merge_move_evidence(&mut change, &evidence).unwrap();

        assert_eq!(extract_move_evidence(&change).unwrap(), Some(evidence));
    }

    #[test]
    fn merge_preserves_other_unhashed_namespaces() {
        let mut change = Change::empty(ChangeHeader::builder().message("move").build());
        change.unhashed = Some(json!({
            "agent_turn": { "session": "session-1" },
            "git": { "commit": "abc123" }
        }));
        let evidence = sample_evidence();

        merge_move_evidence(&mut change, &evidence).unwrap();

        let unhashed = change.unhashed.as_ref().unwrap();
        assert_eq!(
            unhashed.get("agent_turn"),
            Some(&json!({ "session": "session-1" }))
        );
        assert_eq!(unhashed.get("git"), Some(&json!({ "commit": "abc123" })));
        assert!(unhashed.get(MOVE_EVIDENCE_UNHASHED_KEY).is_some());
    }

    #[test]
    fn ordered_sets_make_payload_serialization_deterministic() {
        let first = LossNote::rename_unresolved([
            RenameCandidate::new("b", "c", 8_000, MoveBasis::ContentSimilarity),
            RenameCandidate::new("a", "c", 9_000, MoveBasis::ContentSimilarity),
        ]);
        let second = LossNote::rename_unresolved([
            RenameCandidate::new("a", "c", 9_000, MoveBasis::ContentSimilarity),
            RenameCandidate::new("b", "c", 8_000, MoveBasis::ContentSimilarity),
        ]);

        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap()
        );
    }
}
