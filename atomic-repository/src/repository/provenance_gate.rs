//! CB-12B: trusted provenance gate for protected publication boundaries
//! (RFC-ATOMIC-GIT-CAUSAL-BRIDGE §10.4, §5.6, §12.10).
//!
//! This module answers one question for a set of changes about to cross a
//! protected publication boundary (shared-view insertion, triage promotion,
//! Atomic push, or Git ref export): does every reachable managed-session
//! change carry complete, verifying, trusted evidence?
//!
//! # What is checked (per managed-session change)
//!
//! 1. **Hashed session envelope** — the change's hashed metadata carries a
//!    decodable session envelope (magic + postcard, tamper-evident by hash).
//! 2. **Provenance root present** — a registered provenance graph
//!    (`node_type::PROVENANCE`) explains the change via REV_DEPS.
//! 3. **Session ledger** — a durable `SessionRecord` exists for the session
//!    and carries no `SessionStatus::Incomplete` marker (reason, unbound
//!    commits, affected paths are surfaced verbatim in the refusal).
//! 4. **Verifying turn attestation** — a same-session attestation covers the
//!    change and its signature verifies. Verification uses an injected MAC-key
//!    provider: without the key (e.g. on a receiving remote) the attestation
//!    is **unverifiable and fails closed** — it is never silently accepted.
//! 5. **Trusted signer** — the attestation signer is evaluated against the
//!    explicitly configured `[git.trust]` policy (`SignerTrust::Trusted`).
//!    Revocation always wins; unknown signers are untrusted.
//! 6. **No unexplained Git commit** — a change with
//!    `ChangeOrigin::GitSynthesized` attributed to a managed session whose
//!    ledger does not carry the corresponding incomplete marker is unexplained
//!    boundary work (RFC §10.3.2 synthesized sessions stay incomplete until
//!    reviewed).
//!
//! # What is explicitly NOT claimed
//!
//! The verdict carries a limitation register. As of CB-12A review 8 the
//! attestation signature is a session-scoped keyed-Blake3 MAC, **not** the
//! RFC-required agent-DID signature, and it omits boundary/outcome/binding/
//! provenance roots (F3); fabricated capture evidence is not detectable here
//! (F2); full boundary/manifest/ref/operation ancestry and leases are absent
//! (F4/F5). A passing gate therefore proves *configured-policy enforcement
//! over the available evidence*, nothing stronger. RFC §19 Q4 (open-source
//! trust default) is UNRESOLVED: no default trust is granted or invented
//! here; only explicitly configured signers ever satisfy the trust check.
//!
//! # Content vs provenance separation (RFC §5.2/§12.8)
//!
//! A refused gate says nothing about content correctness. Content and tree/
//! closure correctness are verified independently elsewhere (manifest
//! equivalence, apply verification); unknown-signer content stays recomputable
//! and usable. The verdict keeps the two dimensions separate.

use std::collections::{BTreeMap, HashSet};

use atomic_config::{GitTrustConfig, SignerTrust};
use atomic_core::change::envelope::SessionEnvelope;
use atomic_core::change::session::SessionStatus;
use atomic_core::change::ChangeOrigin;
use atomic_core::types::{Base32, Hash};

use crate::error::RepositoryError;
use crate::Repository;

/// Optional callback supplying a session's attestation MAC key.
///
/// The key lives beside the evidence it protects in the agent session state
/// file (`.atomic/sessions/<id>.json`); it is authentication material, not a
/// trust decision. Callers without access to local session state (receiving
/// remotes, servers, CI) pass `None` — attestations then verify as
/// *unverifiable* and the gate fails closed.
pub type MacKeyProvider<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Configuration for one gate evaluation.
#[derive(Debug, Clone)]
pub struct PublicationGateConfig {
    /// Explicitly configured binding signer trust policy (`[git.trust]`).
    pub trust: GitTrustConfig,
    /// The repository's own signer DID (stated default trust root), if any.
    pub repository_identity: Option<String>,
}

impl PublicationGateConfig {
    /// Load the gate configuration from the repository's `.atomic/config.toml`.
    pub fn from_repo(repo: &Repository) -> Result<Self, RepositoryError> {
        let config = atomic_config::RepoConfig::load(&repo.dot_dir().join("config.toml")).map_err(
            |error| RepositoryError::InvalidOperation {
                message: error.to_string(),
            },
        )?;
        Ok(Self {
            repository_identity: config
                .author
                .as_ref()
                .and_then(|author| author.identity.clone()),
            trust: config.git.trust,
        })
    }
}

/// A single reason the gate refused one change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateBlocker {
    /// The change's hashed metadata carries no session envelope.
    MissingSessionEnvelope { change: String },
    /// Envelope bytes exist but fail to decode (tamper or corruption).
    TamperedSessionEnvelope { change: String, reason: String },
    /// No provenance graph (provenance root) explains this change.
    MissingProvenanceRoot { change: String, session: String },
    /// No durable session record exists for the session.
    MissingSessionLedger { change: String, session: String },
    /// The durable session record carries an `Incomplete` marker.
    SessionIncomplete {
        change: String,
        session: String,
        reason: String,
        unbound_commits: Vec<String>,
        paths: Vec<String>,
    },
    /// No same-session attestation covers this change.
    MissingAttestation { change: String, session: String },
    /// A same-session attestation exists but its signature does not verify.
    AttestationNotVerifying {
        change: String,
        session: String,
        attestation: String,
    },
    /// The attestation cannot be verified in this context (no signature or
    /// no MAC key available).
    AttestationUnverifiable {
        change: String,
        session: String,
        attestation: String,
        reason: String,
    },
    /// The attestation signer is not trusted under the configured policy.
    AttestationSignerUntrusted {
        change: String,
        session: String,
        attestation: String,
        signer: String,
        verdict: String,
    },
    /// A Git-synthesized change is attributed to a managed session without
    /// the session carrying an incomplete marker.
    UnexplainedGitCommit { change: String, session: String },
}

