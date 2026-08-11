//! Triage candidate-set primitive (milestone T0).
//!
//! The candidate set is the foundation every other triage stage consumes.
//! Given a `feature` view and a `target` view, it answers three questions:
//!
//! 1. **`only_in_feature`** — which change hashes are visible to `feature`
//!    but not to `target` (the raw change-level diff).
//! 2. **`closure_additions`** — which additional changes would be dragged in
//!    by the transitive dependency closure of those changes (changes not
//!    already visible to `target` and not themselves in `only_in_feature`).
//! 3. **`baggage`** — which closure additions are *not* covered by any intent
//!    (i.e. their modified files are not touched by a task in the knowledge
//!    graph).
//!
//! A change's modified files are read from the change's own `file_ops`
//! (authoritative and always present), so change→file coverage no longer
//! depends on running `atomic vault query enrich`. Only the *intent* side lives
//! in the KG: a task's `TOUCHES` edge (projected on `vault sync`). This is a
//! pure, read-only projection over `diff_views`, the pristine dependency index,
//! the change store, and the vault knowledge graph. It creates no records.

use super::*;

use atomic_core::pristine::ontology::edge_kind;
use serde::Serialize;

/// Whether a closure addition is covered by an intent in the knowledge graph.
///
/// Serialized as a lowercase string (`"covered"`, `"uncovered"`, `"unknown"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Coverage {
    /// At least one modified file is touched by a task node.
    Covered,
    /// The change modifies files, but none are touched by any task node.
    Uncovered,
    /// The change has no file ops at all (e.g. it could not be loaded).
    /// Coverage is genuinely indeterminate — *not* the same as being uncovered.
    /// In normal use this is unreachable: every recorded change has file ops.
    Unknown,
}

/// A closure addition flagged as baggage (uncovered or coverage-unknown).
#[derive(Debug, Clone, Serialize)]
pub struct BaggageEntry {
    /// The change hash (base32).
    pub change: String,
    /// The `file:` node ids this change modifies (from the change's `file_ops`).
    pub modifies: Vec<String>,
    /// Why this change is baggage.
    pub coverage: Coverage,
}

/// The triage candidate set for a `feature` view relative to a `target` view.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateSet {
    /// The feature (source) view name.
    pub feature: String,
    /// The target view name.
    pub target: String,
    /// Change hashes (base32) visible to `feature` but not `target`.
    pub only_in_feature: Vec<String>,
    /// Change hashes (base32) pulled in by the dependency closure that are not
    /// already in `only_in_feature` and not already visible to `target`.
    pub closure_additions: Vec<String>,
    /// Per-change coverage detail for the closure additions that are baggage.
    pub baggage: Vec<BaggageEntry>,
}

