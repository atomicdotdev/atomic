//! Identity types and structures for multi-identity support
//!
//! This module provides the core `Identity` struct and related types for
//! managing multiple user and agent identities in Atomic.
//!
//! # Identity Types
//!
//! Atomic supports three types of identities:
//!
//! - **User**: A human user's identity (most common)
//! - **Agent**: An AI or automated system identity
//! - **Delegated**: An agent acting on behalf of a user
//!
//! # Example
//!
//! ```rust
//! use atomic_identity::{Identity, IdentityType, IdentityUsage};
//!
//! // Create a personal identity
//! let identity = Identity::builder("alice")
//!     .email("alice@example.com")
//!     .usage(IdentityUsage::Personal)
//!     .build()?;
//!
//! // Create an agent identity
//! let agent = Identity::builder("ci-bot")
//!     .identity_type(IdentityType::Agent)
//!     .description("Continuous integration bot")
//!     .build()?;
//! # Ok::<(), atomic_identity::IdentityError>(())
//! ```

use crate::keypair::{KeyPair, PublicKey};
use crate::usage::IdentityUsage;
use crate::IdentityError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Unique identifier for an identity, derived from the public key.
///
/// This is the Blake3 hash of the public key bytes, providing a stable
/// identifier that can be used to reference identities without exposing
/// the full public key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdentityId([u8; 32]);

impl IdentityId {
    /// Create an identity ID from a public key.
    pub fn from_public_key(public_key: &PublicKey) -> Self {
        let hash = blake3::hash(public_key.as_bytes());
        IdentityId(*hash.as_bytes())
    }

    /// Create an identity ID from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        IdentityId(bytes)
    }

    /// Get the raw bytes of the identity ID.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Encode the identity ID as base32.
    pub fn to_base32(&self) -> String {
        data_encoding::BASE32_NOPAD.encode(&self.0)
    }

    /// Decode an identity ID from base32.
    pub fn from_base32(s: &str) -> Result<Self, IdentityError> {
        let bytes = data_encoding::BASE32_NOPAD
            .decode(s.as_bytes())
            .map_err(|_| IdentityError::InvalidBase32)?;

        if bytes.len() != 32 {
            return Err(IdentityError::InvalidKey(format!(
                "Expected 32 bytes, got {}",
                bytes.len()
            )));
        }

        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(IdentityId(arr))
    }

    /// Get a short form of the ID for display (first 8 characters).
    pub fn short(&self) -> String {
        self.to_base32()[..8].to_string()
    }
}

impl fmt::Debug for IdentityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IdentityId({})", self.short())
    }
}

impl fmt::Display for IdentityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_base32())
    }
}

/// The type of identity.
///
/// Determines the role and capabilities of the identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IdentityType {
    /// A human user's identity.
    ///
    /// This is the most common identity type, representing a person
    /// who creates and signs changes.
    #[default]
    User,

    /// An AI or automated system identity.
    ///
    /// Agents can create changes autonomously. Their contributions
    /// are tracked separately for attribution purposes.
    Agent,

    /// An agent acting on behalf of a user.
    ///
    /// Delegated identities have a reference to the delegating user
    /// and operate within a defined scope of authority.
    Delegated,
}

impl IdentityType {
    /// Get a human-readable description.
    pub fn description(&self) -> &'static str {
        match self {
            IdentityType::User => "User",
            IdentityType::Agent => "Agent",
            IdentityType::Delegated => "Delegated Agent",
        }
    }

    /// Check if this is a human user identity.
    #[inline]
    pub fn is_user(&self) -> bool {
        matches!(self, IdentityType::User)
    }

    /// Check if this is an agent identity (includes delegated).
    #[inline]
    pub fn is_agent(&self) -> bool {
        matches!(self, IdentityType::Agent | IdentityType::Delegated)
    }

    /// Check if this is a delegated identity.
    #[inline]
    pub fn is_delegated(&self) -> bool {
        matches!(self, IdentityType::Delegated)
    }
}

impl fmt::Display for IdentityType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

/// Metadata about an identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityMetadata {
    /// When the identity was created.
    pub created_at: DateTime<Utc>,

    /// When the identity was last modified.
    #[serde(default)]
    pub modified_at: Option<DateTime<Utc>>,

    /// When the identity expires (if set).
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,

    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,

    /// URL or reference for more information.
    #[serde(default)]
    pub url: Option<String>,

    /// Whether this identity is the default for its usage type.
    #[serde(default)]
    pub is_default: bool,
}

