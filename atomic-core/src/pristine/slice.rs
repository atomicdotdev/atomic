//! Graph slices: a repository's own pristine rows, copied for a remote
//! sandbox's local cache.
//!
//! A remote sandbox never holds its repository, but `record` has to read the
//! graph to compute a change — and the change it computes carries the
//! repository's internal node ids and inode numbers, so the sandbox must read
//! exactly the rows the repository has. A slice is those rows, byte for byte:
//!
//! - the **skeleton** ([`ReadTxn::export_skeleton`]): the view's tree —
//!   paths, inodes and their positions, directories — and the id tables for
//!   every change the view can see. Enough for `status` with no graph at all.
//! - a **graph slice** ([`ReadTxn::export_graph_slice`]): for a set of
//!   inodes, the GRAPH / INODE_GRAPH rows of each file's content and of its
//!   name chain up to the root, one hop of neighbours (so `find_block` finds
//!   what it would find on the repository), the file's CRDT rows, and its
//!   recorded conflicts on the view.
//!
//! [`WriteTxn::import_skeleton`] and [`WriteTxn::import_graph_slice`] write
//! them into a fresh pristine; [`WriteTxn::import_view_snapshot`] writes the
//! view itself, flattened and with the repository's own Merkle state.
//!
//! Rows are copied, never re-derived, so a read on the cache returns what the
//! same read returns on the repository for every row the slice covers.
//!
//! Edges are not filtered by view: a vertex's key exists only while it has an
//! edge, and `find_block` must see the same vertices the repository sees.
//! Reads that matter filter by view themselves. Content is another matter —
//! the slice carries none; span bytes travel separately, and only for changes
//! the view can see.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use redb::ReadableTable;
use serde::{Deserialize, Serialize};

use crate::crdt::tables::{
    BRANCHES, BRANCH_AFTER, BRANCH_LEAVES, BRANCH_VERTEX, INODE_TRUNK, LEAVES, PATH_TRUNK, TRUNKS,
    TRUNK_BRANCHES, VERTEX_BRANCH,
};
use crate::pristine::error::PristineResult;
use crate::pristine::tables::*;
use crate::pristine::traits::{GraphTxnT, ViewScope, ViewState};
use crate::pristine::txn::{ReadTxn, WriteTxn};
use crate::types::{ChangePosition, EdgeFlags, GraphNode, Merkle, NodeId, Position};

/// Inodes a remote sandbox allocates for itself (`atomic add`) start here,
/// far above any the repository hands out, so the two never collide.
pub const LOCAL_INODE_FLOOR: u64 = 1 << 62;

/// An EXTERNAL / INTERNAL / NODE_TYPES row: `(id, hash, node type)`.
pub type IdRow = (u64, [u8; 32], Option<u8>);

/// Raw pristine rows. Every field is one table's rows, key and value exactly
/// as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSlice {
    /// EXTERNAL / INTERNAL / NODE_TYPES: `(id, hash, node type)`.
    pub ids: Vec<IdRow>,
    /// TREE: `path → inode`.
    pub tree: Vec<(String, u64)>,
    /// REV_TREE: `inode → path`.
    pub rev_tree: Vec<(u64, String)>,
    /// INODES (REV_INODES is its inverse): `inode → position`.
    pub inodes: Vec<(u64, [u8; 16])>,
    /// DIRECTORIES: `inode → flags`.
    pub directories: Vec<(u64, u8)>,
    /// GRAPH: `vertex → edges`.
    pub graph: Vec<([u8; 24], Vec<[u8; 24]>)>,
    /// INODE_GRAPH: `(inode, vertex) → edges`.
    pub inode_graph: Vec<([u8; 32], Vec<[u8; 24]>)>,
    pub crdt: CrdtRows,
    /// CONFLICTS for the view: `inode → serialized conflicts`. Keyed by inode
    /// alone; the view id is the importer's.
    pub conflicts: Vec<(u64, Vec<u8>)>,
}

