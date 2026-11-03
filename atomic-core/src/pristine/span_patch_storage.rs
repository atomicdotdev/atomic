//! Transactional index for immutable authored span ranges.
//!
//! Content stays in change objects. These tables contain file identities,
//! authored range identities, and edits against those ranges. Callers must
//! abort the surrounding transaction on error.

use super::{
    CachedGraphTxn, EffectiveProjectionClosure, GraphTxnT, ReadTxn, ViewMembershipSet, ViewTxnT,
    WriteTxn,
};
use crate::change::span_patch::{ExternalFile, ExternalSpan, SpanOp, SpanPatch};
use crate::change::{Change, ChangeStore};
use crate::types::{Hash, Inode, NodeId};
use anyhow::{bail, ensure, Context, Result};
use rayon::prelude::*;
use redb::{MultimapTableDefinition, ReadableMultimapTable, ReadableTable, TableDefinition};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

const FILES: TableDefinition<&[u8; 36], u64> = TableDefinition::new("span_patch_files_v1");
const ROOTS: TableDefinition<u64, &[u8; 24]> = TableDefinition::new("span_patch_roots_v1");
const SPANS: TableDefinition<&[u8; 32], u8> = TableDefinition::new("span_patch_nodes_v1");
const EDITS: MultimapTableDefinition<&[u8; 32], &[u8; 40]> =
    MultimapTableDefinition::new("span_patch_edits_v1");
const APPLIED: TableDefinition<u64, u8> = TableDefinition::new("span_patch_applied_v1");

fn file_key(file: &ExternalFile) -> [u8; 36] {
    let mut key = [0; 36];
    key[..32].copy_from_slice(file.owner.as_bytes());
    key[32..].copy_from_slice(&file.ordinal.to_be_bytes());
    key
}

fn words<const N: usize>(values: [u64; N]) -> Vec<u8> {
    values.into_iter().flat_map(u64::to_be_bytes).collect()
}

fn decode_words<const N: usize>(bytes: &[u8]) -> [u64; N] {
    std::array::from_fn(|i| u64::from_be_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap()))
}

fn span_key(inode: Inode, owner: NodeId, start: u64, end: u64) -> [u8; 32] {
    words([inode.get(), owner.get(), start, end])
        .try_into()
        .expect("fixed span key size")
}

