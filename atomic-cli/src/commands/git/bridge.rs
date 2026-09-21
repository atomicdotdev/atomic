//! Minimal experimental bridge between a clean Git checkout and an Atomic view.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use atomic_core::operation::{GitHashAlgorithm, GitObjectId, GitRefTarget};
use atomic_core::pristine::{GraphTxnT, ViewTxnT};
use atomic_core::types::Base32;
use atomic_core::types::WorkingCopyId;
use atomic_repository::repository::observability::{
    BridgeEventJournal, BridgeEventKind, EventOutcome, PublicationBoundary, PublicationUnit,
    ReconcileDirectionCode, RefusalClass, RemediationCode, WorkspaceModeCode,
};
use atomic_repository::{
    compare_project_to_worktree, graph_visibility_closure, observe_git_index, observe_worktree,
    verify_prospective_equivalence, GitAttributesFilter, GitObjectKind, InsertOptions, ProjectTree,
    Repository, RepositoryError, StatusOptions, WorkspaceTxnMode,
};
use clap::{Parser, Subcommand};
use git2::{
    ObjectType, Oid, Repository as GitRepository, Status, StatusOptions as GitStatusOptions,
};

use super::checkpoint::{self, BridgeCheckpoint, VerifiedCheckpointInput};
use super::observation::{
    observe_git, observe_head, BridgeCheckpointObservation, GitObservation, HeadObservation,
    ObservationError,
};
use super::parallel::{
    conversion_policy, equivalence_error, git_object_id, verify_complete_equivalence,
};
use super::{anchor, hooks, Import};
use crate::commands::workspace_txn::{
    enter_remediation_workspace, enter_remediation_workspace_budgeted, enter_workspace,
    remediation_error,
};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_success, print_warning};
use atomic_agent::watcher::bridge::{bracket_bridge_transaction, watcher_config};
use atomic_repository::repository::ReconcileEffectBudget;

/// Journal one bridge-owned Git ref move before it becomes visible, perform
/// the move, then record its leased receipt and verify the operation.
///
/// Journal failure returns an error before the write is performed; an
/// unexpected ref value after the write appends a durable rejection receipt
/// and fails closed.
pub(crate) fn journal_and_move_branch(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
    target_ref: &str,
    commit: Oid,
    message: &str,
) -> CliResult<()> {
    let observed_old = git
        .find_reference(target_ref)
        .ok()
        .and_then(|reference| reference.target())
        .and_then(canonical_ref_target);
    let intended_new = GitRefTarget::Direct(
        GitObjectId::new(GitHashAlgorithm::Sha1, commit.as_bytes().to_vec())
            .map_err(|error| git_error(error.to_string()))?,
    );
    // An already-satisfied move is a no-op: there is no transition to journal
    // and no effect to lease (the journal refuses identical old/new values by
    // construction).
    if observed_old == Some(intended_new.clone()) {
        return Ok(());
    }
    let evidence =
        atomic_core::Hash::of(format!("atomic:bridge-ref:v1\0{target_ref}\0{commit}").as_bytes());
    // Journal-before-visibility: the operation (ID + intended effect) is
    // durable before the ref write. Any failure here prevents the write.
    let prepared = repo
        .prepare_bridge_git_ref_write(
            working_copy,
            target_ref,
            observed_old.clone(),
            intended_new.clone(),
            evidence,
        )
        .map_err(|error| {
            git_error(format!(
                "cannot journal '{target_ref}' before writing: {error}"
            ))
        })?;

    // Review R3: the move runs under a REAL Git reference transaction — the
    // ref is locked, the observed old value is re-compared under Git's own
    // ref lock, and a symbolic ref (or any moved target) fails the
    // transaction instead of being observed-then-set.
    {
        let mut transaction = git
            .transaction()
            .map_err(|error| git_error(format!("cannot start a Git reference transaction: {error}")))?;
        transaction.lock_ref(target_ref).map_err(|error| {
            git_error(format!(
                "cannot lock '{target_ref}' for the transaction: {error}"
            ))
        })?;
        let observed_under_lock = git
            .find_reference(target_ref)
            .ok()
            .and_then(|reference| {
                match reference.kind() {
                    // A symbolic ref appearing after the preflight is
                    // rejected under the lock, never treated as absence or
                    // silently re-pointed.
                    Some(git2::ReferenceType::Symbolic) => None,
                    _ => reference.target(),
                }
            })
            .and_then(canonical_ref_target);
        match (&observed_old, &observed_under_lock) {
            (None, None) => {}
            (Some(expected), Some(observed)) if expected == observed => {}
            (expected, observed) => {
                return Err(git_error(format!(
                    "refusing to move '{target_ref}': the ref moved under the transaction lock \
                     (expected {expected:?}, observed {observed:?}); nothing was written"
                )));
            }
        }
        transaction
            .set_target(target_ref, commit, None, message)
            .map_err(|error| {
                git_error(format!(
                    "cannot stage '{target_ref}' in the transaction: {error}"
                ))
            })?;
        transaction.commit().map_err(|error| {
            git_error(format!(
                "cannot commit the '{target_ref}' transaction: {error}"
            ))
        })?;
    }

    // Leased receipts follow the write: observed-after must equal the
    // intended target or the pre-write observation (idempotent replay).
    let observed_after = git
        .find_reference(target_ref)
        .ok()
        .and_then(|reference| reference.target())
        .and_then(canonical_ref_target);
    repo.record_bridge_git_ref_receipt(&prepared, observed_after.clone())
        .map_err(|error| git_error(error.to_string()))?;
    // CB-13D ::24 AC-5 crash-during-effect failpoint: the effect has
    // LANDED (the ref move + its leased receipt are durable) but the
    // operation's Verified finalize has not run — the interrupted head is
    // recovered on the next writable open and the retry converges without
    // partial bytes. Shipping builds compile the no-op stub.
    #[cfg(feature = "adoption-test-injection")]
    if std::env::var_os("ATOMIC_FAIL_RECONCILE_EXPORT_MID_EFFECT").is_some() {
        return Err(git_error(
            "debug failpoint: ATOMIC_FAIL_RECONCILE_EXPORT_MID_EFFECT",
        ));
    }
    repo.finalize_bridge_git_write(prepared, observed_after)
        .map_err(|error| git_error(error.to_string()))?;
    Ok(())
}

fn canonical_ref_target(oid: Oid) -> Option<GitRefTarget> {
    let bytes = oid.as_bytes().to_vec();
    let algorithm = match bytes.len() {
        20 => GitHashAlgorithm::Sha1,
        32 => GitHashAlgorithm::Sha256,
        _ => return None,
    };
    GitObjectId::new(algorithm, bytes)
        .ok()
        .map(GitRefTarget::Direct)
}

/// Journal an expected-absent ref creation, perform it through a real Git
/// reference transaction, then record its leased receipt and verify.
///
/// CB-10A review R3: the ref is locked with libgit2's reference transaction
/// and re-compared under Git's own ref lock, so a concurrent write to the
/// target fails the transaction instead of being lost or rewound by a
/// post-write observation. The expected old is *absence*: publication is
/// create-only.
pub(crate) fn journal_and_create_branch_cas(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
    target_ref: &str,
    commit: Oid,
    message: &str,
) -> CliResult<()> {
    let observed_old = git
        .find_reference(target_ref)
        .ok()
        .and_then(|reference| reference.target())
        .and_then(canonical_ref_target);
    if observed_old.is_some() {
        return Err(git_error(format!(
            "refusing to create '{target_ref}': it already exists; publication is create-only"
        )));
    }
    let intended_new = GitRefTarget::Direct(
        GitObjectId::new(GitHashAlgorithm::Sha1, commit.as_bytes().to_vec())
            .map_err(|error| git_error(error.to_string()))?,
    );
    let evidence = atomic_core::Hash::of(
        format!("atomic:bridge-ref-create:v1\0{target_ref}\0{commit}").as_bytes(),
    );
    // Journal-before-visibility: the operation (ID + intended effect) is
    // durable before the ref write. Any failure here prevents the write.
    let prepared = repo
        .prepare_bridge_git_ref_write(working_copy, target_ref, None, intended_new, evidence)
        .map_err(|error| {
            git_error(format!(
                "cannot journal '{target_ref}' before writing: {error}"
            ))
        })?;

    // Real Git reference transaction: lock the ref (reserving it), re-compare
    // under the lock, then commit the single-ref transaction.
    let mut transaction = git
        .transaction()
        .map_err(|error| git_error(format!("cannot start a Git reference transaction: {error}")))?;
    transaction.lock_ref(target_ref).map_err(|error| {
        git_error(format!(
            "cannot lock '{target_ref}' for the transaction: {error}"
        ))
    })?;
    // Review R3: presence is refused in ANY form. `reference.target()` is
    // `None` for a symbolic reference, so the target-only read would treat a
    // symbolic ref as absence and overwrite it; the kind is checked instead.
    if let Ok(reference) = git.find_reference(target_ref) {
        let shape = match reference.kind() {
            Some(git2::ReferenceType::Symbolic) => "a symbolic reference",
            Some(git2::ReferenceType::Direct) => "a direct reference",
            _ => "a reference",
        };
        return Err(git_error(format!(
            "refusing to create '{target_ref}': it appeared as {shape} while the publication \
             transaction was being journaled; nothing was written"
        )));
    }
    transaction
        .set_target(target_ref, commit, None, message)
        .map_err(|error| {
            git_error(format!(
                "cannot stage '{target_ref}' in the transaction: {error}"
            ))
        })?;
    transaction.commit().map_err(|error| {
        git_error(format!(
            "cannot commit the '{target_ref}' transaction: {error}"
        ))
    })?;

    // Leased receipts follow the write: observed-after must equal the
    // intended target or the pre-write observation (idempotent replay).
    let observed_after = git
        .find_reference(target_ref)
        .ok()
        .and_then(|reference| reference.target())
        .and_then(canonical_ref_target);
    repo.record_bridge_git_ref_receipt(&prepared, observed_after.clone())
        .map_err(|error| git_error(error.to_string()))?;
    repo.finalize_bridge_git_write(prepared, observed_after)
        .map_err(|error| git_error(error.to_string()))?;
    Ok(())
}

/// CB-10A review R3: the publication ref write with its durable MAPPING
/// INTENT in the same journaled operation. The operation carries both the
/// `ExportGitRefs` effect lease and the `RefMapping` metadata transition, so
/// no published branch can ever survive without recoverable mapping intent:
/// the intent is durable (and lease-validated) BEFORE the ref write, and it
/// is applied — exactly once, after the ref receipt — by the completion
/// below. The create itself is the same create-only transaction CAS with
/// symbolic-presence rejection.
pub(crate) fn journal_and_create_branch_cas_with_mapping(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
    target_ref: &str,
    commit: Oid,
    message: &str,
    mapping_intent: atomic_core::operation::MetadataTransition,
) -> CliResult<()> {
    let observed_old = git
        .find_reference(target_ref)
        .ok()
        .and_then(|reference| reference.target())
        .and_then(canonical_ref_target);
    if observed_old.is_some() {
        return Err(git_error(format!(
            "refusing to create '{target_ref}': it already exists; publication is create-only"
        )));
    }
    let intended_new = GitRefTarget::Direct(
        GitObjectId::new(GitHashAlgorithm::Sha1, commit.as_bytes().to_vec())
            .map_err(|error| git_error(error.to_string()))?,
    );
    let evidence = atomic_core::Hash::of(
        format!("atomic:bridge-ref-create:v1\0{target_ref}\0{commit}").as_bytes(),
    );
    let prepared = repo
        .prepare_bridge_git_ref_write_with_metadata(
            working_copy,
            target_ref,
            None,
            intended_new,
            evidence,
            vec![mapping_intent],
        )
        .map_err(|error| git_error(format!(
            "cannot journal '{target_ref}' (with its mapping intent) before writing: {error}"
        )))?;

    let mut transaction = git
        .transaction()
        .map_err(|error| git_error(format!("cannot start a Git reference transaction: {error}")))?;
    transaction.lock_ref(target_ref).map_err(|error| {
        git_error(format!(
            "cannot lock '{target_ref}' for the transaction: {error}"
        ))
    })?;
    if let Ok(reference) = git.find_reference(target_ref) {
        let shape = match reference.kind() {
            Some(git2::ReferenceType::Symbolic) => "a symbolic reference",
            Some(git2::ReferenceType::Direct) => "a direct reference",
            _ => "a reference",
        };
        return Err(git_error(format!(
            "refusing to create '{target_ref}': it appeared as {shape} while the publication \
             transaction was being journaled; nothing was written"
        )));
    }
    transaction
        .set_target(target_ref, commit, None, message)
        .map_err(|error| {
            git_error(format!(
                "cannot stage '{target_ref}' in the transaction: {error}"
            ))
        })?;
    transaction.commit().map_err(|error| {
        git_error(format!(
            "cannot commit the '{target_ref}' transaction: {error}"
        ))
    })?;

    let observed_after = git
        .find_reference(target_ref)
        .ok()
        .and_then(|reference| reference.target())
        .and_then(canonical_ref_target);
    repo.record_bridge_git_ref_receipt(&prepared, observed_after.clone())
        .map_err(|error| git_error(error.to_string()))?;
    repo.complete_bridge_git_write_with_metadata(prepared, observed_after)
        .map_err(|error| git_error(error.to_string()))?;
    Ok(())
}

/// Reconcile and verify a clean, attached Git checkout against Atomic.
#[derive(Parser, Debug, Default)]
#[command(name = "bridge")]
pub struct Bridge {
    #[command(subcommand)]
    pub command: BridgeCommand,
}