/// A file's CRDT rows (see `crdt::tables`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrdtRows {
    pub trunks: Vec<([u8; 12], Vec<u8>)>,
    pub inode_trunk: Vec<(u64, [u8; 12])>,
    pub path_trunk: Vec<(String, [u8; 12])>,
    pub trunk_branches: Vec<([u8; 12], Vec<[u8; 12]>)>,
    pub branches: Vec<([u8; 12], [u8; 24])>,
    pub branch_after: Vec<([u8; 12], [u8; 12])>,
    pub branch_vertex: Vec<([u8; 12], [u8; 24])>,
    pub vertex_branch: Vec<([u8; 24], [u8; 12])>,
    pub branch_leaves: Vec<([u8; 12], Vec<[u8; 12]>)>,
    pub leaves: Vec<([u8; 12], [u8; 22])>,
}

/// A view as a remote sandbox sees it: its name, the repository's Merkle
/// state and change count for it, and the ids of every change it can see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSnapshotRows {
    pub id: u64,
    pub name: String,
    pub state: Merkle,
    pub change_count: u64,
    /// In application order.
    pub visible: Vec<u64>,
}

fn change_of_edge(edge: &[u8; 24]) -> (u64, u64) {
    (
        u64::from_le_bytes(edge[8..16].try_into().unwrap()),
        u64::from_le_bytes(edge[16..24].try_into().unwrap()),
    )
}

fn edge_flags(edge: &[u8; 24]) -> EdgeFlags {
    let flag_and_pos = u64::from_le_bytes(edge[0..8].try_into().unwrap());
    EdgeFlags::from_bits_truncate((flag_and_pos >> 56) as u8)
}

fn edge_dest(edge: &[u8; 24]) -> Position<NodeId> {
    let flag_and_pos = u64::from_le_bytes(edge[0..8].try_into().unwrap());
    let change = u64::from_le_bytes(edge[8..16].try_into().unwrap());
    Position::new(
        NodeId::new(change),
        ChangePosition::new(flag_and_pos & ((1 << 56) - 1)),
    )
}

fn vertex_key(node: GraphNode<NodeId>) -> [u8; 24] {
    encode_vertex(node.change.get(), node.start.get(), node.end.get())
}

/// How far a traversal expands from a vertex.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Reach {
    /// Its row only.
    Row,
    /// Its row and its name chain towards the root (a name or a directory).
    Up,
    /// Its row, and every edge onward except into another inode's content
    /// (a file's content).
    File,
}

impl ReadTxn {
    /// The view's tree and ids: TREE for the `live` inodes (the paths the
    /// view actually has), REV_TREE / INODES / DIRECTORIES for every inode
    /// whose position a visible change introduced, and the id rows for every
    /// visible change.
    pub fn export_skeleton(
        &self,
        visible: &BTreeSet<u64>,
        live: &BTreeSet<u64>,
    ) -> PristineResult<GraphSlice> {
        let mut out = GraphSlice::default();
        let inodes = self.txn.open_table(INODES)?;
        let mut kept = BTreeSet::new();
        for row in inodes.iter()? {
            let (k, v) = row?;
            let (change, _) = decode_position(v.value());
            if change == 0 || visible.contains(&change) {
                kept.insert(k.value());
                out.inodes.push((k.value(), *v.value()));
            }
        }
        let rev_tree = self.txn.open_table(REV_TREE)?;
        for row in rev_tree.iter()? {
            let (k, v) = row?;
            if kept.contains(&k.value()) {
                out.rev_tree.push((k.value(), v.value().to_string()));
            }
        }
        let tree = self.txn.open_table(TREE)?;
        for row in tree.iter()? {
            let (k, v) = row?;
            if live.contains(&v.value()) && kept.contains(&v.value()) {
                out.tree.push((k.value().to_string(), v.value()));
            }
        }
        let directories = self.txn.open_table(DIRECTORIES)?;
        for row in directories.iter()? {
            let (k, v) = row?;
            if kept.contains(&k.value()) {
                out.directories.push((k.value(), v.value()));
            }
        }
        out.ids = self.id_rows(visible.iter().copied())?;
        Ok(out)
    }

