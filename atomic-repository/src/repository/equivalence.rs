//! Deterministic structured equivalence reports for Atomic, Git index, and worktree state.

use super::git_observation::compute_index_tree;
use super::project_tree::{git_object_id, GitObjectKind};
use super::*;

use std::collections::{BTreeMap, BTreeSet};

use atomic_core::change::InodeKind;
use atomic_core::operation::{GitHashAlgorithm, GitObjectId};
use atomic_objects::ObjectKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EquivalenceLayer {
    Policy,
    GitIndex,
    Worktree,
    Joint,
}

/// Stable CB-4B mismatch taxonomy. Details remain strings so new evidence can
/// be added without weakening deterministic category/path ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MismatchKind {
    MissingPath,
    ExtraPath,
    StaleContent,
    Mode,
    Kind,
    LinkTarget,
    NewlineOrFilterResult,
    EmptyFile,
    ExclusionPolicy,
    ConversionPolicy,
    CaseFoldCollision,
    ConflictStage,
    SparseDirectory,
    IntentToAdd,
    ObjectAlgorithm,
    ObjectHeader,
    ClaimedRootForgery,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EquivalenceMismatch {
    pub layer: EquivalenceLayer,
    pub kind: MismatchKind,
    pub path: Option<RepoPath>,
    pub expected: String,
    pub actual: String,
}

/// Optional identities supplied by a binding/header and verified independently
/// from the observed state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EquivalenceClaims {
    pub manifest_version: Option<u8>,
    pub object_algorithm: Option<GitHashAlgorithm>,
    pub manifest_root: Option<ObjectKey>,
    pub conversion_policy_root: Option<ObjectKey>,
    pub git_tree_root: Option<GitObjectId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EquivalenceReport {
    pub expected_manifest_root: ManifestRoot,
    pub index_root: Option<ManifestRoot>,
    pub worktree_root: Option<ManifestRoot>,
    pub mismatches: Vec<EquivalenceMismatch>,
}

/// Unforgeable evidence that a prospective import projection was compared with
/// the complete Git commit tree before any import persistence began.
#[derive(Clone, Debug)]
pub struct VerifiedProspectiveEquivalence {
    expected_git_tree: GitObjectId,
    manifest_root: ManifestRoot,
}

impl VerifiedProspectiveEquivalence {
    pub fn expected_git_tree(&self) -> &GitObjectId {
        &self.expected_git_tree
    }

    pub fn manifest_root(&self) -> &ManifestRoot {
        &self.manifest_root
    }
}

/// Verify a complete prospective project tree and return typed gate evidence.
pub fn verify_prospective_equivalence(
    project: &ProjectTree,
    expected_git_tree: &GitObjectId,
) -> Result<VerifiedProspectiveEquivalence, ProjectTreeError> {
    if &project.git.root != expected_git_tree {
        return Err(ProjectTreeError::ProspectiveTreeMismatch {
            expected: format!("{expected_git_tree:?}"),
            actual: format!("{:?}", project.git.root),
        });
    }
    Ok(VerifiedProspectiveEquivalence {
        expected_git_tree: expected_git_tree.clone(),
        manifest_root: project.manifest.root(),
    })
}

impl EquivalenceReport {
    pub fn is_equivalent(&self) -> bool {
        self.mismatches.is_empty()
    }

    fn new(project: &ProjectTree) -> Self {
        Self {
            expected_manifest_root: project.manifest.root(),
            index_root: None,
            worktree_root: None,
            mismatches: Vec::new(),
        }
    }

    fn finish(&mut self) {
        self.mismatches.sort();
        self.mismatches.dedup();
    }

    fn push(
        &mut self,
        layer: EquivalenceLayer,
        kind: MismatchKind,
        path: Option<RepoPath>,
        expected: impl Into<String>,
        actual: impl Into<String>,
    ) {
        self.mismatches.push(EquivalenceMismatch {
            layer,
            kind,
            path,
            expected: expected.into(),
            actual: actual.into(),
        });
    }
}

