//! Delegation support for agent on-behalf-of operations
//!
//! This module provides structures and utilities for managing delegated
//! identities - AI agents or automated systems that act on behalf of
//! a human user.
//!
//! # Overview
//!
//! Delegation allows users to authorize agents to perform actions in their
//! name. This is essential for:
//!
//! - **AI Assistants**: Claude, Copilot, etc. making changes on user's behalf
//! - **CI/CD Systems**: Automated builds and deployments
//! - **Bots**: Automated maintenance, dependency updates
//!
//! # Delegation Model
//!
//! ```text
//! ┌─────────────────┐     delegates to     ┌─────────────────┐
//! │   User Identity │ ──────────────────▶ │  Agent Identity │
//! │   (delegator)   │                      │   (delegate)    │
//! └─────────────────┘                      └─────────────────┘
//!         │                                        │
//!         │ owns                                   │ has
//!         ▼                                        ▼
//! ┌─────────────────┐                      ┌─────────────────┐
//! │ Delegation      │◀─────────────────────│ DelegationScope │
//! │ Certificate     │      defines         │ (permissions)   │
//! └─────────────────┘                      └─────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_identity::{Identity, IdentityType};
//! use atomic_identity::delegation::{Delegation, DelegationScope, DelegationPermission};
//!
//! // Create a user identity
//! let user = Identity::generate("alice");
//!
//! // Create an agent identity
//! let agent = Identity::builder("alice-assistant")
//!     .identity_type(IdentityType::Agent)
//!     .delegated_by(user.id)
//!     .build()?;
//!
//! // Create a delegation with specific scope
//! let scope = DelegationScope::builder()
//!     .permission(DelegationPermission::Record)
//!     .permission(DelegationPermission::Push)
//!     .repository_pattern("alice/*")
//!     .build();
//!
//! let delegation = Delegation::new(&user, &agent, scope)?;
//! # Ok::<(), atomic_identity::IdentityError>(())
//! ```

use crate::identity::{Identity, IdentityId};
use crate::signing::Signature;
use crate::IdentityError;
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

    /// Permission to create/delete stacks.
    ManageStacks,

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
            DelegationPermission::ManageStacks => "Create and delete stacks",
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
                    | DelegationPermission::ManageStacks
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
            DelegationPermission::ManageStacks,
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
            DelegationPermission::ManageStacks => write!(f, "manage_stacks"),
            DelegationPermission::ManageTags => write!(f, "manage_tags"),
            DelegationPermission::Admin => write!(f, "admin"),
            DelegationPermission::Full => write!(f, "full"),
        }
    }
}

/// The scope of a delegation, defining what the delegate can do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationScope {
    /// Permissions granted to the delegate.
    pub permissions: Vec<DelegationPermission>,

    /// Repository patterns the delegation applies to (glob patterns).
    ///
    /// Empty means all repositories.
    #[serde(default)]
    pub repository_patterns: Vec<String>,

    /// View patterns the delegation applies to (glob patterns).
    ///
    /// Empty means all views.
    #[serde(default, alias = "stack_patterns")]
    pub view_patterns: Vec<String>,

    /// Maximum number of changes the delegate can create.
    #[serde(default)]
    pub max_changes: Option<u64>,

    /// Human-readable description of the scope.
    #[serde(default)]
    pub description: Option<String>,
}

impl Default for DelegationScope {
    fn default() -> Self {
        Self {
            permissions: vec![DelegationPermission::Read],
            repository_patterns: Vec::new(),
            view_patterns: Vec::new(),
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
            repository_patterns: Vec::new(),
            view_patterns: Vec::new(),
            max_changes: None,
            description: Some("Full access".to_string()),
        }
    }

    /// Create a scope for read-only access.
    pub fn read_only() -> Self {
        Self {
            permissions: vec![DelegationPermission::Read],
            repository_patterns: Vec::new(),
            view_patterns: Vec::new(),
            max_changes: None,
            description: Some("Read-only access".to_string()),
        }
    }

