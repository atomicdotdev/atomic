use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use atomic_core::pristine::{MutTxnT, Pristine, WriteTxn};
use atomic_core::WorkingCopyId;
use fs2::FileExt;

use super::Repository;
use crate::{RepositoryError, RepositoryLockKind};

const COMMON_OPERATION_LOCK: &str = "bridge.lock";
const WORKING_COPIES_DIR: &str = "working-copies";
const WORKING_COPY_OPERATION_LOCK: &str = "operation.lock";
const WORKING_COPY_SHELF_LOCK: &str = "shelf.lock";
const DEFERRED_TREE_LOCK: &str = "deferred-tree-alignment.lock";

/// A repository-common operation lock.
///
/// This is the first stage in the operation lock hierarchy. It intentionally
/// remains distinct from the legacy `shadow-commit.lock`; a later migration can
/// route that pipeline through this guard without changing the canonical path.
pub struct RepositoryCommonLockGuard {
    _common_lock: AdvisoryFileLock,
    dot_dir: PathBuf,
    pristine: Arc<Pristine>,
    reentrant: bool,
}

/// A repository operation lock scoped to one persistent working copy.
///
/// The embedded common guard keeps the repository-common lock alive until this
/// guard and every write stage borrowed from it are dropped.
pub(super) struct WorkingCopyOperationLockGuard {
    _working_copy_lock: AdvisoryFileLock,
    common: RepositoryCommonLockGuard,
    working_copy: WorkingCopyId,
}

/// A pristine write transaction entered through the common operation lock.
pub(super) struct CommonPristineWriteTxn<'a> {
    txn: WriteTxn<'a>,
    _common: &'a RepositoryCommonLockGuard,
}

/// A pristine write transaction entered through the ordered operation locks.
///
/// Final shelf and deferred-tree locks are only constructible by consuming this
/// stage, which makes the acquisition order structural rather than conventional.
pub(super) struct OrderedPristineWriteTxn<'a> {
    txn: WriteTxn<'a>,
    operation: &'a WorkingCopyOperationLockGuard,
}

/// A pristine write transaction holding one final external-resource lock.
///
/// The final lock is private and owned by the write stage, so it cannot be
/// acquired before or retained independently of the ordered transaction.
pub(super) struct FinalResourceWriteTxn<'a> {
    _final_lock: AdvisoryFileLock,
    write: OrderedPristineWriteTxn<'a>,
}

thread_local! {
#[allow(clippy::type_complexity)] // (count, handle, reentrant) tuple is module-local
    static HELD_LOCKS: RefCell<HashMap<PathBuf, (usize, Weak<File>, bool)>> = RefCell::new(HashMap::new());
}

/// One thread-owned advisory lock acquisition.
///
/// Nested repository operations on the same thread reuse the outer workspace
/// transaction's OS lock. Other threads and processes still contend on the file.
struct AdvisoryFileLock {
    file: Arc<File>,
    path: PathBuf,
}

impl AdvisoryFileLock {
    /// A same-thread alias of this held lock (CB-8B): the alias shares the
    /// underlying OS lock and increments the nesting count, so a nested
    /// executor can hold its own guard under the caller's boundary without
    /// a second flock. Dropping the alias only decrements the count.
    fn alias(&self) -> Self {
        HELD_LOCKS.with(|held| {
            let mut held = held.borrow_mut();
            match held.get_mut(&self.path) {
                Some((count, _, _)) => *count += 1,
                None => {
                    // The outer guard always has a HELD_LOCKS entry; a
                    // missing entry means the alias is outliving its
                    // boundary, which the caller must never do.
                    let entry =
                        held.entry(self.path.clone())
                            .or_insert((0usize, Weak::new(), false));
                    entry.0 += 1;
                }
            }
        });
        Self {
            file: Arc::clone(&self.file),
            path: self.path.clone(),
        }
    }
}

impl Drop for AdvisoryFileLock {
    fn drop(&mut self) {
        let release = HELD_LOCKS.with(|held| {
            let mut held = held.borrow_mut();
            match held.get_mut(&self.path) {
                Some((count, _, _)) if *count > 1 => {
                    *count -= 1;
                    false
                }
                Some(_) => {
                    held.remove(&self.path);
                    true
                }
                None => false,
            }
        });
        if release {
            let _ = FileExt::unlock(&*self.file);
        }
    }
}

#[derive(Clone, Copy)]
enum FinalResource {
    Shelf,
    DeferredTree,
}

impl Repository {
    pub(super) fn common_operation_lock_path(&self) -> PathBuf {
        common_operation_lock_path(&self.dot_dir)
    }

    #[cfg(test)]
    pub(super) fn working_copy_operation_lock_path(&self, working_copy: WorkingCopyId) -> PathBuf {
        working_copy_operation_lock_path(&self.dot_dir, working_copy)
    }

    #[cfg(test)]
    pub(super) fn working_copy_shelf_lock_path(&self, working_copy: WorkingCopyId) -> PathBuf {
        working_copy_shelf_lock_path(&self.dot_dir, working_copy)
    }