/// Index a registered change under the same transaction as its namespace and
/// view publication. Root inode bindings must come from the namespace writer.
/// The caller must abort the transaction if this function returns an error.
pub fn index_change<S: ChangeStore>(
    txn: &mut WriteTxn<'_>,
    store: &S,
    view_name: &str,
    hash: Hash,
    change: &Change,
    root_inodes: &BTreeMap<u32, Inode>,
) -> Result<()> {
    ensure!(change.hash()? == hash, "change/hash mismatch");
    let patch = change
        .hashed
        .span
        .as_ref()
        .context("typed span section missing")?;
    patch.validate_content_ranges(change.contents.len().try_into()?)?;
    let id = txn
        .get_internal(&hash)?
        .context("change is not registered")?;
    ensure!(
        txn.get_external(id)? == Some(hash),
        "change identity mismatch"
    );
    let view = txn.get_view(view_name)?.context("view not found")?;
    let visible =
        EffectiveProjectionClosure::try_from_membership(txn, &txn.view_membership_set(&view)?)?;
    let mut dep_ids = Vec::new();
    for dep in change.dependencies() {
        let dep_id = txn.get_internal(dep)?.context("missing dependency")?;
        ensure!(visible.contains(dep_id), "dependency outside target view");
        dep_ids.push(dep_id);
    }
    let dependencies = EffectiveProjectionClosure::try_from_membership(
        txn,
        &ViewMembershipSet::from_ordered(dep_ids),
    )?;
    let already = txn.txn.open_table(APPLIED)?.get(id.get())?.is_some();
    if already {
        return Ok(());
    }

    let mut ordinals = BTreeSet::new();
    let mut inodes = BTreeSet::new();
    for op in &patch.ops {
        match op {
            SpanOp::Root {
                ordinal,
                start,
                end,
                ..
            } => {
                let inode = *root_inodes.get(ordinal).context("missing root inode")?;
                ensure!(ordinals.insert(*ordinal), "duplicate root ordinal");
                ensure!(inodes.insert(inode.get()), "multiple roots for inode");
                ensure!(*start <= *end, "reversed root");
                ensure!(
                    txn.txn.open_table(ROOTS)?.get(inode.get())?.is_none(),
                    "root already exists"
                );
                let file = ExternalFile {
                    owner: hash,
                    ordinal: *ordinal,
                };
                ensure!(
                    txn.txn.open_table(FILES)?.get(&file_key(&file))?.is_none(),
                    "file identity already exists"
                );
            }
            SpanOp::Edit { file, target, .. } => {
                let file_owner = txn
                    .get_internal(&file.owner)?
                    .context("file owner missing")?;
                let target_owner = txn
                    .get_internal(&target.owner)?
                    .context("target owner missing")?;
                ensure!(
                    dependencies.contains(file_owner) && dependencies.contains(target_owner),
                    "span owner not in change dependency closure"
                );
                let inode = txn
                    .span_file_inode(file)?
                    .context("file identity missing")?;
                ensure!(
                    txn.txn
                        .open_table(SPANS)?
                        .get(&span_key(inode, target_owner, target.start, target.end))?
                        .is_some(),
                    "target span missing"
                );
                let source = store.get_change(&target.owner)?;
                ensure!(
                    source.hash()? == target.owner,
                    "target change hash mismatch"
                );
                let source_patch = source
                    .hashed
                    .span
                    .as_ref()
                    .context("target typed span section missing")?;
                let mut matching = 0;
                for source_op in &source_patch.ops {
                    match source_op {
                        SpanOp::Root {
                            ordinal,
                            start,
                            end,
                            ..
                        } if *start == target.start && *end == target.end => {
                            let source_file = ExternalFile {
                                owner: target.owner,
                                ordinal: *ordinal,
                            };
                            if txn.span_file_inode(&source_file)? == Some(inode) {
                                matching += 1;
                            }
                        }
                        SpanOp::Edit {
                            file: source_file,
                            insert_start,
                            insert_end,
                            ..
                        } if *insert_start == target.start
                            && *insert_end == target.end
                            && insert_start < insert_end
                            && txn.span_file_inode(source_file)? == Some(inode) =>
                        {
                            matching += 1;
                        }
                        _ => {}
                    }
                }
                ensure!(
                    matching == 1,
                    "target span index differs from hashed owner change"
                );
                let lo = usize::try_from(target.start).context("target start too large")?;
                let hi = usize::try_from(target.end).context("target end too large")?;
                source
                    .contents
                    .get(lo..hi)
                    .context("target out of bounds")?;
            }
        }
    }
    ensure!(
        ordinals.len() == root_inodes.len(),
        "unexpected root inode mapping"
    );

    for op in &patch.ops {
        match op {
            SpanOp::Root {
                ordinal,
                start,
                end,
                ..
            } => {
                let inode = root_inodes[ordinal];
                let root: [u8; 24] = words([id.get(), *start, *end]).try_into().unwrap();
                txn.txn.open_table(ROOTS)?.insert(inode.get(), &root)?;
                txn.txn.open_table(FILES)?.insert(
                    &file_key(&ExternalFile {
                        owner: hash,
                        ordinal: *ordinal,
                    }),
                    inode.get(),
                )?;
                txn.txn
                    .open_table(SPANS)?
                    .insert(&span_key(inode, id, *start, *end), 1)?;
            }
            SpanOp::Edit {
                file,
                target,
                start,
                end,
                insert_start,
                insert_end,
            } => {
                let inode = txn
                    .span_file_inode(file)?
                    .context("file identity disappeared")?;
                let owner = txn
                    .get_internal(&target.owner)?
                    .context("span owner disappeared")?;
                let value: [u8; 40] = words([*start, *end, id.get(), *insert_start, *insert_end])
                    .try_into()
                    .unwrap();
                txn.txn
                    .open_multimap_table(EDITS)?
                    .insert(&span_key(inode, owner, target.start, target.end), &value)?;
                if insert_start < insert_end {
                    txn.txn
                        .open_table(SPANS)?
                        .insert(&span_key(inode, id, *insert_start, *insert_end), 1)?;
                }
            }
        }
    }
    txn.txn.open_table(APPLIED)?.insert(id.get(), 1)?;
    Ok(())
}

/// Span index reads bound to the transaction used for visibility.
pub trait SpanPatchReadTxn: GraphTxnT {
    /// Whether the change's span operations were indexed transactionally.
    fn span_change_indexed(&self, change: NodeId) -> Result<bool>;
    /// The file's initial authored span, if indexed.
    fn span_root(&self, inode: Inode) -> Result<Option<[u64; 3]>>;
    /// Resolve a portable file identity.
    fn span_file_inode(&self, file: &ExternalFile) -> Result<Option<Inode>>;
    /// Test whether an authored span is indexed.
    fn span_exists(&self, key: &[u8; 32]) -> Result<bool>;
    /// Read all edit rows belonging to an inode.
    fn span_edits_for_inode(&self, inode: Inode) -> Result<Vec<([u8; 32], [u8; 40])>>;
    /// Read edits anchored to one authored span.
    fn span_edits_at(&self, key: &[u8; 32]) -> Result<Vec<[u8; 40]>>;
}

