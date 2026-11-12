use super::*;
use crate::change::{ChangeHeader, MemoryChangeStore};
use crate::pristine::{MutTxnT, Pristine, PristineError};
use crate::types::{EdgeFlags, GraphNode, Position};
use std::cell::{Cell, RefCell};

struct CountedTxn<'a> {
    inner: &'a ReadTxn,
    dependencies: Cell<usize>,
    internals: RefCell<HashMap<Hash, usize>>,
    files: RefCell<HashMap<(Hash, u32), usize>>,
    missing_internal: Option<Hash>,
}

impl<'a> CountedTxn<'a> {
    fn new(inner: &'a ReadTxn) -> Self {
        Self {
            inner,
            dependencies: Cell::new(0),
            internals: RefCell::default(),
            files: RefCell::default(),
            missing_internal: None,
        }
    }
}

impl GraphTxnT for CountedTxn<'_> {
    type Adj = <ReadTxn as GraphTxnT>::Adj;
    fn get_external(&self, id: NodeId) -> std::result::Result<Option<Hash>, PristineError> {
        self.inner.get_external(id)
    }
    fn get_internal(&self, hash: &Hash) -> std::result::Result<Option<NodeId>, PristineError> {
        *self.internals.borrow_mut().entry(*hash).or_default() += 1;
        if self.missing_internal == Some(*hash) {
            return Ok(None);
        }
        self.inner.get_internal(hash)
    }
    fn iter_adjacent(
        &self,
        node: GraphNode<NodeId>,
        min: EdgeFlags,
        max: EdgeFlags,
    ) -> std::result::Result<Self::Adj, PristineError> {
        self.inner.iter_adjacent(node, min, max)
    }
    fn find_block(
        &self,
        pos: Position<NodeId>,
    ) -> std::result::Result<GraphNode<NodeId>, PristineError> {
        self.inner.find_block(pos)
    }
    fn find_block_end(
        &self,
        pos: Position<NodeId>,
    ) -> std::result::Result<GraphNode<NodeId>, PristineError> {
        self.inner.find_block_end(pos)
    }
    fn has_vertex(&self, node: GraphNode<NodeId>) -> std::result::Result<bool, PristineError> {
        self.inner.has_vertex(node)
    }
    fn get_node_type(&self, id: NodeId) -> std::result::Result<Option<u8>, PristineError> {
        self.inner.get_node_type(id)
    }
    fn get_rev_deps(&self, id: NodeId) -> std::result::Result<Vec<NodeId>, PristineError> {
        self.inner.get_rev_deps(id)
    }
    fn get_change_deps(&self, id: NodeId) -> std::result::Result<Vec<Hash>, PristineError> {
        self.dependencies.set(self.dependencies.get() + 1);
        self.inner.get_change_deps(id)
    }
    fn has_change_in_graph(&self, id: NodeId) -> std::result::Result<bool, PristineError> {
        self.inner.has_change_in_graph(id)
    }
    fn change_deps_indexed_count(
        &self,
        id: NodeId,
    ) -> std::result::Result<Option<u64>, PristineError> {
        self.inner.change_deps_indexed_count(id)
    }
}

impl SpanPatchReadTxn for CountedTxn<'_> {
    fn span_change_indexed(&self, change: NodeId) -> Result<bool> {
        self.inner.span_change_indexed(change)
    }
    fn span_root(&self, inode: Inode) -> Result<Option<[u64; 3]>> {
        self.inner.span_root(inode)
    }
    fn span_file_inode(&self, file: &ExternalFile) -> Result<Option<Inode>> {
        *self
            .files
            .borrow_mut()
            .entry((file.owner, file.ordinal))
            .or_default() += 1;
        self.inner.span_file_inode(file)
    }
    fn span_exists(&self, key: &[u8; 32]) -> Result<bool> {
        self.inner.span_exists(key)
    }
    fn span_edits_for_inode(&self, inode: Inode) -> Result<Vec<([u8; 32], [u8; 40])>> {
        self.inner.span_edits_for_inode(inode)
    }
    fn span_edits_at(&self, key: &[u8; 32]) -> Result<Vec<[u8; 40]>> {
        self.inner.span_edits_at(key)
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    pristine: Pristine,
    store: MemoryChangeStore,
    root: Hash,
    edit: Hash,
    inode: Inode,
}

impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let pristine = Pristine::open(directory.path().join("pristine.redb"))?;
        let store = MemoryChangeStore::new();
        let inode = Inode::new(42);
        {
            let mut txn = pristine.write_txn()?;
            txn.open_or_create_view("dev")?;
            txn.commit()?;
        }
        let put = |change: Change, roots| -> Result<Hash> {
            let hash = store.insert_change(change.clone())?;
            let mut txn = pristine.write_txn()?;
            let id = txn.register_change(&hash)?;
            txn.put_change_deps(id, change.dependencies())?;
            index_change(&mut txn, &store, "dev", hash, &change, &roots)?;
            let mut view = txn.get_view("dev")?.context("view missing")?;
            txn.put_change(&mut view, id, &hash)?;
            txn.update_view(&view)?;
            txn.commit()?;
            Ok(hash)
        };
        let mut root = Change::new(
            ChangeHeader::new("root"),
            vec![],
            b"abcdefgh".to_vec(),
            vec![],
        );
        root.hashed.span = Some(SpanPatch::new(
            vec![],
            vec![SpanOp::Root {
                path: "file.txt".into(),
                ordinal: 0,
                start: 0,
                end: 8,
            }],
        ));
        let root = put(root, BTreeMap::from([(0, inode)]))?;
        let mut edit = Change::new(
            ChangeHeader::new("three edits"),
            vec![],
            b"ABC".to_vec(),
            vec![root],
        );
        edit.hashed.span = Some(SpanPatch::new(
            vec![],
            (0..3)
                .map(|index| SpanOp::Edit {
                    file: ExternalFile {
                        owner: root,
                        ordinal: 0,
                    },
                    target: ExternalSpan {
                        owner: root,
                        start: 0,
                        end: 8,
                    },
                    start: index * 2,
                    end: index * 2 + 1,
                    insert_start: index,
                    insert_end: index + 1,
                })
                .collect(),
        ));
        let edit = put(edit, BTreeMap::new())?;
        Ok(Self {
            _directory: directory,
            pristine,
            store,
            root,
            edit,
            inode,
        })
    }
    fn visible(&self, txn: &ReadTxn) -> Result<EffectiveProjectionClosure> {
        let view = txn.get_view("dev")?.context("view missing")?;
        Ok(EffectiveProjectionClosure::try_from_membership(
            txn,
            &txn.view_membership_set(&view)?,
        )?)
    }
}

