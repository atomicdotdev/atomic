//! CB-10B: binding transport integration (RFC §8.6, §8.5, tracker CB-10B).
//!
//! Three responsibilities, all bounded and explicit:
//!
//! 1. **Fetch refspecs on enable** ([`configure_fetch_refspecs`]): the RFC
//!    §8.6 enable step adds the explicit `refs/atomic/bindings/*` and
//!    `refs/atomic/views/*` fetch refspecs to a named remote. Idempotent:
//!    existing identical refspecs are left alone and never duplicated.
//!
//! 2. **Bounded create-only publication queue** ([`BindingPublishQueue`]):
//!    stored bindings whose create-only ref is absent are published through
//!    the journaled CAS machinery (CB-6A), bounded per run. With a remote,
//!    the binding namespace transfers with create-only
//!    `--force-with-lease=<ref>:` (an empty expected value requires the ref
//!    to not exist), and publication is only reported after a `git
//!    ls-remote` verification that the remote actually holds every expected
//!    ref at its exact target. An interrupted run is retried idempotently.
//!
//! 3. **Remote expected-old leases** ([`remote_lease_refspec`]): mapped-ref
//!    remote updates push against `last_observed_remote` with
//!    `--force-with-lease=<ref>:<expected>` so a stale push refuses without
//!    overwriting newer external work (RFC §8.5).
//!
//! No nested pushes are ever started from hooks: the queue runs only from
//! the explicit `git bridge binding publish --queue` entry, and every
//! transferred ref is filtered through the reviewed transport allowlist
//! ([`is_transferable_ref`]) so WIP recovery refs and snapshots are
//! structurally excluded. The namespace allowlist is only the ref NAME
//! boundary: before any network write, every published carrier is validated
//! as the exact immutable parentless publication commit with an allowlisted
//! tree and verified public payloads (review R1), and the transfer pushes
//! verified object ids rather than mutable ref names.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use git2::Repository as GitRepository;

use atomic_core::types::WorkingCopyId;
use atomic_repository::git_binding::{
    binding_id_from_ref_name, is_transferable_ref, namespace_rejection_diagnostic,
    transferable_binding_refs,
};
pub(crate) use atomic_repository::git_binding::DEGRADED_HEAD_BINDING_PREFIX;
use atomic_core::types::Base32;
use atomic_repository::BindingChangeSource;
use atomic_objects::{ObjectFamily, ObjectRecord};
use crate::output::print_warning;
use atomic_repository::{BindingPublication, Repository};

use crate::error::{CliError, CliResult};

/// The RFC §8.6 fetch refspec for the create-only binding namespace.
pub const BINDING_REFSPEC: &str = "+refs/atomic/bindings/*:refs/atomic/bindings/*";
/// The RFC §8.6 fetch refspec for draft-view projection refs.
pub const VIEWS_REFSPEC: &str = "+refs/atomic/views/*:refs/atomic/views/*";

/// Default per-run bound on the publication queue.
pub const QUEUE_MAX_DEFAULT: usize = 64;

// ── 1. Fetch refspecs on enable ──────────────────────────────────────────

/// Add the RFC §8.6 explicit fetch refspecs for `remote` to the repository
/// config, idempotently. Returns how many refspecs were newly added.
///
/// A missing remote is a typed error: refspecs are only meaningful for a
/// configured remote, and guessing one would hide a typo from the operator.
pub fn configure_fetch_refspecs(git: &GitRepository, remote: &str) -> CliResult<usize> {
    // Fail closed when the remote does not exist.
    git.find_remote(remote)
        .map_err(|error| {
            CliError::GitError {
                message: format!(
                    "cannot configure Atomic fetch refspecs: remote '{remote}' does not exist ({error})"
                ),
            }
        })?;
    let mut config = git
        .config()
        .map_err(|error| CliError::GitError { message: format!("cannot read Git config: {error}") })?;
    let existing: Vec<String> = config
        .entries(Some(&format!("remote.{remote}.fetch")))
        .and_then(|mut entries| {
            let mut out = Vec::new();
            while let Some(entry) = entries.next() {
                let entry = entry?;
                if let Some(value) = entry.value() {
                    out.push(value.to_string());
                }
            }
            Ok(out)
        })
        .map_err(|error| CliError::GitError { message: format!("cannot read remote refspecs: {error}") })?;
    let mut added = 0;
    for refspec in [BINDING_REFSPEC, VIEWS_REFSPEC] {
        if existing.iter().any(|line| line == refspec) {
            continue;
        }
        // git2 has no add-multivar; append via a dedicated entries set is
        // unsupported, so use the git CLI's --add, which is exactly the RFC
        // §8.6 command and never rewrites existing values.
        let status = Command::new("git")
            .args(["config", "--add", &format!("remote.{remote}.fetch"), refspec])
            .current_dir(git.workdir().ok_or_else(|| CliError::GitError {
                message: "Git repository has no working directory".to_string(),
            })?)
            .status()
            .map_err(|error| CliError::GitError { message: format!("cannot run git config: {error}") })?;
        if !status.success() {
            return Err(CliError::GitError {
                message: format!("git config --add remote.{remote}.fetch {refspec} failed"),
            });
        }
        added += 1;
    }
    Ok(added)
}

