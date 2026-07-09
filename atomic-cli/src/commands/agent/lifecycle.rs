//! Managed lifecycle declarations for orchestrated agent runs.
//!
//! `lifecycle begin` declares a run (owner, executor, view, workdir). Hooks
//! are never suppressed: sessions created under a governing run **adopt**
//! its declared view and are **stamped** with the run context; `lifecycle
//! end --json` harvests the summary from those stamps.
//!
//! Lifecycles are keyed by `run_id` (one JSON file each) so concurrent runs
//! coexist; hooks resolve the governing run by longest-matching `workdir`.
//! The store lives in the *canonical* `.atomic` (resolved through the
//! sandbox pointer) so sandbox hooks see project-root lifecycles.
//!
//! `expires_at` is crash protection, not a session limit: a dead
//! orchestrator's lease expires on its own and is cleaned up lazily.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use atomic_agent::turn::orchestrator::ManagedRunContext;
use atomic_agent::turn::session::{ManagedRunStamp, SessionStore};
use atomic_core::types::Base32;

use crate::commands::Command;
use crate::error::{CliError, CliResult};

const LIFECYCLE_DIR: &str = "agent-lifecycle";
const DEFAULT_TTL_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct Lifecycle {
    #[command(subcommand)]
    command: LifecycleCommands,
}

#[derive(Debug, Subcommand)]
enum LifecycleCommands {
    /// Declare a managed agent run owned by an outer orchestrator.
    Begin(Begin),

    /// Renew a managed run's lease.
    Renew(Renew),

    /// End a managed run and print its summary (sessions, changes, views).
    End(End),

    /// Show active managed runs.
    Status(Status),
}

#[derive(Debug, Args)]
struct Begin {
    /// Lifecycle owner agent, e.g. "sherpa".
    #[arg(long)]
    owner: String,

    /// Owner session id.
    #[arg(long)]
    session: String,

    /// Executor agent whose hooks participate in this run (registry key of
    /// the *inner* agent, e.g. "codex", "claude-code"). When set, hooks from
    /// other non-owner agents are left untouched; when omitted, any agent
    /// inside the workdir participates.
    #[arg(long)]
    executor: Option<String>,

    /// Optional work item id to associate with the managed run.
    #[arg(long)]
    work_item: Option<String>,

    /// View the run's sessions should adopt for recording. When omitted,
    /// sessions fork their usual per-session view and are only stamped.
    #[arg(long)]
    view: Option<String>,

    /// Working directory the run covers (a sandbox path or the repo root).
    /// Hooks participate only when their cwd is inside it. Defaults to the
    /// current directory.
    #[arg(long)]
    workdir: Option<PathBuf>,

