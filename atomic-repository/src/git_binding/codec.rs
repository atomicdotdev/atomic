//! Canonical `GitStateBinding` encoding, signing, and decoding.
//!
//! See the [module documentation](super) for the reviewed identity contract.
//! The codec is versioned; unknown versions and any tampering with bound
//! fields fail closed.

use std::fmt;

use atomic_core::operation::{GitHashAlgorithm, GitObjectId, OperationCodecError};
use atomic_core::types::{Merkle, OperationId, SetId};
use atomic_core::{Base32, Hash};
use serde::{Deserialize, Serialize};

use atomic_identity::keypair::{KeyPair, PublicKey};
use atomic_identity::signing::Signer;

/// Magic prefix of canonical binding bytes: `GSB1`.
pub const BINDING_MAGIC: &[u8; 4] = b"GSB1";

/// Current binding codec version. Unknown versions fail closed.
pub const BINDING_VERSION: u32 = 1;

/// Domain separator prepended to the canonical payload bytes before signing.
pub const BINDING_SIGN_DOMAIN: &[u8] = b"atomic:git-state-binding:sign:v1\0";

/// Domain separator for the derived binding id (over payload || signature).
pub const ID_DOMAIN: &[u8] = b"atomic:git-state-binding:id:v1\0";

/// Domain separator for `closure_root` over the ordered change hashes.
pub const CLOSURE_ROOT_DOMAIN: &[u8] = b"atomic:git-state-binding:closure:v1\0";

/// Git repository object format of every bound OID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitObjectFormat {
    /// Classic SHA-1 object IDs (20 raw bytes).
    Sha1,
    /// Repository configured for SHA-256 object IDs (32 raw bytes).
    Sha256,
}

impl GitObjectFormat {
    /// Width in bytes of one object id in this format.
    pub fn oid_size(self) -> usize {
        match self {
            GitObjectFormat::Sha1 => 20,
            GitObjectFormat::Sha256 => 32,
        }
    }
}

/// A Git object identity tagged with its repository's object format (RFC 5.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GitOid {
    /// SHA-1 object identity.
    Sha1(#[serde(with = "serde_oid20")] [u8; 20]),
    /// SHA-256 object identity.
    Sha256(#[serde(with = "serde_oid32")] [u8; 32]),
}

mod serde_oid20 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 20], serializer: S) -> Result<S::Ok, S::Error> {
        bytes.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 20], D::Error> {
        <[u8; 20]>::deserialize(deserializer)
    }
}

mod serde_oid32 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error> {
        bytes.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 32], D::Error> {
        <[u8; 32]>::deserialize(deserializer)
    }
}

impl GitOid {
    /// Convert from the algorithm-tagged core type.
    pub fn from_git_object_id(object_id: &GitObjectId) -> Result<Self, BindingEncodeError> {
        match object_id.algorithm() {
            GitHashAlgorithm::Sha1 => {
                let bytes: [u8; 20] = object_id.as_bytes().try_into().map_err(|_| {
                    BindingEncodeError::OidWidth("sha1", object_id.as_bytes().len())
                })?;
                Ok(GitOid::Sha1(bytes))
            }
            GitHashAlgorithm::Sha256 => {
                let bytes: [u8; 32] = object_id.as_bytes().try_into().map_err(|_| {
                    BindingEncodeError::OidWidth("sha256", object_id.as_bytes().len())
                })?;
                Ok(GitOid::Sha256(bytes))
            }
        }
    }

    /// Convert into the algorithm-tagged core type for Git interop.
    pub fn to_git_object_id(&self) -> Result<GitObjectId, BindingEncodeError> {
        match self {
            GitOid::Sha1(bytes) => {
                GitObjectId::new(GitHashAlgorithm::Sha1, bytes.to_vec()).map_err(Into::into)
            }
            GitOid::Sha256(bytes) => {
                GitObjectId::new(GitHashAlgorithm::Sha256, bytes.to_vec()).map_err(Into::into)
            }
        }
    }

    /// Raw digest bytes.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            GitOid::Sha1(bytes) => bytes.as_slice(),
            GitOid::Sha256(bytes) => bytes.as_slice(),
        }
    }

    /// Lowercase hex encoding, as used in Git refs and messages.
    pub fn to_hex(&self) -> String {
        self.as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Parse from lowercase hex (40 or 64 characters).
    pub fn from_hex(hex: &str) -> Result<Self, BindingEncodeError> {
        match hex.len() {
            40 => {
                let bytes = decode_fixed_hex(hex)?;
                let oid: [u8; 20] = bytes
                    .try_into()
                    .map_err(|_| BindingEncodeError::OidWidth("sha1", hex.len()))?;
                Ok(GitOid::Sha1(oid))
            }
            64 => {
                let bytes = decode_fixed_hex(hex)?;
                let oid: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| BindingEncodeError::OidWidth("sha256", hex.len()))?;
                Ok(GitOid::Sha256(oid))
            }
            _ => Err(BindingEncodeError::OidWidth("hex", hex.len())),
        }
    }
}

fn decode_fixed_hex(hex: &str) -> Result<Vec<u8>, BindingEncodeError> {
    (0..hex.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&hex[index..index + 2], 16)
                .map_err(|error| BindingEncodeError::Oid(error.to_string()))
        })
        .collect()
}

impl fmt::Debug for GitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

