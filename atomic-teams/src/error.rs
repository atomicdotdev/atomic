//! Error types for team collaboration operations.

use thiserror::Error;

/// Errors that can occur during team operations.
#[derive(Debug, Error)]
pub enum TeamsError {
    /// An error from the remote transport layer.
    #[error("Remote error: {0}")]
    Remote(#[from] atomic_remote::RemoteError),

    /// The requested organization was not found.
    #[error("Organization not found: {0}")]
    OrgNotFound(String),

    /// The requested team was not found.
    #[error("Team not found: {0}")]
    TeamNotFound(String),

    /// The requested member was not found.
    #[error("Member not found: {0}")]
    MemberNotFound(String),

    /// The caller lacks permission for the requested operation.
    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    /// The resource already exists.
    #[error("Already exists: {0}")]
    AlreadyExists(String),

    /// The caller provided invalid input.
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// Cannot remove the last owner of an organization.
    #[error("Cannot remove last owner")]
    LastOwner,

    /// Catch-all for unexpected situations.
    #[error("{0}")]
    Other(String),
}

/// Convenience result alias used throughout the crate.
pub type TeamsResult<T> = Result<T, TeamsError>;

/// Inspect a [`atomic_remote::RemoteError`] and convert HTTP 404 responses into
/// a context-specific [`TeamsError`] using the provided constructor, and HTTP
/// 403 responses into [`TeamsError::PermissionDenied`]. All other remote errors
/// are wrapped as [`TeamsError::Remote`].
pub(crate) fn map_remote_error(
    err: atomic_remote::RemoteError,
    not_found_msg: impl Into<String>,
) -> TeamsError {
    match &err {
        atomic_remote::RemoteError::HttpError { status, .. } if *status == 404 => {
            let msg = not_found_msg.into();
            // Heuristic: pick the most specific "not found" variant based on
            // whether the message mentions team or member.
            let lower = msg.to_lowercase();
            if lower.contains("team") {
                TeamsError::TeamNotFound(msg)
            } else if lower.contains("member") || lower.contains("identity") {
                TeamsError::MemberNotFound(msg)
            } else {
                TeamsError::OrgNotFound(msg)
            }
        }
        atomic_remote::RemoteError::HttpError { status, message } if *status == 403 => {
            TeamsError::PermissionDenied(message.clone())
        }
        atomic_remote::RemoteError::HttpError { status, message } if *status == 409 => {
            TeamsError::AlreadyExists(message.clone())
        }
        _ => TeamsError::Remote(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teams_error_display() {
        let err = TeamsError::OrgNotFound("acme".into());
        assert_eq!(err.to_string(), "Organization not found: acme");

        let err = TeamsError::TeamNotFound("backend".into());
        assert_eq!(err.to_string(), "Team not found: backend");

        let err = TeamsError::MemberNotFound("user-42".into());
        assert_eq!(err.to_string(), "Member not found: user-42");

        let err = TeamsError::PermissionDenied("not an admin".into());
        assert_eq!(err.to_string(), "Permission denied: not an admin");

        let err = TeamsError::AlreadyExists("slug taken".into());
        assert_eq!(err.to_string(), "Already exists: slug taken");

        let err = TeamsError::InvalidInput("name too long".into());
        assert_eq!(err.to_string(), "Invalid input: name too long");

        let err = TeamsError::LastOwner;
        assert_eq!(err.to_string(), "Cannot remove last owner");

        let err = TeamsError::Other("boom".into());
        assert_eq!(err.to_string(), "boom");
    }

    #[test]
    fn map_remote_404_to_org_not_found() {
        let remote = atomic_remote::RemoteError::http(404, "Not Found");
        let err = map_remote_error(remote, "org acme");
        assert!(matches!(err, TeamsError::OrgNotFound(ref s) if s == "org acme"));
    }

    #[test]
    fn map_remote_404_to_team_not_found() {
        let remote = atomic_remote::RemoteError::http(404, "Not Found");
        let err = map_remote_error(remote, "team backend");
        assert!(matches!(err, TeamsError::TeamNotFound(ref s) if s == "team backend"));
    }

    #[test]
    fn map_remote_404_to_member_not_found() {
        let remote = atomic_remote::RemoteError::http(404, "Not Found");
        let err = map_remote_error(remote, "member xyz");
        assert!(matches!(err, TeamsError::MemberNotFound(ref s) if s == "member xyz"));
    }

    #[test]
    fn map_remote_403_to_permission_denied() {
        let remote = atomic_remote::RemoteError::http(403, "Forbidden");
        let err = map_remote_error(remote, "ignored");
        assert!(matches!(err, TeamsError::PermissionDenied(ref s) if s == "Forbidden"));
    }

    #[test]
    fn map_remote_409_to_already_exists() {
        let remote = atomic_remote::RemoteError::http(409, "Conflict");
        let err = map_remote_error(remote, "ignored");
        assert!(matches!(err, TeamsError::AlreadyExists(ref s) if s == "Conflict"));
    }

    #[test]
    fn map_remote_500_to_remote() {
        let remote = atomic_remote::RemoteError::http(500, "Internal");
        let err = map_remote_error(remote, "ignored");
        assert!(matches!(err, TeamsError::Remote(_)));
    }

    #[test]
    fn from_remote_error() {
        let remote = atomic_remote::RemoteError::other("network down");
        let err: TeamsError = remote.into();
        assert!(matches!(err, TeamsError::Remote(_)));
        assert!(err.to_string().contains("network down"));
    }
}
