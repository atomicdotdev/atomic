//! Bounded native prototype: transport semantic names in an explicit inode and
//! graph-endpoint context. No stored patch bytes or dependency facts are changed.
//! Unsupported/ambiguous contexts refuse; this is not the general legacy repair.

use atomic_core::change::{Atom, AtomRef, Change, FileOps, GraphOp};
use atomic_core::crdt::tables::{decode_trunk_id, encode_branch_id, encode_leaf_id};
use atomic_core::crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId, TrunkOp};
use atomic_core::pristine::{
    CrdtTxnT, EffectiveProjectionClosure, GraphTxnT, TreeTxnT, ViewMembershipSet,
};
use atomic_core::types::{Hash, NodeId, Position};
use std::collections::{BTreeMap, BTreeSet};

type Result<T> = std::result::Result<T, String>;
type Domains = BTreeMap<NodeId, BTreeSet<NodeId>>;

pub(crate) fn localize<T: GraphTxnT + TreeTxnT + CrdtTxnT>(
    txn: &T,
    applying: NodeId,
    change: &Change,
    #[cfg(feature = "prototype-context-completion")] completion: Option<
        &crate::repository::prototype_context::Projection,
    >,
) -> Result<Vec<FileOps>> {
    if change.hashed.span.is_some() {
        return Err("inode prototype does not replace the span localizer".into());
    }
    if change
        .file_ops()
        .iter()
        .all(|op| op.referenced_node_ids().is_empty())
    {
        return Ok(change.file_ops().to_vec());
    }
    #[allow(unused_mut)]
    let mut roots = change
        .dependencies()
        .iter()
        .map(|h| {
            txn.get_internal(h)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "missing patch context".into())
        })
        .collect::<Result<Vec<_>>>()?;
    #[cfg(feature = "prototype-context-completion")]
    let explicit = {
        let consumer = txn
            .get_external(applying)
            .map_err(|e| e.to_string())?
            .ok_or("inode prototype: missing applying hash")?;
        let mut explicit = BTreeMap::new();
        if let Some(completion) = completion {
            for (symbol, provider) in completion.bindings(consumer)? {
                let id = txn
                    .get_internal(&provider)
                    .map_err(|e| e.to_string())?
                    .ok_or("inode prototype: completion provider not applied")?;
                roots.push(id);
                explicit.insert(NodeId::new(symbol), id);
            }
        }
        explicit
    };
    let context = EffectiveProjectionClosure::try_from_membership(
        txn,
        &ViewMembershipSet::from_ordered(roots),
    )
    .map_err(|e| e.to_string())?;
    let mut domains = Domains::new();
    #[cfg(feature = "prototype-context-completion")]
    for (raw, owner) in &explicit {
        domains.insert(*raw, BTreeSet::from([*owner]));
    }
    let mut bindings = Vec::new();
    for op in change.file_ops() {
        if op.referenced_node_ids().is_empty() {
            bindings.push(None);
            continue;
        }
        let mut anchors = BTreeSet::new();
        let mut candidates = BTreeSet::new();
        for hunk in change
            .hunks()
            .iter()
            .filter(|h| h.path() == Some(op.path()))
        {
            if let GraphOp::SetAttr { inode, .. } = hunk {
                anchors.insert(resolve(txn, applying, *inode)?);
            }
            for atom in hunk.atoms() {
                match atom {
                    AtomRef::Insertion(insert) | AtomRef::Atom(Atom::Insertion(insert)) => {
                        anchors.insert(resolve(txn, applying, insert.inode)?);
                        for p in insert.predecessors.iter().chain(&insert.successors) {
                            candidate(txn, p.change, &mut candidates)?;
                        }
                    }
                    AtomRef::EdgeUpdate(update) | AtomRef::Atom(Atom::EdgeUpdate(update)) => {
                        anchors.insert(resolve(txn, applying, update.inode)?);
                        for edge in &update.edges {
                            candidate(txn, edge.from.change, &mut candidates)?;
                            candidate(txn, edge.to.change, &mut candidates)?;
                            candidate(txn, edge.introduced_by, &mut candidates)?;
                        }
                    }
                }
            }
        }
        if anchors.len() != 1 {
            return Err(format!(
                "inode prototype requires one explicit graph inode for '{}'",
                op.path()
            ));
        }
        let anchor = *anchors.first().unwrap();
        if !context.contains(anchor.change) {
            return Err("inode prototype: file owner is outside declared patch context".into());
        }
        let inode = txn
            .position_inode(anchor)
            .map_err(|e| e.to_string())?
            .ok_or("inode prototype: canonical graph inode is not indexed")?;
        let key = txn
            .get_crdt_inode_trunk(inode.get())
            .map_err(|e| e.to_string())?
            .ok_or("inode prototype: graph inode has no semantic trunk")?;
        let trunk = decode_trunk_id(&key);
        if trunk.change_id() != anchor.change || trunk.file_idx() != op.trunk_id().file_idx() {
            return Err("inode prototype: canonical inode/trunk identity disagreement".into());
        }
        if op.trunk_id().change_id().is_root() {
            return Err(
                "inode prototype: existing-file operations need an external trunk name".into(),
            );
        }
        constrain(
            &mut domains,
            op.trunk_id().change_id(),
            BTreeSet::from([trunk.change_id()]),
        )?;
        candidates.insert(trunk.change_id());
        #[cfg(feature = "prototype-context-completion")]
        candidates.extend(explicit.values());
        candidates.retain(|id| context.contains(id));
        match op.trunk_op() {
            Some(TrunkOp::Delete { trunk: raw })
            | Some(TrunkOp::Move { trunk: raw, .. })
            | Some(TrunkOp::Undelete { trunk: raw })
            | Some(TrunkOp::SetMode { trunk: raw, .. })
            | Some(TrunkOp::SetKind { trunk: raw, .. })
                if *raw != op.trunk_id() =>
            {
                return Err("inode prototype: conflicting trunk operands".into());
            }
            Some(TrunkOp::Create { .. } | TrunkOp::ProjectedSpanEdit { .. }) => {
                return Err("inode prototype: unsupported external creation/projection".into());
            }
            _ => {}
        }
        for line in op.line_ops() {
            branch_domain(txn, trunk, line.branch_id(), &candidates, &mut domains)?;
            match line.operation() {
                BranchOp::Insert { after, content } => {
                    if let Some(id) = after {
                        branch_domain(txn, trunk, *id, &candidates, &mut domains)?;
                    }
                    leaves(txn, trunk, content, &candidates, &mut domains)?;
                }
                BranchOp::Delete { branch, content } => {
                    branch_domain(txn, trunk, *branch, &candidates, &mut domains)?;
                    leaves(txn, trunk, content, &candidates, &mut domains)?;
                }
                BranchOp::Modify {
                    branch,
                    old_content,
                    new_content,
                } => {
                    branch_domain(txn, trunk, *branch, &candidates, &mut domains)?;
                    leaves(txn, trunk, old_content, &candidates, &mut domains)?;
                    leaves(txn, trunk, new_content, &candidates, &mut domains)?;
                }
                BranchOp::Restore { branch } => {
                    branch_domain(txn, trunk, *branch, &candidates, &mut domains)?
                }
                BranchOp::Reparent { branch, new_after } => {
                    branch_domain(txn, trunk, *branch, &candidates, &mut domains)?;
                    if let Some(id) = new_after {
                        branch_domain(txn, trunk, *id, &candidates, &mut domains)?;
                    }
                }
            }
        }
        bindings.push(Some(trunk));
    }
    // Resolve only a unique injective substitution; never choose by numeric
    // equality, iteration order, path history or a guessed provider.
    loop {
        let fixed: Vec<_> = domains
            .values()
            .filter(|d| d.len() == 1)
            .map(|d| *d.first().unwrap())
            .collect();
        let used: BTreeSet<_> = fixed.iter().copied().collect();
        if used.len() != fixed.len() {
            return Err("inode prototype: aliased foreign owner names".into());
        }
        let mut progress = false;
        for domain in domains.values_mut().filter(|d| d.len() > 1) {
            let old = domain.len();
            domain.retain(|id| !used.contains(id));
            if domain.is_empty() {
                return Err("inode prototype: contradictory name substitution".into());
            }
            progress |= old != domain.len();
        }
        if !progress {
            break;
        }
    }
    if domains.values().any(|d| d.len() != 1) {
        return Err("inode prototype: ambiguous graph-context owner".into());
    }
    let mut bound = change.file_ops().to_vec();
    for (op, binding) in bound.iter_mut().zip(bindings) {
        op.remap_referenced_node_ids(|raw| {
            domains
                .get(&raw)
                .and_then(|d| d.first())
                .copied()
                .ok_or_else(|| "inode prototype: unbound owner".to_string())
        })?;
        if let Some(trunk) = binding {
            if op.trunk_id() != trunk {
                return Err("inode prototype: transport did not preserve inode binding".into());
            }
            validate_tokens(txn, applying, op)?;
        }
    }
    Ok(bound)
}