/// Compare the Atomic project tree to the complete stage-aware Git index.
pub fn compare_project_to_index(project: &ProjectTree, index: &GitIndexState) -> EquivalenceReport {
    let mut report = EquivalenceReport::new(project);
    report.index_root = Some(index.root());
    compare_index_into(project, index, &mut report);
    report.finish();
    report
}

/// Compare canonical repository bytes and inode facts to the physical worktree.
pub fn compare_project_to_worktree(
    project: &ProjectTree,
    worktree: &WorktreeObservation,
    policy: &ConversionPolicy,
) -> EquivalenceReport {
    let mut report = EquivalenceReport::new(project);
    report.worktree_root = Some(worktree.root());
    compare_policy_into(project, worktree, policy, &mut report);
    compare_worktree_into(project, worktree, policy, &mut report);
    report.finish();
    report
}

/// Joint comparison used by future push/import adoption gates.
pub fn compare_project_state(
    project: &ProjectTree,
    index: &GitIndexState,
    worktree: &WorktreeObservation,
    policy: &ConversionPolicy,
    claims: &EquivalenceClaims,
) -> EquivalenceReport {
    let mut report = EquivalenceReport::new(project);
    report.index_root = Some(index.root());
    report.worktree_root = Some(worktree.root());
    compare_policy_into(project, worktree, policy, &mut report);
    compare_index_into(project, index, &mut report);
    compare_worktree_into(project, worktree, policy, &mut report);
    verify_claims_into(project, policy, claims, &mut report);

    let skip_worktree: BTreeSet<_> = index
        .entries
        .iter()
        .filter(|entry| entry.stage == 0 && entry.skip_worktree)
        .map(|entry| entry.path.clone())
        .collect();
    report.mismatches.retain(|mismatch| {
        !(mismatch.layer == EquivalenceLayer::Worktree
            && mismatch.kind == MismatchKind::MissingPath
            && mismatch
                .path
                .as_ref()
                .is_some_and(|path| skip_worktree.contains(path)))
    });
    report.finish();
    report
}

fn compare_policy_into(
    project: &ProjectTree,
    worktree: &WorktreeObservation,
    policy: &ConversionPolicy,
    report: &mut EquivalenceReport,
) {
    let policy_root = policy.root().content_key;
    if project.manifest.conversion_policy_root != policy_root {
        report.push(
            EquivalenceLayer::Policy,
            MismatchKind::ConversionPolicy,
            None,
            project.manifest.conversion_policy_root.clone(),
            policy_root,
        );
    }
    if worktree.platform != policy.platform {
        report.push(
            EquivalenceLayer::Policy,
            MismatchKind::ConversionPolicy,
            None,
            format!("{:?}", policy.platform),
            format!("{:?}", worktree.platform),
        );
    }
}

