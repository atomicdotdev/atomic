//! The `tag show` command for displaying tag details.
//!
//! This module implements the `atomic tag show` command, which displays
//! detailed information about a specific tag including its state, sequence
//! number, timestamp, and any annotation (message/author).
//!
//! # Usage
//!
//! ```text
//! atomic tag show <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the tag to show
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Examples
//!
//! Show tag details:
//! ```text
//! $ atomic tag show v1.0.0
//! Tag: v1.0.0
//! View: dev
//! Sequence: 42
//! State: ABCDEF123456789...
//! Created: 2024-01-15 10:30:00 UTC
//! Type: annotated
//! Message: Release version 1.0.0
//! Author: Alice <alice@example.com>
//! ```

use clap::Parser;

use atomic_core::types::{Base32, Hash};
use atomic_repository::{Repository, TagKind};

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{emphasis, hint};

#[cfg(test)]
use std::path::PathBuf;

// Show Command

/// Show details for a specific tag.
///
/// Displays detailed information about a tag including its state,
/// sequence number, timestamp, and any annotation.
#[derive(Parser, Debug, Default)]
#[command(name = "show")]
pub struct Show {
    /// Name of the tag to show.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,
}

impl Show {
    /// Create a new Show command targeting the given tag.
    pub fn with_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
        }
    }
}

impl Command for Show {
    fn run(&self) -> CliResult<()> {
        // Route every form through the service layer: the wire carries the
        // full tag record plus the ReviewGate per-record presence the local
        // render reads from the repository.
        if crate::commands::rpc::tag_show(self)? {
            return Ok(());
        }

        // Get the tag name
        let name = self
            .name
            .as_ref()
            .ok_or_else(|| CliError::InvalidArgument {
                message: "Tag name is required".to_string(),
            })?;

        // Find the repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // Get the tag
        let tag = repo.get_tag(name).map_err(CliError::Repository)?;

        match tag {
            Some(tag) => {
                println!("{}: {}", emphasis("Tag"), tag.name);
                println!("{}: {}", emphasis("View"), tag.view);
                println!("{}: {}", emphasis("Sequence"), tag.sequence);
                println!("{}: {}", emphasis("State"), tag.state.to_base32());
                println!(
                    "{}: {}",
                    emphasis("Created"),
                    tag.timestamp.format("%Y-%m-%d %H:%M:%S UTC")
                );
                println!("{}: {}", emphasis("Kind"), tag.kind);
                println!(
                    "{}: {}",
                    emphasis("Type"),
                    if tag.is_annotated() {
                        "annotated"
                    } else {
                        "lightweight"
                    }
                );

                if let Some(ref message) = tag.message {
                    println!("{}: {}", emphasis("Message"), message);
                }

                if let Some(ref author) = tag.author {
                    let author_str = match &author.email {
                        Some(email) => format!("{} <{}>", author.name, email),
                        None => author.name.clone(),
                    };
                    println!("{}: {}", emphasis("Author"), author_str);
                }

                if let Some(ref metadata) = tag.metadata {
                    if tag.kind == TagKind::ReviewGate {
                        render_review_gate_metadata(&repo, metadata);
                    } else {
                        println!("{}: {}", emphasis("Metadata"), metadata);
                    }
                }

                Ok(())
            }
            None => {
                println!(
                    "{}",
                    hint(&format!(
                        "Tag '{}' not found. Use 'atomic tag list' to see available tags.",
                        name
                    ))
                );
                Ok(())
            }
        }
    }
}

/// Render `ReviewGate` tag metadata as a readable provenance block.
///
/// Only lines whose underlying data is present are printed. All lookups are
/// defensive against absent fields, matching the extensible metadata shape.
fn render_review_gate_metadata(repo: &Repository, metadata: &serde_json::Value) {
    // The per-record presence/membership is the repository read the routed
    // path cannot make — computed here for the local body, computed by the
    // GetTag handler for the wire.
    let mut records = Vec::new();
    if let Some(original) = metadata
        .get("changes")
        .and_then(|c| c.get("original_hashes"))
        .and_then(|v| v.as_array())
    {
        for entry in original {
            let Some(text) = entry.as_str() else { continue };
            match Hash::from_base32(text.as_bytes()) {
                Some(hash) => {
                    let present = repo.has_change(&hash);
                    let views = repo.views_containing_change(&hash).unwrap_or_default();
                    records.push(crate::commands::rpc::TagRecordWire {
                        hash: text.to_string(),
                        parseable: true,
                        present,
                        views,
                    });
                }
                None => records.push(crate::commands::rpc::TagRecordWire {
                    hash: text.to_string(),
                    parseable: false,
                    present: false,
                    views: Vec::new(),
                }),
            }
        }
    }
    render_review_gate_records(&metadata.to_string(), &records);
}

