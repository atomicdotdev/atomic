//! Outcome rollup — "what did this body of work cost and produce?".
//!
//! A [`CandidateSet`](crate::repository::CandidateSet) answers "is *this* view
//! ready to promote?" for exactly one source view against one target. An
//! outcome answers the question above that: given **N** views, what would
//! promoting all of them at once cost, touch, and collide on?
//!
//! # Why this is cheap
//!
//! Everything the graph layer consumes is already a `HashSet<NodeId>` and is
//! indifferent to which views produced it — `ViewGraph`, `RetrieveOptions`,
//! `MaterializeOptions.change_filter`. Only the filter *constructors* are
//! single-view, and [`visible_change_ids_for_views`](crate::repository::filter::visible_change_ids_for_views)
//! closes that gap. So the union of N views is representable everywhere below
//! the repository API today; this module is a set of folds over data that is
//! already loaded, plus one read-only materialization into a discard-everything
//! working copy.
//!
//! # Honesty rules
//!
//! Two places in this module are deliberately conservative, because a confident
//! wrong number is worse than no number:
//!
//! - **Cost is per-change provenance, never session attestations.** An
//!   attestation's `cost_usd` covers a whole session, so across N views it
//!   would either double-count or have to be split with no principled basis.
//!   Per-change [`Provenance`](atomic_core::change::provenance::Provenance) is
//!   already attributed. When nothing in the set carries provenance the report
//!   says so via [`OutcomeCost::cost_known`] rather than printing `$0.00`,
//!   which would read as "this work was free".
//!
//! - **Mergeability is a three-state answer, not a boolean.** The semantic
//!   merge engine is two-way only (see `SemanticMergeEngine::try_merge`),
//!   so a conflict spanning three or more source views is *unadjudicable* —
//!   it degrades to `NoCrdtData` internally and surfaces identically to a real
//!   conflict. Reporting those as "needs a human" would be indistinguishable
//!   from a pass, which is precisely the unfalsifiable claim this product
//!   exists to eliminate. See [`Mergeability`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use atomic_canonical::{outcome_reference, OutcomePins};
use atomic_core::output::repo::FileConflictType;
use atomic_core::pristine::CachedGraphTxn;
use serde::Serialize;

use super::*;

/// Token and dollar spend for one model.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelSpend {
    /// Model name as recorded in provenance.
    pub model: String,
    /// Candidate changes that used this model.
    pub changes: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    /// `input + output` (cache and reasoning are reported separately; they are
    /// subsets of the provider's own billing categories, not additive here).
    pub total_tokens: u64,
    /// Exact cost in millionths of a dollar.
    pub micro_usd: u64,
    /// The same cost as a float, for display.
    pub usd: f64,
}

/// The cost side of an outcome.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OutcomeCost {
    /// Total cost, float view.
    pub usd: f64,
    /// Total cost, exact integer micro-USD. Authoritative; `usd` is derived.
    pub micro_usd: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    /// Spend bucketed by model, ordered by model name.
    pub by_model: Vec<ModelSpend>,
    /// Candidate changes carrying at least one provenance record.
    pub attributed_changes: usize,
    /// Candidate changes with no provenance — human- or system-authored.
    pub unattributed_changes: usize,
    /// Candidate changes the change store could not load. Excluded from every
    /// count and total above, and reported separately so a silent skip cannot
    /// quietly distort the spend figure.
    pub unreadable_changes: usize,
    /// False when *no* candidate change carried provenance — the harness did
    /// not record it. `usd == 0` then means "unknown", not "free".
    pub cost_known: bool,
}

impl OutcomeCost {
    /// True when the rollup is empty of attributable spend.
    pub fn is_empty(&self) -> bool {
        self.by_model.is_empty()
    }
}

