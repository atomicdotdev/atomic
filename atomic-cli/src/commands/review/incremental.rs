//! Bounded-by-selection, read-only inspection. This is not a review approval.

use std::collections::{BTreeMap, BTreeSet};

use atomic_agent::turn::session::{AgentSession, SessionStore};
use atomic_core::change::session::{SessionRecord, SessionTurn};
use atomic_core::kg::iri;
use atomic_core::pristine::{GraphTxnT, ViewMembershipSet};
use atomic_core::types::{Base32, Hash};
use atomic_repository::intent_bridge as bridge;
use atomic_repository::{graph_visibility_from_membership, Repository};
use clap::Args;
use serde::Serialize;
use serde_json::{json, Value};

use crate::commands::find_repository_root;
use crate::error::{CliError, CliResult};

#[derive(Args, Debug, Clone, Default)]
pub struct IncrementalArgs {
    /// Inspect exactly one ledger turn (SESSION:TURN), including pending work.
    #[arg(long, value_name = "SESSION:TURN", conflicts_with_all = ["intent", "change", "view"])]
    pub turn: Option<String>,
    /// Inspect changes with an actual recorded governing intent declaration.
    #[arg(long, value_name = "ID", conflicts_with_all = ["turn", "change", "view"])]
    pub intent: Option<String>,
    /// Inspect one recorded change (full hash or unambiguous Base32 prefix).
    #[arg(long, value_name = "HASH", conflicts_with_all = ["turn", "intent", "view"])]
    pub change: Option<String>,
    /// Audit every live recorded file in this view, including inherited files.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["turn", "intent", "change", "findings", "risk_profile"])]
    pub view: Option<String>,
    /// Before view for a local recorded-view comparison (requires --to).
    #[arg(long, value_name="VIEW", requires="to", conflicts_with_all=["view","change","turn","intent","findings","risk_profile"])]
    pub from: Option<String>,
    /// After view for a local recorded-view comparison (requires --from).
    #[arg(long, value_name="VIEW", requires="from", conflicts_with_all=["view","change","turn","intent","findings","risk_profile"])]
    pub to: Option<String>,
    #[arg(long, conflicts_with = "html")]
    pub json: bool,
    /// Open an offline file/region review viewer in the default browser.
    #[arg(long, conflicts_with = "details")]
    pub html: bool,
    /// Do not open a browser; emit HTML to stdout or --output.
    #[arg(long, requires = "html")]
    pub no_open: bool,
    /// Write the requested HTML report to this file.
    #[arg(short, long, requires = "html", value_name = "FILE")]
    pub output: Option<std::path::PathBuf>,
    /// Add exact-scope reviewer findings, criterion judgments and risk assessment.
    #[arg(long, value_name = "FILE")]
    pub findings: Option<std::path::PathBuf>,
    /// Override the versioned change-review risk thresholds with a JSON profile.
    #[arg(long, value_name = "FILE")]
    pub risk_profile: Option<std::path::PathBuf>,
    /// Include the inspection worklist, intent text and diffs below the graph.
    #[arg(long)]
    pub details: bool,
}

/// Exact selected roots are separate from their required causal context.
#[derive(Debug, Serialize)]
pub struct IncrementalReport {
    pub schema_version: u32,
    pub purpose: &'static str,
    pub independent_approval: bool,
    pub selection: Selection,
    pub source: Value,
    pub selected_roots: Vec<String>,
    /// Includes roots, in canonical dependency-first order.
    pub dependency_closure: Vec<String>,
    pub dependency_only: Vec<String>,
    /// Dependency-only causal references, never ancestor review payloads.
    pub dependency_references: Vec<DependencyReference>,
    /// Full inspections belong exclusively to selected roots.
    pub changes: Vec<ChangeInspection>,
    /// Whole-view snapshot evidence, not an invented authored change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_snapshot: Option<ViewInspection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_comparison: Option<super::view_comparison::ViewComparison>,
    pub turns: Vec<TurnInspection>,
    /// Live work on this view, explicitly outside the recorded selection.
    pub pending_work_outside_selection: Vec<TurnInspection>,
    pub intents: Vec<Value>,
    pub warnings: Vec<String>,
    pub graph: Option<atomic_canonical::review_graph::ReviewGraph>,
}

#[derive(Debug, Serialize)]
pub struct ViewInspection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conformance: Option<super::view_comparison::Conformance>,
    pub source_ref: String,
    pub view: String,
    pub state: String,
    pub membership_hash: String,
    pub paths: Vec<String>,
    pub analysis: atomic_canonical::review_finding::AnalysisReport,
    #[serde(skip)]
    pub(crate) content_hashes: BTreeMap<String, String>,
}

impl IncrementalReport {
    pub(super) fn analyses(
        &self,
    ) -> impl Iterator<Item = (&str, &atomic_canonical::review_finding::AnalysisReport)> {
        self.changes
            .iter()
            .filter(|change| change.selected_root)
            .map(|change| (change.hash.as_str(), &change.analysis))
            .chain(
                self.view_snapshot
                    .iter()
                    .map(|snapshot| (snapshot.source_ref.as_str(), &snapshot.analysis)),
            )
            .chain(self.view_comparison.iter().flat_map(|comparison| {
                comparison
                    .files
                    .iter()
                    .map(|file| (file.source_ref.as_str(), &file.analysis))
            }))
    }

    pub(super) fn is_view_review(&self) -> bool {
        self.view_snapshot.is_some() || self.view_comparison.is_some()
    }

    pub(super) fn conformance(&self) -> Option<&super::view_comparison::Conformance> {
        self.view_comparison
            .as_ref()
            .map(|comparison| &comparison.conformance)
            .or_else(|| self.view_snapshot.as_ref()?.conformance.as_ref())
    }
}

#[derive(Debug, Serialize)]
pub struct DependencyReference {
    pub hash: String,
    pub direct: bool,
    pub on_current_view: bool,
    /// First header line, at most 240 Unicode characters; not the full message.
    pub summary: String,
    pub dependencies: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Selection {
    pub kind: String,
    pub value: Option<String>,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct ChangeInspection {
    pub hash: String,
    pub selected_root: bool,
    pub on_current_view: bool,
    pub metadata: Value,
    pub dependencies: Vec<String>,
    pub paths: Vec<String>,
    pub intent_context: Value,
    pub recorded_patch: Value,
    pub diff: Option<String>,
    pub diff_status: String,
    pub analysis: atomic_canonical::review_finding::AnalysisReport,
}

#[derive(Debug, Serialize)]
pub struct TurnInspection {
    pub session_id: String,
    pub view_name: Option<String>,
    pub turn_number: u32,
    pub status: String,
    pub session_status: String,
    pub plan_id: Option<String>,
    pub provenance_hash: Option<String>,
    pub change_hashes: Vec<String>,
    pub outcome: Value,
}

fn invalid(message: impl Into<String>) -> CliError {
    CliError::InvalidArgument {
        message: message.into(),
    }
}

fn encode<T: Serialize>(value: &T) -> CliResult<Value> {
    serde_json::to_value(value).map_err(|e| CliError::Internal(e.into()))
}

fn validate_session(id: &str) -> CliResult<()> {
    // Match SessionStore's path-component contract even when no store exists.
    if id.is_empty() || id.len() > 255 || id.contains("..") || id.contains(['/', '\\', '\0']) {
        return Err(invalid("Invalid session ID in turn/session context"));
    }
    Ok(())
}

fn parse_turn(input: &str) -> CliResult<(&str, u32)> {
    let (session, number) = input
        .rsplit_once(':')
        .ok_or_else(|| invalid("--turn must be SESSION:TURN"))?;
    validate_session(session)?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("--turn requires a nonnegative ledger turn number"));
    }
    let number = number
        .parse::<u32>()
        .map_err(|_| invalid("Turn number exceeds u32"))?;
    Ok((session, number))
}

fn runtime_store(repo: &Repository) -> CliResult<Option<SessionStore>> {
    let directory = repo.dot_dir().join("sessions");
    // SessionStore::new creates directories. Never call it on an absent store.
    match std::fs::metadata(&directory) {
        Ok(metadata) if metadata.is_dir() => SessionStore::new(directory)
            .map(Some)
            .map_err(|e| CliError::Internal(e.into())),
        Ok(_) => Err(invalid("Session store is not a directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CliError::Internal(e.into())),
    }
}

fn runtime_session(store: Option<&SessionStore>, id: &str) -> CliResult<Option<AgentSession>> {
    validate_session(id)?;
    let session = store
        .map(|s| s.load(id))
        .transpose()
        .map_err(|e| CliError::Internal(e.into()))?
        .flatten();
    if session.as_ref().is_some_and(|s| s.session_id != id) {
        return Err(invalid(
            "Runtime session identity does not match its filename",
        ));
    }
    Ok(session)
}

fn intent_key(repo: &Repository, reference: &str) -> CliResult<String> {
    if reference.trim() != reference || reference.is_empty() {
        return Err(invalid(
            "Intent selector must not be empty or padded with whitespace",
        ));
    }
    let key = repo
        .resolve_intent_key(reference)
        .map_err(CliError::Repository)?;
    // resolve_intent_key intentionally permits not-yet-created human keys.
    repo.vault_intent_show(&key).map_err(CliError::Repository)?;
    Ok(key)
}

fn same_intent(repo: &Repository, reference: &str, key: &str) -> bool {
    repo.resolve_intent_key(reference)
        .is_ok_and(|resolved| resolved == key)
}

fn current_plan(
    repo: &Repository,
    store: Option<&SessionStore>,
    session_id: &str,
) -> CliResult<Option<String>> {
    let runtime = runtime_session(store, session_id)?;
    let ledger = repo
        .get_session_ledger(session_id)
        .map_err(CliError::Repository)?;
    if runtime.is_none() && ledger.is_none() {
        return Err(invalid(format!(
            "Unknown explicit session context: {session_id}"
        )));
    }
    let mut keys = BTreeSet::new();
    let runtime_active = runtime.as_ref().is_some_and(AgentSession::is_turn_active);
    if let Some(runtime) = runtime {
        if runtime.view_name != repo.current_view() {
            return Err(invalid(
                "Explicit session context belongs to a different view",
            ));
        }
        if runtime.is_turn_active() {
            let turn = runtime
                .turn_count
                .checked_add(1)
                .ok_or_else(|| invalid("Turn overflow"))?;
            if let Some(binding) = store
                .map(|s| s.turn_intent_binding(session_id, turn))
                .transpose()
                .map_err(|e| CliError::Internal(e.into()))?
                .flatten()
            {
                keys.insert(intent_key(repo, &binding.intent_uid)?);
            }
            if let Some(work_item) = runtime.managed_run.and_then(|run| run.work_item_id) {
                keys.insert(intent_key(repo, &work_item)?);
            }
        }
    }
    if let Some((record, turns)) = ledger.filter(|_| !runtime_active) {
        if record
            .view_name
            .as_deref()
            .is_some_and(|view| view != repo.current_view())
        {
            return Err(invalid(
                "Explicit session ledger belongs to a different view",
            ));
        }
        for plan in turns.iter().filter_map(|turn| turn.plan_id.as_deref()) {
            keys.insert(intent_key(repo, plan)?);
        }
    }
    if keys.len() > 1 {
        return Err(invalid(
            "Ambiguous governing intent in explicit session context",
        ));
    }
    Ok(keys.into_iter().next())
}

/// Read the selection-relevant projection of the existing lifecycle manifest.
/// Its CLI type and loader are private to `commands::agent`; unlike that hook
/// loader, inspection does not discard malformed manifests or clean up leases.
/// These declarations select work; they are never authentication/approval.
fn manifest_plan(repo: &Repository) -> CliResult<Option<String>> {
    let directory = repo.dot_dir().join("agent-lifecycle");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(CliError::Internal(error.into())),
    };
    let root = repo
        .root()
        .canonicalize()
        .map_err(|e| CliError::Internal(e.into()))?;
    let now = chrono::Utc::now().timestamp();
    let mut paths = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| CliError::Internal(e.into()))?;
    paths.sort();
    let mut plans = BTreeSet::new();
    for path in paths {
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|e| CliError::Internal(e.into()))?;
        let manifest: Value = serde_json::from_slice(&bytes).map_err(|e| {
            invalid(format!(
                "Malformed lifecycle manifest {}: {e}",
                path.display()
            ))
        })?;
        let failure = || invalid(format!("Malformed lifecycle manifest {}", path.display()));
        let run_id = manifest["run_id"].as_str().ok_or_else(failure)?;
        if path.file_stem().and_then(|stem| stem.to_str()) != Some(run_id) {
            return Err(failure());
        }
        let expires_at = manifest["expires_at"].as_i64().ok_or_else(failure)?;
        let workdir = manifest["workdir"].as_str().ok_or_else(failure)?;
        if expires_at <= now || !manifest["stop_state"].is_null() {
            continue;
        }
        let workdir = std::path::Path::new(workdir);
        if !workdir.is_absolute() {
            return Err(failure());
        }
        // Do not infer that a run in another worktree, subdirectory or view
        // governs this physical source root.
        if !root.starts_with(workdir) {
            continue;
        }
        if manifest
            .get("view")
            .filter(|view| !view.is_null())
            .is_some_and(|view| view.as_str() != Some(repo.current_view()))
        {
            continue;
        }
        if let Some(work_item) = manifest.get("work_item_id").filter(|item| !item.is_null()) {
            plans.insert(intent_key(repo, work_item.as_str().ok_or_else(failure)?)?);
        }
    }
    if plans.len() > 1 {
        return Err(invalid("Ambiguous governing intents in active lifecycle manifests; select --intent, --turn or --change explicitly"));
    }
    Ok(plans.into_iter().next())
}

