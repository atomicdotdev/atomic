//! Agent delegation — authorizing an agent to act on behalf of a human.
//!
//! A delegation is the answer to "does this agent key belong to that person,
//! and what may it do?". It names a delegator (a human identity), a delegate
//! (an agent identity with its own keypair), a [`DelegationScope`], and an
//! expiry. On the wire and on disk it is a signed **certificate** — a canonical
//! JSON-LD node carrying an `eddsa-jcs-2022` Data Integrity proof, minted and
//! verified by `atomic-canonical`'s `delegation` module.
//!
//! This module owns the *data model* only. It deliberately does no signing:
//! there is exactly one signature format in Atomic (JCS + Data Integrity), and
//! it lives one layer up, in the crate that owns canonicalization. That keeps
//! the certificate's bytes identical in the CLI, on disk, and in the server's
//! database.
//!
//! # Delegation model
//!
//! ```text
//! ┌─────────────────┐     signs a certificate     ┌─────────────────┐
//! │  User Identity  │ ─────────────────────────▶ │  Agent Identity │
//! │   (delegator)   │   naming the agent's DID    │   (delegate)    │
//! └─────────────────┘   + scope + expiry          └─────────────────┘
//!         │                                               │
//!         │ holds grants on the server                    │ holds none
//!         ▼                                               ▼
//!    effective permissions = delegator's grants ∩ delegation scope
//! ```
//!
//! The intersection is the invariant that makes the whole thing safe: an agent
//! can never do more than the human who issued it, so revoking the human's
//! access revokes the agent's with no extra bookkeeping.
//!
//! # Example
//!
//! ```rust
//! use atomic_identity::{Identity, IdentityType};
//! use atomic_identity::delegation::{
//!     Delegation, DelegationPermission, DelegationScope, ResourceRef,
//! };
//!
//! let user = Identity::generate("alice");
//! let agent = Identity::builder("alice+claude")
//!     .identity_type(IdentityType::Agent)
//!     .delegated_by(user.id)
//!     .build()?;
//!
//! let scope = DelegationScope::builder()
//!     .permission(DelegationPermission::Record)
//!     .permission(DelegationPermission::Push)
//!     .server("https://atomic.storage")
//!     .project("alice/*")
//!     .build();
//!
//! let delegation = Delegation::new(&user, &agent, scope);
//!
//! assert!(delegation.allows(
//!     DelegationPermission::Push,
//!     &ResourceRef::new().server("https://atomic.storage").project("alice/api"),
//! ));
//! // Out of scope: a different project namespace.
//! assert!(!delegation.allows(
//!     DelegationPermission::Push,
//!     &ResourceRef::new().project("bob/api"),
//! ));
//! # Ok::<(), atomic_identity::IdentityError>(())
//! ```

use crate::identity::{Identity, IdentityId};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Permissions that can be granted to a delegated identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationPermission {
    /// Permission to read repository content.
    Read,

    /// Permission to record (commit) changes.
    Record,

    /// Permission to push changes to remotes.
    Push,

    /// Permission to pull changes from remotes.
    Pull,

    /// Permission to create/delete views.
    ManageViews,

    /// Permission to create/delete tags.
    ManageTags,

    /// Permission to manage repository settings.
    Admin,

    /// Full permissions (all of the above).
    Full,
}

impl DelegationPermission {
    /// Get a human-readable description.
    pub fn description(&self) -> &'static str {
        match self {
            DelegationPermission::Read => "Read repository content",
            DelegationPermission::Record => "Record (commit) changes",
            DelegationPermission::Push => "Push changes to remotes",
            DelegationPermission::Pull => "Pull changes from remotes",
            DelegationPermission::ManageViews => "Create and delete views",
            DelegationPermission::ManageTags => "Create and delete tags",
            DelegationPermission::Admin => "Manage repository settings",
            DelegationPermission::Full => "Full access (all permissions)",
        }
    }

    /// Check if this permission implies another permission.
    pub fn implies(&self, other: &DelegationPermission) -> bool {
        match self {
            DelegationPermission::Full => true,
            DelegationPermission::Admin => matches!(
                other,
                DelegationPermission::Read
                    | DelegationPermission::ManageViews
                    | DelegationPermission::ManageTags
                    | DelegationPermission::Admin
            ),
            _ => self == other,
        }
    }

    /// Get all standard permissions (excluding Full).
    pub fn standard_permissions() -> &'static [DelegationPermission] {
        &[
            DelegationPermission::Read,
            DelegationPermission::Record,
            DelegationPermission::Push,
            DelegationPermission::Pull,
            DelegationPermission::ManageViews,
            DelegationPermission::ManageTags,
            DelegationPermission::Admin,
        ]
    }
}

