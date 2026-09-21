//! Signed Git state binding commands (CB-6A, RFC §5.1/5.2/5.6/8.6).
//!
//! `atomic git bridge binding publish` binds the current Git HEAD to the
//! current Atomic view state and publishes the signed object to a
//! create-only `refs/atomic/bindings/<shard>/<id>` ref through the journaled
//! operation machinery.
//!
//! The signer is an explicit Ed25519 key file (hex secret key). Wiring the
//! binding signature into the default identity pipeline and publication
//! gates is CB-7A/12B; this unit exposes the verified primitives.

use std::path::{Path, PathBuf};

use atomic_core::operation::{GitHashAlgorithm, GitObjectId};
use atomic_core::pristine::ViewTxnT;
use atomic_core::types::{Base32, OperationId};
use atomic_core::Hash;
use atomic_objects::{ObjectFamily, ObjectRecord};
use atomic_repository::git_binding::{
    assemble_changes_pack, evaluate_binding_trust, verify_binding_content,
    verify_binding_cryptography, BindingPackLimits, BindingSigner, BindingVerificationInput,
    CausalOrigin, GitObjectFormat, GitOid, GitStateBinding, GitStateBindingPayload, LossNote,
};
use atomic_repository::{BindingPublication, ClosureReadiness, Repository};
use git2::Repository as GitRepository;

use super::transport::HttpBindingChangeSource;
use crate::commands::workspace_txn::{boundary_mode, enter_workspace};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_info, print_success, print_warning};

use clap::Subcommand;

#[derive(Subcommand, Debug, Clone)]
pub enum BindingCommand {
    /// Sign and publish a binding for the current Git HEAD and Atomic view.
    Publish {
        /// File holding the hex-encoded 32-byte Ed25519 secret key.
        #[arg(long)]
        key_file: PathBuf,
        /// Attach the bounded `changes.pack` fallback to the binding tree
        /// (RFC §8.6): the hash-verified V3 change files of the effective
        /// closure, for receivers that cannot reach an Atomic remote.
        /// Private sidecars and unhashed bodies never enter the pack.
        #[arg(long)]
        with_changes_pack: bool,
        /// Attach the complete conflict-object pack to the binding tree
        /// (RFC §8.3, CB-8B). Requires the current view to have persisted
        /// unresolved conflicts; the pack is the canonical conflict object
        /// and is identity-verified against the conflict snapshot's
        /// `atomic-conflict <hash>` header at restore time.
        #[arg(long)]
        with_conflicts_pack: bool,
    },
    /// Verify a published binding: signature, bound fields, and Git content.
    Verify {
        /// Binding id (64 hex characters) or an unambiguous prefix.
        id: String,
    },
    /// Fetch a published binding's change closure: Atomic remote first,
    /// bounded `changes.pack` fallback (RFC §8.6, CB-6B).
    ///
    /// Fetch caches verified change files only — it never advances view
    /// membership, refs, or working-copy state. Incompleteness (missing
    /// objects, shallow boundaries, unavailable fallback) is explicit.
    Fetch {
        /// Binding id (64 hex characters) or an unambiguous prefix.
        id: String,
        /// Atomic remote name or URL to prefer for change objects. Without
        /// it, only the binding's own changes.pack fallback is available.
        #[arg(long)]
        remote: Option<String>,
        /// Remote request timeout in seconds.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        /// Skip TLS certificate verification (testing only).
        #[arg(short = 'k', long)]
        insecure: bool,
    },
    /// Resurrect a published binding's exact original state into a view
    /// (RFC §5.2, CB-6C): closure fetch (remote first, pack fallback),
    /// isolated projection proof against the bound Git tree and SetId, then
    /// exact membership restoration in binding Merkle order.
    Resurrect {
        /// Binding id (64 hex characters) or an unambiguous prefix.
        id: String,
        /// Atomic remote name or URL to prefer for change objects.
        #[arg(long)]
        remote: Option<String>,
        /// Remote request timeout in seconds.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        /// Skip TLS certificate verification (testing only).
        #[arg(short = 'k', long)]
        insecure: bool,
        /// Target view (defaults to the working copy's desired view).
        #[arg(long)]
        to_view: Option<String>,
    },
    /// CB-10B bounded create-only publication queue (RFC §8.6, §8.5).
    ///
    /// Publishes every stored binding that still lacks its local
    /// create-only `refs/atomic/bindings/<shard>/<id>` ref, in deterministic
    /// order, through the journaled CAS machinery — bounded per run and
    /// idempotent on retry. With `--remote`, the binding namespace then
    /// transfers create-only (`--force-with-lease=<ref>:` requires each
    /// remote ref to not exist), and publication is reported only after
    /// `git ls-remote` verifies that the remote holds every ref at its exact
    /// target. Stale/external remote refs are never overwritten.
    Queue {
        /// Remote to transfer the binding namespace to (optional; local
        /// publication always runs).
        #[arg(long)]
        remote: Option<String>,
        /// Maximum number of bindings to publish locally per run.
        #[arg(long, default_value_t = super::transport::QUEUE_MAX_DEFAULT)]
        max: usize,
        /// Explicit opt-in to the DEGRADED `refs/heads/atomic/bindings/*`
        /// fallback when the host rejects the binding namespace. Never
        /// automatic and not semantically equivalent (RFC §8.6).
        #[arg(long)]
        degraded_head_fallback: bool,
    },
}

