//! Embeddings trait implementations for WriteTxn.
//!
//! Implements both `EmbeddingsTxnT` (read) and `EmbeddingsMutTxnT` (write) for `WriteTxn`.
//! Embeddings are stored in the EMBEDDINGS table with composite keys "path\0chunk_idx".

use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::tables::{decode_embedding_key, encode_embedding_key, EMBEDDINGS};
use crate::pristine::traits::{EmbeddingsMutTxnT, EmbeddingsTxnT};
use crate::pristine::vault::{EmbeddingRecord, SearchResult};

use redb::{ReadableTable, ReadableTableMetadata};

use super::WriteTxn;

/// Compute cosine similarity between two vectors.
///
/// Returns 0.0 if the vectors have different lengths, are empty, or if either
/// has zero magnitude.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

impl<'a> EmbeddingsTxnT for WriteTxn<'a> {
    fn get_embedding(&self, path: &str, chunk_idx: u32) -> PristineResult<Option<EmbeddingRecord>> {
        let table = match self.txn.open_table(EMBEDDINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(PristineError::from(e)),
        };

        let key = encode_embedding_key(path, chunk_idx);
        let result = match table.get(key.as_str())? {
            Some(guard) => {
                let bytes = guard.value();
                let record: EmbeddingRecord =
                    postcard::from_bytes(bytes).map_err(|e| PristineError::Serialization {
                        message: format!(
                            "failed to deserialize EmbeddingRecord at '{}': {}",
                            key, e
                        ),
                    })?;
                Ok(Some(record))
            }
            None => Ok(None),
        };
        result
    }

    fn list_embeddings(&self, path: &str) -> PristineResult<Vec<(u32, EmbeddingRecord)>> {
        let table = match self.txn.open_table(EMBEDDINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let mut results = Vec::new();
        let prefix = format!("{}\0", path);

        let iter = table.range::<&str>(prefix.as_str()..)?;

        for item in iter {
            let (key_guard, value_guard) = item?;
            let key_str = key_guard.value();

            if !key_str.starts_with(&prefix) {
                break;
            }

            let (_, chunk_idx) = match decode_embedding_key(key_str) {
                Some(decoded) => decoded,
                None => continue,
            };

            let bytes = value_guard.value();
            let record: EmbeddingRecord =
                postcard::from_bytes(bytes).map_err(|e| PristineError::Serialization {
                    message: format!(
                        "failed to deserialize EmbeddingRecord at '{}': {}",
                        key_str, e
                    ),
                })?;
            results.push((chunk_idx, record));
        }

        Ok(results)
    }

    fn count_embeddings(&self) -> PristineResult<usize> {
        let table = match self.txn.open_table(EMBEDDINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(PristineError::from(e)),
        };

        let count = table.len()? as usize;
        Ok(count)
    }

    fn search_embeddings(
        &self,
        query_vector: &[f32],
        top_k: usize,
    ) -> PristineResult<Vec<SearchResult>> {
        let table = match self.txn.open_table(EMBEDDINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };

        // Brute-force cosine similarity search over all embeddings
        let mut scored: Vec<SearchResult> = Vec::new();

        for item in table.iter()? {
            let (key_guard, value_guard) = item?;
            let key_str = key_guard.value();

            let (path, chunk_idx) = match decode_embedding_key(key_str) {
                Some(decoded) => decoded,
                None => continue,
            };

            let bytes = value_guard.value();
            let record: EmbeddingRecord =
                postcard::from_bytes(bytes).map_err(|e| PristineError::Serialization {
                    message: format!(
                        "failed to deserialize EmbeddingRecord at '{}': {}",
                        key_str, e
                    ),
                })?;

            let score = cosine_similarity(query_vector, &record.vector);

            scored.push(SearchResult {
                path: path.to_string(),
                chunk_idx,
                score,
                preview: record.preview,
            });
        }

        // Sort by descending score
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Return top-k
        scored.truncate(top_k);
        Ok(scored)
    }
}

impl<'a> EmbeddingsMutTxnT for WriteTxn<'a> {
    fn put_embedding(
        &mut self,
        path: &str,
        chunk_idx: u32,
        record: &EmbeddingRecord,
    ) -> PristineResult<()> {
        let bytes = postcard::to_allocvec(record).map_err(|e| PristineError::Serialization {
            message: format!("failed to serialize EmbeddingRecord for '{}': {}", path, e),
        })?;

        let key = encode_embedding_key(path, chunk_idx);
        let mut table = self.txn.open_table(EMBEDDINGS)?;
        table.insert(key.as_str(), bytes.as_slice())?;
        Ok(())
    }

