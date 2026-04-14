//! Vault identity resolution.
//!
//! Resolves the current identity for vault operations. If an identity
//! store is configured, uses the default identity. Otherwise falls back
//! to environment variables or system username.

use super::*;

/// Resolved identity for vault provenance.
#[derive(Debug, Clone)]
pub struct VaultIdentity {
    /// Display name (e.g., "alice", "pi-agent")
    pub name: String,
    /// Email if available
    pub email: Option<String>,
    /// Whether this is an agent (vs human)
    pub is_agent: bool,
    /// Public key fingerprint (if identity store is available)
    pub fingerprint: Option<String>,
}

impl VaultIdentity {
    /// Format as a provenance string for frontmatter.
    pub fn to_provenance_string(&self) -> String {
        let prefix = if self.is_agent { "🤖" } else { "👤" };
        match &self.email {
            Some(email) => format!("{} {} <{}>", prefix, self.name, email),
            None => format!("{} {}", prefix, self.name),
        }
    }

    /// Format as a simple identity string for KG nodes.
    pub fn to_identity_id(&self) -> String {
        format!("identity:{}", self.name)
    }
}

impl std::fmt::Display for VaultIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.email {
            Some(email) => write!(f, "{} <{}>", self.name, email),
            None => write!(f, "{}", self.name),
        }
    }
}

impl Repository {
    /// Resolve the current identity for vault operations.
    ///
    /// Resolution order:
    /// 1. Atomic identity store (default identity)
    /// 2. Git config (user.name + user.email)
    /// 3. Environment variables (USER / USERNAME)
    /// 4. Fallback: "unknown"
    pub fn resolve_vault_identity(&self) -> VaultIdentity {
        // Try atomic identity store
        if let Ok(store) = atomic_identity::IdentityStore::open_default() {
            if let Ok(Some(identity)) = store.get_default() {
                return VaultIdentity {
                    name: identity.name.clone(),
                    email: identity.email.clone(),
                    is_agent: identity.identity_type.is_agent()
                        || identity.identity_type.is_delegated(),
                    fingerprint: Some(identity.public_key_base32()),
                };
            }
        }

        // Try git config
        if let Some(git_identity) = resolve_git_identity(self.root()) {
            return git_identity;
        }

        // Try environment
        let username = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".to_string());

        VaultIdentity {
            name: username,
            email: None,
            is_agent: false,
            fingerprint: None,
        }
    }
}

/// Try to resolve identity from git config in the repository.
fn resolve_git_identity(repo_root: &std::path::Path) -> Option<VaultIdentity> {
    let git_dir = repo_root.join(".git");
    if !git_dir.exists() {
        return None;
    }

    let mut name = None;
    let mut email = None;

    // Try local .git/config first
    let config_path = git_dir.join("config");
    if let Ok(content) = std::fs::read_to_string(&config_path) {
        parse_git_config_user_section(&content, &mut name, &mut email);
    }

    // Fall back to global ~/.gitconfig for any missing fields
    if name.is_none() || email.is_none() {
        if let Some(home) = dirs::home_dir() {
            let global_config = home.join(".gitconfig");
            if let Ok(content) = std::fs::read_to_string(&global_config) {
                parse_git_config_user_section(&content, &mut name, &mut email);
            }
        }
    }

    name.map(|n| VaultIdentity {
        name: n,
        email,
        is_agent: false,
        fingerprint: None,
    })
}