impl BindingCommand {
    pub fn run(self) -> CliResult<()> {
        match self {
            BindingCommand::Publish {
                key_file,
                with_changes_pack,
                with_conflicts_pack,
            } => publish(key_file, with_changes_pack, with_conflicts_pack),
            BindingCommand::Verify { id } => verify(&id),
            BindingCommand::Fetch {
                id,
                remote,
                timeout,
                insecure,
            } => fetch(&id, remote, timeout, insecure),
            BindingCommand::Resurrect {
                id,
                remote,
                timeout,
                insecure,
                to_view,
            } => resurrect(&id, remote, timeout, insecure, to_view),
            BindingCommand::Queue {
                remote,
                max,
                degraded_head_fallback,
            } => queue(remote.as_deref(), max, degraded_head_fallback),
        }
    }

    /// Run from a shared reference (clap passes subcommands by reference).
    pub fn run_owned(self) -> CliResult<()> {
        self.run()
    }
}

/// Load an explicit Ed25519 signing key file (64 hex characters).
///
/// CB-7A keeps binding/Anchor signatures on explicit key files only; no
/// default identity is wired (managed signing is CB-12B).
pub(crate) fn load_signing_key(key_file: &Path) -> CliResult<atomic_identity::keypair::KeyPair> {
    let text = std::fs::read_to_string(key_file).map_err(|error| {
        CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
            message: format!(
                "cannot read signing key file '{}': {error}",
                key_file.display()
            ),
        })
    })?;
    let hex = text.trim();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(CliError::Repository(
            atomic_repository::RepositoryError::InvalidOperation {
                message: "signing key file must hold exactly 64 hex characters (a 32-byte Ed25519 secret key)"
                    .to_string(),
            },
        ));
    }
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).map_err(|error| {
            CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
                message: format!("invalid hex in signing key file: {error}"),
            })
        })?;
    }
    let secret = atomic_identity::keypair::SecretKey::from_bytes(&secret);
    Ok(atomic_identity::keypair::KeyPair::from_secret_key(secret))
}

/// Assemble the bounded `changes.pack` blob for `ordered` from the
/// repository's own change store (RFC §8.6). Pack records are addressed
/// by blake3 of the exact stored file bytes; the assembler re-quarantines
/// every change (canonical decode + privacy) and enforces the budgets.
pub(crate) fn assemble_changes_pack_from_view(
    repo: &Repository,
    ordered: &[atomic_core::Hash],
) -> CliResult<Vec<u8>> {
    let mut records = Vec::with_capacity(ordered.len());
    for hash in ordered {
        if !repo.has_change(hash) {
            return Err(CliError::Repository(
                atomic_repository::RepositoryError::InvalidOperation {
                    message: format!(
                        "cannot assemble the changes.pack: change {} is not in the local store; \
                         the pack must carry the complete closure",
                        hash.to_base32()
                    ),
                },
            ));
        }
        let path = repo.change_store().change_path(hash);
        let bytes = std::fs::read(&path).map_err(|error| {
            CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
                message: format!("cannot read change {}: {error}", hash.to_base32()),
            })
        })?;
        // Re-confirm the stored bytes hash to the requested identity
        // before they may enter Git (hash-authoritative bytes).
        let mut probe = bytes.as_slice();
        let (_, identity) =
            atomic_core::change::Change::deserialize(&mut probe).map_err(|error| {
                CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
                    message: format!("stored change {} is corrupt: {error}", hash.to_base32()),
                })
            })?;
        if &identity != hash {
            return Err(CliError::Repository(
                atomic_repository::RepositoryError::InvalidOperation {
                    message: format!(
                        "stored change {} does not hash to its identity; refusing to pack it",
                        hash.to_base32()
                    ),
                },
            ));
        }
        records.push(ObjectRecord::new(
            ObjectFamily::Change,
            atomic_core::Hash::of(&bytes).to_hex(),
            bytes,
        ));
    }
    atomic_repository::git_binding::assemble_changes_pack(
        &records,
        &BindingPackLimits::default_limits(),
    )
    .map_err(|error| {
        CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
            message: format!("cannot assemble the changes.pack: {error}"),
        })
    })
}

