//! CB-9A: transactional foreign Git synthesis through normal assembly.
//!
//! Root and single-parent Git commits are synthesized like any native
//! recorded change: `RecordedFile`s are diffed against the bytes the
//! repository already holds, assembled by the normal
//! [`assemble_change`](atomic_core::record::workflow::assemble_change)
//! globalization (which computes graph dependencies from graph context and
//! emits semantic FileOps), applied additively by the normal graph writers,
//! and projected through the central operation-aware tree projection — all
//! inside the shared workspace boundary. Every synthesized change carries a
//! hash-authoritative [`ChangeOrigin::GitSynthesized`] naming the tagged
//! commit OID, the complete ordered Git parents, and the derivation; the
//! tagged commit/tree OIDs, boundaries, and the bridge-sequencing label
//! travel in the unhashed metadata. Git parents are ancestry evidence: they
//! never enter Atomic dependencies.
//!
//! The synthesis is journaled as an [`OperationKind::SynthesizeGit`]
//! operation with a leased `ViewChange` metadata transition before any
//! visible effect, so an interrupted import can never publish an unverified
//! view advance.

use unicode_normalization::UnicodeNormalization;

use atomic_core::change::{
    CausalFrontier, Change, ChangeHeader, ChangeKind, ChangeOrigin, ChangeStore, GitDerivation,
};
use atomic_core::operation::{
    ActorRef, GitHashAlgorithm, GitObjectId, MetadataTarget, MetadataTransition, MetadataValue,
    OperationKind, RepoStateRef, ViewStateRef,
};
use atomic_core::pristine::{GraphTxnT, MutTxnT, ViewTxnT};
use atomic_core::types::{Base32, Hash, Merkle};

use super::content::retrieve_content_with_filter_fast;
use super::insert::validate_import_deleted_paths;
use super::locks::WorkingCopyOperationLockGuard;
use super::operation::{current_operation_timestamp_ms, working_copy_state_ref};
use super::{ImportWriteOutcome, Repository, RepositoryError, WorkspaceTxn};

/// Hashed origin facts for one synthesized Git commit.
///
/// Every OID is algorithm-tagged ([`GitObjectId`]) and the parent list is the
/// complete, original-order Git parent list of the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSynthesisOrigin {
    /// Git commit represented by the synthesized change.
    pub commit: GitObjectId,
    /// Git tree of the commit (unhashed provenance; never identity).
    pub tree: GitObjectId,
    /// Complete ordered Git parents — ancestry evidence, never dependencies.
    pub parents: Vec<GitObjectId>,
    /// How the Atomic change derives from the commit.
    pub derivation: GitDerivation,
}

/// Hashed origin facts for one synthesized Git merge resolution (CB-9B).
///
/// The origin names the merge commit and its complete ordered parents; the
/// canonical frontier roots name the closures the resolution was assembled
/// on top of. Verification of the frontier happens at apply time
/// ([`atomic_core::apply::verify_causal_frontier`]); a missing root or an
/// incomplete dependency index fails closed before any mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitResolutionOrigin {
    /// Git merge commit represented by this resolution.
    pub merge_commit: GitObjectId,
    /// Complete ordered Git parents — ancestry evidence, never dependencies.
    pub parents: Vec<GitObjectId>,
    /// Verified-at-apply causal frontier covering every parent closure,
    /// including transitive ancestors.
    pub frontier: CausalFrontier,
}

impl GitResolutionOrigin {
    /// Construct and validate a merge-resolution origin with the given
    /// frontier. The frontier must be non-empty and canonically ordered.
    pub fn new(
        merge_commit: GitObjectId,
        parents: Vec<GitObjectId>,
        frontier: CausalFrontier,
    ) -> Result<Self, RepositoryError> {
        if frontier.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: "a Git resolution requires a non-empty causal frontier".to_string(),
            });
        }
        let origin = Self {
            merge_commit,
            parents,
            frontier,
        };
        // Reuse the hashed classification contract so an origin that would
        // fail change validation is refused before any bytes are written.
        origin
            .validated()
            .map_err(|error| RepositoryError::InvalidOperation {
                message: format!("invalid Git resolution origin: {error}"),
            })?;
        Ok(origin)
    }

    /// Validate the origin against the hashed classification contract.
    pub fn validated(&self) -> Result<ChangeOrigin, RepositoryError> {
        ChangeOrigin::git_resolution(self.merge_commit.clone(), self.parents.clone()).map_err(
            |error| RepositoryError::InvalidOperation {
                message: format!("invalid Git resolution origin: {error}"),
            },
        )
    }
}

/// The hashed classification applied to one synthesized change.
#[derive(Debug, Clone)]
pub(crate) enum SynthesisClassification {
    /// A single-commit synthesis (`ChangeOrigin::GitSynthesized`).
    Synthesized(GitSynthesisOrigin),
    /// A merge resolution (`ChangeOrigin::GitResolution`) with a verified
    /// causal frontier.
    Resolution(GitResolutionOrigin),
}

impl SynthesisClassification {
    fn validated_origin(&self) -> Result<ChangeOrigin, RepositoryError> {
        match self {
            Self::Synthesized(origin) => origin.validated(),
            Self::Resolution(origin) => origin.validated(),
        }
    }

    fn frontier(&self) -> CausalFrontier {
        match self {
            Self::Synthesized(_) => CausalFrontier::empty(),
            Self::Resolution(origin) => origin.frontier.clone(),
        }
    }

    /// The unhashed provenance enrichment shared by both synthesis kinds.
    fn unhashed_git_facts(&self, git: &mut serde_json::Map<String, serde_json::Value>) {
        match self {
            Self::Synthesized(origin) => {
                git.insert(
                    "origin".to_string(),
                    serde_json::Value::String("git_synthesized".to_string()),
                );
                git.insert(
                    "commit".to_string(),
                    serde_json::Value::String(git_oid_hex(&origin.commit)),
                );
                git.insert(
                    "commit_algorithm".to_string(),
                    serde_json::Value::String(
                        git_algorithm_label(origin.commit.algorithm()).to_string(),
                    ),
                );
                git.insert(
                    "tree".to_string(),
                    serde_json::Value::String(git_oid_hex(&origin.tree)),
                );
                git.insert(
                    "tree_algorithm".to_string(),
                    serde_json::Value::String(
                        git_algorithm_label(origin.tree.algorithm()).to_string(),
                    ),
                );
                git.insert(
                    "parents".to_string(),
                    serde_json::Value::Array(
                        origin
                            .parents
                            .iter()
                            .map(|parent| serde_json::Value::String(git_oid_hex(parent)))
                            .collect(),
                    ),
                );
                git.insert(
                    "derivation".to_string(),
                    serde_json::Value::String(derivation_label(origin.derivation).to_string()),
                );
            }
            Self::Resolution(origin) => {
                git.insert(
                    "origin".to_string(),
                    serde_json::Value::String("git_resolution".to_string()),
                );
                git.insert(
                    "commit".to_string(),
                    serde_json::Value::String(git_oid_hex(&origin.merge_commit)),
                );
                git.insert(
                    "commit_algorithm".to_string(),
                    serde_json::Value::String(
                        git_algorithm_label(origin.merge_commit.algorithm()).to_string(),
                    ),
                );
                git.insert(
                    "parents".to_string(),
                    serde_json::Value::Array(
                        origin
                            .parents
                            .iter()
                            .map(|parent| serde_json::Value::String(git_oid_hex(parent)))
                            .collect(),
                    ),
                );
                git.insert(
                    "frontier_roots".to_string(),
                    serde_json::Value::Array(
                        origin
                            .frontier
                            .roots()
                            .iter()
                            .map(|root| serde_json::Value::String(root.to_base32()))
                            .collect(),
                    ),
                );
            }
        }
    }
}

impl GitSynthesisOrigin {
    /// Construct and validate a root-commit synthesis origin.
    pub fn root(commit: GitObjectId, tree: GitObjectId) -> Result<Self, RepositoryError> {
        Self::build(commit, tree, Vec::new(), GitDerivation::Root)
    }

    /// Construct and validate a single-parent synthesis origin.
    pub fn first_parent(
        commit: GitObjectId,
        tree: GitObjectId,
        parent: GitObjectId,
    ) -> Result<Self, RepositoryError> {
        Self::build(commit, tree, vec![parent], GitDerivation::FirstParent)
    }

    /// Construct and validate an origin with an explicit derivation
    /// (CB-9B: empty commits carry `Derivation::EmptyCommit` with their
    /// complete ordered parents).
    pub fn with_derivation(
        commit: GitObjectId,
        tree: GitObjectId,
        parents: Vec<GitObjectId>,
        derivation: GitDerivation,
    ) -> Result<Self, RepositoryError> {
        Self::build(commit, tree, parents, derivation)
    }

    fn build(
        commit: GitObjectId,
        tree: GitObjectId,
        parents: Vec<GitObjectId>,
        derivation: GitDerivation,
    ) -> Result<Self, RepositoryError> {
        let origin = Self {
            commit,
            tree,
            parents,
            derivation,
        };
        origin.validated()?;
        Ok(origin)
    }

    /// Validate the origin against the hashed classification contract.
    ///
    /// This reuses [`ChangeOrigin::git_synthesized`] validation so an origin
    /// that would fail change validation is refused before any bytes are
    /// written.
    pub fn validated(&self) -> Result<ChangeOrigin, RepositoryError> {
        ChangeOrigin::git_synthesized(self.commit.clone(), self.parents.clone(), self.derivation)
            .map_err(|error| RepositoryError::InvalidOperation {
                message: format!("invalid Git synthesis origin: {error}"),
            })
    }
}

/// Outcome of resurrecting a verified binding's closure into a view.
#[derive(Debug, Clone)]
pub struct ResurrectionOutcome {
    /// The binding whose closure was resurrected.
    pub binding_id: crate::git_binding::BindingId,
    /// Changes referenced into the view by this call, in binding order.
    pub inserted: Vec<Hash>,
    /// Closure changes that were already members of the view.
    pub already_present: Vec<Hash>,
}

/// Outcome of one journaled Git synthesis.
#[derive(Debug, Clone)]
pub struct SynthesisOutcome {
    /// Graph write outcome (hash, timings, view insertion).
    pub write: ImportWriteOutcome,
    /// The journaled `SynthesizeGit` operation, when this call advanced the
    /// view. Idempotent re-imports return `None`.
    pub operation: Option<atomic_core::types::OperationId>,
    /// Whether the synthesized change was already a member of the target
    /// view (true idempotent re-import: nothing was mutated).
    pub already_in_view: bool,
}

/// Lowercase hex of a tagged Git object ID.
pub(crate) fn git_oid_hex(oid: &GitObjectId) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(oid.as_bytes().len() * 2);
    for byte in oid.as_bytes() {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Lowercase name of a Git hash algorithm (unhashed provenance tag).
pub(crate) fn git_algorithm_label(algorithm: GitHashAlgorithm) -> &'static str {
    match algorithm {
        GitHashAlgorithm::Sha1 => "sha1",
        GitHashAlgorithm::Sha256 => "sha256",
    }
}

/// Lowercase name of a Git derivation (unhashed provenance tag).
pub(crate) fn derivation_label(derivation: GitDerivation) -> &'static str {
    match derivation {
        GitDerivation::Root => "root",
        GitDerivation::FirstParent => "first_parent",
        GitDerivation::MultiParent => "multi_parent",
        GitDerivation::Squash => "squash",
        GitDerivation::EmptyCommit => "empty_commit",
        GitDerivation::RewriteCandidate => "rewrite_candidate",
    }
}

/// Compute the union-state causal frontier of `view`: the minimal canonical
/// set of members no other member depends on. Their transitive dependency
/// closures (verified at apply time) cover every member, including every
/// transitive ancestor of every imported parent closure.
pub(crate) fn compute_union_frontier<T: ViewTxnT>(
    txn: &T,
    view: &atomic_core::pristine::ViewState,
) -> Result<CausalFrontier, RepositoryError> {
    let mut members: std::collections::BTreeSet<Hash> = std::collections::BTreeSet::new();
    let mut depended: std::collections::BTreeSet<Hash> = std::collections::BTreeSet::new();
    for entry in txn
        .iter_changes(view, 0)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    {
        let (_, change_id, _) = entry.map_err(|e| RepositoryError::Database(e.to_string()))?;
        let hash = txn
            .get_external(change_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| {
                RepositoryError::Database(format!("view member {change_id:?} has no external hash"))
            })?;
        members.insert(hash);
        for dep in txn
            .get_change_deps(change_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            depended.insert(dep);
        }
    }
    let roots: Vec<Hash> = members
        .into_iter()
        .filter(|hash| !depended.contains(hash))
        .collect();
    if roots.is_empty() {
        return Err(RepositoryError::InvalidOperation {
            message: "cannot compute a causal frontier for an empty view state".to_string(),
        });
    }
    CausalFrontier::new(roots).map_err(|error| RepositoryError::InvalidOperation {
        message: format!("union frontier is not canonical: {error}"),
    })
}

/// Compute the union-state causal frontier over an explicit member set.
///
/// The roots are the given changes that no other *given* change depends on.
/// Their transitive dependency closures (verified at apply time) cover the
/// set. Unrelated view members are never consulted, so a resolution's
/// frontier claims exactly the parent closures (review R1).
pub(crate) fn compute_union_frontier_for_hashes<T: GraphTxnT>(
    txn: &T,
    hashes: &[Hash],
) -> Result<CausalFrontier, RepositoryError> {
    let mut members: std::collections::BTreeSet<Hash> = std::collections::BTreeSet::new();
    let mut depended: std::collections::BTreeSet<Hash> = std::collections::BTreeSet::new();
    for hash in hashes {
        members.insert(*hash);
        let Some(change_id) = txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "frontier member {} is not registered locally",
                    hash.to_base32()
                ),
            });
        };
        for dep in txn
            .get_change_deps(change_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            depended.insert(dep);
        }
    }
    let roots: Vec<Hash> = members
        .into_iter()
        .filter(|hash| !depended.contains(hash))
        .collect();
    if roots.is_empty() {
        return Err(RepositoryError::InvalidOperation {
            message: "cannot compute a causal frontier for an empty member set".to_string(),
        });
    }
    CausalFrontier::new(roots).map_err(|error| RepositoryError::InvalidOperation {
        message: format!("union frontier is not canonical: {error}"),
    })
}

