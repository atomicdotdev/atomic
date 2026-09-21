use super::*;
use std::collections::HashMap;

use atomic_core::operation::{
    ActorRef, EffectPlan, EffectReceiptKind, EffectTarget, EffectValue, FileKind, FileState,
    Operation, OperationKind, OperationRelation, OperationScope, RepoStateRef, ViewStateRef,
};
use atomic_core::pristine::OperationTxnT;

/// Return the workspace directory path for a given view.
///
/// The path is `.atomic/workspaces/<view_name>/`.  View names may
/// contain `/` (e.g. `agent/ses_abc123`), which becomes a nested
/// directory structure.
pub(super) fn workspace_path(dot_dir: &Path, view_name: &str) -> PathBuf {
    dot_dir.join(WORKSPACES_DIR).join(view_name)
}

/// Ensure the workspace directory for a view exists.
///
/// Creates `.atomic/workspaces/<view_name>/` and any intermediate
/// directories.  This is called from `init`, `create_view`, and
/// `create_view_from`.
pub(super) fn ensure_workspace_dir(dot_dir: &Path, view_name: &str) -> Result<(), RepositoryError> {
    let ws = workspace_path(dot_dir, view_name);
    std::fs::create_dir_all(&ws)?;
    Ok(())
}

pub(super) fn working_copy_workspace_path(
    dot_dir: &Path,
    working_copy: WorkingCopyId,
    view_name: &str,
) -> PathBuf {
    dot_dir
        .join("working-copies")
        .join(working_copy.to_string())
        .join(WORKSPACES_DIR)
        .join(view_name)
}

fn ensure_working_copy_workspace_dir(
    dot_dir: &Path,
    working_copy: WorkingCopyId,
    view_name: &str,
) -> Result<(), RepositoryError> {
    std::fs::create_dir_all(working_copy_workspace_path(
        dot_dir,
        working_copy,
        view_name,
    ))?;
    Ok(())
}

/// Remove empty ancestor directories after file removal.
///
/// Given an iterator of relative paths that were just deleted, this
/// collects every parent directory, sorts them deepest-first, and
/// attempts `std::fs::remove_dir` on each.  Because `remove_dir` only
/// succeeds on *empty* directories, this is always safe — a directory
/// that still contains files (tracked, untracked, or otherwise) will
/// simply fail silently.
///
/// Extracting this into a standalone helper keeps `switch_view` at the
/// orchestration level and makes the cleanup logic reusable for other
/// operations (e.g. `atomic clean`).
fn rename_and_sync(source: &Path, destination: &Path) -> Result<(), RepositoryError> {
    let source_parent = source
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!("rename source has no parent: {}", source.display()),
        })?;
    let destination_parent =
        destination
            .parent()
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!(
                    "rename destination has no parent: {}",
                    destination.display()
                ),
            })?;
    std::fs::rename(source, destination)?;
    #[cfg(unix)]
    {
        std::fs::File::open(source_parent)?.sync_all()?;
        if source_parent != destination_parent {
            std::fs::File::open(destination_parent)?.sync_all()?;
        }
    }
    Ok(())
}

fn operation_timestamp_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(unix)]
fn default_directory_mode() -> u32 {
    0o755
}

#[cfg(not(unix))]
fn default_directory_mode() -> u32 {
    0o666
}

pub(super) struct SwitchMaterializationExecutor<'a> {
    repository: &'a Repository,
    operation_lock: &'a super::locks::WorkingCopyOperationLockGuard,
    operation: Operation,
}

impl<'a> SwitchMaterializationExecutor<'a> {
    pub(super) fn new(
        repository: &'a Repository,
        operation_lock: &'a super::locks::WorkingCopyOperationLockGuard,
        operation: Operation,
    ) -> Self {
        Self {
            repository,
            operation_lock,
            operation,
        }
    }

    fn effect_for_expected_new(
        &self,
        path: &str,
        predicate: impl Fn(&EffectValue) -> bool,
    ) -> Option<&EffectPlan> {
        self.operation
            .payload()
            .delta
            .effects
            .iter()
            .rev()
            .find(|effect| {
                matches!(
                    &effect.target,
                    EffectTarget::FilesystemPath { path: target } if target == path
                ) && predicate(&effect.expected_new)
            })
    }

    fn execute_effect(
        &self,
        effect: &EffectPlan,
        content: Option<&[u8]>,
    ) -> Result<super::operation::PendingFilesystemEffect, RepositoryError> {
        self.repository.execute_filesystem_effect(
            self.operation_lock,
            self.operation.id(),
            effect.ordinal,
            content,
        )
    }

