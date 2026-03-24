//! `atomic agent explain` command implementation.
//!
//! Generates AI reasoning summaries for agent turns by reading the condensed
//! transcript from the change's unhashed section and calling Claude CLI.
//!
//! This is deliberately NOT part of the recording hot path — it's a separate
//! command the user runs when they want reasoning. Calling Claude CLI from
//! inside a Claude Code hook would be recursive and add 30+ seconds.
//!
//! # Usage
//!
//! ```text
//! # Explain the most recent turn in a session
//! atomic agent explain <session-id>
//!
//! # Explain a specific turn
//! atomic agent explain <session-id> --turn 3
//!
//! # Explain all turns in a session
//! atomic agent explain <session-id> --all
//!
//! # Save reasoning back into the change (persists on push)
//! atomic agent explain <session-id> --save
//! ```
//!
//! # How It Works
//!
//! 1. Looks up the session → gets the agent stack name
//! 2. Loads changes from the agent stack via `repo.log()`
//! 3. For each target turn:
//!    a. Reads the condensed transcript from `change.unhashed["agent_turn"]`
//!    b. If no transcript in the change, reads from the session's transcript file
//!    c. Calls Claude CLI (`claude --print`) to generate reasoning
//!    d. Anchors code learnings to the change's CRDT graph (FileOps)
//!    e. Prints the reasoning
//!    f. Optionally saves it back into the change's unhashed section

use clap::Args;

use atomic_agent::learnings::save_learnings_to_context_file;
use atomic_agent::transcript::{
    self, anchor_to_graph, attach_unhashed, extract_unhashed, ClaudeCliGenerator,
    ReasoningGenerator, TurnReasoning, UnhashedTurnData,
};
use atomic_agent::turn::session::SessionStore;
use atomic_core::types::Base32;
use atomic_repository::history::HistoryOptions;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{emphasis, hint, info, print_error, print_success, print_warning};

// Explain Command

/// Generate AI reasoning summaries for agent turns.
///
/// Reads the condensed transcript from recorded changes and calls Claude CLI
/// to generate structured reasoning: intent, outcome, learnings (repo, code,
/// workflow), friction, and open items.
///
/// Code learnings are anchored to the CRDT graph so they follow the code
/// through renames and refactors. Users see files, functions, and lines —
/// the graph anchoring is invisible.
///
/// Requires Claude CLI (`claude`) to be installed and authenticated.
#[derive(Debug, Args)]
pub struct Explain {
    /// Session ID to explain.
    ///
    /// The session must have at least one recorded turn on its agent stack.
    /// Use `atomic agent status` to see available sessions.
    session_id: String,

    /// Explain a specific turn number (1-indexed).
    ///
    /// If not specified, explains the most recent turn.
    #[arg(long, value_name = "N")]
    turn: Option<u32>,

    /// Explain all turns in the session.
    #[arg(long)]
    all: bool,

    /// Save the reasoning back into the change's unhashed section.
    ///
    /// When saved, the reasoning persists in the change file and is
    /// included when you `atomic push`. Without this flag, reasoning
    /// is only displayed, not stored.
    #[arg(long)]
    save: bool,

    /// Claude CLI model to use for reasoning generation.
    ///
    /// Defaults to "sonnet". Use "opus" for higher quality at higher cost,
    /// or "haiku" for faster/cheaper results.
    #[arg(long, default_value = "sonnet")]
    model: String,
}

impl Explain {
    /// Create a default instance for testing.
    #[cfg(test)]
    pub(crate) fn default_for_test() -> Self {
        Self {
            session_id: "test-session".to_string(),
            turn: None,
            all: false,
            save: false,
            model: "sonnet".to_string(),
        }
    }
}

