//! Conflict detection and tracking for CRDT apply operations.
//!
//! This module provides types for representing and tracking conflicts that
//! can arise when applying CRDT operations. Conflicts occur when concurrent
//! operations affect the same entities in incompatible ways.
//!
//! # Conflict Types
//!
//! | Kind | Description | Resolution |
//! |------|-------------|------------|
//! | `ConcurrentInsert` | Two inserts at same position | Order by ID |
//! | `DeleteModify` | Delete and modify same entity | Create zombie |
//! | `MoveDelete` | Move and delete same file | Delete wins |
//! | `DuplicatePath` | Two files claim same path | Rename one |
//! | `OrderingCycle` | Circular ordering dependency | Error |
//!
//! # Example
//!
//! ```rust
//! use atomic_core::crdt::apply::conflict::{
//!     ConflictKind, CrdtConflict, CrdtConflictTracker,
//! };
//!
//! let mut tracker = CrdtConflictTracker::new();
//!
//! // Record a conflict
//! let conflict = CrdtConflict::new(
//!     ConflictKind::ConcurrentInsert,
//!     "Two branches inserted after B1:0".to_string(),
//! );
//! tracker.add(conflict);
//!
//! assert!(tracker.has_conflicts());
//! assert_eq!(tracker.count(), 1);
//! ```

use crate::crdt::{BranchId, LeafId, TrunkId};
use serde::{Deserialize, Serialize};
use std::fmt;

// =============================================================================
// ConflictKind
// =============================================================================

/// The kind of CRDT conflict detected.
///
/// Each conflict kind has specific semantics and resolution strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConflictKind {
    /// Two concurrent insertions at the same position.
    ///
    /// This is the most common conflict type. It occurs when two changes
    /// both insert after the same reference point.
    ///
    /// **Resolution**: Order insertions by their CRDT IDs (deterministic).
    ConcurrentInsert,

    /// One operation deletes content that another modifies.
    ///
    /// This creates a "zombie" - content that has been both deleted and
    /// modified. The content may need user intervention to resolve.
    ///
    /// **Resolution**: Keep the modified content as a zombie for user review.
    DeleteModify,

    /// One operation moves a file that another deletes.
    ///
    /// The file cannot be both moved and deleted simultaneously.
    ///
    /// **Resolution**: Delete takes precedence (can be undone).
    MoveDelete,

    /// Two operations claim the same file path.
    ///
    /// This happens when two files are created or moved to the same path.
    ///
    /// **Resolution**: Rename one file with a conflict suffix.
    DuplicatePath,

    /// A circular dependency in ordering was detected.
    ///
    /// This indicates data corruption or a bug, as CRDT ordering should
    /// always be acyclic.
    ///
    /// **Resolution**: Error - requires manual intervention.
    OrderingCycle,

    /// Concurrent modifications to the same entity.
    ///
    /// Two operations modify the same branch or leaf concurrently.
    ///
    /// **Resolution**: Apply both modifications in ID order.
    ConcurrentModify,

    /// Reference to a deleted entity.
    ///
    /// An operation references an entity that has been deleted.
    ///
    /// **Resolution**: Restore the deleted entity or skip the operation.
    DeletedReference,

    /// Restore of non-deleted entity.
    ///
    /// An operation tries to restore an entity that isn't deleted.
    ///
    /// **Resolution**: Skip the restore operation.
    RestoreNonDeleted,
}

impl ConflictKind {
    /// Returns `true` if this conflict type can be automatically resolved.
    #[inline]
    pub fn is_auto_resolvable(&self) -> bool {
        matches!(
            self,
            ConflictKind::ConcurrentInsert
                | ConflictKind::MoveDelete
                | ConflictKind::ConcurrentModify
                | ConflictKind::RestoreNonDeleted
        )
    }

    /// Returns `true` if this conflict requires user intervention.
    #[inline]
    pub fn requires_user_action(&self) -> bool {
        matches!(
            self,
            ConflictKind::DeleteModify
                | ConflictKind::DuplicatePath
                | ConflictKind::OrderingCycle
                | ConflictKind::DeletedReference
        )
    }

    /// Returns `true` if this conflict indicates potential data corruption.
    #[inline]
    pub fn indicates_corruption(&self) -> bool {
        matches!(self, ConflictKind::OrderingCycle)
    }