impl fmt::Display for DelegationPermission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DelegationPermission::Read => write!(f, "read"),
            DelegationPermission::Record => write!(f, "record"),
            DelegationPermission::Push => write!(f, "push"),
            DelegationPermission::Pull => write!(f, "pull"),
            DelegationPermission::ManageViews => write!(f, "manage_views"),
            DelegationPermission::ManageTags => write!(f, "manage_tags"),
            DelegationPermission::Admin => write!(f, "admin"),
            DelegationPermission::Full => write!(f, "full"),
        }
    }
}

impl std::str::FromStr for DelegationPermission {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().replace('-', "_").as_str() {
            "read" => Ok(DelegationPermission::Read),
            "record" => Ok(DelegationPermission::Record),
            "push" => Ok(DelegationPermission::Push),
            "pull" => Ok(DelegationPermission::Pull),
            "manage_views" | "manage_stacks" => Ok(DelegationPermission::ManageViews),
            "manage_tags" => Ok(DelegationPermission::ManageTags),
            "admin" => Ok(DelegationPermission::Admin),
            "full" => Ok(DelegationPermission::Full),
            other => Err(format!(
                "unknown permission '{other}' (expected one of: read, record, push, pull, \
                 manage_views, manage_tags, admin, full)"
            )),
        }
    }
}

/// The resource an authorization decision is being made about.
///
/// Every field is optional: an absent field means "this dimension is not being
/// constrained by the caller", and the corresponding scope patterns are not
/// consulted. A server MUST populate the fields it can derive from the request
/// path — never from a client-supplied value — since these are what the scope
/// is matched against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceRef<'a> {
    /// Canonical server URL, e.g. `https://atomic.storage`.
    pub server: Option<&'a str>,
    /// Workspace slug.
    pub workspace: Option<&'a str>,
    /// Project path, e.g. `acme/api`.
    pub project: Option<&'a str>,
    /// View name.
    pub view: Option<&'a str>,
}

impl<'a> ResourceRef<'a> {
    /// An unconstrained resource reference.
    pub fn new() -> Self {
        Self::default()
    }

    /// Constrain the server.
    pub fn server(mut self, server: &'a str) -> Self {
        self.server = Some(server);
        self
    }

    /// Constrain the workspace.
    pub fn workspace(mut self, workspace: &'a str) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Constrain the project.
    pub fn project(mut self, project: &'a str) -> Self {
        self.project = Some(project);
        self
    }

    /// Constrain the view.
    pub fn view(mut self, view: &'a str) -> Self {
        self.view = Some(view);
        self
    }
}

/// The scope of a delegation, defining what the delegate can do.
///
/// Every pattern list follows the same rule: **empty means unrestricted on that
/// dimension**, a non-empty list means the value must match one of the globs.
/// Scope only ever narrows — it is intersected with the delegator's own
/// permissions, never unioned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationScope {
    /// Permissions granted to the delegate.
    pub permissions: Vec<DelegationPermission>,

    /// Server URLs this delegation is valid against.
    ///
    /// Empty means all servers. A certificate minted for staging must not
    /// authenticate against production, so the CLI always populates this.
    #[serde(default)]
    pub servers: Vec<String>,

    /// Workspace slugs (glob patterns) the delegation applies to.
    #[serde(default)]
    pub workspaces: Vec<String>,

    /// Project paths (glob patterns) the delegation applies to.
    #[serde(default, alias = "repository_patterns")]
    pub projects: Vec<String>,

    /// View names (glob patterns) the delegation applies to.
    #[serde(default, alias = "view_patterns", alias = "stack_patterns")]
    pub views: Vec<String>,

    /// Maximum number of changes the delegate can create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_changes: Option<u64>,

