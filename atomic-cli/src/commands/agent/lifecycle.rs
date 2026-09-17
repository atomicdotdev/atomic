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
//! orchestrator's lease expires on its own, stops governing hooks, and remains
//! visible as stale until `lifecycle end` harvests and removes it.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use atomic_agent::turn::orchestrator::ManagedRunContext;
use atomic_agent::turn::session::{AgentSession, ManagedRunStamp, SessionStore};
use atomic_agent::{JournalStopCause, JournalTurnLifecycle, ProvenanceJournalSink};
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

    /// Stop a managed run while preserving a resumable pending turn.
    Stop(Stop),

    /// Resume a stopped managed run and fence prior writers.
    Resume(Resume),

    /// Explicitly abandon and seal a pending turn.
    Abandon(Abandon),

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

#[derive(Clone, Copy, Debug, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum StopReason {
    UserRequested,
    ProcessExited,
    HookFailure,
    SystemShutdown,
}

#[derive(Debug, Args)]
struct Stop {
    #[arg(long)]
    run_id: String,

    #[arg(long, value_enum, default_value_t = StopReason::UserRequested)]
    cause: StopReason,

    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct Resume {
    #[arg(long)]
    run_id: String,

    #[arg(long, default_value_t = DEFAULT_TTL_SECONDS)]
    ttl_seconds: i64,

    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct Abandon {
    #[arg(long)]
    run_id: String,

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ManagedStopState {
    cause: JournalStopCauseWire,
    observed_at: i64,
    resumable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum JournalStopCauseWire {
    UserRequested,
    LeaseExpired,
    ProcessExited,
    HookFailure,
    SystemShutdown,
    Abandoned,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stop_state: Option<ManagedStopState>,
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
        if self.stop_state.is_some() || !cwd.starts_with(&self.workdir) {
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
    interrupted: bool,
    lifecycles: Vec<ManagedLifecycle>,
    stale_lifecycles: Vec<ManagedLifecycle>,
    turns: Vec<LifecycleTurnStatus>,
}

#[derive(Debug, Serialize)]
struct LifecycleTurnStatus {
    run_id: String,
    session_id: String,
    turn_number: u32,
    provenance_id: u64,
    generation: u64,
    state: String,
    last_event_seq: Option<u64>,
    resumable: bool,
}

impl Command for Lifecycle {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            LifecycleCommands::Begin(cmd) => cmd.run(),
            LifecycleCommands::Renew(cmd) => cmd.run(),
            LifecycleCommands::Stop(cmd) => cmd.run(),
            LifecycleCommands::Resume(cmd) => cmd.run(),
            LifecycleCommands::Abandon(cmd) => cmd.run(),
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

        // Idempotent begin: the same owner+session renews its existing run
        // (noname retries begin on reconnect) instead of minting a new id.
        let existing = list_lifecycles(&dir)
            .into_iter()
            .find(|l| l.owner_agent == self.owner && l.owner_session_id == self.session);

        let lifecycle = match existing {
            Some(mut active) => {
                if matches!(
                    active.stop_state.as_ref().map(|state| state.cause),
                    Some(JournalStopCauseWire::Abandoned)
                ) {
                    return Err(CliError::InvalidArgument {
                        message: format!(
                            "managed lifecycle '{}' was abandoned and cannot resume",
                            active.run_id
                        ),
                    });
                }
                if active.stop_state.is_some() || active.is_expired(now) {
                    resume_lifecycle_turns(&dot_dir, &active, now)?;
                    active.stop_state = None;
                }
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
                stop_state: None,
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
            .filter(|l| !l.is_expired(now) && l.stop_state.is_none())
            .ok_or_else(|| CliError::InvalidArgument {
                message: format!("no active managed lifecycle with run_id '{}'", self.run_id),
            })?;

        lifecycle.renew(now, self.ttl_seconds);
        save_lifecycle(&dir, &lifecycle)?;
        print_lifecycle(&lifecycle, self.json)
    }
}

impl Stop {
    fn run(&self) -> CliResult<()> {
        let dot_dir = resolve_dot_dir()?;
        let dir = lifecycle_dir(&dot_dir);
        let mut lifecycle =
            load_lifecycle(&dir, &self.run_id)?.ok_or_else(|| CliError::InvalidArgument {
                message: format!("no managed lifecycle with run_id '{}'", self.run_id),
            })?;
        let now = now_secs();
        let cause = match self.cause {
            StopReason::UserRequested => JournalStopCauseWire::UserRequested,
            StopReason::ProcessExited => JournalStopCauseWire::ProcessExited,
            StopReason::HookFailure => JournalStopCauseWire::HookFailure,
            StopReason::SystemShutdown => JournalStopCauseWire::SystemShutdown,
        };
        stop_lifecycle_turns(&dot_dir, &lifecycle, cause, true, now)?;
        lifecycle.stop_state = Some(ManagedStopState {
            cause,
            observed_at: now,
            resumable: true,
        });
        lifecycle.updated_at = now;
        save_lifecycle(&dir, &lifecycle)?;
        print_lifecycle(&lifecycle, self.json)
    }
}

impl Resume {
    fn run(&self) -> CliResult<()> {
        let dot_dir = resolve_dot_dir()?;
        let dir = lifecycle_dir(&dot_dir);
        let mut lifecycle =
            load_lifecycle(&dir, &self.run_id)?.ok_or_else(|| CliError::InvalidArgument {
                message: format!("no managed lifecycle with run_id '{}'", self.run_id),
            })?;
        if matches!(
            lifecycle.stop_state.as_ref().map(|state| state.cause),
            Some(JournalStopCauseWire::Abandoned)
        ) {
            return Err(CliError::InvalidArgument {
                message: "abandoned lifecycle cannot resume".to_string(),
            });
        }
        let now = now_secs();
        resume_lifecycle_turns(&dot_dir, &lifecycle, now)?;
        lifecycle.stop_state = None;
        lifecycle.renew(now, self.ttl_seconds);
        save_lifecycle(&dir, &lifecycle)?;
        print_lifecycle(&lifecycle, self.json)
    }
}

impl Abandon {
    fn run(&self) -> CliResult<()> {
        let dot_dir = resolve_dot_dir()?;
        let dir = lifecycle_dir(&dot_dir);
        let mut lifecycle =
            load_lifecycle(&dir, &self.run_id)?.ok_or_else(|| CliError::InvalidArgument {
                message: format!("no managed lifecycle with run_id '{}'", self.run_id),
            })?;
        let now = now_secs();
        abandon_lifecycle_turns(&dot_dir, &lifecycle, now)?;
        lifecycle.stop_state = Some(ManagedStopState {
            cause: JournalStopCauseWire::Abandoned,
            observed_at: now,
            resumable: false,
        });
        lifecycle.updated_at = now;
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
        if let Some(lifecycle) = &lifecycle {
            if lifecycle.stop_state.is_none() {
                stop_lifecycle_turns(
                    &dot_dir,
                    lifecycle,
                    JournalStopCauseWire::UserRequested,
                    true,
                    now_secs(),
                )?;
            }
        }
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
        let now = now_secs();
        for mut lifecycle in list_lifecycles(&dir) {
            if lifecycle.is_expired(now) && lifecycle.stop_state.is_none() {
                stop_lifecycle_turns(
                    &dot_dir,
                    &lifecycle,
                    JournalStopCauseWire::LeaseExpired,
                    true,
                    now,
                )?;
                lifecycle.stop_state = Some(ManagedStopState {
                    cause: JournalStopCauseWire::LeaseExpired,
                    observed_at: now,
                    resumable: true,
                });
                lifecycle.updated_at = now;
                save_lifecycle(&dir, &lifecycle)?;
            }
        }
        let (lifecycles, stale_lifecycles) = list_by_state(&dir, now);
        let mut all_lifecycles = lifecycles.clone();
        all_lifecycles.extend(stale_lifecycles.clone());
        let turns = collect_lifecycle_turn_statuses(&dot_dir, &all_lifecycles)?;

        if self.json {
            let status = LifecycleStatus {
                active: !lifecycles.is_empty(),
                interrupted: !stale_lifecycles.is_empty(),
                lifecycles,
                stale_lifecycles,
                turns,
            };
            println!("{}", serde_json::to_string(&status).unwrap());
        } else {
            if lifecycles.is_empty() {
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
            for lifecycle in stale_lifecycles {
                let remediation = match lifecycle.stop_state.as_ref() {
                    Some(state) if state.cause == JournalStopCauseWire::Abandoned => {
                        "sealed; start a new run"
                    }
                    Some(state) if state.resumable => {
                        "run lifecycle resume --run-id <id> to continue"
                    }
                    Some(_) => "non-resumable; abandon or end the run",
                    None => "run lifecycle status again to persist lease expiry",
                };
                println!(
                    "Interrupted managed run {}: owner={} session={} view={} workdir={} stop={:?} ({})",
                    lifecycle.run_id,
                    lifecycle.owner_agent,
                    lifecycle.owner_session_id,
                    lifecycle.view.as_deref().unwrap_or("-"),
                    lifecycle.workdir.display(),
                    lifecycle.stop_state.as_ref().map(|state| state.cause),
                    remediation.replace("<id>", &lifecycle.run_id),
                );
            }
            for turn in turns {
                println!(
                    "  session={} turn={} provenance={} generation={} state={} frontier={:?} resumable={}",
                    turn.session_id,
                    turn.turn_number,
                    turn.provenance_id,
                    turn.generation,
                    turn.state,
                    turn.last_event_seq,
                    turn.resumable,
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

fn list_lifecycles(dir: &Path) -> Vec<ManagedLifecycle> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut lifecycles = Vec::new();
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
        lifecycles.push(lifecycle);
    }
    lifecycles
}

fn list_by_state(dir: &Path, now: i64) -> (Vec<ManagedLifecycle>, Vec<ManagedLifecycle>) {
    list_lifecycles(dir)
        .into_iter()
        .partition(|lifecycle| !lifecycle.is_expired(now) && lifecycle.stop_state.is_none())
}

fn list_active(dir: &Path, now: i64) -> Vec<ManagedLifecycle> {
    list_by_state(dir, now).0
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

fn sessions_for_run(dot_dir: &Path, run_id: &str) -> Vec<AgentSession> {
    let Ok(store) = SessionStore::new(dot_dir.join("sessions")) else {
        return Vec::new();
    };
    store
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|session| {
            session
                .managed_run
                .as_ref()
                .is_some_and(|stamp| stamp.run_id == run_id)
        })
        .collect()
}

fn pending_turn_number(session: &AgentSession) -> u32 {
    if session.is_turn_active() {
        session.turn_count.saturating_add(1)
    } else {
        session.turn_count.max(1)
    }
}

fn owner_sink(dot_dir: &Path) -> CliResult<super::owner::OwnerJournalSink> {
    let root = dot_dir.parent().ok_or_else(|| {
        CliError::Internal(anyhow!(
            "canonical .atomic directory has no repository parent"
        ))
    })?;
    Ok(super::owner::OwnerJournalSink::new(root))
}

fn map_stop_cause(cause: JournalStopCauseWire) -> JournalStopCause {
    match cause {
        JournalStopCauseWire::UserRequested => JournalStopCause::UserRequested,
        JournalStopCauseWire::LeaseExpired => JournalStopCause::LeaseExpired,
        JournalStopCauseWire::ProcessExited => JournalStopCause::ProcessExited,
        JournalStopCauseWire::HookFailure => JournalStopCause::HookFailure,
        JournalStopCauseWire::SystemShutdown => JournalStopCause::SystemShutdown,
        JournalStopCauseWire::Abandoned => JournalStopCause::Abandoned,
    }
}

fn stop_lifecycle_turns(
    dot_dir: &Path,
    lifecycle: &ManagedLifecycle,
    cause: JournalStopCauseWire,
    resumable: bool,
    observed_at: i64,
) -> CliResult<()> {
    let sink = owner_sink(dot_dir)?;
    for session in sessions_for_run(dot_dir, &lifecycle.run_id) {
        sink.stop_turn(
            &session.session_id,
            pending_turn_number(&session),
            map_stop_cause(cause),
            resumable,
            observed_at,
        )
        .map_err(|reason| CliError::Internal(anyhow!(reason)))?;
    }
    Ok(())
}

fn resume_lifecycle_turns(dot_dir: &Path, lifecycle: &ManagedLifecycle, now: i64) -> CliResult<()> {
    let sink = owner_sink(dot_dir)?;
    for session in sessions_for_run(dot_dir, &lifecycle.run_id) {
        sink.resume_turn(&session.session_id, pending_turn_number(&session), now)
            .map_err(|reason| CliError::Internal(anyhow!(reason)))?;
    }
    Ok(())
}

fn abandon_lifecycle_turns(
    dot_dir: &Path,
    lifecycle: &ManagedLifecycle,
    now: i64,
) -> CliResult<()> {
    let sink = owner_sink(dot_dir)?;
    for session in sessions_for_run(dot_dir, &lifecycle.run_id) {
        sink.abandon_turn(&session.session_id, pending_turn_number(&session), now)
            .map_err(|reason| CliError::Internal(anyhow!(reason)))?;
    }
    Ok(())
}

fn collect_lifecycle_turn_statuses(
    dot_dir: &Path,
    lifecycles: &[ManagedLifecycle],
) -> CliResult<Vec<LifecycleTurnStatus>> {
    let sink = owner_sink(dot_dir)?;
    let mut result = Vec::new();
    for lifecycle in lifecycles {
        for session in sessions_for_run(dot_dir, &lifecycle.run_id) {
            let turn_number = pending_turn_number(&session);
            let Some(status) = sink
                .turn_status(&session.session_id, turn_number)
                .map_err(|reason| CliError::Internal(anyhow!(reason)))?
            else {
                continue;
            };
            let (state, last_event_seq, resumable) = match status.lifecycle {
                JournalTurnLifecycle::Running => ("running", None, true),
                JournalTurnLifecycle::Stopped {
                    last_event_seq,
                    resumable,
                    ..
                } => ("stopped", last_event_seq, resumable),
                JournalTurnLifecycle::Checkpointing => ("checkpointing", None, false),
                JournalTurnLifecycle::Completed => ("completed", None, false),
                JournalTurnLifecycle::Abandoned { last_event_seq, .. } => {
                    ("abandoned", last_event_seq, false)
                }
            };
            result.push(LifecycleTurnStatus {
                run_id: lifecycle.run_id.clone(),
                session_id: session.session_id,
                turn_number,
                provenance_id: status.provenance_id,
                generation: status.generation,
                state: state.to_string(),
                last_event_seq,
                resumable,
            });
        }
    }
    result.sort_by(|left, right| {
        (&left.run_id, &left.session_id, left.turn_number).cmp(&(
            &right.run_id,
            &right.session_id,
            right.turn_number,
        ))
    });
    Ok(result)
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
            stop_state: None,
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
    fn expired_lifecycles_are_ignored_but_retained_for_interruption_detection() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join(LIFECYCLE_DIR);
        let now = now_secs();

        save_lifecycle(&dir, &lifecycle("run-old", "sherpa", temp.path(), now - 1)).unwrap();
        save_lifecycle(&dir, &lifecycle("run-new", "sherpa", temp.path(), now + 60)).unwrap();

        let (active, stale) = list_by_state(&dir, now);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].run_id, "run-new");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].run_id, "run-old");

        assert!(lifecycle_path(&dir, "run-old").exists());
        assert!(lifecycle_path(&dir, "run-new").exists());
    }

    #[test]
    fn lifecycle_status_reports_interrupted_stale_runs() {
        let temp = TempDir::new().unwrap();
        let status = LifecycleStatus {
            active: false,
            interrupted: true,
            lifecycles: Vec::new(),
            stale_lifecycles: vec![lifecycle("run-old", "sherpa", temp.path(), 1)],
            turns: Vec::new(),
        };

        let json = serde_json::to_value(status).unwrap();
        assert_eq!(json["active"], false);
        assert_eq!(json["interrupted"], true);
        assert_eq!(json["stale_lifecycles"][0]["run_id"], "run-old");
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
