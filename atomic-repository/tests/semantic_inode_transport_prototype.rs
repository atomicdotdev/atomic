//! Working native prototype, not a context-completion opcode or legacy repair.
use atomic_core::change::envelope::SessionEnvelope;
#[cfg(feature = "prototype-inode-transport")]
use atomic_core::change::GraphOp;
#[cfg(feature = "prototype-inode-transport")]
use atomic_core::change::LineOps;
use atomic_core::change::{Atom, AtomRef, Change, ChangeHeader, VerifiedChange};
use atomic_core::crdt::tables::encode_branch_id;
use atomic_core::crdt::BranchId;
#[cfg(feature = "prototype-inode-transport")]
use atomic_core::crdt::{BranchOp, LeafId, LeafOp};
use atomic_core::pristine::{CrdtTxnT, GraphTxnT, MutTxnT};
use atomic_core::types::Hash;
#[cfg(feature = "prototype-inode-transport")]
use atomic_core::types::NodeId;
#[cfg(feature = "prototype-inode-transport")]
use atomic_core::types::Position;
use atomic_repository::record::RecordOptions;
#[cfg(feature = "prototype-inode-transport")]
use atomic_repository::UnrecordOptions;
use atomic_repository::{InsertOptions, Repository, TrackingOptions};
#[cfg(feature = "prototype-inode-transport")]
use std::collections::BTreeSet;
use tempfile::TempDir;

const VIEW: &str = "native-algebra-prototype";
const BASE: &[u8] = b"alpha\nbeta\ntheta\n";
const EDITED: &[u8] = b"alpha\nbeta\ntheta\ngamma\n";

struct Fixture {
    _dir: TempDir,
    source: Repository,
    changes: Vec<Change>,
    hashes: Vec<Hash>,
    envelope: Vec<u8>,
}

fn repo(dir: &std::path::Path) -> Repository {
    let mut repo = Repository::init(dir).unwrap();
    repo.create_draft_view(VIEW, "dev").unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    repo.switch_view(wc, VIEW).unwrap();
    repo
}