impl Default for IdentityMetadata {
    fn default() -> Self {
        Self {
            created_at: Utc::now(),
            modified_at: None,
            expires_at: None,
            description: None,
            url: None,
            is_default: false,
        }
    }
}

impl IdentityMetadata {
    /// Create metadata with the current timestamp.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if the identity has expired.
    pub fn is_expired(&self) -> bool {
        self.expires_at.map(|exp| exp < Utc::now()).unwrap_or(false)
    }

    /// Set the description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the expiration date.
    pub fn with_expiry(mut self, expires_at: DateTime<Utc>) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Mark as modified now.
    pub fn touch(&mut self) {
        self.modified_at = Some(Utc::now());
    }
}

/// A complete identity with cryptographic keys.
///
/// An identity represents a user or agent that can sign changes.
/// It includes:
/// - Display information (name, email)
/// - Cryptographic keys (public key, optionally secret key)
/// - Type and usage classification
/// - Metadata (creation time, expiry, etc.)
///
/// # Security
///
/// The secret key is optional because:
/// 1. You might only have the public key (for verification)
/// 2. The secret key might be stored separately (e.g., hardware key)
/// 3. The identity might be loaded without unlocking the secret
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    /// Unique identifier (derived from public key).
    pub id: IdentityId,

    /// Display name.
    pub name: String,

    /// Email address (optional).
    #[serde(default)]
    pub email: Option<String>,

    /// The identity's public key.
    pub public_key: PublicKey,

    /// The type of identity.
    #[serde(default)]
    pub identity_type: IdentityType,

    /// The usage context for this identity.
    #[serde(default)]
    pub usage: IdentityUsage,

    /// Identity metadata.
    #[serde(default)]
    pub metadata: IdentityMetadata,

    /// For delegated identities: the delegating user's ID.
    #[serde(default)]
    pub delegated_by: Option<IdentityId>,
}

impl Identity {
    /// Create a new identity from a keypair.
    pub fn new(name: impl Into<String>, keypair: &KeyPair) -> Self {
        let name = name.into();
        let public_key = keypair.public.clone();
        let id = IdentityId::from_public_key(&public_key);

        Self {
            id,
            name,
            email: None,
            public_key,
            identity_type: IdentityType::User,
            usage: IdentityUsage::Personal,
            metadata: IdentityMetadata::new(),
            delegated_by: None,
        }
    }

    /// Create an identity from just a public key (for verification).
    pub fn from_public_key(name: impl Into<String>, public_key: PublicKey) -> Self {
        let id = IdentityId::from_public_key(&public_key);

        Self {
            id,
            name: name.into(),
            email: None,
            public_key,
            identity_type: IdentityType::User,
            usage: IdentityUsage::Personal,
            metadata: IdentityMetadata::new(),
            delegated_by: None,
        }
    }

    /// Create a builder for constructing an identity.
    pub fn builder(name: impl Into<String>) -> IdentityBuilder {
        IdentityBuilder::new(name)
    }

    /// Generate a new identity with a fresh keypair.
    pub fn generate(name: impl Into<String>) -> Self {
        let keypair = KeyPair::generate();
        Self::new(name, &keypair)
    }

    /// Get the public key as a base32 string.
    pub fn public_key_base32(&self) -> String {
        self.public_key.to_base32()
    }

    /// Verify a signature made by this identity.
    pub fn verify(&self, message: &[u8], signature: &[u8; 64]) -> Result<(), IdentityError> {
        self.public_key.verify(message, signature)
    }

    /// Check if the identity has expired.
    pub fn is_expired(&self) -> bool {
        self.metadata.is_expired()
    }

    /// Check if this identity is valid (not expired, etc.).
    pub fn is_valid(&self) -> bool {
        !self.is_expired()
    }

    /// Get a short display string.
    pub fn display_short(&self) -> String {
        match &self.email {
            Some(email) => format!("{} <{}>", self.name, email),
            None => self.name.clone(),
        }
    }

    /// Convert this identity to an Author for change headers.
    ///
    /// This creates an Author struct compatible with atomic-core's
    /// change header format.
    pub fn to_author(&self) -> crate::Author {
        crate::Author {
            name: self.name.clone(),
            email: self.email.clone(),
            identity: Some(self.public_key_base32()),
        }
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_short())
    }
}

