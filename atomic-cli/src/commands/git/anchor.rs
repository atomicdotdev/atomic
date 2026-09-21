//! CB-7A: `git bridge enable` anchoring phase (RFC §7, Phase 7 task 1).
//!
//! After the advisory hook integration is installed, enable proves complete
//! repository/index/worktree equivalence under one conversion policy and only
//! then creates the signed Anchor binding and the verified working-copy
//! checkpoint (RFC §6, §7, §12). Passive enable reports typed refusals as
//! notices so pre-existing repositories keep succeeding; explicit remediation
//! flags (`--adopt-atomic`) and the explicitly unavailable `--adopt-git` fail
//! closed with their typed refusals.

use std::path::Path;

use atomic_repository::{BridgeAnchorError, BridgeAnchorRefusal, Repository};

use super::binding::load_signing_key;
use super::bridge::{current_heads, project_atomic_to_git};
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_success, print_warning};

/// Options for the anchoring phase of `bridge enable`.
#[derive(Debug, Default, Clone)]
pub(crate) struct EnableAnchorOptions {
    /// Run the Atomic-origin projection remediation first (explicit consent).
    pub adopt_atomic: bool,
    /// Always refused: unbound Git adoption ships with Phase 9 (CB-9B/9C).
    pub adopt_git: bool,
    /// Explicit Ed25519 signing key file for the Anchor binding.
    pub binding_key_file: Option<std::path::PathBuf>,
    /// CB-10B (RFC §8.6): attach the bounded `changes.pack` fallback to the
    /// Anchor's binding tree. The pack is assembled from the repository's own
    /// hash-verified change store; private material is refused closed.
    pub with_changes_pack: bool,
}

/// What the anchoring phase concluded.
pub(crate) enum EnableAnchorOutcome {
    /// The signed Anchor was created (or replayed idempotently) and the
    /// verified checkpoint was written.
    Anchored {
        binding_id: String,
        view: String,
        git_head: String,
        replayed: bool,
    },
    /// A typed, actionable refusal. Enable still reports success for its hook
    /// integration, but no Anchor or checkpoint was created.
    NotAnchored { refusal: BridgeAnchorRefusal },
}

/// Typed refusal for explicitly requested, explicitly unavailable behavior.
pub(crate) fn adopt_git_refusal() -> CliError {
    CliError::GitError {
        message: BridgeAnchorRefusal::AdoptGitUnavailable.to_string(),
    }
}

/// Run the CB-7A anchoring phase for `bridge enable`.
pub(crate) fn run_enable_anchor(root: &Path, options: &EnableAnchorOptions) -> CliResult<()> {
    if options.adopt_git {
        return Err(adopt_git_refusal());
    }
    match enable_anchor_inner(root, options) {
        Ok(EnableAnchorOutcome::Anchored {
            binding_id,
            view,
            git_head,
            replayed,
        }) => {
            if replayed {
                print_success(&format!(
                    "Anchor binding {binding_id} already exists; repeat enable changed nothing (view '{view}', Git HEAD {git_head})"
                ));
            } else {
                print_success(&format!(
                    "Anchor binding {binding_id} created for view '{view}' at Git HEAD {git_head}; verified working-copy checkpoint written"
                ));
            }
            print_info("Unbound Git adoption and --adopt-git remain unavailable until Phase 9 foreign synthesis (CB-9B/9C).");
            Ok(())
        }
        Ok(EnableAnchorOutcome::NotAnchored { refusal }) => {
            print_warning(&format!("bridge anchor not created: {refusal}"));
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn enable_anchor_inner(
    root: &Path,
    options: &EnableAnchorOptions,
) -> CliResult<EnableAnchorOutcome> {
    let Some(key_file) = options.binding_key_file.as_ref() else {
        return Ok(EnableAnchorOutcome::NotAnchored {
            refusal: BridgeAnchorRefusal::DirtyState {
                detail: "no --binding-key-file was provided, so the Anchor cannot be signed; \
                         bridge enable signs the Anchor with an explicitly supplied Ed25519 \
                         key file only"
                    .to_string(),
            },
        });
    };
    let signer = load_signing_key(key_file)?;

    let mut repo = Repository::open(root.to_path_buf()).map_err(CliError::from)?;
    let git = super::bridge::open_git(root)?;

    if options.adopt_atomic {
        // Explicit remediation: project the Atomic state onto Git through the
        // safe Atomic-origin path (journaled ref move), then re-prove
        // equivalence. Pending work is preserved by refusing dirty inputs.
        let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
        let current = current_heads(&repo, working_copy, &git)?;
        project_atomic_to_git(root, &repo, working_copy, &git, &current).map_err(|error| {
            CliError::GitError {
                message: format!(
                    "adopt-atomic remediation refused: {error}; pending work is preserved \
                     until CB-7B — commit or record it, then retry"
                ),
            }
        })?;
        print_info(
            "Projected the Atomic state onto Git HEAD (--adopt-atomic); re-proving equivalence.",
        );
    }

    // CB-10B (RFC §8.6): assemble the bounded `changes.pack` fallback from the
    // repository's own hash-verified change store before the anchor publishes.
    let changes_pack = if options.with_changes_pack {
        let view = repo
            .desired_view_name(repo.require_working_copy_id().map_err(CliError::from)?)
            .map_err(CliError::from)?;
        let ordered: Vec<atomic_core::Hash> = repo
            .effective_history(Some(&view))
            .map_err(CliError::from)?
            .iter()
            .map(|entry| entry.hash)
            .collect();
        Some(super::binding::assemble_changes_pack_from_view(
            &repo,
            ordered.as_slice(),
        )?)
    } else {
        None
    };

    let anchor_outcome = match changes_pack.as_deref() {
        Some(pack) => repo.enable_bridge_anchor_with_changes_pack(&git, &signer, pack),
        None => repo.enable_bridge_anchor(&git, &signer),
    };
    match anchor_outcome {
        Ok(outcome) => Ok(EnableAnchorOutcome::Anchored {
            binding_id: outcome.binding.id().to_hex(),
            view: outcome.view,
            git_head: outcome.git_head,
            replayed: matches!(
                outcome.publication,
                atomic_repository::BindingPublication::Idempotent { .. }
            ),
        }),
        Err(BridgeAnchorError::Refusal(refusal)) => {
            Ok(EnableAnchorOutcome::NotAnchored { refusal })
        }
        Err(error) => Err(CliError::Repository(
            atomic_repository::RepositoryError::InvalidOperation {
                message: format!("bridge anchoring failed: {error}"),
            },
        )),
    }
}
