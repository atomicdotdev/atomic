//! Exact Git interpretation closures stored in a shared content-addressed trie.
//! Unsorted or duplicate-containing vectors retain the legacy exact encoding.

use super::{PristineError, PristineResult};
use crate::types::Hash;

const MAGIC: &[u8; 8] = b"AGCLT001";
const DOMAIN: &[u8] = b"atomic:git-interpretation-closure:radix:v1\0";
const EMPTY: [u8; 32] = [0; 32];

fn invalid(message: impl Into<String>) -> PristineError {
    PristineError::Serialization {
        message: message.into(),
    }
}

fn node_id(node: &[u8; 64]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);
    hasher.update(node);
    *hasher.finalize().as_bytes()
}

pub(crate) fn encode(
    closure: &[Hash],
    mut intern: impl FnMut(&[u8; 32], &[u8; 64]) -> PristineResult<()>,
) -> PristineResult<Vec<u8>> {
    let count =
        u32::try_from(closure.len()).map_err(|_| invalid("Git closure exceeds u32 length"))?;
    if !closure
        .windows(2)
        .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
    {
        let mut bytes = Vec::with_capacity(4 + closure.len() * 32);
        bytes.extend_from_slice(&count.to_le_bytes());
        for hash in closure {
            bytes.extend_from_slice(hash.as_bytes());
        }
        return Ok(bytes);
    }
    let tree = encode_tree(closure, &mut intern)?;
    // 44 bytes is never a valid legacy count + hashes row (4 mod 32).
    let mut root = Vec::with_capacity(44);
    root.extend_from_slice(MAGIC);
    root.extend_from_slice(&count.to_le_bytes());
    root.extend_from_slice(&tree);
    Ok(root)
}

pub(crate) fn is_shared(bytes: &[u8]) -> bool {
    bytes.len() == 44 && bytes.starts_with(MAGIC)
}

fn encode_tree(
    hashes: &[Hash],
    intern: &mut impl FnMut(&[u8; 32], &[u8; 64]) -> PristineResult<()>,
) -> PristineResult<[u8; 32]> {
    if hashes.is_empty() {
        return Ok(EMPTY);
    }
    let mut node = [0u8; 64];
    if hashes.len() == 1 {
        node[32..].copy_from_slice(hashes[0].as_bytes());
    } else {
        let first = hashes.first().unwrap().as_bytes();
        let last = hashes.last().unwrap().as_bytes();
        let byte = first.iter().zip(last).position(|(a, b)| a != b).unwrap();
        let mask = 0x80u8 >> (first[byte] ^ last[byte]).leading_zeros();
        let split = hashes.partition_point(|hash| hash.as_bytes()[byte] & mask == 0);
        let left = encode_tree(&hashes[..split], intern)?;
        let right = encode_tree(&hashes[split..], intern)?;
        node[..32].copy_from_slice(&left);
        node[32..].copy_from_slice(&right);
    }
    let id = node_id(&node);
    if id == EMPTY {
        return Err(invalid("Git closure node collides with the empty tree"));
    }
    intern(&id, &node)?;
    Ok(id)
}