impl GateBlocker {
    /// The change this blocker applies to (base32).
    pub fn change(&self) -> &str {
        match self {
            Self::MissingSessionEnvelope { change }
            | Self::TamperedSessionEnvelope { change, .. }
            | Self::MissingProvenanceRoot { change, .. }
            | Self::MissingSessionLedger { change, .. }
            | Self::SessionIncomplete { change, .. }
            | Self::MissingAttestation { change, .. }
            | Self::AttestationNotVerifying { change, .. }
            | Self::AttestationUnverifiable { change, .. }
            | Self::AttestationSignerUntrusted { change, .. }
            | Self::UnexplainedGitCommit { change, .. } => change,
        }
    }

    /// The session this blocker applies to, when known.
    pub fn session(&self) -> Option<&str> {
        match self {
            Self::MissingSessionEnvelope { .. }
            | Self::TamperedSessionEnvelope { .. }
            | Self::UnexplainedGitCommit { .. } => None,
            Self::MissingProvenanceRoot { session, .. }
            | Self::MissingSessionLedger { session, .. }
            | Self::SessionIncomplete { session, .. }
            | Self::MissingAttestation { session, .. }
            | Self::AttestationNotVerifying { session, .. }
            | Self::AttestationUnverifiable { session, .. }
            | Self::AttestationSignerUntrusted { session, .. } => Some(session),
        }
    }
}

impl std::fmt::Display for GateBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSessionEnvelope { change } => write!(
                f,
                "change {change}: no hashed session envelope in metadata"
            ),
            Self::TamperedSessionEnvelope { change, reason } => {
                write!(
                    f,
                    "change {change}: session envelope fails to decode ({reason})"
                )
            }
            Self::MissingProvenanceRoot { change, session } => {
                write!(
                    f,
                    "change {change}: no provenance root for session {session}"
                )
            }
            Self::MissingSessionLedger { change, session } => {
                write!(
                    f,
                    "change {change}: no durable session ledger for session {session}"
                )
            }
            Self::SessionIncomplete {
                change,
                session,
                reason,
                unbound_commits,
                paths,
            } => {
                write!(f, "change {change}: session {session} is INCOMPLETE ({reason})")?;
                if !unbound_commits.is_empty() {
                    write!(f, "; unbound commits: {}", unbound_commits.join(", "))?;
                }
                if !paths.is_empty() {
                    write!(f, "; affected paths: {}", paths.join(", "))?;
                }
                Ok(())
            }
            Self::MissingAttestation { change, session } => {
                write!(
                    f,
                    "change {change}: no same-session attestation for session {session}"
                )
            }
            Self::AttestationNotVerifying {
                change,
                session,
                attestation,
            } => write!(
                f,
                "change {change}: attestation {attestation} (session {session}) signature does not verify"
            ),
            Self::AttestationUnverifiable {
                change,
                session,
                attestation,
                reason,
            } => write!(
                f,
                "change {change}: attestation {attestation} (session {session}) cannot be \
                 verified ({reason}); unverifiable evidence fails closed"
            ),
            Self::AttestationSignerUntrusted {
                change,
                session,
                attestation,
                signer,
                verdict,
            } => write!(
                f,
                "change {change}: attestation {attestation} (session {session}) signer '{signer}' \
                 is {verdict} under the configured trust policy and cannot satisfy a publication gate"
            ),
            Self::UnexplainedGitCommit { change, session } => write!(
                f,
                "change {change}: Git-synthesized change attributed to session {session} without \
                 an incomplete marker (unexplained Git commit between session boundaries)"
            ),
        }
    }
}

/// Verdict of one gate evaluation, with explicit evidence limitations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateVerdict {
    /// Number of changes examined.
    pub checked: usize,
    /// How many of those are managed-session changes.
    pub managed_changes: usize,
    /// Exact refusal reasons, sorted by change for deterministic diagnostics.
    pub blocks: Vec<GateBlocker>,
    /// Honest statement of what this verdict does NOT prove (CB-12A open
    /// findings, policy-dependent gaps). Never empty for a managed gate run.
    pub limitations: Vec<String>,
}

impl GateVerdict {
    /// Whether publication may proceed (no blockers).
    pub fn allowed(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Human-readable multi-line refusal diagnostics.
    pub fn refusal_report(&self) -> String {
        let mut lines: Vec<String> = self.blocks.iter().map(ToString::to_string).collect();
        for limitation in &self.limitations {
            lines.push(format!("limitation: {limitation}"));
        }
        lines.join("\n")
    }
}

fn default_limitations() -> Vec<String> {
    vec![
        "attestations are session-MAC authenticated, not agent-DID signed; boundary, outcome, \
         binding and provenance roots are not yet covered by the signature (CB-12A review F3)"
            .to_string(),
        "fabricated commit-time capture evidence is not detectable at this gate (CB-12A review F2)"
            .to_string(),
        "full boundary/manifest/ref/operation ancestry and repair leases are not yet enforced \
         (CB-12A review F4/F5)"
            .to_string(),
        "RFC section 19 Q4 (open-source trust default) is unresolved: only explicitly configured \
         signers satisfy the trust check; no default trust is granted"
            .to_string(),
        "this verdict enforces configured policy over available evidence; content correctness is \
         verified independently of signatures"
            .to_string(),
    ]
}

/// Cached per-session ledger status.
enum LedgerStatus {
    Missing,
    Ok,
    Incomplete {
        reason: String,
        unbound_commits: Vec<String>,
        paths: Vec<String>,
    },
}

/// Cached attestation verification for one (change, session) pair.
#[derive(Clone)]
struct AttestationCheck {
    hash: String,
    signer: Option<String>,
    verified: Option<bool>,
    unverifiable_reason: Option<String>,
}

fn trust_label(trust: SignerTrust) -> String {
    match trust {
        SignerTrust::Trusted => "trusted".to_string(),
        SignerTrust::Revoked => "revoked".to_string(),
        SignerTrust::Unknown => "unknown (not configured)".to_string(),
    }
}

fn kind_rank(blocker: &GateBlocker) -> u8 {
    match blocker {
        GateBlocker::MissingSessionEnvelope { .. } => 0,
        GateBlocker::TamperedSessionEnvelope { .. } => 1,
        GateBlocker::MissingProvenanceRoot { .. } => 2,
        GateBlocker::MissingSessionLedger { .. } => 3,
        GateBlocker::SessionIncomplete { .. } => 4,
        GateBlocker::MissingAttestation { .. } => 5,
        GateBlocker::AttestationNotVerifying { .. } => 6,
        GateBlocker::AttestationUnverifiable { .. } => 7,
        GateBlocker::AttestationSignerUntrusted { .. } => 8,
        GateBlocker::UnexplainedGitCommit { .. } => 9,
    }
}

/// Validate a session id for local file lookup (mirrors the agent session
/// store's traversal rules). Returns `None` for ids that cannot name a file.
fn safe_session_id(session: &str) -> Option<&str> {
    if session.is_empty()
        || session.contains('/')
        || session.contains('\\')
        || session.contains("..")
    {
        return None;
    }
    Some(session)
}

/// A [`MacKeyProvider`] that reads the per-session MAC key from the local
/// agent session state file (`.atomic/sessions/<id>.json`).
///
/// The key authenticates evidence; it never decides trust. Sessions whose
/// state file is absent (resumed elsewhere, or a receiving repository) yield
/// `None`, which makes their attestations unverifiable — fail closed.
pub fn local_session_mac_key_provider(repo: &Repository) -> impl Fn(&str) -> Option<String> + '_ {
    let sessions_dir = repo.dot_dir().join("sessions");
    move |session: &str| {
        let session_id = safe_session_id(session)?;
        let path = sessions_dir.join(format!("{session_id}.json"));
        let data = std::fs::read(path).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&data).ok()?;
        value
            .get("mac_key")
            .and_then(serde_json::Value::as_str)
            .filter(|key| !key.is_empty())
            .map(ToString::to_string)
    }
}