fn inspect_turn(record: &SessionRecord, turn: &SessionTurn) -> CliResult<TurnInspection> {
    let mut hashes: Vec<_> = turn.change_hashes.iter().map(Hash::to_base32).collect();
    hashes.sort();
    hashes.dedup();
    Ok(TurnInspection {
        session_id: record.session_id.clone(),
        view_name: record.view_name.clone(),
        turn_number: turn.turn_number,
        status: if hashes.is_empty() {
            "no_recorded_changes"
        } else {
            "recorded"
        }
        .into(),
        session_status: record.status.label().into(),
        plan_id: turn.plan_id.clone(),
        provenance_hash: Some(turn.provenance_hash.to_base32()),
        change_hashes: hashes,
        outcome: encode(&turn.outcome)?,
    })
}

fn pending_turn(store: &SessionStore, session: &AgentSession) -> CliResult<Option<TurnInspection>> {
    if !session.is_turn_active() {
        return Ok(None);
    }
    let number = session
        .turn_count
        .checked_add(1)
        .ok_or_else(|| invalid("Turn overflow"))?;
    let binding = store
        .turn_intent_binding(&session.session_id, number)
        .map_err(|e| CliError::Internal(e.into()))?;
    Ok(Some(TurnInspection {
        session_id: session.session_id.clone(),
        view_name: Some(session.view_name.clone()),
        turn_number: number,
        status: "pending_active_unrecorded".into(),
        session_status: session.status.label().into(),
        plan_id: binding.map(|b| b.intent_uid).or_else(|| {
            session
                .managed_run
                .as_ref()
                .and_then(|run| run.work_item_id.clone())
        }),
        provenance_hash: None,
        change_hashes: vec![],
        outcome: Value::Null,
    }))
}

/// Resolve without mutation, network requests, model invocation or approval.
/// Only an explicit `ATOMIC_SESSION_ID` can supply ambient session context.
pub fn resolve(repo: &Repository, args: &IncrementalArgs) -> CliResult<IncrementalReport> {
    if args.from.is_some() || args.to.is_some() {
        return super::view_comparison::resolve(repo, args);
    }
    if args.view.is_some() {
        return super::view_snapshot::resolve(repo, args);
    }
    let session = match std::env::var("ATOMIC_SESSION_ID") {
        Ok(session) => Some(session),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(invalid("ATOMIC_SESSION_ID is not valid Unicode"))
        }
    };
    resolve_with_session(repo, args, session.as_deref())
}

fn resolve_with_session(
    repo: &Repository,
    args: &IncrementalArgs,
    session_context: Option<&str>,
) -> CliResult<IncrementalReport> {
    if args.from.is_some() || args.to.is_some() {
        return super::view_comparison::resolve(repo, args);
    }
    if [&args.turn, &args.intent, &args.change, &args.view]
        .iter()
        .filter(|s| s.is_some())
        .count()
        > 1
    {
        return Err(invalid(
            "--turn, --intent, --change and --view are mutually exclusive",
        ));
    }
    if args.view.is_some() {
        return super::view_snapshot::resolve(repo, args);
    }
    let store = runtime_store(repo)?;
    let mut selection = Selection {
        kind: String::new(),
        value: None,
        reason: String::new(),
    };
    let mut selected_intent = args
        .intent
        .as_deref()
        .map(|id| intent_key(repo, id))
        .transpose()?;
    if args.turn.is_none() && args.change.is_none() && selected_intent.is_none() {
        if let Some(session) = session_context {
            selected_intent = current_plan(repo, store.as_ref(), session)?;
            if selected_intent.is_some() {
                selection.reason =
                    format!("Unambiguous governing declaration for explicit session {session}");
            }
        }
        if selected_intent.is_none() && session_context.is_none() {
            selected_intent = manifest_plan(repo)?;
            if selected_intent.is_some() {
                selection.reason = "Unambiguous governing declaration in active lifecycle manifest for this source root/view".into();
            }
        }
    }
    let mut roots = BTreeMap::<String, Hash>::new();
    let mut turns = BTreeMap::<(String, u32), TurnInspection>::new();
    let mut related_intents = BTreeSet::new();
    let mut warnings = vec![
        "Inspection only: supplied criterion assessments are review observations, not independent signed approval or automatic acceptance/test-passing verdicts.".into(),
        "Current intent documents are not authenticated historical snapshots; signature verification is not performed.".into(),
        "Working-copy bytes are not part of the recorded selection. Active turns are pending; working-copy status is not inferred.".into(),
    ];
    let ledgers = if let Some(input) = &args.turn {
        let (session, _) = parse_turn(input)?;
        repo.get_session_ledger(session)
            .map_err(CliError::Repository)?
            .into_iter()
            .collect()
    } else if selected_intent.is_some() {
        repo.list_session_ledgers(usize::MAX)
            .map_err(CliError::Repository)?
    } else {
        Vec::new()
    };

    if let Some(input) = &args.change {
        let upper = input.to_ascii_uppercase();
        if !(4..=52).contains(&upper.len())
            || !upper
                .bytes()
                .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
            || (upper.len() == 52 && Hash::from_base32(upper.as_bytes()).is_none())
        {
            return Err(invalid(
                "--change requires a 4–52 character Base32 hash, not a sequence or view selector",
            ));
        }
        let hash = repo
            .find_change_by_prefix(&upper)
            .map_err(CliError::Repository)?
            .ok_or_else(|| CliError::ChangeNotFound {
                hash: input.clone(),
            })?;
        roots.insert(hash.to_base32(), hash);
        selection = Selection {
            kind: "change".into(),
            value: Some(hash.to_base32()),
            reason: "Explicit change selector".into(),
        };
    } else if let Some(input) = &args.turn {
        let (session, number) = parse_turn(input)?;
        let mut found = false;
        for (record, entries) in &ledgers {
            if record.session_id != session {
                continue;
            }
            for turn in entries.iter().filter(|turn| turn.turn_number == number) {
                if found {
                    return Err(invalid("Ambiguous session turn in ledger"));
                }
                if turn.session_id != session {
                    return Err(invalid("Session-turn identity mismatch"));
                }
                found = true;
                for hash in &turn.change_hashes {
                    roots.insert(hash.to_base32(), *hash);
                }
                turns.insert((session.into(), number), inspect_turn(record, turn)?);
                if let Some(plan) = &turn.plan_id {
                    related_intents.insert(plan.clone());
                }
            }
        }
        if let Some(runtime) = runtime_session(store.as_ref(), session)? {
            if let Some(pending) = store
                .as_ref()
                .map(|s| pending_turn(s, &runtime))
                .transpose()?
                .flatten()
            {
                if pending.turn_number == number {
                    if found {
                        return Err(invalid(
                            "Turn is both active and recorded; retry after checkpoint publication",
                        ));
                    }
                    found = true;
                    if let Some(plan) = &pending.plan_id {
                        related_intents.insert(plan.clone());
                    }
                    turns.insert((session.into(), number), pending);
                }
            }
        }
        if !found {
            return Err(invalid(format!("Unknown session turn: {input}")));
        }
        selection = Selection {
            kind: "turn".into(),
            value: Some(input.clone()),
            reason: "Exact session-ledger turn; other turns are excluded".into(),
        };
    } else if let Some(key) = &selected_intent {
        selection.kind = "intent".into();
        selection.value = Some(key.clone());
        if selection.reason.is_empty() {
            selection.reason =
                "Explicit governing intent selector (document-edit associations are excluded)"
                    .into();
        }
        related_intents.insert(key.clone());
        // Canonical per-change governance resolution, not KG path-touch heuristics
        // or the session/turn that happened to create the intent document.
        for hash in repo.iter_changes() {
            let hash = hash.map_err(CliError::Repository)?;
            let change = repo.load_change(&hash).map_err(CliError::Repository)?;
            if change.kind().is_snapshot() {
                continue;
            }
            let graphs = repo
                .find_provenance_for_change(&hash)
                .map_err(CliError::Repository)?;
            let context = repo.change_intent_context(&hash, &change, &graphs);
            if context
                .plans
                .iter()
                .any(|plan| same_intent(repo, &plan.plan_id, key))
            {
                if context.status == "ambiguous" || context.status == "partial" {
                    return Err(invalid(format!(
                        "Incomplete or ambiguous governance for change {}",
                        hash.to_base32()
                    )));
                }
                roots.insert(hash.to_base32(), hash);
            }
        }
        for (record, entries) in &ledgers {
            for turn in entries {
                if turn
                    .plan_id
                    .as_deref()
                    .is_some_and(|plan| same_intent(repo, plan, key))
                {
                    // Preserve observation-only/unrecorded turns too. Recorded
                    // roots above require actual change-specific governance.
                    turns.insert(
                        (record.session_id.clone(), turn.turn_number),
                        inspect_turn(record, turn)?,
                    );
                }
            }
        }
        if let Some(store) = &store {
            for session in store
                .list_sessions()
                .map_err(|e| CliError::Internal(e.into()))?
            {
                if let Some(pending) = pending_turn(store, &session)? {
                    if pending
                        .plan_id
                        .as_deref()
                        .is_some_and(|plan| same_intent(repo, plan, key))
                    {
                        turns.insert((pending.session_id.clone(), pending.turn_number), pending);
                    }
                }
            }
        }
        if roots.is_empty() && turns.is_empty() {
            return Err(invalid(format!(
                "Intent {key} has no recorded governing changes or matching turns"
            )));
        }
    } else {
        // effective_history includes inherited membership; do not silently
        // widen a default review into the entire view's candidate set.
        let history = repo.effective_history(None).map_err(CliError::Repository)?;
        let mut latest = None;
        for entry in history.iter().rev() {
            let change = repo
                .load_change(&entry.hash)
                .map_err(CliError::Repository)?;
            if !change.kind().is_snapshot() {
                latest = Some(entry.hash);
                break;
            }
        }
        let hash = latest.ok_or_else(|| invalid("Current view has no recorded change to inspect; choose --turn or --intent for pending work"))?;
        roots.insert(hash.to_base32(), hash);
        selection = Selection {
            kind: "change".into(),
            value: Some(hash.to_base32()),
            reason:
                "Latest recorded change on current view; no unambiguous current intent declaration"
                    .into(),
        };
    }

    // Validate exactly the selected root membership through the canonical
    // fail-closed dependency API, never through the whole-view triage API.
    let txn = repo
        .pristine()
        .read_txn()
        .map_err(|e| CliError::Internal(e.into()))?;
    let mut ids = Vec::new();
    for hash in roots.values() {
        ids.push(
            txn.get_internal(hash)
                .map_err(|e| CliError::Internal(e.into()))?
                .ok_or_else(|| {
                    invalid(format!(
                        "Selected change {} is not registered locally",
                        hash.to_base32()
                    ))
                })?,
        );
    }
    let closure = graph_visibility_from_membership(&txn, &ViewMembershipSet::from_ordered(ids))
        .map_err(CliError::Repository)?;
    let mut closure_hashes = Vec::new();
    for id in closure.iter_dependency_first() {
        closure_hashes.push(
            txn.get_external(*id)
                .map_err(|e| CliError::Internal(e.into()))?
                .ok_or_else(|| {
                    invalid("Dependency closure contains a change with no external hash")
                })?,
        );
    }
    drop(txn);
    let current_members: BTreeSet<_> = repo
        .effective_history(None)
        .map_err(CliError::Repository)?
        .into_iter()
        .map(|entry| entry.hash.to_base32())
        .collect();
    let mut changes = Vec::new();
    let mut dependency_references = Vec::new();
    let mut registry = atomic_semantic::ParserRegistry::new();
    for hash in &closure_hashes {
        let change = repo.load_change(hash).map_err(CliError::Repository)?;
        let mut dependencies: Vec<_> = change
            .hashed
            .dependencies
            .iter()
            .map(Hash::to_base32)
            .collect();
        dependencies.sort();
        dependencies.dedup();
        let on_current_view = current_members.contains(&hash.to_base32());
        // Stop at the reference boundary before collecting provenance, intent
        // documents, paths, diffs, patch/token evidence or analysis for ancestors.
        if !roots.contains_key(&hash.to_base32()) {
            dependency_references.push(DependencyReference {
                hash: hash.to_base32(),
                direct: false,
                on_current_view,
                summary: change
                    .hashed
                    .header
                    .message
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(240)
                    .collect(),
                dependencies,
            });
            continue;
        }
        let graphs = repo
            .find_provenance_for_change(hash)
            .map_err(CliError::Repository)?;
        let context = repo.change_intent_context(hash, &change, &graphs);
        for plan in &context.plans {
            related_intents.insert(plan.plan_id.clone());
        }
        if selected_intent.is_none() && args.turn.is_none() && roots.contains_key(&hash.to_base32())
        {
            let sessions: BTreeSet<_> = graphs
                .iter()
                .map(|(_, graph)| graph.session_id.as_str())
                .filter(|session| !session.is_empty())
                .collect();
            for session in sessions {
                if let Some((record, entries)) = repo
                    .get_session_ledger(session)
                    .map_err(CliError::Repository)?
                {
                    for turn in entries.iter().filter(|turn| {
                        turn.change_hashes.contains(hash)
                            && graphs.iter().any(|(graph_hash, graph)| {
                                graph.session_id == turn.session_id
                                    && *graph_hash == turn.provenance_hash
                            })
                    }) {
                        turns.insert(
                            (record.session_id.clone(), turn.turn_number),
                            inspect_turn(&record, turn)?,
                        );
                    }
                }
            }
        }
        let mut paths = repo
            .change_modified_paths(hash)
            .map_err(CliError::Repository)?;
        paths.sort();
        paths.dedup();
        let (diff, diff_status) = if on_current_view {
            match recorded_diff(repo, &change, hash) {
                Ok(diff) if !diff.is_empty() || paths.is_empty() => {
                    (Some(diff), "available_current_view_context".into())
                }
                Ok(_) => {
                    warnings.push(format!(
                        "Diff builder returned no display hunks for {}; use recorded_patch",
                        hash.to_base32()
                    ));
                    (None, "unavailable; inspect recorded_patch".into())
                }
                Err(error) => {
                    warnings.push(format!(
                        "Diff unavailable for {}: {error}",
                        hash.to_base32()
                    ));
                    (None, "unavailable; inspect recorded_patch".into())
                }
            }
        } else {
            (None, "off_current_view; inspect recorded_patch".into())
        };
        let patch = repo
            .patch_evidence(&iri::mint_change(hash), Default::default())
            .map_err(CliError::Repository)?;
        let analysis = super::analysis::analyze_change(
            repo,
            &change,
            hash,
            &paths,
            roots.contains_key(&hash.to_base32()),
            on_current_view,
            &mut registry,
        );
        changes.push(ChangeInspection {
            hash: hash.to_base32(),
            selected_root: roots.contains_key(&hash.to_base32()),
            on_current_view,
            metadata: encode(&change.hashed.header)?,
            dependencies,
            paths,
            intent_context: encode(&context)?,
            recorded_patch: encode(&patch)?,
            diff,
            diff_status,
            analysis,
        });
    }
    let direct_dependencies: BTreeSet<_> = changes
        .iter()
        .flat_map(|change| change.dependencies.iter())
        .collect();
    for reference in &mut dependency_references {
        reference.direct = direct_dependencies.contains(&reference.hash);
    }
    let mut pending_work = BTreeMap::new();
    if let Some(store) = &store {
        for session in store
            .list_sessions()
            .map_err(|e| CliError::Internal(e.into()))?
        {
            if session.view_name != repo.current_view() {
                continue;
            }
            if let Some(pending) = pending_turn(store, &session)? {
                let key = (pending.session_id.clone(), pending.turn_number);
                if !turns.contains_key(&key) {
                    pending_work.insert(key, pending);
                }
            }
        }
    }
    let mut intents = BTreeMap::new();
    for reference in related_intents {
        match intent_key(repo, &reference)
            .and_then(|key| inspect_intent(repo, &key).map(|value| (key, value)))
        {
            Ok((key, value)) => {
                intents.insert(key, value);
            }
            Err(error) => warnings.push(format!("Related intent {reference} unavailable: {error}")),
        }
    }
    let view = repo
        .get_view_info(repo.current_view())
        .map_err(CliError::Repository)?;
    let working_copy = repo
        .working_copy_id()
        .map(|id| repo.working_copy_record(id))
        .transpose()
        .map_err(CliError::Repository)?;
    let project = repo.project_identity().map_err(CliError::Repository)?;
    let closure_strings: Vec<_> = closure_hashes.iter().map(Hash::to_base32).collect();
    let dependency_only = closure_strings
        .iter()
        .filter(|hash| !roots.contains_key(*hash))
        .cloned()
        .collect();
    warnings.sort();
    warnings.dedup();
    let mut report = IncrementalReport {
        schema_version: 2,
        purpose: "incremental_inspection_worklist",
        independent_approval: false,
        selection,
        source: json!({
            "repository_root": repo.root(), "atomic_directory": repo.dot_dir(),
            "project_identity": encode(&project)?, "view": view.name,
            "view_state": view.state.to_base32(), "view_scope": view.kind_label(),
            "parent_view": view.parent_name, "view_change_count": view.change_count,
            "working_copy": working_copy.map(|wc| json!({
                "id": wc.id.to_string(), "location_fingerprint": wc.location_fingerprint.to_base32(),
                "desired_view_id": wc.desired_view, "desired_state": wc.desired_state.to_base32(),
                "materialized_state": wc.materialized_state.map(|state| state.to_base32()),
                "materialized_manifest": wc.materialized_manifest.map(|state| state.to_base32()),
            })), "working_copy_status": "not_scanned",
        }),
        selected_roots: roots.into_keys().collect(),
        dependency_closure: closure_strings,
        dependency_only,
        dependency_references,
        changes,
        view_snapshot: None,
        view_comparison: None,
        turns: turns.into_values().collect(),
        pending_work_outside_selection: pending_work.into_values().collect(),
        intents: intents.into_values().collect(),
        warnings,
        graph: None,
    };
    report.graph = Some(super::graph::build(
        repo,
        &report,
        args.findings.as_deref(),
        args.risk_profile.as_deref(),
    )?);
    Ok(report)
}

