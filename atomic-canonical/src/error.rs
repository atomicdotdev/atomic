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

    #[error("identity error: {0}")]
    Identity(#[from] atomic_identity::IdentityError),

    /// A document arriving as bytes was refused before it became a value.
    ///
    /// Carries the admission fault itself rather than a rendered string, so a
    /// caller can branch on it: a repeated member means two readings of one
    /// document and a depth refusal is a bound on the work, and the two want
    /// different handling.
    #[error("document refused at the ingest boundary: {0}")]
    Admission(#[from] jcs_admit::Error),
}

pub type Result<T> = std::result::Result<T, CanonicalError>;
