//! Change-scoped semantic names and their portable owner identities.
//!
//! A frame describes one author's encoding, never repository-wide aliases.
//! Resolving it proves dependency visibility before transporting a copy of the
//! operations. Capturing new names and reconstructing missing legacy context
//! remain separate responsibilities of the record and completion workflows.

use super::FileOps;
use crate::pristine::{EffectiveProjectionClosure, GraphTxnT, PristineError, ViewMembershipSet};
use crate::types::{Hash, NodeId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// A non-ROOT name in one change's encoding and the change that owns it.
///
/// Field order preserves the existing span semantic-owner wire representation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticOwner {
    /// Non-ROOT numeric name used by this change's semantic encoding.
    pub authored_id: u64,
    /// Immutable identity of the change that introduced the named items.
    pub owner: Hash,
}

/// A declaration or receiver-context failure, before caller operations mutate.
#[derive(Debug, Error)]
pub enum SemanticContextError {
    #[error("ROOT is a self-relative placeholder, not a foreign semantic name")]
    RootName,
    #[error("a semantic owner must identify a real change, not ROOT")]
    RootOwner,
    #[error("semantic owner names must be strictly ordered without duplicates")]
    NonCanonicalNames,
    #[error("distinct semantic owner names cannot alias the same change {0}")]
    AliasedOwner(Hash),
    #[error("semantic identities lack a complete portable owner mapping (missing {missing:?}, unused {unused:?})")]
    IncompleteMapping {
        missing: Vec<NodeId>,
        unused: Vec<NodeId>,
    },
    #[error("semantic dependency missing: {0}")]
    MissingDependency(Hash),
    #[error("semantic owner missing: {0}")]
    MissingOwner(Hash),
    #[error("semantic owner {0} is outside its dependency closure")]
    OutsideDependencyClosure(Hash),
    #[error("semantic owner {owner} does not round-trip through receiver id {local_id}")]
    InconsistentReceiverIdentity { owner: Hash, local_id: NodeId },
    #[error(transparent)]
    Database(#[from] PristineError),
}

/// Validated declarations for one immutable change's semantic encoding frame.
///
/// This in-memory contract validates the existing span owner declarations;
/// it does not add another stored representation or duplicate file content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticNameFrame {
    owners: Vec<SemanticOwner>,
}

impl SemanticNameFrame {
    /// Validate strictly ordered, one-to-one foreign-owner declarations.
    /// Invalid input is rejected, not normalized into a different declaration.
    ///
    /// # Examples
    ///
    /// ```
    /// use atomic_core::change::{FileOps, semantic_context::{SemanticNameFrame, SemanticOwner}};
    /// use atomic_core::{crdt::TrunkId, types::{Hash, NodeId}};
    /// let frame = SemanticNameFrame::new(vec![SemanticOwner {
    ///     authored_id: 91,
    ///     owner: Hash::of(b"creating change"),
    /// }])?;
    /// let ops = vec![FileOps::edit(TrunkId::new(NodeId::new(91), 0), "src/lib.rs".into())];
    /// frame.validate_for(&ops)?;
    /// # Ok::<(), atomic_core::change::semantic_context::SemanticContextError>(())
    /// ```
    pub fn new(owners: Vec<SemanticOwner>) -> Result<Self, SemanticContextError> {
        if owners.iter().any(|entry| entry.authored_id == 0) {
            return Err(SemanticContextError::RootName);
        }
        if owners.iter().any(|entry| entry.owner == Hash::NONE) {
            return Err(SemanticContextError::RootOwner);
        }
        if owners
            .windows(2)
            .any(|pair| pair[0].authored_id >= pair[1].authored_id)
        {
            return Err(SemanticContextError::NonCanonicalNames);
        }
        let mut hashes = BTreeSet::new();
        for entry in &owners {
            if !hashes.insert(entry.owner) {
                return Err(SemanticContextError::AliasedOwner(entry.owner));
            }
        }
        Ok(Self { owners })
    }

    pub fn owners(&self) -> &[SemanticOwner] {
        &self.owners
    }

    /// Require exactly one declaration for every foreign name, with no unused
    /// declarations that could silently expand the operation's context.
    pub fn validate_for(&self, ops: &[FileOps]) -> Result<(), SemanticContextError> {
        let used: BTreeSet<_> = ops.iter().flat_map(FileOps::referenced_node_ids).collect();
        let declared: BTreeSet<_> = self
            .owners
            .iter()
            .map(|entry| NodeId::new(entry.authored_id))
            .collect();
        let missing = used.difference(&declared).copied().collect::<Vec<_>>();
        let unused = declared.difference(&used).copied().collect::<Vec<_>>();
        if !missing.is_empty() || !unused.is_empty() {
            return Err(SemanticContextError::IncompleteMapping { missing, unused });
        }
        Ok(())
    }