fn record(repo: &Repository, path: &str, message: &str, metadata: Vec<u8>) -> Hash {
    *repo
        .record(
            repo.require_working_copy_id().unwrap(),
            ChangeHeader::new(message),
            RecordOptions::new()
                .paths(vec![path])
                .with_all(true)
                .metadata_bytes(metadata)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap()
        .hash()
}

fn stored(repo: &Repository, hash: Hash) -> Vec<u8> {
    let store =
        atomic_repository::changestore::ChangeStore::open_existing(repo.changes_dir(), 16).unwrap();
    let bytes = std::fs::read(store.change_path(&hash)).unwrap();
    assert_eq!(VerifiedChange::from_bytes(&bytes).unwrap().hash(), hash);
    bytes
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let source = repo(dir.path());
    // Real, unrelated stored objects occupy source IDs 1 and 2 without being
    // part of this view. Receiver allocation must not have to reproduce them.
    for i in 0..2 {
        let hash = source
            .save_change(&Change::empty(ChangeHeader::new(format!(
                "unrelated fixture {i}"
            ))))
            .unwrap();
        let mut txn = source.pristine().write_txn().unwrap();
        txn.register_change(&hash).unwrap();
        txn.commit().unwrap();
    }
    let mut hashes = Vec::new();
    for (path, bytes) in [
        ("a.txt", BASE),
        ("b.txt", b"other\n"),
        ("c.txt", b"charlie\n"),
    ] {
        std::fs::write(dir.path().join(path), bytes).unwrap();
        source
            .add(
                source.require_working_copy_id().unwrap(),
                path,
                TrackingOptions::default(),
            )
            .unwrap();
        hashes.push(record(&source, path, &format!("create {path}"), vec![]));
    }
    let envelope = SessionEnvelope::builder("synthetic-native-prototype", "synthetic-agent")
        .turn_number(1)
        .files_in_turn(vec!["a.txt".into()])
        .build()
        .encode()
        .unwrap();
    std::fs::write(dir.path().join("a.txt"), EDITED).unwrap();
    hashes.push(record(
        &source,
        "a.txt",
        "E: append gamma",
        envelope.clone(),
    ));
    std::fs::write(dir.path().join("a.txt"), BASE).unwrap();
    hashes.push(record(
        &source,
        "a.txt",
        "R: inverse of gamma insertion",
        vec![],
    ));
    let changes: Vec<_> = hashes
        .iter()
        .map(|hash| source.load_change(hash).unwrap())
        .collect();
    assert!(changes.iter().all(|c| c.hashed.span.is_none()));
    assert!(changes[..3].iter().all(|c| c.dependencies().is_empty()));
    assert_eq!(changes[3].dependencies(), [hashes[0]]);
    assert!(changes[4].dependencies().contains(&hashes[3]));
    assert!(changes[4]
        .dependencies()
        .iter()
        .all(|h| *h == hashes[0] || *h == hashes[3]));
    assert!(
        changes[4]
            .hunks()
            .iter()
            .flat_map(|h| h.atoms())
            .any(|atom| {
                let update = match atom {
                    AtomRef::EdgeUpdate(update) | AtomRef::Atom(Atom::EdgeUpdate(update)) => {
                        Some(update)
                    }
                    _ => None,
                };
                update.is_some_and(|u| u.edges.iter().any(|edge| edge.is_deletion()))
            }),
        "R must contain a real graph edge reversal, not just remove E from a view"
    );
    for hunk in changes[4].hunks() {
        for atom in hunk.atoms() {
            let update = match atom {
                AtomRef::EdgeUpdate(update) | AtomRef::Atom(Atom::EdgeUpdate(update)) => update,
                _ => panic!("R must reverse E's edges, not recreate the baseline content"),
            };
            assert!(update.edges.iter().all(|edge| edge.is_deletion()
                && edge.to.change == Some(hashes[3])
                && edge.introduced_by == Some(hashes[3])));
        }
    }
    Fixture {
        _dir: dir,
        source,
        changes,
        hashes,
        envelope,
    }
}

#[cfg(feature = "prototype-inode-transport")]
fn permutations(prefix: Vec<usize>, remaining: Vec<usize>, out: &mut Vec<Vec<usize>>) {
    if remaining.is_empty() {
        out.push(prefix);
        return;
    }
    for (i, index) in remaining.iter().enumerate() {
        let mut prefix = prefix.clone();
        prefix.push(*index);
        let mut rest = remaining.clone();
        rest.remove(i);
        permutations(prefix, rest, out);
    }
}

fn save_fixture(receiver: &Repository, f: &Fixture) {
    for (change, hash) in f.changes.iter().zip(&f.hashes) {
        assert_eq!(receiver.save_change(change).unwrap(), *hash);
        assert_eq!(stored(receiver, *hash), stored(&f.source, *hash));
    }
}

#[test]
#[cfg(feature = "prototype-inode-transport")]
fn all_twenty_native_orders_preserve_inode_ownership_inverse_and_recordability() {
    let f = fixture();
    let root = tempfile::tempdir().unwrap();
    let mut permutations_ = Vec::new();
    permutations(vec![], vec![0, 1, 2, 3, 4], &mut permutations_);
    let mut cases = 0;
    let mut common_set = None;
    for order in permutations_ {
        let mut seen = BTreeSet::new();
        if !order.iter().all(|i| {
            let valid = f.changes[*i]
                .dependencies()
                .iter()
                .all(|h| seen.contains(h));
            seen.insert(f.hashes[*i]);
            valid
        }) {
            continue;
        }
        let name: String = order
            .iter()
            .map(|i| ['A', 'B', 'C', 'E', 'R'][*i])
            .collect();
        let receiver = repo(&root.path().join(&name));
        save_fixture(&receiver, &f);
        for index in &order {
            receiver
                .insert_change_rec(&f.hashes[*index], InsertOptions::default().view(VIEW))
                .unwrap();
        }
        assert_eq!(
            receiver
                .get_file_content_on_view("a.txt", VIEW)
                .unwrap()
                .unwrap(),
            BASE,
            "{name}"
        );
        assert_eq!(
            receiver
                .get_file_content_on_view("b.txt", VIEW)
                .unwrap()
                .unwrap(),
            b"other\n"
        );
        assert_eq!(
            receiver
                .get_file_content_on_view("c.txt", VIEW)
                .unwrap()
                .unwrap(),
            b"charlie\n"
        );
        let set = receiver.view_set_id(VIEW).unwrap();
        assert_eq!(*common_set.get_or_insert(set), set);
        let txn = receiver.pristine().read_txn().unwrap();
        let a = txn.get_internal(&f.hashes[0]).unwrap().unwrap();
        let e = txn.get_internal(&f.hashes[3]).unwrap().unwrap();
        let key = encode_branch_id(&BranchId::new(e, 0));
        let line = txn.get_crdt_branch(&key).unwrap().unwrap();
        assert_eq!(
            line.trunk_id.change_id(),
            a,
            "E attached to a different file in {name}"
        );
        assert!(
            line.state.is_deleted(),
            "the actual inverse must tombstone E's line"
        );
        for index in 0..3 {
            let original = txn
                .get_crdt_branch(&encode_branch_id(&BranchId::new(a, index)))
                .unwrap()
                .unwrap();
            assert!(
                original.state.is_alive(),
                "R changed an original line identity"
            );
            assert_eq!(original.trunk_id.change_id(), a);
        }
        drop(txn);
        assert_eq!(
            receiver.load_change(&f.hashes[3]).unwrap().hashed.metadata,
            f.envelope
        );
        receiver
            .insert_change_rec(&f.hashes[4], InsertOptions::default().view(VIEW))
            .unwrap();
        assert_eq!(receiver.view_set_id(VIEW).unwrap(), set);
        assert!(
            receiver
                .unrecord(&f.hashes[3], UnrecordOptions::default().view(VIEW))
                .is_err(),
            "R requires E"
        );
        assert_eq!(receiver.view_set_id(VIEW).unwrap(), set);
        receiver
            .unrecord(&f.hashes[4], UnrecordOptions::default().view(VIEW))
            .unwrap();
        assert_eq!(
            receiver
                .get_file_content_on_view("a.txt", VIEW)
                .unwrap()
                .unwrap(),
            EDITED
        );
        let visible = receiver
            .record_foreign_deleted_file_on_view_excluding("a.txt", VIEW, &[])
            .unwrap();
        assert_eq!(visible.crdt_ops().unwrap().line_ops().len(), 4);
        // The inverse is absent from this view, even though its rows remain in
        // the ambient database. Recording must use the selected patch context.
        receiver.materialize_view_to(VIEW, receiver.root()).unwrap();
        std::fs::write(
            receiver.root().join("a.txt"),
            b"alpha\nbeta\ntheta\ngamma\nintermediate\n",
        )
        .unwrap();
        let intermediate = record(
            &receiver,
            "a.txt",
            "record while inverse is unrecorded",
            vec![],
        );
        assert_eq!(
            receiver
                .get_file_content_on_view("a.txt", VIEW)
                .unwrap()
                .unwrap(),
            b"alpha\nbeta\ntheta\ngamma\nintermediate\n"
        );
        receiver
            .unrecord(&intermediate, UnrecordOptions::default().view(VIEW))
            .unwrap();
        assert_eq!(
            receiver
                .get_file_content_on_view("a.txt", VIEW)
                .unwrap()
                .unwrap(),
            EDITED
        );
        receiver
            .insert_change_rec(&f.hashes[4], InsertOptions::default().view(VIEW))
            .unwrap();
        assert_eq!(
            receiver
                .get_file_content_on_view("a.txt", VIEW)
                .unwrap()
                .unwrap(),
            BASE
        );
        assert_eq!(receiver.view_set_id(VIEW).unwrap(), set);
        // Native materialization followed by native recording: no alternate
        // interpreter, semantic-table editing or caller-side identity override.
        receiver.materialize_view_to(VIEW, receiver.root()).unwrap();
        std::fs::write(receiver.root().join("a.txt"), b"alpha\nbeta\ntheta\nnext\n").unwrap();
        let next = record(
            &receiver,
            "a.txt",
            "record after commuting patches and inverse",
            vec![],
        );
        assert_eq!(
            receiver
                .get_file_content_on_view("a.txt", VIEW)
                .unwrap()
                .unwrap(),
            b"alpha\nbeta\ntheta\nnext\n"
        );
        assert_eq!(
            receiver
                .get_file_content_on_view("c.txt", VIEW)
                .unwrap()
                .unwrap(),
            b"charlie\n"
        );
        assert_ne!(next, f.hashes[4]);
        for hash in &f.hashes {
            assert_eq!(stored(&receiver, *hash), stored(&f.source, *hash));
        }
        eprintln!("{name}: native ownership, actual inverse, attachment, replay and subsequent record PASS");
        cases += 1;
    }
    assert_eq!(cases, 20);
}

#[test]
#[cfg(feature = "prototype-inode-transport")]
fn invalid_context_refuses_without_publishing_the_change() {
    let f = fixture();
    for case in [
        "missing-branch",
        "token-from-another-line",
        "contradictory-branch",
        "undeclared-context",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let receiver = repo(dir.path());
        save_fixture(&receiver, &f);
        for hash in &f.hashes[..3] {
            receiver
                .insert_change_rec(hash, InsertOptions::default().view(VIEW))
                .unwrap();
        }
        let before = receiver.view_set_id(VIEW).unwrap();
        let mut bad = f.changes[3].clone();
        bad.hashed.header.message = format!("invalid prototype fixture: {case}");
        if case == "undeclared-context" {
            bad.hashed.dependencies.clear();
        } else {
            let line = bad.file_ops()[0].line_ops()[0].clone();
            let mut operation = line.operation().clone();
            if let BranchOp::Insert { after, content } = &mut operation {
                if case == "missing-branch" {
                    *after = Some(BranchId::new(NodeId::new(3), 999_999));
                }
                if case == "token-from-another-line" {
                    if let LeafOp::Insert { after, .. } = &mut content[0] {
                        *after = Some(LeafId::new(NodeId::new(3), 0));
                    }
                }
            }
            if case == "contradictory-branch" {
                operation = BranchOp::Delete {
                    branch: BranchId::new(NodeId::new(3), 2),
                    content: vec![],
                };
            }
            let (lo, hi) = line.content_range().unwrap();
            bad.hashed.file_ops[0].line_ops_mut()[0] =
                LineOps::new(line.branch_id(), operation).with_content_range(lo, hi);
        }
        let hash = receiver.save_change(&bad).unwrap();
        let error = receiver
            .insert_change(&hash, InsertOptions::default().view(VIEW))
            .unwrap_err();
        assert!(
            error.to_string().contains("inode prototype"),
            "wrong refusal for {case}: {error}"
        );
        assert_eq!(receiver.view_set_id(VIEW).unwrap(), before);
        assert_eq!(
            receiver
                .get_file_content_on_view("a.txt", VIEW)
                .unwrap()
                .unwrap(),
            BASE
        );
    }
}

#[test]
#[cfg(feature = "prototype-inode-transport")]
fn ambiguous_context_does_not_prefer_a_matching_local_number() {
    let f = fixture();
    let dir = tempfile::tempdir().unwrap();
    let receiver = repo(dir.path());
    save_fixture(&receiver, &f);
    for hash in &f.hashes[..4] {
        receiver
            .insert_change_rec(hash, InsertOptions::default().view(VIEW))
            .unwrap();
    }
    let mut concurrent = f.changes[3].clone();
    concurrent.hashed.header.message = "independent second insertion at the same anchor".into();
    let other = receiver.save_change(&concurrent).unwrap();
    receiver
        .insert_change_rec(&other, InsertOptions::default().view(VIEW))
        .unwrap();
    let txn = receiver.pristine().read_txn().unwrap();
    let matching_number = txn.get_internal(&f.hashes[3]).unwrap().unwrap();
    drop(txn);
    let before_set = receiver.view_set_id(VIEW).unwrap();
    let before_bytes = receiver.get_file_content_on_view("a.txt", VIEW).unwrap();
    let mut ambiguous = f.changes[3].clone();
    ambiguous.hashed.header.message =
        "ambiguous semantic owner in an explicit two-vertex context".into();
    ambiguous.hashed.dependencies = vec![f.hashes[0], f.hashes[3], other];
    ambiguous.hashed.dependencies.sort();
    let end = match &f.changes[3].hunks()[0] {
        GraphOp::Edit {
            change: Atom::Insertion(insert),
            ..
        } => insert.end,
        _ => panic!("fixture must be a graph insertion"),
    };
    if let GraphOp::Edit {
        change: Atom::Insertion(insert),
        ..
    } = &mut ambiguous.hashed.hunks[0]
    {
        insert.predecessors = vec![
            Position::new(Some(f.hashes[3]), end),
            Position::new(Some(other), end),
        ];
    }
    let line = ambiguous.file_ops()[0].line_ops()[0].clone();
    let mut operation = line.operation().clone();
    if let BranchOp::Insert { after, .. } = &mut operation {
        *after = Some(BranchId::new(matching_number, 0));
    }
    let (lo, hi) = line.content_range().unwrap();
    ambiguous.hashed.file_ops[0].line_ops_mut()[0] =
        LineOps::new(line.branch_id(), operation).with_content_range(lo, hi);
    let hash = receiver.save_change(&ambiguous).unwrap();
    let error = receiver
        .insert_change(&hash, InsertOptions::default().view(VIEW))
        .unwrap_err();
    assert!(
        error.to_string().contains("ambiguous graph-context owner"),
        "{error}"
    );
    assert_eq!(receiver.view_set_id(VIEW).unwrap(), before_set);
    assert_eq!(
        receiver.get_file_content_on_view("a.txt", VIEW).unwrap(),
        before_bytes
    );
}

#[test]
#[cfg(feature = "prototype-inode-transport")]
fn overlapping_native_edits_remain_conflicts_in_both_orders() {
    let f = fixture();
    let mut edits = Vec::new();
    let producers = tempfile::tempdir().unwrap();
    for label in ["LEFT", "RIGHT"] {
        let producer = repo(&producers.path().join(label));
        producer.save_change(&f.changes[0]).unwrap();
        producer
            .insert_change_rec(&f.hashes[0], InsertOptions::default().view(VIEW))
            .unwrap();
        producer.materialize_view_to(VIEW, producer.root()).unwrap();
        std::fs::write(
            producer.root().join("a.txt"),
            format!("{label}\nbeta\ntheta\n"),
        )
        .unwrap();
        let hash = record(&producer, "a.txt", label, vec![]);
        edits.push((hash, producer.load_change(&hash).unwrap()));
    }
    for order in [[0, 1], [1, 0]] {
        let dir = tempfile::tempdir().unwrap();
        let receiver = repo(dir.path());
        receiver.save_change(&f.changes[0]).unwrap();
        receiver
            .insert_change_rec(&f.hashes[0], InsertOptions::default().view(VIEW))
            .unwrap();
        for index in order {
            receiver.save_change(&edits[index].1).unwrap();
            receiver
                .insert_change_rec(&edits[index].0, InsertOptions::default().view(VIEW))
                .unwrap();
        }
        let bytes = receiver
            .get_file_content_on_view("a.txt", VIEW)
            .unwrap()
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.contains("<<<<<<<") && text.contains("LEFT") && text.contains("RIGHT"),
            "overlap was silently selected: {text}"
        );
    }
}