/// One conflicted file, attributed to the source views that produced the
/// overlapping changes.
#[derive(Debug, Clone, Serialize)]
pub struct OutcomeConflict {
    /// Repository-relative path.
    pub path: String,
    /// Graph conflict kind: `name`, `order`, `cyclic`, `zombie`, or
    /// `zombie_file`.
    pub kind: String,
    /// Distinct source views contributing changes to this file, sorted.
    pub views: Vec<String>,
    /// How many of those views are *concurrent* with each other. 0 or 1 means
    /// the overlap is only with already-present content; 2 is a two-way
    /// disagreement the merge engine could adjudicate; **3 or more is beyond
    /// the two-way engine's reach** and is reported as not computable.
    pub concurrent_sources: usize,
}

/// Whether the candidate set can be promoted as one unit.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Mergeability {
    /// The union materializes with no residual conflict.
    Clean,
    /// Conflicts found, and every one spans at most two source views — each is
    /// a two-way overlap the merge engine could adjudicate, so a human decides.
    NeedsHuman {
        /// The conflicts, sorted by path.
        conflicts: Vec<OutcomeConflict>,
    },
    /// At least one conflict spans three or more source views. The semantic
    /// merge engine is two-way only, so this report **cannot** say whether the
    /// set merges cleanly. This is explicitly not a pass.
    NotComputable {
        /// Why the answer is unavailable, in plain English.
        reason: String,
        /// Every conflict found, including the adjudicable ones.
        conflicts: Vec<OutcomeConflict>,
    },
}

impl Mergeability {
    /// True only for [`Mergeability::Clean`].
    pub fn is_clean(&self) -> bool {
        matches!(self, Mergeability::Clean)
    }
}

/// The shape of the work: how many changes, how many files, how much of it is
/// unaccounted for.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OutcomeFootprint {
    /// Distinct candidate changes across all sources.
    pub changes: usize,
    /// Distinct files touched across all sources.
    pub files: usize,
    /// Every path touched, sorted.
    pub paths: Vec<String>,
    /// Changes in the set that no task in any intent touches — "baggage".
    pub baggage: usize,
    /// The `file:` node ids of the baggage changes, sorted.
    pub baggage_files: Vec<String>,
}

/// One source view's own contribution to the outcome.
#[derive(Debug, Clone, Serialize)]
pub struct ViewOutcome {
    /// View name.
    pub view: String,
    /// `shared` or `draft`.
    pub scope: String,
    /// Parent view name, if any.
    pub parent: Option<String>,
    /// Changes this view contributes to the union that `target` does not have.
    pub changes: usize,
    /// Distinct files those changes touch.
    pub files: usize,
    /// Spend attributed to exactly this view's contribution.
    ///
    /// The per-view contributions form a **partition** of the union: a change
    /// present in several sources is attributed to exactly one of them
    /// (deterministically, the lexicographically first view name) so the
    /// per-view figures sum to the total rather than double-counting it.
    pub cost: OutcomeCost,
}

/// The pinned facts an outcome is a fact about.
#[derive(Debug, Clone, Serialize)]
pub struct OutcomeInputs {
    /// Base32 Merkle of the target view.
    pub target_merkle: String,
    /// View name → base32 Merkle, for every source.
    pub source_merkle: BTreeMap<String, String>,
    /// Base32 change hashes in the union of the sources' deltas, sorted.
    pub candidate_changes: Vec<String>,
}

/// The collapsed rollup of N views against a target.
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    /// The source views rolled up, in the order the caller named them.
    pub sources: Vec<String>,
    /// The view they would be promoted into.
    pub target: String,
    /// The pinned inputs this outcome is a fact about.
    pub inputs: OutcomeInputs,
    /// `urn:atomic:outcome:<blake3>` over `inputs`.
    pub reference: String,
    /// Total spend across the union.
    pub cost: OutcomeCost,
    /// The shape of the work.
    pub footprint: OutcomeFootprint,
    /// Whether it can be promoted as one unit.
    pub mergeability: Mergeability,
    /// Per-source breakdown.
    pub views: Vec<ViewOutcome>,
}

