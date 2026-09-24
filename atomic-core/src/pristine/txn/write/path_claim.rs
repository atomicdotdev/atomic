use redb::{ReadableMultimapTable, ReadableTable};

use crate::pristine::path_claim::{
    decode_path_claim_event, encode_path_claim_event, PathClaimEntry, PathClaimEvent,
    PATH_CLAIM_SCHEMA_KEY, PATH_CLAIM_SCHEMA_VERSION,
};
use crate::pristine::tables::{PATH_CLAIMS, PRISTINE_META, REV_TREE};
use crate::pristine::traits::{PathClaimMutTxnT, PathClaimTxnT};
use crate::pristine::PristineResult;
use crate::types::Inode;

use super::WriteTxn;

impl PathClaimTxnT for WriteTxn<'_> {
    fn path_claim_schema_version(&self) -> PristineResult<Option<u32>> {
        let table = self.txn.open_table(PRISTINE_META)?;
        let version = table
            .get(PATH_CLAIM_SCHEMA_KEY)?
            .map(|version| version.value());
        Ok(version)
    }

    fn get_path_claims(&self, path: &str) -> PristineResult<Vec<PathClaimEvent>> {
        let table = self.txn.open_multimap_table(PATH_CLAIMS)?;
        let mut events = Vec::new();
        for value in table.get(path)? {
            let value = value?;
            events.push(decode_path_claim_event(value.value())?);
        }
        Ok(events)
    }

    fn iter_path_claims(&self) -> PristineResult<Vec<PathClaimEntry>> {
        let table = self.txn.open_multimap_table(PATH_CLAIMS)?;
        let mut entries = Vec::new();
        for row in table.iter()? {
            let (path, values) = row?;
            let path = path.value().to_string();
            for value in values {
                let value = value?;
                entries.push(PathClaimEntry::new(
                    path.clone(),
                    decode_path_claim_event(value.value())?,
                ));
            }
        }
        Ok(entries)
    }

    fn iter_rev_tree_pairs(&self) -> PristineResult<Vec<(Inode, String)>> {
        let table = self.txn.open_table(REV_TREE)?;
        let mut pairs = Vec::new();
        for row in table.iter()? {
            let (inode, path) = row?;
            pairs.push((Inode::new(inode.value()), path.value().to_string()));
        }
        Ok(pairs)
    }
}

impl PathClaimMutTxnT for WriteTxn<'_> {
    fn del_path_claim(&mut self, path: &str, event: &PathClaimEvent) -> PristineResult<bool> {
        let encoded = encode_path_claim_event(event);
        let mut table = self.txn.open_multimap_table(PATH_CLAIMS)?;
        Ok(table.remove(path, &encoded)?)
    }

    fn put_path_claim(&mut self, path: &str, event: &PathClaimEvent) -> PristineResult<bool> {
        let encoded = encode_path_claim_event(event);
        let mut table = self.txn.open_multimap_table(PATH_CLAIMS)?;
        // redb returns true when this exact key/value pair was already present.
        // PathClaimMutTxnT exposes conventional insertion semantics instead:
        // true means newly inserted, false means the idempotent duplicate.
        Ok(!table.insert(path, &encoded)?)
    }

    fn reset_path_claim_migration(&mut self) -> PristineResult<()> {
        let paths = {
            let table = self.txn.open_multimap_table(PATH_CLAIMS)?;
            let mut paths = Vec::new();
            for row in table.iter()? {
                let (path, _) = row?;
                paths.push(path.value().to_string());
            }
            paths
        };
        {
            let mut table = self.txn.open_multimap_table(PATH_CLAIMS)?;
            for path in paths {
                table.remove_all(path.as_str())?;
            }
        }
        let mut metadata = self.txn.open_table(PRISTINE_META)?;
        metadata.remove(PATH_CLAIM_SCHEMA_KEY)?;
        Ok(())
    }

    fn complete_path_claim_migration(&mut self) -> PristineResult<()> {
        self.validate_tree_bijection()?;
        self.iter_path_claims()?;
        let mut metadata = self.txn.open_table(PRISTINE_META)?;
        metadata.insert(PATH_CLAIM_SCHEMA_KEY, PATH_CLAIM_SCHEMA_VERSION)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::pristine::{
        MutTxnT, PathClaimId, PathClaimKind, PathClaimState, Pristine, PATH_CLAIM_SCHEMA_VERSION,
    };
    use crate::types::{ChangePosition, GraphNode, NodeId, Position};
    use tempfile::tempdir;

    use super::*;

    fn event(event_change: u64, operation_index: u32, claimant: u64) -> PathClaimEvent {
        PathClaimEvent::new(
            NodeId::new(event_change),
            operation_index,
            PathClaimKind::File,
            PathClaimState::Alive,
            PathClaimId::new(
                Position::new(NodeId::new(claimant), ChangePosition::new(10)),
                GraphNode::root(),
                GraphNode::new(
                    NodeId::new(event_change),
                    ChangePosition::new(0),
                    ChangePosition::new(8),
                ),
                NodeId::new(event_change),
            ),
        )
    }

    #[test]
    fn path_claim_table_retains_every_transition_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let first = event(3, 0, 3);
        let second = event(5, 0, 5);

        assert!(txn.put_path_claim("same.txt", &first).unwrap());
        assert!(txn.put_path_claim("same.txt", &second).unwrap());
        assert!(!txn.put_path_claim("same.txt", &first).unwrap());

        let mut events = txn.get_path_claims("same.txt").unwrap();
        events.sort();
        assert_eq!(events, vec![first, second]);
        assert_eq!(txn.iter_path_claims().unwrap().len(), 2);
        txn.commit().unwrap();
    }

    #[test]
    fn migration_completion_is_atomic_with_backfill() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.reset_path_claim_migration().unwrap();
        assert_eq!(txn.path_claim_schema_version().unwrap(), None);
        txn.put_path_claim("file.txt", &event(3, 0, 3)).unwrap();
        txn.complete_path_claim_migration().unwrap();
        assert_eq!(
            txn.path_claim_schema_version().unwrap(),
            Some(PATH_CLAIM_SCHEMA_VERSION)
        );
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        assert_eq!(txn.get_path_claims("file.txt").unwrap().len(), 1);
        assert!(txn.path_claim_schema_is_complete().unwrap());
    }
}
