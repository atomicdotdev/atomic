//! Team management operations.
//!
//! Provides async functions for creating, reading, updating, and deleting
//! teams within an organization via the remote storage API.

use atomic_remote::storage::StorageClient;

use crate::error::{map_remote_error, TeamsResult};
use crate::types::{CreateTeamRequest, TeamInfo, TeamVisibility, UpdateTeamRequest};

/// List all teams in an organization.
///
/// Returns every team the caller has visibility into. Secret teams are only
/// returned when the caller is a member or an org admin.
pub async fn list_teams(client: &StorageClient, org_slug: &str) -> TeamsResult<Vec<TeamInfo>> {
    let path = format!("/orgs/{org_slug}/teams");
    client
        .get(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

/// Create a new team in an organization.
///
/// The team slug is derived server-side from the `name`. If `visibility` is
/// `None` the server default (typically [`TeamVisibility::Visible`]) is used.
pub async fn create_team(
    client: &StorageClient,
    org_slug: &str,
    name: &str,
    description: Option<&str>,
    visibility: Option<TeamVisibility>,
) -> TeamsResult<TeamInfo> {
    let path = format!("/orgs/{org_slug}/teams");
    let body = CreateTeamRequest {
        name,
        description,
        visibility,
    };
    client
        .post(&path, &body)
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

/// Get a single team by its slug.
pub async fn get_team(
    client: &StorageClient,
    org_slug: &str,
    team_slug: &str,
) -> TeamsResult<TeamInfo> {
    let path = format!("/orgs/{org_slug}/teams/{team_slug}");
    client
        .get(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("team {team_slug}")))
}

/// Update a team's metadata.
///
/// Only the fields that are `Some` are sent to the server — omitted fields
/// remain unchanged.
pub async fn update_team(
    client: &StorageClient,
    org_slug: &str,
    team_slug: &str,
    name: Option<&str>,
    description: Option<&str>,
    visibility: Option<TeamVisibility>,
) -> TeamsResult<TeamInfo> {
    let path = format!("/orgs/{org_slug}/teams/{team_slug}");
    let body = UpdateTeamRequest {
        name,
        description,
        visibility,
    };
    client
        .put(&path, &body)
        .await
        .map_err(|e| map_remote_error(e, format!("team {team_slug}")))
}

/// Delete a team.
///
/// All team memberships and team-scoped grants are removed along with the
/// team itself.
pub async fn delete_team(
    client: &StorageClient,
    org_slug: &str,
    team_slug: &str,
) -> TeamsResult<()> {
    let path = format!("/orgs/{org_slug}/teams/{team_slug}");
    client
        .delete(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("team {team_slug}")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_team_request_body() {
        let body = CreateTeamRequest {
            name: "Backend",
            description: Some("Backend engineering"),
            visibility: Some(TeamVisibility::Secret),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["name"], "Backend");
        assert_eq!(json["description"], "Backend engineering");
        assert_eq!(json["visibility"], "secret");
    }

    #[test]
    fn create_team_request_omits_none() {
        let body = CreateTeamRequest {
            name: "Frontend",
            description: None,
            visibility: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["name"], "Frontend");
        assert!(json.get("description").is_none());
        assert!(json.get("visibility").is_none());
    }

    #[test]
    fn update_team_request_partial() {
        let body = UpdateTeamRequest {
            name: Some("New Name"),
            description: None,
            visibility: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["name"], "New Name");
        assert!(json.get("description").is_none());
        assert!(json.get("visibility").is_none());
    }

    #[test]
    fn update_team_request_all_fields() {
        let body = UpdateTeamRequest {
            name: Some("Infra"),
            description: Some("Infrastructure"),
            visibility: Some(TeamVisibility::Visible),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["name"], "Infra");
        assert_eq!(json["description"], "Infrastructure");
        assert_eq!(json["visibility"], "visible");
    }

    #[test]
    fn path_construction() {
        let org = "acme";
        let team = "backend";
        assert_eq!(format!("/orgs/{org}/teams"), "/orgs/acme/teams");
        assert_eq!(
            format!("/orgs/{org}/teams/{team}"),
            "/orgs/acme/teams/backend"
        );
    }
}