    /// The rows `record` reads for `inodes`, on the view `view_id`.
    pub fn export_graph_slice(&self, inodes: &[u64], view_id: u64) -> PristineResult<GraphSlice> {
        let mut out = GraphSlice::default();
        let inode_table = self.txn.open_table(INODES)?;
        let rev_inodes = self.txn.open_table(REV_INODES)?;
        let graph = self.txn.open_multimap_table(GRAPH)?;
        let inode_graph = self.txn.open_multimap_table(INODE_GRAPH)?;
        let conflicts = self.txn.open_table(CONFLICTS)?;

        let edges_of = |key: &[u8; 24]| -> PristineResult<Vec<[u8; 24]>> {
            let mut edges = Vec::new();
            for e in graph.get(key)? {
                edges.push(*e?.value());
            }
            Ok(edges)
        };
        let is_inode_vertex = |node: GraphNode<NodeId>| -> PristineResult<bool> {
            Ok(node.start == node.end
                && rev_inodes
                    .get(&encode_position(node.change.get(), node.start.get()))?
                    .is_some())
        };

        // Each vertex once, at the widest reach any path gives it.
        let mut reach: BTreeMap<[u8; 24], Reach> = BTreeMap::new();
        let mut queue: VecDeque<(GraphNode<NodeId>, Reach)> = VecDeque::new();
        let enqueue = |node: GraphNode<NodeId>,
                       r: Reach,
                       reach: &mut BTreeMap<[u8; 24], Reach>,
                       queue: &mut VecDeque<(GraphNode<NodeId>, Reach)>| {
            if node.is_root() {
                return;
            }
            let key = vertex_key(node);
            if reach.get(&key).is_some_and(|have| *have >= r) {
                return;
            }
            reach.insert(key, r);
            queue.push_back((node, r));
        };

        for &inode in inodes {
            let Some(pos) = inode_table.get(inode)? else {
                continue;
            };
            let (change, p) = decode_position(pos.value());
            let seed = GraphNode {
                change: NodeId::new(change),
                start: ChangePosition::new(p),
                end: ChangePosition::new(p),
            };
            enqueue(seed, Reach::File, &mut reach, &mut queue);
            // INODE_GRAPH may hold content the forward walk doesn't reach.
            let lo = encode_inode_vertex(inode, 0, 0, 0);
            let hi = encode_inode_vertex(inode, u64::MAX, u64::MAX, u64::MAX);
            for row in inode_graph.range::<&[u8; 32]>(&lo..=&hi)? {
                let (k, values) = row?;
                let key = *k.value();
                let mut edges = Vec::new();
                for e in values {
                    edges.push(*e?.value());
                }
                let (_, c, s, e) = decode_inode_vertex(&key);
                out.inode_graph.push((key, edges));
                enqueue(
                    GraphNode {
                        change: NodeId::new(c),
                        start: ChangePosition::new(s),
                        end: ChangePosition::new(e),
                    },
                    Reach::File,
                    &mut reach,
                    &mut queue,
                );
            }
            if let Some(c) = conflicts.get(&encode_view_seq(view_id, inode))? {
                out.conflicts.push((inode, c.value().to_vec()));
            }
            self.export_crdt(inode, &mut out.crdt)?;
        }

        let resolve = |pos: Position<NodeId>, parent: bool| -> Vec<GraphNode<NodeId>> {
            let mut found = Vec::new();
            if parent {
                if let Ok(n) = self.find_block_end(pos) {
                    found.push(n);
                }
                if pos.pos.get() > 0 {
                    if let Ok(n) = self.find_block(Position::new(
                        pos.change,
                        ChangePosition::new(pos.pos.get() - 1),
                    )) {
                        found.push(n);
                    }
                }
            } else if let Ok(n) = self.find_block(pos) {
                found.push(n);
            }
            found
        };

        while let Some((node, r)) = queue.pop_front() {
            if r == Reach::Row {
                continue;
            }
            for edge in edges_of(&vertex_key(node))? {
                let flags = edge_flags(&edge);
                let parent = flags.contains(EdgeFlags::PARENT);
                let folder = flags.contains(EdgeFlags::FOLDER);
                for next in resolve(edge_dest(&edge), parent) {
                    let onward = match r {
                        Reach::File if parent && folder => Reach::Up,
                        Reach::File if folder => Reach::Row,
                        Reach::File if is_inode_vertex(next)? => Reach::Row,
                        Reach::File => Reach::File,
                        Reach::Up if parent && folder => Reach::Up,
                        _ => Reach::Row,
                    };
                    enqueue(next, onward, &mut reach, &mut queue);
                }
            }
        }

        let mut ids = BTreeSet::new();
        for key in reach.keys() {
            let edges = edges_of(key)?;
            if edges.is_empty() {
                continue;
            }
            ids.insert(decode_vertex(key).0);
            for e in &edges {
                let (c, by) = change_of_edge(e);
                ids.insert(c);
                ids.insert(by);
            }
            out.graph.push((*key, edges));
        }
        for (_, edges) in &out.inode_graph {
            for e in edges {
                let (c, by) = change_of_edge(e);
                ids.insert(c);
                ids.insert(by);
            }
        }
        for (id, _) in &out.crdt.trunks {
            ids.insert(u64::from_be_bytes(id[0..8].try_into().unwrap()));
        }
        for (id, _) in &out.crdt.branches {
            ids.insert(u64::from_be_bytes(id[0..8].try_into().unwrap()));
        }
        for (id, _) in &out.crdt.leaves {
            ids.insert(u64::from_be_bytes(id[0..8].try_into().unwrap()));
        }
        ids.remove(&0);
        out.ids = self.id_rows(ids.into_iter())?;
        Ok(out)
    }