fn publish(key_file: PathBuf, with_changes_pack: bool, with_conflicts_pack: bool) -> CliResult<()> {
    let keypair = load_signing_key(&key_file)?;
    let root = find_repository_root()?;
    let mut repo = Repository::open(root.clone()).map_err(CliError::from)?;
    let workspace = enter_workspace(&mut repo, atomic_repository::WorkspaceTxnMode::Reconcile)?;
    let working_copy = workspace.working_copy();
    let view_name = repo
        .desired_view_name(working_copy)
        .map_err(CliError::from)?;

    let git = open_git(&root)?;
    let head = git
        .head()
        .ok()
        .and_then(|head| head.target())
        .ok_or_else(|| {
            CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
                message: "binding publish requires an attached Git HEAD commit".to_string(),
            })
        })?;
    let commit = git
        .find_commit(head)
        .map_err(|error| git_map(error.to_string()))?;
    let raw_commit = git
        .odb()
        .map_err(|error| git_map(error.to_string()))?
        .read(head)
        .map_err(|error| git_map(error.to_string()))?
        .data()
        .to_vec();
    let object_format = match head.as_bytes().len() {
        20 => GitObjectFormat::Sha1,
        32 => GitObjectFormat::Sha256,
        other => return Err(git_map(format!("unexpected HEAD OID width {other}"))),
    };

    // Graph-derived identities: additive SetId v1, the Merkle state, and the
    // effective closure in view-log order. Snapshots and private material
    // never enter this payload.
    let identity = repo.view_identity(&view_name).map_err(CliError::from)?;
    let ordered_changes: Vec<atomic_core::Hash> = repo
        .effective_history(Some(&view_name))
        .map_err(CliError::from)?
        .iter()
        .map(|entry| entry.hash)
        .collect();
    let operation = sole_working_copy_operation(&repo, working_copy)?;

    let tree_hex = git
        .find_commit(head)
        .map_err(|error| git_map(error.to_string()))?
        .tree_id()
        .to_string();
    let mut payload = GitStateBindingPayload {
        version: atomic_repository::git_binding::BINDING_VERSION,
        git_object_format: object_format,
        git_commit: GitOid::from_hex(&head.to_string()).map_err(git_map)?,
        git_tree: GitOid::from_hex(&tree_hex).map_err(git_map)?,
        git_parents: Vec::new(),
        raw_commit_object: Some(raw_commit),
        set_id: identity.set_id,
        merkle_state: identity.merkle,
        view_hint: Some(view_name.clone()),
        ordered_changes,
        closure_root: atomic_core::Hash::from_bytes([0u8; 32]),
        operation,
        origin: CausalOrigin::ExactAtomicResurrection,
        // Reviewed loss facts (review CB-9C R7): explicitly tracked empty
        // directories project to no Git tree entry — the projection omits
        // them and the binding records the typed loss so it survives the
        // round trip. Git never receives a fabricated empty-directory entry.
        loss: binding_loss_notes(&repo, &view_name)?,
        provenance_roots: Vec::new(),
        attestation_roots: Vec::new(),
        signer: BindingSigner::for_keypair(&keypair),
    };
    payload.git_parents = parents_from(&git, &head)?;
    payload.closure_root = payload.compute_closure_root();

    // Idempotent retries: if an identical binding (same bound commit, state,
    // closure, and signer) is already stored, republish exactly those bytes
    // instead of signing a fresh object for the same fact.
    let binding = match find_stored_identical_binding(&repo, &payload, &keypair) {
        Some(existing) => existing,
        None => GitStateBinding::sign(payload, &keypair).map_err(|error| {
            CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
                message: format!("cannot sign the binding: {error}"),
            })
        })?,
    };
    let id = binding.id();

    let id = binding.id();

    // Optional bounded fallback pack (RFC §8.6): only hash-verified V3
    // change files of the effective closure, quarantined against private
    // sidecars, prompts, and unhashed bodies before anything enters Git.
    let changes_pack = if with_changes_pack {
        Some(assemble_changes_pack_from_view(
            &repo,
            binding.payload().ordered_changes.as_slice(),
        )?)
    } else {
        None
    };

    // Optional complete conflict-object pack (RFC §8.3, CB-8B): the
    // canonical conflict object of the CURRENT view's persisted unresolved
    // conflicts. Requires conflict state — a clean view has nothing to pack,
    // and markers plus a hash are never a substitute.
    let conflicts_pack = if with_conflicts_pack {
        let object = repo
            .capture_view_conflict_set(&view_name)
            .map_err(CliError::from)?
            .ok_or_else(|| {
                CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
                    message: format!(
                        "--with-conflicts-pack requires persisted unresolved conflicts on \
                         view '{view_name}'; there is no conflict state to pack"
                    ),
                })
            })?;
        let bytes = object.canonical_bytes().map_err(|error| {
            CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
                message: format!("cannot encode the conflict object: {error}"),
            })
        })?;
        Some(bytes)
    } else {
        None
    };

    // Publication journals the create-only ref effect before visibility.
    let publication = match (&changes_pack, &conflicts_pack) {
        (Some(pack), Some(conflicts)) => repo
            .publish_binding_with_changes_and_conflicts_pack(
                working_copy,
                &git,
                &binding,
                None,
                pack,
                conflicts,
            )
            .map_err(CliError::from)?,
        (Some(pack), None) => repo
            .publish_binding_with_changes_pack(working_copy, &git, &binding, None, pack)
            .map_err(CliError::from)?,
        (None, Some(conflicts)) => repo
            .publish_binding_with_conflicts_pack(working_copy, &git, &binding, None, conflicts)
            .map_err(CliError::from)?,
        (None, None) => repo
            .publish_binding(working_copy, &git, &binding, None)
            .map_err(CliError::from)?,
    };
    let binding_commit_hex = match &publication {
        BindingPublication::Published { binding_commit, .. }
        | BindingPublication::Idempotent { binding_commit } => hex_bytes(binding_commit.as_bytes()),
    };
    drop(workspace);
    match publication {
        BindingPublication::Published { .. } => print_success(&format!(
            "Published binding {id} at {prefix}{shard} (commit {binding_commit_hex})",
            prefix = atomic_repository::BINDING_REF_PREFIX,
            shard = &id.to_hex()[..2],
        )),
        BindingPublication::Idempotent { .. } => print_success(&format!(
            "Binding {id} already published; identical retry changed nothing"
        )),
    }
    Ok(())
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Find a stored binding that states the same fact: same bound commit, tree,
/// parents, state identities, closure, hint, origin, and signer. The stored
/// bytes are reused so identical retries are idempotent instead of minting a
/// second object per operation head.
fn find_stored_identical_binding(
    repo: &Repository,
    payload: &GitStateBindingPayload,
    keypair: &atomic_identity::keypair::KeyPair,
) -> Option<GitStateBinding> {
    let my_did = atomic_canonical::did::did_for_public_key(&keypair.public);
    for id in repo.binding_ids().ok()? {
        let binding = repo.load_binding(&id).ok()??;
        let stored = binding.payload();
        let same_fact = stored.git_object_format == payload.git_object_format
            && stored.git_commit == payload.git_commit
            && stored.git_tree == payload.git_tree
            && stored.git_parents == payload.git_parents
            && stored.set_id == payload.set_id
            && stored.merkle_state == payload.merkle_state
            && stored.view_hint == payload.view_hint
            && stored.ordered_changes == payload.ordered_changes
            && stored.closure_root == payload.closure_root
            && stored.origin == payload.origin
            && stored.loss == payload.loss
            && stored.signer.did == my_did
            && stored.signer.did == payload.signer.did;
        if same_fact {
            return Some(binding);
        }
    }
    None
}