impl Command for Explain {
    fn run(&self) -> CliResult<()> {
        let repo_root = find_repository_root()?;

        // Load the session to get the stack name
        let session_store =
            SessionStore::for_repo(&repo_root).map_err(|e| CliError::InvalidRepository {
                reason: format!("Failed to open session store: {}", e),
            })?;

        let session = session_store
            .load(&self.session_id)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to load session: {}", e)))?
            .ok_or_else(|| CliError::InvalidArgument {
                message: format!(
                    "Session '{}' not found. Use 'atomic agent status' to see available sessions.",
                    self.session_id
                ),
            })?;

        // Open the repository
        let repo = Repository::open(&repo_root)?;

        // Get the changes on the agent stack
        let history_options = HistoryOptions::with_headers().stack(&session.stack_name);

        let entries = repo.log(history_options).map_err(|e| match e {
            atomic_repository::RepositoryError::StackNotFound { name } => {
                CliError::StackNotFound { name }
            }
            other => CliError::Repository(other),
        })?;

        if entries.is_empty() {
            println!(
                "Session '{}' has no recorded turns on stack '{}'.",
                self.session_id, session.stack_name,
            );
            return Ok(());
        }

        // Determine which turns to explain
        let turns_to_explain: Vec<usize> = if self.all {
            (0..entries.len()).collect()
        } else if let Some(turn_num) = self.turn {
            let idx =
                (turn_num as usize)
                    .checked_sub(1)
                    .ok_or_else(|| CliError::InvalidArgument {
                        message: "Turn number must be >= 1".to_string(),
                    })?;
            if idx >= entries.len() {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "Turn {} not found. Session has {} turn{}.",
                        turn_num,
                        entries.len(),
                        if entries.len() == 1 { "" } else { "s" }
                    ),
                });
            }
            vec![idx]
        } else {
            // Most recent turn (last entry)
            vec![entries.len() - 1]
        };

        // Check Claude CLI availability
        let generator = ClaudeCliGenerator::new().with_model(&self.model);
        if !generator.is_available() {
            print_error(
                "Claude CLI not found. Install from https://docs.anthropic.com/en/docs/claude-code",
            );
            println!();
            println!("The 'explain' command uses Claude CLI to generate reasoning summaries.");
            println!("Make sure 'claude' is in your PATH and authenticated.");
            return Ok(());
        }

        println!(
            "{} {} turn{} from session {}",
            emphasis("Explaining"),
            turns_to_explain.len(),
            if turns_to_explain.len() == 1 { "" } else { "s" },
            hint(&self.session_id),
        );
        println!();

        // Process each turn
        for &idx in &turns_to_explain {
            let entry = &entries[idx];
            let turn_num = idx + 1;

            // Load the full change
            let change = repo.load_change(&entry.hash).map_err(|e| {
                CliError::Internal(anyhow::anyhow!(
                    "Failed to load change {}: {}",
                    entry.hash.to_base32(),
                    e
                ))
            })?;

            let message = change.hashed.header.message.clone();
            println!("{}", emphasis(&format!("Turn {} — {}", turn_num, message)));

            // Get the condensed transcript text
            let condensed_text = get_condensed_text(&change, &session)?;
            if condensed_text.is_empty() {
                print_warning("  No transcript available for this turn — skipping");
                println!();
                continue;
            }

            // Get the files for this turn — try FileOps paths first (most
            // accurate), then unhashed tool summaries, then session files_touched
            let files: Vec<String> = if !change.file_ops().is_empty() {
                change
                    .file_ops()
                    .iter()
                    .map(|fop| fop.path().to_string())
                    .collect()
            } else if let Some(ref unhashed) = extract_unhashed(&change) {
                unhashed
                    .tools_used
                    .iter()
                    .flat_map(|t| t.files_affected.clone())
                    .collect()
            } else {
                session.files_touched.clone()
            };

            // Generate reasoning
            print!("  Generating reasoning via Claude CLI ({})...", self.model);
            let reasoning = match generator.generate(&condensed_text, &files) {
                Ok(r) => {
                    println!(" ✓");
                    r
                }
                Err(e) => {
                    println!(" ✗");
                    print_error(&format!("  Failed: {}", e));
                    println!();
                    continue;
                }
            };

            if reasoning.is_empty() {
                print_warning("  Reasoning is empty — nothing to show");
                println!();
                continue;
            }

            // Anchor code learnings to the CRDT graph
            let mut reasoning = reasoning;
            if reasoning.has_code_learnings() {
                let file_ops = change.file_ops();
                if !file_ops.is_empty() {
                    anchor_to_graph(&mut reasoning.learnings.code, file_ops);
                    let anchored = reasoning
                        .learnings
                        .code
                        .iter()
                        .filter(|l| l.is_anchored())
                        .count();
                    if anchored > 0 {
                        println!(
                            "  {} {}/{} code learnings anchored to graph",
                            hint("⚓"),
                            anchored,
                            reasoning.learnings.code.len()
                        );
                    }
                }
            }

            // Display the reasoning
            print_reasoning(&reasoning);

            // Save back to the change and context file if requested
            if self.save {
                // Save reasoning into the change's unhashed section (pushable)
                match save_reasoning_to_change(&repo, &entry.hash, &change, &reasoning) {
                    Ok(()) => {
                        print_success(&format!(
                            "  Saved reasoning to change {} (will be included on push)",
                            &entry.hash.to_base32()[..12]
                        ));
                    }
                    Err(e) => {
                        print_warning(&format!("  Failed to save reasoning to change: {}", e));
                    }
                }

                // Save learnings to agent context file (CLAUDE.md, GEMINI.md, etc.)
                // so the agent reads them on the next session start
                if !reasoning.learnings.is_empty() {
                    match save_learnings_to_context_file(
                        &repo_root,
                        &session.agent_name,
                        &reasoning.learnings,
                    ) {
                        Ok(result) => {
                            if result.has_changes() {
                                print_success(&format!("  {}", result));
                            } else {
                                println!("  {}", hint(&result.to_string()));
                            }
                        }
                        Err(e) => {
                            print_warning(&format!(
                                "  Failed to save learnings to context file: {}",
                                e
                            ));
                        }
                    }
                }
            }

            println!();
        }

        if !self.save {
            let context_file = atomic_agent::learnings::context_file_for_agent(&session.agent_name);
            println!(
                "{}",
                hint(&format!(
                    "Use --save to persist reasoning in the change and append learnings to {}.",
                    context_file
                ))
            );
        }

        Ok(())
    }
}

