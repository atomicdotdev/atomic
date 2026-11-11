//! Private, bounded publication for a direct Git producer. This
//! module owns transactions, not graph algorithms or working-copy recording.

use super::operation::{current_operation_timestamp_ms, working_copy_state_ref};
use super::synthesis::{finalize_classified_change, git_oid_hex, SynthesisClassification};
use super::{
    ConversionPolicy, GitSynthesisOrigin, ProjectTree, Repository, RepositoryError,
    StagedExpectation, StagedPathExpectation, StagedTreeExpectation,
    VerifiedProspectiveEquivalence,
};
use atomic_core::change::{Change, GraphOp};
use atomic_core::operation::{
    ActorRef, MetadataTarget, MetadataTransition, MetadataValue, OperationKind, RepoStateRef,
    ViewStateRef,
};
use atomic_core::pristine::{
    GitShaIndexMutTxnT, GitShaIndexTxnT, GraphTxnT, MutTxnT, ViewScope, ViewTxnT,
};
use atomic_core::types::{Hash, Merkle};
use std::collections::{BTreeMap, HashSet};

fn invalid(message: impl Into<String>) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: message.into(),
    }
}

fn verify_incremental_baseline(
    repo: &Repository,
    view: &str,
    policy: &ConversionPolicy,
    expected: &atomic_core::operation::GitObjectId,
) -> Result<BTreeMap<String, StagedPathExpectation>, RepositoryError> {
    let project = repo
        .project_tree(view, policy)
        .map_err(|error| invalid(format!("incremental baseline projection failed: {error}")))?;
    super::verify_prospective_equivalence(&project, expected)
        .map_err(|error| invalid(format!("incremental baseline tree mismatch: {error}")))?;
    let mut retained = BTreeMap::new();
    for entry in &project.manifest.entries {
        if entry.disposition == super::ManifestDisposition::Included {
            continue;
        }
        let path = match std::str::from_utf8(entry.path.as_bytes()) {
            Ok(path) => crate::canonical_path_identity(path),
            Err(_) => crate::escape_repo_path(entry.path.as_bytes()),
        };
        if retained
            .insert(
                path,
                StagedPathExpectation {
                    bytes: entry.repository_bytes.to_vec(),
                    mode: entry.mode,
                    kind: entry.kind,
                },
            )
            .is_some()
        {
            return Err(invalid(
                "incremental baseline has duplicate private path identities",
            ));
        }
    }
    Ok(retained)
}

fn is_atomicignore_bootstrap(change: &Change) -> bool {
    use atomic_core::change::span_patch::SpanOp;
    if !change.origin().is_native() || !change.dependencies().is_empty() {
        return false;
    }
    let Some(patch) = &change.hashed.span else {
        return false;
    };
    if !matches!(patch.ops.as_slice(), [SpanOp::Root { path, .. }] if path == ".atomicignore") {
        return false;
    }
    let mut creators = change.file_ops().iter().filter(|ops| ops.is_create());
    let Some(creator) = creators.next() else {
        return false;
    };
    if creators.next().is_some()
        || creator.path() != ".atomicignore"
        || !creator.trunk_id().change_id().is_root()
    {
        return false;
    }
    for ops in change.file_ops() {
        if ops.path() != ".atomicignore" || ops.trunk_id() != creator.trunk_id() {
            return false;
        }
        match ops.trunk_op() {
            Some(atomic_core::crdt::TrunkOp::Create { path, .. }) if path == ".atomicignore" => {}
            Some(
                atomic_core::crdt::TrunkOp::SetMode { trunk, .. }
                | atomic_core::crdt::TrunkOp::SetKind { trunk, .. },
            ) if *trunk == creator.trunk_id() && ops.line_ops().is_empty() => {}
            _ => return false,
        }
    }
    let mut inode = None;
    for hunk in change.hunks() {
        match hunk {
            GraphOp::FileAdd {
                path,
                add_inode,
                contents: None,
                ..
            } if path == ".atomicignore" && inode.is_none() => inode = Some(add_inode.start),
            GraphOp::SetAttr {
                path,
                inode: attr_inode,
                ..
            } if path == ".atomicignore"
                && attr_inode.change.is_none()
                && Some(attr_inode.pos) == inode => {}
            _ => return false,
        }
    }
    inode.is_some()
}

fn verified_incremental_stage_batches(
    repo: &Repository,
    working_copy: atomic_core::WorkingCopyId,
    stage: &str,
    base_len: usize,
    staged: &[(String, Hash)],
) -> Result<usize, RepositoryError> {
    use super::OperationVerificationState;
    use atomic_core::operation::OperationScope;
    let working = repo.operation_log(OperationScope::WorkingCopy(working_copy), None, false)?;
    let global = repo.operation_log(OperationScope::Repository, None, false)?;
    let global_ids: HashSet<_> = global
        .entries
        .iter()
        .map(|entry| entry.operation.id())
        .collect();
    let mut batches = HashSet::new();
    for (index, (_, hash)) in staged.iter().enumerate() {
        let sequence = u64::try_from(
            base_len
                .checked_add(index)
                .ok_or_else(|| invalid("stage sequence overflow"))?,
        )
        .map_err(|_| invalid("stage sequence overflow"))?;
        let entry = working.entries.iter().find(|entry| {
            let payload = entry.operation.payload();
            entry.verification == OperationVerificationState::Verified
                && global_ids.contains(&entry.operation.id())
                && payload.kind == OperationKind::SynthesizeGit
                && matches!(&payload.actor, ActorRef::System { name } if name == "git-direct-stream-private")
                && payload.evidence.contains(hash)
                && payload.delta.metadata.iter().any(|transition| {
                    matches!(&transition.target, MetadataTarget::ViewChange { view, change } if view == stage && change == hash)
                        && transition.expected_old == MetadataValue::Absent
                        && transition.expected_new == MetadataValue::Sequence(sequence)
                })
        }).ok_or_else(|| invalid(format!("staged change {hash} lacks a verified private-batch operation")))?;
        batches.insert(entry.operation.id());
    }
    Ok(batches.len())
}

fn recovered_private_stage_change(
    working: &super::OperationLog,
    global: &super::OperationLog,
    stage: &str,
    hash: &Hash,
    sequence: u64,
) -> bool {
    use super::OperationVerificationState;
    let global_ids: HashSet<_> = global
        .entries
        .iter()
        .map(|entry| entry.operation.id())
        .collect();
    working.entries.iter().any(|interrupted| {
        let original = interrupted.operation.payload();
        if interrupted.verification == OperationVerificationState::Verified
            || !global_ids.contains(&interrupted.operation.id())
            || original.kind != OperationKind::SynthesizeGit
            || !matches!(&original.actor, ActorRef::System { name } if name == "git-direct-stream-private")
            || !original.evidence.contains(hash)
            || !original.delta.metadata.iter().any(|transition| {
                matches!(&transition.target, MetadataTarget::ViewChange { view, change } if view == stage && change == hash)
                    && transition.expected_old == MetadataValue::Absent
                    && transition.expected_new == MetadataValue::Sequence(sequence)
            })
        {
            return false;
        }
        working.entries.iter().any(|recovery| {
            let inverse = recovery.operation.payload();
            recovery.verification == OperationVerificationState::Verified
                && global_ids.contains(&recovery.operation.id())
                && inverse.kind == OperationKind::Recover
                && inverse.parents.contains(&interrupted.operation.id())
                && inverse.before == original.delta.after
                && inverse.delta.after == original.before
                && inverse.delta.metadata.iter().any(|transition| {
                    matches!(&transition.target, MetadataTarget::ViewChange { view, change } if view == stage && change == hash)
                        && transition.expected_old == MetadataValue::Sequence(sequence)
                        && transition.expected_new == MetadataValue::Absent
                })
        })
    })
}

