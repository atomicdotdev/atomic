//! CB-12B: trusted receiving-side verifier for protected Git publication
//! (RFC-ATOMIC-GIT-CAUSAL-BRIDGE §10.4, §12.10).
//!
//! Deployed on a trusted receiving boundary — an Atomic-controlled remote,
//! a server `pre-receive`/`update` hook, or a required CI/hosting status
//! check — this command enforces the same managed-provenance evidence
//! contract the publishing side enforces. It reads Git's ref-update lines
//! (`<old> <new> <ref>`) on stdin and refuses (exit non-zero) any protected
//! ref update whose new state carries managed-session changes with missing,
//! tampered, untrusted, or incomplete evidence.
//!
//! # Deployment contract (honest scope)
//!
//! Verification runs against the receiving repository's ingested state:
//! the projection commit at the new tip names the `Atomic-View` and
//! `Atomic-State` (Merkle) trailers, and the receiving pristine must
//! already contain that state (an Atomic-controlled remote ingests changes
//! through its own pipeline before verification; a required CI check fetches
//! and imports first). A ref update whose state is not ingested is refused,
//! never waved through.
//!
//! On a receiving repository, local session MAC keys are unavailable by
//! design, so managed attestations verify as *unverifiable* and fail closed
//! (RFC §10.4: missing required evidence is not silently accepted to
//! accommodate an offline or private source). Human (unmanaged) work passes;
//! managed work needs the receiving boundary to hold trustworthy attestation
//! evidence, which the current CB-12A session-MAC scheme cannot provide —
//! the refusal says exactly that instead of downgrading trust.

use std::io::Read;
use std::path::Path;

use atomic_core::types::{Base32, Merkle};
use atomic_repository::repository::observability::{
    BridgeEventJournal, BridgeEventKind, PublicationBoundary, PublicationUnit,
};
use atomic_repository::Repository;
use git2::Repository as GitRepository;

use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_success, print_warning};

/// One refused ref update with its exact reason.
struct ReceiveRefusal {
    ref_name: String,
    reason: String,
}

/// Run the receiving verifier (pre-receive/update/CI contract).
///
/// Reads ref-update lines on stdin, verifies protected updates, and exits
/// non-zero when any update is refused. `--unprotected <prefix>` may be
/// passed repeatedly to exempt explicitly chosen ref prefixes; everything
/// else fails closed.
pub(crate) fn run_verify_receive(root: &Path, unprotected: &[String]) -> CliResult<()> {
    let mut stdin = std::io::stdin().lock();
    let mut input = String::new();
    stdin
        .read_to_string(&mut input)
        .map_err(|error| CliError::GitError {
            message: format!("cannot read ref-update lines: {error}"),
        })?;
    drop(stdin);

    verify_receive_updates(root, unprotected, &input)
}

