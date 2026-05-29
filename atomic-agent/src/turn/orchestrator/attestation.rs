//! Session attestation creation for the turn orchestrator.

use std::collections::{HashMap, HashSet};

use crate::turn::session::AgentSession;

use super::{vendor_from_agent_name, TurnOrchestrator};

impl TurnOrchestrator {
    // =========================================================================
    // Attestation
    // =========================================================================

    /// Create an attestation covering changes this session actually recorded.
    ///
    /// Coverage is based on `session.recorded_change_hashes`, not the full
    /// agent-view history. Agent views inherit parent-view changes, so scanning
    /// the view would over-attribute inherited baseline work to the agent.
    ///
    /// If this same session was already attested before, only newly recorded
    /// hashes not covered by a same-session attestation are included and chained
    /// via `previous_attestation`. Legacy sessions with no recorded hashes are
    /// skipped.
    ///
    /// The attestation is enriched with data from the provenance entries
    /// embedded in each covered change: model name, token counts, cost,
    /// and line-level code change statistics. This data was recorded by
    /// `build_turn_provenance()` at `record_turn()` time.
    ///
    /// Best-effort: if anything fails (repo can't open, view doesn't exist,
    /// no changes recorded), we log and continue — the session still ended
    /// successfully.
    pub(crate) fn create_session_attestation(&self, session: &AgentSession) {
        use atomic_core::change::attestation::{
            AttestAgent, Attestation, CodeChangeStats, ModelUsage,
        };
        use atomic_core::types::Base32;

        // Open the repository
        let repo = match atomic_repository::Repository::open_existing(&self.repo_root) {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "Could not open repository to create attestation for session {}: {}",
                    session.session_id,
                    e,
                );
                return;
            }
        };

        // Use the change hashes the orchestrator actually recorded for THIS
        // session — not the full agent-view history. The agent view is a
        // fork of the parent view and inherits the parent's baseline
        // changes; scanning the full history would (incorrectly) attribute
        // those inherited baseline changes to the agent.
        //
        // For sessions that pre-date this field (or that recorded zero
        // changes), `recorded_change_hashes` is empty — we skip rather
        // than fall back to the old "scan whole view" path, which would
        // resurrect the over-counting bug.
        if session.recorded_change_hashes.is_empty() {
            log::debug!(
                "Session {} has no recorded change hashes — skipping attestation \
                 (legacy session or no turns recorded)",
                session.session_id,
            );
            return;
        }

        let all_change_hashes: Vec<atomic_core::types::Hash> =
            session.recorded_change_hashes.clone();

        // Handle resumed sessions: find existing attestations and determine
        // which changes are new (not yet covered by any attestation).
        //
        // On a fresh session, all changes are new.
        // On a resumed session, only the changes added since the last
        // attestation need to be covered. The new attestation chains to
        // the most recent existing one via `previous_attestation`.
        let mut already_covered: HashSet<atomic_core::types::Hash> = HashSet::new();
        let mut previous_attestation: Option<atomic_core::types::Hash> = None;
        let mut latest_attest_timestamp: i64 = 0;

        // Check each change for existing attestations
        for change_hash in &all_change_hashes {
            let attestations = repo
                .find_attestations_for_change(change_hash)
                .unwrap_or_default();

            for (attest_hash, attest) in &attestations {
                // Only consider attestations from the same session
                if attest.session_id != session.session_id {
                    continue;
                }

                // Track all changes already covered by this session's attestations
                for covered in &attest.changes_covered {
                    already_covered.insert(*covered);
                }

                // Track the most recent attestation for chaining
                if attest.timestamp > latest_attest_timestamp {
                    latest_attest_timestamp = attest.timestamp;
                    previous_attestation = Some(*attest_hash);
                }
            }
        }

        // Determine which changes are new (not covered by existing attestations)
        let new_change_hashes: Vec<atomic_core::types::Hash> = all_change_hashes
            .iter()
            .filter(|h| !already_covered.contains(h))
            .cloned()
            .collect();

        if new_change_hashes.is_empty() {
            log::info!(
                "All {} changes in session {} are already attested — skipping",
                all_change_hashes.len(),
                session.session_id,
            );
            return;
        }

        let is_resume = previous_attestation.is_some();

        // Compute wall duration from session timestamps
        let wall_duration_ms = session
            .ended_at
            .map(|ended| {
                let duration = ended - session.started_at;
                duration.num_milliseconds().max(0) as u64
            })
            .unwrap_or(0);

        // Aggregate provenance data from the covered changes.
        //
        // Each change carries provenance entries (model, tokens, cost) set by
        // `build_turn_provenance()` at record time. We aggregate across all
        // covered changes to populate the attestation with real data instead
        // of leaving it at zeros.
        let mut total_cost: f64 = 0.0;
        let total_api_ms: u64 = 0;
        let mut lines_added: u64 = 0;
        let mut lines_removed: u64 = 0;
        let mut model_agg: HashMap<String, (u64, u64, u64, u64, f64)> = HashMap::new();

        for change_hash in &new_change_hashes {
            let change = match repo.load_change(change_hash) {
                Ok(c) => c,
                Err(e) => {
                    log::debug!(
                        "Could not load change {} for attestation enrichment: {}",
                        change_hash.to_base32(),
                        e,
                    );
                    continue;
                }
            };

            // Aggregate provenance (model, tokens, cost) from each change
            for prov in change.provenance() {
                total_cost += prov.cost.usd;

                let entry = model_agg
                    .entry(prov.model.clone())
                    .or_insert((0, 0, 0, 0, 0.0));
                entry.0 += prov.tokens.input_tokens;
                entry.1 += prov.tokens.output_tokens;
                entry.2 += prov.tokens.cache_read_tokens;
                entry.3 += prov.tokens.cache_write_tokens;
                entry.4 += prov.cost.usd;
            }

            // Count lines from file operations (CRDT semantic layer)
            for file_op in change.file_ops() {
                for line_op in file_op.line_ops() {
                    if line_op.is_insert() {
                        lines_added += 1;
                    } else if line_op.is_delete() {
                        lines_removed += 1;
                    }
                }
            }
        }

        // Build model usage entries from aggregated provenance data
        let models: Vec<ModelUsage> = model_agg
            .into_iter()
            .map(|(model, (inp, out, cr, cw, cost))| {
                ModelUsage::new(&model)
                    .with_input(inp)
                    .with_output(out)
                    .with_cache_read(cr)
                    .with_cache_write(cw)
                    .with_cost(cost)
            })
            .collect();

        // If no provenance data was found in the changes but we know the
        // model from the session, create a minimal model entry so the
        // attestation at least names the model.
        let models = if models.is_empty() && !session.model.is_empty() {
            vec![ModelUsage::new(&session.model)]
        } else {
            models
        };

        // Use the session's vendor (set from OpenCode's provider field)
        // instead of inferring from the agent name, since the session
        // has more accurate data from the actual provider used.
        let vendor = if session.agent_vendor.is_empty() {
            vendor_from_agent_name(&session.agent_name)
        } else {
            &session.agent_vendor
        };
        let agent = AttestAgent::new(&session.agent_name, &session.agent_display_name, vendor);

        let mut builder = Attestation::builder(&session.session_id, agent)
            .cost_usd(total_cost)
            .duration_api_ms(total_api_ms)
            .duration_wall_ms(wall_duration_ms)
            .code_changes(CodeChangeStats::new(lines_added, lines_removed))
            .models(models)
            .changes_covered(new_change_hashes.clone());

        // Chain to previous attestation if this is a resumed session
        if let Some(prev_hash) = previous_attestation {
            builder = builder.previous_attestation(prev_hash);
            builder = builder.notes(format!(
                "Resumed session ({} new changes, {} total in session)",
                new_change_hashes.len(),
                all_change_hashes.len(),
            ));
            log::info!(
                "Chaining to previous attestation {} for resumed session {}",
                prev_hash.to_base32(),
                session.session_id,
            );
        } else if session.turn_count > 0 {
            builder = builder.notes(format!(
                "Auto-created at session end ({} turns)",
                session.turn_count,
            ));
        }

        let attestation = builder.build();

        // Save to the graph
        match repo.save_attestation(&attestation) {
            Ok(hash) => {
                log::info!(
                    "Created attestation {} for session {} ({}{} changes, {} models, +{} -{}, wall: {})",
                    hash.to_base32(),
                    session.session_id,
                    if is_resume { "new: " } else { "" },
                    new_change_hashes.len(),
                    attestation.models.len(),
                    attestation.code_changes.lines_added,
                    attestation.code_changes.lines_removed,
                    attestation.wall_duration_display(),
                );
            }
            Err(e) => {
                log::warn!(
                    "Failed to save attestation for session {}: {}",
                    session.session_id,
                    e,
                );
            }
        }
    }
}