#[derive(Subcommand, Debug, Default)]
pub enum BridgeCommand {
    /// Incrementally import Git HEAD, verify it, and record bridge metadata.
    #[default]
    Reconcile,
    /// Read-only comparison of Git HEAD and the current Atomic view.
    Verify,
    /// Switch Git and Atomic together to an existing view.
    Switch { view: String },
    /// Report per-view ref-mapping reconciliation status (CB-10A).
    ///
    /// Read-only: it observes the persisted mapping, the mapped ref tip and
    /// the Atomic state, and reports Synchronized/GitAhead/AtomicAhead/
    /// Diverged/Unrepresentable with explicit remediation. It never
    /// materializes or moves anything.
    Status,
    /// Explicitly publish a Draft view to a Git branch (RFC §8.1).
    ///
    /// Publication is never implicit: this journals the branch creation at
    /// the draft's projection commit as an expected-old ref operation and
    /// records the branch-backed mapping. The view stays a Draft in Atomic;
    /// the mapping's local ref becomes `refs/heads/<branch>`.
    Publish {
        /// The Draft view to publish.
        view: String,
        /// The branch to publish it on.
        #[arg(long)]
        branch: String,
    },
    /// Enable the advisory Git checkout event bridge and anchor bridge state.
    ///
    /// With `--mirror-ignores`, `.atomicignore` patterns that are not already
    /// represented in Git ignore sources are mirrored into a managed
    /// `.git/info/exclude` block. This is explicit consent (RFC §9.4);
    /// without it, status reports an ignore-policy divergence instead.
    ///
    /// CB-10B transport (RFC §8.6): when a remote is configured (`--remote`,
    /// default `origin`), enable adds the explicit
    /// `+refs/atomic/bindings/*` and `+refs/atomic/views/*` fetch refspecs so
    /// binding refs transfer with every fetch. A missing remote is reported
    /// as a notice, not an error — refspecs need a configured remote.
    ///
    /// CB-7A anchoring (RFC §7): after the hook integration is installed,
    /// enable proves complete repository/index/worktree equivalence and only
    /// then creates the signed Anchor binding and the verified working-copy
    /// checkpoint. Mismatches report a typed refusal offering
    /// `--adopt-atomic`; unbound adoption and `--adopt-git` remain
    /// gated (CB-9B/9C).
    Enable {
        /// Consent to mirroring .atomicignore into .git/info/exclude.
        #[arg(long)]
        mirror_ignores: bool,
        /// Explicit remediation: project the current Atomic state onto Git
        /// HEAD through the safe Atomic-origin path before anchoring. Refuses
        /// dirty worktrees; never silently merges.
        #[arg(long)]
        adopt_atomic: bool,
        /// Explicitly unavailable until Phase 9 foreign synthesis (CB-9B/9C).
        #[arg(long)]
        adopt_git: bool,
        /// File holding the hex-encoded 32-byte Ed25519 secret key that signs
        /// the Anchor binding. Without it the Anchor cannot be created.
        #[arg(long)]
        binding_key_file: Option<PathBuf>,
        /// Attach the bounded `changes.pack` fallback to the Anchor's
        /// binding tree (RFC §8.6, CB-10B): the hash-verified V3 change
        /// files of the effective closure, for receivers that cannot reach
        /// an Atomic remote. Private sidecars and unhashed bodies never
        /// enter the pack; assembly refuses them closed.
        #[arg(long)]
        with_changes_pack: bool,
        /// Remote to configure the RFC §8.6 fetch refspecs on.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Skip the RFC §8.6 fetch-refspec configuration (local-only repos).
        #[arg(long)]
        no_refspecs: bool,
    },
    /// Mirror .atomicignore into the managed .git/info/exclude block
    /// (explicit consent).
    MirrorIgnores,
    /// Roll the bridge back: record `[git.bridge] enabled = false` in the
    /// repository configuration (review R4: the tested rollback path).
    ///
    /// The advisory dispatchers installed by `enable` remain on disk —
    /// they are advisory evidence hooks, not the guarantee (RFC §10.3) —
    /// and the automatic telemetry sink stops writing once consent is
    /// withdrawn. Run `atomic git bridge enable` again to re-record the
    /// opt-in.
    Disable,
    /// Sign, publish, and verify immutable Git state bindings.
    Binding {
        #[command(subcommand)]
        command: super::binding::BindingCommand,
    },
    /// Review squash/rewrite candidates with their evidence and identity
    /// limits (CB-9B).
    ///
    /// Lists imported squash interpretations, rewrite candidates, merge
    /// resolutions, and empty-commit interpretations recorded by the
    /// importer, together with the hook evidence that links them to Git
    /// operations. Evidence never establishes identity by itself: the
    /// output states exactly what each evidence class can and cannot prove
    /// (RFC §5.4).
    Review {
        /// Restrict the review to one view's imported history.
        #[arg(long)]
        view: Option<String>,
    },
    /// Internal callback used only by the Atomic-owned post-checkout dispatcher.
    #[command(hide = true)]
    HookPostCheckout {
        old_head: String,
        new_head: String,
        checkout_flag: String,
    },
    /// Internal callback used only by the Atomic-owned post-rewrite
    /// dispatcher: journals rewrite evidence (operation linkage only).
    #[command(hide = true)]
    HookPostRewrite,
    /// Internal callback used only by the Atomic-owned pre-commit dispatcher
    /// (CB-12A): writes one authenticated commit-time capture per active
    /// managed agent session. Advisory evidence only — it never blocks a
    /// commit, and the RFC §19 Q2 inseparable-commit policy stays undecided.
    #[command(hide = true)]
    HookPreCommit,
    /// Internal callback used only by the Atomic-owned reference-transaction
    /// dispatcher: journals ref movement evidence only. Git passes the
    /// transaction state as argv[1] (review blocker 5) and the
    /// `<old> <new> <ref>` lines on stdin.
    #[command(hide = true)]
    HookReferenceTransaction {
        /// Git's transaction state (committed|prepared|aborted).
        state: String,
    },
    /// Local advisory pre-push verification (CB-12B, RFC §10.4/§11.1).
    ///
    /// Runs the same trusted provenance gate the publication boundaries run
    /// and reports honestly: a local pass proves nothing about the remote,
    /// and a local refusal blocks only this machine's push. Never starts a
    /// nested push and performs no writes. Git invokes this with
    /// `<local-ref> <local-oid> <remote-ref> <remote-oid>` lines on stdin.
    #[command(hide = true)]
    HookPrePush,
    /// Trusted receiving-side verifier (CB-12B, RFC §10.4/§12.10).
    ///
    /// Deploy on an Atomic-controlled remote, as a server pre-receive/update
    /// hook, or behind a required CI status check. Reads Git's
    /// `<old> <new> <ref>` ref-update lines on stdin and refuses protected
    /// updates whose ingested state carries managed-session changes with
    /// missing, tampered, untrusted, or incomplete evidence. Everything is
    /// protected unless explicitly exempted via `--unprotected`.
    VerifyReceive {
        /// Ref prefixes to exempt from managed-provenance verification
        /// (explicit deployment choice; everything else fails closed).
        #[arg(long = "unprotected")]
        unprotected: Vec<String>,
    },
    /// Internal worker for one immutable deferred-observation request.
    #[command(hide = true)]
    ObserveDeferred {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        request: PathBuf,
    },
    /// Watch Git metadata and reconcile reactively (CB-13D, optional).
    ///
    /// The metadata-only bridge watch daemon: an accelerator, never the
    /// guarantee. It treats Git metadata differences as wake-up hints,
    /// waits for at least 250 ms of silence with no Git index/ref locks
    /// and no sequence operations, then runs the shared workspace
    /// transaction path under a metadata-only budget — it never
    /// materializes files and never moves Git refs; export-classified
    /// runs are refused with a notice instead. Opt-in only via
    /// `[git.bridge] enabled = true` plus `[git.bridge.watch] enabled`.
    Watch {
        /// Run exactly one reactive pass and exit (test/equivalence mode).
        #[arg(long)]
        once: bool,
        /// Metadata re-observation interval in milliseconds.
        #[arg(long, default_value_t = super::watch::DEFAULT_POLL_INTERVAL_MS)]
        poll_ms: u64,
    },
    /// Cut the repository over to the colocated bridge (CB-13B).
    ///
    /// The cutover is the exclusive enablement-state transition that ends
    /// the two-writer window: it journals a `Cutover` metadata operation
    /// and moves the `required-capability/git-bridge-cutover` row from
    /// absent to the fenced version, after which the legacy shadow writer
    /// refuses and the colocated bridge owns writes. Requires the explicit
    /// opt-in (`atomic git bridge enable`) and full migration readiness;
    /// hook, remote and unsupported surfaces are reported and refuse —
    /// nothing is migrated partially.
    ///
    /// With `--rollback`, the most recent verified cutover is rolled back
    /// through the journal's typed inverse: the shared repository head
    /// must still denote the cutover, and newer format fences are kept.
    Cutover {
        /// Roll the verified cutover back (shared-head gated).
        #[arg(long)]
        rollback: bool,
    },
}