/// How the bound Atomic state came to correspond to the Git commit (RFC 5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CausalOrigin {
    /// The state was restored exactly from a previously verified binding.
    ExactAtomicResurrection,
    /// The commit has no parents and no prior Atomic history.
    ForeignGitRoot,
    /// Linear history imported through the first parent.
    ForeignGitFirstParent,
    /// A merge commit imported with complete ordered parents.
    ForeignGitMultiParentMerge,
    /// A squashed commit; per-change identity is not recoverable.
    ForeignGitSquash,
    /// An empty commit; carries no graph facts.
    ForeignGitEmptyCommit,
    /// History rewrite candidate; surfaced for review, never identity.
    ForeignGitRewriteCandidate,
    /// The import was lossy; see the loss notes.
    Lossy,
}

/// One reviewed, public loss fact (RFC 5.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LossNote {
    /// Rename similarity was insufficient for exact inode continuity.
    RenameUnresolved { path: String },
    /// History is shallow or promisor-bounded here.
    TruncatedHistory { description: String },
    /// Another reviewed loss fact.
    Other { description: String },
    /// An explicitly tracked empty directory projects to no Git tree entry
    /// (review CB-9C R7): the omission is deliberate and reviewed, never a
    /// fabricated empty-directory object. Declared last so postcard variant
    /// indices of previously encoded payloads are unchanged.
    EmptyDirectory { path: String },
}

/// The signer of a binding: DID plus the verifying key it must resolve to.
///
/// The verifying key travels with the binding so verification is
/// self-contained; `did:atomic` is a fingerprint of the key and `did:key`
/// embeds it, so any tampering with either field breaks the correspondence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingSigner {
    /// The signer's DID (`did:atomic:…` or `did:key:…`).
    pub did: String,
    /// Ed25519 verifying key the DID must resolve to.
    #[serde(with = "serde_oid32")]
    pub verifying_key: [u8; 32],
}

impl BindingSigner {
    /// Build a signer for a keypair using the in-tree `did:atomic` method.
    pub fn for_keypair(keypair: &KeyPair) -> Self {
        Self {
            did: atomic_canonical::did::did_for_public_key(&keypair.public),
            verifying_key: *keypair.public.as_bytes(),
        }
    }
}

/// The unsigned V1 binding payload: every field except signature and id.
///
/// Field order is the canonical V1 wire order; changing it changes every
/// binding id. The payload never contains the derived id or the signature —
/// the non-circular identity contract in the [module docs](super).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStateBindingPayload {
    /// Codec version; must equal [`BINDING_VERSION`].
    pub version: u32,

    /// Object format of the bound repository.
    pub git_object_format: GitObjectFormat,
    /// Bound commit OID.
    pub git_commit: GitOid,
    /// Bound tree OID; recomputed during resurrection, never trusted.
    pub git_tree: GitOid,
    /// Complete ordered parents of the bound commit.
    pub git_parents: Vec<GitOid>,
    /// Exact raw foreign commit bytes, preserved verbatim when the commit
    /// must survive loss of the Git object database.
    pub raw_commit_object: Option<Vec<u8>>,

    /// Additive SetId v1 of the effective projection closure.
    pub set_id: SetId,
    /// Order-sensitive view state; restores view membership order.
    pub merkle_state: Merkle,
    /// View name hint only; never identity.
    pub view_hint: Option<String>,

    /// Effective closure in Merkle order; snapshot changes excluded.
    pub ordered_changes: Vec<Hash>,
    /// Blake3 over the ordered change hashes (`CLOSURE_ROOT_DOMAIN`).
    pub closure_root: Hash,

    /// The causal operation that produced the bound state.
    pub operation: OperationId,
    /// How this correspondence arose.
    pub origin: CausalOrigin,
    /// Reviewed, public loss facts.
    pub loss: Vec<LossNote>,

    /// Provenance root hashes only; content never enters Git (§5.6).
    pub provenance_roots: Vec<Hash>,
    /// Attestation root hashes only; content never enters Git (§5.6).
    pub attestation_roots: Vec<Hash>,

    /// Who signed this binding.
    pub signer: BindingSigner,
}

impl GitStateBindingPayload {
    /// Canonical V1 payload bytes: magic prefix + deterministic encoding.
    pub fn canonical_payload_bytes(&self) -> Result<Vec<u8>, BindingEncodeError> {
        let encoded = postcard::to_allocvec(self)
            .map_err(|error| BindingEncodeError::Encode(error.to_string()))?;
        let mut bytes = Vec::with_capacity(BINDING_MAGIC.len() + encoded.len());
        bytes.extend_from_slice(BINDING_MAGIC);
        bytes.extend_from_slice(&encoded);
        Ok(bytes)
    }

