//! `atomic intent update <ID>` — update a stored intent's fields.
//!
//! The canonical-family analogue of `atomic vault intent update`. It delegates
//! to the exact same [`Repository::vault_intent_update`] write path, so its
//! behavior (frontmatter/status/priority/title/reason/source-memory updates,
//! body replacement, the started-or-linked guard behind `--force`) is
//! byte-for-byte identical to the vault verb it supersedes.

use clap::Parser;

use atomic_repository::{IntentUpdateOptions, Repository};

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Update an intent's fields.
///
/// Modifies the status, assignee, priority, title, icebox reason, source-memory
/// provenance, or Markdown body of an existing intent. The body can be set
/// inline with `--body` or piped via `--body-stdin`.
#[derive(Parser, Debug)]
#[command(name = "update")]
pub struct IntentUpdate {
    /// Intent ID (e.g. "PIMO-1" or "1").
    pub id: String,

    /// New status.
    #[arg(long)]
    pub status: Option<String>,

    /// New assignee.
    #[arg(long)]
    pub assignee: Option<String>,

    /// New priority.
    #[arg(long)]
    pub priority: Option<String>,

    /// New title.
    #[arg(long)]
    pub title: Option<String>,

    /// Reason the intent is being iceboxed. Persisted as `icebox_reason` +
    /// `iceboxed_at` (normally passed alongside `--status icebox`).
    #[arg(long)]
    pub reason: Option<String>,

    /// Source-memory RDF ids that informed this Intent (comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub informed_by: Vec<String>,

    /// New Markdown body content, provided inline.
    #[arg(long, conflicts_with = "body_stdin")]
    pub body: Option<String>,

    /// Read the new Markdown body content from standard input.
    #[arg(long)]
    pub body_stdin: bool,

    /// Rewrite the body even if the intent has started or is linked to a goal.
    #[arg(long, short = 'f')]
    pub force: bool,
}

impl IntentUpdate {
    fn resolve_body(&self) -> CliResult<Option<String>> {
        if let Some(ref body) = self.body {
            return Ok(Some(body.clone()));
        }
        if self.body_stdin {
            use std::io::Read;
            let mut content = String::new();
            std::io::stdin()
                .read_to_string(&mut content)
                .map_err(CliError::Io)?;
            return Ok(Some(content));
        }
        Ok(None)
    }
}

impl Command for IntentUpdate {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let content = self.resolve_body()?;
        if self.status.is_none()
            && self.assignee.is_none()
            && self.priority.is_none()
            && self.title.is_none()
            && self.reason.is_none()
            && self.informed_by.is_empty()
            && content.is_none()
        {
            return Err(CliError::InvalidArgument {
                message: "Nothing to update. Provide at least one of \
                    --status, --assignee, --priority, --title, --reason, \
                    --informed-by, \
                    --body, or --body-stdin."
                    .to_string(),
            });
        }

        let body_updated = content.is_some();
        let info = repo
            .vault_intent_update(
                &self.id,
                IntentUpdateOptions {
                    status: self.status.clone(),
                    assignee: self.assignee.clone(),
                    priority: self.priority.clone(),
                    title: self.title.clone(),
                    informed_by: (!self.informed_by.is_empty()).then(|| self.informed_by.clone()),
                    content,
                    reason: self.reason.clone(),
                    force: self.force,
                },
            )
            .map_err(CliError::Repository)?;

        println!("Updated intent: {}", info.id);
        println!("  status: {}", info.status);
        println!("  priority: {}", info.priority);
        if let Some(ref assignee) = info.assignee {
            println!("  assignee: {}", assignee);
        }
        if body_updated {
            println!("  body: updated");
        }
        if !self.informed_by.is_empty() {
            println!("  informed by: {} source(s)", self.informed_by.len());
        }

        Ok(())
    }
}