fn inspect_intent(repo: &Repository, key: &str) -> CliResult<Value> {
    let inputs = bridge::read_intent(repo, key).map_err(CliError::Repository)?;
    let freshness =
        match bridge::load_attestation(repo, key, &inputs).map_err(CliError::Repository)? {
            bridge::Attestation::Fresh(_) => "fresh_not_signature_verified",
            bridge::Attestation::Stale(_) => "stale",
            bridge::Attestation::None => "missing",
        };
    let (node, lift_error) = match bridge::lift(&inputs) {
        Ok(node) => (encode(&node)?, Value::Null),
        Err(error) => (Value::Null, json!(error.to_string())),
    };
    Ok(
        json!({ "id": key, "authenticated": false, "attestation_status": freshness,
        "source_hash": bridge::source_content_hash(&inputs),
        "source": "current_vault_document_not_historical_signed_snapshot",
        "frontmatter": inputs.frontmatter, "body": inputs.body,
        "authenticated_criteria": Value::Null,
        "current_document_criteria_and_tasks": node, "lift_error": lift_error,
        "acceptance_verdict": "not_evaluated" }),
    )
}

fn recorded_diff(
    repo: &Repository,
    change: &atomic_core::change::Change,
    hash: &Hash,
) -> CliResult<String> {
    use atomic_repository::diff_engine::{change_file_diffs, DiffOutputConfig};
    let (mut files, _) = change_file_diffs(
        repo,
        change,
        hash,
        &DiffOutputConfig::new().with_color(false),
    )
    .map_err(CliError::Repository)?;
    files.sort_by(|a, b| (&a.new_path, &a.old_path).cmp(&(&b.new_path, &b.old_path)));
    let mut output = String::new();
    for file in files {
        output.push_str(&format!("--- {}\n+++ {}\n", file.old_path, file.new_path));
        if file.is_binary {
            output.push_str("Binary content differs\n");
        }
        let total: usize = file.hunks.iter().map(|hunk| hunk.lines.len()).sum();
        if total > 3000 {
            output.push_str(&format!(
                "[diff omitted: {total} lines; use atomic diff -c {}]\n",
                hash.to_base32()
            ));
            continue;
        }
        for hunk in file.hunks {
            output.push_str(&hunk.header());
            output.push('\n');
            for line in hunk.lines {
                output.push_str(&line.to_string());
                output.push('\n');
            }
        }
    }
    Ok(output)
}