    /// Transport a copy into a receiver's numeric encoding frame, using only
    /// hash owners visible through the explicit patch dependency closure.
    /// ROOT names remain self-relative; the input is unchanged on every error.
    pub fn localize<T: GraphTxnT>(
        &self,
        txn: &T,
        dependencies: &[Hash],
        ops: &[FileOps],
    ) -> Result<Vec<FileOps>, SemanticContextError> {
        self.validate_for(ops)?;
        let roots = dependencies
            .iter()
            .map(|hash| {
                txn.get_internal(hash)?
                    .ok_or(SemanticContextError::MissingDependency(*hash))
            })
            .collect::<Result<Vec<_>, SemanticContextError>>()?;
        let closure = EffectiveProjectionClosure::try_from_membership(
            txn,
            &ViewMembershipSet::from_ordered(roots),
        )?;
        let mut mapping = BTreeMap::new();
        for entry in &self.owners {
            let id = txn
                .get_internal(&entry.owner)?
                .ok_or(SemanticContextError::MissingOwner(entry.owner))?;
            if id.is_root() || txn.get_external(id)? != Some(entry.owner) {
                return Err(SemanticContextError::InconsistentReceiverIdentity {
                    owner: entry.owner,
                    local_id: id,
                });
            }
            if !closure.contains(id) {
                return Err(SemanticContextError::OutsideDependencyClosure(entry.owner));
            }
            mapping.insert(NodeId::new(entry.authored_id), id);
        }
        let mut transported = ops.to_vec();
        for file in &mut transported {
            file.remap_referenced_node_ids(|id| {
                mapping
                    .get(&id)
                    .copied()
                    .ok_or_else(|| SemanticContextError::IncompleteMapping {
                        missing: vec![id],
                        unused: vec![],
                    })
            })?;
        }
        Ok(transported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::LineOps;
    use crate::crdt::{BranchId, BranchOp, TrunkId};
    use crate::pristine::{MutTxnT, Pristine};

    fn owner(symbol: u64, hash: Hash) -> SemanticOwner {
        SemanticOwner {
            authored_id: symbol,
            owner: hash,
        }
    }
    fn operations(symbol: u64) -> Vec<FileOps> {
        let mut file = FileOps::edit(TrunkId::new(NodeId::new(symbol), 2), "f".into());
        file.add_line_op(LineOps::new(
            BranchId::new(NodeId::ROOT, 0),
            BranchOp::Insert {
                after: Some(BranchId::new(NodeId::new(symbol), 4)),
                content: vec![],
            },
        ));
        vec![file]
    }

    #[test]
    fn declarations_reject_root_unordered_duplicate_and_aliased_names() {
        let a = Hash::of(b"a");
        let b = Hash::of(b"b");
        assert!(matches!(
            SemanticNameFrame::new(vec![owner(0, a)]),
            Err(SemanticContextError::RootName)
        ));
        assert!(matches!(
            SemanticNameFrame::new(vec![owner(1, Hash::NONE)]),
            Err(SemanticContextError::RootOwner)
        ));
        for entries in [
            vec![owner(2, a), owner(1, b)],
            vec![owner(1, a), owner(1, b)],
        ] {
            assert!(matches!(
                SemanticNameFrame::new(entries),
                Err(SemanticContextError::NonCanonicalNames)
            ));
        }
        assert!(matches!(
            SemanticNameFrame::new(vec![owner(1, a), owner(2, a)]),
            Err(SemanticContextError::AliasedOwner(_))
        ));
    }

    #[test]
    fn exact_coverage_reports_missing_and_unused_names() {
        let frame = SemanticNameFrame::new(vec![owner(7, Hash::of(b"a"))]).unwrap();
        let input = operations(9);
        let before = input.clone();
        assert!(
            matches!(frame.validate_for(&input), Err(SemanticContextError::IncompleteMapping {missing, unused})
            if missing == vec![NodeId::new(9)] && unused == vec![NodeId::new(7)])
        );
        assert_eq!(input, before);
    }

    #[test]
    fn localization_is_change_scoped_transitive_and_allocation_independent() {
        let a = Hash::of(b"owner");
        let parent = Hash::of(b"explicit parent");
        let input = operations(91);
        let before = input.clone();
        let frame = SemanticNameFrame::new(vec![owner(91, a)]).unwrap();
        for padding in [0, 7] {
            let dir = tempfile::tempdir().unwrap();
            let pristine = Pristine::open(dir.path().join("db")).unwrap();
            let mut txn = pristine.write_txn().unwrap();
            for i in 0..padding {
                txn.register_change(&Hash::of(format!("padding {i}").as_bytes()))
                    .unwrap();
            }
            let id = txn.register_change(&a).unwrap();
            txn.put_change_deps(id, &[]).unwrap();
            let parent_id = txn.register_change(&parent).unwrap();
            txn.put_change_deps(parent_id, &[a]).unwrap();
            let localized = frame.localize(&txn, &[parent], &input).unwrap();
            assert_eq!(localized[0].trunk_id(), TrunkId::new(id, 2));
            assert_eq!(
                localized[0].line_ops()[0].branch_id(),
                BranchId::new(NodeId::ROOT, 0)
            );
            assert!(
                matches!(localized[0].line_ops()[0].operation(), BranchOp::Insert {after:Some(after),..} if *after == BranchId::new(id,4))
            );
            assert_eq!(input, before);
            let other = Hash::of(b"other owner");
            let other_id = txn.register_change(&other).unwrap();
            txn.put_change_deps(other_id, &[]).unwrap();
            let other_frame = SemanticNameFrame::new(vec![owner(91, other)]).unwrap();
            assert_eq!(
                other_frame.localize(&txn, &[other], &input).unwrap()[0].trunk_id(),
                TrunkId::new(other_id, 2)
            );
        }
    }

    #[test]
    fn missing_or_out_of_context_owners_never_mutate_operations() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let a = Hash::of(b"owner");
        let input = operations(91);
        let before = input.clone();
        let frame = SemanticNameFrame::new(vec![owner(91, a)]).unwrap();
        assert!(matches!(
            frame.localize(&txn, &[a], &input),
            Err(SemanticContextError::MissingDependency(_))
        ));
        assert!(matches!(
            frame.localize(&txn, &[], &input),
            Err(SemanticContextError::MissingOwner(_))
        ));
        txn.register_change(&a).unwrap();
        assert!(matches!(
            frame.localize(&txn, &[], &input),
            Err(SemanticContextError::OutsideDependencyClosure(_))
        ));
        assert_eq!(input, before);
    }

    #[test]
    fn root_only_operations_need_no_foreign_declarations() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let input = vec![FileOps::edit(TrunkId::new(NodeId::ROOT, 2), "f".into())];
        assert_eq!(
            SemanticNameFrame::new(vec![])
                .unwrap()
                .localize(&pristine.read_txn().unwrap(), &[], &input)
                .unwrap(),
            input
        );
    }

