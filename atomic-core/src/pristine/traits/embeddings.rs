//! Embeddings trait for vector similarity search.

use crate::pristine::error::PristineError;
use crate::pristine::vault::{EmbeddingRecord, SearchResult};

/// Read-only embedding operations.
pub trait EmbeddingsTxnT {
    /// Retrieve an embedding by path and chunk index.
    fn get_embedding(
        &self,
        path: &str,
        chunk_idx: u32,
    ) -> Result<Option<EmbeddingRecord>, PristineError>;

    /// List all embedding keys for a given path (all chunks).
    fn list_embeddings(&self, path: &str) -> Result<Vec<(u32, EmbeddingRecord)>, PristineError>;

    /// Count total embeddings in the store.
    fn count_embeddings(&self) -> Result<usize, PristineError>;

    /// Exact nearest-neighbor search (cosine similarity).
    /// Returns top-k results sorted by descending score.
    fn search_embeddings(
        &self,
        query_vector: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, PristineError>;
}

/// Write operations on the embeddings table.
pub trait EmbeddingsMutTxnT: EmbeddingsTxnT {
    /// Store an embedding for a path and chunk index.
    fn put_embedding(
        &mut self,
        path: &str,
        chunk_idx: u32,
        record: &EmbeddingRecord,
    ) -> Result<(), PristineError>;

    /// Delete all embeddings for a given path.
    fn del_embeddings(&mut self, path: &str) -> Result<usize, PristineError>;

    /// Initialize the embeddings table (create if it doesn't exist).
    fn init_embeddings(&mut self) -> Result<(), PristineError>;
}
