use super::*;
use crate::InsertOptions;
use atomic_core::change::{Author, Change, ChangeHeader};
use atomic_core::types::Base32;
use atomic_core::WorkingCopyId;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use tempfile::TempDir;

mod attribute_tests;
mod binding_fetch_tests;
mod binding_store_tests;
mod binding_verify_tests;
mod bridge_git_journal_tests;
mod capability_tests;
mod change_tests;
mod conflict_restore_tests;
mod conflict_surface_tests;
mod content_filter_tests;
mod cross_view_merge_tests;
mod cutover_tests;
mod delete_propagation_tests;
mod directory_lifecycle_tests;
mod edit_tests;
mod history_tests;
mod init_tests;
mod integration_tests;
mod materialize_fail_closed_tests;
mod merge_property_tests;
mod native_index_repair_tests;
mod nested_parent_tests;
mod operation_head_tests;
mod operation_lock_tests;
mod operation_query_tests;
mod operation_recovery_tests;
mod operation_routing_tests;
mod operation_undo_tests;
mod path_claim_migration_tests;
mod projection_effects_tests;
mod record_duplication_tests;
mod record_tests;
mod ref_mapping_tests;
mod rename_tests;
mod resurrection_tests;
mod shadow_lock_tests;
mod snapshot_tests;
mod stale_conflict_reconcile_tests;
mod status_tests;
mod synthesis_tests;
mod tag_projection_tests;

mod tracking_tests;
mod tree_projection_tests;
mod verify_tests;
mod view_tests;
mod working_copy_identity_tests;
mod working_copy_reconcile_tests;
mod workspace_txn_tests;

// ── Shared Helpers ──────────────────────────────────────────────────────

/// Test capability that binds a repository handle to its persistent working-copy ID.
///
/// Production APIs remain compile-time explicit; tests that use the shared fixture
/// carry the capability in this wrapper instead of repeatedly rediscovering it.
pub(crate) struct TestRepository {
    repo: Repository,
    working_copy: WorkingCopyId,
}

#[allow(dead_code)]
impl TestRepository {
    fn new(repo: Repository) -> Self {
        let working_copy = repo.require_working_copy_id().unwrap();
        Self { repo, working_copy }
    }

    pub fn working_copy(&self) -> WorkingCopyId {
        self.working_copy
    }

