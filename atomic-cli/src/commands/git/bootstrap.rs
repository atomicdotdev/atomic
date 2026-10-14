//! CB-10B: shared Git clone / adopt-Git bootstrap (RFC §7.3, §8.6, task 2).
//!
//! Both entry paths — `atomic clone <git-url>` and
//! `git clone` + `atomic init --adopt-git` — route through this one
//! binding-first bootstrap:
//!
//! 1. install the advisory hook integration (`bridge enable` machinery);
//! 2. configure the RFC §8.6 explicit fetch refspecs on `origin` and fetch
//!    the binding/view namespaces;
//! 3. **install verified fetched bindings** into the immutable local store:
//!    every fetched binding is revalidated (signature + live Git content)
//!    before it is stored — a hostile/malformed binding fails closed before
//!    installation, and nothing unverified becomes adoption evidence;
//! 4. resolve Git HEAD against the verified store
//!    ([`Repository::resolve_git_sha`]);
//! 5. a verified binding is **resurrected exactly** into the working copy's
//!    desired view (closure fetch prefers an Atomic remote, then the
//!    binding's bounded verified `changes.pack`), and the recomputed
//!    projection must equal the bound tree before anything is adopted
//!    (`ExactResurrection`'s projection proof);
//! 6. with an explicit signing key, the CB-7A anchor phase proves complete
//!    equivalence and creates the signed Anchor + verified checkpoint.
//!
//! Refusals are explicit and never silent: an unbound or unverifiable HEAD
//! is a typed refusal that names the supported foreign-synthesis path
//! (`atomic git import`) — never a false exact label and never a merge. The
//! unbound `--adopt-git` anchor stays gated on Phase 9 (CB-9B/9C).

use std::path::Path;
use std::process::Command;

use atomic_core::operation::{GitHashAlgorithm, GitObjectId};
use atomic_repository::git_binding::{
    binding_id_from_ref_name, GitStateBinding, BINDING_BLOB_NAME,
};
use atomic_repository::{GitShaResolution, Repository};

use git2::Repository as GitRepository;

use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_info, print_success, print_warning};

use super::binding::load_signing_key;
use super::bridge::open_git;

/// The outcome of a bound bootstrap attempt.
pub(crate) enum BootstrapOutcome {
    /// A verified binding was resurrected exactly; `anchored` reports the
    /// CB-7A anchor id when a signing key was supplied.
    Bound {
        binding_id: String,
        view: String,
        provenance_trusted: bool,
        anchored: Option<String>,
    },
    /// HEAD is unbound: the typed foreign-synthesis refusal. Nothing was
    /// adopted and no exact label was claimed.
    Unbound { head: String, reason: String },
}

/// Whether `url` selects the Git-transport clone path.
///
/// Explicit structural markers only: `.git` suffixed URLs, scp-like
/// `git@host:path` forms, `ssh://` URLs, or an existing local Git
/// repository (worktree or bare). A plain HTTPS URL that is not a Git
/// repository stays on the atomic-api path.
pub(crate) fn is_git_url(url: &str, _target: &Path) -> bool {
    let url = url.trim();
    if url.ends_with(".git") || url.starts_with("git@") || url.starts_with("ssh://") {
        return true;
    }
    // Local path: the SOURCE is the transport decision (CB-10B review R11) —
    // an existing local Git repository source takes the Git-transport
    // bootstrap regardless of whether the DESTINATION exists yet. The
    // former implementation checked the destination (which does not exist
    // for a fresh clone) and silently mis-routed local Git sources to the
    // Atomic-API path.
    if !url.contains("://") && !url.contains('@') {
        let source = Path::new(url);
        if source.exists() {
            return GitRepository::open(source).is_ok();
        }
    }
    false
}

