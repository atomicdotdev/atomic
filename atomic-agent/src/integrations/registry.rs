//! The curated, embedded integration registry.
//!
//! The registry is deliberately one boring TOML file compiled into the
//! binary (mason/aqua style): it maps an adapter name to the Atomic storage
//! project hosting its integration package. Bumping an integration's pin is
//! a one-line PR; there is no service to run.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

const REGISTRY_TOML: &str = include_str!("registry.toml");

fn default_view() -> String {
    "release".to_string()
}

/// Where an agent's integration package lives and what to pull.
#[derive(Debug, Clone, Deserialize)]
pub struct IntegrationSpec {
    /// Atomic storage remote URL for the package repository.
    pub url: String,
    /// View pulled when no tag is pinned.
    #[serde(default = "default_view")]
    pub view: String,
    /// Optional tag pinning the exact package state. When absent, the view's
    /// head is used (with a notice printed by the caller).
    #[serde(default)]
    pub tag: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegistryFile {
    #[allow(dead_code)]
    schema: u32,
    agents: HashMap<String, IntegrationSpec>,
}

static REGISTRY: OnceLock<RegistryFile> = OnceLock::new();

fn registry() -> &'static RegistryFile {
    REGISTRY.get_or_init(|| {
        toml::from_str(REGISTRY_TOML).expect("embedded integrations registry.toml must parse")
    })
}

/// Resolve an adapter name (e.g. `"opencode"`) to its integration package
/// location, if this agent is externally packaged.
pub fn resolve(agent: &str) -> Option<IntegrationSpec> {
    registry().agents.get(agent).cloned()
}

/// All adapter names that have an external integration package.
pub fn agents() -> Vec<String> {
    let mut names: Vec<String> = registry().agents.keys().cloned().collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_parses_and_covers_known_agents() {
        // Every externally-packaged agent with a repo in the atomicdotdev org.
        for name in [
            "agy",
            "claude-code",
            "cline",
            "codex",
            "copilot",
            "cursor",
            "devin",
            "kilo",
            "kiro",
            "opencode",
            "pi",
        ] {
            assert!(
                resolve(name).is_some(),
                "registry should have an entry for {name}"
            );
        }
    }

    #[test]
    fn entries_have_storage_urls_and_default_view() {
        for name in agents() {
            let spec = resolve(&name).unwrap();
            assert!(
                spec.url.starts_with("https://"),
                "{name}: url should be https, got {}",
                spec.url
            );
            assert!(
                spec.url.contains(".atomic.storage/"),
                "{name}: url should be an atomic.storage remote, got {}",
                spec.url
            );
            assert!(!spec.view.is_empty(), "{name}: view should not be empty");
        }
    }

    #[test]
    fn unknown_agent_resolves_to_none() {
        assert!(resolve("not-a-real-agent").is_none());
    }
}
