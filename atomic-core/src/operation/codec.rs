use super::*;

/// Original canonical operation payload version.
pub const OPERATION_VERSION_V1: u8 = 1;
/// Canonical operation payload version with relations and metadata transitions.
pub const OPERATION_VERSION_V2: u8 = 2;
/// Current canonical operation payload version.
pub const OPERATION_VERSION: u8 = OPERATION_VERSION_V2;
/// Current canonical effect-receipt payload version.
pub const EFFECT_RECEIPT_VERSION: u8 = 1;
/// Current canonical operation-head-set version.
pub const OPERATION_HEADS_VERSION: u8 = 1;

const OPERATION_SCOPE_REPOSITORY: u8 = 0;
const OPERATION_SCOPE_WORKING_COPY: u8 = 1;

/// Encode an operation's immutable payload and verify its derived ID.
pub fn encode_operation(operation: &Operation) -> Result<Vec<u8>, OperationCodecError> {
    let bytes = encode_operation_payload(operation.payload(), operation.encoding_version())?;
    let actual = OperationId::from_canonical_bytes(&bytes);
    if actual != operation.id() {
        return Err(OperationCodecError::new(format!(
            "operation ID mismatch: stored {}, computed {}",
            operation.id(),
            actual
        )));
    }
    Ok(bytes)
}

pub(crate) fn encode_operation_payload(
    payload: &OperationPayload,
    encoding_version: u8,
) -> Result<Vec<u8>, OperationCodecError> {
    payload.validate_canonical()?;
    payload.validate_encoding_version(encoding_version)?;
    let mut encoder = Encoder::new();
    encoder.put_u8(encoding_version)?;
    encoder.put_count(payload.parents.len(), "operation parents")?;
    for parent in &payload.parents {
        encoder.put_operation_id(*parent)?;
    }
    encoder.put_operation_kind(payload.kind)?;
    if encoding_version == OPERATION_VERSION_V2 {
        encoder.put_optional_tag(payload.relation.is_some())?;
        if let Some(relation) = &payload.relation {
            encoder.put_operation_relation(relation)?;
        }
    }
    encoder.put_optional_working_copy(payload.working_copy)?;
    encoder.put_repo_state(&payload.before)?;
    encoder.put_repo_state_delta(&payload.delta, encoding_version)?;
    encoder.put_count(payload.git_observed.len(), "Git ref observations")?;
    for observation in &payload.git_observed {
        encoder.put_git_ref_observation(observation)?;
    }
    encoder.put_count(payload.evidence.len(), "operation evidence")?;
    for evidence in &payload.evidence {
        encoder.put_hash(*evidence)?;
    }
    encoder.put_actor(&payload.actor)?;
    encoder.put_i64(payload.timestamp_ms)?;
    encoder.put_count(payload.lossy.len(), "operation loss notes")?;
    for note in &payload.lossy {
        encoder.put_loss_note(note)?;
    }
    encoder.finish()
}

/// Decode strict canonical operation bytes and recompute the derived ID.
pub fn decode_operation(bytes: &[u8]) -> Result<Operation, OperationCodecError> {
    let mut decoder = Decoder::new(bytes)?;
    let encoding_version = decoder.read_u8()?;
    if !matches!(
        encoding_version,
        OPERATION_VERSION_V1 | OPERATION_VERSION_V2
    ) {
        return Err(OperationCodecError::new(format!(
            "unsupported operation version {encoding_version} (maximum supported version {OPERATION_VERSION})"
        )));
    }
    let parent_count = decoder.read_count("operation parents")?;
    let mut parents = Vec::with_capacity(parent_count);
    for _ in 0..parent_count {
        parents.push(decoder.read_operation_id()?);
    }
    let kind = decoder.read_operation_kind(encoding_version)?;
    let relation = if encoding_version == OPERATION_VERSION_V2
        && decoder.read_optional_tag("operation relation")?
    {
        Some(decoder.read_operation_relation()?)
    } else {
        None
    };
    let working_copy = decoder.read_optional_working_copy()?;
    let before = decoder.read_repo_state()?;
    let delta = decoder.read_repo_state_delta(encoding_version)?;
    let observation_count = decoder.read_count("Git ref observations")?;
    let mut git_observed = Vec::with_capacity(observation_count);
    for _ in 0..observation_count {
        git_observed.push(decoder.read_git_ref_observation()?);
    }
    let evidence_count = decoder.read_count("operation evidence")?;
    let mut evidence = Vec::with_capacity(evidence_count);
    for _ in 0..evidence_count {
        evidence.push(decoder.read_hash()?);
    }
    let actor = decoder.read_actor()?;
    let timestamp_ms = decoder.read_i64()?;
    let loss_count = decoder.read_count("operation loss notes")?;
    let mut lossy = Vec::with_capacity(loss_count);
    for _ in 0..loss_count {
        lossy.push(decoder.read_loss_note()?);
    }
    decoder.finish()?;

    let payload = OperationPayload {
        parents,
        kind,
        relation,
        working_copy,
        before,
        delta,
        git_observed,
        evidence,
        actor,
        timestamp_ms,
        lossy,
    };
    payload.validate_canonical()?;
    payload.validate_encoding_version(encoding_version)?;
    let canonical = encode_operation_payload(&payload, encoding_version)?;
    if canonical != bytes {
        return Err(OperationCodecError::new(
            "operation payload is not canonically encoded",
        ));
    }
    let id = OperationId::from_canonical_bytes(bytes);
    Operation::from_canonical_payload(id, payload, encoding_version)
}

/// Encode an immutable effect receipt and verify its derived ID.
pub fn encode_effect_receipt(receipt: &EffectReceipt) -> Result<Vec<u8>, OperationCodecError> {
    let bytes = encode_effect_receipt_payload(receipt.payload())?;
    let actual = EffectReceiptId::from_canonical_bytes(&bytes);
    if actual != receipt.id() {
        return Err(OperationCodecError::new(format!(
            "effect receipt ID mismatch: stored {}, computed {}",
            receipt.id(),
            actual
        )));
    }
    Ok(bytes)
}

pub(crate) fn encode_effect_receipt_payload(
    payload: &EffectReceiptPayload,
) -> Result<Vec<u8>, OperationCodecError> {
    payload.validate()?;
    let mut encoder = Encoder::new();
    encoder.put_u8(EFFECT_RECEIPT_VERSION)?;
    encoder.put_operation_id(payload.operation)?;
    encoder.put_optional_u32(payload.effect_ordinal)?;
    encoder.put_u32(payload.attempt)?;
    encoder.put_receipt_kind(payload.kind)?;
    encoder.put_optional_effect_value(payload.observed_old.as_ref())?;
    encoder.put_optional_effect_value(payload.observed_new.as_ref())?;
    encoder.put_i64(payload.timestamp_ms)?;
    encoder.finish()
}

/// Decode strict canonical receipt bytes and recompute the derived ID.
pub fn decode_effect_receipt(bytes: &[u8]) -> Result<EffectReceipt, OperationCodecError> {
    let mut decoder = Decoder::new(bytes)?;
    decoder.expect_version(EFFECT_RECEIPT_VERSION, "effect receipt")?;
    let operation = decoder.read_operation_id()?;
    let effect_ordinal = decoder.read_optional_u32()?;
    let attempt = decoder.read_u32()?;
    let kind = decoder.read_receipt_kind()?;
    let observed_old = decoder.read_optional_effect_value()?;
    let observed_new = decoder.read_optional_effect_value()?;
    let timestamp_ms = decoder.read_i64()?;
    decoder.finish()?;

    let payload = EffectReceiptPayload {
        operation,
        effect_ordinal,
        attempt,
        kind,
        observed_old,
        observed_new,
        timestamp_ms,
    };
    payload.validate()?;
    let canonical = encode_effect_receipt_payload(&payload)?;
    if canonical != bytes {
        return Err(OperationCodecError::new(
            "effect receipt payload is not canonically encoded",
        ));
    }
    let id = EffectReceiptId::from_canonical_bytes(bytes);
    EffectReceipt::from_canonical_payload(id, payload)
}

/// Encode a versioned canonical sorted multi-head value.
pub fn encode_operation_heads(heads: &OperationHeads) -> Result<Vec<u8>, OperationCodecError> {
    ensure_strictly_sorted(heads.as_slice(), "operation heads")?;
    let mut encoder = Encoder::new();
    encoder.put_u8(OPERATION_HEADS_VERSION)?;
    encoder.put_count(heads.as_slice().len(), "operation heads")?;
    for head in heads.as_slice() {
        encoder.put_operation_id(*head)?;
    }
    encoder.finish()
}

/// Decode a versioned canonical sorted multi-head value.
pub fn decode_operation_heads(bytes: &[u8]) -> Result<OperationHeads, OperationCodecError> {
    let mut decoder = Decoder::new(bytes)?;
    decoder.expect_version(OPERATION_HEADS_VERSION, "operation heads")?;
    let count = decoder.read_count("operation heads")?;
    let mut heads = Vec::with_capacity(count);
    for _ in 0..count {
        heads.push(decoder.read_operation_id()?);
    }
    decoder.finish()?;
    let heads = OperationHeads::from_canonical(heads)?;
    if encode_operation_heads(&heads)? != bytes {
        return Err(OperationCodecError::new(
            "operation heads are not canonically encoded",
        ));
    }
    Ok(heads)
}

