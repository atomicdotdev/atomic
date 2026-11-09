//! Opt-in replay of narrowly scoped claims over retained evidence.
//!
//! A caller explicitly selects a local checker; manifests cannot select
//! executables. Replay is a read-only supplement to the canonical intent gate,
//! never a derivation of `acStatus`.
//!
//! This module is shared by the CLI and the daemon so the same protocol binds
//! the same native pins on both paths. The caller supplies the manifest bytes
//! and the directory that relative artifact roots resolve against; the service
//! host executes the checker.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use atomic_canonical::node::intent_substance_hash;
use atomic_canonical::{CanonicalNode, Violation};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::RepositoryError;
use crate::intent_bridge::{self, LiftInputs};
use crate::Repository;

/// Manifest and checker input/output ceiling.
pub const MANIFEST_LIMIT: u64 = 1024 * 1024;
/// Wall-clock ceiling for one checker invocation.
pub const CHECKER_TIMEOUT: Duration = Duration::from_secs(10);

const CLAIM_TYPES: &[&str] = &[
    "source_text_coverage/v1",
    "operand_lineage/v1",
    "authority_anchor/v1",
    "event_absence/v1",
];

/// Native pins, ordered from the selected view through its ancestors. Pinning
/// only the selected view would miss changes inherited from a parent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceContext {
    pub intent_id: String,
    pub intent_substance_hash: String,
    pub intent_source_hash: String,
    pub view_chain: Vec<EvidenceViewPin>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceViewPin {
    pub name: String,
    pub merkle: String,
    pub scope: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: String,
    context: EvidenceContext,
    claims: Vec<Claim>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    criterion_id: String,
    claim_type: String,
    artifact_root: PathBuf,
    case: Value,
    policy: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceDecision {
    pub schema_version: String,
    pub request_digest: String,
    pub criterion_id: String,
    pub claim_type: String,
    pub decision: String,
    pub reason: String,
    pub details: Value,
}

#[derive(Debug, Serialize)]
pub struct EvidenceReport {
    pub schema_version: &'static str,
    pub decisions: Vec<EvidenceDecision>,
}

impl EvidenceReport {
    /// Map every non-supported decision to a canonical-gate violation.
    pub fn violations(&self) -> Vec<Violation> {
        self.decisions
            .iter()
            .filter(|decision| decision.decision != "supported")
            .map(|decision| Violation {
                focus_node: decision.criterion_id.clone(),
                shape: "EvidenceReplay".into(),
                path: Some("evidence".into()),
                message: format!(
                    "{}: {} ({})",
                    decision.claim_type, decision.decision, decision.reason
                ),
            })
            .collect()
    }
}

/// What the gate and the pins agree on for one stored intent.
pub struct PreparedIntent {
    pub node: CanonicalNode,
    pub context: EvidenceContext,
}

#[derive(Debug, thiserror::Error)]
pub enum EvidenceError {
    #[error("cycle in evidence replay view chain")]
    ViewCycle,
    #[error("evidence replay manifest exceeds 1 MiB")]
    ManifestTooLarge,
    #[error("evidence replay manifest admission refused: {0}")]
    ManifestAdmission(String),
    #[error("invalid evidence replay manifest: {0}")]
    InvalidManifest(String),
    #[error("evidence replay manifest has an unsupported schema or stale intent/view pins")]
    StalePins,
    #[error("evidence replay requires 1 to 64 claims")]
    ClaimCount,
    #[error("evidence replay refers to an unknown acceptance criterion")]
    UnknownCriterion,
    #[error("duplicate acceptance criterion in evidence replay")]
    DuplicateCriterion,
    #[error("unsupported evidence replay claim type: {0}")]
    UnsupportedClaimType(String),
    #[error("evidence replay case and policy must be objects")]
    InvalidCaseOrPolicy,
    #[error("evidence replay omits a met acceptance criterion")]
    OmittedMetCriterion,
    #[error("intent changed before evidence replay")]
    NodeChanged,
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Build pins from an already-read lift input set. Using the same inputs as
/// the gated node guarantees the pins and the definition cannot disagree.
pub fn context_from_inputs(
    repo: &Repository,
    inputs: &LiftInputs,
) -> Result<EvidenceContext, EvidenceError> {
    let node = intent_bridge::lift(inputs)?;
    let mut next = Some(repo.current_view().to_string());
    let mut seen = HashSet::new();
    let mut view_chain = Vec::new();
    while let Some(name) = next {
        if !seen.insert(name.clone()) {
            return Err(EvidenceError::ViewCycle);
        }
        let info = repo.get_view_info(&name)?;
        view_chain.push(EvidenceViewPin {
            name,
            merkle: info.state_base32(),
            scope: info.kind_label().to_string(),
        });
        next = info.parent_name;
    }
    Ok(EvidenceContext {
        intent_id: node.id.clone(),
        intent_substance_hash: intent_substance_hash(&node),
        intent_source_hash: intent_bridge::source_content_hash(inputs),
        view_chain,
    })
}

/// Re-read an intent and its view chain.
pub fn read_context(repo: &Repository, id: &str) -> Result<EvidenceContext, EvidenceError> {
    let inputs = intent_bridge::read_intent(repo, id)?;
    context_from_inputs(repo, &inputs)
}

/// Resolve the node the gate should validate together with its native pins.
/// A fresh attestation is gated as itself; an unattested or stale intent is
/// gated as the raw lifted node, matching the CLI's ordinary validation.
pub fn prepare(repo: &Repository, id: &str) -> Result<PreparedIntent, EvidenceError> {
    let inputs = intent_bridge::read_intent(repo, id)?;
    let node = match intent_bridge::load_attestation(repo, id, &inputs)? {
        intent_bridge::Attestation::Fresh(node) => *node,
        intent_bridge::Attestation::Stale(_) | intent_bridge::Attestation::None => {
            intent_bridge::lift(&inputs)?
        }
    };
    let context = context_from_inputs(repo, &inputs)?;
    Ok(PreparedIntent { node, context })
}

/// Read a manifest within the protocol ceiling.
pub fn read_manifest(path: &Path) -> Result<Vec<u8>, EvidenceError> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MANIFEST_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MANIFEST_LIMIT {
        return Err(EvidenceError::ManifestTooLarge);
    }
    Ok(bytes)
}

/// Read and replay a manifest file. Relative artifact roots resolve against the
/// manifest's parent directory.
pub fn replay(
    path: &Path,
    checker: &Path,
    context: &EvidenceContext,
    node: &CanonicalNode,
) -> Result<EvidenceReport, EvidenceError> {
    let bytes = read_manifest(path)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    replay_bytes(&bytes, base, checker, context, node)
}

/// Replay already-read manifest bytes. The caller supplies the base directory
/// for relative artifact roots so an RPC caller can name the same directory on
/// the service host.
pub fn replay_bytes(
    bytes: &[u8],
    base: &Path,
    checker: &Path,
    context: &EvidenceContext,
    node: &CanonicalNode,
) -> Result<EvidenceReport, EvidenceError> {
    if context.intent_id != node.id || context.intent_substance_hash != intent_substance_hash(node)
    {
        return Err(EvidenceError::NodeChanged);
    }
    if bytes.len() as u64 > MANIFEST_LIMIT {
        return Err(EvidenceError::ManifestTooLarge);
    }
    let value = atomic_canonical::jcs::admit_document(bytes)
        .map_err(|error| EvidenceError::ManifestAdmission(error.to_string()))?;
    let manifest: Manifest = serde_json::from_value(value)
        .map_err(|error| EvidenceError::InvalidManifest(error.to_string()))?;
    validate_manifest(&manifest, context, node)?;
    let mut decisions = Vec::new();
    for mut claim in manifest.claims {
        // Canonicalize the root once; checker paths remain relative to this
        // root, not its working directory. Missing roots remain unknown.
        let artifact_root = base.join(&claim.artifact_root);
        match artifact_root.canonicalize() {
            Ok(root) if root.is_dir() => claim.artifact_root = root,
            _ => {
                let request = serde_json::json!({
                    "schema_version": "atomic-evidence-replay-request/v1",
                    "context": context,
                    "claim": claim,
                });
                let digest = atomic_canonical::hash::content_hash(&request);
                decisions.push(unknown(&claim, &digest, "artifact_root_unavailable"));
                continue;
            }
        }
        let request = serde_json::json!({
            "schema_version": "atomic-evidence-replay-request/v1",
            "context": context,
            "claim": claim,
        });
        let request_digest = atomic_canonical::hash::content_hash(&request);
        let input = serde_json::to_vec(&serde_json::json!({
            "request": request,
            "request_digest": request_digest,
        }))?;
        let decision = match invoke_checker(checker, &input, CHECKER_TIMEOUT) {
            Ok(bytes) => match atomic_canonical::jcs::admit_document(&bytes)
                .ok()
                .and_then(|value| serde_json::from_value::<EvidenceDecision>(value).ok())
            {
                Some(decision) if valid_decision(&decision, &claim, &request_digest) => decision,
                _ => unknown(
                    &claim,
                    &request_digest,
                    "invalid_or_unbound_checker_response",
                ),
            },
            Err(reason) => unknown(&claim, &request_digest, &reason),
        };
        decisions.push(decision);
    }
    Ok(EvidenceReport {
        schema_version: "atomic-evidence-replay-report/v1",
        decisions,
    })
}

fn validate_manifest(
    manifest: &Manifest,
    context: &EvidenceContext,
    node: &CanonicalNode,
) -> Result<(), EvidenceError> {
    if manifest.schema_version != "atomic-evidence-replay-manifest/v1"
        || &manifest.context != context
    {
        return Err(EvidenceError::StalePins);
    }
    if manifest.claims.is_empty() || manifest.claims.len() > 64 {
        return Err(EvidenceError::ClaimCount);
    }
    let mut seen = HashSet::new();
    for claim in &manifest.claims {
        if !node
            .has_acceptance_criterion
            .iter()
            .any(|ac| ac.id == claim.criterion_id)
        {
            return Err(EvidenceError::UnknownCriterion);
        }
        if !seen.insert(&claim.criterion_id) {
            return Err(EvidenceError::DuplicateCriterion);
        }
        if !CLAIM_TYPES.contains(&claim.claim_type.as_str()) {
            return Err(EvidenceError::UnsupportedClaimType(
                claim.claim_type.clone(),
            ));
        }
        if !claim.case.is_object() || !claim.policy.is_object() {
            return Err(EvidenceError::InvalidCaseOrPolicy);
        }
    }
    if node
        .has_acceptance_criterion
        .iter()
        .any(|ac| ac.ac_status == "met" && !seen.contains(&ac.id))
    {
        return Err(EvidenceError::OmittedMetCriterion);
    }
    Ok(())
}

fn valid_decision(decision: &EvidenceDecision, claim: &Claim, digest: &str) -> bool {
    decision.schema_version == "atomic-evidence-replay-response/v1"
        && decision.request_digest == digest
        && decision.criterion_id == claim.criterion_id
        && decision.claim_type == claim.claim_type
        && !decision.reason.trim().is_empty()
        && ["supported", "contradicted", "not_established"].contains(&decision.decision.as_str())
}

fn unknown(claim: &Claim, digest: &str, reason: &str) -> EvidenceDecision {
    EvidenceDecision {
        schema_version: "atomic-evidence-replay-response/v1".into(),
        request_digest: digest.into(),
        criterion_id: claim.criterion_id.clone(),
        claim_type: claim.claim_type.clone(),
        decision: "not_established".into(),
        reason: reason.into(),
        details: Value::Null,
    }
}

/// File-backed stdio avoids pipe deadlocks. Poll both deadline and output size;
/// never execute a shell or accept arguments embedded in a manifest.
pub fn invoke_checker(checker: &Path, input: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
    if input.len() as u64 > MANIFEST_LIMIT {
        return Err("checker_input_limit_exceeded".into());
    }
    let run = || -> std::io::Result<Vec<u8>> {
        let mut stdin = tempfile::tempfile()?;
        stdin.write_all(input)?;
        stdin.seek(SeekFrom::Start(0))?;
        let mut stdout = tempfile::tempfile()?;
        let stderr = tempfile::tempfile()?;
        let mut child = Command::new(checker)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?))
            .spawn()?;
        let start = Instant::now();
        loop {
            if start.elapsed() >= timeout
                || stdout.metadata()?.len() > MANIFEST_LIMIT
                || stderr.metadata()?.len() > MANIFEST_LIMIT
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::other(
                    "checker timeout or output limit exceeded",
                ));
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(std::io::Error::other("checker exited unsuccessfully"));
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stdout.seek(SeekFrom::Start(0))?;
        let mut output = Vec::new();
        stdout.take(MANIFEST_LIMIT + 1).read_to_end(&mut output)?;
        if output.len() as u64 > MANIFEST_LIMIT {
            return Err(std::io::Error::other("checker output limit exceeded"));
        }
        Ok(output)
    };
    run().map_err(|error| format!("checker_unavailable: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IntentCreateOptions, Repository};
    use atomic_canonical::lift::lift_intent;
    use serde_json::json;

    fn node() -> CanonicalNode {
        lift_intent(
            json!({"id":"TEST-1", "title":"Keep selected passages", "status":"todo"})
                .as_object().unwrap(),
            ":::why\nReview the retained report.\n:::\n\n:::acceptance-criterion{#TEST-1-ac-1 status=unmet}\nThe selected passages remain in the report.\n:::\n",
        ).unwrap()
    }

    fn context(node: &CanonicalNode) -> EvidenceContext {
        EvidenceContext {
            intent_id: node.id.clone(),
            intent_substance_hash: intent_substance_hash(node),
            intent_source_hash: "blake3:source-inputs".into(),
            view_chain: vec![
                EvidenceViewPin {
                    name: "dev".into(),
                    merkle: "CURRENT".into(),
                    scope: "draft".into(),
                },
                EvidenceViewPin {
                    name: "main".into(),
                    merkle: "PARENT".into(),
                    scope: "shared".into(),
                },
            ],
        }
    }

    fn manifest(node: &CanonicalNode) -> Manifest {
        Manifest {
            schema_version: "atomic-evidence-replay-manifest/v1".into(),
            context: context(node),
            claims: vec![Claim {
                criterion_id: node.has_acceptance_criterion[0].id.clone(),
                claim_type: "source_text_coverage/v1".into(),
                artifact_root: ".".into(),
                case: json!({}),
                policy: json!({}),
            }],
        }
    }

    #[test]
    fn native_definition_and_every_view_ancestor_are_pinned() {
        let node = node();
        let current = context(&node);
        let mut manifest = manifest(&node);
        assert!(validate_manifest(&manifest, &current, &node).is_ok());
        manifest.context.intent_substance_hash = "old".into();
        assert!(validate_manifest(&manifest, &current, &node).is_err());
        manifest.context = current.clone();
        manifest.context.view_chain[1].merkle = "changed parent".into();
        assert!(validate_manifest(&manifest, &current, &node).is_err());
        manifest.context = current.clone();
        manifest.context.intent_id = "other".into();
        assert!(validate_manifest(&manifest, &current, &node).is_err());
        manifest.context = current.clone();
        manifest.context.view_chain[0].merkle = "stale view".into();
        assert!(validate_manifest(&manifest, &current, &node).is_err());
    }

    #[test]
    fn unsupported_unknown_duplicate_and_omitted_met_claims_refuse() {
        let mut node = node();
        let current = context(&node);
        let mut manifest = manifest(&node);
        manifest.claims[0].claim_type = "all_tests_passed/v1".into();
        assert!(validate_manifest(&manifest, &current, &node).is_err());
        manifest = self::manifest(&node);
        manifest.claims[0].criterion_id = "unknown".into();
        assert!(validate_manifest(&manifest, &current, &node).is_err());
        manifest = self::manifest(&node);
        manifest.claims.push(self::manifest(&node).claims.remove(0));
        assert!(validate_manifest(&manifest, &current, &node).is_err());
        node.has_acceptance_criterion[0].ac_status = "met".into();
        let mut second = node.has_acceptance_criterion[0].clone();
        second.id = "second".into();
        node.has_acceptance_criterion.push(second);
        manifest = self::manifest(&node);
        assert!(validate_manifest(&manifest, &context(&node), &node).is_err());
    }

    #[test]
    fn responses_bind_request_and_retain_negative_or_unknown() {
        let node = node();
        let claim = manifest(&node).claims.remove(0);
        let mut decision = unknown(&claim, "request-1", "missing_artifact");
        assert!(valid_decision(&decision, &claim, "request-1"));
        assert!(!valid_decision(&decision, &claim, "request-2"));
        assert_eq!(
            EvidenceReport {
                schema_version: "test",
                decisions: vec![decision]
            }
            .violations()
            .len(),
            1
        );
        decision = unknown(&claim, "request-1", "passage_absent");
        decision.decision = "contradicted".into();
        assert_eq!(
            EvidenceReport {
                schema_version: "test",
                decisions: vec![decision]
            }
            .violations()
            .len(),
            1
        );
        decision = unknown(&claim, "request-1", "all_selected_spans_present");
        decision.decision = "supported".into();
        assert!(EvidenceReport {
            schema_version: "test",
            decisions: vec![decision]
        }
        .violations()
        .is_empty());
    }

    #[test]
    fn reads_real_native_intent_and_parent_chain() {
        let temp = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(temp.path()).unwrap();
        repo.init_vault().unwrap();
        let intent = repo
            .vault_intent_create(IntentCreateOptions {
                title: "Read native pins".into(),
                priority: None,
                assignee: None,
                labels: Vec::new(),
                session_id: None,
                turn_id: None,
                kind: None,
            })
            .unwrap();
        let parent = repo.current_view().to_string();
        repo.create_overlay_view("replay", Some(&parent), None)
            .unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        repo.switch_view(working_copy, "replay").unwrap();
        let current = read_context(&repo, &intent.id).unwrap();
        assert!(!current.intent_id.is_empty());
        assert!(current.intent_substance_hash.starts_with("blake3:"));
        assert_eq!(current.view_chain[0].name, "replay");
        assert_eq!(current.view_chain[1].name, parent);
    }

    #[cfg(unix)]
    fn checker(script: &str) -> tempfile::TempPath {
        use std::os::unix::fs::PermissionsExt;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "#!/bin/sh\n{script}\n").unwrap();
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .unwrap();
        file.into_temp_path()
    }

    #[cfg(unix)]
    #[test]
    fn invokes_explicit_checker_and_refuses_response_from_another_request() {
        let node = node();
        let current = context(&node);
        let temp = tempfile::tempdir().unwrap();
        let mut manifest = manifest(&node);
        let path = temp.path().join("manifest.json");
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        manifest.claims[0].artifact_root = temp.path().canonicalize().unwrap();
        let request = json!({"schema_version":"atomic-evidence-replay-request/v1",
            "context":current, "claim":manifest.claims[0]});
        let digest = atomic_canonical::hash::content_hash(&request);
        let mut decision = unknown(&manifest.claims[0], &digest, "selected_passage_present");
        decision.decision = "supported".into();
        let program = checker(&format!(
            "cat <<'RESPONSE'\n{}\nRESPONSE",
            serde_json::to_string(&decision).unwrap()
        ));
        let report = replay(&path, &program, &current, &node).unwrap();
        assert!(report.violations().is_empty());
        decision.request_digest = "another-request".into();
        let other = checker(&format!(
            "cat <<'RESPONSE'\n{}\nRESPONSE",
            serde_json::to_string(&decision).unwrap()
        ));
        let report = replay(&path, &other, &current, &node).unwrap();
        assert_eq!(report.decisions[0].decision, "not_established");
        assert_eq!(report.violations().len(), 1);
        // Duplicate members must be refused while they still exist as bytes.
        decision.request_digest = digest;
        let response = serde_json::to_string(&decision).unwrap();
        let ambiguous = format!("{{\"decision\":\"contradicted\",{}", &response[1..]);
        let duplicate = checker(&format!("cat <<'RESPONSE'\n{ambiguous}\nRESPONSE"));
        let report = replay(&path, &duplicate, &current, &node).unwrap();
        assert_eq!(report.decisions[0].decision, "not_established");
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            format!("{{\"schema_version\":\"other\",{}", &original[1..]),
        )
        .unwrap();
        assert!(replay(&path, &program, &current, &node).is_err());
        std::fs::write(&path, original).unwrap();
        std::fs::remove_dir_all(temp.path()).unwrap();
        // The manifest is retained separately to exercise an unavailable root.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("manifest.json");
        std::fs::write(&missing, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let report = replay(&missing, &program, &current, &node).unwrap();
        assert_eq!(report.decisions[0].reason, "artifact_root_unavailable");
    }

    #[cfg(unix)]
    #[test]
    fn checker_nonzero_timeout_and_excess_output_do_not_pass() {
        let fail = checker("exit 3");
        assert!(invoke_checker(&fail, b"{}", Duration::from_secs(1)).is_err());
        let timeout = checker("exec sleep 2");
        let start = Instant::now();
        assert!(invoke_checker(&timeout, b"{}", Duration::from_millis(50)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        let noisy = checker("exec head -c 1048577 /dev/zero");
        assert!(invoke_checker(&noisy, b"{}", Duration::from_secs(1)).is_err());
    }
}
