//! Session attestation creation for the turn orchestrator.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::error::AgentResult;
use crate::turn::session::AgentSession;

use super::{vendor_from_agent_name, TurnOrchestrator};

// ---------------------------------------------------------------------------
// Complete-payload evidence roots (V4, CB-12A AC3)
// ---------------------------------------------------------------------------

/// The turn-outcomes evidence root: BLAKE3 over the canonical JSON of the
/// session ledger's classified outcomes (boundary pairs + outcomes, in
/// ledger order). Matches [`atomic_core::change::attestation::Attestation`'s
/// `turn_outcomes_root` construction contract.
fn turn_outcomes_root(session: &AgentSession) -> AgentResult<atomic_core::types::Hash> {
    let json = serde_json::to_vec(&session.turn_outcomes).map_err(|e| {
        crate::error::AgentError::AttestationFailed {
            session_id: session.session_id.clone(),
            reason: format!("failed to serialize turn outcomes for evidence root: {e}"),
        }
    })?;
    Ok(atomic_core::types::Hash::of(&json))
}

/// The capture evidence root: BLAKE3 over `(file name, content hash)` pairs
/// of every capture file in the session's captures directory, sorted by
/// name. Matches the `capture_root` construction contract.
pub(crate) fn capture_root(
    sessions_dir: &Path,
    session_id: &str,
) -> AgentResult<atomic_core::types::Hash> {
    let dir = crate::turn::capture::capture_dir(sessions_dir, session_id)?;
    let mut pairs: Vec<(String, Vec<u8>)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let bytes = std::fs::read(&path)?;
            pairs.push((
                name,
                atomic_core::types::Hash::of(&bytes).as_bytes().to_vec(),
            ));
        }
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut buf = Vec::new();
    for (name, hash) in pairs {
        buf.extend_from_slice(name.as_bytes());
        buf.push(0);
        buf.extend_from_slice(&hash);
    }
    Ok(atomic_core::types::Hash::of(&buf))
}

impl TurnOrchestrator {
    /// Load the configured agent DID identity for attestation signing
    /// (CB-12A AC3).
    ///
    /// Returns the signer DID (`did:atomic:<base32>` from
    /// `atomic_canonical::did`) plus the store-verified keypair when a
    /// default identity with a loadable secret key exists. Any failure — no
    /// store, no default identity, encrypted/locked key, secret/public
    /// mismatch — yields `None` and the attestation falls back to the
    /// session MAC, which [`atomic_core::change::attestation::TrustPolicy`]
    /// then refuses as trusted evidence.
    pub(crate) fn load_did_signing_identity(&self) -> Option<(String, atomic_identity::KeyPair)> {
        let store = atomic_identity::IdentityStore::open_default().ok()?;
        // get_default resolves the configured default identity directly.
        let identity = store.get_default().ok()??;
        // load_keypair verifies the secret matches the identity's public key.
        let keypair = store.load_keypair(&identity.id, None).ok()?;
        Some((
            atomic_canonical::did::did_for_public_key(&identity.public_key),
            keypair,
        ))
    }

    // =========================================================================
    // Attestation
    // =========================================================================