/// Encode a fixed-width operation-head scope key.
pub fn encode_operation_scope(scope: OperationScope) -> [u8; 17] {
    let mut key = [0u8; 17];
    match scope {
        OperationScope::Repository => key[0] = OPERATION_SCOPE_REPOSITORY,
        OperationScope::WorkingCopy(id) => {
            key[0] = OPERATION_SCOPE_WORKING_COPY;
            key[1..].copy_from_slice(id.as_bytes());
        }
    }
    key
}

/// Decode and validate a fixed-width operation-head scope key.
pub fn decode_operation_scope(bytes: &[u8; 17]) -> Result<OperationScope, OperationCodecError> {
    match bytes[0] {
        OPERATION_SCOPE_REPOSITORY => {
            if bytes[1..].iter().any(|byte| *byte != 0) {
                return Err(OperationCodecError::new(
                    "repository operation scope contains non-zero payload bytes",
                ));
            }
            Ok(OperationScope::Repository)
        }
        OPERATION_SCOPE_WORKING_COPY => Ok(OperationScope::WorkingCopy(WorkingCopyId::from_bytes(
            bytes[1..]
                .try_into()
                .map_err(|_| OperationCodecError::new("invalid working-copy operation scope"))?,
        ))),
        tag => Err(OperationCodecError::new(format!(
            "unsupported operation scope tag {tag}"
        ))),
    }
}

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn finish(self) -> Result<Vec<u8>, OperationCodecError> {
        if self.bytes.len() > MAX_OPERATION_OBJECT_BYTES {
            return Err(OperationCodecError::new(format!(
                "canonical operation object exceeds {MAX_OPERATION_OBJECT_BYTES} bytes"
            )));
        }
        Ok(self.bytes)
    }

    fn reserve(&self, additional: usize) -> Result<(), OperationCodecError> {
        let total = self
            .bytes
            .len()
            .checked_add(additional)
            .ok_or_else(|| OperationCodecError::new("canonical object length overflow"))?;
        if total > MAX_OPERATION_OBJECT_BYTES {
            return Err(OperationCodecError::new(format!(
                "canonical operation object exceeds {MAX_OPERATION_OBJECT_BYTES} bytes"
            )));
        }
        Ok(())
    }

    fn put_bytes(&mut self, bytes: &[u8]) -> Result<(), OperationCodecError> {
        self.reserve(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn put_u8(&mut self, value: u8) -> Result<(), OperationCodecError> {
        self.put_bytes(&[value])
    }

    fn put_u32(&mut self, value: u32) -> Result<(), OperationCodecError> {
        self.put_bytes(&value.to_le_bytes())
    }

    fn put_u64(&mut self, value: u64) -> Result<(), OperationCodecError> {
        self.put_bytes(&value.to_le_bytes())
    }

    fn put_i64(&mut self, value: i64) -> Result<(), OperationCodecError> {
        self.put_bytes(&value.to_le_bytes())
    }

    fn put_count(&mut self, count: usize, field: &str) -> Result<(), OperationCodecError> {
        if count > MAX_OPERATION_COLLECTION_ITEMS {
            return Err(OperationCodecError::new(format!(
                "{field} has {count} items, maximum is {MAX_OPERATION_COLLECTION_ITEMS}"
            )));
        }
        let count = u32::try_from(count)
            .map_err(|_| OperationCodecError::new(format!("{field} count exceeds u32")))?;
        self.put_u32(count)
    }

    fn put_string(&mut self, value: &str, field: &str) -> Result<(), OperationCodecError> {
        if value.is_empty() {
            return Err(OperationCodecError::new(format!("{field} cannot be empty")));
        }
        if value.len() > MAX_OPERATION_STRING_BYTES {
            return Err(OperationCodecError::new(format!(
                "{field} exceeds {MAX_OPERATION_STRING_BYTES} UTF-8 bytes"
            )));
        }
        let len = u32::try_from(value.len())
            .map_err(|_| OperationCodecError::new(format!("{field} length exceeds u32")))?;
        self.put_u32(len)?;
        self.put_bytes(value.as_bytes())
    }

    fn put_blob(&mut self, value: &[u8], field: &str) -> Result<(), OperationCodecError> {
        if value.len() > MAX_OPERATION_OBJECT_BYTES {
            return Err(OperationCodecError::new(format!(
                "{field} exceeds {MAX_OPERATION_OBJECT_BYTES} bytes"
            )));
        }
        let len = u32::try_from(value.len())
            .map_err(|_| OperationCodecError::new(format!("{field} length exceeds u32")))?;
        self.put_u32(len)?;
        self.put_bytes(value)
    }

    fn put_hash(&mut self, value: Hash) -> Result<(), OperationCodecError> {
        self.put_bytes(value.as_bytes())
    }

    fn put_set_id(&mut self, value: SetId) -> Result<(), OperationCodecError> {
        self.put_bytes(value.as_bytes())
    }

    fn put_operation_id(&mut self, value: OperationId) -> Result<(), OperationCodecError> {
        self.put_bytes(value.as_bytes())
    }

    fn put_working_copy(&mut self, value: WorkingCopyId) -> Result<(), OperationCodecError> {
        self.put_bytes(value.as_bytes())
    }

    fn put_optional_tag(&mut self, present: bool) -> Result<(), OperationCodecError> {
        self.put_u8(u8::from(present))
    }

    fn put_optional_u32(&mut self, value: Option<u32>) -> Result<(), OperationCodecError> {
        self.put_optional_tag(value.is_some())?;
        if let Some(value) = value {
            self.put_u32(value)?;
        }
        Ok(())
    }

    fn put_optional_hash(&mut self, value: Option<Hash>) -> Result<(), OperationCodecError> {
        self.put_optional_tag(value.is_some())?;
        if let Some(value) = value {
            self.put_hash(value)?;
        }
        Ok(())
    }

    fn put_optional_set_id(&mut self, value: Option<SetId>) -> Result<(), OperationCodecError> {
        self.put_optional_tag(value.is_some())?;
        if let Some(value) = value {
            self.put_set_id(value)?;
        }
        Ok(())
    }

    fn put_optional_working_copy(
        &mut self,
        value: Option<WorkingCopyId>,
    ) -> Result<(), OperationCodecError> {
        self.put_optional_tag(value.is_some())?;
        if let Some(value) = value {
            self.put_working_copy(value)?;
        }
        Ok(())
    }

    fn put_operation_kind(&mut self, kind: OperationKind) -> Result<(), OperationCodecError> {
        self.put_u8(match kind {
            OperationKind::Anchor => 0,
            OperationKind::Record => 1,
            OperationKind::PromoteSnapshot => 2,
            OperationKind::SplitSnapshot => 3,
            OperationKind::SwitchView => 4,
            OperationKind::Materialize => 5,
            OperationKind::ImportGitHead => 6,
            OperationKind::ImportGitRefs => 7,
            OperationKind::ExportGitRefs => 8,
            OperationKind::ProjectState => 9,
            OperationKind::SynthesizeGit => 10,
            OperationKind::ResurrectBinding => 11,
            OperationKind::Insert => 12,
            OperationKind::Unrecord => 13,
            OperationKind::Tag => 14,
            OperationKind::Recover => 15,
            OperationKind::Undo => 16,
            OperationKind::Gc => 17,
            OperationKind::Restore => 18,
            OperationKind::Pull => 19,
            OperationKind::Push => 20,
            OperationKind::Consolidate => 21,
            OperationKind::RefMapping => 22,
            OperationKind::Repair => 23,
            OperationKind::Cutover => 24,
            OperationKind::ReconcileWorkingCopy => 25,
        })
    }

    fn put_operation_relation(
        &mut self,
        relation: &OperationRelation,
    ) -> Result<(), OperationCodecError> {
        match relation {
            OperationRelation::Undo { target } => {
                self.put_u8(0)?;
                self.put_operation_id(*target)
            }
            OperationRelation::Restore { target } => {
                self.put_u8(1)?;
                self.put_operation_id(*target)
            }
        }
    }

    fn put_repo_state(&mut self, state: &RepoStateRef) -> Result<(), OperationCodecError> {
        self.put_optional_tag(state.view.is_some())?;
        if let Some(view) = &state.view {
            self.put_view_state(view)?;
        }
        self.put_optional_tag(state.working_copy.is_some())?;
        if let Some(working_copy) = &state.working_copy {
            self.put_working_copy_state(working_copy)?;
        }
        self.put_optional_tag(state.git.is_some())?;
        if let Some(git) = &state.git {
            self.put_git_state(git)?;
        }
        Ok(())
    }

    fn put_repo_state_delta(
        &mut self,
        delta: &RepoStateDelta,
        encoding_version: u8,
    ) -> Result<(), OperationCodecError> {
        delta.validate_canonical()?;
        self.put_repo_state(&delta.after)?;
        if encoding_version == OPERATION_VERSION_V2 {
            self.put_count(delta.metadata.len(), "metadata transitions")?;
            for transition in &delta.metadata {
                self.put_metadata_transition(transition)?;
            }
        }
        self.put_count(delta.effects.len(), "operation effects")?;
        for effect in &delta.effects {
            self.put_effect_plan(effect)?;
        }
        Ok(())
    }

    fn put_metadata_transition(
        &mut self,
        transition: &MetadataTransition,
    ) -> Result<(), OperationCodecError> {
        self.put_metadata_target(&transition.target)?;
        self.put_metadata_value(&transition.expected_old)?;
        self.put_metadata_value(&transition.expected_new)
    }

    fn put_metadata_target(&mut self, target: &MetadataTarget) -> Result<(), OperationCodecError> {
        match target {
            MetadataTarget::ViewChange { view, change } => {
                self.put_u8(0)?;
                self.put_string(view, "metadata view-change view")?;
                self.put_hash(*change)
            }
            MetadataTarget::View { name } => {
                self.put_u8(1)?;
                self.put_string(name, "metadata view name")
            }
            MetadataTarget::Tag { view, name } => {
                self.put_u8(2)?;
                self.put_string(view, "metadata tag view")?;
                self.put_string(name, "metadata tag name")
            }
            MetadataTarget::Remote { name } => {
                self.put_u8(3)?;
                self.put_string(name, "metadata remote name")
            }
            MetadataTarget::RefMapping { view } => {
                self.put_u8(4)?;
                self.put_string(view, "metadata ref-mapping view")
            }
            MetadataTarget::Capability { id } => {
                self.put_u8(5)?;
                self.put_string(id, "metadata capability id")
            }
        }
    }

    fn put_metadata_value(&mut self, value: &MetadataValue) -> Result<(), OperationCodecError> {
        match value {
            MetadataValue::Absent => self.put_u8(0),
            MetadataValue::Sequence(sequence) => {
                self.put_u8(1)?;
                self.put_u64(*sequence)
            }
            MetadataValue::Digest(hash) => {
                self.put_u8(2)?;
                self.put_hash(*hash)
            }
            MetadataValue::Bytes(bytes) => {
                self.put_u8(3)?;
                self.put_blob(bytes, "metadata bytes")
            }
        }
    }

    fn put_view_state(&mut self, state: &ViewStateRef) -> Result<(), OperationCodecError> {
        self.put_string(&state.name, "view name")?;
        self.put_hash(state.state)?;
        self.put_optional_set_id(state.set_id)
    }

    fn put_working_copy_state(
        &mut self,
        state: &WorkingCopyStateRef,
    ) -> Result<(), OperationCodecError> {
        if state.desired_view == 0 {
            return Err(OperationCodecError::new(
                "working-copy desired view must be non-zero",
            ));
        }
        if state.materialized_manifest.is_some() && state.materialized_state.is_none() {
            return Err(OperationCodecError::new(
                "working-copy materialized manifest requires materialized state",
            ));
        }
        self.put_working_copy(state.id)?;
        self.put_hash(state.location_fingerprint)?;
        self.put_u64(state.desired_view)?;
        self.put_hash(state.desired_state)?;
        self.put_optional_hash(state.materialized_state)?;
        self.put_optional_hash(state.materialized_manifest)
    }

    fn put_git_state(&mut self, state: &GitStateRef) -> Result<(), OperationCodecError> {
        self.put_git_head(&state.head)?;
        self.put_optional_tag(state.index.is_some())?;
        if let Some(index) = &state.index {
            self.put_git_index(index)?;
        }
        self.put_hash(state.refs_digest)
    }

    fn put_git_head(&mut self, state: &GitHeadState) -> Result<(), OperationCodecError> {
        match state {
            GitHeadState::Attached { symref, oid } => {
                self.put_u8(0)?;
                self.put_string(symref, "Git HEAD symref")?;
                self.put_git_object_id(oid)
            }
            GitHeadState::Detached { oid } => {
                self.put_u8(1)?;
                self.put_git_object_id(oid)
            }
            GitHeadState::Unborn { symref } => {
                self.put_u8(2)?;
                self.put_string(symref, "Git HEAD symref")
            }
            GitHeadState::MissingTarget { symref } => {
                self.put_u8(3)?;
                self.put_string(symref, "Git HEAD symref")
            }
        }
    }

    fn put_git_object_id(&mut self, oid: &GitObjectId) -> Result<(), OperationCodecError> {
        self.put_u8(match oid.algorithm() {
            GitHashAlgorithm::Sha1 => 0,
            GitHashAlgorithm::Sha256 => 1,
        })?;
        self.put_bytes(oid.as_bytes())
    }

    fn put_git_index(&mut self, state: &GitIndexState) -> Result<(), OperationCodecError> {
        self.put_hash(state.digest)?;
        self.put_optional_tag(state.tree.is_some())?;
        if let Some(tree) = &state.tree {
            self.put_git_object_id(tree)?;
        }
        Ok(())
    }

    fn put_git_ref_target(&mut self, target: &GitRefTarget) -> Result<(), OperationCodecError> {
        match target {
            GitRefTarget::Direct(oid) => {
                self.put_u8(0)?;
                self.put_git_object_id(oid)
            }
            GitRefTarget::Symbolic(name) => {
                self.put_u8(1)?;
                self.put_string(name, "Git symbolic ref target")
            }
        }
    }

    fn put_git_ref_observation(
        &mut self,
        observation: &GitRefObservation,
    ) -> Result<(), OperationCodecError> {
        self.put_string(&observation.name, "Git ref name")?;
        self.put_optional_tag(observation.target.is_some())?;
        if let Some(target) = &observation.target {
            self.put_git_ref_target(target)?;
        }
        Ok(())
    }

    fn put_actor(&mut self, actor: &ActorRef) -> Result<(), OperationCodecError> {
        match actor {
            ActorRef::Human { did } => {
                self.put_u8(0)?;
                self.put_string(did, "human actor DID")
            }
            ActorRef::Agent { did, session, turn } => {
                self.put_u8(1)?;
                self.put_string(did, "agent actor DID")?;
                self.put_string(session, "agent session")?;
                self.put_optional_tag(turn.is_some())?;
                if let Some(turn) = turn {
                    self.put_u64(*turn)?;
                }
                Ok(())
            }
            ActorRef::System { name } => {
                self.put_u8(2)?;
                self.put_string(name, "system actor name")
            }
        }
    }

    fn put_loss_note(&mut self, note: &OperationLossNote) -> Result<(), OperationCodecError> {
        ensure_strictly_sorted(&note.evidence, "loss-note evidence")?;
        self.put_string(&note.code, "loss-note code")?;
        self.put_string(&note.message, "loss-note message")?;
        self.put_count(note.evidence.len(), "loss-note evidence")?;
        for evidence in &note.evidence {
            self.put_hash(*evidence)?;
        }
        Ok(())
    }

    fn put_effect_plan(&mut self, effect: &EffectPlan) -> Result<(), OperationCodecError> {
        self.put_u32(effect.ordinal)?;
        self.put_effect_target(&effect.target)?;
        self.put_effect_value(&effect.expected_old)?;
        self.put_effect_value(&effect.expected_new)
    }

    fn put_effect_target(&mut self, target: &EffectTarget) -> Result<(), OperationCodecError> {
        match target {
            EffectTarget::FilesystemPath { path } => {
                self.put_u8(0)?;
                self.put_string(path, "filesystem path")
            }
            EffectTarget::ShelfPath {
                working_copy,
                view,
                path,
            } => {
                self.put_u8(1)?;
                self.put_working_copy(*working_copy)?;
                self.put_string(view, "shelf view")?;
                self.put_string(path, "shelf path")
            }
            EffectTarget::GitObject { object } => {
                self.put_u8(2)?;
                self.put_git_object_id(object)
            }
            EffectTarget::GitIndex { working_copy } => {
                self.put_u8(3)?;
                self.put_working_copy(*working_copy)
            }
            EffectTarget::GitRef { name } => {
                self.put_u8(4)?;
                self.put_string(name, "Git ref name")
            }
            EffectTarget::GitHead { working_copy } => {
                self.put_u8(5)?;
                self.put_working_copy(*working_copy)
            }
            EffectTarget::Checkpoint { working_copy, kind } => {
                self.put_u8(6)?;
                self.put_working_copy(*working_copy)?;
                self.put_checkpoint_kind(*kind)
            }
            EffectTarget::WorkingCopy { working_copy } => {
                self.put_u8(7)?;
                self.put_working_copy(*working_copy)
            }
            EffectTarget::Verification {
                working_copy,
                scope,
            } => {
                self.put_u8(8)?;
                self.put_optional_working_copy(*working_copy)?;
                self.put_verification_scope(*scope)
            }
            EffectTarget::WorkspacePath { working_copy, path } => {
                self.put_u8(9)?;
                self.put_working_copy(*working_copy)?;
                self.put_string(path, "workspace path")
            }
        }
    }

    fn put_checkpoint_kind(&mut self, kind: CheckpointKind) -> Result<(), OperationCodecError> {
        self.put_u8(match kind {
            CheckpointKind::Bridge => 0,
            CheckpointKind::WorkingCopyCompatibility => 1,
            CheckpointKind::Recovery => 2,
        })
    }

    fn put_verification_scope(
        &mut self,
        scope: VerificationScope,
    ) -> Result<(), OperationCodecError> {
        self.put_u8(match scope {
            VerificationScope::Filesystem => 0,
            VerificationScope::Git => 1,
            VerificationScope::Repository => 2,
            VerificationScope::Complete => 3,
        })
    }

    fn put_optional_effect_value(
        &mut self,
        value: Option<&EffectValue>,
    ) -> Result<(), OperationCodecError> {
        self.put_optional_tag(value.is_some())?;
        if let Some(value) = value {
            self.put_effect_value(value)?;
        }
        Ok(())
    }

    fn put_effect_value(&mut self, value: &EffectValue) -> Result<(), OperationCodecError> {
        match value {
            EffectValue::Absent => self.put_u8(0),
            EffectValue::Digest { kind, hash } => {
                self.put_u8(1)?;
                self.put_digest_kind(*kind)?;
                self.put_hash(*hash)
            }
            EffectValue::File(state) => {
                self.put_u8(2)?;
                self.put_file_state(state)
            }
            EffectValue::GitObject(oid) => {
                self.put_u8(3)?;
                self.put_git_object_id(oid)
            }
            EffectValue::GitRef(target) => {
                self.put_u8(4)?;
                self.put_git_ref_target(target)
            }
            EffectValue::GitIndex(state) => {
                self.put_u8(5)?;
                self.put_git_index(state)
            }
            EffectValue::WorkingCopy(state) => {
                self.put_u8(6)?;
                self.put_working_copy_state(state)
            }
            EffectValue::Verification(hash) => {
                self.put_u8(7)?;
                self.put_hash(*hash)
            }
        }
    }

    fn put_digest_kind(&mut self, kind: DigestKind) -> Result<(), OperationCodecError> {
        self.put_u8(match kind {
            DigestKind::Bytes => 0,
            DigestKind::Manifest => 1,
            DigestKind::Checkpoint => 2,
            DigestKind::Shelf => 3,
            DigestKind::Refs => 4,
        })
    }

    fn put_file_state(&mut self, state: &FileState) -> Result<(), OperationCodecError> {
        self.put_u8(match state.kind {
            FileKind::Regular => 0,
            FileKind::Directory => 1,
            FileKind::Symlink => 2,
            FileKind::Gitlink => 3,
        })?;
        self.put_u32(state.mode)?;
        self.put_hash(state.content)
    }

    fn put_receipt_kind(&mut self, kind: EffectReceiptKind) -> Result<(), OperationCodecError> {
        self.put_u8(match kind {
            EffectReceiptKind::Applied => 0,
            EffectReceiptKind::Verified => 1,
            EffectReceiptKind::RolledBack => 2,
            EffectReceiptKind::LeaseRejected => 3,
            EffectReceiptKind::Recovered => 4,
        })
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self, OperationCodecError> {
        if bytes.len() > MAX_OPERATION_OBJECT_BYTES {
            return Err(OperationCodecError::new(format!(
                "canonical operation object has {} bytes, maximum is {MAX_OPERATION_OBJECT_BYTES}",
                bytes.len()
            )));
        }
        Ok(Self { bytes, offset: 0 })
    }

    fn expect_version(&mut self, expected: u8, object: &str) -> Result<(), OperationCodecError> {
        let actual = self.read_u8()?;
        if actual != expected {
            return Err(OperationCodecError::new(format!(
                "unsupported {object} version {actual} (maximum supported version {expected})"
            )));
        }
        Ok(())
    }

    fn read_exact<const N: usize>(&mut self) -> Result<[u8; N], OperationCodecError> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or_else(|| OperationCodecError::new("canonical object offset overflow"))?;
        let bytes = self.bytes.get(self.offset..end).ok_or_else(|| {
            OperationCodecError::new(format!(
                "canonical object is truncated at byte {} while reading {N} bytes",
                self.offset
            ))
        })?;
        self.offset = end;
        bytes
            .try_into()
            .map_err(|_| OperationCodecError::new("invalid fixed-width canonical field"))
    }

    fn read_u8(&mut self) -> Result<u8, OperationCodecError> {
        Ok(self.read_exact::<1>()?[0])
    }

    fn read_u32(&mut self) -> Result<u32, OperationCodecError> {
        Ok(u32::from_le_bytes(self.read_exact()?))
    }

    fn read_u64(&mut self) -> Result<u64, OperationCodecError> {
        Ok(u64::from_le_bytes(self.read_exact()?))
    }

    fn read_i64(&mut self) -> Result<i64, OperationCodecError> {
        Ok(i64::from_le_bytes(self.read_exact()?))
    }

    fn read_count(&mut self, field: &str) -> Result<usize, OperationCodecError> {
        let count = self.read_u32()? as usize;
        if count > MAX_OPERATION_COLLECTION_ITEMS {
            return Err(OperationCodecError::new(format!(
                "{field} has {count} items, maximum is {MAX_OPERATION_COLLECTION_ITEMS}"
            )));
        }
        Ok(count)
    }

    fn read_string(&mut self, field: &str) -> Result<String, OperationCodecError> {
        let len = self.read_u32()? as usize;
        if len == 0 {
            return Err(OperationCodecError::new(format!("{field} cannot be empty")));
        }
        if len > MAX_OPERATION_STRING_BYTES {
            return Err(OperationCodecError::new(format!(
                "{field} has {len} UTF-8 bytes, maximum is {MAX_OPERATION_STRING_BYTES}"
            )));
        }
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| OperationCodecError::new("canonical string length overflow"))?;
        let bytes = self.bytes.get(self.offset..end).ok_or_else(|| {
            OperationCodecError::new(format!(
                "canonical object is truncated at byte {} while reading {field}",
                self.offset
            ))
        })?;
        self.offset = end;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| OperationCodecError::new(format!("{field} is not valid UTF-8")))
    }

    fn read_blob(&mut self, field: &str) -> Result<Vec<u8>, OperationCodecError> {
        let len = self.read_u32()? as usize;
        if len > MAX_OPERATION_OBJECT_BYTES {
            return Err(OperationCodecError::new(format!(
                "{field} has {len} bytes, maximum is {MAX_OPERATION_OBJECT_BYTES}"
            )));
        }
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| OperationCodecError::new("canonical byte-string length overflow"))?;
        let bytes = self.bytes.get(self.offset..end).ok_or_else(|| {
            OperationCodecError::new(format!(
                "canonical object is truncated at byte {} while reading {field}",
                self.offset
            ))
        })?;
        self.offset = end;
        Ok(bytes.to_vec())
    }

    fn read_hash(&mut self) -> Result<Hash, OperationCodecError> {
        Ok(Hash::from_bytes(self.read_exact()?))
    }

    fn read_set_id(&mut self) -> Result<SetId, OperationCodecError> {
        Ok(SetId::from_bytes(self.read_exact()?))
    }

    fn read_operation_id(&mut self) -> Result<OperationId, OperationCodecError> {
        Ok(OperationId::from_bytes(self.read_exact()?))
    }

    fn read_working_copy(&mut self) -> Result<WorkingCopyId, OperationCodecError> {
        Ok(WorkingCopyId::from_bytes(self.read_exact()?))
    }

    fn read_optional_tag(&mut self, field: &str) -> Result<bool, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(OperationCodecError::new(format!(
                "invalid {field} presence tag {tag}"
            ))),
        }
    }

    fn read_optional_u32(&mut self) -> Result<Option<u32>, OperationCodecError> {
        if self.read_optional_tag("u32")? {
            Ok(Some(self.read_u32()?))
        } else {
            Ok(None)
        }
    }

    fn read_optional_hash(&mut self, field: &str) -> Result<Option<Hash>, OperationCodecError> {
        if self.read_optional_tag(field)? {
            Ok(Some(self.read_hash()?))
        } else {
            Ok(None)
        }
    }

    fn read_optional_set_id(&mut self) -> Result<Option<SetId>, OperationCodecError> {
        if self.read_optional_tag("set ID")? {
            Ok(Some(self.read_set_id()?))
        } else {
            Ok(None)
        }
    }

    fn read_optional_working_copy(&mut self) -> Result<Option<WorkingCopyId>, OperationCodecError> {
        if self.read_optional_tag("working copy")? {
            Ok(Some(self.read_working_copy()?))
        } else {
            Ok(None)
        }
    }

    fn read_operation_kind(
        &mut self,
        encoding_version: u8,
    ) -> Result<OperationKind, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(OperationKind::Anchor),
            1 => Ok(OperationKind::Record),
            2 => Ok(OperationKind::PromoteSnapshot),
            3 => Ok(OperationKind::SplitSnapshot),
            4 => Ok(OperationKind::SwitchView),
            5 => Ok(OperationKind::Materialize),
            6 => Ok(OperationKind::ImportGitHead),
            7 => Ok(OperationKind::ImportGitRefs),
            8 => Ok(OperationKind::ExportGitRefs),
            9 => Ok(OperationKind::ProjectState),
            10 => Ok(OperationKind::SynthesizeGit),
            11 => Ok(OperationKind::ResurrectBinding),
            12 => Ok(OperationKind::Insert),
            13 => Ok(OperationKind::Unrecord),
            14 => Ok(OperationKind::Tag),
            15 => Ok(OperationKind::Recover),
            16 => Ok(OperationKind::Undo),
            17 => Ok(OperationKind::Gc),
            18 if encoding_version == OPERATION_VERSION_V2 => Ok(OperationKind::Restore),
            19 if encoding_version == OPERATION_VERSION_V2 => Ok(OperationKind::Pull),
            20 if encoding_version == OPERATION_VERSION_V2 => Ok(OperationKind::Push),
            21 if encoding_version == OPERATION_VERSION_V2 => Ok(OperationKind::Consolidate),
            22 if encoding_version == OPERATION_VERSION_V2 => Ok(OperationKind::RefMapping),
            23 if encoding_version == OPERATION_VERSION_V2 => Ok(OperationKind::Repair),
            24 if encoding_version == OPERATION_VERSION_V2 => Ok(OperationKind::Cutover),
            25 if encoding_version == OPERATION_VERSION_V2 => {
                Ok(OperationKind::ReconcileWorkingCopy)
            }
            tag => Err(OperationCodecError::new(format!(
                "unsupported operation kind tag {tag} for operation version {encoding_version}"
            ))),
        }
    }

    fn read_operation_relation(&mut self) -> Result<OperationRelation, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(OperationRelation::Undo {
                target: self.read_operation_id()?,
            }),
            1 => Ok(OperationRelation::Restore {
                target: self.read_operation_id()?,
            }),
            tag => Err(OperationCodecError::new(format!(
                "unsupported operation relation tag {tag}"
            ))),
        }
    }

    fn read_repo_state(&mut self) -> Result<RepoStateRef, OperationCodecError> {
        let view = if self.read_optional_tag("view state")? {
            Some(self.read_view_state()?)
        } else {
            None
        };
        let working_copy = if self.read_optional_tag("working-copy state")? {
            Some(self.read_working_copy_state()?)
        } else {
            None
        };
        let git = if self.read_optional_tag("Git state")? {
            Some(self.read_git_state()?)
        } else {
            None
        };
        Ok(RepoStateRef {
            view,
            working_copy,
            git,
        })
    }

    fn read_repo_state_delta(
        &mut self,
        encoding_version: u8,
    ) -> Result<RepoStateDelta, OperationCodecError> {
        let after = self.read_repo_state()?;
        let metadata = if encoding_version == OPERATION_VERSION_V2 {
            let count = self.read_count("metadata transitions")?;
            let mut metadata = Vec::with_capacity(count);
            for _ in 0..count {
                metadata.push(self.read_metadata_transition()?);
            }
            metadata
        } else {
            Vec::new()
        };
        let count = self.read_count("operation effects")?;
        let mut effects = Vec::with_capacity(count);
        for _ in 0..count {
            effects.push(self.read_effect_plan()?);
        }
        let delta = RepoStateDelta {
            after,
            metadata,
            effects,
        };
        delta.validate_canonical()?;
        Ok(delta)
    }

    fn read_metadata_transition(&mut self) -> Result<MetadataTransition, OperationCodecError> {
        Ok(MetadataTransition {
            target: self.read_metadata_target()?,
            expected_old: self.read_metadata_value()?,
            expected_new: self.read_metadata_value()?,
        })
    }

    fn read_metadata_target(&mut self) -> Result<MetadataTarget, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(MetadataTarget::ViewChange {
                view: self.read_string("metadata view-change view")?,
                change: self.read_hash()?,
            }),
            1 => Ok(MetadataTarget::View {
                name: self.read_string("metadata view name")?,
            }),
            2 => Ok(MetadataTarget::Tag {
                view: self.read_string("metadata tag view")?,
                name: self.read_string("metadata tag name")?,
            }),
            3 => Ok(MetadataTarget::Remote {
                name: self.read_string("metadata remote name")?,
            }),
            4 => Ok(MetadataTarget::RefMapping {
                view: self.read_string("metadata ref-mapping view")?,
            }),
            5 => Ok(MetadataTarget::Capability {
                id: self.read_string("metadata capability id")?,
            }),
            tag => Err(OperationCodecError::new(format!(
                "unsupported metadata target tag {tag}"
            ))),
        }
    }

    fn read_metadata_value(&mut self) -> Result<MetadataValue, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(MetadataValue::Absent),
            1 => Ok(MetadataValue::Sequence(self.read_u64()?)),
            2 => Ok(MetadataValue::Digest(self.read_hash()?)),
            3 => Ok(MetadataValue::Bytes(self.read_blob("metadata bytes")?)),
            tag => Err(OperationCodecError::new(format!(
                "unsupported metadata value tag {tag}"
            ))),
        }
    }

    fn read_view_state(&mut self) -> Result<ViewStateRef, OperationCodecError> {
        Ok(ViewStateRef {
            name: self.read_string("view name")?,
            state: self.read_hash()?,
            set_id: self.read_optional_set_id()?,
        })
    }

    fn read_working_copy_state(&mut self) -> Result<WorkingCopyStateRef, OperationCodecError> {
        let state = WorkingCopyStateRef {
            id: self.read_working_copy()?,
            location_fingerprint: self.read_hash()?,
            desired_view: self.read_u64()?,
            desired_state: self.read_hash()?,
            materialized_state: self.read_optional_hash("materialized state")?,
            materialized_manifest: self.read_optional_hash("materialized manifest")?,
        };
        if state.desired_view == 0 {
            return Err(OperationCodecError::new(
                "working-copy desired view must be non-zero",
            ));
        }
        if state.materialized_manifest.is_some() && state.materialized_state.is_none() {
            return Err(OperationCodecError::new(
                "working-copy materialized manifest requires materialized state",
            ));
        }
        Ok(state)
    }

    fn read_git_state(&mut self) -> Result<GitStateRef, OperationCodecError> {
        let head = self.read_git_head()?;
        let index = if self.read_optional_tag("Git index")? {
            Some(self.read_git_index()?)
        } else {
            None
        };
        let refs_digest = self.read_hash()?;
        Ok(GitStateRef {
            head,
            index,
            refs_digest,
        })
    }

    fn read_git_head(&mut self) -> Result<GitHeadState, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(GitHeadState::Attached {
                symref: self.read_string("Git HEAD symref")?,
                oid: self.read_git_object_id()?,
            }),
            1 => Ok(GitHeadState::Detached {
                oid: self.read_git_object_id()?,
            }),
            2 => Ok(GitHeadState::Unborn {
                symref: self.read_string("Git HEAD symref")?,
            }),
            3 => Ok(GitHeadState::MissingTarget {
                symref: self.read_string("Git HEAD symref")?,
            }),
            tag => Err(OperationCodecError::new(format!(
                "unsupported Git HEAD state tag {tag}"
            ))),
        }
    }

    fn read_git_object_id(&mut self) -> Result<GitObjectId, OperationCodecError> {
        let (algorithm, width) = match self.read_u8()? {
            0 => (GitHashAlgorithm::Sha1, 20),
            1 => (GitHashAlgorithm::Sha256, 32),
            tag => {
                return Err(OperationCodecError::new(format!(
                    "unsupported Git hash algorithm tag {tag}"
                )))
            }
        };
        let end = self
            .offset
            .checked_add(width)
            .ok_or_else(|| OperationCodecError::new("Git object ID length overflow"))?;
        let bytes = self.bytes.get(self.offset..end).ok_or_else(|| {
            OperationCodecError::new("canonical object is truncated while reading Git object ID")
        })?;
        self.offset = end;
        GitObjectId::new(algorithm, bytes.to_vec())
    }

    fn read_git_index(&mut self) -> Result<GitIndexState, OperationCodecError> {
        let digest = self.read_hash()?;
        let tree = if self.read_optional_tag("Git index tree")? {
            Some(self.read_git_object_id()?)
        } else {
            None
        };
        Ok(GitIndexState { digest, tree })
    }

    fn read_git_ref_target(&mut self) -> Result<GitRefTarget, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(GitRefTarget::Direct(self.read_git_object_id()?)),
            1 => Ok(GitRefTarget::Symbolic(
                self.read_string("Git symbolic ref target")?,
            )),
            tag => Err(OperationCodecError::new(format!(
                "unsupported Git ref target tag {tag}"
            ))),
        }
    }

    fn read_git_ref_observation(&mut self) -> Result<GitRefObservation, OperationCodecError> {
        let name = self.read_string("Git ref name")?;
        let target = if self.read_optional_tag("Git ref target")? {
            Some(self.read_git_ref_target()?)
        } else {
            None
        };
        Ok(GitRefObservation { name, target })
    }

    fn read_actor(&mut self) -> Result<ActorRef, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(ActorRef::Human {
                did: self.read_string("human actor DID")?,
            }),
            1 => {
                let did = self.read_string("agent actor DID")?;
                let session = self.read_string("agent session")?;
                let turn = if self.read_optional_tag("agent turn")? {
                    Some(self.read_u64()?)
                } else {
                    None
                };
                Ok(ActorRef::Agent { did, session, turn })
            }
            2 => Ok(ActorRef::System {
                name: self.read_string("system actor name")?,
            }),
            tag => Err(OperationCodecError::new(format!(
                "unsupported actor tag {tag}"
            ))),
        }
    }

    fn read_loss_note(&mut self) -> Result<OperationLossNote, OperationCodecError> {
        let code = self.read_string("loss-note code")?;
        let message = self.read_string("loss-note message")?;
        let count = self.read_count("loss-note evidence")?;
        let mut evidence = Vec::with_capacity(count);
        for _ in 0..count {
            evidence.push(self.read_hash()?);
        }
        ensure_strictly_sorted(&evidence, "loss-note evidence")?;
        Ok(OperationLossNote {
            code,
            message,
            evidence,
        })
    }

    fn read_effect_plan(&mut self) -> Result<EffectPlan, OperationCodecError> {
        Ok(EffectPlan {
            ordinal: self.read_u32()?,
            target: self.read_effect_target()?,
            expected_old: self.read_effect_value()?,
            expected_new: self.read_effect_value()?,
        })
    }

    fn read_effect_target(&mut self) -> Result<EffectTarget, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(EffectTarget::FilesystemPath {
                path: self.read_string("filesystem path")?,
            }),
            1 => Ok(EffectTarget::ShelfPath {
                working_copy: self.read_working_copy()?,
                view: self.read_string("shelf view")?,
                path: self.read_string("shelf path")?,
            }),
            2 => Ok(EffectTarget::GitObject {
                object: self.read_git_object_id()?,
            }),
            3 => Ok(EffectTarget::GitIndex {
                working_copy: self.read_working_copy()?,
            }),
            4 => Ok(EffectTarget::GitRef {
                name: self.read_string("Git ref name")?,
            }),
            5 => Ok(EffectTarget::GitHead {
                working_copy: self.read_working_copy()?,
            }),
            6 => Ok(EffectTarget::Checkpoint {
                working_copy: self.read_working_copy()?,
                kind: self.read_checkpoint_kind()?,
            }),
            7 => Ok(EffectTarget::WorkingCopy {
                working_copy: self.read_working_copy()?,
            }),
            8 => Ok(EffectTarget::Verification {
                working_copy: self.read_optional_working_copy()?,
                scope: self.read_verification_scope()?,
            }),
            9 => Ok(EffectTarget::WorkspacePath {
                working_copy: self.read_working_copy()?,
                path: self.read_string("workspace path")?,
            }),
            tag => Err(OperationCodecError::new(format!(
                "unsupported effect target tag {tag}"
            ))),
        }
    }

    fn read_checkpoint_kind(&mut self) -> Result<CheckpointKind, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(CheckpointKind::Bridge),
            1 => Ok(CheckpointKind::WorkingCopyCompatibility),
            2 => Ok(CheckpointKind::Recovery),
            tag => Err(OperationCodecError::new(format!(
                "unsupported checkpoint kind tag {tag}"
            ))),
        }
    }

    fn read_verification_scope(&mut self) -> Result<VerificationScope, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(VerificationScope::Filesystem),
            1 => Ok(VerificationScope::Git),
            2 => Ok(VerificationScope::Repository),
            3 => Ok(VerificationScope::Complete),
            tag => Err(OperationCodecError::new(format!(
                "unsupported verification scope tag {tag}"
            ))),
        }
    }

    fn read_optional_effect_value(&mut self) -> Result<Option<EffectValue>, OperationCodecError> {
        if self.read_optional_tag("effect value")? {
            Ok(Some(self.read_effect_value()?))
        } else {
            Ok(None)
        }
    }

    fn read_effect_value(&mut self) -> Result<EffectValue, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(EffectValue::Absent),
            1 => Ok(EffectValue::Digest {
                kind: self.read_digest_kind()?,
                hash: self.read_hash()?,
            }),
            2 => Ok(EffectValue::File(self.read_file_state()?)),
            3 => Ok(EffectValue::GitObject(self.read_git_object_id()?)),
            4 => Ok(EffectValue::GitRef(self.read_git_ref_target()?)),
            5 => Ok(EffectValue::GitIndex(self.read_git_index()?)),
            6 => Ok(EffectValue::WorkingCopy(self.read_working_copy_state()?)),
            7 => Ok(EffectValue::Verification(self.read_hash()?)),
            tag => Err(OperationCodecError::new(format!(
                "unsupported effect value tag {tag}"
            ))),
        }
    }

    fn read_digest_kind(&mut self) -> Result<DigestKind, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(DigestKind::Bytes),
            1 => Ok(DigestKind::Manifest),
            2 => Ok(DigestKind::Checkpoint),
            3 => Ok(DigestKind::Shelf),
            4 => Ok(DigestKind::Refs),
            tag => Err(OperationCodecError::new(format!(
                "unsupported digest kind tag {tag}"
            ))),
        }
    }

    fn read_file_state(&mut self) -> Result<FileState, OperationCodecError> {
        let kind = match self.read_u8()? {
            0 => FileKind::Regular,
            1 => FileKind::Directory,
            2 => FileKind::Symlink,
            3 => FileKind::Gitlink,
            tag => {
                return Err(OperationCodecError::new(format!(
                    "unsupported file kind tag {tag}"
                )))
            }
        };
        Ok(FileState {
            kind,
            mode: self.read_u32()?,
            content: self.read_hash()?,
        })
    }

    fn read_receipt_kind(&mut self) -> Result<EffectReceiptKind, OperationCodecError> {
        match self.read_u8()? {
            0 => Ok(EffectReceiptKind::Applied),
            1 => Ok(EffectReceiptKind::Verified),
            2 => Ok(EffectReceiptKind::RolledBack),
            3 => Ok(EffectReceiptKind::LeaseRejected),
            4 => Ok(EffectReceiptKind::Recovered),
            tag => Err(OperationCodecError::new(format!(
                "unsupported effect receipt kind tag {tag}"
            ))),
        }
    }

    fn finish(self) -> Result<(), OperationCodecError> {
        if self.offset != self.bytes.len() {
            return Err(OperationCodecError::new(format!(
                "canonical object has {} trailing bytes",
                self.bytes.len() - self.offset
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor_payload() -> OperationPayload {
        OperationPayload {
            parents: Vec::new(),
            kind: OperationKind::Anchor,
            relation: None,
            working_copy: None,
            before: RepoStateRef::EMPTY,
            delta: RepoStateDelta {
                after: RepoStateRef::EMPTY,
                metadata: Vec::new(),
                effects: Vec::new(),
            },
            git_observed: Vec::new(),
            evidence: Vec::new(),
            actor: ActorRef::System {
                name: "system".to_string(),
            },
            timestamp_ms: 0,
            lossy: Vec::new(),
        }
    }

    fn v2_payload() -> OperationPayload {
        let mut payload = anchor_payload();
        payload.parents = vec![OperationId::from_bytes([1; 32])];
        payload.kind = OperationKind::Restore;
        payload.relation = Some(OperationRelation::Restore {
            target: OperationId::from_bytes([2; 32]),
        });
        payload.delta.metadata = vec![
            MetadataTransition {
                target: MetadataTarget::Remote {
                    name: "origin".into(),
                },
                expected_old: MetadataValue::Absent,
                expected_new: MetadataValue::Bytes(b"ssh://example/repo".to_vec()),
            },
            MetadataTransition {
                target: MetadataTarget::Tag {
                    view: "main".into(),
                    name: "v1".into(),
                },
                expected_old: MetadataValue::Digest(Hash::from_bytes([3; 32])),
                expected_new: MetadataValue::Absent,
            },
            MetadataTransition {
                target: MetadataTarget::View {
                    name: "main".into(),
                },
                expected_old: MetadataValue::Absent,
                expected_new: MetadataValue::Digest(Hash::from_bytes([4; 32])),
            },
            MetadataTransition {
                target: MetadataTarget::ViewChange {
                    view: "main".into(),
                    change: Hash::from_bytes([5; 32]),
                },
                expected_old: MetadataValue::Absent,
                expected_new: MetadataValue::Sequence(7),
            },
        ];
        payload
    }

    fn metadata_transition_ranges(bytes: &[u8]) -> Vec<std::ops::Range<usize>> {
        let mut decoder = Decoder::new(bytes).unwrap();
        let encoding_version = decoder.read_u8().unwrap();
        assert_eq!(encoding_version, OPERATION_VERSION_V2);
        let parent_count = decoder.read_count("operation parents").unwrap();
        for _ in 0..parent_count {
            decoder.read_operation_id().unwrap();
        }
        decoder
            .read_operation_kind(encoding_version)
            .expect("operation kind");
        if decoder.read_optional_tag("operation relation").unwrap() {
            decoder.read_operation_relation().unwrap();
        }
        decoder.read_optional_working_copy().unwrap();
        decoder.read_repo_state().unwrap();
        decoder.read_repo_state().unwrap();
        let metadata_count = decoder.read_count("metadata transitions").unwrap();
        let mut ranges = Vec::with_capacity(metadata_count);
        for _ in 0..metadata_count {
            let start = decoder.offset;
            decoder.read_metadata_transition().unwrap();
            ranges.push(start..decoder.offset);
        }
        ranges
    }

    #[test]
    fn operation_v1_golden_vector_and_roundtrip() {
        let operation = Operation::new(anchor_payload()).unwrap();
        let encoded = encode_operation(&operation).unwrap();
        let expected = [
            1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 6, 0, 0,
            0, b's', b'y', b's', b't', b'e', b'm', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(operation.encoding_version(), OPERATION_VERSION_V1);
        assert_eq!(encoded, expected);
        let decoded = decode_operation(&encoded).unwrap();
        assert_eq!(decoded, operation);
        assert_eq!(decoded.encoding_version(), OPERATION_VERSION_V1);
        assert_eq!(decoded.id(), OperationId::from_canonical_bytes(&expected));
        assert_eq!(encode_operation(&decoded).unwrap(), expected);
    }

    #[test]
    fn decoded_operation_retains_v2_for_legacy_compatible_payload() {
        let encoded = encode_operation_payload(&anchor_payload(), OPERATION_VERSION_V2).unwrap();
        let decoded = decode_operation(&encoded).unwrap();

        assert_eq!(decoded.encoding_version(), OPERATION_VERSION_V2);
        assert_eq!(decoded.payload().relation, None);
        assert!(decoded.payload().delta.metadata.is_empty());
        assert_eq!(encode_operation(&decoded).unwrap(), encoded);
        assert_ne!(decoded.id(), Operation::new(anchor_payload()).unwrap().id());
    }

    #[test]
    fn operation_id_covers_all_immutable_payload_fields() {
        let first = Operation::new(anchor_payload()).unwrap();
        let mut changed = anchor_payload();
        changed.timestamp_ms = 1;
        let second = Operation::new(changed).unwrap();
        assert_ne!(first.id(), second.id());

        let v2 = Operation::new(v2_payload()).unwrap();
        let mut changed_relation = v2_payload();
        changed_relation.relation = Some(OperationRelation::Restore {
            target: OperationId::from_bytes([6; 32]),
        });
        assert_ne!(v2.id(), Operation::new(changed_relation).unwrap().id());

        let mut changed_metadata = v2_payload();
        changed_metadata.delta.metadata[0].expected_new = MetadataValue::Bytes(b"changed".to_vec());
        assert_ne!(v2.id(), Operation::new(changed_metadata).unwrap().id());
    }

    #[test]
    fn operation_v2_roundtrips_relation_and_typed_metadata() {
        let operation = Operation::new(v2_payload()).unwrap();
        let encoded = encode_operation(&operation).unwrap();
        let decoded = decode_operation(&encoded).unwrap();

        assert_eq!(operation.encoding_version(), OPERATION_VERSION_V2);
        assert_eq!(encoded[0], OPERATION_VERSION_V2);
        assert_eq!(decoded, operation);
        assert_eq!(encode_operation(&decoded).unwrap(), encoded);
        assert!(matches!(
            decoded.payload().relation,
            Some(OperationRelation::Restore { target })
                if target == OperationId::from_bytes([2; 32])
        ));
        assert_eq!(decoded.payload().delta.metadata.len(), 4);
    }

    #[test]
    fn metadata_transitions_are_canonicalized_and_noncanonical_bytes_are_rejected() {
        let operation = Operation::new(v2_payload()).unwrap();
        let targets: Vec<_> = operation
            .payload()
            .delta
            .metadata
            .iter()
            .map(|transition| transition.target.clone())
            .collect();
        assert_eq!(
            targets,
            vec![
                MetadataTarget::ViewChange {
                    view: "main".into(),
                    change: Hash::from_bytes([5; 32]),
                },
                MetadataTarget::View {
                    name: "main".into(),
                },
                MetadataTarget::Tag {
                    view: "main".into(),
                    name: "v1".into(),
                },
                MetadataTarget::Remote {
                    name: "origin".into(),
                },
            ]
        );

        let mut payload = v2_payload();
        payload.kind = OperationKind::Undo;
        payload.relation = Some(OperationRelation::Undo {
            target: OperationId::from_bytes([2; 32]),
        });
        payload.delta.metadata = vec![
            MetadataTransition {
                target: MetadataTarget::Remote { name: "b".into() },
                expected_old: MetadataValue::Absent,
                expected_new: MetadataValue::Sequence(1),
            },
            MetadataTransition {
                target: MetadataTarget::Remote { name: "a".into() },
                expected_old: MetadataValue::Absent,
                expected_new: MetadataValue::Sequence(1),
            },
        ];
        let mut encoded = encode_operation(&Operation::new(payload).unwrap()).unwrap();
        let ranges = metadata_transition_ranges(&encoded);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].len(), ranges[1].len());
        let first = encoded[ranges[0].clone()].to_vec();
        let second = encoded[ranges[1].clone()].to_vec();
        encoded[ranges[0].clone()].copy_from_slice(&second);
        encoded[ranges[1].clone()].copy_from_slice(&first);
        assert!(decode_operation(&encoded).is_err());
    }

    #[test]
    fn metadata_leases_reject_duplicate_targets_and_unchanged_values() {
        let mut duplicate = v2_payload();
        duplicate
            .delta
            .metadata
            .push(duplicate.delta.metadata[0].clone());
        assert!(Operation::new(duplicate).is_err());

        let mut unchanged = v2_payload();
        unchanged.delta.metadata[0].expected_new = unchanged.delta.metadata[0].expected_old.clone();
        assert!(Operation::new(unchanged).is_err());
    }

    #[test]
    fn appended_kinds_select_v2_and_relation_rules_are_enforced() {
        for kind in [
            OperationKind::Restore,
            OperationKind::Pull,
            OperationKind::Push,
        ] {
            let mut payload = anchor_payload();
            payload.parents = vec![OperationId::from_bytes([1; 32])];
            payload.kind = kind;
            assert_eq!(
                Operation::new(payload).unwrap().encoding_version(),
                OPERATION_VERSION_V2
            );
        }

        let mut consolidate = anchor_payload();
        consolidate.parents = vec![OperationId::from_bytes([1; 32])];
        consolidate.kind = OperationKind::Consolidate;
        assert!(Operation::new(consolidate.clone()).is_err());
        consolidate.parents.push(OperationId::from_bytes([2; 32]));
        assert_eq!(
            Operation::new(consolidate).unwrap().encoding_version(),
            OPERATION_VERSION_V2
        );

        let mut legacy_undo = anchor_payload();
        legacy_undo.parents = vec![OperationId::from_bytes([1; 32])];
        legacy_undo.kind = OperationKind::Undo;
        assert_eq!(
            Operation::new(legacy_undo).unwrap().encoding_version(),
            OPERATION_VERSION_V1
        );

        let mut mismatched = v2_payload();
        mismatched.kind = OperationKind::Push;
        assert!(Operation::new(mismatched).is_err());
    }

    #[test]
    fn receipt_v1_golden_vector_and_roundtrip() {
        let receipt = EffectReceipt::new(EffectReceiptPayload {
            operation: OperationId::from_bytes([1; 32]),
            effect_ordinal: None,
            attempt: 0,
            kind: EffectReceiptKind::Verified,
            observed_old: None,
            observed_new: None,
            timestamp_ms: 0,
        })
        .unwrap();
        let encoded = encode_effect_receipt(&receipt).unwrap();
        let mut expected = vec![1];
        expected.extend_from_slice(&[1; 32]);
        expected.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0]);
        expected.extend_from_slice(&[0; 8]);
        assert_eq!(encoded, expected);
        assert_eq!(decode_effect_receipt(&encoded).unwrap(), receipt);
    }

    #[test]
    fn operation_heads_v1_golden_vector_and_strict_ordering() {
        let heads = OperationHeads::new(vec![
            OperationId::from_bytes([2; 32]),
            OperationId::from_bytes([1; 32]),
        ]);
        let encoded = encode_operation_heads(&heads).unwrap();
        let mut expected = vec![1, 2, 0, 0, 0];
        expected.extend_from_slice(&[1; 32]);
        expected.extend_from_slice(&[2; 32]);
        assert_eq!(encoded, expected);
        assert_eq!(decode_operation_heads(&encoded).unwrap(), heads);

        let mut duplicate = vec![1, 2, 0, 0, 0];
        duplicate.extend_from_slice(&[1; 32]);
        duplicate.extend_from_slice(&[1; 32]);
        assert!(decode_operation_heads(&duplicate).is_err());
    }

    #[test]
    fn codecs_reject_unknown_versions_tags_truncation_and_trailing_bytes() {
        let operation = Operation::new(anchor_payload()).unwrap();
        let encoded = encode_operation(&operation).unwrap();

        let mut unknown_version = encoded.clone();
        unknown_version[0] = 3;
        assert!(decode_operation(&unknown_version).is_err());

        let mut unknown_kind = encoded.clone();
        unknown_kind[5] = u8::MAX;
        assert!(decode_operation(&unknown_kind).is_err());

        let v2 = encode_operation(&Operation::new(v2_payload()).unwrap()).unwrap();
        let relation_tag_offset = {
            let mut decoder = Decoder::new(&v2).unwrap();
            let encoding_version = decoder.read_u8().unwrap();
            let parent_count = decoder.read_count("operation parents").unwrap();
            for _ in 0..parent_count {
                decoder.read_operation_id().unwrap();
            }
            decoder.read_operation_kind(encoding_version).unwrap();
            assert!(decoder.read_optional_tag("operation relation").unwrap());
            decoder.offset
        };
        let mut unknown_relation = v2.clone();
        unknown_relation[relation_tag_offset] = u8::MAX;
        assert!(decode_operation(&unknown_relation).is_err());

        let first_metadata = metadata_transition_ranges(&v2)[0].clone();
        let mut unknown_metadata_target = v2.clone();
        unknown_metadata_target[first_metadata.start] = u8::MAX;
        assert!(decode_operation(&unknown_metadata_target).is_err());

        let metadata_value_offset = {
            let mut decoder = Decoder::new(&v2).unwrap();
            decoder.offset = first_metadata.start;
            decoder.read_metadata_target().unwrap();
            decoder.offset
        };
        let mut unknown_metadata_value = v2;
        unknown_metadata_value[metadata_value_offset] = u8::MAX;
        assert!(decode_operation(&unknown_metadata_value).is_err());

        assert!(decode_operation(&encoded[..encoded.len() - 1]).is_err());
        let mut trailing = encoded;
        trailing.push(0);
        assert!(decode_operation(&trailing).is_err());
    }

    #[test]
    fn constructor_canonicalizes_sets_and_rejects_duplicate_ref_names() {
        let mut payload = anchor_payload();
        payload.evidence = vec![Hash::from_bytes([2; 32]), Hash::from_bytes([1; 32])];
        let operation = Operation::new(payload).unwrap();
        assert_eq!(
            operation.payload().evidence,
            vec![Hash::from_bytes([1; 32]), Hash::from_bytes([2; 32])]
        );

        let mut duplicate_refs = anchor_payload();
        duplicate_refs.git_observed = vec![
            GitRefObservation {
                name: "refs/heads/main".into(),
                target: None,
            },
            GitRefObservation {
                name: "refs/heads/main".into(),
                target: None,
            },
        ];
        assert!(Operation::new(duplicate_refs).is_err());
    }

    #[test]
    fn effect_ordinals_are_contiguous_and_leases_must_change() {
        let mut payload = anchor_payload();
        payload.delta.effects.push(EffectPlan {
            ordinal: 1,
            target: EffectTarget::FilesystemPath {
                path: "file.txt".into(),
            },
            expected_old: EffectValue::Absent,
            expected_new: EffectValue::Verification(Hash::from_bytes([1; 32])),
        });
        assert!(Operation::new(payload).is_err());

        let mut payload = anchor_payload();
        payload.delta.effects.push(EffectPlan {
            ordinal: 0,
            target: EffectTarget::FilesystemPath {
                path: "file.txt".into(),
            },
            expected_old: EffectValue::Absent,
            expected_new: EffectValue::Absent,
        });
        assert!(Operation::new(payload).is_err());
    }

    #[test]
    fn all_typed_state_effect_and_git_variants_roundtrip() {
        let working_copy = WorkingCopyId::from_bytes([3; 16]);
        let sha1 = GitObjectId::new(GitHashAlgorithm::Sha1, vec![4; 20]).unwrap();
        let sha256 = GitObjectId::new(GitHashAlgorithm::Sha256, vec![5; 32]).unwrap();
        let working_copy_state = WorkingCopyStateRef {
            id: working_copy,
            location_fingerprint: Hash::from_bytes([6; 32]),
            desired_view: 7,
            desired_state: Hash::from_bytes([8; 32]),
            materialized_state: Some(Hash::from_bytes([9; 32])),
            materialized_manifest: Some(Hash::from_bytes([10; 32])),
        };
        let before = RepoStateRef {
            view: Some(ViewStateRef {
                name: "main".into(),
                state: Hash::from_bytes([11; 32]),
                set_id: Some(SetId::from_bytes([12; 32])),
            }),
            working_copy: Some(working_copy_state.clone()),
            git: Some(GitStateRef {
                head: GitHeadState::Attached {
                    symref: "refs/heads/main".into(),
                    oid: sha1.clone(),
                },
                index: Some(GitIndexState {
                    digest: Hash::from_bytes([13; 32]),
                    tree: Some(sha256.clone()),
                }),
                refs_digest: Hash::from_bytes([14; 32]),
            }),
        };
        let targets = vec![
            EffectPlan {
                ordinal: 0,
                target: EffectTarget::FilesystemPath {
                    path: "src/main.rs".into(),
                },
                expected_old: EffectValue::Absent,
                expected_new: EffectValue::File(FileState {
                    kind: FileKind::Regular,
                    mode: 0o100644,
                    content: Hash::from_bytes([15; 32]),
                }),
            },
            EffectPlan {
                ordinal: 1,
                target: EffectTarget::ShelfPath {
                    working_copy,
                    view: "main".into(),
                    path: "target/cache".into(),
                },
                expected_old: EffectValue::Absent,
                expected_new: EffectValue::Digest {
                    kind: DigestKind::Shelf,
                    hash: Hash::from_bytes([16; 32]),
                },
            },
            EffectPlan {
                ordinal: 2,
                target: EffectTarget::GitObject {
                    object: sha1.clone(),
                },
                expected_old: EffectValue::Absent,
                expected_new: EffectValue::GitObject(sha1.clone()),
            },
            EffectPlan {
                ordinal: 3,
                target: EffectTarget::GitIndex { working_copy },
                expected_old: EffectValue::Absent,
                expected_new: EffectValue::GitIndex(GitIndexState {
                    digest: Hash::from_bytes([17; 32]),
                    tree: Some(sha256.clone()),
                }),
            },
            EffectPlan {
                ordinal: 4,
                target: EffectTarget::GitRef {
                    name: "refs/heads/main".into(),
                },
                expected_old: EffectValue::Absent,
                expected_new: EffectValue::GitRef(GitRefTarget::Direct(sha1.clone())),
            },
            EffectPlan {
                ordinal: 5,
                target: EffectTarget::GitHead { working_copy },
                expected_old: EffectValue::Absent,
                expected_new: EffectValue::GitRef(GitRefTarget::Symbolic("refs/heads/main".into())),
            },
            EffectPlan {
                ordinal: 6,
                target: EffectTarget::Checkpoint {
                    working_copy,
                    kind: CheckpointKind::Bridge,
                },
                expected_old: EffectValue::Absent,
                expected_new: EffectValue::Digest {
                    kind: DigestKind::Checkpoint,
                    hash: Hash::from_bytes([18; 32]),
                },
            },
            EffectPlan {
                ordinal: 7,
                target: EffectTarget::WorkingCopy { working_copy },
                expected_old: EffectValue::Absent,
                expected_new: EffectValue::WorkingCopy(working_copy_state),
            },
            EffectPlan {
                ordinal: 8,
                target: EffectTarget::Verification {
                    working_copy: Some(working_copy),
                    scope: VerificationScope::Complete,
                },
                expected_old: EffectValue::Absent,
                expected_new: EffectValue::Verification(Hash::from_bytes([19; 32])),
            },
            EffectPlan {
                ordinal: 9,
                target: EffectTarget::WorkspacePath {
                    working_copy,
                    path: "target/private-cache".into(),
                },
                expected_old: EffectValue::Absent,
                expected_new: EffectValue::File(FileState {
                    kind: FileKind::Directory,
                    mode: 0o755,
                    content: Hash::from_bytes([22; 32]),
                }),
            },
        ];
        let operation = Operation::new(OperationPayload {
            parents: vec![OperationId::from_bytes([1; 32])],
            kind: OperationKind::SwitchView,
            relation: None,
            working_copy: Some(working_copy),
            before: before.clone(),
            delta: RepoStateDelta {
                after: before,
                metadata: Vec::new(),
                effects: targets,
            },
            git_observed: vec![GitRefObservation {
                name: "refs/heads/main".into(),
                target: Some(GitRefTarget::Direct(sha1)),
            }],
            evidence: vec![Hash::from_bytes([20; 32])],
            actor: ActorRef::Agent {
                did: "did:atomic:test".into(),
                session: "session-1".into(),
                turn: Some(2),
            },
            timestamp_ms: 42,
            lossy: vec![OperationLossNote {
                code: "test".into(),
                message: "roundtrip".into(),
                evidence: vec![Hash::from_bytes([21; 32])],
            }],
        })
        .unwrap();
        let encoded = encode_operation(&operation).unwrap();
        assert_eq!(decode_operation(&encoded).unwrap(), operation);
    }

    #[test]
    fn scope_keys_are_fixed_width_and_canonical() {
        let repository = encode_operation_scope(OperationScope::Repository);
        assert_eq!(repository, [0; 17]);
        assert_eq!(
            decode_operation_scope(&repository).unwrap(),
            OperationScope::Repository
        );

        let id = WorkingCopyId::from_bytes([7; 16]);
        let working_copy = encode_operation_scope(OperationScope::WorkingCopy(id));
        assert_eq!(working_copy[0], 1);
        assert_eq!(&working_copy[1..], &[7; 16]);
        assert_eq!(
            decode_operation_scope(&working_copy).unwrap(),
            OperationScope::WorkingCopy(id)
        );

        let mut noncanonical_repository = [0; 17];
        noncanonical_repository[1] = 1;
        assert!(decode_operation_scope(&noncanonical_repository).is_err());
    }
}
