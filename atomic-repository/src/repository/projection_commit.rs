//! Operation-specific projection commits (RFC §8.2, CB-8A).
//!
//! [`ProjectTree`](super::project_tree::ProjectTree) is the deterministic,
//! graph-derived tree: equal valid sets always project an equal tree. A
//! projected *commit* is different — it is operation-specific and never
//! derivable from a SetId alone. This module owns that contract:
//!
//! - **Parentage.** The default is exactly one parent — the previous
//!   projected state on the mapped ref. A selective Atomic `insert` also has
//!   one parent (its source binding lives in the message metadata). A second
//!   parent is emitted only for an explicit whole-view merge publication that
//!   first proved the complete source closure ([`WholeViewMergeProof`]);
//!   imported commits keep their original ordered parents untouched.
//! - **Identity (§5.5).** Author/committer names, emails, and timestamps
//!   come from the operation and the configured identity map, never from
//!   ambient Git config. Mapped emails use the configured DID; unmapped
//!   emails receive a deterministic `did:atomic:git:<email-hash>` foreign
//!   identity.
//! - **Message headers.** `atomic-binding`, `atomic-set`, `atomic-state`,
//!   `atomic-author`, and — for agent turns — `atomic-agent` plus a
//!   `Co-authored-by:` trailer.
//! - **Signing.** A new commit is signed only after constructing the exact
//!   unsigned payload. Under [`ProjectionSigning::RequiredAtomic`] any
//!   signing failure blocks publication: no commit object is written and no
//!   OID is returned. The commit signature lives in the commit's `gpgsig`
//!   header and is entirely separate from the Ed25519 signature inside a
//!   `GitStateBinding`.
//! - **Raw foreign objects.** Imported/resurrected commits are written
//!   verbatim ([`write_raw_git_object`]); their OIDs, signatures, headers,
//!   and ordered parents survive byte-for-byte and are never re-signed.

use std::collections::BTreeMap;
use std::fmt;

use atomic_core::operation::{GitHashAlgorithm, GitObjectId};
use atomic_core::types::{Base32, Hash, Merkle, SetId};

use git2::ObjectType;

/// Domain separator prepended to the unsigned commit payload before signing.
///
/// Commit signatures and binding signatures must never be interchangeable
/// (RFC §8.2): the same Ed25519 key produces different signatures for a
/// commit payload and for a binding payload.
pub const COMMIT_SIGNATURE_DOMAIN: &[u8] = b"atomic-commit-signature-v1";

/// Begin line of the armored Atomic commit signature carried in `gpgsig`.
pub const ATOMIC_SIGNATURE_BEGIN: &str = "-----BEGIN ATOMIC COMMIT SIGNATURE-----";
/// End line of the armored Atomic commit signature carried in `gpgsig`.
pub const ATOMIC_SIGNATURE_END: &str = "-----END ATOMIC COMMIT SIGNATURE-----";

/// Fail-closed errors from projection-commit construction.
#[derive(Debug, thiserror::Error)]
pub enum ProjectionCommitError {
    #[error("Git object error: {0}")]
    Git(String),
    #[error(
        "required commit signing failed before publication; no commit object was written: {0}"
    )]
    SigningFailed(String),
    #[error(
        "whole-view merge publication requires a complete source-closure proof; \
         refusing to invent a second parent"
    )]
    MergeProofRequired,
    #[error(
        "whole-view merge proof does not cover the complete source closure: \
         change {0} is missing from the target view"
    )]
    MergeProofIncomplete(String),
    #[error("projected commit parent must be a commit object: {0}")]
    ParentNotCommit(String),
    #[error("raw Git object identity mismatch: expected {expected}, computed {computed}")]
    RawObjectIdentity { expected: String, computed: String },
    #[error("cannot write raw Git object: {0}")]
    RawObject(String),
    #[error("signature header is missing on a signed commit")]
    SignatureHeaderMissing,
}

fn git_error(error: impl fmt::Display) -> ProjectionCommitError {
    ProjectionCommitError::Git(error.to_string())
}

/// Configured `[git.identity]` email → DID map (RFC §5.5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectionIdentityMap {
    entries: BTreeMap<String, String>,
}

impl ProjectionIdentityMap {
    /// Build from the configured map. Empty keys/values are ignored so a
    /// partially written config cannot map an email to no identity.
    pub fn new(entries: BTreeMap<String, String>) -> Self {
        let entries = entries
            .into_iter()
            .filter(|(email, did)| !email.is_empty() && !did.trim().is_empty())
            .collect();
        Self { entries }
    }

    /// The DID for an email: configured mapping, else the deterministic
    /// foreign `did:atomic:git:<email-hash>` identity (RFC §5.5).
    pub fn did_for_email(&self, email: &str) -> String {
        if let Some(did) = self.entries.get(email) {
            return did.clone();
        }
        foreign_git_did(email)
    }

    /// Whether the email has an explicit configured mapping (vs. a
    /// synthesized foreign DID that must be flagged in provenance).
    pub fn is_mapped(&self, email: &str) -> bool {
        self.entries.contains_key(email)
    }
}

/// Deterministic foreign identity for an unmapped email (RFC §5.5).
pub fn foreign_git_did(email: &str) -> String {
    let digest = blake3::hash(email.as_bytes());
    let hex: String = digest
        .as_bytes()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("did:atomic:git:{hex}")
}

/// The author of a projected commit under the §5.5 identity mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionAuthor {
    /// Author display name (from the Atomic identity).
    pub name: String,
    /// Author email (from the Atomic identity).
    pub email: String,
    /// Atomic DID attributed to the change (`atomic-author <did>`).
    pub did: String,
    /// DID of the agent turn that produced the change, when one exists
    /// (`atomic-agent <did>` + `Co-authored-by:` trailer).
    pub agent_did: Option<String>,
}

