//! Error type shared by every facade operation.

use atomic_repository::RepositoryError;

/// Errors returned by facade read operations.
///
/// Variants are deliberately coarse and HTTP-mappable: a server embedding the
/// facade can translate `NotFound`/`ChangeNotFound`/`ViewNotFound` to 404,
/// `InvalidIdentifier`/`Ambiguous` to 400, and everything else to 500.
#[derive(Debug, thiserror::Error)]
pub enum FacadeError {
    /// The underlying repository operation failed.
    #[error(transparent)]
    Repository(#[from] RepositoryError),

    /// The caller-supplied change/intent/memory identifier could not be parsed.
    #[error("invalid identifier: {message}")]
    InvalidIdentifier {
        /// Why the identifier was rejected.
        message: String,
    },

    /// A hash prefix matched more than one change.
    #[error("ambiguous hash prefix '{prefix}' ({} matches)", matches.len())]
    Ambiguous {
        /// The prefix the caller supplied.
        prefix: String,
        /// Every full hash the prefix matched, base32-encoded.
        matches: Vec<String>,
    },

    /// No change matched the identifier.
    #[error("change not found: {id}")]
    ChangeNotFound {
        /// The identifier as supplied by the caller.
        id: String,
    },

    /// The named view does not exist.
    #[error("view not found: {name}")]
    ViewNotFound {
        /// The view name as supplied by the caller.
        name: String,
    },

    /// The named vault entity (intent, memory) does not exist.
    #[error("{kind} not found: {id}")]
    NotFound {
        /// Entity kind ("intent", "memory").
        kind: &'static str,
        /// The identifier as supplied by the caller.
        id: String,
    },

    /// Stored data could not be parsed (corrupt frontmatter, bad JSON-LD).
    #[error("malformed stored data: {message}")]
    Malformed {
        /// What failed to parse and why.
        message: String,
    },
}

impl FacadeError {
    /// Whether this error maps to a caller mistake (vs. server-side failure).
    pub fn is_client_error(&self) -> bool {
        matches!(
            self,
            Self::InvalidIdentifier { .. }
                | Self::Ambiguous { .. }
                | Self::ChangeNotFound { .. }
                | Self::ViewNotFound { .. }
                | Self::NotFound { .. }
        )
    }
}

/// Result alias used across the facade.
pub type FacadeResult<T> = Result<T, FacadeError>;