impl Command for Bridge {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            BridgeCommand::Reconcile => reconcile(),
            BridgeCommand::Verify => {
                verify()?;
                print_success("Git HEAD matches the current Atomic view");
                Ok(())
            }
            BridgeCommand::Switch { view } => switch(view),
            BridgeCommand::Status => super::ref_mapping::run_status(),
            BridgeCommand::Publish { view, branch } => {
                super::ref_mapping::run_publish(view, branch)
            }
            BridgeCommand::Enable {
                mirror_ignores,
                adopt_atomic,
                adopt_git,
                binding_key_file,
                with_changes_pack,
                remote,
                no_refspecs,
            } => {
                let root = find_repository_root()?;
                // The explicitly unavailable flag refuses before any mutation.
                if *adopt_git {
                    return Err(anchor::adopt_git_refusal());
                }
                hooks::enable_bridge(&root, *mirror_ignores)?;
                // Review R4: the explicit CLI enable IS the consent, so it
                // is recorded where it is consumed. The repository
                // configuration now records `[git.bridge] enabled = true`;
                // `git bridge disable` rolls it back.
                if record_bridge_opt_in(&root, true)? {
                    print_info(
                        "Recorded explicit bridge opt-in in the repository configuration ([git.bridge] enabled = true).",
                    );
                }
                // Report the default-enablement gate honestly (review R4:
                // the helper gains a real consumer). Explicit opt-in is
                // experimental consent, never default-rollout approval.
                match bridge_config(&root)?.git.bridge.default_enablement() {
                    Ok(()) => print_info(
                        "All rollout gates are measured and passed; default (automatic) enablement still requires recorded owner approval of the thresholds.",
                    ),
                    Err(refusal) => print_info(&format!(
                        "Default (automatic) enablement remains refused: {refusal}"
                    )),
                }
                if *mirror_ignores {
                    hooks::mirror_ignores_with_consent(&root)?;
                }
                // CB-10B (RFC §8.6): explicit fetch refspecs on enable, before
                // the anchor phase. A missing remote is a notice, not an
                // error — binding refs need a configured remote to transfer.
                if !*no_refspecs {
                    let git = open_git(&root)?;
                    match super::transport::configure_fetch_refspecs(&git, remote) {
                        Ok(0) => print_info(&format!(
                            "Fetch refspecs for refs/atomic/bindings/* and refs/atomic/views/* already configured on '{remote}'."
                        )),
                        Ok(added) => print_success(&format!(
                            "Configured {added} explicit fetch refspec(s) on '{remote}' for refs/atomic/bindings/* and refs/atomic/views/* (RFC §8.6)."
                        )),
                        Err(error) => print_warning(&format!(
                            "fetch refspecs not configured: {error}; binding refs will not \
                             transfer with fetch until 'atomic git bridge enable --remote <name>' \
                             succeeds"
                        )),
                    }
                }
                anchor::run_enable_anchor(
                    &root,
                    &anchor::EnableAnchorOptions {
                        adopt_atomic: *adopt_atomic,
                        adopt_git: *adopt_git,
                        binding_key_file: binding_key_file.clone(),
                        with_changes_pack: *with_changes_pack,
                    },
                )
            }
            BridgeCommand::MirrorIgnores => {
                let root = find_repository_root()?;
                hooks::mirror_ignores_with_consent(&root)
            }
            BridgeCommand::Disable => {
                let root = find_repository_root()?;
                if record_bridge_opt_in(&root, false)? {
                    print_success(
                        "Recorded bridge rollback in the repository configuration ([git.bridge] enabled = false). The automatic telemetry sink stops writing without consent; advisory dispatchers remain on disk.",
                    );
                } else {
                    print_info("Bridge was already disabled in the repository configuration.");
                }
                Ok(())
            }
            BridgeCommand::Cutover { rollback } => run_cutover(*rollback),
            BridgeCommand::Binding { command } => command.clone().run(),
            BridgeCommand::Review { view } => {
                let root = find_repository_root()?;
                super::hooks::run_bridge_review(&root, view.as_deref())
            }
            BridgeCommand::HookPostCheckout {
                old_head,
                new_head,
                checkout_flag,
            } => {
                let root = find_repository_root()?;
                hooks::record_post_checkout(&root, old_head, new_head, checkout_flag)
            }
            BridgeCommand::HookPostRewrite => {
                let root = find_repository_root()?;
                hooks::record_post_rewrite(&root)
            }
            BridgeCommand::HookPreCommit => {
                let root = find_repository_root()?;
                hooks::record_pre_commit(&root)
            }
            BridgeCommand::HookReferenceTransaction { state } => {
                let root = find_repository_root()?;
                hooks::record_reference_transaction(&root, state)
            }
            BridgeCommand::HookPrePush => {
                let root = find_repository_root()?;
                hooks::run_pre_push_verification(&root)
            }
            BridgeCommand::VerifyReceive { unprotected } => {
                let root = find_repository_root()?;
                super::verify_receive::run_verify_receive(&root, unprotected)
            }
            BridgeCommand::ObserveDeferred { root, request } => {
                hooks::run_deferred_observation(root, request)
            }
            BridgeCommand::Watch { once, poll_ms } => {
                let root = find_repository_root()?;
                super::watch::run_watch(&root, *once, *poll_ms)
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct BridgeSnapshot {
    pub(crate) view: String,
    pub(crate) atomic_state: String,
    pub(crate) git_head: String,
    pub(crate) git_tree: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ReconcileDirection {
    Neither,
    GitToAtomic,
    AtomicToGit,
    Diverged,
}

fn classify_direction(
    checkpoint_view: &str,
    checkpoint_git_head: &str,
    checkpoint_atomic_state: &str,
    current_git_branch: &str,
    current_git_head: &str,
    current_atomic_state: &str,
) -> ReconcileDirection {
    let git_changed =
        checkpoint_view != current_git_branch || checkpoint_git_head != current_git_head;
    match (git_changed, checkpoint_atomic_state != current_atomic_state) {
        (false, false) => ReconcileDirection::Neither,
        (true, false) => ReconcileDirection::GitToAtomic,
        (false, true) => ReconcileDirection::AtomicToGit,
        (true, true) => ReconcileDirection::Diverged,
    }
}

fn reconcile() -> CliResult<()> {
    let root = find_repository_root()?;
    // The transaction brackets the FileWatcher with atomic-bridge enter/leave,
    // including failure cleanup; bridge-originated writes inside the span are
    // self-suppressed so they never feed back into reconciliation.
    bracket_bridge_transaction(watcher_config(&root), "bridge-reconcile", || {
        reconcile_transaction_budgeted(&root, ReconcileEffectBudget::Command, None)
    })
}

/// Load the repository configuration for `root` (review R4: the bridge
/// consent surface is read where it is acted on, not only in tests).
pub(crate) fn bridge_config(root: &Path) -> CliResult<atomic_config::RepoConfig> {
    let config_path = Repository::canonical_dot_dir(root)
        .map_err(CliError::from)?
        .join("config.toml");
    atomic_config::RepoConfig::load(&config_path)
        .map_err(|error| git_error(format!("cannot load the repository configuration: {error}")))
}

/// Record the explicit bridge consent in the repository configuration.
/// Returns whether the file changed. Review R4: the CLI enable/disable
/// action and the flag become one reachable consent/activation path.
///
/// CB-13C F2: the transition is serialized under the common operation
/// lock with an expected-old lease and an atomic file replacement — the
/// caller's observed consent state is the lease, so a concurrent consent
/// change refuses instead of being overwritten.
fn record_bridge_opt_in(root: &Path, enabled: bool) -> CliResult<bool> {
    let observed = bridge_config(&root)?.git.bridge.enabled;
    let repo = Repository::open(&root).map_err(CliError::from)?;
    repo.set_bridge_consent(enabled, Some(observed))
        .map_err(CliError::from)
}

/// The reachable CB-13B cutover entry point: the CLI command owns the
/// journaled cutover executor (and its typed inverse). The explicit opt-in
/// consent is a precondition — the cutover never flips enablement
/// implicitly — and every readiness refusal is reported verbatim without
/// partial enablement (hook, remote and unsupported surfaces refuse).
fn run_cutover(rollback: bool) -> CliResult<()> {
    let root = find_repository_root()?;
    if rollback {
        let mut repo = Repository::open(&root).map_err(CliError::from)?;
        let undone = repo.rollback_bridge_cutover().map_err(CliError::from)?;
        print_success(&format!(
            "Cutover {undone} rolled back through the journal's typed inverse; \
             the legacy shadow writer fence is lifted and newer format fences are kept."
        ));
        return Ok(());
    }
    let config = bridge_config(&root)?;
    if !config.git.bridge.enabled {
        return Err(git_error(
            "bridge cutover requires the explicit opt-in consent; run \
             `atomic git bridge enable` first (the cutover never enables implicitly)",
        ));
    }
    let repo = Repository::open(&root).map_err(CliError::from)?;
    let outcome = repo.execute_bridge_cutover().map_err(CliError::from)?;
    if outcome.resumed {
        print_success(&format!(
            "Resumed the interrupted cutover {}: the fence was already durable and the \
             Verified receipt is now appended.",
            outcome
                .operation
                .map(|id| id.to_string())
                .unwrap_or_default()
        ));
    } else if outcome.already_fenced {
        print_info(
            "The repository is already cut over to the colocated bridge; nothing to do.",
        );
    } else {
        print_success(&format!(
            "Cutover {} journaled and verified: the colocated bridge now owns writes and \
             the legacy shadow writer is fenced.",
            outcome
                .operation
                .map(|id| id.to_string())
                .unwrap_or_default()
        ));
    }
    Ok(())
}

/// Record one change-source degradation from an explicit bridge command
/// boundary (review R1: status returns its metrics without writing; only
/// an authorized boundary decides to record them). Consent-gated
/// (`for_repository`) and lossy.
fn emit_change_source_fallback(
    repo: &Repository,
    source: atomic_repository::change_source::ChangeSourceKind,
    fallback_source: Option<atomic_repository::change_source::ChangeSourceKind>,
    fallback_reason: Option<&atomic_repository::change_source::ChangeSourceFallbackReason>,
) {
    use atomic_repository::repository::observability::{
        BridgeEventJournal, BridgeEventKind, FallbackReasonCode, TierCode,
    };
    BridgeEventJournal::for_repository(repo).emit_lossy(BridgeEventKind::ChangeSourceFallback {
        source: TierCode::from_kind(source),
        fallback_source: fallback_source.map(TierCode::from_kind),
        reason: fallback_reason.map(FallbackReasonCode::from_reason),
    });
}

/// CB-13D notice kinds the metadata-only watch path surfaces to active
/// managed sessions (RFC §11.2 rule 6: surface, don't act, when unsafe).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WatchNoticeKind {
    /// Git HEAD moved externally; the workspace is reconciling.
    ExternalHeadChange,
    /// A Git-owned transaction or divergence prevents reconciling now.
    UnsafeState,
    /// Atomic moved ahead of Git; reactive export is suppressed and an
    /// explicit `atomic git bridge reconcile` is required.
    ExportSuppressed,
}

/// Callback receiving one watch notice; `String` is the human detail.
pub(crate) type WatchNoticeSink<'s> = dyn Fn(WatchNoticeKind, &str) + 's;

/// One `atomic git bridge reconcile` run under an explicit effect budget.
///
/// `Command` (the CLI command) may project Atomic→Git under its
/// expected-old lease. `MetadataOnly` (the optional bridge watch daemon,
/// CB-13D) runs the identical shared transaction path — the same entry
/// boundary, observation, classification, and import code — but never
/// exports and never materializes: an export-classified run is refused
/// with a journal event and a session notice instead of moving refs.

/// CB-13D ::24 R4: the suppression attribution marker — the JOURNAL
/// OperationId of the just-completed bridge operation and the working-copy
/// heads it advanced, so watcher self-event matching binds observed changes
/// to the operation that caused them (never an invocation or pid label).
/// Best-effort: suppression attribution is advisory, and a failure to
/// record it never fails the reconciled pass.
pub(crate) fn record_last_bridge_operation(root: &Path) {
    let record = (|| -> CliResult<()> {
        let repo = Repository::open_readonly(root).map_err(CliError::from)?;
        let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
        let log = repo
            .operation_log(atomic_core::operation::OperationScope::WorkingCopy(working_copy), Some(1), true)
            .map_err(CliError::from)?;
        let head = match &log.head_state {
            atomic_repository::repository::OperationHeadState::Single(id) => id.to_string(),
            _ => String::new(),
        };
        let heads = head.clone();
        let path = Repository::canonical_dot_dir(root)
            .map_err(CliError::from)?
            .join("bridge/suppressed-operation.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(CliError::from)?;
        }
        let payload = format!(
            "{{\"operation_id\":\"{head}\",\"heads\":\"{heads}\",\"recorded_at\":{}}}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        std::fs::write(&path, payload).map_err(CliError::from)?;
        Ok(())
    })();
    if let Err(error) = record {
        log::warn!("bridge watch: could not record the suppression attribution: {error}");
    }
}


pub(crate) fn reconcile_transaction_budgeted(
    root: &Path,
    budget: ReconcileEffectBudget,
    notice: Option<&WatchNoticeSink<'_>>,
) -> CliResult<()> {
    let emit_notice = |kind: WatchNoticeKind, detail: &str| {
        if let Some(notice) = notice {
            notice(kind, detail);
        }
    };
    // CB-13C observability: one journal per reconcile run; every terminal
    // outcome below records a stable `reconcile` event (review R5: including
    // the entry/observation failures that previously bypassed the sink).
    // Emission is lossy; reconcile is an explicit bridge command boundary.
    let journal = BridgeEventJournal::new(
        &Repository::canonical_dot_dir(root).map_err(CliError::from)?,
        None,
    );
    let refused_run = |direction: ReconcileDirectionCode, refusal_class: RefusalClass| {
        journal.emit_lossy(BridgeEventKind::Reconcile {
            direction,
            outcome: EventOutcome::Refused,
            refusal_class: Some(refusal_class),
        });
    };

    // Entry boundary. Both budgets enter the same shared workspace
    // transaction path the reconcile command uses (RFC §11.2 rule 3: the
    // watch daemon has no entry/mutation logic of its own). That boundary
    // tolerates an unanchored baseline — a moved HEAD is reconciled by the
    // §7.3 import path below, which for the daemon is exactly the
    // RFC-sanctioned metadata-only ImportGitHead work (rule 4) — while
    // still refusing Git-owned sequence operations, index locks, and
    // diverged operation heads. The budgets differ in the effects granted:
    // the metadata-only budget refuses bound HEAD adoption (shelf/WIP/ref
    // effects), refuses pending recovery work at the writable open, and
    // revalidates Git quiescence under the entry lease, so export-classified
    // and effect-bearing runs become notices (review R1/R2).
    let mut repo = match Repository::open_with_budget(root, budget) {
        Ok(repo) => repo,
        Err(atomic_repository::RepositoryError::ReactiveDeferred { detail }) => {
            // Review R1: the writable open refused pending recovery work
            // before any of it executed; surface the deferral instead of
            // acting.
            refused_run(
                ReconcileDirectionCode::Unknown,
                RefusalClass::MetadataOnlyRecoveryDeferred,
            );
            emit_notice(
                WatchNoticeKind::UnsafeState,
                &format!("reactive reconcile deferred: {detail}"),
            );
            return Err(git_error(format!(
                "the bridge watch daemon defers: {detail}; \
                 run 'atomic git bridge reconcile' explicitly"
            )));
        }
        Err(error) => {
            refused_run(
                ReconcileDirectionCode::Unknown,
                RefusalClass::RepositoryOpenFailed,
            );
            return Err(error.into());
        }
    };
    let workspace = match enter_remediation_workspace_budgeted(&mut repo, budget) {
        Ok(Ok(workspace)) => workspace,
        Ok(Err(remediation)) => {
            // Reconcile is the mutating remediation boundary, so its
            // refusals are recorded (guarded commands never write
            // telemetry on refusal — RFC Phase 0 no-mutation contract).
            // The metadata-only daemon surfaces the same refusal as an
            // unsafe-state notice for active sessions (RFC §11.2 rule 6).
            journal.emit_lossy(BridgeEventKind::WorkspaceRefusal {
                mode: WorkspaceModeCode::from_remediation(&remediation),
                remediation: RemediationCode::from_remediation(&remediation),
            });
            refused_run(
                ReconcileDirectionCode::Unknown,
                RefusalClass::WorkspaceEntryRefused,
            );
            emit_notice(WatchNoticeKind::UnsafeState, &remediation.describe());
            return Err(remediation_error(remediation));
        }
        Err(error) => {
            refused_run(
                ReconcileDirectionCode::Unknown,
                RefusalClass::ObservationFailed,
            );
            return Err(error);
        }
    };

    let working_copy = workspace.working_copy();
    let git = open_git(root)?;
    // The observation phase (heads, branch, checkpoint, mapping) is
    // pre-mutation: any failure here is a true refusal and is recorded as
    // one.
    let (current, direction, mapping_outcome) = match (|| -> CliResult<_> {
        let current = current_heads(&repo, working_copy, &git)?;
        let current_git_branch = current_git_branch_or_view(&git, &current.view)?;
        let checkpoint = read_workspace_metadata(root)?;
        let direction = checkpoint
            .as_ref()
            .map(|checkpoint| {
                classify_direction(
                    &checkpoint.view,
                    &checkpoint.git_head,
                    &checkpoint.atomic_state,
                    &current_git_branch,
                    &current.git_head,
                    &current.atomic_state,
                )
            })
            .unwrap_or(ReconcileDirection::GitToAtomic);
        // CB-10A: the persisted ref mapping decides the three-way ref
        // reconciliation (RFC §8.5). The checkpoint classifier stays for
        // branch/symref-level changes; the mapping refines the both-moved
        // case with closure-containment proofs and fails closed to Diverged.
        let (mapping, baseline_journaled) =
            super::ref_mapping::ensure_baseline_mapping(&repo, working_copy, &git, &current.view)?;
        let decision =
            super::ref_mapping::classify_mapping_direction(&repo, &git, &current.view, &mapping)?;
        Ok((current, direction, Some((mapping, decision, baseline_journaled))))
    })() {
        Ok(observation) => observation,
        Err(error) => {
            refused_run(
                ReconcileDirectionCode::Unknown,
                RefusalClass::ObservationFailed,
            );
            return Err(error);
        }
    };
    // CB-13C F3: whether this run journaled a baseline mapping (a
    // mutation). Every post-observation terminal outcome must be truthful:
    // `Refused` promises before-any-mutation, so a run that journaled the
    // baseline and then hit a gate emits `Failed` with the same refusal
    // class instead.
    let baseline_journaled = match &mapping_outcome {
        Some((_, _, journaled)) => *journaled,
        None => false,
    };
    let refused_after_observation = |direction: ReconcileDirectionCode,
                                     refusal_class: RefusalClass| {
        journal.emit_lossy(BridgeEventKind::Reconcile {
            direction,
            outcome: if baseline_journaled {
                EventOutcome::Failed
            } else {
                EventOutcome::Refused
            },
            refusal_class: Some(refusal_class),
        });
    };

    // CB-10A: the persisted ref mapping decides the three-way ref
    // reconciliation (RFC §8.5).
    let direction = match (&mapping_outcome, direction) {
        // Both moved incompatibly at the mapping level: persist Diverged and
        // move neither side, whatever the checkpoint classifier thought.
        (Some((mapping, decision, _)), _)
            if decision.action == super::ref_mapping::ThreeWayAction::Diverged =>
        {
            super::ref_mapping::persist_diverged_status(&repo, working_copy, mapping)?;
            // CB-13C F3: the divergence status was PERSISTED (a mutation)
            // before this terminal event; `Refused` would promise
            // before-any-mutation falsely. The honest outcome is Failed.
            journal.emit_lossy(BridgeEventKind::Reconcile {
                direction: ReconcileDirectionCode::Diverged,
                outcome: EventOutcome::Failed,
                refusal_class: Some(RefusalClass::MappingDiverged),
            });
            return Err(git_error(format!(
                "Git and Atomic both advanced beyond the last mutually observed mapping for view '{}'; \
                 neither side was moved. Resolve with 'atomic view insert' from the other side or \
                 'git merge', then re-run 'atomic git bridge reconcile'",
                current.view
            )));
        }
        // CB-10A review R4: a mapping Import resolves Git HEAD, so the mapped
        // ref's movement is only reconcilable through it when the mapped tip
        // IS the HEAD commit. A movement independent of HEAD (detached
        // published draft, moved or renamed ref) is refused — importing HEAD
        // instead would falsely mark the foreign movement synchronized.
        (Some((mapping, decision, _)), _)
            if decision.action == super::ref_mapping::ThreeWayAction::Import =>
        {
            let head_tip = observe_head(&git)
                .map_err(observation_error)?
                .oid()
                .map(|oid| oid.to_string());
            let mapped_tip = mapping.local_ref.as_deref().and_then(|reference| {
                git.find_reference(reference)
                    .ok()
                    .and_then(|reference| reference.target())
                    .map(|oid| oid.to_string())
            });
            match (head_tip, mapped_tip) {
                (Some(head), Some(mapped)) if head == mapped => ReconcileDirection::GitToAtomic,
                _ => {
                    refused_after_observation(
                        ReconcileDirectionCode::GitToAtomic,
                        RefusalClass::MappedRefIndependentMove,
                    );
                    return Err(git_error(format!(
                        "the mapped ref '{}' for view '{}' advanced beyond the last observed \
                         mapping independently of Git HEAD; no automatic import was performed. \
                         Reconcile the mapped ref explicitly (resolve it with 'git update-ref' or \
                         re-publish), then re-run 'atomic git bridge reconcile'",
                        mapping.local_ref.as_deref().unwrap_or("<none>"),
                        current.view
                    )));
                }
            }
        }
        // CB-10A review R4: a mapping Export behind an aligned checkpoint is
        // a stale bookkeeping row, not a movement — the checkpoint classifier
        // decides, and the Neither arm refreshes the observation without
        // moving anything. A PROVEN mapping Export (the closure containment
        // proofs resolved the both-moved case to Export) reconciles through
        // the mapped pair under exact verified state instead of being
        // silently healed: the export projection runs, moving the mapped ref
        // under its expected-old lease.
        (Some((_, decision, _)), _)
            if decision.action == super::ref_mapping::ThreeWayAction::Export
                && decision.git_moved
                && decision.atomic_moved =>
        {
            ReconcileDirection::AtomicToGit
        }
        (Some(_), ReconcileDirection::AtomicToGit) => ReconcileDirection::AtomicToGit,
        (Some(_), ReconcileDirection::GitToAtomic) => ReconcileDirection::GitToAtomic,
        (Some(_), ReconcileDirection::Neither) => ReconcileDirection::Neither,
        // The checkpoint classifier saw divergence the mapping has nothing to
        // add to: refuse to overwrite either side.
        (Some(_), ReconcileDirection::Diverged) => ReconcileDirection::Diverged,
        (None, direction) => direction,
    };

    match direction {
        ReconcileDirection::Neither => {
            drop(git);
            drop(repo);
            drop(workspace);
            // Review R5: the Neither/verify-refresh path previously
            // bypassed the sink. verify_at is an observation (no
            // mutation): its failure is a true refusal. The mapping
            // refresh below mutates bookkeeping, so its failure is
            // terminal with unverified effects (Failed).
            let snapshot = match verify_at(root) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    journal.emit_lossy(BridgeEventKind::Reconcile {
                        direction: ReconcileDirectionCode::Neither,
                        outcome: EventOutcome::Refused,
                        refusal_class: Some(RefusalClass::VerifyFailed),
                    });
                    return Err(error);
                }
            };
            // CB-10A: a verified-aligned pair refreshes the mapping
            // observation (a lost refresh heals as bookkeeping, never as a
            // spurious movement). CB-10A review R4: only a mapped ref tip
            // that IS the verified HEAD commit may be bound as the exported
            // state; anything else keeps the previous export binding.
            {
                let repo =
                    Repository::open_with_budget(root, budget).map_err(CliError::from)?;
                let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
                let git = open_git(root)?;
                let verified_export = match &mapping_outcome {
                    Some((mapping, _, _)) => {
                        let mapped_tip = mapping.local_ref.as_deref().and_then(|reference| {
                            git.find_reference(reference)
                                .ok()
                                .and_then(|reference| reference.target())
                                .map(|oid| oid.to_string())
                        });
                        (mapped_tip.as_deref() == Some(snapshot.git_head.as_str()))
                            .then(|| snapshot.git_head.clone())
                    }
                    None => None,
                };
                if let Err(error) = super::ref_mapping::refresh_mapping_observation(
                    &repo,
                    working_copy,
                    &git,
                    &snapshot.view,
                    verified_export.as_deref(),
                    None,
                ) {
                    journal.emit_lossy(BridgeEventKind::Reconcile {
                        direction: ReconcileDirectionCode::Neither,
                        outcome: EventOutcome::Failed,
                        refusal_class: Some(RefusalClass::MappingRefreshFailed),
                    });
                    return Err(error);
                }
            }
            print_success("Git HEAD already matches the current Atomic view");
            journal.emit_lossy(BridgeEventKind::Reconcile {
                direction: ReconcileDirectionCode::Neither,
                outcome: EventOutcome::NoChange,
                refusal_class: None,
            });
            Ok(())
        }
        ReconcileDirection::GitToAtomic => {
            // Review R6: the notice is emitted *before* the import, which
            // may run for a long time — an active session must learn about
            // the external transition before the potentially long work,
            // not after it.
            emit_notice(
                WatchNoticeKind::ExternalHeadChange,
                "Git HEAD moved; the workspace is reconciling",
            );
            // `Import::run` reopens the repository. Release these handles first
            // so redb does not reject a second open in the same process.
            drop(git);
            drop(repo);
            drop(workspace);
            let imported = import_git_to_atomic_budgeted(root, budget);
            // Review R5: an import failure may already have applied effects
            // (synthesis is incremental); `Failed` is the honest terminal
            // class — `Refused` would claim before-any-mutation falsely.
            journal.emit_lossy(BridgeEventKind::Reconcile {
                direction: ReconcileDirectionCode::GitToAtomic,
                outcome: if imported.is_ok() {
                    EventOutcome::Applied
                } else {
                    EventOutcome::Failed
                },
                refusal_class: if imported.is_ok() {
                    None
                } else {
                    Some(RefusalClass::ImportFailed)
                },
            });
            imported
        }
        ReconcileDirection::AtomicToGit => {
            // CB-13D (RFC §11.2 rule 4): the metadata-only watch daemon
            // never projects Atomic→Git. Export belongs to an explicit
            // command boundary; the reactive path records the suppression
            // and surfaces a notice/remediation instead of moving refs.
            if !budget.allows_projection() {
                refused_after_observation(
                    ReconcileDirectionCode::AtomicToGit,
                    RefusalClass::MetadataOnlyExportSuppressed,
                );
                emit_notice(
                    WatchNoticeKind::ExportSuppressed,
                    "the Atomic view advanced beyond Git HEAD; reactive export is suppressed — \
                     run 'atomic git bridge reconcile' explicitly",
                );
                return Err(git_error(
                    "the bridge watch daemon reconciles metadata only and never exports \
                     Atomic→Git; run 'atomic git bridge reconcile' explicitly to project",
                ));
            }
            let projected = (|| -> CliResult<()> {
                project_atomic_to_git(root, &repo, working_copy, &git, &current)?;
                drop(git);
                drop(repo);
                drop(workspace);
                let snapshot = verify_at(root)?;
                write_workspace_metadata(root, &snapshot)?;
                // CB-10A: the export moved the mapped branch under its expected-old
                // lease and the snapshot verified HEAD; record the fresh
                // Synchronized observation bound to that exact verified tip.
                {
                    let repo = Repository::open_with_budget(root, budget)
                        .map_err(CliError::from)?;
                    let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
                    let git = open_git(root)?;
                    super::ref_mapping::refresh_mapping_observation(
                        &repo,
                        working_copy,
                        &git,
                        &snapshot.view,
                        Some(snapshot.git_head.as_str()),
                        None,
                    )?;
                }
                print_success("Projected the current Atomic view to Git HEAD");
                Ok(())
            })();
            // Review R5: projection may have partially applied before the
            // failure; `Refused` would claim before-any-mutation falsely.
            journal.emit_lossy(BridgeEventKind::Reconcile {
                direction: ReconcileDirectionCode::AtomicToGit,
                outcome: if projected.is_ok() {
                    EventOutcome::Applied
                } else {
                    EventOutcome::Failed
                },
                refusal_class: if projected.is_ok() {
                    None
                } else {
                    Some(RefusalClass::ProjectionFailed)
                },
            });
            projected
        }
        ReconcileDirection::Diverged => {
            journal.emit_lossy(BridgeEventKind::Reconcile {
                direction: ReconcileDirectionCode::Diverged,
                outcome: EventOutcome::Refused,
                refusal_class: Some(RefusalClass::CheckpointDiverged),
            });
            emit_notice(
                WatchNoticeKind::UnsafeState,
                "Git HEAD and Atomic state both changed since the bridge checkpoint; \
                 resolve the divergence explicitly",
            );
            Err(git_error(
                "Git HEAD and Atomic state both changed since the bridge checkpoint; reconcile the divergence explicitly",
            ))
        }
    }
}

fn switch(target: &str) -> CliResult<()> {
    let root = find_repository_root()?;
    // The transaction brackets the FileWatcher with atomic-bridge enter/leave,
    // including failure cleanup; bridge-originated writes inside the span are
    // self-suppressed so they never feed back into reconciliation.
    bracket_bridge_transaction(
        watcher_config(&root),
        &format!("bridge-switch:{target}"),
        || switch_transaction(&root, target),
    )
}

fn switch_transaction(root: &Path, target: &str) -> CliResult<()> {
    let checkpoint = read_workspace_metadata(root)?
        .ok_or_else(|| git_error("bridge switch requires an existing checkpoint; run 'atomic git bridge reconcile' first"))?;
    let mut repo = Repository::open(root).map_err(CliError::from)?;

    // Shared workspace transaction: switch is a mutating boundary that owns
    // Git refs, the Git index, and the Atomic working copy together, so it
    // selects an explicit mode and retains WorkspaceTxn authority for the
    // entire body. The baseline and view come from the working-copy record.
    let workspace = enter_workspace(&mut repo, WorkspaceTxnMode::Reconcile)?;
    let working_copy = workspace.working_copy();

    let git = open_git(root)?;
    let current = current_heads(&repo, working_copy, &git)?;
    // §7.5/CB-8A: an attached HEAD must name the current view; a detached
    // HEAD (Draft projection) must project exactly to it.
    let aligned_view = workspace_head_aligned(&repo, working_copy, &git)?.0;
    if aligned_view != current.view || !checkpoint_matches_snapshot(&checkpoint, &current) {
        return Err(git_error(
            "current Git and Atomic state does not match the bridge checkpoint; reconcile before switching",
        ));
    }

    let target_state = repo
        .get_view_info(target)
        .map_err(CliError::from)?
        .state
        .to_string();
    let current_paths = git_head_paths(&git)?;
    let target_paths = git_branch_paths(&git, target)?;
    plan_switch_collisions(root, &current_paths, &target_paths).map_err(git_error)?;
    let policy = conversion_policy(&git)?;
    let target_project = repo.project_tree(target, &policy).map_err(|error| {
        git_error(format!(
            "cannot build complete prospective projection for Atomic view '{target}': {error}"
        ))
    })?;
    drop(git);

    // After collision-specific checks, require the full clean/equality
    // invariant. The already-open Atomic handle is reused (redb admits one
    // open per process); the Git handle reopens for the verifier.
    {
        let git = open_git(root)?;
        verify_with(&repo, working_copy, &git)?;
    }

    if std::env::var("ATOMIC_TEST_BRIDGE_FAIL_BEFORE_MATERIALIZE").as_deref() == Ok("1") {
        return Err(git_error(
            "bridge switch stopped before Atomic materialization by ATOMIC_TEST_BRIDGE_FAIL_BEFORE_MATERIALIZE",
        ));
    }

    // The current TREE projection is not view-scoped enough to build a reliable
    // manifest for an inactive view. For this foreground MVP, materialize the
    // target exactly once, verify Atomic considers it clean, then project that
    // result into Git. The full RFC replaces this with a view-scoped manifest
    // so Git can be prepared before filesystem mutation.
    repo.switch_view(working_copy, target)
        .map_err(CliError::from)?;
    let atomic_status = repo
        .status(working_copy, StatusOptions::default())
        .map_err(CliError::from)?;
    if !atomic_status.is_clean() {
        return Err(git_error(
            "Atomic materialization did not produce a clean target view",
        ));
    }
    let git = open_git(root)?;
    require_clean_git_index(&git)?;
    let target_tree_oid = write_project_tree(&git, &target_project)?;
    let target_commit_oid =
        find_or_create_target_commit(&git, target, target_tree_oid, &target_state)?;
    let target_ref = format!("refs/heads/{target}");
    update_target_branch(&repo, working_copy, &git, &target_ref, target_commit_oid)?;
    git.set_head(&target_ref)
        .map_err(|error| git_error(format!("cannot attach Git HEAD to '{target}': {error}")))?;
    let target_tree = git
        .find_tree(target_tree_oid)
        .map_err(|error| git_error(format!("cannot read target Git tree: {error}")))?;
    let mut index = git
        .index()
        .map_err(|error| git_error(format!("cannot read Git index: {error}")))?;
    index
        .read_tree(&target_tree)
        .and_then(|_| index.write())
        .map_err(|error| git_error(format!("cannot reset Git index to target tree: {error}")))?;
    drop(index);
    drop(target_tree);
    drop(git);
    drop(repo);
    drop(workspace);

    let snapshot = verify_at(root)?;
    write_workspace_metadata(root, &snapshot)?;
    // CB-10A: the switch moved the target's mapped ref under its lease and
    // the snapshot verified HEAD; record the fresh Synchronized observation
    // bound to that exact verified tip.
    {
        let repo = Repository::open(root).map_err(CliError::from)?;
        let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
        let git = open_git(root)?;
        super::ref_mapping::refresh_mapping_observation(
            &repo,
            working_copy,
            &git,
            &snapshot.view,
            Some(snapshot.git_head.as_str()),
            None,
        )?;
    }
    let message = format!("Switched Git and Atomic to view '{target}'");
    print_success(&message);
    Ok(())
}


/// The projection commit's committer signature: Git config first, then the
/// repository's configured default author, then the system identity. The
/// projection commits are DERIVED artifacts of Atomic's verified state —
/// with no Git config (a fresh machine or an isolated test HOME) they must
/// not refuse; they commit as the system rather than fabricating user
/// attribution (review CB-9C hermeticity).
pub(crate) fn projection_signature(git: &GitRepository) -> Result<git2::Signature<'static>, CliError> {
    if let Ok(signature) = git.signature() {
        return Ok(signature);
    }
    git2::Signature::now("atomic", "atomic@invalid")
        .map_err(|error| git_error(format!("cannot build the projection signature: {error}")))
}