    /// Lease duration. The lease is crash protection, not a session limit.
    #[arg(long, default_value_t = DEFAULT_TTL_SECONDS)]
    ttl_seconds: i64,

    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct Renew {
    /// Run id returned by lifecycle begin.
    #[arg(long)]
    run_id: String,

    /// Lease duration from now.
    #[arg(long, default_value_t = DEFAULT_TTL_SECONDS)]
    ttl_seconds: i64,

    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct End {
    /// Run id returned by lifecycle begin.
    #[arg(long)]
    run_id: String,

    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct Status {
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ManagedLifecycle {
    pub run_id: String,
    pub owner_agent: String,
    pub owner_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    pub workdir: PathBuf,
    pub created_at: i64,
    pub updated_at: i64,
    pub expires_at: i64,
}

impl ManagedLifecycle {
    fn is_expired(&self, now: i64) -> bool {
        self.expires_at <= now
    }

    fn renew(&mut self, now: i64, ttl_seconds: i64) {
        self.updated_at = now;
        self.expires_at = now + ttl_seconds.max(1);
    }

    /// Whether a hook from `agent_name` running in `cwd` participates in
    /// this run: the cwd must be inside the run's workdir, and the agent
    /// must be the owner or the declared executor (any agent when no
    /// executor is declared).
    fn governs(&self, cwd: &Path, agent_name: &str) -> bool {
        if !cwd.starts_with(&self.workdir) {
            return false;
        }
        if self.owner_agent == agent_name {
            return true;
        }
        match self.executor_agent.as_deref() {
            Some(executor) => executor == agent_name,
            None => true,
        }
    }

    /// Convert to the orchestrator context injected by the hook handler.
    pub(super) fn to_managed_run_context(&self) -> ManagedRunContext {
        ManagedRunContext {
            stamp: ManagedRunStamp {
                run_id: self.run_id.clone(),
                owner_agent: self.owner_agent.clone(),
                owner_session_id: self.owner_session_id.clone(),
                work_item_id: self.work_item_id.clone(),
            },
            view: self.view.clone(),
        }
    }
}

/// One session recorded under a managed run, harvested from session stamps.
#[derive(Debug, Serialize)]
struct RunSessionSummary {
    session_id: String,
    agent_name: String,
    view: String,
    turn_count: u32,
    change_hashes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct LifecycleEndResult {
    ended: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    lifecycle: Option<ManagedLifecycle>,
    /// Sessions stamped with this run's id, with their recorded changes.
    sessions: Vec<RunSessionSummary>,
    /// All change hashes recorded under this run (across sessions).
    change_hashes: Vec<String>,
    /// Views those changes landed on.
    views: Vec<String>,
}

#[derive(Debug, Serialize)]
struct LifecycleStatus {
    active: bool,
    lifecycles: Vec<ManagedLifecycle>,
}

impl Command for Lifecycle {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            LifecycleCommands::Begin(cmd) => cmd.run(),
            LifecycleCommands::Renew(cmd) => cmd.run(),
            LifecycleCommands::End(cmd) => cmd.run(),
            LifecycleCommands::Status(cmd) => cmd.run(),
        }
    }
}

impl Begin {
    fn run(&self) -> CliResult<()> {
        let dot_dir = resolve_dot_dir()?;
        validate_name("owner", &self.owner)?;
        validate_name("session", &self.session)?;

        let workdir = match &self.workdir {
            Some(dir) => canonicalize_lenient(dir),
            None => current_dir_canonical()?,
        };

        let now = now_secs();
        let dir = lifecycle_dir(&dot_dir);
        cleanup_expired(&dir, now);

        // Idempotent begin: the same owner+session renews its existing run
        // (noname retries begin on reconnect) instead of minting a new id.
        let existing = list_active(&dir, now)
            .into_iter()
            .find(|l| l.owner_agent == self.owner && l.owner_session_id == self.session);

        let lifecycle = match existing {
            Some(mut active) => {
                active.executor_agent = self.executor.clone();
                active.work_item_id = self.work_item.clone();
                active.view = self.view.clone();
                active.workdir = workdir;
                active.renew(now, self.ttl_seconds);
                active
            }
            None => ManagedLifecycle {
                run_id: uuid::Uuid::new_v4().to_string(),
                owner_agent: self.owner.clone(),
                owner_session_id: self.session.clone(),
                executor_agent: self.executor.clone(),
                work_item_id: self.work_item.clone(),
                view: self.view.clone(),
                workdir,
                created_at: now,
                updated_at: now,
                expires_at: now + self.ttl_seconds.max(1),
            },
        };

        save_lifecycle(&dir, &lifecycle)?;
        print_lifecycle(&lifecycle, self.json)
    }
}

impl Renew {
    fn run(&self) -> CliResult<()> {
        let dot_dir = resolve_dot_dir()?;
        let dir = lifecycle_dir(&dot_dir);
        let now = now_secs();

        let mut lifecycle = load_lifecycle(&dir, &self.run_id)?
            .filter(|l| !l.is_expired(now))
            .ok_or_else(|| CliError::InvalidArgument {
                message: format!("no active managed lifecycle with run_id '{}'", self.run_id),
            })?;

        lifecycle.renew(now, self.ttl_seconds);
        save_lifecycle(&dir, &lifecycle)?;
        print_lifecycle(&lifecycle, self.json)
    }
}

impl End {
    fn run(&self) -> CliResult<()> {
        let dot_dir = resolve_dot_dir()?;
        let dir = lifecycle_dir(&dot_dir);

        let lifecycle = load_lifecycle(&dir, &self.run_id)?;
        let ended = lifecycle.is_some();
        remove_lifecycle(&dir, &self.run_id)?;

        // Harvest the run summary from session stamps — even for an
        // already-expired or missing lease, sessions may carry the stamp.
        let sessions = collect_run_sessions(&dot_dir, &self.run_id);

        let mut change_hashes = Vec::new();
        let mut views = Vec::new();
        for s in &sessions {
            for hash in &s.change_hashes {
                if !change_hashes.contains(hash) {
                    change_hashes.push(hash.clone());
                }
            }
            if !s.change_hashes.is_empty() && !views.contains(&s.view) {
                views.push(s.view.clone());
            }
        }

        let result = LifecycleEndResult {
            ended,
            lifecycle,
            sessions,
            change_hashes,
            views,
        };

        if self.json {
            println!("{}", serde_json::to_string(&result).unwrap());
        } else if result.ended {
            println!(
                "Managed lifecycle ended: {} session(s), {} change(s) recorded.",
                result.sessions.len(),
                result.change_hashes.len(),
            );
        } else {
            println!("No active managed lifecycle with that run id.");
        }
        Ok(())
    }
}

impl Status {
    fn run(&self) -> CliResult<()> {
        let dot_dir = resolve_dot_dir()?;
        let dir = lifecycle_dir(&dot_dir);
        let lifecycles = list_active(&dir, now_secs());

        if self.json {
            let status = LifecycleStatus {
                active: !lifecycles.is_empty(),
                lifecycles,
            };
            println!("{}", serde_json::to_string(&status).unwrap());
        } else if lifecycles.is_empty() {
            println!("No active managed lifecycles.");
        } else {
            for l in lifecycles {
                println!(
                    "Managed run {}: owner={} session={} view={} workdir={} expires_at={}",
                    l.run_id,
                    l.owner_agent,
                    l.owner_session_id,
                    l.view.as_deref().unwrap_or("-"),
                    l.workdir.display(),
                    l.expires_at,
                );
            }
        }
        Ok(())
    }
}

/// Resolve the managed run governing a hook from `agent_name` in the current
/// working directory. Returns `None` outside a repository, when no active
/// lifecycle covers the cwd, or when the agent is not a participant.
///
/// With overlapping workdirs (an in-repo run plus a sandboxed run), the most
/// specific (longest) workdir wins; ties go to the newest run.
pub(super) fn find_governing_lifecycle_for_hook(agent_name: &str) -> Option<ManagedLifecycle> {
    let cwd = current_dir_canonical().ok()?;
    let dot_dir = atomic_repository::Repository::canonical_dot_dir(&cwd).ok()?;
    let dir = lifecycle_dir(&dot_dir);
    let now = now_secs();

    list_active(&dir, now)
        .into_iter()
        .filter(|l| l.governs(&cwd, agent_name))
        .max_by_key(|l| (l.workdir.components().count(), l.created_at))
}

fn resolve_dot_dir() -> CliResult<PathBuf> {
    let cwd = std::env::current_dir().map_err(CliError::Io)?;
    atomic_repository::Repository::canonical_dot_dir(&cwd).map_err(|e| {
        CliError::Internal(anyhow!("not inside an Atomic repository or sandbox: {}", e))
    })
}

fn lifecycle_dir(dot_dir: &Path) -> PathBuf {
    dot_dir.join(LIFECYCLE_DIR)
}

fn lifecycle_path(dir: &Path, run_id: &str) -> PathBuf {
    dir.join(format!("{}.json", run_id))
}

fn validate_name(label: &str, value: &str) -> CliResult<()> {
    if value.trim().is_empty() {
        return Err(CliError::InvalidArgument {
            message: format!("{label} cannot be empty"),
        });
    }
    Ok(())
}

fn validate_run_id(run_id: &str) -> CliResult<()> {
    if run_id.is_empty()
        || run_id.contains("..")
        || run_id.contains('/')
        || run_id.contains('\\')
        || run_id.contains('\0')
    {
        return Err(CliError::InvalidArgument {
            message: format!("invalid run id '{}'", run_id),
        });
    }
    Ok(())
}

fn canonicalize_lenient(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn current_dir_canonical() -> CliResult<PathBuf> {
    let cwd = std::env::current_dir().map_err(CliError::Io)?;
    Ok(canonicalize_lenient(&cwd))
}

fn list_active(dir: &Path, now: i64) -> Vec<ManagedLifecycle> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut active = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(data) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(lifecycle) = serde_json::from_str::<ManagedLifecycle>(&data) else {
            log::warn!("skipping unparseable lifecycle file {}", path.display());
            continue;
        };
        if !lifecycle.is_expired(now) {
            active.push(lifecycle);
        }
    }
    active
}

fn cleanup_expired(dir: &Path, now: i64) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let expired = fs::read_to_string(&path)
            .ok()
            .and_then(|data| serde_json::from_str::<ManagedLifecycle>(&data).ok())
            .map(|l| l.is_expired(now))
            .unwrap_or(false);
        if expired {
            let _ = fs::remove_file(&path);
        }
    }
}

