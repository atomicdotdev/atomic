//! Immutable verification witnesses and operation-local decoded-change reuse.
use super::{Change, ChangeError, ChangeStore};
use crate::types::Hash;
use std::io::Cursor;
use std::sync::{Arc, RwLock};

/// A change bound to an identity by checked decoding or canonical serialization.
///
/// No mutable access is exposed: modifying a clone of the decoded change cannot
/// change the value covered by this witness. This is not a persistent trust bit.
#[derive(Clone, Debug)]
pub struct VerifiedChange {
    hash: Hash,
    change: Arc<Change>,
    source_digest: Option<Hash>,
}

impl VerifiedChange {
    /// Verify the original V3 bytes, section manifest and decoded semantics.
    /// The original encoded identity is authoritative; no recompression is needed.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ChangeError> {
        let mut reader = Cursor::new(bytes);
        let (change, hash) = Change::deserialize(&mut reader)?;
        if reader.position() != bytes.len() as u64 {
            return Err(ChangeError::Invalid(
                "trailing bytes after verified change".into(),
            ));
        }
        Ok(Self {
            hash,
            change: Arc::new(change),
            source_digest: Some(Hash::of(bytes)),
        })
    }

    /// Conservative fallback for stores that only provide decoded values.
    pub fn from_change(change: Change) -> Result<Self, ChangeError> {
        let hash = change.hash()?;
        Ok(Self {
            hash,
            change: Arc::new(change),
            source_digest: None,
        })
    }

    pub fn hash(&self) -> Hash {
        self.hash
    }
    pub fn change(&self) -> &Change {
        &self.change
    }

    /// Check the requested identity even when a store returned a valid witness
    /// for some other change.
    pub fn check_hash(&self, expected: &Hash) -> Result<(), ChangeError> {
        if self.hash != *expected {
            return Err(ChangeError::Invalid(format!(
                "change hash mismatch: expected {expected:?}, computed {:?}",
                self.hash
            )));
        }
        Ok(())
    }

    /// Whether these bytes exactly match the previously verified source,
    /// including unhashed notes/signatures. A change in any byte requires a
    /// new checked decode, not a timestamp-based cache hit.
    pub fn matches_source_bytes(&self, bytes: &[u8]) -> bool {
        self.source_digest
            .is_some_and(|digest| digest == Hash::of(bytes))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VerifiedLoadError<E: std::error::Error + Send + Sync + 'static> {
    #[error(transparent)]
    Store(E),
    #[error(transparent)]
    Verification(#[from] ChangeError),
}

/// A bounded, operation-local cache of verified decoded values, never indexes
/// or causal proofs. Every hit asks the underlying store to revalidate its source.
///
/// `max_entries` is an entry-count bound, not a decoded-byte memory bound. Once
/// full, further changes are verified without admission. Drop the adapter when
/// the operation finishes; do not install it as a global repository cache.
pub struct VerifiedChangeCache<'a, S: ChangeStore> {
    store: &'a S,
    entries: RwLock<std::collections::HashMap<Hash, VerifiedChange>>,
    max_entries: usize,
}

impl<'a, S: ChangeStore> VerifiedChangeCache<'a, S> {
    pub fn new(store: &'a S, max_entries: usize) -> Self {
        Self {
            store,
            entries: RwLock::new(Default::default()),
            max_entries,
        }
    }
}

