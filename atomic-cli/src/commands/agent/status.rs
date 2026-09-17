//! `atomic agent status` command implementation.
//!
//! Displays information about the current state of agent integration:
//! - Which agents have hooks installed
//! - Active and recent sessions
//! - File watcher availability
//!
//! # Examples
//!
//! ```text
//! # Show status
//! atomic agent status
//!
//! # Show verbose status with session details
//! atomic agent status --verbose
//!
//! # Machine-readable, for tools that gate on integration state
//! atomic agent status --json
//! ```

use clap::Args;
use serde::Serialize;

use atomic_agent::hooks::AgentRegistry;
use atomic_agent::turn::session::SessionStore;

use crate::commands::{find_repository_root, Command};
use crate::error::CliResult;

// AgentStatus Command

/// Show agent integration status.
///
/// Displays which agents have hooks installed, any active sessions,
/// and the current state of the file watcher.
#[derive(Debug, Args)]
pub struct AgentStatus {
    /// Show detailed session information.
    ///
    /// When enabled, shows per-session details including turn count,
    /// files touched, duration, and first prompt.
    #[arg(short, long)]
    verbose: bool,

    /// Print JSON.
    ///
    /// The human output is a report; this is the same facts as data, for
    /// callers that need to *decide* something — a tool asking "does this agent
    /// have hooks installed, and should I install them before recording a run?"
    /// had no option but to scrape the ✓/○ lines, which are prose and free to
    /// change. `--verbose` is ignored here: JSON always carries the full detail.
    #[arg(long)]
    json: bool,
}

/// One agent the registry knows about, and where it stands in this repository.
#[derive(Debug, Serialize)]
struct AgentEntry {
    /// Registry name, e.g. `claude-code`. The stable key to match on.
    name: String,
    display_name: String,
    /// The agent's config was found in this repository.
    detected: bool,
    /// Atomic's hooks are installed for it. Without this its turns are not
    /// recorded, which is the question most callers are actually asking.
    hooks_installed: bool,
}

#[derive(Debug, Serialize)]
struct SessionEntry {
    session_id: String,
    agent_display_name: String,
    view: String,
    phase: String,
    model: String,
    agent_vendor: String,
    turn_count: u32,
    files_touched: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_prompt: Option<String>,
    /// Not ended. Mirrors the ●/○ split in the human output.
    active: bool,
    duration: String,
}

#[derive(Debug, Serialize)]
struct Totals {
    sessions: usize,
    turns: u32,
    files_touched: usize,
}

#[derive(Debug, Serialize)]
struct StatusJson {
    agents: Vec<AgentEntry>,
    sessions: Vec<SessionEntry>,
    totals: Totals,
    /// Why the session list is empty, when it is empty for a reason. The human
    /// output prints this and carries on; dropping it from JSON would turn a
    /// broken session store into an indistinguishable "no sessions".
    #[serde(skip_serializing_if = "Option::is_none")]
    sessions_error: Option<String>,
}

impl AgentStatus {
    /// Create a default instance for testing.
    #[cfg(test)]
    pub(crate) fn default_for_test() -> Self {
        Self {
            verbose: false,
            json: false,
        }
    }

