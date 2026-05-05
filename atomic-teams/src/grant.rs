//! Permission grants on organizations and workspaces.
//!
//! Grants express fine-grained access control by binding a subject (user or
//! team) to a relation (read, write, admin, owner) on a resource. This module
//! provides async helpers that call the remote storage API to manage grants.

use uuid::Uuid;

use atomic_remote::storage::StorageClient;

use crate::error::{map_remote_error, TeamsResult};
use crate::types::{
    AddGrantRequest, GrantInfo, GrantRelation, GrantSubjectType, RevokeGrantRequest,
};

// ---------------------------------------------------------------------------
// Organization grants
// ---------------------------------------------------------------------------

/// List all permission grants on an organization.
///
/// # Errors
///
/// Returns [`TeamsError::OrgNotFound`](crate::error::TeamsError::OrgNotFound)
/// if the org does not exist, or
/// [`TeamsError::PermissionDenied`](crate::error::TeamsError::PermissionDenied)
/// if the caller lacks access.
pub async fn list_org_grants(
    client: &StorageClient,
    org_slug: &str,
) -> TeamsResult<Vec<GrantInfo>> {
    let path = format!("/orgs/{org_slug}/grants");
    client
        .get(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

/// Add a permission grant to an organization.
///
/// Binds `subject_id` (a user or team identity) with the given `relation` on
/// the organization identified by `org_slug`.
///
/// # Errors
///
/// Returns [`TeamsError::AlreadyExists`](crate::error::TeamsError::AlreadyExists)
/// if the grant already exists, or
/// [`TeamsError::PermissionDenied`](crate::error::TeamsError::PermissionDenied)
/// if the caller is not an org admin/owner.
pub async fn add_org_grant(
    client: &StorageClient,
    org_slug: &str,
    subject_type: GrantSubjectType,
    subject_id: Option<Uuid>,
    relation: GrantRelation,
) -> TeamsResult<GrantInfo> {
    let path = format!("/orgs/{org_slug}/grants");
    let body = AddGrantRequest {
        subject_type,
        subject_id,
        relation,
    };
    client
        .post(&path, &body)
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

/// Revoke a permission grant from an organization.
///
/// Removes the grant that matches the given `subject_type` and `subject_id`.
///
/// # Errors
///
/// Returns [`TeamsError::OrgNotFound`](crate::error::TeamsError::OrgNotFound)
/// if the org does not exist, or
/// [`TeamsError::PermissionDenied`](crate::error::TeamsError::PermissionDenied)
/// if the caller is not an org admin/owner.
pub async fn revoke_org_grant(
    client: &StorageClient,
    org_slug: &str,
    subject_type: GrantSubjectType,
    subject_id: Option<Uuid>,
) -> TeamsResult<()> {
    let path = format!("/orgs/{org_slug}/grants/revoke");
    let body = RevokeGrantRequest {
        subject_type,
        subject_id,
    };
    // Use post for the revoke action (it carries a body).
    client
        .post::<_, ()>(&path, &body)
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

// ---------------------------------------------------------------------------
// Workspace grants
// ---------------------------------------------------------------------------

/// List all permission grants on a workspace.
///
/// # Errors
///
/// Returns [`TeamsError::OrgNotFound`](crate::error::TeamsError::OrgNotFound)
/// if the workspace does not exist, or
/// [`TeamsError::PermissionDenied`](crate::error::TeamsError::PermissionDenied)
/// if the caller lacks access.
pub async fn list_workspace_grants(
    client: &StorageClient,
    workspace_slug: &str,
) -> TeamsResult<Vec<GrantInfo>> {
    let path = format!("/workspaces/{workspace_slug}/grants");
    client
        .get(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("workspace {workspace_slug}")))
}

/// Add a permission grant to a workspace.
///
/// Binds `subject_id` (a user or team identity) with the given `relation` on
/// the workspace identified by `workspace_slug`.
///
/// # Errors
///
/// Returns [`TeamsError::AlreadyExists`](crate::error::TeamsError::AlreadyExists)
/// if the grant already exists, or
/// [`TeamsError::PermissionDenied`](crate::error::TeamsError::PermissionDenied)
/// if the caller is not an admin/owner of the workspace's parent organization.
pub async fn add_workspace_grant(
    client: &StorageClient,
    workspace_slug: &str,
    subject_type: GrantSubjectType,
    subject_id: Option<Uuid>,
    relation: GrantRelation,
) -> TeamsResult<GrantInfo> {
    let path = format!("/workspaces/{workspace_slug}/grants");
    let body = AddGrantRequest {
        subject_type,
        subject_id,
        relation,
    };
    client
        .post(&path, &body)
        .await
        .map_err(|e| map_remote_error(e, format!("workspace {workspace_slug}")))
}

/// Revoke a permission grant from a workspace.
///
/// Removes the grant that matches the given `subject_type` and `subject_id`.
///
/// # Errors
///
/// Returns [`TeamsError::OrgNotFound`](crate::error::TeamsError::OrgNotFound)
/// if the workspace does not exist, or
/// [`TeamsError::PermissionDenied`](crate::error::TeamsError::PermissionDenied)
/// if the caller is not an admin/owner of the workspace's parent organization.
pub async fn revoke_workspace_grant(
    client: &StorageClient,
    workspace_slug: &str,
    subject_type: GrantSubjectType,
    subject_id: Option<Uuid>,
) -> TeamsResult<()> {
    let path = format!("/workspaces/{workspace_slug}/grants/revoke");
    let body = RevokeGrantRequest {
        subject_type,
        subject_id,
    };
    client
        .post::<_, ()>(&path, &body)
        .await
        .map_err(|e| map_remote_error(e, format!("workspace {workspace_slug}")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AddGrantRequest, RevokeGrantRequest};

    #[test]
    fn add_grant_request_serializes_with_subject() {
        let req = AddGrantRequest {
            subject_type: GrantSubjectType::User,
            subject_id: Some(Uuid::nil()),
            relation: GrantRelation::Write,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"subjectType\":\"user\""));
        assert!(json.contains("\"relation\":\"write\""));
        assert!(json.contains("\"subjectId\""));
    }

    #[test]
    fn add_grant_request_skips_none_subject_id() {
        let req = AddGrantRequest {
            subject_type: GrantSubjectType::Team,
            subject_id: None,
            relation: GrantRelation::Read,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("subjectId"));
    }

    #[test]
    fn revoke_grant_request_serializes() {
        let req = RevokeGrantRequest {
            subject_type: GrantSubjectType::User,
            subject_id: Some(Uuid::nil()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"subjectType\":\"user\""));
        assert!(json.contains("\"subjectId\""));
    }

    #[test]
    fn revoke_grant_request_skips_none_subject_id() {
        let req = RevokeGrantRequest {
            subject_type: GrantSubjectType::Team,
            subject_id: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("subjectId"));
    }

    #[test]
    fn org_grant_path_format() {
        let slug = "acme";
        assert_eq!(format!("/orgs/{slug}/grants"), "/orgs/acme/grants");
        assert_eq!(
            format!("/orgs/{slug}/grants/revoke"),
            "/orgs/acme/grants/revoke"
        );
    }

    #[test]
    fn workspace_grant_path_format() {
        let slug = "my-ws";
        assert_eq!(
            format!("/workspaces/{slug}/grants"),
            "/workspaces/my-ws/grants"
        );
        assert_eq!(
            format!("/workspaces/{slug}/grants/revoke"),
            "/workspaces/my-ws/grants/revoke"
        );
    }
}