    /// Returns a short description of this conflict kind.
    pub fn description(&self) -> &'static str {
        match self {
            ConflictKind::ConcurrentInsert => "concurrent insertion at same position",
            ConflictKind::DeleteModify => "entity deleted and modified concurrently",
            ConflictKind::MoveDelete => "file moved and deleted concurrently",
            ConflictKind::DuplicatePath => "multiple files at same path",
            ConflictKind::OrderingCycle => "circular ordering dependency",
            ConflictKind::ConcurrentModify => "concurrent modifications to same entity",
            ConflictKind::DeletedReference => "reference to deleted entity",
            ConflictKind::RestoreNonDeleted => "restore of non-deleted entity",
        }
    }
}

impl fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

// =============================================================================
// ConflictEntity
// =============================================================================

/// The entity involved in a conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictEntity {
    /// A trunk (file) is involved.
    Trunk(TrunkId),

    /// A branch (line) is involved.
    Branch(BranchId),

    /// A leaf (token) is involved.
    Leaf(LeafId),

    /// A file path is involved.
    Path(String),

    /// No specific entity (general conflict).
    None,
}

impl fmt::Display for ConflictEntity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConflictEntity::Trunk(id) => write!(f, "trunk {}", id),
            ConflictEntity::Branch(id) => write!(f, "branch {}", id),
            ConflictEntity::Leaf(id) => write!(f, "leaf {}", id),
            ConflictEntity::Path(path) => write!(f, "path {:?}", path),
            ConflictEntity::None => write!(f, "none"),
        }
    }
}

// =============================================================================
// CrdtConflict
// =============================================================================

/// A conflict detected during CRDT operation application.
///
/// Contains information about the conflict type, affected entities,
/// and a human-readable description.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::apply::conflict::{ConflictKind, CrdtConflict, ConflictEntity};
/// use atomic_core::crdt::BranchId;
/// use atomic_core::types::NodeId;
///
/// let conflict = CrdtConflict::builder(ConflictKind::ConcurrentInsert)
///     .description("Two branches inserted after same point")
///     .entity(ConflictEntity::Branch(BranchId::new(NodeId::new(1), 0)))
///     .other_entity(ConflictEntity::Branch(BranchId::new(NodeId::new(2), 0)))
///     .build();
///
/// assert!(conflict.kind().is_auto_resolvable());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrdtConflict {
    /// The kind of conflict.
    kind: ConflictKind,

    /// Human-readable description of the conflict.
    description: String,

    /// The primary entity involved in the conflict.
    entity: ConflictEntity,

    /// The other entity involved (for two-party conflicts).
    other_entity: ConflictEntity,

    /// Whether this conflict has been resolved.
    resolved: bool,

    /// Resolution notes, if any.
    resolution: Option<String>,
}

impl CrdtConflict {
    /// Creates a new conflict with the given kind and description.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::crdt::apply::conflict::{ConflictKind, CrdtConflict};
    ///
    /// let conflict = CrdtConflict::new(
    ///     ConflictKind::ConcurrentInsert,
    ///     "Two insertions at position 5".to_string(),
    /// );
    /// ```
    #[inline]
    pub fn new(kind: ConflictKind, description: String) -> Self {
        Self {
            kind,
            description,
            entity: ConflictEntity::None,
            other_entity: ConflictEntity::None,
            resolved: false,
            resolution: None,
        }
    }

    /// Creates a builder for constructing a conflict.
    #[inline]
    pub fn builder(kind: ConflictKind) -> CrdtConflictBuilder {
        CrdtConflictBuilder::new(kind)
    }

    /// Creates a concurrent insert conflict.
    pub fn concurrent_insert(entity1: ConflictEntity, entity2: ConflictEntity) -> Self {
        Self {
            kind: ConflictKind::ConcurrentInsert,
            description: format!(
                "Concurrent insertions: {} and {}",
                entity1, entity2
            ),
            entity: entity1,
            other_entity: entity2,
            resolved: false,
            resolution: None,
        }
    }

