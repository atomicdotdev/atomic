use super::*;
use crate::change::{Atom, AtomRef, GraphOp};
use crate::types::{GraphNode, Position};

/// A canonical inode's unambiguous legacy byte graph under one checked closure.
pub struct LegacySpanGraph {
    pub(super) position: Position<NodeId>,
    pub(super) nodes: Vec<GraphNode<NodeId>>,
    pub(super) contributors: BTreeSet<NodeId>,
}

#[derive(Default)]
pub(super) struct LegacyMetadata {
    files: Vec<u64>,
    ranges: BTreeSet<(Hash, u64, u64, u64)>,
    pub(super) projected_files: Vec<ExternalFile>,
    foreign_trunks: Vec<(Hash, u64, u64)>,
}
impl LegacyMetadata {
    pub(super) fn mismatched_trunk(&self, owner: Hash, pos: u64, actual: NodeId) -> bool {
        self.foreign_trunks
            .iter()
            .any(|(file, position, raw)| *file == owner && *position == pos && *raw != actual.get())
    }
    pub(super) fn file_position(&self, ordinal: u32) -> Option<u64> {
        self.files.get(ordinal as usize).copied()
    }
    pub(super) fn contains(&self, owner: Hash, pos: u64, start: u64, end: u64) -> bool {
        self.ranges.contains(&(owner, pos, start, end))
    }
}

pub(super) fn covers(state: &SpanRecordState, target: &ExternalSpan, start: u64, end: u64) -> bool {
    if target == &state.root && target.start == target.end {
        return state.content.is_empty() && start == 0 && end == 0;
    }
    let mut ranges = state
        .spans
        .iter()
        .filter(|span| &span.target == target)
        .map(|span| {
            (
                span.target_start,
                span.target_start + span.output.len() as u64,
            )
        })
        .collect::<Vec<_>>();
    ranges.sort();
    let mut at = start;
    for (lo, hi) in ranges {
        if start == end && lo <= start && start <= hi {
            return true;
        }
        if hi <= at {
            continue;
        }
        if lo > at {
            return false;
        }
        at = hi;
        if at >= end {
            return true;
        }
    }
    false
}

pub(super) fn metadata(hash: Hash, change: &Change) -> LegacyMetadata {
    let mut result = LegacyMetadata::default();
    for ops in change.file_ops() {
        if let Some(crate::crdt::TrunkOp::ProjectedSpanEdit { file, .. }) = ops.trunk_op() {
            result.projected_files.push(file.clone());
        }
        if change.hashed.span.is_none() && !ops.trunk_id().change_id().is_root() {
            for op in change
                .hunks()
                .iter()
                .filter(|op| op.path() == Some(ops.path()))
            {
                for atom in op.atoms() {
                    let inode = match atom {
                        AtomRef::Insertion(i) | AtomRef::Atom(Atom::Insertion(i)) => i.inode,
                        AtomRef::EdgeUpdate(e) | AtomRef::Atom(Atom::EdgeUpdate(e)) => e.inode,
                    };
                    result.foreign_trunks.push((
                        inode.change.unwrap_or(hash),
                        inode.pos.get(),
                        ops.trunk_id().change_id().get(),
                    ));
                }
            }
        }
    }
    for op in change.hunks() {
        if let GraphOp::FileAdd { add_inode, .. } = op {
            let pos = add_inode.start.get();
            result.files.push(pos);
            result.ranges.insert((hash, pos, pos, pos));
        }
        for atom in op.atoms() {
            if let AtomRef::Insertion(insert) | AtomRef::Atom(Atom::Insertion(insert)) = atom {
                if !insert.flag.contains(crate::EdgeFlags::FOLDER) && insert.start < insert.end {
                    result.ranges.insert((
                        insert.inode.change.unwrap_or(hash),
                        insert.inode.pos.get(),
                        insert.start.get(),
                        insert.end.get(),
                    ));
                }
            }
        }
    }
    result
}