// ── 2. Bounded create-only publication queue ─────────────────────────────

/// One queue item outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueItemOutcome {
    /// The local create-only ref was created this run.
    Published,
    /// The ref already held exactly this binding's commit.
    AlreadyPresent,
    /// No stored binding exists for this id (hostile local row; skipped).
    Unresolvable,
}

/// What one bounded queue run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct QueueOutcome {
    /// Locally created create-only refs.
    pub published: usize,
    /// Refs that already held the identical binding commit.
    pub already_present: usize,
    /// Stored bindings that failed to load/decode (never published).
    pub unresolvable: usize,
    /// The binding refs the run considered, in deterministic order.
    pub refs: Vec<String>,
}

impl QueueOutcome {
    /// Whether every considered binding ended up published or idempotent.
    pub fn all_local_ok(&self) -> bool {
        self.unresolvable == 0
    }
}

/// Enumerate the stored bindings still missing a local create-only ref and
/// publish up to `max` of them in deterministic (id-hex) order.
///
/// Each publication reuses [`Repository::publish_binding`] — the journaled
/// CAS path with its immutable receipt — so an interrupted run replays
/// idempotently: already-created refs are observed identical and skipped.
/// A ref that exists with a *different* binding is a create-only violation
/// and aborts the run without overwriting it.
pub fn run_local_publish_queue(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
    max: usize,
) -> CliResult<QueueOutcome> {
    let mut outcome = QueueOutcome::default();
    let mut ids: Vec<atomic_repository::git_binding::BindingId> =
        repo.binding_ids().map_err(CliError::Repository)?;
    ids.sort_by_key(|id| id.to_hex());
    for id in ids {
        if outcome.published >= max {
            break;
        }
        let Some(binding) = repo.load_binding(&id).map_err(CliError::Repository)? else {
            outcome.unresolvable += 1;
            outcome
                .refs
                .push(Repository::binding_ref_name(&id));
            continue;
        };
        let ref_name = Repository::binding_ref_name(&id);
        outcome.refs.push(ref_name.clone());
        // Create-only invariant: an existing ref may only already hold THIS
        // binding, and only when its full carrier validates — the exact
        // immutable parentless commit, an allowlisted tree, and valid public
        // payloads. A ref carrying any other binding, a hostile carrier, or
        // undecodable bytes would leak every reachable object on transfer:
        // refused, never overwritten, never transferred.
        match repo.verify_published_binding_carrier(git, &id) {
            Ok(Some(verified)) => {
                if verified.binding.encode() != binding.encode() {
                    return Err(CliError::GitError {
                        message: format!(
                            "binding ref '{ref_name}' publishes binding {} with bytes differing \
                             from the stored binding; refusing to treat it as present or to \
                             transfer it",
                            id.to_hex()
                        ),
                    });
                }
                outcome.already_present += 1;
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                return Err(CliError::GitError {
                    message: format!(
                        "binding ref '{ref_name}' exists but is not a verifiable carrier of \
                         stored binding {} (create-only violation, refusing to transfer or \
                         overwrite): {error}",
                        id.to_hex()
                    ),
                });
            }
        }
        match repo
            .publish_binding(working_copy, git, &binding, None)
            .map_err(CliError::Repository)?
        {
            BindingPublication::Published { .. } => outcome.published += 1,
            BindingPublication::Idempotent { .. } => outcome.already_present += 1,
        }
    }
    Ok(outcome)
}

