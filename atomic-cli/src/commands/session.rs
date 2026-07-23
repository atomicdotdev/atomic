//! Atomic-native session ledger inspection.

use clap::{Args, Subcommand};

use atomic_core::types::Base32;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct Session {
    #[command(subcommand)]
    command: SessionCommands,
}

#[derive(Debug, Subcommand)]
enum SessionCommands {
    /// Show the ordered Atomic ledger for a session.
    Show(SessionShow),
    /// Fork a session at a turn boundary into a new child session.
    Fork(SessionFork),
    /// Rebuild session indexes from stored provenance graphs.
    Rebuild(SessionRebuild),
}

#[derive(Debug, Args)]
pub struct SessionFork {
    /// External session ID of the parent session.
    pub parent_session_id: String,
    /// Turn number to fork at (inherits turns 0..=N).
    #[arg(long)]
    pub at_turn: u32,
    /// External session ID for the new child session.
    #[arg(long)]
    pub child: String,
}

#[derive(Debug, Args)]
pub struct SessionRebuild {}

#[derive(Debug, Args)]
pub struct SessionHead {
    /// External agent session ID.
    pub session_id: String,
}

#[derive(Debug, Args)]
pub struct SessionShow {
    /// External agent session ID. Omit to show recent sessions.
    pub session_id: Option<String>,

    /// Number of recent sessions to show when no ID is supplied.
    #[arg(short = 'n', long, default_value_t = 5)]
    pub limit: usize,

    /// Emit the indexed ledger as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for Session {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            SessionCommands::Show(show) => show.run(),
            SessionCommands::Fork(fork) => fork.run(),
            SessionCommands::Rebuild(rebuild) => rebuild.run(),
        }
    }
}

impl Command for SessionFork {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;
        let (parent_hash, child_hash) = repo
            .fork_session(&self.parent_session_id, self.at_turn, &self.child)
            .map_err(CliError::Repository)?;
        println!("Forked session {}", self.parent_session_id);
        println!("  Parent manifest: {}", parent_hash.to_base32());
        println!("  Child session: {}", self.child);
        println!("  Child manifest: {}", child_hash.to_base32());
        println!("  Fork turn: {}", self.at_turn);
        Ok(())
    }
}

impl Command for SessionRebuild {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;
        let (indexed, skipped, corrupt) =
            repo.rebuild_session_index().map_err(CliError::Repository)?;
        println!("Session index rebuild complete");
        println!("  Indexed: {}", indexed);
        println!("  Already present: {}", skipped);
        println!("  Corrupt (skipped): {}", corrupt);
        Ok(())
    }
}

impl Command for SessionShow {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;
        match &self.session_id {
            Some(session_id) => show_session_detail(&repo, session_id, self.json),
            None => show_recent_sessions(&repo, self.limit, self.json),
        }
    }
}

fn show_recent_sessions(repo: &Repository, limit: usize, json: bool) -> CliResult<()> {
    let ledgers = repo
        .list_session_ledgers(limit)
        .map_err(CliError::Repository)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ledgers).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("failed to encode session list: {}", e))
            })?
        );
        return Ok(());
    }
    if ledgers.is_empty() {
        println!("No sessions recorded.");
        return Ok(());
    }

    println!("Recent sessions ({}):", ledgers.len());
    println!(
        "  {:<22} {:<22} {:<8} {:>5}  {:<12} {}",
        "SESSION", "VIEW", "STATUS", "TURNS", "PLAN", "LAST ACTIVITY"
    );
    for (record, turns) in ledgers {
        let status = if record.ended_at.is_some() {
            "ended"
        } else {
            "active"
        };
        let activity = turns
            .last()
            .map(|turn| turn.timestamp)
            .or(record.ended_at)
            .unwrap_or(record.started_at);
        let activity = chrono::DateTime::from_timestamp(activity, 0)
            .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| activity.to_string());
        let plan = turns
            .iter()
            .rev()
            .find_map(|turn| turn.plan_id.as_deref())
            .unwrap_or("—");
        println!(
            "  {:<22} {:<22} {:<8} {:>5}  {:<12} {}",
            truncate_column(&record.session_id, 22),
            truncate_column(record.view_name.as_deref().unwrap_or("—"), 22),
            status,
            record.turn_count,
            truncate_column(plan, 12),
            activity,
        );
        if let Some(goal) = turns.last().and_then(|turn| turn.goal.as_deref()) {
            println!("    {}", single_line(goal));
        }
    }
    Ok(())
}