fn parents_from(git: &GitRepository, head: &git2::Oid) -> CliResult<Vec<GitOid>> {
    let commit = git
        .find_commit(*head)
        .map_err(|error| git_map(error.to_string()))?;
    let mut parents = Vec::new();
    for parent in commit.parent_ids() {
        let parent: git2::Oid = parent;
        parents.push(GitOid::from_hex(&parent.to_string()).map_err(git_map)?);
    }
    Ok(parents)
}

/// The binding payload's reviewed loss facts (review CB-9C R7): the view's
/// explicit EmptyDirectory projection losses, mapped to the binding codec's
/// typed note.
fn binding_loss_notes(
    repo: &atomic_repository::Repository,
    view_name: &str,
) -> CliResult<Vec<LossNote>> {
    let notes = repo.empty_directory_loss_notes(view_name)?;
    Ok(notes
        .into_iter()
        .map(|note| match note {
            atomic_repository::record::LossNote::EmptyDirectory { path } => {
                LossNote::EmptyDirectory { path }
            }
            other => LossNote::Other {
                description: format!("{other:?}"),
            },
        })
        .collect())
}

fn sole_working_copy_operation(
    repo: &atomic_repository::Repository,
    working_copy: atomic_core::types::WorkingCopyId,
) -> CliResult<OperationId> {
    let log = repo
        .operation_log(
            atomic_core::operation::OperationScope::WorkingCopy(working_copy),
            Some(1),
            false,
        )
        .map_err(CliError::from)?;
    match log.head_state {
        atomic_repository::OperationHeadState::Single(head) => Ok(head),
        OperationHeadState::Empty => Err(CliError::Repository(
            atomic_repository::RepositoryError::InvalidOperation {
                message: "no causal operation exists to bind; record or project state first"
                    .to_string(),
            },
        )),
        OperationHeadState::Diverged(heads) => Err(CliError::Repository(
            atomic_repository::RepositoryError::InvalidOperation {
                message: format!(
                    "operation heads are diverged: {}",
                    heads
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
        )),
    }
}

fn verify(id: &str) -> CliResult<()> {
    let root = find_repository_root()?;
    let repo = Repository::open_readonly(root.clone()).map_err(CliError::from)?;
    let git = open_git(&root)?;

    let binding = if id.len() == 64 && id.chars().all(|c| c.is_ascii_hexdigit()) {
        resolve_by_hex(&repo, id)?
    } else {
        resolve_by_prefix(&repo, id)?
    };

    verify_binding_cryptography(&binding).map_err(|error| {
        CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
            message: format!("binding {} fails cryptography: {error}", binding.id()),
        })
    })?;
    verify_binding_content(&git, &binding).map_err(|error| {
        CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
            message: format!("binding {} does not match Git: {error}", binding.id()),
        })
    })?;

    // Trust policy: trust is distinct from content validity.
    let config =
        atomic_config::RepoConfig::load(&root.join(".atomic/config.toml")).map_err(|error| {
            CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
                message: error.to_string(),
            })
        })?;
    // Repository identity for the default trust root: the configured author
    // identity reference (a DID when one is configured).
    let repository_identity = config
        .author
        .as_ref()
        .and_then(|author| author.identity.as_deref());
    let content_verified = true;
    let evaluation = evaluate_binding_trust(
        &binding,
        &config.git.trust,
        BindingVerificationInput {
            repository_identity,
        },
        content_verified,
    );
    let verdict = if evaluation.can_satisfy_publication_gate() {
        "trusted"
    } else {
        "UNTRUSTED"
    };
    println!(
        "binding {} commit {} origin {:?}",
        binding.id().to_hex(),
        binding.payload().git_commit,
        binding.payload().origin,
    );
    println!(
        "signature valid: {} · signer: {} · content: recomputed-ok · provenance: {verdict}",
        evaluation.signature_valid,
        binding.payload().signer.did,
    );
    if !evaluation.can_satisfy_publication_gate() {
        return Err(CliError::Repository(
            atomic_repository::RepositoryError::InvalidOperation {
                message: format!(
                    "binding {} verifies but its provenance is {verdict} and cannot satisfy a publication gate",
                    binding.id()
                ),
            },
        ));
    }
    Ok(())
}

