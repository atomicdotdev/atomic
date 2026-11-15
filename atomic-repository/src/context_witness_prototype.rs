//! Read-only prototype: reconstruct a branch-owner context witness from a
//! selected, hash-verified patch set. This neither applies a repair nor changes
//! dependencies. No chronology or receiving-repository numeric IDs select an owner.
//!
//! cargo run -p atomic-repository --example reconstruct_semantic_context -- \
//!   <repo> <consumer-hash> <foreign-symbol> <new-output-json>
use crate::Repository;
use atomic_core::change::{Atom, Change, GraphOp, VerifiedChange};
use atomic_core::crdt::{BranchOp, LeafOp, TrunkOp};
use atomic_core::pristine::{GraphTxnT, ViewTxnT};
use atomic_core::types::{Base32, Hash};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub(crate) type Objects = BTreeMap<Hash, Change>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct File {
    owner: Hash,
    position: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Reference {
    file: File,
    branch: u32,
    role: String,
}

fn anchor(p: atomic_core::types::Position<Option<Hash>>, own: Hash) -> File {
    File {
        owner: p.change.unwrap_or(own),
        position: p.pos.get(),
    }
}

fn file(change: &Change, own: Hash, path: &str) -> Result<File> {
    let mut files = BTreeSet::new();
    for hunk in change.hunks().iter().filter(|h| h.path() == Some(path)) {
        match hunk {
            GraphOp::FileAdd { add_inode, .. } => {
                files.insert(File {
                    owner: own,
                    position: add_inode.start.get(),
                });
            }
            GraphOp::Edit { change, .. } => {
                files.insert(anchor(
                    match change {
                        Atom::Insertion(i) => i.inode,
                        Atom::EdgeUpdate(e) => e.inode,
                    },
                    own,
                ));
            }
            GraphOp::Replacement {
                change,
                replacement,
                ..
            } => {
                files.insert(anchor(change.inode, own));
                files.insert(anchor(replacement.inode, own));
            }
            GraphOp::FileMove { del, add, .. } => {
                files.insert(anchor(del.inode, own));
                files.insert(anchor(add.inode, own));
            }
            GraphOp::FileDel { del, contents, .. } => {
                files.insert(anchor(contents.as_ref().unwrap_or(del).inode, own));
            }
            GraphOp::FileUndel {
                undel, contents, ..
            } => {
                files.insert(anchor(contents.as_ref().unwrap_or(undel).inode, own));
            }
            GraphOp::SetAttr { inode, .. } => {
                files.insert(anchor(*inode, own));
            }
            _ => {}
        }
    }
    if files.len() != 1 {
        return Err(format!("{} graph inode anchors for {path}", files.len()).into());
    }
    Ok(files.into_iter().next().unwrap())
}

fn validate_file(objects: &Objects, f: &File, index: u32) -> Result<()> {
    let creator = objects
        .get(&f.owner)
        .ok_or("file creator is outside selected context")?;
    let additions: Vec<_> = creator
        .hunks()
        .iter()
        .filter_map(|h| match h {
            GraphOp::FileAdd {
                add_inode, path, ..
            } if add_inode.start.get() == f.position => Some(path),
            _ => None,
        })
        .collect();
    if additions.len() != 1 {
        return Err("file inode has no unique canonical creation".into());
    }
    let declarations: Vec<_> = creator
        .file_ops()
        .iter()
        .filter(|o| {
            o.path() == additions[0] && matches!(o.trunk_op(), Some(TrunkOp::Create { .. }))
        })
        .collect();
    if declarations.len() != 1
        || !declarations[0].trunk_id().change_id().is_root()
        || declarations[0].trunk_id().file_idx() != index
    {
        return Err(format!("semantic file index {index} disagrees with graph inode creation {}:{} ({}, declared trunks: {:?})",
            f.owner, f.position, additions[0], declarations.iter().map(|o| o.trunk_id()).collect::<Vec<_>>()).into());
    }
    Ok(())
}

fn requirements(
    objects: &Objects,
    consumer: Hash,
    symbol: u64,
) -> Result<(Vec<Reference>, BTreeMap<u64, Hash>)> {
    if symbol == 0 {
        return Err("ROOT is not a foreign owner symbol".into());
    }
    let change = objects
        .get(&consumer)
        .ok_or("consumer is outside selected context")?;
    let mut refs = Vec::new();
    let mut forced = BTreeMap::new();
    for (oi, ops) in change.file_ops().iter().enumerate() {
        let relevant = ops
            .referenced_node_ids()
            .iter()
            .any(|id| id.get() == symbol);
        let owner = ops.trunk_id().change_id();
        if !relevant && owner.is_root() {
            continue;
        }
        let f = file(change, consumer, ops.path())?;
        if !owner.is_root() {
            if let Some(previous) = forced.insert(owner.get(), f.owner) {
                if previous != f.owner {
                    return Err("consumer has contradictory inode owner bindings".into());
                }
            }
        }
        // This is a witness for one patch-scoped name, not certification of
        // unrelated operations in the entire historical patch. Keep canonical
        // owner constraints above, but validate indices/operands only where
        // this symbol occurs. In particular, unrelated ROOT attribute ops do
        // not supply or consume a foreign branch-owner name.
        if !relevant {
            continue;
        }
        validate_file(objects, &f, ops.trunk_id().file_idx())?;
        let trunk_operand = match ops.trunk_op() {
            Some(TrunkOp::Delete { trunk })
            | Some(TrunkOp::Move { trunk, .. })
            | Some(TrunkOp::Undelete { trunk })
            | Some(TrunkOp::SetMode { trunk, .. })
            | Some(TrunkOp::SetKind { trunk, .. }) => Some(*trunk),
            _ => None,
        };
        if owner.get() == symbol || trunk_operand.is_some_and(|t| t.change_id().get() == symbol) {
            return Err("this bounded witness prototype covers branch-owner references, not trunk completion".into());
        }
        for (li, line) in ops.line_ops().iter().enumerate() {
            let prefix = format!("file_ops[{oi}].line[{li}]");
            let mut add = |b: atomic_core::crdt::BranchId, role: String| {
                if b.change_id().get() == symbol {
                    refs.push(Reference {
                        file: f.clone(),
                        branch: b.branch_idx(),
                        role,
                    });
                }
            };
            add(line.branch_id(), format!("{prefix}.identity"));
            let mut branches = Vec::new();
            let mut groups: Vec<&[LeafOp]> = Vec::new();
            match line.operation() {
                BranchOp::Insert { after, content } => {
                    branches.extend(*after);
                    groups.push(content);
                }
                BranchOp::Delete { branch, content } => {
                    branches.push(*branch);
                    groups.push(content);
                }
                BranchOp::Modify {
                    branch,
                    old_content,
                    new_content,
                } => {
                    branches.push(*branch);
                    groups.push(old_content);
                    groups.push(new_content);
                }
                BranchOp::Restore { branch } => branches.push(*branch),
                BranchOp::Reparent { branch, new_after } => {
                    branches.push(*branch);
                    branches.extend(*new_after);
                }
            }
            if !matches!(line.operation(), BranchOp::Insert { .. })
                && branches[0] != line.branch_id()
            {
                return Err("contradictory branch identity and operation operand".into());
            }
            for (i, b) in branches.into_iter().enumerate() {
                add(b, format!("{prefix}.branch_operand[{i}]"));
            }
            for token in groups.into_iter().flatten() {
                let leaf = match token {
                    LeafOp::Insert { after, .. } => *after,
                    LeafOp::Delete { leaf }
                    | LeafOp::Replace { leaf, .. }
                    | LeafOp::Restore { leaf } => Some(*leaf),
                };
                if leaf.is_some_and(|l| l.change_id().get() == symbol) {
                    return Err("this bounded witness prototype cannot leave token-owner occurrences uncovered".into());
                }
            }
        }
    }
    if refs.is_empty() {
        return Err("symbol has no branch references".into());
    }
    Ok((refs, forced))
}

fn insertions<'a>(
    change: &'a Change,
    own: Hash,
    f: &File,
) -> Vec<&'a atomic_core::change::Insertion<Option<Hash>>> {
    let mut insertions = Vec::new();
    for hunk in change.hunks() {
        let insertion = match hunk {
            GraphOp::FileAdd {
                add_inode,
                contents,
                ..
            } if *f
                == (File {
                    owner: own,
                    position: add_inode.start.get(),
                }) =>
            {
                contents.as_ref()
            }
            GraphOp::Edit {
                change: Atom::Insertion(i),
                ..
            } => Some(i),
            GraphOp::Replacement { replacement, .. } => Some(replacement),
            _ => None,
        };
        if let Some(i) = insertion {
            if !i.flag.contains(atomic_core::types::EdgeFlags::FOLDER) && anchor(i.inode, own) == *f
            {
                insertions.push(i);
            }
        }
    }
    insertions
}