    /// Create an attestation covering changes this session actually recorded
    /// plus any git-only observed operations (CB-12A).
    ///
    /// Change coverage is based on `session.recorded_change_hashes`, not the
    /// full agent-view history. Agent views inherit parent-view changes, so
    /// scanning the view would over-attribute inherited baseline work to the
    /// agent.
    ///
    /// CB-12A (RFC §10.2/§10.3.4): turns classified `RepositoryOperations`
    /// (clean worktree, Git moved) contribute their observed operations to
    /// `operations_covered` — observed-operation-only attribution, never
    /// authorship. A clean turn whose HEAD moved is attested instead of
    /// silently skipped. Operations stay incremental across resumes via
    /// `session.attested_operations`.
    ///
    /// If this same session was already attested before, only newly recorded
    /// hashes not covered by a same-session attestation are included and chained
    /// via `previous_attestation`. Sessions with neither recorded hashes nor
    /// unattested operations are skipped.
    ///
    /// The attestation is enriched with data from the provenance entries
    /// embedded in each covered change: model name, token counts, cost,
    /// and line-level code change statistics. This data was recorded by
    /// `build_turn_provenance()` at `record_turn()` time.
    ///
    /// Review R6 (ATOM::aaron::8): the attestation is SIGNED with the
    /// session MAC key over a domain-separated canonical payload before it
    /// is saved, and the signature is verified before the save is trusted.
    /// This is evidence authentication, not a trust-policy decision (RFC §19
    /// Q4 stays undecided; no open-source trust default is chosen here).
    ///
    /// Review R7: the session's attestation progress is marked by the caller
    /// saving the session AFTER this returns — `session_end` now attests
    /// before its final save instead of saving first and never again.
    ///
    /// Returns `Err` only when the attestation could not be persisted — the
    /// caller must then fail the session end closed (never claim success
    /// with unattested operations). Skips are `Ok`.
    pub(crate) fn create_session_attestation(
        &mut self,
        session: &mut AgentSession,
    ) -> AgentResult<()> {
        use atomic_core::change::attestation::{
            AttestAgent, Attestation, CodeChangeStats, ModelUsage,
        };
        use atomic_core::types::Base32;

        // CB-12A: git-only observed operations not yet attested.
        let git_operations = session.unattested_git_operations();

        // Open the repository
        let repo = match atomic_repository::Repository::open_existing(&self.repo_root) {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "Could not open repository to create attestation for session {}: {}",
                    session.session_id,
                    e,
                );
                return Ok(());
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
        // resurrect the over-counting bug. Git-only operations still get
        // attested below.
        if session.recorded_change_hashes.is_empty() && git_operations.is_empty() {
            log::debug!(
                "Session {} has no recorded change hashes or git operations — skipping \
                 attestation (legacy session or no turns recorded)",
                session.session_id,
            );
            return Ok(());
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

        // Review R7: git-only sessions have no content changes, so the
        // change-based lookup above cannot discover their prior chain. The
        // durable session-scoped link keeps their attestations chained.
        if previous_attestation.is_none() {
            previous_attestation = session.last_attestation;
        }

        // Determine which changes are new (not covered by existing attestations)
        let new_change_hashes: Vec<atomic_core::types::Hash> = all_change_hashes
            .iter()
            .filter(|h| !already_covered.contains(h))
            .cloned()
            .collect();

        if new_change_hashes.is_empty() && git_operations.is_empty() {
            log::info!(
                "All {} changes in session {} are already attested — skipping",
                all_change_hashes.len(),
                session.session_id,
            );
            return Ok(());
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

        // CB-12A AC3: fold the provenance evidence root over the covered
        // changes in `changes_covered` order — `(change hash, hash of the
        // serialized provenance entries)` pairs.
        let mut provenance_buf: Vec<u8> = Vec::new();

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

            // Provenance evidence-root pair for this covered change.
            let prov_json = serde_json::to_vec(change.provenance()).unwrap_or_default();
            provenance_buf.extend_from_slice(change_hash.as_bytes());
            provenance_buf.push(0);
            provenance_buf.extend_from_slice(atomic_core::types::Hash::of(&prov_json).as_bytes());

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
            .changes_covered(new_change_hashes.clone())
            .operations_covered(git_operations.clone())
            // CB-12A AC3: the complete-payload evidence roots. The DID
            // signature covers all three, binding the boundary pairs +
            // outcomes, the retained captures, and the provenance to the
            // attestation.
            .turn_outcomes_root(turn_outcomes_root(session)?)
            .capture_root(capture_root(
                self.session_store.sessions_dir(),
                &session.session_id,
            )?)
            .provenance_root(atomic_core::types::Hash::of(&provenance_buf));

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

        let mut attestation = builder.build();

        // Review R6 + CB-12A AC3: sign with the real configured agent DID
        // identity (Ed25519, asymmetric) when one is available; the DID
        // signature covers the canonical payload INCLUDING the three
        // evidence roots. Fall back to the session MAC key (symmetric,
        // evidence authentication only) when no DID identity can be loaded —
        // trust verification (TrustPolicy) refuses `session-mac:*` signers
        // outright, so a MAC-signed attestation can never be promoted to
        // trusted evidence. Either way the signature is verified before the
        // save is trusted: persisting an unsigned or unverifiable audit
        // object would claim signing it never did.
        match self.load_did_signing_identity() {
            Some((did, keypair)) => {
                attestation.sign_with_did(&did, keypair.secret.as_bytes());
                if !attestation.verify_with_did(keypair.public.as_bytes()) {
                    return Err(crate::error::AgentError::AttestationFailed {
                        session_id: session.session_id.clone(),
                        reason: "DID attestation signature failed verification before save"
                            .to_string(),
                    });
                }
                log::info!(
                    "Attestation for session {} signed by agent DID {}",
                    session.session_id,
                    did,
                );
            }
            None => {
                let mac_key = session.ensure_mac_key();
                attestation.sign_with_mac(&mac_key);
                if !attestation.verify_mac(&mac_key) {
                    return Err(crate::error::AgentError::AttestationFailed {
                        session_id: session.session_id.clone(),
                        reason: "attestation signature failed verification before save".to_string(),
                    });
                }
                log::debug!(
                    "No DID identity available — attestation for session {} signed with the session MAC (evidence authentication only, never trusted)",
                    session.session_id,
                );
            }
        }

        // Save to the graph. A persistence failure is a typed error: the
        // session end must fail closed rather than report success with one
        // unattested operation and no durable refusal (review R3/R6 probe).
        match repo.save_attestation(&attestation) {
            Ok(hash) => {
                log::info!(
                    "Created signed attestation {} for session {} ({}{} changes, {} models, +{} -{}, wall: {})",
                    hash.to_base32(),
                    session.session_id,
                    if is_resume { "new: " } else { "" },
                    new_change_hashes.len(),
                    attestation.models.len(),
                    attestation.code_changes.lines_added,
                    attestation.code_changes.lines_removed,
                    attestation.wall_duration_display(),
                );

                // CB-12A: keep the git-operation coverage incremental across
                // resumes. The caller saves the session AFTER this returns so
                // the marks and the chain link persist (review R7 save
                // order).
                if !git_operations.is_empty() {
                    session.mark_operations_attested(&git_operations);
                }
                session.last_attestation = Some(hash);
                Ok(())
            }
            Err(e) => Err(crate::error::AgentError::AttestationFailed {
                session_id: session.session_id.clone(),
                reason: format!("failed to save signed attestation: {e}"),
            }),
        }
    }
}