fn git_head_paths(git: &GitRepository) -> CliResult<BTreeSet<String>> {
    let tree = git
        .head()
        .and_then(|head| head.peel_to_commit())
        .and_then(|commit| commit.tree())
        .map_err(|error| git_error(format!("cannot read current Git HEAD tree: {error}")))?;
    Ok(read_git_tree(git, &tree)?.into_keys().collect())
}

fn git_branch_paths(git: &GitRepository, branch: &str) -> CliResult<BTreeSet<String>> {
    let reference_name = format!("refs/heads/{branch}");
    let tree = git
        .find_reference(&reference_name)
        .and_then(|reference| reference.peel_to_commit())
        .and_then(|commit| commit.tree())
        .map_err(|error| {
            if error.code() == git2::ErrorCode::NotFound {
                git_error(format!(
                    "target Git branch '{branch}' does not exist; bridge switch requires an existing target branch"
                ))
            } else {
                git_error(format!("cannot read target Git branch '{branch}': {error}"))
            }
        })?;
    Ok(read_git_tree(git, &tree)?.into_keys().collect())
}

fn plan_switch_collisions(
    root: &Path,
    current_paths: &BTreeSet<String>,
    target_paths: &BTreeSet<String>,
) -> Result<(), String> {
    for path in target_paths.difference(current_paths) {
        let relative = Path::new(path);
        let mut parent = relative.parent();
        while let Some(component) = parent {
            if component.as_os_str().is_empty() {
                break;
            }
            match fs::symlink_metadata(root.join(component)) {
                Ok(metadata) if !metadata.file_type().is_dir() => {
                    return Err(format!(
                        "cannot switch: target path '{path}' has non-directory parent '{}'",
                        component.display()
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "cannot inspect target path parent '{}': {error}",
                        component.display()
                    ));
                }
            }
            parent = component.parent();
        }

        match fs::symlink_metadata(root.join(relative)) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                return Err(format!(
                    "cannot switch: target file path '{path}' is occupied by a directory"
                ));
            }
            Ok(_) => {
                return Err(format!(
                    "cannot switch: target tracked path '{path}' is occupied in the working tree"
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("cannot inspect target path '{path}': {error}"));
            }
        }
    }
    Ok(())
}

fn checkpoint_matches_snapshot(checkpoint: &BridgeCheckpoint, snapshot: &BridgeSnapshot) -> bool {
    checkpoint.view == snapshot.view
        && checkpoint.atomic_state == snapshot.atomic_state
        && checkpoint.git_head == snapshot.git_head
        && checkpoint.git_tree == snapshot.git_tree
}

fn find_or_create_target_commit(
    git: &GitRepository,
    target: &str,
    target_tree_oid: Oid,
    atomic_state: &str,
) -> CliResult<Oid> {
    let target_ref = format!("refs/heads/{target}");
    let parent_oid = match git.find_reference(&target_ref) {
        Ok(reference) => {
            let commit = reference.peel_to_commit().map_err(|error| {
                git_error(format!("cannot read target branch '{target}': {error}"))
            })?;
            if commit.tree_id() == target_tree_oid {
                return Ok(commit.id());
            }
            // Append to the target branch so updating it is a fast-forward;
            // never parent from another branch and force-move target history.
            commit.id()
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => git
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(|error| git_error(format!("cannot read current Git HEAD commit: {error}")))?
            .id(),
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect target branch '{target}': {error}"
            )))
        }
    };
    let parent = git
        .find_commit(parent_oid)
        .map_err(|error| git_error(format!("cannot read target parent commit: {error}")))?;
    let tree = git
        .find_tree(target_tree_oid)
        .map_err(|error| git_error(format!("cannot read target Git tree: {error}")))?;
    let signature = projection_signature(git)?;
    let message = format!(
        "Atomic bridge switch projection\n\nAtomic-View: {target}\nAtomic-State: {atomic_state}\n"
    );
    git.commit(None, &signature, &signature, &message, &tree, &[&parent])
        .map_err(|error| {
            git_error(format!(
                "cannot create target compatibility commit: {error}"
            ))
        })
}

