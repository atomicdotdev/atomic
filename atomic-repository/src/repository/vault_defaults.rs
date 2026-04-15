//! Default vault content installed on `init_vault()`.
//!
//! Content lives as raw markdown files in `atomic-repository/vault/`.
//! These are compiled into the binary via `include_str!` and copied
//! to the project's `.vault/` directory on initialization.
//!
//! To edit the defaults, edit the markdown files directly:
//!   - vault/prompts/system_prompt.md
//!   - vault/skills/atomic-vault.md
//!   - vault/skills/code-intelligence.md
//!   - vault/memory/MEMORY.md

use super::*;
use atomic_core::pristine::VaultEntryType;

/// Default files to install into `.vault/`.
///
/// Each entry is (vault_relative_path, entry_type, content, frontmatter_json).
const VAULT_DEFAULTS: &[(&str, VaultEntryType, &str, &str)] = &[
    (
        "prompts/system_prompt.md",
        VaultEntryType::Skill,
        include_str!("../../vault/prompts/system_prompt.md"),
        r#"{"name":"System Prompt","description":"Default system prompt for the AI coding agent"}"#,
    ),
    (
        "skills/atomic-vault.md",
        VaultEntryType::Skill,
        include_str!("../../vault/skills/atomic-vault.md"),
        r#"{"name":"Atomic Vault","description":"How to use the project vault for goals, intents, and shared memory"}"#,
    ),
    (
        "skills/code-intelligence.md",
        VaultEntryType::Skill,
        include_str!("../../vault/skills/code-intelligence.md"),
        r#"{"name":"Code Intelligence","description":"Use the knowledge graph to understand code structure, not just text matches"}"#,
    ),
    (
        "memory/MEMORY.md",
        VaultEntryType::Memory,
        include_str!("../../vault/memory/MEMORY.md"),
        r#"{"name":"MEMORY","type":"index"}"#,
    ),
];

impl Repository {
    /// Install default vault content (prompts, skills, memory index).
    ///
    /// Called automatically by `init_vault()`. Idempotent — skips entries
    /// that already exist.
    pub fn vault_install_defaults(&self) -> Result<(), RepositoryError> {
        for (path, entry_type, content, frontmatter) in VAULT_DEFAULTS {
            if self.vault_retrieve(path)?.is_none() {
                self.vault_store(
                    path,
                    *entry_type,
                    content.as_bytes().to_vec(),
                    frontmatter.to_string(),
                )?;
                self.vault_materialize(path)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_vault_install_defaults() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // All defaults should be installed
        for (path, _, _, _) in VAULT_DEFAULTS {
            let entry = repo.vault_retrieve(path).unwrap();
            assert!(entry.is_some(), "Default not installed: {}", path);

            // File should exist on disk
            assert!(
                repo.vault_dir().join(path).exists(),
                "Default not materialized: {}",
                path
            );
        }
    }

    #[test]
    fn test_vault_install_defaults_idempotent() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Call again — should not fail or duplicate
        repo.vault_install_defaults().unwrap();

        let skills = repo.vault_list("skills/", None).unwrap();
        assert_eq!(skills.len(), 2, "Should have exactly 2 skills");
    }

    #[test]
    fn test_vault_defaults_content_valid() {
        // Verify the included content is non-empty and well-formed
        for (path, _, content, _) in VAULT_DEFAULTS {
            assert!(!content.trim().is_empty(), "Empty content: {}", path);
            if path.ends_with(".md") {
                assert!(
                    content.contains('#'),
                    "Markdown file has no headings: {}",
                    path
                );
            }
        }
    }

    #[test]
    fn test_vault_default_count() {
        assert_eq!(VAULT_DEFAULTS.len(), 4, "Expected 4 default files");
    }
}