macro_rules! impl_span_read {
    ($txn:ty) => {
        impl SpanPatchReadTxn for $txn {
            fn span_change_indexed(&self, change: NodeId) -> Result<bool> {
                let table = match self.txn.open_table(APPLIED) {
                    Ok(table) => table,
                    Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
                    Err(error) => return Err(error.into()),
                };
                let result = match table.get(change.get())? {
                    Some(value) if value.value() == 1 => Ok(true),
                    Some(_) => bail!("invalid span applied marker"),
                    None => Ok(false),
                };
                result
            }
            fn span_root(&self, inode: Inode) -> Result<Option<[u64; 3]>> {
                let table = match self.txn.open_table(ROOTS) {
                    Ok(table) => table,
                    Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
                    Err(error) => return Err(error.into()),
                };
                let result = table.get(inode.get())?.map(|v| decode_words(v.value()));
                Ok(result)
            }
            fn span_file_inode(&self, file: &ExternalFile) -> Result<Option<Inode>> {
                let table = match self.txn.open_table(FILES) {
                    Ok(table) => table,
                    Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
                    Err(error) => return Err(error.into()),
                };
                let result = table.get(&file_key(file))?.map(|v| Inode::new(v.value()));
                Ok(result)
            }
            fn span_exists(&self, key: &[u8; 32]) -> Result<bool> {
                let table = match self.txn.open_table(SPANS) {
                    Ok(table) => table,
                    Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
                    Err(error) => return Err(error.into()),
                };
                let result = table.get(key)?.is_some();
                Ok(result)
            }
            fn span_edits_for_inode(&self, inode: Inode) -> Result<Vec<([u8; 32], [u8; 40])>> {
                let table = match self.txn.open_multimap_table(EDITS) {
                    Ok(table) => table,
                    Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
                    Err(error) => return Err(error.into()),
                };
                let lo = span_key(inode, NodeId::new(0), 0, 0);
                let hi = span_key(inode, NodeId::new(u64::MAX), u64::MAX, u64::MAX);
                let mut rows = Vec::new();
                for row in table.range::<&[u8; 32]>(&lo..=&hi)? {
                    let (key, values) = row?;
                    for value in values {
                        rows.push((*key.value(), *value?.value()));
                    }
                }
                Ok(rows)
            }
            fn span_edits_at(&self, key: &[u8; 32]) -> Result<Vec<[u8; 40]>> {
                let table = match self.txn.open_multimap_table(EDITS) {
                    Ok(table) => table,
                    Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
                    Err(error) => return Err(error.into()),
                };
                let mut rows = Vec::new();
                for value in table.get(key)? {
                    rows.push(*value?.value());
                }
                Ok(rows)
            }
        }
    };
}
impl_span_read!(ReadTxn);
impl_span_read!(WriteTxn<'_>);

impl SpanPatchReadTxn for CachedGraphTxn<'_> {
    fn span_change_indexed(&self, change: NodeId) -> Result<bool> {
        self.txn.span_change_indexed(change)
    }
    fn span_root(&self, inode: Inode) -> Result<Option<[u64; 3]>> {
        self.txn.span_root(inode)
    }
    fn span_file_inode(&self, file: &ExternalFile) -> Result<Option<Inode>> {
        self.txn.span_file_inode(file)
    }
    fn span_exists(&self, key: &[u8; 32]) -> Result<bool> {
        self.txn.span_exists(key)
    }
    fn span_edits_for_inode(&self, inode: Inode) -> Result<Vec<([u8; 32], [u8; 40])>> {
        self.txn.span_edits_for_inode(inode)
    }
    fn span_edits_at(&self, key: &[u8; 32]) -> Result<Vec<[u8; 40]>> {
        self.txn.span_edits_at(key)
    }
}

impl super::CapabilityTxnT for CachedGraphTxn<'_> {
    fn required_capability_version(&self, id: &str) -> super::PristineResult<Option<u32>> {
        super::CapabilityTxnT::required_capability_version(self.txn, id)
    }
}

struct RenderChange {
    contents: Vec<u8>,
    dependencies: Vec<Hash>,
    span: Option<SpanPatch>,
}

type ExpectedEdits = HashMap<u64, BTreeSet<([u8; 32], [u8; 40])>>;
const MAX_PROOF_BYTES: usize = 4 * 1024 * 1024;

// Transaction-local exact reachability; never use local NodeId ordering as
// causality. When the bounded proof does not fit, use checked graph walks.
struct CausalProof {
    ranks: Box<[(NodeId, usize)]>,
    bits: Box<[u64]>,
    words: usize,
}