fn update_target_branch(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
    target_ref: &str,
    commit: Oid,
) -> CliResult<()> {
    journal_and_move_branch(
        repo,
        working_copy,
        git,
        target_ref,
        commit,
        "atomic bridge switch",
    )
}

fn import_git_to_atomic(root: &Path) -> CliResult<()> {
    import_git_to_atomic_budgeted(root, ReconcileEffectBudget::Command)
}

/// One Git→Atomic import under an explicit effect budget (CB-13D review R1).
/// The metadata-only budget never bootstraps a repository, never edits Git
/// admin files, and never materializes: those are command-boundary effects.
/// Every writable open in the path carries the budget so pending recovery
/// defers instead of replaying.
fn import_git_to_atomic_budgeted(root: &Path, budget: ReconcileEffectBudget) -> CliResult<()> {
    let git = open_git(root)?;
    let head = observe_head(&git).map_err(observation_error)?;
    // §7.5: a detached HEAD imports behind its commit into the workspace
    // head-map/checkpoint view, or a fresh ephemeral `git/<oid>` Draft view.
    // An attached HEAD keeps the historical branch-name behavior.
    let (view, detached_tip) = match &head {
        HeadObservation::Attached { symref, .. } => (
            symref
                .strip_prefix("refs/heads/")
                .map(str::to_string)
                .ok_or_else(|| {
                    git_error(format!("Git HEAD symref '{symref}' is not a local branch"))
                })?,
            None,
        ),
        HeadObservation::Detached { oid } => {
            let oid_hex = oid.to_string();
            let mut repo = Repository::open_with_budget(root, budget).map_err(CliError::from)?;
            let target = repo
                .detached_import_target(&oid_hex)
                .map_err(CliError::from)?;
            let (view, created) = match target {
                atomic_repository::DetachedImportTarget::Mapped { view } => (view, false),
                // CB-13D ::24 R1: an ephemeral view creation is a
                // metadata-changing import step — under the metadata-only
                // reactive budget the boundary defers with the unsafe-state
                // remediation instead of creating the view (the refused
                // head adoption must not fall through into a
                // metadata-changing import).
                atomic_repository::DetachedImportTarget::Ephemeral { view }
                    if budget.is_metadata_only() =>
                {
                    return Err(CliError::Repository(
                        atomic_repository::RepositoryError::ReactiveDeferred {
                            detail: format!(
                                "detached Git HEAD '{oid_hex}' would create ephemeral view                                  '{view}'; that metadata-changing import step is deferred to                                  an explicit 'atomic git bridge reconcile'"
                            ),
                        },
                    ));
                }
                atomic_repository::DetachedImportTarget::Ephemeral { view } => {
                    // §7.5: the ephemeral Draft's parent is the nearest bound
                    // Shared workspace view (the checkpoint's view when it is
                    // Shared), else none.
                    let parent = read_workspace_metadata(root)?
                        .map(|checkpoint| checkpoint.view)
                        .filter(|name| {
                            repo.get_view_info(name)
                                .map(|info| info.scope == atomic_core::pristine::ViewScope::Shared)
                                .unwrap_or(false)
                        });
                    match parent.as_deref() {
                        Some(parent) => repo
                            .create_draft_view(&view, parent)
                            .map_err(CliError::from)?,
                        None => repo
                            .create_view_with_identity(
                                &view,
                                atomic_core::pristine::ViewScope::Draft,
                                None,
                            )
                            .map_err(CliError::from)?,
                    }
                    (view, true)
                }
            };
            drop(repo);
            if created {
                print_success(&format!(
                    "Mapped detached Git HEAD to ephemeral view '{view}'"
                ));
            }
            (view, Some(oid_hex))
        }
        HeadObservation::Unborn { symref } => {
            return Err(git_error(format!("Git HEAD is unborn at '{symref}'")));
        }
        HeadObservation::MissingTarget { symref } => {
            return Err(git_error(format!(
                "Git HEAD target '{symref}' does not exist"
            )));
        }
    };
    require_clean_git_worktree(&git)?;
    let git_head = git
        .head()
        .ok()
        .and_then(|head| head.target())
        .map(|oid| oid.to_string())
        .ok_or_else(|| git_error("Git HEAD does not point directly to a commit"))?;
    drop(git);

    // `git checkout -b topic` changes only the symbolic branch while keeping
    // the bound commit/tree. Reuse the existing validated Atomic closure by
    // creating a self-contained shared view instead of replaying Git history.
    if detached_tip.is_none() {
        if let Some(checkpoint) = read_workspace_metadata(root)? {
            let mut repo = Repository::open_with_budget(root, budget).map_err(CliError::from)?;
            let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
            let view_exists = repo.view_exists(&view).map_err(CliError::from)?;
            if git_head == checkpoint.git_head && !view_exists {
                let source_state = repo
                    .get_view_info(&checkpoint.view)
                    .map_err(CliError::from)?
                    .state
                    .to_string();
                if source_state != checkpoint.atomic_state {
                    return Err(git_error(format!(
                    "bridge checkpoint claims Atomic state {} for view '{}', but the actual state is {}",
                    checkpoint.atomic_state, checkpoint.view, source_state
                )));
                }
                let git = open_git(root)?;
                verify_complete_equivalence(
                    &repo,
                    &checkpoint.view,
                    root,
                    &git,
                    "branch-only bridge adoption",
                )?;
                drop(git);

                // Validate and resolve the complete dependency closure before the
                // first mutation. A legacy source with missing dependency metadata
                // must not leave a partially created adoption view behind.
                let closure = {
                    let txn = repo.pristine().read_txn().map_err(|error| {
                        CliError::from(RepositoryError::Database(error.to_string()))
                    })?;
                    let source = txn
                        .get_view(&checkpoint.view)
                        .map_err(|error| {
                            CliError::from(RepositoryError::Database(error.to_string()))
                        })?
                        .ok_or_else(|| {
                            CliError::from(RepositoryError::ViewNotFound {
                                name: checkpoint.view.clone(),
                            })
                        })?;
                    let visibility =
                        graph_visibility_closure(&txn, &source).map_err(CliError::from)?;
                    let mut hashes = Vec::with_capacity(visibility.len());
                    for change_id in visibility.iter_dependency_first().copied() {
                        let hash = txn
                            .get_external(change_id)
                            .map_err(|error| {
                                CliError::from(RepositoryError::Database(error.to_string()))
                            })?
                            .ok_or_else(|| {
                                CliError::from(RepositoryError::Database(format!(
                                    "visible change {} has no external hash",
                                    change_id.get()
                                )))
                            })?;
                        hashes.push(hash);
                    }
                    hashes
                };
                repo.create_shared_view(&view).map_err(CliError::from)?;
                for hash in closure {
                    repo.insert_change(&hash, InsertOptions::with_dependencies().view(&view))
                        .map_err(CliError::from)?;
                }
                repo.align_to_view(working_copy, &view)
                    .map_err(CliError::from)?;
                repo.reindex_working_copy(working_copy)
                    .map_err(CliError::from)?;
                drop(repo);
                let snapshot = verify_at(root)?;
                write_workspace_metadata(root, &snapshot)?;
                // CB-10A: the adopted branch's mapping starts synchronized.
                // CB-13D ::24 R1: the branch-only path keeps the caller's
                // budget — `refresh_import_mapping` would release the
                // workspace lease and reopen writable state through an
                // unbudgeted Command budget, letting the metadata-only
                // daemon escape its grant.
                refresh_import_mapping_budgeted(root, &snapshot, budget)?;
                print_success("Adopted new Git branch from the existing Atomic closure");
                return Ok(());
            }
        }
    }

    let was_detached = detached_tip.is_some();
    Import {
        incremental: true,
        branch: Some(view.clone()),
        no_vault: true,
        with_crdt: false,
        skip_checkpoint_refresh: true,
        detached_tip,
        reactive_budget: (budget.is_metadata_only()).then_some(budget),
        ..Import::default()
    }
    .run()?;

    // Incremental import deliberately preserves the old Atomic working-copy
    // pointer when Git switched to another branch. Adopt the imported view by
    // aligning deferred TREE metadata and rebuilding FILE_INDEX only; neither
    // operation writes source files.
    let mut repo = Repository::open_with_budget(root, budget).map_err(CliError::from)?;
    let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
    repo.align_to_view(working_copy, &view)
        .map_err(CliError::from)?;
    repo.reindex_working_copy(working_copy)
        .map_err(CliError::from)?;
    drop(repo);

    // §7.5: a detached HEAD verifies against its view projection instead of a
    // branch name.
    let snapshot = if was_detached {
        verify_at_detached_ok(root)?
    } else {
        verify_at(root)?
    };
    write_workspace_metadata(root, &snapshot)?;
    // CB-10A: the import moved the workspace view; record the fresh
    // Synchronized observation under a journaled RefMapping operation.
    refresh_import_mapping_budgeted(root, &snapshot, budget)?;
    record_last_bridge_operation(root);
    print_success("Reconciled Git HEAD with the current Atomic view");
    Ok(())
}

/// Refresh the view's ref mapping after a successful Git→Atomic import (the
/// import observed Git's new tip and imported it into the view). No verified
/// export happened: the previous export binding is preserved (CB-10A review
/// R4 — only a verified export transition binds the export state).
fn refresh_import_mapping(root: &Path, snapshot: &BridgeSnapshot) -> CliResult<()> {
    refresh_import_mapping_budgeted(root, snapshot, ReconcileEffectBudget::Command)
}