impl PartialEq for Identity {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Identity {}

impl std::hash::Hash for Identity {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// Builder for constructing identities.
pub struct IdentityBuilder {
    name: String,
    email: Option<String>,
    identity_type: IdentityType,
    usage: IdentityUsage,
    description: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    delegated_by: Option<IdentityId>,
    keypair: Option<KeyPair>,
    public_key: Option<PublicKey>,
}

impl IdentityBuilder {
    /// Create a new identity builder.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: None,
            identity_type: IdentityType::User,
            usage: IdentityUsage::Personal,
            description: None,
            expires_at: None,
            delegated_by: None,
            keypair: None,
            public_key: None,
        }
    }

    /// Set the email address.
    pub fn email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }

    /// Set the identity type.
    pub fn identity_type(mut self, identity_type: IdentityType) -> Self {
        self.identity_type = identity_type;
        self
    }

    /// Set the usage context.
    pub fn usage(mut self, usage: IdentityUsage) -> Self {
        self.usage = usage;
        self
    }

    /// Set a description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set an expiration date.
    pub fn expires_at(mut self, expires_at: DateTime<Utc>) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Set the delegating identity (for delegated identities).
    pub fn delegated_by(mut self, delegator: IdentityId) -> Self {
        self.delegated_by = Some(delegator);
        self.identity_type = IdentityType::Delegated;
        self
    }

    /// Use an existing keypair.
    pub fn keypair(mut self, keypair: KeyPair) -> Self {
        self.keypair = Some(keypair);
        self
    }

    /// Use an existing public key (verification-only identity).
    pub fn public_key(mut self, public_key: PublicKey) -> Self {
        self.public_key = Some(public_key);
        self
    }

    /// Build the identity, generating a keypair if needed.
    pub fn build(self) -> Result<Identity, IdentityError> {
        let public_key = match (self.keypair, self.public_key) {
            (Some(kp), _) => kp.public,
            (None, Some(pk)) => pk,
            (None, None) => KeyPair::generate().public,
        };

        let id = IdentityId::from_public_key(&public_key);

        let mut metadata = IdentityMetadata::new();
        if let Some(desc) = self.description {
            metadata.description = Some(desc);
        }
        if let Some(exp) = self.expires_at {
            metadata.expires_at = Some(exp);
        }

        Ok(Identity {
            id,
            name: self.name,
            email: self.email,
            public_key,
            identity_type: self.identity_type,
            usage: self.usage,
            metadata,
            delegated_by: self.delegated_by,
        })
    }
}

/// Author information compatible with atomic-core's change headers.
///
/// This is a re-export friendly struct that can be used to create
/// Author entries for change headers.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Author {
    /// Display name of the author.
    pub name: String,

    /// Email address (optional).
    #[serde(default)]
    pub email: Option<String>,

    /// Public key reference (optional).
    #[serde(default)]
    pub identity: Option<String>,
}

impl Author {
    /// Create a new author.
    pub fn new(name: impl Into<String>, email: Option<impl Into<String>>) -> Self {
        Self {
            name: name.into(),
            email: email.map(Into::into),
            identity: None,
        }
    }

    /// Create an author with an identity reference.
    pub fn with_identity(
        name: impl Into<String>,
        email: Option<impl Into<String>>,
        identity: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            email: email.map(Into::into),
            identity: Some(identity.into()),
        }
    }

    /// Create an author from an identity.
    pub fn from_identity(identity: &Identity) -> Self {
        Self {
            name: identity.name.clone(),
            email: identity.email.clone(),
            identity: Some(identity.public_key_base32()),
        }
    }
}

impl fmt::Display for Author {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.email {
            Some(email) => write!(f, "{} <{}>", self.name, email),
            None => write!(f, "{}", self.name),
        }
    }
}

impl From<&Identity> for Author {
    fn from(identity: &Identity) -> Self {
        Author::from_identity(identity)
    }
}