    /// Creates a delete/modify conflict.
    pub fn delete_modify(deleted: ConflictEntity, modified: ConflictEntity) -> Self {
        Self {
            kind: ConflictKind::DeleteModify,
            description: format!(
                "Entity {} deleted while {} was modified",
                deleted, modified
            ),
            entity: deleted,
            other_entity: modified,
            resolved: false,
            resolution: None,
        }
    }

    /// Creates a duplicate path conflict.
    pub fn duplicate_path(path: String, trunk1: TrunkId, trunk2: TrunkId) -> Self {
        Self {
            kind: ConflictKind::DuplicatePath,
            description: format!(
                "Path {:?} claimed by both {} and {}",
                path, trunk1, trunk2
            ),
            entity: ConflictEntity::Trunk(trunk1),
            other_entity: ConflictEntity::Trunk(trunk2),
            resolved: false,
            resolution: None,
        }
    }

    // =========================================================================
    // Accessors
    // =========================================================================

    /// Returns the conflict kind.
    #[inline]
    pub fn kind(&self) -> ConflictKind {
        self.kind
    }

    /// Returns the description.
    #[inline]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the primary entity involved.
    #[inline]
    pub fn entity(&self) -> &ConflictEntity {
        &self.entity
    }

    /// Returns the other entity involved.
    #[inline]
    pub fn other_entity(&self) -> &ConflictEntity {
        &self.other_entity
    }

    /// Returns `true` if this conflict has been resolved.
    #[inline]
    pub fn is_resolved(&self) -> bool {
        self.resolved
    }

    /// Returns the resolution notes, if any.
    #[inline]
    pub fn resolution(&self) -> Option<&str> {
        self.resolution.as_deref()
    }

    // =========================================================================
    // Convenience Methods
    // =========================================================================

    /// Returns `true` if this conflict can be automatically resolved.
    #[inline]
    pub fn is_auto_resolvable(&self) -> bool {
        self.kind.is_auto_resolvable()
    }

    /// Returns `true` if this conflict requires user action.
    #[inline]
    pub fn requires_user_action(&self) -> bool {
        self.kind.requires_user_action()
    }

    // =========================================================================
    // Mutation Methods
    // =========================================================================

    /// Marks this conflict as resolved with optional notes.
    pub fn mark_resolved(&mut self, resolution: Option<String>) {
        self.resolved = true;
        self.resolution = resolution;
    }

    /// Marks this conflict as unresolved.
    pub fn mark_unresolved(&mut self) {
        self.resolved = false;
        self.resolution = None;
    }
}

impl fmt::Display for CrdtConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.kind, self.description)?;
        if self.resolved {
            write!(f, " (resolved)")?;
        }
        Ok(())
    }
}

// =============================================================================
// CrdtConflictBuilder
// =============================================================================

/// Builder for constructing [`CrdtConflict`] instances.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::apply::conflict::{ConflictKind, CrdtConflictBuilder, ConflictEntity};
/// use atomic_core::crdt::BranchId;
/// use atomic_core::types::NodeId;
///
/// let conflict = CrdtConflictBuilder::new(ConflictKind::ConcurrentInsert)
///     .description("Branches B1 and B2 both insert after B0")
///     .entity(ConflictEntity::Branch(BranchId::new(NodeId::new(1), 0)))
///     .other_entity(ConflictEntity::Branch(BranchId::new(NodeId::new(2), 0)))
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct CrdtConflictBuilder {
    kind: ConflictKind,
    description: Option<String>,
    entity: ConflictEntity,
    other_entity: ConflictEntity,
}

impl CrdtConflictBuilder {
    /// Creates a new builder for the given conflict kind.
    #[inline]
    pub fn new(kind: ConflictKind) -> Self {
        Self {
            kind,
            description: None,
            entity: ConflictEntity::None,
            other_entity: ConflictEntity::None,
        }
    }

    /// Sets the description.
    #[inline]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Sets the primary entity.
    #[inline]
    pub fn entity(mut self, entity: ConflictEntity) -> Self {
        self.entity = entity;
        self
    }

    /// Sets the other entity.
    #[inline]
    pub fn other_entity(mut self, entity: ConflictEntity) -> Self {
        self.other_entity = entity;
        self
    }

    /// Sets the trunk as the primary entity.
    #[inline]
    pub fn trunk(self, trunk_id: TrunkId) -> Self {
        self.entity(ConflictEntity::Trunk(trunk_id))
    }

