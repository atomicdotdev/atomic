//! `atomic agent hooks` — internal hook handler invoked by agent callbacks.
//!
//!
//! This is the command that agent hooks (installed in `.claude/settings.json`,
//! `.gemini/settings.json`, etc.) call back to. It is **hidden** from `--help`
//! because users never invoke it directly.
//!
//! # Invocation
//!
//! ```text
//! atomic agent hooks <agent-name> <verb> < /dev/stdin
//! ```
//!
//! For example, Claude Code's `stop` hook runs:
//!
//! ```text
//! atomic agent hooks claude-code stop
//! ```
//!
//! With JSON on stdin:
//!
//! ```json
//! {"session_id": "abc-123", "transcript_path": "/tmp/t.jsonl"}
//! ```
//!
//! # Processing Flow
//!
//! 1. Read raw JSON bytes from stdin
//! 2. Look up the agent adapter in the `AgentRegistry`
//! 3. Map the CLI verb to a `HookType` via `HookType::from_verb()`
//! 4. Call `agent.parse_event(hook_type, input)` → `TurnEvent`
//! 5. Create a `TurnOrchestrator` for the repository
//! 6. Call `orchestrator.dispatch(event)` → `DispatchResult`
//! 7. If the result has a `message`, write it as JSON to stdout
//!    (agents like Claude Code read JSON responses from hook stdout)
//!
//! # Error Handling
//!
//! Errors are written to stderr. The command exits with code 0 even on
//! non-fatal errors so that the agent continues normally. Fatal errors
//! (e.g., cannot parse stdin) exit with a non-zero code.
//!
//! # Stdout JSON Response
//!
//! Some agents (Claude Code) read JSON from hook stdout to display
//! system messages. When the orchestrator returns a `message`, we write:
//!
//! ```json
//! {"systemMessage": "Atomic is tracking this session..."}
//! ```

use std::io::{Read, Write};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;

use anyhow::anyhow;
use clap::Args;

use atomic_agent::event::HookType;
use atomic_agent::hooks::AgentRegistry;
use atomic_core::change::session::{IncompleteSession, SessionIncompleteOrigin};
use atomic_core::types::Base32;

use crate::commands::git::guard::GuardOperation;
use crate::commands::git::wip::{capture_or_reuse_tracked_wip, WipCaptureRequest};
use atomic_repository::{Repository, WorkspaceTxnMode, WorkspaceTxnStart};

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

// Hooks Command

/// Internal hook handlers (called by agent hooks, not by users).
///
/// This command is hidden from `--help`. It is invoked by hooks installed
/// in agent configuration files (e.g., `.claude/settings.json`).
///
/// # Usage
///
/// ```text
/// atomic agent hooks <agent> <verb>
/// ```
///
/// Where `<agent>` is the agent registry key (e.g., `claude-code`) and
/// `<verb>` is the hook verb (e.g., `stop`, `user-prompt-submit`).
#[derive(Debug, Args)]
pub struct Hooks {
    /// The agent name (e.g., "claude-code", "gemini-cli").
    agent_name: String,

    /// The hook verb (e.g., "stop", "user-prompt-submit", "session-start").
    verb: String,

    /// Run the hook body in this process.
    #[arg(long, hide = true)]
    foreground: bool,

    /// Print one JSON result line for callers that need the recorded change hash.
    #[arg(long, hide = true)]
    json: bool,
}

/// JSON emitted by `--json`.
#[derive(Debug, serde::Serialize)]
struct HookJsonResult {
    session_id: String,
    recorded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    change_hash: Option<String>,
    /// The view the change was recorded onto (the per-session agent view), so a
    /// caller can locate it — the change lives here, not on the default view.
    #[serde(skip_serializing_if = "Option::is_none")]
    view: Option<String>,
    files: Vec<String>,
    /// The managed run this hook participated in, when a lifecycle governs it.
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    /// Present only when recording was refused after durable WIP capture.
    #[serde(skip_serializing_if = "Option::is_none")]
    incomplete: Option<IncompleteSession>,
}

