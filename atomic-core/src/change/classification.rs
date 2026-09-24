//! Hashed lifecycle, origin, and causal-frontier facts for changes.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::{GitHashAlgorithm, GitObjectId, Hash, WorkingCopyId};
use thiserror::Error;

/// Lifecycle class of a change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ChangeKind {
    /// Durable repository history.
    #[default]
    Durable,
    /// Complete working-copy state relative to its durable baseline.
    Snapshot {
        /// Stable owner of this snapshot.
        working_copy: WorkingCopyId,
    },
}

impl ChangeKind {
    /// Returns the snapshot owner, if this is a snapshot.
    pub fn working_copy(&self) -> Option<WorkingCopyId> {
        match self {
            Self::Durable => None,
            Self::Snapshot { working_copy } => Some(*working_copy),
        }
    }

    /// Returns whether this change is durable history.
    pub fn is_durable(&self) -> bool {
        matches!(self, Self::Durable)
    }

    /// Returns whether this change is a working-copy snapshot.
    pub fn is_snapshot(&self) -> bool {
        matches!(self, Self::Snapshot { .. })
    }
}

/// How an Atomic change was derived from Git history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitDerivation {
    Root,
    FirstParent,
    MultiParent,
    Squash,
    EmptyCommit,
    RewriteCandidate,
}

/// Concise public name for Git change derivation.
pub type Derivation = GitDerivation;

/// Hash-authoritative origin of a change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ChangeOrigin {
    /// Authored natively by Atomic.
    #[default]
    Native,
    /// Synthesized from one Git commit.
    GitSynthesized {
        /// Git commit represented by this change.
        commit: GitObjectId,
        /// Complete Git parent list in original commit order.
        parents: Vec<GitObjectId>,
        /// Derivation used to construct the Atomic change.
        derivation: GitDerivation,
    },
    /// Synthesized resolution for a Git merge commit.
    GitResolution {
        /// Git merge commit represented by this resolution.
        merge_commit: GitObjectId,
        /// Complete Git parent list in original commit order.
        parents: Vec<GitObjectId>,
    },
}

impl ChangeOrigin {
    /// Construct and validate a synthesized Git origin.
    pub fn git_synthesized(
        commit: GitObjectId,
        parents: Vec<GitObjectId>,
        derivation: GitDerivation,
    ) -> Result<Self, ChangeValidationError> {
        let origin = Self::GitSynthesized {
            commit,
            parents,
            derivation,
        };
        origin.validate()?;
        Ok(origin)
    }

    /// Construct and validate a Git merge-resolution origin.
    pub fn git_resolution(
        merge_commit: GitObjectId,
        parents: Vec<GitObjectId>,
    ) -> Result<Self, ChangeValidationError> {
        let origin = Self::GitResolution {
            merge_commit,
            parents,
        };
        origin.validate()?;
        Ok(origin)
    }

    /// Returns whether this origin was authored natively by Atomic.
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native)
    }

    /// Ordered Git parents, if this is a Git origin.
    pub fn git_parents(&self) -> Option<&[GitObjectId]> {
        match self {
            Self::Native => None,
            Self::GitSynthesized { parents, .. } | Self::GitResolution { parents, .. } => {
                Some(parents)
            }
        }
    }

    /// Validate origin-local invariants without changing parent order.
    pub fn validate(&self) -> Result<(), ChangeValidationError> {
        match self {
            Self::Native => Ok(()),
            Self::GitSynthesized {
                commit,
                parents,
                derivation,
            } => {
                validate_git_algorithms("GitSynthesized", commit, parents)?;
                let actual = parents.len();
                let valid = match derivation {
                    GitDerivation::Root => actual == 0,
                    GitDerivation::FirstParent => actual == 1,
                    GitDerivation::MultiParent => actual >= 2,
                    GitDerivation::Squash
                    | GitDerivation::EmptyCommit
                    | GitDerivation::RewriteCandidate => true,
                };
                if !valid {
                    return Err(ChangeValidationError::InvalidDerivationParentCount {
                        derivation: *derivation,
                        actual,
                    });
                }
                Ok(())
            }
            Self::GitResolution {
                merge_commit,
                parents,
            } => {
                validate_git_algorithms("GitResolution", merge_commit, parents)?;
                if parents.len() < 2 {
                    return Err(
                        ChangeValidationError::GitResolutionRequiresMultipleParents {
                            actual: parents.len(),
                        },
                    );
                }
                Ok(())
            }
        }
    }
}