    /// Sets the branch as the primary entity.
    #[inline]
    pub fn branch(self, branch_id: BranchId) -> Self {
        self.entity(ConflictEntity::Branch(branch_id))
    }

    /// Sets the leaf as the primary entity.
    #[inline]
    pub fn leaf(self, leaf_id: LeafId) -> Self {
        self.entity(ConflictEntity::Leaf(leaf_id))
    }

    /// Sets the path as the primary entity.
    #[inline]
    pub fn path(self, path: impl Into<String>) -> Self {
        self.entity(ConflictEntity::Path(path.into()))
    }

    /// Builds the conflict.
    pub fn build(self) -> CrdtConflict {
        CrdtConflict {
            kind: self.kind,
            description: self.description.unwrap_or_else(|| self.kind.description().to_string()),
            entity: self.entity,
            other_entity: self.other_entity,
            resolved: false,
            resolution: None,
        }
    }
}

// =============================================================================
// CrdtConflictTracker
// =============================================================================

/// Tracks conflicts during CRDT apply operations.
///
/// The tracker accumulates conflicts and provides methods to query and
/// filter them by various criteria.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::apply::conflict::{
///     ConflictKind, CrdtConflict, CrdtConflictTracker,
/// };
///
/// let mut tracker = CrdtConflictTracker::new();
///
/// // Add some conflicts
/// tracker.add(CrdtConflict::new(
///     ConflictKind::ConcurrentInsert,
///     "conflict 1".to_string(),
/// ));
/// tracker.add(CrdtConflict::new(
///     ConflictKind::DeleteModify,
///     "conflict 2".to_string(),
/// ));
///
/// assert_eq!(tracker.count(), 2);
/// assert!(tracker.has_conflicts());
///
/// // Filter by kind
/// let auto_resolvable: Vec<_> = tracker.iter()
///     .filter(|c| c.is_auto_resolvable())
///     .collect();
/// assert_eq!(auto_resolvable.len(), 1);
/// ```
#[derive(Debug, Clone, Default)]
pub struct CrdtConflictTracker {
    /// The collected conflicts.
    conflicts: Vec<CrdtConflict>,
}

impl CrdtConflictTracker {
    /// Creates a new empty tracker.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a tracker with pre-allocated capacity.
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            conflicts: Vec::with_capacity(capacity),
        }
    }

    /// Adds a conflict to the tracker.
    #[inline]
    pub fn add(&mut self, conflict: CrdtConflict) {
        self.conflicts.push(conflict);
    }

    /// Adds a conflict with a specific kind and description.
    #[inline]
    pub fn add_simple(&mut self, kind: ConflictKind, description: impl Into<String>) {
        self.conflicts.push(CrdtConflict::new(kind, description.into()));
    }

    /// Returns `true` if any conflicts have been recorded.
    #[inline]
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// Returns the number of conflicts.
    #[inline]
    pub fn count(&self) -> usize {
        self.conflicts.len()
    }

    /// Returns `true` if there are no conflicts.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.conflicts.is_empty()
    }

    /// Returns an iterator over the conflicts.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &CrdtConflict> {
        self.conflicts.iter()
    }

    /// Returns a mutable iterator over the conflicts.
    #[inline]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut CrdtConflict> {
        self.conflicts.iter_mut()
    }

    /// Returns the conflicts as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[CrdtConflict] {
        &self.conflicts
    }

    /// Consumes the tracker and returns the conflicts.
    #[inline]
    pub fn into_conflicts(self) -> Vec<CrdtConflict> {
        self.conflicts
    }

    /// Clears all recorded conflicts.
    #[inline]
    pub fn clear(&mut self) {
        self.conflicts.clear();
    }

    // =========================================================================
    // Filtering Methods
    // =========================================================================

    /// Returns conflicts of the specified kind.
    pub fn by_kind(&self, kind: ConflictKind) -> impl Iterator<Item = &CrdtConflict> {
        self.conflicts.iter().filter(move |c| c.kind == kind)
    }

    /// Returns the count of conflicts of the specified kind.
    pub fn count_by_kind(&self, kind: ConflictKind) -> usize {
        self.conflicts.iter().filter(|c| c.kind == kind).count()
    }

    /// Returns `true` if there are any conflicts requiring user action.
    pub fn has_user_actionable(&self) -> bool {
        self.conflicts.iter().any(|c| c.requires_user_action())
    }

    /// Returns conflicts that require user action.
    pub fn user_actionable(&self) -> impl Iterator<Item = &CrdtConflict> {
        self.conflicts.iter().filter(|c| c.requires_user_action())
    }

    /// Returns `true` if there are any unresolved conflicts.
    pub fn has_unresolved(&self) -> bool {
        self.conflicts.iter().any(|c| !c.is_resolved())
    }

    /// Returns unresolved conflicts.
    pub fn unresolved(&self) -> impl Iterator<Item = &CrdtConflict> {
        self.conflicts.iter().filter(|c| !c.is_resolved())
    }

    /// Returns the count of unresolved conflicts.
    pub fn unresolved_count(&self) -> usize {
        self.conflicts.iter().filter(|c| !c.is_resolved()).count()
    }

    // =========================================================================
    // Merge
    // =========================================================================

    /// Merges another tracker's conflicts into this one.
    pub fn merge(&mut self, other: CrdtConflictTracker) {
        self.conflicts.extend(other.conflicts);
    }

    /// Merges another tracker's conflicts by reference.
    pub fn merge_from(&mut self, other: &CrdtConflictTracker) {
        self.conflicts.extend(other.conflicts.iter().cloned());
    }
}

