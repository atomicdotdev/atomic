//! Organization member management.
//!
//! Functions for listing, adding, updating, and removing members of an
//! organization via the remote storage API.

use uuid::Uuid;

use atomic_remote::storage::StorageClient;

use crate::error::{map_remote_error, TeamsResult};
use crate::types::{AddMemberRequest, OrgMemberInfo, OrgRole, UpdateMemberRoleRequest};

/// List all members of an organization.
///
/// # Arguments
///
/// * `client` — Authenticated storage client.
/// * `org_slug` — URL-safe slug of the organization.
pub async fn list_members(
    client: &StorageClient,
    org_slug: &str,
) -> TeamsResult<Vec<OrgMemberInfo>> {
    let path = format!("/orgs/{org_slug}/members");
    client
        .get(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

/// Add a member to an organization.
///
/// # Arguments
///
/// * `client` — Authenticated storage client.
/// * `org_slug` — URL-safe slug of the organization.
/// * `identity_id` — Identity to add.
/// * `role` — Role to assign to the new member.
pub async fn add_member(
    client: &StorageClient,
    org_slug: &str,
    identity_id: Uuid,
    role: OrgRole,
) -> TeamsResult<OrgMemberInfo> {
    let path = format!("/orgs/{org_slug}/members");
    let body = AddMemberRequest { identity_id, role };
    client
        .post(&path, &body)
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

/// Get details for a single organization member.
///
/// # Arguments
///
/// * `client` — Authenticated storage client.
/// * `org_slug` — URL-safe slug of the organization.
/// * `identity_id` — Identity of the member to retrieve.
pub async fn get_member(
    client: &StorageClient,
    org_slug: &str,
    identity_id: Uuid,
) -> TeamsResult<OrgMemberInfo> {
    let path = format!("/orgs/{org_slug}/members/{identity_id}");
    client.get(&path).await.map_err(|e| {
        map_remote_error(
            e,
            format!("member identity {identity_id} in org {org_slug}"),
        )
    })
}

/// Update the role of an existing organization member.
///
/// # Arguments
///
/// * `client` — Authenticated storage client.
/// * `org_slug` — URL-safe slug of the organization.
/// * `identity_id` — Identity of the member to update.
/// * `role` — New role to assign.
pub async fn update_member_role(
    client: &StorageClient,
    org_slug: &str,
    identity_id: Uuid,
    role: OrgRole,
) -> TeamsResult<OrgMemberInfo> {
    let path = format!("/orgs/{org_slug}/members/{identity_id}");
    let body = UpdateMemberRoleRequest { role };
    client.put(&path, &body).await.map_err(|e| {
        map_remote_error(
            e,
            format!("member identity {identity_id} in org {org_slug}"),
        )
    })
}

/// Remove a member from an organization.
///
/// Returns an error if the member is the last owner
/// ([`TeamsError::LastOwner`](crate::error::TeamsError::LastOwner) mapped from
/// a 409 response).
///
/// # Arguments
///
/// * `client` — Authenticated storage client.
/// * `org_slug` — URL-safe slug of the organization.
/// * `identity_id` — Identity of the member to remove.
pub async fn remove_member(
    client: &StorageClient,
    org_slug: &str,
    identity_id: Uuid,
) -> TeamsResult<()> {
    let path = format!("/orgs/{org_slug}/members/{identity_id}");
    client.delete(&path).await.map_err(|e| {
        map_remote_error(
            e,
            format!("member identity {identity_id} in org {org_slug}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_member_request_serializes() {
        let req = AddMemberRequest {
            identity_id: Uuid::nil(),
            role: OrgRole::Member,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("identity_id"));
        assert!(json.contains("member"));
    }

    #[test]
    fn update_member_role_request_serializes() {
        let req = UpdateMemberRoleRequest {
            role: OrgRole::Admin,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("role"));
        assert!(json.contains("admin"));
    }

    #[test]
    fn role_variants_serialize_correctly() {
        for (role, expected) in [
            (OrgRole::Owner, "\"owner\""),
            (OrgRole::Admin, "\"admin\""),
            (OrgRole::Member, "\"member\""),
        ] {
            assert_eq!(serde_json::to_string(&role).unwrap(), expected);
        }
    }
}