    fn execute_absence(
        &self,
        path: &str,
    ) -> Result<super::operation::PendingFilesystemEffect, RepositoryError> {
        let effect = self
            .effect_for_expected_new(path, |value| *value == EffectValue::Absent)
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("switch operation omitted removal lease for '{path}'"),
            })?;
        self.execute_effect(effect, None)
    }

    fn execute_directory(
        &self,
        path: &str,
    ) -> Result<super::operation::PendingFilesystemEffect, RepositoryError> {
        let effect = self
            .effect_for_expected_new(path, |value| {
                matches!(value, EffectValue::File(state) if state.kind == FileKind::Directory)
            })
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("switch operation omitted directory lease for '{path}'"),
            })?;
        self.execute_effect(effect, None)
    }

    fn execute_file(
        &self,
        path: &str,
        content: &[u8],
    ) -> Result<super::operation::PendingFilesystemEffect, RepositoryError> {
        let content_hash = Hash::of(content);
        let effect = self
            .effect_for_expected_new(path, |value| {
                matches!(
                    value,
                    EffectValue::File(state)
                        if state.kind != FileKind::Directory && state.content == content_hash
                )
            })
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("switch operation omitted file lease for '{path}'"),
            })?;
        self.execute_effect(effect, Some(content))
    }

    fn record(
        &self,
        pending: super::operation::PendingFilesystemEffect,
    ) -> Result<bool, RepositoryError> {
        self.repository.record_pending_filesystem_effect(
            self.operation_lock,
            self.operation.id(),
            pending,
        )
    }

    pub(super) fn complete_preparatory(
        &mut self,
        target_view: &str,
    ) -> Result<(), RepositoryError> {
        self.complete_effects(target_view, true)
    }

    pub(super) fn complete_remaining(&mut self, target_view: &str) -> Result<(), RepositoryError> {
        self.complete_effects(target_view, false)
    }

    fn complete_effects(
        &mut self,
        target_view: &str,
        preparatory_only: bool,
    ) -> Result<(), RepositoryError> {
        let completed: HashSet<u32> = {
            let txn = self
                .repository
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            txn.get_effect_receipts(self.operation.id())
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .into_iter()
                .filter_map(|receipt| match receipt.payload().kind {
                    EffectReceiptKind::Applied
                    | EffectReceiptKind::Recovered
                    | EffectReceiptKind::RolledBack => receipt.payload().effect_ordinal,
                    EffectReceiptKind::Verified | EffectReceiptKind::LeaseRejected => None,
                })
                .collect()
        };
        let effects: Vec<EffectPlan> = self
            .operation
            .payload()
            .delta
            .effects
            .iter()
            .filter(|effect| matches!(effect.target, EffectTarget::FilesystemPath { .. }))
            .cloned()
            .collect();
        for effect in &effects {
            if completed.contains(&effect.ordinal) {
                continue;
            }
            if preparatory_only
                && !effects
                    .iter()
                    .any(|later| later.ordinal > effect.ordinal && later.target == effect.target)
            {
                continue;
            }
            let EffectTarget::FilesystemPath { path } = &effect.target else {
                continue;
            };
            let observed = self
                .repository
                .observe_filesystem_effect(self.operation_lock.working_copy(), &effect.target)?;
            let pending = if observed == effect.expected_old {
                let content = match &effect.expected_new {
                    EffectValue::File(state) if state.kind != FileKind::Directory => Some(
                        self.repository
                            .switch_target_file_bytes(path, target_view)?
                            .ok_or_else(|| RepositoryError::InvalidOperation {
                                message: format!(
                                    "target view '{target_view}' has no bytes for leased path '{path}'"
                                ),
                            })?,
                    ),
                    _ => None,
                };
                self.repository.execute_filesystem_effect(
                    self.operation_lock,
                    self.operation.id(),
                    effect.ordinal,
                    content.as_deref(),
                )?
            } else if observed == effect.expected_new {
                self.repository.execute_filesystem_effect(
                    self.operation_lock,
                    self.operation.id(),
                    effect.ordinal,
                    None,
                )?
            } else {
                self.repository.record_effect_outcome(
                    self.operation_lock,
                    self.operation.id(),
                    effect.ordinal,
                    observed.clone(),
                    observed,
                )?;
                unreachable!("divergent receipt recording always returns an error")
            };
            self.record(pending)?;
        }
        Ok(())
    }
}

impl super::materialize::MaterializationEffectExecutor for SwitchMaterializationExecutor<'_> {
    fn create_directory(&mut self, path: &str) -> Result<bool, RepositoryError> {
        if self
            .effect_for_expected_new(path, |value| {
                matches!(value, EffectValue::File(state) if state.kind == FileKind::Directory)
            })
            .is_none()
        {
            return match std::fs::symlink_metadata(self.repository.root().join(path)) {
                Ok(metadata) if metadata.is_dir() => Ok(false),
                Ok(_) => Err(RepositoryError::InvalidOperation {
                    message: format!("unleased non-directory entry blocks '{path}'"),
                }),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Err(RepositoryError::InvalidOperation {
                        message: format!("switch operation omitted directory lease for '{path}'"),
                    })
                }
                Err(error) => Err(RepositoryError::Io(error)),
            };
        }
        let pending = self.execute_directory(path)?;
        self.record(pending)
    }

    fn write_file(&mut self, path: &str, content: &[u8]) -> Result<bool, RepositoryError> {
        let content_hash = Hash::of(content);
        if self
            .effect_for_expected_new(path, |value| {
                matches!(
                    value,
                    EffectValue::File(state)
                        if state.kind != FileKind::Directory && state.content == content_hash
                )
            })
            .is_none()
        {
            return match std::fs::read(self.repository.root().join(path)) {
                Ok(current) if current == content => Ok(false),
                Ok(_) => Err(RepositoryError::InvalidOperation {
                    message: format!("unleased filesystem content changed at '{path}'"),
                }),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Err(RepositoryError::InvalidOperation {
                        message: format!("switch operation omitted file lease for '{path}'"),
                    })
                }
                Err(error) => Err(RepositoryError::Io(error)),
            };
        }
        let pending = self.execute_file(path, content)?;
        self.record(pending)
    }

    fn remove_path(&mut self, path: &str) -> Result<bool, RepositoryError> {
        if self
            .effect_for_expected_new(path, |value| *value == EffectValue::Absent)
            .is_none()
        {
            return match std::fs::symlink_metadata(self.repository.root().join(path)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Ok(_) => Err(RepositoryError::InvalidOperation {
                    message: format!("switch operation omitted removal lease for '{path}'"),
                }),
                Err(error) => Err(RepositoryError::Io(error)),
            };
        }
        let pending = self.execute_absence(path)?;
        self.record(pending)
    }
}

impl Repository {
    /// Switch to a different view and update the working copy.
    ///
    /// This is the primary method for switching views. It:
    /// 1. Validates the view exists
    /// 2. Updates the current view pointer
    /// 3. Materializes the working copy to match the new view's state
    ///
    /// # Arguments
    ///
    /// * `working_copy` - The validated physical working-copy identity
    /// * `view` - The name of the view to switch to
    ///
    /// # Returns
    ///
    /// Statistics about the materialize operation (files written, etc.)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The view does not exist
    /// - The working copy cannot be updated
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut repo = Repository::open(".")?;
    ///
    /// // Switch to feature view and update working copy
    /// let result = repo.switch_view(working_copy, "feature")?;
    /// println!("Updated {} files", result.files_written);
    /// ```
    pub fn switch_view(
        &mut self,
        working_copy: WorkingCopyId,
        view: &str,
    ) -> Result<MaterializeResult, RepositoryError> {
        self.switch_view_with_operation(
            working_copy,
            view,
            OperationKind::SwitchView,
            None,
            ActorRef::System {
                name: "repository-switch".to_string(),
            },
            None,
        )
    }