    /// Build the JSON view of the same state the human output describes.
    ///
    /// Every agent in the registry appears, including ones that are neither
    /// detected nor installed: a caller deciding whether to install needs to
    /// distinguish "this agent is unknown to Atomic" from "known, and not set
    /// up", and an omitted entry cannot say which.
    fn to_json(&self, repo_root: &std::path::Path, registry: &AgentRegistry) -> StatusJson {
        let installed = registry.installed(repo_root);
        let detected = registry.detect(repo_root);

        let mut agents: Vec<AgentEntry> = registry
            .list()
            .into_iter()
            .map(|name| AgentEntry {
                display_name: registry
                    .get(name)
                    .map_or_else(|| name.to_string(), |a| a.display_name().to_string()),
                detected: detected.contains(&name),
                hooks_installed: installed.contains(&name),
                name: name.to_string(),
            })
            .collect();
        agents.sort_by(|a, b| a.name.cmp(&b.name));

        let (sessions, sessions_error) =
            match SessionStore::for_repo(repo_root).and_then(|store| store.list()) {
                Ok(list) => (list, None),
                Err(e) => (Vec::new(), Some(e.to_string())),
            };

        let totals = Totals {
            sessions: sessions.len(),
            turns: sessions.iter().map(|s| s.turn_count).sum(),
            files_touched: sessions.iter().map(|s| s.files_touched.len()).sum(),
        };

        StatusJson {
            agents,
            sessions: sessions
                .into_iter()
                .map(|s| SessionEntry {
                    active: !s.is_ended(),
                    duration: s.duration_display(),
                    session_id: s.session_id,
                    agent_display_name: s.agent_display_name,
                    view: s.view_name,
                    phase: s.phase.to_string(),
                    model: s.model,
                    agent_vendor: s.agent_vendor,
                    turn_count: s.turn_count,
                    files_touched: s.files_touched,
                    first_prompt: s.first_prompt,
                })
                .collect(),
            totals,
            sessions_error,
        }
    }
}