impl IntoIterator for CrdtConflictTracker {
    type Item = CrdtConflict;
    type IntoIter = std::vec::IntoIter<CrdtConflict>;

    fn into_iter(self) -> Self::IntoIter {
        self.conflicts.into_iter()
    }
}

impl<'a> IntoIterator for &'a CrdtConflictTracker {
    type Item = &'a CrdtConflict;
    type IntoIter = std::slice::Iter<'a, CrdtConflict>;

    fn into_iter(self) -> Self::IntoIter {
        self.conflicts.iter()
    }
}

impl Extend<CrdtConflict> for CrdtConflictTracker {
    fn extend<T: IntoIterator<Item = CrdtConflict>>(&mut self, iter: T) {
        self.conflicts.extend(iter);
    }
}

impl FromIterator<CrdtConflict> for CrdtConflictTracker {
    fn from_iter<T: IntoIterator<Item = CrdtConflict>>(iter: T) -> Self {
        Self {
            conflicts: iter.into_iter().collect(),
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeId;

    // =========================================================================
    // ConflictKind Tests
    // =========================================================================

    #[test]
    fn test_conflict_kind_auto_resolvable() {
        assert!(ConflictKind::ConcurrentInsert.is_auto_resolvable());
        assert!(ConflictKind::MoveDelete.is_auto_resolvable());
        assert!(ConflictKind::ConcurrentModify.is_auto_resolvable());
        assert!(ConflictKind::RestoreNonDeleted.is_auto_resolvable());

        assert!(!ConflictKind::DeleteModify.is_auto_resolvable());
        assert!(!ConflictKind::DuplicatePath.is_auto_resolvable());
        assert!(!ConflictKind::OrderingCycle.is_auto_resolvable());
        assert!(!ConflictKind::DeletedReference.is_auto_resolvable());
    }

    #[test]
    fn test_conflict_kind_requires_user_action() {
        assert!(ConflictKind::DeleteModify.requires_user_action());
        assert!(ConflictKind::DuplicatePath.requires_user_action());
        assert!(ConflictKind::OrderingCycle.requires_user_action());
        assert!(ConflictKind::DeletedReference.requires_user_action());

        assert!(!ConflictKind::ConcurrentInsert.requires_user_action());
        assert!(!ConflictKind::MoveDelete.requires_user_action());
    }

    #[test]
    fn test_conflict_kind_indicates_corruption() {
        assert!(ConflictKind::OrderingCycle.indicates_corruption());
        assert!(!ConflictKind::ConcurrentInsert.indicates_corruption());
        assert!(!ConflictKind::DeleteModify.indicates_corruption());
    }

    #[test]
    fn test_conflict_kind_description() {
        let desc = ConflictKind::ConcurrentInsert.description();
        assert!(!desc.is_empty());
        assert!(desc.contains("concurrent") || desc.contains("insertion"));
    }

    #[test]
    fn test_conflict_kind_display() {
        let display = ConflictKind::ConcurrentInsert.to_string();
        assert!(!display.is_empty());
    }

    #[test]
    fn test_conflict_kind_serde() {
        let kind = ConflictKind::DeleteModify;
        let json = serde_json::to_string(&kind).unwrap();
        let restored: ConflictKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, restored);
    }

    // =========================================================================
    // ConflictEntity Tests
    // =========================================================================

    #[test]
    fn test_conflict_entity_trunk() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let entity = ConflictEntity::Trunk(trunk_id);
        let display = entity.to_string();
        assert!(display.contains("trunk"));
    }