fn resolve_by_hex(repo: &Repository, id: &str) -> CliResult<GitStateBinding> {
    let ids = repo.binding_ids().map_err(CliError::from)?;
    for candidate in ids {
        if candidate.to_hex() == id.to_ascii_lowercase() {
            return repo
                .load_binding(&candidate)
                .map_err(CliError::from)?
                .ok_or_else(|| {
                    CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
                        message: format!("binding {id} not stored"),
                    })
                });
        }
    }
    Err(CliError::Repository(
        atomic_repository::RepositoryError::InvalidOperation {
            message: format!("binding {id} is not stored locally"),
        },
    ))
}

fn resolve_by_prefix(repo: &Repository, prefix: &str) -> CliResult<GitStateBinding> {
    let lower = prefix.to_ascii_lowercase();
    let ids = repo.binding_ids().map_err(CliError::from)?;
    let matches: Vec<_> = ids
        .iter()
        .filter(|id| id.to_hex().starts_with(&lower))
        .collect();
    match matches.as_slice() {
        [id] => repo
            .load_binding(id)
            .map_err(CliError::from)?
            .ok_or_else(|| {
                CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
                    message: format!("binding {prefix} is not stored locally"),
                })
            }),
        [] => Err(CliError::Repository(
            atomic_repository::RepositoryError::InvalidOperation {
                message: format!("binding prefix {prefix} matches nothing"),
            },
        )),
        many => Err(CliError::Repository(
            atomic_repository::RepositoryError::InvalidOperation {
                message: format!(
                    "binding prefix {prefix} is ambiguous: {} matches",
                    matches.len()
                ),
            },
        )),
    }
}

fn git_map(error: impl std::fmt::Display) -> CliError {
    CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
        message: error.to_string(),
    })
}