fn spans(change: &Change, own: Hash, f: &File) -> Vec<(u64, u64)> {
    let mut spans: Vec<_> = insertions(change, own, f)
        .iter()
        .map(|i| (i.start.get(), i.end.get()))
        .collect();
    spans.sort();
    spans
}

fn line_start(objects: &Objects, change: &Change, own: Hash, f: &File, lo: u64) -> bool {
    insertions(change, own, f).iter().any(|i| {
        if i.start.get() < lo && lo < i.end.get() {
            return change.contents.get(lo as usize - 1) == Some(&b'\n');
        }
        i.start.get() == lo
            && !i.predecessors.is_empty()
            && i.predecessors.iter().all(|p| {
                let position = anchor(*p, own);
                if position == *f {
                    return true;
                } // Exact empty file-inode marker.
                position
                    .position
                    .checked_sub(1)
                    .and_then(|index| objects.get(&position.owner)?.contents.get(index as usize))
                    == Some(&b'\n')
            })
    })
}

fn covers(spans: &[(u64, u64)], lo: u64, hi: u64) -> bool {
    let mut at = lo;
    for (start, end) in spans {
        if *end <= at {
            continue;
        }
        if *start > at {
            break;
        }
        at = at.max(*end);
        if at >= hi {
            return true;
        }
    }
    at >= hi
}