impl Repository {
    /// Compute the triage candidate set for `feature` relative to `target`.
    ///
    /// See the module documentation for the semantics of each field.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::ViewNotFound`] if either view does not exist,
    /// or [`RepositoryError::Database`] on any pristine access failure.
    pub fn triage_candidate_set(
        &self,
        feature: &str,
        target: &str,
    ) -> Result<CandidateSet, RepositoryError> {
        // Step 1: the raw change-level diff. `.0` is only-in-feature.
        let (only_in_feature_hashes, _only_in_target, _common) =
            self.diff_views(feature, target)?;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // The target view's full visible set (with dependency closure). Any
        // closure addition already visible to the target is not an "addition".
        let target_view = txn
            .get_view(target)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: target.to_string(),
            })?;
        let target_visible = collect_visible_change_ids_with_deps(&txn, &target_view)?;

        // Seed the closure with the only-in-feature node ids, remembering the
        // seed set so we can subtract it from the additions later.
        let mut feature_node_ids: HashSet<NodeId> = HashSet::new();
        for hash in &only_in_feature_hashes {
            if let Some(id) = txn
                .get_internal(hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                feature_node_ids.insert(id);
            }
        }

        // Step 2: expand the transitive dependency closure in place.
        let mut closure: HashSet<NodeId> = feature_node_ids.clone();
        expand_indexed_dependency_closure(&txn, &mut closure)?;

        // Closure additions = closure minus the seed minus the target's set.
        let mut addition_hashes: Vec<Hash> = Vec::new();
        for id in &closure {
            if feature_node_ids.contains(id) || target_visible.contains(id) {
                continue;
            }
            if let Some(hash) = txn
                .get_external(*id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                addition_hashes.push(hash);
            }
        }

        // Deterministic ordering.
        let mut only_in_feature: Vec<String> = only_in_feature_hashes
            .iter()
            .map(|h| h.to_base32())
            .collect();
        only_in_feature.sort();

        let mut closure_additions: Vec<String> =
            addition_hashes.iter().map(|h| h.to_base32()).collect();
        closure_additions.sort();

        // Step 3: baggage test for each closure addition.
        let mut baggage: Vec<BaggageEntry> = Vec::new();
        for hash in &addition_hashes {
            let (modifies, coverage) = self.change_coverage(hash)?;
            if matches!(coverage, Coverage::Uncovered | Coverage::Unknown) {
                baggage.push(BaggageEntry {
                    change: hash.to_base32(),
                    modifies,
                    coverage,
                });
            }
        }
        baggage.sort_by(|a, b| a.change.cmp(&b.change));

        Ok(CandidateSet {
            feature: feature.to_string(),
            target: target.to_string(),
            only_in_feature,
            closure_additions,
            baggage,
        })
    }

    /// The file paths a change modifies, read directly from the change's own
    /// `file_ops` — authoritative and always present, so it needs no KG
    /// enrichment (`kg_enrich_*` derives its `MODIFIES` edges from exactly this
    /// source). Deduplicated and sorted.
    ///
    /// On a load failure this returns an empty `Vec` (best-effort): a single
    /// unreadable change must never error the whole triage.
    pub fn change_modified_paths(&self, hash: &Hash) -> Result<Vec<String>, RepositoryError> {
        let change = match self.load_change(hash) {
            Ok(change) => change,
            Err(_) => return Ok(Vec::new()),
        };
        let mut paths: Vec<String> = Vec::new();
        for op in change.file_ops() {
            let path = op.path();
            if !path.is_empty() {
                paths.push(path.to_string());
            }
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Determine intent coverage for a single change.
    ///
    /// The change's modified files come from its own `file_ops` (via
    /// [`change_modified_paths`](Self::change_modified_paths)), mapped to
    /// `file:<path>` node ids — so coverage no longer depends on KG `MODIFIES`
    /// enrichment. The intent side stays in the KG: a file is covered when it
    /// carries an incoming `TOUCHES` edge from a `task:` node (projected on
    /// `vault sync`). Returns the `file:` node ids plus a [`Coverage`] verdict:
    ///
    /// - `Covered` if any modified file is touched by a task node.
    /// - `Uncovered` if the change modifies files but none are touched.
    /// - `Unknown` only when the change has no file ops at all (e.g. a load
    ///   failure) — unreachable for a normally recorded change.
    fn change_coverage(&self, hash: &Hash) -> Result<(Vec<String>, Coverage), RepositoryError> {
        // change → file comes from the change's own ops (authoritative), mapped
        // to the shared `file:<path>` node id the intent side also uses.
        let modifies: Vec<String> = self
            .change_modified_paths(hash)?
            .into_iter()
            .map(|p| format!("file:{p}"))
            .collect();

        if modifies.is_empty() {
            // No file ops (or the change could not be loaded): coverage is
            // genuinely indeterminate — not the same as uncovered.
            return Ok((modifies, Coverage::Unknown));
        }

        // A file is "covered" if some task TOUCHES it. Only this intent side is
        // read from the KG — per modified file, via its 1-hop neighborhood
        // (incoming edges included), looking for a `task: --TOUCHES--> file:`.
        let mut covered = false;
        for file_id in &modifies {
            let neighborhood = self.vault_kg_neighbors(file_id, 1)?;
            if neighborhood.edges.iter().any(|e| {
                e.kind == edge_kind::TOUCHES
                    && e.to_id == *file_id
                    && e.from_id.starts_with("task:")
            }) {
                covered = true;
                break;
            }
        }

        let coverage = if covered {
            Coverage::Covered
        } else {
            Coverage::Uncovered
        };
        Ok((modifies, coverage))
    }
}

#[cfg(test)]
mod tests {
    use super::Coverage;
    use crate::record::RecordOptions;
    use crate::tracking::TrackingOptions;
    use crate::Repository;
    use atomic_core::change::ChangeHeader;
    use atomic_core::pristine::ontology::edge_kind;
    use atomic_core::pristine::VaultEntryType;
    use atomic_core::types::Hash;
    use atomic_core::Base32;
    use tempfile::TempDir;

    fn record_all(repo: &Repository, message: &str) -> Hash {
        let header = ChangeHeader::new(message);
        // `enrich_kg(false)` reproduces the opencode workflow: changes are
        // recorded WITHOUT KG enrichment, so no `MODIFIES` edge is ever created.
        // Coverage must still resolve from the change's own `file_ops`.
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
            .enrich_kg(false);
        *repo.record(header, options).unwrap().hash()
    }

    /// The key regression: coverage resolves from the change's own `file_ops`
    /// even when the KG has NO `MODIFIES` edge for the change (i.e. the
    /// opencode workflow recorded it without `atomic vault query enrich`). A
    /// task touching the same file (via `::file-ref`) marks it `Covered`.
    #[test]
    fn coverage_resolves_from_file_ops_without_kg_enrichment() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        repo.init_vault().unwrap();
        repo.init_kg().unwrap();

        // Record a change touching src/foo.rs — file_ops present, NO enrichment.
        let foo = temp.path().join("src/foo.rs");
        std::fs::create_dir_all(foo.parent().unwrap()).unwrap();
        std::fs::write(&foo, "fn main() {}\n").unwrap();
        repo.add("src/foo.rs", TrackingOptions::default()).unwrap();
        let hash = record_all(&repo, "add foo");

        // Precondition: the KG has NO MODIFIES edge for this change (unenriched).
        let node_id = format!("change:{}", &hash.to_base32()[..12]);
        let sub = repo.vault_kg_neighbors(&node_id, 2).unwrap();
        assert!(
            !sub.edges.iter().any(|e| e.kind == edge_kind::MODIFIES),
            "precondition: no MODIFIES edge should exist without enrichment"
        );

        // change_modified_paths reads straight from the change's file_ops.
        assert_eq!(
            repo.change_modified_paths(&hash).unwrap(),
            vec!["src/foo.rs".to_string()]
        );

        // An intent whose task touches src/foo.rs projects a TOUCHES edge on
        // store (via project_intent_semantics) — the only KG-side dependency.
        let fm = r#"{"id":"COV-1","title":"Cover foo","status":"in-progress"}"#;
        let body = "\
:::task{#cov-1-1 status=open}\nWork on foo.\n::file-ref{path=src/foo.rs}\n:::";
        repo.vault_store(
            "intents/cov-1/intent.md",
            VaultEntryType::Intent,
            body.as_bytes().to_vec(),
            fm.to_string(),
        )
        .unwrap();

        // Covered — resolved from file_ops + the task's TOUCHES, no MODIFIES.
        let (modifies, coverage) = repo.change_coverage(&hash).unwrap();
        assert_eq!(modifies, vec!["file:src/foo.rs".to_string()]);
        assert_eq!(coverage, Coverage::Covered);
    }

    /// A change touching a file that no task touches is `Uncovered` (not
    /// `Unknown`): the file ops resolve, but nothing in the KG covers them.
    #[test]
    fn coverage_uncovered_when_no_task_touches_the_file() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        repo.init_vault().unwrap();
        repo.init_kg().unwrap();

        let bar = temp.path().join("src/bar.rs");
        std::fs::create_dir_all(bar.parent().unwrap()).unwrap();
        std::fs::write(&bar, "fn bar() {}\n").unwrap();
        repo.add("src/bar.rs", TrackingOptions::default()).unwrap();
        let hash = record_all(&repo, "add bar");

        // An intent whose task touches a DIFFERENT file.
        let fm = r#"{"id":"COV-2","title":"Cover foo","status":"in-progress"}"#;
        let body = "\
:::task{#cov-2-1 status=open}\nWork on foo.\n::file-ref{path=src/foo.rs}\n:::";
        repo.vault_store(
            "intents/cov-2/intent.md",
            VaultEntryType::Intent,
            body.as_bytes().to_vec(),
            fm.to_string(),
        )
        .unwrap();

        let (modifies, coverage) = repo.change_coverage(&hash).unwrap();
        assert_eq!(modifies, vec!["file:src/bar.rs".to_string()]);
        assert_eq!(coverage, Coverage::Uncovered);
    }

    /// A change recorded only on `feature` shows up in `only_in_feature`, and
    /// not in `closure_additions`, while a change shared with `target` does not.
    #[test]
    fn candidate_set_reports_only_in_feature() {
        let temp = TempDir::new().unwrap();
        let mut repo = Repository::init(temp.path()).unwrap();
        let base_view = repo.current_view().to_string();

        // Base change on the shared/base view.
        let file = temp.path().join("a.txt");
        std::fs::write(&file, "base\n").unwrap();
        repo.add("a.txt", TrackingOptions::default()).unwrap();
        record_all(&repo, "base change");

        // Fork a feature view and record a change only there.
        repo.create_view_from("feature", &base_view).unwrap();
        repo.switch_view("feature").unwrap();
        std::fs::write(&file, "base\nfeature edit\n").unwrap();
        record_all(&repo, "feature change");

        // Feature-only change relative to the base view.
        let set = repo.triage_candidate_set("feature", &base_view).unwrap();

        assert_eq!(set.feature, "feature");
        assert_eq!(set.target, base_view);
        assert_eq!(
            set.only_in_feature.len(),
            1,
            "expected exactly one feature-only change, got {:?}",
            set.only_in_feature
        );

        // The feature-only change must not appear as a closure addition.
        for add in &set.closure_additions {
            assert!(
                !set.only_in_feature.contains(add),
                "closure additions must be disjoint from only_in_feature"
            );
        }
    }

    /// The reverse diff (base has nothing the feature lacks) is empty.
    #[test]
    fn candidate_set_empty_when_no_divergence() {
        let temp = TempDir::new().unwrap();
        let mut repo = Repository::init(temp.path()).unwrap();
        let base_view = repo.current_view().to_string();

        let file = temp.path().join("a.txt");
        std::fs::write(&file, "base\n").unwrap();
        repo.add("a.txt", TrackingOptions::default()).unwrap();
        record_all(&repo, "base change");

        repo.create_view_from("feature", &base_view).unwrap();

        // feature == base (no new changes): nothing only in feature.
        let set = repo.triage_candidate_set("feature", &base_view).unwrap();
        assert!(
            set.only_in_feature.is_empty(),
            "expected no divergence, got {:?}",
            set.only_in_feature
        );
        assert!(set.closure_additions.is_empty());
        assert!(set.baggage.is_empty());
    }
}
