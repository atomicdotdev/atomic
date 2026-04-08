//! Error types for remote operations.
//!
//! This module defines the error types used throughout the `atomic-remote` crate.
//! Errors are designed to be informative and actionable, providing context about
//! what went wrong and how to potentially fix it.

use thiserror::Error;

/// Result type alias for remote operations.
pub type RemoteResult<T> = Result<T, RemoteError>;

/// Errors that can occur during remote operations.
///
/// These errors cover the full range of failure modes when communicating with
/// remote Atomic repositories, from network issues to protocol mismatches.
#[derive(Debug, Error)]
pub enum RemoteError {
    /// Failed to connect to the remote server.
    ///
    /// This can be caused by network issues, DNS resolution failures,
    /// or the server being unavailable.
    #[error("Failed to connect to remote: {url}")]
    ConnectionFailed {
        /// The URL that failed to connect.
        url: String,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Authentication failed (401 or 403 response).
    ///
    /// The provided credentials were invalid or insufficient.
    #[error("Authentication failed for {url}: {message}")]
    AuthenticationFailed {
        /// The URL where authentication failed.
        url: String,
        /// Additional details about the failure.
        message: String,
    },

    /// The repository was not found (404 response).
    #[error("Repository not found: {url}")]
    RepositoryNotFound {
        /// The URL of the repository that wasn't found.
        url: String,
    },

    /// The specified view does not exist on the remote.
    #[error("View not found: {view}")]
    ViewNotFound {
        /// The name of the view that wasn't found.
        view: String,
    },

    /// The specified change does not exist on the remote.
    #[error("Change not found: {hash}")]
    ChangeNotFound {
        /// The hash (base32) of the change that wasn't found.
        hash: String,
    },

    /// The specified tag/state does not exist on the remote.
    #[error("Tag not found for state: {state}")]
    TagNotFound {
        /// The state (base32) of the tag that wasn't found.
        state: String,
    },

    /// State mismatch during tag upload.
    ///
    /// The remote's current state doesn't match the state being tagged.
    #[error("State mismatch: remote is at {remote_state}, cannot tag {requested_state}")]
    StateMismatch {
        /// The current state on the remote.
        remote_state: String,
        /// The state that was requested to be tagged.
        requested_state: String,
    },

    /// Required dependencies are missing on the remote.
    ///
    /// Before a change can be applied, all its dependencies must exist.
    #[error("Missing {count} dependencies on remote: {}", format_hashes(.missing_hashes))]
    MissingDependencies {
        /// Number of missing dependencies.
        count: usize,
        /// Hashes (base32) of the missing dependencies.
        missing_hashes: Vec<String>,
    },

    /// The remote returned an invalid or unexpected response.
    #[error("Protocol error: {message}")]
    ProtocolError {
        /// Description of what was expected vs what was received.
        message: String,
    },

    /// HTTP error with status code.
    #[error("HTTP error {status}: {message}")]
    HttpError {
        /// The HTTP status code.
        status: u16,
        /// The error message from the server.
        message: String,
    },

    /// Request timed out.
    #[error("Request timed out after {seconds} seconds")]
    Timeout {
        /// How long we waited before timing out.
        seconds: u64,
    },

    /// I/O error (file system, network stream, etc.).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// URL parsing error.
    #[error("Invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),

    /// JSON parsing error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// The remote view is empty.
    #[error("View {view} is empty")]
    EmptyView {
        /// The name of the empty view.
        view: String,
    },

    /// The operation was cancelled.
    #[error("Operation cancelled")]
    Cancelled,

    /// Generic error for unexpected situations.
    #[error("{0}")]
    Other(String),
}

/// Format a list of hashes for display.
fn format_hashes(hashes: &[String]) -> String {
    if hashes.is_empty() {
        return String::from("(none)");
    }
    if hashes.len() <= 3 {
        hashes.join(", ")
    } else {
        format!(
            "{}, ... and {} more",
            hashes[..3].join(", "),
            hashes.len() - 3
        )
    }
}

impl RemoteError {
    /// Create a connection failed error.
    pub fn connection_failed(
        url: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::ConnectionFailed {
            url: url.into(),
            source: Box::new(source),
        }
    }

    /// Create an authentication failed error.
    pub fn auth_failed(url: impl Into<String>, message: impl Into<String>) -> Self {
        Self::AuthenticationFailed {
            url: url.into(),
            message: message.into(),
        }
    }

    /// Create a repository not found error.
    pub fn repo_not_found(url: impl Into<String>) -> Self {
        Self::RepositoryNotFound { url: url.into() }
    }

    /// Create a view not found error.
    pub fn view_not_found(view: impl Into<String>) -> Self {
        Self::ViewNotFound { view: view.into() }
    }

    /// Create a change not found error.
    pub fn change_not_found(hash: impl Into<String>) -> Self {
        Self::ChangeNotFound { hash: hash.into() }
    }

    /// Create a tag not found error.
    pub fn tag_not_found(state: impl Into<String>) -> Self {
        Self::TagNotFound {
            state: state.into(),
        }
    }

    /// Create a state mismatch error.
    pub fn state_mismatch(
        remote_state: impl Into<String>,
        requested_state: impl Into<String>,
    ) -> Self {
        Self::StateMismatch {
            remote_state: remote_state.into(),
            requested_state: requested_state.into(),
        }
    }

    /// Create a missing dependencies error.
    pub fn missing_deps(missing_hashes: Vec<String>) -> Self {
        Self::MissingDependencies {
            count: missing_hashes.len(),
            missing_hashes,
        }
    }