    pub fn ignore_rules(&self) -> IgnoreRules {
        self.repo.ignore_rules(self.working_copy).unwrap()
    }

    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        self.repo
            .is_ignored(self.working_copy, path, is_dir)
            .unwrap()
    }

    pub fn archive<P: AsRef<Path>>(
        &self,
        destination: P,
        options: ArchiveOptions,
    ) -> Result<ArchiveOutcome, RepositoryError> {
        self.repo.archive(self.working_copy, destination, options)
    }

    pub fn archive_tag<P: AsRef<Path>>(
        &self,
        tag_name: &str,
        destination: P,
        options: ArchiveOptions,
    ) -> Result<ArchiveOutcome, RepositoryError> {
        self.repo
            .archive_tag(self.working_copy, tag_name, destination, options)
    }

    pub fn provision_sandbox<P: AsRef<Path>>(
        &self,
        destination: P,
        view: &str,
    ) -> Result<usize, RepositoryError> {
        self.repo
            .provision_sandbox(self.working_copy, destination, view)
    }

    pub fn add<P: AsRef<Path>>(
        &self,
        path: P,
        options: TrackingOptions,
    ) -> Result<TrackingStats, RepositoryError> {
        self.repo.add(self.working_copy, path, options)
    }

    pub fn add_batch(&self, paths: &[&str]) -> Result<usize, RepositoryError> {
        self.repo.add_batch(self.working_copy, paths)
    }

    pub fn add_directory<P: AsRef<Path>>(
        &self,
        path: P,
        options: TrackingOptions,
    ) -> Result<TrackingStats, RepositoryError> {
        self.repo.add_directory(self.working_copy, path, options)
    }

    pub fn remove<P: AsRef<Path>>(
        &self,
        path: P,
        options: TrackingOptions,
    ) -> Result<TrackingStats, RepositoryError> {
        self.repo.remove(self.working_copy, path, options)
    }

    pub fn remove_batch(&self, paths: &[&str]) -> Result<usize, RepositoryError> {
        self.repo.remove_batch(self.working_copy, paths)
    }

    pub fn move_file<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        from: P,
        to: Q,
    ) -> Result<Inode, RepositoryError> {
        self.repo.move_file(self.working_copy, from, to)
    }

    pub fn status(&self, options: StatusOptions) -> Result<RepositoryStatus, RepositoryError> {
        self.repo.status(self.working_copy, options)
    }

    pub fn list_conflicts(
        &self,
    ) -> Result<Vec<(String, Vec<atomic_core::pristine::StoredConflict>)>, RepositoryError> {
        self.repo.list_conflicts(self.working_copy)
    }

    pub fn status_quick(&self) -> Result<RepositoryStatus, RepositoryError> {
        self.repo.status_quick(self.working_copy)
    }

    pub fn status_tracked(&self) -> Result<RepositoryStatus, RepositoryError> {
        self.repo.status_tracked(self.working_copy)
    }

    pub fn is_working_copy_clean(&self) -> Result<bool, RepositoryError> {
        self.repo.is_working_copy_clean(self.working_copy)
    }

    pub fn modified_files(&self) -> Result<Vec<PathBuf>, RepositoryError> {
        self.repo.modified_files(self.working_copy)
    }

    pub fn untracked_files(&self) -> Result<Vec<PathBuf>, RepositoryError> {
        self.repo.untracked_files(self.working_copy)
    }

    pub fn deleted_files(&self) -> Result<Vec<PathBuf>, RepositoryError> {
        self.repo.deleted_files(self.working_copy)
    }

    pub fn record(
        &self,
        header: ChangeHeader,
        options: RecordOptions,
    ) -> Result<RecordOutcome, RecordError> {
        self.repo.record(self.working_copy, header, options)
    }

    pub fn record_with_message(
        &self,
        message: impl Into<String>,
        options: RecordOptions,
    ) -> Result<RecordOutcome, RecordError> {
        self.repo
            .record_with_message(self.working_copy, message, options)
    }

    pub fn record_all(&self, message: impl Into<String>) -> Result<RecordOutcome, RecordError> {
        self.repo.record_all(self.working_copy, message)
    }

    pub fn materialize(&self) -> Result<MaterializeResult, RepositoryError> {
        self.repo.materialize(self.working_copy)
    }

    pub fn materialize_sequential(&self) -> Result<MaterializeResult, RepositoryError> {
        self.repo.materialize_sequential(self.working_copy)
    }

    pub fn materialize_paths(
        &self,
        paths: std::collections::HashSet<String>,
    ) -> Result<MaterializeResult, RepositoryError> {
        self.repo.materialize_paths(self.working_copy, paths)
    }

    pub fn materialize_paths_sequential(
        &self,
        paths: std::collections::HashSet<String>,
    ) -> Result<MaterializeResult, RepositoryError> {
        self.repo
            .materialize_paths_sequential(self.working_copy, paths)
    }

    pub fn materialize_parallel(
        &self,
        paths: Option<std::collections::HashSet<String>>,
    ) -> Result<MaterializeResult, RepositoryError> {
        self.repo.materialize_parallel(self.working_copy, paths)
    }

    pub fn materialize_prefix(&self, prefix: &str) -> Result<MaterializeResult, RepositoryError> {
        self.repo.materialize_prefix(self.working_copy, prefix)
    }

    pub fn switch_view(&mut self, view: &str) -> Result<MaterializeResult, RepositoryError> {
        self.repo.switch_view(self.working_copy, view)
    }

    pub fn set_current_view(&mut self, view: &str) -> Result<(), RepositoryError> {
        self.repo.set_current_view(self.working_copy, view)
    }

    pub fn align_to_view(&mut self, view: &str) -> Result<(), RepositoryError> {
        self.repo.align_to_view(self.working_copy, view)
    }

    pub fn split_view(&mut self, options: SplitOptions) -> Result<SplitOutcome, RepositoryError> {
        self.repo.split_view(self.working_copy, options)
    }

    pub fn verify_working_copy(&self) -> Result<VerifyReport, RepositoryError> {
        self.repo.verify_working_copy(self.working_copy)
    }

    pub fn first_working_copy_conflict_marker(
        &self,
    ) -> Result<Option<(String, u32)>, RepositoryError> {
        self.repo
            .first_working_copy_conflict_marker(self.working_copy)
    }

    pub fn reindex_working_copy(&self) -> Result<usize, RepositoryError> {
        self.repo.reindex_working_copy(self.working_copy)
    }

    pub fn del_file_index(&self, path: &str) -> Result<(), RepositoryError> {
        self.repo.del_file_index(self.working_copy, path)
    }

    pub fn del_file_index_batch(&self, paths: &[&str]) -> Result<(), RepositoryError> {
        self.repo.del_file_index_batch(self.working_copy, paths)
    }

    pub fn update_file_index(
        &self,
        files: &[(String, i64, u32, u64, Hash)],
    ) -> Result<(), RepositoryError> {
        self.repo.update_file_index(self.working_copy, files)
    }
}

impl Deref for TestRepository {
    type Target = Repository;

    fn deref(&self) -> &Self::Target {
        &self.repo
    }
}

impl DerefMut for TestRepository {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.repo
    }
}

#[allow(private_interfaces)]
pub(crate) fn create_temp_repo() -> (TempDir, TestRepository) {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    (temp_dir, TestRepository::new(repo))
}

/// Create a simple test change with the given message.
pub(crate) fn create_test_change(message: &str) -> Change {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test Author", Some("test@example.com")))
        .build();

    Change::new(header, Vec::new(), Vec::new(), Vec::new())
}

/// Create a test change with some content.
pub(super) fn create_test_change_with_content(message: &str, content: &[u8]) -> Change {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test Author", Some("test@example.com")))
        .build();

    Change::new(header, Vec::new(), content.to_vec(), Vec::new())
}
