//! CB-13B: the transactional shadow → colocated-bridge cutover.
//!
//! The cutover is the ownership transfer that ends the two-writer window
//! between the legacy Git shadow pipeline and the colocated bridge. It is
//! planned from a read-only audit that inventories the durable schemas and
//! refuses unsupported or unready states before any mutation, journaled as a
//! repository-scoped [`OperationKind::Cutover`] metadata operation, and
//! reversed only through the journal's typed inverse (`Undo`).
//!
//! Order of a cutover under the canonical operation locks:
//!
//! 1. **Audit** (read-only): capability rows, schema markers, colocated
//!    Git presence. Unsupported requirements or a missing PATH_CLAIMS
//!    schema marker refuse with remediation and preserve the repository.
//! 2. **Lock** (canonical order): the common `bridge.lock` and the
//!    working-copy `operation.lock` are acquired together by
//!    `try_lock_operation`; every pristine write happens inside them.
//! 3. **Journal before the fence**: `prepare_metadata_operation` commits the
//!    `Cutover` operation — carrying its before-state and the inverse-derivable
//!    capability transition — BEFORE any fence write. That committed journal
//!    entry is the durable rollback point required by CB-13B.
//! 4. **Fence**: `apply_operation_metadata_locked` moves the
//!    `required-capability/git-bridge-cutover` row from `Absent` to exactly
//!    the leased version (never a substituted build maximum).
//! 5. **Verify**: `finalize_operation_verified` re-observes the lease against
//!    its expected-new value and appends the operation-level `Verified`
//!    receipt in one durable transaction. An interruption before this point
//!    leaves the head incomplete and is recovered by the standard head
//!    recovery on the next open; a completed fence re-runs as an idempotent
//!    already-fenced outcome.
//!
//! Legacy writer exclusion is structural: once the requirement row is
//! durable, [`Repository::require_legacy_shadow_write_allowed`] — consulted by
//! the legacy shadow commit lock — refuses with a typed error, so the shadow
//! pipeline cannot write after cutover even if it never consults the audit.

use atomic_core::operation::{
    ActorRef, EffectPlan, EffectTarget, EffectValue, FileKind, FileState, MetadataTarget,
    MetadataTransition, MetadataValue, OperationKind, OperationScope, RepoStateRef,
};
use atomic_core::pristine::{
    CapabilityTxnT, FileIndexV2TxnT, GitShaIndexTxnT, RequiredRepositoryCapability, ViewTxnT,
    WorkingCopyTxnT, BRIDGE_CUTOVER_CAPABILITY, SUPPORTED_REPOSITORY_CAPABILITIES,
};
use atomic_core::{Hash, OperationId, WorkingCopyId};

use super::git_observation::{observe_colocated_git_readiness, ColocatedGitForm, ColocatedGitReadiness};
use super::operation::{current_operation_timestamp_ms, pristine_error, working_copy_state_ref};
use super::workspace_txn::read_workspace_checkpoint;
use super::{OperationVerificationState, Repository, RepositoryCommonLockGuard};
use crate::RepositoryError;

/// The requirement version this build writes when it cuts a repository over.
const BRIDGE_CUTOVER_VERSION: u32 = 1;

const CUTOVER_ACTOR: &str = "git-bridge-cutover";

/// Disposition of one audited schema surface for the CB-13B cutover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CutoverAuditDisposition {
    /// Content-addressed immutable data the cutover never rewrites.
    PreservedImmutable,
    /// A derived cache rebuildable from the canonical graph.
    RebuildableCache,
    /// A schema marker the repository must already carry before cutover.
    Required,
    /// Not migrated by this build: cutover refuses instead of guessing.
    Refused,
}

/// One audited schema surface with its bounded disposition and detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutoverSchemaAudit {
    pub item: String,
    pub disposition: CutoverAuditDisposition,
    pub detail: String,
}

/// Read-only CB-13B cutover audit over the repository's durable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeCutoverAudit {
    /// Durable `git-bridge-cutover` requirement version, if any.
    pub cutover_requirement: Option<u32>,
    /// Capability rows this build cannot satisfy; cutover fails closed.
    pub unsupported_requirements: Vec<RequiredRepositoryCapability>,
    /// The durable PATH_CLAIMS schema version, if the schema is current.
    pub path_claim_schema_version: Option<u32>,
    /// Colocated Git readiness: physical form, hooks, remotes and HEAD.
    pub colocated_git: ColocatedGitReadiness,
    /// Every reason the current state refuses the cutover (CB-13B R6):
    /// empty readiness means the migration may proceed.
    pub readiness_refusals: Vec<String>,
    /// Per-surface inventory of what the cutover preserves, rebuilds and refuses.
    pub surfaces: Vec<CutoverSchemaAudit>,
    /// The final-schema census (CB-13B R4): the named durable schema
    /// surfaces, each observed live and classified as preserved-immutable,
    /// rebuildable, required or explicitly refused by this migration.
    pub final_schema: Vec<CutoverSchemaAudit>,
}