/// The ReviewGate render over wire-carried records — the SAME lines the
/// local body prints (Git/Aggregate/Records blocks), driven by the
/// presence/membership the handler computed.
pub(crate) fn render_review_gate_records(
    metadata_json: &str,
    records: &[crate::commands::rpc::TagRecordWire],
) {
    let Ok(metadata) = serde_json::from_str::<serde_json::Value>(metadata_json) else {
        return;
    };
    // Git provenance line: "Git: <merge_strategy> <sha> (PR #<n>)"
    if let Some(git) = metadata.get("git") {
        let sha = git.get("sha").and_then(|v| v.as_str());
        let strategy = git.get("merge_strategy").and_then(|v| v.as_str());
        if sha.is_some() || strategy.is_some() {
            let mut line = String::new();
            if let Some(strategy) = strategy {
                line.push_str(strategy);
            }
            if let Some(sha) = sha {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(sha);
            }
            if let Some(pr) = git.get("pr_number").and_then(|v| v.as_u64()) {
                line.push_str(&format!(" (PR #{})", pr));
            }
            println!("{}: {}", emphasis("Git"), line);
        }
    }

    let changes = metadata.get("changes");
    let record_count = records.len();

    // Aggregate line.
    if let Some(changes) = changes {
        let from = changes.get("from").and_then(|v| v.as_str());
        let to = changes.get("to").and_then(|v| v.as_str());
        let count = changes.get("count").and_then(|v| v.as_u64());
        let inserted = changes
            .get("inserted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if let (Some(from), Some(to)) = (from, to) {
            let count_val = count.map(|c| c as usize).unwrap_or(record_count);
            let inserted_suffix = if inserted { ", inserted" } else { "" };
            println!(
                "{}: {} \u{2026} {}  ({} records{})",
                emphasis("Aggregate"),
                from,
                to,
                count_val,
                inserted_suffix
            );
        } else if record_count > 0 {
            let count_val = count.map(|c| c as usize).unwrap_or(record_count);
            println!("{}: {} records", emphasis("Aggregate"), count_val);
        }
    }

    // Per-record presence and view membership.
    if !records.is_empty() {
        println!("{}:", emphasis("Records"));
        for record in records {
            if !record.parseable {
                println!("  {}", record.hash);
                continue;
            }
            let mark = if record.present {
                "\u{2713}"
            } else {
                "\u{2717}"
            };
            let status = if record.present { "present" } else { "missing" };
            let views_suffix = if record.views.is_empty() {
                String::new()
            } else {
                format!("  ({})", record.views.join(", "))
            };
            println!("  {} {}  {}{}", mark, record.hash, status, views_suffix);
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_repository::TagKind;
    use serial_test::serial;

    // -------------------------------------------------------------------------
    // Command Builder Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_show_with_name() {
        let cmd = Show::with_name("v1.0.0");
        assert_eq!(cmd.name, Some("v1.0.0".to_string()));
    }

    #[test]
    fn test_default() {
        let cmd = Show::default();
        assert!(cmd.name.is_none());
    }

    // -------------------------------------------------------------------------
    // Error Handling Tests (without repository)
    // -------------------------------------------------------------------------

    #[test]
    fn test_run_without_name() {
        let cmd = Show::default();
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("required"));
            }
            other => panic!("Expected InvalidArgument, got: {:?}", other),
        }
    }

    // -------------------------------------------------------------------------
    // Directory Guard for Safe Current Dir Changes
    // -------------------------------------------------------------------------

    struct DirGuard {
        original: PathBuf,
    }

    impl DirGuard {
        fn new() -> Self {
            Self {
                original: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    // -------------------------------------------------------------------------
    // Integration Tests (require temp repository)
    // -------------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_show_existing_tag() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a tag
        {
            let repo = Repository::init(repo_path).unwrap();
            repo.create_tag("v1.0.0", None, TagKind::Release).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Show the tag
        let cmd = Show::with_name("v1.0.0");
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_show_annotated_tag() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create an annotated tag
        {
            let repo = Repository::init(repo_path).unwrap();
            repo.create_tag("v1.0.0", Some("Release version 1.0.0"), TagKind::Release)
                .unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Show the tag
        let cmd = Show::with_name("v1.0.0");
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_show_nonexistent_tag() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository without any tags
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Try to show a non-existent tag (should succeed with "not found" message)
        let cmd = Show::with_name("nonexistent");
        let result = cmd.run();
        assert!(result.is_ok());
    }
}
