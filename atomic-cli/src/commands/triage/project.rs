//! The triage projection (milestone T3).
//!
//! [`build_report`] walks the change → file → task → intent → acceptance-
//! criterion join over the knowledge graph, gates each reached intent, attaches
//! best-effort provenance, and mints a reproducible `urn:atomic:triage:<hash>`
//! reference over the pinned inputs. It is pure and read-only — it creates no
//! records.
//!
//! ## Join topology (why two KG lookups per change)
//!
//! `kg_neighbors(depth=2)` is a strict breadth-limited BFS: from a
//! `change:<hash>` node it reaches `file:` nodes (hop 1, `MODIFIES`) and the
//! `task:` nodes that `TOUCHES` those files (hop 2), but NOT the task's own
//! outgoing edges (`HAS_TASK` from the parent intent, `SATISFIES` to an AC),
//! which sit at hop 3. So we take a second `kg_neighbors(task, 1)` per touching
//! task to resolve its parent intent and satisfied criteria. This is the one
//! place the implementation diverges from the doc's "depth-2 subgraph" prose.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use atomic_canonical::node::CanonicalNode;
use atomic_canonical::{intent_substance_hash, triage_reference, validate_intent, TriagePins};
use atomic_core::pristine::ontology::edge_kind;
use atomic_core::types::{Base32, Hash};
use atomic_repository::Repository;

use atomic_core::change::Change;

use crate::commands::change::{hunk_display_summaries, HunkDisplaySummary};
use crate::commands::diff::{change_file_diffs, DiffFormat, DiffOutputConfig, FileChangeStatus};
use crate::commands::intent::bridge;
use crate::commands::provenance::command::load_graphs;
use crate::error::CliError;

use super::model::*;

/// Case-insensitive edge-kind comparison, mirroring `QueryCallers`.
fn kind_is(kind: &str, expected: &str) -> bool {
    kind.eq_ignore_ascii_case(expected)
}

/// First 12 base32 chars — the short id used for `change:<short>` KG nodes.
fn short12(hash_b32: &str) -> String {
    hash_b32.chars().take(12).collect()
}

/// Map a canonical `urn:atomic:<kind>:<local>` id to its KG child id
/// (`<kind>:<LOCAL>`), matching `vault_triples::child_kg_id`.
fn child_kg_id(urn: &str) -> String {
    match urn.strip_prefix("urn:atomic:") {
        Some(rest) => match rest.split_once(':') {
            Some((kind, local)) => format!("{kind}:{}", local.to_uppercase()),
            None => rest.to_uppercase(),
        },
        None => urn.to_string(),
    }
}

/// If any acceptance criterion on `node` carries a view-scoped verification
/// whose LATEST view-scoped record failed, return that record's `kind` (for the
/// finding message). `None` means the view baseline is not known to be red.
fn failing_view_verification(node: &CanonicalNode) -> Option<String> {
    for ac in &node.has_acceptance_criterion {
        // The latest view-scoped record is the last one in append order.
        if let Some(rec) = ac
            .verifications
            .iter()
            .rev()
            .find(|v| v.scope.eq_ignore_ascii_case("view"))
        {
            if rec.outcome.eq_ignore_ascii_case("fail") {
                return Some(rec.kind.clone());
            }
        }
    }
    None
}

