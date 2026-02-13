//! User identity and cryptographic signing for Atomic VCS.
//!
//! This crate provides comprehensive identity management for Atomic,
//! including:
//!
//! - **Multi-identity support**: Personal, work, community, and custom identities
//! - **Agent identities**: AI/automated systems with standalone or delegated access
//! - **Ed25519 cryptography**: Key generation, signing, and verification
//! - **Identity storage**: Persistent storage with optional secret key encryption
//! - **Delegation**: Authorize agents to act on behalf of users
//!
//! # Overview
//!
//! Atomic supports multiple types of identities:
//!
//! | Type | Description | Use Case |
//! |------|-------------|----------|
//! | User | Human user identity | Personal/work changes |
//! | Agent | AI or automated system | CI/CD, bots |
//! | Delegated | Agent acting on behalf of user | AI assistants |
//!
//! # Identity Usage Contexts
//!
//! Users can maintain separate identities for different contexts:
//!
//! - **Personal**: Side projects, open source contributions
//! - **Work**: Professional/employer-related work
//! - **Community**: Organization or maintainer work
//! - **Bot**: Automated systems and service accounts
//! - **Custom**: User-defined contexts
//!
//! # Example
//!
//! ```rust
//! use atomic_identity::{Identity, IdentityType, IdentityUsage, KeyPair};
//!
//! // Generate a new identity
//! let identity = Identity::builder("alice")
//!     .email("alice@example.com")
//!     .usage(IdentityUsage::Personal)
//!     .build()?;
//!
//! println!("Identity: {}", identity.display_short());
//! println!("Public key: {}", identity.public_key_base32());
//!
//! // Create an agent identity
//! let agent = Identity::builder("ci-bot")
//!     .identity_type(IdentityType::Agent)
//!     .usage(IdentityUsage::Bot)
//!     .description("Continuous integration bot")
//!     .build()?;
//!
//! // Create a delegated identity (agent acting on behalf of user)
//! let delegated = Identity::builder("alice-assistant")
//!     .delegated_by(identity.id)
//!     .build()?;
//!
//! assert!(delegated.identity_type.is_delegated());
//! # Ok::<(), atomic_identity::IdentityError>(())
//! ```
//!
//! # Signing Changes
//!
//! ```rust
//! use atomic_identity::{Identity, KeyPair};
//! use atomic_identity::signing::{Signer, SignedData};
//!
//! // Create a keypair and identity
//! let keypair = KeyPair::generate();
//! let identity = Identity::new("alice", &keypair);
//!
//! // Sign some data
//! let data = b"Change content to sign";
//! let signer = Signer::new(&keypair);
//! let signature = signer.sign(data);
//!
//! // Verify the signature
//! assert!(signature.verify(data, &identity.public_key).is_ok());
//! ```
//!
//! # Identity Storage
//!
//! ```rust,ignore
//! use atomic_identity::{Identity, IdentityStore, IdentityUsage};
//!
//! // Open the default identity store
//! let mut store = IdentityStore::open_default()?;
//!
//! // Create and save an identity
//! let identity = Identity::builder("alice")
//!     .email("alice@example.com")
//!     .usage(IdentityUsage::Personal)
//!     .build()?;
//!
//! store.save(&identity)?;
//! store.set_default(&identity.id)?;
//!
//! // Later, load the default identity
//! let default = store.get_default()?.expect("No default identity");
//! ```
//!
//! # Delegation
//!
//! ```rust
//! use atomic_identity::{Identity, IdentityType};
//! use atomic_identity::delegation::{Delegation, DelegationScope, DelegationPermission};
//!
//! // Create user and agent identities
//! let user = Identity::generate("alice");
//! let agent = Identity::builder("alice-assistant")
//!     .identity_type(IdentityType::Agent)
//!     .delegated_by(user.id)
//!     .build()?;
//!
//! // Create a delegation with specific permissions
//! let scope = DelegationScope::builder()
//!     .permission(DelegationPermission::Read)
//!     .permission(DelegationPermission::Record)
//!     .repository_pattern("alice/*")
//!     .build();
//!
//! let delegation = Delegation::new(&user, &agent, scope)?;
//!
//! // Check if an operation is allowed
//! assert!(delegation.allows(
//!     DelegationPermission::Read,
//!     Some("alice/my-project"),
//!     None
//! ));
//! # Ok::<(), atomic_identity::IdentityError>(())
//! ```
//!
//! # Integration with atomic-core
//!
//! Identities integrate with atomic-core's `Author` type for change headers:
//!
//! ```rust
//! use atomic_identity::{Identity, Author};
//!
//! let identity = Identity::generate("alice");
//!
//! // Convert identity to Author for change headers
//! let author = identity.to_author();
//! // Or use the From trait
//! let author: Author = (&identity).into();
//!
//! assert_eq!(author.name, "alice");
//! assert!(author.identity.is_some()); // Contains public key reference
//! ```

