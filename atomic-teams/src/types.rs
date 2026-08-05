//! API response types for team collaboration features.
//!
//! These types mirror the server-side API models and use camelCase JSON
//! serialization to match the server's wire format.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Role of a member within an organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OrgRole {
    /// Full control over the organization.
    Owner,
    /// Can manage members and teams but cannot delete the org.
    Admin,
    /// Regular member with default permissions.
    Member,
}

impl fmt::Display for OrgRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Owner => write!(f, "owner"),
            Self::Admin => write!(f, "admin"),
            Self::Member => write!(f, "member"),
        }
    }
}

impl FromStr for OrgRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "owner" => Ok(Self::Owner),
            "admin" => Ok(Self::Admin),
            "member" => Ok(Self::Member),
            other => Err(format!("unknown org role: {other}")),
        }
    }
}

/// Role within a team.
///
/// The role hierarchy (strongest to weakest):
/// `Maintainer` > `Contributor` > `Collaborator` > `Consumer`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TeamRole {
    /// Full control of the team: manage members, update settings, delete.
    Maintainer,
    /// Can write to team resources (push changes, create projects).
    Contributor,
    /// Can participate (read, comment, review) but not push.
    Collaborator,
    /// Read-only access to team resources.
    Consumer,
}

impl fmt::Display for TeamRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TeamRole::Maintainer => write!(f, "maintainer"),
            TeamRole::Contributor => write!(f, "contributor"),
            TeamRole::Collaborator => write!(f, "collaborator"),
            TeamRole::Consumer => write!(f, "consumer"),
        }
    }
}

impl FromStr for TeamRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "maintainer" => Ok(Self::Maintainer),
            "contributor" => Ok(Self::Contributor),
            "collaborator" => Ok(Self::Collaborator),
            "consumer" => Ok(Self::Consumer),
            other => Err(format!(
                "invalid team role: '{}' (expected 'maintainer', 'contributor', 'collaborator', or 'consumer')",
                other
            )),
        }
    }
}

/// Visibility of a team within its organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TeamVisibility {
    /// Visible to all organization members.
    Visible,
    /// Only visible to team members and org admins.
    Secret,
}

impl fmt::Display for TeamVisibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Visible => write!(f, "visible"),
            Self::Secret => write!(f, "secret"),
        }
    }
}

impl FromStr for TeamVisibility {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "visible" => Ok(Self::Visible),
            "secret" => Ok(Self::Secret),
            other => Err(format!("unknown team visibility: {other}")),
        }
    }
}

/// The relation (permission level) expressed by a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GrantRelation {
    /// Read-only access.
    Read,
    /// Read and write access.
    Write,
    /// Administrative access.
    Admin,
    /// Full ownership.
    Owner,
}

impl fmt::Display for GrantRelation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read => write!(f, "read"),
            Self::Write => write!(f, "write"),
            Self::Admin => write!(f, "admin"),
            Self::Owner => write!(f, "owner"),
        }
    }
}

impl FromStr for GrantRelation {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "admin" => Ok(Self::Admin),
            "owner" => Ok(Self::Owner),
            other => Err(format!("unknown grant relation: {other}")),
        }
    }
}

/// The kind of subject a grant is assigned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GrantSubjectType {
    /// An individual user identity.
    User,
    /// A team within the organization.
    Team,
}

impl fmt::Display for GrantSubjectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User => write!(f, "user"),
            Self::Team => write!(f, "team"),
        }
    }
}