#[test]
fn context_reuses_verified_dependencies_and_resolutions_with_exact_counters() -> Result<()> {
    let fixture = Fixture::new()?;
    let txn = fixture.pristine.read_txn()?;
    let visible = fixture.visible(&txn)?;
    let counted = CountedTxn::new(&txn);
    let context = SpanRenderContext::new(&counted, &fixture.store, &visible)?;
    let metrics = context.build_metrics();
    assert_eq!(metrics.changes, 2);
    assert_eq!(metrics.edits, 3);
    assert_eq!(metrics.dependency_reads, 2);
    assert_eq!(
        counted.dependencies.get(),
        2,
        "causal proof must not read DEPS again"
    );
    assert_eq!(
        counted
            .internals
            .borrow()
            .values()
            .copied()
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(
        counted.files.borrow().values().copied().collect::<Vec<_>>(),
        [1]
    );
    assert_eq!((metrics.internal_lookups, metrics.internal_hits), (1, 3));
    assert_eq!((metrics.file_lookups, metrics.file_hits), (1, 2));
    let phases = metrics.resolve_changes
        + metrics.load_changes
        + metrics.verify_hashes
        + metrics.validate_dependencies
        + metrics.assemble_edits
        + metrics.causal_proof;
    assert!(phases <= metrics.total);
    assert_eq!(context.render(fixture.inode)?, b"AbBdCfgh");
    let without_matrix =
        SpanRenderContext::with_dependency_proof_budget(&txn, &fixture.store, &visible, 0)?;
    assert!(without_matrix.proof.is_none());
    assert_eq!(without_matrix.render(fixture.inode)?, b"AbBdCfgh");
    Ok(())
}

#[test]
fn cached_resolutions_do_not_cross_transaction_snapshots() -> Result<()> {
    let fixture = Fixture::new()?;
    let txn = fixture.pristine.read_txn()?;
    let visible = fixture.visible(&txn)?;
    let verified = crate::change::VerifiedChangeCache::new(&fixture.store, 4);
    let context = SpanRenderContext::new(&txn, &verified, &visible)?;
    let file = ExternalFile {
        owner: fixture.root,
        ordinal: 0,
    };
    {
        let write = fixture.pristine.write_txn()?;
        {
            let mut files = write.txn.open_table(FILES)?;
            assert!(files.remove(&file_key(&file))?.is_some());
        }
        write.commit()?;
    }
    assert_eq!(
        context.render(fixture.inode)?,
        b"AbBdCfgh",
        "old snapshot stays consistent"
    );
    let next_txn = fixture.pristine.read_txn()?;
    let next_visible = fixture.visible(&next_txn)?;
    let error = SpanRenderContext::new(&next_txn, &verified, &next_visible)
        .err()
        .context("new snapshot accepted missing file mapping")?;
    assert!(
        error.to_string().contains("span file mapping missing"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn reused_verified_payloads_do_not_hide_changed_span_index_rows() -> Result<()> {
    let fixture = Fixture::new()?;
    let verified = crate::change::VerifiedChangeCache::new(&fixture.store, 4);
    let old_txn = fixture.pristine.read_txn()?;
    let old_visible = fixture.visible(&old_txn)?;
    let old_context = SpanRenderContext::new(&old_txn, &verified, &old_visible)?;
    {
        let write = fixture.pristine.write_txn()?;
        let rows = write.span_edits_for_inode(fixture.inode)?;
        assert_eq!(rows.len(), 3);
        {
            let mut edits = write.txn.open_multimap_table(EDITS)?;
            assert!(edits.remove(&rows[0].0, &rows[0].1)?);
        }
        write.commit()?;
    }
    assert_eq!(old_context.render(fixture.inode)?, b"AbBdCfgh");
    let txn = fixture.pristine.read_txn()?;
    let visible = fixture.visible(&txn)?;
    let context = SpanRenderContext::new(&txn, &verified, &visible)?;
    let error = context
        .render(fixture.inode)
        .err()
        .context("changed span index was trusted")?;
    assert!(
        error.to_string().contains("span index differs"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn context_still_checks_dependency_index_and_inverse_id_rows() -> Result<()> {
    let fixture = Fixture::new()?;
    let txn = fixture.pristine.read_txn()?;
    let visible = fixture.visible(&txn)?;
    let mut counted = CountedTxn::new(&txn);
    counted.missing_internal = Some(fixture.root);
    let error = SpanRenderContext::new(&counted, &fixture.store, &visible)
        .err()
        .context("missing inverse ID accepted")?;
    assert!(
        error.to_string().contains("dependency missing"),
        "{error:#}"
    );
    {
        let mut write = fixture.pristine.write_txn()?;
        let edit_id = write.get_internal(&fixture.edit)?.context("edit missing")?;
        write.put_change_deps(edit_id, &[])?;
        write.commit()?;
    }
    let next_txn = fixture.pristine.read_txn()?;
    let next_visible = fixture.visible(&next_txn)?;
    let error = SpanRenderContext::new(&next_txn, &fixture.store, &next_visible)
        .err()
        .context("dependency mismatch accepted")?;
    assert!(
        error.to_string().contains("dependency index differs"),
        "{error:#}"
    );
    Ok(())
}

struct TamperedStore<'a> {
    inner: &'a MemoryChangeStore,
    corrupt: Hash,
    missing: Hash,
}
impl ChangeStore for TamperedStore<'_> {
    type Error = <MemoryChangeStore as ChangeStore>::Error;
    fn get_change(&self, hash: &Hash) -> std::result::Result<Change, Self::Error> {
        if *hash == self.missing {
            return self.inner.get_change(&Hash::of(b"intentionally absent"));
        }
        let mut change = self.inner.get_change(hash)?;
        if *hash == self.corrupt {
            change.hashed.header.message.push_str(" tampered");
        }
        Ok(change)
    }
    fn get_contents<F>(
        &self,
        hash_fn: F,
        node: GraphNode<NodeId>,
        bytes: &mut [u8],
    ) -> std::result::Result<usize, Self::Error>
    where
        F: Fn(NodeId) -> Option<Hash>,
    {
        self.inner.get_contents(hash_fn, node, bytes)
    }
    fn get_contents_ext(
        &self,
        node: GraphNode<Option<Hash>>,
        bytes: &mut [u8],
    ) -> std::result::Result<usize, Self::Error> {
        self.inner.get_contents_ext(node, bytes)
    }
}

#[test]
fn separate_load_and_hash_phases_preserve_dependency_ordered_failures() -> Result<()> {
    let fixture = Fixture::new()?;
    let txn = fixture.pristine.read_txn()?;
    let visible = fixture.visible(&txn)?;
    let store = TamperedStore {
        inner: &fixture.store,
        corrupt: fixture.root,
        missing: fixture.edit,
    };
    let error = SpanRenderContext::new(&txn, &store, &visible)
        .err()
        .context("corrupt hash accepted")?;
    assert!(
        error.to_string().contains("change hash mismatch"),
        "earlier hash error must precede later load error: {error:#}"
    );
    Ok(())
}

#[test]
fn reused_adjacency_proof_is_exact_bounded_and_not_node_id_ordered() -> Result<()> {
    let (root, child, tip) = (NodeId::new(90), NodeId::new(3), NodeId::new(7));
    let order = [root, child, tip];
    let mut dependencies = HashMap::from([(root, vec![]), (child, vec![root]), (tip, vec![child])]);
    let proof =
        CausalProof::build(&order, &dependencies, MAX_PROOF_BYTES)?.context("proof missing")?;
    assert_eq!(proof.contains(tip, root), Some(true));
    assert_eq!(proof.contains(root, tip), Some(false));
    assert_eq!(proof.contains(tip, tip), Some(false));
    assert!(CausalProof::build(&order, &dependencies, 0)?.is_none());
    assert!(CausalProof::build(&order, &dependencies, 1)?.is_none());
    dependencies.insert(child, vec![tip]);
    let error = CausalProof::build(&order, &dependencies, MAX_PROOF_BYTES)
        .err()
        .context("wrong topological order accepted")?;
    assert!(error.to_string().contains("not dependency-first"));
    dependencies.insert(child, vec![NodeId::new(999)]);
    assert!(CausalProof::build(&order, &dependencies, MAX_PROOF_BYTES).is_err());
    Ok(())
}