impl Command for Hooks {
    fn run(&self) -> CliResult<()> {
        // Read stdin (agent sends JSON here)
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input).map_err(|e| {
            CliError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to read hook input from stdin: {}", e),
            ))
        })?;

        // Resolve managed ownership before considering Codex's background
        // handoff. A governed ending stays in the foreground so a typed guard
        // refusal can reach the lifecycle caller.
        let mut managed = super::lifecycle::find_governing_lifecycle_for_hook(&self.agent_name);
        if self.agent_name == "opencode" && self.verb == "file-snapshot" {
            let value: serde_json::Value =
                serde_json::from_slice(&input).map_err(|e| CliError::Internal(anyhow!(e)))?;
            let cwd = value
                .get("cwd")
                .and_then(|v| v.as_str())
                .ok_or_else(|| CliError::Internal(anyhow!("file-snapshot requires cwd")))?;
            let extra: Vec<String> = serde_json::from_value(
                value
                    .get("paths")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!([])),
            )
            .map_err(|e| CliError::Internal(anyhow!(e)))?;
            let snapshot = atomic_agent::record::scope::snapshot(std::path::Path::new(cwd), &extra)
                .map_err(|e| CliError::Internal(anyhow!(e)))?;
            println!("{}", snapshot);
            return Ok(());
        }

        if self.should_handoff_codex_lifecycle(managed.is_some()) {
            return self.handoff_codex_lifecycle(&input);
        }

        // Look up the agent adapter
        let registry = AgentRegistry::with_defaults();
        let agent = registry
            .require(&self.agent_name)
            .map_err(|e| CliError::InvalidArgument {
                message: format!("Unknown agent '{}': {}", self.agent_name, e),
            })?;

        // Map the CLI verb to a HookType
        let hook_type =
            HookType::from_verb(&self.verb).ok_or_else(|| CliError::InvalidArgument {
                message: format!(
                    "Unknown hook verb '{}' for agent '{}'. Known verbs: {}",
                    self.verb,
                    self.agent_name,
                    agent.hook_verbs().join(", "),
                ),
            })?;

        // Parse the agent-specific JSON into a common TurnEvent
        let event = agent.parse_event(hook_type, &input).map_err(|e| {
            // Log to stderr but don't fail hard on parse errors for
            // non-critical hooks (tool use events)
            if hook_type.is_tool_use() {
                eprintln!(
                    "[atomic] Warning: failed to parse {} {} input: {}",
                    self.agent_name, self.verb, e
                );
                // Return a generic event so we can continue
                return CliError::Internal(anyhow!("Hook parse failed: {}", e));
            }
            CliError::Internal(anyhow!(
                "Failed to parse hook input for {} {}: {}",
                self.agent_name,
                self.verb,
                e
            ))
        })?;

        // Find the repository root. Agents whose hooks run detached from the
        // workspace (e.g., Antigravity plugin hooks, which execute with the
        // plugin directory as cwd) supply candidate directories from the
        // event payload; everyone else resolves from the process cwd.
        let repo_root = match agent.repo_root_hints(&event) {
            Some(hints) => {
                let resolved = hints
                    .iter()
                    .find_map(|dir| crate::commands::find_repository_root_from(dir).ok());
                match resolved {
                    Some(root) => root,
                    None => {
                        // The workspace that fired this hook is not an Atomic
                        // repository — nothing to record. Exit quietly (with
                        // the agent's stdout contract satisfied) so the hook
                        // never shows up as a failure in the agent's UI.
                        if let Some(response) = agent.stdout_response(hook_type) {
                            println!("{}", response);
                        }
                        return Ok(());
                    }
                }
            }
            None => find_repository_root().map_err(|e| {
                // Log to stderr — the agent should continue even if Atomic can't find a repo
                eprintln!("[atomic] Warning: {}", e);
                e
            })?,
        };

        // Hooks are never suppressed under a managed run: sessions adopt its
        // declared view and correlation stamp.
        if let Some(lc) = &managed {
            log::info!(
                "hook {} {} participates in managed run {} (owner: {}, view: {})",
                self.agent_name,
                self.verb,
                lc.run_id,
                lc.owner_agent,
                lc.view.as_deref().unwrap_or("-"),
            );
        }

        // Terminal hooks hold cross-process publication coordination BEFORE
        // the workspace boundary guard (dev-sync ordering): a Stop blocked on
        // a transient writer must be observable as holding its publication
        // lock, and a killed Stop releases it by process death alone.
        let _publication_guard = if matches!(hook_type, HookType::TurnEnd | HookType::SessionEnd) {
            hold_turn_publication_lock(&repo_root)?
        } else {
            None
        };

        // CB-0C owns the only stale-baseline classification at agent ending
        // boundaries. It runs before orchestrator construction, so no watcher
        // flush, status, view alignment, record, provenance, or attestation can
        // happen first. Refusal requests capture-or-reuse WIP for both managed
        // and direct agent sessions.
        let boundary_refusal =
            guard_agent_boundary(&repo_root, &mut managed, &event.session_id, hook_type)?;

        // Create a tokio runtime for the async orchestrator
        // Hook handlers are short-lived — one runtime per invocation is fine.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CliError::Internal(anyhow!("Failed to create async runtime: {}", e)))?;

        let agent_name = agent.name().to_string();
        let agent_display = agent.display_name().to_string();

        // Keep hooks quiet on stdout/stderr — Claude Code surfaces both, so
        // route diagnostics through `log` instead.
        log::debug!(
            "hooks: agent={} display={} verb={}",
            agent_name,
            agent_display,
            self.verb
        );

        let result = rt.block_on(async {
            // Create the orchestrator
            let mut orchestrator =
                atomic_agent::turn::orchestrator::TurnOrchestrator::new(&repo_root)
                    .await
                    .map_err(|e| {
                        CliError::Internal(anyhow!("Failed to create orchestrator: {}", e))
                    })?;

            // Set the agent identity so new sessions get the correct name
            // (e.g., "claude-code" / "Claude Code" instead of "unknown")
            orchestrator.set_agent(&agent_name, &agent_display);
            orchestrator
                .set_journal_sink(Arc::new(super::owner::OwnerJournalSink::new(&repo_root)));
            if _publication_guard.is_some() {
                orchestrator.set_publication_lock_held();
            }

            // Under a managed lifecycle, sessions adopt the declared view
            // and carry the run stamp (see lifecycle module docs).
            if let Some(lc) = &managed {
                orchestrator.set_managed_run(lc.to_managed_run_context());
            }
            if let Some(refusal) = boundary_refusal {
                orchestrator.set_boundary_refusal(refusal);
            }

            // Dispatch the event through the state machine → watcher → recorder
            let dispatch_result = orchestrator
                .dispatch(event)
                .await
                .map_err(|e| CliError::Internal(anyhow!("Failed to dispatch hook event: {}", e)))?;

            Ok::<_, CliError>(dispatch_result)
        })?;

        // Log warnings via log crate (not stderr — that leaks into agent TUIs)
        for warning in &result.warnings {
            log::warn!("{}", warning);
        }

        // Log recording info at debug level
        if let Some(ref outcome) = result.change_recorded {
            log::debug!("{}", outcome);
        }

        // Keep hook stdout quiet by default; some agent TUIs display it directly.
        if let Some(ref message) = result.message {
            log::debug!("Hook response: {}", message);
        }

        // Some agents (e.g., Antigravity) read a JSON object from hook stdout
        // as part of their hook contract. Print the adapter's response body.
        if let Some(response) = agent.stdout_response(hook_type) {
            println!("{}", response);
        }

        // Emit the recorded change hash for UI/bridge callers.
        if self.json {
            let run_id = managed.as_ref().map(|lc| lc.run_id.clone());
            let json = if result.incomplete.is_some() {
                HookJsonResult {
                    session_id: result.session_id.clone(),
                    recorded: false,
                    change_hash: None,
                    view: result.view.clone(),
                    files: Vec::new(),
                    run_id,
                    incomplete: result.incomplete.clone(),
                }
            } else {
                match &result.change_recorded {
                    Some(outcome) => HookJsonResult {
                        session_id: result.session_id.clone(),
                        recorded: true,
                        change_hash: Some(outcome.hash.to_base32()),
                        view: result.view.clone(),
                        files: outcome.recorded_file_list().to_vec(),
                        run_id,
                        incomplete: None,
                    },
                    None => HookJsonResult {
                        session_id: result.session_id.clone(),
                        recorded: false,
                        change_hash: None,
                        view: result.view.clone(),
                        files: Vec::new(),
                        run_id,
                        incomplete: None,
                    },
                }
            };
            match serde_json::to_string(&json) {
                Ok(s) => println!("{}", s),
                Err(e) => log::warn!("Failed to serialize hook JSON result: {}", e),
            }
        }

        if let Some(outcome) = result.incomplete {
            return Err(CliError::ManagedAgentIncomplete {
                session_id: result.session_id,
                outcome,
            });
        }

        Ok(())
    }
}

