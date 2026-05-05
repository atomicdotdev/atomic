//! Domain alias management for organizations.
//!
//! Domain aliases allow organizations to claim ownership of DNS domains,
//! enabling features like verified email domains and custom routing.

use uuid::Uuid;

use atomic_remote::storage::StorageClient;

use crate::error::{map_remote_error, TeamsResult};
use crate::types::{ClaimDomainRequest, DomainAliasInfo};

/// List all domain aliases claimed by an organization.
///
/// # Errors
///
/// Returns [`TeamsError::OrgNotFound`] if the organization does not exist,
/// or [`TeamsError::Remote`] on transport failure.
///
/// [`TeamsError::OrgNotFound`]: crate::error::TeamsError::OrgNotFound
/// [`TeamsError::Remote`]: crate::error::TeamsError::Remote
pub async fn list_domains(
    client: &StorageClient,
    org_slug: &str,
) -> TeamsResult<Vec<DomainAliasInfo>> {
    let path = format!("/orgs/{org_slug}/domains");
    client
        .get(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

/// Claim a new domain alias for an organization.
///
/// The domain is created in `"pending"` status. Call [`verify_domain`] after
/// configuring the required DNS records to complete verification.
///
/// # Errors
///
/// Returns [`TeamsError::AlreadyExists`] if the domain is already claimed,
/// [`TeamsError::PermissionDenied`] if the caller is not an org admin or owner,
/// or [`TeamsError::Remote`] on transport failure.
///
/// [`TeamsError::AlreadyExists`]: crate::error::TeamsError::AlreadyExists
/// [`TeamsError::PermissionDenied`]: crate::error::TeamsError::PermissionDenied
/// [`TeamsError::Remote`]: crate::error::TeamsError::Remote
pub async fn claim_domain(
    client: &StorageClient,
    org_slug: &str,
    domain: &str,
) -> TeamsResult<DomainAliasInfo> {
    let path = format!("/orgs/{org_slug}/domains");
    let body = ClaimDomainRequest { domain };
    client
        .post(&path, &body)
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

/// Verify a previously claimed domain alias.
///
/// The server will check the required DNS records and transition the domain
/// from `"pending"` to `"verified"` on success.
///
/// # Errors
///
/// Returns [`TeamsError::OrgNotFound`] if the organization or domain does not
/// exist, or [`TeamsError::Remote`] on transport failure.
///
/// [`TeamsError::OrgNotFound`]: crate::error::TeamsError::OrgNotFound
/// [`TeamsError::Remote`]: crate::error::TeamsError::Remote
pub async fn verify_domain(
    client: &StorageClient,
    org_slug: &str,
    domain_id: Uuid,
) -> TeamsResult<DomainAliasInfo> {
    let path = format!("/orgs/{org_slug}/domains/{domain_id}/verify");
    client
        .post_empty(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

/// Revoke (delete) a domain alias from an organization.
///
/// # Errors
///
/// Returns [`TeamsError::OrgNotFound`] if the organization or domain does not
/// exist, [`TeamsError::PermissionDenied`] if the caller is not an org admin
/// or owner, or [`TeamsError::Remote`] on transport failure.
///
/// [`TeamsError::OrgNotFound`]: crate::error::TeamsError::OrgNotFound
/// [`TeamsError::PermissionDenied`]: crate::error::TeamsError::PermissionDenied
/// [`TeamsError::Remote`]: crate::error::TeamsError::Remote
pub async fn revoke_domain(
    client: &StorageClient,
    org_slug: &str,
    domain_id: Uuid,
) -> TeamsResult<()> {
    let path = format!("/orgs/{org_slug}/domains/{domain_id}");
    client
        .delete(&path)
        .await
        .map_err(|e| map_remote_error(e, format!("org {org_slug}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_domains_path() {
        let path = format!("/orgs/{}/domains", "acme");
        assert_eq!(path, "/orgs/acme/domains");
    }

    #[test]
    fn claim_domain_request_body() {
        let body = ClaimDomainRequest {
            domain: "eng.acme.com",
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("eng.acme.com"));
        assert!(json.contains("domain"));
    }

    #[test]
    fn verify_domain_path() {
        let id = uuid::Uuid::nil();
        let path = format!("/orgs/{}/domains/{}/verify", "acme", id);
        assert_eq!(
            path,
            "/orgs/acme/domains/00000000-0000-0000-0000-000000000000/verify"
        );
    }

    #[test]
    fn revoke_domain_path() {
        let id = uuid::Uuid::nil();
        let path = format!("/orgs/{}/domains/{}", "acme", id);
        assert_eq!(
            path,
            "/orgs/acme/domains/00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn domain_alias_info_roundtrip() {
        let info = DomainAliasInfo {
            id: Uuid::new_v4(),
            org_id: Uuid::new_v4(),
            domain: "eng.acme.com".into(),
            status: "pending".into(),
            verification_method: "dns-txt".into(),
            verification_token: Some("tok_abc".into()),
        };
        let json = serde_json::to_string(&info).unwrap();
        let de: DomainAliasInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(de.domain, "eng.acme.com");
        assert_eq!(de.status, "pending");
        assert_eq!(de.verification_method, "dns-txt");
        assert_eq!(de.verification_token.as_deref(), Some("tok_abc"));
    }
}