impl From<Identity> for Author {
    fn from(identity: Identity) -> Self {
        Author::from_identity(&identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_id_from_public_key() {
        let keypair = KeyPair::generate();
        let id = IdentityId::from_public_key(&keypair.public);

        // Should be deterministic
        let id2 = IdentityId::from_public_key(&keypair.public);
        assert_eq!(id, id2);
    }

    #[test]
    fn test_identity_id_base32_roundtrip() {
        let keypair = KeyPair::generate();
        let id = IdentityId::from_public_key(&keypair.public);
        let base32 = id.to_base32();
        let recovered = IdentityId::from_base32(&base32).unwrap();
        assert_eq!(id, recovered);
    }

    #[test]
    fn test_identity_id_short() {
        let keypair = KeyPair::generate();
        let id = IdentityId::from_public_key(&keypair.public);
        let short = id.short();
        assert_eq!(short.len(), 8);
    }

    #[test]
    fn test_identity_type_is_user() {
        assert!(IdentityType::User.is_user());
        assert!(!IdentityType::Agent.is_user());
        assert!(!IdentityType::Delegated.is_user());
    }

    #[test]
    fn test_identity_type_is_agent() {
        assert!(!IdentityType::User.is_agent());
        assert!(IdentityType::Agent.is_agent());
        assert!(IdentityType::Delegated.is_agent());
    }

    #[test]
    fn test_identity_generate() {
        let identity = Identity::generate("alice");
        assert_eq!(identity.name, "alice");
        assert_eq!(identity.identity_type, IdentityType::User);
        assert!(!identity.is_expired());
    }

    #[test]
    fn test_identity_builder_basic() {
        let identity = Identity::builder("bob")
            .email("bob@example.com")
            .build()
            .unwrap();

        assert_eq!(identity.name, "bob");
        assert_eq!(identity.email, Some("bob@example.com".to_string()));
        assert_eq!(identity.identity_type, IdentityType::User);
        assert_eq!(identity.usage, IdentityUsage::Personal);
    }

    #[test]
    fn test_identity_builder_agent() {
        let identity = Identity::builder("ci-bot")
            .identity_type(IdentityType::Agent)
            .description("CI bot for testing")
            .build()
            .unwrap();

        assert_eq!(identity.name, "ci-bot");
        assert!(identity.identity_type.is_agent());
        assert_eq!(
            identity.metadata.description,
            Some("CI bot for testing".to_string())
        );
    }

    #[test]
    fn test_identity_builder_delegated() {
        let user = Identity::generate("alice");
        let agent = Identity::builder("alice-assistant")
            .delegated_by(user.id)
            .build()
            .unwrap();

        assert!(agent.identity_type.is_delegated());
        assert_eq!(agent.delegated_by, Some(user.id));
    }

    #[test]
    fn test_identity_display_short() {
        let identity = Identity::builder("alice")
            .email("alice@example.com")
            .build()
            .unwrap();

        assert_eq!(identity.display_short(), "alice <alice@example.com>");

        let identity2 = Identity::builder("bob").build().unwrap();
        assert_eq!(identity2.display_short(), "bob");
    }

    #[test]
    fn test_identity_to_author() {
        let identity = Identity::builder("alice")
            .email("alice@example.com")
            .build()
            .unwrap();

        let author = identity.to_author();
        assert_eq!(author.name, "alice");
        assert_eq!(author.email, Some("alice@example.com".to_string()));
        assert!(author.identity.is_some());
    }

    #[test]
    fn test_identity_verify_signature() {
        let keypair = KeyPair::generate();
        let identity = Identity::new("alice", &keypair);
        let message = b"Hello, Atomic!";
        let signature = keypair.sign(message);

        assert!(identity.verify(message, &signature).is_ok());
    }

    #[test]
    fn test_identity_metadata_expiry() {
        use chrono::Duration;

        let mut metadata = IdentityMetadata::new();
        assert!(!metadata.is_expired());

        // Set expiry in the past
        metadata.expires_at = Some(Utc::now() - Duration::hours(1));
        assert!(metadata.is_expired());

        // Set expiry in the future
        metadata.expires_at = Some(Utc::now() + Duration::hours(1));
        assert!(!metadata.is_expired());
    }

    #[test]
    fn test_identity_equality() {
        let keypair = KeyPair::generate();
        let id1 = Identity::new("alice", &keypair);
        let id2 = Identity::new("bob", &keypair); // Same keypair, different name

        // Equality is based on the identity ID (derived from public key)
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_author_from_identity() {
        let identity = Identity::builder("alice")
            .email("alice@example.com")
            .build()
            .unwrap();

        let author: Author = (&identity).into();
        assert_eq!(author.name, "alice");
        assert_eq!(author.email, Some("alice@example.com".to_string()));
        assert!(author.identity.is_some());
    }

    #[test]
    fn test_identity_json_roundtrip() {
        let identity = Identity::builder("alice")
            .email("alice@example.com")
            .identity_type(IdentityType::User)
            .description("Test identity")
            .build()
            .unwrap();

        let json = serde_json::to_string(&identity).unwrap();
        let recovered: Identity = serde_json::from_str(&json).unwrap();

        assert_eq!(identity.id, recovered.id);
        assert_eq!(identity.name, recovered.name);
        assert_eq!(identity.email, recovered.email);
    }
}