    fn export_crdt(&self, inode: u64, out: &mut CrdtRows) -> PristineResult<()> {
        let inode_trunk = self.txn.open_table(INODE_TRUNK)?;
        let Some(trunk) = inode_trunk.get(inode)? else {
            return Ok(());
        };
        let trunk = *trunk.value();
        out.inode_trunk.push((inode, trunk));
        if let Some(t) = self.txn.open_table(TRUNKS)?.get(&trunk)? {
            out.trunks.push((trunk, t.value().to_vec()));
        }
        for row in self.txn.open_table(PATH_TRUNK)?.iter()? {
            let (k, v) = row?;
            if *v.value() == trunk {
                out.path_trunk.push((k.value().to_string(), trunk));
            }
        }
        let trunk_branches = self.txn.open_multimap_table(TRUNK_BRANCHES)?;
        let branches = self.txn.open_table(BRANCHES)?;
        let branch_after = self.txn.open_table(BRANCH_AFTER)?;
        let branch_vertex = self.txn.open_table(BRANCH_VERTEX)?;
        let vertex_branch = self.txn.open_table(VERTEX_BRANCH)?;
        let branch_leaves = self.txn.open_multimap_table(BRANCH_LEAVES)?;
        let leaves = self.txn.open_table(LEAVES)?;
        let mut ids = Vec::new();
        for b in trunk_branches.get(&trunk)? {
            ids.push(*b?.value());
        }
        for b in &ids {
            if let Some(v) = branches.get(b)? {
                out.branches.push((*b, *v.value()));
            }
            if let Some(v) = branch_after.get(b)? {
                out.branch_after.push((*b, *v.value()));
            }
            if let Some(v) = branch_vertex.get(b)? {
                let vertex = *v.value();
                out.branch_vertex.push((*b, vertex));
                if let Some(back) = vertex_branch.get(&vertex)? {
                    out.vertex_branch.push((vertex, *back.value()));
                }
            }
            let mut ls = Vec::new();
            for l in branch_leaves.get(b)? {
                let l = *l?.value();
                if let Some(v) = leaves.get(&l)? {
                    out.leaves.push((l, *v.value()));
                }
                ls.push(l);
            }
            if !ls.is_empty() {
                out.branch_leaves.push((*b, ls));
            }
        }
        out.trunk_branches.push((trunk, ids));
        Ok(())
    }

    fn id_rows(&self, ids: impl Iterator<Item = u64>) -> PristineResult<Vec<IdRow>> {
        let external = self.txn.open_table(EXTERNAL)?;
        let node_types = self.txn.open_table(NODE_TYPES)?;
        let mut out = Vec::new();
        for id in ids {
            if let Some(h) = external.get(id)? {
                let t = node_types.get(id)?.map(|t| t.value());
                out.push((id, *h.value(), t));
            }
        }
        Ok(out)
    }

    /// The view's snapshot: its own state, and every change it can see.
    pub fn export_view_snapshot(&self, view: &ViewState, visible: Vec<u64>) -> ViewSnapshotRows {
        ViewSnapshotRows {
            id: view.id,
            name: view.name.clone(),
            state: view.state,
            change_count: view.change_count,
            visible,
        }
    }
}