impl BridgeCutoverAudit {
    /// Stable evidence digest of the pre-fence audit state.
    pub fn evidence_digest(&self) -> Hash {
        let mut material = String::new();
        material.push_str(&format!(
            "cutover-requirement={:?}\n",
            self.cutover_requirement
        ));
        for requirement in &self.unsupported_requirements {
            material.push_str(&format!(
                "unsupported={}:{}\n",
                requirement.id, requirement.minimum_version
            ));
        }
        material.push_str(&format!(
            "path-claim-schema={:?}\ncolocated-git={:?}\n",
            self.path_claim_schema_version, self.colocated_git.form
        ));
        for hook in &self.colocated_git.active_hooks {
            material.push_str(&format!("hook={hook}\n"));
        }
        for remote in &self.colocated_git.remotes {
            material.push_str(&format!("remote={remote}\n"));
        }
        material.push_str(&format!("head={:?}\n", self.colocated_git.head));
        for refusal in &self.readiness_refusals {
            material.push_str(&format!("refusal={refusal}\n"));
        }
        for surface in &self.surfaces {
            material.push_str(&format!(
                "surface={}:{:?}:{}\n",
                surface.item, surface.disposition, surface.detail
            ));
        }
        for surface in &self.final_schema {
            material.push_str(&format!(
                "final-schema={}:{:?}:{}\n",
                surface.item, surface.disposition, surface.detail
            ));
        }
        Hash::of(material.as_bytes())
    }
}

/// A validated, not-yet-applied cutover plan (CB-13B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeCutoverPlan {
    /// The audit snapshot the plan was derived from (rollback evidence).
    pub audit: BridgeCutoverAudit,
    /// The single fenced metadata transition the cutover applies.
    pub transition: MetadataTransition,
    /// The repository already durably requires the cutover capability.
    pub already_fenced: bool,
}

/// Outcome of one cutover execution (CB-13B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeCutoverOutcome {
    /// The journaled `Cutover` operation, when one was prepared, or the
    /// owning operation that was resumed to its Verified receipt.
    pub operation: Option<OperationId>,
    /// The fence was already durable; nothing new was prepared.
    pub already_fenced: bool,
    /// An interrupted fence (landed before the receipt) was resumed in
    /// place under the operation lease (CB-13B R7).
    pub resumed: bool,
}

/// The verified-lifecycle disposition of a durable cutover requirement
/// (CB-13B R7): the row's presence alone is never resume proof.
enum CutoverFenceProof {
    /// No requirement row: nothing was ever fenced.
    Unfenced,
    /// Exactly this build's version, owned by a Verified `Cutover`
    /// operation: the safe idempotent no-op.
    Verified,
    /// The fence landed but the owning operation's receipt did not: the
    /// incomplete head is resumed in place under the held lease.
    Resumable { operation: OperationId },
    /// The durable requirement cannot be proven owned (foreign version,
    /// missing journal entry, or an unresolvable incomplete lifecycle):
    /// refuse instead of a no-op success.
    Refused { reason: String },
}