/// Build the canonical triage report for `feature` relative to `target`.
pub fn build_report(
    repo: &Repository,
    feature: &str,
    target: &str,
) -> Result<TriageReport, CliError> {
    // 1. The candidate set (T0): only-in-feature, closure additions, baggage.
    let set = repo
        .triage_candidate_set(feature, target)
        .map_err(CliError::Repository)?;

    // 2. The pinned view Merkle (the materialized state the report is about).
    let view_merkle = repo
        .get_view_info(feature)
        .map_err(CliError::Repository)?
        .state_base32();

    // Promotion scope: an unreviewed change BLOCKS promotion into a shared
    // view, but is only a warning into a draft. Resolved once here.
    let target_is_shared = repo
        .get_view_info(target)
        .map_err(CliError::Repository)?
        .scope
        .is_shared();

    // 3. The change → intent join.
    //
    // The change's modified files come from the change itself
    // (`change_modified_paths`) — authoritative and present WITHOUT any KG
    // enrichment. Only the intent side of the join (TOUCHES/HAS_TASK/SATISFIES,
    // projected on `vault sync`) is read from the KG, so a change is orphaned
    // iff no intent's task touches one of its files — never merely because
    // `atomic vault query enrich` has not run.
    //
    // intent_to_changes: KG intent id  → candidate hashes that reach it.
    // ac_to_changes:     KG ac id       → candidate hashes that satisfy it.
    let mut change_reports: Vec<ChangeReport> = Vec::new();
    let mut intent_to_changes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut ac_to_changes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut reached_any_intent: HashMap<String, bool> = HashMap::new();
    let mut all_modified_files: HashSet<String> = HashSet::new();
    // Raw modified paths per candidate hash (for the actionable orphan message).
    let mut change_raw_paths: HashMap<String, Vec<String>> = HashMap::new();
    // Entities defined in each candidate's modified files (parallel to
    // `change_reports`) — the seeds for the blast-radius walk below.
    let mut change_entities: Vec<Vec<String>> = Vec::new();

    for full in &set.only_in_feature {
        let hash = Hash::from_base32(full.as_bytes());

        // Authoritative modified paths straight from the change's file_ops —
        // no KG `MODIFIES` edge (hence no `atomic vault query enrich`) required.
        let paths: Vec<String> = hash
            .map(|h| repo.change_modified_paths(&h).unwrap_or_default())
            .unwrap_or_default();
        change_raw_paths.insert(full.clone(), paths.clone());

        // Review context straight from the loaded change (best-effort): commit
        // message, per-file hunk summaries (the `atomic change` view), the REAL
        // per-file unified diff (actual code hunks), and the inspect command.
        let (message, files, diff) =
            match hash.and_then(|h| repo.load_change(&h).ok().map(|c| (h, c))) {
                Some((h, change)) => {
                    let message = change.hashed.header.message.trim().to_string();
                    let files: Vec<FileChange> = hunk_display_summaries(&change.hashed.hunks)
                        .into_iter()
                        .map(|hs: HunkDisplaySummary| FileChange {
                            symbol: hs.symbol.to_string(),
                            path: hs.path,
                            summary: hs.info,
                        })
                        .collect();
                    let diff = build_change_diff(repo, &change, &h);
                    (message, files, diff)
                }
                None => (String::new(), Vec::new(), Vec::new()),
            };
        let review_command = format!("atomic change {full} --show-hunks");

        // The shared `file:<path>` node ids that MODIFIES/TOUCHES/DEFINES/
        // SCOPE_OUT_FILE all meet on.
        let mut modifies: Vec<String> = paths.iter().map(|p| format!("file:{p}")).collect();
        modifies.sort();
        modifies.dedup();
        for f in &modifies {
            all_modified_files.insert(f.clone());
        }

        // Per modified file, read its KG neighborhood: incoming TOUCHES (from
        // tasks) drives coverage + the intent join; outgoing DEFINES (to
        // entities) seeds the blast walk. Both sit at depth 1 from the file
        // node. Absent enrichment these are simply empty (best-effort).
        let mut touching_tasks: BTreeSet<String> = BTreeSet::new();
        let mut entities: Vec<String> = Vec::new();
        let mut covered = false;
        for file_id in &modifies {
            let Ok(fsub) = repo.vault_kg_neighbors(file_id, 1) else {
                continue;
            };
            for e in &fsub.edges {
                if e.to_id == *file_id && kind_is(&e.kind, edge_kind::TOUCHES) {
                    covered = true;
                    touching_tasks.insert(e.from_id.clone());
                }
                if e.from_id == *file_id && kind_is(&e.kind, edge_kind::DEFINES) {
                    entities.push(e.to_id.clone());
                }
            }
        }

        // Coverage: no files → unknown; else covered iff some file is touched.
        let coverage = if modifies.is_empty() {
            "unknown"
        } else if covered {
            "covered"
        } else {
            "uncovered"
        }
        .to_string();

        // Second hop: each task's parent intent (HAS_TASK) and satisfied ACs.
        let mut reached = false;
        for task in &touching_tasks {
            let tsub = repo
                .vault_kg_neighbors(task, 1)
                .map_err(CliError::Repository)?;
            for e in &tsub.edges {
                if e.to_id == *task && kind_is(&e.kind, edge_kind::HAS_TASK) {
                    reached = true;
                    intent_to_changes
                        .entry(e.from_id.clone())
                        .or_default()
                        .insert(full.clone());
                }
                if e.from_id == *task && kind_is(&e.kind, edge_kind::SATISFIES) {
                    ac_to_changes
                        .entry(e.to_id.clone())
                        .or_default()
                        .insert(full.clone());
                }
            }
        }
        reached_any_intent.insert(full.clone(), reached);

        entities.sort();
        entities.dedup();
        change_entities.push(entities);

        change_reports.push(ChangeReport {
            id: full.clone(),
            message,
            modifies,
            coverage,
            files,
            diff,
            review_command,
            blast_radius: Vec::new(),
            provenance: load_provenance_compact(repo, full),
        });
    }

    // 4. Gate each reached intent and build its report + substance hash.
    let mut findings: Vec<Finding> = Vec::new();
    let mut intent_reports: Vec<IntentReport> = Vec::new();
    let mut intent_substance_hashes: BTreeMap<String, String> = BTreeMap::new();

    for (intent_kg_id, _changes) in &intent_to_changes {
        let bare = intent_kg_id.strip_prefix("intent:").unwrap_or(intent_kg_id);
        match load_intent_node(repo, bare) {
            Ok(node) => {
                let report = validate_intent(&node);
                let current_substance = intent_substance_hash(&node);
                intent_substance_hashes.insert(intent_kg_id.clone(), current_substance.clone());

                // STALE_TRIAGE (warn, T5b): a granted `done` whose reviewable
                // substance drifted from the pin recorded at grant time. The
                // intent still says done, but its definition moved afterward, so
                // the standing grant is no longer fresh — re-triage is required.
                // The pin lives in the intent's frontmatter (not the lifted
                // node), so read it via the bridge.
                if node.status == "done" {
                    if let Some(pin) = load_intent_done_pin(repo, bare) {
                        if pin != current_substance {
                            findings.push(
                                Finding::new(
                                    F_STALE_TRIAGE,
                                    SEV_WARN,
                                    intent_kg_id.clone(),
                                    format!(
                                        "intent {intent_kg_id} is done but its substance drifted \
                                         from the triage pin ({pin} → {current_substance})"
                                    ),
                                )
                                .with_remedy(
                                    "clear it by re-granting done ('atomic intent update <id> \
                                     --status done') to re-pin at the current substance, or revert \
                                     the definition change. Adding a verification record does NOT \
                                     trigger this — only changing an acceptance criterion's text or \
                                     requiredKinds does.",
                                ),
                            );
                        }
                    }
                }

                // OPEN_REMEDIATION (info, T6b): a promoted intent A that a
                // still-in-flight intent B `REMEDIATES`. `kg_neighbors` returns
                // incoming + outgoing edges, so the B --REMEDIATES--> A edge
                // shows up here as an incoming edge whose `to_id` is A. Surfaced,
                // never blocking.
                if let Ok(sub) = repo.vault_kg_neighbors(intent_kg_id, 1) {
                    for e in &sub.edges {
                        if e.to_id != *intent_kg_id || !kind_is(&e.kind, edge_kind::REMEDIATES) {
                            continue;
                        }
                        let b_id = &e.from_id;
                        let b_bare = b_id.strip_prefix("intent:").unwrap_or(b_id);
                        // In flight = B is not `done`. If B loads and is done,
                        // the remediation is resolved → skip. If B fails to load
                        // (or is any non-done status), treat it as in flight.
                        let b_done = load_intent_node(repo, b_bare)
                            .map(|n| n.status == "done")
                            .unwrap_or(false);
                        if b_done {
                            continue;
                        }
                        findings.push(
                            Finding::new(
                                F_OPEN_REMEDIATION,
                                SEV_INFO,
                                intent_kg_id.clone(),
                                format!(
                                    "promoted intent {intent_kg_id} has an open remediation \
                                     {b_id} in flight"
                                ),
                            )
                            .with_query(format!("atomic vault query neighbors {b_id} -d 1 --json"))
                            .with_remedy(
                                "track the remediation to completion; promotion is not blocked",
                            ),
                        );
                    }
                }

                // UNREVIEWED_CHANGE (block into shared / warn into draft): a
                // reached WORK intent (kind != "review") must be covered by an
                // independent, completed review intent — one with a
                // `REVIEWS`-edge to it, status `done`, and a DIFFERENT
                // `attributedTo` than the work intent's author. Reviews of
                // reviews are not required.
                let mut reviewed_by: Option<String> = None;
                if node.kind != "review" {
                    let work_author = node.attributed_to.clone().unwrap_or_default();
                    let mut review_exists = false;
                    if let Ok(sub) = repo.vault_kg_neighbors(intent_kg_id, 1) {
                        for e in &sub.edges {
                            // review --REVIEWS--> work (incoming to the work intent).
                            if e.to_id != *intent_kg_id || !kind_is(&e.kind, edge_kind::REVIEWS) {
                                continue;
                            }
                            review_exists = true;
                            let r_bare = e.from_id.strip_prefix("intent:").unwrap_or(&e.from_id);
                            if let Ok(rnode) = load_intent_node(repo, r_bare) {
                                let review_author = rnode.attributed_to.clone().unwrap_or_default();
                                let independent = !work_author.is_empty()
                                    && !review_author.is_empty()
                                    && review_author != work_author;
                                if rnode.status == "done" && independent {
                                    reviewed_by = Some(review_author);
                                    break;
                                }
                            }
                        }
                    }

                    if reviewed_by.is_none() {
                        let severity = if target_is_shared {
                            SEV_BLOCK
                        } else {
                            SEV_WARN
                        };
                        let message = if review_exists {
                            format!(
                                "{intent_kg_id}'s changes have a review, but it is self-authored \
                                 or not yet done — an independent, completed review is required"
                            )
                        } else {
                            format!(
                                "{intent_kg_id}'s changes are not covered by an independent, \
                                 completed review"
                            )
                        };
                        findings.push(
                            Finding::new(
                                F_UNREVIEWED_CHANGE,
                                severity,
                                intent_kg_id.clone(),
                                message,
                            )
                            .with_query(format!(
                                "atomic intent new --review {bare} --reviews {bare}"
                            ))
                            .with_remedy(
                                "have a different identity/model author and attest a review intent \
                                 that `reviews` this intent",
                            ),
                        );
                    }
                }

                // GATE_VIOLATION (block): one per violation.
                if !report.conforms {
                    for v in &report.results {
                        findings.push(
                            Finding::new(
                                F_GATE_VIOLATION,
                                SEV_BLOCK,
                                intent_kg_id.clone(),
                                v.message.clone(),
                            )
                            .with_query(format!("atomic intent validate {bare} --json"))
                            .with_remedy(
                                "fix the intent so it conforms to the gate, then re-attest",
                            ),
                        );
                    }
                }

                let mut criteria = Vec::new();
                for ac in &node.has_acceptance_criterion {
                    let ac_kg = child_kg_id(&ac.id);
                    let satisfied_by: Vec<String> = ac_to_changes
                        .get(&ac_kg)
                        .map(|s| s.iter().cloned().collect())
                        .unwrap_or_default();
                    let is_met = ac.ac_status == "met";
                    let judgment_required = !satisfied_by.is_empty() && !is_met;

                    // MET_AC_NO_EVIDENCE (block): met but no verifiedBy /
                    // evidence / verification records.
                    let no_verified = ac.verified_by.as_deref().unwrap_or("").is_empty();
                    let no_evidence = ac.evidence.as_deref().unwrap_or("").is_empty();
                    if is_met && no_verified && no_evidence && ac.verifications.is_empty() {
                        findings.push(
                            Finding::new(
                                F_MET_AC_NO_EVIDENCE,
                                SEV_BLOCK,
                                ac.id.clone(),
                                format!(
                                    "acceptance criterion {} is marked met but carries no \
                                     verifiedBy, evidence, or verification record",
                                    ac.id
                                ),
                            )
                            .with_remedy(
                                "attach a passing verification record (verifiedBy + evidence) \
                                 or demote the criterion",
                            ),
                        );
                    }

                    // UNMET_AC_WITH_CANDIDATE (warn): a candidate claims to
                    // satisfy an unmet criterion — a human/skill must judge it.
                    if judgment_required {
                        findings.push(
                            Finding::new(
                                F_UNMET_AC_WITH_CANDIDATE,
                                SEV_WARN,
                                ac.id.clone(),
                                format!(
                                    "criterion {} is unmet but {} candidate change(s) claim to \
                                     satisfy it — judge the content",
                                    ac.id,
                                    satisfied_by.len()
                                ),
                            )
                            .with_remedy(
                                "review the linked changes; if they satisfy it, record a \
                                 passing verification",
                            ),
                        );
                    }

                    criteria.push(CriterionReport {
                        id: ac.id.clone(),
                        text: ac.text.clone(),
                        status: ac.ac_status.clone(),
                        verified_by: ac.verified_by.clone(),
                        judgment_required,
                        satisfied_by,
                    });
                }

                // VIEW_VERIFY_FAIL (block): the materialized view failed its
                // baseline. Among this intent's ACs, if any carries a
                // view-scoped verification whose LATEST such record failed, the
                // view baseline is red. Deduped to one finding per intent.
                if let Some(kind) = failing_view_verification(&node) {
                    findings.push(
                        Finding::new(
                            F_VIEW_VERIFY_FAIL,
                            SEV_BLOCK,
                            intent_kg_id.clone(),
                            format!(
                                "intent {intent_kg_id} has a failing view-scoped '{kind}' \
                                 verification — the materialized view fails its baseline"
                            ),
                        )
                        .with_remedy(
                            "fix the regression and record a passing view-scoped verification, \
                             then re-triage",
                        ),
                    );
                }

                let gate_violations: Vec<String> =
                    report.results.iter().map(|v| v.message.clone()).collect();

                intent_reports.push(IntentReport {
                    id: intent_kg_id.clone(),
                    why: node.why.clone(),
                    conforms: report.conforms,
                    gate_violations,
                    criteria,
                    reviewed_by,
                });
            }
            Err(e) => {
                // An intent reached via the KG that we can't load is a real
                // problem — surface it as a gate violation rather than dropping it.
                let msg = format!("could not load intent {intent_kg_id} for gating: {e}");
                findings.push(
                    Finding::new(
                        F_GATE_VIOLATION,
                        SEV_BLOCK,
                        intent_kg_id.clone(),
                        msg.clone(),
                    )
                    .with_remedy("ensure the intent exists in the vault and re-run"),
                );
                intent_reports.push(IntentReport {
                    id: intent_kg_id.clone(),
                    why: None,
                    conforms: false,
                    gate_violations: vec![msg],
                    criteria: Vec::new(),
                    reviewed_by: None,
                });
            }
        }
    }

    // 5. BAGGAGE_DEP (warn): closure additions not under a covered intent.
    for b in &set.baggage {
        findings.push(
            Finding::new(
                F_BAGGAGE_DEP,
                SEV_WARN,
                b.change.clone(),
                format!(
                    "change landed via dependency closure and is not covered by any intent \
                     (coverage: {})",
                    coverage_label(&b.coverage)
                ),
            )
            .with_query(format!(
                "atomic vault query neighbors change:{} -d 1 --json",
                short12(&b.change)
            ))
            .with_remedy("link this change to an intent, or confirm it is intended baggage"),
        );
    }

    // 6. ORPHAN_CHANGE (block): a candidate that reaches no intent at all.
    //    The message names the change's actual modified files so the agent
    //    knows exactly which `::file-ref` to add to an intent's task.
    for full in &set.only_in_feature {
        if !reached_any_intent.get(full).copied().unwrap_or(false) {
            let paths = change_raw_paths.get(full).cloned().unwrap_or_default();
            let message = if paths.is_empty() {
                "candidate change has no task/intent link — nothing explains why it exists"
                    .to_string()
            } else {
                format!(
                    "candidate change modifies [{}] but no intent's task touches those files — \
                     nothing explains why it exists",
                    paths.join(", ")
                )
            };
            findings.push(
                Finding::new(F_ORPHAN_CHANGE, SEV_BLOCK, full.clone(), message)
                    .with_query(format!(
                        "atomic vault query neighbors change:{} -d 2 --json",
                        short12(full)
                    ))
                    .with_remedy(
                        "add a ::file-ref{path=<one of the paths above>} to a task on the owning \
                         intent (path must match exactly), then run 'atomic vault sync' and \
                         re-triage",
                    ),
            );
        }
    }

    // 7. SCOPE_OUT_BREACH (block): a reached intent declares a file out of
    //    scope (via `intent --SCOPE_OUT_FILE--> file:`) that a candidate change
    //    modifies. Scope-out files sit on the same `file:` nodes as MODIFIES
    //    targets, so the overlap is a direct set intersection.
    let mut scope_breaches: HashSet<(String, String, String)> = HashSet::new();
    for intent_kg_id in intent_to_changes.keys() {
        let Ok(isub) = repo.vault_kg_neighbors(intent_kg_id, 1) else {
            continue;
        };
        let scope_out: HashSet<String> = isub
            .edges
            .iter()
            .filter(|e| e.from_id == *intent_kg_id && kind_is(&e.kind, edge_kind::SCOPE_OUT_FILE))
            .map(|e| e.to_id.clone())
            .collect();
        if scope_out.is_empty() {
            continue;
        }
        for cr in &change_reports {
            for f in &cr.modifies {
                if scope_out.contains(f)
                    && scope_breaches.insert((cr.id.clone(), intent_kg_id.clone(), f.clone()))
                {
                    findings.push(
                        Finding::new(
                            F_SCOPE_OUT_BREACH,
                            SEV_BLOCK,
                            cr.id.clone(),
                            format!(
                                "change modifies {f}, which intent {intent_kg_id} marks out of scope"
                            ),
                        )
                        .with_remedy(
                            "move the edit out of this change or widen the intent's scope",
                        ),
                    );
                }
            }
        }
    }

    // 8. BLAST_UNREVIEWED (warn, best-effort): callers of modified code that
    //    live outside the candidate change-set. For each change's defined
    //    entities, walk incoming CALLS to caller entities, resolve each
    //    caller's defining file, and flag any caller whose file no candidate
    //    modifies. No CALLS/DEFINES edges (unenriched KG) → nothing emitted.
    let mut emitted_blast: HashSet<String> = HashSet::new();
    for (i, entities) in change_entities.iter().enumerate() {
        let mut callers: BTreeSet<String> = BTreeSet::new();
        for entity in entities {
            let Ok(esub) = repo.vault_kg_neighbors(entity, 2) else {
                continue;
            };
            for e in &esub.edges {
                if e.to_id != *entity || !kind_is(&e.kind, edge_kind::CALLS) {
                    continue;
                }
                let caller = &e.from_id;
                // The caller's defining file(s): incoming DEFINES to the caller.
                let caller_files: Vec<&String> = esub
                    .edges
                    .iter()
                    .filter(|d| d.to_id == *caller && kind_is(&d.kind, edge_kind::DEFINES))
                    .map(|d| &d.from_id)
                    .collect();
                // Only flag when we can see a defining file and none of them are
                // in the change-set (avoids noise on partially-enriched graphs).
                if caller_files.is_empty() {
                    continue;
                }
                let in_change_set = caller_files.iter().any(|f| all_modified_files.contains(*f));
                if !in_change_set {
                    callers.insert(caller.clone());
                }
            }
        }
        for caller in &callers {
            if emitted_blast.insert(caller.clone()) {
                findings.push(
                    Finding::new(
                        F_BLAST_UNREVIEWED,
                        SEV_WARN,
                        caller.clone(),
                        format!("{caller} calls modified code but is outside the change-set"),
                    )
                    .with_query(format!("atomic vault query callers {caller} --json"))
                    .with_remedy(
                        "review the caller for compatibility, or bring it into the change-set",
                    ),
                );
            }
        }
        change_reports[i].blast_radius = callers.into_iter().collect();
    }

    // Stable, severity-sorted findings (block → warn → info).
    findings.sort_by(|a, b| {
        a.severity_rank()
            .cmp(&b.severity_rank())
            .then_with(|| a.code.cmp(&b.code))
            .then_with(|| a.focus.cmp(&b.focus))
    });
    intent_reports.sort_by(|a, b| a.id.cmp(&b.id));

    // 9. Summary counts.
    let criteria_met = intent_reports
        .iter()
        .flat_map(|i| &i.criteria)
        .filter(|c| c.status == "met")
        .count();
    let criteria_unmet = intent_reports
        .iter()
        .flat_map(|i| &i.criteria)
        .filter(|c| c.status != "met")
        .count();
    let findings_block = findings.iter().filter(|f| f.severity == SEV_BLOCK).count();
    let findings_warn = findings.iter().filter(|f| f.severity == SEV_WARN).count();
    let findings_info = findings.iter().filter(|f| f.severity == SEV_INFO).count();

    let summary = Summary {
        changes: change_reports.len(),
        files: all_modified_files.len(),
        criteria_met,
        criteria_unmet,
        findings_block,
        findings_warn,
        findings_info,
    };

    // 10. Verdict. Precedence: a blocking finding always wins; otherwise a
    //    STALE_TRIAGE finding (T5b: a done intent whose substance drifted from
    //    its pin) yields `stale`; otherwise `ready`. warn/info never force it.
    let findings_stale = findings.iter().filter(|f| f.code == F_STALE_TRIAGE).count();
    let verdict = if findings_block > 0 {
        Verdict::Blocked
    } else if findings_stale > 0 {
        Verdict::Stale
    } else {
        Verdict::Ready
    };

    // 11. Mint the reproducible reference over the pinned facts.
    let pins = TriagePins {
        feature: feature.to_string(),
        target: target.to_string(),
        view_merkle: view_merkle.clone(),
        candidate_changes: set.only_in_feature.clone(),
        intent_substance_hashes: intent_substance_hashes.clone(),
    };
    let reference = triage_reference(&pins);

    Ok(TriageReport {
        reference,
        verdict,
        inputs: Inputs {
            feature: feature.to_string(),
            target: target.to_string(),
            view_merkle,
            candidate_changes: set.only_in_feature.clone(),
            closure_additions: set.closure_additions.clone(),
            intent_substance_hashes,
        },
        summary,
        intents: intent_reports,
        changes: change_reports,
        findings,
    })
}

