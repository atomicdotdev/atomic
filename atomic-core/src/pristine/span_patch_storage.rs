//! Transactional index for immutable authored span ranges.
//!
//! Content stays in change objects. These tables contain file identities,
//! authored range identities, and edits against those ranges. Callers must
//! abort the surrounding transaction on error.

use super::{
    CachedGraphTxn, EffectiveProjectionClosure, GraphTxnT, ReadTxn, ViewMembershipSet, ViewTxnT,
    WriteTxn,
};
pub use crate::change::span_patch::SpanProjectionId;
use crate::change::span_patch::{ExternalFile, ExternalSpan, SpanOp, SpanPatch};
use crate::change::{Change, ChangeStore, VerifiedChange};
use crate::types::{Hash, Inode, NodeId};
use anyhow::{bail, ensure, Context, Result};
use rayon::prelude::*;
use redb::{MultimapTableDefinition, ReadableMultimapTable, ReadableTable, TableDefinition};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) const FILES: TableDefinition<&[u8; 36], u64> =
    TableDefinition::new("span_patch_files_v1");
pub(super) const ROOTS: TableDefinition<u64, &[u8; 24]> =
    TableDefinition::new("span_patch_roots_v1");
pub(super) const SPANS: TableDefinition<&[u8; 32], u8> =
    TableDefinition::new("span_patch_nodes_v1");
pub(super) const EDITS: MultimapTableDefinition<&[u8; 32], &[u8; 40]> =
    MultimapTableDefinition::new("span_patch_edits_v1");
pub(super) const APPLIED: TableDefinition<u64, u8> = TableDefinition::new("span_patch_applied_v1");

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
    validate_projected_semantics(txn, store, change, &dependencies)?;
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