pub mod delegation;
pub mod error;
pub mod identity;
pub mod keypair;
pub mod signing;
pub mod store;
pub mod usage;

// Re-export main types
pub use error::IdentityError;
pub use identity::{Author, Identity, IdentityBuilder, IdentityId, IdentityMetadata, IdentityType};
pub use keypair::{KeyPair, PublicKey, SecretKey};
pub use store::{IdentityFilter, IdentityStore, LoadOptions, StoreConfig};
pub use usage::IdentityUsage;

// Re-export delegation types
pub use delegation::{
    Delegation, DelegationId, DelegationPermission, DelegationScope, DelegationScopeBuilder,
};

// Re-export signing types
pub use signing::{
    Signature, SignatureInfo, SignatureSet, SignedData, Signer, VerificationResult, SIGNATURE_SIZE,
};

/// Result type for identity operations
pub type Result<T> = std::result::Result<T, IdentityError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_generation() {
        let identity = Identity::generate("test-user");
        assert_eq!(identity.name, "test-user");
        assert_eq!(identity.identity_type, IdentityType::User);
    }

    #[test]
    fn test_identity_builder() {
        let identity = Identity::builder("alice")
            .email("alice@example.com")
            .usage(IdentityUsage::Personal)
            .build()
            .unwrap();

        assert_eq!(identity.name, "alice");
        assert_eq!(identity.email, Some("alice@example.com".to_string()));
        assert!(identity.usage.is_personal());
    }

    #[test]
    fn test_agent_identity() {
        let agent = Identity::builder("ci-bot")
            .identity_type(IdentityType::Agent)
            .usage(IdentityUsage::Bot)
            .build()
            .unwrap();

        assert!(agent.identity_type.is_agent());
        assert!(agent.usage.is_bot());
    }

    #[test]
    fn test_delegated_identity() {
        let user = Identity::generate("alice");
        let delegated = Identity::builder("alice-assistant")
            .delegated_by(user.id)
            .build()
            .unwrap();

        assert!(delegated.identity_type.is_delegated());
        assert_eq!(delegated.delegated_by, Some(user.id));
    }

    #[test]
    fn test_signing_workflow() {
        let keypair = KeyPair::generate();
        let identity = Identity::new("signer", &keypair);

        let message = b"Important message";
        let signer = Signer::new(&keypair);
        let signature = signer.sign(message);

        assert!(signature.verify(message, &identity.public_key).is_ok());
        assert!(signature.verify(b"wrong message", &identity.public_key).is_err());
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
    fn test_delegation() {
        let user = Identity::generate("alice");
        let agent = Identity::builder("bot")
            .identity_type(IdentityType::Agent)
            .delegated_by(user.id)
            .build()
            .unwrap();

        let scope = DelegationScope::builder()
            .permission(DelegationPermission::Read)
            .permission(DelegationPermission::Record)
            .build();

        let delegation = Delegation::new(&user, &agent, scope).unwrap();

        assert!(delegation.allows(DelegationPermission::Read, None, None));
        assert!(delegation.allows(DelegationPermission::Record, None, None));
        assert!(!delegation.allows(DelegationPermission::Push, None, None));
    }
}