    #[test]
    fn test_conflict_entity_branch() {
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let entity = ConflictEntity::Branch(branch_id);
        let display = entity.to_string();
        assert!(display.contains("branch"));
    }

    #[test]
    fn test_conflict_entity_leaf() {
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let entity = ConflictEntity::Leaf(leaf_id);
        let display = entity.to_string();
        assert!(display.contains("leaf"));
    }

    #[test]
    fn test_conflict_entity_path() {
        let entity = ConflictEntity::Path("src/main.rs".to_string());
        let display = entity.to_string();
        assert!(display.contains("path"));
        assert!(display.contains("main.rs"));
    }

    #[test]
    fn test_conflict_entity_none() {
        let entity = ConflictEntity::None;
        let display = entity.to_string();
        assert!(display.contains("none"));
    }

    #[test]
    fn test_conflict_entity_serde() {
        let entity = ConflictEntity::Path("test.rs".to_string());
        let json = serde_json::to_string(&entity).unwrap();
        let restored: ConflictEntity = serde_json::from_str(&json).unwrap();
        assert_eq!(entity, restored);
    }

    // =========================================================================
    // CrdtConflict Tests
    // =========================================================================

    #[test]
    fn test_conflict_new() {
        let conflict = CrdtConflict::new(
            ConflictKind::ConcurrentInsert,
            "test description".to_string(),
        );

        assert_eq!(conflict.kind(), ConflictKind::ConcurrentInsert);
        assert_eq!(conflict.description(), "test description");
        assert!(!conflict.is_resolved());
        assert!(conflict.resolution().is_none());
    }

    #[test]
    fn test_conflict_builder() {
        let branch_id = BranchId::new(NodeId::new(1), 0);

        let conflict = CrdtConflict::builder(ConflictKind::DeleteModify)
            .description("test conflict")
            .branch(branch_id)
            .build();

        assert_eq!(conflict.kind(), ConflictKind::DeleteModify);
        assert_eq!(conflict.description(), "test conflict");
        assert_eq!(conflict.entity(), &ConflictEntity::Branch(branch_id));
    }

    #[test]
    fn test_conflict_builder_default_description() {
        let conflict = CrdtConflict::builder(ConflictKind::OrderingCycle).build();

        // Should use the kind's description
        assert!(!conflict.description().is_empty());
    }

    #[test]
    fn test_conflict_concurrent_insert() {
        let branch1 = BranchId::new(NodeId::new(1), 0);
        let branch2 = BranchId::new(NodeId::new(2), 0);

        let conflict = CrdtConflict::concurrent_insert(
            ConflictEntity::Branch(branch1),
            ConflictEntity::Branch(branch2),
        );

        assert_eq!(conflict.kind(), ConflictKind::ConcurrentInsert);
        assert!(conflict.is_auto_resolvable());
    }

    #[test]
    fn test_conflict_delete_modify() {
        let branch = BranchId::new(NodeId::new(1), 0);

        let conflict = CrdtConflict::delete_modify(
            ConflictEntity::Branch(branch),
            ConflictEntity::Branch(branch),
        );

        assert_eq!(conflict.kind(), ConflictKind::DeleteModify);
        assert!(conflict.requires_user_action());
    }

    #[test]
    fn test_conflict_duplicate_path() {
        let trunk1 = TrunkId::new(NodeId::new(1), 0);
        let trunk2 = TrunkId::new(NodeId::new(2), 0);

        let conflict = CrdtConflict::duplicate_path(
            "src/main.rs".to_string(),
            trunk1,
            trunk2,
        );

        assert_eq!(conflict.kind(), ConflictKind::DuplicatePath);
        assert!(conflict.description().contains("main.rs"));
    }