fn compare_index_into(
    project: &ProjectTree,
    index: &GitIndexState,
    report: &mut EquivalenceReport,
) {
    if index.version != GIT_INDEX_STATE_VERSION {
        report.push(
            EquivalenceLayer::GitIndex,
            MismatchKind::ObjectHeader,
            None,
            GIT_INDEX_STATE_VERSION.to_string(),
            index.version.to_string(),
        );
    }
    if index.object_format != project.git.algorithm {
        report.push(
            EquivalenceLayer::GitIndex,
            MismatchKind::ObjectAlgorithm,
            None,
            format!("{:?}", project.git.algorithm),
            format!("{:?}", index.object_format),
        );
    }
    match compute_index_tree(index.object_format, &index.entries) {
        Ok(computed) if computed != index.tree => report.push(
            EquivalenceLayer::GitIndex,
            MismatchKind::ClaimedRootForgery,
            None,
            format!("{:?}", computed),
            format!("{:?}", index.tree),
        ),
        Err(error) => report.push(
            EquivalenceLayer::GitIndex,
            MismatchKind::ObjectHeader,
            None,
            "valid index tree".to_string(),
            error.to_string(),
        ),
        _ => {}
    }

    let mut stage_zero = BTreeMap::new();
    let mut exceptional_paths = BTreeSet::new();
    for entry in &index.entries {
        if entry.stage != 0 {
            exceptional_paths.insert(entry.path.clone());
            report.push(
                EquivalenceLayer::GitIndex,
                MismatchKind::ConflictStage,
                Some(entry.path.clone()),
                "stage 0",
                format!("stage {}", entry.stage),
            );
            continue;
        }
        if entry.intent_to_add {
            exceptional_paths.insert(entry.path.clone());
            report.push(
                EquivalenceLayer::GitIndex,
                MismatchKind::IntentToAdd,
                Some(entry.path.clone()),
                "materialized stage-0 object",
                "intent-to-add",
            );
        }
        if entry.sparse_directory {
            exceptional_paths.insert(entry.path.clone());
            report.push(
                EquivalenceLayer::GitIndex,
                MismatchKind::SparseDirectory,
                Some(entry.path.clone()),
                "expanded stage-0 paths",
                "sparse directory entry",
            );
        }
        stage_zero.insert(entry.path.clone(), entry);
    }

    let expected: BTreeMap<_, _> = project
        .manifest
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    for (path, expected_entry) in &expected {
        if expected_entry.disposition != ManifestDisposition::Included {
            if stage_zero.contains_key(path) {
                report.push(
                    EquivalenceLayer::GitIndex,
                    MismatchKind::ExclusionPolicy,
                    Some(path.clone()),
                    "excluded",
                    "present in index",
                );
            }
            continue;
        }
        let Some(actual) = stage_zero.get(path) else {
            if !exceptional_paths.contains(path) {
                report.push(
                    EquivalenceLayer::GitIndex,
                    MismatchKind::MissingPath,
                    Some(path.clone()),
                    "present",
                    "missing",
                );
            }
            continue;
        };
        if actual.intent_to_add || actual.sparse_directory {
            continue;
        }
        let expected_kind = expected_entry.kind;
        let actual_kind = kind_from_git_mode(actual.mode);
        if actual_kind != Some(expected_kind) {
            report.push(
                EquivalenceLayer::GitIndex,
                MismatchKind::Kind,
                Some(path.clone()),
                expected_kind.to_string(),
                actual_kind.map_or_else(
                    || format!("mode {:#o}", actual.mode),
                    |kind| kind.to_string(),
                ),
            );
        } else if actual.mode != expected_entry.git_mode() {
            report.push(
                EquivalenceLayer::GitIndex,
                MismatchKind::Mode,
                Some(path.clone()),
                format!("{:#o}", expected_entry.git_mode()),
                format!("{:#o}", actual.mode),
            );
        }
        match expected_git_oid(expected_entry, project.git.algorithm) {
            Ok(expected_oid) if actual.oid.as_ref() != Some(&expected_oid) => report.push(
                EquivalenceLayer::GitIndex,
                MismatchKind::StaleContent,
                Some(path.clone()),
                format!("{:?}", expected_oid),
                format!("{:?}", actual.oid),
            ),
            Err(error) => report.push(
                EquivalenceLayer::GitIndex,
                MismatchKind::ObjectHeader,
                Some(path.clone()),
                "valid expected object",
                error.to_string(),
            ),
            _ => {}
        }
    }
    for (path, actual) in stage_zero {
        if !expected.contains_key(&path) && !actual.sparse_directory {
            report.push(
                EquivalenceLayer::GitIndex,
                MismatchKind::ExtraPath,
                Some(path),
                "absent",
                "present",
            );
        }
    }
}