// Get Condensed Transcript Text

/// Get the condensed transcript text for a change.
///
/// First tries to read from the change's unhashed section (already condensed
/// during recording). If not available, falls back to reading the raw
/// transcript file from the session and condensing it on the fly.
fn get_condensed_text(
    change: &atomic_core::change::Change,
    session: &atomic_agent::turn::session::AgentSession,
) -> CliResult<String> {
    // Try the change's unhashed section first
    if let Some(unhashed) = extract_unhashed(change) {
        if !unhashed.condensed_text.is_empty() {
            return Ok(unhashed.condensed_text);
        }
        // Has entries but no formatted text — format them
        if !unhashed.condensed_transcript.is_empty() {
            let files: Vec<String> = unhashed
                .tools_used
                .iter()
                .flat_map(|t| t.files_affected.clone())
                .collect();
            return Ok(transcript::format_condensed(
                &unhashed.condensed_transcript,
                &files,
            ));
        }
    }

    // Fall back to reading the transcript file
    if let Some(ref path) = session.transcript_path {
        if path.exists() {
            let raw = std::fs::read(path).map_err(|e| {
                CliError::Internal(anyhow::anyhow!(
                    "Failed to read transcript at {}: {}",
                    path.display(),
                    e
                ))
            })?;

            let format = if session.agent_name.contains("gemini") {
                "json"
            } else {
                "jsonl"
            };

            let entries = transcript::condense_transcript(&raw, format);
            if entries.is_empty() {
                return Ok(String::new());
            }

            // Get files from FileOps (CRDT layer) or session state
            let files: Vec<String> = if !change.file_ops().is_empty() {
                change
                    .file_ops()
                    .iter()
                    .map(|fop| fop.path().to_string())
                    .collect()
            } else {
                session.files_touched.clone()
            };

            return Ok(transcript::format_condensed(&entries, &files));
        }
    }

    Ok(String::new())
}

// Display Reasoning

/// Print a reasoning summary in a readable format.
fn print_reasoning(reasoning: &TurnReasoning) {
    println!("  ├── {}: {}", emphasis("Intent"), info(&reasoning.intent));
    println!(
        "  ├── {}: {}",
        emphasis("Outcome"),
        info(&reasoning.outcome)
    );

    // Learnings
    if !reasoning.learnings.repo.is_empty()
        || !reasoning.learnings.code.is_empty()
        || !reasoning.learnings.workflow.is_empty()
    {
        println!("  ├── {}:", emphasis("Learnings"));

        for learning in &reasoning.learnings.code {
            let location = if let Some(line) = learning.line {
                if let Some(ref func) = learning.function {
                    format!("{}:{} ({})", learning.path, line, func)
                } else {
                    format!("{}:{}", learning.path, line)
                }
            } else {
                learning.path.clone()
            };

            let category = learning
                .category
                .as_deref()
                .map(|c| format!(" [{}]", c))
                .unwrap_or_default();

            println!(
                "  │   ├── {} — {}{}",
                info(&location),
                learning.finding,
                hint(&category),
            );
        }

        for learning in &reasoning.learnings.repo {
            println!("  │   ├── {}: {}", hint("Repo"), learning);
        }

        for learning in &reasoning.learnings.workflow {
            println!("  │   ├── {}: {}", hint("Workflow"), learning);
        }
    }

    // Friction
    if !reasoning.friction.is_empty() {
        println!("  ├── {}:", emphasis("Friction"));
        for item in &reasoning.friction {
            println!("  │   ├── {}", item);
        }
    }

    // Open items
    if !reasoning.open_items.is_empty() {
        println!("  └── {}:", emphasis("Open Items"));
        for (i, item) in reasoning.open_items.iter().enumerate() {
            let connector = if i == reasoning.open_items.len() - 1 {
                "└──"
            } else {
                "├──"
            };
            println!("      {} {}", connector, item);
        }
    }
}