impl Repository {
    /// Validate a replayed first-parent span prefix against the persisted view.
    ///
    /// This verifies content identities, Git origins, indexed dependencies and
    /// exact persisted interpretation closures. It does not verify the Git
    /// tree, working-copy cleanliness, or hold a lease after returning. An
    /// importer must check those separately and revalidate under its operation
    /// locks before it publishes any changes.
    pub fn validate_span_git_prefix(
        &self,
        view_name: &str,
        replayed: &[(String, Hash)],
    ) -> Result<(), RepositoryError> {
        use atomic_core::change::ChangeOrigin;
        use atomic_core::pristine::{
            CapabilityTxnT, GitCommitClosureTxnT, GitShaIndexTxnT, SPAN_RANGE_CAPABILITY,
        };

        if replayed.is_empty() {
            return Err(invalid("span Git prefix must not be empty"));
        }
        let txn = self.pristine.read_txn()?;
        if txn.required_capability_version(SPAN_RANGE_CAPABILITY.id())?
            != Some(SPAN_RANGE_CAPABILITY.minimum_version())
        {
            return Err(invalid("span capability is missing or unsupported"));
        }
        let view = txn
            .get_view(view_name)?
            .ok_or_else(|| invalid("span Git prefix view does not exist"))?;
        let visibility = super::filter::graph_visibility_closure(&txn, &view)?;
        let mut hashes = Vec::with_capacity(replayed.len());
        let mut previous: Option<&str> = None;
        let mut seen = HashSet::new();
        for (sha, expected) in replayed {
            if !seen.insert(*expected) {
                return Err(invalid("span Git prefix contains a duplicate change"));
            }
            let id = txn
                .get_by_git_sha(sha)?
                .ok_or_else(|| invalid(format!("Git prefix commit {sha} is not indexed")))?;
            if !visibility.contains(id)
                || txn.get_external(id)? != Some(*expected)
                || !txn.was_change_applied(id)?
            {
                return Err(invalid(format!(
                    "Git prefix commit {sha} differs from the selected view"
                )));
            }
            let change = self.load_change(expected)?;
            if change.hash().map_err(|error| invalid(error.to_string()))? != *expected
                || change.hashed.span.is_none()
                || !change.file_ops().is_empty()
            {
                return Err(invalid(format!(
                    "Git prefix commit {sha} is not the expected graph-only span change"
                )));
            }
            match change.origin() {
                ChangeOrigin::GitSynthesized {
                    commit, parents, ..
                } if git_oid_hex(commit) == *sha
                    && parents.first().map(git_oid_hex).as_deref() == previous => {}
                _ => {
                    return Err(invalid(format!(
                        "Git prefix commit {sha} has a different first-parent origin"
                    )))
                }
            }
            let mut indexed = txn.get_change_deps(id)?;
            let mut declared = change.dependencies().to_vec();
            indexed.sort();
            declared.sort();
            if indexed != declared {
                return Err(invalid(format!(
                    "Git prefix commit {sha} has inconsistent dependencies"
                )));
            }
            hashes.push(*expected);
            let mut expected_closure = hashes.clone();
            expected_closure.sort();
            expected_closure.dedup();
            if txn.get_git_commit_closure(sha)? != Some(expected_closure) {
                return Err(invalid(format!(
                    "Git prefix commit {sha} has a different interpretation closure"
                )));
            }
            previous = Some(sha);
        }
        Ok(())
    }
}

/// Bounds compiled transaction payload, not total current-tree/compiler memory.
#[derive(Clone, Copy, Debug)]
pub struct GitStreamLimits {
    commits: usize,
    bytes: usize,
}
impl GitStreamLimits {
    pub fn new(commits: usize, bytes: usize) -> Result<Self, RepositoryError> {
        if !(1..=256).contains(&commits) || !(1..=256 * 1024 * 1024).contains(&bytes) {
            return Err(invalid(
                "stream limits require 1..=256 commits and 1..=256 MiB",
            ));
        }
        Ok(Self { commits, bytes })
    }
    pub fn commits(self) -> usize {
        self.commits
    }
    pub fn bytes(self) -> usize {
        self.bytes
    }
}

/// Finalized once by the producer; subsequent cache anchors use this exact hash.
pub struct PreparedGitGraphChange {
    change: Change,
    bytes: Vec<u8>,
    hash: Hash,
    sha: String,
    parent: Option<String>,
    deleted: Vec<String>,
}
impl PreparedGitGraphChange {
    pub fn new(
        change: Change,
        origin: GitSynthesisOrigin,
        metadata: Vec<u8>,
        unhashed: serde_json::Value,
    ) -> Result<Self, RepositoryError> {
        Self::prepare(change, origin, metadata, unhashed, false)
    }

    /// Prepare a typed span change while preserving its opaque Git facts.
    pub fn new_span(
        change: Change,
        origin: GitSynthesisOrigin,
        metadata: Vec<u8>,
        unhashed: serde_json::Value,
    ) -> Result<Self, RepositoryError> {
        Self::prepare(change, origin, metadata, unhashed, true)
    }

    fn prepare(
        change: Change,
        origin: GitSynthesisOrigin,
        metadata: Vec<u8>,
        mut unhashed: serde_json::Value,
        span_mode: bool,
    ) -> Result<Self, RepositoryError> {
        if change.hashed.span.is_some() != span_mode {
            return Err(invalid("stream change format does not match producer mode"));
        }
        if span_mode {
            let patch = change.hashed.span.as_ref().expect("span mode checked");
            if !patch.prior_metadata.is_empty() || !change.hashed.metadata.is_empty() {
                return Err(invalid(
                    "span compiler must not prepopulate foreign metadata",
                ));
            }
        } else if change.hashed.metadata.starts_with(b"ATSP") {
            return Err(invalid(
                "opaque span-prefixed metadata is not accepted by the graph stream",
            ));
        }
        if !change.file_ops().is_empty() {
            return Err(invalid("direct graph stream does not import native CRDT identities; eager semantics are unsupported"));
        }
        let sha = git_oid_hex(&origin.commit);
        let git = unhashed
            .get_mut("git")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| invalid("prepared stream metadata needs the standard Git namespace"))?;
        if git
            .get("sha")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value != sha)
        {
            return Err(invalid(
                "stream metadata SHA differs from its hashed Git origin",
            ));
        }
        git.insert("sha".into(), serde_json::Value::String(sha.clone()));
        let parent = origin.parents.first().map(git_oid_hex);
        let deleted = change
            .hunks()
            .iter()
            .filter_map(|op| match op {
                GraphOp::FileDel { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        let (change, bytes, hash) = finalize_classified_change(
            change,
            &SynthesisClassification::Synthesized(origin),
            metadata,
            unhashed,
        )?;
        Ok(Self {
            change,
            bytes,
            hash,
            sha,
            parent,
            deleted,
        })
    }
    pub fn hash(&self) -> Hash {
        self.hash
    }
    pub fn payload_bytes(&self) -> usize {
        self.bytes.len()
    }
}

#[derive(Debug)]
pub struct GitStreamOutcome {
    pub commits: usize,
    pub batches: usize,
    pub state: Merkle,
    pub private_publish_seconds: f64,
    pub final_proof_promotion_seconds: f64,
}

/// Initially constrained to an empty repository/view. No content is visible in
/// the current view until `finish` proves and commits its complete final state.
/// Failure retains the private stage for inspection; retry requires a new target.
pub struct GitGraphImportSession<'a> {
    repo: &'a Repository,
    stage: String,
    target: String,
    limits: GitStreamLimits,
    target_id: u64,
    target_state: Merkle,
    target_count: u64,
    baseline_git_tree: Option<(ConversionPolicy, atomic_core::operation::GitObjectId)>,
    retained_private: BTreeMap<String, StagedPathExpectation>,
    base_prefix: Vec<(String, Hash)>,
    base_hashes: Vec<Hash>,
    hashes: Vec<Hash>,
    last_sha: Option<String>,
    last_hash: Option<Hash>,
    batches: usize,
    publish_seconds: f64,
    span_mode: bool,
}

#[cfg(all(test, unix))]
fn pause_incremental_batch_for_test(
    operation: atomic_core::OperationId,
    entries: &[PreparedGitGraphChange],
    window: &str,
) {
    use atomic_core::types::Base32;
    use std::io::Write;
    if std::env::var("ATOMIC_TEST_INCREMENTAL_WINDOW")
        .ok()
        .as_deref()
        != Some(window)
        || std::env::var("ATOMIC_TEST_INCREMENTAL_SHA").ok().as_deref()
            != entries.first().map(|entry| entry.sha.as_str())
    {
        return;
    }
    let hashes = entries
        .iter()
        .map(|entry| entry.hash.to_base32())
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "ATOMIC_INCREMENTAL_READY {} {}",
        operation.to_base32(),
        hashes
    );
    std::io::stdout().flush().expect("flush crash marker");
    loop {
        std::thread::park();
    }
}
impl<'a> GitGraphImportSession<'a> {
    pub fn begin(
        repo: &'a mut Repository,
        limits: GitStreamLimits,
    ) -> Result<Self, RepositoryError> {
        let target = repo.current_view().to_string();
        if !repo.registered_change_hashes()?.is_empty() {
            return Err(invalid("direct stream requires a fresh repository; existing/native history is not supported"));
        }
        let view = repo
            .pristine
            .read_txn()?
            .get_view(&target)?
            .ok_or_else(|| invalid("missing target view"))?;
        if view.change_count != 0 || view.state != Merkle::ZERO {
            return Err(invalid("stream target must be empty"));
        }
        let stage = "git-stream-private".to_string();
        repo.pristine
            .require_repository_capability(super::CHANGE_FORMAT_VNEXT_CAPABILITY)?;
        repo.create_draft_view(&stage, &target)?;
        Ok(Self {
            repo,
            stage,
            target,
            limits,
            target_id: view.id,
            target_state: view.state,
            target_count: view.change_count,
            baseline_git_tree: None,
            retained_private: BTreeMap::new(),
            base_prefix: Vec::new(),
            base_hashes: Vec::new(),
            hashes: Vec::new(),
            last_sha: None,
            last_hash: None,
            batches: 0,
            publish_seconds: 0.0,
            span_mode: false,
        })
    }

    /// Begin span-backed streaming into a fresh, untracked repository.
    pub fn begin_span(
        repo: &'a mut Repository,
        limits: GitStreamLimits,
    ) -> Result<Self, RepositoryError> {
        use atomic_core::pristine::TreeTxnT;
        let working_copy = repo.require_working_copy_id()?;
        let _lock = repo.try_lock_operation(working_copy)?;
        {
            let txn = repo.pristine.read_txn()?;
            let first = txn.iter_tree()?.next().transpose()?;
            if let Some((path, _)) = first {
                return Err(invalid(format!(
                    "span stream requires a fresh repository without tracked or staged paths; found '{path}'"
                )));
            }
        }
        let mut session = Self::begin(repo, limits)?;
        session
            .repo
            .pristine
            .require_repository_capability(atomic_core::pristine::SPAN_RANGE_CAPABILITY)?;
        session.span_mode = true;
        Ok(session)
    }