impl CausalProof {
    fn build<T: SpanPatchReadTxn>(
        txn: &T,
        visible: &EffectiveProjectionClosure,
        budget: usize,
    ) -> Result<Option<Self>> {
        if budget == 0 {
            return Ok(None);
        }
        let count = visible.len();
        let words = count.div_ceil(64);
        let Some(cells) = count.checked_mul(words) else {
            return Ok(None);
        };
        let Some(bytes) = cells.checked_mul(8).and_then(|size| {
            count
                .checked_mul(std::mem::size_of::<(NodeId, usize)>())
                .and_then(|ranks| size.checked_add(ranks))
        }) else {
            return Ok(None);
        };
        if bytes > budget.min(MAX_PROOF_BYTES) {
            return Ok(None);
        }
        let order: Vec<_> = visible.iter_dependency_first().copied().collect();
        let mut ranks: Vec<_> = order
            .iter()
            .enumerate()
            .map(|(rank, &id)| (id, rank))
            .collect();
        ranks.sort_unstable_by_key(|(id, _)| id.get());
        ensure!(
            ranks.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "duplicate visible change in span causal proof"
        );
        let mut bits = Vec::new();
        if bits.try_reserve_exact(cells).is_err() {
            return Ok(None);
        }
        bits.resize(cells, 0u64);
        for (rank, &id) in order.iter().enumerate() {
            let (earlier, rest) = bits.split_at_mut(rank * words);
            let row = &mut rest[..words];
            row[rank / 64] |= 1u64 << (rank % 64);
            for dep in txn.get_change_deps(id)? {
                let dep_id = txn
                    .get_internal(&dep)?
                    .context("span causal proof dependency missing")?;
                let index = ranks
                    .binary_search_by_key(&dep_id.get(), |(id, _)| id.get())
                    .map_err(|_| anyhow::anyhow!("span causal proof closure is incomplete"))?;
                let dep_rank = ranks[index].1;
                ensure!(dep_rank < rank, "span causal proof is not dependency-first");
                for (word, previous) in row
                    .iter_mut()
                    .zip(&earlier[dep_rank * words..(dep_rank + 1) * words])
                {
                    *word |= *previous;
                }
            }
        }
        Ok(Some(Self {
            ranks: ranks.into_boxed_slice(),
            bits: bits.into_boxed_slice(),
            words,
        }))
    }

    fn contains(&self, descendant: NodeId, ancestor: NodeId) -> Option<bool> {
        let d = self
            .ranks
            .binary_search_by_key(&descendant.get(), |(id, _)| id.get())
            .ok()?;
        let a = self
            .ranks
            .binary_search_by_key(&ancestor.get(), |(id, _)| id.get())
            .ok()?;
        if descendant == ancestor {
            return Some(false);
        }
        let (d, a) = (self.ranks[d].1, self.ranks[a].1);
        Some(self.bits[d * self.words + a / 64] & (1u64 << (a % 64)) != 0)
    }
}

/// Validated span content shared by file renders in one read transaction.
pub struct SpanRenderContext<'a, T: SpanPatchReadTxn> {
    txn: &'a T,
    visible: &'a EffectiveProjectionClosure,
    changes: HashMap<Hash, Arc<RenderChange>>,
    expected: ExpectedEdits,
    proof: Option<CausalProof>,
}

/// One visible portion of an authored span, in rendered file order.
#[derive(Debug, Clone)]
pub struct VisibleSpan {
    /// Byte range in the rendered file.
    pub output: std::ops::Range<usize>,
    /// Immutable authored span containing the bytes.
    pub target: ExternalSpan,
    /// Offset of the visible portion within the target span.
    pub target_start: u64,
}

/// A verified recording baseline and its visible authored ranges.
pub struct SpanRecordState {
    /// Portable file identity.
    pub file: ExternalFile,
    /// Original authored span, including for an empty file.
    pub root: ExternalSpan,
    /// Bytes visible to the recording view.
    pub content: Vec<u8>,
    /// Authored ranges corresponding to the visible bytes.
    pub spans: Vec<VisibleSpan>,
    /// Visible changes that edited this file and must remain causal parents.
    pub dependencies: Vec<Hash>,
}

impl<'a, T: SpanPatchReadTxn> SpanRenderContext<'a, T> {
    /// Verify the closure and content-addressed changes once for this snapshot.
    pub fn new<S: ChangeStore>(
        txn: &'a T,
        store: &S,
        visible: &'a EffectiveProjectionClosure,
    ) -> Result<Self> {
        Self::with_dependency_proof_budget(txn, store, visible, MAX_PROOF_BYTES)
    }

