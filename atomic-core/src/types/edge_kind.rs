//! Type-safe edge model for the Atomic graph.
//!
//! This module replaces runtime `EdgeFlags` bitflag checks with exhaustive
//! enums that the compiler can verify. Every variant is a valid edge kind;
//! invalid combinations cannot be constructed.
//!
//! The two enum types — [`EdgeKind`] (forward) and [`ParentEdgeKind`]
//! (reverse) — are kept as separate types so the compiler prevents
//! accidentally mixing forward and reverse edges.
//!
//! # Relationship to `EdgeFlags`
//!
//! `EdgeFlags` remains the wire/storage format. The types here are a
//! semantic layer on top: construct them via [`EdgeKind::from_flags`] or
//! [`ForwardEdge::from_serialized`], and convert back via
//! [`EdgeKind::to_flags`].

use std::fmt;

use super::{EdgeFlags, NodeId, Position, SerializedGraphEdge};

// ---------------------------------------------------------------------------
// EdgeKind — 6 forward edge variants
// ---------------------------------------------------------------------------

/// The semantic kind of a forward graph edge.
///
/// This replaces runtime `EdgeFlags` bitflag checks with an exhaustive enum
/// that the compiler can verify. Every variant is a valid edge kind; invalid
/// combinations cannot be constructed.
///
/// Forward edges represent the direction of content flow (predecessor →
/// successor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EdgeKind {
    /// Live content within a file (`BLOCK`).
    Block,
    /// Deleted content — still in graph, not alive (`BLOCK | DELETED`).
    BlockDeleted,
    /// Live directory hierarchy (`FOLDER`).
    Folder,
    /// Deleted directory hierarchy (`FOLDER | DELETED`).
    FolderDeleted,
    /// Synthetic content connectivity — computed, not stored in changes
    /// (`PSEUDO | BLOCK`).
    PseudoBlock,
    /// Synthetic folder connectivity (`PSEUDO | FOLDER`).
    PseudoFolder,
}

impl EdgeKind {
    /// The reverse (parent) variant of this forward edge.
    ///
    /// Every forward edge has exactly one parent mirror.
    #[inline]
    pub fn as_parent(self) -> ParentEdgeKind {
        match self {
            Self::Block => ParentEdgeKind::Block,
            Self::BlockDeleted => ParentEdgeKind::BlockDeleted,
            Self::Folder => ParentEdgeKind::Folder,
            Self::FolderDeleted => ParentEdgeKind::FolderDeleted,
            Self::PseudoBlock => ParentEdgeKind::PseudoBlock,
            Self::PseudoFolder => ParentEdgeKind::PseudoFolder,
        }
    }

    /// `true` if this edge represents deleted content or folder.
    #[inline]
    pub fn is_deleted(self) -> bool {
        matches!(self, Self::BlockDeleted | Self::FolderDeleted)
    }

    /// `true` if this is a folder edge (alive, deleted, or pseudo).
    #[inline]
    pub fn is_folder(self) -> bool {
        matches!(
            self,
            Self::Folder | Self::FolderDeleted | Self::PseudoFolder
        )
    }

    /// `true` if this is a pseudo (synthetic connectivity) edge.
    #[inline]
    pub fn is_pseudo(self) -> bool {
        matches!(self, Self::PseudoBlock | Self::PseudoFolder)
    }

    /// `true` if this is a block (content) edge (alive, deleted, or pseudo).
    #[inline]
    pub fn is_block(self) -> bool {
        matches!(self, Self::Block | Self::BlockDeleted | Self::PseudoBlock)
    }

    /// Convert to the wire-format `EdgeFlags`.
    #[inline]
    pub fn to_flags(self) -> EdgeFlags {
        match self {
            Self::Block => EdgeFlags::BLOCK,
            Self::BlockDeleted => EdgeFlags::BLOCK | EdgeFlags::DELETED,
            Self::Folder => EdgeFlags::FOLDER,
            Self::FolderDeleted => EdgeFlags::FOLDER | EdgeFlags::DELETED,
            Self::PseudoBlock => EdgeFlags::PSEUDO | EdgeFlags::BLOCK,
            Self::PseudoFolder => EdgeFlags::PSEUDO | EdgeFlags::FOLDER,
        }
    }