    /// Continue a direct span stream over an unchanged imported prefix,
    /// optionally retaining its native `.atomicignore` bootstrap change.
    ///
    /// The original private stage is reused, not recreated. This deliberately
    /// refuses other native changes, inherited members, or a partial prior
    /// stage. The supplied baseline tree must be obtained from the pinned Git
    /// commit; the repository projection is checked against it under the
    /// operation lock. This method does not change or verify working bytes.
    /// The CLI does not yet expose this restricted path.
    pub fn begin_span_incremental(
        repo: &'a mut Repository,
        limits: GitStreamLimits,
        replayed: &[(String, Hash)],
        policy: &ConversionPolicy,
        expected_git_tree: &atomic_core::operation::GitObjectId,
    ) -> Result<Self, RepositoryError> {
        Self::begin_span_incremental_inner(repo, limits, replayed, &[], policy, expected_git_tree)
    }

    /// Resume a previously verified private stage using the exact replayed
    /// staged suffix. No unverified batch or mismatched SHA is reused.
    pub fn resume_span_incremental(
        repo: &'a mut Repository,
        limits: GitStreamLimits,
        replayed: &[(String, Hash)],
        staged: &[(String, Hash)],
        policy: &ConversionPolicy,
        expected_git_tree: &atomic_core::operation::GitObjectId,
    ) -> Result<Self, RepositoryError> {
        if staged.is_empty() {
            return Err(invalid(
                "resuming an incremental span stage requires staged changes",
            ));
        }
        Self::begin_span_incremental_inner(
            repo,
            limits,
            replayed,
            staged,
            policy,
            expected_git_tree,
        )
    }

    fn begin_span_incremental_inner(
        repo: &'a mut Repository,
        limits: GitStreamLimits,
        replayed: &[(String, Hash)],
        staged: &[(String, Hash)],
        policy: &ConversionPolicy,
        expected_git_tree: &atomic_core::operation::GitObjectId,
    ) -> Result<Self, RepositoryError> {
        let working_copy = repo.require_working_copy_id()?;
        let lock = repo.try_lock_operation(working_copy)?;
        let target_name = repo.current_view().to_string();
        repo.validate_span_git_prefix(&target_name, replayed)?;
        let stage_name = "git-stream-private".to_string();
        let txn = repo.pristine.read_txn()?;
        let target = txn
            .get_view(&target_name)?
            .ok_or_else(|| invalid("incremental stream target missing"))?;
        let stage = txn
            .get_view(&stage_name)?
            .ok_or_else(|| invalid("incremental stream stage missing"))?;
        let base_hashes: Vec<_> = replayed.iter().map(|(_, hash)| *hash).collect();
        let members =
            |view: &atomic_core::pristine::ViewState| -> Result<Vec<Hash>, RepositoryError> {
                txn.iter_changes(view, 0)?
                    .map(|row| {
                        let (_, id, _) = row?;
                        txn.get_external(id)?
                            .ok_or_else(|| invalid("stream member has no hash"))
                    })
                    .collect()
            };
        let staged_hashes: Vec<_> = staged.iter().map(|(_, hash)| *hash).collect();
        let expected_stage_members: Vec<_> =
            base_hashes.iter().chain(&staged_hashes).copied().collect();
        let expected_state = expected_stage_members
            .iter()
            .fold(Merkle::ZERO, |state, hash| state.next(hash));
        let visibility = super::filter::graph_visibility_closure(&txn, &target)?;
        let target_members = members(&target)?;
        let mut expected_index = 0usize;
        let mut bootstrap_count = 0usize;
        for hash in &target_members {
            if base_hashes.get(expected_index) == Some(hash) {
                expected_index += 1;
            } else {
                let change = repo.load_change(hash)?;
                if change.hash().map_err(|error| invalid(error.to_string()))? != *hash
                    || !is_atomicignore_bootstrap(&change)
                {
                    return Err(invalid("incremental stream requires an unchanged Git-only target plus optional .atomicignore bootstrap"));
                }
                bootstrap_count += 1;
            }
        }
        if stage.parent != Some(target.id)
            || stage.kind != ViewScope::Draft
            || members(&stage)? != expected_stage_members
            || expected_index != base_hashes.len()
            || bootstrap_count > 1
            || stage.state != expected_state
            || visibility
                .iter_dependency_first()
                .filter(|id| !id.is_root())
                .count()
                != target_members.len()
        {
            return Err(invalid(
                "incremental stream requires an unchanged Git-only target and private stage",
            ));
        }
        drop(txn);
        let resumed_batches = if staged.is_empty() {
            0
        } else {
            let mut all = replayed.to_vec();
            all.extend_from_slice(staged);
            repo.validate_span_git_prefix(&stage_name, &all)?;
            verified_incremental_stage_batches(
                repo,
                working_copy,
                &stage_name,
                base_hashes.len(),
                staged,
            )?
        };
        let working = repo.working_copy_record(working_copy)?;
        if working.desired_view != target.id || working.desired_state != target.state {
            return Err(invalid(
                "working copy does not match the incremental stream target",
            ));
        }
        let retained_private =
            verify_incremental_baseline(repo, &target_name, policy, expected_git_tree)?;
        if retained_private.keys().any(|path| path != ".atomicignore") {
            return Err(invalid(
                "incremental stream cannot preserve additional private paths yet",
            ));
        }
        let (sha, hash) = staged
            .last()
            .or_else(|| replayed.last())
            .ok_or_else(|| invalid("span Git prefix must not be empty"))?;
        let last_sha = Some(sha.clone());
        let last_hash = Some(*hash);
        drop(lock);
        Ok(Self {
            repo,
            stage: stage_name,
            target: target_name,
            limits,
            target_id: target.id,
            target_state: target.state,
            target_count: target.change_count,
            baseline_git_tree: Some((policy.clone(), expected_git_tree.clone())),
            retained_private,
            base_prefix: replayed.to_vec(),
            base_hashes,
            hashes: staged_hashes,
            last_sha,
            last_hash,
            batches: resumed_batches,
            publish_seconds: 0.0,
            span_mode: true,
        })
    }

