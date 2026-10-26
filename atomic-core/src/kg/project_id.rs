//! Stable repository identity, independent of working copies and view membership.

use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectId([u8; 32]);

#[derive(Debug, thiserror::Error)]
#[error("expected 52-character uppercase unpadded RFC 4648 base32 with zero trailing bits")]
pub struct ProjectIdError;

pub(crate) fn strict_base32(s: &str) -> Result<[u8; 32], ProjectIdError> {
    if s.len() != 52
        || !s
            .bytes()
            .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
    {
        return Err(ProjectIdError);
    }
    let bytes = data_encoding::BASE32_NOPAD
        .decode(s.as_bytes())
        .map_err(|_| ProjectIdError)?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| ProjectIdError)?;
    if data_encoding::BASE32_NOPAD.encode(&bytes) != s {
        return Err(ProjectIdError);
    }
    Ok(bytes)
}

impl ProjectId {
    pub fn generate() -> Self {
        let mut bytes = [0; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }
    pub fn from_base32(s: &str) -> Result<Self, ProjectIdError> {
        strict_base32(s).map(Self)
    }
    pub fn to_base32(self) -> String {
        data_encoding::BASE32_NOPAD.encode(&self.0)
    }
    pub fn to_urn(self) -> String {
        format!("urn:atomic:project:{}", self.to_base32())
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base32())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MintReason {
    /// Minted by `atomic init`/`atomic git import` at project creation.
    Created,
    /// Minted on a writable open of a repository predating identities.
    /// Marks the identity as replaceable by transport: a clone may adopt its
    /// origin's identity over an `Upgrade`-minted one.
    Upgrade,
    /// Carried over from the project's origin via clone/pull identity
    /// transport. Never minted locally.
    Adopted,
}

/// Singleton persisted outside the change/view system. `object` is canonical
/// JSON bytes; hash authority belongs to its domain-separated proof-free view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectIdentityRecord {
    pub object: Vec<u8>,
    pub hash: crate::types::Hash,
    pub mint_reason: MintReason,
    pub created_at: String,
    pub previous: Option<crate::types::Hash>,
}

impl ProjectIdentityRecord {
    pub fn project_id(&self) -> Result<ProjectId, ProjectIdError> {
        let object: serde_json::Value =
            serde_json::from_slice(&self.object).map_err(|_| ProjectIdError)?;
        ProjectId::from_base32(
            object
                .get("atomic:projectId")
                .and_then(serde_json::Value::as_str)
                .ok_or(ProjectIdError)?,
        )
    }
}