pub(crate) fn decode(
    bytes: &[u8],
    mut lookup: impl FnMut(&[u8; 32]) -> PristineResult<Option<[u8; 64]>>,
) -> PristineResult<Vec<Hash>> {
    if !bytes.starts_with(MAGIC) || bytes.len() % 32 == 4 {
        return decode_legacy(bytes);
    }
    if bytes.len() != 44 {
        return Err(invalid(
            "Git shared closure root must contain exactly 44 bytes",
        ));
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let root: [u8; 32] = bytes[12..].try_into().unwrap();
    // Never allocate from an untrusted count before checking the tree.
    let mut closure = Vec::new();
    if count == 0 && root == EMPTY {
        return Ok(closure);
    }
    let mut stack = vec![(root, 0u16)];
    let mut visited = 0u64;
    while let Some((id, depth)) = stack.pop() {
        visited += 1;
        if id == EMPTY || depth > 256 || visited > 2 * u64::from(count) {
            return Err(invalid(
                "Git closure tree has invalid structure or declared length",
            ));
        }
        let node = lookup(&id)?.ok_or_else(|| invalid("Git closure tree node is missing"))?;
        if node_id(&node) != id {
            return Err(invalid(
                "Git closure tree node failed content-hash verification",
            ));
        }
        let left: [u8; 32] = node[..32].try_into().unwrap();
        let right: [u8; 32] = node[32..].try_into().unwrap();
        if left == EMPTY {
            if closure.len() >= count as usize
                || closure
                    .last()
                    .is_some_and(|hash: &Hash| hash.as_bytes() >= &right)
            {
                return Err(invalid(
                    "Git closure leaves are not strictly ordered or exceed declared length",
                ));
            }
            closure.push(Hash::from_bytes(right));
        } else {
            if right == EMPTY {
                return Err(invalid("Git closure branch has an empty child"));
            }
            stack.push((right, depth + 1));
            stack.push((left, depth + 1));
        }
    }
    if closure.len() != count as usize {
        return Err(invalid(
            "Git closure tree length differs from its declared length",
        ));
    }
    Ok(closure)
}

fn decode_legacy(bytes: &[u8]) -> PristineResult<Vec<Hash>> {
    let count_bytes = bytes
        .get(..4)
        .ok_or_else(|| invalid("Git commit closure is truncated"))?;
    let count = u32::from_le_bytes(count_bytes.try_into().unwrap()) as u64;
    if bytes.len() as u64 != 4 + count * 32 {
        return Err(invalid(format!(
            "Git commit closure claims {count} hashes but holds {} bytes",
            bytes.len()
        )));
    }
    Ok(bytes[4..]
        .as_chunks::<32>()
        .0
        .iter()
        .map(|hash| Hash::from_bytes(*hash))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::{
        CapabilityMutTxnT, GitCommitClosureMutTxnT, GitCommitClosureTxnT, MutTxnT, Pristine,
        GIT_CLOSURE_NODES, GIT_CLOSURE_SHARING_CAPABILITY, GIT_COMMIT_CLOSURES,
    };
    use redb::{ReadableTable, ReadableTableMetadata};
    use std::collections::HashMap;

    fn hash(n: u64) -> Hash {
        Hash::of(&n.to_le_bytes())
    }
    fn legacy(hashes: &[Hash]) -> Vec<u8> {
        let mut bytes = (hashes.len() as u32).to_le_bytes().to_vec();
        for hash in hashes {
            bytes.extend_from_slice(hash.as_bytes());
        }
        bytes
    }

    #[test]
    fn exact_order_duplicates_empty_and_divergent_sets_roundtrip() {
        let mut nodes = HashMap::new();
        for hashes in [
            vec![],
            vec![hash(1)],
            vec![hash(1), hash(2)],
            vec![hash(1), hash(3)],
            vec![hash(3), hash(1), hash(1)],
        ] {
            let root = encode(&hashes, |id, node| {
                nodes.insert(*id, *node);
                Ok(())
            })
            .unwrap();
            let sorted_unique = hashes.windows(2).all(|p| p[0].as_bytes() < p[1].as_bytes());
            assert_eq!(is_shared(&root), sorted_unique);
            if !sorted_unique {
                assert_eq!(root, legacy(&hashes));
            }
            assert_eq!(
                decode(&root, |id| Ok(nodes.get(id).copied())).unwrap(),
                hashes
            );
            assert_eq!(
                decode(&legacy(&hashes), |_| panic!("legacy requested a node")).unwrap(),
                hashes
            );
        }
        assert!(!nodes.is_empty());
    }

    #[quickcheck_macros::quickcheck]
    fn shared_and_legacy_encodings_preserve_arbitrary_vectors(values: Vec<u64>) -> bool {
        let hashes: Vec<_> = values.into_iter().take(256).map(hash).collect();
        let mut nodes = HashMap::new();
        let root = encode(&hashes, |id, node| {
            nodes.insert(*id, *node);
            Ok(())
        })
        .unwrap();
        if decode(&root, |id| Ok(nodes.get(id).copied())).unwrap() != hashes
            || decode(&legacy(&hashes), |_| Ok(None)).unwrap() != hashes
        {
            return false;
        }
        let mut sorted = hashes;
        sorted.sort();
        sorted.dedup();
        let root = encode(&sorted, |id, node| {
            nodes.insert(*id, *node);
            Ok(())
        })
        .unwrap();
        is_shared(&root) && decode(&root, |id| Ok(nodes.get(id).copied())).unwrap() == sorted
    }

    #[test]
    fn invalid_lengths_missing_and_modified_nodes_fail_closed() {
        let mut nodes = HashMap::new();
        let mut hashes = vec![hash(1), hash(2)];
        hashes.sort();
        let root = encode(&hashes, |id, node| {
            nodes.insert(*id, *node);
            Ok(())
        })
        .unwrap();
        assert!(decode(&root, |_| Ok(None)).is_err());
        assert!(decode(&root, |_| Ok(Some([0; 64]))).is_err());
        assert!(decode(&root, |_| Err(invalid("lookup failed"))).is_err());
        for count in [0, 1, 3, u32::MAX] {
            let mut bad = root.clone();
            bad[8..12].copy_from_slice(&count.to_le_bytes());
            assert!(decode(&bad, |id| Ok(nodes.get(id).copied())).is_err());
        }
        for bad in [
            vec![],
            vec![0; 3],
            vec![1, 0, 0, 0],
            root[..43].to_vec(),
            [root.clone(), vec![0]].concat(),
            u32::MAX.to_le_bytes().to_vec(),
        ] {
            assert!(decode(&bad, |id| Ok(nodes.get(id).copied())).is_err());
        }
    }

    #[test]
    fn content_addressed_but_unordered_duplicate_or_empty_branches_are_rejected() {
        let mut nodes = HashMap::new();
        let mut hashes = vec![hash(1), hash(2)];
        hashes.sort();
        let roots: Vec<_> = hashes
            .iter()
            .map(|hash| {
                encode(&[*hash], |id, node| {
                    nodes.insert(*id, *node);
                    Ok(())
                })
                .unwrap()
            })
            .collect();
        let left: [u8; 32] = roots[0][12..].try_into().unwrap();
        let right: [u8; 32] = roots[1][12..].try_into().unwrap();
        for (a, b) in [(right, left), (left, left), (left, EMPTY)] {
            let mut node = [0u8; 64];
            node[..32].copy_from_slice(&a);
            node[32..].copy_from_slice(&b);
            let id = node_id(&node);
            nodes.insert(id, node);
            let mut root = MAGIC.to_vec();
            root.extend_from_slice(&2u32.to_le_bytes());
            root.extend_from_slice(&id);
            assert!(decode(&root, |id| Ok(nodes.get(id).copied())).is_err());
        }
    }

    #[test]
    fn sorted_history_shares_subtrees_across_middle_insertions_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pristine");
        let pristine = Pristine::open(&path).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let hashes: Vec<_> = (0..256).map(hash).collect();
        let histories: Vec<Vec<Hash>> = (1..=hashes.len())
            .map(|n| {
                let mut h = hashes[..n].to_vec();
                h.sort();
                h
            })
            .collect();
        for (index, history) in histories.iter().enumerate() {
            let sha = format!("{:040x}", index + 1);
            txn.put_git_commit_closure(&sha, history).unwrap();
            assert_eq!(txn.get_git_commit_closure(&sha).unwrap().unwrap(), *history);
        }
        assert!(
            txn.txn
                .open_table(GIT_CLOSURE_NODES)
                .unwrap()
                .len()
                .unwrap()
                < 256 * 16
        );
        assert_eq!(
            txn.txn
                .open_table(GIT_COMMIT_CLOSURES)
                .unwrap()
                .len()
                .unwrap(),
            256
        );
        txn.commit().unwrap();
        assert!(pristine
            .required_repository_capabilities()
            .unwrap()
            .iter()
            .any(|c| c.id == GIT_CLOSURE_SHARING_CAPABILITY.id()));
        drop(pristine);
        let pristine = Pristine::open_readonly(&path).unwrap();
        let txn = pristine.read_txn().unwrap();
        for (index, history) in histories.iter().enumerate() {
            assert_eq!(
                txn.get_git_commit_closure(&format!("{:040x}", index + 1))
                    .unwrap()
                    .unwrap(),
                *history
            );
        }
        assert!(!txn.has_git_commit_closure("absent").unwrap());
    }

    #[test]
    fn legacy_rows_stay_readable_and_identical_writes_are_noops() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let hashes = vec![hash(2), hash(1), hash(1)];
        let bytes = legacy(&hashes);
        let mut txn = pristine.write_txn().unwrap();
        txn.txn
            .open_table(GIT_COMMIT_CLOSURES)
            .unwrap()
            .insert("legacy", bytes.as_slice())
            .unwrap();
        txn.put_git_commit_closure("legacy", &hashes).unwrap();
        assert_eq!(
            txn.get_git_commit_closure("legacy").unwrap(),
            Some(hashes.clone())
        );
        assert!(txn.put_git_commit_closure("legacy", &[hash(3)]).is_err());
        assert_eq!(
            txn.txn
                .open_table(GIT_COMMIT_CLOSURES)
                .unwrap()
                .get("legacy")
                .unwrap()
                .unwrap()
                .value(),
            bytes
        );
        txn.commit().unwrap();
        assert!(!pristine
            .required_repository_capabilities()
            .unwrap()
            .iter()
            .any(|c| c.id == GIT_CLOSURE_SHARING_CAPABILITY.id()));
        assert_eq!(
            pristine
                .read_txn()
                .unwrap()
                .get_git_commit_closure("legacy")
                .unwrap(),
            Some(hashes)
        );
    }

    #[test]
    fn shared_idempotence_conflict_empty_and_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.put_git_commit_closure("empty", &[]).unwrap();
        txn.put_git_commit_closure("one", &[hash(1)]).unwrap();
        txn.put_git_commit_closure("one", &[hash(1)]).unwrap();
        assert!(txn.put_git_commit_closure("one", &[hash(2)]).is_err());
        assert_eq!(
            txn.get_git_commit_closure("one").unwrap(),
            Some(vec![hash(1)])
        );
        assert_eq!(
            txn.txn
                .open_table(GIT_CLOSURE_NODES)
                .unwrap()
                .len()
                .unwrap(),
            1
        );
        txn.abort().unwrap();
        assert!(!pristine
            .required_repository_capabilities()
            .unwrap()
            .iter()
            .any(|c| c.id == GIT_CLOSURE_SHARING_CAPABILITY.id()));
        assert!(!pristine
            .read_txn()
            .unwrap()
            .has_git_commit_closure("one")
            .unwrap());
        let mut txn = pristine.write_txn().unwrap();
        txn.put_git_commit_closure("empty", &[]).unwrap();
        txn.commit().unwrap();
        assert_eq!(
            pristine
                .read_txn()
                .unwrap()
                .get_git_commit_closure("empty")
                .unwrap(),
            Some(vec![])
        );
    }

    #[test]
    fn corrupt_persisted_node_is_rejected_by_both_readers() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.put_git_commit_closure("one", &[hash(1)]).unwrap();
        let root = txn
            .txn
            .open_table(GIT_COMMIT_CLOSURES)
            .unwrap()
            .get("one")
            .unwrap()
            .unwrap()
            .value()
            .to_vec();
        let id: [u8; 32] = root[12..].try_into().unwrap();
        txn.txn
            .open_table(GIT_CLOSURE_NODES)
            .unwrap()
            .insert(&id, &[0u8; 64])
            .unwrap();
        assert!(txn.get_git_commit_closure("one").is_err());
        txn.commit().unwrap();
        assert!(pristine
            .read_txn()
            .unwrap()
            .get_git_commit_closure("one")
            .is_err());
    }

    #[test]
    fn unsupported_sharing_capability_version_is_fenced_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pristine");
        let pristine = Pristine::open(&path).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.put_required_capability_exact(GIT_CLOSURE_SHARING_CAPABILITY.id(), 2)
            .unwrap();
        txn.commit().unwrap();
        drop(pristine);
        assert!(Pristine::open_readonly(&path).is_err());
        assert!(Pristine::open_existing(&path).is_err());
    }
}