    /// One prepared journal operation and one graph transaction per batch.
    pub fn publish(&mut self, entries: &[PreparedGitGraphChange]) -> Result<(), RepositoryError> {
        let started = std::time::Instant::now();
        let size = entries
            .iter()
            .try_fold(0usize, |sum, entry| sum.checked_add(entry.bytes.len()));
        if entries.is_empty()
            || entries.len() > self.limits.commits
            || size.is_none_or(|bytes| bytes > self.limits.bytes)
        {
            return Err(invalid(
                "stream batch exceeds its count/byte limit or is empty",
            ));
        }
        let mut previous = self.last_sha.as_deref();
        let mut unique = HashSet::new();
        for entry in entries {
            if entry.change.hashed.span.is_some() != self.span_mode {
                return Err(invalid("stream change format does not match session mode"));
            }
            if entry.parent.as_deref() != previous || !unique.insert(&entry.sha) {
                return Err(invalid(
                    "stream batch must follow the exact first-parent chain",
                ));
            }
            previous = Some(&entry.sha);
        }
        let wc = self.repo.require_working_copy_id()?;
        let lock = self.repo.try_lock_operation(wc)?;
        if !self.base_prefix.is_empty() {
            self.repo
                .validate_span_git_prefix(&self.target, &self.base_prefix)?;
            let (policy, expected) = self
                .baseline_git_tree
                .as_ref()
                .expect("incremental baseline");
            if verify_incremental_baseline(self.repo, &self.target, policy, expected)?
                != self.retained_private
            {
                return Err(invalid("incremental private baseline changed"));
            }
        }
        let txn = self.repo.pristine.read_txn()?;
        let stage = txn
            .get_view(&self.stage)?
            .ok_or_else(|| invalid("private stream view disappeared"))?;
        let target = txn
            .get_view(&self.target)?
            .ok_or_else(|| invalid("target disappeared"))?;
        let expected_stage = self
            .base_hashes
            .iter()
            .chain(&self.hashes)
            .fold(Merkle::ZERO, |state, hash| state.next(hash));
        if target.id != self.target_id
            || target.change_count != self.target_count
            || target.state != self.target_state
            || stage.parent != Some(target.id)
            || stage.kind != ViewScope::Draft
            || stage.change_count as usize != self.base_hashes.len() + self.hashes.len()
            || stage.state != expected_stage
        {
            return Err(invalid("stream target/stage changed; refusing publication"));
        }
        if !self.base_prefix.is_empty() {
            use atomic_core::operation::OperationScope;
            use atomic_core::pristine::{
                span_patch_storage::SpanPatchReadTxn, GitCommitClosureTxnT,
            };
            let mut closure: Vec<_> = self
                .base_hashes
                .iter()
                .chain(&self.hashes)
                .copied()
                .collect();
            let mut logs = None;
            for (index, entry) in entries.iter().enumerate() {
                closure.push(entry.hash);
                let sha_id = txn.get_by_git_sha(&entry.sha)?;
                let hash_id = txn.get_internal(&entry.hash)?;
                if sha_id.is_none() && hash_id.is_none() {
                    continue;
                }
                let valid_identity = match (sha_id, hash_id) {
                    (Some(sha_id), Some(hash_id)) if sha_id == hash_id => {
                        let mut expected_closure = closure.clone();
                        expected_closure.sort();
                        expected_closure.dedup();
                        txn.get_external(hash_id)? == Some(entry.hash)
                            && txn.was_change_applied(hash_id)?
                            && txn
                                .span_change_indexed(hash_id)
                                .map_err(|error| invalid(error.to_string()))?
                            && txn.get_change_seq(&stage, hash_id)?.is_none()
                            && txn.get_change_seq(&target, hash_id)?.is_none()
                            && txn.get_git_commit_closure(&entry.sha)? == Some(expected_closure)
                            && self
                                .repo
                                .load_change(&entry.hash)?
                                .hash()
                                .map_err(|error| invalid(error.to_string()))?
                                == entry.hash
                    }
                    _ => false,
                };
                if logs.is_none() && valid_identity {
                    logs = Some((
                        self.repo
                            .operation_log(OperationScope::WorkingCopy(wc), None, false)?,
                        self.repo
                            .operation_log(OperationScope::Repository, None, false)?,
                    ));
                }
                let recovered = logs.as_ref().is_some_and(|(working, global)| {
                    recovered_private_stage_change(
                        working,
                        global,
                        &self.stage,
                        &entry.hash,
                        stage.change_count + index as u64,
                    )
                });
                if !valid_identity || !recovered {
                    return Err(invalid(format!(
                        "incremental stream commit {} already has a repository identity",
                        entry.sha
                    )));
                }
            }
        }
        drop(txn);
        let record = self.repo.working_copy_record(wc)?;
        if record.desired_view != target.id || record.desired_state != target.state {
            return Err(invalid(
                "working copy changed during private stream publication",
            ));
        }
        let after = entries
            .iter()
            .fold(stage.state, |state, entry| state.next(&entry.hash));
        let state = |merkle| RepoStateRef {
            view: Some(ViewStateRef {
                name: self.stage.clone(),
                state: merkle,
                set_id: None,
            }),
            working_copy: Some(working_copy_state_ref(record.clone())),
            git: None,
        };
        let operation = self.repo.prepare_metadata_operation(
            &lock,
            OperationKind::SynthesizeGit,
            None,
            state(stage.state),
            state(after),
            entries
                .iter()
                .enumerate()
                .map(|(index, entry)| MetadataTransition {
                    target: MetadataTarget::ViewChange {
                        view: self.stage.clone(),
                        change: entry.hash,
                    },
                    expected_old: MetadataValue::Absent,
                    expected_new: MetadataValue::Sequence(stage.change_count + index as u64),
                })
                .collect(),
            entries.iter().map(|entry| entry.hash).collect(),
            ActorRef::System {
                name: "git-direct-stream-private".into(),
            },
            current_operation_timestamp_ms(),
        )?;
        let result = (|| {
            let mut txn = lock.begin_write_immediate()?;
            let mut closure = self.base_hashes.clone();
            closure.extend_from_slice(&self.hashes);
            for entry in entries {
                self.repo.apply_synthesized_change_in_txn(
                    &mut txn,
                    &self.stage,
                    &entry.hash,
                    &entry.bytes,
                    &entry.change,
                    &entry.deleted,
                    true,
                    &Default::default(),
                    &[],
                    &StagedExpectation::empty(),
                    Some(&entry.sha),
                    &closure,
                    &[],
                )?;
                let id = txn
                    .get_internal(&entry.hash)?
                    .ok_or_else(|| invalid("applied stream change has no id"))?;
                txn.put_git_sha(&entry.sha, id)?;
                closure.push(entry.hash);
                #[cfg(all(test, unix))]
                if !self.base_prefix.is_empty() && entry.sha == entries[0].sha {
                    pause_incremental_batch_for_test(operation.id(), entries, "before-commit");
                }
            }
            txn.commit()
        })();
        #[cfg(all(test, unix))]
        if !self.base_prefix.is_empty() && result.is_ok() {
            pause_incremental_batch_for_test(operation.id(), entries, "after-commit");
        }
        match result {
            Ok(()) => {
                self.repo
                    .apply_operation_metadata_locked(&lock, operation.id())?;
                #[cfg(all(test, unix))]
                if !self.base_prefix.is_empty() {
                    pause_incremental_batch_for_test(operation.id(), entries, "after-metadata");
                }
                self.repo
                    .finalize_operation_verified(&lock, operation.id())?;
                #[cfg(all(test, unix))]
                if !self.base_prefix.is_empty() {
                    pause_incremental_batch_for_test(operation.id(), entries, "after-verified");
                }
            }
            Err(error) => {
                self.repo
                    .abort_prepared_metadata_operation(&lock, &operation)?;
                return Err(error);
            }
        }
        self.hashes.extend(entries.iter().map(|entry| entry.hash));
        self.last_sha = entries.last().map(|entry| entry.sha.clone());
        self.last_hash = entries.last().map(|entry| entry.hash);
        self.batches += 1;
        self.publish_seconds += started.elapsed().as_secs_f64();
        Ok(())
    }

    /// Prove the actual target projection before committing its references.
    /// This certifies the tip only, not every historical intermediate state.
    pub fn finish(
        self,
        project: &ProjectTree,
        proof: &VerifiedProspectiveEquivalence,
    ) -> Result<GitStreamOutcome, RepositoryError> {
        self.finish_with_guard(project, proof, || Ok(()))
    }