impl FromStr for GrantSubjectType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "user" => Ok(Self::User),
            "team" => Ok(Self::Team),
            other => Err(format!("unknown grant subject type: {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Response structs
// ---------------------------------------------------------------------------

/// Organization metadata returned by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgInfo {
    /// Unique identifier.
    pub id: Uuid,
    /// URL-safe slug (e.g. `"acme"`).
    pub slug: String,
    /// Human-readable display name.
    pub name: String,
    /// Contact email for the organization.
    pub email: Option<String>,
    /// Organization kind (e.g. `"personal"`, `"team"`).
    pub kind: String,
    /// Billing plan (e.g. `"free"`, `"team"`, `"enterprise"`).
    pub plan: String,
    /// When the organization was created.
    #[serde(alias = "created_at")]
    pub created_at: DateTime<Utc>,
    /// When the organization was last updated.
    #[serde(alias = "updated_at")]
    pub updated_at: DateTime<Utc>,
}

/// An organization the caller belongs to, with their membership role.
///
/// Returned by the apex `GET /orgs` ("list my orgs") endpoint. Unlike
/// [`OrgInfo`] (which redacts `email`/`plan` for non-members), the caller is
/// always a member here, so `email` and `plan` are always present, and the
/// caller's `role`, `joined_at`, and `invited_by` are included.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyOrgInfo {
    /// Unique identifier.
    pub id: Uuid,
    /// URL-safe slug (e.g. `"acme"`).
    pub slug: String,
    /// Human-readable display name.
    pub name: String,
    /// Contact email for the organization.
    pub email: Option<String>,
    /// Organization kind (e.g. `"personal"`, `"team"`).
    pub kind: String,
    /// Billing plan (e.g. `"free"`, `"team"`, `"enterprise"`).
    pub plan: String,
    /// When the organization was created.
    #[serde(alias = "created_at")]
    pub created_at: DateTime<Utc>,
    /// When the organization was last updated.
    #[serde(alias = "updated_at")]
    pub updated_at: DateTime<Utc>,
    /// The caller's role in this org (`"owner"`, `"admin"`, or `"member"`).
    pub role: String,
    /// When the caller joined this org.
    #[serde(alias = "joined_at")]
    pub joined_at: DateTime<Utc>,
    /// Identity that invited the caller, if any.
    #[serde(alias = "invited_by")]
    pub invited_by: Option<Uuid>,
}

/// Organization member metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgMemberInfo {
    /// Organization this membership belongs to.
    #[serde(alias = "org_id")]
    pub org_id: Uuid,
    /// Identity of the member.
    #[serde(alias = "identity_id")]
    pub identity_id: Uuid,
    /// Role within the organization.
    pub role: OrgRole,
    /// When the member joined.
    #[serde(alias = "joined_at")]
    pub joined_at: DateTime<Utc>,
    /// Identity that sent the invitation, if applicable.
    #[serde(alias = "invited_by")]
    pub invited_by: Option<Uuid>,

    // The fields below describe the member's *identity* rather than the
    // membership. Every one is `#[serde(default)]` because a server older than
    // the release that added them omits it entirely — without the defaults the
    // whole response would fail to deserialize and `member list` would break
    // against an un-upgraded deployment. `None` therefore means "this server
    // did not say", not "this member has none".
    /// Display name of the member's identity.
    #[serde(default)]
    pub name: Option<String>,
    /// Ed25519 public key, base32 no-pad — the same encoding used elsewhere in
    /// Atomic, and the value a caller's token is keyed by.
    #[serde(default, alias = "public_key")]
    pub public_key: Option<String>,
    /// Identity lifecycle status: `active`, `suspended`, or `deleted`.
    #[serde(default)]
    pub status: Option<String>,
    /// Preferred email — verified if available, otherwise the oldest on file.
    #[serde(default)]
    pub email: Option<String>,
}

/// Team metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamInfo {
    /// Unique identifier.
    pub id: Uuid,
    /// Organization this team belongs to.
    #[serde(alias = "org_id")]
    pub org_id: Uuid,
    /// URL-safe slug (e.g. `"backend-eng"`).
    pub slug: String,
    /// Human-readable display name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Visibility within the organization.
    pub visibility: TeamVisibility,
    /// When the team was created.
    #[serde(alias = "created_at")]
    pub created_at: DateTime<Utc>,
    /// When the team was last updated.
    #[serde(alias = "updated_at")]
    pub updated_at: DateTime<Utc>,
}

/// Team member metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMemberInfo {
    /// Team this membership belongs to.
    #[serde(alias = "team_id")]
    pub team_id: Uuid,
    /// Identity of the member.
    #[serde(alias = "identity_id")]
    pub identity_id: Uuid,
    /// Role within the team.
    pub role: TeamRole,
    /// When the member was added.
    #[serde(alias = "added_at")]
    pub added_at: DateTime<Utc>,
    /// Identity that added this member.
    #[serde(alias = "added_by")]
    pub added_by: Uuid,
}