    /// Set the optional causal-proof budget. Zero uses checked graph walks.
    pub fn with_dependency_proof_budget<S: ChangeStore>(
        txn: &'a T,
        store: &S,
        visible: &'a EffectiveProjectionClosure,
        budget: usize,
    ) -> Result<Self> {
        let mut changes = HashMap::new();
        let mut expected = ExpectedEdits::new();
        let ids: Vec<_> = visible.iter_dependency_first().copied().collect();
        // Hashing and reads may run in parallel, but report errors in the
        // dependency order and limit speculative work after a bad object.
        for batch in ids.chunks(64) {
            let hashes: Vec<_> = batch
                .iter()
                .map(|id| {
                    txn.get_external(*id)
                        .map_err(anyhow::Error::from)
                        .and_then(|hash| hash.context("visible hash missing"))
                })
                .collect();
            let loaded: Vec<Result<(Hash, Arc<RenderChange>)>> = hashes
                .into_par_iter()
                .map(|hash| {
                    let hash = hash?;
                    let mut change = store.get_change(&hash)?;
                    ensure!(change.hash()? == hash, "change hash mismatch");
                    Ok((
                        hash,
                        Arc::new(RenderChange {
                            contents: std::mem::take(&mut change.contents),
                            dependencies: std::mem::take(&mut change.hashed.dependencies),
                            span: change.hashed.span.take(),
                        }),
                    ))
                })
                .collect();
            for (id, loaded) in batch.iter().zip(loaded) {
                let (hash, change) = loaded?;
                let mut indexed = txn.get_change_deps(*id)?;
                let mut declared = change.dependencies.clone();
                indexed.sort();
                declared.sort();
                ensure!(
                    indexed == declared,
                    "dependency index differs from hashed change"
                );
                changes.insert(hash, Arc::clone(&change));
                if let Some(patch) = &change.span {
                    for op in &patch.ops {
                        if let SpanOp::Edit {
                            file,
                            target,
                            start,
                            end,
                            insert_start,
                            insert_end,
                        } = op
                        {
                            let inode = txn
                                .span_file_inode(file)?
                                .context("span file mapping missing")?;
                            let owner = txn
                                .get_internal(&target.owner)?
                                .context("span target missing")?;
                            ensure!(visible.contains(owner), "span target outside view");
                            let key = span_key(inode, owner, target.start, target.end);
                            let row: [u8; 40] =
                                words([*start, *end, id.get(), *insert_start, *insert_end])
                                    .try_into()
                                    .unwrap();
                            ensure!(
                                expected.entry(inode.get()).or_default().insert((key, row)),
                                "duplicate hashed span edit"
                            );
                        }
                    }
                }
            }
        }
        let proof = CausalProof::build(txn, visible, budget)?;
        Ok(Self {
            txn,
            visible,
            changes,
            expected,
            proof,
        })
    }

    /// Render an indexed root when its creating change is visible.
    pub fn render_if_visible(&self, inode: Inode) -> Result<Option<Vec<u8>>> {
        let Some(root) = self.txn.span_root(inode)? else {
            return Ok(None);
        };
        if !self.visible.contains(NodeId::new(root[0])) {
            return Ok(None);
        }
        self.render(inode).map(Some)
    }

    /// Obtain byte-to-span mappings under this exact view closure.
    pub fn record_state(&self, inode: Inode) -> Result<Option<SpanRecordState>> {
        let Some(root) = self.txn.span_root(inode)? else {
            return Ok(None);
        };
        if !self.visible.contains(NodeId::new(root[0])) {
            return Ok(None);
        }
        let mut spans = Vec::new();
        let content = self.render_internal(inode, Some(&mut spans))?;
        let owner = self
            .txn
            .get_external(NodeId::new(root[0]))?
            .context("root hash missing")?;
        let change = self
            .changes
            .get(&owner)
            .context("root change outside closure")?;
        let patch = change
            .span
            .as_ref()
            .context("root typed span section missing")?;
        let mut found = None;
        for op in &patch.ops {
            if let SpanOp::Root {
                ordinal,
                start,
                end,
                ..
            } = op
            {
                let file = ExternalFile {
                    owner,
                    ordinal: *ordinal,
                };
                if *start == root[1]
                    && *end == root[2]
                    && self.txn.span_file_inode(&file)? == Some(inode)
                {
                    ensure!(found.is_none(), "ambiguous span root");
                    found = Some(file);
                }
            }
        }
        let file = found.context("root index differs from hashed change")?;
        let mut dependencies = BTreeSet::new();
        dependencies.insert(owner);
        if let Some(edits) = self.expected.get(&inode.get()) {
            for (_, row) in edits {
                let author = NodeId::new(decode_words::<5>(row)[2]);
                dependencies.insert(
                    self.txn
                        .get_external(author)?
                        .context("span edit author missing")?,
                );
            }
        }
        Ok(Some(SpanRecordState {
            file,
            root: ExternalSpan {
                owner,
                start: root[1],
                end: root[2],
            },
            content,
            spans,
            dependencies: dependencies.into_iter().collect(),
        }))
    }

    /// Render the visible span tree for an inode.
    pub fn render(&self, inode: Inode) -> Result<Vec<u8>> {
        self.render_internal(inode, None)
    }