/// Transfer the create-only binding namespace to `remote` and verify the
/// remote actually holds every expected ref at its exact target before
/// reporting publication.
///
/// Pre-transfer validation (RFC §8.6 privacy boundary; CB-10B review R1):
/// only refs backed by a stored binding transfer, each ref name must be the
/// canonical `refs/atomic/bindings/<shard>/<id>` layout for its binding, and
/// each carrier must pass the full exact-carrier validation (parentless,
/// allowlisted tree, canonical signed payload, deterministic republication).
/// Any unregistered ref, mismatched id, hostile carrier, or invalid payload
/// refuses the whole transfer — a valid-looking namespace is not a privacy
/// boundary for Git object traversal.
///
/// The push is create-only: `--force-with-lease=<ref>:` (empty expected
/// value) requires each remote ref to not exist, so a remote that already
/// carries a binding ref (or any external value) is never overwritten — the
/// push refuses instead. Each push sources the VERIFIED carrier object id,
/// never the mutable local ref name, so a concurrent local ref move cannot
/// change what travels. After the transfer, `git ls-remote` re-reads the
/// remote and every expected ref must be present with the exact target; any
/// gap is a typed refusal and **no publication is reported** (RFC §8.6,
/// §12.11). WIP refs and snapshots are excluded by the transport allowlist.
pub fn transfer_binding_refs(
    repo: &Repository,
    repo_root: &Path,
    git: &GitRepository,
    remote: &str,
    degraded_fallback: bool,
) -> CliResult<Vec<String>> {
    // The advertised-ref surface: exactly the reviewed binding namespace.
    let refs = transferable_binding_refs(git)
        .map_err(|error| CliError::GitError { message: error })?;
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let workdir = git
        .workdir()
        .ok_or_else(|| CliError::GitError { message: "Git repository has no working directory".to_string() })?
        .to_path_buf();

    let stored_ids: BTreeSet<String> = repo
        .binding_ids()
        .map_err(CliError::Repository)?
        .into_iter()
        .map(|id| id.to_hex())
        .collect();

    // CB-10B review R8: per-ref transfer evidence. The journal is
    // consent-gated (the bridge opt-in) and lossy; every transferred ref
    // gets an event after its exact-target verification.
    let journal = atomic_repository::repository::observability::BridgeEventJournal::for_repository(repo);
    let transfer_event = |binding_id: &atomic_repository::git_binding::BindingId,
                          outcome: atomic_repository::repository::observability::EventOutcome,
                          destination: Option<String>| {
        journal.emit_lossy(atomic_repository::repository::observability::BridgeEventKind::BindingTransfer {
            binding: atomic_repository::repository::observability::HexBindingId::new(&binding_id.to_hex()).expect("validated binding id hex"),
            outcome,
            destination,
        });
    };

    let mut spec: Vec<(String, String)> = Vec::new();
    for name in &refs {
        // Unregistered refs never transfer: the namespace allowlist decides
        // the SHAPE of the surface, the stored-binding set decides its
        // MEMBERSHIP, and the carrier validation decides its CONTENT.
        let Some(id) = binding_id_from_ref_name(name) else {
            return Err(CliError::GitError {
                message: format!(
                    "binding ref '{name}' does not carry its binding id in the canonical \
                     refs/atomic/bindings/<shard>/<id> layout; unregistered refs never transfer"
                ),
            });
        };
        let canonical = Repository::binding_ref_name(&id);
        if name != &canonical {
            return Err(CliError::GitError {
                message: format!(
                    "binding ref '{name}' is not the canonical ref for binding {} \
                     ('{canonical}'); re-shaped binding refs never transfer",
                    id.to_hex()
                ),
            });
        }
        if !stored_ids.contains(&id.to_hex()) {
            return Err(CliError::GitError {
                message: format!(
                    "binding ref '{name}' is not backed by a stored binding; unregistered \
                     binding refs never transfer because their reachable object surface was \
                     never validated"
                ),
            });
        }
        let stored_binding = repo
            .load_binding(&id)
            .map_err(CliError::Repository)?
            .ok_or_else(|| CliError::GitError {
                message: format!(
                    "stored binding {} disappeared between enumeration and transfer",
                    id.to_hex()
                ),
            })?;
        let verified = repo
            .verify_published_binding_carrier(git, &id)
            .map_err(CliError::Repository)?
            .ok_or_else(|| CliError::GitError {
                message: format!("binding ref '{name}' vanished during carrier validation"),
            })?;
        if verified.binding.encode() != stored_binding.encode() {
            return Err(CliError::GitError {
                message: format!(
                    "binding ref '{name}' publishes binding {} with bytes differing from the \
                     stored binding; the published bytes never transfer",
                    id.to_hex()
                ),
            });
        }
        let oid = git2::Oid::from_bytes(verified.carrier_oid.as_bytes())
            .map_err(|error| CliError::GitError {
                message: format!("binding carrier oid is invalid: {error}"),
            })?;
        spec.push((name.clone(), oid.to_string()));
    }

    let mut args: Vec<String> = vec!["push".to_string(), remote.to_string()];
    for (name, oid) in &spec {
        // Create-only: empty expected value requires the remote ref to not
        // already exist. Never `--force`: an existing remote ref must refuse.
        // Source is the verified object id, not the mutable ref name.
        args.push(format!("--force-with-lease={name}:"));
        args.push(format!("{oid}:{name}"));
    }
    let status = Command::new("git")
        .args(&args)
        .current_dir(&workdir)
        .status()
        .map_err(|error| CliError::GitError { message: format!("cannot run git push: {error}") })?;
    if !status.success() {
        // Namespace rejection: surface the explicit diagnostic. It never
        // retries through hidden branches and never starts a nested push.
        let detail = format!(
            "git push of the binding namespace to '{remote}' failed; hosts that reject \
             custom namespaces must not be silently degraded"
        );
        for (name, _) in &spec {
            if let Some(id) = binding_id_from_ref_name(name) {
                transfer_event(
                    &id,
                    atomic_repository::repository::observability::EventOutcome::Refused,
                    Some(name.clone()),
                );
            }
        }
        let diagnostic = namespace_rejection_diagnostic(remote, &spec[0].0, &detail);
        eprintln!("binding transport refused: {}", diagnostic.detail);
        eprintln!("  - {}", diagnostic.remediation[0]);
        eprintln!("  - {}", diagnostic.remediation[1]);
        if degraded_fallback {
            return publish_degraded_head_fallback(repo, repo_root, remote, &spec, &journal);
        }
        return Err(CliError::GitError { message: detail });
    }

    // Publication is reported only after remote verification (RFC §8.6);
    // each verified ref records its per-ref transfer evidence (review R8).
    let verified = verify_remote_binding_refs(&workdir, remote, &spec)?;
    for (name, _) in &spec {
        if let Some(id) = binding_id_from_ref_name(name) {
            transfer_event(
                &id,
                atomic_repository::repository::observability::EventOutcome::Applied,
                Some(name.clone()),
            );
        }
    }
    Ok(verified)
}