/// Run `git clone <url> <path>` with inherited stdio (the user's auth
/// config, agents, and askpass prompts behave exactly like plain Git).
pub(crate) fn git_clone(url: &str, target: &Path) -> CliResult<()> {
    let status = Command::new("git")
        .args(["clone", url])
        .arg(target)
        .status()
        .map_err(|error| CliError::GitError {
            message: format!("cannot run git clone: {error}"),
        })?;
    if !status.success() {
        return Err(CliError::GitError {
            message: format!("git clone of '{url}' exited with {status}"),
        });
    }
    Ok(())
}

/// Fetch the RFC §8.6 binding/view namespaces from `remote` after the
/// refspecs are configured. Plain `git clone` transfers only
/// `refs/heads/*` (plus tags), so this explicit fetch is what carries the
/// binding refs into a fresh clone.
pub(crate) fn fetch_binding_refs(
    git: &GitRepository,
    remote: &str,
    include_degraded: bool,
) -> CliResult<()> {
    let workdir = git.workdir().ok_or_else(|| CliError::GitError {
        message: "Git repository has no working directory".to_string(),
    })?;
    let mut spec = vec![
        super::transport::BINDING_REFSPEC.to_string(),
        super::transport::VIEWS_REFSPEC.to_string(),
    ];
    // CB-10B review R9: the degraded namespace is consumed ONLY behind the
    // explicit degraded-reader opt-in, with the same fail-closed install
    // validation the primary namespace runs.
    if include_degraded {
        spec.push(format!(
            "+{}*:{}*",
            super::transport::DEGRADED_HEAD_BINDING_PREFIX,
            super::transport::DEGRADED_HEAD_BINDING_PREFIX
        ));
    }
    let status = Command::new("git")
        .args(["fetch", remote])
        .args(&spec)
        .current_dir(workdir)
        .status()
        .map_err(|error| CliError::GitError {
            message: format!("cannot run git fetch for binding refs: {error}"),
        })?;
    if !status.success() {
        return Err(CliError::GitError {
            message: format!(
                "fetching binding refs from '{remote}' failed (exit {status}); the clone \
                 cannot claim exact restoration without the binding namespace"
            ),
        });
    }
    Ok(())
}

/// Install every fetched binding ref into the immutable local store after
/// full revalidation. Returns the number of newly stored bindings.
///
/// Fail-closed contract (RFC §5.2, §12): a ref whose binding bytes are
/// undecodable, whose signature fails, or whose bound commit/tree OIDs do
/// not match the live Git ODB **aborts the whole installation** — no
/// partial install, no silent skip of a tampered binding.
pub(crate) fn install_fetched_bindings(repo: &Repository, git: &GitRepository) -> CliResult<usize> {
    let refs = atomic_repository::git_binding::transferable_binding_refs(git)
        .map_err(|error| CliError::GitError { message: error })?;
    install_binding_refs(repo, git, refs)
}