impl ProjectionAuthor {
    /// Resolve an author through the configured identity map (§5.5).
    pub fn resolve(
        map: &ProjectionIdentityMap,
        name: &str,
        email: &str,
        agent_did: Option<String>,
    ) -> Self {
        Self {
            name: name.to_string(),
            email: email.to_string(),
            did: map.did_for_email(email),
            agent_did,
        }
    }
}

/// How a projected commit attaches to Git history (RFC §8.2).
#[derive(Clone, Debug)]
pub enum ProjectionParents {
    /// One parent: the previous projected state on the mapped ref. This is
    /// the default for records and for selective Atomic `insert`s.
    Single { parent: GitObjectId },
    /// Two parents, allowed only for an explicit whole-view merge
    /// publication that first proved the complete source closure.
    Merge {
        /// The previous projection on the target mapped ref (first parent).
        mainline: GitObjectId,
        /// The source view's previous projection (second parent).
        merged: GitObjectId,
        /// Proof that every source-closure change is present in the target.
        proof: WholeViewMergeProof,
    },
}

impl ProjectionParents {
    /// Parent OIDs in Git commit order.
    pub fn oids(&self) -> Vec<GitObjectId> {
        match self {
            Self::Single { parent } => vec![parent.clone()],
            Self::Merge { mainline, merged, .. } => vec![mainline.clone(), merged.clone()],
        }
    }
}

/// Proof that a whole-view merge publication contains the complete source
/// closure (RFC §8.2).
///
/// Construction is restricted to
/// [`Repository::prove_whole_view_merge`](super::Repository::prove_whole_view_merge),
/// which verifies every source change against the target view's effective
/// projection closure. A second parent cannot be attached without one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WholeViewMergeProof {
    /// Hashes of the complete source closure, verified present in the target.
    pub source_change_hashes: Vec<Hash>,
    /// Merkle state of the source view at proof time.
    pub source_state: Merkle,
}

/// Operation-specific metadata for one projected commit.
#[derive(Clone, Debug)]
pub struct ProjectionCommitInput {
    /// Change message(s) — the human-readable commit body.
    pub message: String,
    /// Additive SetId v1 of the effective projection closure.
    pub set_id: SetId,
    /// Order-sensitive Merkle state of the projected view.
    pub state: Merkle,
    /// Binding identity for this state, when bound
    /// (`atomic-binding <hex>`).
    pub binding_id: Option<atomic_repository_bindings::BindingId>,
    /// Conflict-set identity for a conflict snapshot commit (RFC §8.3,
    /// CB-8B): the `atomic-conflict <hash>` header names the complete
    /// conflict object whose pack accompanies the snapshot. `None` for
    /// clean-state projections.
    pub conflict_set_hash: Option<atomic_core::Hash>,
    /// Author identity from the operation and the configured identity map.
    pub author: ProjectionAuthor,
    /// Operation timestamp (seconds since the epoch).
    pub timestamp: i64,
    /// Operation timestamp offset (minutes east of UTC).
    pub timestamp_offset: i32,
}

/// Re-export alias so the field type above stays spelled `BindingId` without
/// importing the codec module into the public path of this module.
pub(crate) mod atomic_repository_bindings {
    pub use crate::git_binding::BindingId;
}

/// Build the deterministic commit message (RFC §8.2).
///
/// Body = change message(s); trailer block carries `atomic-binding`,
/// `atomic-set`, `atomic-state`, `atomic-author`, and — for agent turns —
/// `atomic-agent` plus a `Co-authored-by:` trailer.
pub fn projection_commit_message(input: &ProjectionCommitInput) -> String {
    let mut message = String::new();
    message.push_str(input.message.trim_end());
    message.push_str("\n\n");
    if let Some(binding) = &input.binding_id {
        message.push_str(&format!("atomic-binding {}\n", binding.to_hex()));
    }
    if let Some(conflict_set) = &input.conflict_set_hash {
        // RFC §8.3: an ordinary Git commit whose tree carries
        // marker-materialized files, named by the complete conflict object
        // it is accompanied by. This is NOT Git unmerged index state.
        message.push_str(&format!(
            "atomic-conflict {}\n",
            atomic_core::types::Base32::to_base32(conflict_set)
        ));
    }
    message.push_str(&format!("atomic-set {}\n", input.set_id.to_base32()));
    message.push_str(&format!("atomic-state {}\n", input.state.to_base32()));
    message.push_str(&format!("atomic-author {}\n", input.author.did));
    if let Some(agent) = &input.author.agent_did {
        message.push_str(&format!("atomic-agent {agent}\n"));
        message.push_str(&format!(
            "Co-authored-by: {} <{}>\n",
            input.author.name, agent
        ));
    }
    message
}

/// Signing policy for one projected commit (RFC §8.2).
pub enum ProjectionSigning {
    /// Publish the commit unsigned.
    Unsigned,
    /// The commit must carry an Atomic Ed25519 signature over the exact
    /// unsigned payload (domain-separated by [`COMMIT_SIGNATURE_DOMAIN`]).
    /// Any signing failure blocks publication: no commit object is written
    /// and no OID is returned.
    RequiredAtomic { keypair: atomic_identity::KeyPair },
}

impl fmt::Debug for ProjectionSigning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsigned => formatter.write_str("Unsigned"),
            Self::RequiredAtomic { .. } => formatter.write_str("RequiredAtomic(..)"),
        }
    }
}

/// Armored signature block embedded in the commit's `gpgsig` header.
pub fn armor_commit_signature(signature: &[u8; 64]) -> String {
    let hex: String = signature.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{ATOMIC_SIGNATURE_BEGIN}\n{hex}\n{ATOMIC_SIGNATURE_END}\n")
}