/// The explicit, UI-visible degraded fallback (RFC §8.6): mirror the
/// create-only binding refs into `refs/heads/atomic/bindings/<shard>/<id>`.
/// NOT semantically equivalent — the output says so, the refs are
/// create-only (an existing degraded ref with a different target refuses),
/// and the fallback is only ever entered after an explicit operator flag.
fn publish_degraded_head_fallback(
    repo: &Repository,
    repo_root: &Path,
    remote: &str,
    spec: &[(String, String)],
    journal: &atomic_repository::repository::observability::BridgeEventJournal,
) -> CliResult<Vec<String>> {
    let mut pushed: Vec<(String, String)> = Vec::new();
    for (name, oid) in spec {
        if !is_transferable_ref(name) {
            continue;
        }
        let shard_id = name.strip_prefix("refs/atomic/bindings/").unwrap_or(name);
        let degraded = format!("{DEGRADED_HEAD_BINDING_PREFIX}{shard_id}");
        // Create-only on the degraded branch namespace too: the empty
        // expected value requires the remote ref to not already exist.
        let status = Command::new("git")
            .args(["push", remote])
            .arg(format!("--force-with-lease={degraded}:"))
            .arg(format!("{oid}:{degraded}"))
            .current_dir(repo_root)
            .status()
            .map_err(|error| {
                CliError::GitError { message: format!("cannot run degraded fallback push: {error}") }
            })?;
        if status.success() {
            pushed.push((degraded, oid.clone()));
        } else {
            // R7: a failed degraded push is recorded as refused — never a
            // silent success.
            if let Some(id) = binding_id_from_ref_name(name) {
                journal.emit_lossy(atomic_repository::repository::observability::BridgeEventKind::BindingTransfer {
                    binding: atomic_repository::repository::observability::HexBindingId::new(&id.to_hex()).expect("validated binding id hex"),
                    outcome: atomic_repository::repository::observability::EventOutcome::Refused,
                    destination: Some(degraded),
                });
            }
        }
    }
    // CB-10B review R7: the degraded fallback is EXACT-VERIFIED at its
    // target before any success is reported — a push that "succeeded" but
    // whose ref then disappeared (or landed at the wrong target) is a typed
    // failure, never a success.
    let workdir = repo
        .root()
        .to_path_buf();
    let verified = verify_remote_binding_refs_pattern(&workdir, remote, &pushed, &format!(
        "{}*",
        DEGRADED_HEAD_BINDING_PREFIX
    ))?;
    for name in &verified {
        if let Some(id) = binding_id_from_ref_name(name) {
            journal.emit_lossy(atomic_repository::repository::observability::BridgeEventKind::BindingTransfer {
                binding: atomic_repository::repository::observability::HexBindingId::new(&id.to_hex()).expect("validated binding id hex"),
                outcome: atomic_repository::repository::observability::EventOutcome::Applied,
                destination: Some(name.clone()),
            });
        }
    }
    println!(
        "DEGRADED publication: binding refs were mirrored to {DEGRADED_HEAD_BINDING_PREFIX}* \
         after an explicit opt-in. This namespace is NOT semantically equivalent to \
         refs/atomic/bindings/* and is visible to every Git client."
    );
    Ok(verified)
}

