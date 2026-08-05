//! `atomic vault show <path>` — print a vault entry's content.

use clap::Parser;

use atomic_core::pristine::vault::VaultEntry;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

use super::vault_entry_revision_hash;

/// Print a vault entry's content.
///
/// Retrieves and displays the content of a single vault entry by its
/// vault-relative path. By default, outputs the raw markdown content.
/// Use `--json` for structured output including frontmatter and metadata.
///
/// # Examples
///
/// ```text
/// # Show a memory file
/// atomic vault show memory/architecture.md
///
/// # Show a goal file as JSON
/// atomic vault show goals/swift-meadow-a3f2/_goal.md --json
///
/// # Pull a candidate only if it is still the retrieved revision
/// atomic vault show memory/architecture.md --revision ABC123 --json
/// ```
#[derive(Parser, Debug)]
#[command(name = "show")]
pub struct Show {
    /// Vault-relative path to the entry (e.g., "memory/architecture.md").
    pub path: String,

    /// Output as JSON instead of markdown.
    #[arg(long)]
    pub json: bool,

    /// Require the entry to match this exact revision before printing JSON.
    ///
    /// JSON is required so the response keeps its explicit untrusted-data
    /// authority marker.
    #[arg(long, value_name = "HASH", requires = "json")]
    pub revision: Option<String>,
}

impl Command for Show {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open_readonly(&root).map_err(CliError::Repository)?;

        let entry = repo
            .vault_retrieve(&self.path)
            .map_err(CliError::Repository)?
            .ok_or_else(|| CliError::VaultEntityNotFound {
                kind: "vault entry",
                id: self.path.clone(),
                hint: None,
            })?;
        let revision_hash =
            (self.json || self.revision.is_some()).then(|| vault_entry_revision_hash(&entry));
        if let Some(actual) = revision_hash.as_deref() {
            require_revision(&self.path, self.revision.as_deref(), actual)?;
        }

        if self.json {
            let json = entry_json(
                &self.path,
                &entry,
                revision_hash
                    .as_deref()
                    .expect("JSON output always computes a revision"),
            );
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            print!("{}", String::from_utf8_lossy(&entry.content_bytes));
        }

        Ok(())
    }
}

fn require_revision(path: &str, expected: Option<&str>, actual: &str) -> CliResult<()> {
    match expected {
        Some(expected) if !expected.eq_ignore_ascii_case(actual) => {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "Vault entry revision changed for {path}: expected {expected}, found {actual}"
                ),
            });
        }
        _ => {}
    }
    Ok(())
}

fn entry_json(path: &str, entry: &VaultEntry, revision_hash: &str) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "mode": "vault_entry_body",
        "content_authority": "untrusted_historical_data",
        "path": path,
        "entry_type": entry.entry_type.to_string(),
        "revision_hash": revision_hash,
        "content": String::from_utf8_lossy(&entry.content_bytes),
        "frontmatter": serde_json::from_str::<serde_json::Value>(
            &entry.frontmatter_json
        ).unwrap_or_default(),
        "created_at": entry.created_at,
        "updated_at": entry.updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::pristine::vault::VaultEntryType;

    fn entry() -> VaultEntry {
        VaultEntry::new(
            VaultEntryType::Memory,
            b"Use RS256 signing.".to_vec(),
            r#"{"name":"auth","status":"active"}"#.to_string(),
            "2026-07-01T00:00:00Z".to_string(),
        )
    }

    #[test]
    fn json_includes_the_exact_entry_revision() {
        let entry = entry();
        let revision = vault_entry_revision_hash(&entry);
        let json = entry_json("memory/auth.md", &entry, &revision);
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["mode"], "vault_entry_body");
        assert_eq!(json["content_authority"], "untrusted_historical_data");
        assert_eq!(json["path"], "memory/auth.md");
        assert_eq!(json["revision_hash"], revision);
        assert_eq!(json["content"], "Use RS256 signing.");
    }

    #[test]
    fn exact_revision_allows_the_pull() {
        assert!(require_revision("memory/auth.md", Some("ABC"), "ABC").is_ok());
        assert!(require_revision("memory/auth.md", Some("abc"), "ABC").is_ok());
        assert!(require_revision("memory/auth.md", None, "ABC").is_ok());
    }

    #[test]
    fn revision_requires_json_at_the_cli_boundary() {
        assert!(Show::try_parse_from(["show", "memory/auth.md", "--revision", "ABC"]).is_err());
        assert!(
            Show::try_parse_from(["show", "memory/auth.md", "--revision", "ABC", "--json"]).is_ok()
        );
    }

    #[test]
    fn changed_revision_blocks_the_pull() {
        match require_revision("memory/auth.md", Some("OLD"), "NEW").unwrap_err() {
            CliError::InvalidArgument { message } => assert_eq!(
                message,
                "Vault entry revision changed for memory/auth.md: expected OLD, found NEW"
            ),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }
}