impl<S: ChangeStore> ChangeStore for VerifiedChangeCache<'_, S> {
    type Error = S::Error;

    fn get_change(&self, hash: &Hash) -> Result<Change, Self::Error> {
        self.store.get_change(hash)
    }

    fn get_verified_change(
        &self,
        hash: &Hash,
    ) -> Result<VerifiedChange, VerifiedLoadError<Self::Error>> {
        let cached = self
            .entries
            .read()
            .ok()
            .and_then(|entries| entries.get(hash).cloned());
        let verified = match cached {
            Some(cached) => self.store.revalidate_verified_change(hash, &cached)?,
            None => self.store.get_verified_change(hash)?,
        };
        verified.check_hash(hash)?;
        if let Ok(mut entries) = self.entries.write() {
            if entries.contains_key(hash) || entries.len() < self.max_entries {
                entries.insert(*hash, verified.clone());
            }
        }
        Ok(verified)
    }

    fn get_contents<F>(
        &self,
        hash_fn: F,
        node: crate::types::GraphNode<crate::types::NodeId>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error>
    where
        F: Fn(crate::types::NodeId) -> Option<Hash>,
    {
        self.store.get_contents(hash_fn, node, buf)
    }

    fn get_contents_ext(
        &self,
        node: crate::types::GraphNode<Option<Hash>>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error> {
        self.store.get_contents_ext(node, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::ChangeHeader;

    #[test]
    fn verified_bytes_bind_identity_and_do_not_allow_mutation() {
        let change = Change::new(
            ChangeHeader::new("original"),
            vec![],
            b"hello".to_vec(),
            vec![],
        );
        let mut bytes = Vec::new();
        let hash = change.serialize(&mut bytes).unwrap();
        let verified = VerifiedChange::from_bytes(&bytes).unwrap();
        verified.check_hash(&hash).unwrap();
        assert!(verified.matches_source_bytes(&bytes));
        let mut cloned = verified.change().clone();
        cloned.contents[0] = b'X';
        assert_eq!(verified.change().contents, b"hello");
        assert!(verified.check_hash(&Hash::of(b"wrong identity")).is_err());
        bytes.push(1);
        assert!(!verified.matches_source_bytes(&bytes));
        assert!(VerifiedChange::from_bytes(&bytes).is_err());
    }

    #[test]
    fn changed_encoded_bytes_and_truncated_sources_are_rejected() {
        let change = Change::new(ChangeHeader::new("root"), vec![], b"hello".to_vec(), vec![]);
        let mut bytes = Vec::new();
        change.serialize(&mut bytes).unwrap();
        for index in [0, bytes.len() / 2, bytes.len() - 1] {
            let mut altered = bytes.clone();
            altered[index] ^= 1;
            assert!(VerifiedChange::from_bytes(&altered).is_err());
        }
        assert!(VerifiedChange::from_bytes(&bytes[..bytes.len() - 1]).is_err());
        assert!(!VerifiedChange::from_change(change)
            .unwrap()
            .matches_source_bytes(&bytes));
    }

    #[test]
    fn verified_identity_uses_original_encoding_not_a_rechunked_rewrite() {
        use crate::change::format_v3::types::envelope::encode_change_header_v2;
        use crate::change::format_v3::{ChangeWriter, FileHeader, HashDedupTable, WriterOptions};
        let change = Change::new(
            ChangeHeader::new("two original chunks"),
            vec![],
            b"hello".to_vec(),
            vec![],
        );
        let table = HashDedupTable::new([0; 32]);
        let header = FileHeader::builder()
            .hash_table_entries(table.len() as u32)
            .contents_chunks(2)
            .build();
        let mut bytes = Vec::new();
        let mut writer = ChangeWriter::new(&mut bytes, WriterOptions::default());
        writer.write_file_header(&header).unwrap();
        writer.write_hash_table(&table).unwrap();
        writer
            .write_change_header_v2_payload(
                &encode_change_header_v2(&change.hashed, &table).unwrap(),
            )
            .unwrap();
        writer.write_dependencies(&[]).unwrap();
        writer.write_content_chunk(0, b"he").unwrap();
        writer.write_content_chunk(1, b"llo").unwrap();
        let original = Hash::from_bytes(writer.finalize().unwrap().content_hash);
        let verified = VerifiedChange::from_bytes(&bytes).unwrap();
        assert_eq!(verified.change().contents, b"hello");
        verified.check_hash(&original).unwrap();
        assert_ne!(verified.hash(), verified.change().hash().unwrap());
    }

    #[test]
    fn custom_store_cache_hits_still_reload_and_verify_and_admission_is_bounded() {
        use crate::change::MemoryChangeStore;
        let store = MemoryChangeStore::new();
        let first = Change::new(ChangeHeader::new("first"), vec![], b"one".to_vec(), vec![]);
        let hash = store.insert_change(first.clone()).unwrap();
        let other = store
            .insert_change(Change::new(
                ChangeHeader::new("other"),
                vec![],
                b"two".to_vec(),
                vec![],
            ))
            .unwrap();
        let cache = VerifiedChangeCache::new(&store, 1);
        cache.get_verified_change(&hash).unwrap();
        cache.get_verified_change(&other).unwrap();
        assert_eq!(cache.entries.read().unwrap().len(), 1);
        let mut tampered = first.clone();
        tampered.contents[0] ^= 1;
        store.insert(hash, tampered);
        assert!(cache.get_verified_change(&hash).is_err());
        store.insert(hash, first);
        cache.get_verified_change(&hash).unwrap();
        store.remove(&hash);
        assert!(cache.get_verified_change(&hash).is_err());
        let disabled = VerifiedChangeCache::new(&store, 0);
        disabled.get_verified_change(&other).unwrap();
        assert!(disabled.entries.read().unwrap().is_empty());
    }
}