    /// Check an external observation while holding the operation lock before
    /// preparing the final promotion. A failed check leaves the staged changes
    /// private and does not start a target-view operation.
    pub fn finish_with_guard<F>(
        self,
        project: &ProjectTree,
        proof: &VerifiedProspectiveEquivalence,
        guard: F,
    ) -> Result<GitStreamOutcome, RepositoryError>
    where
        F: FnOnce() -> Result<(), RepositoryError>,
    {
        let started = std::time::Instant::now();
        if self.hashes.is_empty() {
            return Err(invalid("empty stream has no new commits to promote"));
        }
        if project.git.root != *proof.expected_git_tree()
            || project.manifest.root() != *proof.manifest_root()
        {
            return Err(invalid("stream final manifest/proof mismatch"));
        }
        let last_hash = self
            .last_hash
            .ok_or_else(|| invalid("empty stream has no final tip"))?;
        let last = self.repo.load_change(&last_hash)?;
        let mut expected = BTreeMap::new();
        for entry in &project.manifest.entries {
            if entry.disposition != super::ManifestDisposition::Included {
                continue;
            }
            let path = match std::str::from_utf8(entry.path.as_bytes()) {
                Ok(path) => crate::canonical_path_identity(path),
                Err(_) => crate::escape_repo_path(entry.path.as_bytes()),
            };
            expected.insert(
                path,
                StagedPathExpectation {
                    bytes: entry.repository_bytes.to_vec(),
                    mode: entry.mode,
                    kind: entry.kind,
                },
            );
        }
        for (path, retained) in &self.retained_private {
            if expected.insert(path.clone(), retained.clone()).is_some() {
                return Err(invalid("Git target collides with a retained private path"));
            }
        }
        let expectation = StagedExpectation {
            paths: Vec::new(),
            tree: Some(StagedTreeExpectation {
                entries: expected,
                // Full closure verification below checks every inherited
                // semantic path; an empty Git tip need not invent FileOps.
                semantic_paths: Vec::new(),
                verify_paths: None,
                semantic_verify_paths: None,
            }),
        };
        let wc = self.repo.require_working_copy_id()?;
        let lock = self.repo.try_lock_operation(wc)?;
        guard()?;
        if !self.base_prefix.is_empty() {
            self.repo
                .validate_span_git_prefix(&self.target, &self.base_prefix)?;
            let (policy, expected) = self
                .baseline_git_tree
                .as_ref()
                .expect("incremental baseline");
            if verify_incremental_baseline(self.repo, &self.target, policy, expected)?
                != self.retained_private
            {
                return Err(invalid("incremental private baseline changed"));
            }
        }
        let txn = self.repo.pristine.read_txn()?;
        let mut target = txn
            .get_view(&self.target)?
            .ok_or_else(|| invalid("stream target disappeared"))?;
        let stage = txn
            .get_view(&self.stage)?
            .ok_or_else(|| invalid("stream stage disappeared"))?;
        let staged = txn
            .iter_changes(&stage, 0)?
            .map(|row| {
                let (_, id, _) = row?;
                txn.get_external(id)?
                    .ok_or_else(|| invalid("stage member has no hash"))
            })
            .collect::<Result<Vec<_>, RepositoryError>>()?;
        let expected_staged: Vec<_> = self
            .base_hashes
            .iter()
            .chain(&self.hashes)
            .copied()
            .collect();
        if target.id != self.target_id
            || target.change_count != self.target_count
            || target.state != self.target_state
            || stage.parent != Some(target.id)
            || stage.kind != ViewScope::Draft
            || staged != expected_staged
            || stage.state
                != self
                    .base_hashes
                    .iter()
                    .chain(&self.hashes)
                    .fold(Merkle::ZERO, |state, hash| state.next(hash))
        {
            return Err(invalid("target/stage changed before final promotion"));
        }
        drop(txn);
        let before = self.repo.working_copy_record(wc)?;
        let mut after = before.clone();
        if after.desired_view != target.id || after.desired_state != target.state {
            return Err(invalid("working copy changed during direct stream"));
        }
        let target_after = self
            .hashes
            .iter()
            .fold(target.state, |state, hash| state.next(hash));
        after.desired_state = target_after;
        let state = |record, merkle| RepoStateRef {
            view: Some(ViewStateRef {
                name: self.target.clone(),
                state: merkle,
                set_id: None,
            }),
            working_copy: Some(working_copy_state_ref(record)),
            git: None,
        };
        let operation = self.repo.prepare_metadata_operation(
            &lock,
            OperationKind::SynthesizeGit,
            None,
            state(before, target.state),
            state(after, target_after),
            self.hashes
                .iter()
                .enumerate()
                .map(|(index, hash)| MetadataTransition {
                    target: MetadataTarget::ViewChange {
                        view: self.target.clone(),
                        change: *hash,
                    },
                    expected_old: MetadataValue::Absent,
                    expected_new: MetadataValue::Sequence(target.change_count + index as u64),
                })
                .collect(),
            self.hashes.clone(),
            ActorRef::System {
                name: "git-direct-stream-final-promotion".into(),
            },
            current_operation_timestamp_ms(),
        )?;
        let result = (|| {
            let mut txn = lock.begin_write_immediate()?;
            for hash in &self.hashes {
                let id = txn
                    .get_internal(hash)?
                    .ok_or_else(|| invalid("missing staged id"))?;
                txn.put_change(&mut target, id, hash)?;
            }
            txn.update_view(&target)?;
            self.repo
                .realign_tree_projection_in_txn(&mut txn, &self.target)?;
            self.repo.verify_staged_projection(
                &txn,
                &self.target,
                &last,
                &[],
                &expectation,
                &last_hash,
                true,
            )?;
            // Git omits empty directories: check the complete namespace too.
            let visibility = super::filter::graph_visibility_closure(&*txn, &target)?;
            let projection = self.repo.project_tree_for_visibility(&*txn, &visibility)?;
            let mut paths = std::collections::BTreeSet::new();
            for path in expectation.tree.as_ref().unwrap().entries.keys() {
                for ancestor in std::path::Path::new(path).ancestors() {
                    let path = ancestor.to_string_lossy();
                    if !path.is_empty() {
                        paths.insert(path.into_owned());
                    }
                }
            }
            // Projection keys use repository spelling (raw UTF-8 or escaped),
            // while the Git expectation is keyed by canonical escaped identity.
            // Compare that same lossless identity and reject any folded duplicate
            // rather than letting set collection hide an extra graph path.
            let actual_paths = projection
                .present
                .keys()
                .map(|path| crate::canonical_path_identity(path))
                .collect::<std::collections::BTreeSet<_>>();
            if actual_paths.len() != projection.present.len() || actual_paths != paths {
                return Err(invalid(
                    "final stream namespace has missing/extra paths or directories",
                ));
            }
            txn.commit()
        })();
        match result {
            Ok(()) => {
                self.repo
                    .apply_operation_metadata_locked(&lock, operation.id())?;
                self.repo
                    .finalize_operation_verified(&lock, operation.id())?;
            }
            Err(error) => {
                self.repo
                    .abort_prepared_metadata_operation(&lock, &operation)?;
                return Err(error);
            }
        }
        Ok(GitStreamOutcome {
            commits: self.hashes.len(),
            batches: self.batches,
            state: target_after,
            private_publish_seconds: self.publish_seconds,
            final_proof_promotion_seconds: started.elapsed().as_secs_f64(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ConversionPolicy, ManifestDisposition, RepoPath, RepositoryEntry, RepositoryManifest,
    };
    use atomic_core::change::{ChangeHeader, Encoding, Insertion};
    use atomic_core::operation::{GitHashAlgorithm, GitObjectId};
    use atomic_core::types::{ChangePosition, EdgeFlags, Position, SetId};

    fn project() -> (ProjectTree, VerifiedProspectiveEquivalence) {
        let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
        let entries = [("a.txt", b"one\n".to_vec()), ("b.txt", b"two\n".to_vec())]
            .into_iter()
            .map(|(path, bytes)| {
                RepositoryEntry::new(
                    RepoPath::from_bytes(path.as_bytes()).unwrap(),
                    bytes,
                    0o644,
                    atomic_core::change::InodeKind::Regular,
                    None,
                    ManifestDisposition::Included,
                )
                .unwrap()
            })
            .collect();
        let manifest =
            RepositoryManifest::new(SetId::ZERO, policy.root().content_key, entries).unwrap();
        let project = ProjectTree::from_manifest(manifest, &policy).unwrap();
        let proof = crate::verify_prospective_equivalence(&project, &project.git.root).unwrap();
        (project, proof)
    }
    fn entry(
        seed: u8,
        parent: Option<u8>,
        path: &str,
        bytes: &[u8],
        project: &ProjectTree,
        missing_dep: bool,
    ) -> PreparedGitGraphChange {
        let end = ChangePosition::new(path.len() as u64);
        let inode = Position {
            change: None,
            pos: end,
        };
        let root = Position {
            change: Some(Hash::NONE),
            pos: ChangePosition::ROOT,
        };
        let folder = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
        let mut content = path.as_bytes().to_vec();
        content.extend_from_slice(bytes);
        let op = GraphOp::FileAdd {
            add_name: Insertion {
                predecessors: vec![root],
                successors: vec![],
                flag: folder,
                start: ChangePosition::ROOT,
                end,
                inode: root,
            },
            add_inode: Insertion {
                predecessors: vec![inode],
                successors: vec![],
                flag: folder,
                start: end,
                end,
                inode,
            },
            contents: Some(Insertion {
                predecessors: vec![inode],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: end,
                end: ChangePosition::new(content.len() as u64),
                inode,
            }),
            path: path.into(),
            encoding: Some(Encoding::Utf8),
        };
        let change = Change::new(
            ChangeHeader::new(path),
            vec![op],
            content,
            if missing_dep {
                vec![Hash::of(b"absent dependency")]
            } else {
                Vec::new()
            },
        );
        let oid = |seed| GitObjectId::new(GitHashAlgorithm::Sha1, vec![seed; 20]).unwrap();
        let origin = match parent {
            Some(parent) => {
                GitSynthesisOrigin::first_parent(oid(seed), project.git.root.clone(), oid(parent))
            }
            None => GitSynthesisOrigin::root(oid(seed), project.git.root.clone()),
        }
        .unwrap();
        PreparedGitGraphChange::new(
            change,
            origin.clone(),
            Vec::new(),
            crate::git_synthesis_metadata(&origin, &[], None),
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn crash_span_entry(
        seed: u8,
        parent: Option<u8>,
        path: &str,
        bytes: &[u8],
        project: &ProjectTree,
        dependency: Option<Hash>,
    ) -> PreparedGitGraphChange {
        use atomic_core::change::span_patch::{SpanOp, SpanPatch};
        let mut change = entry(seed, parent, path, bytes, project, false).change;
        change.hashed.header.timestamp =
            chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        change.hashed.dependencies = dependency.into_iter().collect();
        let (start, end) = match &mut change.hashed.hunks[0] {
            GraphOp::FileAdd { contents, .. } => {
                let content = contents.take().unwrap();
                (content.start.get(), content.end.get())
            }
            _ => panic!("expected file addition"),
        };
        change.hashed.span = Some(SpanPatch::new(
            Vec::new(),
            vec![SpanOp::Root {
                path: path.into(),
                ordinal: 0,
                start,
                end,
            }],
        ));
        let oid = |value| GitObjectId::new(GitHashAlgorithm::Sha1, vec![value; 20]).unwrap();
        let origin = match parent {
            Some(parent) => {
                GitSynthesisOrigin::first_parent(oid(seed), project.git.root.clone(), oid(parent))
            }
            None => GitSynthesisOrigin::root(oid(seed), project.git.root.clone()),
        }
        .unwrap();
        PreparedGitGraphChange::new_span(
            change,
            origin.clone(),
            b"incremental crash fixture".to_vec(),
            crate::git_synthesis_metadata(&origin, &[], None),
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn crash_baseline_project() -> (ProjectTree, VerifiedProspectiveEquivalence) {
        let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
        let entry = RepositoryEntry::new(
            RepoPath::from_bytes(b"a.txt").unwrap(),
            b"one\n".to_vec(),
            0o644,
            atomic_core::change::InodeKind::Regular,
            None,
            ManifestDisposition::Included,
        )
        .unwrap();
        let manifest =
            RepositoryManifest::new(SetId::ZERO, policy.root().content_key, vec![entry]).unwrap();
        let project = ProjectTree::from_manifest(manifest, &policy).unwrap();
        let proof = crate::verify_prospective_equivalence(&project, &project.git.root).unwrap();
        (project, proof)
    }

    #[cfg(unix)]
    fn crash_final_project() -> (ProjectTree, VerifiedProspectiveEquivalence) {
        let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
        let entries = [
            ("a.txt", b"one\n".as_slice()),
            ("b.txt", b"two\n".as_slice()),
            ("c.txt", b"three\n".as_slice()),
        ]
        .into_iter()
        .map(|(path, bytes)| {
            RepositoryEntry::new(
                RepoPath::from_bytes(path.as_bytes()).unwrap(),
                bytes.to_vec(),
                0o644,
                atomic_core::change::InodeKind::Regular,
                None,
                ManifestDisposition::Included,
            )
            .unwrap()
        })
        .collect();
        let manifest =
            RepositoryManifest::new(SetId::ZERO, policy.root().content_key, entries).unwrap();
        let project = ProjectTree::from_manifest(manifest, &policy).unwrap();
        let proof = crate::verify_prospective_equivalence(&project, &project.git.root).unwrap();
        (project, proof)
    }
    #[test]
    fn prepared_graph_boundary_rejects_file_ops() {
        let (project, _) = project();
        let prepared = entry(1, None, "a.txt", b"one\n", &project, false);
        let mut change = prepared.change;
        change
            .hashed
            .file_ops
            .push(atomic_core::change::FileOps::create(
                atomic_core::crdt::TrunkId::new(atomic_core::types::NodeId::ROOT, 0),
                "a.txt".into(),
                Some(Encoding::Utf8),
            ));
        let origin = GitSynthesisOrigin::root(
            GitObjectId::new(GitHashAlgorithm::Sha1, vec![1; 20]).unwrap(),
            project.git.root.clone(),
        )
        .unwrap();
        let error = PreparedGitGraphChange::new(
            change,
            origin.clone(),
            Vec::new(),
            crate::git_synthesis_metadata(&origin, &[], None),
        )
        .err()
        .expect("FileOps must be rejected before finalization or publication");
        assert!(error
            .to_string()
            .contains("eager semantics are unsupported"));
    }

    #[test]
    fn prepared_stream_fences_span_and_graph_formats() {
        use atomic_core::change::span_patch::{SpanOp, SpanPatch};
        let (project, _) = project();
        let prepared = entry(1, None, "a.txt", b"one\n", &project, false);
        let origin = GitSynthesisOrigin::root(
            GitObjectId::new(GitHashAlgorithm::Sha1, vec![1; 20]).unwrap(),
            project.git.root.clone(),
        )
        .unwrap();
        let metadata = crate::git_synthesis_metadata(&origin, &[], None);
        let mut graph = prepared.change;
        let missing = PreparedGitGraphChange::new_span(
            graph.clone(),
            origin.clone(),
            vec![],
            metadata.clone(),
        )
        .err()
        .unwrap();
        assert!(missing.to_string().contains("format does not match"));
        graph.hashed.span = Some(SpanPatch::new(
            Vec::new(),
            vec![SpanOp::Root {
                path: "a.txt".into(),
                ordinal: 0,
                start: 5,
                end: 9,
            }],
        ));
        let error =
            PreparedGitGraphChange::new(graph.clone(), origin.clone(), vec![], metadata.clone())
                .err()
                .unwrap();
        assert!(error.to_string().contains("format does not match"));
        graph.hashed.metadata = b"must not overwrite".to_vec();
        let error =
            PreparedGitGraphChange::new_span(graph, origin, b"Git facts".to_vec(), metadata)
                .err()
                .unwrap();
        assert!(error.to_string().contains("must not prepopulate"));
    }

    #[test]
    fn span_stream_refuses_existing_staged_paths_before_installing_capability() {
        use atomic_core::pristine::{CapabilityTxnT, SPAN_RANGE_CAPABILITY};
        let dir = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        std::fs::write(dir.path().join("user.txt"), b"user bytes\n").unwrap();
        repo.add(wc, "user.txt", Default::default()).unwrap();
        let error = match GitGraphImportSession::begin_span(
            &mut repo,
            GitStreamLimits::new(2, 1024 * 1024).unwrap(),
        ) {
            Ok(_) => panic!("span stream must not accept a staged path"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("fresh repository without tracked or staged paths"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(dir.path().join("user.txt")).unwrap(),
            b"user bytes\n"
        );
        let txn = repo.pristine().read_txn().unwrap();
        assert_eq!(
            txn.required_capability_version(SPAN_RANGE_CAPABILITY.id())
                .unwrap(),
            None
        );
        assert!(txn.get_view("git-stream-private").unwrap().is_none());
    }

    #[test]
    fn git_import_preflight_lock_excludes_concurrent_span_initialization() {
        use atomic_core::pristine::{CapabilityTxnT, SPAN_RANGE_CAPABILITY};

        let dir = tempfile::tempdir().unwrap();
        drop(Repository::init(dir.path()).unwrap());
        let guard = Repository::try_lock_git_import_without_span(dir.path(), false)
            .unwrap()
            .expect("existing repository needs a preflight lock");
        let path = dir.path().to_path_buf();
        let error = std::thread::spawn(move || {
            let mut repo = Repository::open_for_workspace_transaction(&path).unwrap();
            match GitGraphImportSession::begin_span(
                &mut repo,
                GitStreamLimits::new(2, 1024 * 1024).unwrap(),
            ) {
                Ok(_) => panic!("span initialization crossed the Git preflight lock"),
                Err(error) => error.to_string(),
            }
        })
        .join()
        .unwrap();
        assert!(error.contains("lock") || error.contains("busy"), "{error}");
        drop(guard);
        let mut repo = Repository::open(dir.path()).unwrap();
        {
            let txn = repo.pristine().read_txn().unwrap();
            assert_eq!(
                txn.required_capability_version(SPAN_RANGE_CAPABILITY.id())
                    .unwrap(),
                None
            );
        }
        let session = GitGraphImportSession::begin_span(
            &mut repo,
            GitStreamLimits::new(2, 1024 * 1024).unwrap(),
        )
        .unwrap();
        drop(session);
    }

    #[cfg(unix)]
    #[test]
    fn incremental_span_batch_process_kill_recovers_and_retries() {
        use atomic_core::operation::OperationScope;
        use atomic_core::types::Base32;
        use std::io::BufRead;
        use std::os::unix::process::ExitStatusExt;
        const CHILD_ROOT: &str = "ATOMIC_TEST_INCREMENTAL_CHILD_ROOT";
        const READY: &str = "ATOMIC_INCREMENTAL_READY ";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = std::path::PathBuf::from(root);
            std::fs::write(root.join("user.txt"), b"untracked user bytes\n").unwrap();
            let mut repo = Repository::init(&root).unwrap();
            let (baseline, proof) = crash_baseline_project();
            let (final_project, _) = crash_final_project();
            let limits = GitStreamLimits::new(2, 1024 * 1024).unwrap();
            let first = crash_span_entry(1, None, "a.txt", b"one\n", &baseline, None);
            let first_hash = first.hash();
            let mut session = GitGraphImportSession::begin_span(&mut repo, limits).unwrap();
            session.publish(&[first]).unwrap();
            session.finish(&baseline, &proof).unwrap();
            let prefix = vec![("01".repeat(20), first_hash)];
            let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
            let second = crash_span_entry(
                2,
                Some(1),
                "b.txt",
                b"two\n",
                &final_project,
                Some(first_hash),
            );
            let third = crash_span_entry(
                3,
                Some(2),
                "c.txt",
                b"three\n",
                &final_project,
                Some(second.hash()),
            );
            let mut session = GitGraphImportSession::begin_span_incremental(
                &mut repo,
                limits,
                &prefix,
                &policy,
                &baseline.git.root,
            )
            .unwrap();
            session.publish(&[second, third]).unwrap();
            panic!("child did not reach incremental crash window");
        }

        for window in [
            "before-commit",
            "after-commit",
            "after-metadata",
            "after-verified",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "repository::git_stream::tests::incremental_span_batch_process_kill_recovers_and_retries", "--nocapture"])
                .current_dir(std::env::temp_dir())
                .env(CHILD_ROOT, dir.path())
                .env("ATOMIC_TEST_INCREMENTAL_WINDOW", window)
                .env("ATOMIC_TEST_INCREMENTAL_SHA", "02".repeat(20))
                .stdout(std::process::Stdio::piped())
                .spawn().unwrap();
            let stdout = child.stdout.take().unwrap();
            let (send, recv) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                let mut lines = String::new();
                for line in std::io::BufReader::new(stdout).lines() {
                    match line {
                        Ok(line) => {
                            if let Some((_, marker)) = line.split_once(READY) {
                                let _ = send.send(Ok(marker.to_string()));
                                return;
                            }
                            lines.push_str(&line);
                            lines.push('\n');
                        }
                        Err(error) => {
                            let _ = send.send(Err(error.to_string()));
                            return;
                        }
                    }
                }
                let _ = send.send(Err(format!("child exited before crash point:\n{lines}")));
            });
            let ready = recv.recv_timeout(std::time::Duration::from_secs(60));
            let killed = child.kill();
            let status = child.wait().unwrap();
            if ready.is_ok() {
                reader.join().unwrap();
            }
            let marker = ready
                .expect("child timed out")
                .expect("child did not reach crash point");
            assert!(
                killed.is_ok() && status.signal() == Some(9),
                "child was not killed"
            );
            let parts: Vec<_> = marker.split_whitespace().collect();
            assert_eq!(parts.len(), 3, "{marker}");
            let interrupted = atomic_core::OperationId::from_base32(parts[0].as_bytes()).unwrap();
            let staged_hash = Hash::from_base32(parts[1].as_bytes()).unwrap();
            let staged_third = Hash::from_base32(parts[2].as_bytes()).unwrap();
            let mut repo = Repository::open(dir.path()).unwrap();
            let wc = repo.require_working_copy_id().unwrap();
            let initial_log = repo
                .operation_log(OperationScope::WorkingCopy(wc), None, false)
                .unwrap();
            let global_log = repo
                .operation_log(OperationScope::Repository, None, false)
                .unwrap();
            let interrupted_details = repo.operation_details(interrupted).unwrap();
            if window == "after-verified" {
                assert_eq!(
                    interrupted_details.verification,
                    super::super::OperationVerificationState::Verified
                );
            } else {
                assert_ne!(
                    interrupted_details.verification,
                    super::super::OperationVerificationState::Verified
                );
                let recovered = initial_log
                    .entries
                    .iter()
                    .find(|entry| {
                        entry.operation.payload().kind == OperationKind::Recover
                            && entry.operation.payload().parents.contains(&interrupted)
                            && entry.verification
                                == super::super::OperationVerificationState::Verified
                    })
                    .expect("incomplete batch must have a verified recovery child");
                assert!(!repo
                    .operation_details(recovered.operation.id())
                    .unwrap()
                    .receipts
                    .is_empty());
                assert!(global_log
                    .entries
                    .iter()
                    .any(|entry| entry.operation.id() == recovered.operation.id()));
            }
            assert_eq!(
                std::fs::read(dir.path().join("user.txt")).unwrap(),
                b"untracked user bytes\n"
            );
            assert_eq!(
                repo.get_file_content("a.txt").unwrap(),
                Some(b"one\n".to_vec())
            );
            assert_eq!(repo.get_file_content("b.txt").unwrap(), None);
            assert_eq!(repo.get_file_content("c.txt").unwrap(), None);
            let first_hash = {
                let txn = repo.pristine().read_txn().unwrap();
                let id = txn.get_by_git_sha(&"01".repeat(20)).unwrap().unwrap();
                txn.get_external(id).unwrap().unwrap()
            };
            let prefix = vec![("01".repeat(20), first_hash)];
            let (baseline, _) = crash_baseline_project();
            let (final_project, final_proof) = crash_final_project();
            let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
            let limits = GitStreamLimits::new(2, 1024 * 1024).unwrap();
            let outcome = if window == "after-verified" {
                let staged = vec![
                    ("02".repeat(20), staged_hash),
                    ("03".repeat(20), staged_third),
                ];
                let session = GitGraphImportSession::resume_span_incremental(
                    &mut repo,
                    limits,
                    &prefix,
                    &staged,
                    &policy,
                    &baseline.git.root,
                )
                .unwrap();
                session.finish(&final_project, &final_proof)
            } else {
                let second = crash_span_entry(
                    2,
                    Some(1),
                    "b.txt",
                    b"two\n",
                    &final_project,
                    Some(first_hash),
                );
                assert_eq!(
                    second.hash(),
                    staged_hash,
                    "replayed change identity changed"
                );
                let third = crash_span_entry(
                    3,
                    Some(2),
                    "c.txt",
                    b"three\n",
                    &final_project,
                    Some(staged_hash),
                );
                assert_eq!(
                    third.hash(),
                    staged_third,
                    "replayed second change identity changed"
                );
                let mut session = GitGraphImportSession::begin_span_incremental(
                    &mut repo,
                    limits,
                    &prefix,
                    &policy,
                    &baseline.git.root,
                )
                .unwrap();
                session.publish(&[second, third]).unwrap();
                session.finish(&final_project, &final_proof)
            };
            assert_eq!(outcome.unwrap().commits, 2);
            assert_eq!(
                repo.get_file_content("b.txt").unwrap(),
                Some(b"two\n".to_vec())
            );
            assert_eq!(
                repo.get_file_content("c.txt").unwrap(),
                Some(b"three\n".to_vec())
            );
            assert_eq!(
                std::fs::read(dir.path().join("user.txt")).unwrap(),
                b"untracked user bytes\n"
            );
            let log = repo
                .operation_log(OperationScope::WorkingCopy(wc), None, false)
                .unwrap();
            assert!(log
                .entries
                .iter()
                .any(|entry| entry.operation.id() == interrupted));
            drop(repo);
            let reopened = Repository::open(dir.path()).unwrap();
            assert_eq!(
                reopened.get_file_content("a.txt").unwrap(),
                Some(b"one\n".to_vec())
            );
            assert_eq!(
                reopened.get_file_content("b.txt").unwrap(),
                Some(b"two\n".to_vec())
            );
            assert_eq!(
                reopened.get_file_content("c.txt").unwrap(),
                Some(b"three\n".to_vec())
            );
            eprintln!("incremental span crash window {window}: recovered and retried");
        }
    }

    #[cfg(unix)]
    #[test]
    fn projected_record_process_kill_recovers_and_preserves_capability() {
        use atomic_core::operation::OperationScope;
        use atomic_core::pristine::{CapabilityTxnT, SPAN_PROJECTED_SEMANTICS_CAPABILITY};
        use atomic_core::types::Base32;
        use std::io::BufRead;
        use std::os::unix::process::ExitStatusExt;
        const CHILD_ROOT: &str = "ATOMIC_TEST_PROJECTED_RECORD_ROOT";
        const READY: &str = "ATOMIC_PROJECTED_RECORD_READY ";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = std::path::PathBuf::from(root);
            let mut repo = Repository::init(&root).unwrap();
            let (project, proof) = crash_baseline_project();
            let initial = crash_span_entry(1, None, "a.txt", b"one\n", &project, None);
            let mut session = GitGraphImportSession::begin_span(
                &mut repo,
                GitStreamLimits::new(2, 1024 * 1024).unwrap(),
            )
            .unwrap();
            session.publish(&[initial]).unwrap();
            session.finish(&project, &proof).unwrap();
            let wc = repo.require_working_copy_id().unwrap();
            repo.materialize(wc).unwrap();
            std::fs::write(root.join("a.txt"), b"native bytes\n").unwrap();
            std::fs::write(root.join("user.txt"), b"untracked user bytes\n").unwrap();
            repo.record(
                wc,
                ChangeHeader::new("native crash fixture"),
                crate::RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap();
            panic!("child did not reach projected record kill point");
        }
        for window in ["after-fence", "after-save", "after-apply", "after-verified"] {
            let dir = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "repository::git_stream::tests::projected_record_process_kill_recovers_and_preserves_capability", "--nocapture"])
                .current_dir(std::env::temp_dir())
                .env(CHILD_ROOT, dir.path())
                .env("ATOMIC_TEST_PROJECTED_RECORD_WINDOW", window)
                .stdout(std::process::Stdio::piped()).spawn().unwrap();
            let stdout = child.stdout.take().unwrap();
            let (send, receive) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                let mut output = String::new();
                for line in std::io::BufReader::new(stdout).lines() {
                    match line {
                        Ok(line) => {
                            if let Some((_, marker)) = line.split_once(READY) {
                                let _ = send.send(Ok(marker.to_string()));
                                return;
                            }
                            output.push_str(&line);
                            output.push('\n');
                        }
                        Err(error) => {
                            let _ = send.send(Err(error.to_string()));
                            return;
                        }
                    }
                }
                let _ = send.send(Err(format!("child exited before kill point:\n{output}")));
            });
            let ready = receive.recv_timeout(std::time::Duration::from_secs(60));
            let killed = child.kill();
            let status = child.wait().unwrap();
            if ready.is_ok() {
                reader.join().unwrap();
            }
            let marker = ready
                .expect("child timed out")
                .expect("child did not reach kill point");
            assert!(killed.is_ok() && status.signal() == Some(9));
            let fields: Vec<_> = marker.split_whitespace().collect();
            assert_eq!(fields.len(), 2, "{marker}");
            let interrupted = atomic_core::OperationId::from_base32(fields[0].as_bytes()).unwrap();
            let hash = Hash::from_base32(fields[1].as_bytes()).unwrap();
            let repo = Repository::open(dir.path()).unwrap();
            let wc = repo.require_working_copy_id().unwrap();
            assert_eq!(
                std::fs::read(dir.path().join("a.txt")).unwrap(),
                b"native bytes\n"
            );
            assert_eq!(
                std::fs::read(dir.path().join("user.txt")).unwrap(),
                b"untracked user bytes\n"
            );
            assert_eq!(
                repo.pristine()
                    .read_txn()
                    .unwrap()
                    .required_capability_version(SPAN_PROJECTED_SEMANTICS_CAPABILITY.id())
                    .unwrap(),
                Some(SPAN_PROJECTED_SEMANTICS_CAPABILITY.minimum_version())
            );
            let details = repo.operation_details(interrupted).unwrap();
            let log = repo
                .operation_log(OperationScope::WorkingCopy(wc), None, false)
                .unwrap();
            assert_eq!(
                repo.load_change(&hash).is_ok(),
                window != "after-fence",
                "stored bytes at {window}"
            );
            if window == "after-verified" {
                assert_eq!(
                    details.verification,
                    super::super::OperationVerificationState::Verified
                );
                assert_eq!(
                    repo.get_file_content("a.txt").unwrap(),
                    Some(b"native bytes\n".to_vec())
                );
                assert!(repo.load_change(&hash).is_ok());
            } else {
                assert_ne!(
                    details.verification,
                    super::super::OperationVerificationState::Verified
                );
                let recovery = log
                    .entries
                    .iter()
                    .find(|entry| {
                        entry.operation.payload().kind == OperationKind::Recover
                            && entry.operation.payload().parents.contains(&interrupted)
                            && entry.verification
                                == super::super::OperationVerificationState::Verified
                    })
                    .unwrap_or_else(|| panic!("missing verified recovery for {window}"));
                assert!(!repo
                    .operation_details(recovery.operation.id())
                    .unwrap()
                    .receipts
                    .is_empty());
                let global = repo
                    .operation_log(OperationScope::Repository, None, false)
                    .unwrap();
                assert!(global
                    .entries
                    .iter()
                    .any(|entry| entry.operation.id() == recovery.operation.id()));
                assert_eq!(
                    repo.get_file_content("a.txt").unwrap(),
                    Some(b"one\n".to_vec())
                );
                repo.record(
                    wc,
                    ChangeHeader::new("retry native"),
                    crate::RecordOptions::new()
                        .with_all(true)
                        .save_to_store(true)
                        .apply_after_record(true),
                )
                .unwrap();
                assert_eq!(
                    repo.get_file_content("a.txt").unwrap(),
                    Some(b"native bytes\n".to_vec())
                );
            }
            drop(repo);
            let repo = Repository::open(dir.path()).unwrap();
            assert_eq!(
                repo.get_file_content("a.txt").unwrap(),
                Some(b"native bytes\n".to_vec())
            );
            assert_eq!(
                std::fs::read(dir.path().join("user.txt")).unwrap(),
                b"untracked user bytes\n"
            );
            eprintln!("projected record crash window {window}: recovered and verified");
        }
    }