impl Command for AgentStatus {
    fn run(&self) -> CliResult<()> {
        let repo_root = find_repository_root()?;

        let registry = AgentRegistry::with_defaults();

        if self.json {
            println!(
                "{}",
                serde_json::to_string(&self.to_json(&repo_root, &registry))
                    .expect("status is plain data and always serializes")
            );
            return Ok(());
        }

        // Installed agents

        println!("Agent Integration Status");
        println!("=======================");
        println!();

        let installed = registry.installed(&repo_root);
        let detected = registry.detect(&repo_root);

        if installed.is_empty() {
            println!("  Hooks: not installed");
            println!();
            if !detected.is_empty() {
                println!(
                    "  Detected agent{}: {}",
                    if detected.len() == 1 { "" } else { "s" },
                    detected.join(", "),
                );
                println!();
                println!("  Run 'atomic agent enable' to start recording agent turns.");
            } else {
                println!("  No agents detected in this repository.");
                println!(
                    "  Create a .claude/ directory (or similar) and run 'atomic agent enable'."
                );
            }
            println!();
            return Ok(());
        }

        // Show installed agents
        for agent_name in &installed {
            if let Some(agent) = registry.get(agent_name) {
                println!("  ✓ {} — hooks installed", agent.display_name());
            }
        }

        // Show detected but not installed agents
        for agent_name in &detected {
            if !installed.contains(agent_name) {
                if let Some(agent) = registry.get(agent_name) {
                    println!(
                        "  ○ {} — detected but hooks not installed",
                        agent.display_name()
                    );
                }
            }
        }

        println!();

        // Sessions

        let session_store = match SessionStore::for_repo(&repo_root) {
            Ok(store) => store,
            Err(e) => {
                println!("  Sessions: error loading ({e})");
                println!();
                return Ok(());
            }
        };

        let sessions = match session_store.list() {
            Ok(s) => s,
            Err(e) => {
                println!("  Sessions: error listing ({e})");
                println!();
                return Ok(());
            }
        };

        if sessions.is_empty() {
            println!("  Sessions: none");
            println!();
            println!("  Start using your AI agent — turns will be recorded automatically.");
            println!();
            return Ok(());
        }

        // Separate active and ended sessions
        let active: Vec<_> = sessions.iter().filter(|s| !s.is_ended()).collect();
        let ended: Vec<_> = sessions.iter().filter(|s| s.is_ended()).collect();

        // Active sessions
        if !active.is_empty() {
            println!(
                "  Active session{}:",
                if active.len() == 1 { "" } else { "s" }
            );
            for session in &active {
                println!(
                    "    ● {} ({}, {} turn{}, {})",
                    session.session_id,
                    session.agent_display_name,
                    session.turn_count,
                    if session.turn_count == 1 { "" } else { "s" },
                    session.duration_display(),
                );
                if self.verbose {
                    println!("      View: {}", session.view_name);
                    println!("      Phase: {}", session.phase);
                    if !session.model.is_empty() {
                        println!("      Model: {}", session.model);
                    }
                    if !session.agent_vendor.is_empty() {
                        println!("      Vendor: {}", session.agent_vendor);
                    }
                    if let Some(ref prompt) = session.first_prompt {
                        println!("      First prompt: \"{}\"", prompt);
                    }
                    if !session.files_touched.is_empty() {
                        println!(
                            "      Files touched: {} ({})",
                            session.files_touched.len(),
                            session
                                .files_touched
                                .iter()
                                .take(5)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        if session.files_touched.len() > 5 {
                            println!("        ... and {} more", session.files_touched.len() - 5);
                        }
                    }
                    if let Some(ref path) = session.transcript_path {
                        println!("      Transcript: {}", path.display());
                    }
                    println!(
                        "      View turn: atomic change -p --view {}",
                        session.view_name
                    );
                    println!();
                }
            }
            if !self.verbose {
                println!();
            }
        }

        // Ended sessions
        if !ended.is_empty() {
            let show_count = if self.verbose { ended.len() } else { 5 };

            println!(
                "  Recent session{} ({} total):",
                if ended.len() == 1 { "" } else { "s" },
                ended.len(),
            );
            for session in ended.iter().take(show_count) {
                println!(
                    "    ○ {} ({}, {} turn{}, {})",
                    session.session_id,
                    session.agent_display_name,
                    session.turn_count,
                    if session.turn_count == 1 { "" } else { "s" },
                    session.duration_display(),
                );
                if self.verbose {
                    println!("      View: {}", session.view_name);
                    if !session.model.is_empty() {
                        println!("      Model: {}", session.model);
                    }
                    if let Some(ref prompt) = session.first_prompt {
                        println!("      First prompt: \"{}\"", prompt);
                    }
                    if !session.files_touched.is_empty() {
                        println!("      Files touched: {}", session.files_touched.len());
                    }
                    println!(
                        "      View turn: atomic change -p --view {}",
                        session.view_name
                    );
                    println!();
                }
            }
            if !self.verbose && ended.len() > show_count {
                println!(
                    "    ... and {} more (use --verbose to see all)",
                    ended.len() - show_count,
                );
            }
            println!();
        }

        // Summary

        let total_turns: u32 = sessions.iter().map(|s| s.turn_count).sum();
        let total_files: usize = sessions.iter().map(|s| s.files_touched.len()).sum();

        println!(
            "  Total: {} session{}, {} turn{}, {} file{} touched",
            sessions.len(),
            if sessions.len() == 1 { "" } else { "s" },
            total_turns,
            if total_turns == 1 { "" } else { "s" },
            total_files,
            if total_files == 1 { "" } else { "s" },
        );
        println!();

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_default_for_test() {
        let cmd = AgentStatus::default_for_test();
        assert!(!cmd.verbose);
    }

    #[test]
    fn test_status_verbose_flag() {
        let cmd = AgentStatus {
            verbose: true,
            json: false,
        };
        assert!(cmd.verbose);
    }

    fn json_cmd() -> AgentStatus {
        AgentStatus {
            verbose: false,
            json: true,
        }
    }

    fn temp_repo() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".atomic")).unwrap();
        dir
    }

    /// Every registry agent is listed, whatever its state. A caller deciding
    /// whether to install has to tell "Atomic does not know this agent" from
    /// "known, and not set up", and an omitted entry cannot say which.
    ///
    /// Deliberately does not assert that a bare repo detects nothing: several
    /// agents are configured in `$HOME` rather than the repository, so what a
    /// fixture detects depends on the machine running the test.
    #[test]
    fn json_lists_every_registry_agent() {
        let dir = temp_repo();
        let registry = AgentRegistry::with_defaults();
        let out = json_cmd().to_json(dir.path(), &registry);

        assert_eq!(out.agents.len(), registry.list().len());
        assert!(
            out.agents.iter().any(|a| a.name == "claude-code"),
            "the registry's own names must be the keys callers match on"
        );

        let mut names: Vec<&str> = out.agents.iter().map(|a| a.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), out.agents.len(), "no agent listed twice");
    }

    /// The two flags must say what the registry says — that is the whole
    /// contract, since callers gate installs on `hooks_installed`.
    #[test]
    fn json_flags_agree_with_the_registry() {
        let dir = temp_repo();
        let registry = AgentRegistry::with_defaults();
        let installed = registry.installed(dir.path());
        let detected = registry.detect(dir.path());

        for entry in json_cmd().to_json(dir.path(), &registry).agents {
            assert_eq!(
                entry.hooks_installed,
                installed.contains(&entry.name.as_str()),
                "hooks_installed disagrees for {}",
                entry.name
            );
            assert_eq!(
                entry.detected,
                detected.contains(&entry.name.as_str()),
                "detected disagrees for {}",
                entry.name
            );
        }
    }

    /// Stable order, so a caller diffing two runs sees real changes only.
    #[test]
    fn json_agents_are_sorted_by_name() {
        let dir = temp_repo();
        let out = json_cmd().to_json(dir.path(), &AgentRegistry::with_defaults());

        let names: Vec<&str> = out.agents.iter().map(|a| a.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn json_reports_sessions_totals_and_which_are_active() {
        use atomic_agent::turn::session::AgentSession;

        let dir = temp_repo();
        let store = SessionStore::for_repo(dir.path()).unwrap();

        store
            .save(&AgentSession::new(
                "sess-live",
                "claude-code",
                "Claude Code",
            ))
            .unwrap();

        let mut ended = AgentSession::new("sess-done", "claude-code", "Claude Code");
        ended.phase = atomic_agent::turn::phase::Phase::Ended;
        ended.ended_at = Some(chrono::Utc::now());
        ended.turn_count = 4;
        store.save(&ended).unwrap();

        let out = json_cmd().to_json(dir.path(), &AgentRegistry::with_defaults());

        assert_eq!(out.totals.sessions, 2);
        assert_eq!(out.totals.turns, 4);
        assert!(out.sessions_error.is_none());

        let live = out
            .sessions
            .iter()
            .find(|s| s.session_id == "sess-live")
            .expect("the active session is present");
        assert!(live.active);
        let done = out
            .sessions
            .iter()
            .find(|s| s.session_id == "sess-done")
            .expect("the ended session is present");
        assert!(!done.active);
        assert_eq!(done.turn_count, 4);
    }

    /// The payload has to round-trip as JSON, since that is the only reason it
    /// exists — and `to_string` is called with `expect` in the command.
    #[test]
    fn json_payload_serializes() {
        let dir = temp_repo();
        let out = json_cmd().to_json(dir.path(), &AgentRegistry::with_defaults());
        let text = serde_json::to_string(&out).expect("serializes");

        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(parsed["agents"].is_array());
        assert!(parsed["totals"]["sessions"].is_number());
        // Absent rather than null, so consumers can test for presence.
        assert!(parsed.get("sessions_error").is_none());
    }

    #[test]
    fn test_session_store_for_temp_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".atomic")).unwrap();

        let store = SessionStore::for_repo(dir.path()).unwrap();
        let sessions = store.list().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_session_store_with_sessions() {
        use atomic_agent::turn::session::AgentSession;

        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".atomic")).unwrap();

        let store = SessionStore::for_repo(dir.path()).unwrap();

        // Create some sessions
        let s1 = AgentSession::new("sess-1", "claude-code", "Claude Code");
        store.save(&s1).unwrap();

        let mut s2 = AgentSession::new("sess-2", "claude-code", "Claude Code");
        s2.phase = atomic_agent::turn::phase::Phase::Ended;
        s2.ended_at = Some(chrono::Utc::now());
        s2.turn_count = 5;
        store.save(&s2).unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 2);

        let active = store.find_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id, "sess-1");

        let ended = store.find_ended().unwrap();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].session_id, "sess-2");
        assert_eq!(ended[0].turn_count, 5);
    }
}