/// Extract the raw 64-byte Atomic signature from an armored `gpgsig` value.
pub fn unarmor_commit_signature(header_value: &str) -> Option<[u8; 64]> {
    let start = header_value.find(ATOMIC_SIGNATURE_BEGIN)?;
    let rest = &header_value[start + ATOMIC_SIGNATURE_BEGIN.len()..];
    let end = rest.find(ATOMIC_SIGNATURE_END)?;
    let encoded = rest[..end].trim();
    if encoded.len() != 128 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut signature = [0u8; 64];
    for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
        signature[index] =
            u8::from_str_radix(std::str::from_utf8(pair).expect("hex checked"), 16)
                .expect("hex checked");
    }
    Some(signature)
}

/// Construct, without writing, the exact commit object a projection
/// publishes (CB-8B ac-3: the commit bytes are known before any mutation, so
/// the immutable operation can journal the creation with its bounded
/// canonical hash and retained bytes before the object database write).
///
/// Returns the unsigned payload's bytes — the object an unsigned projection
/// writes — together with the identity Git computes for those bytes. The
/// caller journals `(oid, bytes)`; the executor verifies the ODB returns
/// exactly that identity. A required signature still goes through
/// [`write_projected_commit`], which fails closed before publication.
pub fn build_projected_commit(
    git: &git2::Repository,
    tree_oid: &GitObjectId,
    parents: ProjectionParents,
    input: &ProjectionCommitInput,
) -> Result<(GitObjectId, Vec<u8>), ProjectionCommitError> {
    if let ProjectionParents::Merge { proof, .. } = &parents {
        if proof.source_change_hashes.is_empty() {
            return Err(ProjectionCommitError::MergeProofRequired);
        }
    }
    let algorithm = tree_oid.algorithm();
    let parent_oids = parents.oids();
    let mut git_parents = Vec::with_capacity(parent_oids.len());
    for oid in &parent_oids {
        let object = git
            .find_object(to_git2_oid(oid)?, Some(ObjectType::Commit))
            .map_err(|error| {
                ProjectionCommitError::ParentNotCommit(format!("{oid:?}: {error}"))
            })?;
        let commit = object
            .into_commit()
            .map_err(|_| {
                ProjectionCommitError::ParentNotCommit(format!(
                    "{oid:?} did not peel to a commit"
                ))
            })?;
        git_parents.push(commit);
    }
    let git_tree = git.find_tree(to_git2_oid(tree_oid)?).map_err(git_error)?;
    let parent_refs: Vec<&git2::Commit> = git_parents.iter().collect();

    let time = git2::Time::new(input.timestamp, input.timestamp_offset);
    let signature = git2::Signature::new(&input.author.name, &input.author.email, &time)
        .map_err(git_error)?;
    let unsigned = git
        .commit_create_buffer(
            &signature,
            &signature,
            &projection_commit_message(input),
            &git_tree,
            &parent_refs,
        )
        .map_err(git_error)?;
    let unsigned_bytes: Vec<u8> = unsigned.to_vec();
    let computed = git_object_oid(ObjectType::Commit, &unsigned_bytes);
    if computed.algorithm() != algorithm {
        return Err(ProjectionCommitError::Git(format!(
            "projected commit identity {:?} is unsupported",
            algorithm
        )));
    }
    Ok((computed, unsigned_bytes))
}

/// Construct and write one operation-specific projection commit (RFC §8.2).
///
/// The unsigned payload is built first; only then is it signed, and only
/// under [`ProjectionSigning::RequiredAtomic`]. This function never moves a
/// ref — the caller publishes the returned OID through its own leased ref
/// write, so a failed signature publishes nothing at all.
pub fn write_projected_commit(
    git: &git2::Repository,
    tree_oid: &GitObjectId,
    parents: ProjectionParents,
    input: &ProjectionCommitInput,
    signing: &ProjectionSigning,
) -> Result<GitObjectId, ProjectionCommitError> {
    if let ProjectionParents::Merge { proof, .. } = &parents {
        if proof.source_change_hashes.is_empty() {
            return Err(ProjectionCommitError::MergeProofRequired);
        }
    }
    let algorithm = tree_oid.algorithm();
    let parent_oids = parents.oids();
    let mut git_parents = Vec::with_capacity(parent_oids.len());
    for oid in &parent_oids {
        let object = git
            .find_object(to_git2_oid(oid)?, Some(ObjectType::Commit))
            .map_err(|error| {
                ProjectionCommitError::ParentNotCommit(format!("{oid:?}: {error}"))
            })?;
        let commit = object
            .into_commit()
            .map_err(|_| {
                ProjectionCommitError::ParentNotCommit(format!(
                    "{oid:?} did not peel to a commit"
                ))
            })?;
        git_parents.push(commit);
    }
    let git_tree = git.find_tree(to_git2_oid(tree_oid)?).map_err(git_error)?;
    let parent_refs: Vec<&git2::Commit> = git_parents.iter().collect();

    let time = git2::Time::new(input.timestamp, input.timestamp_offset);
    let signature = git2::Signature::new(&input.author.name, &input.author.email, &time)
        .map_err(git_error)?;

    // The unsigned payload is constructed first; the signature, when
    // required, covers exactly these bytes.
    let unsigned = git
        .commit_create_buffer(
            &signature,
            &signature,
            &projection_commit_message(input),
            &git_tree,
            &parent_refs,
        )
        .map_err(git_error)?;
    let unsigned_bytes: &[u8] = &unsigned;

    let commit_oid = match signing {
        ProjectionSigning::Unsigned => git
            .odb()
            .map_err(git_error)?
            .write(ObjectType::Commit, unsigned_bytes)
            .map_err(git_error)?,
        ProjectionSigning::RequiredAtomic { keypair } => {
            let mut message = Vec::with_capacity(COMMIT_SIGNATURE_DOMAIN.len() + unsigned_bytes.len());
            message.extend_from_slice(COMMIT_SIGNATURE_DOMAIN);
            message.extend_from_slice(unsigned_bytes);
            let signer = atomic_identity::Signer::new(&keypair);
            let signature = signer.sign(&message);
            let armored = armor_commit_signature(signature.as_bytes());
            git.commit_signed(
                std::str::from_utf8(unsigned_bytes)
                    .map_err(|error| ProjectionCommitError::Git(error.to_string()))?,
                &armored,
                Some("gpgsig"),
            )
            .map_err(|error| {
                // Fail closed: a failed required signature publishes nothing.
                ProjectionCommitError::SigningFailed(error.to_string())
            })?
        }
    };
    GitObjectId::new(algorithm, commit_oid.as_bytes().to_vec())
        .map_err(|error| ProjectionCommitError::Git(error.to_string()))
}