pub(super) fn retrieve<T: GraphTxnT + super::super::TreeTxnT>(
    txn: &T,
    inode: Inode,
    visible: &EffectiveProjectionClosure,
) -> Result<Option<LegacySpanGraph>> {
    use crate::output::alive::{compute_order, retrieve_graph, RetrieveOptions};
    let Some(position) = txn.inode_position(inode)? else {
        return Ok(None);
    };
    if !visible.contains(position.change) {
        return Ok(None);
    }
    let mut retrieved = retrieve_graph(
        txn,
        position,
        RetrieveOptions::new().with_graph_visibility(visible.clone()),
    )?;
    ensure!(!retrieved.truncated, "legacy span graph is truncated");
    let order = compute_order(&mut retrieved.graph);
    ensure!(
        !order.has_conflicts(),
        "legacy span baseline contains unresolved ordering conflicts"
    );
    let mut nodes = Vec::new();
    for scc in order.sccs.iter().rev() {
        for id in scc {
            let vertex = retrieved
                .graph
                .try_get_vertex(*id)
                .context("legacy graph vertex missing")?;
            ensure!(
                !vertex.is_zombie(),
                "legacy span baseline contains a zombie conflict"
            );
            if !vertex.node.change.is_root() && vertex.node.start < vertex.node.end {
                nodes.push(vertex.node);
            }
        }
    }
    let mut contributors = BTreeSet::from([position.change]);
    for row in txn.iter_inode_vertices(inode)? {
        let (node, edge) = row?;
        for id in [node.change, edge.introduced_by()] {
            if !id.is_root() && visible.contains(id) {
                contributors.insert(id);
            }
        }
    }
    Ok(Some(LegacySpanGraph {
        position,
        nodes,
        contributors,
    }))
}

pub(super) fn record_state<T: SpanPatchReadTxn>(
    context: &SpanRenderContext<'_, T>,
    inode: Inode,
) -> Result<Option<SpanRecordState>> {
    layout(context, inode, true)
}

pub(super) fn layout<T: SpanPatchReadTxn>(
    context: &SpanRenderContext<'_, T>,
    inode: Inode,
    retain_content: bool,
) -> Result<Option<SpanRecordState>> {
    let Some(graph) = context.txn.legacy_span_graph(inode, context.visible)? else {
        return Ok(None);
    };
    let owner = context
        .txn
        .get_external(graph.position.change)?
        .context("legacy file owner hash missing")?;
    let creator = context
        .changes
        .get(&owner)
        .context("legacy file creator outside closure")?;
    let ordinal = creator
        .legacy
        .files
        .iter()
        .position(|pos| *pos == graph.position.pos.get())
        .context("legacy inode has no canonical FileAdd")?;
    let ordinal = u32::try_from(ordinal)?;
    ensure!(
        creator.span.is_none(),
        "typed span root index is missing; cannot fall back to legacy graph"
    );
    let mut content = Vec::new();
    let mut spans: Vec<VisibleSpan> = Vec::new();
    let mut output_len = 0usize;
    for node in graph.nodes {
        let hash = context
            .txn
            .get_external(node.change)?
            .context("legacy content owner hash missing")?;
        let change = context
            .changes
            .get(&hash)
            .context("legacy content outside closure")?;
        let candidates: Vec<_> = change
            .legacy
            .ranges
            .iter()
            .filter(|(file_owner, pos, start, end)| {
                *file_owner == owner
                    && *pos == graph.position.pos.get()
                    && *start <= node.start.get()
                    && node.end.get() <= *end
            })
            .collect();
        ensure!(
            candidates.len() == 1,
            "legacy vertex has no unique immutable authored range"
        );
        let (_, _, start, end) = *candidates[0];
        ensure!(
            end <= change.contents.len() as u64,
            "legacy declaration is outside verified content"
        );
        let bytes = change
            .contents
            .get(usize::try_from(node.start.get())?..usize::try_from(node.end.get())?)
            .context("legacy authored bytes missing")?;
        let lo = output_len;
        output_len = output_len
            .checked_add(bytes.len())
            .context("legacy output overflow")?;
        if retain_content {
            content.extend_from_slice(bytes);
        }
        let target = ExternalSpan {
            owner: hash,
            start,
            end,
        };
        let offset = node.start.get() - start;
        if let Some(previous) = spans.last_mut() {
            if previous.target == target
                && previous.target_start + previous.output.len() as u64 == offset
            {
                previous.output.end = output_len;
                continue;
            }
        }
        spans.push(VisibleSpan {
            output: lo..output_len,
            target,
            target_start: offset,
        });
    }
    let dependencies = graph
        .contributors
        .into_iter()
        .map(|id| {
            context
                .txn
                .get_external(id)?
                .context("legacy contributor hash missing")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    Ok(Some(SpanRecordState {
        legacy_graph: true,
        projected_semantics: true,
        file: ExternalFile { owner, ordinal },
        root: ExternalSpan {
            owner,
            start: graph.position.pos.get(),
            end: graph.position.pos.get(),
        },
        content,
        spans,
        dependencies: dependencies.into_iter().collect(),
    }))
}