impl Repository {
    /// Evaluate the trusted provenance gate over a change closure.
    ///
    /// `changes` must be the complete reachable closure that is about to be
    /// published — transitive dependencies included, not just selected tips.
    /// Managed-session changes are identified by session provenance or a
    /// hashed session envelope; unmanaged changes pass without evidence
    /// requirements. The verdict separates content correctness from
    /// provenance trust and always carries the limitation register.
    pub fn evaluate_publication_gate(
        &self,
        changes: &[Hash],
        config: &PublicationGateConfig,
        mac_key_provider: Option<MacKeyProvider<'_>>,
    ) -> Result<GateVerdict, RepositoryError> {
        let mut verdict = GateVerdict {
            checked: changes.len(),
            managed_changes: 0,
            blocks: Vec::new(),
            limitations: default_limitations(),
        };

        // Per-session caches: ledger status and MAC keys are session-scoped.
        let mut ledger_cache: BTreeMap<String, LedgerStatus> = BTreeMap::new();
        let mut mac_key_cache: BTreeMap<String, Option<String>> = BTreeMap::new();

        for hash in changes {
            let change_b32 = hash.to_base32();
            let change = match self.load_change(hash) {
                Ok(change) => change,
                // A change in the closure that cannot be loaded fails closed:
                // missing bytes at a publication boundary are an evidence hole.
                Err(error) => {
                    verdict.blocks.push(GateBlocker::TamperedSessionEnvelope {
                        change: change_b32.clone(),
                        reason: format!("change bytes cannot be loaded: {error}"),
                    });
                    continue;
                }
            };

            // Identify managed-session work: provenance with a session id or
            // a hashed session envelope in metadata.
            let provenance_session = change
                .provenance()
                .iter()
                .find_map(|prov| prov.session_id.clone());
            let has_envelope_bytes = SessionEnvelope::is_session_envelope(&change.hashed.metadata);

            if provenance_session.is_none() && !has_envelope_bytes {
                // Unmanaged change: no provenance requirements apply.
                continue;
            }
            verdict.managed_changes += 1;

            // Envelope: present AND decodable; session identity from either
            // source must agree (mismatch is tamper evidence).
            let mut session = provenance_session;
            if has_envelope_bytes {
                match SessionEnvelope::decode(&change.hashed.metadata) {
                    Ok(envelope) => {
                        if let Some(envelope_session) = Some(envelope.session_id) {
                            match &session {
                                Some(existing) if existing != &envelope_session => {
                                    verdict.blocks.push(GateBlocker::TamperedSessionEnvelope {
                                        change: change_b32.clone(),
                                        reason: format!(
                                            "envelope session {envelope_session} does not \
                                                 match provenance session {existing}"
                                        ),
                                    });
                                    continue;
                                }
                                Some(_) => {}
                                None => session = Some(envelope_session),
                            }
                        }
                    }
                    Err(error) => {
                        verdict.blocks.push(GateBlocker::TamperedSessionEnvelope {
                            change: change_b32.clone(),
                            reason: error.reason,
                        });
                        continue;
                    }
                }
            } else {
                verdict.blocks.push(GateBlocker::MissingSessionEnvelope {
                    change: change_b32.clone(),
                });
            }

            let Some(session) = session else {
                verdict.blocks.push(GateBlocker::MissingSessionLedger {
                    change: change_b32.clone(),
                    session: "<unidentified>".to_string(),
                });
                continue;
            };

            // Provenance root: at least one registered provenance graph
            // explains this change.
            let has_provenance_root = self
                .find_provenance_for_change(hash)
                .map(|graphs| !graphs.is_empty())
                .unwrap_or(false);
            if !has_provenance_root {
                verdict.blocks.push(GateBlocker::MissingProvenanceRoot {
                    change: change_b32.clone(),
                    session: session.clone(),
                });
            }

            // Durable ledger + incomplete markers (cached per session).
            let incomplete_in_ledger = match ledger_cache.entry(session.clone()) {
                std::collections::btree_map::Entry::Occupied(occupied) => match occupied.get() {
                    LedgerStatus::Missing => {
                        verdict.blocks.push(GateBlocker::MissingSessionLedger {
                            change: change_b32.clone(),
                            session: session.clone(),
                        });
                        false
                    }
                    LedgerStatus::Ok => false,
                    LedgerStatus::Incomplete {
                        reason,
                        unbound_commits,
                        paths,
                    } => {
                        verdict.blocks.push(GateBlocker::SessionIncomplete {
                            change: change_b32.clone(),
                            session: session.clone(),
                            reason: reason.clone(),
                            unbound_commits: unbound_commits.clone(),
                            paths: paths.clone(),
                        });
                        true
                    }
                },
                std::collections::btree_map::Entry::Vacant(vacant) => {
                    let status = match self.get_session_ledger(&session) {
                        Ok(Some((record, _turns))) => match record.status {
                            SessionStatus::Incomplete(incomplete) => {
                                let entry = LedgerStatus::Incomplete {
                                    reason: incomplete.reason.clone(),
                                    unbound_commits: incomplete.unbound_commits.clone(),
                                    paths: incomplete.paths.clone(),
                                };
                                vacant.insert(entry);
                                verdict.blocks.push(GateBlocker::SessionIncomplete {
                                    change: change_b32.clone(),
                                    session: session.clone(),
                                    reason: incomplete.reason.clone(),
                                    unbound_commits: incomplete.unbound_commits.clone(),
                                    paths: incomplete.paths.clone(),
                                });
                                true
                            }
                            _ => {
                                vacant.insert(LedgerStatus::Ok);
                                false
                            }
                        },
                        Ok(None) => {
                            vacant.insert(LedgerStatus::Missing);
                            verdict.blocks.push(GateBlocker::MissingSessionLedger {
                                change: change_b32.clone(),
                                session: session.clone(),
                            });
                            false
                        }
                        Err(error) => {
                            vacant.insert(LedgerStatus::Missing);
                            verdict.blocks.push(GateBlocker::MissingSessionLedger {
                                change: change_b32.clone(),
                                session: format!("{session} (ledger read failed: {error})"),
                            });
                            false
                        }
                    };
                    status
                }
            };

            // Unexplained Git commit: synthesized origin attributed to a
            // managed session whose ledger never recorded the incompleteness.
            if matches!(change.origin(), ChangeOrigin::GitSynthesized { .. })
                && !incomplete_in_ledger
            {
                verdict.blocks.push(GateBlocker::UnexplainedGitCommit {
                    change: change_b32.clone(),
                    session: session.clone(),
                });
            }

            // Attestation: same-session coverage of THIS change, verifying
            // signature, trusted signer. EVERY same-session attestation must
            // verify: a second attestation that fails under the session key
            // is tamper/forgery evidence, never an alternative to trust.
            let attestations = self.find_attestations_for_change(hash).unwrap_or_default();
            let same_session: Vec<_> = attestations
                .iter()
                .filter(|(_, attest)| attest.session_id == session)
                .collect();
            let key_available = mac_key_cache
                .entry(session.clone())
                .or_insert_with(|| mac_key_provider.and_then(|provider| provider(&session)))
                .clone();
            let mut checks: Vec<AttestationCheck> = same_session
                .iter()
                .map(|(attest_hash, attest)| {
                    let signature_available = attest.signature.is_some();
                    match (signature_available, &key_available) {
                        (false, _) => AttestationCheck {
                            hash: attest_hash.to_base32(),
                            signer: attest.signer.clone(),
                            verified: None,
                            unverifiable_reason: Some(
                                "attestation carries no signature (historical V1/V2 format)"
                                    .to_string(),
                            ),
                        },
                        (true, None) => AttestationCheck {
                            hash: attest_hash.to_base32(),
                            signer: attest.signer.clone(),
                            verified: None,
                            unverifiable_reason: Some(
                                "session MAC key unavailable in this context".to_string(),
                            ),
                        },
                        (true, Some(key)) => {
                            let verified = attest.verify_mac(key);
                            AttestationCheck {
                                hash: attest_hash.to_base32(),
                                signer: attest.signer.clone(),
                                verified: Some(verified),
                                unverifiable_reason: None,
                            }
                        }
                    }
                })
                .collect();
            // Latest attestation last so the trust check reads its signer.
            checks.sort_by_key(|check| check.hash.clone());

            let check = if checks.is_empty() {
                None
            } else {
                // Any unverifiable or non-verifying attestation is a refusal.
                Some(
                    checks
                        .iter()
                        .find(|check| check.unverifiable_reason.is_some())
                        .cloned()
                        .or_else(|| {
                            checks
                                .iter()
                                .find(|check| check.verified != Some(true))
                                .cloned()
                        })
                        .unwrap_or_else(|| checks[checks.len() - 1].clone()),
                )
            };

            match check {
                None => {
                    verdict.blocks.push(GateBlocker::MissingAttestation {
                        change: change_b32.clone(),
                        session: session.clone(),
                    });
                }
                Some(AttestationCheck {
                    hash: attest_b32,
                    signer,
                    verified,
                    unverifiable_reason,
                }) => {
                    if let Some(reason) = unverifiable_reason {
                        verdict.blocks.push(GateBlocker::AttestationUnverifiable {
                            change: change_b32.clone(),
                            session: session.clone(),
                            attestation: attest_b32,
                            reason,
                        });
                    } else if verified != Some(true) {
                        verdict.blocks.push(GateBlocker::AttestationNotVerifying {
                            change: change_b32.clone(),
                            session: session.clone(),
                            attestation: attest_b32,
                        });
                    } else {
                        // Signature authenticates; now the trust policy.
                        let signer = signer.unwrap_or_default();
                        let trust = config
                            .trust
                            .evaluate(&signer, config.repository_identity.as_deref());
                        if trust != SignerTrust::Trusted {
                            verdict
                                .blocks
                                .push(GateBlocker::AttestationSignerUntrusted {
                                    change: change_b32.clone(),
                                    session: session.clone(),
                                    attestation: attest_b32,
                                    signer,
                                    verdict: trust_label(trust),
                                });
                        }
                    }
                }
            }
        }

        // Deterministic diagnostics: group by change, then blocker kind.
        verdict.blocks.sort_by(|a, b| {
            a.change()
                .cmp(b.change())
                .then_with(|| kind_rank(a).cmp(&kind_rank(b)))
        });
        Ok(verdict)
    }