    /// Create a scope for typical CI/CD operations.
    pub fn ci_cd() -> Self {
        Self {
            permissions: vec![
                DelegationPermission::Read,
                DelegationPermission::Record,
                DelegationPermission::Push,
                DelegationPermission::Pull,
            ],
            repository_patterns: Vec::new(),
            view_patterns: Vec::new(),
            max_changes: None,
            description: Some("CI/CD operations".to_string()),
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

    /// Check if this scope allows access to a repository.
    pub fn allows_repository(&self, repo_path: &str) -> bool {
        if self.repository_patterns.is_empty() {
            return true;
        }

        self.repository_patterns
            .iter()
            .any(|pattern| Self::matches_pattern(pattern, repo_path))
    }

    /// Check if this scope allows access to a view.
    pub fn allows_view(&self, view_name: &str) -> bool {
        if self.view_patterns.is_empty() {
            return true;
        }

        self.view_patterns
            .iter()
            .any(|pattern| Self::matches_pattern(pattern, view_name))
    }

    /// Simple glob pattern matching (supports * and ?).
    fn matches_pattern(pattern: &str, value: &str) -> bool {
        let pattern_chars: Vec<char> = pattern.chars().collect();
        let value_chars: Vec<char> = value.chars().collect();
        Self::matches_pattern_recursive(&pattern_chars, &value_chars)
    }

    fn matches_pattern_recursive(pattern: &[char], value: &[char]) -> bool {
        match (pattern.first(), value.first()) {
            (None, None) => true,
            (Some('*'), _) => {
                // Try matching zero or more characters
                Self::matches_pattern_recursive(&pattern[1..], value)
                    || (!value.is_empty() && Self::matches_pattern_recursive(pattern, &value[1..]))
            }
            (Some('?'), Some(_)) => Self::matches_pattern_recursive(&pattern[1..], &value[1..]),
            (Some(p), Some(v)) if p == v => {
                Self::matches_pattern_recursive(&pattern[1..], &value[1..])
            }
            _ => false,
        }
    }
}

/// Builder for creating delegation scopes.
pub struct DelegationScopeBuilder {
    permissions: Vec<DelegationPermission>,
    repository_patterns: Vec<String>,
    view_patterns: Vec<String>,
    max_changes: Option<u64>,
    description: Option<String>,
}

impl DelegationScopeBuilder {
    /// Create a new scope builder.
    pub fn new() -> Self {
        Self {
            permissions: Vec::new(),
            repository_patterns: Vec::new(),
            view_patterns: Vec::new(),
            max_changes: None,
            description: None,
        }
    }

    /// Add a permission.
    pub fn permission(mut self, permission: DelegationPermission) -> Self {
        if !self.permissions.contains(&permission) {
            self.permissions.push(permission);
        }
        self
    }

    /// Add multiple permissions.
    pub fn permissions(
        mut self,
        permissions: impl IntoIterator<Item = DelegationPermission>,
    ) -> Self {
        for p in permissions {
            if !self.permissions.contains(&p) {
                self.permissions.push(p);
            }
        }
        self
    }

    /// Add a repository pattern.
    pub fn repository_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.repository_patterns.push(pattern.into());
        self
    }

    /// Add a view pattern.
    pub fn view_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.view_patterns.push(pattern.into());
        self
    }

    /// Set maximum number of changes.
    pub fn max_changes(mut self, max: u64) -> Self {
        self.max_changes = Some(max);
        self
    }

    /// Set a description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Build the delegation scope.
    pub fn build(mut self) -> DelegationScope {
        // Ensure at least read permission
        if self.permissions.is_empty() {
            self.permissions.push(DelegationPermission::Read);
        }

        DelegationScope {
            permissions: self.permissions,
            repository_patterns: self.repository_patterns,
            view_patterns: self.view_patterns,
            max_changes: self.max_changes,
            description: self.description,
        }
    }
}