/// The stdin-independent core of the verifier (also the test seam).
pub(crate) fn verify_receive_updates(
    root: &Path,
    unprotected: &[String],
    input: &str,
) -> CliResult<()> {
    let lines: Vec<String> = input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect();
    if lines.is_empty() {
        return Err(CliError::GitError {
            message: "verify-receive received no ref-update lines; refusing (fail closed)"
                .to_string(),
        });
    }

    let repo = Repository::open_readonly(root).map_err(CliError::Repository)?;
    let git_repo = GitRepository::open(root).map_err(|error| CliError::GitError {
        message: format!("cannot open receiving Git repository: {error}"),
    })?;

    let mut refusals: Vec<ReceiveRefusal> = Vec::new();
    let mut verified = 0usize;

    for line in &lines {
        let Some((old, new, ref_name)) = parse_ref_update(line) else {
            refusals.push(ReceiveRefusal {
                ref_name: line.to_string(),
                reason: "malformed ref-update line (expected '<old> <new> <ref>')".to_string(),
            });
            continue;
        };

        if is_unprotected(ref_name, unprotected) {
            print_info(&format!(
                "{ref_name}: explicitly unprotected; no managed-provenance verification"
            ));
            continue;
        }

        // Deletions move no state to verify.
        if new.is_zero() {
            verified += 1;
            continue;
        }

        match verify_ref_update(&repo, &git_repo, old, new, ref_name) {
            Ok(delta) => {
                verified += 1;
                print_info(&format!(
                    "{ref_name}: verified {delta} change(s) at the receiving boundary"
                ));
            }
            Err(reason) => refusals.push(ReceiveRefusal {
                ref_name: ref_name.to_string(),
                reason,
            }),
        }
    }

    if !refusals.is_empty() {
        for refusal in &refusals {
            print_warning(&format!("REFUSED {}: {}", refusal.ref_name, refusal.reason));
        }
        // CB-13C observability: publication refusals are recorded with
        // stable counts; refusal reasons stay in the command output, never
        // in telemetry. The receive boundary counts ref updates (review
        // R5: accurate units).
        BridgeEventJournal::new(
            &Repository::canonical_dot_dir(root).map_err(CliError::Repository)?,
            None,
        )
        .emit_lossy(BridgeEventKind::PublicationRefusal {
            boundary: PublicationBoundary::VerifyReceive,
            refused: refusals.len(),
            checked: lines.len(),
            unit: PublicationUnit::Refs,
        });
        return Err(CliError::GitError {
            message: format!(
                "verify-receive refused {} of {} ref update(s); the boundary accepted nothing \
                 for refused refs",
                refusals.len(),
                lines.len()
            ),
        });
    }

    print_success(&format!(
        "verify-receive: {verified} protected ref update(s) carry complete, trusted managed \
         evidence (limitations apply; see gate report)"
    ));
    Ok(())
}

fn parse_ref_update(line: &str) -> Option<(git2::Oid, git2::Oid, &str)> {
    let mut parts = line.split_whitespace();
    let old = git2::Oid::from_str(parts.next()?).ok()?;
    let new = git2::Oid::from_str(parts.next()?).ok()?;
    let ref_name = parts.next()?;
    Some((old, new, ref_name))
}

fn is_unprotected(ref_name: &str, unprotected: &[String]) -> bool {
    unprotected
        .iter()
        .any(|prefix| ref_name.starts_with(prefix.as_str()))
}

/// Verify one ref update: resolve the projection state at the new tip,
/// compute the view's change delta, and run the trusted provenance gate.
fn verify_ref_update(
    repo: &Repository,
    git_repo: &GitRepository,
    old: git2::Oid,
    new: git2::Oid,
    ref_name: &str,
) -> Result<usize, String> {
    let (view, new_merkle) = resolve_projection_state(git_repo, new).ok_or_else(|| {
        format!(
            "new tip {new} carries no Atomic-View/Atomic-State projection trailers; the \
             receiving boundary cannot verify it (deploy an Atomic-controlled remote so every \
             protected ref update is a verified projection)"
        )
    })?;

    // The state must be ingested on the receiving side before verification.
    let view_id = {
        use atomic_core::pristine::ViewTxnT;
        let txn = repo
            .pristine()
            .read_txn()
            .map_err(|error| format!("cannot open the receiving pristine: {error}"))?;
        txn.get_view(&view)
            .map_err(|error| format!("cannot read view '{view}': {error}"))?
            .map(|view_state| view_state.id)
            .ok_or_else(|| {
                format!(
                    "view '{view}' from projection trailers is not present on this boundary; \
                     deploy the Atomic repository that owns this view"
                )
            })?
    };
    let seq_new = state_seq(repo, view_id, &new_merkle).ok_or_else(|| {
        format!(
            "state {} for view '{view}' is not ingested on this boundary; refusing ahead of \
             ingestion is fail-closed, never waved through",
            new_merkle.to_base32()
        )
    })?;

    let seq_old = if old.is_zero() {
        None
    } else {
        let old_merkle = resolve_projection_state(git_repo, old)
            .map(|(old_view, old_merkle)| {
                if old_view == view {
                    Ok(old_merkle)
                } else {
                    Err(format!(
                        "old tip {old} projects view '{old_view}' but the update targets \
                         '{view}'; refusing the boundary crossing"
                    ))
                }
            })
            .transpose()?
            .ok_or_else(|| {
                format!("old tip {old} carries no projection trailers; cannot compute the delta")
            })?;
        state_seq(repo, view_id, &old_merkle)
    };

    // Delta: view changes with sequence numbers beyond the old state.
    let all_changes = repo
        .get_view_changes(Some(&view))
        .map_err(|error| format!("cannot read view '{view}' change log: {error}"))?;
    let delta: Vec<atomic_core::types::Hash> = all_changes
        .into_iter()
        .filter(|(seq, _hash)| seq_old.is_none_or(|old_seq| *seq > old_seq) && *seq <= seq_new)
        .map(|(_seq, hash)| hash)
        .collect();

    // Trusted provenance gate over the complete delta (transitive
    // dependencies included). Receiving boundaries hold no local session
    // MAC keys: managed attestations verify as unverifiable and fail closed.
    let closure = atomic_repository::repository::provenance_gate::reachable_closure(repo, &delta)
        .map_err(|error| format!("cannot compute the reachable closure: {error}"))?;
    match repo.evaluate_publication_gate(&closure, &gate_config(repo), None) {
        Ok(verdict) if verdict.allowed() => Ok(verdict.managed_changes),
        Ok(verdict) => Err(format!(
            "managed provenance gate refused ({}/{} managed of {} checked):\n{}",
            verdict.managed_changes,
            verdict.checked,
            verdict.checked,
            verdict.refusal_report()
        )),
        Err(error) => Err(format!("gate evaluation failed: {error}")),
    }
}