impl WriteTxn<'_> {
    /// Replace the tree tables with `slice`'s, keeping inodes this cache
    /// allocated itself (at or above [`LOCAL_INODE_FLOOR`]); add its ids.
    pub fn import_skeleton(&mut self, slice: &GraphSlice) -> PristineResult<()> {
        let local = |inode: u64| inode >= LOCAL_INODE_FLOOR;
        // A path the cache added itself that the repository now has (its
        // change landed) is the repository's inode from here on.
        let arrived: std::collections::HashSet<&str> =
            slice.tree.iter().map(|(path, _)| path.as_str()).collect();
        let mut superseded = BTreeSet::new();
        for row in self.txn.open_table(REV_TREE)?.iter()? {
            let (inode, path) = row?;
            if local(inode.value()) && arrived.contains(path.value()) {
                superseded.insert(inode.value());
            }
        }
        let local = |inode: u64| local(inode) && !superseded.contains(&inode);
        {
            let mut tree = self.txn.open_table(TREE)?;
            tree.retain(|_, inode| local(inode))?;
            for (path, inode) in &slice.tree {
                tree.insert(path.as_str(), *inode)?;
            }
        }
        {
            let mut rev_tree = self.txn.open_table(REV_TREE)?;
            rev_tree.retain(|inode, _| local(inode))?;
            for (inode, path) in &slice.rev_tree {
                rev_tree.insert(*inode, path.as_str())?;
            }
        }
        {
            let mut inodes = self.txn.open_table(INODES)?;
            inodes.retain(|inode, _| local(inode))?;
            let mut rev = self.txn.open_table(REV_INODES)?;
            rev.retain(|_, inode| local(inode))?;
            for (inode, pos) in &slice.inodes {
                inodes.insert(*inode, pos)?;
                rev.insert(pos, *inode)?;
            }
        }
        {
            let mut dirs = self.txn.open_table(DIRECTORIES)?;
            dirs.retain(|inode, _| local(inode))?;
            for (inode, flags) in &slice.directories {
                dirs.insert(*inode, *flags)?;
            }
        }
        self.import_ids(&slice.ids)?;
        // From here on this cache's own inodes (`atomic add`) come from above
        // the floor, clear of every inode the repository has or will hand out.
        self.next_inode
            .fetch_max(LOCAL_INODE_FLOOR, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Drop every graph, CRDT and conflict row, then write `slice`'s.
    /// Conflicts are written for `view_id`.
    pub fn import_graph_slice(&mut self, slice: &GraphSlice, view_id: u64) -> PristineResult<()> {
        self.txn.delete_multimap_table(GRAPH)?;
        self.txn.delete_multimap_table(INODE_GRAPH)?;
        self.txn.delete_table(CONFLICTS)?;
        self.txn.delete_table(TRUNKS)?;
        self.txn.delete_table(INODE_TRUNK)?;
        self.txn.delete_table(PATH_TRUNK)?;
        self.txn.delete_multimap_table(TRUNK_BRANCHES)?;
        self.txn.delete_table(BRANCHES)?;
        self.txn.delete_table(BRANCH_AFTER)?;
        self.txn.delete_table(BRANCH_VERTEX)?;
        self.txn.delete_table(VERTEX_BRANCH)?;
        self.txn.delete_multimap_table(BRANCH_LEAVES)?;
        self.txn.delete_table(LEAVES)?;

        {
            let mut t = self.txn.open_multimap_table(GRAPH)?;
            for (k, edges) in &slice.graph {
                for e in edges {
                    t.insert(k, e)?;
                }
            }
        }
        {
            let mut t = self.txn.open_multimap_table(INODE_GRAPH)?;
            for (k, edges) in &slice.inode_graph {
                for e in edges {
                    t.insert(k, e)?;
                }
            }
        }
        {
            let mut t = self.txn.open_table(CONFLICTS)?;
            for (inode, c) in &slice.conflicts {
                t.insert(&encode_view_seq(view_id, *inode), c.as_slice())?;
            }
        }
        let c = &slice.crdt;
        {
            let mut t = self.txn.open_table(TRUNKS)?;
            for (k, v) in &c.trunks {
                t.insert(k, v.as_slice())?;
            }
            let mut t = self.txn.open_table(INODE_TRUNK)?;
            for (k, v) in &c.inode_trunk {
                t.insert(*k, v)?;
            }
            let mut t = self.txn.open_table(PATH_TRUNK)?;
            for (k, v) in &c.path_trunk {
                t.insert(k.as_str(), v)?;
            }
            let mut t = self.txn.open_multimap_table(TRUNK_BRANCHES)?;
            for (k, vs) in &c.trunk_branches {
                for v in vs {
                    t.insert(k, v)?;
                }
            }
            let mut t = self.txn.open_table(BRANCHES)?;
            for (k, v) in &c.branches {
                t.insert(k, v)?;
            }
            let mut t = self.txn.open_table(BRANCH_AFTER)?;
            for (k, v) in &c.branch_after {
                t.insert(k, v)?;
            }
            let mut t = self.txn.open_table(BRANCH_VERTEX)?;
            for (k, v) in &c.branch_vertex {
                t.insert(k, v)?;
            }
            let mut t = self.txn.open_table(VERTEX_BRANCH)?;
            for (k, v) in &c.vertex_branch {
                t.insert(k, v)?;
            }
            let mut t = self.txn.open_multimap_table(BRANCH_LEAVES)?;
            for (k, vs) in &c.branch_leaves {
                for v in vs {
                    t.insert(k, v)?;
                }
            }
            let mut t = self.txn.open_table(LEAVES)?;
            for (k, v) in &c.leaves {
                t.insert(k, v)?;
            }
        }
        self.import_ids(&slice.ids)
    }

    fn import_ids(&mut self, ids: &[IdRow]) -> PristineResult<()> {
        let mut external = self.txn.open_table(EXTERNAL)?;
        let mut internal = self.txn.open_table(INTERNAL)?;
        let mut types = self.txn.open_table(NODE_TYPES)?;
        for (id, hash, t) in ids {
            external.insert(*id, hash)?;
            internal.insert(hash, *id)?;
            if let Some(t) = t {
                types.insert(*id, *t)?;
            }
        }
        let max = ids.iter().map(|(id, _, _)| *id).max().unwrap_or(0);
        self.next_node_id
            .fetch_max(max + 1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Write `snapshot` as a shared, parentless view with the repository's
    /// own id, state and change count — replacing any view with that name or
    /// id — and its visible changes as the view's change log.
    pub fn import_view_snapshot(&mut self, snapshot: &ViewSnapshotRows) -> PristineResult<()> {
        let state = ViewState {
            id: snapshot.id,
            name: snapshot.name.clone(),
            state: snapshot.state,
            change_count: snapshot.change_count,
            kind: ViewScope::Shared,
            parent: None,
        };
        {
            let mut views = self.txn.open_table(VIEWS)?;
            let mut stale = Vec::new();
            for row in views.iter()? {
                let (k, v) = row?;
                let existing = crate::pristine::txn::deserialize_view_state(v.value())?;
                if existing.id == snapshot.id || k.value() == snapshot.name {
                    stale.push(k.value().to_string());
                }
            }
            for name in stale {
                views.remove(name.as_str())?;
            }
            let bytes = crate::pristine::txn::serialize_view_state(&state);
            views.insert(snapshot.name.as_str(), bytes.as_slice())?;
        }
        let lo = encode_view_seq(snapshot.id, 0);
        let hi = encode_view_seq(snapshot.id, u64::MAX);
        {
            let mut log = self.txn.open_table(VIEW_CHANGES)?;
            log.retain_in::<&[u8; 16], _>(&lo..=&hi, |_, _| false)?;
            for (seq, change) in snapshot.visible.iter().enumerate() {
                log.insert(&encode_view_seq(snapshot.id, seq as u64), *change)?;
            }
        }
        {
            let mut rev = self.txn.open_table(REV_VIEW_CHANGES)?;
            rev.retain_in::<&[u8; 16], _>(&lo..=&hi, |_, _| false)?;
            for (seq, change) in snapshot.visible.iter().enumerate() {
                rev.insert(&encode_view_seq(snapshot.id, *change), seq as u64)?;
            }
        }
        self.next_view_id
            .fetch_max(snapshot.id + 1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}
