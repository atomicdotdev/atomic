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

use super::vault::{parse_vault_frontmatter_at, render_entry_to_markdown};
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
    /// that already exist in the database. Never overwrites user work:
    ///
    /// - a default path that already exists on disk is hydrated into the
    ///   database from its disk bytes (a colocated repository tracks
    ///   `.vault/` in Git, or the file was restored from elsewhere);
    /// - a default path that exists but cannot be read (permissions,
    ///   invalid UTF-8, ...) fails closed instead of being replaced by the
    ///   template;
    /// - a path absent from both the database and the disk is created
    ///   under an exclusive-create lease, so a concurrent creator's bytes
    ///   can never be overwritten by the template.
    pub fn vault_install_defaults(&self) -> Result<(), RepositoryError> {
        for (path, entry_type, content, frontmatter) in VAULT_DEFAULTS {
            if self.vault_retrieve(path)?.is_some() {
                continue;
            }
            let disk_path = self.vault_dir().join(path);
            match std::fs::read_to_string(&disk_path) {
                Ok(file_content) => {
                    let (file_frontmatter, body) =
                        parse_vault_frontmatter_at(Some(path), &file_content);
                    self.vault_store(
                        path,
                        *entry_type,
                        body.as_bytes().to_vec(),
                        file_frontmatter,
                    )?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.create_default_exclusively(
                        path,
                        *entry_type,
                        content,
                        frontmatter,
                        &disk_path,
                    )?;
                }
                Err(error) => {
                    // The path exists but cannot be read. Overwriting it
                    // with the template would destroy user work; refuse
                    // closed with an actionable message instead.
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "vault path '{}' exists but cannot be read ({error}); refusing to \
                             replace it with the default template — resolve the file and re-run",
                            disk_path.display()
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Create one default file under an exclusive-create lease.
    ///
    /// The database entry is stored first, then the rendered markdown is
    /// written through a `create_new` handle: if a concurrent writer won
    /// the race, their bytes are hydrated into the database instead of
    /// being overwritten by the template.
    fn create_default_exclusively(
        &self,
        path: &str,
        entry_type: VaultEntryType,
        content: &str,
        frontmatter: &str,
        disk_path: &std::path::Path,
    ) -> Result<(), RepositoryError> {
        self.vault_store(
            path,
            entry_type,
            content.as_bytes().to_vec(),
            frontmatter.to_string(),
        )?;
        if let Some(parent) = disk_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let entry = self
            .vault_retrieve(path)?
            .ok_or_else(|| RepositoryError::FileNotFound {
                path: disk_path.to_path_buf(),
            })?;
        let markdown = render_entry_to_markdown(&entry);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(disk_path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(markdown.as_bytes())?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A concurrent creator won: their bytes are the user work.
                let file_content = std::fs::read_to_string(disk_path).map_err(|error| {
                    RepositoryError::InvalidOperation {
                        message: format!(
                            "vault path '{}' was created concurrently but cannot be read \
                             ({error}); refusing to replace it with the default template",
                            disk_path.display()
                        ),
                    }
                })?;
                let (file_frontmatter, body) =
                    parse_vault_frontmatter_at(Some(path), &file_content);
                self.vault_store(path, entry_type, body.as_bytes().to_vec(), file_frontmatter)?;
                Ok(())
            }
            Err(error) => Err(RepositoryError::InvalidOperation {
                message: format!(
                    "cannot create the default vault file '{}': {error}",
                    disk_path.display()
                ),
            }),
        }
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

    /// A pre-existing on-disk vault is user work: `init_vault` must hydrate
    /// the database from the disk bytes and never overwrite the files with
    /// the templates (the colocated Git-tracked `.vault/` regression).
    #[test]
    fn init_vault_preserves_existing_disk_content() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        let vault = dir.path().join(".vault");
        for (path, _, _, _) in VAULT_DEFAULTS {
            let target = vault.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, format!("CUSTOM {path} marker 6f4c1a\n")).unwrap();
        }

        repo.init_vault().unwrap();

        for (path, _, _, _) in VAULT_DEFAULTS {
            let bytes = std::fs::read(vault.join(path)).unwrap();
            assert_eq!(
                bytes,
                format!("CUSTOM {path} marker 6f4c1a\n").into_bytes(),
                "user bytes at {path} must survive init_vault"
            );
            let entry = repo.vault_retrieve(path).unwrap().unwrap();
            assert_eq!(
                entry.content_bytes,
                format!("CUSTOM {path} marker 6f4c1a\n").into_bytes(),
                "database must hold the user bytes for {path}"
            );
        }
    }

    /// Re-initializing an initialized vault does not reset templates or
    /// memories, even when the on-disk files differ from the templates.
    #[test]
    fn reinit_vault_keeps_disk_and_database_content() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        repo.init_vault().unwrap();
        let memory = repo.vault_dir().join("memory/MEMORY.md");
        std::fs::write(&memory, "# rewritten by the user\n").unwrap();

        repo.init_vault().unwrap();

        assert_eq!(
            std::fs::read(&memory).unwrap(),
            b"# rewritten by the user\n",
            "re-init must not restore the template over user edits"
        );
        let entry = repo.vault_retrieve("memory/MEMORY.md").unwrap().unwrap();
        assert_eq!(entry.content_bytes, b"# rewritten by the user\n");
    }

    /// An unreadable file at a default path (invalid UTF-8 is a read
    /// failure) must fail closed, never fall through to the template —
    /// overwriting would destroy the user's bytes.
    #[test]
    fn install_defaults_refuses_to_overwrite_invalid_utf8() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        let memory = repo.vault_dir().join("memory/MEMORY.md");
        std::fs::create_dir_all(memory.parent().unwrap()).unwrap();
        let invalid_utf8: [u8; 4] = [0xff, 0xfe, 0x00, 0x01];
        std::fs::write(&memory, invalid_utf8).unwrap();

        // The install path itself refuses closed with an actionable message.
        let error = repo
            .vault_install_defaults()
            .expect_err("vault_install_defaults must fail closed");
        assert!(
            error.to_string().contains("refusing to"),
            "the refusal must be actionable, got: {error}"
        );
        assert_eq!(
            std::fs::read(&memory).unwrap(),
            invalid_utf8,
            "invalid-UTF-8 user bytes must survive byte-for-byte"
        );

        // Full initialization also fails closed (the hydration scan reads
        // the same file) without ever touching the bytes.
        assert!(repo.init_vault().is_err());
        assert_eq!(
            std::fs::read(&memory).unwrap(),
            invalid_utf8,
            "the user's bytes must still be intact after the failed init"
        );
    }
}