    /// Human-readable description of the scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Default for DelegationScope {
    fn default() -> Self {
        Self {
            permissions: vec![DelegationPermission::Read],
            servers: Vec::new(),
            workspaces: Vec::new(),
            projects: Vec::new(),
            views: Vec::new(),
            max_changes: None,
            description: None,
        }
    }
}

impl DelegationScope {
    /// Create a new delegation scope with read permission.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a scope with full permissions.
    pub fn full() -> Self {
        Self {
            permissions: vec![DelegationPermission::Full],
            ..Default::default()
        }
    }

    /// Create a read-only scope.
    pub fn read_only() -> Self {
        Self {
            permissions: vec![DelegationPermission::Read, DelegationPermission::Pull],
            ..Default::default()
        }
    }

    /// Create a scope suited to a CI/CD agent: read, record, push, pull.
    pub fn ci_cd() -> Self {
        Self {
            permissions: vec![
                DelegationPermission::Read,
                DelegationPermission::Record,
                DelegationPermission::Push,
                DelegationPermission::Pull,
            ],
            ..Default::default()
        }
    }

    /// Create a builder for custom scopes.
    pub fn builder() -> DelegationScopeBuilder {
        DelegationScopeBuilder::new()
    }

    /// Check if this scope has a specific permission.
    pub fn has_permission(&self, permission: DelegationPermission) -> bool {
        self.permissions.iter().any(|p| p.implies(&permission))
    }

    /// Check if this scope is valid against a server URL.
    ///
    /// Comparison ignores a trailing slash and is case-insensitive on the host,
    /// so `https://Atomic.Storage/` and `https://atomic.storage` are the same
    /// server. Unlike the other dimensions this is an exact match, not a glob:
    /// a wildcard server would defeat the point of binding a certificate to
    /// the deployment it was issued for.
    pub fn allows_server(&self, server_url: &str) -> bool {
        if self.servers.is_empty() {
            return true;
        }
        let wanted = normalize_server_url(server_url);
        self.servers
            .iter()
            .any(|s| normalize_server_url(s) == wanted)
    }

    /// Check if this scope allows access to a workspace.
    pub fn allows_workspace(&self, workspace: &str) -> bool {
        matches_any(&self.workspaces, workspace)
    }

    /// Check if this scope allows access to a project.
    pub fn allows_project(&self, project: &str) -> bool {
        matches_any(&self.projects, project)
    }

    /// Check if this scope allows access to a view.
    pub fn allows_view(&self, view_name: &str) -> bool {
        matches_any(&self.views, view_name)
    }

    /// Check a permission against a resource in one call.
    pub fn allows(&self, permission: DelegationPermission, resource: &ResourceRef<'_>) -> bool {
        if !self.has_permission(permission) {
            return false;
        }
        if let Some(server) = resource.server {
            if !self.allows_server(server) {
                return false;
            }
        }
        if let Some(workspace) = resource.workspace {
            if !self.allows_workspace(workspace) {
                return false;
            }
        }
        if let Some(project) = resource.project {
            if !self.allows_project(project) {
                return false;
            }
        }
        if let Some(view) = resource.view {
            if !self.allows_view(view) {
                return false;
            }
        }
        true
    }
}

/// Normalize a server URL for comparison: lowercased, no trailing slash.
fn normalize_server_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_lowercase()
}

/// An empty pattern list is unrestricted; otherwise the value must match one.
fn matches_any(patterns: &[String], value: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns.iter().any(|p| matches_pattern(p, value))
}

/// Simple glob pattern matching (supports `*` and `?`).
fn matches_pattern(pattern: &str, value: &str) -> bool {
    let pattern_chars: Vec<char> = pattern.chars().collect();
    let value_chars: Vec<char> = value.chars().collect();
    matches_pattern_recursive(&pattern_chars, &value_chars)
}

fn matches_pattern_recursive(pattern: &[char], value: &[char]) -> bool {
    match (pattern.first(), value.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            matches_pattern_recursive(&pattern[1..], value)
                || (!value.is_empty() && matches_pattern_recursive(pattern, &value[1..]))
        }
        (Some('?'), Some(_)) => matches_pattern_recursive(&pattern[1..], &value[1..]),
        (Some(p), Some(v)) if p == v => matches_pattern_recursive(&pattern[1..], &value[1..]),
        _ => false,
    }
}

