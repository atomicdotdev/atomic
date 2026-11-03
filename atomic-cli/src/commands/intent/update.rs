//! `atomic intent update <ID>` — update a stored intent's fields.
//!
//! The canonical-family analogue of `atomic vault intent update`. It delegates
//! to the exact same [`Repository::vault_intent_update`] write path, so its
//! field updates and the started-or-linked guard behind `--force` share the
//! vault verb's implementation. Work-start declarations additionally persist
//! explicit agent turn context before updating status.

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

    /// Bind work start to this active agent session (or ATOMIC_SESSION_ID).
    /// Only valid with --status in_progress; never inferred from active sessions.
    #[arg(long)]
    pub session_id: Option<String>,

    /// Expected 1-indexed hook turn, to reject delayed work-start declarations.
    #[arg(long)]
    pub turn: Option<u32>,

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
    fn bind_work_start(&self) -> CliResult<()> {
        let starting = matches!(self.status.as_deref(), Some("in_progress" | "in-progress"));
        if !starting {
            if self.session_id.is_some() || self.turn.is_some() {
                return Err(CliError::InvalidArgument {
                    message: "--session-id and --turn require --status in_progress".into(),
                });
            }
            return Ok(());
        }
        let session_id = self
            .session_id
            .clone()
            .or_else(|| std::env::var("ATOMIC_SESSION_ID").ok());
        let Some(session_id) = session_id else {
            if self.turn.is_some() {
                return Err(CliError::InvalidArgument {
                    message: "--turn requires --session-id or ATOMIC_SESSION_ID".into(),
                });
            }
            return Ok(()); // Human status-only updates remain supported.
        };
        let root = find_repository_root()?;
        let uid = {
            let repo =
                crate::commands::open_readonly_repository(&root).map_err(CliError::Repository)?;
            let inputs =
                super::bridge::read_intent(&repo, &self.id).map_err(CliError::Repository)?;
            inputs
                .frontmatter
                .get("uid")
                .and_then(|value| value.as_str())
                .filter(|uid| !uid.is_empty())
                .ok_or_else(|| CliError::InvalidArgument {
                    message: "intent has no canonical UID; cannot bind work start".into(),
                })?
                .to_string()
        };
        let dot = Repository::canonical_dot_dir(&root).map_err(CliError::Repository)?;
        let store = atomic_agent::turn::session::SessionStore::new(dot.join("sessions"))
            .map_err(|error| CliError::Internal(error.into()))?;
        // Persist the declaration before status mutation or any record hook.
        let binding = store
            .bind_turn_intent(&session_id, self.turn, &uid)
            .map_err(|error| CliError::Internal(error.into()))?;
        println!(
            "Bound governing intent: {} (session {}, turn {})",
            binding.intent_uid, binding.session_id, binding.turn_number
        );
        Ok(())
    }

    /// Resolve the body override: inline `--body` or `--body-stdin`.
    ///
    /// Shared by the local body and the service-layer hook — reading the
    /// client's stdin is client-side by design, so the routed path resolves
    /// the body with this SAME code and sends it on the wire.
    pub(crate) fn resolve_body(&self) -> CliResult<Option<String>> {
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
        self.bind_work_start()?;
        if crate::commands::rpc::intent_update(self)? {
            return Ok(());
        }

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