/// Parse a git config file's `[user]` section for `name` and `email`.
///
/// Only fills in values that are currently `None`, so local config
/// values take precedence over global ones when called in order.
fn parse_git_config_user_section(
    content: &str,
    name: &mut Option<String>,
    email: &mut Option<String>,
) {
    let mut in_user_section = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[user]" {
            in_user_section = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_user_section = false;
            continue;
        }
        if in_user_section {
            if name.is_none() {
                if let Some(val) = trimmed.strip_prefix("name = ") {
                    *name = Some(val.trim().to_string());
                }
            }
            if email.is_none() {
                if let Some(val) = trimmed.strip_prefix("email = ") {
                    *email = Some(val.trim().to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_vault_identity_display() {
        let id = VaultIdentity {
            name: "alice".to_string(),
            email: Some("alice@example.com".to_string()),
            is_agent: false,
            fingerprint: None,
        };
        assert_eq!(id.to_string(), "alice <alice@example.com>");
        assert!(id.to_provenance_string().contains("👤"));
        assert_eq!(id.to_identity_id(), "identity:alice");
    }

    #[test]
    fn test_vault_identity_display_no_email() {
        let id = VaultIdentity {
            name: "bob".to_string(),
            email: None,
            is_agent: false,
            fingerprint: None,
        };
        assert_eq!(id.to_string(), "bob");
    }

    #[test]
    fn test_vault_identity_agent() {
        let id = VaultIdentity {
            name: "pi-agent".to_string(),
            email: None,
            is_agent: true,
            fingerprint: None,
        };
        assert_eq!(id.to_string(), "pi-agent");
        assert!(id.to_provenance_string().contains("🤖"));
    }

    #[test]
    fn test_vault_identity_provenance_with_email() {
        let id = VaultIdentity {
            name: "ci-bot".to_string(),
            email: Some("ci@example.com".to_string()),
            is_agent: true,
            fingerprint: Some("ABCDEF".to_string()),
        };
        assert_eq!(id.to_provenance_string(), "🤖 ci-bot <ci@example.com>");
    }

    #[test]
    fn test_resolve_vault_identity_fallback() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let identity = repo.resolve_vault_identity();
        // Should at least have a name (from env or fallback)
        assert!(!identity.name.is_empty());
    }

    #[test]
    fn test_parse_git_config_user_section() {
        let config = r#"
[core]
    repositoryformatversion = 0
[user]
    name = Alice Smith
    email = alice@example.com
[remote "origin"]
    url = https://example.com/repo.git
"#;
        let mut name = None;
        let mut email = None;
        parse_git_config_user_section(config, &mut name, &mut email);
        assert_eq!(name.as_deref(), Some("Alice Smith"));
        assert_eq!(email.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn test_parse_git_config_no_user_section() {
        let config = r#"
[core]
    repositoryformatversion = 0
"#;
        let mut name = None;
        let mut email = None;
        parse_git_config_user_section(config, &mut name, &mut email);
        assert!(name.is_none());
        assert!(email.is_none());
    }

    #[test]
    fn test_parse_git_config_does_not_overwrite() {
        let config = r#"
[user]
    name = Global Name
    email = global@example.com
"#;
        let mut name = Some("Local Name".to_string());
        let mut email = None;
        parse_git_config_user_section(config, &mut name, &mut email);
        // name should NOT be overwritten
        assert_eq!(name.as_deref(), Some("Local Name"));
        // email should be filled in
        assert_eq!(email.as_deref(), Some("global@example.com"));
    }

    #[test]
    fn test_resolve_git_identity_local_config() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(
            git_dir.join("config"),
            "[user]\n\tname = Test User\n\temail = test@example.com\n",
        )
        .unwrap();

        let identity = resolve_git_identity(dir.path());
        assert!(identity.is_some());
        let identity = identity.unwrap();
        assert_eq!(identity.name, "Test User");
        assert_eq!(identity.email.as_deref(), Some("test@example.com"));
        assert!(!identity.is_agent);
    }

    #[test]
    fn test_resolve_git_identity_no_git_dir() {
        let dir = tempdir().unwrap();
        let identity = resolve_git_identity(dir.path());
        assert!(identity.is_none());
    }

    #[test]
    fn test_identity_id_format() {
        let id = VaultIdentity {
            name: "alice".to_string(),
            email: None,
            is_agent: false,
            fingerprint: None,
        };
        assert_eq!(id.to_identity_id(), "identity:alice");

        let agent = VaultIdentity {
            name: "claude-agent".to_string(),
            email: None,
            is_agent: true,
            fingerprint: Some("FINGERPRINT123".to_string()),
        };
        assert_eq!(agent.to_identity_id(), "identity:claude-agent");
    }
}