/// Verify an armored Atomic commit signature over the exact unsigned payload.
///
/// The payload is domain-separated from binding payloads, so a commit
/// signature can never stand in for a binding signature (RFC §8.2).
pub fn verify_commit_signature(
    unsigned_payload: &[u8],
    signature_header_value: &str,
    public_key: &atomic_identity::PublicKey,
) -> Result<(), ProjectionCommitError> {
    let Some(signature) = unarmor_commit_signature(signature_header_value) else {
        return Err(ProjectionCommitError::SignatureHeaderMissing);
    };
    let mut message =
        Vec::with_capacity(COMMIT_SIGNATURE_DOMAIN.len() + unsigned_payload.len());
    message.extend_from_slice(COMMIT_SIGNATURE_DOMAIN);
    message.extend_from_slice(unsigned_payload);
    public_key
        .verify(&message, &signature)
        .map_err(|error| ProjectionCommitError::SigningFailed(error.to_string()))
}

/// Recover the exact unsigned payload of a signed commit object by removing
/// its `gpgsig` header (Git's rule for signed content).
pub fn strip_signature_header(raw_commit_bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(raw_commit_bytes.len());
    let mut lines = raw_commit_bytes.split_inclusive(|byte| *byte == b'\n').peekable();
    let mut in_signature = false;
    while let Some(line) = lines.next() {
        if in_signature {
            if line.starts_with(b" ") {
                // Signature continuation line: dropped.
                continue;
            }
            in_signature = false;
        }
        if line.starts_with(b"gpgsig ") {
            in_signature = true;
            continue;
        }
        output.extend_from_slice(line);
    }
    output
}

/// Extract the `gpgsig` header value from raw commit bytes.
pub fn signature_header_value(raw_commit_bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw_commit_bytes);
    let mut value = String::new();
    let mut in_signature = false;
    for line in text.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("gpgsig ") {
            in_signature = true;
            value.push_str(rest.trim_end_matches('\n'));
        } else if in_signature {
            if let Some(rest) = line.strip_prefix(' ') {
                value.push('\n');
                value.push_str(rest.trim_end_matches('\n'));
            } else {
                break;
            }
        }
    }
    (!value.is_empty()).then_some(value)
}

/// Write a foreign commit's exact raw bytes into the Git object database,
/// preserving its original OID byte-for-byte (RFC §5.2/§8.2). Used to
/// re-export imported/resurrected commits — the original signed object is
/// never reconstructed or re-signed.
pub fn write_raw_git_object(
    git: &git2::Repository,
    raw_bytes: &[u8],
) -> Result<GitObjectId, ProjectionCommitError> {
    let (kind, content) = split_raw_object(raw_bytes)?;
    let oid = git
        .odb()
        .map_err(git_error)?
        .write(kind, content)
        .map_err(git_error)?;
    let expected = GitObjectId::new(GitHashAlgorithm::Sha1, oid.as_bytes().to_vec())
        .map_err(|error| ProjectionCommitError::Git(error.to_string()))?;
    // Recompute the object identity independently: a repository whose ODB
    // disagrees with Git's own `<kind> <len>\0<content>` hashing is corrupt.
    let computed = git_object_oid(kind, content);
    if computed != expected {
        return Err(ProjectionCommitError::RawObjectIdentity {
            expected: hex_oid(&expected),
            computed: hex_oid(&computed),
        });
    }
    Ok(expected)
}

impl super::Repository {
    /// Prove that a whole-view merge publication contains the complete
    /// source closure (RFC §8.2).
    ///
    /// Every hash in `source_change_hashes` must be registered in this
    /// repository *and* visible in `target_view`'s effective projection
    /// closure. Any missing change fails closed, so a
    /// [`WholeViewMergeProof`] — and therefore a second commit parent — can
    /// never be obtained from a partial merge.
    pub fn prove_whole_view_merge(
        &self,
        target_view: &str,
        source_change_hashes: &[Hash],
        source_state: Merkle,
    ) -> Result<WholeViewMergeProof, ProjectionCommitError> {
        use atomic_core::pristine::{GraphTxnT, ViewTxnT};
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| ProjectionCommitError::Git(error.to_string()))?;
        let view = txn
            .get_view(target_view)
            .map_err(|error| ProjectionCommitError::Git(error.to_string()))?
            .ok_or_else(|| {
                ProjectionCommitError::Git(format!("target view '{target_view}' not found"))
            })?;
        let closure = super::projection::effective_projection_closure(&txn, &view)
            .map_err(|error| ProjectionCommitError::Git(error.to_string()))?;
        let mut visible = std::collections::BTreeSet::new();
        for change_id in closure.iter_dependency_first().copied() {
            visible.insert(change_id);
        }
        let mut verified = Vec::with_capacity(source_change_hashes.len());
        for hash in source_change_hashes {
            let internal = txn
                .get_internal(hash)
                .map_err(|error| ProjectionCommitError::Git(error.to_string()))?
                .ok_or_else(|| ProjectionCommitError::MergeProofIncomplete(hash.to_base32()))?;
            if !visible.contains(&internal) {
                return Err(ProjectionCommitError::MergeProofIncomplete(
                    hash.to_base32(),
                ));
            }
            verified.push(*hash);
        }
        Ok(WholeViewMergeProof {
            source_change_hashes: verified,
            source_state,
        })
    }
}