impl Repository {
    /// Roll up `sources` into a single outcome relative to `target`.
    ///
    /// `sources` may be one view or many; the result is the same shape either
    /// way, so a caller can promote a set without special-casing the common
    /// single-view case. Order does not matter — the reference is
    /// order-insensitive.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::ViewNotFound`] if any named view does not
    /// exist, or [`RepositoryError::Database`] on a pristine access failure.
    pub fn outcome(&self, sources: &[String], target: &str) -> Result<Outcome, RepositoryError> {
        if sources.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: "outcome requires at least one source view".to_string(),
            });
        }

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let resolve = |name: &str| -> Result<atomic_core::pristine::ViewState, RepositoryError> {
            txn.get_view(name)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: name.to_string(),
                })
        };

        let target_view = resolve(target)?;
        let target_visible = collect_visible_change_ids_with_deps(&txn, &target_view)?;

        // Per-source deltas, and the union of them.
        let mut states = Vec::with_capacity(sources.len());
        let mut per_view_visible: HashMap<String, HashSet<NodeId>> = HashMap::new();
        let mut union: HashSet<NodeId> = HashSet::new();
        for name in sources {
            let state = resolve(name)?;
            let visible = collect_visible_change_ids_with_deps(&txn, &state)?;
            let delta: HashSet<NodeId> = visible
                .iter()
                .copied()
                .filter(|id| !target_visible.contains(id))
                .collect();
            union.extend(delta.iter().copied());
            per_view_visible.insert(name.clone(), delta);
            states.push(state);
        }

        // External hashes for the union, in deterministic order.
        let mut candidate: Vec<Hash> = Vec::new();
        for id in &union {
            if let Some(hash) = txn
                .get_external(*id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                candidate.push(hash);
            }
        }
        candidate.sort_by_key(|h| h.to_base32());

        // ── Cost ────────────────────────────────────────────────────────────
        // A change present in several sources is attributed to exactly one so
        // the per-view figures partition the union instead of double-counting.
        let owner: HashMap<Hash, String> = {
            let mut names: Vec<&String> = sources.iter().collect();
            names.sort();
            let mut map = HashMap::new();
            for name in names {
                for hash in self.hashes_of(&txn, &per_view_visible[name])? {
                    map.entry(hash).or_insert_with(|| name.clone());
                }
            }
            map
        };

        let mut by_view_cost: HashMap<String, Vec<Hash>> = HashMap::new();
        for hash in &candidate {
            if let Some(name) = owner.get(hash) {
                by_view_cost.entry(name.clone()).or_default().push(*hash);
            }
        }

        let cost = self.aggregate_cost(&candidate)?;
        let mut views: Vec<ViewOutcome> = Vec::with_capacity(states.len());
        for state in &states {
            let view_cost =
                self.aggregate_cost(&by_view_cost.get(&state.name).cloned().unwrap_or_default())?;
            let mut files: BTreeSet<String> = BTreeSet::new();
            for hash in by_view_cost.get(&state.name).cloned().unwrap_or_default() {
                for path in self.change_modified_paths(&hash)? {
                    files.insert(path);
                }
            }
            views.push(ViewOutcome {
                view: state.name.clone(),
                scope: if state.kind.is_draft() {
                    "draft".to_string()
                } else {
                    "shared".to_string()
                },
                parent: state
                    .parent
                    .and_then(|pid| self.parent_name(&txn, pid).ok().flatten()),
                changes: by_view_cost.get(&state.name).map_or(0, |v| v.len()),
                files: files.len(),
                cost: view_cost,
            });
        }
        // Deterministic per-view order regardless of input order.
        views.sort_by(|a, b| a.view.cmp(&b.view));

        // ── Footprint ───────────────────────────────────────────────────────
        let mut paths: BTreeSet<String> = BTreeSet::new();
        let mut baggage_files: BTreeSet<String> = BTreeSet::new();
        let mut baggage = 0usize;
        for hash in &candidate {
            let (modifies, coverage) = self.change_coverage(hash)?;
            for file_id in &modifies {
                paths.insert(file_id.trim_start_matches("file:").to_string());
            }
            if matches!(coverage, crate::repository::Coverage::Uncovered) {
                baggage += 1;
                baggage_files.extend(modifies);
            }
        }

        let footprint = OutcomeFootprint {
            changes: candidate.len(),
            files: paths.len(),
            paths: paths.into_iter().collect(),
            baggage,
            baggage_files: baggage_files.into_iter().collect(),
        };

        // ── Mergeability ────────────────────────────────────────────────────
        // The filter must be the state a single promotion would produce: the
        // target's content plus everything the sources would add. Asking the
        // union of {target} ∪ sources for that is exactly right — a source's
        // visible set is its delta plus whatever it already shares with the
        // target, and that shared part is already in the target's own set, so
        // the two formulations are equal.
        let mut all_states = vec![target_view.clone()];
        all_states.extend(states.iter().cloned());
        let merge_filter = visible_change_ids_for_views(&txn, &all_states)?;
        let mergeability = self.probe_mergeability(&txn, merge_filter, &owner)?;

        // ── Pin and reference ───────────────────────────────────────────────
        let mut source_merkle = BTreeMap::new();
        for state in &states {
            source_merkle.insert(state.name.clone(), state.state.to_base32());
        }
        let inputs = OutcomeInputs {
            target_merkle: target_view.state.to_base32(),
            source_merkle,
            candidate_changes: candidate.iter().map(|h| h.to_base32()).collect(),
        };
        let reference = outcome_reference(&OutcomePins {
            sources: sources.to_vec(),
            target: target.to_string(),
            source_merkle: inputs.source_merkle.clone(),
            target_merkle: inputs.target_merkle.clone(),
            candidate_changes: inputs.candidate_changes.clone(),
        });

        Ok(Outcome {
            sources: sources.to_vec(),
            target: target.to_string(),
            inputs,
            reference,
            cost,
            footprint,
            mergeability,
            views,
        })
    }

    /// Fold per-change provenance over `hashes` into a cost rollup.
    ///
    /// Best-effort: an unreadable change increments `unreadable_changes` and is
    /// excluded from every other field, so a single corrupt change cannot
    /// silently understate the total.
    fn aggregate_cost(&self, hashes: &[Hash]) -> Result<OutcomeCost, RepositoryError> {
        let mut cost = OutcomeCost {
            cost_known: false,
            ..Default::default()
        };
        let mut models: BTreeMap<String, ModelSpend> = BTreeMap::new();

        for hash in hashes {
            let change = match self.load_change(hash) {
                Ok(change) => change,
                Err(_) => {
                    cost.unreadable_changes += 1;
                    continue;
                }
            };
            let provenance = change.provenance();
            if provenance.is_empty() {
                cost.unattributed_changes += 1;
                continue;
            }
            cost.attributed_changes += 1;

            let mut seen_in_change: BTreeSet<&str> = BTreeSet::new();
            for prov in provenance {
                let entry = models
                    .entry(prov.model.clone())
                    .or_insert_with(|| ModelSpend {
                        model: prov.model.clone(),
                        ..Default::default()
                    });
                if seen_in_change.insert(prov.model.as_str()) {
                    entry.changes += 1;
                }
                entry.input_tokens += prov.tokens.input_tokens;
                entry.output_tokens += prov.tokens.output_tokens;
                entry.cache_read_tokens += prov.tokens.cache_read_tokens;
                entry.cache_write_tokens += prov.tokens.cache_write_tokens;
                entry.reasoning_tokens += prov.tokens.reasoning_tokens;
                entry.micro_usd += prov.cost.micro_usd;
            }
        }

        for entry in models.values_mut() {
            entry.total_tokens = entry.input_tokens + entry.output_tokens;
            entry.usd = entry.micro_usd as f64 / 1_000_000.0;

            cost.input_tokens += entry.input_tokens;
            cost.output_tokens += entry.output_tokens;
            cost.cache_read_tokens += entry.cache_read_tokens;
            cost.cache_write_tokens += entry.cache_write_tokens;
            cost.reasoning_tokens += entry.reasoning_tokens;
            cost.total_tokens += entry.total_tokens;
            cost.micro_usd += entry.micro_usd;
        }
        cost.usd = cost.micro_usd as f64 / 1_000_000.0;
        cost.by_model = models.into_values().collect();
        cost.cost_known = cost.attributed_changes > 0;
        Ok(cost)
    }

    /// Materialize the union into a discard-everything working copy and read
    /// back the residual graph conflicts.
    ///
    /// This is **read-only**: the [`Sink`] working copy writes nowhere, so
    /// probing leaves the repository byte-for-byte unchanged. It answers
    /// *graph* conflicts (name / order / cyclic / zombie) — not insert-time
    /// edge-context failures, which can only be observed by actually inserting.
    fn probe_mergeability(
        &self,
        txn: &atomic_core::pristine::ReadTxn,
        filter: HashSet<NodeId>,
        owner: &HashMap<Hash, String>,
    ) -> Result<Mergeability, RepositoryError> {
        let cached =
            CachedGraphTxn::new(txn).map_err(|e| RepositoryError::Database(e.to_string()))?;
        let sink = atomic_core::output::Sink::new();
        let options = MaterializeOptions::new().with_change_filter(filter);

        let result = materialize_view(&cached, &self.change_store, &sink, options)
            .map_err(|e| RepositoryError::Output(format!("{e}")))?;

        // Attribute each conflict to the sources that produced its changes, so
        // we can tell a two-way overlap (adjudicable) from a three-way one
        // (beyond the merge engine's reach). `owner` is already the partition
        // built for the cost rollup, so no second attribution pass is needed.
        let mut conflicts: Vec<OutcomeConflict> = result
            .conflicts
            .iter()
            .map(|c| {
                let mut views: BTreeSet<String> = BTreeSet::new();
                for hash in &c.changes {
                    if let Some(view) = owner.get(hash) {
                        views.insert(view.clone());
                    }
                }
                let concurrent = views.len();
                OutcomeConflict {
                    path: c.path.clone(),
                    kind: conflict_kind(c.conflict_type).to_string(),
                    views: views.into_iter().collect(),
                    concurrent_sources: concurrent,
                }
            })
            .collect();
        conflicts.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.kind.cmp(&b.kind)));
        Ok(classify_conflicts(conflicts))
    }

    /// External hashes for a set of internal change ids, sorted.
    fn hashes_of(
        &self,
        txn: &impl GraphTxnT,
        ids: &HashSet<NodeId>,
    ) -> Result<Vec<Hash>, RepositoryError> {
        let mut hashes = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(hash) = txn
                .get_external(*id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                hashes.push(hash);
            }
        }
        hashes.sort_by_key(|h| h.to_base32());
        Ok(hashes)
    }

    /// Resolve a parent view id to its name, if it still exists.
    fn parent_name(
        &self,
        txn: &impl ViewTxnT,
        parent_id: u64,
    ) -> Result<Option<String>, RepositoryError> {
        Ok(txn
            .get_view_by_id(parent_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .map(|p| p.name))
    }
}