/// Resolve a published binding (full id or unambiguous prefix) through the
/// create-only Git refs — not through the local store, so bindings fetched
/// from a Git remote resolve without any local adoption.
fn resolve_published_binding(git: &GitRepository, id: &str) -> CliResult<GitStateBinding> {
    let lower = id.to_ascii_lowercase();
    let mut matches: Vec<GitStateBinding> = Vec::new();
    let glob = format!("{}*", atomic_repository::BINDING_REF_PREFIX);
    for reference in git
        .references_glob(&glob)
        .map_err(|error| git_map(format!("cannot enumerate binding refs: {error}")))?
    {
        let reference = reference.map_err(|error| git_map(error.to_string()))?;
        let Some(name) = reference.name() else {
            continue;
        };
        let Some(target) = reference.target() else {
            continue;
        };
        let binding = load_binding_at_ref(git, name, target)?;
        if binding.id().to_hex() == lower
            || (id.len() < 64 && binding.id().to_hex().starts_with(&lower))
        {
            matches.push(binding);
        }
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(CliError::Repository(
            atomic_repository::RepositoryError::InvalidOperation {
                message: format!("binding {id} is not published on any refs/atomic/bindings/* ref"),
            },
        )),
        n => Err(CliError::Repository(
            atomic_repository::RepositoryError::InvalidOperation {
                message: format!("binding prefix {id} is ambiguous: {n} matches"),
            },
        )),
    }
}

fn load_binding_at_ref(
    git: &GitRepository,
    ref_name: &str,
    target: git2::Oid,
) -> CliResult<GitStateBinding> {
    let commit = git.find_commit(target).map_err(|error| {
        git_map(format!(
            "binding ref '{ref_name}' points at a missing commit: {error}"
        ))
    })?;
    let tree = commit
        .tree()
        .map_err(|error| git_map(format!("binding ref '{ref_name}' has no tree: {error}")))?;
    let entry = tree.get_name("binding.cbor").ok_or_else(|| {
        git_map(format!(
            "binding ref '{ref_name}' does not carry binding.cbor"
        ))
    })?;
    let blob = git
        .find_blob(entry.id())
        .map_err(|error| git_map(format!("binding ref '{ref_name}' blob missing: {error}")))?;
    GitStateBinding::decode(blob.content()).map_err(|error| {
        git_map(format!(
            "binding ref '{ref_name}' carries invalid binding bytes: {error}"
        ))
    })
}

/// Closure acquisition through the CLI (CB-6B, RFC §8.6): Atomic remote
/// first, the binding's bounded `changes.pack` as verified fallback.
fn fetch(id: &str, remote: Option<String>, timeout: u64, insecure: bool) -> CliResult<()> {
    use atomic_repository::{ClosureReadiness, IncompletenessReason};

    let root = find_repository_root()?;
    let mut repo =
        Repository::open_for_workspace_transaction(&root).map_err(CliError::Repository)?;
    let workspace = enter_workspace(&mut repo, boundary_mode(false))?;

    let git = open_git(&root)?;
    let binding = resolve_published_binding(&git, id)?;

    // Cryptography + structural verification first: a tampered binding is
    // never a source of truth for a closure fetch.
    verify_binding_cryptography(&binding).map_err(|error| {
        CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
            message: format!("binding {} fails cryptography: {error}", binding.id()),
        })
    })?;
    verify_binding_content(&git, &binding).map_err(|error| {
        CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
            message: format!("binding {} does not match Git: {error}", binding.id()),
        })
    })?;

    // Trust is informational here: unknown-signer content remains
    // independently verifiable; provenance stays untrusted (RFC §5.6).
    let config = atomic_config::RepoConfig::load(&root.join(".atomic/config.toml"))
        .map_err(|error| git_map(error.to_string()))?;
    let repository_identity = config.author.as_ref().and_then(|a| a.identity.as_deref());
    let evaluation = evaluate_binding_trust(
        &binding,
        &config.git.trust,
        BindingVerificationInput {
            repository_identity,
        },
        true,
    );

    let wanted = repo
        .missing_closure_objects(&binding)
        .map_err(CliError::Repository)?;
    println!(
        "Fetching closure for binding {} ({} change(s), {} missing)",
        binding.id().to_hex(),
        binding.payload().ordered_changes.len(),
        wanted.len()
    );

    let mut source: Option<HttpBindingChangeSource> =
        build_remote_source(&repo, remote.as_deref(), timeout, insecure)?;

    let outcome = match source.as_mut() {
        Some(source) => repo
            .fetch_binding_closure(
                &git,
                &binding,
                Some(source),
                &BindingPackLimits::default_limits(),
            )
            .map_err(CliError::Repository)?,
        None => repo
            .fetch_binding_closure(&git, &binding, None, &BindingPackLimits::default_limits())
            .map_err(CliError::Repository)?,
    };
    drop(source);
    drop(workspace);

    println!(
        "Sources: {} already local · {} from Atomic remote · {} from changes.pack",
        outcome.already_local, outcome.from_remote, outcome.from_pack
    );
    println!(
        "Signer {} · signature valid: {} · provenance: {}",
        binding.payload().signer.did,
        evaluation.signature_valid,
        if evaluation.provenance_trusted() {
            "trusted"
        } else {
            "UNTRUSTED (content remains independently verifiable)"
        }
    );

    match &outcome.readiness {
        ClosureReadiness::Complete => {
            print_success(
                "Closure complete: every ordered change verified, closure root recomputed. \
                 Exact adoption (membership restoration) is CB-6C and did not run.",
            );
            print_hint("Fetch alone advanced no view membership, refs, or working-copy state.");
            Ok(())
        }
        ClosureReadiness::Incomplete { missing, reasons } => {
            print_warning(&format!(
                "Closure INCOMPLETE — refusing to adopt; {} change(s) missing:",
                missing.len()
            ));
            for hash in missing.iter().take(5) {
                println!("  {}", hash.to_base32());
            }
            if missing.len() > 5 {
                println!("  … and {} more", missing.len() - 5);
            }
            for reason in reasons {
                match reason {
                    IncompletenessReason::MissingObjects => {
                        print_info("missing objects: no available source holds the full closure")
                    }
                    IncompletenessReason::RemoteUnavailable { detail } => print_info(&format!(
                        "Atomic remote unavailable or unconfigured: {detail}"
                    )),
                    IncompletenessReason::NoFallbackAvailable => print_info(
                        "no changes.pack fallback: publish with --with-changes-pack for \
                         remote-less transport, or reach the closure via the Atomic remote",
                    ),
                    IncompletenessReason::ShallowBoundary => print_info(
                        "shallow/promisor boundary: the binding declares truncated history, \
                         so completeness cannot be claimed across it",
                    ),
                    IncompletenessReason::PackRefused { detail } => {
                        print_warning(&format!("changes.pack refused: {detail}"))
                    }
                }
            }
            Err(CliError::Repository(
                atomic_repository::RepositoryError::InvalidOperation {
                    message: "binding closure is incomplete; explicit refusal (no adoption)"
                        .to_string(),
                },
            ))
        }
    }
}