    /// Parse from wire-format `EdgeFlags`.
    ///
    /// Returns `None` if the flags contain `PARENT` (use
    /// [`ParentEdgeKind::from_flags`] instead) or if the combination is not
    /// one of the six valid forward kinds.
    #[inline]
    pub fn from_flags(flags: EdgeFlags) -> Option<Self> {
        if flags.contains(EdgeFlags::PARENT) {
            return None;
        }
        match flags {
            f if f == EdgeFlags::BLOCK => Some(Self::Block),
            f if f == (EdgeFlags::BLOCK | EdgeFlags::DELETED) => Some(Self::BlockDeleted),
            f if f == EdgeFlags::FOLDER => Some(Self::Folder),
            f if f == (EdgeFlags::FOLDER | EdgeFlags::DELETED) => Some(Self::FolderDeleted),
            f if f == (EdgeFlags::PSEUDO | EdgeFlags::BLOCK) => Some(Self::PseudoBlock),
            f if f == (EdgeFlags::PSEUDO | EdgeFlags::FOLDER) => Some(Self::PseudoFolder),
            _ => None,
        }
    }
}

impl fmt::Display for EdgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Block => write!(f, "block"),
            Self::BlockDeleted => write!(f, "block+deleted"),
            Self::Folder => write!(f, "folder"),
            Self::FolderDeleted => write!(f, "folder+deleted"),
            Self::PseudoBlock => write!(f, "pseudo+block"),
            Self::PseudoFolder => write!(f, "pseudo+folder"),
        }
    }
}

// ---------------------------------------------------------------------------
// ParentEdgeKind — 6 reverse edge variants
// ---------------------------------------------------------------------------

/// The semantic kind of a parent (reverse) graph edge.
///
/// Every forward edge has exactly one parent mirror. Parent edges enable
/// efficient bidirectional traversal. They are a separate type from
/// [`EdgeKind`] so the compiler prevents accidentally mixing forward and
/// reverse edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ParentEdgeKind {
    /// Reverse of [`EdgeKind::Block`] (`BLOCK | PARENT`).
    Block,
    /// Reverse of [`EdgeKind::BlockDeleted`] (`BLOCK | PARENT | DELETED`).
    BlockDeleted,
    /// Reverse of [`EdgeKind::Folder`] (`FOLDER | PARENT`).
    Folder,
    /// Reverse of [`EdgeKind::FolderDeleted`] (`FOLDER | PARENT | DELETED`).
    FolderDeleted,
    /// Reverse of [`EdgeKind::PseudoBlock`] (`PSEUDO | BLOCK | PARENT`).
    PseudoBlock,
    /// Reverse of [`EdgeKind::PseudoFolder`] (`PSEUDO | FOLDER | PARENT`).
    PseudoFolder,
}

impl ParentEdgeKind {
    /// The forward variant that this parent edge mirrors.
    #[inline]
    pub fn as_forward(self) -> EdgeKind {
        match self {
            Self::Block => EdgeKind::Block,
            Self::BlockDeleted => EdgeKind::BlockDeleted,
            Self::Folder => EdgeKind::Folder,
            Self::FolderDeleted => EdgeKind::FolderDeleted,
            Self::PseudoBlock => EdgeKind::PseudoBlock,
            Self::PseudoFolder => EdgeKind::PseudoFolder,
        }
    }

    /// `true` if this edge represents deleted content or folder.
    #[inline]
    pub fn is_deleted(self) -> bool {
        matches!(self, Self::BlockDeleted | Self::FolderDeleted)
    }

    /// `true` if this is a folder edge (alive, deleted, or pseudo).
    #[inline]
    pub fn is_folder(self) -> bool {
        matches!(
            self,
            Self::Folder | Self::FolderDeleted | Self::PseudoFolder
        )
    }

    /// `true` if this is a pseudo (synthetic connectivity) edge.
    #[inline]
    pub fn is_pseudo(self) -> bool {
        matches!(self, Self::PseudoBlock | Self::PseudoFolder)
    }

    /// Convert to the wire-format `EdgeFlags` (includes `PARENT` bit).
    #[inline]
    pub fn to_flags(self) -> EdgeFlags {
        self.as_forward().to_flags() | EdgeFlags::PARENT
    }