/// The mapping refresh under an explicit effect budget: the metadata-only
/// daemon defers instead of running recovery inside the writable open.
fn refresh_import_mapping_budgeted(
    root: &Path,
    snapshot: &BridgeSnapshot,
    budget: ReconcileEffectBudget,
) -> CliResult<()> {
    let repo = Repository::open_with_budget(root, budget).map_err(CliError::from)?;
    let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
    let git = open_git(root)?;
    super::ref_mapping::refresh_mapping_observation(
        &repo,
        working_copy,
        &git,
        &snapshot.view,
        None,
        None,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckpointRefresh {
    Refreshed,
    SkippedNoGit,
    SkippedUnsupportedHead,
    SkippedViewMismatch,
}

/// Refresh the local v2 checkpoint only when the existing bridge verifier can
/// prove that Git and the persisted Atomic working-copy view are fully aligned.
///
/// No-Git repositories and intentional branch/view mismatches are no-ops. The
/// latter is required by bridge incremental raw-switch adoption: `Import::run`
/// preserves the old Atomic pointer until `import_git_to_atomic` aligns it.
pub(crate) fn refresh_checkpoint_after_verified_import(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
    view: &str,
    expected_tree: &GitObjectId,
    git_tree: &GitObjectId,
) -> CliResult<CheckpointRefresh> {
    if repo
        .desired_view_name(working_copy)
        .map_err(CliError::from)?
        != view
    {
        return Ok(CheckpointRefresh::SkippedViewMismatch);
    }
    let head = git
        .head()
        .map_err(|error| git_error(format!("Git HEAD is unavailable: {error}")))?;
    if head.shorthand() != Some(view) {
        return Ok(CheckpointRefresh::SkippedViewMismatch);
    }
    let commit = head
        .peel_to_commit()
        .map_err(|error| git_error(format!("cannot resolve Git HEAD: {error}")))?;
    let tree = commit
        .tree()
        .map_err(|error| git_error(format!("cannot read Git HEAD tree: {error}")))?;
    // HEAD-stability anchors on the commit's RAW tree; the projection
    // equivalence compares against the same tree MINUS the declared
    // exclusions (bridge-private paths and import-ignore patterns), which is
    // what `expected_tree` carries.
    if tree.id().as_bytes() != git_tree.as_bytes() {
        return Err(git_error(
            "Git HEAD changed after prospective import verification",
        ));
    }

    let policy = conversion_policy(git)?;
    let project = repo
        .project_tree(view, &policy)
        .map_err(|error| git_error(format!("cannot verify imported Atomic tree: {error}")))?;
    verify_prospective_equivalence(&project, expected_tree)
        .map_err(|error| git_error(format!("imported Atomic tree diverged from Git: {error}")))?;

    let observation = observe_git(repo.root()).map_err(observation_error)?;
    let GitObservation::Repository(observed) = observation else {
        return Ok(CheckpointRefresh::SkippedNoGit);
    };
    let HeadObservation::Attached { symref, oid } = observed.head else {
        return Ok(CheckpointRefresh::SkippedUnsupportedHead);
    };
    let tree_id = tree.id().to_string();
    let atomic_state = repo
        .get_view_info(view)
        .map_err(CliError::from)?
        .state
        .to_string();
    let checkpoint = BridgeCheckpoint {
        version: checkpoint::CHECKPOINT_VERSION,
        view: view.to_string(),
        atomic_state,
        atomic_manifest_root: Some(checkpoint::ManifestRootEvidence::git_tree(&tree_id)),
        git_head_symref: Some(symref),
        git_head: oid.to_string(),
        git_tree: tree_id.clone(),
        git_manifest_root: Some(checkpoint::ManifestRootEvidence::git_tree(&tree_id)),
        git_index_tree: observed.index.tree_oid.map(|oid| oid.to_string()),
        git_index_digest: Some(observed.index.canonical_digest.0),
        git_refs_digest: Some(observed.refs_digest.0),
        git_admin: Some(checkpoint::GitAdminIdentity {
            worktree_root: observed.paths.worktree_root,
            worktree_git_dir: observed.paths.worktree_git_dir,
            common_dir: observed.paths.common_dir,
            index_path: observed.paths.index_path,
        }),
    };
    checkpoint::write_checkpoint(repo.root(), &checkpoint).map_err(checkpoint_error)?;
    Ok(CheckpointRefresh::Refreshed)
}

pub(crate) fn refresh_checkpoint_if_aligned(root: &Path) -> CliResult<CheckpointRefresh> {
    let observation = observe_git(root).map_err(observation_error)?;
    let GitObservation::Repository(git) = observation else {
        return Ok(CheckpointRefresh::SkippedNoGit);
    };
    let HeadObservation::Attached { symref, .. } = &git.head else {
        // CB-8A: a Draft view under the view-scope HEAD policy stays detached
        // at its projection commit (RFC §8.1). When the detached commit is
        // exactly the draft's verified projection, the checkpoint is
        // refreshed the same way an attached Shared switch would be.
        return refresh_detached_checkpoint(root, &git.head);
    };
    let Some(branch) = symref.strip_prefix("refs/heads/") else {
        return Ok(CheckpointRefresh::SkippedUnsupportedHead);
    };

    let current_view = {
        let repo = Repository::open(root).map_err(CliError::from)?;
        let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
        repo.desired_view_name(working_copy)
            .map_err(CliError::from)?
    };
    if branch != current_view {
        return Ok(CheckpointRefresh::SkippedViewMismatch);
    }

    let snapshot = verify_at(root)?;
    write_workspace_metadata(root, &snapshot)?;
    Ok(CheckpointRefresh::Refreshed)
}

/// Refresh the verified checkpoint for a detached HEAD (CB-8A Draft policy).
///
/// Detached HEADs are only coherent with Draft views: the workspace is
/// verified against the view's projected tree and the checkpoint is written
/// with a `None` symref. Anything else stays unsupported.
fn refresh_detached_checkpoint(
    root: &Path,
    head: &HeadObservation,
) -> CliResult<CheckpointRefresh> {
    use atomic_core::pristine::ViewScope;

    let HeadObservation::Detached { oid } = head else {
        return Ok(CheckpointRefresh::SkippedUnsupportedHead);
    };
    let repo = Repository::open(root).map_err(CliError::from)?;
    let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
    let view = repo
        .desired_view_name(working_copy)
        .map_err(CliError::from)?;
    let view_state = repo
        .get_view_info(&view)
        .map_err(CliError::from)?
        .state
        .to_string();
    if repo.get_view_info(&view).map_err(CliError::from)?.scope != ViewScope::Draft {
        return Ok(CheckpointRefresh::SkippedUnsupportedHead);
    }
    let git = open_git(root)?;
    require_clean_git_worktree(&git)?;
    let commit = git
        .find_commit(*oid)
        .map_err(|error| git_error(format!("detached HEAD commit is unreadable: {error}")))?;
    let policy = conversion_policy(&git)?;
    let project = repo
        .project_tree(&view, &policy)
        .map_err(|error| git_error(format!("cannot project Atomic tree: {error}")))?;
    if project.git.root.as_bytes() != commit.tree_id().as_bytes() {
        return Ok(CheckpointRefresh::SkippedViewMismatch);
    }
    verify_complete_equivalence(&repo, &view, root, &git, "Git bridge verification")?;
    drop(repo);
    let snapshot = BridgeSnapshot {
        view,
        atomic_state: view_state,
        git_head: oid.to_string(),
        git_tree: commit.tree_id().to_string(),
    };
    write_workspace_metadata(root, &snapshot)?;
    Ok(CheckpointRefresh::Refreshed)
}

/// CB-13A R2: the operations-head layer of `bridge verify`, collected
/// read-only through the public operation log.
///
/// A repository-scope or working-copy-scope head that is diverged, or whose
/// head operation has no verified receipt yet, means the repository is still
/// completing an operation. The verdict fails with actionable guidance; it
/// never performs the recovery itself.
fn operation_layer_verdict(
    repo: &Repository,
    working_copy: atomic_core::types::WorkingCopyId,
) -> (&'static str, bool, String) {
    use atomic_core::operation::OperationScope;
    use atomic_repository::OperationVerificationState;
    let describe = |scope: OperationScope| -> Result<String, String> {
        let log = repo
            .operation_log(scope, None, false)
            .map_err(|error| error.to_string())?;
        match &log.head_state {
            atomic_repository::OperationHeadState::Empty => Ok(String::new()),
            atomic_repository::OperationHeadState::Single(head) => {
                let incomplete: Vec<String> = log
                    .entries
                    .iter()
                    .filter(|entry| {
                        entry.is_head && entry.verification != OperationVerificationState::Verified
                    })
                    .map(|entry| format!("{} ({:?})", entry.operation.id(), entry.verification))
                    .collect();
                if incomplete.is_empty() {
                    Ok(String::new())
                } else {
                    Err(format!(
                        "operation head is incomplete and requires recovery: {}",
                        incomplete.join(", ")
                    ))
                }
            }
            atomic_repository::OperationHeadState::Diverged(heads) => Err(format!(
                "operation heads are diverged and require consolidation: {}",
                heads
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    };
    let mut divergences = Vec::new();
    match describe(OperationScope::Repository) {
        Ok(detail) if detail.is_empty() => {}
        Ok(detail) | Err(detail) => divergences.push(format!("repository scope: {detail}")),
    }
    match describe(OperationScope::WorkingCopy(working_copy)) {
        Ok(detail) if detail.is_empty() => {}
        Ok(detail) | Err(detail) => divergences.push(format!("working-copy scope: {detail}")),
    }
    // CB-13A follow-up R2: incomplete heads are enumerated across ALL
    // working-copy scopes, not just the repository and the inspecting
    // working copy — another linked worktree's interrupted operation is
    // exactly the head the writable path would gate on.
    let all_scopes = repo
        .pristine()
        .read_txn()
        .and_then(|txn| {
            atomic_core::pristine::WorkingCopyTxnT::list_working_copies(&txn)
                .map(|records| {
                    records
                        .into_iter()
                        .map(|record| OperationScope::WorkingCopy(record.id))
                        .collect::<Vec<_>>()
                })
                .map_err(atomic_core::pristine::PristineError::from)
        })
        .unwrap_or_default();
    for scope in all_scopes {
        if scope == OperationScope::WorkingCopy(working_copy) {
            continue;
        }
        match describe(scope) {
            Ok(detail) if detail.is_empty() => {}
            Ok(detail) | Err(detail) => divergences.push(format!("{scope} scope: {detail}")),
        }
    }
    if divergences.is_empty() {
        (
            "operations",
            true,
            "repository and working-copy operation heads are complete".to_string(),
        )
    } else {
        (
            "operations",
            false,
            format!(
                "{} — inspect with `atomic op show`, then run a writable command so recovery completes",
                divergences.join("; ")
            ),
        )
    }
}

fn verify() -> CliResult<BridgeSnapshot> {
    let root = find_repository_root()?;
    // CB-13A R2: diagnosis is a read-only observation boundary. Opening for
    // operation inspection performs no migration, no deferred-tree alignment,
    // and no operation recovery, so an incomplete repository is *reported*
    // rather than silently repaired before any verdict is collected.
    let repo = Repository::open_readonly_for_operation_inspection(&root).map_err(CliError::from)?;
    let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
    let git = open_git(&root)?;
    let policy = conversion_policy(&git)?;
    let filter = GitAttributesFilter::for_repository(&root);

    // CB-11A: `atomic bridge verify` proves all five layers, or reports
    // structured divergence. Every check is read-only.
    let mut verdicts: Vec<(&'static str, bool, String)> = Vec::new();

    // Layer 0 — operations: the journal heads are complete and verified.
    // An incomplete or diverged head means the repository is mid-recovery;
    // diagnosis reports it instead of recovering before inspection.
    verdicts.push(operation_layer_verdict(&repo, working_copy));

    // Layer 1 — durable: working-copy desired state equals the view head.
    let view_name = repo
        .desired_view_name(working_copy)
        .map_err(CliError::from)?;
    let record = repo
        .working_copy_record(working_copy)
        .map_err(CliError::from)?;
    let view_info = repo.get_view_info(&view_name).map_err(CliError::from)?;
    let durable_ok = view_info.state == record.desired_state;
    verdicts.push((
        "durable",
        durable_ok,
        if durable_ok {
            format!(
                "working copy desired state {} matches view '{}' head",
                record.desired_state, view_name
            )
        } else {
            format!(
                "working copy desired state {} diverges from view '{}' head {}",
                record.desired_state, view_name, view_info.state
            )
        },
    ));

    // Layer 2 — manifest: the durable projection builds and equals the Git
    // HEAD tree.
    let mut projected_tree: Option<atomic_core::operation::GitObjectId> = None;
    match repo.project_tree(&view_name, &policy) {
        Ok(project) => {
            let head_tree: Option<git2::Oid> = git
                .head()
                .ok()
                .and_then(|head| head.peel_to_commit().ok())
                .and_then(|commit| commit.tree().ok())
                .map(|tree| tree.id());
            let projected = Some(project.git.root.clone());
            let manifest_ok = match (&projected, &head_tree) {
                (Some(projected), Some(head_tree)) => {
                    git2::Oid::from_bytes(projected.as_bytes()) == Ok(*head_tree)
                }
                _ => false,
            };
            verdicts.push((
                "manifest",
                manifest_ok,
                if manifest_ok {
                    format!(
                        "manifest root projects to Git HEAD tree {}",
                        head_tree.expect("checked above")
                    )
                } else {
                    format!(
                        "manifest projects to {:?} but Git HEAD tree is {:?}",
                        projected, head_tree
                    )
                },
            ));
            projected_tree = projected;
        }
        Err(error) => {
            verdicts.push((
                "manifest",
                false,
                format!("repository manifest could not be projected: {error}"),
            ));
        }
    }

    // Layer 3 — index: the stage-0 index tree matches the manifest projection.
    match atomic_repository::observe_git_index(&root, &policy) {
        Ok(index_state) => {
            let index_ok = match (&index_state.tree, &projected_tree) {
                (Some(index_tree), Some(projected)) => index_tree == projected,
                _ => false,
            };
            verdicts.push((
                "index",
                index_ok,
                if index_ok {
                    "Git stage-0 index tree matches the manifest projection".to_string()
                } else {
                    format!(
                        "index tree {:?} does not match manifest projection {:?}",
                        index_state.tree, projected_tree
                    )
                },
            ));
        }
        Err(error) => {
            verdicts.push(("index", false, format!("cannot observe Git index: {error}")));
        }
    }

    // Layer 4 — worktree: physical bytes match under the conversion policy.
    let worktree_verdict = (|| -> Result<bool, String> {
        let index_state = atomic_repository::observe_git_index(&root, &policy)
            .map_err(|error| error.to_string())?;
        let observed =
            atomic_repository::observe_worktree(&root, Some(&index_state), &filter, &policy)
                .map_err(|error| error.to_string())?;
        let project = repo
            .project_tree(&view_name, &policy)
            .map_err(|error| error.to_string())?;
        let report = atomic_repository::compare_project_to_worktree(&project, &observed, &policy);
        Ok(report.is_equivalent())
    })();
    match worktree_verdict {
        Ok(true) => verdicts.push((
            "worktree",
            true,
            "physical worktree matches under the conversion policy".to_string(),
        )),
        Ok(false) => verdicts.push((
            "worktree",
            false,
            "physical worktree diverges from the durable projection".to_string(),
        )),
        Err(reason) => verdicts.push(("worktree", false, format!("unobservable: {reason}"))),
    }

    // Layer 5 — snapshot: pending snapshot/remainder are content-addressed
    // and re-readable (staged/remainder bytes survive reopen).
    let snapshot_status = repo.snapshot_status(working_copy).map_err(CliError::from)?;
    let snapshot_verdict: (bool, String) =
        match (&snapshot_status.snapshot, &snapshot_status.remainder) {
            (None, None) => (true, "no pending snapshot or remainder".to_string()),
            (Some(snapshot), remainder) => match repo.change_store().load_change(snapshot) {
                Ok(_) => (
                    true,
                    format!(
                        "pending snapshot {} re-readable{}",
                        snapshot.to_base32(),
                        remainder
                            .map(|remainder| format!(
                                "; durable remainder {} re-readable",
                                remainder.to_base32()
                            ))
                            .unwrap_or_default()
                    ),
                ),
                Err(error) => (
                    false,
                    format!(
                        "pending snapshot {} cannot be re-read: {error}",
                        snapshot.to_base32()
                    ),
                ),
            },
            _ => (true, "durable remainder only".to_string()),
        };
    verdicts.push(("snapshot", snapshot_verdict.0, snapshot_verdict.1));

    // CB-13A R2: the deterministic diagnostic census — missing changes,
    // invalid bindings, stale/broken working-copy records, orphaned WIP
    // refs and divergent ref mappings, enumerated read-only with
    // actionable per-class results.
    let census = super::census::census_classes(&repo, &root, &view_name);
    for class in &census {
        verdicts.push((class.name, class.clean, class.detail.clone()));
    }

    let failed: Vec<_> = verdicts.iter().filter(|(_, ok, _)| !ok).collect();
    for (layer, ok, detail) in &verdicts {
        let mark = if *ok { "✓" } else { "✗" };
        println!("{mark} layer {layer}: {detail}");
    }
    if !failed.is_empty() {
        return Err(git_error(format!(
            "bridge verification found {} diverged layer(s): {}",
            failed.len(),
            failed
                .iter()
                .map(|(layer, _, _)| layer.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    // CB-8A/§7.5: an attached HEAD follows the branch rule; a detached HEAD
    // aligns with the desired view's projection.
    let (branch, head, tree) = workspace_head_aligned(&repo, working_copy, &git)?;
    require_clean_git_worktree(&git)?;
    verify_complete_equivalence(&repo, &branch, &root, &git, "Git bridge verification")?;
    let atomic_state = repo
        .get_view_info(&branch)
        .map_err(CliError::from)?
        .state
        .to_string();

    Ok(BridgeSnapshot {
        view: branch,
        atomic_state,
        git_head: head.to_string(),
        git_tree: tree.to_string(),
    })
}

fn verify_at(root: &Path) -> CliResult<BridgeSnapshot> {
    // CB-13A R2: the same read-only diagnostic boundary as `verify` — the
    // repository is observed for operation inspection without recovery.
    let repo = Repository::open_readonly_for_operation_inspection(root).map_err(CliError::from)?;
    let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
    let git = open_git(root)?;
    // CB-13A R2: an incomplete or diverged operation head may never pass
    // the inspection helper when the writable path would recover or gate
    // on it — the operations-head verdict is a hard gate here too.
    let (_, heads_ok, heads_detail) = operation_layer_verdict(&repo, working_copy);
    if !heads_ok {
        return Err(git_error(format!(
            "operation journal heads are incomplete or diverged; run recovery first: {heads_detail}"
        )));
    }
    let (branch, head, tree) = workspace_head_aligned(&repo, working_copy, &git)?;
    require_clean_git_worktree(&git)?;
    let atomic_status = repo
        .status(working_copy, StatusOptions::default())
        .map_err(CliError::from)?;
    // CB-13C review R1: status itself never writes telemetry. This helper
    // runs only inside explicit bridge command flows (reconcile/switch/
    // import/publish), so it is the authorized sink boundary that records
    // the degradation the status metrics just returned — consent-gated via
    // `for_repository`.
    if let Some(metrics) = atomic_status.change_source_metrics() {
        if metrics.fallback_count > 0 {
            emit_change_source_fallback(
                &repo,
                metrics.source,
                metrics.fallback_source,
                metrics.fallback_reason.as_ref(),
            );
        }
    }
    if !atomic_status.is_clean() {
        return Err(git_error("Atomic working copy is not clean"));
    }
    verify_complete_equivalence(&repo, &branch, root, &git, "Git bridge verification")?;
    let atomic_state = repo
        .get_view_info(&branch)
        .map_err(CliError::from)?
        .state
        .to_string();

    Ok(BridgeSnapshot {
        view: branch,
        atomic_state,
        git_head: head.to_string(),
        git_tree: tree.to_string(),
    })
}

/// Verify a workspace whose HEAD may be detached (RFC §7.5, CB-8A Draft
/// policy): an attached HEAD follows the branch rule; a detached HEAD aligns
/// when its commit tree exactly equals the desired view's projection.
pub(crate) fn verify_at_detached_ok(root: &Path) -> CliResult<BridgeSnapshot> {
    verify_at(root)
}

/// Verify against an already-open repository handle (same-process redb opens
/// are exclusive, so callers holding a writable handle must reuse it).
pub(crate) fn verify_with(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
) -> CliResult<BridgeSnapshot> {
    let (branch, head, tree) = workspace_head_aligned(repo, working_copy, git)?;
    require_clean_git_worktree(git)?;
    let atomic_status = repo
        .status(working_copy, StatusOptions::default())
        .map_err(CliError::from)?;
    if !atomic_status.is_clean() {
        return Err(git_error("Atomic working copy is not clean"));
    }
    verify_complete_equivalence(repo, &branch, repo.root(), git, "Git bridge verification")?;
    let atomic_state = repo
        .get_view_info(&branch)
        .map_err(CliError::from)?
        .state
        .to_string();

    Ok(BridgeSnapshot {
        view: branch,
        atomic_state,
        git_head: head.to_string(),
        git_tree: tree.to_string(),
    })
}

/// Resolve the (view, HEAD commit, HEAD tree) triple for the workspace with
/// the §7.5/CB-8A detached alignment rule: an attached HEAD must name the
/// desired view; a detached HEAD must project exactly to the desired view.
pub(crate) fn workspace_head_aligned(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
) -> CliResult<(String, git2::Oid, git2::Oid)> {
    let head_obs = observe_head(git).map_err(observation_error)?;
    match &head_obs {
        HeadObservation::Attached { symref, .. } => {
            let branch = symref
                .strip_prefix("refs/heads/")
                .map(str::to_string)
                .ok_or_else(|| {
                    git_error(format!("Git HEAD symref '{symref}' is not a local branch"))
                })?;
            let desired_view = repo
                .desired_view_name(working_copy)
                .map_err(CliError::from)?;
            if branch != desired_view {
                return Err(git_error(format!(
                    "Git branch '{branch}' does not match current Atomic view '{desired_view}'"
                )));
            }
            let head = git
                .head()
                .map_err(|error| git_error(format!("Git HEAD is unavailable: {error}")))?;
            let oid = head
                .target()
                .ok_or_else(|| git_error("Git HEAD does not point directly to a commit"))?;
            let tree = head
                .peel_to_commit()
                .and_then(|commit| commit.tree())
                .map_err(|error| git_error(format!("cannot read Git HEAD tree: {error}")))?;
            Ok((branch, oid, tree.id()))
        }
        HeadObservation::Detached { oid } => {
            let desired_view = repo
                .desired_view_name(working_copy)
                .map_err(CliError::from)?;
            let commit = git.find_commit(*oid).map_err(|error| {
                git_error(format!("cannot read detached Git HEAD commit: {error}"))
            })?;
            let tree_id = commit.tree_id();
            let policy = conversion_policy(git)?;
            let project = repo.project_tree(&desired_view, &policy).map_err(|error| {
                git_error(format!(
                    "cannot project Atomic view '{desired_view}': {error}"
                ))
            })?;
            if project.git.root.as_bytes() != tree_id.as_bytes() {
                return Err(git_error(format!(
                    "detached Git HEAD tree {tree_id} does not match the projection of view '{desired_view}'"
                )));
            }
            Ok((desired_view, *oid, tree_id))
        }
        HeadObservation::Unborn { symref } => {
            Err(git_error(format!("Git HEAD is unborn at '{symref}'")))
        }
        HeadObservation::MissingTarget { symref } => Err(git_error(format!(
            "Git HEAD target '{symref}' does not exist"
        ))),
    }
}

pub(crate) fn current_heads(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
) -> CliResult<BridgeSnapshot> {
    let head = git
        .head()
        .map_err(|error| git_error(format!("Git HEAD is unavailable: {error}")))?;
    let oid = head
        .target()
        .ok_or_else(|| git_error("Git HEAD does not point directly to a commit"))?;
    let tree = head
        .peel_to_commit()
        .and_then(|commit| commit.tree())
        .map_err(|error| git_error(format!("cannot read Git HEAD tree: {error}")))?;
    let view = repo
        .desired_view_name(working_copy)
        .map_err(CliError::from)?;
    let atomic_state = repo
        .get_view_info(&view)
        .map_err(CliError::from)?
        .state
        .to_string();
    Ok(BridgeSnapshot {
        view,
        atomic_state,
        git_head: oid.to_string(),
        git_tree: tree.id().to_string(),
    })
}

pub(crate) fn read_checkpoint_observation(
    root: &Path,
) -> CliResult<Option<BridgeCheckpointObservation>> {
    read_workspace_metadata(root).map(|checkpoint| {
        checkpoint.map(|checkpoint| BridgeCheckpointObservation {
            view: checkpoint.view,
            atomic_state: checkpoint.atomic_state,
            git_head: checkpoint.git_head,
            git_tree: checkpoint.git_tree,
        })
    })
}

pub(crate) fn read_workspace_metadata(root: &Path) -> CliResult<Option<BridgeCheckpoint>> {
    checkpoint::read_checkpoint(root).map_err(checkpoint_error)
}

pub(crate) fn open_git(root: &Path) -> CliResult<GitRepository> {
    GitRepository::open(root).map_err(|error| git_error(format!("cannot open repository: {error}")))
}

fn current_attached_git_branch(git: &GitRepository) -> CliResult<String> {
    match observe_head(git).map_err(observation_error)? {
        HeadObservation::Attached { symref, .. } => symref
            .strip_prefix("refs/heads/")
            .map(str::to_string)
            .ok_or_else(|| git_error(format!("Git HEAD symref '{symref}' is not a local branch"))),
        HeadObservation::Detached { .. } => {
            Err(git_error("Git HEAD must be attached to a local branch"))
        }
        HeadObservation::Unborn { symref } => {
            Err(git_error(format!("Git HEAD is unborn at '{symref}'")))
        }
        HeadObservation::MissingTarget { symref } => Err(git_error(format!(
            "Git HEAD target '{symref}' does not exist"
        ))),
    }
}

/// The branch name used for reconcile direction classification.
///
/// An attached HEAD keeps its branch; a detached HEAD keeps the workspace
/// view (§7.5): the symbolic branch did not change, so only commit/tree
/// movement counts as a Git-side change and routes to the §7.5 import.
fn current_git_branch_or_view(git: &GitRepository, view: &str) -> CliResult<String> {
    match observe_head(git).map_err(observation_error)? {
        HeadObservation::Attached { symref, .. } => symref
            .strip_prefix("refs/heads/")
            .map(str::to_string)
            .ok_or_else(|| git_error(format!("Git HEAD symref '{symref}' is not a local branch"))),
        HeadObservation::Detached { .. } => Ok(view.to_string()),
        HeadObservation::Unborn { symref } => {
            Err(git_error(format!("Git HEAD is unborn at '{symref}'")))
        }
        HeadObservation::MissingTarget { symref } => Err(git_error(format!(
            "Git HEAD target '{symref}' does not exist"
        ))),
    }
}

pub(crate) fn require_clean_git_worktree(git: &GitRepository) -> CliResult<()> {
    let index = git
        .index()
        .map_err(|error| git_error(format!("cannot read Git index: {error}")))?;
    if index.has_conflicts() {
        return Err(git_error("Git index contains unresolved conflicts"));
    }

    let mut options = GitStatusOptions::new();
    options
        .include_untracked(false)
        .recurse_untracked_dirs(false);
    let statuses = git
        .statuses(Some(&mut options))
        .map_err(|error| git_error(format!("cannot inspect Git status: {error}")))?;
    if statuses.iter().any(|entry| tracked_delta(entry.status())) {
        return Err(git_error(
            "Git has staged or unstaged changes to tracked paths",
        ));
    }
    Ok(())
}

fn require_matching_clean_workspaces<'repo>(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &'repo GitRepository,
    require_atomic_clean: bool,
) -> CliResult<(String, git2::Oid, git2::Tree<'repo>)> {
    let branch = current_attached_git_branch(git)?;
    let desired_view = repo
        .desired_view_name(working_copy)
        .map_err(CliError::from)?;
    if branch != desired_view {
        return Err(git_error(format!(
            "Git branch '{branch}' does not match current Atomic view '{desired_view}'"
        )));
    }

    require_clean_git_worktree(git)?;

    if require_atomic_clean {
        let atomic_status = repo
            .status(working_copy, StatusOptions::default())
            .map_err(CliError::from)?;
        if !atomic_status.is_clean() {
            return Err(git_error("Atomic working copy is not clean"));
        }
    }

    let head = git
        .head()
        .map_err(|error| git_error(format!("Git HEAD is unavailable: {error}")))?;
    let oid = head
        .target()
        .ok_or_else(|| git_error("Git HEAD does not point directly to a commit"))?;
    let tree = head
        .peel_to_commit()
        .and_then(|commit| commit.tree())
        .map_err(|error| git_error(format!("cannot read Git HEAD tree: {error}")))?;
    Ok((branch, oid, tree))
}

pub(crate) fn project_atomic_to_git(
    root: &Path,
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
    current: &BridgeSnapshot,
) -> CliResult<()> {
    let head = git
        .head()
        .map_err(|error| git_error(format!("Git HEAD is unavailable: {error}")))?;
    if !head.is_branch() {
        return Err(git_error("Git HEAD must be attached to a local branch"));
    }
    let branch = head
        .shorthand()
        .ok_or_else(|| git_error("Git branch name is not valid UTF-8"))?;
    let desired_view = repo
        .desired_view_name(working_copy)
        .map_err(CliError::from)?;
    if branch != desired_view {
        return Err(git_error(format!(
            "Git branch '{branch}' does not match current Atomic view '{desired_view}'"
        )));
    }

    let atomic_status = repo
        .status(working_copy, StatusOptions::default())
        .map_err(CliError::from)?;
    if !atomic_status.is_clean() {
        return Err(git_error("Atomic working copy is not clean"));
    }
    require_clean_git_index(git)?;

    // Build and verify the complete graph-derived projection before creating
    // any Git object, commit, ref, or index mutation. The Git index may still
    // describe the old HEAD on an Atomic→Git reconcile, so only the worktree
    // layer is applicable until the new tree is published.
    let policy = conversion_policy(git)?;
    let project = repo
        .project_tree(&desired_view, &policy)
        .map_err(|error| git_error(format!("cannot build complete Atomic projection: {error}")))?;
    let index = observe_git_index(root, &policy)
        .map_err(|error| git_error(format!("cannot observe Git index: {error}")))?;
    let filter = GitAttributesFilter::for_repository(root);
    let worktree = observe_worktree(root, Some(&index), &filter, &policy)
        .map_err(|error| git_error(format!("cannot observe Git worktree: {error}")))?;
    let report = compare_project_to_worktree(&project, &worktree, &policy);
    if !report.is_equivalent() {
        return Err(equivalence_error(
            "Atomic-to-Git bridge projection",
            &report,
        ));
    }
    let tree_oid = write_project_tree(git, &project)?;
    let tree = git
        .find_tree(tree_oid)
        .map_err(|error| git_error(format!("cannot read projected Git tree: {error}")))?;
    let parent = head
        .peel_to_commit()
        .map_err(|error| git_error(format!("cannot read Git HEAD commit: {error}")))?;
    let signature = projection_signature(git)?;
    let message = format!(
        "Atomic bridge projection\n\nAtomic-View: {}\nAtomic-State: {}\n",
        current.view, current.atomic_state
    );
    // Create the commit object without moving any ref first; the ref move is
    // journaled with its operation ID before it becomes visible.
    let commit_oid = git
        .commit(None, &signature, &signature, &message, &tree, &[&parent])
        .map_err(|error| git_error(format!("cannot create Atomic projection commit: {error}")))?;

    let mut index = git
        .index()
        .map_err(|error| git_error(format!("cannot read Git index: {error}")))?;
    index
        .read_tree(&tree)
        .and_then(|_| index.write())
        .map_err(|error| git_error(format!("cannot reset Git index to projected tree: {error}")))?;
    drop(index);
    drop(tree);

    // Journal-before-visibility for the branch ref move.
    let branch_ref = format!("refs/heads/{branch}");
    journal_and_move_branch(
        repo,
        working_copy,
        git,
        &branch_ref,
        commit_oid,
        "atomic bridge projection",
    )?;

    Ok(())
}

fn require_clean_git_index(git: &GitRepository) -> CliResult<()> {
    let index = git
        .index()
        .map_err(|error| git_error(format!("cannot read Git index: {error}")))?;
    if index.has_conflicts() {
        return Err(git_error("Git index contains unresolved conflicts"));
    }
    let mut options = GitStatusOptions::new();
    options
        .include_untracked(false)
        .recurse_untracked_dirs(false);
    let statuses = git
        .statuses(Some(&mut options))
        .map_err(|error| git_error(format!("cannot inspect Git status: {error}")))?;
    let staged: Vec<String> = statuses
        .iter()
        .filter(|entry| staged_delta(entry.status()))
        .map(|entry| {
            format!(
                "{:?} {}",
                entry.status(),
                entry.path().unwrap_or("<unnamed>")
            )
        })
        .collect();
    if !staged.is_empty() {
        return Err(git_error(format!(
            "Git index has staged changes: {}",
            staged.join("; ")
        )));
    }
    Ok(())
}

fn staged_delta(status: Status) -> bool {
    status.intersects(
        Status::INDEX_NEW
            | Status::INDEX_MODIFIED
            | Status::INDEX_DELETED
            | Status::INDEX_RENAMED
            | Status::INDEX_TYPECHANGE
            | Status::CONFLICTED,
    )
}

fn write_project_tree(git: &GitRepository, project: &ProjectTree) -> CliResult<Oid> {
    if project.git.algorithm != atomic_core::operation::GitHashAlgorithm::Sha1 {
        return Err(git_error(format!(
            "libgit2 cannot safely publish {:?} Atomic projections",
            project.git.algorithm
        )));
    }
    let odb = git
        .odb()
        .map_err(|error| git_error(format!("cannot open Git object database: {error}")))?;
    for (expected, object) in project.git.objects.iter() {
        let kind = match object.kind {
            GitObjectKind::Blob => ObjectType::Blob,
            GitObjectKind::Tree => ObjectType::Tree,
        };
        let written = odb.write(kind, &object.bytes).map_err(|error| {
            git_error(format!(
                "cannot write projected Git {kind:?} object: {error}"
            ))
        })?;
        if written.as_bytes() != expected.as_bytes() {
            return Err(git_error(format!(
                "Git object database returned {written} for projected object {expected:?}"
            )));
        }
    }
    Oid::from_bytes(project.git.root.as_bytes()).map_err(|error| {
        git_error(format!(
            "projected Git tree identity {:?} is unsupported: {error}",
            project.git.root
        ))
    })
}

#[derive(Default)]
struct GitTreeNode {
    files: BTreeMap<String, Vec<u8>>,
    directories: BTreeMap<String, GitTreeNode>,
}

fn write_git_tree(git: &GitRepository, files: &BTreeMap<String, Vec<u8>>) -> CliResult<Oid> {
    let mut root = GitTreeNode::default();
    for (path, content) in files {
        let mut components = path.split('/').peekable();
        let mut node = &mut root;
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                node.files.insert(component.to_string(), content.clone());
            } else {
                node = node.directories.entry(component.to_string()).or_default();
            }
        }
    }
    write_git_tree_node(git, &root)
}

fn write_git_tree_node(git: &GitRepository, node: &GitTreeNode) -> CliResult<Oid> {
    let mut builder = git
        .treebuilder(None)
        .map_err(|error| git_error(format!("cannot create Git tree builder: {error}")))?;
    for (name, content) in &node.files {
        let oid = git
            .blob(content)
            .map_err(|error| git_error(format!("cannot create Git blob '{name}': {error}")))?;
        builder
            .insert(name, oid, 0o100644)
            .map_err(|error| git_error(format!("cannot add Git blob '{name}': {error}")))?;
    }
    for (name, child) in &node.directories {
        let oid = write_git_tree_node(git, child)?;
        builder
            .insert(name, oid, 0o040000)
            .map_err(|error| git_error(format!("cannot add Git tree '{name}': {error}")))?;
    }
    builder
        .write()
        .map_err(|error| git_error(format!("cannot write Git tree: {error}")))
}

fn read_filesystem(root: &Path) -> CliResult<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    collect_filesystem(root, root, &mut files)?;
    Ok(files)
}

fn collect_filesystem(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> CliResult<()> {
    let entries = fs::read_dir(directory).map_err(|error| {
        git_error(format!(
            "cannot read working tree directory '{}': {error}",
            directory.display()
        ))
    })?;
    for entry in entries {
        let entry =
            entry.map_err(|error| git_error(format!("cannot read working tree: {error}")))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("filesystem traversal remains below repository root");
        let relative = path_to_git_string(relative)?;
        if excluded_path(&relative) {
            continue;
        }
        let kind = entry
            .file_type()
            .map_err(|error| git_error(format!("cannot inspect '{relative}': {error}")))?;
        if kind.is_dir() {
            collect_filesystem(root, &path, files)?;
        } else if kind.is_file() {
            let content = fs::read(&path)
                .map_err(|error| git_error(format!("cannot read '{relative}': {error}")))?;
            files.insert(relative, content);
        } else {
            return Err(git_error(format!(
                "unsupported working tree entry '{relative}'"
            )));
        }
    }
    Ok(())
}

fn path_to_git_string(path: &Path) -> CliResult<String> {
    let components: Result<Vec<_>, _> = path
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| git_error("working tree contains a non-UTF-8 path"))
        })
        .collect();
    Ok(components?.join("/"))
}

fn tracked_delta(status: Status) -> bool {
    status.intersects(
        Status::INDEX_NEW
            | Status::INDEX_MODIFIED
            | Status::INDEX_DELETED
            | Status::INDEX_RENAMED
            | Status::INDEX_TYPECHANGE
            | Status::WT_MODIFIED
            | Status::WT_DELETED
            | Status::WT_RENAMED
            | Status::WT_TYPECHANGE
            | Status::CONFLICTED,
    )
}

fn read_git_tree(
    git: &GitRepository,
    tree: &git2::Tree<'_>,
) -> CliResult<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    collect_git_tree(git, tree, "", &mut files)?;
    Ok(files)
}

fn collect_git_tree(
    git: &GitRepository,
    tree: &git2::Tree<'_>,
    prefix: &str,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> CliResult<()> {
    for entry in tree.iter() {
        let name = entry
            .name()
            .ok_or_else(|| git_error("Git tree contains a non-UTF-8 path"))?;
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if excluded_path(&path) {
            continue;
        }

        match (entry.kind(), entry.filemode()) {
            (Some(ObjectType::Tree), 0o040000) => {
                let child = git.find_tree(entry.id()).map_err(|error| {
                    git_error(format!("cannot read Git tree '{path}': {error}"))
                })?;
                collect_git_tree(git, &child, &path, files)?;
            }
            (Some(ObjectType::Blob), 0o100644 | 0o100755) => {
                let blob = git.find_blob(entry.id()).map_err(|error| {
                    git_error(format!("cannot read Git blob '{path}': {error}"))
                })?;
                files.insert(path, blob.content().to_vec());
            }
            _ => {
                return Err(git_error(format!(
                    "unsupported Git entry '{path}' with mode {:o}",
                    entry.filemode()
                )));
            }
        }
    }
    Ok(())
}

fn excluded_path(path: &str) -> bool {
    path == ".git"
        || path.starts_with(".git/")
        || path == ".atomicignore"
        || path == ".atomic"
        || path.starts_with(".atomic/")
        || path == ".vault"
        || path.starts_with(".vault/")
}

fn compare_file_sets(
    git: &BTreeMap<String, Vec<u8>>,
    atomic: &BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    let missing: Vec<_> = git
        .keys()
        .filter(|path| !atomic.contains_key(*path))
        .collect();
    let extra: Vec<_> = atomic
        .keys()
        .filter(|path| !git.contains_key(*path))
        .collect();
    let changed: Vec<_> = git
        .iter()
        .filter_map(|(path, content)| {
            atomic
                .get(path)
                .filter(|other| *other != content)
                .map(|_| path)
        })
        .collect();

    if missing.is_empty() && extra.is_empty() && changed.is_empty() {
        return Ok(());
    }

    Err(format!(
        "Git HEAD and Atomic view differ (missing in Atomic: {}; extra in Atomic: {}; different bytes: {})",
        format_paths(&missing),
        format_paths(&extra),
        format_paths(&changed)
    ))
}

fn format_paths<T: AsRef<str>>(paths: &[&T]) -> String {
    if paths.is_empty() {
        "none".to_string()
    } else {
        paths
            .iter()
            .map(|path| path.as_ref())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn write_workspace_metadata(root: &Path, snapshot: &BridgeSnapshot) -> CliResult<()> {
    checkpoint::write_verified_checkpoint(
        root,
        VerifiedCheckpointInput {
            view: &snapshot.view,
            atomic_state: &snapshot.atomic_state,
            git_head: &snapshot.git_head,
            git_tree: &snapshot.git_tree,
        },
    )
    .map(|_| ())
    .map_err(checkpoint_error)
}

fn checkpoint_error(error: checkpoint::CheckpointError) -> CliError {
    git_error(error.to_string())
}

fn observation_error(error: ObservationError) -> CliError {
    git_error(error.to_string())
}

fn git_error(message: impl Into<String>) -> CliError {
    CliError::GitError {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
#[cfg(test)]
fn record_last_bridge_operation_probe(root: &std::path::Path) -> CliResult<()> {
    // The marker writer is best-effort (returns ()). For the regression we
    // verify the WRITE against a real repository: the marker must appear.
    record_last_bridge_operation(root);
    let marker = Repository::canonical_dot_dir(root)
        .map_err(CliError::from)?
        .join("bridge/suppressed-operation.json");
    if marker.exists() {
        Ok(())
    } else {
        Err(git_error("the suppression attribution marker was not written"))
    }
}

/// CB-13D ::24 AC-4 regression: a reconciled pass records the JOURNAL
/// OperationId of its just-completed operation (plus the working-copy
/// heads) in the suppression attribution marker — never an invocation or
/// pid label. Failing before (no marker existed), passing after.
#[test]
fn reconciled_pass_attributes_its_journal_operation_for_suppression() {
    let directory = TestDirectory::new();
    let root = directory.0.clone();
    let mut repo = Repository::init_with_view(&root, "main").unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    // A journaled metadata operation (the ref-mapping lease) gives the
    // attribution a real just-completed journal head.
    repo.set_ref_mapping(
        working_copy,
        "main",
        Some(atomic_core::pristine::RefMapping {
            version: atomic_core::pristine::REF_MAPPING_VERSION,
            view_id: {
                let txn = repo.pristine().read_txn().unwrap();
                txn.get_view("main").unwrap().unwrap().id
            },
            view_name: "main".to_string(),
            scope: 1,
            local_ref: Some("refs/heads/main".to_string()),
            remote: None,
            last_observed_local: None,
            last_observed_remote: None,
            last_exported: None,
            last_exported_state: None,
            last_observed_atomic: None,
            status: atomic_core::pristine::RefSyncStatus::Synchronized,
        }),
    )
    .unwrap();
    drop(repo);
    assert!(record_last_bridge_operation_probe(&root).is_ok());

    let marker = root.join(".atomic/bridge/suppressed-operation.json");
    let bytes = std::fs::read_to_string(&marker).expect("the attribution marker exists");
    let value: serde_json::Value = serde_json::from_str(&bytes).unwrap();
    let operation_id = value
        .get("operation_id")
        .and_then(|id| id.as_str())
        .expect("the marker carries the journal OperationId");
    assert!(
        operation_id.len() >= 32 && operation_id.bytes().all(|b| b.is_ascii_alphanumeric()),
        "the attribution is a journal OperationId, not a pid/label: {operation_id}"
    );
}

    use super::*;
    use git2::Signature;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "atomic-bridge-collision-{}-{unique}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn paths(entries: &[&str]) -> BTreeSet<String> {
        entries.iter().map(|path| (*path).to_string()).collect()
    }

    fn files(entries: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
        entries
            .iter()
            .map(|(path, content)| ((*path).to_string(), content.to_vec()))
            .collect()
    }

    fn init_git_with_commit(root: &Path, branch: &str) {
        let repository = GitRepository::init(root).unwrap();
        repository
            .set_head(&format!("refs/heads/{branch}"))
            .unwrap();
        fs::write(root.join("tracked.txt"), b"tracked\n").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_oid).unwrap();
        let signature = Signature::now("Atomic Test", "atomic@example.com").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
    }

    #[test]
    fn enable_records_opt_in_and_disable_rolls_back() {
        // Review R4: the consent path is exercised live through the real
        // repository configuration file — the recorded opt-in, the honest
        // default-enablement refusal, and the tested rollback — not
        // through constructed booleans.
        let directory = TestDirectory::new();
        let root = directory.0.clone();
        let repo = Repository::init_with_view(&root, "main").unwrap();
        drop(repo);
        init_git_with_commit(&root, "main");

        // Default: un-opted; default (automatic) enablement refuses.
        let config = bridge_config(&root).unwrap();
        assert!(!config.git.bridge.enabled);
        assert!(config.git.bridge.default_enablement().is_err());

        // The enable action records the explicit opt-in.
        assert!(record_bridge_opt_in(&root, true).unwrap());
        let config = bridge_config(&root).unwrap();
        assert!(config.git.bridge.enabled, "explicit enable records consent");
        // Explicit opt-in is NOT default-rollout approval: the gate still
        // refuses while rollout gates are unmeasured.
        assert!(config.git.bridge.default_enablement().is_err());

        // Idempotent re-enable changes nothing.
        assert!(!record_bridge_opt_in(&root, true).unwrap());

        // The disable action rolls the consent back.
        assert!(record_bridge_opt_in(&root, false).unwrap());
        let config = bridge_config(&root).unwrap();
        assert!(!config.git.bridge.enabled, "disable rolls the consent back");
        assert_eq!(
            config.git.bridge.default_enablement(),
            Err(atomic_config::DefaultEnablementRefusal::NotOptedIn)
        );
    }

    #[test]
    fn checkpoint_refresh_skips_intentional_import_view_mismatch() {
        let root = TestDirectory::new();
        let repo = Repository::init_with_view(&root.0, "main").unwrap();
        drop(repo);
        init_git_with_commit(&root.0, "topic");

        assert_eq!(
            refresh_checkpoint_if_aligned(&root.0).unwrap(),
            CheckpointRefresh::SkippedViewMismatch
        );
        assert!(!checkpoint::checkpoint_path(&root.0).exists());
    }

    #[test]
    fn direction_classifier_detects_no_change() {
        assert_eq!(
            classify_direction("main", "git-1", "atomic-1", "main", "git-1", "atomic-1"),
            ReconcileDirection::Neither
        );
    }

    #[test]
    fn direction_classifier_detects_git_only_change() {
        assert_eq!(
            classify_direction("main", "git-1", "atomic-1", "main", "git-2", "atomic-1"),
            ReconcileDirection::GitToAtomic
        );
    }

    #[test]
    fn direction_classifier_detects_branch_only_change() {
        assert_eq!(
            classify_direction("main", "git-1", "atomic-1", "topic", "git-1", "atomic-1"),
            ReconcileDirection::GitToAtomic
        );
    }

    #[test]
    fn direction_classifier_detects_atomic_only_change() {
        assert_eq!(
            classify_direction("main", "git-1", "atomic-1", "main", "git-1", "atomic-2"),
            ReconcileDirection::AtomicToGit
        );
    }

    #[test]
    fn direction_classifier_detects_divergence() {
        assert_eq!(
            classify_direction("main", "git-1", "atomic-1", "main", "git-2", "atomic-2"),
            ReconcileDirection::Diverged
        );
    }

    #[test]
    fn checkpoint_match_requires_every_recorded_head() {
        let checkpoint = BridgeCheckpoint::legacy_compatible("main", "atomic-1", "git-1", "tree-1");
        let mut snapshot = BridgeSnapshot {
            view: "main".to_string(),
            atomic_state: "atomic-1".to_string(),
            git_head: "git-1".to_string(),
            git_tree: "tree-1".to_string(),
        };
        assert!(checkpoint_matches_snapshot(&checkpoint, &snapshot));
        snapshot.git_tree = "tree-2".to_string();
        assert!(!checkpoint_matches_snapshot(&checkpoint, &snapshot));
    }

    #[test]
    fn comparison_accepts_identical_paths_and_bytes() {
        let git = files(&[("README.md", b"same"), ("src/main.rs", b"fn main() {}")]);
        assert_eq!(compare_file_sets(&git, &git), Ok(()));
    }

    #[test]
    fn comparison_reports_path_and_byte_differences() {
        let git = files(&[("changed", b"git"), ("missing", b"value")]);
        let atomic = files(&[("changed", b"atomic"), ("extra", b"value")]);
        let error = compare_file_sets(&git, &atomic).unwrap_err();
        assert!(error.contains("missing"));
        assert!(error.contains("extra"));
        assert!(error.contains("changed"));
    }

    #[test]
    fn collision_planner_allows_new_target_under_existing_directories() {
        let root = TestDirectory::new();
        fs::create_dir(root.0.join("src")).unwrap();

        assert_eq!(
            plan_switch_collisions(&root.0, &paths(&["README.md"]), &paths(&["src/lib.rs"])),
            Ok(())
        );
    }

    #[test]
    fn collision_planner_rejects_file_at_new_target_path() {
        let root = TestDirectory::new();
        fs::write(root.0.join("new.txt"), b"untracked").unwrap();

        let error =
            plan_switch_collisions(&root.0, &BTreeSet::new(), &paths(&["new.txt"])).unwrap_err();
        assert!(error.contains("new.txt"));
        assert!(error.contains("occupied"));
    }

    #[test]
    fn collision_planner_rejects_directory_at_new_target_file_path() {
        let root = TestDirectory::new();
        fs::create_dir(root.0.join("config")).unwrap();

        let error =
            plan_switch_collisions(&root.0, &BTreeSet::new(), &paths(&["config"])).unwrap_err();
        assert!(error.contains("config"));
        assert!(error.contains("directory"));
    }

    #[test]
    fn collision_planner_rejects_non_directory_parent() {
        let root = TestDirectory::new();
        fs::write(root.0.join("src"), b"not a directory").unwrap();

        let error =
            plan_switch_collisions(&root.0, &BTreeSet::new(), &paths(&["src/lib.rs"])).unwrap_err();
        assert!(error.contains("src/lib.rs"));
        assert!(error.contains("non-directory parent 'src'"));
    }

    #[cfg(unix)]
    #[test]
    fn collision_planner_rejects_symlink_at_new_target_path() {
        use std::os::unix::fs::symlink;

        let root = TestDirectory::new();
        symlink("missing", root.0.join("linked")).unwrap();

        let error =
            plan_switch_collisions(&root.0, &BTreeSet::new(), &paths(&["linked"])).unwrap_err();
        assert!(error.contains("linked"));
        assert!(error.contains("occupied"));
    }

    #[test]
    fn collision_planner_ignores_paths_already_tracked_by_current_head() {
        let root = TestDirectory::new();
        fs::write(root.0.join("tracked.txt"), b"current").unwrap();

        assert_eq!(
            plan_switch_collisions(&root.0, &paths(&["tracked.txt"]), &paths(&["tracked.txt"])),
            Ok(())
        );
    }

    #[test]
    fn bridge_exclusions_are_exact_or_recursive() {
        assert!(excluded_path(".atomicignore"));
        assert!(excluded_path(".atomic/data"));
        assert!(excluded_path(".vault/intents/x"));
        assert!(!excluded_path("src/.atomic/file"));
        assert!(!excluded_path(".atomicignore.example"));
    }

    /// CB-13C observability privacy: the structured event journal lives
    /// under `.atomic/`, which the Git projection excludes — telemetry is
    /// never exported into Git metadata or objects.
    #[test]
    fn bridge_telemetry_is_excluded_from_git_projection() {
        assert!(excluded_path(".atomic/bridge/events.jsonl"));
        assert!(!excluded_path("bridge/events.jsonl"));
    }
}