use atomic_repository::OperationHeadState;

use super::bridge::open_git;

/// Build the preferred Atomic-remote closure source, when one is configured.
fn build_remote_source(
    repo: &Repository,
    remote: Option<&str>,
    timeout: u64,
    insecure: bool,
) -> CliResult<Option<HttpBindingChangeSource>> {
    match remote {
        Some(remote) => {
            let url = if remote.contains("://") {
                remote.to_string()
            } else {
                repo.get_remote(remote)
                    .map(|entry| entry.url)
                    .map_err(CliError::Repository)?
            };
            println!("Atomic remote preferred for closure objects: {url}");
            let config = atomic_remote::HttpRemoteConfig::new()
                .with_timeout(std::time::Duration::from_secs(timeout))
                .danger_accept_invalid_certs(insecure);
            Ok(Some(HttpBindingChangeSource {
                remote: atomic_remote::HttpRemote::with_config(&url, config)
                    .map_err(|error| git_map(error.to_string()))?,
            }))
        }
        None => Ok(None),
    }
}

/// `atomic git bridge binding resurrect` (CB-6C): exact restoration of a
/// verified binding's closure into a view.
fn resurrect(
    id: &str,
    remote: Option<String>,
    timeout: u64,
    insecure: bool,
    to_view: Option<String>,
) -> CliResult<()> {
    use atomic_repository::{ClosureReadiness, IncompletenessReason};

    let root = find_repository_root()?;
    let mut repo =
        Repository::open_for_workspace_transaction(&root).map_err(CliError::Repository)?;
    let workspace = enter_workspace(&mut repo, boundary_mode(false))?;

    let git = open_git(&root)?;
    let binding = resolve_published_binding(&git, id)?;

    // Cryptography + structural verification first: a tampered binding is
    // never a source of truth.
    verify_binding_cryptography(&binding).map_err(|error| {
        CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
            message: format!("binding {} fails cryptography: {error}", binding.id()),
        })
    })?;
    verify_binding_content(&git, &binding).map_err(|error| {
        CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
            message: format!("binding {} does not match Git: {error}", binding.id()),
        })
    })?;

    let mut source: Option<HttpBindingChangeSource> =
        build_remote_source(&repo, remote.as_deref(), timeout, insecure)?;
    let view = match &to_view {
        Some(view) => view.clone(),
        None => {
            let working_copy = repo
                .require_working_copy_id()
                .map_err(CliError::Repository)?;
            repo.desired_view_name(working_copy)
                .map_err(CliError::Repository)?
        }
    };

    println!(
        "Resurrecting binding {} ({} change(s)) into view '{}'",
        binding.id().to_hex(),
        binding.payload().ordered_changes.len(),
        view,
    );
    let outcome = match source.as_mut() {
        Some(source) => repo
            .resurrect_binding_exact(&git, &binding, &view, Some(source))
            .map_err(CliError::Repository)?,
        None => repo
            .resurrect_binding_exact(&git, &binding, &view, None)
            .map_err(CliError::Repository)?,
    };
    drop(source);

    match &outcome.proof {
        proof if proof.projected_tree == proof.bound_tree => {
            print_success(&format!(
                "Resurrected {} change(s) ({} already present) at Merkle {} with SetId {}",
                outcome.inserted.len(),
                outcome.already_present.len(),
                outcome.identity.merkle,
                outcome.identity.set_id,
            ));
            if outcome.restored_raw_commit {
                print_info(
                    "The bound commit was absent from the Git odb; exact raw bytes were restored.",
                );
            }
            if !outcome.provenance_trusted {
                print_warning(
                    "Signer is not trusted under the configured policy: content was verified,                      provenance/attestation roots stay UNTRUSTED (RFC §5.6).",
                );
            }
            // RFC §8.3 (CB-8B): when the binding carries a complete
            // conflict-object pack, the restored state must carry exactly
            // the same conflicts — identities, base/sides, modes, claimants
            // — verified in the fresh store. Any divergence refuses.
            if let Some(pack) = repo
                .load_binding_conflicts_pack(&git, &binding.id())
                .map_err(CliError::Repository)?
            {
                let expected = atomic_repository::repository::ConflictSetObject::decode(&pack)
                    .map_err(|error| {
                        CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
                            message: format!("the binding's conflicts.pack is invalid: {error}"),
                        })
                    })?;
                repo.verify_restored_conflict_state(&view, &expected)
                    .map_err(CliError::Repository)?;
                print_success(&format!(
                    "Restored {} conflicted path(s) with complete conflict objects \
                     (conflict set {}); identities, sides, modes, and claimants verified",
                    expected.files.len(),
                    expected
                        .hash()
                        .map(|hash| hash.to_base32())
                        .unwrap_or_default(),
                ));
            }
        }
        proof => {
            return Err(CliError::Repository(
                atomic_repository::RepositoryError::InvalidOperation {
                    message: format!(
                        "projection proof mismatch: recomputed {} vs bound {}",
                        hex_bytes(proof.projected_tree.as_bytes()),
                        hex_bytes(proof.bound_tree.as_bytes()),
                    ),
                },
            ));
        }
    }
    drop(workspace);
    Ok(())
}