/// Permission grant on an organization.
///
/// Matches the server's `OrgGrantResponse` (snake_case, no serde renames).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantInfo {
    /// Unique identifier.
    pub id: Uuid,
    /// Whether the subject is a user or team (`"user"` or `"team"`).
    pub subject_type: GrantSubjectType,
    /// The subject's identity or team ID (absent for wildcard/everyone grants).
    pub subject_id: Option<Uuid>,
    /// The permission level (`"read"`, `"write"`, `"admin"`, or `"owner"`).
    pub relation: GrantRelation,
    /// Identity that created the grant.
    pub granted_by: Option<Uuid>,
    /// When the grant was created.
    pub granted_at: DateTime<Utc>,
}

/// A workspace permission grant as returned by the server.
///
/// Matches the server's `GrantResponse` (snake_case, no serde renames).
/// Unlike [`GrantInfo`] (org grants), workspace grants carry `subject_relation`,
/// `object_type`, and `object_id` — the server always includes these for
/// workspace grants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceGrantInfo {
    /// Unique identifier of the ReBAC tuple.
    pub id: Uuid,
    /// Whether the subject is a user or team (`"user"` or `"team"`).
    pub subject_type: GrantSubjectType,
    /// The subject's identity or team UUID (absent for wildcard/everyone grants).
    pub subject_id: Option<Uuid>,
    /// The subject's relation within its team, if applicable (typically `None`
    /// for direct user/team grants).
    pub subject_relation: Option<String>,
    /// The permission level (`"read"`, `"write"`, or `"admin"`).
    pub relation: GrantRelation,
    /// The resource type (always `"workspace"` for workspace grants).
    pub object_type: String,
    /// The workspace UUID the grant applies to.
    pub object_id: Uuid,
    /// When the grant was created.
    pub granted_at: DateTime<Utc>,
    /// Identity that created the grant.
    pub granted_by: Option<Uuid>,
}

/// Domain alias claimed by an organization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainAliasInfo {
    /// Unique identifier.
    pub id: Uuid,
    /// Organization this domain belongs to.
    pub org_id: Uuid,
    /// The domain name (e.g. `"eng.acme.com"`).
    pub domain: String,
    /// Verification status (e.g. `"pending"`, `"verified"`).
    pub status: String,
    /// How the domain should be verified (e.g. `"dns-txt"`, `"dns-cname"`).
    pub verification_method: String,
    /// One-time token used during DNS verification.
    pub verification_token: Option<String>,
}

// ---------------------------------------------------------------------------
// Request body helpers (internal)
// ---------------------------------------------------------------------------

/// Body for creating an organization.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateOrgRequest<'a> {
    pub slug: &'a str,
    pub name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<&'a str>,
}

/// Body for updating an organization.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateOrgRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<&'a str>,
}

/// Body for adding an organization member.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct AddMemberRequest {
    pub identity_id: Uuid,
    pub role: OrgRole,
}

/// Body for updating an organization member's role.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateMemberRoleRequest {
    pub role: OrgRole,
}

/// Body for creating a team.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateTeamRequest<'a> {
    pub name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<TeamVisibility>,
}

/// Body for updating a team.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateTeamRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<TeamVisibility>,
}

/// Body for adding a team member.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct AddTeamMemberRequest {
    pub identity_id: Uuid,
    pub role: TeamRole,
}

/// Body for updating a team member's role.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateTeamMemberRoleRequest {
    pub role: TeamRole,
}

/// Body for adding a grant (workspace or org).
///
/// Serialized as snake_case to match the server's `AddGrantRequest` /
/// `AddOrgGrantRequest` (no serde renames server-side).
#[derive(Debug, Serialize)]
pub(crate) struct AddGrantRequest {
    pub subject_type: GrantSubjectType,
    pub subject_id: Uuid,
    pub relation: GrantRelation,
}

