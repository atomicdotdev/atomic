//! Error types for identity operations

use thiserror::Error;

/// Errors that can occur during identity operations
#[derive(Debug, Error)]
pub enum IdentityError {
    /// Failed to generate a key pair
    #[error("Failed to generate key pair: {0}")]
    KeyGeneration(String),

    /// Invalid key format or encoding
    #[error("Invalid key format: {0}")]
    InvalidKey(String),

    /// Key not found
    #[error("Identity not found: {name}")]
    NotFound { name: String },

    /// Identity already exists
    #[error("Identity already exists: {name}")]
    AlreadyExists { name: String },

    /// Signature verification failed
    #[error("Signature verification failed")]
    InvalidSignature,

    /// Failed to sign data
    #[error("Failed to sign data: {0}")]
    SigningError(String),

    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(#[from] atomic_config::ConfigError),

    /// Serialization error
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Signature verification failed with reason
    #[error("Verification failed: {0}")]
    VerificationFailed(String),

    /// Base32 encoding/decoding error
    #[error("Invalid base32 encoding")]
    InvalidBase32,

    /// Password required but not provided
    #[error("Password required for encrypted key")]
    PasswordRequired,

    /// Invalid password
    #[error("Invalid password")]
    InvalidPassword,
}

impl From<serde_json::Error> for IdentityError {
    fn from(e: serde_json::Error) -> Self {
        IdentityError::Serialization(e.to_string())
    }
}

impl From<toml::de::Error> for IdentityError {
    fn from(e: toml::de::Error) -> Self {
        IdentityError::Serialization(e.to_string())
    }
}

impl From<toml::ser::Error> for IdentityError {
    fn from(e: toml::ser::Error) -> Self {
        IdentityError::Serialization(e.to_string())
    }
}
