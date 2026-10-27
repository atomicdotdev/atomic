//! Private, bounded publication for a direct Git producer. This
//! module owns transactions, not graph algorithms or working-copy recording.

use super::operation::{current_operation_timestamp_ms, working_copy_state_ref};
use super::synthesis::{finalize_classified_change, git_oid_hex, SynthesisClassification};
use super::{
    GitSynthesisOrigin, ProjectTree, Repository, RepositoryError, StagedExpectation,
    StagedPathExpectation, StagedTreeExpectation, VerifiedProspectiveEquivalence,
};
use atomic_core::change::{Change, GraphOp};
use atomic_core::operation::{
    ActorRef, MetadataTarget, MetadataTransition, MetadataValue, OperationKind, RepoStateRef,
    ViewStateRef,
};
use atomic_core::pristine::{GitShaIndexMutTxnT, GraphTxnT, MutTxnT, ViewScope, ViewTxnT};
use atomic_core::types::{Hash, Merkle};
use std::collections::{BTreeMap, HashSet};

fn invalid(message: impl Into<String>) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: message.into(),
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
        mut unhashed: serde_json::Value,
    ) -> Result<Self, RepositoryError> {
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
    hashes: Vec<Hash>,
    last_sha: Option<String>,
    last_hash: Option<Hash>,
    batches: usize,
    publish_seconds: f64,
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
            hashes: Vec::new(),
            last_sha: None,
            last_hash: None,
            batches: 0,
            publish_seconds: 0.0,
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
            if entry.parent.as_deref() != previous || !unique.insert(&entry.sha) {
                return Err(invalid(
                    "stream batch must follow the exact first-parent chain",
                ));
            }
            previous = Some(&entry.sha);
        }
        let wc = self.repo.require_working_copy_id()?;
        let lock = self.repo.try_lock_operation(wc)?;
        let txn = self.repo.pristine.read_txn()?;
        let stage = txn
            .get_view(&self.stage)?
            .ok_or_else(|| invalid("private stream view disappeared"))?;
        let target = txn
            .get_view(&self.target)?
            .ok_or_else(|| invalid("target disappeared"))?;
        if target.change_count != 0
            || target.state != Merkle::ZERO
            || stage.parent != Some(target.id)
            || stage.kind != ViewScope::Draft
            || stage.change_count as usize != self.hashes.len()
        {
            return Err(invalid("stream target/stage changed; refusing publication"));
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
            let mut closure = self.hashes.clone();
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
        let started = std::time::Instant::now();
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
            let path = std::str::from_utf8(entry.path.as_bytes())
                .map_err(|error| invalid(error.to_string()))?
                .to_string();
            expected.insert(
                path,
                StagedPathExpectation {
                    bytes: entry.repository_bytes.to_vec(),
                    mode: entry.mode,
                    kind: entry.kind,
                },
            );
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
        if target.change_count != 0
            || target.state != Merkle::ZERO
            || stage.parent != Some(target.id)
            || stage.kind != ViewScope::Draft
            || staged != self.hashes
            || stage.state
                != self
                    .hashes
                    .iter()
                    .fold(Merkle::ZERO, |state, hash| state.next(hash))
        {
            return Err(invalid("target/stage changed before final promotion"));
        }
        drop(txn);
        let before = self.repo.working_copy_record(wc)?;
        let mut after = before.clone();
        if after.desired_view != target.id {
            return Err(invalid("working copy changed during direct stream"));
        }
        after.desired_state = stage.state;
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
            state(after, stage.state),
            self.hashes
                .iter()
                .enumerate()
                .map(|(index, hash)| MetadataTransition {
                    target: MetadataTarget::ViewChange {
                        view: self.target.clone(),
                        change: *hash,
                    },
                    expected_old: MetadataValue::Absent,
                    expected_new: MetadataValue::Sequence(index as u64),
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
            if projection
                .present
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                != paths
            {
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
            state: stage.state,
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
}