fn declarations(objects: &Objects, own: Hash) -> Result<BTreeMap<(File, u32), serde_json::Value>> {
    let change = objects
        .get(&own)
        .ok_or("provider is outside selected context")?;
    let mut seen = BTreeSet::new();
    let mut out = BTreeMap::new();
    for (oi, ops) in change.file_ops().iter().enumerate() {
        if !ops.line_ops().iter().any(|line| {
            line.branch_id().change_id().is_root()
                && matches!(line.operation(), BranchOp::Insert { .. })
        }) {
            continue;
        }
        let f = file(change, own, ops.path())?;
        validate_file(objects, &f, ops.trunk_id().file_idx())?;
        let ranges = spans(change, own, &f);
        for (li, line) in ops.line_ops().iter().enumerate() {
            let BranchOp::Insert { content, .. } = line.operation() else {
                continue;
            };
            if !line.branch_id().change_id().is_root() {
                continue;
            }
            if !seen.insert(line.branch_id().branch_idx()) {
                return Err("duplicate provider branch declaration".into());
            }
            let Some((lo, hi)) = line.content_range() else {
                continue;
            };
            let (lo, hi) = (lo.get(), hi.get());
            if hi <= lo {
                continue;
            }
            let raw = change
                .contents
                .get(lo as usize..hi as usize)
                .ok_or("semantic range outside verified contents")?;
            let mut bytes = Vec::new();
            for token in content {
                match token {
                    LeafOp::Insert { content, .. } => bytes.extend_from_slice(content),
                    _ => return Err("unsupported provider token edit".into()),
                }
            }
            // Match the actual tokenizer: LF separates branches; no other byte,
            // including CR and trailing spaces, may be silently trimmed.
            let Some(body) = raw.strip_suffix(b"\n") else {
                continue;
            };
            if body.contains(&b'\n')
                || bytes != body
                || !covers(&ranges, lo, hi)
                || !line_start(objects, change, own, &f, lo)
            {
                continue;
            }
            let relevant: Vec<_> = ranges
                .iter()
                .filter(|(start, end)| *end > lo && *start < hi)
                .copied()
                .collect();
            out.insert(
                (f.clone(), line.branch_id().branch_idx()),
                serde_json::json!({
                    "file_ops":oi,"line_op":li,"owner":own,"start":lo,"end":hi,
                    "bytes_blake3":Hash::of(raw),"graph_inode":f,"graph_spans":relevant
                }),
            );
        }
    }
    Ok(out)
}