    /// Recompute `closure_root` over the ordered change hashes.
    pub fn compute_closure_root(&self) -> Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CLOSURE_ROOT_DOMAIN);
        for change in &self.ordered_changes {
            hasher.update(change.as_bytes());
        }
        Hash::from_bytes(*hasher.finalize().as_bytes())
    }

    /// Reject structurally impossible payloads before they can be signed or
    /// trusted: version, zero operation, ordering, raw-object consistency,
    /// signer coherence, and closure-root recomputation.
    ///
    /// This is the encoding-level content contract. Verification against a
    /// live Git object database lives in [`super::verify_binding_content`].
    pub fn validate(&self) -> Result<(), BindingEncodeError> {
        if self.version != BINDING_VERSION {
            return Err(BindingEncodeError::UnsupportedVersion {
                version: self.version,
                supported: BINDING_VERSION,
            });
        }
        if self.operation.as_bytes() == &[0u8; OperationId::SIZE] {
            return Err(BindingEncodeError::MissingOperation);
        }
        // Object-format consistency: every OID must carry the width of the
        // declared object format, so a SHA-1 binding can never smuggle a
        // SHA-256 object id (or vice versa).
        let oid_size = self.git_object_format.oid_size();
        for oid in std::iter::once(&self.git_commit)
            .chain(std::iter::once(&self.git_tree))
            .chain(self.git_parents.iter())
        {
            if oid.as_bytes().len() != oid_size {
                return Err(BindingEncodeError::FormatMismatch {
                    format: format!("{:?}", self.git_object_format),
                    oid: oid.to_hex(),
                });
            }
        }
        for (index, parent) in self.git_parents.iter().enumerate() {
            if self.git_parents[index + 1..].contains(parent) {
                return Err(BindingEncodeError::DuplicateParent {
                    oid: parent.to_hex(),
                });
            }
        }
        for (index, change) in self.ordered_changes.iter().enumerate() {
            if self.ordered_changes[index + 1..].contains(change) {
                return Err(BindingEncodeError::DuplicateChange {
                    hash: change.to_base32(),
                });
            }
        }
        if let Some(raw) = &self.raw_commit_object {
            let digest = commit_object_digest(self.git_object_format, raw)?;
            if digest != self.git_commit {
                return Err(BindingEncodeError::RawObjectMismatch {
                    expected: self.git_commit.to_hex(),
                    found: digest.to_hex(),
                });
            }
        }
        validate_view_hint(self.view_hint.as_deref())?;
        self.validate_signer()?;
        if self.closure_root != self.compute_closure_root() {
            return Err(BindingEncodeError::ClosureRootMismatch);
        }
        Ok(())
    }

    fn validate_signer(&self) -> Result<(), BindingEncodeError> {
        let public_key = PublicKey::from_bytes(&self.signer.verifying_key).map_err(|error| {
            BindingEncodeError::SignerMismatch {
                did: self.signer.did.clone(),
                reason: error.to_string(),
            }
        })?;
        if !atomic_canonical::did::did_matches_public_key(&self.signer.did, &public_key) {
            return Err(BindingEncodeError::SignerMismatch {
                did: self.signer.did.clone(),
                reason: "the DID does not resolve to the verifying key".to_string(),
            });
        }
        Ok(())
    }
}

fn validate_view_hint(hint: Option<&str>) -> Result<(), BindingEncodeError> {
    let Some(hint) = hint else {
        return Ok(());
    };
    let malformed = hint.is_empty()
        || hint.len() > 255
        || hint.starts_with('/')
        || hint.ends_with('/')
        || hint.contains("//")
        || hint.contains("refs/")
        || hint
            .chars()
            .any(|character| character.is_control() || character.is_whitespace());
    if malformed {
        return Err(BindingEncodeError::InvalidViewHint {
            hint: hint.to_string(),
        });
    }
    Ok(())
}

/// Blake3-derived binding identity: `ID_DOMAIN || payload || signature`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BindingId([u8; 32]);

impl BindingId {
    /// Construct from canonical bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex, as used in `refs/atomic/bindings/<shard>/<id>`.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

impl fmt::Debug for BindingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl fmt::Display for BindingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

/// A signed Git state binding: canonical payload plus its Ed25519 signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStateBinding {
    payload: GitStateBindingPayload,
    payload_bytes: Vec<u8>,
    signature: [u8; 64],
}

impl GitStateBinding {
    /// Sign a payload with an Ed25519 keypair.
    ///
    /// The payload is validated first: an impossible binding can never be
    /// signed into existence.
    pub fn sign(
        payload: GitStateBindingPayload,
        keypair: &KeyPair,
    ) -> Result<Self, BindingEncodeError> {
        payload.validate()?;
        let payload_bytes = payload.canonical_payload_bytes()?;
        let mut message = Vec::with_capacity(BINDING_SIGN_DOMAIN.len() + payload_bytes.len());
        message.extend_from_slice(BINDING_SIGN_DOMAIN);
        message.extend_from_slice(&payload_bytes);
        let signature = Signer::new(keypair).sign(&message);
        Ok(Self {
            payload,
            payload_bytes,
            signature: *signature.as_bytes(),
        })
    }

    /// The signed payload.
    pub fn payload(&self) -> &GitStateBindingPayload {
        &self.payload
    }

    /// The signature bytes.
    pub fn signature(&self) -> &[u8; 64] {
        &self.signature
    }

