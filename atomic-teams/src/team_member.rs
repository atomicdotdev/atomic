//! Team member management operations.
//!
//! Functions in this module manage the membership of individual identities
//! within teams. Each function takes a [`StorageClient`] reference and
//! delegates to the appropriate HTTP endpoint.

use uuid::Uuid;

use atomic_remote::storage::StorageClient;

use crate::error::{map_remote_error, TeamsResult};
use crate::types::{AddTeamMemberRequest, TeamMemberInfo, TeamRole, UpdateTeamMemberRoleRequest};

/// List all members of a team.
///
/// # Errors
///
/// Returns [`TeamsError::TeamNotFound`] if the team does not exist and
/// [`TeamsError::PermissionDenied`] if the caller lacks access.
pub async fn list_team_members(
    client: &StorageClient,
    org_slug: &str,
    team_slug: &str,
) -> TeamsResult<Vec<TeamMemberInfo>> {
    let path = format!("/orgs/{org_slug}/teams/{team_slug}/members");
    client
        .get(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("team {org_slug}/{team_slug}")))
}

/// Add an identity to a team.
///
/// # Errors
///
/// Returns [`TeamsError::TeamNotFound`] if the team does not exist,
/// [`TeamsError::AlreadyExists`] if the identity is already a member,
/// and [`TeamsError::PermissionDenied`] if the caller lacks access.
pub async fn add_team_member(
    client: &StorageClient,
    org_slug: &str,
    team_slug: &str,
    identity_id: Uuid,
    role: TeamRole,
) -> TeamsResult<TeamMemberInfo> {
    let path = format!("/orgs/{org_slug}/teams/{team_slug}/members");
    let body = AddTeamMemberRequest { identity_id, role };
    client
        .post(&path, &body)
        .await
        .map_err(|e| map_remote_error(e, format!("team {org_slug}/{team_slug}")))
}

/// Update the role of a team member.
///
/// # Errors
///
/// Returns [`TeamsError::MemberNotFound`] if the identity is not a member of
/// the team and [`TeamsError::PermissionDenied`] if the caller lacks access.
pub async fn update_team_member_role(
    client: &StorageClient,
    org_slug: &str,
    team_slug: &str,
    identity_id: Uuid,
    role: TeamRole,
) -> TeamsResult<TeamMemberInfo> {
    let path = format!("/orgs/{org_slug}/teams/{team_slug}/members/{identity_id}");
    let body = UpdateTeamMemberRoleRequest { role };
    client.put(&path, &body).await.map_err(|e| {
        map_remote_error(
            e,
            format!("team member {identity_id} in {org_slug}/{team_slug}"),
        )
    })
}

/// Remove an identity from a team.
///
/// # Errors
///
/// Returns [`TeamsError::MemberNotFound`] if the identity is not a member of
/// the team and [`TeamsError::PermissionDenied`] if the caller lacks access.
pub async fn remove_team_member(
    client: &StorageClient,
    org_slug: &str,
    team_slug: &str,
    identity_id: Uuid,
) -> TeamsResult<()> {
    let path = format!("/orgs/{org_slug}/teams/{team_slug}/members/{identity_id}");
    client.delete(&path).await.map_err(|e| {
        map_remote_error(
            e,
            format!("team member {identity_id} in {org_slug}/{team_slug}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_team_member_request_body() {
        let id = Uuid::nil();
        let body = AddTeamMemberRequest {
            identity_id: id,
            role: TeamRole::Contributor,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("identityId"));
        assert!(json.contains("contributor"));
    }

    #[test]
    fn update_team_member_role_request_body() {
        let body = UpdateTeamMemberRoleRequest {
            role: TeamRole::Maintainer,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("maintainer"));
    }

    #[test]
    fn path_construction_list() {
        let org = "acme";
        let team = "backend";
        let path = format!("/orgs/{org}/teams/{team}/members");
        assert_eq!(path, "/orgs/acme/teams/backend/members");
    }

    #[test]
    fn path_construction_member() {
        let org = "acme";
        let team = "backend";
        let id = Uuid::nil();
        let path = format!("/orgs/{org}/teams/{team}/members/{id}");
        assert_eq!(
            path,
            "/orgs/acme/teams/backend/members/00000000-0000-0000-0000-000000000000"
        );
    }
}
