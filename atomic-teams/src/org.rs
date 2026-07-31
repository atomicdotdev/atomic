//! Organization management operations.
//!
//! This module provides async functions for creating, reading, updating, and
//! deleting organizations via the remote storage API.

use log::debug;

use atomic_remote::storage::StorageClient;

use crate::error::{map_remote_error, TeamsResult};
use crate::types::{CreateOrgRequest, MyOrgInfo, OrgInfo, UpdateOrgRequest};

/// Create a new organization.
///
/// # Arguments
///
/// * `client` — Authenticated storage client. Must target the server apex
///   (not an org subdomain): this endpoint spans orgs and is served on the
///   bare host. See `atomic_cli::commands::client::build_apex_client`.
/// * `name` — Human-readable display name for the organization.
/// * `email` — Optional contact email.
///
/// # Errors
///
/// Returns [`TeamsError::AlreadyExists`](crate::error::TeamsError::AlreadyExists)
/// if an organization with the derived slug already exists.
pub async fn create_org(
    client: &StorageClient,
    name: &str,
    email: Option<&str>,
) -> TeamsResult<OrgInfo> {
    debug!("Creating organization: name={name}");
    let slug = slugify(name);
    let body = CreateOrgRequest {
        slug: &slug,
        name,
        email,
    };
    let info: OrgInfo = client
        .post("/orgs", &body)
        .await
        .map_err(|e| map_remote_error(e, format!("org {name}")))?;
    debug!("Created organization: slug={}, id={}", info.slug, info.id);
    Ok(info)
}

/// Fetch an organization by slug.
///
/// # Errors
///
/// Returns [`TeamsError::OrgNotFound`](crate::error::TeamsError::OrgNotFound)
/// if the slug does not match any organization visible to the caller.
pub async fn get_org(client: &StorageClient, slug: &str) -> TeamsResult<OrgInfo> {
    debug!("Fetching organization: slug={slug}");
    let path = format!("/orgs/{slug}");
    let info: OrgInfo = client
        .get(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("org {slug}")))?;
    Ok(info)
}

/// List every organization the caller belongs to, with the caller's role.
///
/// This hits the apex `GET /orgs` endpoint, so `client` must target the
/// server apex (the bare host), not an org subdomain. Use
/// `atomic_cli::commands::client::build_apex_client` to build such a client.
///
/// # Errors
///
/// Returns a [`TeamsError`] derived from the remote response on any failure,
/// including [`TeamsError::Unauthorized`](crate::error::TeamsError::Unauthorized)
/// if the caller's token is rejected.
pub async fn list_my_orgs(client: &StorageClient) -> TeamsResult<Vec<MyOrgInfo>> {
    debug!("Listing organizations the caller belongs to");
    let infos: Vec<MyOrgInfo> = client
        .get("/orgs")
        .await
        .map_err(|e| map_remote_error(e, "list my orgs".to_string()))?;
    Ok(infos)
}

/// Update an existing organization.
///
/// Only fields that are `Some` will be sent to the server; `None` fields are
/// omitted from the request body so the server leaves them unchanged.
///
/// # Arguments
///
/// * `slug` — Current slug of the organization.
/// * `name` — New display name (or `None` to keep current).
/// * `email` — New contact email (or `None` to keep current).
pub async fn update_org(
    client: &StorageClient,
    slug: &str,
    name: Option<&str>,
    email: Option<&str>,
) -> TeamsResult<OrgInfo> {
    debug!("Updating organization: slug={slug}");
    let path = format!("/orgs/{slug}");
    let body = UpdateOrgRequest { name, email };
    let info: OrgInfo = client
        .put(&path, &body)
        .await
        .map_err(|e| map_remote_error(e, format!("org {slug}")))?;
    debug!("Updated organization: slug={}", info.slug);
    Ok(info)
}

/// Delete an organization.
///
/// This is a destructive operation. The caller must be an owner of the
/// organization.
///
/// # Errors
///
/// Returns [`TeamsError::PermissionDenied`](crate::error::TeamsError::PermissionDenied)
/// if the caller is not an owner.
pub async fn delete_org(client: &StorageClient, slug: &str) -> TeamsResult<()> {
    debug!("Deleting organization: slug={slug}");
    let path = format!("/orgs/{slug}");
    client
        .delete(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("org {slug}")))?;
    debug!("Deleted organization: slug={slug}");
    Ok(())
}

/// Upgrade an organization's plan.
///
/// The exact plan transition is determined server-side based on the
/// organization's current plan and eligibility.
fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }

    while slug.ends_with('-') {
        slug.pop();
    }

    slug
}

pub async fn upgrade_org(client: &StorageClient, slug: &str) -> TeamsResult<OrgInfo> {
    debug!("Upgrading organization: slug={slug}");
    let path = format!("/orgs/{slug}/upgrade");
    let info: OrgInfo = client
        .post_empty(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("org {slug}")))?;
    debug!(
        "Upgraded organization: slug={}, plan={}",
        info.slug, info.plan
    );
    Ok(info)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: Full integration tests require a running storage server and are
    // located in the top-level `tests/` directory. The unit tests here verify
    // request construction and URL formatting.

    #[test]
    fn create_org_request_body() {
        let body = CreateOrgRequest {
            slug: "acme-corp",
            name: "Acme Corp",
            email: Some("admin@acme.com"),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["slug"], "acme-corp");
        assert_eq!(json["name"], "Acme Corp");
        assert_eq!(json["email"], "admin@acme.com");
    }

    #[test]
    fn create_org_request_body_no_email() {
        let body = CreateOrgRequest {
            slug: "acme-corp",
            name: "Acme Corp",
            email: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["name"], "Acme Corp");
        assert!(json.get("email").is_none());
    }

    #[test]
    fn update_org_request_partial() {
        let body = UpdateOrgRequest {
            name: Some("New Name"),
            email: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["name"], "New Name");
        assert!(json.get("email").is_none());
    }

    #[test]
    fn update_org_request_empty() {
        let body = UpdateOrgRequest {
            name: None,
            email: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.is_empty());
    }

    #[test]
    fn url_formatting() {
        let slug = "acme";
        assert_eq!(format!("/orgs/{slug}"), "/orgs/acme");
        assert_eq!(format!("/orgs/{slug}/upgrade"), "/orgs/acme/upgrade");
    }
}