    #[cfg(test)]
    pub(super) fn deferred_tree_operation_lock_path(&self) -> PathBuf {
        deferred_tree_operation_lock_path(&self.dot_dir)
    }

    /// Try to enter the repository-common operation scope without blocking.
    ///
    /// The returned guard is the migration target for the legacy shadow lock;
    /// callers that also touch a physical working copy must advance through
    /// [`RepositoryCommonLockGuard::try_lock_working_copy`].
    pub(super) fn try_lock_common_operation(
        &self,
    ) -> Result<RepositoryCommonLockGuard, RepositoryError> {
        self.try_lock_common_operation_with_reentrancy(false)
    }

    pub(super) fn try_lock_common_operation_with_reentrancy(
        &self,
        reentrant: bool,
    ) -> Result<RepositoryCommonLockGuard, RepositoryError> {
        let path = self.common_operation_lock_path();
        let common_lock = try_lock_file(&path, RepositoryLockKind::Common, reentrant)?;
        Ok(RepositoryCommonLockGuard {
            _common_lock: common_lock,
            dot_dir: self.dot_dir.clone(),
            pristine: Arc::clone(&self.pristine),
            reentrant,
        })
    }

    /// Try to enter the common and per-working-copy operation scopes in order.
    pub(super) fn try_lock_operation(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<WorkingCopyOperationLockGuard, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        self.try_lock_common_operation()?
            .try_lock_working_copy(working_copy)
    }

    /// Enter the operation hierarchy as a workspace transaction owner.
    ///
    /// Only this outer boundary permits same-thread nested repository operations
    /// to reuse its locks. Ordinary independent acquisitions still contend.
    pub(super) fn try_lock_workspace_operation(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<WorkingCopyOperationLockGuard, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        self.try_lock_common_operation_with_reentrancy(true)?
            .try_lock_working_copy(working_copy)
    }
}

impl RepositoryCommonLockGuard {
    /// Begin an immediately durable pristine write after the common lock.
    ///
    /// Repository-scoped operations use this stage when no physical working-copy
    /// or final shelf/deferred-tree resource participates in the mutation.
    pub(super) fn begin_write_immediate(
        &self,
    ) -> Result<CommonPristineWriteTxn<'_>, RepositoryError> {
        let txn = self
            .pristine
            .write_txn_immediate()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(CommonPristineWriteTxn { txn, _common: self })
    }

    /// Advance from the common lock to one working-copy lock without blocking.
    pub(super) fn try_lock_working_copy(
        self,
        working_copy: WorkingCopyId,
    ) -> Result<WorkingCopyOperationLockGuard, RepositoryError> {
        let path = working_copy_operation_lock_path(&self.dot_dir, working_copy);
        let working_copy_lock = try_lock_file(
            &path,
            RepositoryLockKind::WorkingCopy { id: working_copy },
            self.reentrant,
        )?;
        Ok(WorkingCopyOperationLockGuard {
            _working_copy_lock: working_copy_lock,
            common: self,
            working_copy,
        })
    }

    /// A same-thread alias of this held common lock (CB-8B): the nested
    /// projection executor builds its own working-copy guard under the
    /// caller's shadow-commit boundary without a second flock on the common
    /// file. Dropping the alias only decrements the nesting count.
    pub(super) fn alias(&self) -> Self {
        Self {
            _common_lock: self._common_lock.alias(),
            dot_dir: self.dot_dir.clone(),
            pristine: Arc::clone(&self.pristine),
            reentrant: self.reentrant,
        }
    }
}

impl WorkingCopyOperationLockGuard {
    pub(super) fn working_copy(&self) -> WorkingCopyId {
        self.working_copy
    }

    /// A same-thread alias of the embedded common guard (CB-10A review R6):
    /// callers nested in a workspace transaction build their own mapping
    /// operations under the held boundary without a second flock.
    pub(super) fn common_alias(&self) -> RepositoryCommonLockGuard {
        self.common.alias()
    }

    /// Begin a pristine write only after both nonblocking file locks are held.
    ///
    /// Participating operation writers contend on the common lock before this
    /// method is reachable, so redb's blocking writer serialization is not used
    /// as operation-level contention control.
    #[cfg(test)]
    pub(super) fn begin_write(&self) -> Result<OrderedPristineWriteTxn<'_>, RepositoryError> {
        let txn = self
            .common
            .pristine
            .write_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(OrderedPristineWriteTxn {
            txn,
            operation: self,
        })
    }

    /// Begin an immediately durable pristine write after both operation locks.
    ///
    /// Prepared operations and effect receipts must use this stage so redb fsyncs
    /// the journal before or after the corresponding external effect.
    pub(super) fn begin_write_immediate(
        &self,
    ) -> Result<OrderedPristineWriteTxn<'_>, RepositoryError> {
        let txn = self
            .common
            .pristine
            .write_txn_immediate()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(OrderedPristineWriteTxn {
            txn,
            operation: self,
        })
    }
}