impl Default for DelegationScopeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A delegation certificate authorizing an agent to act on behalf of a user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegation {
    /// Unique identifier for this delegation.
    pub id: DelegationId,

    /// The delegator's identity ID (the user granting permission).
    pub delegator_id: IdentityId,

    /// The delegator's name (for display).
    pub delegator_name: String,

    /// The delegate's identity ID (the agent receiving permission).
    pub delegate_id: IdentityId,

    /// The delegate's name (for display).
    pub delegate_name: String,

    /// The scope of the delegation.
    pub scope: DelegationScope,

    /// When the delegation was created.
    pub created_at: DateTime<Utc>,

    /// When the delegation expires (if set).
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,

    /// Whether the delegation has been revoked.
    #[serde(default)]
    pub revoked: bool,

    /// When the delegation was revoked (if applicable).
    #[serde(default)]
    pub revoked_at: Option<DateTime<Utc>>,

    /// Signature from the delegator proving authenticity.
    ///
    /// This is the delegator's signature over the delegation data.
    #[serde(default)]
    pub signature: Option<Signature>,
}

/// Unique identifier for a delegation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DelegationId([u8; 32]);

impl DelegationId {
    /// Create a delegation ID from the delegation data.
    pub fn from_delegation_data(
        delegator_id: &IdentityId,
        delegate_id: &IdentityId,
        created_at: DateTime<Utc>,
    ) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(delegator_id.as_bytes());
        hasher.update(delegate_id.as_bytes());
        hasher.update(&created_at.timestamp().to_le_bytes());
        DelegationId(*hasher.finalize().as_bytes())
    }

    /// Create a delegation ID from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        DelegationId(bytes)
    }

    /// Get the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Encode as base32.
    pub fn to_base32(&self) -> String {
        data_encoding::BASE32_NOPAD.encode(&self.0)
    }

    /// Get a short form for display.
    pub fn short(&self) -> String {
        self.to_base32()[..8].to_string()
    }
}

impl fmt::Debug for DelegationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DelegationId({})", self.short())
    }
}

impl fmt::Display for DelegationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_base32())
    }
}

impl Delegation {
    /// Create a new delegation from a delegator to a delegate.
    pub fn new(
        delegator: &Identity,
        delegate: &Identity,
        scope: DelegationScope,
    ) -> Result<Self, IdentityError> {
        let created_at = Utc::now();
        let id = DelegationId::from_delegation_data(&delegator.id, &delegate.id, created_at);

        Ok(Self {
            id,
            delegator_id: delegator.id,
            delegator_name: delegator.name.clone(),
            delegate_id: delegate.id,
            delegate_name: delegate.name.clone(),
            scope,
            created_at,
            expires_at: None,
            revoked: false,
            revoked_at: None,
            signature: None,
        })
    }

    /// Create a delegation with an expiration time.
    pub fn with_expiry(mut self, expires_at: DateTime<Utc>) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Create a delegation that expires after a duration.
    pub fn expires_in(mut self, duration: Duration) -> Self {
        self.expires_at = Some(Utc::now() + duration);
        self
    }

    /// Check if the delegation is currently valid.
    pub fn is_valid(&self) -> bool {
        !self.revoked && !self.is_expired()
    }

    /// Check if the delegation has expired.
    pub fn is_expired(&self) -> bool {
        self.expires_at.map(|exp| exp < Utc::now()).unwrap_or(false)
    }

    /// Revoke the delegation.
    pub fn revoke(&mut self) {
        self.revoked = true;
        self.revoked_at = Some(Utc::now());
    }