    #[test]
    fn incremental_bootstrap_classification_rejects_mixed_or_foreign_targets() {
        let dir = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let session = GitGraphImportSession::begin_span(
            &mut repo,
            GitStreamLimits::new(2, 1024 * 1024).unwrap(),
        )
        .unwrap();
        drop(session);
        let wc = repo.require_working_copy_id().unwrap();
        std::fs::write(dir.path().join(".atomicignore"), b"# bootstrap\n").unwrap();
        repo.add(wc, ".atomicignore", Default::default()).unwrap();
        let recorded = repo
            .record(
                wc,
                ChangeHeader::new("Initialize repository"),
                crate::RecordOptions::new().add_path(".atomicignore"),
            )
            .unwrap();
        let change = recorded.change();
        assert!(is_atomicignore_bootstrap(change));

        let mut foreign_attribute = change.clone();
        for hunk in &mut foreign_attribute.hashed.hunks {
            if let GraphOp::SetAttr { inode, .. } = hunk {
                inode.change = Some(Hash::of(b"foreign inode"));
                break;
            }
        }
        assert!(!is_atomicignore_bootstrap(&foreign_attribute));
        let mut wrong_path = change.clone();
        wrong_path
            .hashed
            .file_ops
            .push(atomic_core::change::FileOps::create(
                atomic_core::crdt::TrunkId::new(atomic_core::types::NodeId::ROOT, 9),
                "other.txt".into(),
                Some(Encoding::Utf8),
            ));
        assert!(!is_atomicignore_bootstrap(&wrong_path));
        let mut extra_dependency = change.clone();
        extra_dependency
            .hashed
            .dependencies
            .push(Hash::of(b"other change"));
        assert!(!is_atomicignore_bootstrap(&extra_dependency));
    }