impl CommonPristineWriteTxn<'_> {
    pub(super) fn commit(self) -> Result<(), RepositoryError> {
        self.txn
            .commit()
            .map_err(|error| RepositoryError::Database(error.to_string()))
    }

    /// Discard the transaction: no journal entry and no table mutation.
    pub(super) fn abort(self) -> Result<(), RepositoryError> {
        use atomic_core::pristine::MutTxnT;
        self.txn
            .abort()
            .map_err(|error| RepositoryError::Database(error.to_string()))
    }
}

impl<'a> OrderedPristineWriteTxn<'a> {
    /// Acquire the working-copy shelf lock after entering the write stage.
    pub(super) fn try_lock_shelf(self) -> Result<FinalResourceWriteTxn<'a>, RepositoryError> {
        self.try_lock_final_resource(FinalResource::Shelf)
    }

    /// Acquire the repository deferred-tree lock after entering the write stage.
    pub(super) fn try_lock_deferred_tree(
        self,
    ) -> Result<FinalResourceWriteTxn<'a>, RepositoryError> {
        self.try_lock_final_resource(FinalResource::DeferredTree)
    }

    fn try_lock_final_resource(
        self,
        resource: FinalResource,
    ) -> Result<FinalResourceWriteTxn<'a>, RepositoryError> {
        let working_copy = self.operation.working_copy;
        let (path, kind) = match resource {
            FinalResource::Shelf => (
                working_copy_shelf_lock_path(&self.operation.common.dot_dir, working_copy),
                RepositoryLockKind::Shelf { id: working_copy },
            ),
            FinalResource::DeferredTree => (
                deferred_tree_operation_lock_path(&self.operation.common.dot_dir),
                RepositoryLockKind::DeferredTree,
            ),
        };
        let final_lock = try_lock_file(&path, kind, self.operation.common.reentrant)?;
        Ok(FinalResourceWriteTxn {
            _final_lock: final_lock,
            write: self,
        })
    }

    pub(super) fn commit(self) -> Result<(), RepositoryError> {
        self.txn
            .commit()
            .map_err(|error| RepositoryError::Database(error.to_string()))
    }
}

impl FinalResourceWriteTxn<'_> {
    pub(super) fn commit(self) -> Result<(), RepositoryError> {
        let Self { _final_lock, write } = self;
        let result = write.commit();
        drop(_final_lock);
        result
    }
}

impl<'a> Deref for CommonPristineWriteTxn<'a> {
    type Target = WriteTxn<'a>;

    fn deref(&self) -> &Self::Target {
        &self.txn
    }
}

impl DerefMut for CommonPristineWriteTxn<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.txn
    }
}

impl<'a> Deref for OrderedPristineWriteTxn<'a> {
    type Target = WriteTxn<'a>;

    fn deref(&self) -> &Self::Target {
        &self.txn
    }
}

impl DerefMut for OrderedPristineWriteTxn<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.txn
    }
}

impl<'a> Deref for FinalResourceWriteTxn<'a> {
    type Target = WriteTxn<'a>;

    fn deref(&self) -> &Self::Target {
        &self.write
    }
}

impl DerefMut for FinalResourceWriteTxn<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.write
    }
}

fn common_operation_lock_path(dot_dir: &Path) -> PathBuf {
    dot_dir.join(COMMON_OPERATION_LOCK)
}

fn working_copy_operation_lock_path(dot_dir: &Path, working_copy: WorkingCopyId) -> PathBuf {
    dot_dir
        .join(WORKING_COPIES_DIR)
        .join(working_copy.to_string())
        .join(WORKING_COPY_OPERATION_LOCK)
}

fn working_copy_shelf_lock_path(dot_dir: &Path, working_copy: WorkingCopyId) -> PathBuf {
    dot_dir
        .join(WORKING_COPIES_DIR)
        .join(working_copy.to_string())
        .join(WORKING_COPY_SHELF_LOCK)
}

fn deferred_tree_operation_lock_path(dot_dir: &Path) -> PathBuf {
    dot_dir.join(DEFERRED_TREE_LOCK)
}

fn try_lock_file(
    path: &Path,
    kind: RepositoryLockKind,
    reentrant_owner: bool,
) -> Result<AdvisoryFileLock, RepositoryError> {
    let path = path.to_path_buf();
    let reentrant = HELD_LOCKS.with(|held| {
        let mut held = held.borrow_mut();
        let (count, file, reentrant) = held.get_mut(&path)?;
        if !*reentrant {
            return None;
        }
        let file = file.upgrade()?;
        *count += 1;
        Some(file)
    });
    if let Some(file) = reentrant {
        return Ok(AdvisoryFileLock { file, path });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            let file = Arc::new(file);
            HELD_LOCKS.with(|held| {
                held.borrow_mut()
                    .insert(path.clone(), (1, Arc::downgrade(&file), reentrant_owner));
            });
            Ok(AdvisoryFileLock { file, path })
        }
        Err(error) if is_lock_contended(&error) => {
            Err(RepositoryError::LockContended { lock: kind, path })
        }
        Err(error) => Err(RepositoryError::Io(error)),
    }
}

/// Classify the cross-platform error returned by a contended `fs2` try-lock.
pub(super) fn is_lock_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || (error.raw_os_error().is_some()
            && error.raw_os_error() == fs2::lock_contended_error().raw_os_error())
}