/// Canonical roots whose verified closures are causally known by a change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CausalFrontier {
    roots: Vec<Hash>,
}

impl CausalFrontier {
    /// Construct a frontier from an already-canonical root list.
    pub fn new(roots: Vec<Hash>) -> Result<Self, ChangeValidationError> {
        validate_canonical_hashes("causal_frontier", &roots)?;
        Ok(Self { roots })
    }

    /// Construct an empty frontier.
    pub const fn empty() -> Self {
        Self { roots: Vec::new() }
    }

    /// Canonically ordered closure roots.
    pub fn roots(&self) -> &[Hash] {
        &self.roots
    }

    /// Returns whether this frontier has no roots.
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub(crate) fn validate(&self) -> Result<(), ChangeValidationError> {
        validate_canonical_hashes("causal_frontier", &self.roots)
    }
}

/// Repository-verified closure membership for one causal frontier.
///
/// Values can only be constructed by the apply verifier after every frontier
/// root and transitive dependency has been found in the complete pristine
/// dependency index. Conflict detection may then use this set without treating
/// an unverified root as proof of causal knowledge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifiedCausalFrontier {
    frontier: CausalFrontier,
    known: Arc<BTreeSet<Hash>>,
}

impl VerifiedCausalFrontier {
    /// Empty verified knowledge for a change with no causal frontier.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether a hash belongs to the verified transitive closure.
    pub fn contains(&self, hash: &Hash) -> bool {
        self.known.contains(hash)
    }

    /// Number of changes in the verified closure, including its roots.
    pub fn len(&self) -> usize {
        self.known.len()
    }

    /// Whether the verified closure contains no changes.
    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }

    pub(crate) fn matches(&self, frontier: &CausalFrontier) -> bool {
        &self.frontier == frontier
    }

    pub(crate) fn from_verified(frontier: CausalFrontier, known: BTreeSet<Hash>) -> Self {
        Self {
            frontier,
            known: Arc::new(known),
        }
    }
}