fn resolve<T: GraphTxnT>(
    txn: &T,
    applying: NodeId,
    p: Position<Option<Hash>>,
) -> Result<Position<NodeId>> {
    let id = match p.change {
        None => applying,
        Some(hash) => txn
            .get_internal(&hash)
            .map_err(|e| e.to_string())?
            .ok_or("missing graph owner")?,
    };
    Ok(Position::new(id, p.pos))
}

fn candidate<T: GraphTxnT>(txn: &T, hash: Option<Hash>, out: &mut BTreeSet<NodeId>) -> Result<()> {
    if let Some(hash) = hash.filter(|h| *h != Hash::NONE) {
        out.insert(
            txn.get_internal(&hash)
                .map_err(|e| e.to_string())?
                .ok_or("missing graph context owner")?,
        );
    }
    Ok(())
}

fn constrain(domains: &mut Domains, raw: NodeId, candidates: BTreeSet<NodeId>) -> Result<()> {
    if raw.is_root() {
        return Ok(());
    }
    let domain = domains.entry(raw).or_insert_with(|| candidates.clone());
    *domain = domain.intersection(&candidates).copied().collect();
    if domain.is_empty() {
        return Err(format!(
            "inode prototype: no typed owner in explicit graph context for symbol {}",
            raw.get()
        ));
    }
    Ok(())
}