/// Builder for creating delegation scopes.
#[derive(Debug, Default)]
pub struct DelegationScopeBuilder {
    permissions: Vec<DelegationPermission>,
    servers: Vec<String>,
    workspaces: Vec<String>,
    projects: Vec<String>,
    views: Vec<String>,
    max_changes: Option<u64>,
    description: Option<String>,
}

impl DelegationScopeBuilder {
    /// Create a new scope builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a permission.
    pub fn permission(mut self, permission: DelegationPermission) -> Self {
        if !self.permissions.contains(&permission) {
            self.permissions.push(permission);
        }
        self
    }

    /// Add several permissions.
    pub fn permissions(
        mut self,
        permissions: impl IntoIterator<Item = DelegationPermission>,
    ) -> Self {
        for permission in permissions {
            self = self.permission(permission);
        }
        self
    }

    /// Bind the delegation to a server URL.
    pub fn server(mut self, url: impl Into<String>) -> Self {
        self.servers.push(url.into());
        self
    }

    /// Restrict the delegation to a workspace pattern.
    pub fn workspace(mut self, pattern: impl Into<String>) -> Self {
        self.workspaces.push(pattern.into());
        self
    }

    /// Restrict the delegation to a project pattern.
    pub fn project(mut self, pattern: impl Into<String>) -> Self {
        self.projects.push(pattern.into());
        self
    }

    /// Restrict the delegation to a view pattern.
    pub fn view(mut self, pattern: impl Into<String>) -> Self {
        self.views.push(pattern.into());
        self
    }

    /// Cap the number of changes the delegate may create.
    pub fn max_changes(mut self, max: u64) -> Self {
        self.max_changes = Some(max);
        self
    }

    /// Describe the scope for humans.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Build the delegation scope.
    pub fn build(mut self) -> DelegationScope {
        // A scope with no permissions would authorize nothing; read is the
        // floor, matching `DelegationScope::default()`.
        if self.permissions.is_empty() {
            self.permissions.push(DelegationPermission::Read);
        }

        DelegationScope {
            permissions: self.permissions,
            servers: self.servers,
            workspaces: self.workspaces,
            projects: self.projects,
            views: self.views,
            max_changes: self.max_changes,
            description: self.description,
        }
    }
}

/// A delegation authorizing an agent to act on behalf of a user.
///
/// This is the *typed view* of a certificate. The authoritative artifact is the
/// signed canonical document produced by `atomic_canonical::delegation::mint`;
/// this struct is what you get back from parsing one, and what you build before
/// minting. It deliberately carries no signature field and no revocation state:
/// the signature lives in the document's `proof`, and revocation is a separate
/// signed document plus server-side status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegation {
    /// Deterministic identifier, derived from delegator + delegate + issue time.
    pub id: DelegationId,

    /// The delegating identity's DID (`did:atomic:...`).
    pub delegator: String,

    /// The delegating identity's name at issue time (a label, not identity).
    pub delegator_name: String,

    /// The delegator's key in `did:key` form.
    ///
    /// Carried so a certificate is self-contained: a machine holding only the
    /// agent's key (a CI runner, a fresh clone) can still check the signature.
    /// Self-verification proves integrity, not trust — a verifier must still
    /// decide whether it trusts this delegator, which is what the server's
    /// registered-key lookup settles.
    pub delegator_key: String,

    /// The delegate identity's DID (`did:atomic:...`).
    pub delegate: String,

    /// The delegate's key in `did:key` form, from which the public key is
    /// recoverable — `did:atomic` is a blake3 fingerprint and is not.
    pub delegate_key: String,

    /// The delegate identity's name at issue time.
    pub delegate_name: String,

    /// Which software agent this key belongs to (`urn:atomic:agent:claude-code`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub software_agent: Option<String>,

    /// What the delegate may do.
    pub scope: DelegationScope,

    /// When the delegation was issued.
    pub issued: DateTime<Utc>,

    /// When the delegation expires. `None` means it never does — strongly
    /// discouraged for agent keys, which are unattended by definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<DateTime<Utc>>,
}

/// Unique identifier for a delegation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DelegationId([u8; 32]);