/// Why a three-or-more-way overlap cannot be adjudicated automatically.
const NOT_COMPUTABLE_REASON: &str = "at least one conflict spans 3+ source views; the semantic \
                                    merge engine adjudicates two sides at a time, so this set \
                                    cannot be verified automatically";

/// Decide the mergeability verdict from a set of attributed conflicts.
///
/// Split out from [`Repository::probe_mergeability`] so the tiering — the part
/// with real logic, and the part that must not lie — is testable without having
/// to hand-construct a graph cycle. That is worth doing deliberately: the merge
/// engine resolves the large majority of overlaps, so an integration test can
/// realistically only ever observe the `Clean` branch.
fn classify_conflicts(mut conflicts: Vec<OutcomeConflict>) -> Mergeability {
    conflicts.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));

    if conflicts.is_empty() {
        return Mergeability::Clean;
    }

    // A conflict touching three or more sources is past the two-way merge
    // engine's reach, so we can say neither that it merges nor that it does
    // not. Reporting it as `NeedsHuman` would be indistinguishable from a
    // real pass, which is exactly the unfalsifiable claim this exists to stop.
    if conflicts.iter().any(|c| c.concurrent_sources > 2) {
        return Mergeability::NotComputable {
            reason: NOT_COMPUTABLE_REASON.to_string(),
            conflicts,
        };
    }

    Mergeability::NeedsHuman { conflicts }
}

