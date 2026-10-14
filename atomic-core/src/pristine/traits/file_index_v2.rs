//! Versioned working-copy file-index transaction traits.

use crate::pristine::{FileIndexV2Entry, FileIndexV2Key, PristineError};
use crate::types::WorkingCopyId;

/// Read access to the versioned working-copy file index.
pub trait FileIndexV2TxnT {
    /// Fetch one row. A missing table or row is a cache miss and must rebuild.
    fn get_file_index_v2(
        &self,
        working_copy: WorkingCopyId,
        path: &[u8],
    ) -> Result<Option<FileIndexV2Entry>, PristineError> {
        Ok(self
            .get_file_index_v2_batch(&[FileIndexV2Key::new(working_copy, path.to_vec())?])?
            .into_iter()
            .next()
            .flatten())
    }

    /// Fetch rows in input order using one table open.
    fn get_file_index_v2_batch(
        &self,
        keys: &[FileIndexV2Key],
    ) -> Result<Vec<Option<FileIndexV2Entry>>, PristineError>;

    /// Iterate one working copy's rows in raw path byte order.
    fn iter_file_index_v2(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<Vec<(Vec<u8>, FileIndexV2Entry)>, PristineError>;
}

/// Write access to the versioned working-copy file index.
pub trait FileIndexV2MutTxnT: FileIndexV2TxnT {
    fn put_file_index_v2(
        &mut self,
        key: &FileIndexV2Key,
        entry: &FileIndexV2Entry,
    ) -> Result<(), PristineError> {
        self.put_file_index_v2_batch(&[(key.clone(), entry.clone())])
    }

    /// Insert or replace rows using one table open.
    fn put_file_index_v2_batch(
        &mut self,
        entries: &[(FileIndexV2Key, FileIndexV2Entry)],
    ) -> Result<(), PristineError>;

    fn del_file_index_v2(&mut self, key: &FileIndexV2Key) -> Result<(), PristineError> {
        self.del_file_index_v2_batch(std::slice::from_ref(key))
    }

    /// Delete rows using one table open.
    fn del_file_index_v2_batch(&mut self, keys: &[FileIndexV2Key]) -> Result<(), PristineError>;
}