pub(super) fn git_object_oid(kind: ObjectType, content: &[u8]) -> GitObjectId {
    use sha1::Digest as _;
    let mut hasher = sha1::Sha1::new();
    let header = format!("{} {}\0", object_kind_name(kind), content.len());
    hasher.update(header.as_bytes());
    hasher.update(content);
    let bytes: [u8; 20] = hasher.finalize().into();
    GitObjectId::new(GitHashAlgorithm::Sha1, bytes.to_vec())
        .expect("a SHA-1 digest is a valid 20-byte object id")
}

fn object_kind_name(kind: ObjectType) -> &'static str {
    match kind {
        ObjectType::Commit => "commit",
        ObjectType::Tree => "tree",
        ObjectType::Blob => "blob",
        ObjectType::Tag => "tag",
        _ => "unknown",
    }
}

fn split_raw_object(raw: &[u8]) -> Result<(ObjectType, &[u8]), ProjectionCommitError> {
    let nul = raw
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| ProjectionCommitError::RawObjectIdentity {
            expected: String::new(),
            computed: "no header terminator".into(),
        })
        .map_err(|_| ProjectionCommitError::Git("raw object has no header terminator".into()))?;
    let header = std::str::from_utf8(&raw[..nul])
        .map_err(|error| ProjectionCommitError::Git(format!("raw object header: {error}")))?;
    let (kind_text, length_text) = header
        .split_once(' ')
        .ok_or_else(|| ProjectionCommitError::Git("raw object header is malformed".into()))?;
    let kind = match kind_text {
        "commit" => ObjectType::Commit,
        "tree" => ObjectType::Tree,
        "blob" => ObjectType::Blob,
        "tag" => ObjectType::Tag,
        other => {
            return Err(ProjectionCommitError::Git(format!(
                "unsupported raw object kind '{other}'"
            )))
        }
    };
    let length: usize = length_text
        .parse()
        .map_err(|error| ProjectionCommitError::Git(format!("raw object length: {error}")))?;
    let content = &raw[nul + 1..];
    if content.len() != length {
        return Err(ProjectionCommitError::Git(format!(
            "raw object length mismatch: header claims {length}, payload has {}",
            content.len()
        )));
    }
    Ok((kind, content))
}

fn to_git2_oid(oid: &GitObjectId) -> Result<git2::Oid, ProjectionCommitError> {
    if oid.algorithm() != GitHashAlgorithm::Sha1 {
        return Err(ProjectionCommitError::Git(format!(
            "libgit2 cannot address {:?} object ids",
            oid.algorithm()
        )));
    }
    git2::Oid::from_bytes(oid.as_bytes()).map_err(git_error)
}