/// CB-10B review R9: the DEGRADED namespace (`refs/heads/atomic/bindings/*`)
/// installs through the SAME fail-closed validation as the primary
/// namespace — decode, canonical ref id, cryptography, live Git content,
/// exact immutable carrier — only behind the explicit degraded-reader
/// opt-in.
pub(crate) fn install_degraded_bindings(
    repo: &Repository,
    git: &GitRepository,
) -> CliResult<usize> {
    use atomic_repository::git_binding::DEGRADED_HEAD_BINDING_PREFIX;
    let mut names = Vec::new();
    for reference in git.references().map_err(|error| CliError::GitError {
        message: format!("cannot enumerate refs: {error}"),
    })? {
        let reference = reference.map_err(|error| CliError::GitError {
            message: format!("cannot read ref: {error}"),
        })?;
        if let Some(name) = reference.name() {
            if let Some(rest) = name.strip_prefix(DEGRADED_HEAD_BINDING_PREFIX) {
                let mut parts = rest.split('/');
                let shard = parts.next().unwrap_or("");
                let id_hex = parts.next().unwrap_or("");
                if parts.next().is_none() && shard.len() == 2 && id_hex.len() == 64 {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    install_binding_refs(repo, git, names)
}

/// The shared fail-closed binding install over an explicit ref-name list.
fn install_binding_refs(
    repo: &Repository,
    git: &GitRepository,
    refs: Vec<String>,
) -> CliResult<usize> {
    let mut installed = 0;
    for ref_name in refs {
        let target = git
            .find_reference(&ref_name)
            .map_err(|error| CliError::GitError {
                message: format!("fetched binding ref '{ref_name}' is unreadable: {error}"),
            })?
            .target()
            .ok_or_else(|| CliError::GitError {
                message: format!("fetched binding ref '{ref_name}' has no direct target"),
            })?;
        let binding =
            load_binding_at_ref(git, &ref_name, target).map_err(|error| CliError::GitError {
                message: format!(
                    "fetched binding on '{ref_name}' is refused before installation: {error}"
                ),
            })?;
        // The ref name must canonically claim the binding it carries: a
        // re-shaped ref is not a valid publication surface. The degraded
        // namespace ref name is verified against the DEGRADED layout by
        // `binding_id_from_ref_name` for the primary namespace; degraded
        // refs are re-shaped by construction, so the id is taken from the
        // name and cross-checked with the payload id.
        let id_from_ref =
            if ref_name.starts_with(atomic_repository::git_binding::DEGRADED_HEAD_BINDING_PREFIX) {
                atomic_repository::git_binding::binding_id_from_ref_name(&ref_name.replace(
                    atomic_repository::git_binding::DEGRADED_HEAD_BINDING_PREFIX,
                    atomic_repository::BINDING_REF_PREFIX,
                ))
            } else {
                atomic_repository::git_binding::binding_id_from_ref_name(&ref_name)
            }
            .ok_or_else(|| CliError::GitError {
                message: format!(
                    "fetched binding ref '{ref_name}' does not carry a binding id in a canonical \
                 <shard>/<id> layout and is refused before installation"
                ),
            })?;
        if id_from_ref != binding.id() {
            return Err(CliError::GitError {
                message: format!(
                    "fetched binding ref '{ref_name}' carries binding {} but its ref name \
                     claims {id_from_ref}; mismatched ref ids are refused before installation",
                    binding.id()
                ),
            });
        }
        // Cryptography (structure + signature) and live-Git content
        // verification before anything is stored: a tampered or malicious
        // binding never reaches the immutable store.
        atomic_repository::git_binding::verify_binding_cryptography(&binding).map_err(|error| {
            CliError::GitError {
                message: format!(
                    "fetched binding on '{ref_name}' fails signature/structure verification \
                     and is refused before installation: {error}"
                ),
            }
        })?;
        atomic_repository::git_binding::verify_binding_content(git, &binding).map_err(|error| {
            CliError::GitError {
                message: format!(
                    "fetched binding on '{ref_name}' does not match the Git object database \
                     and is refused before installation: {error}"
                ),
            }
        })?;
        // Exact immutable carrier (review R1, fetched-installation side): the
        // fetched ref must be the deterministic parentless publication commit
        // of the validated public payloads. A carrier with unexpected
        // parentage, extra tree entries, or tampered payloads never installs.
        let verified = repo
            .verify_published_binding_carrier(git, &binding.id())
            .map_err(|error| CliError::GitError {
                message: format!(
                    "fetched binding on '{ref_name}' is not an exact immutable published \
                     carrier and is refused before installation: {error}"
                ),
            })?
            .ok_or_else(|| CliError::GitError {
                message: format!("fetched binding ref '{ref_name}' vanished during validation"),
            })?;
        let verified_oid =
            git2::Oid::from_bytes(verified.carrier_oid.as_bytes()).map_err(|error| {
                CliError::GitError {
                    message: format!("fetched binding carrier oid is invalid: {error}"),
                }
            })?;
        if verified_oid != target {
            return Err(CliError::GitError {
                message: format!(
                    "fetched binding ref '{ref_name}' moved during validation and is refused \
                     before installation"
                ),
            });
        }
        match repo.store_binding(&binding).map_err(CliError::Repository)? {
            atomic_core::pristine::BindingStoreOutcome::Stored => installed += 1,
            atomic_core::pristine::BindingStoreOutcome::Idempotent => {}
        }
    }
    Ok(installed)
}

/// Decode the binding blob from a binding-commit tree (the reviewed
/// allowlisted blob; the tree confine check in the store reader applies at
/// restore time, and this loader reads only the canonical binding blob).
fn load_binding_at_ref(
    git: &GitRepository,
    ref_name: &str,
    target: git2::Oid,
) -> CliResult<GitStateBinding> {
    let commit = git
        .find_commit(target)
        .map_err(|error| CliError::GitError {
            message: format!("binding ref '{ref_name}' points at a missing commit: {error}"),
        })?;
    let tree = commit.tree().map_err(|error| CliError::GitError {
        message: format!("binding ref '{ref_name}' commit has no tree: {error}"),
    })?;
    let entry = tree
        .get_name(BINDING_BLOB_NAME)
        .ok_or_else(|| CliError::GitError {
            message: format!("binding ref '{ref_name}' tree does not contain {BINDING_BLOB_NAME}"),
        })?;
    if entry.filemode() != i32::from(git2::FileMode::Blob) {
        return Err(CliError::GitError {
            message: format!("binding ref '{ref_name}' entry is not a regular blob"),
        });
    }
    let blob = git
        .find_blob(entry.id())
        .map_err(|error| CliError::GitError {
            message: format!("binding ref '{ref_name}' blob is missing: {error}"),
        })?;
    GitStateBinding::decode(blob.content()).map_err(|error| CliError::GitError {
        message: format!("binding ref '{ref_name}' carries undecodable binding bytes: {error}"),
    })
}

/// The shared bound bootstrap for an existing Git checkout (RFC §7.3 via the
/// CB-7A anchor machinery). `key_file` signs the Anchor when supplied; the
/// bridge otherwise stays unanchored with the resurrection still journaled.
///
/// Refuses active Git sequences (merge/rebase in progress) and dirty
/// bootstraps rather than silently merging: the worktree must be clean
/// relative to HEAD, matching the §7.3 clean-adoption gate.
pub(crate) fn adopt_git_checkout(
    root: &Path,
    key_file: Option<&Path>,
    remote_name: &str,
    include_degraded: bool,
) -> CliResult<BootstrapOutcome> {
    let mut repo = Repository::open(root).map_err(CliError::Repository)?;
    let git = open_git(root)?;

    // Active Git sequences are Git-owned transactions: the bootstrap refuses
    // instead of modeling half-applied state (RFC §7.4).
    let head = git.head().map_err(|error| CliError::GitError {
        message: format!("cannot resolve Git HEAD: {error}"),
    })?;
    let head_oid = head.target().ok_or_else(|| CliError::GitError {
        message: "Git HEAD does not point directly at a commit".to_string(),
    })?;
    let head_hex = head_oid.to_string();

    // CB-10B review R4: the shared sequence/dirty preflight, with typed
    // evidence for each refusal — an active Git sequence (merge/rebase) and
    // a dirty worktree are refused BEFORE any fetch, install, resurrection
    // or anchoring runs. A partially dirty adoption would model half-applied
    // state the §7.3 clean-adoption gate never verified.
    if git.path().join("MERGE_HEAD").exists() {
        return Err(CliError::GitError {
            message: "refusing to bootstrap: a Git merge is in progress (MERGE_HEAD exists);                      complete or abort the merge, then re-run the bootstrap"
                .to_string(),
        });
    }
    for sequence_dir in ["rebase-merge", "rebase-apply"] {
        if git.path().join(sequence_dir).exists() {
            return Err(CliError::GitError {
                message: format!(
                    "refusing to bootstrap: a Git rebase is in progress ({sequence_dir} exists); \
                     complete or abort the rebase, then re-run the bootstrap"
                ),
            });
        }
    }
    {
        let mut statuses = git.statuses(None).map_err(|error| CliError::GitError {
            message: format!("cannot inspect the worktree: {error}"),
        })?;
        let dirty: Vec<String> = statuses
            .iter()
            .filter(|entry| {
                // Untracked files are the ordinary pre-bootstrap state for a
                // fresh clone checkout; STAGED or MODIFIED tracked state is
                // the partial adoption the gate refuses.
                !entry.status().is_wt_new()
            })
            .map(|entry| entry.path().unwrap_or("<unknown>").to_string())
            .collect();
        if !dirty.is_empty() {
            return Err(CliError::GitError {
                message: format!(
                    "refusing to bootstrap: the worktree has uncommitted tracked state ({}); \
                     commit or stash it, then re-run the bootstrap — a partial dirty adoption \
                     is never silently merged",
                    dirty.join(", ")
                ),
            });
        }
    }

    // 1. Hook integration (bridge enable machinery), refspecs, binding fetch.
    super::hooks::enable_bridge(root, false)?;
    let _ = super::transport::configure_fetch_refspecs(&git, remote_name);
    fetch_binding_refs(&git, remote_name, include_degraded)?;

    // 2. Install fetched bindings (verified, fail-closed).
    let mut installed = install_fetched_bindings(&repo, &git)?;
    if include_degraded {
        // CB-10B review R9: the degraded namespace's refs install through
        // the SAME fail-closed validation; they are read only behind the
        // explicit opt-in and never silently upgraded to exact labels.
        installed += install_degraded_bindings(&repo, &git)?;
    }
    if installed > 0 {
        print_info(&format!(
            "Installed {installed} verified binding(s) from the fetched binding refs."
        ));
    }

    // 3. Resolve Git HEAD against the verified store.
    let algorithm = match head_hex.len() {
        40 => GitHashAlgorithm::Sha1,
        64 => GitHashAlgorithm::Sha256,
        other => {
            return Err(CliError::GitError {
                message: format!("unsupported Git OID width {other}"),
            })
        }
    };
    let tagged = GitObjectId::new(algorithm, head_oid.as_bytes().to_vec()).map_err(|error| {
        CliError::GitError {
            message: error.to_string(),
        }
    })?;
    let resolution = repo
        .resolve_git_sha(&git, &tagged)
        .map_err(CliError::Repository)?;
    let binding = match resolution {
        GitShaResolution::VerifiedBinding { binding, .. } => binding,
        GitShaResolution::Cold => {
            return Ok(BootstrapOutcome::Unbound {
                head: head_hex,
                reason: "no verified binding covers this commit; the supported foreign-synthesis \
                     path is 'atomic git import --incremental' (run it explicitly) — the \
                     clone is NOT labeled exact and nothing was silently merged"
                    .to_string(),
            });
        }
        GitShaResolution::Unresolved { detail } => {
            return Ok(BootstrapOutcome::Unbound {
                head: head_hex,
                reason: format!(
                    "binding candidates failed verification ({detail}); the checkout is NOT \
                     labeled exact and nothing was adopted"
                ),
            });
        }
    };

    // 4. Exact resurrection into the working copy's desired view (RFC §5.2):
    //    closure fetch (Atomic remote preferred, bounded pack fallback),
    //    recomputed SetId/closure-root/tree agreement, then journaled
    //    membership restoration. Any gap is a typed refusal.
    let view = {
        let working_copy = repo
            .require_working_copy_id()
            .map_err(CliError::Repository)?;
        repo.desired_view_name(working_copy)
            .map_err(CliError::Repository)?
    };
    print_info(&format!(
        "Resurrecting binding {} ({} ordered change(s)) into view '{view}'",
        binding.id().to_hex(),
        binding.payload().ordered_changes.len(),
    ));
    // CB-10B review R9: the bootstrap CONSUMES its promised transport
    // sources — a configured Atomic remote is the preferred closure source
    // for the resurrection's bounded closure fetch, with the Git pack
    // fallback inherited underneath.
    let mut remote_source =
        super::transport::HttpBindingChangeSource::for_configured_remote(&repo, remote_name)
            .map_err(CliError::Repository)?;
    let outcome = repo
        .resurrect_binding_exact(
            &git,
            &binding,
            &view,
            remote_source
                .as_mut()
                .map(|source| source as &mut dyn atomic_repository::BindingChangeSource),
        )
        .map_err(CliError::Repository)?;
    if outcome.proof.projected_tree != outcome.proof.bound_tree {
        return Err(CliError::GitError {
            message: format!(
                "recomputed projection {} does not match the bound tree {} before adoption; \
                 refusing (no false exact label)",
                hex_encode(outcome.proof.projected_tree.as_bytes()),
                hex_encode(outcome.proof.bound_tree.as_bytes()),
            ),
        });
    }
    print_success(&format!(
        "Exact restoration verified: {} change(s) inserted, {} already present, Merkle {} SetId {}",
        outcome.inserted.len(),
        outcome.already_present.len(),
        outcome.identity.merkle,
        outcome.identity.set_id,
    ));
    if !outcome.provenance_trusted {
        print_warning(
            "Signer is not trusted under the configured policy: content was verified, \
             provenance/attestation roots stay UNTRUSTED (RFC §5.6).",
        );
    }

    // 5. Anchoring (CB-7A): with an explicit key, prove complete equivalence
    //    and create the signed Anchor + verified checkpoint. Without a key
    //    the bound state is restored but the bridge stays unanchored.
    let anchored = match key_file {
        Some(key_file) => {
            let signer = load_signing_key(key_file)?;
            match repo.enable_bridge_anchor(&git, &signer) {
                Ok(outcome) => {
                    print_success(&format!(
                        "Anchor binding {} created for view '{}' at Git HEAD {}",
                        outcome.binding.id().to_hex(),
                        outcome.view,
                        outcome.git_head,
                    ));
                    Some(outcome.binding.id().to_hex())
                }
                Err(atomic_repository::BridgeAnchorError::Refusal(refusal)) => {
                    print_warning(&format!("bridge anchor not created: {refusal}"));
                    None
                }
                Err(error) => {
                    return Err(CliError::GitError {
                        message: format!("bridge anchoring failed: {error}"),
                    })
                }
            }
        }
        None => {
            print_hint("no --binding-key-file given; the bound state is restored but the bridge is not anchored");
            None
        }
    };

    Ok(BootstrapOutcome::Bound {
        binding_id: binding.id().to_hex(),
        view,
        provenance_trusted: outcome.provenance_trusted,
        anchored,
    })
}

/// `atomic clone <git-url>`: initialize Atomic inside an already-cloned
/// checkout and run the shared binding-first bootstrap in it.
///
/// The scaffold is repository-level only (no `.atomicignore` recording): the
/// cloned worktree must stay byte-identical to the bound Git tree so the
/// pre-adoption projection proof compares like with like. Local ignore
/// configuration is an ordinary post-bootstrap untracked path.
pub(crate) fn clone_git_url(
    target: &Path,
    view_name: &str,
    key_file: Option<&Path>,
    include_degraded: bool,
) -> CliResult<BootstrapOutcome> {
    let mut repo = Repository::init(target).map_err(CliError::Repository)?;
    let working_copy = repo
        .require_working_copy_id()
        .map_err(CliError::Repository)?;
    if view_name != atomic_repository::DEFAULT_VIEW {
        repo.create_view(view_name).map_err(CliError::Repository)?;
        repo.align_to_view(working_copy, view_name)
            .map_err(CliError::Repository)?;
    }
    drop(repo);
    adopt_git_checkout(target, key_file, "origin", include_degraded)
}

/// Lowercase hex encoding for error messages.
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