/// Body for claiming a domain.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaimDomainRequest<'a> {
    pub domain: &'a str,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn org_role_display_roundtrip() {
        for role in [OrgRole::Owner, OrgRole::Admin, OrgRole::Member] {
            let s = role.to_string();
            let parsed: OrgRole = s.parse().unwrap();
            assert_eq!(parsed, role);
        }
    }

    #[test]
    fn org_role_parse_case_insensitive() {
        assert_eq!("OWNER".parse::<OrgRole>().unwrap(), OrgRole::Owner);
        assert_eq!("Admin".parse::<OrgRole>().unwrap(), OrgRole::Admin);
        assert!("unknown".parse::<OrgRole>().is_err());
    }

    #[test]
    fn team_role_display_roundtrip() {
        for role in [
            TeamRole::Maintainer,
            TeamRole::Contributor,
            TeamRole::Collaborator,
            TeamRole::Consumer,
        ] {
            let s = role.to_string();
            let parsed: TeamRole = s.parse().unwrap();
            assert_eq!(parsed, role);
        }
    }

    #[test]
    fn team_visibility_display_roundtrip() {
        for vis in [TeamVisibility::Visible, TeamVisibility::Secret] {
            let s = vis.to_string();
            let parsed: TeamVisibility = s.parse().unwrap();
            assert_eq!(parsed, vis);
        }
    }

    #[test]
    fn grant_relation_display_roundtrip() {
        for rel in [
            GrantRelation::Read,
            GrantRelation::Write,
            GrantRelation::Admin,
            GrantRelation::Owner,
        ] {
            let s = rel.to_string();
            let parsed: GrantRelation = s.parse().unwrap();
            assert_eq!(parsed, rel);
        }
    }

    #[test]
    fn grant_subject_type_display_roundtrip() {
        for st in [GrantSubjectType::User, GrantSubjectType::Team] {
            let s = st.to_string();
            let parsed: GrantSubjectType = s.parse().unwrap();
            assert_eq!(parsed, st);
        }
    }

    #[test]
    fn org_info_serde_roundtrip() {
        let now = Utc::now();
        let info = OrgInfo {
            id: Uuid::new_v4(),
            slug: "acme".into(),
            name: "Acme Corp".into(),
            email: Some("admin@acme.com".into()),
            kind: "team".into(),
            plan: "enterprise".into(),
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&info).unwrap();
        let de: OrgInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(de.slug, "acme");
        assert_eq!(de.email.as_deref(), Some("admin@acme.com"));
    }

    #[test]
    fn my_org_info_serde_roundtrip() {
        let now = Utc::now();
        let info = MyOrgInfo {
            id: Uuid::new_v4(),
            slug: "acme".into(),
            name: "Acme Corp".into(),
            email: Some("admin@acme.com".into()),
            kind: "team".into(),
            plan: "enterprise".into(),
            created_at: now,
            updated_at: now,
            role: "owner".into(),
            joined_at: now,
            invited_by: Some(Uuid::new_v4()),
        };
        let json = serde_json::to_string(&info).unwrap();
        // camelCase wire format (server returns createdAt/joinedAt/invitedBy).
        assert!(json.contains("createdAt"));
        assert!(json.contains("joinedAt"));
        assert!(json.contains("invitedBy"));
        assert!(!json.contains("created_at"));

        let de: MyOrgInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(de.slug, "acme");
        assert_eq!(de.role, "owner");
        assert!(de.invited_by.is_some());
    }

    #[test]
    fn my_org_info_accepts_snake_case_aliases() {
        // Server-side `to_string()` emits camelCase, but earlier deployments
        // may emit snake_case; the aliases keep deserialization lenient.
        let json = r#"{
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "slug": "acme",
            "name": "Acme",
            "email": null,
            "kind": "team",
            "plan": "free",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "role": "member",
            "joined_at": "2024-01-02T00:00:00Z",
            "invited_by": null
        }"#;
        let de: MyOrgInfo = serde_json::from_str(json).unwrap();
        assert_eq!(de.slug, "acme");
        assert_eq!(de.role, "member");
        assert!(de.email.is_none());
        assert!(de.invited_by.is_none());
    }

    #[test]
    fn org_info_camel_case_keys() {
        let info = OrgInfo {
            id: Uuid::nil(),
            slug: "x".into(),
            name: "X".into(),
            email: None,
            kind: "personal".into(),
            plan: "free".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("createdAt"));
        assert!(json.contains("updatedAt"));
        assert!(!json.contains("created_at"));
    }

    #[test]
    fn team_info_serde_roundtrip() {
        let now = Utc::now();
        let info = TeamInfo {
            id: Uuid::new_v4(),
            org_id: Uuid::new_v4(),
            slug: "backend".into(),
            name: "Backend".into(),
            description: Some("Backend engineering".into()),
            visibility: TeamVisibility::Visible,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&info).unwrap();
        let de: TeamInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(de.slug, "backend");
        assert_eq!(de.visibility, TeamVisibility::Visible);
    }

    #[test]
    fn grant_info_serde_roundtrip() {
        let now = Utc::now();
        let info = GrantInfo {
            id: Uuid::new_v4(),
            subject_type: GrantSubjectType::Team,
            subject_id: Some(Uuid::new_v4()),
            relation: GrantRelation::Write,
            granted_by: None,
            granted_at: now,
        };
        let json = serde_json::to_string(&info).unwrap();
        let de: GrantInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(de.subject_type, GrantSubjectType::Team);
        assert_eq!(de.relation, GrantRelation::Write);
    }

    #[test]
    fn grant_info_deserializes_from_server_snake_case() {
        // The server sends snake_case JSON (no serde renames). GrantInfo must
        // accept that, not camelCase.
        let json = r#"{
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "subject_type": "team",
            "subject_id": "550e8400-e29b-41d4-a716-446655440001",
            "relation": "admin",
            "granted_by": null,
            "granted_at": "2024-01-01T00:00:00Z"
        }"#;
        let de: GrantInfo = serde_json::from_str(json).unwrap();
        assert_eq!(de.subject_type, GrantSubjectType::Team);
        assert_eq!(de.relation, GrantRelation::Admin);
        assert!(de.granted_by.is_none());
    }

    #[test]
    fn workspace_grant_info_deserializes_from_server_snake_case() {
        // The server's GrantResponse includes subject_relation, object_type,
        // and object_id — all snake_case.
        let json = r#"{
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "subject_type": "team",
            "subject_id": "550e8400-e29b-41d4-a716-446655440001",
            "subject_relation": null,
            "relation": "write",
            "object_type": "workspace",
            "object_id": "550e8400-e29b-41d4-a716-446655440002",
            "granted_at": "2024-01-01T00:00:00Z",
            "granted_by": "550e8400-e29b-41d4-a716-446655440003"
        }"#;
        let de: WorkspaceGrantInfo = serde_json::from_str(json).unwrap();
        assert_eq!(de.subject_type, GrantSubjectType::Team);
        assert_eq!(de.relation, GrantRelation::Write);
        assert_eq!(de.object_type, "workspace");
        assert!(de.subject_relation.is_none());
        assert!(de.granted_by.is_some());
    }

    #[test]
    fn domain_alias_info_serde_roundtrip() {
        let info = DomainAliasInfo {
            id: Uuid::new_v4(),
            org_id: Uuid::new_v4(),
            domain: "eng.acme.com".into(),
            status: "pending".into(),
            verification_method: "dns-txt".into(),
            verification_token: Some("abc123".into()),
        };
        let json = serde_json::to_string(&info).unwrap();
        let de: DomainAliasInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(de.domain, "eng.acme.com");
        assert_eq!(de.verification_token.as_deref(), Some("abc123"));
    }

    #[test]
    fn org_member_info_serde_roundtrip() {
        let now = Utc::now();
        let info = OrgMemberInfo {
            org_id: Uuid::new_v4(),
            identity_id: Uuid::new_v4(),
            role: OrgRole::Admin,
            joined_at: now,
            invited_by: Some(Uuid::new_v4()),
            name: Some("ada".to_string()),
            public_key: Some("MFRGGZDFMZTWQ2LK".to_string()),
            status: Some("active".to_string()),
            email: Some("ada@example.com".to_string()),
        };
        let json = serde_json::to_string(&info).unwrap();
        let de: OrgMemberInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(de.role, OrgRole::Admin);
        assert!(de.invited_by.is_some());
        assert_eq!(de.name.as_deref(), Some("ada"));
        assert_eq!(de.public_key.as_deref(), Some("MFRGGZDFMZTWQ2LK"));
        assert_eq!(de.status.as_deref(), Some("active"));
        assert_eq!(de.email.as_deref(), Some("ada@example.com"));
    }

    /// A server predating the identity-detail fields omits them entirely.
    ///
    /// Without `#[serde(default)]` on each, the whole response would fail to
    /// parse and `org member list` would break against an un-upgraded
    /// deployment rather than degrading to the columns it does know.
    #[test]
    fn org_member_info_tolerates_server_without_identity_details() {
        let json = r#"{
            "org_id": "00000000-0000-0000-0000-000000000001",
            "identity_id": "00000000-0000-0000-0000-000000000002",
            "role": "member",
            "joined_at": "2026-01-01T00:00:00Z",
            "invited_by": null
        }"#;

        let de: OrgMemberInfo = serde_json::from_str(json).expect("legacy payload must parse");
        assert_eq!(de.role, OrgRole::Member);
        assert_eq!(de.name, None);
        assert_eq!(de.public_key, None);
        assert_eq!(de.status, None);
        assert_eq!(de.email, None);
    }

    /// The server serializes these as snake_case; the struct is camelCase.
    #[test]
    fn org_member_info_accepts_snake_case_public_key() {
        let json = r#"{
            "org_id": "00000000-0000-0000-0000-000000000001",
            "identity_id": "00000000-0000-0000-0000-000000000002",
            "role": "owner",
            "joined_at": "2026-01-01T00:00:00Z",
            "invited_by": null,
            "name": "ada",
            "public_key": "MFRGGZDFMZTWQ2LK",
            "status": "active",
            "email": "ada@example.com"
        }"#;

        let de: OrgMemberInfo = serde_json::from_str(json).expect("snake_case payload must parse");
        assert_eq!(de.public_key.as_deref(), Some("MFRGGZDFMZTWQ2LK"));
        assert_eq!(de.name.as_deref(), Some("ada"));
    }

    #[test]
    fn team_member_info_serde_roundtrip() {
        let now = Utc::now();
        let info = TeamMemberInfo {
            team_id: Uuid::new_v4(),
            identity_id: Uuid::new_v4(),
            role: TeamRole::Maintainer,
            added_at: now,
            added_by: Uuid::new_v4(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let de: TeamMemberInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(de.role, TeamRole::Maintainer);
    }

    #[test]
    fn update_org_request_skips_none_fields() {
        let req = UpdateOrgRequest {
            name: Some("New Name"),
            email: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("name"));
        assert!(!json.contains("email"));
    }

    #[test]
    fn update_team_request_skips_none_fields() {
        let req = UpdateTeamRequest {
            name: None,
            description: Some("Updated"),
            visibility: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("name"));
        assert!(json.contains("description"));
        assert!(!json.contains("visibility"));
    }

    #[test]
    fn create_team_request_skips_none_fields() {
        let req = CreateTeamRequest {
            name: "team",
            description: None,
            visibility: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("name"));
        assert!(!json.contains("description"));
        assert!(!json.contains("visibility"));
    }

    #[test]
    fn enum_serde_camel_case() {
        // Verify enum variants serialize as camelCase strings.
        let json = serde_json::to_string(&OrgRole::Owner).unwrap();
        assert_eq!(json, "\"owner\"");

        let json = serde_json::to_string(&TeamVisibility::Secret).unwrap();
        assert_eq!(json, "\"secret\"");

        let json = serde_json::to_string(&GrantRelation::Admin).unwrap();
        assert_eq!(json, "\"admin\"");

        let json = serde_json::to_string(&GrantSubjectType::Team).unwrap();
        assert_eq!(json, "\"team\"");
    }

    #[test]
    fn enum_deserialize_from_camel_case() {
        let role: OrgRole = serde_json::from_str("\"admin\"").unwrap();
        assert_eq!(role, OrgRole::Admin);

        let vis: TeamVisibility = serde_json::from_str("\"secret\"").unwrap();
        assert_eq!(vis, TeamVisibility::Secret);

        let rel: GrantRelation = serde_json::from_str("\"write\"").unwrap();
        assert_eq!(rel, GrantRelation::Write);
    }
}