/// Stable lowercase name for a graph conflict kind.
fn conflict_kind(kind: FileConflictType) -> &'static str {
    match kind {
        FileConflictType::Name => "name",
        FileConflictType::Order => "order",
        FileConflictType::Cyclic => "cyclic",
        FileConflictType::Zombie => "zombie",
        FileConflictType::ZombieFile => "zombie_file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::apply::CrossViewInsertOptions;
    use crate::record::RecordOptions;
    use crate::tracking::TrackingOptions;
    use atomic_core::change::ChangeHeader;
    use tempfile::TempDir;

    /// Record every tracked file into a change and apply it.
    fn record_all(repo: &Repository, message: &str) -> Hash {
        let header = ChangeHeader::new(message);
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
            .enrich_kg(false);
        *repo.record(header, options).unwrap().hash()
    }

    /// A repo with root view `dev` and one draft `feature-a` parented on it.
    fn repo_with_drafts(names: &[&str]) -> (Repository, TempDir) {
        let temp = TempDir::new().unwrap();
        let mut repo = Repository::init(temp.path()).unwrap();
        for name in names {
            repo.create_draft_view(name, "dev").unwrap();
        }
        (repo, temp)
    }

    #[test]
    fn empty_union_of_an_unchanged_draft_is_clean_and_free() {
        let (repo, _temp) = repo_with_drafts(&["feature-a"]);
        let outcome = repo.outcome(&["feature-a".to_string()], "dev").unwrap();

        assert_eq!(outcome.footprint.changes, 0);
        assert_eq!(outcome.footprint.files, 0);
        assert!(outcome.mergeability.is_clean());
        // Nothing was recorded, so cost is *unknown*, not zero-cost work.
        assert!(!outcome.cost.cost_known);
        assert_eq!(outcome.cost.attributed_changes, 0);
        assert!(outcome.reference.starts_with("urn:atomic:outcome:"));
    }

    #[test]
    fn outcome_rolls_up_cost_and_footprint_of_a_single_draft() {
        let (mut repo, temp) = repo_with_drafts(&["feature-a"]);
        repo.set_current_view_in_memory("feature-a");

        let path = temp.path().join("src/lib.rs");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "fn main() {}\n").unwrap();
        repo.add("src/lib.rs", TrackingOptions::default()).unwrap();
        let hash = record_all(&repo, "add lib");

        let outcome = repo.outcome(&["feature-a".to_string()], "dev").unwrap();
        assert_eq!(outcome.footprint.changes, 1);
        assert_eq!(outcome.footprint.paths, vec!["src/lib.rs".to_string()]);
        assert!(outcome.mergeability.is_clean());

        // The per-view breakdown partitions the union: exactly one view, one
        // change, and it matches the total.
        assert_eq!(outcome.views.len(), 1);
        assert_eq!(outcome.views[0].view, "feature-a");
        assert_eq!(outcome.views[0].scope, "draft");
        assert_eq!(outcome.views[0].parent.as_deref(), Some("dev"));
        assert_eq!(outcome.views[0].changes, 1);
        assert_eq!(outcome.views[0].files, 1);
        // `record_all` records no provenance, so this change is unattributed
        // and the report says the cost is *unknown* rather than printing $0.00.
        assert_eq!(outcome.views[0].cost.attributed_changes, 0);
        assert_eq!(outcome.views[0].cost.unattributed_changes, 1);
        assert!(!outcome.views[0].cost.cost_known);
        assert!(!outcome.cost.cost_known);
        assert_eq!(
            outcome.views[0].cost.micro_usd, outcome.cost.micro_usd,
            "single-source totals must match the union total"
        );
        assert!(outcome.inputs.candidate_changes.contains(&hash.to_base32()));
    }

    #[test]
    fn union_across_two_drafts_counts_each_change_once() {
        let (mut repo, temp) = repo_with_drafts(&["feature-a", "feature-b"]);

        // One change in each draft, touching a different file.
        repo.set_current_view_in_memory("feature-a");
        let a = temp.path().join("a.txt");
        std::fs::write(&a, "a\n").unwrap();
        repo.add("a.txt", TrackingOptions::default()).unwrap();
        record_all(&repo, "add a");

        repo.set_current_view_in_memory("feature-b");
        let b = temp.path().join("b.txt");
        std::fs::write(&b, "b\n").unwrap();
        repo.add("b.txt", TrackingOptions::default()).unwrap();
        record_all(&repo, "add b");

        let outcome = repo
            .outcome(&["feature-a".to_string(), "feature-b".to_string()], "dev")
            .unwrap();

        assert_eq!(outcome.footprint.changes, 2);
        assert_eq!(outcome.footprint.paths, vec!["a.txt", "b.txt"]);
        assert!(outcome.mergeability.is_clean());

        // The per-view costs partition the union, so they sum to the total.
        let per_view: u64 = outcome.views.iter().map(|v| v.cost.micro_usd).sum();
        assert_eq!(per_view, outcome.cost.micro_usd);
        assert_eq!(
            outcome.views.iter().map(|v| v.changes).sum::<usize>(),
            outcome.footprint.changes,
            "a change shared by two sources must be attributed to exactly one"
        );
    }

    #[test]
    fn source_order_does_not_change_the_reference() {
        let (mut repo, temp) = repo_with_drafts(&["feature-a", "feature-b"]);
        repo.set_current_view_in_memory("feature-a");
        std::fs::write(temp.path().join("a.txt"), "a\n").unwrap();
        repo.add("a.txt", TrackingOptions::default()).unwrap();
        record_all(&repo, "add a");

        let forward = repo
            .outcome(&["feature-a".to_string(), "feature-b".to_string()], "dev")
            .unwrap();
        let reverse = repo
            .outcome(&["feature-b".to_string(), "feature-a".to_string()], "dev")
            .unwrap();

        assert_eq!(forward.reference, reverse.reference);
        // Per-view output is sorted regardless of the order asked for.
        let names: Vec<&str> = reverse.views.iter().map(|v| v.view.as_str()).collect();
        assert_eq!(names, vec!["feature-a", "feature-b"]);
    }

    #[test]
    fn already_promoted_work_contributes_nothing() {
        let (mut repo, temp) = repo_with_drafts(&["feature-a"]);
        repo.set_current_view_in_memory("feature-a");
        std::fs::write(temp.path().join("a.txt"), "a\n").unwrap();
        repo.add("a.txt", TrackingOptions::default()).unwrap();
        record_all(&repo, "add a");

        // The change is in `dev` already, so a->dev is an empty delta.
        repo.set_current_view_in_memory("dev");
        repo.insert_from_view(CrossViewInsertOptions::new("feature-a", "dev"))
            .unwrap();

        let outcome = repo.outcome(&["feature-a".to_string()], "dev").unwrap();
        assert_eq!(outcome.footprint.changes, 0);
        assert!(outcome.mergeability.is_clean());
    }

    /// The mergeability probe asks for `visible_change_ids_for_views(&[target]
    /// ++ sources)` instead of `target_visible ∪ union`. Pin that the two are
    /// the same set, because the refactor is only valid if they are.
    #[test]
    fn union_of_target_and_sources_equals_target_visible_plus_deltas() {
        let (mut repo, temp) = repo_with_drafts(&["feature-a", "feature-b"]);

        repo.set_current_view_in_memory("feature-a");
        std::fs::write(temp.path().join("a.txt"), "a\n").unwrap();
        repo.add("a.txt", TrackingOptions::default()).unwrap();
        record_all(&repo, "add a");

        repo.set_current_view_in_memory("feature-b");
        std::fs::write(temp.path().join("b.txt"), "b\n").unwrap();
        repo.add("b.txt", TrackingOptions::default()).unwrap();
        record_all(&repo, "add b");

        let txn = repo.pristine.read_txn().expect("read txn");
        let dev = txn.get_view("dev").unwrap().unwrap();
        let a = txn.get_view("feature-a").unwrap().unwrap();
        let b = txn.get_view("feature-b").unwrap().unwrap();

        // The helper's answer.
        let via_helper = visible_change_ids_for_views(&txn, &[dev.clone(), a.clone(), b.clone()])
            .expect("union");

        // The hand-built equivalent it replaced.
        let target_visible = collect_visible_change_ids_with_deps(&txn, &dev).unwrap();
        let mut hand_built = target_visible.clone();
        for view in [&a, &b] {
            for id in collect_visible_change_ids_with_deps(&txn, view).unwrap() {
                if !target_visible.contains(&id) {
                    hand_built.insert(id);
                }
            }
        }

        assert_eq!(via_helper, hand_built);
        // Not vacuous: the set really does carry each draft's own change on
        // top of the shared base, so the two deltas are non-empty.
        let target_only = collect_visible_change_ids_with_deps(&txn, &dev).unwrap();
        assert!(via_helper.len() > target_only.len());
    }

    #[test]
    fn unknown_source_view_is_reported_as_view_not_found() {
        let (repo, _temp) = repo_with_drafts(&["feature-a"]);
        let err = repo
            .outcome(&["feature-a".to_string(), "nope".to_string()], "dev")
            .unwrap_err();
        assert!(
            matches!(err, RepositoryError::ViewNotFound { .. }),
            "unexpected: {err}"
        );
    }

    #[test]
    fn empty_source_list_is_rejected() {
        let (repo, _temp) = repo_with_drafts(&["feature-a"]);
        let err = repo.outcome(&[], "dev").unwrap_err();
        assert!(
            matches!(err, RepositoryError::InvalidOperation { .. }),
            "unexpected: {err}"
        );
    }
}