    /// Convenience wrapper: evaluate the gate and map any blockers to a
    /// typed refusal carrying the full diagnostic report.
    pub fn enforce_publication_gate(
        &self,
        boundary: &str,
        changes: &[Hash],
        mac_key_provider: Option<MacKeyProvider<'_>>,
    ) -> Result<GateVerdict, RepositoryError> {
        let config = PublicationGateConfig::from_repo(self)?;
        let verdict = self.evaluate_publication_gate(changes, &config, mac_key_provider)?;
        if !verdict.allowed() {
            return Err(RepositoryError::PublicationGateRefused {
                boundary: boundary.to_string(),
                report: verdict.refusal_report(),
                managed_changes: verdict.managed_changes,
                checked: verdict.checked,
            });
        }
        Ok(verdict)
    }
}

/// Collect the transitive dependency closure of `roots` (inclusive), loading
/// change bytes as needed. Used by publication boundaries so that gate
/// coverage is never limited to selected tips.
pub fn reachable_closure(repo: &Repository, roots: &[Hash]) -> Result<Vec<Hash>, RepositoryError> {
    let mut seen: HashSet<Hash> = HashSet::new();
    let mut queue: std::collections::VecDeque<Hash> = roots.iter().copied().collect();
    while let Some(hash) = queue.pop_front() {
        if !seen.insert(hash) {
            continue;
        }
        if let Ok(change) = repo.load_change(&hash) {
            for dep in change.dependencies() {
                if !seen.contains(dep) {
                    queue.push_back(*dep);
                }
            }
        }
    }
    Ok(seen.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{create_temp_repo, create_test_change};
    use atomic_core::change::attestation::{AttestAgent, Attestation};
    use atomic_core::change::provenance_graph::{
        ProvenanceGraphBuilder, ProvenanceNode, ProvenanceNodeKind,
    };
    use atomic_core::change::session::IncompleteSession;
    use atomic_core::change::{Author, ChangeHeader};

    const SESSION: &str = "sess-gate";
    // A long, distinctive key: a short MAC key can appear inside the pack
    // bytes by coincidence (the signature is carried, only the key must not
    // be), which made this assertion flaky by construction.
    const MAC_KEY: &str = "deadbeefcafebabe0123456789abcdef-mac-key-never-ships";

    fn provider(_session: &str) -> Option<String> {
        Some(MAC_KEY.to_string())
    }

    fn no_provider(_session: &str) -> Option<String> {
        None
    }

    fn config_with_signer(signer: &str) -> PublicationGateConfig {
        PublicationGateConfig {
            trust: GitTrustConfig {
                signers: vec![signer.to_string()],
                revoked: Vec::new(),
            },
            repository_identity: None,
        }
    }

    /// A managed change: provenance session + hashed envelope in metadata.
    fn managed_change(message: &str) -> atomic_core::change::Change {
        let mut provenance = atomic_core::change::Provenance::new(
            atomic_core::change::AIVendor::default(),
            "test-model",
            atomic_core::change::AITool::Cli("test".into()),
        );
        provenance.session_id = Some(SESSION.to_string());
        let header = ChangeHeader::builder()
            .message(message)
            .author(Author::new("Test Author", Some("test@example.com")))
            .build();
        let mut change =
            atomic_core::change::Change::new(header, Vec::new(), Vec::new(), Vec::new());
        change.hashed.provenance = vec![provenance];
        let envelope = SessionEnvelope::builder(SESSION, "test-agent")
            .build()
            .encode()
            .expect("envelope encodes");
        change.hashed.metadata = envelope;
        change
    }

    /// Provenance root + active session ledger for the change.
    fn install_provenance_root_and_ledger(repo: &Repository, hash: &Hash) {
        let goal = ProvenanceNode {
            id: "goal-1".into(),
            kind: ProvenanceNodeKind::Goal,
            timestamp: 1000,
            summary: "gate test".into(),
            detail: None,
            change_hash: None,
            tool_name: None,
            tool_call_id: None,
            duration_ms: None,
            classified: false,
            confidence: None,
            consolidated_from: Vec::new(),
        };
        let graph = ProvenanceGraphBuilder::new(SESSION, "test-agent")
            .nodes(vec![goal])
            .changes_explained(vec![*hash])
            .build();
        repo.save_provenance_graph(&graph)
            .expect("provenance graph saves");
    }

    fn install_attestation(repo: &Repository, hash: &Hash, sign: bool) -> String {
        install_attestation_inner(repo, hash, sign).0
    }

    fn install_attestation_inner(
        repo: &Repository,
        hash: &Hash,
        sign: bool,
    ) -> (String, atomic_core::types::Hash) {
        let mut attestation = Attestation::builder(
            SESSION,
            AttestAgent::new("test-agent", "Test Agent", "test-vendor"),
        )
        .changes_covered(vec![*hash])
        .build();
        if sign {
            attestation.sign_with_mac(MAC_KEY);
        }
        let att_hash = repo
            .save_attestation(&attestation)
            .expect("attestation saves");
        (att_hash.to_base32(), att_hash)
    }

    fn blocker_kinds(verdict: &GateVerdict) -> Vec<&'static str> {
        verdict
            .blocks
            .iter()
            .map(|blocker| match blocker {
                GateBlocker::MissingSessionEnvelope { .. } => "envelope",
                GateBlocker::TamperedSessionEnvelope { .. } => "tampered",
                GateBlocker::MissingProvenanceRoot { .. } => "provenance",
                GateBlocker::MissingSessionLedger { .. } => "ledger",
                GateBlocker::SessionIncomplete { .. } => "incomplete",
                GateBlocker::MissingAttestation { .. } => "attestation",
                GateBlocker::AttestationNotVerifying { .. } => "signature",
                GateBlocker::AttestationUnverifiable { .. } => "unverifiable",
                GateBlocker::AttestationSignerUntrusted { .. } => "signer",
                GateBlocker::UnexplainedGitCommit { .. } => "unexplained",
            })
            .collect()
    }

    #[test]
    fn unmanaged_change_passes_without_evidence() {
        let (_dir, repo) = create_temp_repo();
        let change = create_test_change("unmanaged");
        let hash = repo.save_change(&change).expect("save");

        let verdict = repo
            .evaluate_publication_gate(&[hash], &config_with_signer("x"), Some(&provider))
            .expect("gate runs");
        assert!(verdict.allowed());
        assert_eq!(verdict.checked, 1);
        assert_eq!(verdict.managed_changes, 0);
        assert!(!verdict.limitations.is_empty());
    }

    /// CB-12B follow-up AC-5 (session-MAC containment): the session MAC key
    /// NEVER leaves the machine — it exists only in the local session JSON
    /// (read by `local_session_mac_key_provider`), and no exported artifact
    /// (attestation bytes, changes-pack records, or the change's own hashed
    /// sections) carries it. The receiving side holds only MAC-based
    /// evidence, fails closed as unverifiable (the legacy MAC-ONLY
    /// refusal), and would need NO session secret for a complete
    /// agent-DID-signed root set (the positive DID fixture is owned by
    /// CB-12A follow-up ATOM::aaron::19's F3 — DID signing does not exist
    /// yet; this test pins the containment and the legacy refusal that
    /// holds until it does).
    #[test]
    fn session_mac_key_never_leaves_the_machine_and_mac_only_fails_closed() {
        use atomic_objects::{ObjectFamily, ObjectRecord};
        let (_dir, repo) = create_temp_repo();
        let change = managed_change("mac containment");
        let hash = repo.save_change(&change).expect("save");
        install_provenance_root_and_ledger(&repo, &hash);
        install_attestation(&repo, &hash, true);

        // 1. The attestation bytes (the only attestation artifact a push
        //    would carry) contain neither the MAC key nor a usable key
        //    form. The MAC signature is verification evidence, not a
        //    secret, and carries no key material.
        let (_att_b32, att_hash) = install_attestation_inner(&repo, &hash, true);
        let attestation_bytes = {
            // Re-serialize the loaded attestation (the artifact bytes match
            // what a push would carry: the attestation's own serialization).
            let attestation = repo
                .load_attestation(&att_hash)
                .expect("the attestation loads");
            attestation.serialize().expect("the attestation serializes")
        };
        let serialized = String::from_utf8_lossy(&attestation_bytes);
        assert!(
            !serialized.contains(MAC_KEY),
            "the attestation must not carry the MAC key: {serialized}"
        );

        // 2. The changes-pack records a push would carry (the change's
        //    hashed content) contain no MAC key material either.
        let change_bytes = std::fs::read(repo.change_store().change_path(&hash))
            .expect("the change store holds the recorded bytes");
        let serialized_change = String::from_utf8_lossy(&change_bytes);
        assert!(
            !serialized_change.contains(MAC_KEY),
            "the change record must not carry the MAC key"
        );
        let record = ObjectRecord::new(
            ObjectFamily::Change,
            atomic_core::types::Hash::of(&change_bytes).to_hex(),
            change_bytes,
        );
        let pack = crate::git_binding::assemble_changes_pack(
            &[record],
            &crate::git_binding::BindingPackLimits::default_limits(),
        )
        .expect("the pack assembles");
        let pack_text = String::from_utf8_lossy(&pack);
        assert!(
            !pack_text.contains(MAC_KEY),
            "the transport pack must not carry the MAC key"
        );

        // 3. The receiving side (no session JSON, no MAC key provider)
        //    holding ONLY MAC-based evidence fails closed as unverifiable —
        //    the legacy MAC-ONLY refusal that holds until DID signing
        //    ships.
        let verdict = repo
            .evaluate_publication_gate(
                &[hash],
                &config_with_signer("session-mac:sess-gate"),
                Some(&no_provider),
            )
            .expect("gate runs");
        assert!(
            !verdict.allowed(),
            "MAC-only evidence at a receiving side must fail closed"
        );
        assert!(
            blocker_kinds(&verdict).contains(&"unverifiable"),
            "the refusal names the unverifiable attestation: {verdict:?}"
        );
    }

    #[test]
    fn managed_change_with_no_evidence_is_refused_exactly() {
        let (_dir, repo) = create_temp_repo();
        let change = managed_change("managed without evidence");
        let hash = repo.save_change(&change).expect("save");

        let verdict = repo
            .evaluate_publication_gate(
                &[hash],
                &config_with_signer("session-mac:sess-gate"),
                Some(&provider),
            )
            .expect("gate runs");
        assert!(!verdict.allowed());
        assert_eq!(verdict.managed_changes, 1);
        let kinds = blocker_kinds(&verdict);
        assert!(kinds.contains(&"provenance"), "{kinds:?}");
        assert!(kinds.contains(&"ledger"), "{kinds:?}");
        assert!(kinds.contains(&"attestation"), "{kinds:?}");
        // Envelope is present and decodable here — no envelope blocker.
        assert!(
            !kinds.contains(&"envelope") && !kinds.contains(&"tampered"),
            "{kinds:?}"
        );
        // Content/provenance separation is stated, not implied.
        assert!(verdict
            .limitations
            .iter()
            .any(|l| l.contains("content correctness is verified independently")));
        assert!(!verdict.refusal_report().is_empty());
    }

    #[test]
    fn complete_trusted_evidence_passes() {
        let (_dir, repo) = create_temp_repo();
        let change = managed_change("fully evidenced");
        let hash = repo.save_change(&change).expect("save");
        install_provenance_root_and_ledger(&repo, &hash);
        install_attestation(&repo, &hash, true);

        let verdict = repo
            .evaluate_publication_gate(
                &[hash],
                &config_with_signer("session-mac:sess-gate"),
                Some(&provider),
            )
            .expect("gate runs");
        assert!(
            verdict.allowed(),
            "expected complete evidence to pass: {:?}",
            verdict.refusal_report()
        );
    }

    #[test]
    fn tampered_envelope_fails_closed() {
        let (_dir, repo) = create_temp_repo();
        let mut change = managed_change("tampered envelope");
        change.hashed.metadata = b"ATSEnot-a-real-envelope".to_vec();
        let hash = repo.save_change(&change).expect("save");

        let verdict = repo
            .evaluate_publication_gate(
                &[hash],
                &config_with_signer("session-mac:sess-gate"),
                Some(&provider),
            )
            .expect("gate runs");
        assert!(blocker_kinds(&verdict).contains(&"tampered"), "{verdict:?}");
    }

    #[test]
    fn unverifiable_attestation_fails_closed_without_mac_key() {
        let (_dir, repo) = create_temp_repo();
        let change = managed_change("no key on receiving side");
        let hash = repo.save_change(&change).expect("save");
        install_provenance_root_and_ledger(&repo, &hash);
        install_attestation(&repo, &hash, true);

        let verdict = repo
            .evaluate_publication_gate(
                &[hash],
                &config_with_signer("session-mac:sess-gate"),
                Some(&no_provider),
            )
            .expect("gate runs");
        assert!(!verdict.allowed());
        assert!(
            blocker_kinds(&verdict).contains(&"unverifiable"),
            "{verdict:?}"
        );
    }

    #[test]
    fn unsigned_historical_attestation_fails_closed() {
        let (_dir, repo) = create_temp_repo();
        let change = managed_change("unsigned attestation");
        let hash = repo.save_change(&change).expect("save");
        install_provenance_root_and_ledger(&repo, &hash);
        install_attestation(&repo, &hash, false);

        let verdict = repo
            .evaluate_publication_gate(
                &[hash],
                &config_with_signer("session-mac:sess-gate"),
                Some(&provider),
            )
            .expect("gate runs");
        assert!(!verdict.allowed());
        assert!(
            blocker_kinds(&verdict).contains(&"unverifiable"),
            "{verdict:?}"
        );
    }

    #[test]
    fn tampered_attestation_signature_is_refused() {
        let (_dir, repo) = create_temp_repo();
        let change = managed_change("tampered attestation");
        let hash = repo.save_change(&change).expect("save");
        install_provenance_root_and_ledger(&repo, &hash);
        install_attestation(&repo, &hash, true);
        // Tamper: re-save a second attestation that claims the same session
        // but was signed with a different key.
        let mut forged = Attestation::builder(
            SESSION,
            AttestAgent::new("test-agent", "Test Agent", "test-vendor"),
        )
        .changes_covered(vec![hash])
        .build();
        forged.sign_with_mac("bb22bb22");
        repo.save_attestation(&forged).expect("forged saves");

        let verdict = repo
            .evaluate_publication_gate(
                &[hash],
                &config_with_signer("session-mac:sess-gate"),
                Some(&provider),
            )
            .expect("gate runs");
        assert!(
            blocker_kinds(&verdict).contains(&"signature"),
            "{verdict:?}"
        );
    }

    #[test]
    fn unknown_and_revoked_signers_cannot_satisfy_the_gate() {
        let (_dir, repo) = create_temp_repo();
        let change = managed_change("signer trust");
        let hash = repo.save_change(&change).expect("save");
        install_provenance_root_and_ledger(&repo, &hash);
        install_attestation(&repo, &hash, true);

        // Unknown signer (empty policy): refused.
        let verdict = repo
            .evaluate_publication_gate(
                &[hash],
                &PublicationGateConfig {
                    trust: GitTrustConfig::default(),
                    repository_identity: None,
                },
                Some(&provider),
            )
            .expect("gate runs");
        assert!(blocker_kinds(&verdict).contains(&"signer"), "{verdict:?}");

        // Revoked wins even when also listed as a signer.
        let revoked_config = PublicationGateConfig {
            trust: GitTrustConfig {
                signers: vec!["session-mac:sess-gate".to_string()],
                revoked: vec!["session-mac:sess-gate".to_string()],
            },
            repository_identity: None,
        };
        let verdict = repo
            .evaluate_publication_gate(&[hash], &revoked_config, Some(&provider))
            .expect("gate runs");
        assert!(blocker_kinds(&verdict).contains(&"signer"), "{verdict:?}");
    }

    #[test]
    fn incomplete_session_marker_blocks_publication() {
        let (_dir, repo) = create_temp_repo();
        let change = managed_change("incomplete session");
        let hash = repo.save_change(&change).expect("save");
        install_provenance_root_and_ledger(&repo, &hash);
        install_attestation(&repo, &hash, true);

        let incomplete = IncompleteSession::new(
            "hook bypass produced an unbound commit",
            Vec::<String>::new(),
            "refs/atomic/wip/test",
            atomic_core::change::session::SessionIncompleteOrigin::UnattributedGitOperation,
        )
        .with_unbound_commits(vec!["abcdef1234567890".to_string()]);
        repo.mark_session_incomplete(SESSION, None, None, &incomplete)
            .expect("incomplete marker persists");

        let verdict = repo
            .evaluate_publication_gate(
                &[hash],
                &config_with_signer("session-mac:sess-gate"),
                Some(&provider),
            )
            .expect("gate runs");
        let kinds = blocker_kinds(&verdict);
        assert!(kinds.contains(&"incomplete"), "{verdict:?}");
        let refusal = verdict.refusal_report();
        assert!(refusal.contains("hook bypass produced an unbound commit"));
        assert!(refusal.contains("abcdef1234567890"));
    }

    #[test]
    fn synthesized_origin_attributed_to_session_is_unexplained() {
        let (_dir, repo) = create_temp_repo();
        let change = managed_change("synthesized without capture")
            .with_classification(
                atomic_core::change::ChangeKind::Durable,
                None,
                atomic_core::change::ChangeOrigin::git_synthesized(
                    atomic_core::operation::GitObjectId::new(
                        atomic_core::operation::GitHashAlgorithm::Sha1,
                        vec![7u8; 20],
                    )
                    .expect("git object id"),
                    Vec::new(),
                    atomic_core::change::GitDerivation::Root,
                )
                .expect("origin"),
                atomic_core::change::CausalFrontier::empty(),
            )
            .expect("lifecycle facts");
        let hash = repo.save_change(&change).expect("save");
        install_provenance_root_and_ledger(&repo, &hash);
        install_attestation(&repo, &hash, true);

        let verdict = repo
            .evaluate_publication_gate(
                &[hash],
                &config_with_signer("session-mac:sess-gate"),
                Some(&provider),
            )
            .expect("gate runs");
        assert!(
            blocker_kinds(&verdict).contains(&"unexplained"),
            "{verdict:?}"
        );
    }

    #[test]
    fn closure_covers_transitive_dependencies_not_just_tips() {
        let (_dir, repo) = create_temp_repo();

        // Dependency change: managed, unevidenced.
        let dep = managed_change("managed dependency");
        let dep_hash = repo.save_change(&dep).expect("save dep");

        // Tip change: unmanaged, depends on the managed dependency.
        let header = ChangeHeader::builder()
            .message("unmanaged tip with managed dep")
            .author(Author::new("Test Author", Some("test@example.com")))
            .build();
        let mut tip =
            atomic_core::change::Change::new(header, Vec::new(), Vec::new(), vec![dep_hash]);
        tip.hashed.metadata = Vec::new();
        let tip_hash = repo.save_change(&tip).expect("save tip");

        // Gate only the TIP: the closure must pull in the managed dependency
        // and refuse for it.
        let closure = reachable_closure(&repo, &[tip_hash]).expect("closure computes");
        assert!(
            closure.contains(&dep_hash),
            "closure includes transitive dep"
        );
        let verdict = repo
            .evaluate_publication_gate(
                &closure,
                &config_with_signer("session-mac:sess-gate"),
                Some(&provider),
            )
            .expect("gate runs");
        assert!(!verdict.allowed());
        assert_eq!(verdict.managed_changes, 1);
        let refused_change = verdict.blocks[0].change();
        assert_eq!(refused_change, dep_hash.to_base32());
    }

    #[test]
    fn shared_insertion_is_refused_and_draft_insertion_is_not_gated() {
        let (_dir, mut repo) = create_temp_repo();
        // Ensure a Shared target exists (Repository::init seeds the default
        // view lazily; create it explicitly so the test is self-contained).
        {
            use atomic_core::pristine::MutTxnT;
            let mut txn = repo.pristine().write_txn().expect("write txn");
            txn.open_or_create_view("main").expect("shared view");
            txn.commit().expect("commit view creation");
        }
        let change = managed_change("publication refusal");
        let hash = repo.save_change(&change).expect("save");
        // No evidence at all: the strongest refusal case.

        // Shared target: refused before mutation, nothing recorded.
        let error = repo
            .insert_change(&hash, crate::InsertOptions::default().view("main"))
            .expect_err("shared insertion must refuse");
        assert!(
            matches!(error, RepositoryError::PublicationGateRefused { .. }),
            "{error:?}"
        );

        // Draft target: the gate does not apply; insertion proceeds.
        let _draft = crate::CrossViewInsertOptions::new("main", "gate-draft");
        repo.create_view("gate-draft").expect("draft view");
        let outcome = repo.insert_change(&hash, crate::InsertOptions::default().view("gate-draft"));
        assert!(outcome.is_ok(), "draft insertion is not gated: {error:?}");
    }
}