    fn render_internal(
        &self,
        inode: Inode,
        segments: Option<&mut Vec<VisibleSpan>>,
    ) -> Result<Vec<u8>> {
        let root = self.txn.span_root(inode)?.context("span root missing")?;
        let root_id = NodeId::new(root[0]);
        ensure!(self.visible.contains(root_id), "span root outside view");
        let root_hash = self
            .txn
            .get_external(root_id)?
            .context("root hash missing")?;
        let root_change = self
            .changes
            .get(&root_hash)
            .context("root change outside closure")?;
        let patch = root_change
            .span
            .as_ref()
            .context("root typed span section missing")?;
        let mut roots = 0;
        for op in &patch.ops {
            if let SpanOp::Root {
                ordinal,
                start,
                end,
                ..
            } = op
            {
                if *start == root[1]
                    && *end == root[2]
                    && self.txn.span_file_inode(&ExternalFile {
                        owner: root_hash,
                        ordinal: *ordinal,
                    })? == Some(inode)
                {
                    roots += 1;
                }
            }
        }
        ensure!(roots == 1, "root index differs from hashed change");
        let observed: BTreeSet<_> = self
            .txn
            .span_edits_for_inode(inode)?
            .into_iter()
            .filter(|(_, row)| {
                self.visible
                    .contains(NodeId::new(decode_words::<5>(row)[2]))
            })
            .collect();
        ensure!(
            observed == self.expected.get(&inode.get()).cloned().unwrap_or_default(),
            "visible span index differs from hashed changes"
        );
        let mut out = Vec::new();
        self.render_spans(inode, root, &mut out, segments)?;
        Ok(out)
    }

    fn depends_on(&self, author: u64, predecessor: u64) -> Result<bool> {
        if let Some(answer) = self
            .proof
            .as_ref()
            .and_then(|proof| proof.contains(NodeId::new(author), NodeId::new(predecessor)))
        {
            return Ok(answer);
        }
        let ids = self
            .txn
            .get_change_deps(NodeId::new(author))?
            .into_iter()
            .map(|dep| self.txn.get_internal(&dep)?.context("dependency missing"))
            .collect::<Result<Vec<_>>>()?;
        Ok(EffectiveProjectionClosure::try_from_membership(
            self.txn,
            &ViewMembershipSet::from_ordered(ids),
        )?
        .contains(NodeId::new(predecessor)))
    }

    fn frame(&self, inode: Inode, span: [u64; 3]) -> Result<RenderFrame> {
        let [owner, start, end] = span;
        let owner = NodeId::new(owner);
        ensure!(self.visible.contains(owner), "span outside view");
        let key = span_key(inode, owner, start, end);
        ensure!(self.txn.span_exists(&key)?, "span index row missing");
        let hash = self.txn.get_external(owner)?.context("span hash missing")?;
        let change = Arc::clone(
            self.changes
                .get(&hash)
                .context("span change outside closure")?,
        );
        let (start, end) = (usize::try_from(start)?, usize::try_from(end)?);
        change
            .contents
            .get(start..end)
            .context("span outside change content")?;
        let mut edits: Vec<[u64; 5]> = self
            .txn
            .span_edits_at(&key)?
            .into_iter()
            .map(|row| decode_words::<5>(&row))
            .filter(|row| self.visible.contains(NodeId::new(row[2])))
            .collect();
        edits.sort_by_key(|row| (row[0], row[1], row[2]));
        let mut group = 0;
        while group < edits.len() {
            let mut end_group = group + 1;
            while end_group < edits.len()
                && edits[group][0] == edits[group][1]
                && edits[end_group][0] == edits[group][0]
                && edits[end_group][1] == edits[group][1]
            {
                end_group += 1;
            }
            for i in group..end_group {
                for j in i + 1..end_group {
                    let left = edits[i][2];
                    let right = edits[j][2];
                    ensure!(
                        left != right,
                        "ambiguous insertions from one change at the same gap"
                    );
                    if self.depends_on(right, left)? {
                        edits.swap(i, j);
                    } else if !self.depends_on(left, right)? {
                        bail!("unsupported concurrent insertions at the same gap");
                    }
                }
            }
            group = end_group;
        }
        Ok(RenderFrame {
            span,
            hash,
            change,
            start,
            end,
            edits,
            next: 0,
            cursor: 0,
            previous: None,
            covered: None,
        })
    }