#[cfg(test)]
mod conflict_tier_tests {
    use super::*;

    fn conflict(path: &str, concurrent: usize) -> OutcomeConflict {
        OutcomeConflict {
            path: path.to_string(),
            kind: "order".to_string(),
            views: (0..concurrent).map(|i| format!("view-{i}")).collect(),
            concurrent_sources: concurrent,
        }
    }

    #[test]
    fn no_conflicts_is_clean() {
        assert!(matches!(
            classify_conflicts(Vec::new()),
            Mergeability::Clean
        ));
    }

    #[test]
    fn one_or_two_way_overlap_is_a_human_problem() {
        for concurrent in 1..=2 {
            let verdict = classify_conflicts(vec![conflict("src/a.ts", concurrent)]);
            match verdict {
                Mergeability::NeedsHuman { conflicts } => {
                    assert_eq!(conflicts.len(), 1);
                    assert_eq!(conflicts[0].concurrent_sources, concurrent);
                }
                other => panic!("expected NeedsHuman, got {other:?}"),
            }
        }
    }

    #[test]
    fn three_way_overlap_is_not_computable_not_a_pass() {
        // This is the load-bearing honesty test: a conflict the engine cannot
        // adjudicate must never be reported as merely "needs a human".
        let verdict = classify_conflicts(vec![conflict("src/a.ts", 3)]);
        match verdict {
            Mergeability::NotComputable { reason, conflicts } => {
                assert!(reason.contains("3+"), "unhelpful reason: {reason}");
                assert_eq!(conflicts.len(), 1);
            }
            other => panic!(
                "a 3-way conflict must be NotComputable, got {other:?} — reporting it as a \
                 plain human problem would be indistinguishable from a pass"
            ),
        }
    }

    #[test]
    fn one_unadjudicable_conflict_downgrades_the_whole_verdict() {
        // A single unadjudicable conflict makes the *set* unverifiable, even
        // when every other conflict is a clean two-way overlap.
        let verdict = classify_conflicts(vec![conflict("src/a.ts", 2), conflict("src/b.ts", 4)]);
        assert!(matches!(verdict, Mergeability::NotComputable { .. }));
    }

    #[test]
    fn conflicts_are_ordered_deterministically() {
        let verdict = classify_conflicts(vec![conflict("src/z.ts", 2), conflict("src/a.ts", 2)]);
        match verdict {
            Mergeability::NeedsHuman { conflicts } => {
                assert_eq!(conflicts[0].path, "src/a.ts");
                assert_eq!(conflicts[1].path, "src/z.ts");
            }
            other => panic!("expected NeedsHuman, got {other:?}"),
        }
    }
}