#[cfg(test)]
mod privacy_tests {
    use super::*;
    use crate::repository::{create_temp_repo, create_test_change};
    use atomic_core::change::attestation::{AttestAgent, Attestation};
    use atomic_core::change::envelope::SessionEnvelope;

    /// Privacy sentinel (RFC §5.6/§12.13): attestation payloads that travel
    /// across publication boundaries carry only hashes and signed summaries
    /// — never prompt text, transcript bodies, or decision-graph content.
    #[test]
    fn attestation_payloads_carry_only_hashes_and_summaries() {
        let secret_prompt = "SECRET-PROMPT-CONTENT-do-not-transport";
        let secret_transcript = "SECRET-TRANSCRIPT-BODY";

        let _envelope = SessionEnvelope::builder("sess-privacy", "test-agent")
            .prompt_summary(secret_prompt)
            .build();
        let mut attestation = Attestation::builder(
            "sess-privacy",
            AttestAgent::new("test-agent", "Test Agent", "test-vendor"),
        )
        .changes_covered(vec![atomic_core::types::Merkle::of(b"change-bytes")])
        .build();
        attestation.sign_with_mac("cc33cc33");
        let bytes = attestation.serialize().expect("attestation serializes");

        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(secret_prompt),
            "attestation must not carry prompt text"
        );
        assert!(
            !text.contains(secret_transcript),
            "attestation must not carry transcript bodies"
        );
        // The signed summary IS present: agent name, session id, counters.
        assert!(text.contains("sess-privacy"));
        assert!(text.contains("test-agent"));

        // The envelope itself is hashed change metadata (travels via Atomic
        // remotes, not Git objects); the GATE's diagnostics — the only
        // boundary text this module produces — never quote envelope bodies.
        let (_dir, repo) = create_temp_repo();
        let change = create_test_change("privacy sentinel");
        let hash = repo.save_change(&change).unwrap();
        let verdict = repo
            .evaluate_publication_gate(
                &[hash],
                &PublicationGateConfig {
                    trust: GitTrustConfig::default(),
                    repository_identity: None,
                },
                Some(&|_| Some("dd44dd44".to_string())),
            )
            .expect("gate runs");
        let report = verdict.refusal_report();
        assert!(
            !report.contains(secret_prompt) && !report.contains(secret_transcript),
            "boundary diagnostics must never quote private evidence"
        );
        // And the envelope's prompt summary never appears either: change
        // metadata was empty here, so there is nothing to leak.
        assert!(!report.contains(secret_prompt));
    }
}