fn coverage_label(cov: &atomic_repository::Coverage) -> &'static str {
    match cov {
        atomic_repository::Coverage::Covered => "covered",
        atomic_repository::Coverage::Uncovered => "uncovered",
        atomic_repository::Coverage::Unknown => "unknown",
    }
}

/// Largest embedded diff per file (in hunk lines) before we replace it with a
/// pointer to `atomic change`, so a huge change can't bloat the report.
const MAX_DIFF_LINES_PER_FILE: usize = 3000;

/// Build the real per-file unified diff for a change by reusing the exact
/// `atomic diff -c <hash>` builder ([`change_file_diffs`]), then converting each
/// `FileDiff` into the serializable [`DiffFileView`] mirror. Best-effort: on any
/// error this returns an empty vec (the report still carries message/files).
fn build_change_diff(repo: &Repository, change: &Change, hash: &Hash) -> Vec<DiffFileView> {
    // Sensible review defaults: unified, a few context lines, no color/word-diff.
    let config = DiffOutputConfig::new().with_color(false);
    debug_assert_eq!(config.format, DiffFormat::Unified);

    let file_diffs = match change_file_diffs(repo, change, hash, &config) {
        Ok((file_diffs, _stats)) => file_diffs,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::with_capacity(file_diffs.len());
    for fd in file_diffs {
        let status = match fd.status {
            FileChangeStatus::Added => "added",
            FileChangeStatus::Deleted => "deleted",
            _ => "modified",
        }
        .to_string();
        let path = if !fd.new_path.is_empty() {
            fd.new_path.clone()
        } else {
            fd.old_path.clone()
        };

        let total_lines: usize = fd.hunks.iter().map(|h| h.lines.len()).sum();
        let hunks = if total_lines > MAX_DIFF_LINES_PER_FILE {
            // Too large to embed: leave a note pointing at the full command.
            vec![DiffHunkView {
                header: format!("@@ {path} @@"),
                lines: vec![DiffLineView {
                    tag: " ".to_string(),
                    content: format!(
                        "[diff omitted: {total_lines} lines exceed embed cap; run: atomic change {} --show-hunks]",
                        hash.to_base32()
                    ),
                }],
            }]
        } else {
            fd.hunks
                .iter()
                .map(|h| DiffHunkView {
                    header: h.header(),
                    lines: h
                        .lines
                        .iter()
                        .map(|l| DiffLineView {
                            tag: l.status.prefix().to_string(),
                            content: l.content.clone(),
                        })
                        .collect(),
                })
                .collect()
        };

        out.push(DiffFileView {
            path,
            status,
            hunks,
        });
    }
    out
}

/// Load a reached intent's [`CanonicalNode`], preferring a fresh attestation
/// and falling back to a live lift — the same resolution `intent validate` uses.
/// Returns a string error (best-effort; a load failure becomes a finding, not a
/// hard error).
fn load_intent_node(repo: &Repository, bare_id: &str) -> Result<CanonicalNode, String> {
    let inputs = bridge::read_intent(repo, bare_id).map_err(|e| e.to_string())?;
    match bridge::load_attestation(repo, bare_id, &inputs).map_err(|e| e.to_string())? {
        bridge::Attestation::Fresh(node) => Ok(*node),
        _ => bridge::lift(&inputs).map_err(|e| e.to_string()),
    }
}

/// The `doneSubstanceHash` pin an intent recorded when `done` was granted (T5b).
/// Lives in the intent's frontmatter (review state, deliberately not lifted into
/// the `CanonicalNode`), so it is read straight off the bridge inputs.
/// `None` when the intent was never granted done or predates the pin.
fn load_intent_done_pin(repo: &Repository, bare_id: &str) -> Option<String> {
    let inputs = bridge::read_intent(repo, bare_id).ok()?;
    inputs
        .frontmatter
        .get("doneSubstanceHash")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Best-effort compact provenance for a change: session, agent label, turn
/// timestamp, and governing plan id. `None` if no provenance graph explains it.
fn load_provenance_compact(repo: &Repository, hash_b32: &str) -> Option<serde_json::Value> {
    let hash = Hash::from_base32(hash_b32.as_bytes())?;
    let graphs = load_graphs(repo, &hash).ok()?;
    let (_, g) = graphs.first()?;
    let agent = if g.agent_display_name.is_empty() {
        g.agent_name.clone()
    } else {
        g.agent_display_name.clone()
    };
    Some(serde_json::json!({
        "session_id": g.session_id,
        "agent": agent,
        "timestamp": g.timestamp,
        "plan_id": g.plan_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::ChangeHeader;
    use atomic_repository::{RecordOptions, Repository};
    use tempfile::TempDir;

    fn record_all(repo: &Repository, message: &str) {
        let header = ChangeHeader::new(message);
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();
    }

    /// A change recorded only on `feature` that reaches no intent (the KG has
    /// no task/intent join) is a candidate, is flagged ORPHAN_CHANGE, and blocks
    /// the verdict.
    #[test]
    fn review_flags_orphan_candidate_and_blocks() {
        let temp = TempDir::new().unwrap();
        let mut repo = Repository::init(temp.path()).unwrap();
        let base_view = repo.current_view().to_string();

        // Base change on the shared/base view.
        let file = temp.path().join("a.txt");
        std::fs::write(&file, "base\n").unwrap();
        repo.add("a.txt", Default::default()).unwrap();
        record_all(&repo, "base change");

        // Fork a feature view and record a change only there.
        repo.create_view_from("feature", &base_view).unwrap();
        repo.switch_view("feature").unwrap();
        std::fs::write(&file, "base\nfeature edit\n").unwrap();
        let outcome = repo
            .record(
                ChangeHeader::new("feature change"),
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap();
        let feature_hash = outcome.hash().to_base32();

        let report = build_report(&repo, "feature", &base_view).unwrap();

        // The feature-only change is the candidate set.
        assert!(
            report.inputs.candidate_changes.contains(&feature_hash),
            "candidate_changes {:?} should contain the feature change {}",
            report.inputs.candidate_changes,
            feature_hash
        );
        assert!(report.changes.iter().any(|c| c.id == feature_hash));

        // With no intent join, the candidate is an orphan → blocked.
        assert_eq!(report.verdict, Verdict::Blocked);
        let orphan = report
            .findings
            .iter()
            .find(|f| f.code == F_ORPHAN_CHANGE)
            .expect("expected an ORPHAN_CHANGE finding");
        assert_eq!(orphan.focus, feature_hash);
        assert_eq!(orphan.severity, SEV_BLOCK);

        // The reference is a content address over the pinned inputs.
        assert!(report.reference.starts_with("urn:atomic:triage:"));
    }

    fn intent_opts(title: &str) -> atomic_repository::IntentCreateOptions {
        atomic_repository::IntentCreateOptions {
            title: title.to_string(),
            priority: Some("medium".to_string()),
            assignee: None,
            labels: Vec::new(),
            session_id: None,
            turn_id: None,
            kind: None,
        }
    }

    /// Overwrite a created intent's body (and optionally its status) with a
    /// directive body, then index it into the KG. Reuses the created entry's
    /// frontmatter so manifest resolution keeps working.
    fn install_intent_body(repo: &Repository, intent_file: &str, body: &str, status: Option<&str>) {
        use atomic_core::pristine::VaultEntryType;
        let entry = repo.vault_retrieve(intent_file).unwrap().unwrap();
        let frontmatter_json = match status {
            Some(s) => {
                let mut fm: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(&entry.frontmatter_json).unwrap();
                fm.insert("status".into(), serde_json::Value::String(s.to_string()));
                serde_json::to_string(&fm).unwrap()
            }
            None => entry.frontmatter_json.clone(),
        };
        repo.vault_store(
            intent_file,
            VaultEntryType::Intent,
            body.as_bytes().to_vec(),
            frontmatter_json,
        )
        .unwrap();
        repo.vault_index_kg(intent_file).unwrap();
    }

    /// Like `install_intent_body` but sets arbitrary frontmatter string fields
    /// (e.g. `kind`, `attributedTo`, `status`) so tests can control the review
    /// gate's inputs. `lift_intent` reads these straight off the frontmatter.
    fn install_intent_body_fm(
        repo: &Repository,
        intent_file: &str,
        body: &str,
        overrides: &[(&str, &str)],
    ) {
        use atomic_core::pristine::VaultEntryType;
        let entry = repo.vault_retrieve(intent_file).unwrap().unwrap();
        let mut fm: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&entry.frontmatter_json).unwrap();
        for (k, v) in overrides {
            fm.insert(
                (*k).to_string(),
                serde_json::Value::String((*v).to_string()),
            );
        }
        repo.vault_store(
            intent_file,
            VaultEntryType::Intent,
            body.as_bytes().to_vec(),
            serde_json::to_string(&fm).unwrap(),
        )
        .unwrap();
        repo.vault_index_kg(intent_file).unwrap();
    }

    /// Build a report where intent A (reached by a candidate change that edits
    /// `a.txt`) is remediated by intent B. `b_done` controls whether the
    /// remediation is resolved.
    fn remediation_report(b_done: bool) -> (TriageReport, String) {
        let temp = TempDir::new().unwrap();
        let mut repo = Repository::init(temp.path()).unwrap();
        repo.init_vault().unwrap();
        let base_view = repo.current_view().to_string();

        // Base file on the shared view.
        let file = temp.path().join("a.txt");
        std::fs::write(&file, "base\n").unwrap();
        repo.add("a.txt", Default::default()).unwrap();
        record_all(&repo, "base change");

        // Intent A: a task that touches a.txt (so a candidate editing a.txt
        // reaches A through the KG join).
        let a = repo.vault_intent_create(intent_opts("Intent A")).unwrap();
        let body_a = "\
:::why
Because a.txt matters.
:::

:::acceptance-criterion{#a-ac-1 status=unmet}
a.txt is correct.
:::

:::task{#a-1 status=unmet criteria=a-ac-1}
Edit a.txt.
::file-ref{path=a.txt}
:::";
        install_intent_body(&repo, &a.intent_file, body_a, None);
        let a_node_id = format!("intent:{}", a.uid.to_uppercase());

        // Intent B: remediates A (B --REMEDIATES--> A), status governed by b_done.
        let b = repo.vault_intent_create(intent_opts("Intent B")).unwrap();
        let body_b = format!(
            "\
:::why
Fix the flaw in A.
:::

:::ref{{to=urn:atomic:intent:{} edge=remediates}}
:::",
            a.uid
        );
        install_intent_body(
            &repo,
            &b.intent_file,
            &body_b,
            Some(if b_done { "done" } else { "in-progress" }),
        );

        // Candidate change on the feature view that edits a.txt (auto-enriches
        // the change → file:a.txt MODIFIES edge). `sync_vault(false)`: the
        // intents above live only in pristine (written via `vault_store`, not to
        // the on-disk `.vault/`), so record's working-copy vault reconciliation
        // would otherwise treat them as deleted and wipe their KG projection.
        repo.create_view_from("feature", &base_view).unwrap();
        repo.switch_view("feature").unwrap();
        std::fs::write(&file, "base\nfeature edit\n").unwrap();
        repo.record(
            ChangeHeader::new("feature change"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
                .enrich_kg(true)
                .sync_vault(false),
        )
        .unwrap();

        let report = build_report(&repo, "feature", &base_view).unwrap();
        (report, a_node_id)
    }

    /// An in-flight remediation surfaces an OPEN_REMEDIATION info finding focused
    /// on the remediated intent, and does NOT force the verdict.
    #[test]
    fn review_surfaces_open_remediation_without_blocking() {
        let (report, a_node_id) = remediation_report(false);

        // A was reached by the candidate.
        assert!(
            report.intents.iter().any(|i| i.id == a_node_id),
            "intent A {a_node_id} should be reached; intents: {:?}",
            report.intents.iter().map(|i| &i.id).collect::<Vec<_>>()
        );

        // The OPEN_REMEDIATION finding is present, info severity, focused on A.
        let f = report
            .findings
            .iter()
            .find(|f| f.code == F_OPEN_REMEDIATION)
            .expect("expected an OPEN_REMEDIATION finding");
        assert_eq!(f.severity, SEV_INFO);
        assert_eq!(f.focus, a_node_id);
        assert!(f.message.contains("open remediation"));

        // The verdict is determined by block/stale findings only — info never
        // forces it. Recompute from non-info findings and compare.
        let block = report
            .findings
            .iter()
            .filter(|x| x.severity == SEV_BLOCK)
            .count();
        let stale = report
            .findings
            .iter()
            .filter(|x| x.code == F_STALE_TRIAGE)
            .count();
        let expected = if block > 0 {
            Verdict::Blocked
        } else if stale > 0 {
            Verdict::Stale
        } else {
            Verdict::Ready
        };
        assert_eq!(
            report.verdict, expected,
            "the info OPEN_REMEDIATION finding must not change the verdict"
        );
    }

    /// A resolved (done) remediation produces no OPEN_REMEDIATION finding.
    #[test]
    fn review_omits_open_remediation_when_done() {
        let (report, a_node_id) = remediation_report(true);
        // A is still reached (so the absence below is due to B being done, not
        // to A being unreachable) — keeps this assertion non-vacuous.
        assert!(
            report.intents.iter().any(|i| i.id == a_node_id),
            "intent A {a_node_id} should be reached even when its remediation is done"
        );
        assert!(
            !report.findings.iter().any(|f| f.code == F_OPEN_REMEDIATION),
            "a done remediation should not surface OPEN_REMEDIATION; findings: {:?}",
            report.findings.iter().map(|f| &f.code).collect::<Vec<_>>()
        );
    }

    /// Build a report where a single intent A (installed with `body_a`) is
    /// reached by a candidate that modifies every path in `files`. Each path is
    /// created + tracked on the base view and edited on the feature view.
    ///
    /// `enrich_change_kg` controls whether the candidate record projects the
    /// change's `MODIFIES` KG edges. Passing `false` proves the change→intent
    /// join no longer depends on KG enrichment (it sources modified files from
    /// the change itself via `change_modified_paths`).
    fn report_with_intent(
        body_a: &str,
        files: &[&str],
        enrich_change_kg: bool,
    ) -> (TriageReport, String) {
        let temp = TempDir::new().unwrap();
        let mut repo = Repository::init(temp.path()).unwrap();
        repo.init_vault().unwrap();
        let base_view = repo.current_view().to_string();

        for rel in files {
            let p = temp.path().join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, "base\n").unwrap();
            repo.add(rel, Default::default()).unwrap();
        }
        record_all(&repo, "base change");

        let a = repo.vault_intent_create(intent_opts("Intent A")).unwrap();
        install_intent_body(&repo, &a.intent_file, body_a, None);
        let a_node_id = format!("intent:{}", a.uid.to_uppercase());

        repo.create_view_from("feature", &base_view).unwrap();
        repo.switch_view("feature").unwrap();
        for rel in files {
            let p = temp.path().join(rel);
            std::fs::write(&p, "base\nfeature edit\n").unwrap();
        }
        repo.record(
            ChangeHeader::new("feature change"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
                .enrich_kg(enrich_change_kg)
                .sync_vault(false),
        )
        .unwrap();

        let report = build_report(&repo, "feature", &base_view).unwrap();
        (report, a_node_id)
    }

    /// A reached intent that declares a file out of scope (via a `:::scope-out`
    /// `::file-ref`) which a candidate change modifies → SCOPE_OUT_BREACH (block).
    #[test]
    fn review_flags_scope_out_breach() {
        // A's task touches a.txt (so A is reached); A declares src/billing.rs
        // out of scope; the candidate modifies both.
        let body_a = "\
:::why
Guard billing.
:::

:::acceptance-criterion{#a-ac-1 status=unmet}
Works.
:::

:::task{#a-1 status=unmet criteria=a-ac-1}
Edit a.txt.
::file-ref{path=a.txt}
:::

:::scope-out
The billing subsystem is off limits.
::file-ref{path=src/billing.rs}
:::";
        let (report, a_node_id) = report_with_intent(body_a, &["a.txt", "src/billing.rs"], true);
        let change_id = report
            .inputs
            .candidate_changes
            .first()
            .expect("one candidate change")
            .clone();

        assert!(
            report.intents.iter().any(|i| i.id == a_node_id),
            "intent A {a_node_id} should be reached; intents: {:?}",
            report.intents.iter().map(|i| &i.id).collect::<Vec<_>>()
        );

        let breach = report
            .findings
            .iter()
            .find(|f| f.code == F_SCOPE_OUT_BREACH)
            .expect("expected a SCOPE_OUT_BREACH finding");
        assert_eq!(breach.severity, SEV_BLOCK);
        assert_eq!(breach.focus, change_id);
        assert!(
            breach.message.contains("src/billing.rs"),
            "message should name the scope-out file: {}",
            breach.message
        );
        assert_eq!(report.verdict, Verdict::Blocked);
    }

    /// A reached intent whose AC carries a view-scoped failing verification →
    /// exactly one VIEW_VERIFY_FAIL (block) focused on the intent.
    #[test]
    fn review_flags_view_verify_fail() {
        let body_a = "\
:::why
Baseline must hold.
:::

:::acceptance-criterion{#a-ac-1 status=unmet requiredKinds=e2e}
The view baseline holds.
::verification{kind=e2e outcome=fail scope=view observedAtMerkle=ABC}
:::

:::task{#a-1 status=unmet criteria=a-ac-1}
Edit a.txt.
::file-ref{path=a.txt}
:::";
        let (report, a_node_id) = report_with_intent(body_a, &["a.txt"], true);

        assert!(
            report.intents.iter().any(|i| i.id == a_node_id),
            "intent A {a_node_id} should be reached"
        );

        let vv: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.code == F_VIEW_VERIFY_FAIL)
            .collect();
        assert_eq!(vv.len(), 1, "expected exactly one VIEW_VERIFY_FAIL");
        assert_eq!(vv[0].severity, SEV_BLOCK);
        assert_eq!(vv[0].focus, a_node_id);
        assert!(
            vv[0].message.contains("e2e"),
            "message should name the failing kind: {}",
            vv[0].message
        );
        assert_eq!(report.verdict, Verdict::Blocked);
    }

    /// The change→intent join is enrichment-independent: a change whose file_ops
    /// touch `src/foo.rs` and an intent task with `::file-ref{path=src/foo.rs}`
    /// are joined even when the candidate is recorded WITHOUT KG enrichment (no
    /// `change --MODIFIES--> file:` edge exists), so it is NOT orphaned.
    #[test]
    fn join_does_not_require_change_kg_enrichment() {
        let body_a = "\
:::why
Own foo.
:::

:::acceptance-criterion{#a-ac-1 status=unmet}
Works.
:::

:::task{#a-1 status=unmet criteria=a-ac-1}
Edit foo.
::file-ref{path=src/foo.rs}
:::";
        // enrich_change_kg = false → no MODIFIES edge for the candidate change.
        let (report, a_node_id) = report_with_intent(body_a, &["src/foo.rs"], false);
        let change_id = report
            .inputs
            .candidate_changes
            .first()
            .expect("one candidate change")
            .clone();

        // The intent is reached purely via change_modified_paths → file:src/foo.rs
        // → TOUCHES → task → intent, with no change-side KG enrichment.
        assert!(
            report.intents.iter().any(|i| i.id == a_node_id),
            "intent A {a_node_id} should be reached without KG enrichment; intents: {:?}",
            report.intents.iter().map(|i| &i.id).collect::<Vec<_>>()
        );
        // The change modifies are sourced from the change itself.
        let cr = report
            .changes
            .iter()
            .find(|c| c.id == change_id)
            .expect("change report present");
        assert!(
            cr.modifies.iter().any(|m| m == "file:src/foo.rs"),
            "modifies should include file:src/foo.rs from change_modified_paths: {:?}",
            cr.modifies
        );
        // Crucially, NOT orphaned.
        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.code == F_ORPHAN_CHANGE && f.focus == change_id),
            "the change must not be flagged ORPHAN_CHANGE; findings: {:?}",
            report
                .findings
                .iter()
                .map(|f| (&f.code, &f.focus))
                .collect::<Vec<_>>()
        );
    }

    /// An orphan change's ORPHAN_CHANGE message names its modified path, so the
    /// agent knows exactly which file-ref to add.
    #[test]
    fn orphan_message_names_modified_path() {
        // An intent whose task touches an UNRELATED file, so the candidate that
        // edits src/foo.rs reaches no task → orphan, message must name foo.
        let body_a = "\
:::why
Unrelated.
:::

:::acceptance-criterion{#a-ac-1 status=unmet}
Works.
:::

:::task{#a-1 status=unmet criteria=a-ac-1}
Edit bar.
::file-ref{path=src/bar.rs}
:::";
        let (report, _a_node_id) = report_with_intent(body_a, &["src/foo.rs"], true);
        let change_id = report
            .inputs
            .candidate_changes
            .first()
            .expect("one candidate change")
            .clone();

        let orphan = report
            .findings
            .iter()
            .find(|f| f.code == F_ORPHAN_CHANGE && f.focus == change_id)
            .expect("expected an ORPHAN_CHANGE finding on the change");
        assert!(
            orphan.message.contains("src/foo.rs"),
            "orphan message should name the modified path: {}",
            orphan.message
        );
        // And the remedy points at the file-ref workflow.
        assert!(
            orphan
                .remedy
                .as_deref()
                .unwrap_or_default()
                .contains("::file-ref"),
            "orphan remedy should mention ::file-ref: {:?}",
            orphan.remedy
        );
    }

    /// Each `ChangeReport` carries real review context: the commit message, a
    /// per-file summary from the change's own hunks, and the exact inspect
    /// command.
    #[test]
    fn change_report_carries_message_files_and_review_command() {
        let temp = TempDir::new().unwrap();
        let mut repo = Repository::init(temp.path()).unwrap();
        let base_view = repo.current_view().to_string();

        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/foo.rs"), "base\n").unwrap();
        repo.add("src/foo.rs", Default::default()).unwrap();
        record_all(&repo, "base change");

        repo.create_view_from("feature", &base_view).unwrap();
        repo.switch_view("feature").unwrap();
        std::fs::write(temp.path().join("src/foo.rs"), "base\nfeature edit\n").unwrap();
        let outcome = repo
            .record(
                ChangeHeader::new("Improve foo handling"),
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap();
        let hash = outcome.hash().to_base32();

        let report = build_report(&repo, "feature", &base_view).unwrap();
        let cr = report
            .changes
            .iter()
            .find(|c| c.id == hash)
            .expect("change report for the candidate");

        assert_eq!(cr.message, "Improve foo handling");
        assert!(
            cr.files.iter().any(|f| f.path == "src/foo.rs"),
            "files should include src/foo.rs: {:?}",
            cr.files
        );
        assert!(
            cr.review_command.contains(&hash) && cr.review_command.contains("--show-hunks"),
            "review_command should reference the full hash and --show-hunks: {}",
            cr.review_command
        );

        // The REAL unified diff is embedded: a DiffFileView for the modified
        // path with at least one hunk carrying an added and/or removed line.
        let dfv = cr
            .diff
            .iter()
            .find(|d| d.path == "src/foo.rs")
            .expect("diff should include a DiffFileView for src/foo.rs");
        assert_eq!(dfv.status, "modified");
        let has_change_line = dfv
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| l.tag == "+" || l.tag == "-");
        assert!(
            has_change_line,
            "diff should contain an added and/or removed line: {:?}",
            dfv.hunks
        );
        // The added feature line should appear verbatim in an added line.
        assert!(
            dfv.hunks
                .iter()
                .flat_map(|h| &h.lines)
                .any(|l| l.tag == "+" && l.content.contains("feature edit")),
            "expected an added line with 'feature edit': {:?}",
            dfv.hunks
        );
    }

    const WORK_AUTHOR: &str = "did:atomic:alice";

    /// Build a report where work intent A (author `WORK_AUTHOR`, touching
    /// `a.txt`) is optionally covered by a review intent R. `review` is
    /// `(review_author, review_done)`. Target is the shared `dev` view.
    fn review_gate_report(review: Option<(&str, bool)>) -> (TriageReport, String) {
        let temp = TempDir::new().unwrap();
        let mut repo = Repository::init(temp.path()).unwrap();
        repo.init_vault().unwrap();
        let base_view = repo.current_view().to_string(); // "dev" (shared)

        let file = temp.path().join("a.txt");
        std::fs::write(&file, "base\n").unwrap();
        repo.add("a.txt", Default::default()).unwrap();
        record_all(&repo, "base change");

        // Work intent A: a task touching a.txt (so the candidate reaches it),
        // authored by WORK_AUTHOR. kind stays default `feature`.
        let a = repo.vault_intent_create(intent_opts("Work A")).unwrap();
        let body_a = "\
:::why
Own a.txt.
:::

:::acceptance-criterion{#a-ac-1 status=unmet}
Works.
:::

:::task{#a-1 status=unmet criteria=a-ac-1}
Edit a.txt.
::file-ref{path=a.txt}
:::";
        install_intent_body_fm(
            &repo,
            &a.intent_file,
            body_a,
            &[("attributedTo", WORK_AUTHOR), ("status", "in-progress")],
        );
        let a_node_id = format!("intent:{}", a.uid.to_uppercase());

        // Optional review intent R: kind=review, `reviews` A.
        if let Some((review_author, done)) = review {
            let r = repo.vault_intent_create(intent_opts("Review R")).unwrap();
            let body_r = format!(
                "\
:::why
Reviewed A independently.
:::

:::ref{{to=urn:atomic:intent:{} edge=reviews}}
:::",
                a.uid
            );
            install_intent_body_fm(
                &repo,
                &r.intent_file,
                &body_r,
                &[
                    ("kind", "review"),
                    ("attributedTo", review_author),
                    ("status", if done { "done" } else { "in-progress" }),
                ],
            );
        }

        repo.create_view_from("feature", &base_view).unwrap();
        repo.switch_view("feature").unwrap();
        std::fs::write(&file, "base\nfeature edit\n").unwrap();
        repo.record(
            ChangeHeader::new("feature change"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
                .enrich_kg(true)
                .sync_vault(false),
        )
        .unwrap();

        let report = build_report(&repo, "feature", &base_view).unwrap();
        (report, a_node_id)
    }

    fn unreviewed_findings<'a>(report: &'a TriageReport, work_id: &str) -> Vec<&'a Finding> {
        report
            .findings
            .iter()
            .filter(|f| f.code == F_UNREVIEWED_CHANGE && f.focus == work_id)
            .collect()
    }

    /// (a) A reached work intent with NO reviewing intent → UNREVIEWED_CHANGE,
    /// block into the shared target, and the verdict is Blocked.
    #[test]
    fn review_gate_flags_unreviewed_into_shared() {
        let (report, a_id) = review_gate_report(None);

        assert!(
            report.intents.iter().any(|i| i.id == a_id),
            "work intent {a_id} should be reached"
        );
        let f = unreviewed_findings(&report, &a_id);
        assert_eq!(f.len(), 1, "expected one UNREVIEWED_CHANGE on {a_id}");
        assert_eq!(f[0].severity, SEV_BLOCK, "shared target → block");
        assert!(
            f[0].message.contains("not covered by an independent"),
            "message: {}",
            f[0].message
        );
        assert_eq!(report.verdict, Verdict::Blocked);
        assert!(report
            .intents
            .iter()
            .find(|i| i.id == a_id)
            .unwrap()
            .reviewed_by
            .is_none());
    }

    /// (b) A work intent with a `done` review authored by a DIFFERENT identity
    /// → no UNREVIEWED_CHANGE, and `reviewed_by` is set to the reviewer.
    #[test]
    fn review_gate_passes_with_independent_completed_review() {
        let (report, a_id) = review_gate_report(Some(("did:atomic:bob", true)));

        assert!(
            unreviewed_findings(&report, &a_id).is_empty(),
            "an independent completed review must clear the gate; findings: {:?}",
            report.findings.iter().map(|f| &f.code).collect::<Vec<_>>()
        );
        let intent = report
            .intents
            .iter()
            .find(|i| i.id == a_id)
            .expect("work intent reached");
        assert_eq!(intent.reviewed_by.as_deref(), Some("did:atomic:bob"));
    }

    /// (c) A work intent whose only review is authored by the SAME identity
    /// → still UNREVIEWED_CHANGE, with the independence message.
    #[test]
    fn review_gate_flags_self_authored_review() {
        let (report, a_id) = review_gate_report(Some((WORK_AUTHOR, true)));

        let f = unreviewed_findings(&report, &a_id);
        assert_eq!(f.len(), 1, "self-authored review must not clear the gate");
        assert!(
            f[0].message.contains("self-authored or not yet done"),
            "expected the independence/incomplete message: {}",
            f[0].message
        );
        assert!(report
            .intents
            .iter()
            .find(|i| i.id == a_id)
            .unwrap()
            .reviewed_by
            .is_none());
    }

    /// (c′) A work intent whose only review is by a different identity but NOT
    /// done → still UNREVIEWED_CHANGE (completeness required).
    #[test]
    fn review_gate_flags_incomplete_review() {
        let (report, a_id) = review_gate_report(Some(("did:atomic:bob", false)));
        let f = unreviewed_findings(&report, &a_id);
        assert_eq!(f.len(), 1, "an in-flight review must not clear the gate");
        assert!(f[0].message.contains("self-authored or not yet done"));
    }
}