/// CB-10B bounded create-only publication queue (RFC §8.6, §8.5).
fn queue(remote: Option<&str>, max: usize, degraded_head_fallback: bool) -> CliResult<()> {
    let root = find_repository_root()?;
    let mut repo = Repository::open(&root).map_err(CliError::Repository)?;
    let workspace = enter_workspace(&mut repo, boundary_mode(false))?;
    let working_copy = workspace.working_copy();
    let git = open_git(&root)?;

    let outcome = super::transport::run_local_publish_queue(&repo, working_copy, &git, max)?;

    if !outcome.all_local_ok() {
        drop(workspace);
        return Err(CliError::Repository(
            atomic_repository::RepositoryError::InvalidOperation {
                message: format!(
                    "{} stored binding(s) failed to load and were not published; \
                     refusing to continue past unresolvable local state",
                    outcome.unresolvable
                ),
            },
        ));
    }
    println!(
        "Local queue: {} published, {} already present ({} ref(s) considered)",
        outcome.published,
        outcome.already_present,
        outcome.refs.len(),
    );

    let Some(remote_name) = remote else {
        print_hint("no --remote given; binding refs stayed local (RFC §8.6 transfer skipped)");
        drop(workspace);
        return Ok(());
    };

    // CB-10B review R8: the workspace lease is retained through the NETWORK
    // transfer — the ordered boundary outlives the remote effects, so the
    // workspace is never dropped before network completion.
    let transferred = super::transport::transfer_binding_refs(
        &repo,
        &root,
        &git,
        remote_name,
        degraded_head_fallback,
    )?;
    drop(workspace);
    if transferred.is_empty() {
        print_info(&format!(
            "No binding refs to transfer to '{remote_name}' (none are published locally)."
        ));
    } else {
        // Publication is reported only after remote verification, which
        // transfer_binding_refs performs before returning.
        print_success(&format!(
            "Verified {} create-only binding ref(s) on remote '{remote_name}' at their exact targets.",
            transferred.len()
        ));
        print_hint("Transfers carry only the reviewed refs/atomic/bindings/* namespace; WIP refs and snapshots never transfer.");
    }
    Ok(())
}