    fn render_spans(
        &self,
        inode: Inode,
        root: [u64; 3],
        out: &mut Vec<u8>,
        mut segments: Option<&mut Vec<VisibleSpan>>,
    ) -> Result<()> {
        let mut active = BTreeSet::new();
        active.insert(root);
        let mut stack = vec![self.frame(inode, root)?];
        while let Some(frame) = stack.last_mut() {
            if frame.next == frame.edits.len() {
                append_visible(
                    frame,
                    frame.cursor,
                    frame.end - frame.start,
                    out,
                    segments.as_deref_mut(),
                );
                active.remove(&frame.span);
                stack.pop();
                continue;
            }
            let [lo, hi, author, insert_start, insert_end] = frame.edits[frame.next];
            frame.next += 1;
            let (lo, hi) = (usize::try_from(lo)?, usize::try_from(hi)?);
            ensure!(
                lo <= hi && hi <= frame.end - frame.start,
                "invalid stored range"
            );
            let interior_gap = if lo < frame.cursor && lo == hi {
                match frame.covered {
                    Some((cover_start, cover_end, cover_author))
                        if cover_start < lo && lo < cover_end && cover_author != author =>
                    {
                        self.depends_on(cover_author, author)?
                    }
                    _ => false,
                }
            } else {
                false
            };
            ensure!(
                lo >= frame.cursor || interior_gap,
                "unsupported overlapping or ambiguous edits"
            );
            if let Some((p_lo, p_hi, p_author)) = frame.previous {
                let touches_gap = lo == p_hi && (lo == hi || p_lo == p_hi);
                let distinct_ranges = (p_lo == p_hi) != (lo == hi);
                let causal_touch = if touches_gap && distinct_ranges && author != p_author {
                    self.depends_on(author, p_author)? || self.depends_on(p_author, author)?
                } else if touches_gap && lo == hi && p_lo == p_hi && author != p_author {
                    self.depends_on(p_author, author)?
                } else {
                    false
                };
                if (lo < p_hi && !interior_gap) || (touches_gap && !causal_touch) {
                    bail!("unsupported overlapping or ambiguous edits");
                }
            }
            if !interior_gap {
                append_visible(frame, frame.cursor, lo, out, segments.as_deref_mut());
                frame.cursor = hi;
            }
            if lo < hi {
                frame.covered = Some((lo, hi, author));
            }
            frame.previous = Some((lo, hi, author));
            if insert_start < insert_end {
                let child = [author, insert_start, insert_end];
                ensure!(active.insert(child), "cyclic span references");
                stack.push(self.frame(inode, child)?);
            }
        }
        Ok(())
    }
}

struct RenderFrame {
    span: [u64; 3],
    hash: Hash,
    change: Arc<RenderChange>,
    start: usize,
    end: usize,
    edits: Vec<[u64; 5]>,
    next: usize,
    cursor: usize,
    previous: Option<(usize, usize, u64)>,
    covered: Option<(usize, usize, u64)>,
}