/// Verify the remote holds every expected ref at its exact target via
/// `git ls-remote`. Returns the verified (ref, oid) list; any missing or
/// mismatched ref is a typed refusal naming the gap.
fn verify_remote_binding_refs(
    workdir: &Path,
    remote: &str,
    expected: &[(String, String)],
) -> CliResult<Vec<String>> {
    verify_remote_binding_refs_pattern(workdir, remote, expected, "refs/atomic/bindings/*")
}

fn verify_remote_binding_refs_pattern(
    workdir: &Path,
    remote: &str,
    expected: &[(String, String)],
    pattern: &str,
) -> CliResult<Vec<String>> {
    let output = Command::new("git")
        .args(["ls-remote", remote, pattern])
        .current_dir(workdir)
        .output()
        .map_err(|error| CliError::GitError { message: format!("cannot run git ls-remote: {error}") })?;
    if !output.status.success() {
        return Err(CliError::GitError {
            message: format!(
                "remote verification of the binding namespace failed (ls-remote exited {}); \
                 publication is NOT reported",
                output.status
            ),
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut observed: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(oid), Some(name)) = (parts.next(), parts.next()) {
            observed.insert(name.to_string(), oid.to_string());
        }
    }
    let mut verified = Vec::new();
    let mut gaps = Vec::new();
    for (name, oid) in expected {
        match observed.get(name) {
            Some(actual) if actual == oid => verified.push(name.clone()),
            Some(actual) => gaps.push(format!(
                "ref '{name}' holds {actual} but the local binding commit is {oid}"
            )),
            None => gaps.push(format!("ref '{name}' is absent from the remote after transfer")),
        }
    }
    if !gaps.is_empty() {
        return Err(CliError::GitError {
            message: format!(
                "binding transfer verification FAILED — publication not reported: {}",
                gaps.join("; ")
            ),
        });
    }
    Ok(verified)
}

// ── 3. Remote expected-old leases (RFC §8.5) ─────────────────────────────

/// Observe the current tip of `remote_ref` on `remote` via `git ls-remote`.
///
/// `None` means the remote does not advertise the ref (create-only push
/// territory); a transport failure is a typed error so the caller never
/// mistakes an unreachable remote for an empty one.
pub fn observe_remote_ref(workdir: &Path, remote: &str, remote_ref: &str) -> CliResult<Option<String>> {
    let output = Command::new("git")
        .args(["ls-remote", remote, remote_ref])
        .current_dir(workdir)
        .output()
        .map_err(|error| CliError::GitError { message: format!("cannot run git ls-remote: {error}") })?;
    if !output.status.success() {
        return Err(CliError::GitError {
            message: format!(
                "cannot observe remote '{remote}' ref '{remote_ref}' (ls-remote exited {})",
                output.status
            ),
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(oid), Some(name)) = (parts.next(), parts.next()) {
            if name == remote_ref {
                return Ok(Some(oid.to_string()));
            }
        }
    }
    Ok(None)
}

/// The `--force-with-lease=<ref>:<expected>` spec for one mapped remote
/// update, from the mapping's `last_observed_remote`.
///
/// `Some` expected oid → lease pinned to that exact value (a stale push
/// refuses without overwriting newer external work). `None` → no lease: the
/// caller must first observe the remote explicitly or push without force
/// (Git's own non-fast-forward refusal still applies).
pub fn remote_lease_refspec(last_observed_remote: Option<&str>, remote_ref: &str) -> Option<String> {
    let expected = last_observed_remote?.trim();
    if expected.is_empty() {
        return None;
    }
    Some(format!("--force-with-lease={remote_ref}:{expected}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_refspec_pins_the_expected_old_value() {
        let lease = remote_lease_refspec(Some("abc123"), "refs/heads/main");
        assert_eq!(lease.as_deref(), Some("--force-with-lease=refs/heads/main:abc123"));
    }

    #[test]
    fn lease_refspec_is_none_without_an_observation() {
        assert!(remote_lease_refspec(None, "refs/heads/main").is_none());
        assert!(remote_lease_refspec(Some("   "), "refs/heads/main").is_none());
    }

    #[test]
    fn create_only_refspecs_are_exact() {
        assert_eq!(BINDING_REFSPEC, "+refs/atomic/bindings/*:refs/atomic/bindings/*");
        assert_eq!(VIEWS_REFSPEC, "+refs/atomic/views/*:refs/atomic/views/*");
        assert_eq!(QUEUE_MAX_DEFAULT, 64);
    }
}

/// Atomic-remote closure source for the CLI (RFC §8.6 preferred transport).
/// The repository-layer fetch flow is synchronous; this adapter drives the
/// async HTTP client on a dedicated current-thread runtime per batch.
pub(crate) struct HttpBindingChangeSource {
    pub(crate) remote: atomic_remote::HttpRemote,
}

impl BindingChangeSource for HttpBindingChangeSource {
    fn fetch_changes(&mut self, wanted: &[atomic_core::Hash]) -> Result<Vec<ObjectRecord>, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("cannot start HTTP runtime: {error}"))?;
        let mut records = Vec::new();
        for hash in wanted {
            match runtime.block_on(self.remote.download_change(&hash.to_base32())) {
                Ok(bytes) => records.push(ObjectRecord::new(
                    ObjectFamily::Change,
                    hash.to_base32(),
                    bytes.to_vec(),
                )),
                Err(error) => {
                    print_warning(&format!(
                        "remote did not serve change {}: {error}",
                        hash.to_base32()
                    ));
                }
            }
        }
        if records.is_empty() && !wanted.is_empty() {
            return Err("the Atomic remote served none of the wanted changes".to_string());
        }
        Ok(records)
    }
}

impl HttpBindingChangeSource {
    /// Build the Atomic-remote closure source from a configured remote
    /// (CB-10B review R9: the bootstrap consumes its promised transport
    /// sources). `None` when the remote is absent or not an HTTP remote.
    pub(crate) fn for_configured_remote(
        repo: &Repository,
        remote_name: &str,
    ) -> Result<Option<Self>, atomic_repository::RepositoryError> {
        let config = repo.load_remotes()?;
        let Some(entry) = config.get(remote_name) else {
            return Ok(None);
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| atomic_repository::RepositoryError::InvalidOperation {
                message: format!("cannot start HTTP runtime: {error}"),
            })?;
        let remote = atomic_remote::HttpRemote::new(&entry.url)
            .map_err(|error| atomic_repository::RepositoryError::InvalidOperation {
                message: format!("cannot connect to the Atomic remote '{remote_name}': {error}"),
            })?;
        Ok(Some(Self { remote }))
    }
}