    fn del_embeddings(&mut self, path: &str) -> PristineResult<usize> {
        // First collect all keys for this path
        let keys_to_delete: Vec<String> = {
            let table = match self.txn.open_table(EMBEDDINGS) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
                Err(e) => return Err(PristineError::from(e)),
            };

            let prefix = format!("{}\0", path);
            let mut keys = Vec::new();

            let iter = table.range::<&str>(prefix.as_str()..)?;
            for item in iter {
                let (key_guard, _value_guard) = item?;
                let key_str = key_guard.value();

                if !key_str.starts_with(&prefix) {
                    break;
                }

                keys.push(key_str.to_string());
            }

            keys
        };

        let count = keys_to_delete.len();

        // Now delete each key
        if count > 0 {
            let mut table = self.txn.open_table(EMBEDDINGS)?;
            for key in &keys_to_delete {
                table.remove(key.as_str())?;
            }
        }

        Ok(count)
    }

    fn init_embeddings(&mut self) -> PristineResult<()> {
        // Opening the table with a WriteTransaction creates it if it doesn't exist
        let _ = self.txn.open_table(EMBEDDINGS)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::traits::MutTxnT;
    use crate::pristine::vault::EmbeddingRecord;
    use crate::pristine::Pristine;
    use tempfile::tempdir;

    fn make_record(dims: usize, chunk: u32) -> EmbeddingRecord {
        EmbeddingRecord {
            vector: vec![0.1; dims],
            content_hash: [0u8; 32],
            introduced_by: 1,
            chunk_idx: chunk,
            preview: format!("chunk {}", chunk),
        }
    }

    #[test]
    fn test_embedding_crud() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_embeddings().unwrap();

        let record = make_record(384, 0);
        txn.put_embedding("memory/arch.md", 0, &record).unwrap();

        let retrieved = txn.get_embedding("memory/arch.md", 0).unwrap().unwrap();
        assert_eq!(retrieved.vector.len(), 384);
        assert_eq!(retrieved.preview, "chunk 0");

        assert!(txn.get_embedding("nonexistent", 0).unwrap().is_none());
        txn.commit().unwrap();
    }

    #[test]
    fn test_embedding_list_chunks() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_embeddings().unwrap();

        txn.put_embedding("doc.md", 0, &make_record(3, 0)).unwrap();
        txn.put_embedding("doc.md", 1, &make_record(3, 1)).unwrap();
        txn.put_embedding("doc.md", 2, &make_record(3, 2)).unwrap();
        txn.put_embedding("other.md", 0, &make_record(3, 0))
            .unwrap();

        let chunks = txn.list_embeddings("doc.md").unwrap();
        assert_eq!(chunks.len(), 3);

