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
///
/// Team routes are org-scoped by the client's base URL/subdomain. For example,
/// `org_slug = "delta"` means the client targets `https://delta.<domain>`, so
/// `/teams/engineering` is Delta's engineering team. The same slug under
/// `https://atomic.<domain>/teams/engineering` is a different team.
pub async fn list_teams(client: &StorageClient, org_slug: &str) -> TeamsResult<Vec<TeamInfo>> {
    debug_assert_eq!(client.org_slug(), org_slug);
    client
        .get("/teams")
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

/// Create a new team in an organization.
///
/// The team slug is derived server-side from the `name`. If `visibility` is
/// `None` the server default (typically [`TeamVisibility::Visible`]) is used.
/// The organization is selected by the client's org-scoped base URL/subdomain,
/// not by repeating the org slug in the path.
pub async fn create_team(
    client: &StorageClient,
    org_slug: &str,
    name: &str,
    description: Option<&str>,
    visibility: Option<TeamVisibility>,
) -> TeamsResult<TeamInfo> {
    debug_assert_eq!(client.org_slug(), org_slug);
    let body = CreateTeamRequest {
        name,
        description,
        visibility,
    };
    client
        .post("/teams", &body)
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

/// Get a single team by its slug.
///
/// Team slugs are resolved within the organization selected by the client's
/// base URL/subdomain, allowing multiple organizations to each have a team
/// with the same slug.
pub async fn get_team(
    client: &StorageClient,
    org_slug: &str,
    team_slug: &str,
) -> TeamsResult<TeamInfo> {
    debug_assert_eq!(client.org_slug(), org_slug);
    let path = format!("/teams/{team_slug}");
    client
        .get(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("team {team_slug}")))
}

/// Update a team's metadata.
///
/// Only the fields that are `Some` are sent to the server — omitted fields
/// remain unchanged. The organization is selected by the client's org-scoped
/// base URL/subdomain, so identical team slugs in different orgs remain
/// distinct.
pub async fn update_team(
    client: &StorageClient,
    org_slug: &str,
    team_slug: &str,
    name: Option<&str>,
    description: Option<&str>,
    visibility: Option<TeamVisibility>,
) -> TeamsResult<TeamInfo> {
    debug_assert_eq!(client.org_slug(), org_slug);
    let path = format!("/teams/{team_slug}");
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
/// team itself. The organization is selected by the client's org-scoped base
/// URL/subdomain.
pub async fn delete_team(
    client: &StorageClient,
    org_slug: &str,
    team_slug: &str,
) -> TeamsResult<()> {
    debug_assert_eq!(client.org_slug(), org_slug);
    let path = format!("/teams/{team_slug}");
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
        let team = "backend";
        assert_eq!("/teams", "/teams");
        assert_eq!(format!("/teams/{team}"), "/teams/backend");
    }

    #[test]
    fn identical_team_slugs_are_distinguished_by_client_base_url() {
        let delta =
            StorageClient::new("https://delta.staging.atomic.storage", "delta", "tok").unwrap();
        let atomic =
            StorageClient::new("https://atomic.staging.atomic.storage", "atomic", "tok").unwrap();

        let path = "/teams/engineering";
        assert_eq!(
            format!("{}{}", delta.base_url(), path),
            "https://delta.staging.atomic.storage/teams/engineering"
        );
        assert_eq!(
            format!("{}{}", atomic.base_url(), path),
            "https://atomic.staging.atomic.storage/teams/engineering"
        );
        assert_ne!(delta.base_url(), atomic.base_url());
    }
}