    /// Parse from wire-format `EdgeFlags`.
    ///
    /// Returns `None` if the `PARENT` bit is **not** set (use
    /// [`EdgeKind::from_flags`] instead) or if the remaining bits are not a
    /// valid forward kind.
    #[inline]
    pub fn from_flags(flags: EdgeFlags) -> Option<Self> {
        if !flags.contains(EdgeFlags::PARENT) {
            return None;
        }
        let without_parent = flags & !EdgeFlags::PARENT;
        EdgeKind::from_flags(without_parent).map(|fwd| fwd.as_parent())
    }
}

impl fmt::Display for ParentEdgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Block => write!(f, "parent+block"),
            Self::BlockDeleted => write!(f, "parent+block+deleted"),
            Self::Folder => write!(f, "parent+folder"),
            Self::FolderDeleted => write!(f, "parent+folder+deleted"),
            Self::PseudoBlock => write!(f, "parent+pseudo+block"),
            Self::PseudoFolder => write!(f, "parent+pseudo+folder"),
        }
    }
}

// ---------------------------------------------------------------------------
// Typed edge structs
// ---------------------------------------------------------------------------

/// A forward edge in the graph with typed kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardEdge {
    /// The semantic kind of this forward edge.
    pub kind: EdgeKind,
    /// Destination position (the node this edge points to).
    pub dest: Position<NodeId>,
    /// The change that introduced this edge.
    pub introduced_by: NodeId,
}

impl ForwardEdge {
    /// Parse from a [`SerializedGraphEdge`].
    ///
    /// Returns `None` if the serialized edge is a parent edge (the `PARENT`
    /// bit is set).
    #[inline]
    pub fn from_serialized(edge: &SerializedGraphEdge) -> Option<Self> {
        let kind = EdgeKind::from_flags(edge.flag())?;
        Some(Self {
            kind,
            dest: edge.dest(),
            introduced_by: edge.introduced_by(),
        })
    }
}

/// A parent (reverse) edge in the graph with typed kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentEdge {
    /// The semantic kind of this parent edge.
    pub kind: ParentEdgeKind,
    /// Destination position (the node this reverse edge points to).
    pub dest: Position<NodeId>,
    /// The change that introduced this edge.
    pub introduced_by: NodeId,
}

impl ParentEdge {
    /// Parse from a [`SerializedGraphEdge`].
    ///
    /// Returns `None` if the serialized edge is a forward edge (the `PARENT`
    /// bit is **not** set).
    #[inline]
    pub fn from_serialized(edge: &SerializedGraphEdge) -> Option<Self> {
        let kind = ParentEdgeKind::from_flags(edge.flag())?;
        Some(Self {
            kind,
            dest: edge.dest(),
            introduced_by: edge.introduced_by(),
        })
    }
}

/// Any edge — either forward or parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// A forward (content-flow direction) edge.
    Forward(ForwardEdge),
    /// A parent (reverse) edge.
    Parent(ParentEdge),
}

impl Edge {
    /// Parse from a [`SerializedGraphEdge`], categorising into the correct
    /// direction variant.
    ///
    /// Returns `None` only if the flags are an invalid combination (not one
    /// of the twelve recognised kinds).
    #[inline]
    pub fn from_serialized(edge: &SerializedGraphEdge) -> Option<Self> {
        if edge.flag().contains(EdgeFlags::PARENT) {
            ParentEdge::from_serialized(edge).map(Edge::Parent)
        } else {
            ForwardEdge::from_serialized(edge).map(Edge::Forward)
        }
    }

    /// The change that introduced this edge.
    #[inline]
    pub fn introduced_by(&self) -> NodeId {
        match self {
            Self::Forward(e) => e.introduced_by,
            Self::Parent(e) => e.introduced_by,
        }
    }