fn branch_domain<T: CrdtTxnT>(
    txn: &T,
    trunk: TrunkId,
    raw: BranchId,
    candidates: &BTreeSet<NodeId>,
    domains: &mut Domains,
) -> Result<()> {
    if raw.change_id().is_root() {
        return Ok(());
    }
    let mut found = BTreeSet::new();
    for owner in candidates {
        if txn
            .get_crdt_branch(&encode_branch_id(&BranchId::new(*owner, raw.branch_idx())))
            .map_err(|e| e.to_string())?
            .is_some_and(|row| row.trunk_id == trunk)
        {
            found.insert(*owner);
        }
    }
    constrain(domains, raw.change_id(), found)
}

fn leaves<T: CrdtTxnT>(
    txn: &T,
    trunk: TrunkId,
    tokens: &[LeafOp],
    candidates: &BTreeSet<NodeId>,
    domains: &mut Domains,
) -> Result<()> {
    for token in tokens {
        let leaf = match token {
            LeafOp::Insert { after, .. } => *after,
            LeafOp::Delete { leaf } | LeafOp::Replace { leaf, .. } | LeafOp::Restore { leaf } => {
                Some(*leaf)
            }
        };
        if let Some(raw) = leaf.filter(|id| !id.change_id().is_root()) {
            let mut found = BTreeSet::new();
            for owner in candidates {
                if let Some(leaf) = txn
                    .get_crdt_leaf(&encode_leaf_id(&LeafId::new(*owner, raw.leaf_idx())))
                    .map_err(|e| e.to_string())?
                {
                    if txn
                        .get_crdt_branch(&encode_branch_id(&leaf.branch_id))
                        .map_err(|e| e.to_string())?
                        .is_some_and(|row| row.trunk_id == trunk)
                    {
                        found.insert(*owner);
                    }
                }
            }
            constrain(domains, raw.change_id(), found)?;
        }
    }
    Ok(())
}

fn validate_tokens<T: CrdtTxnT>(txn: &T, applying: NodeId, op: &FileOps) -> Result<()> {
    for line in op.line_ops() {
        let raw = line.branch_id();
        let branch = if raw.change_id().is_root() {
            BranchId::new(applying, raw.branch_idx())
        } else {
            raw
        };
        let operand = match line.operation() {
            BranchOp::Delete { branch, .. }
            | BranchOp::Modify { branch, .. }
            | BranchOp::Restore { branch }
            | BranchOp::Reparent { branch, .. } => Some(*branch),
            BranchOp::Insert { .. } => None,
        };
        if let Some(raw) = operand {
            let target = if raw.change_id().is_root() {
                BranchId::new(applying, raw.branch_idx())
            } else {
                raw
            };
            if target != branch {
                return Err("inode prototype: contradictory branch operands".into());
            }
        }
        let groups: Vec<&[LeafOp]> = match line.operation() {
            BranchOp::Insert { content, .. } | BranchOp::Delete { content, .. } => vec![content],
            BranchOp::Modify {
                old_content,
                new_content,
                ..
            } => vec![old_content, new_content],
            _ => vec![],
        };
        for token in groups.into_iter().flatten() {
            let leaf = match token {
                LeafOp::Insert { after, .. } => *after,
                LeafOp::Delete { leaf }
                | LeafOp::Replace { leaf, .. }
                | LeafOp::Restore { leaf } => Some(*leaf),
            };
            if let Some(leaf) = leaf.filter(|id| !id.change_id().is_root()) {
                let row = txn
                    .get_crdt_leaf(&encode_leaf_id(&leaf))
                    .map_err(|e| e.to_string())?
                    .ok_or("missing token context")?;
                if row.branch_id != branch {
                    return Err("inode prototype: token belongs to another line".into());
                }
            }
        }
    }
    Ok(())
}