fn compare_worktree_into(
    project: &ProjectTree,
    worktree: &WorktreeObservation,
    policy: &ConversionPolicy,
    report: &mut EquivalenceReport,
) {
    let mut folded = BTreeMap::<Vec<u8>, Vec<RepoPath>>::new();
    for entry in &worktree.entries {
        folded
            .entry(ascii_fold(entry.path.as_bytes()))
            .or_default()
            .push(entry.path.clone());
    }
    for paths in folded
        .values()
        .filter(|paths| !worktree.platform.case_sensitive && paths.len() > 1)
    {
        for path in paths {
            report.push(
                EquivalenceLayer::Worktree,
                MismatchKind::CaseFoldCollision,
                Some(path.clone()),
                "unique case-folded path",
                paths
                    .iter()
                    .map(RepoPath::escaped)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
    }

    let expected: BTreeMap<_, _> = project
        .manifest
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let actual: BTreeMap<_, _> = worktree
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    for (path, expected_entry) in &expected {
        if expected_entry.disposition != ManifestDisposition::Included {
            if actual
                .get(path)
                .is_some_and(|entry| entry.disposition == ManifestDisposition::Included)
            {
                report.push(
                    EquivalenceLayer::Worktree,
                    MismatchKind::ExclusionPolicy,
                    Some(path.clone()),
                    "excluded",
                    "included",
                );
            }
            continue;
        }
        let Some(observed) = actual.get(path) else {
            report.push(
                EquivalenceLayer::Worktree,
                MismatchKind::MissingPath,
                Some(path.clone()),
                "present",
                "missing",
            );
            continue;
        };
        let kind_matches = match expected_entry.kind {
            InodeKind::Regular => observed.physical_kind == PhysicalKind::Regular,
            InodeKind::Symlink if policy.platform.symlinks => {
                observed.physical_kind == PhysicalKind::Symlink
            }
            InodeKind::Symlink => observed.physical_kind == PhysicalKind::Regular,
            InodeKind::Gitlink => observed.physical_kind == PhysicalKind::Directory,
        };
        if !kind_matches {
            report.push(
                EquivalenceLayer::Worktree,
                MismatchKind::Kind,
                Some(path.clone()),
                expected_entry.kind.to_string(),
                format!("{:?}", observed.physical_kind),
            );
            continue;
        }
        if expected_entry.kind == InodeKind::Gitlink {
            if observed.repository_kind != Some(InodeKind::Gitlink) {
                report.push(
                    EquivalenceLayer::Worktree,
                    MismatchKind::Kind,
                    Some(path.clone()),
                    InodeKind::Gitlink.to_string(),
                    format!("{:?}", observed.repository_kind),
                );
            }
            if observed.gitlink.as_ref() != expected_entry.gitlink.as_ref() {
                report.push(
                    EquivalenceLayer::Worktree,
                    MismatchKind::StaleContent,
                    Some(path.clone()),
                    format!("{:?}", expected_entry.gitlink),
                    format!("{:?}", observed.gitlink),
                );
            }
            continue;
        }
        let actual_bytes = observed.repository_bytes_after_clean.as_deref();
        if actual_bytes != Some(expected_entry.repository_bytes.as_slice()) {
            let kind = if expected_entry.kind == InodeKind::Symlink {
                MismatchKind::LinkTarget
            } else if actual_bytes.is_some_and(|bytes| {
                newline_normalized(bytes) == newline_normalized(&expected_entry.repository_bytes)
            }) {
                MismatchKind::NewlineOrFilterResult
            } else if expected_entry.repository_bytes.is_empty()
                || actual_bytes.is_some_and(<[u8]>::is_empty)
            {
                MismatchKind::EmptyFile
            } else {
                MismatchKind::StaleContent
            };
            report.push(
                EquivalenceLayer::Worktree,
                kind,
                Some(path.clone()),
                format_bytes(&expected_entry.repository_bytes),
                actual_bytes.map_or_else(|| "unavailable".into(), format_bytes),
            );
        }
        if policy.platform.executable_bit
            && expected_entry.kind == InodeKind::Regular
            && observed
                .mode
                .is_some_and(|mode| (mode & 0o111 != 0) != (expected_entry.mode & 0o111 != 0))
        {
            report.push(
                EquivalenceLayer::Worktree,
                MismatchKind::Mode,
                Some(path.clone()),
                format!("{:#o}", expected_entry.mode),
                format!("{:#o}", observed.mode.unwrap_or_default()),
            );
        }
    }
    for (path, observed) in actual {
        if observed.disposition == ManifestDisposition::Included && !expected.contains_key(&path) {
            report.push(
                EquivalenceLayer::Worktree,
                MismatchKind::ExtraPath,
                Some(path),
                "absent",
                "present",
            );
        }
    }
}

fn verify_claims_into(
    project: &ProjectTree,
    policy: &ConversionPolicy,
    claims: &EquivalenceClaims,
    report: &mut EquivalenceReport,
) {
    if claims
        .manifest_version
        .is_some_and(|version| version != project.manifest.version)
    {
        report.push(
            EquivalenceLayer::Joint,
            MismatchKind::ObjectHeader,
            None,
            project.manifest.version.to_string(),
            claims.manifest_version.unwrap_or_default().to_string(),
        );
    }
    if claims
        .object_algorithm
        .is_some_and(|algorithm| algorithm != project.git.algorithm)
    {
        report.push(
            EquivalenceLayer::Joint,
            MismatchKind::ObjectAlgorithm,
            None,
            format!("{:?}", project.git.algorithm),
            format!("{:?}", claims.object_algorithm),
        );
    }
    let actual_manifest = project.manifest.root().content_key;
    if claims
        .manifest_root
        .as_ref()
        .is_some_and(|root| root != &actual_manifest)
    {
        report.push(
            EquivalenceLayer::Joint,
            MismatchKind::ClaimedRootForgery,
            None,
            actual_manifest,
            claims.manifest_root.clone().unwrap_or_default(),
        );
    }
    let actual_policy = policy.root().content_key;
    if claims
        .conversion_policy_root
        .as_ref()
        .is_some_and(|root| root != &actual_policy)
    {
        report.push(
            EquivalenceLayer::Joint,
            MismatchKind::ClaimedRootForgery,
            None,
            actual_policy,
            claims.conversion_policy_root.clone().unwrap_or_default(),
        );
    }
    if claims
        .git_tree_root
        .as_ref()
        .is_some_and(|root| root != &project.git.root)
    {
        report.push(
            EquivalenceLayer::Joint,
            MismatchKind::ClaimedRootForgery,
            None,
            format!("{:?}", project.git.root),
            format!("{:?}", claims.git_tree_root),
        );
    }
}

fn expected_git_oid(
    entry: &RepositoryEntry,
    algorithm: GitHashAlgorithm,
) -> Result<GitObjectId, ProjectTreeError> {
    match entry.kind {
        InodeKind::Gitlink => entry
            .gitlink
            .clone()
            .ok_or_else(|| ProjectTreeError::InvalidGitlink("missing Git object identity".into())),
        InodeKind::Regular | InodeKind::Symlink => {
            git_object_id(algorithm, GitObjectKind::Blob, &entry.repository_bytes)
        }
    }
}

fn kind_from_git_mode(mode: u32) -> Option<InodeKind> {
    match mode {
        0o100644 | 0o100755 => Some(InodeKind::Regular),
        0o120000 => Some(InodeKind::Symlink),
        0o160000 => Some(InodeKind::Gitlink),
        _ => None,
    }
}

fn ascii_fold(path: &[u8]) -> Vec<u8> {
    path.iter().map(u8::to_ascii_lowercase).collect()
}

fn newline_normalized(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"\r\n") {
            normalized.push(b'\n');
            index += 2;
        } else {
            normalized.push(bytes[index]);
            index += 1;
        }
    }
    normalized
}

fn format_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}