        assert_eq!(txn.count_embeddings().unwrap(), 4);
        txn.commit().unwrap();
    }

    #[test]
    fn test_embedding_delete() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_embeddings().unwrap();

        txn.put_embedding("doc.md", 0, &make_record(3, 0)).unwrap();
        txn.put_embedding("doc.md", 1, &make_record(3, 1)).unwrap();

        let deleted = txn.del_embeddings("doc.md").unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(txn.count_embeddings().unwrap(), 0);
        txn.commit().unwrap();
    }

    #[test]
    fn test_embedding_search() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_embeddings().unwrap();

        // Store embeddings with different vectors
        let mut r1 = make_record(3, 0);
        r1.vector = vec![1.0, 0.0, 0.0];
        r1.preview = "first".to_string();
        txn.put_embedding("a.md", 0, &r1).unwrap();

        let mut r2 = make_record(3, 0);
        r2.vector = vec![0.0, 1.0, 0.0];
        r2.preview = "second".to_string();
        txn.put_embedding("b.md", 0, &r2).unwrap();

        let mut r3 = make_record(3, 0);
        r3.vector = vec![0.9, 0.1, 0.0];
        r3.preview = "third".to_string();
        txn.put_embedding("c.md", 0, &r3).unwrap();

        // Search for vector closest to [1, 0, 0]
        let results = txn.search_embeddings(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].path, "a.md"); // exact match
        assert!(results[0].score > 0.99);
        assert_eq!(results[1].path, "c.md"); // second closest

        txn.commit().unwrap();
    }

    #[test]
    fn test_embedding_persistence() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        {
            let mut txn = pristine.write_txn().unwrap();
            txn.init_embeddings().unwrap();
            txn.put_embedding("doc.md", 0, &make_record(3, 0)).unwrap();
            txn.commit().unwrap();
        }
        {
            let txn = pristine.read_txn().unwrap();
            let record = txn.get_embedding("doc.md", 0).unwrap().unwrap();
            assert_eq!(record.preview, "chunk 0");
        }
    }

    #[test]
    fn test_read_from_empty_store() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        // No init — table doesn't exist yet
        let txn = pristine.read_txn().unwrap();
        assert!(txn.get_embedding("x", 0).unwrap().is_none());
        assert_eq!(txn.list_embeddings("x").unwrap().len(), 0);
        assert_eq!(txn.count_embeddings().unwrap(), 0);
        assert_eq!(txn.search_embeddings(&[1.0, 0.0], 5).unwrap().len(), 0);
    }

    #[test]
    fn test_init_embeddings_idempotent() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_embeddings().unwrap();
        txn.init_embeddings().unwrap(); // should not panic
        txn.put_embedding("a.md", 0, &make_record(3, 0)).unwrap();
        txn.init_embeddings().unwrap(); // still safe after data
        assert_eq!(txn.count_embeddings().unwrap(), 1);
        txn.commit().unwrap();
    }

    #[test]
    fn test_embedding_overwrite() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_embeddings().unwrap();

        let mut r1 = make_record(3, 0);
        r1.preview = "version 1".to_string();
        txn.put_embedding("doc.md", 0, &r1).unwrap();

        let mut r2 = make_record(3, 0);
        r2.preview = "version 2".to_string();
        txn.put_embedding("doc.md", 0, &r2).unwrap();

        // Should have overwritten, not duplicated
        assert_eq!(txn.count_embeddings().unwrap(), 1);
        let retrieved = txn.get_embedding("doc.md", 0).unwrap().unwrap();
        assert_eq!(retrieved.preview, "version 2");

        txn.commit().unwrap();
    }

    #[test]
    fn test_del_embeddings_nonexistent() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_embeddings().unwrap();
        let deleted = txn.del_embeddings("nonexistent").unwrap();
        assert_eq!(deleted, 0);
        txn.commit().unwrap();
    }

    #[test]
    fn test_del_embeddings_only_target_path() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_embeddings().unwrap();

        txn.put_embedding("doc.md", 0, &make_record(3, 0)).unwrap();
        txn.put_embedding("doc.md", 1, &make_record(3, 1)).unwrap();
        txn.put_embedding("other.md", 0, &make_record(3, 0))
            .unwrap();

        let deleted = txn.del_embeddings("doc.md").unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(txn.count_embeddings().unwrap(), 1);

        // other.md should still be there
        assert!(txn.get_embedding("other.md", 0).unwrap().is_some());

        txn.commit().unwrap();
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let score = cosine_similarity(&[1.0, 0.0, 0.0], &[1.0, 0.0, 0.0]);
        assert!((score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let score = cosine_similarity(&[1.0, 0.0, 0.0], &[0.0, 1.0, 0.0]);
        assert!(score.abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_mismatched_lengths() {
        let score = cosine_similarity(&[1.0, 0.0], &[1.0, 0.0, 0.0]);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn test_cosine_similarity_empty() {
        let score = cosine_similarity(&[], &[]);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let score = cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn test_search_top_k_limits_results() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_embeddings().unwrap();

        // Store 5 embeddings
        for i in 0..5 {
            let mut r = make_record(3, i);
            r.vector = vec![1.0 - (i as f32 * 0.1), i as f32 * 0.1, 0.0];
            txn.put_embedding(&format!("file{}.md", i), 0, &r).unwrap();
        }

        // Ask for top 3
        let results = txn.search_embeddings(&[1.0, 0.0, 0.0], 3).unwrap();
        assert_eq!(results.len(), 3);

        // Scores should be descending
        for w in results.windows(2) {
            assert!(w[0].score >= w[1].score);
        }

        txn.commit().unwrap();
    }
}