fn provider_allowed(provider: Hash, symbol: u64, forced: &BTreeMap<u64, Hash>) -> bool {
    forced.iter().all(|(name, owner)| {
        if *name == symbol {
            *owner == provider
        } else {
            *owner != provider
        }
    })
}

fn order_anomalies(change: &Change) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for (oi, ops) in change.file_ops().iter().enumerate() {
        for (li, line) in ops.line_ops().iter().enumerate() {
            if let BranchOp::Reparent {
                branch,
                new_after: Some(after),
            } = line.operation()
            {
                if branch == after {
                    out.push(serde_json::json!({
                        "kind":"self_reparent", "file_ops":oi, "line_op":li,
                        "foreign_symbol":branch.change_id().get(), "branch":branch.branch_idx()
                    }));
                }
            }
        }
    }
    out
}

fn requires_context(objects: &Objects, provider: Hash, consumer: Hash) -> Result<bool> {
    let mut pending = vec![provider];
    let mut seen = BTreeSet::new();
    while let Some(hash) = pending.pop() {
        if hash == consumer {
            return Ok(true);
        }
        if seen.insert(hash) {
            pending.extend(
                objects
                    .get(&hash)
                    .ok_or("incomplete provider patch context")?
                    .dependencies(),
            );
        }
    }
    Ok(false)
}

pub(crate) fn proof(
    objects: &Objects,
    consumer: Hash,
    symbol: u64,
    provider: Hash,
) -> Result<serde_json::Value> {
    if consumer == provider {
        return Err("foreign owner cannot be self".into());
    }
    if requires_context(objects, provider, consumer)? {
        return Err("completion would make required patch context cyclic".into());
    }
    let (needs, forced) = requirements(objects, consumer, symbol)?;
    if !provider_allowed(provider, symbol, &forced) {
        return Err("provider aliases another inode-proven consumer name".into());
    }
    let declared = declarations(objects, provider)?;
    let mut proofs = Vec::new();
    for need in &needs {
        let declaration = declared
            .get(&(need.file.clone(), need.branch))
            .ok_or("provider does not cover every reference")?;
        proofs.push(serde_json::json!({"reference":need,"declaration":declaration}));
    }
    Ok(
        serde_json::json!({"consumer":consumer,"foreign_symbol":symbol,"provider":provider,"proofs":proofs,
        "consumer_order_anomalies":order_anomalies(&objects[&consumer]),"application_validated":false}),
    )
}

pub(crate) fn reconstruct(
    objects: &Objects,
    consumer: Hash,
    symbol: u64,
) -> Result<serde_json::Value> {
    requirements(objects, consumer, symbol)?;
    let mut choices = Vec::new();
    for provider in objects.keys().copied() {
        if let Ok(witness) = proof(objects, consumer, symbol, provider) {
            choices.push(witness);
        }
    }
    if choices.is_empty() {
        return Err("no complete graph-backed provider in the selected scope".into());
    }
    if choices.len() != 1 {
        return Err(format!("ambiguous context: {} complete providers", choices.len()).into());
    }
    let mut witness = choices.pop().unwrap();
    witness["search_scope"] = serde_json::json!(objects.keys().collect::<Vec<_>>());
    Ok(witness)
}

pub(crate) fn verify_positive_witness(
    objects: &Objects,
    witness: &serde_json::Value,
) -> Result<()> {
    let consumer = serde_json::from_value(witness["consumer"].clone())?;
    let provider = serde_json::from_value(witness["provider"].clone())?;
    let symbol = witness["foreign_symbol"]
        .as_u64()
        .ok_or("invalid foreign symbol")?;
    let expected = proof(objects, consumer, symbol, provider)?;
    for key in [
        "consumer",
        "provider",
        "foreign_symbol",
        "proofs",
        "consumer_order_anomalies",
        "application_validated",
    ] {
        if witness[key] != expected[key] {
            return Err(format!("incomplete or altered context witness: {key}").into());
        }
    }
    // search_scope records how an automatic choice was obtained. It is not a
    // lifetime dependency on a live view: positive endpoint validation does not
    // switch providers when another candidate later becomes available.
    Ok(())
}

