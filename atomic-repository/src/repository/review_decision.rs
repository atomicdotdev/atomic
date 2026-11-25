//! Retained, attested native review evaluations and exact-scope insertion checks.
use std::collections::{BTreeMap, BTreeSet};

use atomic_canonical::review_decision::{Disposition, ReviewDecision, ReviewScope, ViewPin};
use atomic_core::types::{Base32, Hash};
use atomic_identity::keypair::PublicKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::provenance_gate::{
    local_did_key_provider, local_session_mac_key_provider, PublicationGateConfig,
};
use super::{Repository, RepositoryError};
use crate::intent_bridge;

pub const PREFIX: &str = "review-decisions/";
const MAX_DECISIONS: usize = 4096;

fn refused(message: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!("native review decision: {message}"),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewDecisionEvidence {
    pub decision: String,
    pub report_hash: String,
    pub readiness: Disposition,
    pub disposition: Disposition,
    pub accepted_degradations: usize,
    pub signer: String,
}

impl ReviewDecisionEvidence {
    fn from_decision(decision: &ReviewDecision) -> Self {
        Self {
            decision: decision.artifact_hash(),
            report_hash: decision.report_hash.clone(),
            readiness: decision.readiness,
            disposition: decision.evaluation.insertion_disposition(),
            accepted_degradations: decision
                .evaluation
                .passes
                .iter()
                .map(|pass| pass.accepted_degradations.len())
                .sum(),
            signer: decision.attributed_to.clone(),
        }
    }
}

impl Repository {
    fn review_decision_view_pin(&self, view: &str) -> Result<ViewPin, RepositoryError> {
        let info = self.get_view_info(view)?;
        let members: Vec<_> = self
            .effective_history(Some(view))?
            .into_iter()
            .map(|entry| entry.hash.to_base32())
            .collect();
        let membership_hash = Hash::of(&serde_json::to_vec(&members).map_err(refused)?).to_base32();
        Ok(ViewPin {
            view: view.into(),
            scope: info.kind_label().into(),
            state: info.state.to_base32(),
            membership_hash,
            members,
        })
    }

    /// Freeze the same source/destination and current intent context as a v5
    /// native report. This does not grant trust, sign, store or insert anything.
    pub fn review_decision_scope(
        &self,
        source: &str,
        target: &str,
        report: &Value,
    ) -> Result<ReviewScope, RepositoryError> {
        let source = self.review_decision_view_pin(source)?;
        let target = self.review_decision_view_pin(target)?;
        if source.scope != "draft" || target.scope != "shared" || source.view == target.view {
            return Err(refused(
                "publish requires --from a draft and --to a distinct shared view",
            ));
        }
        let available: BTreeSet<_> = source.members.iter().collect();
        if !target.members.iter().all(|hash| available.contains(hash)) {
            return Err(refused("the source does not incorporate the target's recorded state; update the draft and review the combined state before publishing insertion acceptance"));
        }
        let roots = self.get_missing_changes_between(&source.view, Some(&target.view))?;
        let changes = super::provenance_gate::reachable_closure(self, &roots)?
            .into_iter()
            .map(|hash| hash.to_base32())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let intent_scope_hash = intent_bridge::view_intent_sources(self, &source.view)?.hash;
        let mut context_intent_hashes = BTreeMap::new();
        for criterion in report["view_comparison"]["conformance"]["criteria"]
            .as_array()
            .ok_or_else(|| refused("native conformance context unavailable"))?
        {
            if criterion["current_context"].is_object() {
                let id = criterion["intent"]
                    .as_str()
                    .ok_or_else(|| refused("current intent identity missing"))?;
                let key = id.strip_prefix("urn:atomic:intent:").unwrap_or(id);
                let inputs = intent_bridge::read_intent(self, key)?;
                context_intent_hashes
                    .insert(id.into(), intent_bridge::source_content_hash(&inputs));
            }
        }
        let scope = ReviewScope {
            project: self.project_id()?.to_urn(),
            source,
            target,
            changes,
            intent_scope_hash,
            context_intent_hashes,
        };
        scope.validate_report(report).map_err(refused)?;
        Ok(scope)
    }

    /// Existing provenance/integrity preflight; accepting code degradation does
    /// not replace or weaken this gate.
    pub fn review_decision_preflight(
        &self,
        scope: &ReviewScope,
    ) -> Result<super::provenance_gate::GateVerdict, RepositoryError> {
        // Match the existing cross-view gate's complete source-root closure,
        // including original evidence it already requires on shared insertion.
        let roots: Vec<_> = self
            .get_view_changes(Some(&scope.source.view))?
            .into_iter()
            .map(|(_, hash)| hash)
            .collect();
        let hashes = super::provenance_gate::reachable_closure(self, &roots)?;
        let config = PublicationGateConfig::from_repo(self)?;
        self.evaluate_publication_gate(
            &hashes,
            &config,
            Some(&local_session_mac_key_provider(self)),
            Some(&local_did_key_provider(self)),
        )
    }

    fn verify_review_decision_signature(
        &self,
        decision: &ReviewDecision,
        require_trust: bool,
    ) -> Result<(), RepositoryError> {
        decision.validate().map_err(refused)?;
        let key = PublicKey::from_base32(&decision.public_key).map_err(refused)?;
        if !atomic_canonical::did::did_matches_public_key(&decision.attributed_to, &key) {
            return Err(refused("review signer is not bound to the retained key"));
        }
        atomic_canonical::proof::verify_value(&decision.value(), &key).map_err(refused)?;
        let config = PublicationGateConfig::from_repo(self)?;
        if require_trust
            && config.trust.evaluate(
                &decision.attributed_to,
                config.repository_identity.as_deref(),
            ) != atomic_config::SignerTrust::Trusted
        {
            return Err(refused("review signer is not currently trusted by repository policy; publication does not enroll or elevate an identity"));
        }
        Ok(())
    }

    fn verify_review_decision_scope(
        &self,
        decision: &ReviewDecision,
    ) -> Result<(), RepositoryError> {
        let actual = self.review_decision_scope(
            &decision.scope.source.view,
            &decision.scope.target.view,
            &decision.report,
        )?;
        if actual != decision.scope {
            return Err(refused("review scope, insertion closure or intent context is stale; regenerate and evaluate the review"));
        }
        Ok(())
    }

    /// Strict retained inventory. Invalid or misaddressed evidence is not hidden
    /// behind a best-effort filter, and no decision is truncated.
    pub fn native_review_decisions(&self) -> Result<Vec<ReviewDecision>, RepositoryError> {
        let mut result = Vec::new();
        for entry in self.vault_list(PREFIX, None)? {
            if result.len() >= MAX_DECISIONS {
                return Err(refused(
                    "native review inventory exceeds its explicit artifact-count budget",
                ));
            }
            let stored = self
                .vault_retrieve(&entry.path)?
                .ok_or_else(|| refused("registered decision missing"))?;
            let decision = ReviewDecision::parse(&stored.content_bytes).map_err(refused)?;
            if entry.path != format!("{PREFIX}{}/review.md", decision.artifact_hash()) {
                return Err(refused("stored decision address differs from signed bytes"));
            }
            result.push(decision);
        }
        result.sort_by_key(ReviewDecision::artifact_hash);
        Ok(result)
    }

    /// Retain an accountable evaluation of complete native evidence. An accepted
    /// degradation remains not-ready evidence, not an independent/clean verdict.
    pub fn record_review_decision(
        &self,
        decision: &ReviewDecision,
    ) -> Result<ReviewDecisionEvidence, RepositoryError> {
        let _boundary = self.review_evidence_lock()?;
        let bytes = serde_json::to_vec(decision).map_err(refused)?;
        if ReviewDecision::parse(&bytes).map_err(refused)? != *decision {
            return Err(refused("decision does not read back as signed"));
        }
        self.verify_review_decision_signature(decision, true)?;
        self.verify_review_decision_scope(decision)?;
        if decision.evaluation.insertion_disposition() != Disposition::NotReady {
            let gate = self.review_decision_preflight(&decision.scope)?;
            if !gate.allowed() {
                return Err(refused(gate.refusal_report()));
            }
        }
        let inventory = self.native_review_decisions()?;
        if inventory.len() >= MAX_DECISIONS
            && !inventory
                .iter()
                .any(|old| old.artifact_hash() == decision.artifact_hash())
        {
            return Err(refused(
                "native review inventory is full; nothing was truncated or overwritten",
            ));
        }
        let mut superseded = BTreeSet::new();
        for hash in &decision.supersedes {
            if !superseded.insert(hash) {
                return Err(refused("duplicate superseded decision"));
            }
            let old = inventory
                .iter()
                .find(|old| old.artifact_hash() == *hash)
                .ok_or_else(|| refused("superseded review decision is unavailable"))?;
            self.verify_review_decision_signature(old, false)?;
            if old.scope.project != decision.scope.project
                || old.scope.source.view != decision.scope.source.view
                || old.scope.target.view != decision.scope.target.view
            {
                return Err(refused(
                    "supersession cannot change the original project/source/target",
                ));
            }
        }
        let hash = decision.artifact_hash();
        let path = format!("{PREFIX}{hash}/review.md");
        let retired: BTreeSet<_> = inventory
            .iter()
            .flat_map(|old| old.supersedes.iter().cloned())
            .chain(decision.supersedes.iter().cloned())
            .collect();
        if let Some(old) = inventory.iter().find(|old| {
            old.scope == decision.scope
                && old.artifact_hash() != hash
                && !retired.contains(&old.artifact_hash())
        }) {
            return Err(refused(format!("exact-scope review head {} already exists; explicitly --supersedes it rather than silently replacing its evaluation",old.artifact_hash())));
        }
        let existing = self.vault_retrieve(&path)?;
        if let Some(existing) = &existing {
            if existing.content_bytes != bytes {
                return Err(refused("review decision content-address collision"));
            }
        }
        if existing.is_none() {
            self.vault_store(
                &path,
                atomic_core::pristine::VaultEntryType::Attestation,
                bytes,
                "{}".into(),
            )?;
        }
        // Retention must not dirty the working copy and thereby block the
        // insertion this decision evaluates. Export is an explicit CLI action.
        Ok(ReviewDecisionEvidence::from_decision(decision))
    }

    /// Inspect current applicability without treating a stale retained artifact
    /// as corrupt history or silently reusing it for a new insertion.
    pub fn inspect_review_decision(
        &self,
        decision: &ReviewDecision,
    ) -> Result<ReviewDecisionEvidence, RepositoryError> {
        self.verify_review_decision_signature(decision, true)?;
        if self
            .native_review_decisions()?
            .iter()
            .any(|new| new.supersedes.contains(&decision.artifact_hash()))
        {
            return Err(refused("this attested decision has been explicitly superseded; its historical report is retained but no longer authorizes insertion"));
        }
        self.verify_review_decision_scope(decision)?;
        if decision.evaluation.insertion_disposition() != Disposition::NotReady {
            let gate = self.review_decision_preflight(&decision.scope)?;
            if !gate.allowed() {
                return Err(refused(gate.refusal_report()));
            }
        }
        Ok(ReviewDecisionEvidence::from_decision(decision))
    }

    /// Called inside the insertion evidence boundary. A published decision for a
    /// pair must match exactly; moving either side requires a new evaluation.
    pub(super) fn review_decision_for_insert(
        &self,
        source: &str,
        target: &str,
        selected: &[Hash],
    ) -> Result<Option<ReviewDecisionEvidence>, RepositoryError> {
        let inventory = self.native_review_decisions()?;
        let pair: Vec<_> = inventory
            .iter()
            .filter(|decision| {
                decision.scope.source.view == source && decision.scope.target.view == target
            })
            .collect();
        if pair.is_empty() {
            return Ok(None);
        }
        let mut relevant = Vec::new();
        for decision in &pair {
            self.verify_review_decision_signature(decision, false)?;
            if self.verify_review_decision_scope(decision).is_ok() {
                relevant.push(*decision);
            }
        }
        if relevant.is_empty() {
            return Err(refused("published review decision is stale for this source/target; regenerate, evaluate and publish it before inserting"));
        }
        let superseded: BTreeSet<_> = pair
            .iter()
            .flat_map(|decision| decision.supersedes.iter().cloned())
            .collect();
        relevant.retain(|decision| !superseded.contains(&decision.artifact_hash()));
        if relevant.len() != 1 {
            return Err(refused("native review decisions have competing or withdrawn heads; publish an explicit signed supersession"));
        }
        let decision = relevant[0];
        let evidence = self.inspect_review_decision(decision)?;
        if evidence.disposition == Disposition::NotReady {
            return Err(refused(format!(
                "attested review {} explicitly remains not_ready; insertion has not been accepted",
                evidence.decision
            )));
        }
        let selected = super::provenance_gate::reachable_closure(self, selected)?
            .into_iter()
            .map(|hash| hash.to_base32())
            .collect::<BTreeSet<_>>();
        if selected != decision.scope.changes.iter().cloned().collect() {
            return Err(refused(
                "published decision does not cover this exact insertion selection",
            ));
        }
        Ok(Some(evidence))
    }

    /// Direct hash/tag insertion cannot evade an applicable registered decision
    /// simply by omitting the source view's name.
    pub(super) fn review_decision_for_direct_insert(
        &self,
        target: &str,
        selected: &[Hash],
    ) -> Result<Option<ReviewDecisionEvidence>, RepositoryError> {
        let target_pin = self.review_decision_view_pin(target)?;
        let present: BTreeSet<_> = target_pin.members.iter().cloned().collect();
        let wanted: BTreeSet<_> = super::provenance_gate::reachable_closure(self, selected)?
            .into_iter()
            .map(|hash| hash.to_base32())
            .filter(|hash| !present.contains(hash))
            .collect();
        if wanted.is_empty() {
            return Ok(None);
        }
        let inventory = self.native_review_decisions()?;
        let mut sources = BTreeSet::new();
        for decision in inventory
            .iter()
            .filter(|decision| decision.scope.target.view == target)
        {
            let original_target: BTreeSet<_> = decision.scope.target.members.iter().collect();
            let known = decision
                .scope
                .changes
                .iter()
                .any(|hash| !original_target.contains(hash) && wanted.contains(hash));
            let current = self
                .review_decision_view_pin(&decision.scope.source.view)
                .ok()
                .is_some_and(|pin| pin.members.iter().any(|hash| wanted.contains(hash)));
            if known || current {
                sources.insert(decision.scope.source.view.clone());
            }
        }
        if sources.len() > 1 {
            return Err(refused("multiple reviewed source contexts apply; insert the explicitly reviewed source view instead of an ambiguous hash selection"));
        }
        let Some(source) = sources.into_iter().next() else {
            return Ok(None);
        };
        self.review_decision_for_insert(&source, target, selected)
    }
}

#[cfg(test)]
mod tests;