    pub(super) fn switch_view_with_operation(
        &mut self,
        working_copy: WorkingCopyId,
        view: &str,
        kind: OperationKind,
        relation: Option<OperationRelation>,
        actor: ActorRef,
        expected_head: Option<atomic_core::OperationId>,
    ) -> Result<MaterializeResult, RepositoryError> {
        let operation_lock = self.try_lock_operation(working_copy)?;
        self.recover_pending_deferred_tree_alignment_locked(&operation_lock)?;
        if let super::operation::OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: OperationScope::WorkingCopy(working_copy).to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        self.recover_incomplete_operation(&operation_lock)?;
        if let Some(expected_head) = expected_head {
            let actual_head =
                self.sole_operation_head(OperationScope::WorkingCopy(working_copy))?;
            if actual_head != expected_head {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "operation head changed before inverse switch: expected {expected_head}, found {actual_head}"
                    ),
                });
            }
        }

        match self.switch_view_journaled(
            &operation_lock,
            working_copy,
            view,
            kind,
            relation,
            actor,
        ) {
            Ok(result) => Ok(result),
            Err(operation_error) => match self.recover_incomplete_operation(&operation_lock) {
                Ok(_) => Err(operation_error),
                Err(recovery_error) => Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "switch failed ({operation_error}); lease-safe recovery also failed ({recovery_error})"
                    ),
                }),
            },
        }
    }

    fn switch_view_journaled(
        &mut self,
        operation_lock: &super::locks::WorkingCopyOperationLockGuard,
        working_copy: WorkingCopyId,
        view: &str,
        kind: OperationKind,
        relation: Option<OperationRelation>,
        actor: ActorRef,
    ) -> Result<MaterializeResult, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let old_view_name = self.desired_view_name(working_copy)?;
        let before_record = self.working_copy_record(working_copy)?;

        // Resolve both views and validate both dependency closures before the
        // switch publishes a pointer or mutates TREE-derived state.
        let (
            old_files,
            new_files,
            new_file_materialization,
            old_directories,
            new_directories,
            new_absent_files,
            new_absent_directories,
            new_visibility,
            new_view_id,
            old_state,
            new_state,
        ) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let old_view = txn
                .get_view(&old_view_name)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: old_view_name.clone(),
                })?;
            let new_view = txn
                .get_view(view)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view.to_string(),
                })?;

            let old_membership = view_membership(&txn, &old_view)?;
            let new_membership = view_membership(&txn, &new_view)?;
            let old_visibility = graph_visibility_from_membership(&txn, &old_membership)?;
            let new_visibility = graph_visibility_from_membership(&txn, &new_membership)?;
            let old_projection = self.project_tree_for_visibility(&txn, &old_visibility)?;
            let new_projection = self.project_tree_for_visibility(&txn, &new_visibility)?;
            let mut old_files = HashSet::new();
            let mut old_directories = HashSet::new();
            for (path, item) in old_projection.present {
                if item.is_directory {
                    old_directories.insert(path);
                } else {
                    old_files.insert(path);
                }
            }
            for (path, conflict) in old_projection.name_conflicts {
                if conflict.sides.iter().all(|side| side.is_directory()) {
                    old_directories.insert(path);
                } else {
                    old_files.insert(path);
                }
            }
            let new_absent_files: HashSet<String> = new_projection
                .absent_metadata
                .values()
                .filter(|entry| !entry.directory)
                .map(|entry| entry.path.clone())
                .collect();
            let new_absent_directories: HashSet<String> = new_projection
                .absent_metadata
                .values()
                .filter(|entry| entry.directory)
                .map(|entry| entry.path.clone())
                .collect();
            let mut new_files = HashSet::new();
            let mut new_file_materialization = HashMap::new();
            let mut new_directories = HashSet::new();
            for (path, item) in new_projection.present {
                if item.is_directory {
                    new_directories.insert(path);
                } else {
                    let projected = atomic_core::output::project_inode_attributes(
                        &txn,
                        item.position,
                        new_visibility.attribute_visibility(),
                    )
                    .map_err(|error| RepositoryError::Database(error.to_string()))?;
                    if projected.is_conflicted() {
                        return Err(RepositoryError::Output(format!(
                            "cannot switch '{}' with conflicting inode attributes: {:?}",
                            path, projected.conflicts
                        )));
                    }
                    new_file_materialization.insert(path.clone(), projected.materialization);
                    new_files.insert(path);
                }
            }
            for (path, conflict) in new_projection.name_conflicts {
                if conflict.sides.iter().all(|side| side.is_directory()) {
                    new_directories.insert(path);
                } else {
                    new_file_materialization.insert(path.clone(), Default::default());
                    new_files.insert(path);
                }
            }

            (
                old_files,
                new_files,
                new_file_materialization,
                old_directories,
                new_directories,
                new_absent_files,
                new_absent_directories,
                new_visibility,
                new_view.id,
                old_view.state,
                new_view.state,
            )
        };
        if old_view_name == view
            && before_record.desired_state == new_state
            && before_record.materialized_state == Some(new_state)
        {
            return Ok(MaterializeResult::default());
        }

        // Render the complete target before publishing the target pointer,
        // shelving ignored files, or removing tracked paths. This does not make
        // filesystem execution crash-safe, but it guarantees graph/preload/
        // content errors refuse the switch before its first external effect.
        self.validate_materialization_with_visibility(
            working_copy,
            None,
            new_visibility.clone(),
            new_view_id,
        )?;

        let mut tracked_paths: HashSet<String> = old_files.union(&new_files).cloned().collect();
        tracked_paths.extend(old_directories.iter().cloned());
        tracked_paths.extend(new_directories.iter().cloned());
        let ignored_paths = self.collect_switch_ignored_paths(&tracked_paths);
        let new_ws = working_copy_workspace_path(&self.dot_dir, working_copy, view);
        let restore_paths = if new_ws.is_dir() {
            self.collect_ignored_paths_in_workspace(&new_ws)
        } else {
            Vec::new()
        };

        let before_working_copy = super::operation::working_copy_state_ref(before_record);
        let mut target_unmaterialized = before_working_copy.clone();
        target_unmaterialized.desired_view = new_view_id;
        target_unmaterialized.desired_state = new_state;
        target_unmaterialized.materialized_state = None;
        target_unmaterialized.materialized_manifest = None;
        let mut target_materialized = target_unmaterialized.clone();
        target_materialized.materialized_state = Some(new_state);

        let before_state = RepoStateRef {
            view: Some(ViewStateRef {
                name: old_view_name.clone(),
                state: old_state,
                set_id: None,
            }),
            working_copy: Some(before_working_copy.clone()),
            git: None,
        };
        let after_state = RepoStateRef {
            view: Some(ViewStateRef {
                name: view.to_string(),
                state: new_state,
                set_id: None,
            }),
            working_copy: Some(target_materialized.clone()),
            git: None,
        };

        let working_copy_target = EffectTarget::WorkingCopy { working_copy };
        let before_working_value = EffectValue::WorkingCopy(before_working_copy);
        let target_unmaterialized_value = EffectValue::WorkingCopy(target_unmaterialized.clone());
        let mut effects = Vec::new();
        if before_working_value != target_unmaterialized_value {
            effects.push(EffectPlan {
                ordinal: 0,
                target: working_copy_target.clone(),
                expected_old: before_working_value.clone(),
                expected_new: target_unmaterialized_value.clone(),
            });
        }

        self.plan_shelve_ignored_paths(working_copy, &old_view_name, &ignored_paths, &mut effects)?;

        let mut removal_paths: HashSet<String> = old_files
            .difference(&new_files)
            .cloned()
            .chain(old_directories.difference(&new_directories).cloned())
            .chain(new_absent_files.iter().cloned())
            .chain(new_absent_directories.iter().cloned())
            .collect();
        removal_paths.extend(old_files.intersection(&new_directories).cloned());
        removal_paths.extend(old_directories.intersection(&new_files).cloned());
        let mut removal_paths: Vec<String> = removal_paths.into_iter().collect();
        removal_paths.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
        for path in removal_paths {
            let target = EffectTarget::FilesystemPath { path };
            let expected_old = self.observe_filesystem_effect(working_copy, &target)?;
            if expected_old != EffectValue::Absent {
                effects.push(EffectPlan {
                    ordinal: effects.len() as u32,
                    target,
                    expected_old,
                    expected_new: EffectValue::Absent,
                });
            }
        }

        let mut directories_to_create: Vec<String> = new_directories
            .difference(&old_directories)
            .cloned()
            .collect();
        directories_to_create.sort_by_key(|path| path.matches('/').count());
        for path in directories_to_create {
            let target = EffectTarget::FilesystemPath { path };
            let mut expected_old = effects
                .iter()
                .rev()
                .find(|effect| effect.target == target)
                .map(|effect| effect.expected_new.clone())
                .unwrap_or(self.observe_filesystem_effect(working_copy, &target)?);
            if matches!(
                &expected_old,
                EffectValue::File(state) if state.kind != FileKind::Directory
            ) {
                effects.push(EffectPlan {
                    ordinal: effects.len() as u32,
                    target: target.clone(),
                    expected_old,
                    expected_new: EffectValue::Absent,
                });
                expected_old = EffectValue::Absent;
            }
            let mode = match &expected_old {
                EffectValue::File(state) if state.kind == FileKind::Directory => state.mode,
                _ => default_directory_mode(),
            };
            let expected_new = super::operation::filesystem_directory_value(mode);
            if expected_old != expected_new {
                effects.push(EffectPlan {
                    ordinal: effects.len() as u32,
                    target,
                    expected_old,
                    expected_new,
                });
            }
        }

        let mut files_to_write: Vec<String> = new_files.iter().cloned().collect();
        files_to_write.sort();
        for path in files_to_write {
            let target = EffectTarget::FilesystemPath { path: path.clone() };
            let mut expected_old = effects
                .iter()
                .rev()
                .find(|effect| effect.target == target)
                .map(|effect| effect.expected_new.clone())
                .unwrap_or(self.observe_filesystem_effect(working_copy, &target)?);
            if matches!(
                &expected_old,
                EffectValue::File(state) if state.kind == FileKind::Directory
            ) {
                effects.push(EffectPlan {
                    ordinal: effects.len() as u32,
                    target: target.clone(),
                    expected_old,
                    expected_new: EffectValue::Absent,
                });
                expected_old = EffectValue::Absent;
            }
            let bytes = self
                .switch_target_file_bytes(&path, view)?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "target view '{view}' projects '{path}' as a file without materialized content"
                    ),
                })?;
            let materialization = new_file_materialization
                .get(&path)
                .copied()
                .unwrap_or_default();
            let kind = match materialization.kind {
                atomic_core::change::InodeKind::Regular => FileKind::Regular,
                atomic_core::change::InodeKind::Symlink => FileKind::Symlink,
                atomic_core::change::InodeKind::Gitlink => FileKind::Gitlink,
            };
            // A symlink's physical permission bits are the platform's
            // creation mode (Linux yields 0o777 and cannot be changed), not
            // the inode's stored mode; the lease must predict the write that
            // actually happens.
            let mode = if kind == FileKind::Symlink {
                u32::from(atomic_core::output::platform_symlink_mode())
            } else {
                u32::from(materialization.mode)
            };
            let expected_new = EffectValue::File(FileState {
                kind,
                mode,
                content: Hash::of(&bytes),
            });
            if expected_old != expected_new {
                effects.push(EffectPlan {
                    ordinal: effects.len() as u32,
                    target,
                    expected_old,
                    expected_new,
                });
            }
        }

        for path in &restore_paths {
            self.plan_restore_ignored_paths(working_copy, view, path, &mut effects)?;
        }

        effects.push(EffectPlan {
            ordinal: effects.len() as u32,
            target: working_copy_target.clone(),
            expected_old: EffectValue::WorkingCopy(target_unmaterialized.clone()),
            expected_new: EffectValue::WorkingCopy(target_materialized.clone()),
        });

        let prepared = self.prepare_working_copy_transition(
            operation_lock,
            kind,
            relation,
            before_state,
            after_state,
            effects,
            Vec::new(),
            actor,
            operation_timestamp_ms(),
        )?;
        let operation_id = prepared.operation().id();
        let initial_effect = prepared
            .operation()
            .payload()
            .delta
            .effects
            .iter()
            .find(|effect| {
                effect.target == working_copy_target
                    && effect.expected_old == before_working_value
                    && effect.expected_new == target_unmaterialized_value
            })
            .cloned();
        if let Some(initial_effect) = initial_effect {
            let observed_before =
                self.observe_operation_effect(working_copy, &initial_effect.target)?;
            self.apply_working_copy_state_locked(operation_lock, &target_unmaterialized)?;
            let observed_after =
                self.observe_operation_effect(working_copy, &initial_effect.target)?;
            self.record_effect_outcome(
                operation_lock,
                operation_id,
                initial_effect.ordinal,
                observed_before,
                observed_after,
            )?;
        }

        if std::env::var_os("ATOMIC_TRACE_SWITCH").is_some() {
            eprintln!("[switch] {} -> {}", old_view_name, view);
            eprintln!(
                "[switch] old_files={} new_files={}",
                old_files.len(),
                new_files.len()
            );
            for f in old_files.difference(&new_files) {
                eprintln!("[switch] REMOVE (old only): {}", f);
            }
            for f in new_files.difference(&old_files) {
                eprintln!("[switch] ADD (new only): {}", f);
            }
            for effect in &prepared.operation().payload().delta.effects {
                eprintln!(
                    "[switch] EFFECT {} {:?}: {:?} -> {:?}",
                    effect.ordinal, effect.target, effect.expected_old, effect.expected_new
                );
            }
        }

        let mut filesystem_executor =
            SwitchMaterializationExecutor::new(self, operation_lock, prepared.operation().clone());

        // ── Phase 1: Shelve ignored files into the OLD view's workspace ──
        //
        // All ignored files are shelved per-view EXCEPT paths listed in
        // `[workspace] expose` in `.atomic/config.toml`.  Exposed paths
        // persist across all views (tool configs like .opencode/, .vscode/).
        //
        // This uses `rename()` which is O(1) on the same filesystem —
        // no data is copied, just inode pointers are updated.
        //
        // The rule:
        //   - Tracked files      → managed by the graph (phases 2-4)
        //   - Untracked, ignored, exposed  → left alone (persists across views)
        //   - Untracked, ignored, NOT exposed → shelved/restored per-view (phases 1 & 5)
        //   - Untracked, novel   → user's undecided work, left alone
        if !ignored_paths.is_empty() {
            ensure_working_copy_workspace_dir(&self.dot_dir, working_copy, &old_view_name)?;
            for path in &ignored_paths {
                self.execute_shelve_path(operation_lock, &prepared, &old_view_name, path)?;
            }
        }

        // ── Phase 2: Remove tracked files that belong to the old view ──
        //
        // Files visible on the old view but NOT on the new view are
        // removed from disk.
        let mut removed_paths = 0usize;
        let paths_to_remove: Vec<String> = prepared
            .operation()
            .payload()
            .delta
            .effects
            .iter()
            .filter_map(|effect| match (&effect.target, &effect.expected_new) {
                (EffectTarget::FilesystemPath { path }, EffectValue::Absent) => Some(path.clone()),
                _ => None,
            })
            .collect();
        for path in paths_to_remove {
            let pending = filesystem_executor.execute_absence(&path)?;
            if pending.mutated() {
                removed_paths += 1;
                // This failpoint intentionally sits in the effect→receipt crash
                // window. Recovery must infer the landed removal from the new
                // lease even though no receipt exists yet.
                if removed_paths == 1
                    && std::env::var_os("ATOMIC_FAIL_SWITCH_AFTER_FIRST_TRACKED_REMOVAL").is_some()
                {
                    return Err(RepositoryError::Io(std::io::Error::other(
                        "debug failpoint: switch failed after first tracked-path removal",
                    )));
                }
            }
            filesystem_executor.record(pending)?;
        }

        // ── Phase 4: Materialize the new view's tracked files from graph ─
        //
        // Run a complete target materialization. Scoped FILE_INDEX entries let
        // unchanged files skip writes while still proving the whole desired view
        // was output successfully before its materialized state is recorded.
        let result = self.materialize_parallel_with_visibility_journaled(
            working_copy,
            None,
            new_visibility.clone(),
            new_view_id,
            &mut filesystem_executor,
        )?;
        filesystem_executor.complete_remaining(view)?;
        drop(filesystem_executor);

        // ── Phase 5: Restore ignored files from the NEW view's workspace ─
        //
        // Move artifacts from the working-copy-scoped workspace back into the
        // working copy. Again O(1) renames, no data copying.
        if new_ws.is_dir() {
            for path in &restore_paths {
                self.execute_restore_path(operation_lock, &prepared, view, path)?;
            }
        }

        let final_effect = prepared
            .operation()
            .payload()
            .delta
            .effects
            .last()
            .cloned()
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: "switch operation contains no final working-copy effect".to_string(),
            })?;
        let observed_before = self.observe_operation_effect(working_copy, &final_effect.target)?;
        self.apply_working_copy_state_locked(operation_lock, &target_materialized)?;
        let observed_after = self.observe_operation_effect(working_copy, &final_effect.target)?;
        self.record_effect_outcome(
            operation_lock,
            operation_id,
            final_effect.ordinal,
            observed_before,
            observed_after,
        )?;
        self.finalize_operation_verified(operation_lock, operation_id)?;
        Ok(result)
    }

    pub(super) fn execute_shelve_path(
        &self,
        operation_lock: &super::locks::WorkingCopyOperationLockGuard,
        prepared: &super::operation::PreparedSwitchOperation,
        old_view: &str,
        path: &str,
    ) -> Result<(), RepositoryError> {
        let working_copy = operation_lock.working_copy();
        let source_target = EffectTarget::WorkspacePath {
            working_copy,
            path: path.to_string(),
        };
        let shelf_target = EffectTarget::ShelfPath {
            working_copy,
            view: old_view.to_string(),
            path: path.to_string(),
        };
        let effects = &prepared.operation().payload().delta.effects;
        let source_effect = effects
            .iter()
            .find(|effect| {
                effect.target == source_target && effect.expected_new == EffectValue::Absent
            })
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("switch operation omitted workspace shelving lease for '{path}'"),
            })?;
        let final_shelf = effects.iter().find(|effect| {
            effect.target == shelf_target && effect.expected_new == source_effect.expected_old
        });
        if final_shelf.is_none() {
            let write = operation_lock.begin_write_immediate()?;
            let mut txn = write.try_lock_shelf()?;
            let before = self.observe_filesystem_effect(working_copy, &source_effect.target)?;
            if before == source_effect.expected_old {
                let source = self.root.join(path);
                let tombstone = self
                    .working_copy_dot_dir()
                    .join("operation-recovery")
                    .join(prepared.operation().id().to_string())
                    .join("duplicate-workspace")
                    .join(format!("{:010}", source_effect.ordinal));
                if let Some(parent) = tombstone.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                rename_and_sync(&source, &tombstone)?;
            } else if before != source_effect.expected_new {
                self.append_rejected_effect_outcome(
                    &mut txn,
                    prepared.operation(),
                    source_effect.ordinal,
                    before,
                    None,
                )?;
                txn.commit()?;
                return Err(RepositoryError::InvalidOperation {
                    message: format!("duplicate workspace lease diverged for '{path}'"),
                });
            }
            let after = self.observe_filesystem_effect(working_copy, &source_effect.target)?;
            if after != source_effect.expected_new {
                self.append_rejected_effect_outcome(
                    &mut txn,
                    prepared.operation(),
                    source_effect.ordinal,
                    before,
                    Some(after),
                )?;
                txn.commit()?;
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "duplicate workspace effect diverged after mutation for '{path}'"
                    ),
                });
            }
            self.append_successful_effect_outcome(
                &mut txn,
                prepared.operation(),
                source_effect.ordinal,
                before,
                after,
            )?;
            return txn.commit();
        }
        let final_shelf = final_shelf.expect("checked above");
        let stale_shelf = effects.iter().find(|effect| {
            effect.target == shelf_target
                && effect.ordinal < source_effect.ordinal
                && effect.expected_new == EffectValue::Absent
        });

        if let Some(stale) = stale_shelf {
            let write = operation_lock.begin_write_immediate()?;
            let mut txn = write.try_lock_shelf()?;
            let before = self.observe_filesystem_effect(working_copy, &stale.target)?;
            if before == stale.expected_old {
                let stale_path =
                    working_copy_workspace_path(&self.dot_dir, working_copy, old_view).join(path);
                let trash = prepared
                    .backup_root()
                    .join("superseded-shelf")
                    .join(format!("{:010}", stale.ordinal));
                if let Some(parent) = trash.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if trash.exists() {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "stale shelf recovery path already exists: {}",
                            trash.display()
                        ),
                    });
                }
                rename_and_sync(&stale_path, &trash)?;
            } else if before != stale.expected_new {
                self.append_rejected_effect_outcome(
                    &mut txn,
                    prepared.operation(),
                    stale.ordinal,
                    before,
                    None,
                )?;
                txn.commit()?;
                return Err(RepositoryError::InvalidOperation {
                    message: format!("stale shelf lease diverged for '{path}'"),
                });
            }
            let after = self.observe_filesystem_effect(working_copy, &stale.target)?;
            if after != stale.expected_new {
                self.append_rejected_effect_outcome(
                    &mut txn,
                    prepared.operation(),
                    stale.ordinal,
                    before,
                    Some(after),
                )?;
                txn.commit()?;
                return Err(RepositoryError::InvalidOperation {
                    message: format!("stale shelf effect diverged after mutation for '{path}'"),
                });
            }
            self.append_successful_effect_outcome(
                &mut txn,
                prepared.operation(),
                stale.ordinal,
                before,
                after,
            )?;
            txn.commit()?;
        }

        let write = operation_lock.begin_write_immediate()?;
        let mut txn = write.try_lock_shelf()?;
        let source_before = self.observe_filesystem_effect(working_copy, &source_effect.target)?;
        let shelf_before = self.observe_filesystem_effect(working_copy, &final_shelf.target)?;
        if source_before == source_effect.expected_old && shelf_before == final_shelf.expected_old {
            let source = self.root.join(path);
            let destination =
                working_copy_workspace_path(&self.dot_dir, working_copy, old_view).join(path);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            rename_and_sync(&source, &destination)?;
        } else if source_before != source_effect.expected_new
            || shelf_before != final_shelf.expected_new
        {
            self.append_rejected_effect_outcome(
                &mut txn,
                prepared.operation(),
                source_effect.ordinal,
                source_before,
                None,
            )?;
            self.append_rejected_effect_outcome(
                &mut txn,
                prepared.operation(),
                final_shelf.ordinal,
                shelf_before,
                None,
            )?;
            txn.commit()?;
            return Err(RepositoryError::InvalidOperation {
                message: format!("shelf transfer lease diverged for '{path}'"),
            });
        }
        let source_after = self.observe_filesystem_effect(working_copy, &source_effect.target)?;
        let shelf_after = self.observe_filesystem_effect(working_copy, &final_shelf.target)?;
        if source_after != source_effect.expected_new || shelf_after != final_shelf.expected_new {
            self.append_rejected_effect_outcome(
                &mut txn,
                prepared.operation(),
                source_effect.ordinal,
                source_before,
                Some(source_after),
            )?;
            self.append_rejected_effect_outcome(
                &mut txn,
                prepared.operation(),
                final_shelf.ordinal,
                shelf_before,
                Some(shelf_after),
            )?;
            txn.commit()?;
            return Err(RepositoryError::InvalidOperation {
                message: format!("shelf transfer diverged after mutation for '{path}'"),
            });
        }
        self.append_successful_effect_outcome(
            &mut txn,
            prepared.operation(),
            source_effect.ordinal,
            source_before,
            source_after,
        )?;
        self.append_successful_effect_outcome(
            &mut txn,
            prepared.operation(),
            final_shelf.ordinal,
            shelf_before,
            shelf_after,
        )?;
        txn.commit()
    }

    pub(super) fn execute_restore_path(
        &self,
        operation_lock: &super::locks::WorkingCopyOperationLockGuard,
        prepared: &super::operation::PreparedSwitchOperation,
        target_view: &str,
        path: &str,
    ) -> Result<(), RepositoryError> {
        let working_copy = operation_lock.working_copy();
        let shelf_target = EffectTarget::ShelfPath {
            working_copy,
            view: target_view.to_string(),
            path: path.to_string(),
        };
        let destination_target = EffectTarget::WorkspacePath {
            working_copy,
            path: path.to_string(),
        };
        let effects = &prepared.operation().payload().delta.effects;
        let shelf_effect = effects
            .iter()
            .rev()
            .find(|effect| {
                effect.target == shelf_target && effect.expected_new == EffectValue::Absent
            })
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("switch operation omitted shelf restore source for '{path}'"),
            })?;
        let destination_effect = effects
            .iter()
            .rev()
            .find(|effect| {
                effect.target == destination_target
                    && effect.expected_new == shelf_effect.expected_old
            })
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("switch operation omitted shelf restore destination for '{path}'"),
            })?;
        let write = operation_lock.begin_write_immediate()?;
        let mut txn = write.try_lock_shelf()?;
        let shelf_before = self.observe_filesystem_effect(working_copy, &shelf_effect.target)?;
        let destination_before =
            self.observe_filesystem_effect(working_copy, &destination_effect.target)?;
        if shelf_before == shelf_effect.expected_old
            && destination_before == destination_effect.expected_old
        {
            let source =
                working_copy_workspace_path(&self.dot_dir, working_copy, target_view).join(path);
            let destination = self.root.join(path);
            let parent = destination
                .parent()
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("shelf restore destination has no parent: '{path}'"),
                })?;
            if !parent.is_dir() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "shelf restore parent '{}' is not materialized",
                        parent.display()
                    ),
                });
            }
            rename_and_sync(&source, &destination)?;
        } else if shelf_before != shelf_effect.expected_new
            || destination_before != destination_effect.expected_new
        {
            self.append_rejected_effect_outcome(
                &mut txn,
                prepared.operation(),
                shelf_effect.ordinal,
                shelf_before,
                None,
            )?;
            self.append_rejected_effect_outcome(
                &mut txn,
                prepared.operation(),
                destination_effect.ordinal,
                destination_before,
                None,
            )?;
            txn.commit()?;
            return Err(RepositoryError::InvalidOperation {
                message: format!("shelf restore lease diverged for '{path}'"),
            });
        }
        let shelf_after = self.observe_filesystem_effect(working_copy, &shelf_effect.target)?;
        let destination_after =
            self.observe_filesystem_effect(working_copy, &destination_effect.target)?;
        if shelf_after != shelf_effect.expected_new
            || destination_after != destination_effect.expected_new
        {
            self.append_rejected_effect_outcome(
                &mut txn,
                prepared.operation(),
                shelf_effect.ordinal,
                shelf_before,
                Some(shelf_after),
            )?;
            self.append_rejected_effect_outcome(
                &mut txn,
                prepared.operation(),
                destination_effect.ordinal,
                destination_before,
                Some(destination_after),
            )?;
            txn.commit()?;
            return Err(RepositoryError::InvalidOperation {
                message: format!("shelf restore diverged after mutation for '{path}'"),
            });
        }
        self.append_successful_effect_outcome(
            &mut txn,
            prepared.operation(),
            shelf_effect.ordinal,
            shelf_before,
            shelf_after,
        )?;
        self.append_successful_effect_outcome(
            &mut txn,
            prepared.operation(),
            destination_effect.ordinal,
            destination_before,
            destination_after,
        )?;
        txn.commit()
    }

    /// `old_view`'s workspace shelf (RFC §7.2: old shelf / current disk /
    /// target shelf, never overwriting unexplained disk content).
    ///
    /// Shared by view switch and bound-HEAD adoption so both transitions use
    /// the identical lease plan and executor.
    pub(super) fn plan_shelve_ignored_paths(
        &self,
        working_copy: WorkingCopyId,
        old_view: &str,
        ignored_paths: &[String],
        effects: &mut Vec<EffectPlan>,
    ) -> Result<(), RepositoryError> {
        for path in ignored_paths {
            let source = EffectTarget::WorkspacePath {
                working_copy,
                path: path.clone(),
            };
            let source_old = self.observe_filesystem_effect(working_copy, &source)?;
            if source_old == EffectValue::Absent {
                continue;
            }
            let shelf = EffectTarget::ShelfPath {
                working_copy,
                view: old_view.to_string(),
                path: path.clone(),
            };
            let shelf_old = self.observe_filesystem_effect(working_copy, &shelf)?;
            if shelf_old == source_old {
                effects.push(EffectPlan {
                    ordinal: effects.len() as u32,
                    target: source,
                    expected_old: source_old,
                    expected_new: EffectValue::Absent,
                });
                continue;
            }
            if shelf_old != EffectValue::Absent {
                effects.push(EffectPlan {
                    ordinal: effects.len() as u32,
                    target: shelf.clone(),
                    expected_old: shelf_old,
                    expected_new: EffectValue::Absent,
                });
            }
            effects.push(EffectPlan {
                ordinal: effects.len() as u32,
                target: source,
                expected_old: source_old.clone(),
                expected_new: EffectValue::Absent,
            });
            effects.push(EffectPlan {
                ordinal: effects.len() as u32,
                target: shelf,
                expected_old: EffectValue::Absent,
                expected_new: source_old,
            });
        }
        Ok(())
    }

    /// Plan the collision-safe restore of `new_view`'s shelved artifacts into
    /// the working copy. A destination occupied by unexplained content is a
    /// typed refusal, never an overwrite.
    pub(super) fn plan_restore_ignored_paths(
        &self,
        working_copy: WorkingCopyId,
        new_view: &str,
        path: &str,
        effects: &mut Vec<EffectPlan>,
    ) -> Result<(), RepositoryError> {
        let shelf = EffectTarget::ShelfPath {
            working_copy,
            view: new_view.to_string(),
            path: path.to_string(),
        };
        let shelf_old = effects
            .iter()
            .rev()
            .find(|effect| effect.target == shelf)
            .map(|effect| effect.expected_new.clone())
            .unwrap_or(self.observe_filesystem_effect(working_copy, &shelf)?);
        if shelf_old == EffectValue::Absent {
            return Ok(());
        }
        let destination = EffectTarget::WorkspacePath {
            working_copy,
            path: path.to_string(),
        };
        let destination_old = effects
            .iter()
            .rev()
            .find(|effect| effect.target == destination)
            .map(|effect| effect.expected_new.clone())
            .unwrap_or(self.observe_filesystem_effect(working_copy, &destination)?);
        if destination_old != EffectValue::Absent {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "cannot restore shelf path '{path}': working-copy destination has unexplained content"
                ),
            });
        }
        effects.push(EffectPlan {
            ordinal: effects.len() as u32,
            target: shelf,
            expected_old: shelf_old.clone(),
            expected_new: EffectValue::Absent,
        });
        effects.push(EffectPlan {
            ordinal: effects.len() as u32,
            target: destination,
            expected_old: EffectValue::Absent,
            expected_new: shelf_old,
        });
        Ok(())
    }

    /// Ignored-path candidates for the shelf planner, excluding graph-tracked
    /// paths and `[workspace] expose` policy paths.
    pub(super) fn collect_switch_ignored_paths(
        &self,
        tracked_paths: &HashSet<String>,
    ) -> Vec<String> {
        let repo_expose = atomic_config::RepoConfig::load(&self.config_path())
            .unwrap_or_default()
            .workspace
            .expose;
        let mut expose_patterns = atomic_config::GlobalConfig::load()
            .map(|config| config.workspace.expose)
            .unwrap_or_default();
        for pattern in repo_expose {
            if !expose_patterns.contains(&pattern) {
                expose_patterns.push(pattern);
            }
        }
        self.collect_ignored_paths_on_disk()
            .into_iter()
            .filter(|path| {
                !tracked_paths.contains(path)
                    && !tracked_paths
                        .iter()
                        .any(|tracked| tracked.starts_with(&format!("{path}/")))
                    && !expose_patterns
                        .iter()
                        .any(|pattern| path == pattern || path.starts_with(&format!("{pattern}/")))
            })
            .collect()
    }

    pub(super) fn collect_ignored_paths_in_workspace(&self, root: &Path) -> Vec<String> {
        // R4/CB-7B: the worktree's own ignore rules — but when the rules
        // file itself was shelved into a view workspace (it is an ignored
        // path like any other), the shelved artifacts must stay discoverable
        // for restoration. Union the worktree rules with the workspace's own
        // shelved rules so a path ignored by either source is collected; a
        // candidate without shelf content is skipped by the planner anyway.
        let worktree_rules = self.load_ignore_rules();
        let workspace_rules = IgnoreRules::load(root);
        let mut result = Vec::new();
        fn walk(
            root: &Path,
            dir: &Path,
            worktree_rules: &crate::ignore::IgnoreRules,
            workspace_rules: &crate::ignore::IgnoreRules,
            out: &mut Vec<String>,
        ) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(relative) = path.strip_prefix(root) else {
                    continue;
                };
                let is_directory = path.is_dir();
                let ignored = worktree_rules.is_ignored(relative, is_directory)
                    || workspace_rules.is_ignored(relative, is_directory);
                if ignored {
                    if let Some(relative) = relative.to_str() {
                        out.push(relative.to_string());
                    }
                } else if is_directory {
                    walk(root, &path, worktree_rules, workspace_rules, out);
                }
            }
        }
        walk(root, root, &worktree_rules, &workspace_rules, &mut result);
        result.sort();
        result
    }

    /// Walk the working copy and collect relative paths of files and
    /// directories that match `.atomicignore` rules.
    ///
    /// Only top-level ignored entries are returned — if `node_modules/`
    /// matches, we return `"node_modules"` rather than enumerating every
    /// file inside it (the caller will `remove_dir_all`).
    ///
    /// Paths that live inside `.atomic/` are never returned.
    fn collect_ignored_paths_on_disk(&self) -> Vec<String> {
        let rules = self.load_ignore_rules();
        let mut result = Vec::new();

        // Recursive walker that stops descending into ignored directories.
        fn walk(
            root: &Path,
            dir: &Path,
            rules: &crate::ignore::IgnoreRules,
            out: &mut Vec<String>,
        ) {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(_) => return,
            };
            for entry in entries.flatten() {
                let abs = entry.path();
                let rel = match abs.strip_prefix(root) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                // Never touch VCS administrative directories. `.git` can be
                // ignored by `.atomicignore` after `atomic git import`, but
                // it remains owned by Git rather than a view workspace.
                if rel.starts_with(DOT_DIR) || rel.starts_with(".git") {
                    continue;
                }

                let is_dir = abs.is_dir();

                if rules.is_ignored(rel, is_dir) {
                    // Collect the top-level ignored entry — don't recurse.
                    if let Some(s) = rel.to_str() {
                        out.push(s.to_string());
                    }
                } else if is_dir {
                    // Not ignored — recurse to find ignored children.
                    walk(root, &abs, rules, out);
                }
                // Non-ignored files are left alone.
            }
        }

        walk(&self.root, &self.root, &rules, &mut result);
        result
    }
}