// Included by the read-only example as well as the experimental native module.
#[allow(dead_code)]
pub(crate) fn run_example() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 {
        return Err("usage: reconstruct_semantic_context <repo> <consumer-hash> <foreign-symbol> <new-output-json>".into());
    }
    let repo = Repository::open_readonly(&args[1])?;
    let destination = Path::new(&args[4]);
    if destination.exists() {
        return Err("refuse existing output".into());
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let output =
        std::fs::canonicalize(parent)?.join(destination.file_name().ok_or("invalid output name")?);
    if output.starts_with(std::fs::canonicalize(repo.root())?)
        || output.starts_with(std::fs::canonicalize(repo.dot_dir())?)
    {
        return Err("diagnostic output must be outside the read-only repository".into());
    }
    let db = repo.dot_dir().join("atomic.redb");
    let before = std::fs::read(&db)?;
    let consumer = Hash::from_base32(args[2].as_bytes()).ok_or("invalid consumer hash")?;
    let symbol = args[3].parse()?;
    let txn = repo.pristine().read_txn()?;
    let view = txn
        .get_view(repo.current_view())?
        .ok_or("missing selected view")?;
    let scope = crate::repository::effective_projection_closure(&txn, &view)?;
    let store = crate::changestore::ChangeStore::open_existing(repo.changes_dir(), 32)?;
    let mut objects = Objects::new();
    for id in scope.iter_dependency_first().filter(|id| !id.is_root()) {
        let hash = txn
            .get_external(*id)?
            .ok_or("unregistered context object")?;
        let bytes = std::fs::read(store.change_path(&hash))?;
        let original = VerifiedChange::from_bytes(&bytes)?;
        if original.hash() != hash {
            return Err("stored object identity mismatch".into());
        }
        objects.insert(hash, original.change().clone());
    }
    drop(txn);
    let mut witness = reconstruct(&objects, consumer, symbol)?;
    verify_positive_witness(&objects, &witness)?;
    witness["meaning"] = serde_json::json!(
        "read-only prototype context witness; not an applied repair or original-author attestation"
    );
    witness["search_set_id"] = serde_json::json!(repo.view_set_id(repo.current_view())?);
    witness["audit_source_database_blake3"] = serde_json::json!(Hash::of(&before));
    if before != std::fs::read(&db)? {
        return Err("source database changed during planning; refusing a stale result".into());
    }
    let refs = witness["proofs"].as_array().unwrap();
    let distinct: BTreeSet<_> = refs
        .iter()
        .map(|p| {
            (
                p["reference"]["file"].to_string(),
                p["reference"]["branch"].to_string(),
            )
        })
        .collect();
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    std::io::Write::write_all(&mut output, &serde_json::to_vec_pretty(&witness)?)?;
    println!(
        "Reconstructed {} reference occurrences, {} distinct graph-backed branches; provider {}",
        refs.len(),
        distinct.len(),
        witness["provider"]
    );
    println!("Detected {} self-parent ordering operations in the consumer; this endpoint witness does not validate their composition.", witness["consumer_order_anomalies"].as_array().unwrap().len());
    println!(
        "Original database unchanged. No patch, dependency, capability or binding was applied."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::RecordOptions;
    use crate::{TrackingOptions, UnrecordOptions};
    use atomic_core::change::{ChangeHeader, LineOps};
    use atomic_core::crdt::BranchId;
    use atomic_core::types::{ChangePosition, NodeId};

    fn insert(objects: &mut Objects, change: Change) -> Hash {
        let mut raw = Vec::new();
        let encoded = change.serialize(&mut raw).unwrap();
        let verified = VerifiedChange::from_bytes(&raw).unwrap();
        assert_eq!(verified.hash(), encoded);
        let hash = verified.hash();
        objects.insert(hash, verified.change().clone());
        hash
    }
    fn record(repo: &Repository, text: &[u8], message: &str) -> Hash {
        std::fs::write(repo.root().join("f"), text).unwrap();
        *repo
            .record(
                repo.require_working_copy_id().unwrap(),
                ChangeHeader::new(message),
                RecordOptions::new()
                    .paths(vec!["f"])
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap()
            .hash()
    }
    fn fixture() -> (Objects, Hash, Hash) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("f"), b"alpha\nbeta\ntheta\n").unwrap();
        repo.add(
            repo.require_working_copy_id().unwrap(),
            "f",
            TrackingOptions::default(),
        )
        .unwrap();
        let a = record(&repo, b"alpha\nbeta\ntheta\n", "synthetic base");
        let q = record(&repo, b"alpha\nbeta\ntheta\ngamma\n", "synthetic provider");
        repo.unrecord(&q, UnrecordOptions::default()).unwrap();
        let p = record(&repo, b"ALPHA\nbeta\ntheta\n", "synthetic consumer");
        let mut consumer = repo.load_change(&p).unwrap();
        let foreign = BranchId::new(NodeId::new(15), 0);
        consumer.hashed.file_ops[0].add_line_op(LineOps::new(
            foreign,
            BranchOp::Reparent {
                branch: foreign,
                new_after: Some(BranchId::new(NodeId::new(1), 2)),
            },
        ));
        consumer.hashed.header = ChangeHeader::new("synthetic unresolved branch context");
        let mut objects = Objects::new();
        insert(&mut objects, repo.load_change(&a).unwrap());
        let provider = insert(&mut objects, repo.load_change(&q).unwrap());
        let consumer = insert(&mut objects, consumer);
        assert!(!objects[&consumer].dependencies().contains(&provider));
        (objects, consumer, provider)
    }
    fn duplicate(objects: &mut Objects, provider: Hash) -> Hash {
        let p = &objects[&provider];
        let copy = Change::with_file_ops(
            ChangeHeader::new("synthetic second matching provider"),
            p.hunks().to_vec(),
            p.file_ops().to_vec(),
            p.contents.clone(),
            p.dependencies().to_vec(),
        );
        insert(objects, copy)
    }

    #[test]
    fn reconstructs_and_verifies_every_occurrence_without_local_ids() {
        let (objects, consumer, provider) = fixture();
        let witness = reconstruct(&objects, consumer, 15).unwrap();
        verify_positive_witness(&objects, &witness).unwrap();
        assert_eq!(witness["provider"], serde_json::json!(provider));
        assert_eq!(witness["proofs"].as_array().unwrap().len(), 2);
        let reversed: Objects = objects.iter().rev().map(|(h, c)| (*h, c.clone())).collect();
        assert_eq!(witness, reconstruct(&reversed, consumer, 15).unwrap());
    }
    #[test]
    fn unrelated_attribute_indices_do_not_block_a_branch_owner_witness() {
        use atomic_core::change::FileOps;
        use atomic_core::crdt::TrunkId;
        let (mut objects, consumer, provider) = fixture();
        let mut change = objects.remove(&consumer).unwrap();
        // Reproduce the unrelated ROOT attribute-operation pattern in HFAP35.
        // It contains no occurrence of the requested foreign symbol.
        change.hashed.file_ops.push(
            FileOps::set_mode(TrunkId::new(NodeId::ROOT, 1), "unrelated.py".into(), 0o644).unwrap(),
        );
        let consumer = insert(&mut objects, change);
        let witness = reconstruct(&objects, consumer, 15).unwrap();
        assert_eq!(witness["provider"], serde_json::json!(provider));
        assert_eq!(witness["proofs"].as_array().unwrap().len(), 2);
        verify_positive_witness(&objects, &witness).unwrap();
    }
    #[test]
    fn an_index_disagreement_at_the_requested_symbol_still_refuses() {
        use atomic_core::change::FileOps;
        use atomic_core::crdt::TrunkId;
        let (mut objects, consumer, provider) = fixture();
        let mut change = objects.remove(&consumer).unwrap();
        let original = change.hashed.file_ops.remove(0);
        let mut wrong = FileOps::edit(
            TrunkId::new(original.trunk_id().change_id(), 99),
            original.path().into(),
        );
        for line in original.line_ops() {
            wrong.add_line_op(line.clone());
        }
        change.hashed.file_ops.push(wrong);
        let consumer = insert(&mut objects, change);
        assert!(proof(&objects, consumer, 15, provider)
            .unwrap_err()
            .to_string()
            .contains("semantic file index 99"));
    }
    #[test]
    fn missing_provider_never_falls_back_to_an_inode_owner_number() {
        let (mut objects, consumer, provider) = fixture();
        objects.remove(&provider);
        assert!(reconstruct(&objects, consumer, 15)
            .unwrap_err()
            .to_string()
            .contains("no complete"));
    }
    #[test]
    fn another_valid_provider_blocks_new_automatic_choice_but_not_a_pinned_witness() {
        let (mut objects, consumer, provider) = fixture();
        let witness = reconstruct(&objects, consumer, 15).unwrap();
        duplicate(&mut objects, provider);
        assert!(reconstruct(&objects, consumer, 15)
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
        verify_positive_witness(&objects, &witness).unwrap();
        assert_eq!(witness["provider"], serde_json::json!(provider));
    }
    #[test]
    fn incomplete_or_modified_witness_is_rejected() {
        let (objects, consumer, _) = fixture();
        let original = reconstruct(&objects, consumer, 15).unwrap();
        let mut missing = original.clone();
        missing["proofs"].as_array_mut().unwrap().pop();
        assert!(verify_positive_witness(&objects, &missing).is_err());
        let mut changed = original;
        changed["proofs"][0]["declaration"]["end"] = serde_json::json!(9999);
        assert!(verify_positive_witness(&objects, &changed).is_err());
    }
    #[test]
    fn complete_context_does_not_certify_a_self_parent_order_operation() {
        let (mut objects, consumer, provider) = fixture();
        let mut p = objects.remove(&consumer).unwrap();
        let branch = BranchId::new(NodeId::new(15), 0);
        let last = p.hashed.file_ops[0].line_ops_mut().last_mut().unwrap();
        *last = LineOps::new(
            branch,
            BranchOp::Reparent {
                branch,
                new_after: Some(branch),
            },
        );
        let consumer = insert(&mut objects, p);
        let mut witness = reconstruct(&objects, consumer, 15).unwrap();
        verify_positive_witness(&objects, &witness).unwrap();
        assert_eq!(witness["provider"], serde_json::json!(provider));
        assert_eq!(witness["application_validated"], false);
        assert_eq!(
            witness["consumer_order_anomalies"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        witness["consumer_order_anomalies"] = serde_json::json!([]);
        assert!(verify_positive_witness(&objects, &witness).is_err());
    }
    #[test]
    fn a_provider_requiring_the_consumer_is_not_a_valid_completion() {
        let (mut objects, consumer, provider) = fixture();
        let p = objects.remove(&provider).unwrap();
        let cyclic = Change::with_file_ops(
            ChangeHeader::new("synthetic cyclic completion"),
            p.hunks().to_vec(),
            p.file_ops().to_vec(),
            p.contents,
            vec![consumer],
        );
        insert(&mut objects, cyclic);
        assert!(reconstruct(&objects, consumer, 15).is_err());
    }
    #[test]
    fn a_semantic_range_without_graph_byte_support_cannot_supply_context() {
        let (mut objects, consumer, provider) = fixture();
        let mut bad = objects.remove(&provider).unwrap();
        let line = &mut bad.hashed.file_ops[0].line_ops_mut()[0];
        let (lo, hi) = line.content_range().unwrap();
        line.set_content_range(ChangePosition::new(lo.get() + 1), hi);
        insert(&mut objects, bad);
        assert!(reconstruct(&objects, consumer, 15).is_err());
    }
    #[test]
    fn matching_path_text_and_indices_do_not_override_a_different_inode() {
        let (mut objects, consumer, provider) = fixture();
        let original = &objects[&consumer];
        let f = file(original, consumer, "f").unwrap();
        let base = &objects[&f.owner];
        let other = Change::with_file_ops(
            ChangeHeader::new("synthetic different inode at the same path"),
            base.hunks().to_vec(),
            base.file_ops().to_vec(),
            base.contents.clone(),
            base.dependencies().to_vec(),
        );
        let other = insert(&mut objects, other);
        let mut wrong = objects.remove(&provider).unwrap();
        wrong.hashed.header = ChangeHeader::new("synthetic provider on the other inode");
        wrong.hashed.dependencies = vec![other];
        for hunk in &mut wrong.hashed.hunks {
            if let GraphOp::Edit {
                change: Atom::Insertion(i),
                ..
            } = hunk
            {
                i.inode.change = Some(other);
                for p in i.predecessors.iter_mut().chain(&mut i.successors) {
                    p.change = Some(other);
                }
            }
        }
        insert(&mut objects, wrong);
        assert!(reconstruct(&objects, consumer, 15).is_err());
    }
    #[test]
    fn token_bytes_alone_do_not_prove_graph_coverage() {
        let (mut objects, consumer, provider) = fixture();
        let mut wrong = objects.remove(&provider).unwrap();
        if let GraphOp::Edit {
            change: Atom::Insertion(i),
            ..
        } = &mut wrong.hashed.hunks[0]
        {
            i.start = ChangePosition::new(i.start.get() + 1);
        }
        insert(&mut objects, wrong);
        assert!(reconstruct(&objects, consumer, 15).is_err());
    }
    #[test]
    fn matching_tokens_in_the_middle_of_a_line_are_not_a_branch_witness() {
        let (mut objects, consumer, provider) = fixture();
        let mut bad = objects.remove(&provider).unwrap();
        let old = bad.file_ops()[0].line_ops()[0].clone();
        let (lo, hi) = old.content_range().unwrap();
        let mut operation = old.operation().clone();
        if let BranchOp::Insert { content, .. } = &mut operation {
            *content = vec![LeafOp::Insert {
                after: None,
                kind: atomic_core::diff::token::TokenKind::Word,
                content: bad.contents[lo.get() as usize + 1..hi.get() as usize - 1].to_vec(),
            }];
        }
        bad.hashed.file_ops[0].line_ops_mut()[0] = LineOps::new(old.branch_id(), operation)
            .with_content_range(ChangePosition::new(lo.get() + 1), hi);
        insert(&mut objects, bad);
        assert!(reconstruct(&objects, consumer, 15).is_err());
    }
    #[test]
    fn an_uncovered_token_occurrence_prevents_branch_only_certification() {
        let (mut objects, consumer, _) = fixture();
        let mut p = objects.remove(&consumer).unwrap();
        let old = p.file_ops()[0].line_ops()[0].clone();
        let mut op = old.operation().clone();
        if let BranchOp::Modify { old_content, .. } = &mut op {
            old_content.push(LeafOp::Restore {
                leaf: atomic_core::crdt::LeafId::new(NodeId::new(15), 0),
            });
        } else {
            panic!("fixture must modify the original first line");
        }
        let (lo, hi) = old.content_range().unwrap();
        p.hashed.file_ops[0].line_ops_mut()[0] =
            LineOps::new(old.branch_id(), op).with_content_range(lo, hi);
        let consumer = insert(&mut objects, p);
        assert!(reconstruct(&objects, consumer, 15).is_err());
    }
    #[test]
    fn duplicate_declarations_and_uncovered_references_do_not_certify_partial_context() {
        let (mut objects, consumer, provider) = fixture();
        let mut bad = objects.remove(&provider).unwrap();
        let line = bad.file_ops()[0].line_ops()[0].clone();
        bad.hashed.file_ops[0].add_line_op(line);
        insert(&mut objects, bad);
        assert!(reconstruct(&objects, consumer, 15).is_err());
        let (mut objects, consumer, _) = fixture();
        let mut p = objects.remove(&consumer).unwrap();
        let missing = BranchId::new(NodeId::new(15), 999);
        p.hashed.file_ops[0].add_line_op(LineOps::new(
            missing,
            BranchOp::Reparent {
                branch: missing,
                new_after: None,
            },
        ));
        let changed_consumer = insert(&mut objects, p);
        assert!(reconstruct(&objects, changed_consumer, 15).is_err());
    }
}