    #[test]
    fn git_import_preflight_refuses_legacy_span_database_without_migration() {
        let dir = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let session = GitGraphImportSession::begin_span(
            &mut repo,
            GitStreamLimits::new(2, 1024 * 1024).unwrap(),
        )
        .unwrap();
        drop(session);
        drop(repo);
        let dot = dir.path().join(".atomic");
        let current = dot.join(super::super::DATABASE_FILE);
        let legacy = dot.join(super::super::LEGACY_PRISTINE_FILE);
        std::fs::rename(&current, &legacy).unwrap();
        let before = std::fs::read(&legacy).unwrap();

        let error = match Repository::try_lock_git_import_without_span(dir.path(), false) {
            Ok(_) => panic!("legacy span database must not be imported into"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("existing span repository"),
            "{error}"
        );
        assert_eq!(std::fs::read(&legacy).unwrap(), before);
        assert!(!current.exists());
    }

    #[test]
    fn git_import_preflight_refuses_unreadable_database_without_repair() {
        let dir = tempfile::tempdir().unwrap();
        let dot = dir.path().join(".atomic");
        std::fs::create_dir(&dot).unwrap();
        let database = dot.join(super::super::DATABASE_FILE);
        let bytes = b"not a redb database; preserve this evidence";
        std::fs::write(&database, bytes).unwrap();

        assert!(Repository::try_lock_git_import_without_span(dir.path(), false).is_err());
        assert_eq!(std::fs::read(&database).unwrap(), bytes);
        assert!(!dot.join("bridge.lock").exists());
    }

    /// Copy a repository while it is still open: the copy is what a killed
    /// process leaves behind, a database redb must repair before reading.
    fn copy_of_open_repository(source: &std::path::Path) -> tempfile::TempDir {
        fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
            std::fs::create_dir_all(to).unwrap();
            for entry in std::fs::read_dir(from).unwrap() {
                let entry = entry.unwrap();
                let target = to.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    copy_dir(&entry.path(), &target);
                } else {
                    std::fs::copy(entry.path(), &target).unwrap();
                }
            }
        }
        let copy = tempfile::tempdir().unwrap();
        copy_dir(source, copy.path());
        copy
    }

    #[test]
    fn git_import_preflight_repairs_a_database_left_by_a_killed_process() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let killed = copy_of_open_repository(dir.path());
        let dry_run = copy_of_open_repository(dir.path());
        drop(repo);
        let database =
            |root: &std::path::Path| root.join(".atomic").join(super::super::DATABASE_FILE);
        assert!(
            atomic_core::pristine::Pristine::open_readonly_without_recovery(database(
                killed.path()
            ))
            .is_err(),
            "fixture must need redb crash repair"
        );

        // A dry run may not write, so it still refuses.
        assert!(Repository::try_lock_git_import_without_span(dry_run.path(), true).is_err());
        assert!(
            atomic_core::pristine::Pristine::open_readonly_without_recovery(database(
                dry_run.path()
            ))
            .is_err()
        );

        let guard = Repository::try_lock_git_import_without_span(killed.path(), false)
            .expect("a database needing only crash repair must pass the preflight");
        assert!(guard.is_some());
        drop(guard);
        atomic_core::pristine::Pristine::open_readonly_without_recovery(database(killed.path()))
            .expect("the preflight leaves the database readable without recovery");
    }

    #[test]
    fn failed_final_proof_never_promotes_private_stream() {
        let dir = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (project, proof) = project();
        let a = entry(1, None, "a.txt", b"wrong\n", &project, false);
        let b = entry(2, Some(1), "b.txt", b"two\n", &project, false);
        let mut session =
            GitGraphImportSession::begin(&mut repo, GitStreamLimits::new(1, 1024 * 1024).unwrap())
                .unwrap();
        session.publish(&[a]).unwrap();
        assert!(session.repo.effective_history(None).unwrap().is_empty());
        assert_eq!(session.repo.get_file_content("a.txt").unwrap(), None);
        session.publish(&[b]).unwrap();
        assert!(session.finish(&project, &proof).is_err());
        assert!(repo.effective_history(None).unwrap().is_empty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        drop(repo);
        assert!(Repository::open(dir.path())
            .unwrap()
            .effective_history(None)
            .unwrap()
            .is_empty());
    }
    #[test]
    fn failed_batch_is_rolled_back_and_graph_only_retry_uses_hash_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (project, proof) = project();
        let bad = entry(1, None, "a.txt", b"one\n", &project, true);
        let mut session =
            GitGraphImportSession::begin(&mut repo, GitStreamLimits::new(2, 1024 * 1024).unwrap())
                .unwrap();
        assert!(session.publish(&[bad]).is_err());
        assert!(session.repo.effective_history(None).unwrap().is_empty());
        session
            .publish(&[
                entry(1, None, "a.txt", b"one\n", &project, false),
                entry(2, Some(1), "b.txt", b"two\n", &project, false),
            ])
            .unwrap();
        assert_eq!(session.finish(&project, &proof).unwrap().commits, 2);
    }

    #[test]
    fn final_namespace_accepts_native_unicode_spelling_but_refuses_duplicate_identity() {
        let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
        let raw = "caf\u{e9}.txt";
        let manifest = RepositoryManifest::new(
            SetId::ZERO,
            policy.root().content_key,
            vec![RepositoryEntry::new(
                RepoPath::from_bytes(raw.as_bytes()).unwrap(),
                b"one\n".to_vec(),
                0o644,
                atomic_core::change::InodeKind::Regular,
                None,
                ManifestDisposition::Included,
            )
            .unwrap()],
        )
        .unwrap();
        let project = ProjectTree::from_manifest(manifest, &policy).unwrap();
        let proof = crate::verify_prospective_equivalence(&project, &project.git.root).unwrap();
        for duplicate in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut repo = Repository::init(dir.path()).unwrap();
            let mut session = GitGraphImportSession::begin(
                &mut repo,
                GitStreamLimits::new(2, 1024 * 1024).unwrap(),
            )
            .unwrap();
            session
                .publish(&[entry(1, None, raw, b"one\n", &project, false)])
                .unwrap();
            if duplicate {
                session
                    .publish(&[entry(
                        2,
                        Some(1),
                        "caf%C3%A9.txt",
                        b"one\n",
                        &project,
                        false,
                    )])
                    .unwrap();
            }
            let result = session.finish(&project, &proof);
            if duplicate {
                assert!(
                    result.is_err(),
                    "canonicalization must not hide an extra graph path"
                );
                assert!(repo.effective_history(None).unwrap().is_empty());
            } else {
                result.unwrap();
                assert_eq!(repo.get_file_content(raw).unwrap(), Some(b"one\n".to_vec()));
            }
        }
    }
}