// Save Reasoning Back to Change

/// Save reasoning back into the change's unhashed section.
///
/// Reads the existing unhashed data, adds the reasoning, and writes
/// the change back to the store. The change hash is NOT affected
/// (unhashed section doesn't contribute to the hash).
fn save_reasoning_to_change(
    repo: &Repository,
    hash: &atomic_core::types::Hash,
    change: &atomic_core::change::Change,
    reasoning: &TurnReasoning,
) -> CliResult<()> {
    let mut updated_change = change.clone();

    // Get or create the unhashed data
    let mut unhashed_data = extract_unhashed(&updated_change).unwrap_or_else(|| {
        // Create minimal unhashed data if none exists
        UnhashedTurnData::new("", 0, "unknown", Vec::new(), &[])
    });

    // Set the reasoning
    unhashed_data = unhashed_data.with_reasoning(reasoning.clone());

    // Attach to the change
    attach_unhashed(&mut updated_change, &unhashed_data)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to attach reasoning: {}", e)))?;

    // Save the change back to the store.
    // The hash doesn't change because the unhashed section doesn't affect it,
    // so this overwrites the same file at the same content-addressed path.
    let _ = hash; // same hash — save_change recomputes and writes to the same path
    repo.save_change(&updated_change)
        .map_err(CliError::Repository)?;

    Ok(())
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_explain_default_for_test() {
        let cmd = Explain::default_for_test();
        assert_eq!(cmd.session_id, "test-session");
        assert!(cmd.turn.is_none());
        assert!(!cmd.all);
        assert!(!cmd.save);
        assert_eq!(cmd.model, "sonnet");
    }

    #[test]
    fn test_explain_with_turn() {
        let cmd = Explain {
            session_id: "sess-1".to_string(),
            turn: Some(3),
            all: false,
            save: false,
            model: "sonnet".to_string(),
        };
        assert_eq!(cmd.turn, Some(3));
    }

    #[test]
    fn test_explain_with_all() {
        let cmd = Explain {
            session_id: "sess-1".to_string(),
            turn: None,
            all: true,
            save: false,
            model: "sonnet".to_string(),
        };
        assert!(cmd.all);
    }

    #[test]
    fn test_explain_with_save() {
        let cmd = Explain {
            session_id: "sess-1".to_string(),
            turn: None,
            all: false,
            save: true,
            model: "sonnet".to_string(),
        };
        assert!(cmd.save);
    }

    #[test]
    fn test_explain_with_model() {
        let cmd = Explain {
            session_id: "sess-1".to_string(),
            turn: None,
            all: false,
            save: false,
            model: "opus".to_string(),
        };
        assert_eq!(cmd.model, "opus");
    }

    #[test]
    fn test_print_reasoning_basic() {
        let reasoning = TurnReasoning {
            intent: "Fix the auth bug".into(),
            outcome: "Fixed token validation".into(),
            learnings: transcript::Learnings {
                repo: vec!["Uses RS256".into()],
                code: vec![transcript::CodeLearning::new(
                    "src/auth.rs",
                    Some(42),
                    "Wrong timezone",
                )
                .with_function("validate_token")
                .with_category("bug")],
                workflow: vec!["cargo test --lib is faster".into()],
            },
            friction: vec!["Complex middleware".into()],
            open_items: vec!["Refresh endpoint same bug".into()],
        };

        // Just verify it doesn't panic
        print_reasoning(&reasoning);
    }

    #[test]
    fn test_print_reasoning_empty() {
        let reasoning = TurnReasoning {
            intent: "test".into(),
            outcome: "done".into(),
            learnings: transcript::Learnings::default(),
            friction: vec![],
            open_items: vec![],
        };

        print_reasoning(&reasoning);
    }

    #[test]
    fn test_print_reasoning_code_learning_no_line() {
        let reasoning = TurnReasoning {
            intent: "test".into(),
            outcome: "done".into(),
            learnings: transcript::Learnings {
                repo: vec![],
                code: vec![transcript::CodeLearning::new(
                    "src/lib.rs",
                    None,
                    "General pattern",
                )],
                workflow: vec![],
            },
            friction: vec![],
            open_items: vec![],
        };

        print_reasoning(&reasoning);
    }
}