    #[test]
    fn test_conflict_mark_resolved() {
        let mut conflict = CrdtConflict::new(
            ConflictKind::ConcurrentInsert,
            "test".to_string(),
        );

        assert!(!conflict.is_resolved());

        conflict.mark_resolved(Some("ordered by ID".to_string()));

        assert!(conflict.is_resolved());
        assert_eq!(conflict.resolution(), Some("ordered by ID"));
    }

    #[test]
    fn test_conflict_mark_unresolved() {
        let mut conflict = CrdtConflict::new(
            ConflictKind::ConcurrentInsert,
            "test".to_string(),
        );
        conflict.mark_resolved(Some("resolved".to_string()));

        conflict.mark_unresolved();

        assert!(!conflict.is_resolved());
        assert!(conflict.resolution().is_none());
    }

    #[test]
    fn test_conflict_display() {
        let conflict = CrdtConflict::new(
            ConflictKind::ConcurrentInsert,
            "test description".to_string(),
        );
        let display = conflict.to_string();

        assert!(display.contains("concurrent"));
        assert!(display.contains("test description"));
    }

    #[test]
    fn test_conflict_display_resolved() {
        let mut conflict = CrdtConflict::new(
            ConflictKind::ConcurrentInsert,
            "test".to_string(),
        );
        conflict.mark_resolved(None);

        let display = conflict.to_string();
        assert!(display.contains("resolved"));
    }

    #[test]
    fn test_conflict_serde() {
        let conflict = CrdtConflict::new(
            ConflictKind::DeleteModify,
            "test".to_string(),
        );
        let json = serde_json::to_string(&conflict).unwrap();
        let restored: CrdtConflict = serde_json::from_str(&json).unwrap();

        assert_eq!(conflict.kind(), restored.kind());
        assert_eq!(conflict.description(), restored.description());
    }

    // =========================================================================
    // CrdtConflictTracker Tests
    // =========================================================================

    #[test]
    fn test_tracker_new() {
        let tracker = CrdtConflictTracker::new();
        assert!(tracker.is_empty());
        assert!(!tracker.has_conflicts());
        assert_eq!(tracker.count(), 0);
    }

    #[test]
    fn test_tracker_with_capacity() {
        let tracker = CrdtConflictTracker::with_capacity(10);
        assert!(tracker.is_empty());
    }

    #[test]
    fn test_tracker_add() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add(CrdtConflict::new(
            ConflictKind::ConcurrentInsert,
            "test".to_string(),
        ));