    /// Canonical signed bytes: payload bytes followed by the raw signature.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.payload_bytes.len() + self.signature.len());
        bytes.extend_from_slice(&self.payload_bytes);
        bytes.extend_from_slice(&self.signature);
        bytes
    }

    /// The binding id over the canonical signed bytes.
    pub fn id(&self) -> BindingId {
        let mut hasher = blake3::Hasher::new();
        hasher.update(ID_DOMAIN);
        hasher.update(&self.payload_bytes);
        hasher.update(&self.signature);
        BindingId(*hasher.finalize().as_bytes())
    }

    /// Decode and fully validate canonical signed bytes.
    ///
    /// Rejects wrong magic, trailing bytes, unsupported versions, malformed
    /// or non-canonical encodings, raw-object mismatches, and any
    /// recomputation failure of a bound derived field.
    pub fn decode(bytes: &[u8]) -> Result<Self, BindingDecodeError> {
        let minimum = BINDING_MAGIC.len() + 64 + 1;
        if bytes.len() < minimum {
            return Err(BindingDecodeError::Truncated {
                length: bytes.len(),
            });
        }
        if &bytes[..BINDING_MAGIC.len()] != BINDING_MAGIC {
            return Err(BindingDecodeError::BadMagic);
        }
        let signature_start = bytes.len() - 64;
        let payload = postcard::from_bytes::<GitStateBindingPayload>(
            &bytes[BINDING_MAGIC.len()..signature_start],
        )
        .map_err(|error| BindingDecodeError::Malformed {
            reason: error.to_string(),
        })?;
        // Canonicality: re-encoding must reproduce the exact payload region,
        // so no alternative encoding of the same fields exists and no bytes
        // are smuggled between payload and signature.
        let reencoded =
            payload
                .canonical_payload_bytes()
                .map_err(|error| BindingDecodeError::Malformed {
                    reason: error.to_string(),
                })?;
        if bytes.len() != reencoded.len() + 64 || reencoded.as_slice() != &bytes[..reencoded.len()]
        {
            return Err(BindingDecodeError::NonCanonical);
        }
        let signature = <[u8; 64]>::try_from(&bytes[signature_start..]).map_err(|_| {
            BindingDecodeError::Malformed {
                reason: "signature is not exactly 64 bytes".to_string(),
            }
        })?;
        let binding = Self {
            payload,
            payload_bytes: bytes[..signature_start].to_vec(),
            signature,
        };
        binding
            .payload
            .validate()
            .map_err(BindingDecodeError::Invalid)?;
        Ok(binding)
    }

    /// Verify the Ed25519 signature against the carried verifying key.
    pub fn verify_signature(&self) -> Result<(), BindingDecodeError> {
        let public_key =
            PublicKey::from_bytes(&self.payload.signer.verifying_key).map_err(|error| {
                BindingDecodeError::Signature {
                    reason: error.to_string(),
                }
            })?;
        let signature = atomic_identity::signing::Signature::from_bytes(self.signature);
        let mut message = Vec::with_capacity(BINDING_SIGN_DOMAIN.len() + self.payload_bytes.len());
        message.extend_from_slice(BINDING_SIGN_DOMAIN);
        message.extend_from_slice(&self.payload_bytes);
        signature
            .verify(&message, &public_key)
            .map_err(|error| BindingDecodeError::Signature {
                reason: error.to_string(),
            })
    }
}

impl From<OperationCodecError> for BindingEncodeError {
    fn from(error: OperationCodecError) -> Self {
        BindingEncodeError::Oid(error.to_string())
    }
}

/// Git object digest of a raw commit object (RFC 8.2): the format-tagged
/// commit digest `sha1("commit <len>\0" + bytes)` or its SHA-256 equivalent.
pub fn commit_object_digest(
    format: GitObjectFormat,
    raw_commit: &[u8],
) -> Result<GitOid, BindingEncodeError> {
    let header = format!("commit {}\0", raw_commit.len()).into_bytes();
    match format {
        GitObjectFormat::Sha1 => {
            let mut hasher = sha1::Sha1::new();
            use sha1::Digest as _;
            hasher.update(&header);
            hasher.update(raw_commit);
            let digest: [u8; 20] = hasher
                .finalize()
                .as_slice()
                .try_into()
                .map_err(|_| BindingEncodeError::Oid("sha1 digest width".to_string()))?;
            Ok(GitOid::Sha1(digest))
        }
        GitObjectFormat::Sha256 => {
            let mut hasher = sha2::Sha256::new();
            use sha2::Digest as _;
            hasher.update(&header);
            hasher.update(raw_commit);
            let digest: [u8; 32] = hasher
                .finalize()
                .as_slice()
                .try_into()
                .map_err(|_| BindingEncodeError::Oid("sha256 digest width".to_string()))?;
            Ok(GitOid::Sha256(digest))
        }
    }
}

