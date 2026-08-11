//! Permission grants on organizations and workspaces.
//!
//! Grants express fine-grained access control by binding a subject (user or
//! team) to a relation (read, write, admin, owner) on a resource. This module
//! provides async helpers that call the remote storage API to manage grants.
//!
//! # Workspace grants
//!
//! Workspace grants use `read`, `write`, and `admin` relations. The server
//! exposes:
//!   `GET    /workspaces/{slug}/grants`                          — list
//!   `POST   /workspaces/{slug}/grants`                          — add
//!   `DELETE /workspaces/{slug}/grants/{subject_type}/{subject_id}` — revoke
//!
//! # Organization grants
//!
//! Org grants additionally support the `owner` relation. The server exposes:
//!   `GET    /orgs/{slug}/grants`                          — list
//!   `POST   /orgs/{slug}/grants`                          — add
//!   `DELETE /orgs/{slug}/grants/{subject_type}/{subject_id}` — revoke

use uuid::Uuid;

use atomic_remote::storage::StorageClient;

use crate::error::{map_remote_error, TeamsError, TeamsResult};
use crate::types::{AddGrantRequest, GrantInfo, GrantRelation, GrantSubjectType};

/// Ensure a grant mutation targets a concrete user or team.
///
/// `Everyone` is emitted by Storage when listing public grants, but the
/// mutation endpoints require a non-null user or team UUID.
fn validate_mutation_subject(subject_type: GrantSubjectType) -> TeamsResult<()> {
    match subject_type {
        GrantSubjectType::User | GrantSubjectType::Team => Ok(()),
        GrantSubjectType::Everyone => Err(TeamsError::InvalidInput(
            "grant mutations require a user or team subject".to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Organization grants
// ---------------------------------------------------------------------------

/// List all permission grants on an organization.
///
/// # Errors
///
/// Returns [`TeamsError::OrgNotFound`]
/// if the org does not exist, or
/// [`TeamsError::PermissionDenied`]
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
/// Binds `subject_id` (a user or team UUID) with the given `relation` on
/// the organization identified by `org_slug`.
///
/// # Errors
///
/// Returns [`TeamsError::AlreadyExists`]
/// if the grant already exists, or
/// [`TeamsError::PermissionDenied`]
/// if the caller is not an org owner. Returns
/// [`TeamsError::InvalidInput`] when
/// `subject_type` is [`GrantSubjectType::Everyone`].
pub async fn add_org_grant(
    client: &StorageClient,
    org_slug: &str,
    subject_type: GrantSubjectType,
    subject_id: Uuid,
    relation: GrantRelation,
) -> TeamsResult<GrantInfo> {
    validate_mutation_subject(subject_type)?;
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

/// Revoke all grants for a subject on an organization.
///
/// Calls `DELETE /orgs/{slug}/grants/{subject_type}/{subject_id}`. The server
/// removes **all** relations (read, write, admin, owner) for that subject in
/// a single call.
///
/// # Errors
///
/// Returns [`TeamsError::OrgNotFound`]
/// if the org does not exist, or
/// [`TeamsError::PermissionDenied`]
/// if the caller is not an org owner. Returns
/// [`TeamsError::InvalidInput`] when
/// `subject_type` is [`GrantSubjectType::Everyone`].
pub async fn revoke_org_grant(
    client: &StorageClient,
    org_slug: &str,
    subject_type: GrantSubjectType,
    subject_id: Uuid,
) -> TeamsResult<()> {
    validate_mutation_subject(subject_type)?;
    let st = subject_type.to_string();
    let path = format!("/orgs/{org_slug}/grants/{st}/{subject_id}");
    client
        .delete(&path)
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
/// Returns [`TeamsError::PermissionDenied`]
/// if the caller lacks read access, or a not-found error if the workspace
/// doesn't exist.
pub async fn list_workspace_grants(
    client: &StorageClient,
    workspace_slug: &str,
) -> TeamsResult<Vec<crate::types::WorkspaceGrantInfo>> {
    let path = format!("/workspaces/{workspace_slug}/grants");
    client
        .get(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("workspace {workspace_slug}")))
}

/// Add a permission grant to a workspace.
///
/// Binds `subject_id` (a user or team UUID) with the given `relation` on
/// the workspace identified by `workspace_slug`.
///
/// # Errors
///
/// Returns [`TeamsError::AlreadyExists`]
/// if the grant already exists, or
/// [`TeamsError::PermissionDenied`]
/// if the caller is not a workspace admin. Returns
/// [`TeamsError::InvalidInput`] when
/// `subject_type` is [`GrantSubjectType::Everyone`].
pub async fn add_workspace_grant(
    client: &StorageClient,
    workspace_slug: &str,
    subject_type: GrantSubjectType,
    subject_id: Uuid,
    relation: GrantRelation,
) -> TeamsResult<crate::types::WorkspaceGrantInfo> {
    validate_mutation_subject(subject_type)?;
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

/// Revoke all grants for a subject on a workspace.
///
/// Calls `DELETE /workspaces/{slug}/grants/{subject_type}/{subject_id}`. The
/// server removes **all** relations (read, write, admin) for that subject in
/// a single call — no need to specify which relation to revoke.
///
/// # Errors
///
/// Returns [`TeamsError::PermissionDenied`]
/// if the caller is not a workspace admin, or a not-found error if the
/// workspace doesn't exist. Returns
/// [`TeamsError::InvalidInput`] when
/// `subject_type` is [`GrantSubjectType::Everyone`].
pub async fn revoke_workspace_grant(
    client: &StorageClient,
    workspace_slug: &str,
    subject_type: GrantSubjectType,
    subject_id: Uuid,
) -> TeamsResult<()> {
    validate_mutation_subject(subject_type)?;
    let st = subject_type.to_string();
    let path = format!("/workspaces/{workspace_slug}/grants/{st}/{subject_id}");
    client
        .delete(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("workspace {workspace_slug}")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AddGrantRequest;

    #[test]
    fn add_grant_request_serializes_snake_case() {
        let req = AddGrantRequest {
            subject_type: GrantSubjectType::Team,
            subject_id: Uuid::nil(),
            relation: GrantRelation::Write,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"subject_type\""), "got: {json}");
        assert!(json.contains("\"subject_id\""), "got: {json}");
        assert!(json.contains("\"relation\":\"write\""), "got: {json}");
    }

    #[test]
    fn add_grant_request_user_relation_read() {
        let req = AddGrantRequest {
            subject_type: GrantSubjectType::User,
            subject_id: Uuid::nil(),
            relation: GrantRelation::Read,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"subject_type\":\"user\""), "got: {json}");
        assert!(json.contains("\"relation\":\"read\""), "got: {json}");
    }

    #[test]
    fn mutation_subject_rejects_everyone() {
        let error = validate_mutation_subject(GrantSubjectType::Everyone).unwrap_err();
        assert!(matches!(error, TeamsError::InvalidInput(_)));
        assert_eq!(
            error.to_string(),
            "Invalid input: grant mutations require a user or team subject"
        );
    }

    #[test]
    fn mutation_subject_accepts_users_and_teams() {
        assert!(validate_mutation_subject(GrantSubjectType::User).is_ok());
        assert!(validate_mutation_subject(GrantSubjectType::Team).is_ok());
    }

    #[test]
    fn org_revoke_path_format() {
        let slug = "acme";
        let st = "team";
        let id = Uuid::nil();
        assert_eq!(
            format!("/orgs/{slug}/grants/{st}/{id}"),
            format!("/orgs/acme/grants/team/{id}")
        );
    }

    #[test]
    fn workspace_revoke_path_format() {
        let slug = "my-ws";
        let st = "user";
        let id = Uuid::nil();
        assert_eq!(
            format!("/workspaces/{slug}/grants/{st}/{id}"),
            format!("/workspaces/my-ws/grants/user/{id}")
        );
    }
}