/// Validate span-projected FileOps against the exact dependency closure before
/// writing any span index entries. Generic CRDT application cannot apply these
/// operations; their semantic state is derived from the verified span graph.
pub fn validate_projected_semantics<S: ChangeStore, T: SpanPatchReadTxn + super::CapabilityTxnT>(
    txn: &T,
    store: &S,
    change: &Change,
    dependencies: &EffectiveProjectionClosure,
) -> Result<()> {
    use super::SPAN_PROJECTED_SEMANTICS_CAPABILITY;
    use crate::crdt::{TrunkId, TrunkOp};
    let projected: Vec<_> = change
        .file_ops()
        .iter()
        .filter_map(|ops| match ops.trunk_op() {
            Some(TrunkOp::ProjectedSpanEdit { file, baseline }) => Some((ops, file, baseline)),
            _ => None,
        })
        .collect();
    if projected.is_empty() {
        return Ok(());
    }
    ensure!(
        txn.required_capability_version(SPAN_PROJECTED_SEMANTICS_CAPABILITY.id())?
            == Some(SPAN_PROJECTED_SEMANTICS_CAPABILITY.minimum_version()),
        "projected span semantic capability is missing or unsupported"
    );
    let patch = change
        .hashed
        .span
        .as_ref()
        .context("projected semantic edit requires a span patch")?;
    let context = SpanRenderContext::new(txn, store, dependencies)?;
    let mut files = BTreeSet::new();
    for (ops, file, baseline) in projected {
        ensure!(
            ops.trunk_id() == TrunkId::ROOT && ops.line_ops().is_empty(),
            "projected semantic edit cannot contain CRDT identities or line operations"
        );
        ensure!(
            files.insert(file_key(file)),
            "duplicate projected semantic edit for file"
        );
        ensure!(
            change
                .file_ops()
                .iter()
                .filter(|candidate| candidate.path() == ops.path())
                .count()
                == 1,
            "projected semantic path has multiple FileOps"
        );
        ensure!(
            patch
                .ops
                .iter()
                .any(|op| matches!(op, SpanOp::Edit { file: edited, .. } if edited == file)),
            "projected semantic edit has no matching span edit"
        );
        let inode = txn
            .span_file_inode(file)?
            .context("projected semantic file missing")?;
        let state = context
            .record_state(inode)?
            .context("projected semantic file is outside the dependency closure")?;
        ensure!(
            state.file == *file,
            "projected semantic file identity differs from span root"
        );
        ensure!(
            state.semantic_baseline_id()? == *baseline,
            "projected semantic baseline differs from dependency closure"
        );
    }
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

/// Wall-clock construction phases and transaction-local lookup counts.
///
/// Loading includes the store's checked identity verification. The separate
/// `verify_hashes` stage binds its immutable witness to the requested hash and
/// extracts render data; it no longer recompresses filesystem-backed changes.
/// Times are bounded parallel-batch wall time, not summed worker CPU time.
/// `total` also includes allocation and loop overhead.
/// These metrics describe one context, not an entire materialization operation.
#[derive(Clone, Debug, Default)]
pub struct SpanContextBuildMetrics {
    /// Version 2 uses checked loading plus witness binding; the original phase
    /// instrumentation (version 1) reserialized changes in `verify_hashes`.
    pub pipeline_version: u8,
    pub resolve_changes: Duration,
    pub load_changes: Duration,
    pub verify_hashes: Duration,
    pub validate_dependencies: Duration,
    pub assemble_edits: Duration,
    pub causal_proof: Duration,
    pub total: Duration,
    pub changes: usize,
    pub edits: usize,
    pub dependency_reads: usize,
    pub internal_lookups: usize,
    pub internal_hits: usize,
    pub file_lookups: usize,
    pub file_hits: usize,
}

/// Never seed inverse IDs from EXTERNAL: INTERNAL must still be checked at
/// least once per referenced hash in this exact borrowed transaction.
#[derive(Default)]
struct SpanBuildLookups {
    internal: HashMap<Hash, NodeId>,
    files: HashMap<(Hash, u32), Inode>,
}

impl SpanBuildLookups {
    fn internal<T: SpanPatchReadTxn>(
        &mut self,
        txn: &T,
        hash: &Hash,
        missing: &'static str,
        metrics: &mut SpanContextBuildMetrics,
    ) -> Result<NodeId> {
        if let Some(id) = self.internal.get(hash) {
            metrics.internal_hits += 1;
            return Ok(*id);
        }
        metrics.internal_lookups += 1;
        let id = txn.get_internal(hash)?.context(missing)?;
        self.internal.insert(*hash, id);
        Ok(id)
    }

    fn file<T: SpanPatchReadTxn>(
        &mut self,
        txn: &T,
        file: &ExternalFile,
        metrics: &mut SpanContextBuildMetrics,
    ) -> Result<Inode> {
        let key = (file.owner, file.ordinal);
        if let Some(inode) = self.files.get(&key) {
            metrics.file_hits += 1;
            return Ok(*inode);
        }
        metrics.file_lookups += 1;
        let inode = txn
            .span_file_inode(file)?
            .context("span file mapping missing")?;
        self.files.insert(key, inode);
        Ok(inode)
    }
}

// Transaction-local exact reachability; never use local NodeId ordering as
// causality. When the bounded proof does not fit, use checked graph walks.
struct CausalProof {
    ranks: Box<[(NodeId, usize)]>,
    bits: Box<[u64]>,
    words: usize,
}

impl CausalProof {
    fn build(
        order: &[NodeId],
        dependencies: &HashMap<NodeId, Vec<NodeId>>,
        budget: usize,
    ) -> Result<Option<Self>> {
        if budget == 0 {
            return Ok(None);
        }
        let count = order.len();
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
            for dep_id in dependencies
                .get(&id)
                .context("span causal proof dependencies missing")?
            {
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
    metrics: SpanContextBuildMetrics,
}

/// A bounded render never retains more than the requested number of bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum BoundedSpanContent {
    Complete(Vec<u8>),
    Oversized { retained_bytes: usize },
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

fn projection_id(
    file: &ExternalFile,
    domain: &[u8],
    spans: &[VisibleSpan],
) -> Result<SpanProjectionId> {
    let mut normalized: Vec<(&ExternalSpan, u64, u64)> = Vec::new();
    for span in spans {
        let length = u64::try_from(span.output.len())?;
        ensure!(length != 0, "empty projected authored fragment");
        let end = span
            .target_start
            .checked_add(length)
            .context("projected fragment overflow")?;
        if let Some((target, _, previous_end)) = normalized.last_mut() {
            if *target == &span.target && *previous_end == span.target_start {
                *previous_end = end;
                continue;
            }
        }
        normalized.push((&span.target, span.target_start, end));
    }
    ensure!(
        !normalized.is_empty(),
        "projected semantic identity has no authored bytes"
    );
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"atomic.span.semantic.projection.v1\0");
    bytes.extend_from_slice(domain);
    bytes.push(0);
    bytes.extend_from_slice(file.owner.as_bytes());
    bytes.extend_from_slice(&file.ordinal.to_be_bytes());
    bytes.extend_from_slice(&u64::try_from(normalized.len())?.to_be_bytes());
    for (target, start, end) in normalized {
        bytes.extend_from_slice(target.owner.as_bytes());
        bytes.extend_from_slice(&target.start.to_be_bytes());
        bytes.extend_from_slice(&target.end.to_be_bytes());
        bytes.extend_from_slice(&start.to_be_bytes());
        bytes.extend_from_slice(&end.to_be_bytes());
    }
    Ok(SpanProjectionId(Hash::of(&bytes)))
}

/// One token interpreted from a span-backed file, with its exact authored
/// byte fragments. A token may cross changes and therefore has no fabricated
/// single author or CRDT identity.
#[derive(Debug, Clone)]
pub struct SpanSemanticToken {
    /// Identity derived from its authored byte fragments, independent of the
    /// repository-local node IDs and rendered file position.
    pub id: SpanProjectionId,
    /// Token kind produced by the normal code tokenizer.
    pub kind: crate::diff::token::TokenKind,
    /// Byte range in the rendered file.
    pub output: std::ops::Range<usize>,
    /// Authored spans covering the token in byte order.
    pub authors: Vec<VisibleSpan>,
}

/// One rendered line and its lossless token-level interpretation.
#[derive(Debug, Clone)]
pub struct SpanSemanticLine {
    /// Identity derived from the line's authored byte fragments.
    pub id: SpanProjectionId,
    /// Byte range in the rendered file, including any line terminator.
    pub output: std::ops::Range<usize>,
    /// Tokens in source order, including the line terminator when present.
    pub tokens: Vec<SpanSemanticToken>,
}

/// Semantic interpretation of one span-backed file on one view closure.
/// The identity binds the ordered projected lines and the portable file
/// identity; it does not claim authorship for the snapshot as a whole.
#[derive(Debug, Clone)]
pub struct SpanSemanticSnapshot {
    /// Portable identity of the file represented by this snapshot.
    pub file: ExternalFile,
    /// Content-addressed semantic baseline identity.
    pub id: SpanProjectionId,
    /// Projected lines in file order.
    pub lines: Vec<SpanSemanticLine>,
}

impl SpanRecordState {
    /// Identity of the semantic baseline. Text includes line/token structure;
    /// opaque bytes use a separate domain and retain their authored ranges
    /// without inventing text tokens.
    pub fn semantic_baseline_id(&self) -> Result<SpanProjectionId> {
        if self.content.contains(&0) || std::str::from_utf8(&self.content).is_err() {
            return projection_id(
                &self.file,
                b"opaque",
                &self.authored_ranges(0..self.content.len())?,
            );
        }
        Ok(self.semantic_snapshot()?.id)
    }

    /// Derive a portable semantic baseline for validating an edit on a
    /// receiving repository. Identical visible authored spans produce the
    /// same identity even if local node IDs or inode numbers differ. The v1
    /// tokenizer, token-kind tags and identity encoding are wire contracts:
    /// changes to them require a new projected-semantics capability version.
    pub fn semantic_snapshot(&self) -> Result<SpanSemanticSnapshot> {
        let lines = self.semantic_lines()?;
        let mut seen = BTreeSet::new();
        let mut seen_tokens = BTreeSet::new();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"atomic.span.semantic.snapshot.v1\0");
        bytes.extend_from_slice(self.file.owner.as_bytes());
        bytes.extend_from_slice(&self.file.ordinal.to_be_bytes());
        bytes.extend_from_slice(&u64::try_from(lines.len())?.to_be_bytes());
        for line in &lines {
            ensure!(seen.insert(line.id.0), "duplicate projected line identity");
            bytes.extend_from_slice(line.id.0.as_bytes());
            bytes.extend_from_slice(&u64::try_from(line.tokens.len())?.to_be_bytes());
            for token in &line.tokens {
                ensure!(
                    seen_tokens.insert(token.id.0),
                    "duplicate projected token identity"
                );
                use crate::diff::token::TokenKind;
                let kind = match token.kind {
                    TokenKind::Word => 0u8,
                    TokenKind::Whitespace => 1,
                    TokenKind::Operator => 2,
                    TokenKind::Punctuation => 3,
                    TokenKind::String => 4,
                    TokenKind::Number => 5,
                    TokenKind::Comment => 6,
                    TokenKind::Newline => 7,
                    TokenKind::Other => 8,
                };
                bytes.extend_from_slice(token.id.0.as_bytes());
                bytes.push(kind);
            }
        }
        Ok(SpanSemanticSnapshot {
            file: self.file.clone(),
            id: SpanProjectionId(Hash::of(&bytes)),
            lines,
        })
    }

    /// Derive a lossless line/token view without creating CRDT rows or
    /// assigning unchanged bytes to the current editor. Binary or invalid UTF-8
    /// data is rejected; callers must handle it as opaque content.
    pub fn semantic_lines(&self) -> Result<Vec<SpanSemanticLine>> {
        use crate::diff::token::Tokenizer;
        std::str::from_utf8(&self.content).context("span semantic content is not UTF-8")?;
        ensure!(
            !self.content.contains(&0),
            "span semantic content is binary"
        );
        // Validate the complete map once, then walk it monotonically as
        // tokens are emitted. Rescanning all spans per token is quadratic on
        // files with many independently authored fragments.
        let mut covered = 0usize;
        for span in &self.spans {
            ensure!(
                span.output.start == covered
                    && span.output.end > covered
                    && span.output.end <= self.content.len(),
                "authored spans do not uniquely cover semantic content"
            );
            let length = u64::try_from(span.output.end - span.output.start)?;
            let target_len = span
                .target
                .end
                .checked_sub(span.target.start)
                .context("invalid authored span range")?;
            ensure!(
                span.target_start
                    .checked_add(length)
                    .is_some_and(|end| end <= target_len),
                "visible bytes exceed the authored span"
            );
            covered = span.output.end;
        }
        ensure!(
            covered == self.content.len(),
            "authored spans do not cover semantic content"
        );
        let mut result = Vec::new();
        let mut line_start = 0usize;
        let mut span_index = 0usize;
        for line in crate::diff::Line::from_bytes(&self.content) {
            let bytes = line.content();
            let line_end = line_start
                .checked_add(bytes.len())
                .context("semantic line offset overflow")?;
            ensure!(
                self.content.get(line_start..line_end) == Some(bytes),
                "semantic lines do not cover the span content"
            );
            let mut token_end = 0usize;
            let mut tokens = Vec::new();
            let mut line_authors = Vec::new();
            for token in Tokenizer::new(bytes) {
                ensure!(
                    token.offset() == token_end,
                    "semantic tokens do not uniquely cover the line"
                );
                token_end = token_end
                    .checked_add(token.content().len())
                    .context("semantic token offset overflow")?;
                ensure!(
                    bytes.get(token.offset()..token_end) == Some(token.content()),
                    "semantic token bytes differ from the line"
                );
                let start = line_start
                    .checked_add(token.offset())
                    .context("semantic token offset overflow")?;
                let end = line_start
                    .checked_add(token_end)
                    .context("semantic token offset overflow")?;
                let mut authors = Vec::new();
                let mut cursor = start;
                while cursor < end {
                    let span = self
                        .spans
                        .get(span_index)
                        .context("authored semantic span missing")?;
                    ensure!(
                        span.output.start <= cursor && cursor < span.output.end,
                        "authored semantic span order changed"
                    );
                    let hi = end.min(span.output.end);
                    let target_start = span
                        .target_start
                        .checked_add(u64::try_from(cursor - span.output.start)?)
                        .context("authored span offset overflow")?;
                    authors.push(VisibleSpan {
                        output: cursor..hi,
                        target: span.target.clone(),
                        target_start,
                    });
                    cursor = hi;
                    if cursor == span.output.end {
                        span_index += 1;
                    }
                }
                line_authors.extend(authors.iter().cloned());
                tokens.push(SpanSemanticToken {
                    id: projection_id(&self.file, b"token", &authors)?,
                    kind: token.kind(),
                    output: start..end,
                    authors,
                });
            }
            ensure!(
                token_end == bytes.len(),
                "semantic tokens do not cover the line"
            );
            result.push(SpanSemanticLine {
                id: projection_id(&self.file, b"line", &line_authors)?,
                output: line_start..line_end,
                tokens,
            });
            line_start = line_end;
        }
        ensure!(
            line_start == self.content.len(),
            "semantic lines do not cover the span content"
        );
        Ok(result)
    }

    /// Resolve an insertion boundary to a verified authored span and offset.
    /// Boundaries between spans use the preceding byte; empty files use their
    /// original root. The offset is relative to the returned span.
    pub fn authored_anchor(&self, position: usize) -> Result<(ExternalSpan, u64)> {
        ensure!(
            position <= self.content.len(),
            "rendered span anchor is out of bounds"
        );
        if self.content.is_empty() {
            ensure!(
                self.root.start <= self.root.end,
                "invalid authored root range"
            );
            return Ok((self.root.clone(), 0));
        }
        let (range, after) = if position == 0 {
            (0..1, false)
        } else {
            (position - 1..position, true)
        };
        let segment = self
            .authored_ranges(range)?
            .into_iter()
            .next()
            .context("authored anchor missing")?;
        let offset = segment.target_start + u64::from(after);
        Ok((segment.target, offset))
    }

    /// Return the exact authored ranges covering a rendered byte interval.
    /// A line or token can cross multiple spans; each result keeps its actual
    /// change owner rather than assigning the entire interval to one author.
    pub fn authored_ranges(&self, output: std::ops::Range<usize>) -> Result<Vec<VisibleSpan>> {
        ensure!(
            output.start <= output.end && output.end <= self.content.len(),
            "rendered span interval is out of bounds"
        );
        if output.is_empty() {
            return Ok(Vec::new());
        }
        let mut cursor = output.start;
        let mut ranges = Vec::new();
        for segment in &self.spans {
            ensure!(
                segment.output.start <= segment.output.end
                    && segment.output.end <= self.content.len(),
                "invalid visible span output range"
            );
            let lo = output.start.max(segment.output.start);
            let hi = output.end.min(segment.output.end);
            if lo >= hi {
                continue;
            }
            ensure!(
                lo == cursor,
                "authored spans do not uniquely cover the requested interval"
            );
            let offset = segment
                .target_start
                .checked_add((lo - segment.output.start) as u64)
                .context("authored span offset overflow")?;
            let end = offset
                .checked_add((hi - lo) as u64)
                .context("authored span length overflow")?;
            let target_len = segment
                .target
                .end
                .checked_sub(segment.target.start)
                .context("invalid authored span range")?;
            ensure!(end <= target_len, "visible bytes exceed the authored span");
            ranges.push(VisibleSpan {
                output: lo..hi,
                target: segment.target.clone(),
                target_start: offset,
            });
            cursor = hi;
        }
        ensure!(
            cursor == output.end,
            "authored spans do not cover the requested interval"
        );
        Ok(ranges)
    }
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
        let trace = std::env::var_os("ATOMIC_TRACE_SPAN_RENDER").is_some();
        let started = Instant::now();
        let mut metrics = SpanContextBuildMetrics {
            pipeline_version: 2,
            ..Default::default()
        };
        let mut changes = HashMap::new();
        let mut expected = ExpectedEdits::new();
        let ids: Vec<_> = visible.iter_dependency_first().copied().collect();
        metrics.changes = ids.len();
        let mut dependencies = HashMap::with_capacity(ids.len());
        let mut lookups = SpanBuildLookups::default();
        // Hashing and reads may run in parallel, but report errors in the
        // dependency order and limit speculative work after a bad object.
        for batch in ids.chunks(64) {
            let phase = Instant::now();
            let hashes: Vec<_> = batch
                .iter()
                .map(|id| {
                    txn.get_external(*id)
                        .map_err(anyhow::Error::from)
                        .and_then(|hash| hash.context("visible hash missing"))
                })
                .collect();
            metrics.resolve_changes += phase.elapsed();
            let phase = Instant::now();
            let loaded: Vec<Result<(Hash, VerifiedChange)>> = hashes
                .into_par_iter()
                .map(|hash| {
                    let hash = hash?;
                    Ok((hash, store.get_verified_change(&hash)?))
                })
                .collect();
            metrics.load_changes += phase.elapsed();
            let phase = Instant::now();
            let loaded: Vec<Result<(Hash, Arc<RenderChange>)>> = loaded
                .into_par_iter()
                .map(|loaded| {
                    let (hash, verified) = loaded?;
                    verified.check_hash(&hash)?;
                    let change = verified.change();
                    Ok((
                        hash,
                        Arc::new(RenderChange {
                            contents: change.contents.clone(),
                            dependencies: change.hashed.dependencies.clone(),
                            span: change.hashed.span.clone(),
                        }),
                    ))
                })
                .collect();
            metrics.verify_hashes += phase.elapsed();
            for (id, loaded) in batch.iter().zip(loaded) {
                let (hash, change) = loaded?;
                let phase = Instant::now();
                let mut indexed = txn.get_change_deps(*id)?;
                metrics.dependency_reads += 1;
                let mut declared = change.dependencies.clone();
                indexed.sort();
                declared.sort();
                ensure!(
                    indexed == declared,
                    "dependency index differs from hashed change"
                );
                let resolved = indexed
                    .iter()
                    .map(|dep| {
                        let dep_id = lookups.internal(
                            txn,
                            dep,
                            "span causal proof dependency missing",
                            &mut metrics,
                        )?;
                        ensure!(
                            visible.contains(dep_id),
                            "span causal proof closure is incomplete"
                        );
                        Ok(dep_id)
                    })
                    .collect::<Result<Vec<_>>>()?;
                dependencies.insert(*id, resolved);
                metrics.validate_dependencies += phase.elapsed();
                changes.insert(hash, Arc::clone(&change));
                let phase = Instant::now();
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
                            let inode = lookups.file(txn, file, &mut metrics)?;
                            let owner = lookups.internal(
                                txn,
                                &target.owner,
                                "span target missing",
                                &mut metrics,
                            )?;
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
                            metrics.edits += 1;
                        }
                    }
                }
                metrics.assemble_edits += phase.elapsed();
            }
        }
        let phase = Instant::now();
        let proof = CausalProof::build(&ids, &dependencies, budget)?;
        metrics.causal_proof = phase.elapsed();
        metrics.total = started.elapsed();
        if trace {
            eprintln!(
                "[span context] changes={} elapsed={:?} metrics={:?}",
                ids.len(),
                metrics.total,
                metrics,
            );
        }
        Ok(Self {
            txn,
            visible,
            changes,
            expected,
            proof,
            metrics,
        })
    }

    /// Construction costs for this context's exact transaction and visibility.
    pub fn build_metrics(&self) -> &SpanContextBuildMetrics {
        &self.metrics
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
        let (content, oversized) = self.render_internal(inode, Some(&mut spans), usize::MAX)?;
        ensure!(!oversized, "span render exceeds addressable memory");
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
        let (content, oversized) = self.render_internal(inode, None, usize::MAX)?;
        ensure!(!oversized, "span render exceeds addressable memory");
        Ok(content)
    }

    /// Render at most `max_bytes`, validating the complete span tree even when
    /// the visible file is too large to retain.
    pub fn render_bounded(&self, inode: Inode, max_bytes: usize) -> Result<BoundedSpanContent> {
        let (content, oversized) = self.render_internal(inode, None, max_bytes)?;
        if oversized {
            Ok(BoundedSpanContent::Oversized {
                retained_bytes: content.len(),
            })
        } else {
            Ok(BoundedSpanContent::Complete(content))
        }
    }

    fn render_internal(
        &self,
        inode: Inode,
        segments: Option<&mut Vec<VisibleSpan>>,
        max_bytes: usize,
    ) -> Result<(Vec<u8>, bool)> {
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
        let mut out = RenderOutput {
            bytes: Vec::new(),
            limit: max_bytes,
            oversized: false,
            segments,
        };
        self.render_spans(inode, root, &mut out)?;
        Ok((out.bytes, out.oversized))
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

    fn render_spans(&self, inode: Inode, root: [u64; 3], out: &mut RenderOutput<'_>) -> Result<()> {
        let mut active = BTreeSet::new();
        active.insert(root);
        let mut stack = vec![self.frame(inode, root)?];
        while let Some(frame) = stack.last_mut() {
            if frame.next == frame.edits.len() {
                append_visible(frame, frame.cursor, frame.end - frame.start, out);
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
                append_visible(frame, frame.cursor, lo, out);
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

struct RenderOutput<'a> {
    bytes: Vec<u8>,
    limit: usize,
    oversized: bool,
    segments: Option<&'a mut Vec<VisibleSpan>>,
}

fn append_visible(frame: &RenderFrame, start: usize, end: usize, out: &mut RenderOutput<'_>) {
    if start == end || out.oversized {
        return;
    }
    let content = &frame.change.contents[frame.start + start..frame.start + end];
    if content.len() > out.limit.saturating_sub(out.bytes.len()) {
        out.oversized = true;
        return;
    }
    let output_start = out.bytes.len();
    out.bytes.extend_from_slice(content);
    if let Some(segments) = out.segments.as_deref_mut() {
        segments.push(VisibleSpan {
            output: output_start..out.bytes.len(),
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
mod build_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::span_patch::ExternalSpan;
    use crate::change::{ChangeHeader, MemoryChangeStore};
    use crate::pristine::{MutTxnT, Pristine, ViewScope};

    fn mapped_record_state() -> SpanRecordState {
        let first = Hash::of(b"first");
        let second = Hash::of(b"second");
        let root = ExternalSpan {
            owner: first,
            start: 10,
            end: 20,
        };
        SpanRecordState {
            file: ExternalFile {
                owner: first,
                ordinal: 0,
            },
            root: root.clone(),
            content: b"abcDEFghi".to_vec(),
            spans: vec![
                VisibleSpan {
                    output: 0..3,
                    target: root.clone(),
                    target_start: 2,
                },
                VisibleSpan {
                    output: 3..6,
                    target: ExternalSpan {
                        owner: second,
                        start: 30,
                        end: 33,
                    },
                    target_start: 0,
                },
                VisibleSpan {
                    output: 6..9,
                    target: root,
                    target_start: 5,
                },
            ],
            dependencies: vec![first, second],
        }
    }

    #[test]
    fn authored_ranges_preserve_owners_across_split_lines_and_tokens() -> Result<()> {
        let state = mapped_record_state();
        let ranges = state.authored_ranges(1..8)?;
        assert_eq!(ranges.len(), 3);
        assert_eq!(
            ranges
                .iter()
                .map(|range| range.output.clone())
                .collect::<Vec<_>>(),
            vec![1..3, 3..6, 6..8]
        );
        assert_eq!(
            ranges
                .iter()
                .map(|range| range.target_start)
                .collect::<Vec<_>>(),
            vec![3, 0, 5]
        );
        assert_eq!(ranges[0].target.owner, ranges[2].target.owner);
        assert_ne!(ranges[0].target.owner, ranges[1].target.owner);
        assert!(state.authored_ranges(3..3)?.is_empty());
        assert_eq!(state.authored_ranges(3..6)?.len(), 1);
        Ok(())
    }

    #[test]
    fn semantic_projection_keeps_cross_change_token_authorship() -> Result<()> {
        let state = mapped_record_state();
        let lines = state.semantic_lines()?;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].output, 0..9);
        assert_eq!(lines[0].tokens.len(), 1);
        let token = &lines[0].tokens[0];
        assert_eq!(token.kind, crate::diff::token::TokenKind::Word);
        assert_eq!(token.output, 0..9);
        assert_eq!(token.authors.len(), 3);
        assert_eq!(token.authors[0].target.owner, token.authors[2].target.owner);
        assert_ne!(token.authors[0].target.owner, token.authors[1].target.owner);
        Ok(())
    }

    #[test]
    fn semantic_projection_ids_are_portable_across_offsets_and_fragment_splits() -> Result<()> {
        let state = mapped_record_state();
        let original = state.semantic_lines()?;
        assert_ne!(original[0].id, original[0].tokens[0].id);
        let encoded = postcard::to_allocvec(&original[0].id)?;
        assert_eq!(
            postcard::from_bytes::<SpanProjectionId>(&encoded)?,
            original[0].id
        );
        let mut shifted = mapped_record_state();
        shifted.content.splice(0..0, b"prefix\n".iter().copied());
        for span in &mut shifted.spans {
            span.output.start += 7;
            span.output.end += 7;
        }
        let prefix_owner = Hash::of(b"prefix author");
        shifted.spans.insert(
            0,
            VisibleSpan {
                output: 0..7,
                target: ExternalSpan {
                    owner: prefix_owner,
                    start: 0,
                    end: 7,
                },
                target_start: 0,
            },
        );
        let first = shifted.spans[1].clone();
        shifted.spans[1].output.end = first.output.start + 1;
        shifted.spans.insert(
            2,
            VisibleSpan {
                output: first.output.start + 1..first.output.end,
                target: first.target,
                target_start: first.target_start + 1,
            },
        );
        let later = shifted.semantic_lines()?;
        assert_eq!(later.len(), 2);
        assert_eq!(later[1].id, original[0].id);
        assert_eq!(later[1].tokens[0].id, original[0].tokens[0].id);
        shifted.file.ordinal += 1;
        assert_ne!(shifted.semantic_lines()?[1].id, original[0].id);
        Ok(())
    }

    #[test]
    fn projected_semantic_v1_identity_reference_vector() -> Result<()> {
        use crate::types::Base32;
        let owner = Hash::of(b"projection reference owner");
        let content = b"let x = 1;\n".to_vec();
        let root = ExternalSpan {
            owner,
            start: 0,
            end: content.len() as u64,
        };
        let state = SpanRecordState {
            file: ExternalFile { owner, ordinal: 7 },
            root: root.clone(),
            spans: vec![VisibleSpan {
                output: 0..content.len(),
                target: root,
                target_start: 0,
            }],
            content,
            dependencies: vec![owner],
        };
        let snapshot = state.semantic_snapshot()?;
        assert_eq!(
            snapshot.id.0.to_base32(),
            "TTHO66BQVUGLYVDJ7G7OXKX3KOWQQ5Z6UCEXT2Y65HXX245MXUKA"
        );
        assert_eq!(
            snapshot.lines[0].id.0.to_base32(),
            "GBBDUIMWT3HERMRPYH3GQ6XKIF3AFSGW5WKENDWEMD3N4WQ2R3ZQ"
        );
        let tokens: Vec<_> = snapshot.lines[0]
            .tokens
            .iter()
            .map(|token| token.id.0.to_base32())
            .collect();
        assert_eq!(
            tokens.first().unwrap(),
            "XCWKLIVL4AZVFLMUICUOOPNGNS2POKIYY6KEOZN2EMB22ZTDBM3A"
        );
        assert_eq!(
            tokens.last().unwrap(),
            "OSUGQK2YZIJ527KIDII25BX7BU43YESF5ETZRI346QEHOZGZMFAQ"
        );
        Ok(())
    }

    #[test]
    fn empty_semantic_snapshots_keep_portable_file_identity() -> Result<()> {
        let mut state = mapped_record_state();
        state.content.clear();
        state.spans.clear();
        let snapshot = state.semantic_snapshot()?;
        assert!(snapshot.lines.is_empty());
        assert_eq!(snapshot.file, state.file);
        assert_eq!(state.semantic_snapshot()?.id, snapshot.id);
        state.file.ordinal += 1;
        assert_ne!(state.semantic_snapshot()?.id, snapshot.id);
        Ok(())
    }

    #[test]
    fn semantic_snapshot_refuses_duplicate_projected_line_identity() {
        let mut state = mapped_record_state();
        state.content = b"a\na\n".to_vec();
        let target = state.root.clone();
        state.spans = vec![
            VisibleSpan {
                output: 0..2,
                target: target.clone(),
                target_start: 0,
            },
            VisibleSpan {
                output: 2..4,
                target,
                target_start: 0,
            },
        ];
        let error = state.semantic_snapshot().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("duplicate projected line identity"),
            "{error}"
        );
    }

    #[test]
    fn semantic_projection_walks_many_fragments_without_changing_identities() -> Result<()> {
        let owner = Hash::of(b"large fragmented source");
        let content = b"alpha beta\n".repeat(1000);
        let root = ExternalSpan {
            owner,
            start: 0,
            end: content.len() as u64,
        };
        let state = SpanRecordState {
            file: ExternalFile { owner, ordinal: 0 },
            root: root.clone(),
            spans: (0..content.len())
                .map(|index| VisibleSpan {
                    output: index..index + 1,
                    target: root.clone(),
                    target_start: index as u64,
                })
                .collect(),
            content,
            dependencies: vec![owner],
        };
        let snapshot = state.semantic_snapshot()?;
        assert_eq!(snapshot.lines.len(), 1000);
        assert_eq!(snapshot.lines[0].tokens.len(), 4);
        for line in [&snapshot.lines[0], &snapshot.lines[999]] {
            assert_eq!(
                line.id,
                projection_id(
                    &state.file,
                    b"line",
                    &state.authored_ranges(line.output.clone())?
                )?
            );
            for token in &line.tokens {
                assert_eq!(
                    token.id,
                    projection_id(
                        &state.file,
                        b"token",
                        &state.authored_ranges(token.output.clone())?
                    )?
                );
            }
        }
        Ok(())
    }

    #[test]
    fn semantic_projection_preserves_split_crlf_and_refuses_binary_or_gaps() -> Result<()> {
        let first = Hash::of(b"first newline author");
        let second = Hash::of(b"second newline author");
        let mut state = SpanRecordState {
            file: ExternalFile {
                owner: first,
                ordinal: 0,
            },
            root: ExternalSpan {
                owner: first,
                start: 0,
                end: 2,
            },
            content: b"a\r\nb\n".to_vec(),
            spans: vec![
                VisibleSpan {
                    output: 0..2,
                    target: ExternalSpan {
                        owner: first,
                        start: 0,
                        end: 2,
                    },
                    target_start: 0,
                },
                VisibleSpan {
                    output: 2..5,
                    target: ExternalSpan {
                        owner: second,
                        start: 4,
                        end: 7,
                    },
                    target_start: 0,
                },
            ],
            dependencies: vec![first, second],
        };
        let lines = state.semantic_lines()?;
        assert_eq!(
            lines
                .iter()
                .map(|line| line.output.clone())
                .collect::<Vec<_>>(),
            vec![0..3, 3..5]
        );
        let newline = lines[0].tokens.last().unwrap();
        assert_eq!(newline.kind, crate::diff::token::TokenKind::Newline);
        assert_eq!(newline.output, 1..3);
        assert_eq!(newline.authors.len(), 2);
        assert_eq!(newline.authors[0].target.owner, first);
        assert_eq!(newline.authors[1].target.owner, second);
        state.spans[1].output.start = 3;
        assert!(state.semantic_lines().is_err());
        state.content = b"a\0".to_vec();
        assert!(state.semantic_lines().is_err());
        state.content = vec![0xff];
        assert!(state.semantic_lines().is_err());
        Ok(())
    }

    #[test]
    fn authored_ranges_reject_gaps_overlaps_and_invalid_offsets() {
        let mut state = mapped_record_state();
        assert!(state.authored_ranges(4..10).is_err());
        assert!(state
            .authored_ranges(std::ops::Range { start: 3, end: 2 })
            .is_err());
        state.spans[1].output.start = 4;
        assert!(state.authored_ranges(0..9).is_err());
        state.spans[1].output.start = 2;
        assert!(state.authored_ranges(0..9).is_err());
        state.spans[1].output = 3..6;
        state.spans[1].target_start = u64::MAX;
        assert!(state.authored_ranges(3..6).is_err());
        state.spans[1].target_start = 1;
        assert!(state.authored_ranges(3..6).is_err());
        state.spans[1].target.start = 34;
        assert!(state.authored_ranges(3..6).is_err());
    }

    #[test]
    fn authored_anchor_preserves_preceding_owner_and_empty_root() -> Result<()> {
        let mut state = mapped_record_state();
        let (first, offset) = state.authored_anchor(0)?;
        assert_eq!(first, state.spans[0].target);
        assert_eq!(offset, 2);
        let (at_boundary, offset) = state.authored_anchor(3)?;
        assert_eq!(at_boundary, state.spans[0].target);
        assert_eq!(offset, 5);
        let (second, offset) = state.authored_anchor(4)?;
        assert_eq!(second, state.spans[1].target);
        assert_eq!(offset, 1);
        assert!(state.authored_anchor(10).is_err());
        state.spans[1].target_start = u64::MAX;
        assert!(state.authored_anchor(4).is_err());
        state.content.clear();
        state.spans.clear();
        let (root, offset) = state.authored_anchor(0)?;
        assert_eq!(root, state.root);
        assert_eq!(offset, 0);
        state.root.start = state.root.end + 1;
        assert!(state.authored_anchor(0).is_err());
        Ok(())
    }

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
        fn projection_ids(
            repo: &Pristine,
            store: &MemoryChangeStore,
            view_name: &str,
            inode: Inode,
        ) -> Result<(
            SpanProjectionId,
            Vec<(SpanProjectionId, Vec<SpanProjectionId>)>,
        )> {
            let txn = repo.read_txn()?;
            let view = txn.get_view(view_name)?.context("view missing")?;
            let visible = EffectiveProjectionClosure::try_from_membership(
                &txn,
                &txn.view_membership_set(&view)?,
            )?;
            let state = SpanRenderContext::new(&txn, store, &visible)?
                .record_state(inode)?
                .context("span state missing")?;
            let snapshot = state.semantic_snapshot()?;
            Ok((
                snapshot.id,
                snapshot
                    .lines
                    .into_iter()
                    .map(|line| {
                        (
                            line.id,
                            line.tokens.into_iter().map(|token| token.id).collect(),
                        )
                    })
                    .collect(),
            ))
        }
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
        assert_ne!(
            projection_ids(&repo, &store, "left", inode)?.0,
            projection_ids(&repo, &store, "right", inode)?.0
        );
        assert_eq!(
            render(&repo, &store, "left", inode)?,
            b"ALPHA\nbeta\ngamma\nomega"
        );
        assert_eq!(
            render(&repo, &store, "right", inode)?,
            b"alpha\nbeta\nGAMMA\nomega"
        );
        for (view_name, expected_owners) in [
            (
                "left",
                vec![
                    vec![left_hash, root_hash],
                    vec![root_hash],
                    vec![root_hash],
                    vec![root_hash],
                ],
            ),
            (
                "right",
                vec![
                    vec![root_hash],
                    vec![root_hash],
                    vec![right_hash, root_hash],
                    vec![root_hash],
                ],
            ),
        ] {
            let txn = repo.read_txn()?;
            let view = txn.get_view(view_name)?.context("view missing")?;
            let visible = EffectiveProjectionClosure::try_from_membership(
                &txn,
                &txn.view_membership_set(&view)?,
            )?;
            let state = SpanRenderContext::new(&txn, &store, &visible)?
                .record_state(inode)?
                .context("span state missing")?;
            let actual: Vec<Vec<Hash>> = state
                .semantic_lines()?
                .iter()
                .map(|line| {
                    let mut owners = Vec::new();
                    for token in &line.tokens {
                        for fragment in &token.authors {
                            if !owners.contains(&fragment.target.owner) {
                                owners.push(fragment.target.owner);
                            }
                        }
                    }
                    owners
                })
                .collect();
            assert_eq!(
                actual, expected_owners,
                "{view_name} must exclude its sibling's edits"
            );
        }
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
            let context = SpanRenderContext::new(&txn, &store, &visible)?;
            let projected = context.record_state(inode)?.context("span state missing")?;
            let lines = projected.semantic_lines()?;
            assert_eq!(lines.len(), 4);
            for (line, expected_owners) in lines.iter().zip([
                vec![left_hash, root_hash],
                vec![root_hash],
                vec![right_hash, root_hash],
                vec![root_hash],
            ]) {
                let mut owners = Vec::new();
                let mut rendered = Vec::new();
                for token in &line.tokens {
                    for fragment in &token.authors {
                        let owner = fragment.target.owner;
                        if !owners.contains(&owner) {
                            owners.push(owner);
                        }
                        let change = store.get_change(&owner)?;
                        let start = usize::try_from(fragment.target.start + fragment.target_start)?;
                        let end = start + fragment.output.len();
                        rendered.extend_from_slice(&change.contents[start..end]);
                    }
                }
                assert_eq!(owners, expected_owners);
                assert_eq!(rendered, projected.content[line.output.clone()]);
            }
            assert_eq!(
                context.render_bounded(inode, expected.len())?,
                BoundedSpanContent::Complete(expected.to_vec())
            );
            match context.render_bounded(inode, expected.len() - 1)? {
                BoundedSpanContent::Oversized { retained_bytes } => {
                    assert!(retained_bytes < expected.len());
                }
                other => panic!("expected bounded oversized result, got {other:?}"),
            }
            assert_eq!(
                context.render_bounded(inode, 0)?,
                BoundedSpanContent::Oversized { retained_bytes: 0 }
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
        let expected_ids = projection_ids(&repo, &store, "both", inode)?;
        assert_eq!(
            projection_ids(&repo, &store, "reverse", inode)?,
            expected_ids
        );
        // The receiver allocates its own inode and local change IDs. Only
        // immutable change content and portable file identity are transferred.
        let receiver_path = dir.path().join("receiver.redb");
        let receiver = Pristine::open(&receiver_path)?;
        let receiver_inode = Inode::new(987);
        {
            let mut txn = receiver.write_txn()?;
            txn.open_or_create_view("dev")?;
            txn.register_change(&Hash::of(b"unrelated receiver allocation"))?;
            txn.commit()?;
        }
        add_change(
            &receiver,
            &store,
            "dev",
            store.get_change(&root_hash)?,
            BTreeMap::from([(0, receiver_inode)]),
        )?;
        add_change(&receiver, &store, "dev", left.clone(), BTreeMap::new())?;
        add_change(&receiver, &store, "dev", right.clone(), BTreeMap::new())?;
        assert_ne!(
            repo.read_txn()?.get_internal(&root_hash)?,
            receiver.read_txn()?.get_internal(&root_hash)?
        );
        assert_eq!(
            projection_ids(&receiver, &store, "dev", receiver_inode)?,
            expected_ids
        );
        drop(receiver);
        let receiver = Pristine::open(receiver_path)?;
        assert_eq!(
            projection_ids(&receiver, &store, "dev", receiver_inode)?,
            expected_ids
        );
        drop(repo);
        let reopened = Pristine::open(path)?;
        assert_eq!(render(&reopened, &store, "both", inode)?, expected);
        assert_eq!(
            projection_ids(&reopened, &store, "both", inode)?,
            expected_ids
        );
        Ok(())
    }

    #[test]
    fn semantic_projection_refuses_missing_visible_edit_index_after_reopen() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("pristine.redb");
        let repo = Pristine::open(&path)?;
        let store = MemoryChangeStore::new();
        let inode = Inode::new(42);
        {
            let mut txn = repo.write_txn()?;
            txn.open_or_create_view("dev")?;
            txn.commit()?;
        }
        let mut root = Change::new(
            ChangeHeader::new("root"),
            vec![],
            b"hello\n".to_vec(),
            vec![],
        );
        root.hashed.span = Some(SpanPatch::new(
            vec![],
            vec![SpanOp::Root {
                path: "file.txt".into(),
                ordinal: 0,
                start: 0,
                end: 6,
            }],
        ));
        let root_hash = add_change(&repo, &store, "dev", root, BTreeMap::from([(0, inode)]))?;
        let mut edit = Change::new(
            ChangeHeader::new("edit"),
            vec![],
            b"a".to_vec(),
            vec![root_hash],
        );
        edit.hashed.span = Some(SpanPatch::new(
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
                start: 1,
                end: 2,
                insert_start: 0,
                insert_end: 1,
            }],
        ));
        add_change(&repo, &store, "dev", edit, BTreeMap::new())?;
        assert_eq!(render(&repo, &store, "dev", inode)?, b"hallo\n");
        {
            let txn = repo.write_txn()?;
            let owner = txn.get_internal(&root_hash)?.context("root id missing")?;
            let key = span_key(inode, owner, 0, 6);
            let mut edits = txn.txn.open_multimap_table(EDITS)?;
            assert_eq!(edits.remove_all(&key)?.count(), 1);
            drop(edits);
            txn.commit()?;
        }
        drop(repo);
        let reopened = Pristine::open(path)?;
        let txn = reopened.read_txn()?;
        let view = txn.get_view("dev")?.context("view missing")?;
        let visible = EffectiveProjectionClosure::try_from_membership(
            &txn,
            &txn.view_membership_set(&view)?,
        )?;
        let context = SpanRenderContext::new(&txn, &store, &visible)?;
        let error = match context.record_state(inode) {
            Err(error) => error,
            Ok(_) => panic!("corrupt span index was accepted"),
        };
        assert!(
            error.to_string().contains("visible span index differs"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn projected_semantic_edit_validates_capability_and_exact_dependency_snapshot() -> Result<()> {
        use crate::pristine::{CapabilityMutTxnT, SPAN_PROJECTED_SEMANTICS_CAPABILITY};
        let dir = tempfile::tempdir()?;
        let repo = Pristine::open(dir.path().join("pristine.redb"))?;
        let store = MemoryChangeStore::new();
        let inode = Inode::new(42);
        {
            let mut txn = repo.write_txn()?;
            txn.open_or_create_view("dev")?;
            txn.commit()?;
        }
        let mut root = Change::new(
            ChangeHeader::new("root"),
            vec![],
            b"hello\n".to_vec(),
            vec![],
        );
        root.hashed.span = Some(SpanPatch::new(
            vec![],
            vec![SpanOp::Root {
                path: "file.txt".into(),
                ordinal: 0,
                start: 0,
                end: 6,
            }],
        ));
        let root_hash = add_change(&repo, &store, "dev", root, BTreeMap::from([(0, inode)]))?;
        let file = ExternalFile {
            owner: root_hash,
            ordinal: 0,
        };
        let baseline = {
            let txn = repo.read_txn()?;
            let view = txn.get_view("dev")?.unwrap();
            let visible = EffectiveProjectionClosure::try_from_membership(
                &txn,
                &txn.view_membership_set(&view)?,
            )?;
            SpanRenderContext::new(&txn, &store, &visible)?
                .record_state(inode)?
                .unwrap()
                .semantic_snapshot()?
                .id
        };
        let make_edit = |baseline| {
            let mut change = Change::new(
                ChangeHeader::new("edit"),
                vec![],
                b"a".to_vec(),
                vec![root_hash],
            );
            change.hashed.span = Some(SpanPatch::new(
                vec![],
                vec![SpanOp::Edit {
                    file: file.clone(),
                    target: ExternalSpan {
                        owner: root_hash,
                        start: 0,
                        end: 6,
                    },
                    start: 1,
                    end: 2,
                    insert_start: 0,
                    insert_end: 1,
                }],
            ));
            change
                .hashed
                .file_ops
                .push(crate::change::FileOps::projected_span_edit(
                    "file.txt".into(),
                    file.clone(),
                    baseline,
                ));
            change
        };
        let valid = make_edit(baseline);
        let error = add_change(&repo, &store, "dev", valid.clone(), BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("capability"), "{error:#}");
        assert_eq!(render(&repo, &store, "dev", inode)?, b"hello\n");
        {
            let mut txn = repo.write_txn()?;
            txn.put_required_capability(SPAN_PROJECTED_SEMANTICS_CAPABILITY)?;
            txn.commit()?;
        }
        let wrong = make_edit(SpanProjectionId(Hash::of(b"wrong baseline")));
        let error = add_change(&repo, &store, "dev", wrong, BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("baseline differs"), "{error:#}");
        assert_eq!(render(&repo, &store, "dev", inode)?, b"hello\n");
        {
            let mut txn = repo.write_txn()?;
            let id = txn.register_change(&Hash::of(b"generic apply attempt"))?;
            let error =
                crate::apply::apply_file_ops_batched(&mut txn, id, valid.file_ops()).unwrap_err();
            assert!(error.to_string().contains("span-aware"), "{error}");
            txn.abort()?;
        }
        add_change(&repo, &store, "dev", valid, BTreeMap::new())?;
        assert_eq!(render(&repo, &store, "dev", inode)?, b"hallo\n");
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
        let txn = repo.read_txn()?;
        let state = txn.get_view("both")?.unwrap();
        let visible = EffectiveProjectionClosure::try_from_membership(
            &txn,
            &txn.view_membership_set(&state)?,
        )?;
        assert!(SpanRenderContext::new(&txn, &store, &visible)?
            .render_bounded(inode, 0)
            .unwrap_err()
            .to_string()
            .contains("overlapping"));
        Ok(())
    }
}