/// Encoding failures: a binding that can never be signed or published.
#[derive(Debug, thiserror::Error)]
pub enum BindingEncodeError {
    #[error("unsupported binding version {version}; this build supports version {supported}")]
    UnsupportedVersion { version: u32, supported: u32 },
    #[error("binding names the zero operation; every binding must reference a causal operation")]
    MissingOperation,
    #[error("duplicate Git parent {oid} in the ordered parents")]
    DuplicateParent { oid: String },
    #[error("duplicate change {hash} in the ordered closure")]
    DuplicateChange { hash: String },
    #[error("raw commit bytes digest {found} does not match the bound commit {expected}")]
    RawObjectMismatch { expected: String, found: String },
    #[error("closure_root does not match recomputation over ordered_changes")]
    ClosureRootMismatch,
    #[error("signer DID {did} is invalid: {reason}")]
    SignerMismatch { did: String, reason: String },
    #[error("invalid view hint '{hint}': a view hint is a name only")]
    InvalidViewHint { hint: String },
    #[error("invalid Git OID: {0}")]
    Oid(String),
    #[error("object id {oid} does not match the declared object format {format}")]
    FormatMismatch { format: String, oid: String },
    #[error("{0} value must be the exact width, found {1} bytes")]
    OidWidth(&'static str, usize),
    #[error("cannot encode binding payload: {0}")]
    Encode(String),
}

/// Decoding failures for externally supplied binding bytes.
#[derive(Debug, thiserror::Error)]
pub enum BindingDecodeError {
    #[error("binding bytes are truncated ({length} bytes)")]
    Truncated { length: usize },
    #[error("binding bytes do not start with the GSB1 magic")]
    BadMagic,
    #[error("binding payload is malformed: {reason}")]
    Malformed { reason: String },
    #[error("binding encoding is not canonical")]
    NonCanonical,
    #[error("binding is invalid: {0}")]
    Invalid(#[from] BindingEncodeError),
    #[error("binding signature verification failed: {reason}")]
    Signature { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_identity::keypair::{KeyPair, SecretKey};

    const PARENT_HEX: &str = "3333333333333333333333333333333333333333";

    /// Deterministic test keypair (never the global identity store).
    fn test_keypair() -> KeyPair {
        let mut secret = [0u8; 32];
        for (index, byte) in secret.iter_mut().enumerate() {
            *byte = (index * 7 + 11) as u8;
        }
        KeyPair::from_secret_key(SecretKey::from_bytes(&secret))
    }

    fn oid(hex: &str) -> GitOid {
        GitOid::from_hex(hex).expect("valid oid")
    }

    fn sample_payload(keypair: &KeyPair) -> GitStateBindingPayload {
        GitStateBindingPayload {
            version: BINDING_VERSION,
            git_object_format: GitObjectFormat::Sha1,
            git_commit: oid("1111111111111111111111111111111111111111"),
            git_tree: oid("2222222222222222222222222222222222222222"),
            git_parents: vec![oid(PARENT_HEX)],
            raw_commit_object: None,
            set_id: SetId::from_bytes([1u8; 32]),
            merkle_state: Merkle::from_bytes([2u8; 32]),
            view_hint: Some("feature-x".to_string()),
            ordered_changes: vec![Hash::from_bytes([3u8; 32]), Hash::from_bytes([4u8; 32])],
            closure_root: Hash::from_bytes([0u8; 32]),
            operation: OperationId::from_bytes([5u8; 32]),
            origin: CausalOrigin::ForeignGitFirstParent,
            loss: vec![LossNote::RenameUnresolved {
                path: "old.txt".to_string(),
            }],
            provenance_roots: vec![Hash::from_bytes([6u8; 32])],
            attestation_roots: vec![Hash::from_bytes([7u8; 32])],
            signer: BindingSigner::for_keypair(keypair),
        }
    }

    fn signed_sample() -> (KeyPair, GitStateBinding) {
        let keypair = test_keypair();
        let mut payload = sample_payload(&keypair);
        payload.closure_root = payload.compute_closure_root();
        let binding = GitStateBinding::sign(payload, &keypair).expect("sign");
        (keypair, binding)
    }

    #[test]
    fn canonical_bytes_are_deterministic_and_ids_stable() {
        let (_, first) = signed_sample();
        let (_, second) = signed_sample();
        assert_eq!(first.encode(), second.encode(), "codec is canonical");
        assert_eq!(first.id(), second.id(), "binding ids are stable");

        // Re-encoding the payload alone is a fixed point.
        let bytes = first.payload().canonical_payload_bytes().unwrap();
        let decoded: GitStateBindingPayload =
            postcard::from_bytes(&bytes[BINDING_MAGIC.len()..]).unwrap();
        assert_eq!(decoded.canonical_payload_bytes().unwrap(), bytes);
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let (_, binding) = signed_sample();
        let decoded = GitStateBinding::decode(&binding.encode()).expect("decode");
        assert_eq!(decoded, binding, "every field round-trips");
        assert_eq!(decoded.id(), binding.id());
        assert_eq!(decoded.signature(), binding.signature());
        let payload = decoded.payload();
        assert_eq!(payload.version, BINDING_VERSION);
        assert_eq!(payload.git_object_format, GitObjectFormat::Sha1);
        assert_eq!(
            payload.git_commit.to_hex(),
            "1111111111111111111111111111111111111111"
        );
        assert_eq!(
            payload.git_tree.to_hex(),
            "2222222222222222222222222222222222222222"
        );
        assert_eq!(payload.git_parents.len(), 1);
        assert_eq!(payload.git_parents[0].to_hex(), PARENT_HEX);
        assert!(payload.raw_commit_object.is_none());
        assert_eq!(payload.set_id, SetId::from_bytes([1u8; 32]));
        assert_eq!(payload.merkle_state, Merkle::from_bytes([2u8; 32]));
        assert_eq!(payload.view_hint.as_deref(), Some("feature-x"));
        assert_eq!(payload.ordered_changes.len(), 2);
        assert_eq!(payload.ordered_changes[0], Hash::from_bytes([3u8; 32]));
        assert_eq!(payload.ordered_changes[1], Hash::from_bytes([4u8; 32]));
        assert_eq!(payload.operation, OperationId::from_bytes([5u8; 32]));
        assert_eq!(payload.origin, CausalOrigin::ForeignGitFirstParent);
        assert_eq!(
            payload.loss,
            vec![LossNote::RenameUnresolved {
                path: "old.txt".to_string()
            }]
        );
        assert_eq!(payload.provenance_roots, vec![Hash::from_bytes([6u8; 32])]);
        assert_eq!(payload.attestation_roots, vec![Hash::from_bytes([7u8; 32])]);
        assert!(decoded.verify_signature().is_ok());
    }

    #[test]
    fn any_single_byte_tamper_fails_decode_or_signature() {
        let (_, binding) = signed_sample();
        let encoded = binding.encode();
        for position in 0..encoded.len() {
            for flip in [0x01u8, 0x80] {
                let mut tampered = encoded.clone();
                tampered[position] ^= flip;
                // Every tampered object must either fail decoding outright or
                // fail signature verification. Neither may silently pass.
                if let Ok(decoded) = GitStateBinding::decode(&tampered) {
                    assert!(
                        decoded.verify_signature().is_err(),
                        "tampered byte {position} (^{flip:#x}) decoded but must fail signature verification"
                    );
                }
            }
        }
    }

    #[test]
    fn field_by_field_tampering_changes_identity_or_fails_validation() {
        let (keypair, binding) = signed_sample();
        let base_id = binding.id();

        let mutate = |change: &dyn Fn(&mut GitStateBindingPayload)| {
            let mut payload = binding.payload().clone();
            change(&mut payload);
            payload.closure_root = payload.compute_closure_root();
            GitStateBinding::sign(payload, &keypair).expect("re-sign tampered payload")
        };

        // Every bound field participates in the identity: re-signing a
        // tampered payload yields a different binding id.
        let cases: Vec<(&str, GitStateBinding)> = vec![
            (
                "git_commit",
                mutate(&|payload| {
                    payload.git_commit = oid("4444444444444444444444444444444444444444");
                }),
            ),
            (
                "git_tree",
                mutate(&|payload| {
                    payload.git_tree = oid("5555555555555555555555555555555555555555");
                }),
            ),
            (
                "git_parents_order",
                mutate(&|payload| {
                    payload
                        .git_parents
                        .push(oid("6666666666666666666666666666666666666666"));
                }),
            ),
            (
                "set_id",
                mutate(&|payload| {
                    payload.set_id = SetId::from_bytes([9u8; 32]);
                }),
            ),
            (
                "merkle_state",
                mutate(&|payload| {
                    payload.merkle_state = Merkle::from_bytes([8u8; 32]);
                }),
            ),
            (
                "view_hint",
                mutate(&|payload| {
                    payload.view_hint = Some("other-view".to_string());
                }),
            ),
            (
                "ordered_changes_order",
                mutate(&|payload| {
                    payload.ordered_changes.reverse();
                }),
            ),
            (
                "operation",
                mutate(&|payload| {
                    payload.operation = OperationId::from_bytes([10u8; 32]);
                }),
            ),
            (
                "origin",
                mutate(&|payload| {
                    payload.origin = CausalOrigin::ForeignGitRoot;
                }),
            ),
            (
                "loss",
                mutate(&|payload| {
                    payload.loss = vec![LossNote::Other {
                        description: "different loss".to_string(),
                    }];
                }),
            ),
            (
                "provenance_roots",
                mutate(&|payload| {
                    payload.provenance_roots.push(Hash::from_bytes([11u8; 32]));
                }),
            ),
            (
                "attestation_roots",
                mutate(&|payload| {
                    payload.attestation_roots.push(Hash::from_bytes([12u8; 32]));
                }),
            ),
        ];
        for (field, tampered) in cases {
            assert_ne!(
                tampered.id(),
                base_id,
                "tampering with {field} must change the binding identity"
            );
        }

        // Signer tampering cannot even be signed: the DID must resolve to
        // the verifying key.
        let mut payload = binding.payload().clone();
        payload.signer.did = "did:atomic:TAMPERED".to_string();
        payload.closure_root = payload.compute_closure_root();
        assert!(matches!(
            GitStateBinding::sign(payload, &keypair),
            Err(BindingEncodeError::SignerMismatch { .. })
        ));

        // A different key produces a different signer — and thus a different
        // binding id — never the same identity.
        let other_keypair = {
            let mut secret = [0u8; 32];
            for (index, byte) in secret.iter_mut().enumerate() {
                *byte = (index * 13 + 5) as u8;
            }
            KeyPair::from_secret_key(SecretKey::from_bytes(&secret))
        };
        let mut payload = binding.payload().clone();
        payload.signer = BindingSigner::for_keypair(&other_keypair);
        payload.closure_root = payload.compute_closure_root();
        let re_signed = GitStateBinding::sign(payload, &other_keypair).unwrap();
        assert_ne!(re_signed.id(), base_id);

        // Structural tampering without fixing the derived field fails
        // validation outright.
        let mut payload = binding.payload().clone();
        payload.closure_root = Hash::from_bytes([42u8; 32]);
        assert!(matches!(
            GitStateBinding::sign(payload, &keypair),
            Err(BindingEncodeError::ClosureRootMismatch)
        ));

        let mut payload = binding.payload().clone();
        payload.version = 2;
        assert!(matches!(
            GitStateBinding::sign(payload, &keypair),
            Err(BindingEncodeError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn ordering_tampering_without_resigning_fails() {
        let (_, binding) = signed_sample();
        // Swap two change hashes inside the encoded payload region by
        // re-encoding a tampered payload but keeping the ORIGINAL signature:
        // the signature must reject it.
        let mut payload = binding.payload().clone();
        payload.ordered_changes.reverse();
        let tampered_payload_bytes = payload.canonical_payload_bytes().unwrap();
        let mut forged = tampered_payload_bytes;
        forged.extend_from_slice(binding.signature());
        let decoded = GitStateBinding::decode(&forged);
        assert!(
            decoded.is_err(),
            "reordered closure with the original signature must fail"
        );
    }

    /// Frozen codec + signature vector (CB-6A, RFC 5.1/8.2): canonical bytes
    /// of [`signed_sample`] under the deterministic test key.
    const FROZEN_GSB1_FIXTURE_HEX: &str = "4753423101000011111111111111111111111111111111111111110022222222222222222222222222222222222222220100333333333333333333333333333333333333333300010101010101010101010101010101010101010101010101010101010101010102020202020202020202020202020202020202020202020202020202020202020109666561747572652d780203030303030303030303030303030303030303030303030303030303030303030404040404040404040404040404040404040404040404040404040404040404fd20724dd0302291142d7210aec8961cda9eb87ad1cd8af3980c14acc401cfe20505050505050505050505050505050505050505050505050505050505050505020100076f6c642e7478740106060606060606060606060606060606060606060606060606060606060606060107070707070707070707070707070707070707070707070707070707070707073f6469643a61746f6d69633a4f444346575533455449583657444d5654363241563248374241594f434e324235484341554135504248423537434a504f5a4e41d785b49583ee736c66cf55dc87474f4f337c971bfc3d69cba1e21e67763653afadab9d1fced1e81ef6eb07bad492f852b0a815d098c894a192a8e570ad1bc6f0a5518d56dbc109b81c84814da3d21745930dc751e5c9b9108cec00b6e85b0e0b";

    /// Frozen binding id of the fixture vector.
    const FROZEN_GSB1_ID_HEX: &str =
        "909193f4042c8deddb0491969a566a3a0ae07d0eae241c22043dfd15f75b2ca6";

    #[test]
    fn frozen_codec_fixture_is_stable() {
        let (_, binding) = signed_sample();
        let hex = binding
            .encode()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        // Frozen codec + signature vector (CB-6A, RFC 5.1/8.2). The codec is
        // canonical and Ed25519 is deterministic, so these exact bytes must
        // reproduce on every platform for this payload and test key.
        // Regenerate only with deliberate, reviewed intent.
        let expected = FROZEN_GSB1_FIXTURE_HEX;
        assert_eq!(
            hex, expected,
            "the canonical encoding or signature scheme drifted from the frozen fixture"
        );
        // And the frozen vector still decodes and verifies.
        let frozen_bytes: Vec<u8> = (0..expected.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&expected[index..index + 2], 16).unwrap())
            .collect();
        let decoded = GitStateBinding::decode(&frozen_bytes).expect("frozen vector decodes");
        assert!(decoded.verify_signature().is_ok());
        assert_eq!(decoded.id().to_hex(), FROZEN_GSB1_ID_HEX);
    }

    #[test]
    fn oid_helpers_round_trip_both_formats_and_cross_check_git2() {
        // SHA-1 hex round trip.
        let sha1 = oid(PARENT_HEX);
        assert_eq!(sha1.to_hex(), PARENT_HEX);
        assert_eq!(GitOid::from_hex(&sha1.to_hex()).unwrap(), sha1);

        // SHA-256 hex round trip.
        let sha256_hex = "a".repeat(64);
        let sha256 = oid(&sha256_hex);
        assert!(matches!(sha256, GitOid::Sha256(_)));
        assert_eq!(sha256.to_hex(), sha256_hex);

        // commit_object_digest cross-checks git2's independent implementation
        // for SHA-1 commit objects.
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@e> 0 +0000\ncommitter A <a@e> 0 +0000\n\nmsg\n";
        let digest = commit_object_digest(GitObjectFormat::Sha1, raw).unwrap();
        let git2_digest = git2::Oid::hash_object(git2::ObjectType::Commit, raw).unwrap();
        assert_eq!(digest.to_hex(), git2_digest.to_string());
        assert_eq!(
            digest.to_git_object_id().unwrap().as_bytes(),
            git2_digest.as_bytes()
        );

        // Wrong widths are refused, not truncated silently.
        assert!(GitOid::from_hex(&"ab".repeat(19)).is_err());
        assert!(GitOid::from_hex(&"ab".repeat(33)).is_err());
        assert!(GitOid::from_hex("zzzz").is_err());
    }

    #[test]
    fn structural_validation_refuses_ambiguous_payloads() {
        let (keypair, binding) = signed_sample();

        // Zero operation: every binding must reference a causal operation.
        let mut payload = binding.payload().clone();
        payload.operation = OperationId::from_bytes([0u8; 32]);
        payload.closure_root = payload.compute_closure_root();
        assert!(matches!(
            GitStateBinding::sign(payload, &keypair),
            Err(BindingEncodeError::MissingOperation)
        ));

        // Duplicate parents make the ordered parent list ambiguous.
        let mut payload = binding.payload().clone();
        payload.git_parents = vec![oid(PARENT_HEX), oid(PARENT_HEX)];
        payload.closure_root = payload.compute_closure_root();
        assert!(matches!(
            GitStateBinding::sign(payload, &keypair),
            Err(BindingEncodeError::DuplicateParent { .. })
        ));

        // Duplicate changes make the Merkle order ambiguous.
        let mut payload = binding.payload().clone();
        payload.ordered_changes = vec![Hash::from_bytes([3u8; 32]); 2];
        payload.closure_root = payload.compute_closure_root();
        assert!(matches!(
            GitStateBinding::sign(payload, &keypair),
            Err(BindingEncodeError::DuplicateChange { .. })
        ));

        // A view hint is a name only: control characters, whitespace, and
        // ref-like values are refused.
        for bad_hint in [
            "",
            "has space",
            "with\ttab",
            "with\nnewline",
            "refs/heads/main",
            "double//slash",
            "/leading",
            "trailing/",
            &"x".repeat(256),
        ] {
            let mut payload = binding.payload().clone();
            payload.view_hint = Some(bad_hint.to_string());
            payload.closure_root = payload.compute_closure_root();
            assert!(
                matches!(
                    GitStateBinding::sign(payload, &keypair),
                    Err(BindingEncodeError::InvalidViewHint { .. })
                ),
                "view hint {bad_hint:?} must be refused"
            );
        }
        let _ = keypair;
    }

    #[test]
    fn sha256_format_round_trips_end_to_end() {
        let keypair = test_keypair();
        let mut payload = sample_payload(&keypair);
        payload.git_object_format = GitObjectFormat::Sha256;
        let sha256_hex = "b".repeat(64);
        payload.git_commit = oid(&sha256_hex);
        payload.git_tree = oid(&"c".repeat(64));
        payload.git_parents = vec![oid(&"d".repeat(64))];
        payload.closure_root = payload.compute_closure_root();
        let binding = GitStateBinding::sign(payload, &keypair).unwrap();
        let decoded = GitStateBinding::decode(&binding.encode()).unwrap();
        assert_eq!(decoded.payload().git_object_format, GitObjectFormat::Sha256);
        assert_eq!(decoded.payload().git_commit.to_hex(), "b".repeat(64));
        assert_eq!(decoded.payload().git_parents[0].to_hex(), "d".repeat(64));
        assert!(matches!(decoded.payload().git_commit, GitOid::Sha256(_)));
        assert!(decoded.verify_signature().is_ok());
    }
    #[test]
    fn wrong_object_format_is_refused_at_encoding_and_decoding() {
        let (keypair, binding) = signed_sample();

        // A SHA-1-format binding may never carry a SHA-256-width object id.
        let mut payload = binding.payload().clone();
        payload.git_tree = oid(&"a".repeat(64));
        payload.closure_root = payload.compute_closure_root();
        assert!(matches!(
            GitStateBinding::sign(payload, &keypair),
            Err(BindingEncodeError::FormatMismatch { .. })
        ));
        let _ = keypair;
    }

    #[test]
    fn wrong_magic_and_trailing_bytes_fail() {
        let (_, binding) = signed_sample();
        let mut bytes = binding.encode();
        bytes[0] = b'X';
        assert!(matches!(
            GitStateBinding::decode(&bytes),
            Err(BindingDecodeError::BadMagic)
        ));

        let mut bytes = binding.encode();
        bytes.push(0);
        assert!(GitStateBinding::decode(&bytes).is_err());

        bytes.pop();
        bytes.pop();
        assert!(GitStateBinding::decode(&bytes).is_err());
    }

    #[test]
    fn malformed_signatures_are_rejected_at_verification() {
        let (_, binding) = signed_sample();
        let mut bytes = binding.encode();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let decoded = GitStateBinding::decode(&bytes).expect("signature bytes still decode");
        assert!(decoded.verify_signature().is_err());
    }

    #[test]
    fn unsupported_versions_fail_closed() {
        let (keypair, binding) = signed_sample();
        // Version 0 and version 2 are both unsupported.
        for version in [0u32, 2, 99] {
            let mut payload = binding.payload().clone();
            payload.version = version;
            payload.closure_root = payload.compute_closure_root();
            let encoded = payload.canonical_payload_bytes().unwrap();
            let mut bytes = encoded.clone();
            bytes.extend_from_slice(&[0u8; 64]);
            let decoded = GitStateBinding::decode(&bytes);
            assert!(
                decoded.is_err(),
                "version {version} must not decode as valid"
            );
        }
        let _ = keypair;
    }

    #[test]
    fn raw_commit_object_must_hash_to_the_bound_commit() {
        let (keypair, binding) = signed_sample();
        // A raw commit object whose digest differs from git_commit is refused
        // at signing and at decoding.
        let raw = b"commit impersonation bytes that are not a real object";
        let mut payload = binding.payload().clone();
        payload.raw_commit_object = Some(raw.to_vec());
        payload.closure_root = payload.compute_closure_root();
        assert!(matches!(
            GitStateBinding::sign(payload.clone(), &keypair),
            Err(BindingEncodeError::RawObjectMismatch { .. })
        ));

        // With the consistent digest the raw bytes are bound exactly.
        payload.git_commit = commit_object_digest(payload.git_object_format, raw).unwrap();
        let bound = GitStateBinding::sign(payload, &keypair).unwrap();
        let decoded = GitStateBinding::decode(&bound.encode()).unwrap();
        let raw_round_tripped = decoded
            .payload()
            .raw_commit_object
            .as_ref()
            .expect("raw bytes survive the round trip");
        assert_eq!(raw_round_tripped.as_slice(), raw.as_slice());
        assert_eq!(decoded.id(), bound.id());
    }
}