pub(super) fn hex_oid(oid: &GitObjectId) -> String {
    oid.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_fixture() -> (tempfile::TempDir, git2::Repository) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let repo = git2::Repository::init(dir.path()).expect("init git repo");
        (dir, repo)
    }

    fn empty_tree(git: &git2::Repository) -> GitObjectId {
        let builder = git.treebuilder(None).expect("treebuilder");
        let oid = builder.write().expect("empty tree");
        GitObjectId::new(GitHashAlgorithm::Sha1, oid.as_bytes().to_vec()).unwrap()
    }

    fn tree_with_file(git: &git2::Repository, name: &str, bytes: &[u8]) -> GitObjectId {
        let blob = git.blob(bytes).expect("blob");
        let mut builder = git.treebuilder(None).expect("treebuilder");
        builder
            .insert(name, blob, 0o100644)
            .expect("insert entry");
        let oid = builder.write().expect("tree");
        GitObjectId::new(GitHashAlgorithm::Sha1, oid.as_bytes().to_vec()).unwrap()
    }

    fn parent_commit(git: &git2::Repository, tree: &GitObjectId) -> GitObjectId {
        let tree = git
            .find_tree(to_git2_oid(tree).unwrap())
            .expect("parent tree");
        let signature = git2::Signature::now("seed", "seed@example.com").unwrap();
        let oid = git
            .commit(None, &signature, &signature, "seed", &tree, &[])
            .expect("seed commit");
        GitObjectId::new(GitHashAlgorithm::Sha1, oid.as_bytes().to_vec()).unwrap()
    }

    fn author() -> ProjectionAuthor {
        ProjectionAuthor {
            name: "Aaron Ogle".into(),
            email: "aaron@atomic.dev".into(),
            did: "did:atomic:AAA".into(),
            agent_did: None,
        }
    }

    fn input(
        message: &str,
        state: Merkle,
        author: ProjectionAuthor,
        timestamp: i64,
    ) -> ProjectionCommitInput {
        ProjectionCommitInput {
            message: message.to_string(),
            set_id: SetId::ZERO,
            state,
            binding_id: None,
            conflict_set_hash: None,
            author,
            timestamp,
            timestamp_offset: 0,
        }
    }

    fn raw_bytes_of(git: &git2::Repository, oid: git2::Oid) -> Vec<u8> {
        let content = object_content_of(git, oid);
        let header = format!("commit {}\0", content.len());
        let mut raw = header.into_bytes();
        raw.extend_from_slice(&content);
        raw
    }

    /// Full object content (headers + message), without the `commit N\0`
    /// prefix — the same bytes `git cat-file commit <oid>` prints.
    fn object_content_of(git: &git2::Repository, oid: git2::Oid) -> Vec<u8> {
        let odb = git.odb().expect("odb");
        let object = odb.read(oid).expect("object");
        object.data().to_vec()
    }

    // ── Equal sets / distinct metadata (ac-1) ───────────────────────────

    #[test]
    fn identical_metadata_reproduces_the_identical_commit() {
        let (_dir, git) = git_fixture();
        let tree = empty_tree(&git);
        let parent = parent_commit(&git, &tree);
        let build = |timestamp: i64| {
            write_projected_commit(
                &git,
                &tree,
                ProjectionParents::Single { parent: parent.clone() },
                &input("m", Merkle::of(b"s"), author(), timestamp),
                &ProjectionSigning::Unsigned,
            )
            .unwrap()
        };
        assert_eq!(build(1_700_000_000), build(1_700_000_000));
    }

    #[test]
    fn distinct_operation_metadata_produces_distinct_commits() {
        let (_dir, git) = git_fixture();
        let tree = empty_tree(&git);
        let parent = parent_commit(&git, &tree);
        let parents = ProjectionParents::Single { parent };
        let base = write_projected_commit(
            &git,
            &tree,
            parents.clone(),
            &input("m", Merkle::of(b"s"), author(), 1_700_000_000),
            &ProjectionSigning::Unsigned,
        )
        .unwrap();
        let later = write_projected_commit(
            &git,
            &tree,
            parents.clone(),
            &input("m", Merkle::of(b"s"), author(), 1_700_000_001),
            &ProjectionSigning::Unsigned,
        )
        .unwrap();
        assert_ne!(base, later, "timestamps are operation metadata, not SetId");
        let other_identity = ProjectionAuthor {
            did: "did:atomic:BBB".into(),
            ..author()
        };
        let by_identity = write_projected_commit(
            &git,
            &tree,
            parents,
            &input("m", Merkle::of(b"s"), other_identity, 1_700_000_000),
            &ProjectionSigning::Unsigned,
        )
        .unwrap();
        assert_ne!(base, by_identity, "identity is operation metadata");
    }

    // ── Parentage (ac-1) ────────────────────────────────────────────────

    #[test]
    fn default_parentage_is_exactly_one_parent() {
        let (_dir, git) = git_fixture();
        let tree = empty_tree(&git);
        let parent = parent_commit(&git, &tree);
        let oid = write_projected_commit(
            &git,
            &tree,
            ProjectionParents::Single { parent: parent.clone() },
            &input("m", Merkle::ZERO, author(), 0),
            &ProjectionSigning::Unsigned,
        )
        .unwrap();
        let commit = git.find_commit(to_git2_oid(&oid).unwrap()).unwrap();
        assert_eq!(commit.parent_count(), 1);
        assert_eq!(
            commit.parent_id(0).unwrap(),
            to_git2_oid(&parent).unwrap()
        );
    }

    #[test]
    fn a_second_parent_requires_a_whole_view_proof() {
        let (_dir, git) = git_fixture();
        let tree = empty_tree(&git);
        let parent = parent_commit(&git, &tree);
        // A proof with no verified change hashes cannot exist through
        // prove_whole_view_merge; constructing one by hand is refused.
        let forged = WholeViewMergeProof {
            source_change_hashes: Vec::new(),
            source_state: Merkle::ZERO,
        };
        let error = write_projected_commit(
            &git,
            &tree,
            ProjectionParents::Merge {
                mainline: parent.clone(),
                merged: parent,
                proof: forged,
            },
            &input("m", Merkle::ZERO, author(), 0),
            &ProjectionSigning::Unsigned,
        )
        .unwrap_err();
        assert!(
            matches!(error, ProjectionCommitError::MergeProofRequired),
            "empty proof fails closed: {error}"
        );
    }

    #[test]
    fn merge_parents_keep_declared_order() {
        let (_dir, git) = git_fixture();
        let tree = empty_tree(&git);
        let mainline = parent_commit(&git, &tree);
        let merged = parent_commit(&git, &tree);
        let proof = WholeViewMergeProof {
            source_change_hashes: vec![Hash::of(b"complete-closure")],
            source_state: Merkle::of(b"src"),
        };
        let oid = write_projected_commit(
            &git,
            &tree,
            ProjectionParents::Merge {
                mainline: mainline.clone(),
                merged: merged.clone(),
                proof,
            },
            &input("whole-view merge", Merkle::of(b"s"), author(), 0),
            &ProjectionSigning::Unsigned,
        )
        .unwrap();
        let commit = git.find_commit(to_git2_oid(&oid).unwrap()).unwrap();
        assert_eq!(commit.parent_count(), 2);
        assert_eq!(commit.parent_id(0).unwrap(), to_git2_oid(&mainline).unwrap());
        assert_eq!(commit.parent_id(1).unwrap(), to_git2_oid(&merged).unwrap());
    }

    // ── Message headers (ac-3) ──────────────────────────────────────────

    #[test]
    fn message_carries_binding_set_state_and_author_headers() {
        let mut map = BTreeMap::new();
        map.insert("aaron@atomic.dev".to_string(), "did:atomic:U4NN".to_string());
        let mapped = ProjectionAuthor::resolve(
            &ProjectionIdentityMap::new(map),
            "Aaron Ogle",
            "aaron@atomic.dev",
            None,
        );
        let commit_input = ProjectionCommitInput {
            message: "feat: add auth\n".into(),
            set_id: SetId::of([Merkle::of(b"one"), Merkle::of(b"two")].iter()),
            state: Merkle::of(b"state"),
            binding_id: Some(crate::git_binding::BindingId::from_bytes([7u8; 32])),
            conflict_set_hash: None,
            author: mapped,
            timestamp: 0,
            timestamp_offset: 0,
        };
        let message = projection_commit_message(&commit_input);
        assert!(message.starts_with("feat: add auth\n"), "{message}");
        let full_hex: String = [7u8; 32].iter().map(|byte| format!("{byte:02x}")).collect();
        assert!(
            message.contains(&format!("\natomic-binding {full_hex}\n")),
            "binding header present: {message}"
        );
        assert!(message.contains("\natomic-set "), "{message}");
        assert!(message.contains("\natomic-state "), "{message}");
        assert!(
            message.contains("\natomic-author did:atomic:U4NN\n"),
            "mapped DID attribution: {message}"
        );
        assert!(
            !message.contains("atomic-agent") && !message.contains("Co-authored-by:"),
            "no agent metadata without an agent turn: {message}"
        );
    }

    #[test]
    fn agent_turns_add_agent_header_and_co_author_trailer() {
        let with_agent = ProjectionAuthor {
            agent_did: Some("did:atomic:AGENT".into()),
            ..author()
        };
        let message = projection_commit_message(&input("m", Merkle::ZERO, with_agent, 0));
        assert!(
            message.contains("atomic-agent did:atomic:AGENT\n"),
            "{message}"
        );
        assert!(
            message.contains("Co-authored-by: Aaron Ogle <did:atomic:AGENT>\n"),
            "{message}"
        );
    }

    #[test]
    fn messages_without_bindings_omit_the_binding_header() {
        let message = projection_commit_message(&input("m", Merkle::ZERO, author(), 0));
        assert!(!message.contains("atomic-binding"), "{message}");
    }

    // ── Identity mapping (ac-3) ─────────────────────────────────────────

    #[test]
    fn mapped_email_uses_the_configured_did() {
        let mut map = BTreeMap::new();
        map.insert("lee@atomic.dev".to_string(), "did:atomic:U4NN".to_string());
        let map = ProjectionIdentityMap::new(map);
        assert_eq!(map.did_for_email("lee@atomic.dev"), "did:atomic:U4NN");
        assert!(map.is_mapped("lee@atomic.dev"));
    }

    #[test]
    fn unmapped_email_gets_a_deterministic_foreign_did() {
        let map = ProjectionIdentityMap::default();
        let first = map.did_for_email("stranger@example.com");
        assert_eq!(first, map.did_for_email("stranger@example.com"));
        assert!(first.starts_with("did:atomic:git:"), "{first}");
        assert_ne!(first, map.did_for_email("other@example.com"));
        assert!(!map.is_mapped("stranger@example.com"));
    }

    #[test]
    fn blank_identity_entries_are_ignored() {
        let mut map = BTreeMap::new();
        map.insert("empty@x.dev".to_string(), "  ".to_string());
        map.insert("".to_string(), "did:atomic:X".to_string());
        let map = ProjectionIdentityMap::new(map);
        assert!(!map.is_mapped("empty@x.dev"));
        assert!(!map.is_mapped(""));
    }

    // ── Signing (ac-3) ──────────────────────────────────────────────────

    fn test_secret(seed: u8) -> [u8; 32] {
        let mut secret = [0u8; 32];
        for (index, byte) in secret.iter_mut().enumerate() {
            *byte = seed.wrapping_add((index as u8).wrapping_mul(13).wrapping_add(5));
        }
        secret
    }

    #[test]
    fn required_signing_embeds_a_verifiable_signature() {
        let (_dir, git) = git_fixture();
        let tree = tree_with_file(&git, "file.txt", b"payload\n");
        let parent = parent_commit(&git, &tree);
        let secret = atomic_identity::SecretKey::from_bytes(&test_secret(0x2A));
        let verification_keypair = atomic_identity::KeyPair::from_secret_key(secret);
        let signing_secret = atomic_identity::SecretKey::from_bytes(&test_secret(0x2A));
        let oid = write_projected_commit(
            &git,
            &tree,
            ProjectionParents::Single { parent },
            &input("signed", Merkle::of(b"s"), author(), 42),
            &ProjectionSigning::RequiredAtomic {
                keypair: atomic_identity::KeyPair::from_secret_key(signing_secret),
            },
        )
        .expect("signed commit");

        let content = object_content_of(&git, to_git2_oid(&oid).unwrap());
        let header = signature_header_value(&content).expect("gpgsig header");
        assert!(header.contains(ATOMIC_SIGNATURE_BEGIN), "{header}");
        let unsigned = strip_signature_header(&content);
        // The stripped content is exactly what was signed, domain-separated.
        verify_commit_signature(&unsigned, &header, &verification_keypair.public)
            .expect("signature verifies over the exact unsigned payload");
        // A tampered payload fails.
        let mut tampered = unsigned.clone();
        let position = tampered
            .windows(6)
            .position(|window| window == b"signed")
            .expect("message marker present");
        tampered[position..position + 6].copy_from_slice(b"tamper");
        assert!(verify_commit_signature(&tampered, &header, &verification_keypair.public).is_err());
        // A different key fails.
        assert!(verify_commit_signature(&unsigned, &header, &atomic_identity::KeyPair::generate().public).is_err());
    }

    #[test]
    fn unsigned_commits_carry_no_signature_header() {
        let (_dir, git) = git_fixture();
        let tree = empty_tree(&git);
        let parent = parent_commit(&git, &tree);
        let oid = write_projected_commit(
            &git,
            &tree,
            ProjectionParents::Single { parent },
            &input("plain", Merkle::ZERO, author(), 0),
            &ProjectionSigning::Unsigned,
        )
        .unwrap();
        let raw = raw_bytes_of(&git, to_git2_oid(&oid).unwrap());
        assert!(signature_header_value(&raw).is_none());
    }

    #[test]
    fn commit_signatures_are_separate_from_binding_signatures() {
        let keypair = atomic_identity::KeyPair::generate();
        let payload = b"unsigned commit payload".to_vec();
        // Commit signature: COMMIT_SIGNATURE_DOMAIN || payload.
        let mut commit_message = Vec::new();
        commit_message.extend_from_slice(COMMIT_SIGNATURE_DOMAIN);
        commit_message.extend_from_slice(&payload);
        let commit_signature = atomic_identity::Signer::new(&keypair).sign(&commit_message);

        // A binding-style payload (different domain, as GitStateBinding uses
        // BINDING_SIGN_DOMAIN over canonical payload bytes).
        let mut binding_message = b"atomic-binding-domain-v1\0".to_vec();
        binding_message.extend_from_slice(&payload);
        assert!(
            keypair.verify(&binding_message, commit_signature.as_bytes()).is_err(),
            "a commit signature never verifies as a binding signature"
        );
        assert!(
            keypair.verify(&payload, commit_signature.as_bytes()).is_err(),
            "the raw payload alone does not verify without the domain separator"
        );
        keypair
            .verify(&commit_message, commit_signature.as_bytes())
            .expect("the commit signature covers its exact domain-separated payload");
    }

    // ── Raw foreign objects (ac-3) ──────────────────────────────────────

    #[test]
    fn raw_commit_round_trips_byte_for_byte_with_original_oid() {
        let (_dir, git) = git_fixture();
        let tree = empty_tree(&git);
        let tree = git.find_tree(to_git2_oid(&tree).unwrap()).unwrap();
        let signature = git2::Signature::now("Foreign", "foreign@example.com").unwrap();
        let oid = git
            .commit(None, &signature, &signature, "foreign root", &tree, &[])
            .unwrap();
        let original = raw_bytes_of(&git, oid);

        let restored = write_raw_git_object(&git, &original).expect("raw write");
        assert_eq!(
            hex_oid(&restored),
            oid.to_string(),
            "the original OID is preserved exactly"
        );
        assert_eq!(
            raw_bytes_of(&git, to_git2_oid(&restored).unwrap()),
            original,
            "bytes survive byte-for-byte"
        );
    }

    #[test]
    fn signed_foreign_objects_keep_their_signature_bytes() {
        let (_dir, git) = git_fixture();
        let tree = empty_tree(&git);
        let tree = git.find_tree(to_git2_oid(&tree).unwrap()).unwrap();
        let signature = git2::Signature::now("Signer", "signer@example.com").unwrap();
        let unsigned = git
            .commit_create_buffer(&signature, &signature, "foreign signed", &tree, &[])
            .unwrap();
        let content = String::from_utf8(unsigned.to_vec()).unwrap();
        let armored = "-----BEGIN PGP SIGNATURE-----\nforeign-signature\n-----END PGP SIGNATURE-----\n";
        let oid = git.commit_signed(&content, armored, None).unwrap();
        let original = raw_bytes_of(&git, oid);

        let restored = write_raw_git_object(&git, &original).expect("raw write");
        assert_eq!(restored.as_bytes(), oid.as_bytes(), "signed OID preserved");
        let rewritten = raw_bytes_of(&git, to_git2_oid(&restored).unwrap());
        assert_eq!(original, rewritten, "raw bytes unchanged");
        assert!(
            signature_header_value(&rewritten)
                .expect("signature survives")
                .contains("foreign-signature"),
            "the original foreign signature is preserved verbatim"
        );
    }

    #[test]
    fn corrupted_raw_objects_are_refused() {
        let (_dir, git) = git_fixture();
        assert!(write_raw_git_object(&git, b"commit 999\0not really").is_err());
        assert!(write_raw_git_object(&git, b"no header at all").is_err());
        assert!(write_raw_git_object(&git, b"wat 3\0abc").is_err());
    }

    // ── Timestamps (ac-3) ───────────────────────────────────────────────

    #[test]
    fn operation_timestamps_reach_the_commit_object() {
        let (_dir, git) = git_fixture();
        let tree = empty_tree(&git);
        let parent = parent_commit(&git, &tree);
        let oid = write_projected_commit(
            &git,
            &tree,
            ProjectionParents::Single { parent },
            &ProjectionCommitInput {
                message: "tz".into(),
                set_id: SetId::ZERO,
                state: Merkle::ZERO,
                binding_id: None,
                conflict_set_hash: None,
                author: author(),
                timestamp: 1_700_000_000,
                timestamp_offset: 120,
            },
            &ProjectionSigning::Unsigned,
        )
        .unwrap();
        let commit = git.find_commit(to_git2_oid(&oid).unwrap()).unwrap();
        assert_eq!(commit.time().seconds(), 1_700_000_000);
        assert_eq!(commit.time().offset_minutes(), 120);
    }

    // ── Signed-content recovery helpers ─────────────────────────────────

    #[test]
    fn strip_signature_header_recovers_the_signed_payload() {
        let raw = "tree 1111111111111111111111111111111111111111\n\
                   gpgsig -----BEGIN ATOMIC COMMIT SIGNATURE-----\n \
                   AAAA\n \
                   -----END ATOMIC COMMIT SIGNATURE-----\n\
                   \nbody\n";
        let stripped = String::from_utf8(strip_signature_header(raw.as_bytes())).unwrap();
        assert!(!stripped.contains("gpgsig"), "{stripped}");
        assert!(stripped.starts_with("tree 1111"), "{stripped}");
        assert!(stripped.ends_with("\nbody\n"), "{stripped}");
    }

}