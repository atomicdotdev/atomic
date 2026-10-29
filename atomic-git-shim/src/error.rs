//! Errors returned by Git interoperability operations.

use std::path::PathBuf;

pub type ShimResult<T> = Result<T, ShimError>;

#[derive(Debug, thiserror::Error)]
pub enum ShimError {
    #[error("Git error: {message}")]
    GitError { message: String },
    #[error("{report}")]
    StaleBaseline { report: String },
    #[error("Not in an Atomic repository: '{searched_path}' (no repository found in this directory or its parents). Run 'atomic init' here, or cd into an Atomic repository.")]
    RepositoryNotFound { searched_path: PathBuf },
    #[error("Invalid repository structure: {reason}")]
    InvalidRepository { reason: String },
    #[error("View '{name}' not found")]
    ViewNotFound { name: String },
    #[error("{0}")]
    Repository(#[from] atomic_repository::RepositoryError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Configuration error: {0}")]
    Config(#[from] atomic_config::ConfigError),
    #[error("Internal error: {0}")]
    Internal(#[from] anyhow::Error),
}