fn unsupported_requirements(
    requirements: &[RequiredRepositoryCapability],
) -> Vec<RequiredRepositoryCapability> {
    let mut unsupported = requirements
        .iter()
        .filter(|required| {
            !SUPPORTED_REPOSITORY_CAPABILITIES.iter().any(|supported| {
                supported.id() == required.id
                    && supported.minimum_version() >= required.minimum_version
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    unsupported.sort_by(|left, right| left.id.cmp(&right.id));
    unsupported
}

fn surface_inventory(
    cutover_requirement: Option<u32>,
    requirements: &[RequiredRepositoryCapability],
) -> Vec<CutoverSchemaAudit> {
    let capability_detail = match (cutover_requirement, requirements) {
        (Some(version), _) => format!(
            "git-bridge-cutover version {version} is durably required; the colocated \
             bridge owns writes"
        ),
        (None, []) => "no capability requirement is durable yet".to_string(),
        (None, rows) => format!(
            "required rows: {}",
            rows.iter()
                .map(|row| format!("{} v{}", row.id, row.minimum_version))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    vec![
        CutoverSchemaAudit {
            item: "change-objects".to_string(),
            disposition: CutoverAuditDisposition::PreservedImmutable,
            detail: "content-addressed change bytes and hashes are never rewritten; the \
                     cutover is a metadata transition"
                .to_string(),
        },
        CutoverSchemaAudit {
            item: "operation-journal".to_string(),
            disposition: CutoverAuditDisposition::PreservedImmutable,
            detail: "OPERATIONS, OP_HEADS and EFFECT_RECEIPTS are append-only; the cutover \
                     joins them as a verified Cutover operation"
                .to_string(),
        },
        CutoverSchemaAudit {
            item: "working-copy-associations".to_string(),
            disposition: CutoverAuditDisposition::PreservedImmutable,
            detail: "WORKING_COPIES desired/materialized state is untouched by the cutover"
                .to_string(),
        },
        CutoverSchemaAudit {
            item: "capability-requirements".to_string(),
            disposition: CutoverAuditDisposition::Required,
            detail: capability_detail,
        },
        CutoverSchemaAudit {
            item: "derived-indexes".to_string(),
            disposition: CutoverAuditDisposition::RebuildableCache,
            detail: "TREE/REV_TREE, INODE_GRAPH and FILE_INDEX_V2 are rebuildable projections \
                     of the canonical graph; the cutover does not rewrite them"
                .to_string(),
        },
        CutoverSchemaAudit {
            item: "legacy-git-hooks".to_string(),
            disposition: CutoverAuditDisposition::Refused,
            detail: "hook migration is leased separately (CB-13B task 3); the legacy shadow \
                     writer is fenced by the capability, not rewritten here"
                .to_string(),
        },
        CutoverSchemaAudit {
            item: "remote-capability-negotiation".to_string(),
            disposition: CutoverAuditDisposition::Refused,
            detail: "remote/client negotiation is not implemented by this build; the cutover \
                     refuses while any unsupported capability requirement is present"
                .to_string(),
        },
    ]
}

impl Repository {
    /// Audit the repository for the CB-13B shadow → bridge cutover.
    ///
    /// Read-only: inventories the durable capability rows, the PATH_CLAIMS
    /// schema marker, the colocated Git readiness and every readiness
    /// refusal, and classifies every schema surface as
    /// preserved-immutable, rebuildable, required or refused. The audit
    /// never mutates repository state.
    pub fn audit_bridge_cutover(&self) -> Result<BridgeCutoverAudit, RepositoryError> {
        let requirements = self
            .pristine
            .required_repository_capabilities()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let path_claim_schema_version = self
            .pristine
            .path_claim_schema_version()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let cutover_requirement = requirements
            .iter()
            .find(|required| required.id == BRIDGE_CUTOVER_CAPABILITY.id())
            .map(|required| required.minimum_version);
        let unsupported_requirements = unsupported_requirements(&requirements);
        let colocated_git = observe_colocated_git_readiness(&self.root);
        let readiness_refusals = self.cutover_readiness_refusals(&colocated_git)?;
        let surfaces = surface_inventory(cutover_requirement, &requirements);
        let final_schema = self.final_schema_census(&colocated_git)?;
        let final_schema = if final_schema.is_empty() {
            final_schema
        } else {
            final_schema
                .into_iter()
                .chain(self.binding_census(&colocated_git)?)
                .collect()
        };
        Ok(BridgeCutoverAudit {
            cutover_requirement,
            unsupported_requirements,
            path_claim_schema_version,
            colocated_git,
            readiness_refusals,
            surfaces,
            final_schema,
        })
    }

    /// The CB-13B R4 final-schema census: the named durable schema
    /// surfaces, each observed live from the repository state and
    /// classified. Nothing here rewrites legacy data: every surface is
    /// either preserved-immutable (its byte/hash immutability is golden
    /// proven), rebuildable, or an explicit refusal the readiness gates
    /// enforce — never bindings by convention.
    fn final_schema_census(
        &self,
        readiness: &ColocatedGitReadiness,
    ) -> Result<Vec<CutoverSchemaAudit>, RepositoryError> {
        let working_copy = self.require_working_copy_id()?;
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let views = txn.list_views().map_err(pristine_error)?;
        let view_count = views.len();
        let conflicts = txn.snapshot_conflicts().map_err(pristine_error)?.len();
        let git_sha_rows = txn.list_git_shas().map_err(pristine_error)?.len();
        let working_copies = txn
            .list_working_copies()
            .map_err(pristine_error)?
            .len();
        let view_state = txn.get_view(&self.current_view).map_err(pristine_error)?;
        let file_index_rows = match &view_state {
            Some(_) => txn
                .iter_file_index_v2(working_copy)
                .map_err(pristine_error)?
                .len(),
            None => 0,
        };
        let set_id = view_state
            .as_ref()
            .map(|view| super::set_id::view_set_id(&txn, view).map(|set_id| set_id.to_string()))
            .transpose()
            .map_err(pristine_error)?;
        drop(txn);
        // Operation payload versions across both head scopes: the journal
        // is append-only and the cutover never rewrites it.
        let mut operation_versions = std::collections::BTreeSet::new();
        for scope in [OperationScope::Repository, OperationScope::WorkingCopy(working_copy)] {
            let log = self.operation_log(scope, None, false)?;
            for entry in &log.entries {
                operation_versions.insert(entry.operation.encoding_version());
            }
        }
        let versions = operation_versions
            .iter()
            .map(|version| version.to_string())
            .collect::<Vec<_>>()
            .join(",");

        Ok(vec![
            CutoverSchemaAudit {
                item: "extra_known".to_string(),
                disposition: CutoverAuditDisposition::PreservedImmutable,
                detail: "unknown change-header fields ride in content-addressed change \
                         bytes; the migration never rewrites changes, so extra_known \
                         data survives byte-for-byte"
                    .to_string(),
            },
            CutoverSchemaAudit {
                item: "view-state-set-id".to_string(),
                disposition: CutoverAuditDisposition::PreservedImmutable,
                detail: format!(
                    "{view_count} durable view(s); the current view's projection SetId \
                     is {}",
                    set_id.unwrap_or_else(|| "<none>".to_string())
                ),
            },
            CutoverSchemaAudit {
                item: "unrecord-suffixes".to_string(),
                disposition: CutoverAuditDisposition::PreservedImmutable,
                detail: "unrecord removes only the view reference; the content-addressed \
                         change files are retained unsuffixed and never rewritten"
                    .to_string(),
            },
            CutoverSchemaAudit {
                item: "raw-repo-path".to_string(),
                disposition: CutoverAuditDisposition::PreservedImmutable,
                detail: "paths are raw bytes (RepoPath); the migration performs no path \
                         re-encoding and preserves byte-for-byte identity"
                    .to_string(),
            },
            CutoverSchemaAudit {
                item: "conflicts".to_string(),
                disposition: CutoverAuditDisposition::PreservedImmutable,
                detail: format!(
                    "{conflicts} stored conflict row(s); durable ambiguity items are \
                     preserved and re-rendered by the bridge, never resolved silently"
                ),
            },
            CutoverSchemaAudit {
                item: "file-index-v2".to_string(),
                disposition: CutoverAuditDisposition::RebuildableCache,
                detail: format!(
                    "{file_index_rows} V2 file-index row(s) for the current view; \
                     rebuildable from the canonical graph via the backfill path"
                ),
            },
            CutoverSchemaAudit {
                item: "worktree-identity".to_string(),
                disposition: CutoverAuditDisposition::PreservedImmutable,
                detail: format!(
                    "{working_copies} working-copy record(s) keyed by ULID identity; \
                     linked worktrees keep their local pointers and share the fence"
                ),
            },
            CutoverSchemaAudit {
                item: "operation-versions".to_string(),
                disposition: CutoverAuditDisposition::PreservedImmutable,
                detail: format!(
                    "canonical payload versions observed across the heads: {versions} \
                     (supported: 1,2); a future version fails closed at operation load"
                ),
            },
            CutoverSchemaAudit {
                item: "attributes".to_string(),
                disposition: CutoverAuditDisposition::PreservedImmutable,
                detail: "attribute policies (.gitattributes content filters and ignore \
                         mirrors) are consented working-copy state; the cutover rewrites \
                         none of them"
                    .to_string(),
            },
            CutoverSchemaAudit {
                item: "remote-capabilities".to_string(),
                disposition: CutoverAuditDisposition::Refused,
                detail: format!(
                    "remote/client capability negotiation is not implemented by this \
                     build; {} configured remote(s) refuse the migration and the \
                     refusal is enforced by the readiness gates",
                    readiness.remotes.len()
                ),
            },
        ])
    }

    /// The projection-verified binding census (CB-13B R4): GIT_SHA_INDEX
    /// rows are candidate bindings written by the projection under the
    /// shadow lock (verified by construction), aggregate/ReviewGate tags
    /// are Atomic-only and project nothing (explicit refusal), and
    /// same-name ambiguity becomes a durable stored-conflict review item
    /// — never a binding by convention.
    fn binding_census(
        &self,
        readiness: &ColocatedGitReadiness,
    ) -> Result<Vec<CutoverSchemaAudit>, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let git_sha_rows = txn.list_git_shas().map_err(pristine_error)?.len();
        let conflicts = txn.snapshot_conflicts().map_err(pristine_error)?.len();
        drop(txn);
        Ok(vec![
            CutoverSchemaAudit {
                item: "git-sha-index-bindings".to_string(),
                disposition: CutoverAuditDisposition::Required,
                detail: format!(
                    "{git_sha_rows} GIT_SHA_INDEX row(s); every row is written by the \
                     projection publication under the shadow lock — a projection-verified \
                     candidate binding, never a convention"
                ),
            },
            CutoverSchemaAudit {
                item: "trailer-bindings".to_string(),
                disposition: CutoverAuditDisposition::PreservedImmutable,
                detail: "commit trailers ride in content-addressed Git objects; the \
                         migration never rewrites or reinterprets them as Atomic bindings"
                    .to_string(),
            },
            CutoverSchemaAudit {
                item: "aggregate-tag-bindings".to_string(),
                disposition: CutoverAuditDisposition::Refused,
                detail: "ReviewGate/aggregate tags are Atomic-only and project nothing; \
                         they are never bindings"
                    .to_string(),
            },
            CutoverSchemaAudit {
                item: "same-name-ambiguity".to_string(),
                disposition: CutoverAuditDisposition::PreservedImmutable,
                detail: format!(
                    "{conflicts} stored conflict row(s) are the durable ambiguity review \
                     items; a same-name conflict never becomes a binding by convention"
                ),
            },
        ])
    }

    /// The four leased readiness observations (CB-13B R6): Git repository
    /// open, bridge-enabled equivalence (verified checkpoint matching the
    /// live HEAD), candidate-binding proof (the view's stored mapping
    /// resolving in the colocated repository) and hook/remote readiness.
    ///
    /// Every refusal is explicit; an empty list is the only readiness
    /// pass. The `Refused` audit surfaces (legacy-git-hooks,
    /// remote-capability-negotiation) actually refuse here instead of
    /// being unconditional prose.
    fn cutover_readiness_refusals(
        &self,
        readiness: &ColocatedGitReadiness,
    ) -> Result<Vec<String>, RepositoryError> {
        let mut refusals = Vec::new();
        // 1. Git repository open: the physical `.git` form is the proof.
        match readiness.form {
            ColocatedGitForm::Repository => {}
            ColocatedGitForm::Absent => refusals.push(format!(
                "no colocated Git repository at '{}'; run `atomic git bridge enable` \
                 before cutting over",
                self.root.join(".git").display()
            )),
            ColocatedGitForm::InvalidDirectory => refusals.push(
                "the colocated .git path is a directory but not a valid Git repository \
                 (empty directory?); unsupported migration"
                    .to_string(),
            ),
            ColocatedGitForm::InvalidFile => refusals.push(
                "the colocated .git path is a regular file that is not a valid Git \
                 gitdir pointer; unsupported migration"
                    .to_string(),
            ),
            ColocatedGitForm::InvalidSymlink => refusals.push(
                "the colocated .git symlink does not resolve to a valid Git repository; \
                 unsupported migration"
                    .to_string(),
            ),
        }
        // 2. Bridge-enabled equivalence: a verified checkpoint for the
        // current view whose git_head equals the live colocated HEAD.
        match read_workspace_checkpoint(&self.root)? {
            Some(checkpoint) if checkpoint.view == self.current_view => {
                if readiness.head.as_deref() != Some(checkpoint.git_head.as_str()) {
                    refusals.push(format!(
                        "the bridge checkpoint does not verify against the colocated HEAD \
                         (checkpoint head '{}'; live head {:?}); re-project the view \
                         before cutting over",
                        checkpoint.git_head, readiness.head
                    ));
                }
            }
            Some(checkpoint) => refusals.push(format!(
                "the verified bridge checkpoint is for view '{}', not the current view \
                 '{}'; switch to the checkpointed view or re-project before cutting over",
                checkpoint.view, self.current_view
            )),
            None => refusals.push(
                "no verified bridge checkpoint; the shadow pipeline has not published a \
                 verified projection here — re-project before cutting over"
                    .to_string(),
            ),
        }
        // 3. Candidate-binding proof: the stored mapping resolves.
        match self.get_ref_mapping(&self.current_view)? {
            Some(mapping) => {
                if let Some(local_ref) = &mapping.local_ref {
                    let resolves = git2::Repository::open(&self.root)
                        .map(|git| git.find_reference(local_ref).map(|_| ()).ok())
                        .unwrap_or(None)
                        .is_some();
                    if !resolves {
                        refusals.push(format!(
                            "the candidate binding for view '{}' maps to Git ref \
                             '{local_ref}' which does not exist in the colocated \
                             repository; unsupported migration",
                            self.current_view
                        ));
                    }
                }
            }
            None => refusals.push(format!(
                "no candidate binding is recorded for view '{}'; the view has no Git \
                 mapping to cut over; unsupported migration",
                self.current_view
            )),
        }
        // 4. Hook/remote readiness: the Refused surfaces refuse. Foreign
        // hooks (hook managers, custom scripts, core.hooksPath) refuse;
        // Atomic-owned advisory dispatchers do NOT — they route through
        // the journaled cutover as migration-effect leases (CB-13B R2).
        for hook in &readiness.active_hooks {
            refusals.push(format!(
                "hook migration is not implemented for foreign surfaces by this build; \
                 the active hook surface '{hook}' must be migrated or removed before the \
                 cutover (the audit surface 'legacy-git-hooks' refuses)"
            ));
        }
        for hook in &readiness.owned_dispatchers {
            if hook.path.is_none() {
                refusals.push(format!(
                    "the Atomic-owned dispatcher '{}' lives outside the worktree root \
                     (a linked worktree's common gitdir); decommission it manually before \
                     the cutover (unsupported migration)",
                    hook.name
                ));
            }
        }
        for remote in &readiness.remotes {
            refusals.push(format!(
                "remote capability negotiation is not implemented by this build; the \
                 configured remote '{remote}' refuses the cutover (the audit surface \
                 'remote-capability-negotiation' refuses)"
            ));
        }
        Ok(refusals)
    }

    /// The hook decommission migration effects (CB-13B R2): every
    /// Atomic-owned advisory dispatcher is removed through a journaled
    /// filesystem effect whose expected-old lease is the dispatcher's
    /// exact bytes and mode, so a rollback (or an interruption recovery)
    /// restores it byte-for-byte. Foreign hooks never appear here — they
    /// refuse instead.
    fn hook_decommission_effects(
        &self,
        readiness: &ColocatedGitReadiness,
    ) -> Result<Vec<EffectPlan>, RepositoryError> {
        readiness
            .owned_dispatchers
            .iter()
            .enumerate()
            .map(|(ordinal, hook)| {
                let path = hook.path.clone().ok_or_else(|| {
                    RepositoryError::InvalidOperation {
                        message: format!(
                            "the Atomic-owned dispatcher '{}' lives outside the worktree \
                             root; it cannot carry a journaled migration effect",
                            hook.name
                        ),
                    }
                })?;
                Ok(EffectPlan {
                    ordinal: u32::try_from(ordinal).map_err(|_| RepositoryError::InvalidOperation {
                        message: "too many owned dispatchers for one cutover".to_string(),
                    })?,
                    target: EffectTarget::FilesystemPath { path },
                    expected_old: EffectValue::File(FileState {
                        kind: FileKind::Regular,
                        mode: hook.mode,
                        content: hook.content,
                    }),
                    expected_new: EffectValue::Absent,
                })
            })
            .collect()
    }

    /// Validate the repository for cutover without mutating anything.
    ///
    /// Refuses (before any mutation) with remediation when the repository
    /// carries capability requirements this build cannot satisfy, when the
    /// PATH_CLAIMS schema is missing, when there is no colocated Git
    /// repository to cut over to, or when any readiness observation
    /// refuses (CB-13B R6). `already_fenced` is informational row
    /// presence: execution decides through the verified-lifecycle proof
    /// under the operation locks (CB-13B R7), never through this flag.
    pub fn plan_bridge_cutover(&self) -> Result<BridgeCutoverPlan, RepositoryError> {
        let audit = self.audit_bridge_cutover()?;
        if let Some(requirement) = audit.unsupported_requirements.first() {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "bridge cutover refused: repository requires unsupported capability \
                     '{}' version {} (this build supports through {:?}); preserve the \
                     repository and upgrade Atomic before migrating",
                    requirement.id,
                    requirement.minimum_version,
                    SUPPORTED_REPOSITORY_CAPABILITIES
                        .iter()
                        .find(|supported| supported.id() == requirement.id)
                        .map(|supported| supported.minimum_version()),
                ),
            });
        }
        if audit.path_claim_schema_version.is_none() {
            return Err(RepositoryError::InvalidOperation {
                message: "bridge cutover refused: the repository PATH_CLAIMS schema is not \
                          current; run the pending repository migration first"
                    .to_string(),
            });
        }
        // CB-13B R6: readiness is projection proof, not `.git` existence.
        // An already-fenced repository plans as an idempotent no-op without
        // readiness requirements; an unfenced one must pass every gate.
        if audit.cutover_requirement.is_none() {
            if let Some(refusal) = audit.readiness_refusals.first() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "bridge cutover refused: {refusal} Preserve the repository and \
                         resolve every readiness refusal before migrating."
                    ),
                });
            }
        }
        let already_fenced = audit.cutover_requirement.is_some();
        let transition = MetadataTransition {
            target: MetadataTarget::Capability {
                id: BRIDGE_CUTOVER_CAPABILITY.id().to_string(),
            },
            expected_old: audit
                .cutover_requirement
                .map(|version| MetadataValue::Sequence(u64::from(version)))
                .unwrap_or(MetadataValue::Absent),
            expected_new: MetadataValue::Sequence(u64::from(BRIDGE_CUTOVER_VERSION)),
        };
        Ok(BridgeCutoverPlan {
            audit,
            transition,
            already_fenced,
        })
    }

    /// Execute the journaled shadow → bridge cutover (CB-13B).
    ///
    /// Holds the canonical operation locks, re-observes the capability
    /// lease under them with its full verified-lifecycle proof (CB-13B R7:
    /// exact version, owning `Cutover` operation and Verified receipt),
    /// journals the `Cutover` operation as a durable rollback point BEFORE
    /// writing the fence, applies the exact leased version, and completes
    /// only after the verified receipt. An already-fenced repository is a
    /// no-op only when that proof is complete; an interrupted fence whose
    /// receipt never landed is resumed in place; anything else refuses.
    pub fn execute_bridge_cutover(&self) -> Result<BridgeCutoverOutcome, RepositoryError> {
        let plan = self.plan_bridge_cutover()?;
        let working_copy = self.require_working_copy_id()?;
        // Canonical order: the common bridge lock is acquired first and the
        // working-copy operation lock nests inside it; every pristine write
        // below happens through the operation lock.
        let operation_lock = self.try_lock_operation(working_copy)?;
        // Re-observe under the locks with the full lifecycle proof: a
        // durable row alone is never resume proof.
        match self.already_fenced_proof(working_copy)? {
            CutoverFenceProof::Verified => {
                return Ok(BridgeCutoverOutcome {
                    operation: None,
                    already_fenced: true,
                    resumed: false,
                });
            }
            CutoverFenceProof::Resumable { operation } => {
                // The fence landed before the receipt (interruption after
                // the metadata apply, before finalize): finalize re-observes
                // the lease against expected-new and appends the Verified
                // receipt durably. Never a no-op success.
                self.finalize_operation_verified(&operation_lock, operation)?;
                return Ok(BridgeCutoverOutcome {
                    operation: Some(operation),
                    already_fenced: true,
                    resumed: true,
                });
            }
            CutoverFenceProof::Refused { reason } => {
                return Err(RepositoryError::InvalidOperation { message: reason });
            }
            CutoverFenceProof::Unfenced => {}
        }
        // CB-13B R6: the readiness observations re-run under the leases
        // that will hold the fence — the pre-lock audit is informational,
        // this gate is the one that precedes the fence.
        let readiness = observe_colocated_git_readiness(&self.root);
        let refusals = self.cutover_readiness_refusals(&readiness)?;
        if !refusals.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "bridge cutover refused under the operation lease: {}; the fence \
                     was not written",
                    refusals.join("; ")
                ),
            });
        }
        let before = self.cutover_state_ref(working_copy)?;
        let after = before.clone();
        // CB-13B R2: the cutover owns the hook/config/remote migration
        // writers through the journal. The Atomic-owned advisory
        // dispatchers decommission through journaled filesystem effects
        // with exact-bytes leases (restored byte-for-byte by the typed
        // undo); the observed config consent's digest travels in the
        // evidence; remote negotiation refuses (explicit, no partial
        // enablement).
        let effects = self.hook_decommission_effects(&readiness)?;
        let mut evidence = vec![plan.audit.evidence_digest()];
        if let Ok(consent) = std::fs::read(self.dot_dir.join("config.toml")) {
            evidence.push(Hash::of(&consent));
        }
        // The journal entry is the durable rollback point: it carries the
        // before-state, the inverse-derivable transition, the migration
        // effects and the retained old-value backups, and it commits
        // before any fence or decommission moves.
        let operation = self
            .prepare_working_copy_transition_with_metadata(
                &operation_lock,
                OperationKind::Cutover,
                None,
                before,
                after,
                effects,
                vec![plan.transition],
                evidence,
                ActorRef::System {
                    name: CUTOVER_ACTOR.to_string(),
                },
                current_operation_timestamp_ms(),
            )?
            .operation;
        // Decommission the owned dispatchers through their journaled
        // leases (removal: no prepared content bytes). Every effect gets
        // its receipt before finalize will verify.
        for effect in &operation.payload().delta.effects {
            self.execute_and_record_effect(&operation_lock, operation.id(), effect.ordinal, None)?;
        }
        self.apply_operation_metadata_locked(&operation_lock, operation.id())?;
        #[cfg(test)]
        cutover_interrupt_before_finalize()?;
        // finalize re-observes the lease against expected-new before it
        // appends the operation-level Verified receipt durably.
        self.finalize_operation_verified(&operation_lock, operation.id())?;
        Ok(BridgeCutoverOutcome {
            operation: Some(operation.id()),
            already_fenced: false,
            resumed: false,
        })
    }

    /// The CB-13B R7 verified-lifecycle proof for a durable cutover
    /// requirement, observed under the operation locks: the row must be
    /// exactly this build's version, owned by a `Cutover` operation in the
    /// shared-repository or initiating working-copy journal, whose receipt
    /// state resolves the lifecycle (Verified, or an incomplete head that
    /// the resume path may finish in place).
    fn already_fenced_proof(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<CutoverFenceProof, RepositoryError> {
        let observed = {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            txn.required_capability_version(BRIDGE_CUTOVER_CAPABILITY.id())
                .map_err(pristine_error)?
        };
        let Some(version) = observed else {
            return Ok(CutoverFenceProof::Unfenced);
        };
        // Exact version only: version 0 is valid in the exact-lease API
        // yet is a foreign fence for this build, and a late higher version
        // was written by a newer build. Neither is a resume proof.
        if version != BRIDGE_CUTOVER_VERSION {
            return Ok(CutoverFenceProof::Refused {
                reason: format!(
                    "bridge cutover refused: the durable requirement version is {version}, \
                     not exactly this build's {BRIDGE_CUTOVER_VERSION}; a foreign fence \
                     version cannot be resumed or undone through this journal"
                ),
            });
        }
        // Owning operation: a `Cutover`-kind journal entry carrying the
        // capability transition, in the shared repository scope or the
        // initiating working-copy scope.
        let mut owning = None;
        for scope in [
            OperationScope::Repository,
            OperationScope::WorkingCopy(working_copy),
        ] {
            let log = self.operation_log(scope, None, false)?;
            for entry in &log.entries {
                let payload = entry.operation.payload();
                if payload.kind != OperationKind::Cutover {
                    continue;
                }
                let carries = payload.delta.metadata.iter().any(|transition| {
                    matches!(
                        &transition.target,
                        MetadataTarget::Capability { id } if id == BRIDGE_CUTOVER_CAPABILITY.id()
                    )
                });
                if carries {
                    owning = Some(entry.operation.id());
                    break;
                }
            }
            if owning.is_some() {
                break;
            }
        }
        let Some(operation) = owning else {
            return Ok(CutoverFenceProof::Refused {
                reason: format!(
                    "bridge cutover refused: the requirement row is durable but no owning \
                     Cutover operation exists in the journal; a fence without its owning \
                     operation cannot be resumed through this journal"
                ),
            });
        };
        let details = self.operation_details(operation)?;
        match details.verification {
            OperationVerificationState::Verified => Ok(CutoverFenceProof::Verified),
            // The fence landed before the receipt and the owning operation
            // is still an incomplete head: the resume path finishes it in
            // place under the held lease.
            _ if !details.head_of.is_empty() => Ok(CutoverFenceProof::Resumable { operation }),
            _ => Ok(CutoverFenceProof::Refused {
                reason: format!(
                    "bridge cutover refused: the owning Cutover operation {operation} is \
                     not verified and is no longer an operation head; its lifecycle \
                     cannot be resolved under this lock"
                ),
            }),
        }
    }

    /// Roll the verified cutover back through the journal's typed inverse.
    ///
    /// The gate is the SHARED repository head, not the initiating
    /// working-copy head (CB-13B R2): another worktree's later bridge
    /// activity, or a generic op undo/restore, advances the shared head
    /// past the cutover and rollback must refuse instead of undoing out of
    /// order. The inverse deletes only the cutover requirement row — every
    /// other required capability (e.g. a newer format fence) is kept — and
    /// refuses unless the current verified shared head IS the cutover
    /// operation itself (kind `Cutover` carrying the
    /// `git-bridge-cutover` capability transition), so a later operation —
    /// including the rollback's own undo child — can never be mistaken for
    /// the cutover.
    pub fn rollback_bridge_cutover(&mut self) -> Result<OperationId, RepositoryError> {
        let working_copy = self.require_working_copy_id()?;
        // CB-13B R2: the shared repository head is the rollback authority.
        let shared_head = self.sole_operation_head(OperationScope::Repository)?;
        let details = self.operation_details(shared_head)?;
        if details.verification != OperationVerificationState::Verified {
            return Err(RepositoryError::OperationNotVerified {
                operation: shared_head.to_string(),
            });
        }
        let is_cutover = details.operation.payload().kind == OperationKind::Cutover
            && details
                .operation
                .payload()
                .delta
                .metadata
                .iter()
                .any(|transition| {
                    matches!(
                        &transition.target,
                        MetadataTarget::Capability { id } if id == BRIDGE_CUTOVER_CAPABILITY.id()
                    )
                });
        if !is_cutover {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "bridge cutover rollback refused: the shared repository head {} is not \
                     the cutover",
                    shared_head
                ),
            });
        }
        self.undo_operation(working_copy, Some(shared_head))
    }

    /// Refuse while the repository durably requires the `git-bridge-cutover`
    /// capability (CB-13B): the colocated bridge owns writes after cutover.
    ///
    /// The legacy shadow commit lock consults this before taking its lock, so
    /// every legacy writer refuses before mutating any state.
    pub fn require_legacy_shadow_write_allowed(&self) -> Result<(), RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let required = txn
            .required_capability_version(BRIDGE_CUTOVER_CAPABILITY.id())
            .map_err(pristine_error)?;
        drop(txn);
        match required {
            Some(version) => Err(RepositoryError::LegacyShadowWriterFenced {
                capability: format!(
                    "{} (required version {version})",
                    BRIDGE_CUTOVER_CAPABILITY.id()
                ),
            }),
            None => Ok(()),
        }
    }

    /// Re-observe the legacy-writer fence while the common operation lock is
    /// held (CB-13B R5): a cutover fences only under the same common lock,
    /// so an unfenced observation made with the guard in hand cannot be
    /// invalidated before the writer path releases it. The guard parameter
    /// makes calling this without the lock a type error.
    pub fn require_legacy_shadow_write_allowed_under_lock(
        &self,
        _common: &RepositoryCommonLockGuard,
    ) -> Result<(), RepositoryError> {
        self.require_legacy_shadow_write_allowed()
    }

    fn cutover_state_ref(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<RepoStateRef, RepositoryError> {
        let record = self.working_copy_record(working_copy)?;
        Ok(RepoStateRef {
            view: None,
            working_copy: Some(working_copy_state_ref(record)),
            git: None,
        })
    }
}

/// CB-13B R2 test seam: fail the cutover between the fence and the
/// Verified receipt so the interruption recovery (restoring the
/// decommissioned dispatchers through the inverse effects) is exercisable
/// end to end. The seam does not exist in shipping builds.
#[cfg(test)]
fn cutover_interrupt_before_finalize() -> Result<(), RepositoryError> {
    // One-shot: the flag is consumed when it fires, so a later cutover on
    // the same thread proceeds normally.
    if CUTOVER_INTERRUPT.with(|slot| slot.replace(false)) {
        return Err(RepositoryError::Io(std::io::Error::other(
            "debug failpoint: cutover interrupted before finalize",
        )));
    }
    Ok(())
}

/// Install the CB-13B R2 interrupt seam for the calling thread. Test-only.
#[cfg(test)]
pub(crate) fn install_cutover_interrupt_before_finalize() {
    CUTOVER_INTERRUPT.with(|slot| slot.set(true));
}

/// Per-thread flag for [`cutover_interrupt_before_finalize`]. Test-only.
#[cfg(test)]
thread_local! {
    static CUTOVER_INTERRUPT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