fn load_lifecycle(dir: &Path, run_id: &str) -> CliResult<Option<ManagedLifecycle>> {
    validate_run_id(run_id)?;
    let path = lifecycle_path(dir, run_id);
    if !path.exists() {
        return Ok(None);
    }
    let data = fs::read_to_string(&path).map_err(|e| {
        CliError::Internal(anyhow!(
            "failed to read managed lifecycle '{}': {}",
            path.display(),
            e
        ))
    })?;
    let lifecycle = serde_json::from_str(&data).map_err(|e| {
        CliError::Internal(anyhow!(
            "failed to parse managed lifecycle '{}': {}",
            path.display(),
            e
        ))
    })?;
    Ok(Some(lifecycle))
}

fn save_lifecycle(dir: &Path, lifecycle: &ManagedLifecycle) -> CliResult<()> {
    validate_run_id(&lifecycle.run_id)?;
    fs::create_dir_all(dir)?;
    let path = lifecycle_path(dir, &lifecycle.run_id);
    let data = serde_json::to_vec_pretty(lifecycle)
        .map_err(|e| CliError::Internal(anyhow!("failed to serialize managed lifecycle: {}", e)))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn remove_lifecycle(dir: &Path, run_id: &str) -> CliResult<()> {
    validate_run_id(run_id)?;
    match fs::remove_file(lifecycle_path(dir, run_id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CliError::Internal(anyhow!(
            "failed to remove managed lifecycle '{}': {}",
            run_id,
            e
        ))),
    }
}

/// Collect sessions stamped with `run_id` from the canonical session store.
fn collect_run_sessions(dot_dir: &Path, run_id: &str) -> Vec<RunSessionSummary> {
    let store = match SessionStore::new(dot_dir.join("sessions")) {
        Ok(store) => store,
        Err(e) => {
            log::warn!("could not open session store for run summary: {}", e);
            return Vec::new();
        }
    };
    let sessions = match store.list() {
        Ok(sessions) => sessions,
        Err(e) => {
            log::warn!("could not list sessions for run summary: {}", e);
            return Vec::new();
        }
    };

    sessions
        .into_iter()
        .filter(|s| {
            s.managed_run
                .as_ref()
                .map(|m| m.run_id == run_id)
                .unwrap_or(false)
        })
        .map(|s| RunSessionSummary {
            session_id: s.session_id.clone(),
            agent_name: s.agent_name.clone(),
            view: s.view_name.clone(),
            turn_count: s.turn_count,
            change_hashes: s
                .recorded_change_hashes
                .iter()
                .map(|h| h.to_base32())
                .collect(),
        })
        .collect()
}

fn print_lifecycle(lifecycle: &ManagedLifecycle, json: bool) -> CliResult<()> {
    if json {
        println!("{}", serde_json::to_string(lifecycle).unwrap());
    } else {
        println!(
            "Managed run {}: owner={} session={} view={} workdir={} expires_at={}",
            lifecycle.run_id,
            lifecycle.owner_agent,
            lifecycle.owner_session_id,
            lifecycle.view.as_deref().unwrap_or("-"),
            lifecycle.workdir.display(),
            lifecycle.expires_at,
        );
    }
    Ok(())
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_agent::turn::session::AgentSession;
    use tempfile::TempDir;

    fn lifecycle(run_id: &str, owner: &str, workdir: &Path, expires_at: i64) -> ManagedLifecycle {
        ManagedLifecycle {
            run_id: run_id.to_string(),
            owner_agent: owner.to_string(),
            owner_session_id: format!("{}-session", owner),
            executor_agent: None,
            work_item_id: None,
            view: Some("sherpa-run".to_string()),
            workdir: workdir.to_path_buf(),
            created_at: 1,
            updated_at: 1,
            expires_at,
        }
    }

    #[test]
    fn keyed_store_holds_concurrent_runs() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join(LIFECYCLE_DIR);
        let now = now_secs();

        save_lifecycle(&dir, &lifecycle("run-a", "sherpa", temp.path(), now + 60)).unwrap();
        save_lifecycle(&dir, &lifecycle("run-b", "sherpa", temp.path(), now + 60)).unwrap();

        let active = list_active(&dir, now);
        assert_eq!(active.len(), 2, "two concurrent runs must coexist");
    }

    #[test]
    fn expired_lifecycles_are_ignored_and_cleaned() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join(LIFECYCLE_DIR);
        let now = now_secs();

        save_lifecycle(&dir, &lifecycle("run-old", "sherpa", temp.path(), now - 1)).unwrap();
        save_lifecycle(&dir, &lifecycle("run-new", "sherpa", temp.path(), now + 60)).unwrap();

        let active = list_active(&dir, now);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].run_id, "run-new");

        cleanup_expired(&dir, now);
        assert!(!lifecycle_path(&dir, "run-old").exists());
        assert!(lifecycle_path(&dir, "run-new").exists());
    }

    #[test]
    fn governs_requires_cwd_inside_workdir() {
        let temp = TempDir::new().unwrap();
        let sandbox = temp.path().join("sandboxes").join("run-a");
        std::fs::create_dir_all(&sandbox).unwrap();
        let l = lifecycle("run-a", "sherpa", &sandbox, now_secs() + 60);

        assert!(l.governs(&sandbox, "codex"));
        assert!(l.governs(&sandbox.join("src"), "codex"));
        assert!(
            !l.governs(temp.path(), "codex"),
            "a hook outside the workdir is not governed"
        );
    }

    #[test]
    fn declared_executor_narrows_participation() {
        let temp = TempDir::new().unwrap();
        let mut l = lifecycle("run-a", "sherpa", temp.path(), now_secs() + 60);
        l.executor_agent = Some("codex".to_string());

        assert!(l.governs(temp.path(), "codex"), "declared executor governs");
        assert!(l.governs(temp.path(), "sherpa"), "owner always governs");
        assert!(
            !l.governs(temp.path(), "claude-code"),
            "an unrelated agent's direct session is untouched"
        );
    }

    #[test]
    fn no_executor_means_any_agent_participates() {
        let temp = TempDir::new().unwrap();
        let l = lifecycle("run-a", "sherpa", temp.path(), now_secs() + 60);
        assert!(l.governs(temp.path(), "claude-code"));
    }

    #[test]
    fn most_specific_workdir_wins() {
        let temp = TempDir::new().unwrap();
        let sandbox = temp.path().join("sbx");
        std::fs::create_dir_all(&sandbox).unwrap();
        let now = now_secs();

        let broad = lifecycle("run-repo", "sherpa", temp.path(), now + 60);
        let narrow = lifecycle("run-sbx", "sherpa", &sandbox, now + 60);

        let winner = [broad, narrow]
            .into_iter()
            .filter(|l| l.governs(&sandbox, "codex"))
            .max_by_key(|l| (l.workdir.components().count(), l.created_at))
            .unwrap();
        assert_eq!(winner.run_id, "run-sbx");
    }

    #[test]
    fn end_summary_collects_stamped_sessions() {
        let temp = TempDir::new().unwrap();
        let dot_dir = temp.path().join(".atomic");
        let store = SessionStore::new(dot_dir.join("sessions")).unwrap();

        let mut stamped = AgentSession::new("sess-in-run", "codex", "Codex");
        stamped.managed_run = Some(ManagedRunStamp {
            run_id: "run-a".to_string(),
            owner_agent: "sherpa".to_string(),
            owner_session_id: "sherpa-1".to_string(),
            work_item_id: None,
        });
        stamped
            .recorded_change_hashes
            .push(atomic_core::types::Hash::of(b"change-1"));
        store.save(&stamped).unwrap();

        let direct = AgentSession::new("sess-direct", "claude-code", "Claude Code");
        store.save(&direct).unwrap();

        let sessions = collect_run_sessions(&dot_dir, "run-a");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "sess-in-run");
        assert_eq!(sessions[0].agent_name, "codex");
        assert_eq!(sessions[0].change_hashes.len(), 1);

        assert!(collect_run_sessions(&dot_dir, "run-other").is_empty());
    }

    #[test]
    fn load_rejects_path_traversal_run_ids() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join(LIFECYCLE_DIR);
        assert!(load_lifecycle(&dir, "../evil").is_err());
        assert!(load_lifecycle(&dir, "a/b").is_err());
    }

    #[test]
    fn remove_missing_lifecycle_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join(LIFECYCLE_DIR);
        assert!(remove_lifecycle(&dir, "does-not-exist").is_ok());
    }

    #[test]
    fn to_managed_run_context_maps_fields() {
        let temp = TempDir::new().unwrap();
        let mut l = lifecycle("run-a", "sherpa", temp.path(), now_secs() + 60);
        l.work_item_id = Some("NONA-12".to_string());

        let ctx = l.to_managed_run_context();
        assert_eq!(ctx.stamp.run_id, "run-a");
        assert_eq!(ctx.stamp.owner_agent, "sherpa");
        assert_eq!(ctx.stamp.work_item_id.as_deref(), Some("NONA-12"));
        assert_eq!(ctx.view.as_deref(), Some("sherpa-run"));
    }
}
