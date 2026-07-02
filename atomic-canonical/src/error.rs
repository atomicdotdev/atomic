//! Error type for the canonical layer.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CanonicalError {
    #[error("lift error: {0}")]
    Lift(String),

    #[error("directive parse error: {0}")]
    Directive(String),

    #[error("proof error: {0}")]
    Proof(String),

    #[error("content hash mismatch: node says {expected}, recomputed {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("signature verification failed: {0}")]
    Verification(String),

    #[error("shacl validation error: {0}")]
    Shacl(String),

    #[error("identity error: {0}")]
    Identity(#[from] atomic_identity::IdentityError),
}

pub type Result<T> = std::result::Result<T, CanonicalError>;