pub fn run(args: &IncrementalArgs) -> CliResult<()> {
    let root = find_repository_root()?;
    let repo = crate::commands::open_readonly_repository(&root).map_err(CliError::Repository)?;
    let thinking = super::progress::Thinking::start(args.json || args.html);
    let report = resolve(&repo, args);
    drop(thinking);
    let report = report?;
    if args.html {
        let html = super::html::render(&repo, &report)?;
        if !args.no_open {
            crate::output::html::write_and_open_html(&html, args.output.as_deref(), |url| {
                open::that(url)
            })?;
        } else if let Some(path) = &args.output {
            std::fs::write(path, html)?;
        } else {
            print!("{html}");
        }
        return Ok(());
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| CliError::Internal(e.into()))?
        );
    } else {
        print!(
            "{}",
            super::graph::render_with_details(
                report.graph.as_ref().expect("resolved review graph"),
                args.details
            )
        );
        if !args.details {
            for turn in &report.turns {
                println!(
                    "Turn {}:{} — {}",
                    turn.session_id, turn.turn_number, turn.status
                );
            }
            for turn in &report.pending_work_outside_selection {
                println!(
                    "Pending outside recorded selection: {}:{} — {}",
                    turn.session_id, turn.turn_number, turn.status
                );
            }
            for warning in &report.warnings {
                println!("Warning: {warning}");
            }
            return Ok(());
        }
        println!("Incremental inspection worklist — NOT independent signed approval");
        println!(
            "Selection: {} {}",
            report.selection.kind,
            report.selection.value.as_deref().unwrap_or("")
        );
        println!("{}", report.selection.reason);
        println!(
            "Source: {} (state {})",
            report.source["view"].as_str().unwrap_or("unavailable"),
            report.source["view_state"]
        );
        println!("Selected roots: {}", report.selected_roots.join(", "));
        println!("Dependency context: {}", report.dependency_only.join(", "));
        for turn in &report.turns {
            println!(
                "Turn {}:{} — {}",
                turn.session_id, turn.turn_number, turn.status
            );
        }
        for turn in &report.pending_work_outside_selection {
            println!(
                "Pending outside recorded selection: {}:{} — {}",
                turn.session_id, turn.turn_number, turn.status
            );
        }
        for intent in &report.intents {
            println!(
                "\nIntent {} (unauthenticated current document):\n{}",
                intent["id"],
                intent["body"].as_str().unwrap_or("")
            );
        }
        for change in &report.changes {
            println!(
                "\n{} {}\n{}",
                if change.selected_root {
                    "Selected"
                } else {
                    "Dependency"
                },
                change.hash,
                change.metadata["message"].as_str().unwrap_or("")
            );
            println!(
                "Paths: {}\nDiff: {}",
                change.paths.join(", "),
                change.diff_status
            );
            if let Some(diff) = &change.diff {
                print!("{diff}");
            }
            println!(
                "Analyzer: {} ({})",
                change.analysis.analyzer, change.analysis.analyzer_version
            );
            for check in &change.analysis.checks {
                println!(
                    "  {:?} {} {}: {}",
                    check.status, check.rule, check.path, check.detail
                );
            }
            for finding in &change.analysis.findings {
                println!(
                    "  {:?} / {:?}: {} [{}; introduced: {:?}]",
                    finding.severity,
                    finding.confidence,
                    finding.summary,
                    finding.requirement_or_rule.id,
                    finding.introduced_by_change.status
                );
                for location in &finding.locations {
                    println!(
                        "    {}:{}-{}",
                        location.path, location.start_line, location.end_line
                    );
                }
                for evidence in &finding.evidence {
                    println!(
                        "    Evidence {}: {}",
                        evidence.reference, evidence.observation
                    );
                }
                println!("    Correction: {}", finding.proposed_correction);
            }
        }
        for warning in &report.warnings {
            println!("Warning: {warning}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::{Change, ChangeHeader, ProvenanceGraph};
    use atomic_repository::record::RecordOptions;
    use atomic_repository::tracking::TrackingOptions;
    use atomic_repository::{IntentCreateOptions, IntentCreateResult};
    use clap::Parser;

    fn fixture() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        (dir, repo)
    }

    fn record(repo: &Repository, content: &str) -> Hash {
        record_path(repo, "subject.txt", content)
    }

    fn record_path(repo: &Repository, relative: &str, content: &str) -> Hash {
        let path = repo.root().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let exists = path.exists();
        std::fs::write(path, content).unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        if !exists {
            repo.add(wc, relative, TrackingOptions::default()).unwrap();
        }
        *repo
            .record(
                wc,
                ChangeHeader::new(content),
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true)
                    .enrich_kg(false),
            )
            .unwrap()
            .hash()
    }

    fn work(repo: &Repository, title: &str, session: Option<&str>) -> IntentCreateResult {
        repo.vault_intent_create(IntentCreateOptions {
            title: title.into(),
            priority: None,
            assignee: None,
            labels: vec![],
            session_id: session.map(str::to_owned),
            turn_id: session.map(|_| 1),
            kind: None,
        })
        .unwrap()
    }

    fn checkpoint(
        repo: &Repository,
        session: &str,
        timestamp: i64,
        hashes: Vec<Hash>,
        plan: Option<&str>,
    ) -> u32 {
        let mut builder = ProvenanceGraph::builder(session, "test").timestamp(timestamp);
        let previous = repo
            .get_session_ledger(session)
            .unwrap()
            .and_then(|(_, turns)| turns.last().map(|turn| turn.provenance_hash));
        if let Some(previous) = previous {
            builder = builder.previous(previous);
        }
        for hash in &hashes {
            builder = builder.add_change_explained(*hash);
        }
        if let Some(plan) = plan {
            builder = builder.plan_id(plan);
        }
        let graph = builder.build();
        let graph_hash = Hash::of(&graph.serialize().unwrap());
        repo.publish_provenance_checkpoint(
            &graph,
            SessionTurn {
                session_id: session.into(),
                turn_number: 0,
                goal: None,
                provenance_hash: graph_hash,
                change_hashes: hashes,
                previous_provenance: previous,
                timestamp,
                plan_id: plan.map(str::to_owned),
                todos: vec![],
                boundary_start: None,
                boundary_end: None,
                outcome: None,
            },
        )
        .unwrap();
        repo.get_session_ledger(session)
            .unwrap()
            .unwrap()
            .1
            .into_iter()
            .find(|turn| turn.provenance_hash == graph_hash)
            .unwrap()
            .turn_number
    }

    fn inspect(repo: &Repository, args: IncrementalArgs) -> CliResult<IncrementalReport> {
        resolve_with_session(repo, &args, None)
    }

    fn graph_fixture() -> (tempfile::TempDir, Repository, IntentCreateResult, Hash) {
        let (dir, repo) = fixture();
        let intent = work(&repo, "Deliver work", None);
        repo.vault_intent_update(&intent.id, atomic_repository::IntentUpdateOptions {
            content: Some(format!(":::why\nDeliver working behavior.\n:::\n\n:::acceptance-criterion{{#{}-ac-1 status=met verifiedBy=author evidence=claimed}}\nCalling work returns successfully.\n:::\n\n:::task{{#{}-1 status=done criteria={}-ac-1}}\nImplement work.\n::file-ref{{path=src/lib.rs}}\n:::\n", intent.uid, intent.uid, intent.uid)),
            ..Default::default()
        }).unwrap();
        let hash = record_path(&repo, "src/lib.rs", "pub fn work() { todo!(); }\n");
        checkpoint(
            &repo,
            "review-graph-source",
            10,
            vec![hash],
            Some(&intent.uid),
        );
        (dir, repo, intent, hash)
    }

    fn input_file(
        repo: &Repository,
        report: &IncrementalReport,
        findings: Vec<atomic_canonical::review_graph::SubmittedFinding>,
    ) -> std::path::PathBuf {
        let path = repo.root().join("review-observations.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&atomic_canonical::review_graph::FindingsInput {
                schema: atomic_canonical::review_graph::INPUT_SCHEMA.into(),
                scope: report.graph.as_ref().unwrap().scope.clone(),
                findings,
                risk_assessment: None,
            })
            .unwrap(),
        )
        .unwrap();
        path
    }

    #[test]
    fn graph_links_numbered_findings_to_unmet_criteria_without_rewriting_intents() {
        use atomic_canonical::{
            review_finding::{CriterionAssessment, CriterionOutcome},
            review_graph::{ReviewPass, SubmittedFinding},
        };
        let (_dir, repo, intent, hash) = graph_fixture();
        let args = IncrementalArgs {
            change: Some(hash.to_base32()),
            ..Default::default()
        };
        let before = inspect(&repo, args.clone()).unwrap();
        let graph = before.graph.as_ref().unwrap();
        assert_eq!(graph.passes.len(), 3);
        assert_eq!(graph.criteria.len(), 1);
        assert_eq!(graph.criteria[0].declared_status, "met");
        assert_eq!(
            graph.criteria[0].review_assessment,
            CriterionOutcome::Unknown
        );
        assert_eq!(graph.findings.len(), 1);
        assert_eq!(graph.findings[0].number, 1);
        assert_eq!(
            graph.findings[0].related_criteria,
            vec![graph.criteria[0].id.clone()]
        );
        assert!(!graph
            .edges
            .iter()
            .any(|edge| edge.relation == "assesses_unmet"));
        let mut finding = graph.findings[0].finding.clone();
        finding.acceptance_criteria.push(CriterionAssessment {
            criterion: graph.criteria[0].id.clone(),
            assessment: CriterionOutcome::Unmet,
            rationale: "The unshadowed todo! macro panics instead of returning successfully".into(),
        });
        let path = input_file(
            &repo,
            &before,
            vec![SubmittedFinding {
                pass: ReviewPass::IntentConformance,
                finding,
            }],
        );
        let database_before = std::fs::read(repo.dot_dir().join("atomic.redb")).unwrap();
        let source_before =
            bridge::source_content_hash(&bridge::read_intent(&repo, &intent.id).unwrap());
        let reviewed = inspect(
            &repo,
            IncrementalArgs {
                findings: Some(path),
                ..args
            },
        )
        .unwrap();
        let graph = reviewed.graph.as_ref().unwrap();
        assert_eq!(
            graph.findings.len(),
            1,
            "same finding across passes is numbered once"
        );
        assert_eq!(
            graph.findings[0].passes,
            vec![ReviewPass::AntiSlop, ReviewPass::IntentConformance]
        );
        assert_eq!(graph.criteria[0].review_assessment, CriterionOutcome::Unmet);
        assert_eq!(graph.criteria[0].findings, vec![1]);
        assert!(graph
            .edges
            .iter()
            .any(|edge| edge.from == graph.findings[0].id
                && edge.relation == "assesses_unmet"
                && edge.to == graph.criteria[0].id));
        assert!(graph.edges.iter().any(|edge| edge.relation == "within"));
        assert!(graph
            .edges
            .iter()
            .all(|edge| graph.nodes.iter().any(|node| node.id == edge.from)
                && graph.nodes.iter().any(|node| node.id == edge.to)));
        let rendered = super::super::graph::render(graph);
        for expected in [
            "F1",
            "Anti-slop",
            "Intent conformance",
            "Blast radius / risk",
            "review=Unmet",
            "1–1 (after)",
            "src/lib.rs",
            "declared=met",
        ] {
            assert!(rendered.contains(expected), "{expected}: {rendered}");
        }
        if let Some(path) = std::env::var_os("ATOMIC_REVIEW_CONFORMANCE_ARTIFACT") {
            std::fs::write(path, super::super::html::render(&repo, &reviewed).unwrap()).unwrap();
        }
        assert_eq!(
            source_before,
            bridge::source_content_hash(&bridge::read_intent(&repo, &intent.id).unwrap())
        );
        assert_eq!(
            database_before,
            std::fs::read(repo.dot_dir().join("atomic.redb")).unwrap()
        );
    }

    #[test]
    fn graph_rejects_stale_foreign_unsupported_and_unevidenced_judgments() {
        use atomic_canonical::{
            review_finding::{CriterionAssessment, CriterionOutcome},
            review_graph::{ReviewPass, SubmittedFinding},
        };
        let (_dir, repo, _intent, hash) = graph_fixture();
        let args = IncrementalArgs {
            change: Some(hash.to_base32()),
            ..Default::default()
        };
        let report = inspect(&repo, args.clone()).unwrap();
        let mut finding = report.graph.as_ref().unwrap().findings[0].finding.clone();
        finding.acceptance_criteria.push(CriterionAssessment {
            criterion: report.graph.as_ref().unwrap().criteria[0].id.clone(),
            assessment: CriterionOutcome::Unmet,
            rationale: "The placeholder panics".into(),
        });
        let observation = SubmittedFinding {
            pass: ReviewPass::IntentConformance,
            finding,
        };
        for mutation in [
            "criterion",
            "evidence",
            "path",
            "line",
            "change",
            "scope",
            "source_hash",
            "membership",
            "rationale",
            "conflict",
        ] {
            let path = input_file(&repo, &report, vec![observation.clone()]);
            let mut input: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            match mutation {
                "criterion" => {
                    input["findings"][0]["finding"]["acceptance_criteria"][0]["criterion"] =
                        json!("urn:atomic:ac:unrelated")
                }
                "evidence" => input["findings"][0]["finding"]["evidence"] = json!([]),
                "path" => {
                    input["findings"][0]["finding"]["locations"][0]["path"] = json!("../lib.rs")
                }
                "line" => input["findings"][0]["finding"]["locations"][0]["end_line"] = json!(100),
                "change" => input["findings"][0]["finding"]["change"] = json!("unselected-change"),
                "scope" => input["scope"]["view_state"] = json!("stale-state"),
                "source_hash" => {
                    let id = report.graph.as_ref().unwrap().criteria[0].intent.clone();
                    input["scope"]["intent_sources"][id] = json!("stale-source");
                }
                "membership" => {
                    input["scope"]["source_membership_hash"] = json!("stale-membership")
                }
                "rationale" => {
                    input["findings"][0]["finding"]["acceptance_criteria"][0]["rationale"] =
                        json!("")
                }
                "conflict" => {
                    let mut conflicting = input["findings"][0].clone();
                    conflicting["finding"]["acceptance_criteria"][0]["assessment"] =
                        json!("unknown");
                    input["findings"].as_array_mut().unwrap().push(conflicting);
                }
                _ => unreachable!(),
            }
            std::fs::write(&path, serde_json::to_vec(&input).unwrap()).unwrap();
            assert!(
                inspect(
                    &repo,
                    IncrementalArgs {
                        findings: Some(path),
                        ..args.clone()
                    }
                )
                .is_err(),
                "accepted {mutation}"
            );
        }
        let unrelated = work(&repo, "Unrelated intent", None);
        repo.vault_intent_update(&unrelated.id, atomic_repository::IntentUpdateOptions {
            content: Some(format!(":::why\nUnrelated work.\n:::\n\n:::acceptance-criterion{{#{}-ac-1 status=unmet}}\nA different requirement.\n:::\n", unrelated.uid)),
            ..Default::default()
        }).unwrap();
        let unrelated_node =
            bridge::lift(&bridge::read_intent(&repo, &unrelated.id).unwrap()).unwrap();
        let path = input_file(&repo, &report, vec![observation]);
        let mut input: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        input["findings"][0]["finding"]["acceptance_criteria"][0]["criterion"] =
            json!(unrelated_node.has_acceptance_criterion[0].id);
        std::fs::write(&path, serde_json::to_vec(&input).unwrap()).unwrap();
        assert!(inspect(
            &repo,
            IncrementalArgs {
                findings: Some(path),
                ..args
            }
        )
        .unwrap_err()
        .to_string()
        .contains("governing intent"));
    }

    #[test]
    fn graph_numbers_are_deterministic_and_input_order_does_not_duplicate_findings() {
        use atomic_canonical::review_graph::{ReviewPass, SubmittedFinding};
        let (_dir, repo) = fixture();
        let hash = record_path(
            &repo,
            "src/lib.rs",
            "fn a() { todo!(); }\nfn b() { unimplemented!(); }\n",
        );
        let args = IncrementalArgs {
            change: Some(hash.to_base32()),
            ..Default::default()
        };
        let report = inspect(&repo, args.clone()).unwrap();
        let observations: Vec<_> = report
            .graph
            .as_ref()
            .unwrap()
            .findings
            .iter()
            .map(|finding| SubmittedFinding {
                pass: ReviewPass::BlastRadiusRisk,
                finding: finding.finding.clone(),
            })
            .collect();
        let path = input_file(&repo, &report, observations.clone());
        let forward = inspect(
            &repo,
            IncrementalArgs {
                findings: Some(path),
                ..args.clone()
            },
        )
        .unwrap();
        let path = input_file(&repo, &report, observations.into_iter().rev().collect());
        let reverse = inspect(
            &repo,
            IncrementalArgs {
                findings: Some(path),
                ..args
            },
        )
        .unwrap();
        assert_eq!(
            encode(&forward.graph).unwrap(),
            encode(&reverse.graph).unwrap()
        );
        let graph = forward.graph.unwrap();
        assert_eq!(
            graph
                .findings
                .iter()
                .map(|finding| finding.number)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(graph
            .findings
            .iter()
            .all(|finding| finding.passes.len() == 2));
        assert!(
            graph.criteria.is_empty(),
            "no governance must not become invented criteria"
        );
    }

    #[test]
    fn graph_risk_context_includes_prior_callers_but_not_later_or_dirty_source() {
        let (_dir, repo) = fixture();
        record_path(
            &repo,
            "src/api.ts",
            "export function changed() { return 1; }\n",
        );
        record_path(
            &repo,
            "src/caller.ts",
            "import { changed } from './api';\nexport function consumer() { return changed(); }\n",
        );
        let selected = record_path(
            &repo,
            "src/api.ts",
            "export function changed() { return 2; }\n",
        );
        record_path(
            &repo,
            "src/caller.ts",
            "export function later() { return 3; }\n",
        );
        std::fs::write(
            repo.root().join("src/caller.ts"),
            "export function dirty() {}\n",
        )
        .unwrap();
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(selected.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.selected_roots, vec![selected.to_base32()]);
        let html = super::super::html::render(&repo, &report).unwrap();
        let document = html
            .split("id=\"review-data\"")
            .nth(1)
            .unwrap()
            .split_once('>')
            .unwrap()
            .1
            .split("</script>")
            .next()
            .unwrap();
        let document: Value = serde_json::from_str(document).unwrap();
        let snapshot = document["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|snapshot| snapshot["path"] == "src/caller.ts")
            .expect("caller reference remains visible as context");
        assert_eq!(snapshot["reviewed_root"], false);
        for side in ["before", "after"] {
            assert_eq!(snapshot[side]["status"], "unavailable");
            assert!(snapshot[side]["text"].is_null());
        }
        let graph = report.graph.unwrap();
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.relation == "statically_bound_direct_call"),
            "{}",
            serde_json::to_string_pretty(&graph).unwrap()
        );
        assert!(graph
            .nodes
            .iter()
            .any(|node| node.label.contains("consumer")));
        assert!(!graph
            .nodes
            .iter()
            .any(|node| node.label.contains("#later") || node.label.contains("#dirty")));
        assert!(graph
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("context only")));
        let risk = graph.risk.as_ref().unwrap();
        assert_eq!(
            risk.assessment.rating,
            atomic_canonical::review_graph::RiskRating::Medium
        );
        assert_eq!(risk.impact[0].callers_after.len(), 1);
        let text = super::super::graph::render(&graph);
        assert!(text.contains("Risk: Medium") && text.contains("Direct impact:"));
        assert!(!text.contains("(potential impact)") && !text.contains("#consumer"));
    }

    #[test]
    fn automatic_impact_measures_callers_and_reports_real_contract_breakage() {
        use atomic_canonical::review_graph::RiskRating;
        let (_dir, repo) = fixture();
        record_path(
            &repo,
            "src/api.ts",
            "export function work(x: number) { return x; }\n",
        );
        record_path(&repo, "src/user.ts", "import {work as invoke} from './api';\nexport function consumer() { return invoke(1); }\n");
        let hash = record_path(
            &repo,
            "src/api.ts",
            "export function work(x: number, y: number) { return x + y; }\n",
        );
        record_path(
            &repo,
            "src/user.ts",
            "export function later() { return 0; }\n",
        );
        std::fs::write(repo.root().join("src/user.ts"), "function dirty() {}\n").unwrap();
        let database_before = std::fs::read(repo.dot_dir().join("atomic.redb")).unwrap();
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let graph = report.graph.as_ref().unwrap();
        let risk = graph.risk.as_ref().unwrap();
        assert_eq!(risk.assessment.rating, RiskRating::High);
        assert_eq!(risk.impact[0].changed_functions.len(), 1);
        assert_eq!(risk.impact[0].callers_before.len(), 1);
        assert_eq!(risk.impact[0].callers_after.len(), 1);
        assert_eq!(risk.impact[0].checked_call_contracts, 1);
        assert_eq!(risk.impact[0].issues.len(), 1);
        assert_eq!(graph.findings.len(), 1);
        assert_eq!(
            graph.findings[0].finding.requirement_or_rule.id,
            "compatibility.resolved-direct-call-contract"
        );
        assert_eq!(graph.findings[0].finding.locations[0].path, "src/api.ts");
        assert!(graph.findings[0]
            .finding
            .evidence
            .iter()
            .any(|evidence| evidence.reference.contains("src/user.ts")
                && evidence.observation.contains("consumer")));
        assert!(graph
            .edges
            .iter()
            .any(|edge| edge.relation == "statically_bound_direct_call"));
        assert!(graph
            .edges
            .iter()
            .all(|edge| graph.nodes.iter().any(|node| node.id == edge.from)
                && graph.nodes.iter().any(|node| node.id == edge.to)));
        assert!(!graph
            .nodes
            .iter()
            .any(|node| node.label.contains("#later") || node.label.contains("#dirty")));
        let text = super::super::graph::render(graph);
        assert!(text.contains("Risk: High") && text.contains("1 current bound caller(s)"));
        assert!(text.contains("F1") && text.contains("1 incompatible after-contract(s)"));
        assert_eq!(
            database_before,
            std::fs::read(repo.dot_dir().join("atomic.redb")).unwrap()
        );
    }

    #[test]
    fn added_bad_call_is_a_numbered_risk_issue_without_user_submitted_findings() {
        let (_dir, repo) = fixture();
        let hash = record_path(
            &repo,
            "src/lib.rs",
            "fn work(x: i32) {} fn consumer() { work(); }\n",
        );
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let graph = report.graph.as_ref().unwrap();
        assert_eq!(graph.findings.len(), 1);
        assert_eq!(
            graph.findings[0].finding.introduced_by_change.status,
            atomic_canonical::review_finding::IntroductionStatus::Yes
        );
        assert!(graph.findings[0].finding.validate().is_ok());
        assert_eq!(
            graph.risk.as_ref().unwrap().assessment.rating,
            atomic_canonical::review_graph::RiskRating::High
        );
    }

    #[test]
    fn removed_function_preserves_before_side_finding_and_caller_evidence() {
        let (_dir, repo) = fixture();
        record_path(&repo, "src/api.ts", "export function work() {}\n");
        record_path(
            &repo,
            "src/user.ts",
            "import {work} from './api'; function consumer() { work(); }\n",
        );
        let hash = record_path(&repo, "src/api.ts", "\n");
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let graph = report.graph.as_ref().unwrap();
        assert_eq!(graph.findings.len(), 1);
        assert_eq!(
            graph.findings[0].finding.locations[0].side,
            atomic_canonical::review_finding::SourceSide::Before
        );
        assert_eq!(
            graph.risk.as_ref().unwrap().impact[0].callers_before.len(),
            1
        );
        assert!(graph
            .edges
            .iter()
            .any(|edge| edge.relation == "surviving_reference_to_removed_function"));
    }

    #[test]
    fn semantic_review_searches_before_ast_budgets_and_reaches_late_changed_files() {
        let (_dir, repo) = fixture();
        let wc = repo.require_working_copy_id().unwrap();
        std::fs::create_dir_all(repo.root().join("a_noise")).unwrap();
        for index in 0..300 {
            // Deliberately unreliable unrelated syntax: neither AST errors nor
            // normal cfg/macro limitations should enter this change's review.
            std::fs::write(
                repo.root().join(format!("a_noise/{index:03}.rs")),
                format!("#[cfg(test)] macro_noise_{index}!(); invalid value {{{{\n"),
            )
            .unwrap();
        }
        repo.add(wc, "a_noise", TrackingOptions::default()).unwrap();
        record_path(
            &repo,
            "z_api.ts",
            "export function work(x:number) { return x; } function value(x:number) { return x; }\n",
        );
        record_path(
            &repo,
            "z_user.ts",
            "import {work as invoke} from './z_api'; function consumer() { return invoke(1); }\n",
        );
        let hash = record_path(
            &repo,
            "z_api.ts",
            "export function work(x:number,y:number) { return x+y; } function value(x:number) { return x+1; }\n",
        );
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let graph = report.graph.as_ref().unwrap();
        let impact = &graph.risk.as_ref().unwrap().impact[0];
        assert!(impact.searched_files_before >= 300 && impact.searched_files_after >= 300);
        assert_eq!(impact.inspected_files_before, 2);
        assert_eq!(impact.inspected_files_after, 2);
        assert_eq!(impact.callers_after.len(), 1);
        assert_eq!(impact.issues.len(), 1);
        assert_eq!(
            graph.risk.as_ref().unwrap().assessment.rating,
            atomic_canonical::review_graph::RiskRating::High
        );
        assert!(!impact
            .gaps
            .iter()
            .any(|gap| gap.contains("a_noise") || gap.contains("256 files")));
    }

    #[test]
    fn risk_profile_is_explicit_validated_and_cannot_lower_a_known_issue_floor() {
        let (_dir, repo) = fixture();
        record_path(
            &repo,
            "src/lib.rs",
            "fn work(x:i32) {} fn caller() { work(1); }\n",
        );
        let hash = record_path(
            &repo,
            "src/lib.rs",
            "fn work(x:i32,y:i32) {} fn caller() { work(1); }\n",
        );
        let path = repo.root().join("risk-profile.json");
        std::fs::write(
            &path,
            r#"{"name":"team-calibration","broad_functions":1000,"api_growth":1000}"#,
        )
        .unwrap();
        let args = IncrementalArgs {
            change: Some(hash.to_base32()),
            risk_profile: Some(path.clone()),
            ..Default::default()
        };
        let report = inspect(&repo, args.clone()).unwrap();
        let risk = report.graph.as_ref().unwrap().risk.as_ref().unwrap();
        assert_eq!(
            risk.assessment.rating,
            atomic_canonical::review_graph::RiskRating::High
        );
        assert_eq!(
            risk.model.as_ref().unwrap().profile.name,
            "team-calibration"
        );
        assert_eq!(risk.findings.len(), 1);
        std::fs::write(path, r#"{"api_growth":0}"#).unwrap();
        assert!(inspect(&repo, args).is_err());
    }

    #[test]
    fn common_name_collisions_remain_diagnostics_and_never_set_risk() {
        let (_dir, repo) = fixture();
        record_path(
            &repo,
            "unrelated.ts",
            "function run() { return 1; }\nfunction consumer() { return run(); }\n",
        );
        let hash = record_path(&repo, "tests/helper.ts", "function run() { return 2; }\n");
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let graph = report.graph.as_ref().unwrap();
        let risk = graph.risk.as_ref().unwrap();
        assert_eq!(
            risk.impact[0].callers_after.len(),
            0,
            "unrelated same-name functions are not callers"
        );
        assert_eq!(
            risk.assessment.rating,
            atomic_canonical::review_graph::RiskRating::Low
        );
        assert_eq!(risk.basis, "rule_model");
        assert_eq!(
            risk.model
                .as_ref()
                .unwrap()
                .facts
                .observed_production_callers,
            0
        );
        assert!(risk.findings.is_empty());
        let text = super::super::graph::render(graph);
        assert!(!text.contains("#consumer") && !text.contains("potential impact"));
        assert!(graph
            .edges
            .iter()
            .all(|edge| graph.nodes.iter().any(|node| node.id == edge.from)
                && graph.nodes.iter().any(|node| node.id == edge.to)));
    }

    #[test]
    fn reviewer_risk_is_scope_pinned_and_cannot_downgrade_numbered_issues() {
        use atomic_canonical::review_graph::{
            ReviewPass, RiskCoverage, RiskRating, SubmittedFinding,
        };
        let (_dir, repo, _intent, hash) = graph_fixture();
        let args = IncrementalArgs {
            change: Some(hash.to_base32()),
            ..Default::default()
        };
        let report = inspect(&repo, args.clone()).unwrap();
        let mut issue = report.graph.as_ref().unwrap().findings[0].finding.clone();
        issue.severity = atomic_canonical::review_finding::Severity::High;
        issue.confidence = atomic_canonical::review_finding::Confidence::Medium;
        issue.summary =
            "The selected placeholder panics when invoked, violating the caller's success contract"
                .into();
        let path = input_file(
            &repo,
            &report,
            vec![SubmittedFinding {
                pass: ReviewPass::BlastRadiusRisk,
                finding: issue,
            }],
        );
        let reviewed = inspect(
            &repo,
            IncrementalArgs {
                findings: Some(path.clone()),
                ..args.clone()
            },
        )
        .unwrap();
        let graph = reviewed.graph.as_ref().unwrap();
        let risk = graph.risk.as_ref().unwrap();
        assert_eq!(risk.assessment.rating, RiskRating::High);
        assert_eq!(risk.assessment.coverage, RiskCoverage::Partial);
        assert_eq!(
            risk.assessment.confidence,
            atomic_canonical::review_finding::Confidence::Medium
        );
        assert_eq!(risk.basis, "findings");
        assert_eq!(risk.findings.len(), 1);
        let text = super::super::graph::render_with_details(graph, true);
        assert!(
            text.contains("Risk: High")
                && text.contains("success contract")
                && text.contains("Correction:")
        );
        let mut input: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        input["risk_assessment"] = json!({
            "rating": "low", "confidence": "high", "coverage": "complete",
            "rationale": "Claims low risk", "limitations": [],
            "evidence": [{"kind": "source", "reference": "src/lib.rs:1", "observation": "Selected source inspected"}],
        });
        std::fs::write(&path, serde_json::to_vec(&input).unwrap()).unwrap();
        assert!(inspect(
            &repo,
            IncrementalArgs {
                findings: Some(path),
                ..args
            }
        )
        .is_err());
    }

    #[test]
    fn public_api_growth_gets_an_estimate_without_inventing_findings() {
        use atomic_canonical::review_graph::RiskRating;
        let (_dir, repo) = fixture();
        let hash = record_path(
            &repo,
            "src/api.ts",
            "export function work() { return 1; }\n",
        );
        let args = IncrementalArgs {
            change: Some(hash.to_base32()),
            ..Default::default()
        };
        let report = inspect(&repo, args.clone()).unwrap();
        assert_eq!(
            report
                .graph
                .as_ref()
                .unwrap()
                .risk
                .as_ref()
                .unwrap()
                .assessment
                .rating,
            RiskRating::Medium
        );
        let path = input_file(&repo, &report, vec![]);
        let mut input: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        input["risk_assessment"] = json!({
            "rating": "low", "confidence": "medium", "coverage": "complete",
            "rationale": "Reviewed the selected return contract and supported consumers", "limitations": [],
            "evidence": [{"kind": "source", "reference": "src/api.ts:1", "observation": "Selected function returns a constant; caller and export inspection found no incompatible consumers in scoped recorded context"}],
        });
        std::fs::write(&path, serde_json::to_vec(&input).unwrap()).unwrap();
        let database_before = std::fs::read(repo.dot_dir().join("atomic.redb")).unwrap();
        let reviewed = inspect(
            &repo,
            IncrementalArgs {
                findings: Some(path.clone()),
                ..args.clone()
            },
        )
        .unwrap();
        let graph = reviewed.graph.as_ref().unwrap();
        assert_eq!(
            graph.risk.as_ref().unwrap().assessment.rating,
            RiskRating::Low
        );
        assert_eq!(graph.risk.as_ref().unwrap().basis, "reviewer");
        assert_eq!(
            graph.risk.as_ref().unwrap().model.as_ref().unwrap().rating,
            atomic_audit::risk::RiskLevel::Medium
        );
        assert!(graph.findings.is_empty());
        assert!(super::super::graph::render(graph).contains("Risk: Low"));
        assert_eq!(
            database_before,
            std::fs::read(repo.dot_dir().join("atomic.redb")).unwrap()
        );
        input["scope"]["view_state"] = json!("stale");
        std::fs::write(&path, serde_json::to_vec(&input).unwrap()).unwrap();
        assert!(inspect(
            &repo,
            IncrementalArgs {
                findings: Some(path),
                ..args
            }
        )
        .is_err());
    }

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: IncrementalArgs,
    }

    #[test]
    fn clap_selectors_are_exclusive_but_json_is_orthogonal() {
        let cli = Cli::try_parse_from([
            "review",
            "--change",
            "ABCD",
            "--findings",
            "observations.json",
            "--details",
            "--json",
        ])
        .unwrap();
        assert!(cli.args.details && cli.args.json && cli.args.findings.is_some());
        assert!(Cli::try_parse_from(["review", "--turn", "s:0", "--intent", "1"]).is_err());
        assert!(Cli::try_parse_from(["review", "--intent", "1", "--change", "ABCD"]).is_err());
        assert!(Cli::try_parse_from(["review", "--turn", "s:0", "--change", "ABCD"]).is_err());
        assert!(
            Cli::try_parse_from(["review", "--change", "ABCD", "--json"])
                .unwrap()
                .args
                .json
        );
    }

    #[test]
    fn html_flags_follow_existing_export_conventions_without_leaking_to_json() {
        let cli = Cli::try_parse_from([
            "review",
            "--change",
            "ABCD",
            "--html",
            "--no-open",
            "--output",
            "review.html",
        ])
        .unwrap();
        assert!(cli.args.html && cli.args.no_open);
        assert_eq!(
            cli.args.output.unwrap(),
            std::path::PathBuf::from("review.html")
        );
        for args in [
            vec!["review", "--html", "--json"],
            vec!["review", "--html", "--details"],
            vec!["review", "--no-open"],
            vec!["review", "--output", "review.html"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn html_exports_recorded_source_and_card_references_not_dirty_bytes() {
        let (_dir, repo) = fixture();
        let source =
            "fn work() { let label = r#\"</script><img src=x onerror=alert(1)>\"#; todo!(); }\n";
        let hash = record_path(&repo, "src/lib.rs", source);
        std::fs::write(
            repo.root().join("src/lib.rs"),
            "fn UNRECORDED_VIEWER_PAYLOAD() {}\n",
        )
        .unwrap();
        let mut report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let before = std::fs::read(repo.dot_dir().join("atomic.redb")).unwrap();
        let archive_only = "selected patch archive probe ".repeat(20_000);
        report.changes[0].recorded_patch["archive_only_probe"] =
            json!({"payload":&archive_only,"id":u64::MAX});
        let html = super::super::html::render(&repo, &report).unwrap();
        assert!(
            html.len() < archive_only.len() / 2,
            "selected patch archive must not inflate viewer startup"
        );
        assert!(!html.contains("UNRECORDED_VIEWER_PAYLOAD") && !html.contains("<img src=x"));
        assert!(html.contains("\\u003c/script\\u003e"));
        let data = html
            .split("id=\"review-data\"")
            .nth(1)
            .unwrap()
            .split_once('>')
            .unwrap()
            .1
            .split("</script>")
            .next()
            .unwrap();
        let document: Value = serde_json::from_str(data).unwrap();
        let compressed = data_encoding::BASE64
            .decode(
                document["report_archive"]["data"]
                    .as_str()
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        let original: Value =
            serde_json::from_reader(flate2::read::GzDecoder::new(compressed.as_slice())).unwrap();
        assert_eq!(original, serde_json::to_value(&report).unwrap());
        assert_eq!(
            document["view"]["graph"]["findings"],
            original["graph"]["findings"]
        );
        assert!(document["view"]["changes"][0]
            .get("recorded_patch")
            .is_none());
        assert_eq!(document["audit_cards"].as_array().unwrap().len(), 10);
        assert_eq!(document["metric_cards"].as_array().unwrap().len(), 10);
        let snapshot = document["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|source| source["path"] == "src/lib.rs")
            .unwrap();
        assert_eq!(snapshot["after"]["text"], source);
        assert_eq!(snapshot["before"]["status"], "absent");
        let graph = report.graph.as_ref().unwrap();
        let projected = super::super::presentation::regions(graph);
        let metric_ids: BTreeSet<_> = projected
            .iter()
            .flat_map(|region| region.metrics.iter().cloned())
            .collect();
        assert_eq!(
            metric_ids,
            graph
                .nodes
                .iter()
                .filter(|node| node.kind == "metric")
                .map(|node| node.id.clone())
                .collect()
        );
        assert!(projected.iter().any(|region| region.findings.contains(&1)));
        assert_eq!(
            before,
            std::fs::read(repo.dot_dir().join("atomic.redb")).unwrap()
        );
    }

    #[test]
    fn analysis_uses_selected_recorded_bytes_not_later_or_dirty_content() {
        use atomic_canonical::review_finding::IntroductionStatus;
        let (_dir, repo) = fixture();
        record_path(&repo, "src/lib.rs", "pub fn work() {}\n");
        let selected = record_path(&repo, "src/lib.rs", "pub fn work() { todo!(); }\n");
        record_path(
            &repo,
            "src/lib.rs",
            "pub fn work() { println!(\"done\"); }\n",
        );
        std::fs::write(
            repo.root().join("src/lib.rs"),
            "pub fn work() { unimplemented!(); }\n",
        )
        .unwrap();
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(selected.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let root = report
            .changes
            .iter()
            .find(|change| change.selected_root)
            .unwrap();
        assert_eq!(root.analysis.findings.len(), 1);
        let finding = &root.analysis.findings[0];
        assert_eq!(finding.change, selected.to_base32());
        assert_eq!(finding.requirement_or_rule.id, "anti-slop.rust-placeholder");
        assert_eq!(finding.introduced_by_change.status, IntroductionStatus::Yes);
        assert!(finding.evidence[0].observation.contains("todo!()"));
        assert!(!finding.evidence[0].observation.contains("unimplemented"));
        assert_eq!(report.changes.len(), 1);
        assert!(!report.dependency_references.is_empty());
        assert!(report.changes.iter().all(|change| change.selected_root));
        assert!(!report.independent_approval);
        assert_eq!(
            serde_json::to_value(&root.analysis).unwrap()["schema"],
            "atomic.review-analysis.v1"
        );
    }

    #[test]
    fn automatic_metrics_use_recorded_baselines_and_graph_anchors_without_failing_criteria() {
        use atomic_audit::{Measurement, Metric};
        let (_dir, repo) = fixture();
        record_path(
            &repo,
            "src/measure.ts",
            "export class C {\n run(x: number) { return x; }\n}\n",
        );
        let selected = record_path(&repo, "src/measure.ts", "export class C {\n run(x: number, y: number) {\n  if (x) {\n   return x + y;\n  }\n  return y;\n }\n private helper() { return 0; }\n}\n");
        record_path(&repo, "src/measure.ts", "export class Later {}\n");
        std::fs::write(
            repo.root().join("src/measure.ts"),
            "export class Dirty {}\n",
        )
        .unwrap();
        let database_before = std::fs::read(repo.dot_dir().join("atomic.redb")).unwrap();
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(selected.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let change = report
            .changes
            .iter()
            .find(|change| change.selected_root)
            .unwrap();
        assert_eq!(
            change
                .analysis
                .metrics
                .iter()
                .map(|record| record.observation.metric)
                .collect::<BTreeSet<_>>(),
            Metric::ALL.into_iter().collect()
        );
        for (metric, subject, before, after) in [
            (Metric::MethodLength, "function:C::run:1", 1, 6),
            (Metric::ParameterCount, "function:C::run:1", 1, 2),
            (Metric::CyclomaticComplexity, "function:C::run:1", 1, 2),
            (Metric::MethodCount, "class:C:1", 1, 2),
            (Metric::ClassLength, "class:C:1", 3, 9),
            (Metric::ClassComplexity, "class:C:1", 1, 3),
            (Metric::PublicSurface, "src/measure.ts", 2, 2),
        ] {
            let record = change
                .analysis
                .metrics
                .iter()
                .find(|record| {
                    record.observation.metric == metric
                        && record.observation.subject.ends_with(subject)
                })
                .unwrap();
            assert_eq!(
                record.observation.before,
                Measurement::Measured { value: before },
                "{metric:?}"
            );
            assert_eq!(
                record.observation.after,
                Measurement::Measured { value: after },
                "{metric:?}"
            );
            assert_eq!(record.delta, Some(i64::from(after) - i64::from(before)));
            assert!(record.before_location.is_some() && record.after_location.is_some());
        }
        let graph = report.graph.as_ref().unwrap();
        assert_eq!(
            graph
                .nodes
                .iter()
                .filter(|node| node.kind == "metric_card")
                .count(),
            10
        );
        assert!(graph.nodes.iter().any(|node| node.kind == "metric"));
        assert!(graph.edges.iter().any(|edge| edge.relation == "defined_by"));
        assert!(graph
            .edges
            .iter()
            .all(|edge| graph.nodes.iter().any(|node| node.id == edge.from)
                && graph.nodes.iter().any(|node| node.id == edge.to)));
        assert!(
            graph.findings.is_empty(),
            "metric values are not automatic bug findings"
        );
        assert!(!graph
            .edges
            .iter()
            .any(|edge| edge.relation == "assesses_unmet"));
        let text = super::super::graph::render(graph);
        assert!(!text.contains("Metric changes") && !text.contains("├──"));
        assert!(text.contains("--html") && text.contains("No numbered audit findings"));
        let details = super::super::graph::render_with_details(graph, true);
        assert!(details.contains("metric.method-length") && details.contains("delta +5"));
        for metric in graph.nodes.iter().filter(|node| node.kind == "metric") {
            assert!(details.contains(&metric.label), "missing {}", metric.label);
        }
        for card in atomic_audit::METRIC_CARDS {
            assert!(details.contains(card.title), "missing {}", card.title);
        }
        assert!(details.contains("Audit Invalid or missing syntax: 0 finding(s)"));
        assert!(details.contains("Audit Failure reported as success: 0 finding(s)"));
        assert!(!text.contains("Not automatically audited:"));
        assert!(!change.analysis.metrics.iter().any(|record| record
            .observation
            .subject
            .contains("Later")
            || record.observation.subject.contains("Dirty")));
        assert_eq!(
            database_before,
            std::fs::read(repo.dot_dir().join("atomic.redb")).unwrap()
        );
    }

    #[test]
    fn vault_only_changes_skip_code_analysis_without_losing_intent_evidence() {
        use atomic_canonical::review_graph::PassStatus;
        let (_dir, repo, intent, _) = graph_fixture();
        let hash = record_path(
            &repo,
            ".vault/memory/generated.ts",
            "not valid TypeScript {{{\n",
        );
        checkpoint(
            &repo,
            "vault-only-review",
            20,
            vec![hash],
            Some(&intent.uid),
        );
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.selected_roots, vec![hash.to_base32()]);
        let change = report
            .changes
            .iter()
            .find(|change| change.selected_root)
            .unwrap();
        assert!(change.paths.contains(&".vault/memory/generated.ts".into()));
        assert!(change
            .diff
            .as_ref()
            .unwrap()
            .contains("not valid TypeScript"));
        assert_eq!(
            change.analysis.excluded_metadata_paths,
            vec![".vault/memory/generated.ts"]
        );
        assert!(
            change.analysis.checks.is_empty()
                && change.analysis.metrics.is_empty()
                && change.analysis.findings.is_empty()
        );
        assert!(change.analysis.metric_comparison_scope.is_empty());
        assert!(
            !report.intents.is_empty(),
            "vault remains available as intent evidence"
        );
        let graph = report.graph.as_ref().unwrap();
        assert!(!graph.criteria.is_empty());
        assert_eq!(graph.passes[0].status, PassStatus::NotApplicable);
        assert_eq!(graph.passes[2].status, PassStatus::NotApplicable);
        assert_eq!(
            graph.risk.as_ref().unwrap().assessment.rating,
            atomic_canonical::review_graph::RiskRating::NotApplicable
        );
        assert!(!graph
            .nodes
            .iter()
            .any(|node| node.kind == "metric" || node.kind == "check"));
        assert!(!graph
            .diagnostics
            .iter()
            .any(|message| message.contains("Static risk context")
                || message.contains("Static relationship budget")));
        let text = super::super::graph::render(graph);
        assert!(text.contains("No code to analyze") && text.contains("retained as evidence"));
        if let Some(path) = std::env::var_os("ATOMIC_REVIEW_EMPTY_ARTIFACT") {
            std::fs::write(path, super::super::html::render(&repo, &report).unwrap()).unwrap();
        }
        assert!(!text.contains("metric.class-") && !text.contains("Unsupported language"));
    }

    #[test]
    fn mixed_code_and_vault_changes_measure_only_the_code() {
        let (_dir, repo) = fixture();
        record_path(&repo, ".vault/generated.ts", "invalid {{{\n");
        std::fs::write(
            repo.root().join(".vault/generated.ts"),
            "still invalid }}}\n",
        )
        .unwrap();
        let hash = record_path(
            &repo,
            "src/real.ts",
            "export class C { work(x: number) { return x; } }\n",
        );
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let change = report
            .changes
            .iter()
            .find(|change| change.selected_root)
            .unwrap();
        assert!(change.paths.contains(&".vault/generated.ts".into()));
        assert!(change
            .analysis
            .excluded_metadata_paths
            .contains(&".vault/generated.ts".into()));
        assert!(!change.analysis.metrics.is_empty());
        assert!(change
            .analysis
            .metrics
            .iter()
            .all(|record| record.path == "src/real.ts"));
        assert!(change
            .analysis
            .checks
            .iter()
            .all(|check| check.path == "src/real.ts"));
        assert_eq!(change.analysis.metric_comparison_scope, vec!["src/real.ts"]);
        assert!(!change.analysis.findings.iter().any(|finding| finding
            .locations
            .iter()
            .any(|location| atomic_repository::is_vault_path(&location.path))));
    }

    #[test]
    fn all_seven_substantive_audits_produce_numbered_recorded_findings() {
        let (_dir, repo) = fixture();
        let mut source = String::from(
            r#"
fn masking() -> Result<(),()> { match write() { Ok(())=>Ok(()), Err(_)=>Ok(()) } }
fn duplicate(b:bool) { if b { send(1); } else { send(1); } }
fn implementation(x:i32)->i32 { x+1 }
fn middle(x:i32)->i32 { implementation(x) }
fn outer(x:i32)->i32 { middle(x) }
async fn locking() { let m=std::sync::Mutex::new(0); let guard=m.lock().unwrap(); work().await; drop(guard); }
#[test] fn weak_test() { let result=work(); assert_eq!(result,result); }
fn unsafe_cast(bits:u32)->f32 { unsafe { std::mem::transmute::<u32,f32>(bits) } }
fn large(x:i32) {
"#,
        );
        for i in 0..15 {
            source.push_str(&format!("if x == {i} {{ work(); }}\n"));
        }
        for _ in 0..100 {
            source.push_str("work();\n");
        }
        source.push_str("}\n");
        let hash = record_path(&repo, "src/audited.rs", &source);
        // Later and dirty source must not replace the recorded audit evidence.
        std::fs::write(repo.root().join("src/audited.rs"), "fn clean() {}\n").unwrap();
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let graph = report.graph.as_ref().unwrap();
        let expected: BTreeSet<_> = [
            "anti-slop.failure-masking",
            "anti-slop.duplicated-behavior",
            "anti-slop.unnecessary-machinery",
            "anti-slop.manageability",
            "anti-slop.async-resource-safety",
            "anti-slop.weak-verification",
            "anti-slop.unsafe-shortcuts",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            graph
                .findings
                .iter()
                .map(|finding| finding.finding.requirement_or_rule.id.as_str())
                .collect::<BTreeSet<_>>(),
            expected
        );
        assert_eq!(graph.findings.len(), 7);
        for finding in &graph.findings {
            assert!(finding.finding.validate().is_ok());
            assert_eq!(finding.finding.change, hash.to_base32());
            assert_eq!(
                finding.finding.introduced_by_change.status,
                atomic_canonical::review_finding::IntroductionStatus::Yes
            );
            assert!(finding
                .finding
                .locations
                .iter()
                .all(|location| location.path == "src/audited.rs"));
            assert!(!finding.finding.evidence.is_empty());
        }
        let text = super::super::graph::render(graph);
        for number in 1..=7 {
            assert!(text.contains(&format!("F{number} ·")));
        }
        assert!(!text.contains("Not automatically audited:"));
        assert!(text.contains("Audit/rule: anti-slop.failure-masking"));
        assert!(!text.contains("├──"));
    }

    #[test]
    fn structural_findings_precede_metric_signals_and_audit_coverage_is_named() {
        let (_dir, repo) = fixture();
        let hash = record_path(&repo, "src/lib.rs", "fn work() { todo!(); }\n");
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let graph = report.graph.as_ref().unwrap();
        let text = super::super::graph::render_with_details(graph, true);
        assert!(text.contains("Audit Unfinished required behavior: 1 finding(s) · 1 completed"));
        assert!(
            text.contains("Audit Undocumented empty catch: 0 finding(s) · 0 completed · 1 skipped")
        );
        assert!(text.find("F1 ·").unwrap() < text.find("Metric changes").unwrap());
        assert!(!text.contains("Not automatically audited:"));
        assert!(text.contains("Tests or completion claims without behavioral evidence"));
        assert_eq!(
            graph.findings.len(),
            1,
            "metric signals must not manufacture findings"
        );
    }

    #[test]
    fn terminal_groups_noncode_metrics_without_discarding_json_observations() {
        let (_dir, repo) = fixture();
        let wc = repo.require_working_copy_id().unwrap();
        for (path, content) in [
            ("README.md", "# Example\n"),
            ("package.json", "{}\n"),
            ("package-lock.json", "{}\n"),
            ("tsconfig.json", "{}\n"),
            ("test/cli.test.mjs", "console.log('test');\n"),
        ] {
            let absolute = repo.root().join(path);
            std::fs::create_dir_all(absolute.parent().unwrap()).unwrap();
            std::fs::write(absolute, content).unwrap();
            repo.add(wc, path, TrackingOptions::default()).unwrap();
        }
        let hash = record_path(&repo, "src/cli.ts", "console.log('hello');\n");
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let analysis = &report
            .changes
            .iter()
            .find(|change| change.selected_root)
            .unwrap()
            .analysis;
        assert_eq!(analysis.metrics.len(), 60);
        let graph = report.graph.as_ref().unwrap();
        let json_before = serde_json::to_value(graph).unwrap();
        let compact = super::super::graph::render(graph);
        assert!(!compact.contains("Unsupported language") && !compact.contains("N/A → N/A"));
        let text = super::super::graph::render_with_details(graph, true);
        assert_eq!(
            text.split("Metric coverage gaps")
                .nth(1)
                .unwrap()
                .matches("Unsupported language")
                .count(),
            1
        );
        assert!(text.contains("5 path(s), 50 observation(s)"));
        assert!(text.contains("test/cli.test.mjs"));
        assert!(text.contains("Metric changes (subject counts, not findings)"));
        assert!(text.contains("Risk: Medium") && text.contains("Declared public surface"));
        let duplication_row = text
            .lines()
            .find(|line| {
                line.trim_start_matches("│   ")
                    .starts_with("Duplicated tokens in changed code  ")
            })
            .unwrap();
        assert!(
            duplication_row.ends_with("   0    0     0     1       0       5     0"),
            "{duplication_row}"
        );
        assert!(text.contains("metric.duplicated-tokens src/cli.ts: N/A → 0"));
        assert!(!compact.contains("README.md#<Class>"));
        assert!(text.contains("Intent conformance [Blocked]"));
        assert_eq!(json_before, serde_json::to_value(graph).unwrap());
        assert_eq!(
            graph
                .nodes
                .iter()
                .filter(|node| node.kind == "metric")
                .count(),
            60
        );
    }

    #[test]
    fn analysis_reports_explicit_skips_for_unregistered_file_languages() {
        use atomic_canonical::review_finding::CheckStatus;
        let (_dir, repo) = fixture();
        let hash = record(&repo, "not a supported code file\n");
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        let analysis = &report
            .changes
            .iter()
            .find(|change| change.selected_root)
            .unwrap()
            .analysis;
        assert!(analysis.findings.is_empty());
        assert!(!analysis.checks.is_empty());
        assert!(analysis
            .checks
            .iter()
            .all(|check| check.status == CheckStatus::Skipped
                && check.detail.contains("Unsupported language")));
    }

    #[test]
    fn exact_change_has_only_its_causal_context_and_real_diff() {
        let (_dir, repo) = fixture();
        let first = record(&repo, "first\n");
        let second = record(&repo, "second\n");
        let third = record(&repo, "third\n");
        let report = inspect(
            &repo,
            IncrementalArgs {
                change: Some(second.to_base32()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.selected_roots, vec![second.to_base32()]);
        assert!(report.dependency_only.contains(&first.to_base32()));
        assert!(!report.dependency_closure.contains(&third.to_base32()));
        let selected = report
            .changes
            .iter()
            .find(|change| change.selected_root)
            .unwrap();
        assert_eq!(selected.metadata["message"], "second\n");
        let diff = selected.diff.as_deref().unwrap();
        assert!(diff.contains("+second"), "{diff}");
        assert!(diff.contains("-first"), "{diff}");
        assert!(!report.independent_approval);
    }

    #[test]
    fn exact_turn_does_not_sweep_other_turns_in_session() {
        let (_dir, repo) = fixture();
        let first = record(&repo, "first\n");
        let second = record(&repo, "second\n");
        let turn = checkpoint(&repo, "same-session", 10, vec![first], None);
        checkpoint(&repo, "same-session", 20, vec![second], None);
        let report = inspect(
            &repo,
            IncrementalArgs {
                turn: Some(format!("same-session:{turn}")),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.selected_roots, vec![first.to_base32()]);
        assert!(!report.dependency_closure.contains(&second.to_base32()));
        assert_eq!(report.turns.len(), 1);
        assert_eq!(report.turns[0].turn_number, turn);
    }

    #[test]
    fn dependency_references_preserve_edges_without_duplicating_selected_ancestors() {
        let (_dir, repo) = fixture();
        let first = record(&repo, "first\n");
        let second = record(&repo, "second\n");
        let third = record(&repo, "third\n");
        let fourth = record(&repo, "fourth\n");
        let turn = checkpoint(&repo, "multi-root", 10, vec![third, fourth], None);
        let report = inspect(
            &repo,
            IncrementalArgs {
                turn: Some(format!("multi-root:{turn}")),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.changes.len(), 2);
        assert!(report.changes.iter().all(|change| change.selected_root));
        assert_eq!(report.dependency_references.len(), 2);
        let references: BTreeMap<_, _> = report
            .dependency_references
            .iter()
            .map(|reference| (reference.hash.as_str(), reference))
            .collect();
        // Graph context may depend directly on older changes as well as the
        // immediately preceding one. Use recorded edges, never chronology.
        for reference in references.values() {
            assert_eq!(
                reference.direct,
                report
                    .changes
                    .iter()
                    .any(|change| change.dependencies.contains(&reference.hash))
            );
        }
        assert!(references[second.to_base32().as_str()].direct);
        assert!(references[second.to_base32().as_str()]
            .dependencies
            .contains(&first.to_base32()));
        assert!(!references.contains_key(third.to_base32().as_str()));
        assert!(!references.contains_key(fourth.to_base32().as_str()));
    }

    #[test]
    fn exact_intent_uses_governance_not_creation_session_or_path_overlap() {
        let (_dir, repo) = fixture();
        let selected = work(&repo, "Selected work", Some("shared-session"));
        let other = work(&repo, "Other work", Some("shared-session"));
        let first = record(&repo, "first\n");
        let second = record(&repo, "second\n");
        checkpoint(
            &repo,
            "shared-session",
            10,
            vec![first],
            Some(&selected.uid),
        );
        checkpoint(&repo, "shared-session", 20, vec![second], Some(&other.id));
        let report = inspect(
            &repo,
            IncrementalArgs {
                intent: Some(selected.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.selected_roots, vec![first.to_base32()]);
        assert!(!report.dependency_closure.contains(&second.to_base32()));
        assert_eq!(report.turns.len(), 1);
        assert_eq!(report.intents[0]["authenticated"], false);
        assert_eq!(report.intents[0]["acceptance_verdict"], "not_evaluated");
    }

    #[test]
    fn empty_turn_is_reported_without_falling_back_to_latest() {
        let (_dir, repo) = fixture();
        record(&repo, "unrelated\n");
        let turn = checkpoint(&repo, "observation", 10, vec![], None);
        let report = inspect(
            &repo,
            IncrementalArgs {
                turn: Some(format!("observation:{turn}")),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(report.selected_roots.is_empty());
        assert!(report.dependency_closure.is_empty());
        assert_eq!(report.turns[0].status, "no_recorded_changes");
    }

    #[test]
    fn unknown_and_malformed_selectors_fail_closed() {
        let (_dir, repo) = fixture();
        for turn in [
            "",
            "session",
            ":1",
            "../escape:1",
            "s:-1",
            "s:one",
            "s:4294967296",
            "unknown:0",
        ] {
            assert!(
                inspect(
                    &repo,
                    IncrementalArgs {
                        turn: Some(turn.into()),
                        ..Default::default()
                    }
                )
                .is_err(),
                "{turn}"
            );
        }
        for hash in [
            "",
            "A",
            "@",
            "@~1",
            "bad/hash",
            "ABCDEFG1",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(
                inspect(
                    &repo,
                    IncrementalArgs {
                        change: Some(hash.into()),
                        ..Default::default()
                    }
                )
                .is_err(),
                "{hash}"
            );
        }
        assert!(inspect(
            &repo,
            IncrementalArgs {
                change: Some(Hash::of(b"absent").to_base32()),
                ..Default::default()
            }
        )
        .is_err());
        assert!(inspect(
            &repo,
            IncrementalArgs {
                intent: Some("UNKNOWN::nobody::999".into()),
                ..Default::default()
            }
        )
        .is_err());
        assert!(inspect(
            &repo,
            IncrementalArgs {
                turn: Some("s:1".into()),
                change: Some("ABCD".into()),
                ..Default::default()
            }
        )
        .is_err());
    }

    #[test]
    fn known_intent_without_governance_is_no_match_not_whole_view() {
        let (_dir, repo) = fixture();
        let intent = work(&repo, "Creation is not governance", Some("s"));
        let hash = record(&repo, "unrelated\n");
        checkpoint(&repo, "s", 10, vec![hash], None);
        let result = inspect(
            &repo,
            IncrementalArgs {
                intent: Some(intent.id),
                ..Default::default()
            },
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no recorded governing"));
    }

    #[test]
    fn default_selects_only_latest_and_empty_view_errors() {
        let (_dir, repo) = fixture();
        assert!(inspect(&repo, IncrementalArgs::default()).is_err());
        record(&repo, "first\n");
        let latest = record(&repo, "latest\n");
        let report = inspect(&repo, IncrementalArgs::default()).unwrap();
        assert_eq!(report.selected_roots, vec![latest.to_base32()]);
        assert_eq!(report.selection.kind, "change");
    }

    #[test]
    fn active_turn_and_default_explicit_context_do_not_invent_recorded_changes() {
        let (_dir, repo) = fixture();
        let intent = work(&repo, "Pending work", None);
        let store = SessionStore::new(repo.dot_dir().join("sessions")).unwrap();
        let mut session = AgentSession::new("active", "test", "Test");
        session.view_name = repo.current_view().into();
        session.phase = atomic_agent::turn::phase::Phase::Active;
        session.begin_turn();
        store.save(&session).unwrap();
        store
            .bind_turn_intent("active", Some(1), &intent.uid)
            .unwrap();
        let report =
            resolve_with_session(&repo, &IncrementalArgs::default(), Some("active")).unwrap();
        assert_eq!(report.selection.kind, "intent");
        assert_eq!(report.selection.value, Some(intent.id));
        assert!(report.selected_roots.is_empty());
        assert_eq!(report.turns[0].status, "pending_active_unrecorded");
        let turn = inspect(
            &repo,
            IncrementalArgs {
                turn: Some("active:1".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(turn.changes.is_empty());
        assert_eq!(turn.turns[0].status, "pending_active_unrecorded");
        assert!(resolve_with_session(&repo, &IncrementalArgs::default(), Some("missing")).is_err());
    }

    #[test]
    fn missing_dependency_fails_instead_of_returning_partial_closure() {
        use atomic_core::pristine::MutTxnT;
        let (_dir, repo) = fixture();
        let mut change = Change::empty(ChangeHeader::new("incomplete"));
        change
            .hashed
            .dependencies
            .push(Hash::of(b"missing dependency"));
        let hash = repo.save_change(&change).unwrap();
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.register_change(&hash).unwrap();
        txn.commit().unwrap();
        let result = inspect(
            &repo,
            IncrementalArgs {
                change: Some(hash.to_base32()),
                ..Default::default()
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn ambiguous_hash_and_intent_prefixes_are_errors() {
        let (_dir, repo) = fixture();
        // Search headers in memory and persist only the colliding pair. This
        // exercises the real shared prefix resolver, not a mocked lookup.
        let mut seen = BTreeMap::<String, Change>::new();
        let mut pair = None;
        for index in 0..16384 {
            let change = Change::empty(ChangeHeader::new(format!("collision-{index}")));
            let prefix = change.hash().unwrap().to_base32()[..4].to_owned();
            if let Some(previous) = seen.remove(&prefix) {
                pair = Some((prefix, previous, change));
                break;
            }
            seen.insert(prefix, change);
        }
        let (prefix, first, second) = pair.expect("deterministic four-character prefix collision");
        repo.save_change(&first).unwrap();
        repo.save_change(&second).unwrap();
        let error = inspect(
            &repo,
            IncrementalArgs {
                change: Some(prefix),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().to_ascii_lowercase().contains("ambiguous"));
        let first = work(&repo, "First", None);
        let second = work(&repo, "Second", None);
        let prefix: String = first
            .uid
            .chars()
            .zip(second.uid.chars())
            .take_while(|(left, right)| left == right)
            .map(|(ch, _)| ch)
            .collect();
        assert!(prefix.chars().any(|ch| ch.is_ascii_alphabetic()));
        let error = inspect(
            &repo,
            IncrementalArgs {
                intent: Some(prefix),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().to_ascii_lowercase().contains("ambiguous"));
    }

    fn manifest(repo: &Repository, run: &str, intent: &str, expires_at: i64) {
        let directory = repo.dot_dir().join("agent-lifecycle");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("{run}.json")),
            serde_json::to_vec(&json!({
                "run_id": run, "owner_agent": "test", "owner_session_id": "owner",
                "work_item_id": intent, "view": repo.current_view(), "workdir": repo.root(),
                "created_at": 1, "updated_at": 1, "expires_at": expires_at,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn default_uses_unambiguous_active_manifest_and_rejects_conflicts() {
        let (_dir, repo) = fixture();
        let selected = work(&repo, "Manifest work", None);
        let other = work(&repo, "Other work", None);
        let first = record(&repo, "first\n");
        checkpoint(&repo, "recorded", 10, vec![first], Some(&selected.uid));
        let latest = record(&repo, "latest unrelated\n");
        manifest(
            &repo,
            "selected",
            &selected.id,
            chrono::Utc::now().timestamp() + 3600,
        );
        manifest(&repo, "expired", &other.id, 1);
        let report = inspect(&repo, IncrementalArgs::default()).unwrap();
        assert_eq!(report.selection.kind, "intent");
        assert_eq!(report.selected_roots, vec![first.to_base32()]);
        assert!(!report.dependency_closure.contains(&latest.to_base32()));
        manifest(
            &repo,
            "conflict",
            &other.id,
            chrono::Utc::now().timestamp() + 3600,
        );
        assert!(inspect(&repo, IncrementalArgs::default())
            .unwrap_err()
            .to_string()
            .contains("Ambiguous"));
        // Explicit selectors are not affected by unrelated ambient manifests.
        assert!(inspect(
            &repo,
            IncrementalArgs {
                change: Some(latest.to_base32()),
                ..Default::default()
            }
        )
        .is_ok());
    }

    #[test]
    fn malformed_manifest_and_corrupt_explicit_session_fail_closed() {
        let (_dir, repo) = fixture();
        record(&repo, "latest\n");
        let directory = repo.dot_dir().join("agent-lifecycle");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("broken.json"), b"{").unwrap();
        assert!(inspect(&repo, IncrementalArgs::default()).is_err());
        let store = SessionStore::new(repo.dot_dir().join("sessions")).unwrap();
        std::fs::write(store.sessions_dir().join("broken.json"), b"{").unwrap();
        assert!(resolve_with_session(&repo, &IncrementalArgs::default(), Some("broken")).is_err());
    }

    #[test]
    fn explicit_ledger_context_requires_unambiguous_governance() {
        let (_dir, repo) = fixture();
        let first_intent = work(&repo, "First", None);
        let second_intent = work(&repo, "Second", None);
        let first = record(&repo, "first\n");
        checkpoint(&repo, "ledger", 10, vec![first], Some(&first_intent.id));
        let report =
            resolve_with_session(&repo, &IncrementalArgs::default(), Some("ledger")).unwrap();
        assert_eq!(report.selection.value, Some(first_intent.id));
        let second = record(&repo, "second\n");
        checkpoint(&repo, "ledger", 20, vec![second], Some(&second_intent.id));
        assert!(resolve_with_session(&repo, &IncrementalArgs::default(), Some("ledger")).is_err());
    }

    #[test]
    fn inspection_reopened_readonly_does_not_change_database_or_create_session_store() {
        let (dir, repo) = fixture();
        let hash = record(&repo, "stable\n");
        let database = repo.dot_dir().join("atomic.redb");
        drop(repo);
        let before = std::fs::read(&database).unwrap();
        let repo = Repository::open_readonly(dir.path()).unwrap();
        let args = IncrementalArgs {
            change: Some(hash.to_base32()),
            ..Default::default()
        };
        let first = encode(&inspect(&repo, args.clone()).unwrap()).unwrap();
        let second = encode(&inspect(&repo, args).unwrap()).unwrap();
        assert_eq!(first, second);
        assert_eq!(before, std::fs::read(&database).unwrap());
        assert!(!repo.dot_dir().join("sessions").exists());
    }
}