impl DelegationId {
    /// URN prefix for the rendered form.
    pub const URN_PREFIX: &'static str = "urn:atomic:delegation:";

    /// Derive the deterministic id for a delegation's defining triple.
    ///
    /// Deterministic so a verifier can recompute it from the certificate body
    /// and confirm the `@id` was not swapped — the id is a claim like any
    /// other, and the only claims worth trusting are the ones you can recheck.
    pub fn from_delegation_data(
        delegator_id: &IdentityId,
        delegate_id: &IdentityId,
        created_at: DateTime<Utc>,
    ) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"atomic:delegation:v1");
        hasher.update(delegator_id.as_bytes());
        hasher.update(delegate_id.as_bytes());
        hasher.update(&created_at.timestamp().to_le_bytes());
        Self(*hasher.finalize().as_bytes())
    }

    /// Wrap raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Base32 (no padding) rendering — the form used in filenames and URNs.
    pub fn to_base32(&self) -> String {
        data_encoding::BASE32_NOPAD.encode(&self.0)
    }

    /// Parse a base32 rendering, with or without the `urn:atomic:delegation:`
    /// prefix.
    pub fn from_base32(s: &str) -> Option<Self> {
        let raw = s.strip_prefix(Self::URN_PREFIX).unwrap_or(s);
        let bytes = data_encoding::BASE32_NOPAD.decode(raw.as_bytes()).ok()?;
        let bytes: [u8; 32] = bytes.try_into().ok()?;
        Some(Self(bytes))
    }

    /// The canonical URN form (`urn:atomic:delegation:<base32>`).
    pub fn to_urn(&self) -> String {
        format!("{}{}", Self::URN_PREFIX, self.to_base32())
    }

    /// A short prefix for display.
    pub fn short(&self) -> String {
        self.to_base32().chars().take(8).collect()
    }
}

impl fmt::Display for DelegationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_base32())
    }
}

/// Runtime status of a delegation, combining local facts (expiry) with
/// whatever the server reports (revocation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    /// Usable right now.
    Active,
    /// Past its `expires` timestamp.
    Expired,
    /// Explicitly revoked by the delegator.
    Revoked,
}

impl fmt::Display for DelegationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DelegationStatus::Active => write!(f, "active"),
            DelegationStatus::Expired => write!(f, "expired"),
            DelegationStatus::Revoked => write!(f, "revoked"),
        }
    }
}

impl Delegation {
    /// Build a delegation from a delegator and delegate identity.
    ///
    /// The DIDs are derived from each identity's public key. Issue time is now;
    /// use [`Self::expires_in`] or [`Self::with_expiry`] to bound it.
    pub fn new(delegator: &Identity, delegate: &Identity, scope: DelegationScope) -> Self {
        let issued = Utc::now();
        Self {
            id: DelegationId::from_delegation_data(&delegator.id, &delegate.id, issued),
            delegator: delegator.id.to_did(),
            delegator_name: delegator.name.clone(),
            delegator_key: delegator.public_key.to_did_key(),
            delegate: delegate.id.to_did(),
            delegate_key: delegate.public_key.to_did_key(),
            delegate_name: delegate.name.clone(),
            software_agent: None,
            scope,
            issued,
            expires: None,
        }
    }

    /// Name the software agent this key belongs to.
    pub fn with_software_agent(mut self, agent: impl Into<String>) -> Self {
        self.software_agent = Some(agent.into());
        self
    }

    /// Set an explicit expiry.
    pub fn with_expiry(mut self, expires: DateTime<Utc>) -> Self {
        self.expires = Some(expires);
        self
    }

    /// Expire after a duration from the issue time.
    pub fn expires_in(mut self, duration: Duration) -> Self {
        self.expires = Some(self.issued + duration);
        self
    }

    /// Has this delegation passed its expiry?
    pub fn is_expired(&self) -> bool {
        self.expires.map(|exp| exp < Utc::now()).unwrap_or(false)
    }

    /// Status from purely local facts. Revocation is server state, so a caller
    /// that knows a revocation exists should report [`DelegationStatus::Revoked`]
    /// itself rather than asking this.
    pub fn status(&self) -> DelegationStatus {
        if self.is_expired() {
            DelegationStatus::Expired
        } else {
            DelegationStatus::Active
        }
    }