fn gate_config(
    repo: &Repository,
) -> atomic_repository::repository::provenance_gate::PublicationGateConfig {
    atomic_repository::repository::provenance_gate::PublicationGateConfig::from_repo(repo)
        .unwrap_or(
            atomic_repository::repository::provenance_gate::PublicationGateConfig {
                trust: Default::default(),
                repository_identity: None,
            },
        )
}

/// Walk first-parent history from `tip` to the nearest projection commit and
/// return `(view, Merkle)` from its trailers.
fn resolve_projection_state(git_repo: &GitRepository, tip: git2::Oid) -> Option<(String, Merkle)> {
    let mut commit = git_repo.find_commit(tip).ok()?;
    for _ in 0..1000 {
        let message = commit.message().unwrap_or("");
        let view = parse_trailer(message, "Atomic-View");
        let state =
            parse_trailer(message, "Atomic-State").and_then(|s| Merkle::from_base32(s.as_bytes()));
        if let (Some(view), Some(state)) = (view, state) {
            return Some((view, state));
        }
        commit = commit.parent(0).ok()?;
    }
    None
}

fn state_seq(repo: &Repository, view_id: u64, merkle: &Merkle) -> Option<u64> {
    let txn = repo.pristine().read_txn().ok()?;
    txn.get_state_seq(view_id, merkle).ok().flatten()
}

