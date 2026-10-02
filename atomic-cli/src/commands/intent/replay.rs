//! Opt-in replay of narrowly scoped claims. A caller explicitly selects a local
//! checker; manifests cannot select executables. This is a read-only supplement
//! to the ordinary canonical gate, never a derivation of `acStatus`.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use atomic_canonical::node::intent_substance_hash;
use atomic_canonical::{CanonicalNode, Violation};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{bridge, validation_failed};
use crate::error::{CliError, CliResult};

const LIMIT: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
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
pub(crate) struct ReplayContext {
    intent_id: String,
    intent_substance_hash: String,
    intent_source_hash: String,
    view_chain: Vec<ViewPin>,
}

impl ReplayContext {
    pub(crate) fn source_hash(&self) -> &str {
        &self.intent_source_hash
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewPin {
    name: String,
    merkle: String,
    scope: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: String,
    context: ReplayContext,
    claims: Vec<Claim>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    criterion_id: String,
    claim_type: String,
    artifact_root: PathBuf,
    case: Value,
    policy: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    schema_version: String,
    request_digest: String,
    criterion_id: String,
    claim_type: String,
    decision: String,
    reason: String,
    details: Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct ReplayReport {
    schema_version: &'static str,
    decisions: Vec<Decision>,
}

impl ReplayReport {
    pub(crate) fn violations(&self) -> Vec<Violation> {
        self.decisions
            .iter()
            .filter(|d| d.decision != "supported")
            .map(|d| Violation {
                focus_node: d.criterion_id.clone(),
                shape: "EvidenceReplay".into(),
                path: Some("evidence".into()),
                message: format!("{}: {} ({})", d.claim_type, d.decision, d.reason),
            })
            .collect()
    }
}

/// Read through the native vault bridge. No repository handle is retained while
/// the external checker runs, so replay does not hold a writer or owner lock.
pub(crate) fn read_context(root: &Path, id: &str) -> CliResult<ReplayContext> {
    let repo = crate::commands::open_readonly_repository(root).map_err(CliError::Repository)?;
    let inputs = bridge::read_intent(&repo, id)?;
    let node = bridge::lift(&inputs)?;
    let mut next = Some(repo.current_view().to_string());
    let mut seen = HashSet::new();
    let mut view_chain = Vec::new();
    while let Some(name) = next {
        if !seen.insert(name.clone()) {
            return Err(validation_failed("cycle in evidence replay view chain"));
        }
        let info = repo.get_view_info(&name).map_err(CliError::Repository)?;
        view_chain.push(ViewPin {
            name,
            merkle: info.state_base32(),
            scope: info.kind_label().to_string(),
        });
        next = info.parent_name;
    }
    Ok(ReplayContext {
        intent_id: node.id.clone(),
        intent_substance_hash: intent_substance_hash(&node),
        intent_source_hash: bridge::source_content_hash(&inputs),
        view_chain,
    })
}

pub(crate) fn replay(
    path: &Path,
    checker: &Path,
    context: &ReplayContext,
    node: &CanonicalNode,
) -> CliResult<ReplayReport> {
    if context.intent_id != node.id || context.intent_substance_hash != intent_substance_hash(node)
    {
        return Err(validation_failed("intent changed before evidence replay"));
    }
    let bytes = read_limited(path)?;
    let value = atomic_canonical::jcs::admit_document(&bytes).map_err(|e| {
        validation_failed(format!("evidence replay manifest admission refused: {e}"))
    })?;
    let manifest: Manifest = serde_json::from_value(value)
        .map_err(|e| validation_failed(format!("invalid evidence replay manifest: {e}")))?;
    validate_manifest(&manifest, context, node)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
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
        }))
        .map_err(|e| validation_failed(e.to_string()))?;
        let decision = match invoke(checker, &input, TIMEOUT) {
            Ok(bytes) => match atomic_canonical::jcs::admit_document(&bytes)
                .ok()
                .and_then(|value| serde_json::from_value::<Decision>(value).ok())
            {
                Some(d) if valid_decision(&d, &claim, &request_digest) => d,
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
    Ok(ReplayReport {
        schema_version: "atomic-evidence-replay-report/v1",
        decisions,
    })
}

fn validate_manifest(
    manifest: &Manifest,
    context: &ReplayContext,
    node: &CanonicalNode,
) -> CliResult<()> {
    if manifest.schema_version != "atomic-evidence-replay-manifest/v1"
        || &manifest.context != context
    {
        return Err(validation_failed(
            "evidence replay manifest has an unsupported schema or stale intent/view pins",
        ));
    }
    if manifest.claims.is_empty() || manifest.claims.len() > 64 {
        return Err(validation_failed("evidence replay requires 1 to 64 claims"));
    }
    let mut seen = HashSet::new();
    for claim in &manifest.claims {
        if !node
            .has_acceptance_criterion
            .iter()
            .any(|ac| ac.id == claim.criterion_id)
        {
            return Err(validation_failed(
                "evidence replay refers to an unknown acceptance criterion",
            ));
        }
        if !seen.insert(&claim.criterion_id) {
            return Err(validation_failed(
                "duplicate acceptance criterion in evidence replay",
            ));
        }
        if !CLAIM_TYPES.contains(&claim.claim_type.as_str()) {
            return Err(validation_failed(format!(
                "unsupported evidence replay claim type: {}",
                claim.claim_type
            )));
        }
        if !claim.case.is_object() || !claim.policy.is_object() {
            return Err(validation_failed(
                "evidence replay case and policy must be objects",
            ));
        }
    }
    if node
        .has_acceptance_criterion
        .iter()
        .any(|ac| ac.ac_status == "met" && !seen.contains(&ac.id))
    {
        return Err(validation_failed(
            "evidence replay omits a met acceptance criterion",
        ));
    }
    Ok(())
}

fn valid_decision(d: &Decision, claim: &Claim, digest: &str) -> bool {
    d.schema_version == "atomic-evidence-replay-response/v1"
        && d.request_digest == digest
        && d.criterion_id == claim.criterion_id
        && d.claim_type == claim.claim_type
        && !d.reason.trim().is_empty()
        && ["supported", "contradicted", "not_established"].contains(&d.decision.as_str())
}

fn unknown(claim: &Claim, digest: &str, reason: &str) -> Decision {
    Decision {
        schema_version: "atomic-evidence-replay-response/v1".into(),
        request_digest: digest.into(),
        criterion_id: claim.criterion_id.clone(),
        claim_type: claim.claim_type.clone(),
        decision: "not_established".into(),
        reason: reason.into(),
        details: Value::Null,
    }
}

fn read_limited(path: &Path) -> CliResult<Vec<u8>> {
    let file = std::fs::File::open(path).map_err(CliError::Io)?;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(CliError::Io)?;
    if bytes.len() as u64 > LIMIT {
        return Err(validation_failed("evidence replay manifest exceeds 1 MiB"));
    }
    Ok(bytes)
}

/// File-backed stdio avoids pipe deadlocks. Poll both deadline and output size;
/// never execute a shell or accept arguments embedded in a manifest.
fn invoke(checker: &Path, input: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
    if input.len() as u64 > LIMIT {
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
                || stdout.metadata()?.len() > LIMIT
                || stderr.metadata()?.len() > LIMIT
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
        stdout.take(LIMIT + 1).read_to_end(&mut output)?;
        if output.len() as u64 > LIMIT {
            return Err(std::io::Error::other("checker output limit exceeded"));
        }
        Ok(output)
    };
    run().map_err(|e| format!("checker_unavailable: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::lift::lift_intent;
    use atomic_repository::{IntentCreateOptions, Repository};
    use serde_json::json;

    fn node() -> CanonicalNode {
        lift_intent(
            json!({"id":"TEST-1", "title":"Keep selected passages", "status":"todo"})
                .as_object().unwrap(),
            ":::why\nReview the retained report.\n:::\n\n:::acceptance-criterion{#TEST-1-ac-1 status=unmet}\nThe selected passages remain in the report.\n:::\n",
        ).unwrap()
    }

    fn context(node: &CanonicalNode) -> ReplayContext {
        ReplayContext {
            intent_id: node.id.clone(),
            intent_substance_hash: intent_substance_hash(node),
            intent_source_hash: "blake3:source-inputs".into(),
            view_chain: vec![
                ViewPin {
                    name: "dev".into(),
                    merkle: "CURRENT".into(),
                    scope: "draft".into(),
                },
                ViewPin {
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
        let mut m = manifest(&node);
        assert!(validate_manifest(&m, &current, &node).is_ok());
        m.context.intent_substance_hash = "old".into();
        assert!(validate_manifest(&m, &current, &node).is_err());
        m.context = current.clone();
        m.context.view_chain[1].merkle = "changed parent".into();
        assert!(validate_manifest(&m, &current, &node).is_err());
        m.context = current.clone();
        m.context.intent_id = "other".into();
        assert!(validate_manifest(&m, &current, &node).is_err());
        m.context = current.clone();
        m.context.view_chain[0].merkle = "stale view".into();
        assert!(validate_manifest(&m, &current, &node).is_err());
    }

    #[test]
    fn unsupported_unknown_duplicate_and_omitted_met_claims_refuse() {
        let mut node = node();
        let current = context(&node);
        let mut m = manifest(&node);
        m.claims[0].claim_type = "all_tests_passed/v1".into();
        assert!(validate_manifest(&m, &current, &node).is_err());
        m = manifest(&node);
        m.claims[0].criterion_id = "unknown".into();
        assert!(validate_manifest(&m, &current, &node).is_err());
        m = manifest(&node);
        m.claims.push(manifest(&node).claims.remove(0));
        assert!(validate_manifest(&m, &current, &node).is_err());
        node.has_acceptance_criterion[0].ac_status = "met".into();
        let mut second = node.has_acceptance_criterion[0].clone();
        second.id = "second".into();
        node.has_acceptance_criterion.push(second);
        m = manifest(&node);
        assert!(validate_manifest(&m, &context(&node), &node).is_err());
    }

    #[test]
    fn responses_bind_request_and_retain_negative_or_unknown() {
        let n = node();
        let claim = manifest(&n).claims.remove(0);
        let mut d = unknown(&claim, "request-1", "missing_artifact");
        assert!(valid_decision(&d, &claim, "request-1"));
        assert!(!valid_decision(&d, &claim, "request-2"));
        assert_eq!(
            ReplayReport {
                schema_version: "test",
                decisions: vec![d]
            }
            .violations()
            .len(),
            1
        );
        d = unknown(&claim, "request-1", "passage_absent");
        d.decision = "contradicted".into();
        assert_eq!(
            ReplayReport {
                schema_version: "test",
                decisions: vec![d]
            }
            .violations()
            .len(),
            1
        );
        d = unknown(&claim, "request-1", "all_selected_spans_present");
        d.decision = "supported".into();
        assert!(ReplayReport {
            schema_version: "test",
            decisions: vec![d]
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
        repo.switch_view("replay").unwrap();
        drop(repo);
        let current = read_context(temp.path(), &intent.id).unwrap();
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
        let n = node();
        let current = context(&n);
        let temp = tempfile::tempdir().unwrap();
        let mut m = manifest(&n);
        let path = temp.path().join("manifest.json");
        std::fs::write(&path, serde_json::to_vec(&m).unwrap()).unwrap();
        m.claims[0].artifact_root = temp.path().canonicalize().unwrap();
        let request = json!({"schema_version":"atomic-evidence-replay-request/v1",
            "context":current, "claim":m.claims[0]});
        let digest = atomic_canonical::hash::content_hash(&request);
        let mut d = unknown(&m.claims[0], &digest, "selected_passage_present");
        d.decision = "supported".into();
        let program = checker(&format!(
            "cat <<'RESPONSE'\n{}\nRESPONSE",
            serde_json::to_string(&d).unwrap()
        ));
        let report = replay(&path, &program, &current, &n).unwrap();
        assert!(report.violations().is_empty());
        d.request_digest = "another-request".into();
        let other = checker(&format!(
            "cat <<'RESPONSE'\n{}\nRESPONSE",
            serde_json::to_string(&d).unwrap()
        ));
        let report = replay(&path, &other, &current, &n).unwrap();
        assert_eq!(report.decisions[0].decision, "not_established");
        assert_eq!(report.violations().len(), 1);
        // Duplicate members must be refused while they still exist as bytes.
        d.request_digest = digest;
        let response = serde_json::to_string(&d).unwrap();
        let ambiguous = format!("{{\"decision\":\"contradicted\",{}", &response[1..]);
        let duplicate = checker(&format!("cat <<'RESPONSE'\n{ambiguous}\nRESPONSE"));
        let report = replay(&path, &duplicate, &current, &n).unwrap();
        assert_eq!(report.decisions[0].decision, "not_established");
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            format!("{{\"schema_version\":\"other\",{}", &original[1..]),
        )
        .unwrap();
        assert!(replay(&path, &program, &current, &n).is_err());
        std::fs::write(&path, original).unwrap();
        std::fs::remove_dir_all(temp.path()).unwrap();
        // The manifest is retained separately to exercise an unavailable root.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("manifest.json");
        std::fs::write(&missing, serde_json::to_vec(&m).unwrap()).unwrap();
        let report = replay(&missing, &program, &current, &n).unwrap();
        assert_eq!(report.decisions[0].reason, "artifact_root_unavailable");
    }

    #[cfg(unix)]
    #[test]
    fn checker_nonzero_timeout_and_excess_output_do_not_pass() {
        let fail = checker("exit 3");
        assert!(invoke(&fail, b"{}", Duration::from_secs(1)).is_err());
        let timeout = checker("exec sleep 2");
        let start = Instant::now();
        assert!(invoke(&timeout, b"{}", Duration::from_millis(50)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        let noisy = checker("exec head -c 1048577 /dev/zero");
        assert!(invoke(&noisy, b"{}", Duration::from_secs(1)).is_err());
    }
}