    /// Time remaining before expiry, or `None` if it never expires.
    pub fn time_remaining(&self) -> Option<Duration> {
        self.expires.map(|exp| exp - Utc::now())
    }

    /// Does the delegation authorize `permission` on `resource`?
    ///
    /// Checks expiry and scope. It does **not** check revocation (server state)
    /// or the delegator's own grants — a server must check both, and the
    /// effective answer is always the intersection.
    pub fn allows(&self, permission: DelegationPermission, resource: &ResourceRef<'_>) -> bool {
        if self.is_expired() {
            return false;
        }
        self.scope.allows(permission, resource)
    }

    /// Recompute the id from the body and compare against the carried one.
    ///
    /// The identity ids are recovered from the DIDs, which are fingerprints, so
    /// this needs the two [`IdentityId`]s rather than the DID strings.
    pub fn id_matches(&self, delegator_id: &IdentityId, delegate_id: &IdentityId) -> bool {
        self.id == DelegationId::from_delegation_data(delegator_id, delegate_id, self.issued)
    }
}

impl fmt::Display for Delegation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} -> {} ({})",
            self.delegator_name,
            self.delegate_name,
            self.status()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::IdentityType;

    fn create_test_identities() -> (Identity, Identity) {
        let user = Identity::generate("alice");
        let agent = Identity::builder("alice+claude")
            .identity_type(IdentityType::Agent)
            .delegated_by(user.id)
            .build()
            .unwrap();
        (user, agent)
    }

    #[test]
    fn test_delegation_permission_implies() {
        assert!(DelegationPermission::Full.implies(&DelegationPermission::Read));
        assert!(DelegationPermission::Full.implies(&DelegationPermission::Push));
        assert!(DelegationPermission::Admin.implies(&DelegationPermission::Read));
        assert!(!DelegationPermission::Admin.implies(&DelegationPermission::Push));
        assert!(DelegationPermission::Read.implies(&DelegationPermission::Read));
        assert!(!DelegationPermission::Read.implies(&DelegationPermission::Push));
    }

    #[test]
    fn test_permission_from_str_round_trip() {
        for p in DelegationPermission::standard_permissions() {
            let parsed: DelegationPermission = p.to_string().parse().unwrap();
            assert_eq!(&parsed, p);
        }
        // The pre-"view" spelling still parses, so old scripts keep working.
        assert_eq!(
            "manage_stacks".parse::<DelegationPermission>().unwrap(),
            DelegationPermission::ManageViews
        );
        assert!("teleport".parse::<DelegationPermission>().is_err());
    }

    #[test]
    fn test_scope_builder() {
        let scope = DelegationScope::builder()
            .permission(DelegationPermission::Read)
            .permission(DelegationPermission::Record)
            .server("https://atomic.storage")
            .project("acme/*")
            .view("main")
            .max_changes(100)
            .build();

        assert!(scope.has_permission(DelegationPermission::Read));
        assert!(scope.has_permission(DelegationPermission::Record));
        assert!(!scope.has_permission(DelegationPermission::Push));
        assert_eq!(scope.max_changes, Some(100));
    }

    #[test]
    fn test_scope_empty_dimension_is_unrestricted() {
        let scope = DelegationScope::builder()
            .permission(DelegationPermission::Push)
            .build();
        assert!(scope.allows_project("anything/at/all"));
        assert!(scope.allows_view("some-view"));
        assert!(scope.allows_server("https://elsewhere.example"));
    }

    #[test]
    fn test_scope_project_globs() {
        let scope = DelegationScope::builder()
            .permission(DelegationPermission::Push)
            .project("acme/*")
            .build();
        assert!(scope.allows_project("acme/api"));
        assert!(!scope.allows_project("other/api"));
    }

    /// An unbound scope is valid against every deployment. That is correct
    /// behaviour for the type — but it is why the CLI binds to the active
    /// server unless told otherwise, rather than leaving this empty.
    #[test]
    fn an_unbound_scope_is_valid_everywhere() {
        let scope = DelegationScope::builder()
            .permission(DelegationPermission::Push)
            .build();
        assert!(scope.servers.is_empty());
        assert!(scope.allows_server("https://atomic.storage"));
        assert!(scope.allows_server("https://staging.example"));
    }

    #[test]
    fn test_scope_server_is_exact_not_glob() {
        let scope = DelegationScope::builder()
            .permission(DelegationPermission::Push)
            .server("https://atomic.storage")
            .build();
        assert!(scope.allows_server("https://atomic.storage"));
        // Trailing slash and case are noise, not a different server.
        assert!(scope.allows_server("https://Atomic.Storage/"));
        // A wildcard must not smuggle in a different deployment.
        assert!(!scope.allows_server("https://staging.atomic.storage"));
        assert!(!scope.allows_server("https://evil.example"));
    }

    #[test]
    fn test_delegation_allows() {
        let (user, agent) = create_test_identities();
        let scope = DelegationScope::builder()
            .permission(DelegationPermission::Read)
            .permission(DelegationPermission::Push)
            .server("https://atomic.storage")
            .project("acme/*")
            .build();
        let delegation = Delegation::new(&user, &agent, scope).expires_in(Duration::days(30));

        let ok = ResourceRef::new()
            .server("https://atomic.storage")
            .project("acme/api");
        assert!(delegation.allows(DelegationPermission::Push, &ok));
        assert!(delegation.allows(DelegationPermission::Read, &ok));
        // Permission not granted.
        assert!(!delegation.allows(DelegationPermission::Admin, &ok));
        // Project out of scope.
        assert!(!delegation.allows(
            DelegationPermission::Push,
            &ResourceRef::new().project("other/api")
        ));
        // Right project, wrong server.
        assert!(!delegation.allows(
            DelegationPermission::Push,
            &ResourceRef::new()
                .server("https://staging.atomic.storage")
                .project("acme/api")
        ));
    }

    #[test]
    fn test_expired_delegation_allows_nothing() {
        let (user, agent) = create_test_identities();
        let delegation = Delegation::new(&user, &agent, DelegationScope::full())
            .with_expiry(Utc::now() - Duration::hours(1));

        assert!(delegation.is_expired());
        assert_eq!(delegation.status(), DelegationStatus::Expired);
        assert!(!delegation.allows(DelegationPermission::Read, &ResourceRef::new()));
    }

    #[test]
    fn test_delegation_id_deterministic_and_recomputable() {
        let (user, agent) = create_test_identities();
        let delegation = Delegation::new(&user, &agent, DelegationScope::read_only());

        assert!(delegation.id_matches(&user.id, &agent.id));
        // A different delegate yields a different id.
        let other = Identity::generate("mallory");
        assert!(!delegation.id_matches(&user.id, &other.id));
    }

    #[test]
    fn test_delegation_id_base32_round_trip() {
        let (user, agent) = create_test_identities();
        let delegation = Delegation::new(&user, &agent, DelegationScope::read_only());

        let urn = delegation.id.to_urn();
        assert!(urn.starts_with(DelegationId::URN_PREFIX));
        assert_eq!(DelegationId::from_base32(&urn), Some(delegation.id));
        assert_eq!(
            DelegationId::from_base32(&delegation.id.to_base32()),
            Some(delegation.id)
        );
        assert_eq!(DelegationId::from_base32("not base32!"), None);
    }

    #[test]
    fn test_delegate_key_is_recoverable_did_key() {
        let (user, agent) = create_test_identities();
        let delegation = Delegation::new(&user, &agent, DelegationScope::read_only());

        // did:atomic is a fingerprint; did:key carries the key itself.
        assert!(delegation.delegate.starts_with("did:atomic:"));
        assert!(delegation.delegate_key.starts_with("did:key:z6Mk"));
        assert!(delegation.delegator.starts_with("did:atomic:"));
        assert!(delegation.delegator_key.starts_with("did:key:z6Mk"));
    }

    #[test]
    fn test_scope_deserializes_legacy_field_names() {
        // Scopes written before the rename must still load.
        let legacy = r#"{
            "permissions": ["read"],
            "repository_patterns": ["acme/*"],
            "view_patterns": ["main"]
        }"#;
        let scope: DelegationScope = serde_json::from_str(legacy).unwrap();
        assert_eq!(scope.projects, vec!["acme/*".to_string()]);
        assert_eq!(scope.views, vec!["main".to_string()]);
        assert!(scope.servers.is_empty());
    }
}