/// Historical unframed compatibility control, not the span-backed product path.
#[test]
#[cfg(not(feature = "prototype-inode-transport"))]
fn legacy_unframed_control_reproduces_semantic_misbinding() {
    let f = fixture();
    let dir = tempfile::tempdir().unwrap();
    let receiver = repo(dir.path());
    save_fixture(&receiver, &f);
    for hash in &f.hashes[..4] {
        receiver
            .insert_change_rec(hash, InsertOptions::default().view(VIEW))
            .unwrap();
    }
    assert_eq!(
        receiver
            .get_file_content_on_view("a.txt", VIEW)
            .unwrap()
            .unwrap(),
        EDITED
    );
    let txn = receiver.pristine().read_txn().unwrap();
    let a = txn.get_internal(&f.hashes[0]).unwrap().unwrap();
    let c = txn.get_internal(&f.hashes[2]).unwrap().unwrap();
    let e = txn.get_internal(&f.hashes[3]).unwrap().unwrap();
    let line = txn
        .get_crdt_branch(&encode_branch_id(&BranchId::new(e, 0)))
        .unwrap()
        .unwrap();
    assert_ne!(line.trunk_id.change_id(), a);
    assert_eq!(line.trunk_id.change_id(), c);
    assert_eq!(
        receiver.load_change(&f.hashes[3]).unwrap().hashed.metadata,
        f.envelope
    );
}