/// Build the CB-9A/9B unhashed Git provenance object.
///
/// `extra` is the caller's existing `{"git": {...}}` payload (repository,
/// sha, short_sha, diff lines); its `git` object is preserved for legacy
/// readers and enriched with the tagged commit/tree OIDs, the complete
/// ordered parents, the derivation, the bridge-sequencing label, and any
/// explicit closure boundaries. Unhashed metadata is provenance, never
/// identity: the hash-authoritative origin lives in the change header.
pub fn git_synthesis_metadata(
    origin: &GitSynthesisOrigin,
    boundaries: &[&'static str],
    extra: Option<serde_json::Value>,
) -> serde_json::Value {
    git_classification_metadata(
        &SynthesisClassification::Synthesized(origin.clone()),
        boundaries,
        extra,
    )
}

/// Build the unhashed Git provenance object for any synthesis
/// classification (single commit or merge resolution).
pub fn git_classification_metadata(
    classification: &SynthesisClassification,
    boundaries: &[&'static str],
    extra: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut git = extra
        .as_ref()
        .and_then(|value| value.get("git"))
        .and_then(|value| value.as_object())
        .cloned()
        .unwrap_or_default();
    classification.unhashed_git_facts(&mut git);
    git.insert(
        "sequencing".to_string(),
        serde_json::Value::String("bridge".to_string()),
    );
    if !boundaries.is_empty() {
        git.insert(
            "boundaries".to_string(),
            serde_json::Value::Array(
                boundaries
                    .iter()
                    .map(|boundary| serde_json::Value::String((*boundary).to_string()))
                    .collect(),
            ),
        );
    }
    serde_json::json!({ "git": git })
}

/// Build the unhashed Git provenance object for a merge resolution (CB-9B).
pub fn git_resolution_metadata(
    origin: &GitResolutionOrigin,
    boundaries: &[&'static str],
    extra: Option<serde_json::Value>,
) -> serde_json::Value {
    git_classification_metadata(
        &SynthesisClassification::Resolution(origin.clone()),
        boundaries,
        extra,
    )
}

// Deterministic mutation-stage fault switch for tests only. When set, the
// apply stage fails after the `SynthesizeGit` intent was journaled, so
// recovery/containment can be exercised end to end.
#[cfg(test)]
thread_local! {
    pub(crate) static SYNTHESIS_APPLY_FAULT: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Per-path expectation checked against the staged graph projection after a
/// synthesized change is applied and before its transaction commits.
///
/// Every path the Git commit touched must project exactly as Git recorded it:
/// `Some(bytes)` for present paths and `None` for deleted paths. A mismatch
/// fails the synthesis closed (nothing is published), so a wrong or partially
/// applied change can never become a visible, verified view member.
#[derive(Debug, Clone, Default)]
pub struct StagedExpectation {
    /// path → expected bytes; `None` requires the path to be absent.
    pub paths: Vec<(String, Option<Vec<u8>>)>,
    /// The complete expected repository tree after this commit (review R3):
    /// every path the commit's verified Git tree holds, with the exact
    /// repository bytes, mode, and kind. When present, staged verification
    /// requires full tree equality — present paths render exactly these
    /// bytes with the expected attributes, no extra path may be alive, and
    /// no conflict-table shortcut waives a mismatch.
    pub tree: Option<StagedTreeExpectation>,
}

/// One path in the complete staged tree expectation (review R3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedPathExpectation {
    /// Exact repository (post-clean-filter) bytes Git's tree holds.
    pub bytes: Vec<u8>,
    /// Expected repository mode (e.g. `0o644`).
    pub mode: u16,
    /// Expected inode kind.
    pub kind: atomic_core::change::InodeKind,
}

/// The complete expected repository tree for one imported commit (review R3).
///
/// Built from the commit's verified prospective manifest — every entry Git's
/// tree holds after the commit (including untouched paths carried from the
/// parent tree), so the staged check compares the whole canonical tree, not
/// only the bytes the commit touched.
#[derive(Debug, Clone, Default)]
pub struct StagedTreeExpectation {
    /// path → expected repository entry.
    pub entries: std::collections::BTreeMap<String, StagedPathExpectation>,
    /// Paths that must carry regenerated semantic FileOps in the staged
    /// change (regular text files the commit adds or modifies). Deletions,
    /// pure moves, binary content, and opaque generated files are excluded
    /// by the caller.
    pub semantic_paths: Vec<String>,
}

impl StagedExpectation {
    /// An empty expectation: nothing to verify beyond structural integrity.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether any path is expected present.
    pub fn expects_present_paths(&self) -> bool {
        self.paths.iter().any(|(_, bytes)| bytes.is_some())
    }

    /// Whether a complete tree expectation is present.
    pub fn expects_complete_tree(&self) -> bool {
        self.tree.is_some()
    }
}

/// Injection seams for the CB-9B fail-closed matrix. Every seam is compiled
/// out of shipping builds: no environment variable is read unless the
/// `adoption-test-injection` feature is enabled (the same policy as the
/// CB-8B adoption failpoints).
#[cfg(feature = "adoption-test-injection")]
pub(crate) mod failpoints {
    use super::RepositoryError;
    use atomic_core::pristine::MutTxnT;

    fn fail(name: &str) -> Result<(), RepositoryError> {
        if std::env::var_os(name).is_some() {
            return Err(RepositoryError::InvalidOperation {
                message: format!("injected synthesis failpoint: {name}"),
            });
        }
        Ok(())
    }

    /// Simulates a missing graph vertex: staged content retrieval fails.
    pub fn vertex() -> Result<(), RepositoryError> {
        fail("ATOMIC_FAIL_SYNTHESIS_VERTEX")
    }

    /// Simulates an incomplete dependency index: frontier verification fails.
    pub fn deps_index() -> Result<(), RepositoryError> {
        fail("ATOMIC_FAIL_SYNTHESIS_DEPS_INDEX")
    }

    /// Simulates a change-store record failure before the graph write.
    pub fn record() -> Result<(), RepositoryError> {
        fail("ATOMIC_FAIL_SYNTHESIS_RECORD")
    }

    /// Review ::26 R4 oracle: GENUINE process death at the NAMED durability
    /// checkpoint "change bytes durably saved + change registered, graph
    /// NOT yet applied". Called at that exact point when
    /// ATOMIC_FAIL_SYNTHESIS_KILL_CHANGE_BOUNDARY is set: abort() raises
    /// SIGABRT — no destructors, no fsync of the open redb transaction, the
    /// process dies between the change-store save and the graph write. This
    /// is NOT an orderly Err like `record()`: the caller must treat the
    /// child's non-zero/signal exit as the oracle.
    #[cfg(feature = "adoption-test-injection")]
    pub fn kill_at_change_boundary() {
        if std::env::var_os("ATOMIC_FAIL_SYNTHESIS_KILL_CHANGE_BOUNDARY").is_some() {
            // Flush nothing deliberately: the checkpoint is BEFORE the redb
            // commit, so the open transaction is discarded by the death.
            std::process::abort();
        }
    }

    /// Injected semantic corruption (review F4, untouched path): repoints
    /// every CRDT branch of the expectation's UNTOUCHED paths (expected
    /// entries this change did not record deltas for) to a wrong graph
    /// range, so the staged semantic reconstruction must refuse the
    /// corrupted inherited state before publication.
    pub fn semantic_corrupt_untouched<T: MutTxnT>(
        txn: &mut T,
        untouched_paths: &[String],
    ) -> Result<(), RepositoryError> {
        if std::env::var_os("ATOMIC_FAIL_SYNTHESIS_SEMANTIC_CORRUPT_UNTOUCHED").is_some() {
            use atomic_core::crdt::queries::iter_trunk_branches_in_file_order;
            use atomic_core::crdt::tables::{encode_branch_id, encode_vertex_position};
            use atomic_core::pristine::CrdtTxnT;
            use atomic_core::types::{ChangePosition, GraphNode, NodeId};
            for path in untouched_paths {
                let Some(trunk_id) = txn.get_trunk_by_path(path)? else {
                    continue;
                };
                for branch_id in iter_trunk_branches_in_file_order(&*txn, trunk_id)? {
                    let branch_key = encode_branch_id(&branch_id);
                    if txn.get_crdt_branch(&branch_key)?.is_none() {
                        continue;
                    }
                    let wrong = GraphNode {
                        change: NodeId::ROOT,
                        start: ChangePosition::ROOT,
                        end: ChangePosition::ROOT,
                    };
                    txn.put_crdt_branch_vertex(&branch_key, &encode_vertex_position(&wrong))?;
                }
            }
        }
        Ok(())
    }

    /// Simulates a tree-projection failure after the graph write.
    pub fn projection() -> Result<(), RepositoryError> {
        fail("ATOMIC_FAIL_SYNTHESIS_PROJECTION")
    }

    /// Injected semantic corruption (review F4, missing-trunk): deletes the
    /// path→trunk index rows for the expectation's semantic delta paths
    /// AFTER the FileOps were applied but BEFORE staged verification, so the
    /// verification must refuse the corrupted CRDT state before publication.
    pub fn semantic_missing_trunk<T: MutTxnT>(
        txn: &mut T,
        paths: &[String],
    ) -> Result<(), RepositoryError> {
        if std::env::var_os("ATOMIC_FAIL_SYNTHESIS_SEMANTIC_MISSING_TRUNK").is_some() {
            for path in paths {
                txn.del_crdt_path_trunk(path)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Injected semantic corruption (review F4, wrong payload): repoints
    /// every CRDT branch of the expectation's semantic delta paths to a
    /// wrong graph range, so the staged semantic reconstruction must refuse
    /// the corrupted payload before publication.
    pub fn semantic_corrupt_vertex<T: MutTxnT>(
        txn: &mut T,
        paths: &[String],
    ) -> Result<(), RepositoryError> {
        if std::env::var_os("ATOMIC_FAIL_SYNTHESIS_SEMANTIC_CORRUPT_VERTEX").is_some() {
            use atomic_core::crdt::queries::iter_trunk_branches_in_file_order;
            use atomic_core::crdt::tables::{encode_branch_id, encode_vertex_position};
            use atomic_core::types::{ChangePosition, GraphNode, NodeId};
            for path in paths {
                let Some(trunk_id) = txn.get_trunk_by_path(path)? else {
                    continue;
                };
                for branch_id in iter_trunk_branches_in_file_order(&*txn, trunk_id)? {
                    let branch_key = encode_branch_id(&branch_id);
                    if txn.get_crdt_branch(&branch_key)?.is_none() {
                        continue;
                    }
                    let wrong = GraphNode {
                        change: NodeId::ROOT,
                        start: ChangePosition::ROOT,
                        end: ChangePosition::ROOT,
                    };
                    txn.put_crdt_branch_vertex(&branch_key, &encode_vertex_position(&wrong))?;
                }
            }
        }
        Ok(())
    }

    /// Injected inherited-leaf corruption (review C1): flips every leaf
    /// row's payload bounds of the expectation's UNTOUCHED paths far outside
    /// the recorded token lengths. The staged verification must refuse the
    /// corrupted inherited token rows before publication.
    pub fn semantic_corrupt_leaf_range<T: MutTxnT>(
        txn: &mut T,
        untouched_paths: &[String],
    ) -> Result<(), RepositoryError> {
        if std::env::var_os("ATOMIC_FAIL_SYNTHESIS_SEMANTIC_LEAF_RANGE").is_some() {
            use atomic_core::crdt::queries::iter_trunk_branches_in_file_order;
            use atomic_core::crdt::tables::{decode_leaf_id, encode_leaf_value};
            use atomic_core::pristine::CrdtTxnT;
            for path in untouched_paths {
                let Some(trunk_id) = txn.get_trunk_by_path(path)? else {
                    continue;
                };
                for branch_id in iter_trunk_branches_in_file_order(&*txn, trunk_id)? {
                    let branch_key = atomic_core::crdt::tables::encode_branch_id(&branch_id);
                    let leaf_keys: Vec<[u8; 12]> = txn
                        .iter_branch_leaves(&branch_key)?
                        .collect::<Result<Vec<_>, _>>()?;
                    for leaf_key in leaf_keys {
                        if let Some(mut row) = txn.get_crdt_leaf(&leaf_key)? {
                            row.content_start = 10_000;
                            row.content_end = 20_000;
                            txn.put_crdt_leaf(&leaf_key, &encode_leaf_value(&row))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Injected inherited-leaf corruption (review C1): flips every token's
    /// kind of the expectation's UNTOUCHED paths to Punctuation.
    pub fn semantic_corrupt_leaf_kind<T: MutTxnT>(
        txn: &mut T,
        untouched_paths: &[String],
    ) -> Result<(), RepositoryError> {
        if std::env::var_os("ATOMIC_FAIL_SYNTHESIS_SEMANTIC_LEAF_KIND").is_some() {
            use atomic_core::crdt::queries::iter_trunk_branches_in_file_order;
            use atomic_core::crdt::tables::{decode_leaf_id, encode_leaf_value};
            use atomic_core::diff::TokenKind;
            use atomic_core::pristine::CrdtTxnT;
            for path in untouched_paths {
                let Some(trunk_id) = txn.get_trunk_by_path(path)? else {
                    continue;
                };
                for branch_id in iter_trunk_branches_in_file_order(&*txn, trunk_id)? {
                    let branch_key = atomic_core::crdt::tables::encode_branch_id(&branch_id);
                    let leaf_keys: Vec<[u8; 12]> = txn
                        .iter_branch_leaves(&branch_key)?
                        .collect::<Result<Vec<_>, _>>()?;
                    for leaf_key in leaf_keys {
                        if let Some(mut row) = txn.get_crdt_leaf(&leaf_key)? {
                            let _ = decode_leaf_id(&leaf_key);
                            row.kind = TokenKind::Punctuation;
                            txn.put_crdt_leaf(&leaf_key, &encode_leaf_value(&row))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Injected inherited-leaf corruption (review C1): repoints every leaf
    /// row of the expectation's UNTOUCHED paths to a nonexistent branch
    /// (`BranchId(ROOT, 999)`), so token ownership/linkage must be refused.
    pub fn semantic_corrupt_leaf_owner<T: MutTxnT>(
        txn: &mut T,
        untouched_paths: &[String],
    ) -> Result<(), RepositoryError> {
        if std::env::var_os("ATOMIC_FAIL_SYNTHESIS_SEMANTIC_LEAF_OWNER").is_some() {
            use atomic_core::crdt::queries::iter_trunk_branches_in_file_order;
            use atomic_core::crdt::tables::{encode_branch_id, encode_leaf_value};
            use atomic_core::crdt::{BranchId, LeafId};
            use atomic_core::types::NodeId;
            for path in untouched_paths {
                let Some(trunk_id) = txn.get_trunk_by_path(path)? else {
                    continue;
                };
                for branch_id in iter_trunk_branches_in_file_order(&*txn, trunk_id)? {
                    let branch_key = encode_branch_id(&branch_id);
                    let leaf_keys: Vec<[u8; 12]> = txn
                        .iter_branch_leaves(&branch_key)?
                        .collect::<Result<Vec<_>, _>>()?;
                    for leaf_key in leaf_keys {
                        if let Some(mut row) = txn.get_crdt_leaf(&leaf_key)? {
                            row.branch_id = BranchId::new(NodeId::ROOT, 999);
                            let _ = LeafId::ROOT;
                            txn.put_crdt_leaf(&leaf_key, &encode_leaf_value(&row))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Injected trunk-lifecycle corruption (review C1): marks the trunk row
    /// of the expectation's UNTOUCHED paths Deleted while no change deleted
    /// the file — the live-file tombstone must be refused.
    pub fn semantic_corrupt_trunk_deleted<T: MutTxnT>(
        txn: &mut T,
        untouched_paths: &[String],
    ) -> Result<(), RepositoryError> {
        if std::env::var_os("ATOMIC_FAIL_SYNTHESIS_SEMANTIC_TRUNK_DELETED").is_some() {
            use atomic_core::crdt::tables::{encode_trunk_id, encode_trunk_value};
            use atomic_core::crdt::TrunkState;
            use atomic_core::pristine::CrdtTxnT;
            for path in untouched_paths {
                let Some(trunk_id) = txn.get_trunk_by_path(path)? else {
                    continue;
                };
                let key = encode_trunk_id(&trunk_id);
                if let Some(mut row) = txn.get_crdt_trunk(&key)? {
                    row.state = TrunkState::Deleted;
                    txn.put_crdt_trunk(&key, &encode_trunk_value(&row))?;
                }
            }
        }
        Ok(())
    }
}

/// Attach the hashed Git origin and unhashed provenance to an assembled
/// change and serialize it, returning `(change, bytes, hash)`.
#[allow(dead_code)]
pub(crate) fn finalize_synthesized_change(
    change: Change,
    origin: &GitSynthesisOrigin,
    unhashed: serde_json::Value,
) -> Result<(Change, Vec<u8>, Hash), RepositoryError> {
    finalize_classified_change(
        change,
        &SynthesisClassification::Synthesized(origin.clone()),
        Vec::new(),
        unhashed,
    )
}

/// Attach the hashed Git origin (with optional causal frontier) and unhashed
/// provenance to an assembled change and serialize it.
pub(crate) fn finalize_classified_change(
    change: Change,
    classification: &SynthesisClassification,
    hashed_metadata: Vec<u8>,
    unhashed: serde_json::Value,
) -> Result<(Change, Vec<u8>, Hash), RepositoryError> {
    let validated_origin = classification.validated_origin()?;
    let mut change = change
        .with_classification(
            ChangeKind::Durable,
            None,
            validated_origin,
            classification.frontier(),
        )
        .map_err(|error| RepositoryError::InvalidOperation {
            message: format!("synthesized change classification refused: {error}"),
        })?;
    if !hashed_metadata.is_empty() {
        change.hashed.metadata = hashed_metadata;
    }
    change.unhashed = Some(unhashed);

    let mut v3_bytes = Vec::new();
    let hash = change
        .serialize(&mut v3_bytes)
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let (final_change, verified_hash) = Change::deserialize(&mut v3_bytes.as_slice())
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    debug_assert_eq!(hash, verified_hash);
    Ok((final_change, v3_bytes, hash))
}

/// One replayed branch of the expected semantic domain for one path
/// (review B2): the lifecycle, last-writer vertex, line bytes, chain anchor,
/// leaf sequence, and the closure change that last wrote the row.
#[derive(Clone)]
struct ExpectedBranch {
    state: atomic_core::crdt::BranchState,
    /// The graph vertex the closure's last writer bound this line to.
    vertex: Option<atomic_core::types::GraphNode<atomic_core::types::NodeId>>,
    /// The line's bytes in the last writer's content blob.
    bytes: Vec<u8>,
    /// The replayed `BRANCH_AFTER` reference (`None` = file start).
    after: Option<atomic_core::crdt::BranchId>,
    /// Expected alive leaves of the last writer (review C1): identity, kind
    /// and payload bounds are compared exactly for the whole closure domain,
    /// inherited branches included — the apply keys leaf rows by the applying
    /// change, so the expected identity is stable across applies.
    leaves: Vec<ExpectedLeaf>,
    /// Leaf ids the closure tombstoned for this branch (a Modify keeps the
    /// branch id and rewrites the line's tokens): the staged rows must not
    /// be alive.
    dead_leaves: Vec<atomic_core::crdt::LeafId>,
    /// The closure change that last wrote this branch.
    last_writer: atomic_core::types::NodeId,
}

/// One expected alive token row: identity, kind and the exact payload bounds
/// the apply writes (review C1).
#[derive(Clone)]
struct ExpectedLeaf {
    id: atomic_core::crdt::LeafId,
    kind: atomic_core::diff::TokenKind,
    content_start: u32,
    content_end: u32,
}

/// One line of the closure-aware consumer domain (review E1): the exact
/// bytes the CRDT output walker must render for one staged chain line. The
/// consumer equality is ALWAYS enforced — genuine concurrent state extends
/// the expected domain with the validated writer's own recorded content
/// instead of waiving the comparison.
enum ConsumerLine {
    /// The closure replay's exact line bytes (the staged binding equals the
    /// replayed last-writer vertex).
    Closure(Vec<u8>),
    /// A validated concurrent writer's binding: the consumer renders the
    /// writer's content blob slice at exactly this vertex.
    Writer(atomic_core::types::GraphNode<atomic_core::types::NodeId>),
}

/// The global (every applied change, unfiltered) alive vertex set of one
/// file: the arbitration set for unattributed tombstones (review B2/C1).
fn global_alive_set(
    txn: &atomic_core::pristine::WriteTxn<'_>,
    file_position: atomic_core::types::Position<atomic_core::types::NodeId>,
) -> Result<
    std::collections::HashSet<atomic_core::types::GraphNode<atomic_core::types::NodeId>>,
    String,
> {
    let started = std::time::Instant::now();
    let alive_graph = atomic_core::output::alive::retrieve_graph(
        txn,
        file_position,
        atomic_core::output::alive::RetrieveOptions::new(),
    )
    .map_err(|error| format!("cannot project the global graph: {error}"))?;
    if std::env::var_os("ATOMIC_TRACE_GIT_IMPORT").is_some() {
        eprintln!(
            "[git-import] global_alive_set: {}ms vertices={} visited={} edges={}",
            started.elapsed().as_millis(),
            alive_graph.graph.len_vertices(),
            alive_graph.positions_visited,
            alive_graph.edges_traversed,
        );
    }
    let mut set = std::collections::HashSet::new();
    for (_vertex_id, vertex) in alive_graph.graph.iter_vertices() {
        if vertex.node.is_root() || vertex.node.is_empty() {
            continue;
        }
        set.insert(vertex.node);
    }
    Ok(set)
}

/// Whether any applied change OUTSIDE the interpreted closure recorded the
/// semantic op that deleted `branch` (review C1/C3: tombstones are attributed
/// by actual operations — a registered writer's Delete/Modify for the branch
/// — not merely by graph liveness, because a whole-file replace tombstones
/// semantic branches without graph-deleting the replaced vertices).
pub(crate) fn out_of_closure_writer_deleted_branch(
    repo_change_loader: &dyn Fn(&Hash) -> Result<Change, String>,
    txn: &atomic_core::pristine::WriteTxn<'_>,
    path: &str,
    branch: atomic_core::crdt::BranchId,
    visibility: &atomic_core::pristine::GraphVisibilityClosure,
) -> Result<bool, String> {
    use atomic_core::pristine::GraphTxnT;
    for (change_id, hash) in txn.list_registered_changes().map_err(|e| e.to_string())? {
        if visibility.contains(change_id) {
            continue;
        }
        // Review F1: a registered change is NOT an applied writer. The
        // scanner may attribute a tombstone only to a change whose operations
        // actually ran in the graph; a merely registered (e.g. fetched or
        // staged) change's FileOps are not execution proof. Mirrors
        // `applied_writer_deleted_branch` (review D2).
        if !txn
            .has_change_in_graph(change_id)
            .map_err(|e| e.to_string())?
        {
            continue;
        }
        let Ok(change) = repo_change_loader(&hash) else {
            continue;
        };
        for ops in change.file_ops() {
            if ops.path() != path {
                continue;
            }
            for line_op in ops.line_ops() {
                let raw_branch = line_op.branch_id();
                let resolved = if raw_branch.change_id().is_root() {
                    atomic_core::crdt::BranchId::new(change_id, raw_branch.branch_idx())
                } else {
                    raw_branch
                };
                if resolved != branch {
                    continue;
                }
                match line_op.operation() {
                    // A Delete tomstones the branch; a Modify tombstones and
                    // re-inserts the same id (either writer legitimately
                    // produced the row's tombstone at some point).
                    atomic_core::crdt::BranchOp::Delete { .. }
                    | atomic_core::crdt::BranchOp::Modify { .. } => return Ok(true),
                    _ => {}
                }
            }
        }
    }
    Ok(false)
}

/// Whether ANY applied change recorded the semantic op that deleted
/// `branch` for `path`, regardless of which closure the writer belongs to
/// (review D2, unexpected-chain-branch attribution): a forced whole-file
/// replace inside the interpreted closure tombstones every bound branch —
/// including branches created by out-of-closure concurrent writers — so a
/// genuine tombstone's writer may live on either side of the closure
/// boundary. The loaded change must be a real applied change (a registered
/// id alone is not a writer, review D2).
fn applied_writer_deleted_branch(
    repo_change_loader: &dyn Fn(&Hash) -> Result<Change, String>,
    txn: &atomic_core::pristine::WriteTxn<'_>,
    path: &str,
    branch: atomic_core::crdt::BranchId,
) -> Result<bool, String> {
    use atomic_core::pristine::GraphTxnT;
    for (change_id, hash) in txn.list_registered_changes().map_err(|e| e.to_string())? {
        if !txn
            .has_change_in_graph(change_id)
            .map_err(|e| e.to_string())?
        {
            continue;
        }
        let Ok(change) = repo_change_loader(&hash) else {
            continue;
        };
        for ops in change.file_ops() {
            if ops.path() != path {
                continue;
            }
            for line_op in ops.line_ops() {
                let raw_branch = line_op.branch_id();
                let resolved = if raw_branch.change_id().is_root() {
                    atomic_core::crdt::BranchId::new(change_id, raw_branch.branch_idx())
                } else {
                    raw_branch
                };
                if resolved != branch {
                    continue;
                }
                match line_op.operation() {
                    // A Delete tomstones the branch; a Modify tombstones and
                    // re-inserts the same id (either writer legitimately
                    // produced the row's tombstone at some point).
                    atomic_core::crdt::BranchOp::Delete { .. }
                    | atomic_core::crdt::BranchOp::Modify { .. } => return Ok(true),
                    _ => {}
                }
            }
        }
    }
    Ok(false)
}

/// Review E1: whether the GLOBAL graph genuinely proves `branch`'s bound
/// vertex dead FOR THIS FILE. The binding must reference a real vertex of an
/// APPLIED change (a registered id alone is not a writer), the vertex must
/// exist, it must not be alive in this file's global alive set, AND the
/// introducing change's own FileOps must record THIS exact branch→vertex
/// binding for this path (exact branch→vertex file ownership). A vertex
/// belonging to a different file — registered, existing, and merely absent
/// from this file's alive set — is never deletion evidence (fixture
/// cb9b-r8-real-unrelated-vertex tombstoned f.txt's branch while binding
/// g.txt's real vertex).
fn graph_proves_dead_with_ownership(
    repo: &Repository,
    txn: &atomic_core::pristine::WriteTxn<'_>,
    path: &str,
    branch: atomic_core::crdt::BranchId,
    node: &atomic_core::types::GraphNode<atomic_core::types::NodeId>,
    global_alive: &std::collections::HashSet<
        atomic_core::types::GraphNode<atomic_core::types::NodeId>,
    >,
) -> Result<bool, String> {
    if node.change.is_root() {
        return Ok(false);
    }
    let registered = txn
        .get_external(node.change)
        .map_err(|e| e.to_string())?
        .is_some();
    let applied = txn
        .has_change_in_graph(node.change)
        .map_err(|e| e.to_string())?;
    if !(registered && applied) {
        return Ok(false);
    }
    let exists = txn.has_vertex(*node).map_err(|e| e.to_string())?;
    if !exists || global_alive.contains(node) {
        return Ok(false);
    }
    let hash = txn
        .get_external(node.change)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "branch vertex writer has no external hash".to_string())?;
    let writer = repo
        .load_change(&hash)
        .map_err(|e| format!("cannot load branch vertex writer: {e}"))?;
    Ok(change_wrote_branch_vertex_for_branch(
        &writer, path, branch, *node,
    ))
}

/// The bytes a validated branch→vertex binding renders to consumers (the
/// same slice `output_file_via_crdt` reads for the binding): the writer
/// change's content blob at exactly the vertex's range (review E1).
fn vertex_content_bytes(
    repo: &Repository,
    txn: &atomic_core::pristine::WriteTxn<'_>,
    node: &atomic_core::types::GraphNode<atomic_core::types::NodeId>,
) -> Result<Vec<u8>, String> {
    let len = (node.end.get().saturating_sub(node.start.get())) as usize;
    if len == 0 {
        return Ok(Vec::new());
    }
    if node.change.is_root() {
        return Err(
            "a branch binds the ROOT sentinel vertex; the semantic layer cannot \
             reconstruct its content"
                .to_string(),
        );
    }
    let content_hash = txn
        .get_external(node.change)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "branch vertex writer has no external hash".to_string())?;
    let hash_fn = move |id: atomic_core::types::NodeId| -> Option<Hash> {
        if id == node.change {
            Some(content_hash)
        } else {
            None
        }
    };
    let mut out = vec![0u8; len];
    repo.change_store
        .get_contents(hash_fn, *node, &mut out)
        .map_err(|e| format!("cannot read the branch vertex content: {e}"))?;
    Ok(out)
}

/// Whether an applied change OUTSIDE `visibility` recorded the semantic
/// trunk delete for `path` (review C1: operation attribution for tombstones
/// written by concurrent or superseded writers).
pub(crate) fn out_of_closure_writer_deleted_trunk(
    repo_change_loader: &dyn Fn(&Hash) -> Result<Change, String>,
    txn: &atomic_core::pristine::WriteTxn<'_>,
    path: &str,
    visibility: &atomic_core::pristine::GraphVisibilityClosure,
) -> Result<bool, String> {
    use atomic_core::pristine::GraphTxnT;
    for (change_id, hash) in txn.list_registered_changes().map_err(|e| e.to_string())? {
        if visibility.contains(change_id) {
            continue;
        }
        // Review F1: same application proof as the branch scanner — a
        // registered-but-unapplied change never attributes a trunk tombstone.
        if !txn
            .has_change_in_graph(change_id)
            .map_err(|e| e.to_string())?
        {
            continue;
        }
        let Ok(change) = repo_change_loader(&hash) else {
            continue;
        };
        for ops in change.file_ops() {
            if ops.path() != path {
                continue;
            }
            if matches!(
                ops.trunk_op(),
                Some(atomic_core::crdt::TrunkOp::Delete { .. })
            ) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Whether `change`'s own FileOps for `path` recorded the line op that bound
/// branch `branch` to `vertex` (review C1: concurrency attribution by actual
/// operations, not bare registered ids; review E1: EXACT branch→vertex file
/// ownership). The op must resolve to THIS branch id AND carry exactly the
/// vertex's content range for THIS path — a different file's real vertex
/// (the same change's other-file line op at that range) never proves a
/// binding for this file's branch.
fn change_wrote_branch_vertex_for_branch(
    change: &Change,
    path: &str,
    branch: atomic_core::crdt::BranchId,
    vertex: atomic_core::types::GraphNode<atomic_core::types::NodeId>,
) -> bool {
    for ops in change.file_ops() {
        if ops.path() != path {
            continue;
        }
        for line_op in ops.line_ops() {
            let raw_branch = line_op.branch_id();
            let resolved = if raw_branch.change_id().is_root() {
                atomic_core::crdt::BranchId::new(vertex.change, raw_branch.branch_idx())
            } else {
                raw_branch
            };
            if resolved != branch {
                continue;
            }
            match line_op.operation() {
                atomic_core::crdt::BranchOp::Insert { .. }
                | atomic_core::crdt::BranchOp::Modify { .. } => {}
                _ => continue,
            }
            if let Some((start, end)) = line_op.content_range() {
                if start.get() == vertex.start.get() && end.get() == vertex.end.get() {
                    return true;
                }
            }
        }
    }
    false
}

/// Whether `change`'s own FileOps key `leaf`'s id when applied (review D2:
/// a registered NodeId is not proof of a writer — the leaf must come from
/// the writer's own per-apply counter over the Insert/Modify content it
/// recorded for this path and branch). The replay mirrors the apply's
/// leaf-id contract exactly: one counter per change spanning every file
/// entry in order, advanced by every recorded leaf op.
fn change_wrote_leaf(
    change: &Change,
    path: &str,
    branch: atomic_core::crdt::BranchId,
    leaf: atomic_core::crdt::LeafId,
) -> bool {
    let owner = leaf.change_id();
    let mut counter = 0u32;
    for ops in change.file_ops() {
        let is_target = ops.path() == path;
        for line_op in ops.line_ops() {
            let raw_branch = line_op.branch_id();
            let resolved = if raw_branch.change_id().is_root() {
                atomic_core::crdt::BranchId::new(owner, raw_branch.branch_idx())
            } else {
                raw_branch
            };
            let content: &[atomic_core::crdt::LeafOp] = match line_op.operation() {
                atomic_core::crdt::BranchOp::Insert { content, .. } => content,
                atomic_core::crdt::BranchOp::Modify { new_content, .. } => new_content,
                _ => continue,
            };
            for leaf_op in content {
                let is_writer_leaf = matches!(leaf_op, atomic_core::crdt::LeafOp::Insert { .. })
                    && is_target
                    && resolved == branch
                    && atomic_core::crdt::LeafId::new(owner, counter) == leaf;
                if is_writer_leaf {
                    return true;
                }
                counter += 1;
            }
        }
    }
    false
}

impl Repository {
    /// Ensure every named change (with its dependency closure) is a member
    /// of `view` before a merge resolution assembles the union state.
    ///
    /// Multi-parent merges may reach the resolution with one parent's
    /// closure imported into a *different* view (e.g. `git import --all`
    /// imports side branches into their own views first). The union state
    /// must hold every parent closure, so a parent change that is verified
    /// locally but absent from `view` is referenced into it through the
    /// normal idempotent insert (metadata-only, with dependency closure).
    /// Returns the hashes that were newly referenced.
    pub fn ensure_union_state(
        &self,
        view: &str,
        hashes: &[Hash],
    ) -> Result<Vec<Hash>, RepositoryError> {
        use atomic_core::pristine::ViewTxnT;
        let mut newly_referenced = Vec::new();
        for hash in hashes {
            let present = {
                let txn = self
                    .pristine
                    .read_txn()
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                let view_state = txn
                    .get_view(view)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::ViewNotFound {
                        name: view.to_string(),
                    })?;
                match txn.get_internal(hash) {
                    Ok(Some(change_id)) => txn
                        .get_change_seq(&view_state, change_id)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?
                        .is_some(),
                    Ok(None) => false,
                    Err(e) => return Err(RepositoryError::Database(e.to_string())),
                }
            };
            if present {
                continue;
            }
            self.insert_change_rec(hash, crate::InsertOptions::default().view(view.to_string()))?;
            newly_referenced.push(*hash);
        }
        Ok(newly_referenced)
    }

    /// Compute the causal frontier of a view's current union state (CB-9B).
    ///
    /// Returns the minimal canonically ordered set of view members whose
    /// transitive dependency closures cover every member: the members that
    /// no other member depends on. A merge resolution assembled on top of
    /// this state carries exactly these roots, so its apply-time verified
    /// frontier covers every imported parent closure including transitive
    /// ancestors. `extra_known` stays untouched — it remains lossless
    /// direct-hash knowledge, never a frontier substitute.
    pub fn view_union_frontier(&self, view_name: &str) -> Result<CausalFrontier, RepositoryError> {
        use atomic_core::pristine::ViewTxnT;
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        compute_union_frontier(&txn, &view)
    }

    /// Compute the causal frontier of an explicit member set (CB-9B R1).
    ///
    /// Like [`Repository::view_union_frontier`], but computed over exactly
    /// `hashes` — the reconstructed parent interpretation closures — instead
    /// of the whole view. Unrelated same-view members never pollute the
    /// resolution's claimed coverage, and every parent closure (including
    /// transitive ancestors and unrelated-file ancestors) is covered.
    pub fn view_frontier_for_members(
        &self,
        hashes: &[Hash],
    ) -> Result<CausalFrontier, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        compute_union_frontier_for_hashes(&txn, hashes)
    }

    /// The persisted interpretation closure of one Git commit (CB-9B R1).
    ///
    /// `None` means the commit was imported before closure persistence (or
    /// was never imported here); callers fall back to reconstruction.
    pub fn git_commit_closure(&self, sha: &str) -> Result<Option<Vec<Hash>>, RepositoryError> {
        use atomic_core::pristine::GitCommitClosureTxnT;
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.get_git_commit_closure(sha)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Persist the complete interpreted closure of a Git commit that was
    /// handled outside the synthesis path (squash-insert originals, exact
    /// resurrection): `sha` → the union of the given changes plus their
    /// dependency closures.
    ///
    /// The row is what later runs and other views read to reconstruct the
    /// commit's exact parent visibility; a conflicting existing row fails
    /// closed. Returns the persisted closure.
    pub fn record_git_commit_closure(
        &self,
        sha: &str,
        change_hashes: &[Hash],
    ) -> Result<Vec<Hash>, RepositoryError> {
        let mut closure = std::collections::BTreeSet::new();
        for hash in change_hashes {
            closure.insert(*hash);
            for dep in self.change_dependency_closure(hash)? {
                closure.insert(dep);
            }
        }
        let closure: Vec<Hash> = closure.into_iter().collect();
        use atomic_core::pristine::GitCommitClosureMutTxnT;
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.put_git_commit_closure(sha, &closure)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(closure)
    }

    /// The complete transitive dependency closure of one change (including
    /// itself), from the registered dependency index.
    pub fn change_dependency_closure(&self, hash: &Hash) -> Result<Vec<Hash>, RepositoryError> {
        use atomic_core::pristine::GraphTxnT;
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let mut closure = std::collections::BTreeSet::new();
        let mut queue = vec![*hash];
        while let Some(hash) = queue.pop() {
            if !closure.insert(hash) {
                continue;
            }
            let Some(change_id) = txn
                .get_internal(&hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            else {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "dependency closure names change {} which is not registered locally",
                        hash.to_base32()
                    ),
                });
            };
            let deps = txn
                .get_change_deps(change_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            queue.extend(deps);
        }
        Ok(closure.into_iter().collect())
    }

    /// Full-chain view membership with each member's Git provenance (CB-9B
    /// R1): every change in `view` and its complete ancestor chain, paired
    /// with the Git commit SHA the member's unhashed provenance names (or
    /// `None` for non-Git work that cannot be attributed).
    ///
    /// The importer reconstructs each commit's interpreted closure from
    /// these origins plus the persisted closure rows, so exclusions are
    /// exact across runs, bindings, and views — not a per-run ledger.
    pub fn full_chain_member_origins(
        &self,
        view_name: &str,
    ) -> Result<Vec<(Hash, Option<String>)>, RepositoryError> {
        use atomic_core::pristine::ViewTxnT;
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let mut members = Vec::new();
        for member_view in txn
            .resolve_full_view_chain(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            for entry in txn
                .iter_changes(&member_view, 0)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                let (_, change_id, _) =
                    entry.map_err(|e| RepositoryError::Database(e.to_string()))?;
                let hash = txn
                    .get_external(change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| {
                        RepositoryError::Database(format!(
                            "view member {change_id:?} has no external hash"
                        ))
                    })?;
                let origin_sha = match self.change_store.load_change(&hash) {
                    Ok(change) => change
                        .unhashed
                        .as_ref()
                        .and_then(|value| value.get("git"))
                        .and_then(|git| git.get("sha"))
                        .and_then(|sha| sha.as_str())
                        .map(|sha| sha.to_ascii_lowercase()),
                    Err(_) => None,
                };
                members.push((hash, origin_sha));
            }
        }
        Ok(members)
    }

    /// Capture bridge hook event bytes immutably (CB-9B R4).
    ///
    /// Returns the capture digest. The bytes are stored in the pristine
    /// database, which only the Atomic library writes; a JSONL journal line
    /// is operation-linkage evidence only when this capture exists.
    pub fn capture_bridge_event(&self, event_bytes: &[u8]) -> Result<[u8; 32], RepositoryError> {
        use atomic_core::pristine::BridgeEventCaptureMutTxnT;
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let digest = txn
            .put_bridge_event_capture(event_bytes)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(digest)
    }

    /// Whether the exact event bytes have an immutable capture (CB-9B R4).
    pub fn bridge_event_captured(&self, event_bytes: &[u8]) -> Result<bool, RepositoryError> {
        use atomic_core::pristine::BridgeEventCaptureTxnT;
        let digest: [u8; 32] = blake3::hash(event_bytes).into();
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.get_bridge_event_capture(&digest)
            .map(|found| found.is_some())
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Capture bridge hook event bytes immutably AND anchor them to
    /// `operation` (review C2).
    ///
    /// RFC §5.4 grants the operation-linkage tier only to a post-rewrite
    /// event captured DURING AN ACTIVE CAPTURED OPERATION. The anchor is
    /// accepted only while `operation` exists in the journal and currently
    /// holds the active head of its scope, so an anchored capture is proof
    /// it was written while that operation was the live head. The advisory
    /// hook path (direct stdin, no active operation) must use
    /// [`Repository::capture_bridge_event`] instead: its captures carry no
    /// anchor and read as unauthenticated evidence downstream.
    pub fn capture_bridge_event_anchored(
        &self,
        event_bytes: &[u8],
        operation: atomic_core::types::OperationId,
    ) -> Result<[u8; 32], RepositoryError> {
        use atomic_core::pristine::BridgeEventCaptureMutTxnT;
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let digest = txn
            .put_anchored_bridge_event_capture(event_bytes, &operation)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(digest)
    }

    /// Whether the anchored capture for `event_bytes` genuinely binds the
    /// event to `operation` (review D3): the anchor row must name this
    /// operation AND the operation's GitRef effect leases must describe
    /// exactly the rewrite the event names AND the captured bytes must carry
    /// the exact capture token minted for this operation at preparation time
    /// (review E2). Anchors written without the exact-lease binding or
    /// without the operation's capture token fail here, so the
    /// authentication tier never trusts a bare anchor row and can never
    /// authenticate an advisory capture that predates the operation.
    pub fn bridge_anchor_binds_operation(
        &self,
        event_bytes: &[u8],
        operation: atomic_core::types::OperationId,
    ) -> Result<bool, RepositoryError> {
        use atomic_core::operation::{
            effect_lease_matches_oid, post_rewrite_event_capture_token, post_rewrite_event_pairs,
            EffectTarget, OperationKind,
        };
        use atomic_core::pristine::{BridgeEventCaptureTxnT, OperationTxnT};
        let digest: [u8; 32] = blake3::hash(event_bytes).into();
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let Some(anchor) = txn
            .get_bridge_event_capture_anchor(&digest)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            return Ok(false);
        };
        if anchor != *operation.as_bytes() {
            return Ok(false);
        }
        let Some(stored) = txn
            .get_operation(operation)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            return Ok(false);
        };
        // Review E2: the event bytes must carry the operation's minted
        // capture token — reader-side re-validation of the same binding the
        // write path enforced, so anchors written by any older or bypassing
        // writer never authenticate.
        let Some(event_token) = post_rewrite_event_capture_token(event_bytes) else {
            return Ok(false);
        };
        let Some(minted) = txn
            .get_bridge_ref_capture_token(operation.as_bytes())
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            return Ok(false);
        };
        let minted_hex = minted
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        if event_token != minted_hex {
            return Ok(false);
        }
        let Some(pairs) = post_rewrite_event_pairs(event_bytes) else {
            return Ok(false);
        };
        Ok(
            matches!(stored.payload().kind, OperationKind::ExportGitRefs)
                && pairs.iter().all(|(old_oid, new_oid)| {
                    stored.payload().delta.effects.iter().any(|effect| {
                        matches!(effect.target, EffectTarget::GitRef { .. })
                            && effect_lease_matches_oid(&effect.expected_old, old_oid)
                            && effect_lease_matches_oid(&effect.expected_new, new_oid)
                    })
                }),
        )
    }

    /// The operation id `event_bytes`' capture is anchored to, if any
    /// (review C2). Unanchored captures — everything the advisory hook path
    /// writes — are never operation linkage.
    pub fn bridge_event_capture_anchor(
        &self,
        event_bytes: &[u8],
    ) -> Result<Option<atomic_core::types::OperationId>, RepositoryError> {
        use atomic_core::pristine::BridgeEventCaptureTxnT;
        let digest: [u8; 32] = blake3::hash(event_bytes).into();
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.get_bridge_event_capture_anchor(&digest)
            .map(|found| found.map(atomic_core::types::OperationId::from_bytes))
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// The capture token minted for `operation`'s capture context, if any
    /// (review E2). An executor driving Git hooks inside the operation's
    /// capture context exports this token as `ATOMIC_BRIDGE_CAPTURE_TOKEN`;
    /// only captured events carrying it can anchor to the operation.
    pub fn bridge_ref_capture_token(
        &self,
        operation: atomic_core::types::OperationId,
    ) -> Result<Option<[u8; 32]>, RepositoryError> {
        use atomic_core::pristine::{BridgeEventCaptureTxnT, OperationTxnT};
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        if txn
            .get_operation(operation)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_none()
        {
            return Ok(None);
        }
        txn.get_bridge_ref_capture_token(operation.as_bytes())
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Whether `operation` exists in the journal with a verified effect
    /// receipt (review C2): the authentication tier requires the anchored
    /// operation to be genuinely verified, not merely journaled.
    pub fn operation_has_verified_receipt(
        &self,
        operation: atomic_core::types::OperationId,
    ) -> Result<bool, RepositoryError> {
        use atomic_core::pristine::OperationTxnT;
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        if txn
            .get_operation(operation)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_none()
        {
            return Ok(false);
        }
        let receipts = txn
            .get_effect_receipts(operation)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(super::operation::has_operation_verified_receipt(&receipts))
    }

    /// Find a locally stored, fully verified Git-state binding for `commit`.
    ///
    /// "Verified" means both the binding signature verifies and the bound
    /// Git content (commit, tree, ordered parents, closure root) matches the
    /// live Git object database. Bindings that fail either check are skipped,
    /// never trusted by their name, index position, or trailers.
    pub fn verified_binding_for_commit(
        &self,
        git: &git2::Repository,
        commit: &GitObjectId,
    ) -> Result<Option<crate::git_binding::GitStateBinding>, RepositoryError> {
        for id in self.binding_ids()? {
            let Ok(Some(binding)) = self.load_binding(&id) else {
                continue;
            };
            let payload = binding.payload();
            let Ok(bound_commit) = payload.git_commit.to_git_object_id() else {
                continue;
            };
            if &bound_commit != commit {
                continue;
            }
            if crate::git_binding::verify_binding_cryptography(&binding).is_err() {
                continue;
            }
            if crate::git_binding::verify_binding_content(git, &binding).is_err() {
                continue;
            }
            return Ok(Some(binding));
        }
        Ok(None)
    }

    /// Resurrect a verified binding's ordered change closure into `view`.
    ///
    /// Resurrection references the changes the binding already vouches for
    /// into the view in binding order; it never synthesizes new changes from
    /// Git and never rewrites the bound records. The full closure must be
    /// present in the local change store — anything missing fails closed
    /// (closure acquisition is CB-7 transport, not a synthesis decision).
    pub fn resurrect_binding_into_view(
        &self,
        binding: &crate::git_binding::GitStateBinding,
        view: &str,
    ) -> Result<ResurrectionOutcome, RepositoryError> {
        let ordered = binding.payload().ordered_changes.clone();
        let missing: Vec<String> = ordered
            .iter()
            .filter(|hash| !self.has_change(hash))
            .map(|hash| hash.to_base32())
            .collect();
        if !missing.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "binding {} names {} change(s) missing locally ({}); acquire the verified closure before resurrection",
                    binding.id(),
                    missing.len(),
                    missing.first().map(String::as_str).unwrap_or_default()
                ),
            });
        }
        let mut inserted = Vec::new();
        let mut already_present = Vec::new();
        for hash in &ordered {
            // Membership probe: insert_change_rec is idempotent, but the
            // explicit split keeps the outcome honest for callers and tests.
            let present = {
                let txn = self
                    .pristine
                    .read_txn()
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                let view_state = txn
                    .get_view(view)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::ViewNotFound {
                        name: view.to_string(),
                    })?;
                match txn.get_internal(hash) {
                    Ok(Some(change_id)) => txn
                        .get_change_seq(&view_state, change_id)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?
                        .is_some(),
                    _ => false,
                }
            };
            if present {
                already_present.push(*hash);
                continue;
            }
            self.insert_change_rec(hash, crate::InsertOptions::default().view(view))?;
            inserted.push(*hash);
        }
        Ok(ResurrectionOutcome {
            binding_id: binding.id(),
            inserted,
            already_present,
        })
    }

    /// Synthesize one root/single-parent Git commit through the normal
    /// recorded-file assembly and journal it as a `SynthesizeGit` operation.
    ///
    /// The change is assembled against the target view's visible graph
    /// (normal globalization: graph-context dependencies, semantic FileOps),
    /// classified with [`ChangeOrigin::GitSynthesized`], saved, registered,
    /// projected through the central operation-aware tree projection, and
    /// applied additively by the normal graph writers. The `SynthesizeGit`
    /// operation is prepared — under the common → working-copy lock order —
    /// before the pristine mutation and finalized with a verified receipt
    /// after it; any failure aborts through the immutable recovery path so no
    /// unverified view advance survives.
    ///
    /// Re-importing a change that is already a member of the target view is a
    /// pure no-op (`already_in_view = true`, no operation). A change that is
    /// already applied to the global graph but not yet referenced by the view
    /// is referenced without re-applying its hunks.
    ///
    /// `assembly_excluded` names changes that must stay invisible while this
    /// change is assembled (CB-9B merge legs: a sibling leg's changes are
    /// excluded so a leg anchors only to its own Git parent's interpreted
    /// closure and never inherits false sibling causality). An empty slice
    /// assembles against the full view visibility. `expectation` is verified
    /// against the staged graph projection before the transaction commits.
    /// `hashed_metadata` is hash-covered caller metadata (CB-9B raw foreign
    /// facts); empty when none.
    #[allow(clippy::too_many_arguments)]
    pub fn synthesize_git_change(
        &self,
        header: ChangeHeader,
        recorded_files: &[atomic_core::record::workflow::RecordedFile],
        origin: GitSynthesisOrigin,
        unhashed: serde_json::Value,
        hashed_metadata: Vec<u8>,
        deleted_paths: &[String],
        preserve_existing_tree_paths: bool,
        verified: &super::VerifiedProspectiveEquivalence,
        options: crate::InsertOptions,
        assembly_excluded: &[Hash],
        expectation: &StagedExpectation,
        git_commit_sha: Option<&str>,
        parent_interpretation_closure: &[Hash],
        supersede_excluded: bool,
    ) -> Result<SynthesisOutcome, RepositoryError> {
        let working_copy = self.require_working_copy_id()?;
        // Reentrant under an outer workspace transaction on the same thread;
        // a fresh, correctly ordered common → working-copy acquisition otherwise.
        let operation_lock = self.try_lock_operation(working_copy)?;
        self.synthesize_git_change_locked(
            &operation_lock,
            header,
            recorded_files,
            SynthesisClassification::Synthesized(origin),
            hashed_metadata,
            unhashed,
            deleted_paths,
            preserve_existing_tree_paths,
            verified,
            options,
            assembly_excluded,
            expectation,
            git_commit_sha,
            parent_interpretation_closure,
            supersede_excluded,
        )
    }

    /// Synthesize one Git merge resolution (CB-9B) through normal assembly
    /// and journal it as a `SynthesizeGit` operation.
    ///
    /// The resolution change is assembled from the union state (every parent
    /// closure was imported first, in deterministic bridge order) to the
    /// merge tree by the caller's recorded files; its dependencies remain
    /// context-derived from globalization. The hashed
    /// [`ChangeOrigin::GitResolution`] carries the tagged merge commit and
    /// the complete ordered parents, and the hashed `CausalFrontier` carries
    /// the closure roots of the union state. Apply-time verification
    /// (`validate_can_apply_with_frontier`) fails closed when any frontier
    /// root is missing or its dependency closure is incomplete.
    ///
    /// `assembly_excluded` and `expectation` follow
    /// [`Repository::synthesize_git_change`]: the resolution assembles
    /// against the exact parent union (unrelated same-run changes excluded)
    /// and is verified against the merge tree before publication.
    #[allow(clippy::too_many_arguments)]
    pub fn synthesize_git_resolution(
        &self,
        header: ChangeHeader,
        recorded_files: &[atomic_core::record::workflow::RecordedFile],
        origin: GitResolutionOrigin,
        unhashed: serde_json::Value,
        deleted_paths: &[String],
        preserve_existing_tree_paths: bool,
        verified: &super::VerifiedProspectiveEquivalence,
        options: crate::InsertOptions,
        assembly_excluded: &[Hash],
        expectation: &StagedExpectation,
        git_commit_sha: Option<&str>,
        parent_interpretation_closure: &[Hash],
    ) -> Result<SynthesisOutcome, RepositoryError> {
        let working_copy = self.require_working_copy_id()?;
        let operation_lock = self.try_lock_operation(working_copy)?;
        self.synthesize_git_change_locked(
            &operation_lock,
            header,
            recorded_files,
            SynthesisClassification::Resolution(origin),
            Vec::new(),
            unhashed,
            deleted_paths,
            preserve_existing_tree_paths,
            verified,
            options,
            assembly_excluded,
            expectation,
            git_commit_sha,
            parent_interpretation_closure,
            // A resolution assembles the union of every parent closure (review
            // R1): its assembly exclusions are sibling lineages that the union
            // legitimately contains, so they are never superseded from the
            // view — `ensure_union_state` re-references them first.
            false,
        )
    }

    /// Synthesize one empty Git commit as a distinct `Change::empty` object
    /// (CB-9B) and journal it as a `SynthesizeGit` operation.
    ///
    /// The change emits no graph facts. `hashed_metadata` carries the raw
    /// foreign facts (tagged tree OID, author/committer identities and
    /// times) in the hashed portion, and the validated origin carries the
    /// tagged commit OID, ordered parents, and `Derivation::EmptyCommit`.
    #[allow(clippy::too_many_arguments)]
    pub fn synthesize_empty_git_change(
        &self,
        header: ChangeHeader,
        origin: GitSynthesisOrigin,
        hashed_metadata: Vec<u8>,
        unhashed: serde_json::Value,
        preserve_existing_tree_paths: bool,
        verified: &super::VerifiedProspectiveEquivalence,
        options: crate::InsertOptions,
        git_commit_sha: Option<&str>,
        parent_interpretation_closure: &[Hash],
        expectation: &StagedExpectation,
        assembly_excluded: &[Hash],
        supersede_excluded: bool,
    ) -> Result<SynthesisOutcome, RepositoryError> {
        let working_copy = self.require_working_copy_id()?;
        let operation_lock = self.try_lock_operation(working_copy)?;
        self.synthesize_git_change_locked(
            &operation_lock,
            header,
            &[],
            SynthesisClassification::Synthesized(origin),
            hashed_metadata,
            unhashed,
            &[],
            preserve_existing_tree_paths,
            verified,
            options,
            assembly_excluded,
            expectation,
            git_commit_sha,
            parent_interpretation_closure,
            supersede_excluded,
        )
    }

    /// Journal and execute one synthesis under an explicit workspace
    /// transaction's ordered operation lock.
    #[allow(clippy::too_many_arguments)]
    pub fn synthesize_git_change_under_workspace(
        &self,
        workspace: &WorkspaceTxn,
        header: ChangeHeader,
        recorded_files: &[atomic_core::record::workflow::RecordedFile],
        origin: GitSynthesisOrigin,
        unhashed: serde_json::Value,
        deleted_paths: &[String],
        preserve_existing_tree_paths: bool,
        verified: &super::VerifiedProspectiveEquivalence,
        options: crate::InsertOptions,
        git_commit_sha: Option<&str>,
        parent_interpretation_closure: &[Hash],
        expectation: &StagedExpectation,
        assembly_excluded: &[Hash],
        supersede_excluded: bool,
    ) -> Result<SynthesisOutcome, RepositoryError> {
        self.synthesize_git_change_locked(
            workspace.operation_lock(),
            header,
            recorded_files,
            SynthesisClassification::Synthesized(origin),
            Vec::new(),
            unhashed,
            deleted_paths,
            preserve_existing_tree_paths,
            verified,
            options,
            assembly_excluded,
            expectation,
            git_commit_sha,
            parent_interpretation_closure,
            supersede_excluded,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn synthesize_git_change_locked(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        header: ChangeHeader,
        recorded_files: &[atomic_core::record::workflow::RecordedFile],
        classification: SynthesisClassification,
        hashed_metadata: Vec<u8>,
        unhashed: serde_json::Value,
        deleted_paths: &[String],
        preserve_existing_tree_paths: bool,
        _verified: &super::VerifiedProspectiveEquivalence,
        options: crate::InsertOptions,
        assembly_excluded: &[Hash],
        expectation: &StagedExpectation,
        git_commit_sha: Option<&str>,
        parent_interpretation_closure: &[Hash],
        supersede_excluded: bool,
    ) -> Result<SynthesisOutcome, RepositoryError> {
        use atomic_core::pristine::ViewTxnT;
        use atomic_core::record::workflow::assemble_change;
        use atomic_core::record::workflow::assembly::AssemblyOptions;

        let view_name = options.view.as_deref().unwrap_or(&self.current_view);
        self.pristine
            .require_repository_capability(super::CHANGE_FORMAT_VNEXT_CAPABILITY)
            .map_err(RepositoryError::from)?;

        // 1. Normal assembly against the visible graph (read stage).
        //
        // The assembly visibility is the view's validated closure minus
        // `assembly_excluded` (CB-9B merge legs exclude their sibling's
        // changes so each leg anchors only within its own Git parent's
        // interpreted closure). Dependency metadata is validated during the
        // closure build: a missing root or incomplete index fails closed
        // before any bytes are assembled.
        let assemble_start = std::time::Instant::now();
        let (final_change, v3_bytes, hash, view_state, change_count) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let view = txn
                .get_view(view_name)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view_name.to_string(),
                })?;
            let assembled = if recorded_files.is_empty() {
                Change::empty(header.clone())
            } else {
                let visibility =
                    super::filter::assembly_visibility_excluding(&txn, &view, assembly_excluded)?;
                let view_graph = atomic_core::pristine::ViewGraph::new(&txn, visibility);
                // CB-9C: imported trees legitimately contain empty tracked
                // files (`.gitkeep`, lockfile placeholders). Their FileAdd
                // must still be emitted with no content span, or the staged
                // full-tree check would refuse the commit for a missing
                // path — so globalization includes empty files here.
                // CB-9C: the imported change must carry the full tracked
                // content — the 500 MB opaque corpus exceeds the default
                // 100 MB assembly cap, and refusing a tracked blob is the
                // silent-loss class the corpus forbids. The budget scales
                // with the total imported bytes (2x headroom, floored at
                // the default).
                let content_budget = recorded_files
                    .iter()
                    .map(|file| file.content_len())
                    .sum::<usize>()
                    .saturating_mul(2)
                    .max(AssemblyOptions::DEFAULT_MAX_CONTENT_SIZE);
                match assemble_change(
                    &view_graph,
                    recorded_files,
                    header.clone(),
                    &AssemblyOptions::new()
                        .max_content_size(content_budget)
                        .include_empty_files(true),
                ) {
                    Ok(result) => {
                        let change = result.into_change();
                        if std::env::var_os("ATOMIC_TRACE_GIT_IMPORT").is_some() {
                            eprintln!(
                                "[git-import] assembled {}: hunks={} contents={} file_paths={:?}",
                                change.hashed.header.message.lines().next().unwrap_or(""),
                                change.hunks().len(),
                                change.contents.len(),
                                change
                                    .file_ops()
                                    .iter()
                                    .map(|o| o.path())
                                    .collect::<Vec<_>>()
                            );
                            for (idx, op) in change.hunks().iter().enumerate() {
                                match &op {
                                    atomic_core::change::GraphOp::Replacement {
                                        change: edge_update,
                                        replacement,
                                        ..
                                    } => {
                                        eprintln!(
                                            "    hunk {idx}: Replacement insert_range=({},{}) preds={:?} succs={:?} del_edges={}",
                                            replacement.start.get(),
                                            replacement.end.get(),
                                            replacement.predecessors,
                                            replacement.successors,
                                            edge_update.edges.len()
                                        );
                                        for e in &edge_update.edges {
                                            eprintln!(
                                                "      del edge from=({:?},{}) to=({},{},{}) prev={:?} new={:?}",
                                                e.from.change,
                                                e.from.pos.get(),
                                                e.to.change.as_ref().map(Base32::to_base32).unwrap_or_default(),
                                                e.to.start.get(),
                                                e.to.end.get(),
                                                e.previous,
                                                e.flag,
                                            );
                                        }
                                    }
                                    other => eprintln!("    hunk {idx}: other={other:?}"),
                                }
                            }
                        }
                        change
                    }
                    Err(e) => {
                        let err_msg = e.to_string();
                        if err_msg.contains("empty") || err_msg.contains("AllEmpty") {
                            Change::empty(header)
                        } else {
                            return Err(RepositoryError::Apply(e.to_string()));
                        }
                    }
                }
            };
            let (final_change, v3_bytes, hash) =
                finalize_classified_change(assembled, &classification, hashed_metadata, unhashed)?;
            // Fail fast on deleted-path leases before journaling anything.
            super::insert::validate_import_deleted_paths(&final_change, deleted_paths)?;
            (final_change, v3_bytes, hash, view.state, view.change_count)
        };
        let timings = super::ImportWriteTimings {
            assemble_ms: assemble_start.elapsed().as_millis(),
            ..super::ImportWriteTimings::default()
        };

        // 2. Idempotency: a change already referenced by the view is a no-op;
        // one applied to the global graph but absent from the view is
        // referenced without re-applying hunks.
        let existing_id = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.get_internal(&hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
        };
        if let Some(change_id) = existing_id {
            let (in_view, applied_to_graph) = {
                let txn = self
                    .pristine
                    .read_txn()
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                let view = txn
                    .get_view(view_name)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::ViewNotFound {
                        name: view_name.to_string(),
                    })?;
                let in_view = txn
                    .get_change_seq(&view, change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .is_some();
                let applied = txn
                    .has_change_in_graph(change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                (in_view, applied)
            };
            if in_view {
                // Review C4: an already-known hash cannot bypass the
                // supersede alignment. When this synthesis would supersede
                // assembly exclusions, the published effective view must not
                // still contain them — including through ancestors. A
                // survivor means the boundary cannot be aligned without
                // touching ancestor/unrelated work: refuse instead of
                // reporting a clean already-in-view outcome.
                if supersede_excluded && !assembly_excluded.is_empty() {
                    let txn = self
                        .pristine
                        .read_txn()
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    let survivors =
                        self.surviving_exclusions(&txn, view_name, assembly_excluded)?;
                    if !survivors.is_empty() {
                        let named = survivors
                            .iter()
                            .map(|(view, hash)| format!("{} in view '{}'", hash.to_base32(), view))
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "superseded assembly exclusion(s) still survive in ancestor \
                                 views of '{view_name}' while change {} is already a member: \
                                 {named}; the published effective view would still contain \
                                 the excluded history, so the synthesis refuses",
                                hash.to_base32()
                            ),
                        });
                    }
                }
                // Review D1: an already-present hash must still verify the
                // caller's staged expectation against the actual effective
                // projection. The historical fast path returned
                // `already_in_view` without checking the expectation, so a
                // re-synthesis carrying deliberately wrong expected bytes
                // read as a clean no-op. The verification opens a throwaway
                // write transaction under the ordered operation lock and
                // never commits it: the committed view is the thing being
                // proven, and the checked projection equals the effective
                // post-alignment boundary exactly as on the reference path.
                if expectation.tree.is_some() || !expectation.paths.is_empty() {
                    let stored = self
                        .load_change(&hash)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    let txn = operation_lock.begin_write_immediate()?;
                    let verification = self.verify_staged_projection(
                        &txn,
                        view_name,
                        &stored,
                        assembly_excluded,
                        expectation,
                        &hash,
                        supersede_excluded,
                    );
                    // The transaction is dropped uncommitted: this is a
                    // read-side proof, not a mutation stage.
                    drop(txn);
                    verification.map_err(|error| RepositoryError::InvalidOperation {
                        message: format!(
                            "already-present change {} does not render the supplied \
                             expectation: {error}",
                            hash.to_base32()
                        ),
                    })?;
                }
                return Ok(SynthesisOutcome {
                    write: ImportWriteOutcome {
                        hash,
                        timings,
                        insert: crate::InsertOutcome::new(
                            view_state,
                            change_count,
                            false,
                            crate::InsertStats::new(),
                        ),
                    },
                    operation: None,
                    already_in_view: true,
                });
            }
            if applied_to_graph {
                return self.reference_registered_change_locked(
                    operation_lock,
                    view_name,
                    &hash,
                    change_id,
                    timings,
                    assembly_excluded,
                    expectation,
                    supersede_excluded,
                );
            }
        }

        // 3. Journal the SynthesizeGit intent before any visible effect.
        //
        // CB-9B review B4: when the caller supersedes its assembly
        // exclusions (single-parent rewrite synthesis), the excluded
        // Git-origin members that live in THIS view leave the view in the
        // same journal operation that adds the new change, so the published
        // boundary equals the commit's exact interpreted closure. Members
        // living in ancestor views are never removed: ancestor views are
        // shared perspectives whose membership other work depends on
        // (review B4: preserve other views/history).
        let working_copy = operation_lock.working_copy();
        let (before_state, after_state, sequence, removal_transitions) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let view = txn
                .get_view(view_name)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view_name.to_string(),
                })?;
            let mut excluded_ids: std::collections::HashSet<atomic_core::types::NodeId> =
                std::collections::HashSet::new();
            if supersede_excluded {
                for excluded in assembly_excluded {
                    let id = txn
                        .get_internal(excluded)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?
                        .ok_or_else(|| RepositoryError::InvalidOperation {
                            message: format!(
                                "superseded assembly exclusion names change {} which is not registered locally",
                                excluded.to_base32()
                            ),
                        })?;
                    excluded_ids.insert(id);
                }
            }
            let mut removal_transitions: Vec<MetadataTransition> = Vec::new();
            let mut after_view_state = Merkle::ZERO;
            for row in txn
                .iter_changes(&view, 0)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                let (seq, change_id, _) =
                    row.map_err(|e| RepositoryError::Database(e.to_string()))?;
                let member_hash = txn
                    .get_external(change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or(RepositoryError::ChangeNotFound {
                        hash: change_id.to_string(),
                    })?;
                if excluded_ids.contains(&change_id) {
                    removal_transitions.push(MetadataTransition {
                        target: MetadataTarget::ViewChange {
                            view: view_name.to_string(),
                            change: member_hash,
                        },
                        expected_old: MetadataValue::Sequence(seq),
                        expected_new: MetadataValue::Absent,
                    });
                    continue;
                }
                after_view_state = after_view_state.next(&member_hash);
            }
            // The new change is appended last, after the surviving members.
            let after_view_state = after_view_state.next(&hash);
            let before_record = self.working_copy_record(working_copy)?;
            let mut after_record = before_record.clone();
            if before_record.desired_view == view.id {
                after_record.desired_state = after_view_state;
            }
            (
                RepoStateRef {
                    view: Some(ViewStateRef {
                        name: view_name.to_string(),
                        state: view.state,
                        set_id: None,
                    }),
                    working_copy: Some(working_copy_state_ref(before_record)),
                    git: None,
                },
                RepoStateRef {
                    view: Some(ViewStateRef {
                        name: view_name.to_string(),
                        state: after_view_state,
                        set_id: None,
                    }),
                    working_copy: Some(working_copy_state_ref(after_record)),
                    git: None,
                },
                view.change_count,
                removal_transitions,
            )
        };
        // The add lease's sequence is the change's FINAL position after the
        // superseded exclusions compact the log (review B4): the removals
        // shift every later entry down, so the new change lands at
        // count − removals, not at the pre-put count.
        let final_sequence = sequence - removal_transitions.len() as u64;
        let mut transitions = vec![MetadataTransition {
            target: MetadataTarget::ViewChange {
                view: view_name.to_string(),
                change: hash,
            },
            expected_old: MetadataValue::Absent,
            expected_new: MetadataValue::Sequence(final_sequence),
        }];
        transitions.extend(removal_transitions);
        let operation = self.prepare_metadata_operation(
            operation_lock,
            OperationKind::SynthesizeGit,
            None,
            before_state,
            after_state,
            transitions,
            vec![hash],
            ActorRef::System {
                name: "git-synthesis".to_string(),
            },
            current_operation_timestamp_ms(),
        )?;

        // 4. Apply inside one pristine write transaction. The staged
        // projection is verified against `expectation` inside the same
        // transaction before it commits, so a wrong or partially applied
        // change never becomes a visible, verified view member.
        let apply_result = self.apply_synthesized_change_locked(
            operation_lock.begin_write_immediate()?,
            view_name,
            &hash,
            &v3_bytes,
            &final_change,
            deleted_paths,
            preserve_existing_tree_paths,
            &options,
            assembly_excluded,
            expectation,
            git_commit_sha,
            parent_interpretation_closure,
            supersede_excluded,
        );

        match apply_result {
            Ok(write) => {
                // The graph write already advanced VIEW_CHANGES, so the
                // metadata lease classifies as already applied.
                self.apply_operation_metadata_locked(operation_lock, operation.id())?;
                self.finalize_operation_verified(operation_lock, operation.id())?;
                Ok(SynthesisOutcome {
                    write,
                    operation: Some(operation.id()),
                    already_in_view: false,
                })
            }
            Err(error) => {
                // Immutable recovery: invert the prepared operation so no
                // unverified view/checkpoint advance survives.
                self.abort_prepared_metadata_operation(operation_lock, &operation)?;
                Err(error)
            }
        }
    }

    /// Reference an already-applied change into a view (no hunk re-apply),
    /// journaled with the same SynthesizeGit lease contract as the fresh
    /// apply.
    ///
    /// Review C4: the reference path carries the same supersede alignment and
    /// pre-publication verification duties as the apply path — excluded
    /// target-local members leave the view inside the same transaction,
    /// ancestor survivors refuse before commit, and the staged proof runs
    /// against the actual post-alignment effective view closure.
    ///
    /// Review D1: the prepared operation carries the exact lease set the
    /// fresh-apply path prepares — every target-local exclusion becomes a
    /// removal lease captured before the transaction, the after-state folds
    /// the surviving members and the new hash (the `del_change` compaction
    /// contract), and the add lease's expected sequence is the FINAL compacted
    /// position. Stale leases (prepared from the pre-removal count) committed
    /// membership the lease verification then rejected after the commit,
    /// leaving a Prepared head that blocked ordinary reopen.
    #[allow(clippy::too_many_arguments)]
    fn reference_registered_change_locked(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        view_name: &str,
        hash: &Hash,
        change_id: atomic_core::types::NodeId,
        mut timings: super::ImportWriteTimings,
        assembly_excluded: &[Hash],
        expectation: &StagedExpectation,
        supersede_excluded: bool,
    ) -> Result<SynthesisOutcome, RepositoryError> {
        use atomic_core::apply::compute_new_state;

        let working_copy = operation_lock.working_copy();
        // Review D1: the reference path prepares the SAME complete lease set
        // as the fresh-apply path. Before the operation exists, the exact
        // before/after membership is computed from the live view: every
        // superseded exclusion that lives in THIS view becomes a removal
        // lease, the after-state folds the surviving members in sequence
        // order and then the new hash (the core `del_change` compaction
        // contract), and the add lease's expected sequence is the change's
        // FINAL position after the compaction — never the pre-removal count.
        // Preparing stale leases here committed membership that the lease
        // verification then rejected outside the transaction, leaving a
        // Prepared head that blocked ordinary reopen (review D1).
        let (before_state, after_state, final_sequence, removal_transitions) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let view = txn
                .get_view(view_name)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view_name.to_string(),
                })?;
            let mut excluded_ids: std::collections::HashSet<atomic_core::types::NodeId> =
                std::collections::HashSet::new();
            if supersede_excluded {
                for excluded in assembly_excluded {
                    let id = txn
                        .get_internal(excluded)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?
                        .ok_or_else(|| RepositoryError::InvalidOperation {
                            message: format!(
                                "superseded assembly exclusion names change {} which is not registered locally",
                                excluded.to_base32()
                            ),
                        })?;
                    excluded_ids.insert(id);
                }
            }
            let mut removal_transitions: Vec<MetadataTransition> = Vec::new();
            let mut after_view_state = Merkle::ZERO;
            for row in txn
                .iter_changes(&view, 0)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                let (seq, change_id, _) =
                    row.map_err(|e| RepositoryError::Database(e.to_string()))?;
                let member_hash = txn
                    .get_external(change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or(RepositoryError::ChangeNotFound {
                        hash: change_id.to_string(),
                    })?;
                if excluded_ids.contains(&change_id) {
                    removal_transitions.push(MetadataTransition {
                        target: MetadataTarget::ViewChange {
                            view: view_name.to_string(),
                            change: member_hash,
                        },
                        expected_old: MetadataValue::Sequence(seq),
                        expected_new: MetadataValue::Absent,
                    });
                    continue;
                }
                after_view_state = after_view_state.next(&member_hash);
            }
            // The new change is appended last, after the surviving members.
            let after_view_state = after_view_state.next(hash);
            let before_record = self.working_copy_record(working_copy)?;
            let mut after_record = before_record.clone();
            if before_record.desired_view == view.id {
                after_record.desired_state = after_view_state;
            }
            (
                RepoStateRef {
                    view: Some(ViewStateRef {
                        name: view_name.to_string(),
                        state: view.state,
                        set_id: None,
                    }),
                    working_copy: Some(working_copy_state_ref(before_record)),
                    git: None,
                },
                RepoStateRef {
                    view: Some(ViewStateRef {
                        name: view_name.to_string(),
                        state: after_view_state,
                        set_id: None,
                    }),
                    working_copy: Some(working_copy_state_ref(after_record)),
                    git: None,
                },
                // The add lease's sequence is the change's FINAL position
                // after the superseded exclusions compact the log (review
                // B4/D1): the removals shift every later entry down, so the
                // new change lands at count − removals.
                view.change_count - removal_transitions.len() as u64,
                removal_transitions,
            )
        };
        let mut transitions = vec![MetadataTransition {
            target: MetadataTarget::ViewChange {
                view: view_name.to_string(),
                change: *hash,
            },
            expected_old: MetadataValue::Absent,
            expected_new: MetadataValue::Sequence(final_sequence),
        }];
        transitions.extend(removal_transitions);
        let operation = self.prepare_metadata_operation(
            operation_lock,
            OperationKind::SynthesizeGit,
            None,
            before_state,
            after_state,
            transitions,
            vec![*hash],
            ActorRef::System {
                name: "git-synthesis".to_string(),
            },
            current_operation_timestamp_ms(),
        )?;

        let apply_start = std::time::Instant::now();
        let apply_result = (|| -> Result<ImportWriteOutcome, RepositoryError> {
            let mut txn = operation_lock.begin_write_immediate()?;
            // Review C4: supersede target-local exclusions and refuse
            // ancestor survivors BEFORE the reference is published, mirroring
            // the apply path's alignment contract.
            if supersede_excluded {
                self.supersede_excluded_members_locked(&mut txn, view_name, assembly_excluded)?;
                let survivors = self.surviving_exclusions(&*txn, view_name, assembly_excluded)?;
                if !survivors.is_empty() {
                    let named = survivors
                        .iter()
                        .map(|(view, hash)| format!("{} in view '{}'", hash.to_base32(), view))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "superseded assembly exclusion(s) still survive in ancestor views \
                             of '{view_name}' after target-local alignment: {named}; the \
                             published effective view would still contain the excluded \
                             history, so the reference refuses instead of hiding it",
                        ),
                    });
                }
            }
            let mut view = txn
                .open_or_create_view(view_name)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            // `put_change` advances the Merkle and sequence itself; pin the
            // precomputed single-advance values after the put.
            let new_state = compute_new_state(&view.state, hash);
            let sequence = view.change_count + 1;
            txn.put_change(&mut view, change_id, hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            view.state = new_state;
            view.change_count = sequence;
            txn.update_view(&view)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            // Review C4: the referenced change must render its commit tree
            // through the post-alignment view before the transaction commits.
            self.verify_staged_projection(
                &txn,
                view_name,
                &self
                    .load_change(hash)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?,
                assembly_excluded,
                expectation,
                hash,
                supersede_excluded,
            )?;
            let outcome = ImportWriteOutcome {
                hash: *hash,
                timings: super::ImportWriteTimings::default(),
                insert: crate::InsertOutcome::new(
                    view.state,
                    view.change_count,
                    false,
                    crate::InsertStats::new(),
                ),
            };
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            Ok(outcome)
        })();
        match apply_result {
            Ok(outcome) => {
                timings.apply_ms = apply_start.elapsed().as_millis();
                let mut outcome = outcome;
                outcome.timings = timings;
                self.apply_operation_metadata_locked(operation_lock, operation.id())?;
                self.finalize_operation_verified(operation_lock, operation.id())?;
                Ok(SynthesisOutcome {
                    write: outcome,
                    operation: Some(operation.id()),
                    already_in_view: false,
                })
            }
            Err(error) => {
                self.abort_prepared_metadata_operation(operation_lock, &operation)?;
                Err(error)
            }
        }
    }

    /// The pristine mutation stage of one synthesis: save, register, project,
    /// and apply the change additively inside one write transaction, then
    /// verify the staged projection against `expectation` before commit.
    #[allow(clippy::too_many_arguments)]
    fn apply_synthesized_change_locked(
        &self,
        mut txn: super::locks::OrderedPristineWriteTxn<'_>,
        view_name: &str,
        hash: &Hash,
        v3_bytes: &[u8],
        final_change: &Change,
        deleted_paths: &[String],
        preserve_existing_tree_paths: bool,
        options: &crate::InsertOptions,
        assembly_excluded: &[Hash],
        expectation: &StagedExpectation,
        git_commit_sha: Option<&str>,
        parent_interpretation_closure: &[Hash],
        supersede_excluded: bool,
    ) -> Result<ImportWriteOutcome, RepositoryError> {
        use crate::apply::write_change_to_graph;
        use atomic_core::apply::compute_new_state;
        use atomic_core::pristine::GraphTxnT;

        // Deterministic test fault: fails the mutation stage after the
        // SynthesizeGit intent was journaled, exercising the recovery path.
        #[cfg(test)]
        if super::synthesis::SYNTHESIS_APPLY_FAULT.with(std::cell::Cell::get) {
            return Err(RepositoryError::InvalidOperation {
                message: "injected synthesis apply fault (test)".to_string(),
            });
        }

        validate_import_deleted_paths(final_change, deleted_paths)?;
        let mut timings = super::ImportWriteTimings::default();

        let save_start = std::time::Instant::now();
        // Failpoint: record (change-store) failure before the graph write.
        #[cfg(feature = "adoption-test-injection")]
        failpoints::record()?;
        self.save_change_bytes_after_capability_declaration(hash, v3_bytes, final_change)?;
        timings.save_ms = save_start.elapsed().as_millis();

        let change_id = txn
            .register_change(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.put_change_deps(change_id, final_change.dependencies())
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Review ::26 R4 oracle: genuine process death at the named
        // durability checkpoint (change bytes saved + registered, graph NOT
        // applied, redb transaction uncommitted).
        #[cfg(feature = "adoption-test-injection")]
        failpoints::kill_at_change_boundary();

        // Persist the commit's complete interpreted closure in the same
        // transaction that publishes the change (review R1): descendants —
        // including ones imported in later runs against the persisted row —
        // reconstruct the exact parent visibility from it. The row is
        // refused (fail closed) when a different closure already exists.
        if let Some(sha) = git_commit_sha {
            use atomic_core::pristine::GitCommitClosureMutTxnT;
            let mut closure: Vec<Hash> = parent_interpretation_closure.to_vec();
            if !closure.contains(hash) {
                closure.push(*hash);
            }
            closure.sort();
            closure.dedup();
            txn.put_git_commit_closure(sha, &closure)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // The change's hunks are applied only when its vertices are not yet
        // in the global graph. A registered-but-unapplied change (interrupted
        // import, draft reference) is applied normally; anything already in
        // the graph is reference-only and was routed before this stage.
        let already_applied = txn
            .has_change_in_graph(change_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let tree_projection = self.plan_tree_projection(
            &mut txn,
            change_id,
            *hash,
            final_change,
            deleted_paths,
            preserve_existing_tree_paths,
        )?;
        tree_projection.apply_prerequisites(&mut *txn)?;

        let apply_start = std::time::Instant::now();
        let insert = if already_applied {
            // Reference-only: the change's edges are already in the global
            // graph; advance the view membership without re-applying hunks.
            // `put_change` advances the Merkle and sequence itself, so the
            // new state is precomputed and pinned after the put (mirroring
            // the reference writer in `write_import_graph_change`).
            let mut view = txn
                .open_or_create_view(view_name)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let new_state = compute_new_state(&view.state, hash);
            let sequence = view.change_count + 1;
            txn.put_change(&mut view, change_id, hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            view.state = new_state;
            view.change_count = sequence;
            txn.update_view(&view)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            crate::InsertOutcome::new(
                view.state,
                view.change_count,
                false,
                crate::InsertStats::new(),
            )
        } else {
            write_change_to_graph(
                &mut txn,
                view_name,
                change_id,
                hash,
                final_change,
                options,
                false,
            )
            .map_err(|e| RepositoryError::Apply(e.to_string()))?
        };
        timings.apply_ms = apply_start.elapsed().as_millis();

        let commit_start = std::time::Instant::now();
        // Failpoint: tree-projection failure after the graph write.
        #[cfg(feature = "adoption-test-injection")]
        failpoints::projection()?;

        // Injected semantic corruption between the graph write and staged
        // verification (review F4, test injection): the corrupted CRDT
        // state must be refused before publication.
        #[cfg(feature = "adoption-test-injection")]
        {
            let semantic_paths = expectation
                .tree
                .as_ref()
                .map(|tree| tree.semantic_paths.clone())
                .unwrap_or_default();
            failpoints::semantic_missing_trunk(&mut *txn, &semantic_paths)?;
            failpoints::semantic_corrupt_vertex(&mut *txn, &semantic_paths)?;
            let untouched_paths = expectation
                .tree
                .as_ref()
                .map(|tree| {
                    tree.entries
                        .keys()
                        .filter(|path| !semantic_paths.contains(*path))
                        .cloned()
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();
            failpoints::semantic_corrupt_untouched(&mut *txn, &untouched_paths)?;
            // Review C1 inherited-semantic corruption injections: flipped
            // token payload bounds, flipped token kinds, foreign token
            // ownership, and an unattributed trunk tombstone on paths this
            // commit did not touch must all be refused before publication.
            failpoints::semantic_corrupt_leaf_range(&mut *txn, &untouched_paths)?;
            failpoints::semantic_corrupt_leaf_kind(&mut *txn, &untouched_paths)?;
            failpoints::semantic_corrupt_leaf_owner(&mut *txn, &untouched_paths)?;
            failpoints::semantic_corrupt_trunk_deleted(&mut *txn, &untouched_paths)?;
        }
        self.apply_tree_projection(
            &mut txn,
            &tree_projection,
            view_name,
            preserve_existing_tree_paths,
        )?;
        let projection_done = commit_start.elapsed();

        // CB-9B review B4 + C4: align the published target to the exact
        // intended closure BEFORE verification. Every superseded assembly
        // exclusion leaves this view inside the same transaction, so the
        // staged full-tree check below proves the post-publication view
        // state — not just the isolated interpretation — and a failing check
        // aborts before commit, so no misaligned boundary or failing
        // successor is ever published.
        if supersede_excluded {
            self.supersede_excluded_members_locked(&mut txn, view_name, assembly_excluded)?;
            // Review C4: the alignment removes target-local members only.
            // An exclusion that also lives in an ANCESTOR view survives the
            // removal and would remain visible in the published effective
            // view while a filtered check hid it. Refuse before commit:
            // ancestor views are shared perspectives and are never
            // flattened or deleted to make alignment pass.
            let survivors = self.surviving_exclusions(&*txn, view_name, assembly_excluded)?;
            if !survivors.is_empty() {
                let named = survivors
                    .iter()
                    .map(|(view, hash)| format!("{} in view '{}'", hash.to_base32(), view))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "superseded assembly exclusion(s) still survive in ancestor views of \
                         '{view_name}' after target-local alignment: {named}; the published \
                         effective view would still contain the excluded history, so the \
                         synthesis refuses instead of hiding it",
                    ),
                });
            }
        }

        // Fail-closed verification of the staged state, still inside the
        // same pristine write transaction: an error aborts before commit, so
        // no failing state or successor can become visible or verified.
        // The supersede path verifies the ACTUAL post-alignment effective
        // view closure — no exclusion subtraction — so the proof covers
        // exactly what publication will show (review C4).
        self.verify_staged_projection(
            &txn,
            view_name,
            final_change,
            assembly_excluded,
            expectation,
            hash,
            supersede_excluded,
        )
        .map_err(|error| RepositoryError::InvalidOperation {
            message: format!(
                "staged synthesis verification failed for {}: {error}",
                hash.to_base32()
            ),
        })?;
        let verify_done = commit_start.elapsed();

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        timings.commit_ms = commit_start.elapsed().as_millis();
        if std::env::var_os("ATOMIC_TRACE_GIT_IMPORT").is_some() {
            eprintln!(
                "[git-import] write phases {}: projection={}ms verify={}ms commit_disk={}ms",
                hash.to_base32(),
                projection_done.as_millis(),
                (verify_done - projection_done).as_millis(),
                (commit_start.elapsed() - verify_done).as_millis(),
            );
        }

        Ok(ImportWriteOutcome {
            hash: *hash,
            timings,
            insert,
        })
    }

    /// Which superseded exclusions still survive in the post-alignment
    /// effective membership of `view` (review C4): the target view's full
    /// ancestor chain is scanned AFTER the target-local removals, because a
    /// removal from this view cannot touch ancestor views. Every survivor
    /// means the published effective view would still contain the excluded
    /// history, so the caller must refuse before commit instead of hiding
    /// the survivor behind an assembly filter.
    fn surviving_exclusions<T>(
        &self,
        txn: &T,
        view_name: &str,
        assembly_excluded: &[Hash],
    ) -> Result<Vec<(String, Hash)>, RepositoryError>
    where
        T: atomic_core::pristine::ViewTxnT + atomic_core::pristine::GraphTxnT,
    {
        if assembly_excluded.is_empty() {
            return Ok(Vec::new());
        }
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let excluded_ids: std::collections::HashSet<atomic_core::types::NodeId> = assembly_excluded
            .iter()
            .map(|hash| {
                txn.get_internal(hash)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::InvalidOperation {
                        message: format!(
                            "superseded assembly exclusion names change {} which is not registered locally",
                            hash.to_base32()
                        ),
                    })
            })
            .collect::<Result<Vec<_>, RepositoryError>>()?
            .into_iter()
            .collect();
        let mut survivors = Vec::new();
        for member_view in txn
            .resolve_full_view_chain(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            for entry in txn
                .iter_changes(&member_view, 0)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                let (_seq, change_id, _) =
                    entry.map_err(|e| RepositoryError::Database(e.to_string()))?;
                if !excluded_ids.contains(&change_id) {
                    continue;
                }
                let hash = txn
                    .get_external(change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or(RepositoryError::ChangeNotFound {
                        hash: change_id.to_string(),
                    })?;
                survivors.push((member_view.name.clone(), hash));
            }
        }
        Ok(survivors)
    }

    /// Remove every superseded assembly exclusion from `view_name` inside the
    /// still-open write transaction (CB-9B review B4).
    ///
    /// Each excluded hash names a Git-origin view member outside the imported
    /// commit's interpreted closure — history the rewrite replaced. The
    /// removal compacts the view log and recomputes the Merkle state (the
    /// core `del_change` contract), so the view's boundary equals the exact
    /// intended closure. A hash that is already absent is a no-op (the
    /// alignment already holds); a hash that is registered but lives only in
    /// an ancestor view is left alone — ancestor views are shared
    /// perspectives whose membership other work depends on (review B4:
    /// preserve other views/history).
    fn supersede_excluded_members_locked(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        view_name: &str,
        assembly_excluded: &[Hash],
    ) -> Result<(), RepositoryError> {
        use atomic_core::pristine::MutTxnT;
        if assembly_excluded.is_empty() {
            return Ok(());
        }
        let mut view = txn
            .open_or_create_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let mut removed_any = false;
        for hash in assembly_excluded {
            let change_id = txn
                .get_internal(hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "superseded assembly exclusion names change {} which is not registered locally",
                        hash.to_base32()
                    ),
                })?;
            if txn
                .del_change(&mut view, change_id, hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .is_some()
            {
                removed_any = true;
            }
        }
        txn.update_view(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        if removed_any {
            self.realign_tree_projection_in_txn(txn, view_name)?;
        }
        Ok(())
    }

    /// Verify the staged, just-applied synthesis against its expectation,
    /// inside the still-open pristine write transaction (CB-9B fail-closed
    /// publication).
    ///
    /// Three checks run:
    /// 1. `ChangeOrigin::GitResolution` changes verify their hashed causal
    ///    frontier against the live dependency index — a missing root or an
    ///    incomplete index fails closed here, before publication.
    /// 2. When `expectation.tree` is present (review R3), the complete
    ///    staged canonical tree must equal the expected Git tree exactly:
    ///    every expected path is present with the exact bytes through the
    ///    change's own assembly visibility (view membership minus
    ///    `assembly_excluded`, which keeps sibling merge legs out of the
    ///    projection), with the expected mode and kind, no path outside the
    ///    expected tree may be alive (extra/missing/rename-source), and the
    ///    semantic (CRDT) reconstruction of every text path must equal the
    ///    expected bytes. There is no persisted-conflict waiver: a resolution
    ///    must render exactly the merge tree or the synthesis fails closed.
    /// 3. Without a complete tree, every touched path is checked exactly as
    ///    recorded: present paths render the expected bytes, deleted paths
    ///    are absent — again with no conflict-table waiver.
    ///
    /// Content is read from the graph, never from the Git worktree: the Git
    /// side of the comparison is the expectation captured from the commit's
    /// tree, the Atomic side is the freshly applied graph state.
    #[allow(clippy::too_many_arguments)]
    fn verify_staged_projection(
        &self,
        txn: &atomic_core::pristine::WriteTxn<'_>,
        view_name: &str,
        final_change: &Change,
        assembly_excluded: &[Hash],
        expectation: &StagedExpectation,
        staged_hash: &Hash,
        verify_effective: bool,
    ) -> Result<(), RepositoryError> {
        use atomic_core::pristine::TreeTxnT;

        // 1. Resolution frontier verification against the dependency index.
        if matches!(
            final_change.origin(),
            atomic_core::change::ChangeOrigin::GitResolution { .. }
        ) {
            // Failpoint: incomplete dependency index (test injection).
            #[cfg(feature = "adoption-test-injection")]
            failpoints::deps_index()?;
            atomic_core::apply::verify_causal_frontier(txn, final_change).map_err(|error| {
                RepositoryError::InvalidOperation {
                    message: format!("causal frontier verification failed: {error}"),
                }
            })?;
        }

        // Failpoint: missing graph vertex during staged retrieval (test
        // injection).
        #[cfg(feature = "adoption-test-injection")]
        failpoints::vertex()?;

        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        // Review C4: the supersede path proves the ACTUAL post-alignment
        // effective view closure — the exact projection publication will
        // show, ancestors included, with no exclusion subtraction. The
        // isolated assembly filter remains the merge-leg proof: a resolution
        // assembles the parent union, whose sibling legs legitimately stay
        // view members and appear in the merge tree itself.
        let visibility = if verify_effective {
            super::filter::graph_visibility_closure(txn, &view)?
        } else {
            super::filter::assembly_visibility_excluding(txn, &view, assembly_excluded)?
        };

        if let Some(tree) = &expectation.tree {
            return self.verify_staged_full_tree(txn, &visibility, final_change, tree, staged_hash);
        }

        // 3. Staged per-path projection equality (legacy touched-path check;
        // no persisted-conflict waiver — review R2).
        for (path, expected) in &expectation.paths {
            match expected {
                Some(bytes) => {
                    // Resolve the file's inode through the TREE superset and
                    // read its staged bytes through the change's own
                    // assembly visibility. The graph content is the source
                    // of truth here; the path-lifecycle projection is
                    // separately validated by the tree-projection stage.
                    let inode = txn
                        .get_inode(path)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?
                        .ok_or_else(|| RepositoryError::InvalidOperation {
                            message: format!(
                                "staged path '{path}' has no inode binding but the synthesis must present it"
                            ),
                        })?;
                    let position = txn
                        .inode_position(inode)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?
                        .ok_or_else(|| RepositoryError::InvalidOperation {
                            message: format!("staged path '{path}' inode has no graph position"),
                        })?;
                    let rendered = retrieve_content_with_filter_fast(
                        txn,
                        &self.change_store,
                        inode,
                        position,
                        atomic_core::output::alive::RetrieveOptions::new()
                            .with_graph_visibility(visibility.clone()),
                    )
                    .map_err(|error| RepositoryError::InvalidOperation {
                        message: format!("staged retrieval failed for '{path}': {error}"),
                    })?;
                    if rendered.as_slice() != bytes.as_slice() {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "staged path '{path}' renders {} byte(s) but the merge tree expects {} byte(s)",
                                rendered.len(),
                                bytes.len()
                            ),
                        });
                    }
                }
                None => {
                    let alive = match txn
                        .get_inode(path)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?
                    {
                        None => false,
                        Some(inode) => {
                            let position = txn
                                .inode_position(inode)
                                .map_err(|e| RepositoryError::Database(e.to_string()))?
                                .ok_or_else(|| RepositoryError::InvalidOperation {
                                    message: format!(
                                        "staged path '{path}' inode has no graph position"
                                    ),
                                })?;
                            super::status::is_file_alive_via_retrieval(
                                txn,
                                inode,
                                position,
                                &visibility,
                            )?
                        }
                    };
                    if alive {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "staged path '{path}' is still present but the synthesis must delete it"
                            ),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Full staged tree equivalence (review R3): the complete staged
    /// canonical projection must equal the commit's verified Git tree.
    ///
    /// Compared layers, all fail-closed before the transaction commits:
    /// - **Tree**: every expected path is present, every staged present path
    ///   is expected (no extras, no surviving rename sources), and every
    ///   deleted path is absent. Persisted conflicts waive nothing.
    /// - **Bytes**: each present path renders the exact expected repository
    ///   bytes through the assembly visibility.
    /// - **Attributes**: each present path's projected mode/kind equals the
    ///   expected repository representation.
    /// - **Semantic reconstruction**: each present path whose CRDT trunk is
    ///   text reconstructs line-by-line to exactly the expected bytes —
    ///   the semantic layer must agree with the graph, not just the bytes.
    #[allow(unused_assignments, unused_variables)] // concurrent-writer instrumentation (RFC §17)
    fn verify_staged_full_tree(
        &self,
        txn: &atomic_core::pristine::WriteTxn<'_>,
        visibility: &atomic_core::pristine::GraphVisibilityClosure,
        final_change: &Change,
        expected_tree: &StagedTreeExpectation,
        staged_hash: &Hash,
    ) -> Result<(), RepositoryError> {
        let verify_start = std::time::Instant::now();
        let projection = self.project_tree_for_visibility(txn, visibility)?;
        let _projection_ms = verify_start.elapsed().as_millis();
        if !projection.name_conflicts.is_empty() {
            let mut conflicted_paths: Vec<&str> = projection
                .name_conflicts
                .keys()
                .map(String::as_str)
                .collect();
            conflicted_paths.sort_unstable();
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "staged projection has {} unresolved name conflict(s) at [{}]; refusing to publish",
                    projection.name_conflicts.len(),
                    conflicted_paths.join(", ")
                ),
            });
        }

        for (path, expected) in &expected_tree.entries {
            let path_start = std::time::Instant::now();
            let Some(item) = projection.present.get(path) else {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "staged tree is missing '{path}' but the commit's Git tree requires it"
                    ),
                });
            };
            if item.is_directory {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "staged path '{path}' is a directory but the commit tree holds a file"
                    ),
                });
            }
            let rendered = retrieve_content_with_filter_fast(
                txn,
                &self.change_store,
                item.inode,
                item.position,
                atomic_core::output::alive::RetrieveOptions::new()
                    .with_graph_visibility(visibility.clone()),
            )
            .map_err(|error| RepositoryError::InvalidOperation {
                message: format!("staged retrieval failed for '{path}': {error}"),
            })?;
            if rendered.as_slice() != expected.bytes.as_slice() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "staged path '{path}' renders {} byte(s) but the commit tree expects {} byte(s)",
                        rendered.len(),
                        expected.bytes.len()
                    ),
                });
            }
            // Attribute equivalence: mode and kind must project exactly.
            let attributes = atomic_core::output::project_inode_attributes(
                txn,
                item.position,
                &visibility.iter_dependency_first().copied().collect(),
            )
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
            if attributes.is_conflicted() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("staged path '{path}' has conflicted inode attributes"),
                });
            }
            if attributes.materialization.kind != expected.kind {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "staged path '{path}' is kind {:?} but the commit tree requires {:?}",
                        attributes.materialization.kind, expected.kind
                    ),
                });
            }
            if attributes.materialization.mode != expected.mode {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "staged path '{path}' has mode {:#o} but the commit tree requires {:#o}",
                        attributes.materialization.mode, expected.mode
                    ),
                });
            }
            if std::env::var_os("ATOMIC_TRACE_GIT_IMPORT").is_some() {
                eprintln!(
                    "[git-import] staged path {path}: {}ms ({} bytes)",
                    path_start.elapsed().as_millis(),
                    expected.bytes.len()
                );
            }
        }

        // CB-9C ac-3: full-tree manifests surface case/normalization
        // collisions explicitly. Distinct tracked paths that fold to the
        // same identity under Unicode case-folding or NFC/NFD equivalence
        // coexist legitimately on a case-sensitive filesystem, but a
        // case-insensitive checkout (macOS/Windows default) or a
        // normalizing filesystem silently merges them — so the check
        // reports every folding class with more than one member instead of
        // letting them pass unreported. This is a surfaced report, never a
        // refusal: the per-state byte checks above already prove the
        // recorded state is exact.
        {
            let mut folded: std::collections::BTreeMap<String, Vec<&str>> =
                std::collections::BTreeMap::new();
            for path in expected_tree.entries.keys() {
                // The canonical String identity is the reversible ESCAPED
                // form (raw non-UTF-8 bytes carry %XX escapes); the fold
                // must see the raw path bytes, never the escape text, or
                // two raw paths that normalize together would report as
                // distinct identities.
                let text = match crate::repository::project_tree::unescape_repo_path(path) {
                    Ok(raw) => String::from_utf8_lossy(&raw).into_owned(),
                    Err(_) => String::from_utf8_lossy(path.as_bytes()).into_owned(),
                };
                let folded_text: String = text
                    .chars()
                    .flat_map(|character| character.nfd())
                    .flat_map(|decomposed| decomposed.to_lowercase())
                    .collect();
                folded.entry(folded_text).or_default().push(path);
            }
            for (identity, members) in &folded {
                if members.len() > 1 {
                    eprintln!(
                        "[git-import] WARNING: {} tracked paths fold to the same \
                         case/normalization identity '{identity}' and would collide on a \
                         case-insensitive or normalizing filesystem: {}",
                        members.len(),
                        members.join(", ")
                    );
                }
            }
        }

        // No extra alive path may exist outside the expected tree: untouched
        // corruption, surviving rename sources, or leaked conflicts all fail.
        // Atomic-private bridge state (`.atomic*`, e.g. `.atomicignore`) is
        // deliberately tracked alive in the view while absent from project
        // manifests, so the conversion policy's own exemption applies.
        //
        // Like-for-like tree representations (review F1): the expected tree
        // holds file entries only, while the canonical projection also holds
        // the structural directories those files live under. Required
        // directories are therefore derived from the expected file paths and
        // verified present as directories instead of being rejected as
        // extras — an ordinary nested tree (`src`, `src/domain`) or a sparse
        // adoption fixture never fails as a phantom extra.
        let mut required_directories: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for path in expected_tree.entries.keys() {
            let bytes = path.as_bytes();
            let mut offset = 0usize;
            while let Some(slash) = bytes[offset..].iter().position(|byte| *byte == b'/') {
                offset += slash;
                required_directories.insert(&path[..offset]);
                offset += 1;
            }
        }
        for path in projection.present.keys() {
            // Like-for-like tree representations (review F1): the canonical
            // projection also holds structural directory entries. Git trees
            // record no directory objects at all, so a tracked directory — a
            // required parent of expected files, or a file-less directory
            // whose contents were all removed — projects away and is never a
            // phantom extra (review CB-9C R7: emptied directories surface the
            // omission as explicit EmptyDirectory loss instead of refusing
            // the import or fabricating a directory-only tree entry).
            if projection
                .present
                .get(path)
                .is_some_and(|item| item.is_directory)
            {
                continue;
            }
            if expected_tree.entries.contains_key(path)
                || crate::repository::project_tree::atomic_private_path(path.as_bytes())
                || crate::repository::project_tree::bridge_private_path(path.as_bytes())
            {
                continue;
            }
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "staged path '{path}' is present but the commit tree does not hold it"
                ),
            });
        }
        // Every required structural directory must project present and as an
        // actual directory (not a file collision at an interior path).
        for directory in &required_directories {
            match projection.present.get(*directory) {
                Some(item) if item.is_directory => {}
                Some(_) => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "staged path '{directory}' is a file but the commit tree requires a directory"
                        ),
                    });
                }
                None => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "staged tree is missing directory '{directory}' but the commit tree requires it"
                        ),
                    });
                }
            }
        }

        // Semantic-layer coverage (review F2/F4): two independent checks.
        //
        // 1. Delta coverage: every path the caller required regenerated
        //    semantic FileOps for must carry at least one FileOp in the
        //    staged change. The set is the caller's ACTUAL recorded delta
        //    paths (review F2), so a resolution whose union already renders
        //    the merge bytes legitimately emits no new FileOps and is not
        //    required to invent edits. An opaque generated-file shortcut
        //    (graph facts without a semantic interpretation) for a recorded
        //    delta still fails closed here (review R3; the historical
        //    merge-resolution zero-FileOps defect).
        // 2. Real semantic reconstruction (review F4): every present text
        //    path with a CRDT trunk must reconstruct through the semantic
        //    layer — trunk → branches in file order → BRANCH_VERTEX → change
        //    content bytes — to exactly the expected bytes. This inspects the
        //    staged CRDT tables themselves (not just FileOp path names), so
        //    missing trunks, orphan branches, corrupted after-chain order,
        //    and wrong vertex ranges fail closed. A delta path whose staged
        //    FileOps claim content edits must have a trunk (missing-trunk
        //    refusal); an inherited path without a trunk (binary, symlink,
        //    or a pre-semantic legacy change) is content-checked by the
        //    graph layer above, not waived here — its semantic state simply
        //    does not exist to compare.
        let staged_file_ops: std::collections::HashMap<&str, &atomic_core::change::FileOps> =
            final_change
                .file_ops()
                .iter()
                .map(|ops| (ops.path(), ops))
                .collect();
        // Preload the interpreted closure ONCE for every path's semantic
        // verification (review ::15 scaling fix, 2026-09-16): the historical
        // per-path verification re-loaded and re-replayed the whole closure
        // per path — O(paths x closure leaf ops), which alone made a
        // 300-file import take minutes. The preloaded records carry, per
        // change, each file entry's leaf-counter OFFSET (the counter value
        // when the apply reaches that entry: one counter per change,
        // advanced by every earlier entry's leaf ops), so each path replays
        // only its own entries without re-deriving the shared prefix.
        let mut staged_closure: Vec<(atomic_core::types::NodeId, Change, Vec<u32>)> = Vec::new();
        {
            fn entry_counter_offsets(change: &Change) -> Vec<u32> {
                let mut offsets = Vec::with_capacity(change.file_ops().len());
                let mut counter = 0u32;
                for ops in change.file_ops() {
                    offsets.push(counter);
                    for line_op in ops.line_ops() {
                        match line_op.operation() {
                            atomic_core::crdt::BranchOp::Insert { content, .. } => {
                                counter += content.len() as u32;
                            }
                            atomic_core::crdt::BranchOp::Modify { new_content, .. } => {
                                counter += new_content.len() as u32;
                            }
                            _ => {}
                        }
                    }
                }
                offsets
            }
            let mut seen_changes = std::collections::HashSet::new();
            for change_id in visibility.iter_dependency_first().copied() {
                if change_id.is_root() || !seen_changes.insert(change_id) {
                    continue;
                }
                let change_hash = txn
                    .get_external(change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::InvalidOperation {
                        message: format!("closure change {change_id:?} has no external hash"),
                    })?;
                let change = self.load_change(&change_hash).map_err(|e| {
                    RepositoryError::InvalidOperation {
                        message: format!("cannot load closure change: {e}"),
                    }
                })?;
                let offsets = entry_counter_offsets(&change);
                staged_closure.push((change_id, change, offsets));
            }
        }
        for (path, expected) in &expected_tree.entries {
            // Review CB-9C R2: EVERY supported manifest entry receives
            // complete semantic verification, including inherited untouched
            // symlinks/gitlinks. Delta FileOps presence is not a substitute
            // for validating trunk existence, path identity, lifecycle and
            // the appropriate representation of the expected bytes — a
            // corrupted untouched symlink trunk must refuse before
            // publication just like a regular file's. Text-specific token
            // replay applies wherever the trunk classification has a line
            // domain; opaque content is guarded by the same trunk
            // existence/identity/lifecycle checks plus the graph-layer byte
            // equality above.
            let Some(item) = projection.present.get(path) else {
                // Already refused above; nothing further to reconstruct.
                continue;
            };
            let semantic_start = std::time::Instant::now();
            self.verify_staged_semantic_closure(
                txn,
                path.as_str(),
                &expected.bytes,
                item.position,
                visibility,
                staged_hash,
                &staged_closure,
            )
            .map_err(|error| RepositoryError::InvalidOperation {
                message: format!("staged semantic closure for '{path}': {error}"),
            })?;
            if std::env::var_os("ATOMIC_TRACE_GIT_IMPORT").is_some() {
                eprintln!(
                    "[git-import] semantic closure {path}: {}ms",
                    semantic_start.elapsed().as_millis()
                );
            }
        }
        for path in &expected_tree.semantic_paths {
            if !staged_file_ops.contains_key(path.as_str()) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "staged change carries no semantic FileOps for '{path}'; the semantic layer must reconstruct it"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Exact semantic closure verification (review B2 / B4): replay every
    /// interpreted closure change's FileOps for `path` to derive the EXPECTED
    /// semantic domain — trunk existence and classification, branch set,
    /// lifecycle, file order, token bindings — and refuse the staged CRDT
    /// state when it disagrees.
    ///
    /// The expected domain comes from the closure replay, never from
    /// whichever rows happen to survive: a missing trunk for a path the
    /// closure semantically created, a flipped text/binary classification, an
    /// unattributed tombstone (a branch row marked Deleted while the graph
    /// proves the line alive in the global graph — no change ever deleted it),
    /// a wrong after-chain order, and wrong leaf lifecycle/payload all fail
    /// closed here. Concurrent state written by changes outside this closure
    /// is preserved, not refused: a branch whose global `BRANCH_VERTEX` is
    /// owned by an out-of-closure writer is that writer's responsibility, and
    /// a tombstone is genuine exactly when the global graph (every applied
    /// change, unfiltered) proves the line dead.
    ///
    /// The replay's own aggregate must equal the expected tree bytes: the
    /// closure's semantic operations must regenerate the commit tree, so a
    /// closure that disagrees with Git fails closed before publication.
    #[allow(clippy::too_many_arguments)]
    fn verify_staged_semantic_closure(
        &self,
        txn: &atomic_core::pristine::WriteTxn<'_>,
        path: &str,
        expected_bytes: &[u8],
        file_position: atomic_core::types::Position<atomic_core::types::NodeId>,
        visibility: &atomic_core::pristine::GraphVisibilityClosure,
        _staged_hash: &Hash,
        staged_closure: &[(atomic_core::types::NodeId, Change, Vec<u32>)],
    ) -> Result<(), String> {
        use atomic_core::crdt::{
            queries::iter_trunk_branches_in_file_order,
            tables::{encode_branch_id, encode_trunk_id},
            BranchState, TrunkState,
        };
        use atomic_core::pristine::{CrdtTxnT, GraphTxnT};

        // ── 1. Replay the closure's FileOps for this path. ────────────────
        //
        // Each closure change is applied exactly once in dependency order;
        // within a change the file entries and lines keep their recorded
        // order, and the leaf-id counter spans the whole change (the
        // `apply_file_ops` contract — one counter per change, advanced only by
        // Insert and Modify line contents). The replay reproduces those
        // per-change substitutions and counters, so the expected domain is
        // derived from the interpreted closure itself — never from whichever
        // rows happen to survive (review B2).

        fn expected_leaves(
            content: &[atomic_core::crdt::LeafOp],
            owner: atomic_core::types::NodeId,
            counter: &mut u32,
        ) -> Vec<ExpectedLeaf> {
            let mut leaves = Vec::new();
            for leaf_op in content {
                if let atomic_core::crdt::LeafOp::Insert { kind, content, .. } = leaf_op {
                    // The apply keys leaf rows by the APPLYING change and its
                    // per-apply counter (review C1 source fix), so the replay
                    // derives the expected identity the same way.
                    leaves.push(ExpectedLeaf {
                        id: atomic_core::crdt::LeafId::new(owner, *counter),
                        kind: *kind,
                        // The apply records every Insert token at
                        // content_start 0 with content_end = the token's
                        // byte length (the apply contract).
                        content_start: 0,
                        content_end: content.len() as u32,
                    });
                }
                *counter += 1;
            }
            leaves
        }

        let mut expected_trunk_encoding: Option<u8> = None;
        let mut trunk_created = false;
        let mut trunk_deleted = false;
        let mut branches: std::collections::BTreeMap<atomic_core::crdt::BranchId, ExpectedBranch> =
            std::collections::BTreeMap::new();
        // Branch rows the closure CREATED (Insert ops): their `BRANCH_AFTER`
        // rows are this closure's own chain state, so their after-refs and
        // chain membership are enforced below.
        let mut closure_created: std::collections::HashSet<atomic_core::crdt::BranchId> =
            std::collections::HashSet::new();

        // ── 1a. Trunk-identity discovery across Move ops (review CB-9C R2). ─
        //
        // A pure imported rename is a real semantic move: the closure's move
        // change retargets the OLD path's trunk to this path with
        // `TrunkOp::Move`. The branch domain (base creation inserts, edits)
        // keeps being recorded under that same trunk id — with the OLD path's
        // name in the historical FileOps entries. The replay must therefore
        // follow trunk identity across the move: entries whose resolved trunk
        // id equals the moved trunk belong to this path's expected domain
        // even though their recorded path is the predecessor. Without this,
        // a moved path replayed by literal path name would derive an empty
        // expected domain and (before the source fix) accept a missing
        // path→trunk mapping.
        //
        // A Move op retargeting `path` names its trunk explicitly (the apply
        // resolved real ids when recording, so no placeholder substitution
        // applies to it). Two distinct trunks retargeting one path is a
        // contradiction — the apply refuses path collisions, so the replay
        // refuses it too.
        // The LAST move of each trunk decides its final location: import
        // FileOps entries name the NEW path (review ::15 evidence — rename
        // records name the destination), so "moved away" cannot be keyed on
        // the entry's path. A trunk whose final move retargets this path is
        // followed here; a trunk whose final move retargets any other path
        // left this path's domain for good.
        let mut last_move: std::collections::BTreeMap<atomic_core::crdt::TrunkId, String> =
            std::collections::BTreeMap::new();
        for (change_id, change, _offsets) in staged_closure {
            for ops in change.file_ops() {
                if let Some(atomic_core::crdt::TrunkOp::Move { trunk, new_path }) = ops.trunk_op() {
                    let resolved = if trunk.change_id().is_root() {
                        atomic_core::crdt::TrunkId::new(*change_id, trunk.file_idx())
                    } else {
                        *trunk
                    };
                    last_move.insert(resolved, new_path.clone());
                }
            }
        }
        let identity_trunks: std::collections::HashSet<atomic_core::crdt::TrunkId> = last_move
            .iter()
            .filter(|(_, target)| target.as_str() == path)
            .map(|(trunk, _)| *trunk)
            .collect();
        let moved_away: std::collections::HashSet<atomic_core::crdt::TrunkId> = last_move
            .iter()
            .filter(|(_, target)| target.as_str() != path)
            .map(|(trunk, _)| *trunk)
            .collect();
        if identity_trunks.len() > 1 {
            return Err(format!(
                "{} distinct trunks' final moves retarget path '{path}' inside one \
                 interpreted closure; the semantic move chain is contradictory",
                identity_trunks.len()
            ));
        }

        // C3 fix (::16): trunks DELETED and later superseded by a fresh
        // Create for this path. A recreated path starts a fresh trunk; the
        // deleted trunk's historical semantics (its Create/Insert entries
        // and its own Delete) belong to the tombstoned incarnation, never
        // to the recreated path's expected domain. Without this set the
        // replay unions the deleted trunk's lines into the recreated path
        // and the staged ownership check fails on the stale branch rows.
        let mut superseded: std::collections::HashSet<atomic_core::crdt::TrunkId> =
            std::collections::HashSet::new();
        let mut pending_delete: Option<atomic_core::crdt::TrunkId> = None;
        for (change_id, change, _offsets) in staged_closure {
            for ops in change.file_ops() {
                if ops.path() != path {
                    continue;
                }
                match ops.trunk_op() {
                    Some(atomic_core::crdt::TrunkOp::Delete { trunk }) => {
                        let resolved = if trunk.change_id().is_root() {
                            atomic_core::crdt::TrunkId::new(*change_id, trunk.file_idx())
                        } else {
                            *trunk
                        };
                        pending_delete = Some(resolved);
                    }
                    Some(atomic_core::crdt::TrunkOp::Create { .. }) => {
                        if let Some(deleted) = pending_delete.take() {
                            superseded.insert(deleted);
                        }
                    }
                    _ => {}
                }
            }
        }

        for (change_id, change, offsets) in staged_closure {
            // One leaf counter per change, spanning every file entry in
            // order. Each entry starts at its precomputed offset (the exact
            // counter value the apply reaches it with); unrelated entries
            // advance nothing here — their contribution is already in the
            // offsets — so a path replays only its own entries.
            let change_id = *change_id;
            let mut leaf_counter;
            for (entry_index, ops) in change.file_ops().iter().enumerate() {
                leaf_counter = offsets[entry_index];
                // Trunk identity across Move (review CB-9C R2): an entry
                // belongs to this path's semantic domain when it names this
                // path OR its resolved trunk is one of the trunks a closure
                // move retargeted here. Placeholder trunk ids (the change
                // created this file) resolve to the creating change exactly
                // like the apply does.
                let resolved_trunk = {
                    let raw = ops.trunk_id();
                    if raw.change_id().is_root() {
                        atomic_core::crdt::TrunkId::new(change_id, raw.file_idx())
                    } else {
                        raw
                    }
                };
                let trunk_followed = identity_trunks.contains(&resolved_trunk)
                    && !moved_away.contains(&resolved_trunk);
                let is_target = ((ops.path() == path && !moved_away.contains(&resolved_trunk))
                    || trunk_followed)
                    && !superseded.contains(&resolved_trunk);
                if is_target {
                    match ops.trunk_op() {
                        Some(atomic_core::crdt::TrunkOp::Create { encoding, .. }) => {
                            trunk_created = true;
                            expected_trunk_encoding = Some(crdt_encoding_u8(encoding.as_ref()));
                        }
                        // Trunk lifecycle is part of the replayed domain
                        // (review C1): the closure's Delete/Undelete ops
                        // determine the expected trunk row state.
                        Some(atomic_core::crdt::TrunkOp::Delete { .. }) => {
                            trunk_deleted = true;
                        }
                        Some(atomic_core::crdt::TrunkOp::Undelete { .. }) => {
                            trunk_deleted = false;
                        }
                        _ => {}
                    }
                }
                if !is_target {
                    // Unrelated file entries contribute nothing to this
                    // path's domain: their leaf-id counter advance is
                    // already baked into the precomputed per-entry offsets
                    // (review ::26 R6 — the historical code still replayed
                    // and discarded every unrelated leaf here).
                    continue;
                }
                for line_op in ops.line_ops() {
                    let raw_branch = line_op.branch_id();
                    let branch_id = if raw_branch.change_id().is_root() {
                        atomic_core::crdt::BranchId::new(change_id, raw_branch.branch_idx())
                    } else {
                        raw_branch
                    };
                    match line_op.operation() {
                        atomic_core::crdt::BranchOp::Insert { after, content } => {
                            let (vertex, bytes) = match line_op.content_range() {
                                Some((start, end)) => {
                                    let start = start.get() as usize;
                                    let end = end.get() as usize;
                                    (
                                        Some(atomic_core::types::GraphNode {
                                            change: change_id,
                                            start: (start as u64).into(),
                                            end: (end as u64).into(),
                                        }),
                                        change.contents[start..end].to_vec(),
                                    )
                                }
                                None => (None, Vec::new()),
                            };
                            let resolved_after = after.map(|id| {
                                if id.change_id().is_root() {
                                    atomic_core::crdt::BranchId::new(change_id, id.branch_idx())
                                } else {
                                    id
                                }
                            });
                            let leaves = expected_leaves(content, change_id, &mut leaf_counter);
                            branches.insert(
                                branch_id,
                                ExpectedBranch {
                                    state: BranchState::Alive,
                                    vertex,
                                    bytes,
                                    after: resolved_after,
                                    leaves,
                                    dead_leaves: Vec::new(),
                                    last_writer: change_id,
                                },
                            );
                            closure_created.insert(branch_id);
                        }
                        atomic_core::crdt::BranchOp::Modify { new_content, .. } => {
                            let (vertex, bytes) = match line_op.content_range() {
                                Some((start, end)) => {
                                    let start = start.get() as usize;
                                    let end = end.get() as usize;
                                    (
                                        Some(atomic_core::types::GraphNode {
                                            change: change_id,
                                            start: (start as u64).into(),
                                            end: (end as u64).into(),
                                        }),
                                        change.contents[start..end].to_vec(),
                                    )
                                }
                                None => (None, Vec::new()),
                            };
                            let leaves = expected_leaves(new_content, change_id, &mut leaf_counter);
                            match branches.get_mut(&branch_id) {
                                Some(existing) => {
                                    // The apply tombstones the branch's
                                    // previous alive tokens when it rewrites
                                    // the line (review C1 source fix): the
                                    // replay mirrors that lifecycle.
                                    let previous = std::mem::replace(&mut existing.leaves, leaves);
                                    existing
                                        .dead_leaves
                                        .extend(previous.into_iter().map(|leaf| leaf.id));
                                    existing.state = BranchState::Alive;
                                    existing.vertex = vertex;
                                    existing.bytes = bytes;
                                    existing.last_writer = change_id;
                                }
                                None => {
                                    // A modify bound to a branch this closure
                                    // did not create (ambient binding): this
                                    // change now owns the row's content, but
                                    // the chain position predates the closure
                                    // and is not replayed here. The branch's
                                    // pre-closure tokens were written before
                                    // the closure and are tombstoned by the
                                    // apply; they are not part of the
                                    // replayed domain.
                                    branches.insert(
                                        branch_id,
                                        ExpectedBranch {
                                            state: BranchState::Alive,
                                            vertex,
                                            bytes,
                                            after: None,
                                            leaves,
                                            dead_leaves: Vec::new(),
                                            last_writer: change_id,
                                        },
                                    );
                                }
                            }
                        }
                        atomic_core::crdt::BranchOp::Delete { .. } => {
                            if let Some(existing) = branches.get_mut(&branch_id) {
                                existing.state = BranchState::Deleted;
                            }
                        }
                        atomic_core::crdt::BranchOp::Restore { .. } => {
                            if let Some(existing) = branches.get_mut(&branch_id) {
                                existing.state = BranchState::Alive;
                            }
                        }
                        atomic_core::crdt::BranchOp::Reparent { new_after, .. } => {
                            let resolved_after = new_after.map(|id| {
                                if id.change_id().is_root() {
                                    atomic_core::crdt::BranchId::new(change_id, id.branch_idx())
                                } else {
                                    id
                                }
                            });
                            if let Some(existing) = branches.get_mut(&branch_id) {
                                existing.after = resolved_after;
                            }
                        }
                    }
                }
            }
        }

        // ── 2. Trunk existence, identity, classification and lifecycle. ────
        //
        // The trunk index is path-keyed — the same index the CRDT output
        // walker uses. A path the closure semantically created MUST have its
        // trunk row and classification: a missing path→trunk mapping or a
        // flipped text/binary encoding is corruption, never a waiver (review
        // B2). Review C1 adds trunk identity and lifecycle: the trunk row
        // must claim this path (mapping integrity) and its Alive/Deleted
        // state must agree with the closure replay — a live file whose trunk
        // row was tombstoned by nobody is corruption, not a waiver.
        let Some(trunk_key) = txn.get_trunk_by_path(path).map_err(|e| e.to_string())? else {
            if trunk_created {
                return Err(
                    "the closure created this trunk but the staged CRDT tables hold no \
                     path→trunk mapping; the semantic layer must reconstruct it"
                        .to_string(),
                );
            }
            // Review CB-9C R2: a move retargeted this path inside the
            // closure, so the semantic move MUST have produced the
            // destination's path→trunk mapping. A graph-present supported
            // file with no mapping after reopen is exactly the moved-path
            // identity defect — never accepted because "no Create replayed
            // here".
            if !identity_trunks.is_empty() {
                return Err(
                    "the closure moves a trunk to this path but the staged CRDT tables \
                     hold no path→trunk mapping; the semantic move must retarget the \
                     moved trunk to its new path"
                        .to_string(),
                );
            }
            return Ok(());
        };
        if let Some(expected_trunk) = identity_trunks.iter().next() {
            if &trunk_key != expected_trunk {
                return Err(format!(
                    "the path→trunk mapping for '{path}' resolves to {:?} but the closure's \
                     semantic move targets {:?}; the moved trunk identity must match the \
                     closure",
                    trunk_key, expected_trunk
                ));
            }
        }
        let trunk_row = txn
            .get_crdt_trunk(&encode_trunk_id(&trunk_key))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                "the path→trunk mapping resolves but the trunk row is missing".to_string()
            })?;
        if trunk_row.path != path {
            return Err(format!(
                "the path→trunk mapping resolves to a trunk row claiming path {:?}; the \
                 semantic index is corrupt",
                trunk_row.path
            ));
        }
        // Whether any genuine out-of-closure concurrent state was accepted on
        // this trunk (review D2): every tolerance path sets this flag ONLY
        // after the concurrent writer was validated by its actual operations
        // (a loadable, applied change whose FileOps recorded the binding).
        // While the flag stays false, the consumer-facing CRDT reconstruction
        // must equal the expected bytes exactly at the end of this function —
        // no tolerance applies.
        let mut concurrent_state = false;
        let expected_trunk_encoding = match expected_trunk_encoding {
            Some(expected) => {
                if trunk_row.encoding != expected {
                    return Err(format!(
                        "the closure created this trunk with encoding {} but the staged row \
                         claims encoding {}; the classification must match the closure",
                        expected, trunk_row.encoding
                    ));
                }
                expected
            }
            None => trunk_row.encoding,
        };
        // Trunk lifecycle (review C1/D2): enforced for EVERY classification —
        // opaque content has no text tokens to invent, but its trunk row must
        // still be alive when the closure expects it alive. The historical
        // early return for encodings 0|4 skipped lifecycle verification and
        // accepted an unattributed tombstone on a binary trunk (review D2,
        // fixture cb9b-current-binary-deleted). The closure replay's
        // Delete/Undelete ops determine the expected state; a trunk row
        // marked Deleted while the closure expects it alive is genuine
        // exactly when some applied out-of-closure change recorded the
        // semantic delete — operation attribution, not a waiver (review C1).
        let trunk_expected_alive = !trunk_deleted;
        if trunk_expected_alive && trunk_row.state != TrunkState::Alive {
            let loader = |hash: &Hash| -> Result<Change, String> {
                self.load_change(hash).map_err(|e| e.to_string())
            };
            if out_of_closure_writer_deleted_trunk(&loader, txn, path, visibility)? {
                // A validated out-of-closure writer recorded the semantic
                // delete: genuine concurrent state (review D2).
                concurrent_state = true;
            } else {
                return Err(
                    "the file is alive in the interpreted closure and no applied change \
                     deleted it, but the trunk row is marked Deleted: an unattributed \
                     trunk tombstone is corruption"
                        .to_string(),
                );
            }
        }
        if matches!(expected_trunk_encoding, 0 | 4) {
            // Opaque binary content has no semantic line/token layer to
            // replay: the graph layer above is authoritative for its bytes
            // (review B2: an appropriate opaque-trunk check, not invented
            // text tokens). Lifecycle was verified above (review D2).
            return Ok(());
        }

        // ── 3. Row agreement for the closure's own branch domain. ──────────
        //
        // The GLOBAL graph's alive set (every applied change, unfiltered)
        // attributes tombstones: a branch row marked Deleted is genuine
        // exactly when some applied change deleted the line's vertex; a
        // tombstone with no deleting change at all is corruption (review B2).
        // A branch whose global row is owned by an out-of-closure writer is
        // that writer's responsibility — concurrent semantic state is
        // preserved, not refused (review B2).

        // Convert a semantic Encoding to its storage byte (the same mapping
        // the apply pass uses).
        fn crdt_encoding_u8(encoding: Option<&atomic_core::change::Encoding>) -> u8 {
            use atomic_core::change::Encoding;
            match encoding {
                None => 0,
                Some(Encoding::Utf8) => 1,
                Some(Encoding::Utf16Le) => 2,
                Some(Encoding::Utf16Be) => 3,
                Some(Encoding::Binary) => 4,
                Some(Encoding::Latin1) => 5,
            }
        }

        // Verify one branch's staged leaf rows against the replay's expected
        // domain (review C1: exact token identity for the WHOLE closure
        // domain, inherited branches included).
        //
        // The apply keys leaf rows by the applying change and its per-apply
        // counter (review C1 source fix), so the expected identity is stable
        // across applies and the comparison is exact everywhere: a missing
        // row, a flipped lifecycle, a flipped kind, wrong payload bounds
        // (content_start or content_end), a row claiming a different branch,
        // or a missing BRANCH_LEAVES linkage all fail closed. There is no
        // inherited-leaf skip and no last-apply-wins escape hatch.
        fn verify_branch_leaves(
            txn: &atomic_core::pristine::WriteTxn<'_>,
            branch: atomic_core::crdt::BranchId,
            branch_key: &[u8; 12],
            expected_leaves: &[ExpectedLeaf],
        ) -> Result<(), String> {
            for expected in expected_leaves {
                let key = atomic_core::crdt::tables::encode_leaf_id(&expected.id);
                let row = txn
                    .get_crdt_leaf(&key)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| {
                        format!(
                            "leaf {:?} is expected alive by the closure but the staged \
                             LEAVES row is missing",
                            expected.id
                        )
                    })?;
                if !row.state.is_alive() {
                    return Err(format!(
                        "leaf {:?} is expected alive by the closure but the staged row \
                         is not; token lifecycle corruption",
                        expected.id
                    ));
                }
                if row.kind != expected.kind {
                    return Err(format!(
                        "leaf {:?} has kind {:?} but the closure's writer recorded {:?}",
                        expected.id, row.kind, expected.kind
                    ));
                }
                if row.content_start != expected.content_start
                    || row.content_end != expected.content_end
                {
                    return Err(format!(
                        "leaf {:?} spans {}..{} but the closure's writer recorded \
                         {}..{}; token payload bounds must match the closure",
                        expected.id,
                        row.content_start,
                        row.content_end,
                        expected.content_start,
                        expected.content_end
                    ));
                }
                if row.branch_id != branch {
                    return Err(format!(
                        "leaf {:?} claims branch {:?} but the closure's writer recorded \
                         {:?}; token ownership must match the closure",
                        expected.id, row.branch_id, branch
                    ));
                }
                let linked: Vec<[u8; 12]> = txn
                    .iter_branch_leaves(branch_key)
                    .map_err(|e| e.to_string())?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| e.to_string())?;
                if !linked.contains(&key) {
                    return Err(format!(
                        "leaf {:?} is not linked to its branch; token bindings must \
                         match the closure",
                        expected.id
                    ));
                }
            }
            Ok(())
        }

        let alive_start = std::time::Instant::now();
        let global_alive = global_alive_set(txn, file_position)?;
        if std::env::var_os("ATOMIC_TRACE_GIT_IMPORT").is_some() {
            eprintln!(
                "[git-import] semantic replay+alive {}: {}ms (alive={})",
                path,
                alive_start.elapsed().as_millis(),
                global_alive.len()
            );
        }

        // Review E1: per-line expectations for the closure-aware consumer
        // equality. The equality between the actual CRDT reconstruction
        // consumers read and the expected domain is ALWAYS enforced — genuine
        // concurrent state cannot waive it. The expected domain is
        // closure-aware: the closure's own replay bytes for closure lines
        // bound to the replayed vertices, plus each VALIDATED concurrent
        // writer's own recorded content at exactly the vertex its operations
        // bound; validated tombstones contribute nothing. Any row that failed
        // attribution above already refused.
        let mut consumer_domain: std::collections::HashMap<
            atomic_core::crdt::BranchId,
            ConsumerLine,
        > = std::collections::HashMap::new();

        let mut replay_alive_ids: Vec<atomic_core::crdt::BranchId> = Vec::new();
        for (branch_id, expected) in &branches {
            let branch_key = encode_branch_id(branch_id);
            let Some(row) = txn
                .get_crdt_branch(&branch_key)
                .map_err(|e| e.to_string())?
            else {
                return Err(format!(
                    "branch {branch_id:?} is written by this closure but the staged \
                     BRANCHES row is missing"
                ));
            };
            // Branch ownership (review C1): the row must belong to this
            // path's trunk. A row re-keyed to another trunk is corruption.
            if row.trunk_id != trunk_key {
                return Err(format!(
                    "branch {branch_id:?} claims a different trunk than the closure \
                     replayed for '{path}'; branch ownership must match the closure"
                ));
            }
            let row_vertex = txn
                .get_crdt_branch_vertex(&branch_key)
                .map_err(|e| e.to_string())?;
            if expected.state.is_alive() {
                replay_alive_ids.push(*branch_id);
                if !row.state.is_alive() {
                    // The row says Deleted. Genuine tombstones are attributed:
                    // either the global graph proves the line dead (some
                    // applied change graph-deleted the vertex), or some
                    // applied out-of-closure change recorded the semantic
                    // Delete/Modify op that tombstoned the row (review C1:
                    // operation attribution). A tombstone with no writer at
                    // all is corruption and fails closed (review B2).
                    //
                    // Review D2: a nonexistent vertex is NOT proof of
                    // deletion. The binding must reference a real applied
                    // change AND a real graph vertex — absence from the
                    // global alive set alone also covers vertices that were
                    // never applied at all (fixture
                    // cb9b-current-tombstone-vertex binds a live branch to
                    // [10000:20000] in its real creator and reads as
                    // "graph-deleted" without this check).
                    //
                    // Review E1: a vertex of ANOTHER file is NOT deletion
                    // evidence either. This file's global alive set only
                    // covers THIS file's graph, so an unrelated file's real,
                    // alive vertex satisfies `has_vertex && !alive` without
                    // being dead at all. The binding must prove exact
                    // branch→vertex file ownership: the vertex's introducing
                    // change is applied AND its own FileOps recorded this
                    // exact branch id and content range for this path (fixture
                    // cb9b-r8-real-unrelated-vertex tombstones f.txt's branch
                    // while binding g.txt's real vertex).
                    let graph_proves_dead = match row_vertex.as_ref() {
                        Some(node) => graph_proves_dead_with_ownership(
                            self,
                            txn,
                            path,
                            *branch_id,
                            node,
                            &global_alive,
                        )?,
                        None => false,
                    };
                    let attributed = graph_proves_dead || {
                        let loader = |hash: &Hash| -> Result<Change, String> {
                            self.load_change(hash).map_err(|e| e.to_string())
                        };
                        out_of_closure_writer_deleted_branch(
                            &loader, txn, path, *branch_id, visibility,
                        )?
                    };
                    if !attributed {
                        return Err(
                            "a branch is marked Deleted but no applied change deleted it (the \
                             global graph proves the line alive and no writer recorded the \
                             delete): an unattributed tombstone is corruption"
                                .to_string(),
                        );
                    }
                    concurrent_state = true;
                    continue;
                }
                // No missing-vertex if-let bypass (review C1): an expected
                // vertex must be present in the staged table, and a vertex
                // binding the closure never wrote must be refused.
                let Some(expected_vertex) = &expected.vertex else {
                    // The closure's writer recorded no content range for this
                    // line (empty line): the staged row must not claim one.
                    if row_vertex.is_some() {
                        return Err(format!(
                            "branch {branch_id:?} is bound to a graph vertex but the \
                             closure's last writer recorded no content range for it"
                        ));
                    }
                    continue;
                };
                let Some(row_node) = &row_vertex else {
                    return Err(format!(
                        "branch {branch_id:?} has no BRANCH_VERTEX binding but the \
                         closure's last writer bound it to {expected_vertex:?}; the line \
                         cannot be reconstructed without its vertex"
                    ));
                };
                if row_node == expected_vertex {
                    // Exact token agreement for the closure's own writer
                    // (review C1: identity/kind/payload bounds/ownership for
                    // the whole closure domain, inherited branches included —
                    // the apply keys leaf rows by the applying change, so the
                    // expected identity is deterministic).
                    verify_branch_leaves(txn, *branch_id, &branch_key, &expected.leaves)?;
                    // The consumer renders the replay's own bytes for this
                    // line (review E1).
                    consumer_domain
                        .insert(*branch_id, ConsumerLine::Closure(expected.bytes.clone()));
                } else {
                    // Review D2: a registered NodeId is NOT proof of a
                    // writer. The out-of-closure attribution requires a
                    // loadable, APPLIED change whose own FileOps recorded
                    // this exact vertex for this path (review C1); anything
                    // else is corruption.
                    let writer_registered = !row_node.change.is_root()
                        && txn
                            .get_external(row_node.change)
                            .map_err(|e| e.to_string())?
                            .is_some();
                    let writer_applied = !row_node.change.is_root()
                        && txn
                            .has_change_in_graph(row_node.change)
                            .map_err(|e| e.to_string())?;
                    if writer_registered && writer_applied && !visibility.contains(row_node.change)
                    {
                        // A registered, applied change outside this closure
                        // claims the row. Attribution is by ACTUAL operations
                        // (review C1), not bare registered ids: the writer's
                        // own FileOps must have recorded this exact branch
                        // and vertex for this path; otherwise the binding is
                        // corruption.
                        let writer_hash = txn
                            .get_external(row_node.change)
                            .map_err(|e| e.to_string())?
                            .ok_or_else(|| {
                                "branch vertex writer has no external hash".to_string()
                            })?;
                        let writer = self
                            .load_change(&writer_hash)
                            .map_err(|e| format!("cannot load concurrent writer: {e}"))?;
                        if !change_wrote_branch_vertex_for_branch(
                            &writer, path, *branch_id, *row_node,
                        ) {
                            return Err(format!(
                                "branch {branch_id:?} is bound to {row_node:?} but the \
                                 closure's last writer bound it to {expected_vertex:?} and \
                                 no concurrent change recorded that binding for this path"
                            ));
                        }
                        // Concurrent semantic state, validated by actual
                        // operations (review D2). The consumer renders the
                        // writer's own recorded content at its vertex
                        // (review E1).
                        consumer_domain.insert(*branch_id, ConsumerLine::Writer(*row_node));
                        concurrent_state = true;
                    } else if !writer_registered || !writer_applied {
                        // A binding that references no real applied change
                        // (e.g. the ROOT sentinel from injected corruption)
                        // can never reconstruct the line: refuse.
                        return Err(
                            "branch vertex binding references no real applied change; the \
                             semantic layer cannot reconstruct the line"
                                .to_string(),
                        );
                    } else {
                        return Err(format!(
                            "branch is bound to {row_node:?} but the closure's last \
                             writer bound it to {expected_vertex:?}"
                        ));
                    }
                }
            } else if row.state.is_alive() {
                // Review E1: the closure deleted this line but the staged row
                // is Alive — an out-of-closure writer restored it. This is
                // genuine concurrent state ONLY when the binding is exactly
                // attributed: either the row still binds the closure's own
                // last-writer vertex (a restore without rebinding), or an
                // applied out-of-closure writer's own FileOps recorded this
                // exact branch→vertex binding for this path. Anything else is
                // corruption: the historical code accepted this case with no
                // validation at all and the skipped consumer check could not
                // catch a corrupted payload behind it.
                let Some(row_node) = &row_vertex else {
                    return Err(format!(
                        "branch {branch_id:?} was deleted by the closure but the staged \
                         row is alive with no BRANCH_VERTEX binding; the semantic \
                         reconstruction consumers read fails with OrphanBranch for it"
                    ));
                };
                if expected.vertex.as_ref() == Some(row_node) {
                    consumer_domain
                        .insert(*branch_id, ConsumerLine::Closure(expected.bytes.clone()));
                } else {
                    let writer_registered = !row_node.change.is_root()
                        && txn
                            .get_external(row_node.change)
                            .map_err(|e| e.to_string())?
                            .is_some();
                    let writer_applied = !row_node.change.is_root()
                        && txn
                            .has_change_in_graph(row_node.change)
                            .map_err(|e| e.to_string())?;
                    if !(writer_registered && writer_applied)
                        || visibility.contains(row_node.change)
                    {
                        return Err(format!(
                            "branch {branch_id:?} was deleted by the closure but the staged \
                             row is alive bound to {row_node:?}; neither the closure's own \
                             last-writer vertex nor an applied out-of-closure writer's \
                             recorded binding, so the restore is corruption"
                        ));
                    }
                    let writer_hash = txn
                        .get_external(row_node.change)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| "branch vertex writer has no external hash".to_string())?;
                    let writer = self
                        .load_change(&writer_hash)
                        .map_err(|e| format!("cannot load concurrent writer: {e}"))?;
                    if !change_wrote_branch_vertex_for_branch(&writer, path, *branch_id, *row_node)
                    {
                        return Err(format!(
                            "branch {branch_id:?} was deleted by the closure but the staged \
                             row is alive bound to {row_node:?}; no applied out-of-closure \
                             change recorded that binding for '{path}', so the restore is \
                             corruption"
                        ));
                    }
                    consumer_domain.insert(*branch_id, ConsumerLine::Writer(*row_node));
                }
                concurrent_state = true;
            }
            // A closure-deleted line whose staged row is also Deleted is the
            // closure's own tombstone: the consumer skips it.
        }

        // Token linkage coverage for the closure's own branches (review C1):
        // every leaf linked to a closure-domain branch must be an expected
        // alive token (row intact) or an expected dead token (a Modify's
        // tombstoned predecessor); an alive extra token or a missing/foreign
        // row is corruption. Leaves whose rows are dead and whose ids the
        // replay does not know (tokens written before the closure, tombstoned
        // by an ambient-binding Modify) are stale predecessor rows, not
        // corruption. Leaf ownership/linkage flips — leaves re-linked under a
        // nonexistent branch, or rows claiming a foreign branch — fail here.
        for (branch_id, expected) in &branches {
            if !expected.state.is_alive() {
                continue;
            }
            let branch_key = encode_branch_id(branch_id);
            // Review F1: the LINKAGE audit runs for EVERY closure branch,
            // concurrently owned ones included. The historical skip treated a
            // concurrent writer's token domain as entirely its own
            // responsibility, which let any live linked leaf under a
            // concurrent-owned branch escape validation. The attribution
            // checks below already validate out-of-closure leaves by the
            // writer's actual operations (`change_wrote_leaf`) and attribute
            // them as genuine concurrent state, while an in-closure or
            // unattributable live leaf still fails closed.
            let expected_alive: std::collections::HashSet<[u8; 12]> = expected
                .leaves
                .iter()
                .map(|leaf| atomic_core::crdt::tables::encode_leaf_id(&leaf.id))
                .collect();
            let expected_dead: std::collections::HashSet<[u8; 12]> = expected
                .dead_leaves
                .iter()
                .map(atomic_core::crdt::tables::encode_leaf_id)
                .collect();
            let linked: Vec<[u8; 12]> = txn
                .iter_branch_leaves(&branch_key)
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            for key in linked {
                if expected_alive.contains(&key) || expected_dead.contains(&key) {
                    continue;
                }
                let row = txn
                    .get_crdt_leaf(&key)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| {
                        "the branch links a leaf whose LEAVES row is missing; token \
                         bindings are corrupt"
                            .to_string()
                    })?;
                if row.state.is_alive() {
                    let leaf_id = atomic_core::crdt::LeafId::from_bytes(&key);
                    // Review D2: a registered NodeId is NOT proof of a
                    // writer. The out-of-closure tolerance requires a
                    // loadable, APPLIED change whose own FileOps key this
                    // exact leaf id for this path and branch (the apply's
                    // per-apply counter replay); registering a bare hash
                    // with no saved or applied change and linking a live
                    // token under it is corruption (fixture
                    // cb9b-current-extra-leaf).
                    let writer_registered = !leaf_id.change_id().is_root()
                        && txn
                            .get_external(leaf_id.change_id())
                            .map_err(|e| e.to_string())?
                            .is_some();
                    let writer_applied = !leaf_id.change_id().is_root()
                        && txn
                            .has_change_in_graph(leaf_id.change_id())
                            .map_err(|e| e.to_string())?;
                    let attributed = writer_registered
                        && writer_applied
                        && !visibility.contains(leaf_id.change_id())
                        && {
                            let writer_hash = txn
                                .get_external(leaf_id.change_id())
                                .map_err(|e| e.to_string())?
                                .ok_or_else(|| "leaf writer has no external hash".to_string())?;
                            let writer = self
                                .load_change(&writer_hash)
                                .map_err(|e| format!("cannot load leaf writer: {e}"))?;
                            change_wrote_leaf(&writer, path, *branch_id, leaf_id)
                        };
                    if !attributed {
                        return Err(format!(
                            "leaf {leaf_id:?} is linked to branch {branch_id:?} but the \
                             closure neither created nor tombstoned it and no applied \
                             out-of-closure change recorded it; token linkage corruption"
                        ));
                    }
                    // An out-of-closure applier's token: validated by its
                    // actual operations (review D2), preserved.
                    concurrent_state = true;
                }
            }
            // Expected-dead rows must not be alive: the closure tombstoned
            // them and no out-of-closure applier can resurrect another
            // applier's leaf keys. Enforced for every branch — concurrently
            // owned ones included (review E1).
            for dead_id in &expected.dead_leaves {
                let key = atomic_core::crdt::tables::encode_leaf_id(dead_id);
                if let Some(row) = txn.get_crdt_leaf(&key).map_err(|e| e.to_string())? {
                    if row.state.is_alive() {
                        return Err(format!(
                            "leaf {dead_id:?} was tombstoned by the closure's rewrite of \
                             its line but the staged row is alive; token lifecycle \
                             corruption"
                        ));
                    }
                }
            }
        }

        // The closure's semantic replay must regenerate the commit tree:
        // its alive lines aggregate - joined in the staged after-chain's
        // file order - to exactly the expected bytes. The CONTENT comes
        // from the closure replay (each line's own writer blob slice); the
        // ORDER comes from the staged chain, so a corrupted after-chain
        // reorders or drops lines and fails closed here (review B2).
        let actual_chain =
            iter_trunk_branches_in_file_order(txn, trunk_key).map_err(|e| e.to_string())?;
        let mut replay_alive_bytes: Vec<u8> = Vec::new();
        let mut visited: std::collections::HashSet<atomic_core::crdt::BranchId> =
            std::collections::HashSet::new();
        for branch_id in &actual_chain {
            visited.insert(*branch_id);
            let Some(expected) = branches.get(branch_id) else {
                // Review D2: a staged after-chain branch the closure never
                // wrote is NOT silently skipped. The historical skip let an
                // unexpected live branch with no vertex into the chain (the
                // consumer walker then fails with OrphanBranch while the
                // graph output stays intact — fixture
                // cb9b-current-extra-branch). The branch must be genuine
                // out-of-closure state attributed by actual operations, or
                // the staged chain is corrupt and the synthesis refuses.
                // Review E1: an unexpected branch with an attributed vertex
                // contributes its writer's own recorded content to the
                // closure-aware consumer domain; an attributed tombstone
                // contributes nothing.
                if let Some(writer_vertex) = self.verify_unexpected_chain_branch(
                    txn,
                    path,
                    trunk_key,
                    *branch_id,
                    visibility,
                    &global_alive,
                )? {
                    consumer_domain.insert(*branch_id, ConsumerLine::Writer(writer_vertex));
                    concurrent_state = true;
                }
                continue;
            };
            if expected.state.is_alive() {
                replay_alive_bytes.extend_from_slice(&expected.bytes);
            }
        }
        if replay_alive_bytes.as_slice() != expected_bytes {
            return Err(format!(
                "the closure's semantic operations render {} byte(s) for the alive lines but \
                     the commit tree expects {} byte(s); the semantic closure must regenerate \
                     the commit tree",
                replay_alive_bytes.len(),
                expected_bytes.len()
            ));
        }
        for branch_id in &replay_alive_ids {
            if !visited.contains(branch_id) {
                return Err(
                    "a branch written by this closure is unreachable in the staged after-chain; \
                     the file order is corrupt"
                        .to_string(),
                );
            }
        }

        // Review D2/E1: the ACTUAL CRDT reconstruction consumers read (the
        // trunk → file-order → BRANCH_VERTEX → change-bytes walker) must
        // regenerate the expected domain — ALWAYS, including when genuine
        // concurrent state was accepted (review E1: concurrency cannot waive
        // consumer semantic equality). The expected domain is closure-aware:
        // the closure replay's own bytes for closure lines bound to their
        // replayed vertices, plus each VALIDATED concurrent writer's own
        // recorded content at exactly the vertex its operations bound, in the
        // staged chain's file order. Every concurrency tolerance above was
        // validated by the writer's actual operations first, so a divergence
        // between this expected domain and the actual walker output is
        // corruption the row checks missed — an orphan branch, an unvalidated
        // chain entry, or a diverged payload — never a tolerated
        // reconstruction.
        let actual = atomic_core::output::crdt::output_file_via_crdt(txn, &self.change_store, path)
            .map_err(|error| {
                format!(
                    "the semantic reconstruction consumers read failed for '{path}': {error}; \
                 the staged CRDT state is corrupt"
                )
            })?;
        let mut expected_consumer_bytes: Vec<u8> = Vec::new();
        for branch_id in &actual_chain {
            match consumer_domain.get(branch_id) {
                Some(ConsumerLine::Closure(bytes)) => {
                    expected_consumer_bytes.extend_from_slice(bytes);
                }
                Some(ConsumerLine::Writer(vertex)) => {
                    let bytes = vertex_content_bytes(self, txn, vertex)?;
                    expected_consumer_bytes.extend_from_slice(&bytes);
                }
                // A validated tombstone: the consumer skips the line.
                None => {}
            }
        }
        if actual.as_slice() != expected_consumer_bytes.as_slice() {
            return Err(format!(
                "the actual semantic reconstruction consumers read renders {} byte(s) but the \
                 closure-aware expected domain renders {} byte(s); the CRDT output must \
                 regenerate the closure plus validated concurrent state exactly",
                actual.len(),
                expected_consumer_bytes.len()
            ));
        }

        // Concurrent-writer instrumentation read: the flag is informational
        // today (set by validated out-of-closure writers; RFC §17).
        let _ = concurrent_state;
        Ok(())
    }

    /// Verify one staged file-order branch that the closure replay never
    /// wrote (review D2). Such a branch is genuine only when it is real,
    /// attributed out-of-closure state: its row exists and belongs to this
    /// trunk, its lifecycle is attributed (alive rows carry a BRANCH_VERTEX
    /// recorded by an applied out-of-closure change's own FileOps; dead rows
    /// were deleted — by the graph with proven branch→vertex file ownership,
    /// or by a recorded semantic op), and no sentinel or unapplied id
    /// masquerades as a writer. Anything else is corruption and the
    /// synthesis refuses — an unexpected branch either leaks into the
    /// consumer output or hides a corrupt chain behind a silent skip.
    /// Returns the validated writer vertex for an alive branch (its recorded
    /// content joins the closure-aware consumer domain, review E1) or `None`
    /// for an attributed tombstone.
    fn verify_unexpected_chain_branch(
        &self,
        txn: &atomic_core::pristine::WriteTxn<'_>,
        path: &str,
        trunk_key: atomic_core::crdt::TrunkId,
        branch: atomic_core::crdt::BranchId,
        visibility: &atomic_core::pristine::GraphVisibilityClosure,
        global_alive: &std::collections::HashSet<
            atomic_core::types::GraphNode<atomic_core::types::NodeId>,
        >,
    ) -> Result<Option<atomic_core::types::GraphNode<atomic_core::types::NodeId>>, String> {
        use atomic_core::crdt::tables::encode_branch_id;
        use atomic_core::pristine::{CrdtTxnT, GraphTxnT};
        let branch_key = encode_branch_id(&branch);
        let row = txn
            .get_crdt_branch(&branch_key)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "branch {branch:?} is in the staged file-order chain but its BRANCHES \
                     row is missing; the semantic chain is corrupt"
                )
            })?;
        if row.trunk_id != trunk_key {
            return Err(format!(
                "branch {branch:?} appears in the staged file-order chain of '{path}' but \
                 its row claims a different trunk; the semantic chain is corrupt"
            ));
        }
        let loader = |hash: &Hash| -> Result<Change, String> {
            self.load_change(hash).map_err(|e| e.to_string())
        };
        if !row.state.is_alive() {
            // Genuine tombstones are attributed either by the global graph
            // (a real, applied vertex that is globally dead AND owned by
            // this file's branch — review E1: a different file's existing
            // vertex is not deletion evidence), by an out-of-closure
            // writer's recorded semantic Delete / Modify op, or by an
            // in-closure whole-file replace that tombstoned the foreign
            // branch (review D2: the replace legitimately tombstones every
            // bound branch, foreign ones included).
            let attributed = match txn
                .get_crdt_branch_vertex(&branch_key)
                .map_err(|e| e.to_string())?
                .as_ref()
            {
                Some(node) => {
                    graph_proves_dead_with_ownership(self, txn, path, branch, node, global_alive)?
                }
                None => false,
            } || out_of_closure_writer_deleted_branch(
                &loader, txn, path, branch, visibility,
            )? || applied_writer_deleted_branch(&loader, txn, path, branch)?;
            if !attributed {
                return Err(
                    "the staged file-order chain contains a branch the closure never wrote \
                     and no applied change deleted it: an unattributed tombstone is corruption"
                        .to_string(),
                );
            }
            return Ok(None);
        }
        let Some(row_node) = txn
            .get_crdt_branch_vertex(&branch_key)
            .map_err(|e| e.to_string())?
        else {
            return Err(format!(
                "the staged file-order chain contains an alive branch {branch:?} with no \
                 BRANCH_VERTEX; the semantic reconstruction consumers read fails with \
                 OrphanBranch for it, so the chain is corrupt"
            ));
        };
        let writer_registered = !row_node.change.is_root()
            && txn
                .get_external(row_node.change)
                .map_err(|e| e.to_string())?
                .is_some();
        let writer_applied = !row_node.change.is_root()
            && txn
                .has_change_in_graph(row_node.change)
                .map_err(|e| e.to_string())?;
        if !(writer_registered && writer_applied) || visibility.contains(row_node.change) {
            return Err(format!(
                "branch {branch:?} claims vertex {row_node:?} but the closure neither \
                 wrote it nor attributes it to an applied out-of-closure change; the \
                 staged chain is corrupt"
            ));
        }
        let writer_hash = txn
            .get_external(row_node.change)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "branch vertex writer has no external hash".to_string())?;
        let writer = self
            .load_change(&writer_hash)
            .map_err(|e| format!("cannot load concurrent writer: {e}"))?;
        // Review E1: exact branch→vertex file ownership — the writer's own
        // FileOps must record THIS branch id bound to THIS vertex for this
        // path; a different file's vertex at a matching range is not this
        // branch's content.
        if !change_wrote_branch_vertex_for_branch(&writer, path, branch, row_node) {
            return Err(format!(
                "branch {branch:?} is bound to {row_node:?} but no applied out-of-closure \
                 change recorded that binding for '{path}'; the binding is corruption"
            ));
        }
        Ok(Some(row_node))
    }
}