        assert!(tracker.has_conflicts());
        assert_eq!(tracker.count(), 1);
    }

    #[test]
    fn test_tracker_add_simple() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::DeleteModify, "simple test");

        assert_eq!(tracker.count(), 1);
        assert_eq!(tracker.as_slice()[0].kind(), ConflictKind::DeleteModify);
    }

    #[test]
    fn test_tracker_iter() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "1");
        tracker.add_simple(ConflictKind::DeleteModify, "2");

        let kinds: Vec<_> = tracker.iter().map(|c| c.kind()).collect();
        assert_eq!(kinds.len(), 2);
    }

    #[test]
    fn test_tracker_iter_mut() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "test");

        for conflict in tracker.iter_mut() {
            conflict.mark_resolved(None);
        }

        assert!(tracker.as_slice()[0].is_resolved());
    }

    #[test]
    fn test_tracker_into_conflicts() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "test");

        let conflicts = tracker.into_conflicts();
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn test_tracker_clear() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "test");
        tracker.clear();

        assert!(tracker.is_empty());
    }

    #[test]
    fn test_tracker_by_kind() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "1");
        tracker.add_simple(ConflictKind::DeleteModify, "2");
        tracker.add_simple(ConflictKind::ConcurrentInsert, "3");

        let insert_conflicts: Vec<_> = tracker.by_kind(ConflictKind::ConcurrentInsert).collect();
        assert_eq!(insert_conflicts.len(), 2);
    }

    #[test]
    fn test_tracker_count_by_kind() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "1");
        tracker.add_simple(ConflictKind::DeleteModify, "2");
        tracker.add_simple(ConflictKind::ConcurrentInsert, "3");

        assert_eq!(tracker.count_by_kind(ConflictKind::ConcurrentInsert), 2);
        assert_eq!(tracker.count_by_kind(ConflictKind::DeleteModify), 1);
        assert_eq!(tracker.count_by_kind(ConflictKind::OrderingCycle), 0);
    }

    #[test]
    fn test_tracker_has_user_actionable() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "auto");
        assert!(!tracker.has_user_actionable());

        tracker.add_simple(ConflictKind::DeleteModify, "user");
        assert!(tracker.has_user_actionable());
    }

    #[test]
    fn test_tracker_user_actionable() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "auto");
        tracker.add_simple(ConflictKind::DeleteModify, "user");
        tracker.add_simple(ConflictKind::OrderingCycle, "user2");

        let user_actionable: Vec<_> = tracker.user_actionable().collect();
        assert_eq!(user_actionable.len(), 2);
    }

    #[test]
    fn test_tracker_has_unresolved() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "test");

        assert!(tracker.has_unresolved());

        for conflict in tracker.iter_mut() {
            conflict.mark_resolved(None);
        }

        assert!(!tracker.has_unresolved());
    }

    #[test]
    fn test_tracker_unresolved() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "1");
        tracker.add_simple(ConflictKind::DeleteModify, "2");

        // Resolve the first one
        tracker.iter_mut().next().unwrap().mark_resolved(None);

        let unresolved: Vec<_> = tracker.unresolved().collect();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(tracker.unresolved_count(), 1);
    }

    #[test]
    fn test_tracker_merge() {
        let mut tracker1 = CrdtConflictTracker::new();
        tracker1.add_simple(ConflictKind::ConcurrentInsert, "1");

        let mut tracker2 = CrdtConflictTracker::new();
        tracker2.add_simple(ConflictKind::DeleteModify, "2");

        tracker1.merge(tracker2);

        assert_eq!(tracker1.count(), 2);
    }

    #[test]
    fn test_tracker_merge_from() {
        let mut tracker1 = CrdtConflictTracker::new();
        tracker1.add_simple(ConflictKind::ConcurrentInsert, "1");

        let mut tracker2 = CrdtConflictTracker::new();
        tracker2.add_simple(ConflictKind::DeleteModify, "2");

        tracker1.merge_from(&tracker2);

        assert_eq!(tracker1.count(), 2);
        assert_eq!(tracker2.count(), 1); // tracker2 not consumed
    }

    #[test]
    fn test_tracker_into_iterator() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "1");
        tracker.add_simple(ConflictKind::DeleteModify, "2");

        let mut count = 0;
        for _conflict in tracker {
            count += 1;
        }
        assert_eq!(count, 2);
    }

    #[test]
    fn test_tracker_ref_iterator() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "1");

        let mut count = 0;
        for _conflict in &tracker {
            count += 1;
        }
        assert_eq!(count, 1);
        // tracker still usable
        assert_eq!(tracker.count(), 1);
    }

    #[test]
    fn test_tracker_extend() {
        let mut tracker = CrdtConflictTracker::new();

        let conflicts = vec![
            CrdtConflict::new(ConflictKind::ConcurrentInsert, "1".to_string()),
            CrdtConflict::new(ConflictKind::DeleteModify, "2".to_string()),
        ];

        tracker.extend(conflicts);
        assert_eq!(tracker.count(), 2);
    }

    #[test]
    fn test_tracker_from_iterator() {
        let conflicts = vec![
            CrdtConflict::new(ConflictKind::ConcurrentInsert, "1".to_string()),
            CrdtConflict::new(ConflictKind::DeleteModify, "2".to_string()),
        ];

        let tracker: CrdtConflictTracker = conflicts.into_iter().collect();
        assert_eq!(tracker.count(), 2);
    }

    #[test]
    fn test_tracker_default() {
        let tracker = CrdtConflictTracker::default();
        assert!(tracker.is_empty());
    }

    #[test]
    fn test_tracker_clone() {
        let mut tracker = CrdtConflictTracker::new();
        tracker.add_simple(ConflictKind::ConcurrentInsert, "test");

        let cloned = tracker.clone();
        assert_eq!(cloned.count(), tracker.count());
    }
}