fn show_session_detail(repo: &Repository, session_id: &str, json: bool) -> CliResult<()> {
    let (record, turns) = repo
        .get_session_ledger(session_id)
        .map_err(CliError::Repository)?
        .ok_or_else(|| CliError::InvalidArgument {
            message: format!("session not found: {}", session_id),
        })?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&(record, turns)).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("failed to encode session ledger: {}", e))
            })?
        );
        return Ok(());
    }

    println!("Session {}", record.session_id);
    println!(
        "  Status: {}",
        if record.ended_at.is_some() {
            "ended"
        } else {
            "active"
        }
    );
    if let Some(view) = &record.view_name {
        println!("  View: {}", view);
    }
    if let Some(parent) = &record.parent_view {
        println!("  Parent view: {}", parent);
    }
    println!("  JSON: {}", record.json_path);
    println!("  Turns: {}", record.turn_count);
    if let Ok(Some(head)) = repo.get_session_head(session_id) {
        println!("  Manifest head: {}", head.to_base32());
        if let Ok(Some(manifest)) = repo.get_session_manifest(&head) {
            if let Some(parent) = manifest.parent_session {
                println!("  Parent manifest: {}", parent.to_base32());
            }
            if let Some(fork_turn) = manifest.fork_turn {
                println!("  Forked at turn: {}", fork_turn);
            }
        }
    }
    if let Some(first) = record.first_provenance {
        println!("  First provenance: {}", first.to_base32());
    }
    if let Some(latest) = record.latest_provenance {
        println!("  Latest provenance: {}", latest.to_base32());
    }

    for turn in turns {
        let changes = turn
            .change_hashes
            .iter()
            .map(|hash| hash.to_base32())
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "\nTurn {}  provenance {}",
            turn.turn_number,
            turn.provenance_hash.to_base32()
        );
        if let Some(goal) = turn.goal {
            println!("  Goal marker: {}", goal);
        }
        if let Some(plan_id) = turn.plan_id {
            println!("  Plan: {}", plan_id);
        }
        if !turn.todos.is_empty() {
            println!("  Todos ({}):", turn.todos.len());
            for todo in turn.todos {
                let (marker, unknown_status) = todo_status_marker(&todo.status);
                let content = single_line(&todo.content);
                let priority = if todo.priority.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", todo.priority)
                };
                let status = unknown_status
                    .map(|status| format!(" (status: {})", status))
                    .unwrap_or_default();
                println!("    {}{} {}{}", marker, priority, content, status);
            }
        }
        println!(
            "  Changes: {}",
            if changes.is_empty() {
                "(none)"
            } else {
                &changes
            }
        );
        if let Some(previous) = turn.previous_provenance {
            println!("  Previous: {}", previous.to_base32());
        }
    }

    Ok(())
}

fn truncate_column(value: &str, width: usize) -> String {
    let count = value.chars().count();
    if count <= width {
        value.to_string()
    } else if width <= 1 {
        "…".to_string()
    } else {
        format!("{}…", value.chars().take(width - 1).collect::<String>())
    }
}

fn todo_status_marker(status: &str) -> (&'static str, Option<&str>) {
    match status.to_ascii_lowercase().as_str() {
        "completed" | "complete" | "done" => ("[x]", None),
        "in_progress" | "in-progress" | "active" => ("[~]", None),
        "pending" | "todo" | "not_started" | "not-started" => ("[ ]", None),
        _ => ("[?]", Some(status)),
    }
}

fn single_line(content: &str) -> String {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        "(untitled todo)".to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_markers_preserve_unknown_status() {
        assert_eq!(todo_status_marker("completed"), ("[x]", None));
        assert_eq!(todo_status_marker("in_progress"), ("[~]", None));
        assert_eq!(todo_status_marker("pending"), ("[ ]", None));
        assert_eq!(todo_status_marker("blocked"), ("[?]", Some("blocked")));
    }

    #[test]
    fn todo_content_renders_on_one_line() {
        assert_eq!(single_line("first\n  second"), "first second");
        assert_eq!(single_line("  \n "), "(untitled todo)");
    }

    #[test]
    fn session_columns_truncate_deterministically() {
        assert_eq!(truncate_column("short", 8), "short");
        assert_eq!(truncate_column("abcdefghij", 8), "abcdefg…");
    }
}