    /// The destination position of this edge.
    #[inline]
    pub fn dest(&self) -> Position<NodeId> {
        match self {
            Self::Forward(e) => e.dest,
            Self::Parent(e) => e.dest,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    /// All six forward variants in definition order.
    const ALL_FORWARD_KINDS: [EdgeKind; 6] = [
        EdgeKind::Block,
        EdgeKind::BlockDeleted,
        EdgeKind::Folder,
        EdgeKind::FolderDeleted,
        EdgeKind::PseudoBlock,
        EdgeKind::PseudoFolder,
    ];

    /// All six parent variants in definition order.
    const ALL_PARENT_KINDS: [ParentEdgeKind; 6] = [
        ParentEdgeKind::Block,
        ParentEdgeKind::BlockDeleted,
        ParentEdgeKind::Folder,
        ParentEdgeKind::FolderDeleted,
        ParentEdgeKind::PseudoBlock,
        ParentEdgeKind::PseudoFolder,
    ];

    // -- helpers ----------------------------------------------------------

    /// Build a `SerializedGraphEdge` with the given flags and arbitrary but
    /// deterministic dest / introduced_by values.
    fn make_serialized(flags: EdgeFlags) -> SerializedGraphEdge {
        let dest = Position::new(NodeId::new(42), ChangePosition::new(100));
        let introduced_by = NodeId::new(7);
        SerializedGraphEdge::new(flags, dest, introduced_by)
    }

    // -- EdgeKind round-trip ----------------------------------------------

    #[test]
    fn edge_kind_to_flags_round_trip() {
        for kind in ALL_FORWARD_KINDS {
            let flags = kind.to_flags();
            let recovered = EdgeKind::from_flags(flags)
                .unwrap_or_else(|| panic!("from_flags failed for {kind}"));
            assert_eq!(kind, recovered, "round-trip failed for {kind}");
        }
    }

    #[test]
    fn parent_edge_kind_to_flags_round_trip() {
        for kind in ALL_PARENT_KINDS {
            let flags = kind.to_flags();
            let recovered = ParentEdgeKind::from_flags(flags)
                .unwrap_or_else(|| panic!("from_flags failed for {kind}"));
            assert_eq!(kind, recovered, "round-trip failed for {kind}");
        }
    }

    // -- from_flags rejects invalid combinations --------------------------

    #[test]
    fn edge_kind_from_flags_rejects_block_or_folder() {
        // BLOCK | FOLDER is not a valid edge kind
        let bad = EdgeFlags::BLOCK | EdgeFlags::FOLDER;
        assert!(EdgeKind::from_flags(bad).is_none());
    }

    #[test]
    fn edge_kind_from_flags_rejects_pseudo_alone() {
        assert!(EdgeKind::from_flags(EdgeFlags::PSEUDO).is_none());
    }

    #[test]
    fn edge_kind_from_flags_rejects_deleted_alone() {
        assert!(EdgeKind::from_flags(EdgeFlags::DELETED).is_none());
    }

    #[test]
    fn edge_kind_from_flags_rejects_empty() {
        assert!(EdgeKind::from_flags(EdgeFlags::empty()).is_none());
    }

    #[test]
    fn edge_kind_from_flags_rejects_all_bits() {
        let all = EdgeFlags::BLOCK
            | EdgeFlags::PSEUDO
            | EdgeFlags::FOLDER
            | EdgeFlags::PARENT
            | EdgeFlags::DELETED;
        assert!(EdgeKind::from_flags(all).is_none());
    }

    #[test]
    fn edge_kind_from_flags_rejects_parent_flags() {
        // Every valid parent combination must be rejected by EdgeKind
        for kind in ALL_PARENT_KINDS {
            assert!(
                EdgeKind::from_flags(kind.to_flags()).is_none(),
                "EdgeKind accepted parent flags for {kind}"
            );
        }
    }

    #[test]
    fn parent_edge_kind_from_flags_rejects_non_parent_flags() {
        // Every valid forward combination must be rejected by ParentEdgeKind
        for kind in ALL_FORWARD_KINDS {
            assert!(
                ParentEdgeKind::from_flags(kind.to_flags()).is_none(),
                "ParentEdgeKind accepted forward flags for {kind}"
            );
        }
    }

    #[test]
    fn parent_edge_kind_from_flags_rejects_bare_parent() {
        // PARENT alone (0x20) — used as range minimum, not a real kind
        assert!(ParentEdgeKind::from_flags(EdgeFlags::PARENT).is_none());
    }

    #[test]
    fn parent_edge_kind_from_flags_rejects_invalid_with_parent() {
        // PARENT | BLOCK | FOLDER — invalid even with PARENT set
        let bad = EdgeFlags::PARENT | EdgeFlags::BLOCK | EdgeFlags::FOLDER;
        assert!(ParentEdgeKind::from_flags(bad).is_none());
    }

    // -- ForwardEdge::from_serialized -------------------------------------

    #[test]
    fn forward_edge_from_serialized_parses_all_forward_kinds() {
        for kind in ALL_FORWARD_KINDS {
            let serialized = make_serialized(kind.to_flags());
            let edge = ForwardEdge::from_serialized(&serialized)
                .unwrap_or_else(|| panic!("failed to parse forward edge for {kind}"));
            assert_eq!(edge.kind, kind);
            assert_eq!(edge.dest.change, NodeId::new(42));
            assert_eq!(edge.dest.pos.get(), 100);
            assert_eq!(edge.introduced_by, NodeId::new(7));
        }
    }

    #[test]
    fn forward_edge_from_serialized_rejects_parent_edges() {
        for kind in ALL_PARENT_KINDS {
            let serialized = make_serialized(kind.to_flags());
            assert!(
                ForwardEdge::from_serialized(&serialized).is_none(),
                "ForwardEdge accepted parent edge for {kind}"
            );
        }
    }

    // -- ParentEdge::from_serialized --------------------------------------

    #[test]
    fn parent_edge_from_serialized_parses_all_parent_kinds() {
        for kind in ALL_PARENT_KINDS {
            let serialized = make_serialized(kind.to_flags());
            let edge = ParentEdge::from_serialized(&serialized)
                .unwrap_or_else(|| panic!("failed to parse parent edge for {kind}"));
            assert_eq!(edge.kind, kind);
            assert_eq!(edge.dest.change, NodeId::new(42));
            assert_eq!(edge.dest.pos.get(), 100);
            assert_eq!(edge.introduced_by, NodeId::new(7));
        }
    }

    #[test]
    fn parent_edge_from_serialized_rejects_forward_edges() {
        for kind in ALL_FORWARD_KINDS {
            let serialized = make_serialized(kind.to_flags());
            assert!(
                ParentEdge::from_serialized(&serialized).is_none(),
                "ParentEdge accepted forward edge for {kind}"
            );
        }
    }

    // -- Edge::from_serialized --------------------------------------------

    #[test]
    fn edge_from_serialized_categorises_forward() {
        for kind in ALL_FORWARD_KINDS {
            let serialized = make_serialized(kind.to_flags());
            let edge = Edge::from_serialized(&serialized)
                .unwrap_or_else(|| panic!("Edge failed to parse forward {kind}"));
            match edge {
                Edge::Forward(fwd) => assert_eq!(fwd.kind, kind),
                Edge::Parent(_) => panic!("expected Forward, got Parent for {kind}"),
            }
        }
    }

    #[test]
    fn edge_from_serialized_categorises_parent() {
        for kind in ALL_PARENT_KINDS {
            let serialized = make_serialized(kind.to_flags());
            let edge = Edge::from_serialized(&serialized)
                .unwrap_or_else(|| panic!("Edge failed to parse parent {kind}"));
            match edge {
                Edge::Parent(par) => assert_eq!(par.kind, kind),
                Edge::Forward(_) => panic!("expected Parent, got Forward for {kind}"),
            }
        }
    }

    #[test]
    fn edge_from_serialized_rejects_invalid() {
        // BLOCK | FOLDER — invalid combination
        let serialized = make_serialized(EdgeFlags::BLOCK | EdgeFlags::FOLDER);
        assert!(Edge::from_serialized(&serialized).is_none());
    }

    #[test]
    fn edge_accessors_match_inner() {
        let dest = Position::new(NodeId::new(99), ChangePosition::new(555));
        let introduced_by = NodeId::new(13);
        let serialized = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, introduced_by);
        let edge = Edge::from_serialized(&serialized).unwrap();
        assert_eq!(edge.dest(), dest);
        assert_eq!(edge.introduced_by(), introduced_by);

        let parent_serialized =
            SerializedGraphEdge::new(EdgeFlags::FOLDER | EdgeFlags::PARENT, dest, introduced_by);
        let parent_edge = Edge::from_serialized(&parent_serialized).unwrap();
        assert_eq!(parent_edge.dest(), dest);
        assert_eq!(parent_edge.introduced_by(), introduced_by);
    }

    // -- as_parent / as_forward round-trips -------------------------------

    #[test]
    fn as_parent_then_as_forward_is_identity() {
        for kind in ALL_FORWARD_KINDS {
            assert_eq!(
                kind.as_parent().as_forward(),
                kind,
                "as_parent().as_forward() != identity for {kind}"
            );
        }
    }

    #[test]
    fn as_forward_then_as_parent_is_identity() {
        for kind in ALL_PARENT_KINDS {
            assert_eq!(
                kind.as_forward().as_parent(),
                kind,
                "as_forward().as_parent() != identity for {kind}"
            );
        }
    }

    // -- is_* predicates --------------------------------------------------

    #[test]
    fn edge_kind_is_deleted() {
        assert!(!EdgeKind::Block.is_deleted());
        assert!(EdgeKind::BlockDeleted.is_deleted());
        assert!(!EdgeKind::Folder.is_deleted());
        assert!(EdgeKind::FolderDeleted.is_deleted());
        assert!(!EdgeKind::PseudoBlock.is_deleted());
        assert!(!EdgeKind::PseudoFolder.is_deleted());
    }

    #[test]
    fn edge_kind_is_folder() {
        assert!(!EdgeKind::Block.is_folder());
        assert!(!EdgeKind::BlockDeleted.is_folder());
        assert!(EdgeKind::Folder.is_folder());
        assert!(EdgeKind::FolderDeleted.is_folder());
        assert!(!EdgeKind::PseudoBlock.is_folder());
        assert!(EdgeKind::PseudoFolder.is_folder());
    }

    #[test]
    fn edge_kind_is_pseudo() {
        assert!(!EdgeKind::Block.is_pseudo());
        assert!(!EdgeKind::BlockDeleted.is_pseudo());
        assert!(!EdgeKind::Folder.is_pseudo());
        assert!(!EdgeKind::FolderDeleted.is_pseudo());
        assert!(EdgeKind::PseudoBlock.is_pseudo());
        assert!(EdgeKind::PseudoFolder.is_pseudo());
    }

    #[test]
    fn edge_kind_is_block() {
        assert!(EdgeKind::Block.is_block());
        assert!(EdgeKind::BlockDeleted.is_block());
        assert!(!EdgeKind::Folder.is_block());
        assert!(!EdgeKind::FolderDeleted.is_block());
        assert!(EdgeKind::PseudoBlock.is_block());
        assert!(!EdgeKind::PseudoFolder.is_block());
    }

    #[test]
    fn parent_edge_kind_is_deleted() {
        assert!(!ParentEdgeKind::Block.is_deleted());
        assert!(ParentEdgeKind::BlockDeleted.is_deleted());
        assert!(!ParentEdgeKind::Folder.is_deleted());
        assert!(ParentEdgeKind::FolderDeleted.is_deleted());
        assert!(!ParentEdgeKind::PseudoBlock.is_deleted());
        assert!(!ParentEdgeKind::PseudoFolder.is_deleted());
    }

    #[test]
    fn parent_edge_kind_is_folder() {
        assert!(!ParentEdgeKind::Block.is_folder());
        assert!(!ParentEdgeKind::BlockDeleted.is_folder());
        assert!(ParentEdgeKind::Folder.is_folder());
        assert!(ParentEdgeKind::FolderDeleted.is_folder());
        assert!(!ParentEdgeKind::PseudoBlock.is_folder());
        assert!(ParentEdgeKind::PseudoFolder.is_folder());
    }

    #[test]
    fn parent_edge_kind_is_pseudo() {
        assert!(!ParentEdgeKind::Block.is_pseudo());
        assert!(!ParentEdgeKind::BlockDeleted.is_pseudo());
        assert!(!ParentEdgeKind::Folder.is_pseudo());
        assert!(!ParentEdgeKind::FolderDeleted.is_pseudo());
        assert!(ParentEdgeKind::PseudoBlock.is_pseudo());
        assert!(ParentEdgeKind::PseudoFolder.is_pseudo());
    }

    // -- Display ----------------------------------------------------------

    #[test]
    fn edge_kind_display() {
        assert_eq!(EdgeKind::Block.to_string(), "block");
        assert_eq!(EdgeKind::BlockDeleted.to_string(), "block+deleted");
        assert_eq!(EdgeKind::Folder.to_string(), "folder");
        assert_eq!(EdgeKind::FolderDeleted.to_string(), "folder+deleted");
        assert_eq!(EdgeKind::PseudoBlock.to_string(), "pseudo+block");
        assert_eq!(EdgeKind::PseudoFolder.to_string(), "pseudo+folder");
    }

    #[test]
    fn parent_edge_kind_display() {
        assert_eq!(ParentEdgeKind::Block.to_string(), "parent+block");
        assert_eq!(
            ParentEdgeKind::BlockDeleted.to_string(),
            "parent+block+deleted"
        );
        assert_eq!(ParentEdgeKind::Folder.to_string(), "parent+folder");
        assert_eq!(
            ParentEdgeKind::FolderDeleted.to_string(),
            "parent+folder+deleted"
        );
        assert_eq!(
            ParentEdgeKind::PseudoBlock.to_string(),
            "parent+pseudo+block"
        );
        assert_eq!(
            ParentEdgeKind::PseudoFolder.to_string(),
            "parent+pseudo+folder"
        );
    }

    // -- Flags agree with EdgeFlags constants -----------------------------

    #[test]
    fn to_flags_matches_edge_flags_constants() {
        assert_eq!(EdgeKind::Block.to_flags(), EdgeFlags::BLOCK);
        assert_eq!(
            EdgeKind::BlockDeleted.to_flags(),
            EdgeFlags::BLOCK | EdgeFlags::DELETED
        );
        assert_eq!(EdgeKind::Folder.to_flags(), EdgeFlags::FOLDER);
        assert_eq!(
            EdgeKind::FolderDeleted.to_flags(),
            EdgeFlags::FOLDER | EdgeFlags::DELETED
        );
        assert_eq!(
            EdgeKind::PseudoBlock.to_flags(),
            EdgeFlags::PSEUDO | EdgeFlags::BLOCK
        );
        assert_eq!(
            EdgeKind::PseudoFolder.to_flags(),
            EdgeFlags::PSEUDO | EdgeFlags::FOLDER
        );
    }

    #[test]
    fn parent_to_flags_includes_parent_bit() {
        for kind in ALL_PARENT_KINDS {
            let flags = kind.to_flags();
            assert!(
                flags.contains(EdgeFlags::PARENT),
                "PARENT bit missing for {kind}"
            );
        }
    }

    #[test]
    fn forward_to_flags_excludes_parent_bit() {
        for kind in ALL_FORWARD_KINDS {
            let flags = kind.to_flags();
            assert!(
                !flags.contains(EdgeFlags::PARENT),
                "PARENT bit unexpectedly set for {kind}"
            );
        }
    }

    // -- Correspondence between forward/parent flag values ----------------

    #[test]
    fn parent_flags_equal_forward_flags_or_parent() {
        for (fwd, par) in ALL_FORWARD_KINDS.iter().zip(ALL_PARENT_KINDS.iter()) {
            assert_eq!(
                par.to_flags(),
                fwd.to_flags() | EdgeFlags::PARENT,
                "flags mismatch for {fwd} / {par}"
            );
        }
    }

    // -- Edge::from_serialized preserves dest and introduced_by -----------

    #[test]
    fn edge_from_serialized_preserves_payload() {
        let dest = Position::new(NodeId::new(1000), ChangePosition::new(9999));
        let intro = NodeId::new(77);

        // Forward
        let ser = SerializedGraphEdge::new(EdgeFlags::FOLDER | EdgeFlags::DELETED, dest, intro);
        let edge = Edge::from_serialized(&ser).unwrap();
        assert_eq!(edge.dest(), dest);
        assert_eq!(edge.introduced_by(), intro);
        assert!(matches!(
            edge,
            Edge::Forward(ForwardEdge {
                kind: EdgeKind::FolderDeleted,
                ..
            })
        ));

        // Parent
        let ser = SerializedGraphEdge::new(
            EdgeFlags::PSEUDO | EdgeFlags::BLOCK | EdgeFlags::PARENT,
            dest,
            intro,
        );
        let edge = Edge::from_serialized(&ser).unwrap();
        assert_eq!(edge.dest(), dest);
        assert_eq!(edge.introduced_by(), intro);
        assert!(matches!(
            edge,
            Edge::Parent(ParentEdge {
                kind: ParentEdgeKind::PseudoBlock,
                ..
            })
        ));
    }
}