    /// Check if an operation is allowed by this delegation.
    pub fn allows(
        &self,
        permission: DelegationPermission,
        repository: Option<&str>,
        view: Option<&str>,
    ) -> bool {
        if !self.is_valid() {
            return false;
        }

        if !self.scope.has_permission(permission) {
            return false;
        }

        if let Some(repo) = repository {
            if !self.scope.allows_repository(repo) {
                return false;
            }
        }

        if let Some(view_name) = view {
            if !self.scope.allows_view(view_name) {
                return false;
            }
        }

        true
    }

    /// Get the data to be signed for this delegation.
    pub fn signing_data(&self) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(self.delegator_id.as_bytes());
        data.extend_from_slice(self.delegate_id.as_bytes());
        data.extend_from_slice(&self.created_at.timestamp().to_le_bytes());
        if let Some(exp) = self.expires_at {
            data.extend_from_slice(&exp.timestamp().to_le_bytes());
        }
        data
    }
}

impl fmt::Display for Delegation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} -> {} ({})",
            self.delegator_name,
            self.delegate_name,
            if self.is_valid() { "valid" } else { "invalid" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::IdentityType;

    fn create_test_identities() -> (Identity, Identity) {
        let user = Identity::generate("alice");
        let agent = Identity::builder("alice-assistant")
            .identity_type(IdentityType::Agent)
            .delegated_by(user.id)
            .build()
            .unwrap();
        (user, agent)
    }

    #[test]
    fn test_delegation_permission_implies() {
        assert!(DelegationPermission::Full.implies(&DelegationPermission::Read));
        assert!(DelegationPermission::Full.implies(&DelegationPermission::Record));
        assert!(DelegationPermission::Full.implies(&DelegationPermission::Full));

        assert!(!DelegationPermission::Read.implies(&DelegationPermission::Record));
        assert!(DelegationPermission::Read.implies(&DelegationPermission::Read));
    }

    #[test]
    fn test_delegation_scope_has_permission() {
        let scope = DelegationScope::builder()
            .permission(DelegationPermission::Read)
            .permission(DelegationPermission::Record)
            .build();

        assert!(scope.has_permission(DelegationPermission::Read));
        assert!(scope.has_permission(DelegationPermission::Record));
        assert!(!scope.has_permission(DelegationPermission::Push));
    }

    #[test]
    fn test_delegation_scope_full() {
        let scope = DelegationScope::full();

        assert!(scope.has_permission(DelegationPermission::Read));
        assert!(scope.has_permission(DelegationPermission::Record));
        assert!(scope.has_permission(DelegationPermission::Push));
        assert!(scope.has_permission(DelegationPermission::Admin));
    }

    #[test]
    fn test_delegation_scope_repository_patterns() {
        let scope = DelegationScope::builder()
            .permission(DelegationPermission::Read)
            .repository_pattern("alice/*")
            .repository_pattern("shared/*")
            .build();

        assert!(scope.allows_repository("alice/project"));
        assert!(scope.allows_repository("alice/another"));
        assert!(scope.allows_repository("shared/common"));
        assert!(!scope.allows_repository("bob/project"));
    }

    #[test]
    fn test_delegation_scope_pattern_matching() {
        // Test wildcard matching
        assert!(DelegationScope::matches_pattern("*", "anything"));
        assert!(DelegationScope::matches_pattern("prefix*", "prefix-suffix"));
        assert!(DelegationScope::matches_pattern("*suffix", "prefix-suffix"));
        assert!(DelegationScope::matches_pattern("pre*fix", "prefix"));

        // Test question mark
        assert!(DelegationScope::matches_pattern("te?t", "test"));
        assert!(DelegationScope::matches_pattern("te?t", "text"));
        assert!(!DelegationScope::matches_pattern("te?t", "toast"));

        // Test exact match
        assert!(DelegationScope::matches_pattern("exact", "exact"));
        assert!(!DelegationScope::matches_pattern("exact", "different"));
    }

    #[test]
    fn test_delegation_new() {
        let (user, agent) = create_test_identities();
        let scope = DelegationScope::read_only();

        let delegation = Delegation::new(&user, &agent, scope).unwrap();

        assert_eq!(delegation.delegator_id, user.id);
        assert_eq!(delegation.delegate_id, agent.id);
        assert!(delegation.is_valid());
    }

    #[test]
    fn test_delegation_expiry() {
        let (user, agent) = create_test_identities();
        let scope = DelegationScope::read_only();

        let delegation = Delegation::new(&user, &agent, scope)
            .unwrap()
            .expires_in(Duration::hours(1));

        assert!(delegation.is_valid());
        assert!(!delegation.is_expired());
    }

    #[test]
    fn test_delegation_revoke() {
        let (user, agent) = create_test_identities();
        let scope = DelegationScope::read_only();

        let mut delegation = Delegation::new(&user, &agent, scope).unwrap();
        assert!(delegation.is_valid());

        delegation.revoke();
        assert!(!delegation.is_valid());
        assert!(delegation.revoked);
        assert!(delegation.revoked_at.is_some());
    }

    #[test]
    fn test_delegation_allows() {
        let (user, agent) = create_test_identities();
        let scope = DelegationScope::builder()
            .permission(DelegationPermission::Read)
            .permission(DelegationPermission::Record)
            .repository_pattern("alice/*")
            .build();

        let delegation = Delegation::new(&user, &agent, scope).unwrap();

        // Allowed operations
        assert!(delegation.allows(DelegationPermission::Read, Some("alice/project"), None));
        assert!(delegation.allows(DelegationPermission::Record, Some("alice/project"), None));

        // Not allowed (wrong permission)
        assert!(!delegation.allows(DelegationPermission::Push, Some("alice/project"), None));

        // Not allowed (wrong repository)
        assert!(!delegation.allows(DelegationPermission::Read, Some("bob/project"), None));
    }

    #[test]
    fn test_delegation_id_deterministic() {
        let (user, agent) = create_test_identities();
        let created_at = Utc::now();

        let id1 = DelegationId::from_delegation_data(&user.id, &agent.id, created_at);
        let id2 = DelegationId::from_delegation_data(&user.id, &agent.id, created_at);

        assert_eq!(id1, id2);
    }

    #[test]
    fn test_delegation_json_roundtrip() {
        let (user, agent) = create_test_identities();
        let scope = DelegationScope::ci_cd();
        let delegation = Delegation::new(&user, &agent, scope).unwrap();

        let json = serde_json::to_string(&delegation).unwrap();
        let recovered: Delegation = serde_json::from_str(&json).unwrap();

        assert_eq!(delegation.id, recovered.id);
        assert_eq!(delegation.delegator_id, recovered.delegator_id);
        assert_eq!(delegation.delegate_id, recovered.delegate_id);
    }

    #[test]
    fn test_delegation_scope_builder() {
        let scope = DelegationScope::builder()
            .permission(DelegationPermission::Read)
            .permission(DelegationPermission::Record)
            .repository_pattern("project/*")
            .view_pattern("main")
            .view_pattern("feature-*")
            .max_changes(100)
            .description("Limited access for testing")
            .build();

        assert!(scope.has_permission(DelegationPermission::Read));
        assert!(scope.has_permission(DelegationPermission::Record));
        assert_eq!(scope.repository_patterns.len(), 1);
        assert_eq!(scope.view_patterns.len(), 2);
        assert_eq!(scope.max_changes, Some(100));
        assert!(scope.description.is_some());
    }

    #[test]
    fn test_delegation_ci_cd_scope() {
        let scope = DelegationScope::ci_cd();

        assert!(scope.has_permission(DelegationPermission::Read));
        assert!(scope.has_permission(DelegationPermission::Record));
        assert!(scope.has_permission(DelegationPermission::Push));
        assert!(scope.has_permission(DelegationPermission::Pull));
        assert!(!scope.has_permission(DelegationPermission::Admin));
    }
}