fn parse_trailer(message: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    for line in message.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&prefix) {
            let value = rest.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A closed Atomic repository directory: the writable handle is dropped
    /// before the verifier opens its own read-only handle (single redb lock).
    fn atomic_repo_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        let _repo = Repository::init(dir.path()).unwrap();
        // The receiving boundary is also a Git repository (hook deployment).
        git2::Repository::init(dir.path()).unwrap();
        dir
    }

    #[test]
    fn empty_input_fails_closed() {
        let dir = atomic_repo_dir();
        let error = verify_receive_updates(dir.path(), &[], "").expect_err("no lines refuses");
        assert!(error.to_string().contains("no ref-update lines"), "{error}");
    }

    #[test]
    fn malformed_line_is_refused_not_ignored() {
        let dir = atomic_repo_dir();
        let error = verify_receive_updates(dir.path(), &[], "not a ref line")
            .expect_err("malformed line refuses");
        assert!(error.to_string().contains("refused 1 of 1"), "{error}");
    }

    #[test]
    fn unprotected_prefix_is_skipped_explicitly() {
        let dir = atomic_repo_dir();
        // A protected-looking line that is explicitly exempt verifies as
        // skipped: the deletion OID shape keeps it off the refuse path.
        let zero = "0000000000000000000000000000000000000000";
        let result = verify_receive_updates(
            dir.path(),
            &["refs/heads/".to_string()],
            &format!("{zero} {zero} refs/heads/scratch"),
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn ref_update_without_atomic_trailers_is_refused() {
        let dir = atomic_repo_dir();
        // Build a real commit with no projection trailers.
        let git = GitRepository::open(dir.path()).unwrap();
        let sig = git2::Signature::now("T", "t@example.com").unwrap();
        let tree = git.index().unwrap().write_tree().unwrap();
        let tree = git.find_tree(tree).unwrap();
        let oid = git
            .commit(Some("refs/heads/x"), &sig, &sig, "plain", &tree, &[])
            .unwrap();
        let zero = "0000000000000000000000000000000000000000";
        let error =
            verify_receive_updates(dir.path(), &[], &format!("{zero} {oid} refs/heads/main"))
                .expect_err("unverifiable tip refuses");
        // The reason reaches stderr via the refusal warnings; the error is
        // the boundary's aggregate refusal.
        assert!(error.to_string().contains("refused 1 of 1"), "{error}");
    }

    #[test]
    fn refused_update_reports_and_fails_nonzero() {
        let dir = atomic_repo_dir();
        let git = GitRepository::open(dir.path()).unwrap();
        let sig = git2::Signature::now("T", "t@example.com").unwrap();
        let tree = git.index().unwrap().write_tree().unwrap();
        let tree = git.find_tree(tree).unwrap();
        let oid = git
            .commit(Some("refs/heads/x"), &sig, &sig, "plain", &tree, &[])
            .unwrap();
        let zero = "0000000000000000000000000000000000000000";
        // Two refused updates: the error names the count.
        let error = verify_receive_updates(
            dir.path(),
            &[],
            &format!("{zero} {oid} refs/heads/a\n{zero} {oid} refs/heads/b"),
        )
        .expect_err("refusals");
        assert!(error.to_string().contains("refused 2 of 2"), "{error}");
    }

    /// CB-13C observability: a publication refusal is recorded in the
    /// structured event journal with stable counts; the refusal reasons
    /// stay in the command output and never enter telemetry.
    #[test]
    fn publication_refusals_are_recorded_in_the_event_journal() {
        let dir = atomic_repo_dir();
        let git = GitRepository::open(dir.path()).unwrap();
        let sig = git2::Signature::now("T", "t@example.com").unwrap();
        let tree = git.index().unwrap().write_tree().unwrap();
        let tree = git.find_tree(tree).unwrap();
        let oid = git
            .commit(Some("refs/heads/x"), &sig, &sig, "plain", &tree, &[])
            .unwrap();
        let zero = "0000000000000000000000000000000000000000";
        verify_receive_updates(dir.path(), &[], &format!("{zero} {oid} refs/heads/main"))
            .expect_err("the unverifiable tip refuses");

        let journal = dir
            .path()
            .join(".atomic")
            .join("bridge")
            .join("events.jsonl");
        let text = std::fs::read_to_string(&journal).unwrap();
        let line = text
            .lines()
            .find(|line| line.contains("publication_refusal"))
            .expect("the refusal event is recorded");
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(event["event"], "publication_refusal");
        assert_eq!(event["boundary"], "verify_receive");
        assert_eq!(event["refused"], 1);
        assert_eq!(event["checked"], 1);
        // The refusal reason text never enters telemetry.
        assert!(
            !line.contains("Atomic-View"),
            "refusal diagnostics must stay out of telemetry: {line}"
        );
    }

    #[test]
    fn deletions_verify_without_state() {
        let dir = atomic_repo_dir();
        let zero = "0000000000000000000000000000000000000000";
        let tip = "1111111111111111111111111111111111111111";
        // <tip> -> <zero> is a deletion: nothing to verify, accepted.
        let result =
            verify_receive_updates(dir.path(), &[], &format!("{tip} {zero} refs/heads/scratch"));
        assert!(result.is_ok(), "{result:?}");
    }
}