/// Cross-process publication coordination for terminal hooks.
///
/// The orchestrator acquires the same `turn-publication.lock` inside
/// `handle_turn_end`; acquiring it HERE (before the workspace boundary
/// guard) mirrors the dev-sync ordering: a Stop blocked on a transient
/// writer must still be observable as holding its publication lock, and a
/// killed Stop must release it by process death alone.
pub(crate) struct TurnPublicationLock(std::fs::File);
pub(crate) fn hold_turn_publication_lock(
    repository_root: &std::path::Path,
) -> CliResult<Option<TurnPublicationLock>> {
    use fs2::FileExt;
    let canonical = match atomic_repository::Repository::canonical_dot_dir(repository_root) {
        Ok(dot) => dot,
        Err(_) => return Ok(None),
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(canonical.join("turn-publication.lock"))
        .map_err(|error| CliError::Internal(anyhow!("failed to open publication lock: {error}")))?;
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(10);
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(Some(TurnPublicationLock(file))),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
            {
                if start.elapsed() >= timeout {
                    return Err(CliError::Internal(anyhow!(
                        "timed out waiting for another Stop to publish; retry this Stop"
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => {
                return Err(CliError::Internal(anyhow!(
                    "publication lock unavailable: {error}"
                )))
            }
        }
    }
}

pub(crate) fn guard_agent_boundary(
    repository_root: &std::path::Path,
    managed: &mut Option<super::lifecycle::ManagedLifecycle>,
    session_id: &str,
    hook_type: HookType,
) -> CliResult<Option<IncompleteSession>> {
    let operation = match hook_type {
        HookType::TurnEnd => GuardOperation::AgentTurnEnd,
        HookType::SessionEnd => GuardOperation::AgentSessionEnd,
        _ => return Ok(None),
    };

    // A managed run is terminal after its first refusal. Reuse that exact
    // recovery object rather than reclassifying a filesystem that may already
    // be under remediation.
    if let Some(existing) = managed
        .as_ref()
        .and_then(|lifecycle| lifecycle.refusal.as_ref())
    {
        return Ok(Some(existing.outcome.clone()));
    }

    // A transient writer (stop checkpoint publication, recording) must not
    // fail the guard: wait for it inside the opener so the stop hook can
    // acquire its publication coordination first (dev-sync behavior).
    let mut repository = Repository::open_for_workspace_transaction_wait(
        repository_root,
        std::time::Duration::from_secs(10),
    )
    .map_err(|error| CliError::StaleBaseline {
        report: format!(
            "Unsafe operation: {operation}\nGuard failed before agent boundary: {error}"
        ),
    })?;
    let boundary_start = repository
        .begin_workspace_txn(WorkspaceTxnMode::Reconcile)
        .map_err(|error| CliError::StaleBaseline {
            report: format!(
                "Unsafe operation: {operation}\nGuard failed before agent boundary: {error}"
            ),
        })?;

    match boundary_start {
        WorkspaceTxnStart::Ready(_workspace) => {
            // Safe baseline: the turn proceeds to the orchestrator.
            //
            // A session ending for a session Atomic never saw is the last
            // chance to preserve pending tracked work: no session-start ever
            // ran, so no pre-session capture exists and the ending must not
            // quietly discard unattributed bytes. Preservation failure is
            // never an empty successful turn — it persists a durable
            // incomplete reason and ends non-zero. Successful preservation of
            // pending work is evidence-only: nothing was imported, projected,
            // or recorded, so the ending reports incomplete rather than
            // claiming success.
            if hook_type == HookType::SessionEnd
                && !agent_session_exists(repository_root, session_id)?
            {
                let capture = capture_or_reuse_tracked_wip(WipCaptureRequest::new(
                    repository_root,
                    session_id,
                    "agent-lifecycle-boundary",
                ));
                match capture {
                    Ok(evidence) if evidence.paths.is_empty() => {
                        // Nothing pending — a quiet ending claims nothing.
                        return Ok(None);
                    }
                    Ok(evidence) => {
                        return Ok(Some(IncompleteSession::new(
                            format!(
                                "Unsafe operation: agent session-end\n\
                                 Ending session with pending tracked work preserved as \
                                 evidence only (recovery ref {}); nothing was recorded, \
                                 imported, or claimed",
                                evidence.ref_name
                            ),
                            evidence
                                .paths
                                .iter()
                                .map(|path| String::from_utf8_lossy(path).into_owned()),
                            evidence.ref_name,
                            SessionIncompleteOrigin::UnknownPostCheckout,
                        )));
                    }
                    Err(wip_error) => {
                        return Ok(Some(IncompleteSession::new(
                            format!(
                                "Unsafe operation: agent session-end\n\
                                 WIP preservation failed before the session ended: {wip_error}\n\
                                 No import, projection, or recording may be claimed"
                            ),
                            Vec::<String>::new(),
                            "",
                            SessionIncompleteOrigin::UnknownPostCheckout,
                        )));
                    }
                }
            }
            Ok(None)
        }
        WorkspaceTxnStart::Remediation(remediation) => {
            // Refused: classify with the typed remediation and preserve
            // pending work as capture-or-reuse WIP evidence. Never import,
            // project, or claim a successful recording during a Git-owned
            // partial operation.
            let report = format!(
                "Unsafe operation: {operation}\nRefusal: {}",
                remediation.describe()
            );
            let incomplete = match capture_or_reuse_tracked_wip(WipCaptureRequest::new(
                repository_root,
                session_id,
                "agent-lifecycle-boundary",
            )) {
                Ok(recovery) => IncompleteSession::new(
                    report,
                    recovery
                        .paths
                        .iter()
                        .map(|path| String::from_utf8_lossy(path).into_owned()),
                    recovery.ref_name,
                    SessionIncompleteOrigin::UnknownPostCheckout,
                ),
                Err(wip_error) => {
                    // Preservation itself failed: persist the durable
                    // incomplete reason (no WIP ref exists) and end the turn
                    // non-zero. No import, projection, or recording may be
                    // claimed.
                    IncompleteSession::new(
                        format!(
                            "{report}\nWIP preservation failed before the turn ended: {wip_error}"
                        ),
                        Vec::<String>::new(),
                        "",
                        SessionIncompleteOrigin::UnknownPostCheckout,
                    )
                }
            };

            if let Some(lifecycle) = managed.as_mut() {
                *lifecycle = super::lifecycle::persist_refusal(
                    repository_root,
                    lifecycle,
                    session_id,
                    incomplete.clone(),
                )?;
                return Ok(lifecycle
                    .refusal
                    .as_ref()
                    .map(|refusal| refusal.outcome.clone()));
            }
            Ok(Some(incomplete))
        }
    }
}

/// Whether a durable session record exists for `session_id` in the canonical
/// session store. Loading never creates state.
fn agent_session_exists(repository_root: &std::path::Path, session_id: &str) -> CliResult<bool> {
    let store = match atomic_repository::Repository::canonical_dot_dir(repository_root) {
        Ok(dot_dir) => atomic_agent::turn::session::SessionStore::new(dot_dir.join("sessions"))
            .map_err(|e| CliError::Internal(anyhow!("session store unavailable: {}", e)))?,
        Err(_) => atomic_agent::turn::session::SessionStore::for_repo(repository_root)
            .map_err(|e| CliError::Internal(anyhow!("session store unavailable: {}", e)))?,
    };
    store
        .load(session_id)
        .map(|loaded| loaded.is_some())
        .map_err(|e| CliError::Internal(anyhow!("cannot load session {}: {}", session_id, e)))
}

impl Hooks {
    fn should_handoff_codex_lifecycle(&self, managed: bool) -> bool {
        // Codex clamps SessionEnd hooks to three seconds. Managed lifecycles
        // stay foreground so WIP-backed refusal can reach the original caller.
        !managed
            && !self.foreground
            && !self.json
            && self.agent_name == "codex"
            && matches!(self.verb.as_str(), "stop" | "session-end")
    }

    fn handoff_codex_lifecycle(&self, input: &[u8]) -> CliResult<()> {
        let exe = std::env::current_exe().map_err(|e| {
            CliError::Internal(anyhow!("Failed to resolve atomic executable: {}", e))
        })?;

        let mut child = ProcessCommand::new(exe)
            .arg("agent")
            .arg("hooks")
            .arg(&self.agent_name)
            .arg(&self.verb)
            .arg("--foreground")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                CliError::Internal(anyhow!(
                    "Failed to start background Codex {} worker: {}",
                    self.verb,
                    e,
                ))
            })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input).map_err(|e| {
                CliError::Io(std::io::Error::new(
                    e.kind(),
                    format!(
                        "Failed to pass Codex {} input to background worker: {}",
                        self.verb, e,
                    ),
                ))
            })?;
        }

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use git2::{Oid, Repository as GitRepository, Signature};
    use std::{fs, path::Path};

    use crate::commands::git::checkpoint::{self, VerifiedCheckpointInput};

    fn commit_file(repository: &GitRepository, root: &Path, content: &[u8], message: &str) -> Oid {
        fs::write(root.join("tracked.txt"), content).unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_oid).unwrap();
        let signature = Signature::now("Atomic Test", "test@atomic.invalid").unwrap();
        let parents = repository
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| repository.find_commit(oid).unwrap());
        match parents.as_ref() {
            Some(parent) => repository
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    message,
                    &tree,
                    &[parent],
                )
                .unwrap(),
            None => repository
                .commit(Some("HEAD"), &signature, &signature, message, &tree, &[])
                .unwrap(),
        }
    }

    fn initialized_colocated_repository(root: &Path) -> (GitRepository, Oid) {
        atomic_repository::Repository::init(root).unwrap();
        let git = GitRepository::init(root).unwrap();
        let head = commit_file(&git, root, b"checkpoint\n", "checkpoint");
        let tree = git.find_commit(head).unwrap().tree_id();
        let atomic = checkpoint::observe_current_atomic(root).unwrap();
        checkpoint::write_verified_checkpoint(
            root,
            VerifiedCheckpointInput {
                view: &atomic.view,
                atomic_state: &atomic.state,
                git_head: &head.to_string(),
                git_tree: &tree.to_string(),
            },
        )
        .unwrap();
        (git, head)
    }

    fn managed_lifecycle(root: &Path) -> super::super::lifecycle::ManagedLifecycle {
        let now = Utc::now().timestamp();
        super::super::lifecycle::ManagedLifecycle {
            run_id: "run-cb0c".into(),
            owner_agent: "sherpa".into(),
            owner_session_id: "owner-session".into(),
            executor_agent: Some("codex".into()),
            work_item_id: Some("CB-0C".into()),
            view: Some("agent-view".into()),
            refusal: None,
            workdir: root.to_path_buf(),
            created_at: now,
            updated_at: now,
            expires_at: now + 60,
            stop_state: None,
        }
    }

    fn save_managed_lifecycle(root: &Path, lifecycle: &super::super::lifecycle::ManagedLifecycle) {
        let directory = root.join(".atomic/agent-lifecycle");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join(format!("{}.json", lifecycle.run_id)),
            serde_json::to_vec_pretty(lifecycle).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn test_hooks_struct_fields() {
        let hooks = Hooks {
            agent_name: "claude-code".to_string(),
            verb: "stop".to_string(),
            foreground: false,
            json: false,
        };
        assert_eq!(hooks.agent_name, "claude-code");
        assert_eq!(hooks.verb, "stop");
    }

    #[test]
    fn test_hook_type_from_verb_all_claude_verbs() {
        let verbs_and_types = vec![
            ("session-start", HookType::SessionStart),
            ("session-end", HookType::SessionEnd),
            ("stop", HookType::TurnEnd),
            ("user-prompt-submit", HookType::TurnStart),
            ("pre-task", HookType::PreToolUse),
            ("post-task", HookType::PostToolUse),
            ("post-todo", HookType::PostToolUse),
        ];

        for (verb, expected) in verbs_and_types {
            let hook_type = HookType::from_verb(verb);
            assert_eq!(
                hook_type,
                Some(expected),
                "Verb '{}' should map to {:?}",
                verb,
                expected,
            );
        }
    }

    #[test]
    fn test_hook_type_from_verb_all_gemini_verbs() {
        let verbs_and_types = vec![
            ("session-start", HookType::SessionStart),
            ("session-end", HookType::SessionEnd),
            ("before-agent", HookType::TurnStart),
            ("after-agent", HookType::TurnEnd),
            ("before-tool", HookType::PreToolUse),
            ("after-tool", HookType::PostToolUse),
        ];

        for (verb, expected) in verbs_and_types {
            let hook_type = HookType::from_verb(verb);
            assert_eq!(
                hook_type,
                Some(expected),
                "Verb '{}' should map to {:?}",
                verb,
                expected,
            );
        }
    }

    #[test]
    fn test_hook_type_from_verb_unknown() {
        assert_eq!(HookType::from_verb("unknown-verb"), None);
        assert_eq!(HookType::from_verb(""), None);
        assert_eq!(HookType::from_verb("STOP"), None); // case-sensitive
    }

    #[test]
    fn test_agent_registry_has_claude_code() {
        let registry = AgentRegistry::with_defaults();
        assert!(registry.get("claude-code").is_some());
    }

    #[test]
    fn test_agent_registry_require_unknown_fails() {
        let registry = AgentRegistry::with_defaults();
        let err = registry.require("nonexistent");
        assert!(err.is_err());
    }

    #[test]
    fn test_json_response_format() {
        // Verify the JSON response format matches what Claude Code expects
        let message = "Atomic is tracking this session.";
        let response = serde_json::json!({
            "systemMessage": message
        });
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("systemMessage"));
        assert!(json.contains("Atomic is tracking this session."));

        // Parse it back to verify structure
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["systemMessage"].as_str(),
            Some("Atomic is tracking this session.")
        );
    }

    #[test]
    fn test_json_response_no_message() {
        // When there's no message, we should not produce output.
        // This tests the logic: if message.is_none(), don't println.
        let result = atomic_agent::turn::orchestrator::DispatchResult::new(
            "sess-1",
            atomic_agent::turn::phase::Phase::Idle,
        );
        assert!(result.message.is_none());
    }

    #[test]
    fn test_hooks_debug() {
        let hooks = Hooks {
            agent_name: "claude-code".to_string(),
            verb: "session-start".to_string(),
            foreground: false,
            json: false,
        };
        let debug = format!("{:?}", hooks);
        assert!(debug.contains("claude-code"));
        assert!(debug.contains("session-start"));
    }

    #[test]
    fn test_codex_stop_handoffs_by_default() {
        let hooks = Hooks {
            agent_name: "codex".to_string(),
            verb: "stop".to_string(),
            foreground: false,
            json: false,
        };

        assert!(hooks.should_handoff_codex_lifecycle(false));
        assert!(
            !hooks.should_handoff_codex_lifecycle(true),
            "managed Codex endings must stay foreground so refusal propagates"
        );
    }

    #[test]
    fn test_codex_stop_json_disables_handoff() {
        let hooks = Hooks {
            agent_name: "codex".to_string(),
            verb: "stop".to_string(),
            foreground: false,
            json: true,
        };

        assert!(!hooks.should_handoff_codex_lifecycle(false));
    }

    #[test]
    fn test_codex_stop_foreground_disables_handoff() {
        let hooks = Hooks {
            agent_name: "codex".to_string(),
            verb: "stop".to_string(),
            foreground: true,
            json: false,
        };

        assert!(!hooks.should_handoff_codex_lifecycle(false));
    }

    #[test]
    fn test_non_codex_stop_does_not_handoff() {
        let hooks = Hooks {
            agent_name: "claude-code".to_string(),
            verb: "stop".to_string(),
            foreground: false,
            json: false,
        };

        assert!(!hooks.should_handoff_codex_lifecycle(false));
    }

    #[test]
    fn test_codex_session_end_handoffs_by_default() {
        let hooks = Hooks {
            agent_name: "codex".to_string(),
            verb: "session-end".to_string(),
            foreground: false,
            json: false,
        };

        assert!(hooks.should_handoff_codex_lifecycle(false));
        assert!(!hooks.should_handoff_codex_lifecycle(true));
    }

    #[test]
    fn test_hook_json_result_recorded() {
        let result = HookJsonResult {
            session_id: "sess-abc".to_string(),
            recorded: true,
            change_hash: Some("ABC123".to_string()),
            view: Some("bold-creek-a3f2".to_string()),
            files: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
            run_id: Some("run-42".to_string()),
            incomplete: None,
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();

        assert_eq!(parsed["session_id"], "sess-abc");
        assert_eq!(parsed["recorded"], true);
        assert_eq!(parsed["change_hash"], "ABC123");
        assert_eq!(parsed["view"], "bold-creek-a3f2");
        assert_eq!(parsed["files"][0], "src/main.rs");
        assert_eq!(parsed["files"][1], "src/lib.rs");
        assert_eq!(parsed["run_id"], "run-42");
    }

    #[test]
    fn test_hook_json_result_not_recorded_omits_hash() {
        let result = HookJsonResult {
            session_id: "sess-xyz".to_string(),
            recorded: false,
            change_hash: None,
            view: None,
            files: Vec::new(),
            run_id: None,
            incomplete: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["recorded"], false);
        assert!(
            !json.contains("change_hash"),
            "change_hash must be omitted when None, got: {}",
            json
        );
        assert!(
            !json.contains("view"),
            "view must be omitted when None, got: {}",
            json
        );
        assert!(
            !json.contains("run_id"),
            "run_id must be omitted outside managed runs, got: {}",
            json
        );
        assert_eq!(parsed["files"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn managed_boundary_uses_shared_guard_and_persists_first_refusal() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let (repository, _) = initialized_colocated_repository(root);
        let current_head = commit_file(&repository, root, b"after checkout\n", "git moved");
        fs::write(root.join("tracked.txt"), b"unattributed tracked bytes\n").unwrap();

        let lifecycle = managed_lifecycle(root);
        save_managed_lifecycle(root, &lifecycle);
        let mut managed = Some(lifecycle);
        let refusal = guard_agent_boundary(root, &mut managed, "agent-session", HookType::TurnEnd)
            .unwrap()
            .expect("shared guard must refuse Git drift");

        assert_eq!(refusal.paths, vec!["tracked.txt"]);
        assert_eq!(refusal.origin, SessionIncompleteOrigin::UnknownPostCheckout);
        assert!(refusal.recovery_ref.starts_with("refs/atomic/wip/"));
        assert!(refusal.reason.contains("Unsafe operation: agent turn-end"));
        assert!(refusal.reason.contains("HeadChanged"));
        let persisted = managed
            .as_ref()
            .and_then(|lifecycle| lifecycle.refusal.as_ref())
            .expect("managed refusal must be persisted");
        assert_eq!(persisted.session_id, "agent-session");
        assert_eq!(persisted.outcome, refusal);

        let recovery = repository
            .find_reference(&refusal.recovery_ref)
            .unwrap()
            .peel_to_commit()
            .unwrap();
        assert_eq!(recovery.parent_id(0).unwrap(), current_head);
        let entry = recovery
            .tree()
            .unwrap()
            .get_path(Path::new("tracked.txt"))
            .unwrap();
        assert_eq!(
            repository.find_blob(entry.id()).unwrap().content(),
            b"unattributed tracked bytes\n"
        );

        let later = guard_agent_boundary(root, &mut managed, "agent-session", HookType::SessionEnd)
            .unwrap()
            .unwrap();
        assert_eq!(later, refusal, "the first lifecycle refusal must win");
    }

    #[test]
    fn unmanaged_boundary_reuses_wip_on_duplicate_and_crash_retry() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let (repository, _) = initialized_colocated_repository(root);
        commit_file(&repository, root, b"git moved\n", "git moved");
        fs::write(root.join("tracked.txt"), b"unattributed tracked bytes\n").unwrap();
        let mut managed = None;

        let first = guard_agent_boundary(root, &mut managed, "direct-session", HookType::TurnEnd)
            .unwrap()
            .unwrap();
        let retry =
            guard_agent_boundary(root, &mut managed, "direct-session", HookType::SessionEnd)
                .unwrap()
                .unwrap();

        assert_eq!(retry.recovery_ref, first.recovery_ref);
        assert_eq!(retry.paths, first.paths);
        assert!(repository.find_reference(&first.recovery_ref).is_ok());
    }

    #[test]
    fn agent_boundary_passes_without_git() {
        let directory = tempfile::tempdir().unwrap();
        atomic_repository::Repository::init(directory.path()).unwrap();
        let mut managed = None;

        assert!(guard_agent_boundary(
            directory.path(),
            &mut managed,
            "no-git-session",
            HookType::TurnEnd,
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn inherited_agent_view_name_mismatch_passes_shared_guard() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let (git_repo, head) = initialized_colocated_repository(root);
        let head_tree = git_repo.find_commit(head).unwrap().tree_id().to_string();
        drop(git_repo);

        let mut repository = atomic_repository::Repository::open_existing(root).unwrap();
        let parent = repository.current_view().to_string();
        repository.create_view_from("agent-view", &parent).unwrap();
        let working_copy = repository.require_working_copy_id().unwrap();
        repository
            .set_current_view(working_copy, "agent-view")
            .unwrap();
        drop(repository);

        // The routed view switch keeps the bridge checkpoint aligned with the
        // working-copy record; refresh it exactly as the routed path would.
        let atomic = atomic_repository::Repository::open_readonly(root).unwrap();
        let view_info = atomic.get_view_info("agent-view").unwrap();
        drop(atomic);
        checkpoint::write_verified_checkpoint(
            root,
            VerifiedCheckpointInput {
                view: "agent-view",
                atomic_state: &view_info.state.to_string(),
                git_head: &head.to_string(),
                git_tree: &head_tree,
            },
        )
        .unwrap();

        let mut managed = Some(managed_lifecycle(root));
        assert!(guard_agent_boundary(
            root,
            &mut managed,
            "agent-view-session",
            HookType::SessionEnd,
        )
        .unwrap()
        .is_none());
        assert!(managed.as_ref().unwrap().refusal.is_none());
    }

    #[test]
    fn test_hook_json_incomplete_has_no_false_record_data() {
        let result = HookJsonResult {
            session_id: "sess-incomplete".to_string(),
            recorded: false,
            change_hash: None,
            view: Some("managed-view".to_string()),
            files: Vec::new(),
            run_id: Some("run-42".to_string()),
            incomplete: Some(IncompleteSession::new(
                "checkout drift",
                vec!["src/lib.rs".into()],
                "refs/atomic/wip/run-42",
                atomic_core::change::session::SessionIncompleteOrigin::UnknownPostCheckout,
            )),
        };

        let json = serde_json::to_string(&result).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["recorded"], false);
        assert!(parsed.get("change_hash").is_none());
        assert_eq!(parsed["files"].as_array().unwrap().len(), 0);
        assert_eq!(
            parsed["incomplete"]["recovery_ref"],
            "refs/atomic/wip/run-42"
        );
        assert_eq!(parsed["incomplete"]["origin"], "unknown_post_checkout");
    }
}