    /// Create a protocol error.
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::ProtocolError {
            message: message.into(),
        }
    }

    /// Create an HTTP error.
    pub fn http(status: u16, message: impl Into<String>) -> Self {
        Self::HttpError {
            status,
            message: message.into(),
        }
    }

    /// Create a timeout error.
    pub fn timeout(seconds: u64) -> Self {
        Self::Timeout { seconds }
    }

    /// Create an empty view error.
    pub fn empty_view(view: impl Into<String>) -> Self {
        Self::EmptyView { view: view.into() }
    }

    /// Create a generic error.
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    /// Check if this is a retryable error.
    ///
    /// Some errors (like timeouts or temporary network issues) may succeed
    /// if retried, while others (like authentication failures) will not.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::ConnectionFailed { .. } | Self::Timeout { .. })
    }

    /// Check if this is an authentication error.
    pub fn is_auth_error(&self) -> bool {
        matches!(self, Self::AuthenticationFailed { .. })
    }

    /// Check if this is a "not found" error.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::RepositoryNotFound { .. }
                | Self::ViewNotFound { .. }
                | Self::ChangeNotFound { .. }
                | Self::TagNotFound { .. }
        )
    }

    /// Get a user-friendly suggestion for how to resolve this error.
    pub fn suggestion(&self) -> Option<&'static str> {
        match self {
            Self::ConnectionFailed { .. } => {
                Some("Check your network connection and verify the remote URL is correct.")
            }
            Self::AuthenticationFailed { .. } => {
                Some("Check your credentials. You may need to run 'atomic identity prove'.")
            }
            Self::RepositoryNotFound { .. } => {
                Some("Verify the repository URL is correct and you have access.")
            }
            Self::ViewNotFound { .. } => {
                Some("Check the view name. Use 'atomic view list' to see available views.")
            }
            Self::MissingDependencies { .. } => {
                Some("Push the missing dependencies first, or use '--all' to push all changes.")
            }
            Self::Timeout { .. } => {
                Some("The server may be slow. Try again or increase the timeout.")
            }
            Self::EmptyView { .. } => Some("The view has no changes yet."),
            _ => None,
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_hashes_empty() {
        assert_eq!(format_hashes(&[]), "(none)");
    }

    #[test]
    fn test_format_hashes_few() {
        let hashes = vec!["ABC".to_string(), "DEF".to_string()];
        assert_eq!(format_hashes(&hashes), "ABC, DEF");
    }

    #[test]
    fn test_format_hashes_many() {
        let hashes = vec![
            "A".to_string(),
            "B".to_string(),
            "C".to_string(),
            "D".to_string(),
            "E".to_string(),
        ];
        assert_eq!(format_hashes(&hashes), "A, B, C, ... and 2 more");
    }

    #[test]
    fn test_connection_failed() {
        let err = RemoteError::connection_failed(
            "http://example.com",
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        );
        assert!(err.to_string().contains("http://example.com"));
        assert!(err.is_retryable());
    }

    #[test]
    fn test_auth_failed() {
        let err = RemoteError::auth_failed("http://example.com", "invalid token");
        assert!(err.is_auth_error());
        assert!(!err.is_retryable());
        assert!(err.suggestion().is_some());
    }

    #[test]
    fn test_repo_not_found() {
        let err = RemoteError::repo_not_found("http://example.com/repo");
        assert!(err.is_not_found());
        assert!(err.suggestion().is_some());
    }

    #[test]
    fn test_missing_deps() {
        let err = RemoteError::missing_deps(vec!["ABC123".to_string(), "DEF456".to_string()]);
        let msg = err.to_string();
        assert!(msg.contains("2 dependencies"));
        assert!(msg.contains("ABC123"));
    }

    #[test]
    fn test_protocol_error() {
        let err = RemoteError::protocol("unexpected response format");
        assert!(err.to_string().contains("unexpected response format"));
    }

    #[test]
    fn test_http_error() {
        let err = RemoteError::http(500, "Internal Server Error");
        assert!(err.to_string().contains("500"));
    }

    #[test]
    fn test_timeout() {
        let err = RemoteError::timeout(30);
        assert!(err.is_retryable());
        assert!(err.to_string().contains("30 seconds"));
    }

    #[test]
    fn test_is_not_found_variants() {
        assert!(RemoteError::repo_not_found("url").is_not_found());
        assert!(RemoteError::view_not_found("main").is_not_found());
        assert!(RemoteError::change_not_found("ABC").is_not_found());
        assert!(RemoteError::tag_not_found("XYZ").is_not_found());
        assert!(!RemoteError::timeout(10).is_not_found());
    }

    #[test]
    fn test_state_mismatch() {
        let err = RemoteError::state_mismatch("ABC123", "DEF456");
        let msg = err.to_string();
        assert!(msg.contains("ABC123"));
        assert!(msg.contains("DEF456"));
    }

    #[test]
    fn test_empty_view() {
        let err = RemoteError::empty_view("main");
        assert!(err.to_string().contains("main"));
        assert!(err.suggestion().is_some());
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let remote_err: RemoteError = io_err.into();
        assert!(matches!(remote_err, RemoteError::Io(_)));
    }

    #[test]
    fn test_url_error_conversion() {
        let url_result: Result<url::Url, _> = "not a valid url".parse();
        let url_err = url_result.unwrap_err();
        let remote_err: RemoteError = url_err.into();
        assert!(matches!(remote_err, RemoteError::InvalidUrl(_)));
    }
}
