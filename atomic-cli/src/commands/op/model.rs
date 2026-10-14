use atomic_core::operation::{
    ActorRef, CheckpointKind, DigestKind, EffectPlan, EffectReceipt, EffectReceiptKind,
    EffectTarget, EffectValue, FileKind, GitHashAlgorithm, GitHeadState, GitObjectId,
    GitRefObservation, GitRefTarget, MetadataTarget, MetadataTransition, MetadataValue, Operation,
    OperationKind, OperationRelation, OperationScope, RepoStateRef, VerificationScope,
};
use atomic_core::types::Base32;
use atomic_repository::{
    OperationDetails, OperationHeadState, OperationLog, OperationLogEntry,
    OperationVerificationState,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum OperationScopeDto {
    Repository,
    WorkingCopy { id: String },
}

impl From<OperationScope> for OperationScopeDto {
    fn from(scope: OperationScope) -> Self {
        match scope {
            OperationScope::Repository => Self::Repository,
            OperationScope::WorkingCopy(id) => Self::WorkingCopy { id: id.to_string() },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum OperationHeadStateDto {
    Empty,
    Single { head: String },
    Diverged { heads: Vec<String> },
}

impl From<&OperationHeadState> for OperationHeadStateDto {
    fn from(state: &OperationHeadState) -> Self {
        match state {
            OperationHeadState::Empty => Self::Empty,
            OperationHeadState::Single(head) => Self::Single {
                head: head.to_string(),
            },
            OperationHeadState::Diverged(heads) => Self::Diverged {
                heads: heads.iter().map(ToString::to_string).collect(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum OperationRelationDto {
    Undo { target: String },
    Restore { target: String },
}

impl From<&OperationRelation> for OperationRelationDto {
    fn from(relation: &OperationRelation) -> Self {
        match relation {
            OperationRelation::Undo { target } => Self::Undo {
                target: target.to_string(),
            },
            OperationRelation::Restore { target } => Self::Restore {
                target: target.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct OperationLogDto {
    pub scope: OperationScopeDto,
    pub head_state: OperationHeadStateDto,
    pub entries: Vec<OperationLogEntryDto>,
}

impl From<&OperationLog> for OperationLogDto {
    fn from(log: &OperationLog) -> Self {
        Self {
            scope: log.scope.into(),
            head_state: (&log.head_state).into(),
            entries: log.entries.iter().map(OperationLogEntryDto::from).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct OperationLogEntryDto {
    pub id: String,
    pub encoding_version: u8,
    pub parents: Vec<String>,
    pub kind: String,
    pub relation: Option<OperationRelationDto>,
    pub scope: OperationScopeDto,
    pub actor: ActorDto,
    pub timestamp_ms: i64,
    pub timestamp_utc: Option<String>,
    pub verification: String,
    pub is_head: bool,
}

impl From<&OperationLogEntry> for OperationLogEntryDto {
    fn from(entry: &OperationLogEntry) -> Self {
        let operation = &entry.operation;
        let payload = operation.payload();
        Self {
            id: operation.id().to_string(),
            encoding_version: operation.encoding_version(),
            parents: payload.parents.iter().map(ToString::to_string).collect(),
            kind: operation_kind(payload.kind).to_string(),
            relation: payload.relation.as_ref().map(OperationRelationDto::from),
            scope: operation_scope(operation),
            actor: (&payload.actor).into(),
            timestamp_ms: payload.timestamp_ms,
            timestamp_utc: timestamp_utc(payload.timestamp_ms),
            verification: verification_state(entry.verification).to_string(),
            is_head: entry.is_head,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct OperationDetailsDto {
    pub id: String,
    pub encoding_version: u8,
    pub parents: Vec<String>,
    pub kind: String,
    pub relation: Option<OperationRelationDto>,
    pub scope: OperationScopeDto,
    pub actor: ActorDto,
    pub timestamp_ms: i64,
    pub timestamp_utc: Option<String>,
    pub verification: String,
    pub before: RepoStateDto,
    pub delta: RepoStateDeltaDto,
    pub git_observed: Vec<GitRefObservationDto>,
    pub evidence: Vec<String>,
    pub loss_notes: Vec<OperationLossNoteDto>,
    pub receipts: Vec<EffectReceiptDto>,
    pub head_scopes: Vec<OperationScopeDto>,
}

impl From<&OperationDetails> for OperationDetailsDto {
    fn from(details: &OperationDetails) -> Self {
        let operation = &details.operation;
        let payload = operation.payload();
        Self {
            id: operation.id().to_string(),
            encoding_version: operation.encoding_version(),
            parents: payload.parents.iter().map(ToString::to_string).collect(),
            kind: operation_kind(payload.kind).to_string(),
            relation: payload.relation.as_ref().map(OperationRelationDto::from),
            scope: operation_scope(operation),
            actor: (&payload.actor).into(),
            timestamp_ms: payload.timestamp_ms,
            timestamp_utc: timestamp_utc(payload.timestamp_ms),
            verification: verification_state(details.verification).to_string(),
            before: (&payload.before).into(),
            delta: RepoStateDeltaDto {
                after: (&payload.delta.after).into(),
                metadata: payload
                    .delta
                    .metadata
                    .iter()
                    .map(MetadataTransitionDto::from)
                    .collect(),
                effects: payload
                    .delta
                    .effects
                    .iter()
                    .map(EffectPlanDto::from)
                    .collect(),
            },
            git_observed: payload
                .git_observed
                .iter()
                .map(GitRefObservationDto::from)
                .collect(),
            evidence: payload.evidence.iter().map(Base32::to_base32).collect(),
            loss_notes: payload
                .lossy
                .iter()
                .map(|note| OperationLossNoteDto {
                    code: note.code.clone(),
                    message: note.message.clone(),
                    evidence: note.evidence.iter().map(Base32::to_base32).collect(),
                })
                .collect(),
            receipts: details
                .receipts
                .iter()
                .map(EffectReceiptDto::from)
                .collect(),
            head_scopes: details.head_of.iter().copied().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum ActorDto {
    Human {
        did: String,
    },
    Agent {
        did: String,
        session: String,
        turn: Option<u64>,
    },
    System {
        name: String,
    },
}

impl From<&ActorRef> for ActorDto {
    fn from(actor: &ActorRef) -> Self {
        match actor {
            ActorRef::Human { did } => Self::Human { did: did.clone() },
            ActorRef::Agent { did, session, turn } => Self::Agent {
                did: did.clone(),
                session: session.clone(),
                turn: *turn,
            },
            ActorRef::System { name } => Self::System { name: name.clone() },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct RepoStateDto {
    pub view: Option<ViewStateDto>,
    pub working_copy: Option<WorkingCopyStateDto>,
    pub git: Option<GitStateDto>,
}

impl From<&RepoStateRef> for RepoStateDto {
    fn from(state: &RepoStateRef) -> Self {
        Self {
            view: state.view.as_ref().map(|view| ViewStateDto {
                name: view.name.clone(),
                state: view.state.to_base32(),
                set_id: view.set_id.map(|id| id.to_string()),
            }),
            working_copy: state.working_copy.as_ref().map(WorkingCopyStateDto::from),
            git: state.git.as_ref().map(GitStateDto::from),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct RepoStateDeltaDto {
    pub after: RepoStateDto,
    pub metadata: Vec<MetadataTransitionDto>,
    pub effects: Vec<EffectPlanDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct MetadataTransitionDto {
    pub target: MetadataTargetDto,
    pub expected_old: MetadataValueDto,
    pub expected_new: MetadataValueDto,
}

impl From<&MetadataTransition> for MetadataTransitionDto {
    fn from(transition: &MetadataTransition) -> Self {
        Self {
            target: (&transition.target).into(),
            expected_old: (&transition.expected_old).into(),
            expected_new: (&transition.expected_new).into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum MetadataTargetDto {
    ViewChange { view: String, change: String },
    View { name: String },
    Tag { view: String, name: String },
    Remote { name: String },
    RefMapping { view: String },
    Capability { id: String },
}

impl From<&MetadataTarget> for MetadataTargetDto {
    fn from(target: &MetadataTarget) -> Self {
        match target {
            MetadataTarget::ViewChange { view, change } => Self::ViewChange {
                view: view.clone(),
                change: change.to_base32(),
            },
            MetadataTarget::View { name } => Self::View { name: name.clone() },
            MetadataTarget::Tag { view, name } => Self::Tag {
                view: view.clone(),
                name: name.clone(),
            },
            MetadataTarget::Remote { name } => Self::Remote { name: name.clone() },
            MetadataTarget::RefMapping { view } => Self::RefMapping { view: view.clone() },
            MetadataTarget::Capability { id } => Self::Capability { id: id.clone() },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum MetadataValueDto {
    Absent,
    Sequence { sequence: u64 },
    Digest { hash: String },
    Bytes { hex: String },
}

impl From<&MetadataValue> for MetadataValueDto {
    fn from(value: &MetadataValue) -> Self {
        match value {
            MetadataValue::Absent => Self::Absent,
            MetadataValue::Sequence(sequence) => Self::Sequence {
                sequence: *sequence,
            },
            MetadataValue::Digest(hash) => Self::Digest {
                hash: hash.to_base32(),
            },
            MetadataValue::Bytes(bytes) => Self::Bytes {
                hex: hex_bytes(bytes),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct ViewStateDto {
    pub name: String,
    pub state: String,
    pub set_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct WorkingCopyStateDto {
    pub id: String,
    pub location_fingerprint: String,
    pub desired_view: u64,
    pub desired_state: String,
    pub materialized_state: Option<String>,
    pub materialized_manifest: Option<String>,
}

impl From<&atomic_core::operation::WorkingCopyStateRef> for WorkingCopyStateDto {
    fn from(state: &atomic_core::operation::WorkingCopyStateRef) -> Self {
        Self {
            id: state.id.to_string(),
            location_fingerprint: state.location_fingerprint.to_base32(),
            desired_view: state.desired_view,
            desired_state: state.desired_state.to_base32(),
            materialized_state: state.materialized_state.as_ref().map(Base32::to_base32),
            materialized_manifest: state.materialized_manifest.as_ref().map(Base32::to_base32),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct GitStateDto {
    pub head: GitHeadStateDto,
    pub index: Option<GitIndexStateDto>,
    pub refs_digest: String,
}

impl From<&atomic_core::operation::GitStateRef> for GitStateDto {
    fn from(state: &atomic_core::operation::GitStateRef) -> Self {
        Self {
            head: (&state.head).into(),
            index: state.index.as_ref().map(GitIndexStateDto::from),
            refs_digest: state.refs_digest.to_base32(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum GitHeadStateDto {
    Attached { symref: String, oid: GitObjectDto },
    Detached { oid: GitObjectDto },
    Unborn { symref: String },
    MissingTarget { symref: String },
}

impl From<&GitHeadState> for GitHeadStateDto {
    fn from(head: &GitHeadState) -> Self {
        match head {
            GitHeadState::Attached { symref, oid } => Self::Attached {
                symref: symref.clone(),
                oid: oid.into(),
            },
            GitHeadState::Detached { oid } => Self::Detached { oid: oid.into() },
            GitHeadState::Unborn { symref } => Self::Unborn {
                symref: symref.clone(),
            },
            GitHeadState::MissingTarget { symref } => Self::MissingTarget {
                symref: symref.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct GitIndexStateDto {
    pub digest: String,
    pub tree: Option<GitObjectDto>,
}

impl From<&atomic_core::operation::GitIndexState> for GitIndexStateDto {
    fn from(index: &atomic_core::operation::GitIndexState) -> Self {
        Self {
            digest: index.digest.to_base32(),
            tree: index.tree.as_ref().map(GitObjectDto::from),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct GitObjectDto {
    pub algorithm: String,
    pub oid: String,
}

impl From<&GitObjectId> for GitObjectDto {
    fn from(object: &GitObjectId) -> Self {
        Self {
            algorithm: git_hash_algorithm(object.algorithm()).to_string(),
            oid: hex_bytes(object.as_bytes()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct GitRefObservationDto {
    pub name: String,
    pub target: Option<GitRefTargetDto>,
}

impl From<&GitRefObservation> for GitRefObservationDto {
    fn from(observation: &GitRefObservation) -> Self {
        Self {
            name: observation.name.clone(),
            target: observation.target.as_ref().map(GitRefTargetDto::from),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum GitRefTargetDto {
    Direct { object: GitObjectDto },
    Symbolic { name: String },
}

impl From<&GitRefTarget> for GitRefTargetDto {
    fn from(target: &GitRefTarget) -> Self {
        match target {
            GitRefTarget::Direct(object) => Self::Direct {
                object: object.into(),
            },
            GitRefTarget::Symbolic(name) => Self::Symbolic { name: name.clone() },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct OperationLossNoteDto {
    pub code: String,
    pub message: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct EffectPlanDto {
    pub ordinal: u32,
    pub target: EffectTargetDto,
    pub expected_old: EffectValueDto,
    pub expected_new: EffectValueDto,
}

impl From<&EffectPlan> for EffectPlanDto {
    fn from(effect: &EffectPlan) -> Self {
        Self {
            ordinal: effect.ordinal,
            target: (&effect.target).into(),
            expected_old: (&effect.expected_old).into(),
            expected_new: (&effect.expected_new).into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum EffectTargetDto {
    FilesystemPath {
        path: String,
    },
    WorkspacePath {
        working_copy: String,
        path: String,
    },
    ShelfPath {
        working_copy: String,
        view: String,
        path: String,
    },
    GitObject {
        object: GitObjectDto,
    },
    GitIndex {
        working_copy: String,
    },
    GitRef {
        name: String,
    },
    GitHead {
        working_copy: String,
    },
    Checkpoint {
        working_copy: String,
        checkpoint_kind: String,
    },
    WorkingCopy {
        working_copy: String,
    },
    Verification {
        working_copy: Option<String>,
        scope: String,
    },
}

impl From<&EffectTarget> for EffectTargetDto {
    fn from(target: &EffectTarget) -> Self {
        match target {
            EffectTarget::FilesystemPath { path } => Self::FilesystemPath { path: path.clone() },
            EffectTarget::WorkspacePath { working_copy, path } => Self::WorkspacePath {
                working_copy: working_copy.to_string(),
                path: path.clone(),
            },
            EffectTarget::ShelfPath {
                working_copy,
                view,
                path,
            } => Self::ShelfPath {
                working_copy: working_copy.to_string(),
                view: view.clone(),
                path: path.clone(),
            },
            EffectTarget::GitObject { object } => Self::GitObject {
                object: object.into(),
            },
            EffectTarget::GitIndex { working_copy } => Self::GitIndex {
                working_copy: working_copy.to_string(),
            },
            EffectTarget::GitRef { name } => Self::GitRef { name: name.clone() },
            EffectTarget::GitHead { working_copy } => Self::GitHead {
                working_copy: working_copy.to_string(),
            },
            EffectTarget::Checkpoint { working_copy, kind } => Self::Checkpoint {
                working_copy: working_copy.to_string(),
                checkpoint_kind: checkpoint_kind(*kind).to_string(),
            },
            EffectTarget::WorkingCopy { working_copy } => Self::WorkingCopy {
                working_copy: working_copy.to_string(),
            },
            EffectTarget::Verification {
                working_copy,
                scope,
            } => Self::Verification {
                working_copy: working_copy.map(|id| id.to_string()),
                scope: verification_scope(*scope).to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum EffectValueDto {
    Absent,
    Digest { digest_kind: String, hash: String },
    File { file: FileStateDto },
    GitObject { object: GitObjectDto },
    GitRef { target: GitRefTargetDto },
    GitIndex { index: GitIndexStateDto },
    WorkingCopy { state: WorkingCopyStateDto },
    Verification { hash: String },
}

impl From<&EffectValue> for EffectValueDto {
    fn from(value: &EffectValue) -> Self {
        match value {
            EffectValue::Absent => Self::Absent,
            EffectValue::Digest { kind, hash } => Self::Digest {
                digest_kind: digest_kind(*kind).to_string(),
                hash: hash.to_base32(),
            },
            EffectValue::File(file) => Self::File {
                file: FileStateDto {
                    kind: file_kind(file.kind).to_string(),
                    mode: file.mode,
                    content: file.content.to_base32(),
                },
            },
            EffectValue::GitObject(object) => Self::GitObject {
                object: object.into(),
            },
            EffectValue::GitRef(target) => Self::GitRef {
                target: target.into(),
            },
            EffectValue::GitIndex(index) => Self::GitIndex {
                index: index.into(),
            },
            EffectValue::WorkingCopy(state) => Self::WorkingCopy {
                state: state.into(),
            },
            EffectValue::Verification(hash) => Self::Verification {
                hash: hash.to_base32(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct FileStateDto {
    pub kind: String,
    pub mode: u32,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct EffectReceiptDto {
    pub id: String,
    pub operation: String,
    pub effect_ordinal: Option<u32>,
    pub attempt: u32,
    pub kind: String,
    pub observed_old: Option<EffectValueDto>,
    pub observed_new: Option<EffectValueDto>,
    pub timestamp_ms: i64,
    pub timestamp_utc: Option<String>,
}

impl From<&EffectReceipt> for EffectReceiptDto {
    fn from(receipt: &EffectReceipt) -> Self {
        let payload = receipt.payload();
        Self {
            id: receipt.id().to_string(),
            operation: payload.operation.to_string(),
            effect_ordinal: payload.effect_ordinal,
            attempt: payload.attempt,
            kind: receipt_kind(payload.kind).to_string(),
            observed_old: payload.observed_old.as_ref().map(EffectValueDto::from),
            observed_new: payload.observed_new.as_ref().map(EffectValueDto::from),
            timestamp_ms: payload.timestamp_ms,
            timestamp_utc: timestamp_utc(payload.timestamp_ms),
        }
    }
}

fn operation_scope(operation: &Operation) -> OperationScopeDto {
    operation
        .payload()
        .working_copy
        .map(OperationScope::WorkingCopy)
        .unwrap_or(OperationScope::Repository)
        .into()
}

fn timestamp_utc(timestamp_ms: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(timestamp_ms)
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn operation_kind(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::Anchor => "anchor",
        OperationKind::Record => "record",
        OperationKind::PromoteSnapshot => "promote_snapshot",
        OperationKind::SplitSnapshot => "split_snapshot",
        OperationKind::SwitchView => "switch_view",
        OperationKind::Materialize => "materialize",
        OperationKind::ImportGitHead => "import_git_head",
        OperationKind::ImportGitRefs => "import_git_refs",
        OperationKind::ExportGitRefs => "export_git_refs",
        OperationKind::ProjectState => "project_state",
        OperationKind::SynthesizeGit => "synthesize_git",
        OperationKind::ResurrectBinding => "resurrect_binding",
        OperationKind::Insert => "insert",
        OperationKind::Unrecord => "unrecord",
        OperationKind::Tag => "tag",
        OperationKind::Recover => "recover",
        OperationKind::Undo => "undo",
        OperationKind::Gc => "gc",
        OperationKind::Restore => "restore",
        OperationKind::Pull => "pull",
        OperationKind::Push => "push",
        OperationKind::Consolidate => "consolidate",
        OperationKind::RefMapping => "ref_mapping",
        OperationKind::Repair => "repair",
        OperationKind::Cutover => "cutover",
        OperationKind::ReconcileWorkingCopy => "reconcile_working_copy",
    }
}

fn verification_state(state: OperationVerificationState) -> &'static str {
    match state {
        OperationVerificationState::Prepared => "prepared",
        OperationVerificationState::InProgress => "in_progress",
        OperationVerificationState::LeaseRejected => "lease_rejected",
        OperationVerificationState::Verified => "verified",
    }
}

fn git_hash_algorithm(algorithm: GitHashAlgorithm) -> &'static str {
    match algorithm {
        GitHashAlgorithm::Sha1 => "sha1",
        GitHashAlgorithm::Sha256 => "sha256",
    }
}

fn checkpoint_kind(kind: CheckpointKind) -> &'static str {
    match kind {
        CheckpointKind::Bridge => "bridge",
        CheckpointKind::WorkingCopyCompatibility => "working_copy_compatibility",
        CheckpointKind::Recovery => "recovery",
    }
}

fn verification_scope(scope: VerificationScope) -> &'static str {
    match scope {
        VerificationScope::Filesystem => "filesystem",
        VerificationScope::Git => "git",
        VerificationScope::Repository => "repository",
        VerificationScope::Complete => "complete",
    }
}

fn digest_kind(kind: DigestKind) -> &'static str {
    match kind {
        DigestKind::Bytes => "bytes",
        DigestKind::Manifest => "manifest",
        DigestKind::Checkpoint => "checkpoint",
        DigestKind::Shelf => "shelf",
        DigestKind::Refs => "refs",
    }
}

fn file_kind(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Regular => "regular",
        FileKind::Directory => "directory",
        FileKind::Symlink => "symlink",
        FileKind::Gitlink => "gitlink",
    }
}

fn receipt_kind(kind: EffectReceiptKind) -> &'static str {
    match kind {
        EffectReceiptKind::Applied => "applied",
        EffectReceiptKind::Verified => "verified",
        EffectReceiptKind::RolledBack => "rolled_back",
        EffectReceiptKind::LeaseRejected => "lease_rejected",
        EffectReceiptKind::Recovered => "recovered",
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}