/// Invalid or noncanonical hashed change facts.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ChangeValidationError {
    #[error("{field} contains the reserved zero hash at index {index}")]
    ZeroHash { field: &'static str, index: usize },

    #[error("{field} is not strictly ordered at index {index}")]
    NonCanonicalHashOrder { field: &'static str, index: usize },

    #[error("snapshot changes must have Native origin")]
    SnapshotMustBeNative,

    #[error("supersedes is only valid for Snapshot changes")]
    SupersedesRequiresSnapshot,

    #[error("a snapshot may not depend on the change it supersedes")]
    SupersedesDependency,

    #[error("dependencies and extra_known overlap at {hash}")]
    DependencyExtraKnownOverlap { hash: Hash },

    #[error("Git parent {parent_index} was copied into Atomic dependencies")]
    GitParentDependency { parent_index: usize },

    #[error("Git origins require Durable change kind")]
    GitOriginRequiresDurable,

    #[error("GitResolution requires at least two ordered parents, got {actual}")]
    GitResolutionRequiresMultipleParents { actual: usize },

    #[error("GitResolution requires a non-empty causal frontier")]
    GitResolutionRequiresFrontier,

    #[error("a causal frontier is only valid for GitResolution changes")]
    CausalFrontierRequiresGitResolution,

    #[error("{origin} mixes Git hash algorithms at parent index {parent_index}: expected {expected:?}, got {actual:?}")]
    MixedGitHashAlgorithms {
        origin: &'static str,
        parent_index: usize,
        expected: GitHashAlgorithm,
        actual: GitHashAlgorithm,
    },

    #[error("{derivation:?} derivation has invalid parent count {actual}")]
    InvalidDerivationParentCount {
        derivation: GitDerivation,
        actual: usize,
    },
}

pub(crate) fn validate_canonical_hashes(
    field: &'static str,
    hashes: &[Hash],
) -> Result<(), ChangeValidationError> {
    for (index, hash) in hashes.iter().enumerate() {
        if *hash == Hash::NONE {
            return Err(ChangeValidationError::ZeroHash { field, index });
        }
        if index > 0 && hashes[index - 1] >= *hash {
            return Err(ChangeValidationError::NonCanonicalHashOrder { field, index });
        }
    }
    Ok(())
}

fn validate_git_algorithms(
    origin: &'static str,
    primary: &GitObjectId,
    parents: &[GitObjectId],
) -> Result<(), ChangeValidationError> {
    let expected = primary.algorithm();
    for (parent_index, parent) in parents.iter().enumerate() {
        let actual = parent.algorithm();
        if actual != expected {
            return Err(ChangeValidationError::MixedGitHashAlgorithms {
                origin,
                parent_index,
                expected,
                actual,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha1(byte: u8) -> GitObjectId {
        GitObjectId::new(GitHashAlgorithm::Sha1, vec![byte; 20]).unwrap()
    }

    fn sha256(byte: u8) -> GitObjectId {
        GitObjectId::new(GitHashAlgorithm::Sha256, vec![byte; 32]).unwrap()
    }

    #[test]
    fn causal_frontier_rejects_duplicate_and_unsorted_roots() {
        let low = Hash::from_bytes([1; 32]);
        let high = Hash::from_bytes([2; 32]);
        assert!(CausalFrontier::new(vec![low, low]).is_err());
        assert!(CausalFrontier::new(vec![high, low]).is_err());
        assert!(CausalFrontier::new(vec![low, high]).is_ok());
    }

    #[test]
    fn causal_frontier_rejects_zero_hash() {
        assert!(matches!(
            CausalFrontier::new(vec![Hash::NONE]),
            Err(ChangeValidationError::ZeroHash { .. })
        ));
    }

    #[test]
    fn synthesized_origin_preserves_parent_order() {
        let parents = vec![sha1(9), sha1(2)];
        let origin =
            ChangeOrigin::git_synthesized(sha1(1), parents.clone(), GitDerivation::MultiParent)
                .unwrap();
        assert_eq!(origin.git_parents().unwrap(), parents);
    }

    #[test]
    fn derivation_parent_counts_are_checked() {
        assert!(
            ChangeOrigin::git_synthesized(sha1(1), vec![sha1(2)], GitDerivation::Root).is_err()
        );
        assert!(
            ChangeOrigin::git_synthesized(sha1(1), vec![], GitDerivation::FirstParent).is_err()
        );
        assert!(
            ChangeOrigin::git_synthesized(sha1(1), vec![sha1(2)], GitDerivation::MultiParent,)
                .is_err()
        );
    }

    #[test]
    fn git_origin_rejects_mixed_algorithms() {
        assert!(matches!(
            ChangeOrigin::git_synthesized(sha1(1), vec![sha256(2)], GitDerivation::FirstParent,),
            Err(ChangeValidationError::MixedGitHashAlgorithms { .. })
        ));
    }

    #[test]
    fn git_resolution_requires_multiple_parents() {
        assert!(matches!(
            ChangeOrigin::git_resolution(sha1(1), vec![sha1(2)]),
            Err(ChangeValidationError::GitResolutionRequiresMultipleParents { actual: 1 })
        ));
    }
}