    #[test]
    fn later_owner_failure_preserves_every_file_and_unindexed_context_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let a = Hash::of(b"a");
        let b = Hash::of(b"missing b");
        let id = txn.register_change(&a).unwrap();
        let mut input = operations(91);
        input.extend(operations(92));
        let before = input.clone();
        let frame = SemanticNameFrame::new(vec![owner(91, a), owner(92, b)]).unwrap();
        assert!(matches!(
            frame.localize(&txn, &[a], &input),
            Err(SemanticContextError::Database(_))
        ));
        txn.put_change_deps(id, &[]).unwrap();
        assert!(
            matches!(frame.localize(&txn,&[a],&input),Err(SemanticContextError::MissingOwner(hash)) if hash == b)
        );
        assert_eq!(input, before);
    }

    #[test]
    fn existing_span_owner_wire_bytes_and_public_import_path_are_unchanged() {
        use super::super::span_patch::{SemanticOwner as ExistingPublicOwner, SpanPatch};
        let mut patch = SpanPatch::new(vec![], vec![]);
        let entry: ExistingPublicOwner = owner(300, Hash::from_bytes([0xa5; 32]));
        patch.semantic_owners.push(entry);
        let mut expected = b"ATSP".to_vec();
        expected.extend_from_slice(&[2, 0, 0, 1, 0xac, 2]);
        expected.extend_from_slice(&[0xa5; 32]);
        assert_eq!(patch.encode().unwrap(), expected);
        assert_eq!(SpanPatch::decode(&expected).unwrap(), patch);
    }
}