fn append_visible(
    frame: &RenderFrame,
    start: usize,
    end: usize,
    out: &mut Vec<u8>,
    segments: Option<&mut Vec<VisibleSpan>>,
) {
    if start == end {
        return;
    }
    let output_start = out.len();
    out.extend_from_slice(&frame.change.contents[frame.start + start..frame.start + end]);
    if let Some(segments) = segments {
        segments.push(VisibleSpan {
            output: output_start..out.len(),
            target: ExternalSpan {
                owner: frame.hash,
                start: frame.span[1],
                end: frame.span[2],
            },
            target_start: start as u64,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::span_patch::ExternalSpan;
    use crate::change::{ChangeHeader, MemoryChangeStore};
    use crate::pristine::{MutTxnT, Pristine, ViewScope};

    fn add_change(
        repo: &Pristine,
        store: &MemoryChangeStore,
        view_name: &str,
        change: Change,
        roots: BTreeMap<u32, Inode>,
    ) -> Result<Hash> {
        let hash = store.insert_change(change.clone())?;
        let mut txn = repo.write_txn_immediate()?;
        let id = txn.register_change(&hash)?;
        txn.put_change_deps(id, change.dependencies())?;
        index_change(&mut txn, store, view_name, hash, &change, &roots)?;
        let mut view = txn.get_view(view_name)?.context("view missing")?;
        txn.put_change(&mut view, id, &hash)?;
        txn.update_view(&view)?;
        txn.commit()?;
        Ok(hash)
    }

    fn render(
        repo: &Pristine,
        store: &MemoryChangeStore,
        view_name: &str,
        inode: Inode,
    ) -> Result<Vec<u8>> {
        let txn = repo.read_txn()?;
        let view = txn.get_view(view_name)?.context("view missing")?;
        let visible = EffectiveProjectionClosure::try_from_membership(
            &txn,
            &txn.view_membership_set(&view)?,
        )?;
        SpanRenderContext::new(&txn, store, &visible)?.render(inode)
    }

    #[test]
    fn independent_span_edits_render_in_both_insert_orders() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("pristine.redb");
        let repo = Pristine::open(&path)?;
        let store = MemoryChangeStore::new();
        let inode = Inode::new(42);
        {
            let mut txn = repo.write_txn()?;
            let base = txn.open_or_create_view("dev")?;
            for name in ["left", "right", "both", "reverse"] {
                txn.create_view(name, ViewScope::Draft, Some(base.id))?;
            }
            txn.commit()?;
        }
        let initial = b"alpha\nbeta\ngamma\nomega";
        let mut root = Change::new(ChangeHeader::new("root"), vec![], initial.to_vec(), vec![]);
        root.hashed.span = Some(SpanPatch::new(
            vec![],
            vec![SpanOp::Root {
                path: "file.txt".into(),
                ordinal: 0,
                start: 0,
                end: initial.len() as u64,
            }],
        ));
        let root_hash = add_change(&repo, &store, "dev", root, BTreeMap::from([(0, inode)]))?;
        let file = ExternalFile {
            owner: root_hash,
            ordinal: 0,
        };
        let span = ExternalSpan {
            owner: root_hash,
            start: 0,
            end: initial.len() as u64,
        };
        let edit = |name: &str, bytes: &[u8], start, end| {
            let mut change = Change::new(
                ChangeHeader::new(name),
                vec![],
                bytes.to_vec(),
                vec![root_hash],
            );
            change.hashed.span = Some(SpanPatch::new(
                vec![],
                vec![SpanOp::Edit {
                    file: file.clone(),
                    target: span.clone(),
                    start,
                    end,
                    insert_start: 0,
                    insert_end: bytes.len() as u64,
                }],
            ));
            change
        };
        let left = edit("left", b"ALPHA", 0, 5);
        let right = edit("right", b"GAMMA", 11, 16);
        let left_hash = add_change(&repo, &store, "left", left.clone(), BTreeMap::new())?;
        let right_hash = add_change(&repo, &store, "right", right.clone(), BTreeMap::new())?;
        assert_eq!(
            render(&repo, &store, "left", inode)?,
            b"ALPHA\nbeta\ngamma\nomega"
        );
        assert_eq!(
            render(&repo, &store, "right", inode)?,
            b"alpha\nbeta\nGAMMA\nomega"
        );
        let expected = b"ALPHA\nbeta\nGAMMA\nomega";
        for (view, order) in [("both", [&left, &right]), ("reverse", [&right, &left])] {
            for change in order {
                add_change(&repo, &store, view, change.clone(), BTreeMap::new())?;
            }
            assert_eq!(render(&repo, &store, view, inode)?, expected);
            let txn = repo.read_txn()?;
            let state = txn.get_view(view)?.unwrap();
            let visible = EffectiveProjectionClosure::try_from_membership(
                &txn,
                &txn.view_membership_set(&state)?,
            )?;
            assert_eq!(
                SpanRenderContext::with_dependency_proof_budget(&txn, &store, &visible, 0)?
                    .render(inode)?,
                expected
            );
        }
        let txn = repo.read_txn()?;
        assert_eq!(
            txn.span_edits_for_inode(inode)?.len(),
            2,
            "view insertion must not duplicate index rows"
        );
        assert_ne!(left_hash, right_hash);
        drop(txn);
        drop(repo);
        let reopened = Pristine::open(path)?;
        assert_eq!(render(&reopened, &store, "both", inode)?, expected);
        Ok(())
    }

    #[test]
    fn overlapping_span_edits_are_not_silently_rendered() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo = Pristine::open(dir.path().join("pristine.redb"))?;
        let store = MemoryChangeStore::new();
        let inode = Inode::new(42);
        {
            let mut txn = repo.write_txn()?;
            let base = txn.open_or_create_view("dev")?;
            for name in ["left", "right", "both"] {
                txn.create_view(name, ViewScope::Draft, Some(base.id))?;
            }
            txn.commit()?;
        }
        let mut root = Change::new(
            ChangeHeader::new("root"),
            vec![],
            b"alpha\n".to_vec(),
            vec![],
        );
        root.hashed.span = Some(SpanPatch::new(
            vec![],
            vec![SpanOp::Root {
                path: "a".into(),
                ordinal: 0,
                start: 0,
                end: 6,
            }],
        ));
        let root_hash = add_change(&repo, &store, "dev", root, BTreeMap::from([(0, inode)]))?;
        let make_edit = |name: &str, content: &[u8]| {
            let mut change = Change::new(
                ChangeHeader::new(name),
                vec![],
                content.to_vec(),
                vec![root_hash],
            );
            change.hashed.span = Some(SpanPatch::new(
                vec![],
                vec![SpanOp::Edit {
                    file: ExternalFile {
                        owner: root_hash,
                        ordinal: 0,
                    },
                    target: ExternalSpan {
                        owner: root_hash,
                        start: 0,
                        end: 6,
                    },
                    start: 0,
                    end: 5,
                    insert_start: 0,
                    insert_end: content.len() as u64,
                }],
            ));
            change
        };
        let left = make_edit("left", b"left");
        let right = make_edit("right", b"right");
        add_change(&repo, &store, "left", left.clone(), BTreeMap::new())?;
        add_change(&repo, &store, "right", right.clone(), BTreeMap::new())?;
        add_change(&repo, &store, "both", left, BTreeMap::new())?;
        add_change(&repo, &store, "both", right, BTreeMap::new())?;
        assert_eq!(render(&repo, &store, "left", inode)?, b"left\n");
        assert_eq!(render(&repo, &store, "right", inode)?, b"right\n");
        assert!(render(&repo, &store, "both", inode)
            .unwrap_err()
            .to_string()
            .contains("overlapping"));
        Ok(())
    }
}
