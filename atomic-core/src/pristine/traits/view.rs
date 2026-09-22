//! View types and read-only view operations trait.
//!
//! Contains `ViewScope`, `ViewState` (view metadata), and `ViewTxnT`
//! (the read-only trait for querying views and their change logs).

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::types::{Inode, Merkle, NodeId};

use crate::pristine::error::PristineError;

use super::graph::GraphTxnT;

/// Ordered direct membership collected from view change logs.
///
/// This type deliberately represents only changes named by a view and its
/// parent chain. It is not dependency-expanded and therefore cannot be passed
/// to graph traversal APIs that require [`GraphVisibilityClosure`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewMembershipSet {
    ordered: Vec<NodeId>,
    membership: HashSet<NodeId>,
}

impl ViewMembershipSet {
    /// Build membership in the supplied order, keeping the first occurrence of
    /// every change.
    pub fn from_ordered<I>(changes: I) -> Self
    where
        I: IntoIterator<Item = NodeId>,
    {
        let mut ordered = Vec::new();
        let mut membership = HashSet::new();
        for change_id in changes {
            if membership.insert(change_id) {
                ordered.push(change_id);
            }
        }
        Self {
            ordered,
            membership,
        }
    }

    /// Return whether this membership directly names `change_id`.
    pub fn contains(&self, change_id: impl Borrow<NodeId>) -> bool {
        self.membership.contains(change_id.borrow())
    }

    /// Return the number of directly named changes.
    pub fn len(&self) -> usize {
        self.ordered.len()
    }

    /// Return whether no changes are directly named.
    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }

    /// Iterate direct members in root-to-leaf view-log order.
    pub fn iter(&self) -> std::slice::Iter<'_, NodeId> {
        self.ordered.iter()
    }

    /// Return membership with one directly named change removed.
    pub fn without(&self, change_id: impl Borrow<NodeId>) -> Self {
        let change_id = change_id.borrow();
        Self::from_ordered(
            self.ordered
                .iter()
                .copied()
                .filter(|candidate| candidate != change_id),
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
struct EffectiveProjectionData {
    dependency_first: Vec<NodeId>,
    membership: HashSet<NodeId>,
}

/// Validated dependency closure used by every projection consumer.
///
/// Construction verifies that every reachable change has a complete indexed
/// dependency list, that every dependency is registered locally, and that the
/// dependency graph is acyclic. Cloning is O(1) because the immutable data is
/// shared through an [`Arc`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveProjectionClosure {
    data: Arc<EffectiveProjectionData>,
}

/// Compatibility name for graph APIs migrated before the projection domain was
/// shared with attributes, semantics, SetId, export, and bindings.
pub type GraphVisibilityClosure = EffectiveProjectionClosure;

impl Default for EffectiveProjectionClosure {
    fn default() -> Self {
        Self::empty()
    }
}

impl EffectiveProjectionClosure {
    /// Build a validated, deterministic dependency closure from direct view
    /// membership.
    ///
    /// Direct dependency hashes are sorted before traversal. The resulting
    /// order always places dependencies before dependents while preserving the
    /// membership order wherever dependency constraints permit.
    pub fn try_from_membership<T: GraphTxnT>(
        txn: &T,
        membership: &ViewMembershipSet,
    ) -> Result<Self, PristineError> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum VisitColor {
            White,
            Gray,
            Black,
        }

        struct VisitFrame {
            change_id: NodeId,
            dependencies: Vec<crate::types::Hash>,
            next_dependency: usize,
        }

        fn load_frame<T: GraphTxnT>(
            txn: &T,
            change_id: NodeId,
        ) -> Result<VisitFrame, PristineError> {
            if txn.get_external(change_id)?.is_none() {
                return Err(PristineError::ChangeNotFound {
                    id: change_id.get(),
                });
            }
            Ok(VisitFrame {
                change_id,
                dependencies: txn.get_indexed_change_deps(change_id)?,
                next_dependency: 0,
            })
        }

        let mut colors = HashMap::new();
        let mut dependency_first = Vec::new();
        let mut closure_membership = HashSet::new();

        for root_id in membership.iter().copied() {
            let root_color = colors.get(&root_id).copied().unwrap_or(VisitColor::White);
            if root_color == VisitColor::Black {
                continue;
            }

            let root_frame = load_frame(txn, root_id)?;
            colors.insert(root_id, VisitColor::Gray);
            let mut stack = vec![root_frame];

            while let Some(frame) = stack.last_mut() {
                if frame.next_dependency == frame.dependencies.len() {
                    let completed_id = frame.change_id;
                    stack.pop();
                    colors.insert(completed_id, VisitColor::Black);
                    if closure_membership.insert(completed_id) {
                        dependency_first.push(completed_id);
                    }
                    continue;
                }

                let change_id = frame.change_id;
                let dependency_hash = frame.dependencies[frame.next_dependency];
                frame.next_dependency += 1;

                let dependency_id = txn.get_internal(&dependency_hash)?.ok_or_else(|| {
                    PristineError::MissingRegisteredDependency {
                        change_id: change_id.get(),
                        dependency: dependency_hash.to_string(),
                    }
                })?;
                let registered_hash = txn.get_external(dependency_id)?;
                if registered_hash.as_ref() != Some(&dependency_hash) {
                    return Err(PristineError::MissingRegisteredDependency {
                        change_id: change_id.get(),
                        dependency: dependency_hash.to_string(),
                    });
                }

                match colors
                    .get(&dependency_id)
                    .copied()
                    .unwrap_or(VisitColor::White)
                {
                    VisitColor::Black => {}
                    VisitColor::Gray => {
                        let cycle_start = stack
                            .iter()
                            .position(|candidate| candidate.change_id == dependency_id)
                            .unwrap_or(0);
                        let mut cycle = stack[cycle_start..]
                            .iter()
                            .map(|candidate| candidate.change_id.get())
                            .collect::<Vec<_>>();
                        cycle.push(dependency_id.get());
                        return Err(PristineError::DependencyCycle { cycle });
                    }
                    VisitColor::White => {
                        let dependency_frame = load_frame(txn, dependency_id)?;
                        colors.insert(dependency_id, VisitColor::Gray);
                        stack.push(dependency_frame);
                    }
                }
            }
        }

        Ok(Self {
            data: Arc::new(EffectiveProjectionData {
                dependency_first,
                membership: closure_membership,
            }),
        })
    }

    /// Return an explicitly filtered closure containing no changes.
    pub fn empty() -> Self {
        Self {
            data: Arc::new(EffectiveProjectionData {
                dependency_first: Vec::new(),
                membership: HashSet::new(),
            }),
        }
    }

    /// Return whether `change_id` is in the validated closure.
    pub fn contains(&self, change_id: impl Borrow<NodeId>) -> bool {
        self.data.membership.contains(change_id.borrow())
    }

    /// Return the number of changes in the validated closure.
    pub fn len(&self) -> usize {
        self.data.dependency_first.len()
    }

    /// Return whether the validated closure contains no changes.
    pub fn is_empty(&self) -> bool {
        self.data.dependency_first.is_empty()
    }

    /// Iterate the closure in deterministic dependency-first order.
    pub fn iter_dependency_first(&self) -> std::slice::Iter<'_, NodeId> {
        self.data.dependency_first.iter()
    }

    /// Visibility domain for graph traversal.
    pub fn graph_visibility(&self) -> &GraphVisibilityClosure {
        self
    }

    /// Visibility domain for causal inode attributes.
    pub fn attribute_visibility(&self) -> &HashSet<NodeId> {
        &self.data.membership
    }

    /// Visibility domain for trunk/branch/leaf semantic projection.
    pub fn semantic_visibility(&self) -> &HashSet<NodeId> {
        &self.data.membership
    }

    /// Construct a closure tolerating members whose dependency metadata
    /// predates the index (legacy repositories). Unindexed members become
    /// their own frames — the walk degrades to "no supersession knowledge"
    /// for them instead of refusing the whole projection. Frontier
    /// verification keeps the strict `try_from_membership`.
    pub fn try_from_membership_lenient<T: GraphTxnT>(
        txn: &T,
        membership: &ViewMembershipSet,
    ) -> Result<Self, PristineError> {
        fn load_frame<T: GraphTxnT>(
            txn: &T,
            change_id: NodeId,
        ) -> Result<(NodeId, Vec<NodeId>), PristineError> {
            if txn.get_external(change_id)?.is_none() {
                return Err(PristineError::ChangeNotFound {
                    id: change_id.get(),
                });
            }
            let dep_hashes = match txn.get_indexed_change_deps(change_id) {
                Ok(deps) => deps,
                Err(_) => txn.get_change_deps(change_id)?,
            };
            let mut dependencies = Vec::with_capacity(dep_hashes.len());
            for dep_hash in dep_hashes {
                dependencies.push(txn.get_internal(&dep_hash)?.ok_or_else(|| {
                    PristineError::MissingRegisteredDependency {
                        change_id: change_id.get(),
                        dependency: dep_hash.to_string(),
                    }
                })?);
            }
            Ok((change_id, dependencies))
        }

        struct Frame {
            change_id: NodeId,
            dependencies: Vec<NodeId>,
            next_dependency: usize,
        }

        let mut colors: HashMap<NodeId, u8> = HashMap::new();
        const WHITE: u8 = 0;
        const GRAY: u8 = 1;
        const BLACK: u8 = 2;

        let mut dependency_first = Vec::new();
        let mut closure_membership = HashSet::new();

        for root_id in membership.iter().copied() {
            let root_color = colors.get(&root_id).copied().unwrap_or(WHITE);
            if root_color == BLACK {
                continue;
            }
            let (id, deps) = load_frame(txn, root_id)?;
            colors.insert(root_id, GRAY);
            let mut stack = vec![Frame {
                change_id: id,
                dependencies: deps,
                next_dependency: 0,
            }];

            while let Some(frame) = stack.last_mut() {
                if frame.next_dependency == frame.dependencies.len() {
                    let completed_id = frame.change_id;
                    stack.pop();
                    colors.insert(completed_id, BLACK);
                    if closure_membership.insert(completed_id) {
                        dependency_first.push(completed_id);
                    }
                    continue;
                }
                let next = frame.dependencies[frame.next_dependency];
                frame.next_dependency += 1;
                let next_color = colors.get(&next).copied().unwrap_or(WHITE);
                match next_color {
                    BLACK => continue,
                    GRAY => {
                        return Err(PristineError::DependencyCycle {
                            cycle: vec![next.get(), root_id.get()],
                        })
                    }
                    // WHITE and any unexpected color: visit.
                    _ => {
                        let (next_id, next_deps) = load_frame(txn, next)?;
                        colors.insert(next, GRAY);
                        stack.push(Frame {
                            change_id: next_id,
                            dependencies: next_deps,
                            next_dependency: 0,
                        });
                    }
                }
            }
        }

        Ok(Self {
            data: Arc::new(EffectiveProjectionData {
                dependency_first,
                membership: closure_membership,
            }),
        })
    }

    /// Construct a closure without dependency validation. Production
    /// change-filter callers (dev #203) prove dependency completeness from
    /// the view membership upstream; unit tests use it directly.
    pub(crate) fn from_ordered_unchecked<I>(changes: I) -> Self
    where
        I: IntoIterator<Item = NodeId>,
    {
        let membership = ViewMembershipSet::from_ordered(changes);
        Self {
            data: Arc::new(EffectiveProjectionData {
                dependency_first: membership.ordered,
                membership: membership.membership,
            }),
        }
    }
}

/// Controls the lifecycle and change-filter strategy for a view.
///
/// # View Scopes
///
/// - **Shared** views (dev, release, main) write edges to the global `GRAPH`
///   table. These edges are visible to all views and persist permanently.
/// - **Draft** views (feature, bug, experiment) record changes to `GRAPH`
///   immediately but only expose them through this view's filter.
///   Can be deleted freely.
///
/// # View Chain
///
/// A draft view's effective content is determined by its own changes plus
/// those of its ancestor views:
///
/// ```text
/// feature-login view = changes[feature-login]
///                     ∪ changes[service-auth]   (parent)
///                     ∪ GRAPH                    (dev is Shared → stop)
/// ```
///
/// # Example
///
/// ```
/// use atomic_core::pristine::ViewScope;
///
/// let scope = ViewScope::Draft;
/// assert_eq!(scope as u8, 0);
/// assert!(!scope.is_shared());
/// assert!(scope.is_draft());
///
/// let scope = ViewScope::Shared;
/// assert_eq!(scope as u8, 1);
/// assert!(scope.is_shared());
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
#[repr(u8)]
pub enum ViewScope {
    /// Personal workspace (feature, bug, experiment).
    ///
    /// Changes are recorded to GRAPH immediately but only visible through
    /// this view's filter. Can be deleted freely.
    Draft = 0,

    /// Collaborative view (dev, release, main).
    ///
    /// Changes inserted here become part of the base filter.
    /// Deletion is restricted.
    #[default]
    Shared = 1,
}

impl ViewScope {
    /// Check if this is a shared view.
    #[inline]
    pub fn is_shared(self) -> bool {
        self == Self::Shared
    }

    /// Check if this is a draft view.
    #[inline]
    pub fn is_draft(self) -> bool {
        self == Self::Draft
    }

    /// Convert from a raw u8 value.
    ///
    /// Returns `None` if the value is not a valid `ViewScope`.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Draft),
            1 => Some(Self::Shared),
            _ => None,
        }
    }
}

impl std::fmt::Display for ViewScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "draft"),
            Self::Shared => write!(f, "shared"),
        }
    }
}

/// View state information
///
/// A View represents a **perspective** of the repository graph. Unlike Git
/// branches which point to a commit and represent a fork of history, a View
/// is an ordered sequence of changes applied to the same shared graph.
///
/// # Key Properties
///
/// - **id**: Repository-local identifier for the view
/// - **name**: Human-readable name (like "main", "feature-x")
/// - **state**: Merkle hash representing the cumulative state
/// - **change_count**: Number of changes applied to this view
///
/// # Merkle State
///
/// The `state` field is a Merkle hash computed incrementally:
///
/// ```text
/// state_0 = Hash(empty)
/// state_n = Hash(state_{n-1} || change_hash_n)
/// ```
///
/// This allows efficient comparison of view states:
/// - Same state → views have identical changes in identical order
/// - Different state → views differ somehow
///
/// # Example
///
/// ```
/// use atomic_core::pristine::ViewState;
/// use atomic_core::types::Merkle;
///
/// let view = ViewState::new(1, "feature-login".to_string());
/// assert_eq!(view.name, "feature-login");
/// assert_eq!(view.change_count, 0);
/// assert_eq!(view.state, Merkle::ZERO);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewState {
    /// View ID (internal, repository-local)
    ///
    /// This is an auto-incrementing identifier assigned when the view is created.
    /// It's used as part of the key in various tables.
    pub id: u64,

    /// View name (human-readable)
    ///
    /// This is the user-facing name like "main", "develop", or "feature-x".
    /// View names must be unique within a repository.
    pub name: String,

    /// Current Merkle state (cumulative hash of applied changes)
    ///
    /// This hash uniquely identifies the state of the view. Two views with
    /// the same Merkle state have the exact same changes in the exact same order.
    pub state: Merkle,

    /// Number of changes applied to this view
    ///
    /// This is the sequence number of the next change to be applied.
    /// If change_count is 5, changes 0-4 have been applied.
    pub change_count: u64,

    /// View scope (Draft or Shared)
    ///
    /// Controls change visibility:
    /// - `Shared`: changes become part of the base filter (permanent)
    /// - `Draft`: changes are recorded to GRAPH but only visible through this view's filter
    pub kind: ViewScope,

    /// Parent view ID
    ///
    /// The view this one was created from. Used to build the view chain
    /// for change filtering. Every view except the root has a parent.
    ///
    /// - `None`: This is the root view (e.g., "main"). Only one view should
    ///   have `parent = None` — the root of the hierarchy.
    /// - `Some(id)`: The parent view's internal ID. The parent can be either
    ///   Shared or Draft. For example, `feature-login` might have
    ///   `parent = Some(service_auth_id)` which itself has
    ///   `parent = Some(dev_id)`.
    pub parent: Option<u64>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            state: Merkle::ZERO,
            change_count: 0,
            kind: ViewScope::Shared,
            parent: None,
        }
    }
}

impl ViewState {
    /// Create a new shared view state with the given name and no parent.
    ///
    /// This is the default constructor for backward compatibility. New code
    /// should prefer [`ViewState::with_scope`] for explicit scope/parent.
    ///
    /// # Example
    ///
    /// ```
    /// use atomic_core::pristine::ViewState;
    ///
    /// let view = ViewState::new(1, "main".to_string());
    /// assert_eq!(view.id, 1);
    /// assert_eq!(view.name, "main");
    /// assert_eq!(view.change_count, 0);
    /// assert!(view.kind.is_shared());
    /// assert!(view.parent.is_none());
    /// ```
    pub fn new(id: u64, name: String) -> Self {
        Self {
            id,
            name,
            state: Merkle::ZERO,
            change_count: 0,
            kind: ViewScope::Shared,
            parent: None,
        }
    }

    /// Create a new view with explicit scope and parent.
    ///
    /// # Example
    ///
    /// ```
    /// use atomic_core::pristine::{ViewState, ViewScope};
    ///
    /// // Create a shared "dev" view parented on "main" (id=1)
    /// let dev = ViewState::with_scope(2, "dev".to_string(), ViewScope::Shared, Some(1));
    /// assert!(dev.kind.is_shared());
    /// assert_eq!(dev.parent, Some(1));
    ///
    /// // Create a draft "feature" view parented on "dev" (id=2)
    /// let feature = ViewState::with_scope(3, "feature".to_string(), ViewScope::Draft, Some(2));
    /// assert!(feature.kind.is_draft());
    /// assert_eq!(feature.parent, Some(2));
    /// ```
    pub fn with_scope(id: u64, name: String, kind: ViewScope, parent: Option<u64>) -> Self {
        Self {
            id,
            name,
            state: Merkle::ZERO,
            change_count: 0,
            kind,
            parent,
        }
    }

    /// Check if the view has any changes.
    pub fn is_empty(&self) -> bool {
        self.change_count == 0
    }

    /// Check if this is the root view (no parent).
    #[inline]
    pub fn is_root(&self) -> bool {
        self.parent.is_none()
    }
}

/// The kind of a persisted conflict, mirroring the output layer's
/// `FileConflictType` in a storage-stable form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StoredConflictKind {
    /// Ambiguous ordering of content (concurrent insert at one position).
    Order,
    /// Cyclic conflict (an SCC with more than one vertex).
    Cyclic,
    /// Deleted content that still has live connections.
    Zombie,
    /// Two changes assigned different names to the same path.
    Name,
}

impl std::fmt::Display for StoredConflictKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Order => write!(f, "order"),
            Self::Cyclic => write!(f, "cyclic"),
            Self::Zombie => write!(f, "zombie"),
            Self::Name => write!(f, "name"),
        }
    }
}

/// A conflict persisted in the `CONFLICTS` table for one file on one view.
///
/// Captured from the materialization result so the repository can report
/// conflicted files (`atomic status`) and refuse to `record` over markers
/// without re-running a full materialize.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredConflict {
    /// The kind of conflict.
    pub kind: StoredConflictKind,
    /// The file's path at detection time (for display).
    pub path: String,
    /// 1-based line where the conflict region begins, if known.
    pub line: Option<u32>,
    /// Base32 hashes of the changes involved.
    pub sides: Vec<String>,
}

impl StoredConflict {
    /// A short, human-readable summary for `atomic status` detail output.
    pub fn summary(&self) -> String {
        match self.line {
            Some(line) => format!("{} conflict at line {}", self.kind, line),
            None => format!("{} conflict", self.kind),
        }
    }
}

// ---------------------------------------------------------------------------
// ViewTxnT — read-only view operations
// ---------------------------------------------------------------------------

/// View operations
///
/// This trait provides read access to view metadata and change logs.
/// Views are perspectives of the graph that track which changes have been applied
/// and in what order.
///
/// Think of a View like a playlist of songs (changes) from a shared music
/// library (the graph). Different playlists can contain different songs in
/// different orders, but they all reference the same library. "Merging"
/// playlists means adding songs from one playlist that the other doesn't have.
///
/// # Example
///
/// ```ignore
/// fn print_view_info<T: ViewTxnT>(txn: &T, name: &str) -> PristineResult<()> {
///     if let Some(view) = txn.get_view(name)? {
///         println!("View: {}", view.name);
///         println!("Changes: {}", view.change_count);
///         println!("State: {}", view.state);
///
///         // List recent changes
///         for result in txn.iter_changes(&view, 0)? {
///             let (seq, change_id, merkle) = result?;
///             let hash = txn.get_external(change_id)?.unwrap();
///             println!("  #{}: {}", seq, hash);
///         }
///     }
///     Ok(())
/// }
/// ```
pub trait ViewTxnT: GraphTxnT {
    /// Look up a view by its internal ID.
    ///
    /// This is used to resolve parent references when walking the view
    /// chain. Unlike [`Self::get_view`] which looks up by name, this looks up
    /// by the internal numeric ID stored in `ViewState::id`.
    fn get_view_by_id(&self, id: u64) -> Result<Option<ViewState>, PristineError>;

    /// Resolve the view chain for a draft view.
    ///
    /// Walks the `parent` links from the given view upward, collecting
    /// the IDs of each **Draft** ancestor. Stops when a **Shared**
    /// ancestor (or the root) is reached.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // feature-login (Draft, parent=service-auth)
    /// // service-auth  (Draft, parent=dev)
    /// // dev           (Shared,  parent=main)
    ///
    /// let chain = txn.resolve_view_chain(&feature_login)?;
    /// // chain = [feature_login.id, service_auth.id]
    /// // GRAPH is the implicit base (dev is Shared → stop)
    /// ```
    fn resolve_view_chain(&self, view: &ViewState) -> Result<Vec<u64>, PristineError> {
        let mut chain: Vec<u64> = Vec::new();

        if view.kind.is_shared() {
            // Shared views read directly from GRAPH, no chain needed
            return Ok(chain);
        }

        chain.push(view.id);

        let mut cursor = view.parent;
        while let Some(parent_id) = cursor {
            let parent = self.get_view_by_id(parent_id)?;
            match parent {
                Some(p) if p.kind.is_draft() => {
                    chain.push(p.id);
                    cursor = p.parent;
                }
                _ => break, // Shared ancestor or not found → GRAPH is the base
            }
        }

        Ok(chain)
    }

    /// Resolve every view from the hierarchy root through `view`.
    ///
    /// Unlike [`Self::resolve_view_chain`], this includes both Shared and Draft
    /// views, works identically for every scope, and returns root-to-leaf order.
    /// Missing parents and cycles are reported instead of being treated as an
    /// implicit ambient-graph base.
    fn resolve_full_view_chain(&self, view: &ViewState) -> Result<Vec<ViewState>, PristineError> {
        let mut leaf_to_root = vec![view.clone()];
        let mut seen = HashSet::new();
        seen.insert(view.id);

        let mut child = view.clone();
        while let Some(parent_id) = child.parent {
            let parent =
                self.get_view_by_id(parent_id)?
                    .ok_or_else(|| PristineError::BrokenViewParent {
                        view_id: child.id,
                        view_name: child.name.clone(),
                        parent_id,
                    })?;

            if !seen.insert(parent.id) {
                return Err(PristineError::ViewCycleDetected {
                    name: child.name,
                    parent_name: parent.name,
                });
            }

            child = parent.clone();
            leaf_to_root.push(parent);
        }

        leaf_to_root.reverse();
        Ok(leaf_to_root)
    }

    /// Collect ordered direct membership from the full parent chain.
    ///
    /// Each view log is read in sequence order from hierarchy root to leaf.
    /// If malformed or legacy logs name a change more than once, the first
    /// occurrence wins.
    fn view_membership_set(&self, view: &ViewState) -> Result<ViewMembershipSet, PristineError> {
        let chain = self.resolve_full_view_chain(view)?;
        let mut ordered = Vec::new();
        for chain_view in &chain {
            for entry in self.iter_changes(chain_view, 0)? {
                let (_sequence, change_id, _state) = entry?;
                ordered.push(change_id);
            }
        }
        Ok(ViewMembershipSet::from_ordered(ordered))
    }

    /// Find all views that have the given view as their parent.
    ///
    /// Used during view deletion to check for child views that would be
    /// orphaned. A view with children cannot be deleted without first
    /// reparenting or deleting its children.
    fn get_children_views(&self, parent_id: u64) -> Result<Vec<ViewState>, PristineError> {
        // Default implementation: scan all views and filter by parent.
        // View counts are typically small (<100), so this is fine.
        let names = self.list_views()?;
        let mut children = Vec::new();
        for name in names {
            if let Some(view) = self.get_view(&name)? {
                if view.parent == Some(parent_id) {
                    children.push(view);
                }
            }
        }
        Ok(children)
    }

    /// Read the persisted conflicts for a file (`inode`) on a view.
    ///
    /// Returns an empty vec if the file has no recorded conflicts.
    fn get_conflicts(&self, view_id: u64, inode: u64)
        -> Result<Vec<StoredConflict>, PristineError>;

    /// Iterate all persisted conflicts on a view.
    ///
    /// Returns `(inode, conflicts)` pairs for every file that has recorded
    /// conflict state on this view.
    #[allow(clippy::type_complexity)]
    fn iter_conflicts(
        &self,
        view_id: u64,
    ) -> Result<Vec<(u64, Vec<StoredConflict>)>, PristineError>;

    /// Return every CONFLICTS row in deterministic key order.
    ///
    /// Unlike [`Self::iter_conflicts`], this is a complete storage snapshot: it
    /// includes rows for orphan view IDs and rows with empty conflict lists.
    /// Malformed stored payloads are returned as errors rather than skipped.
    #[allow(clippy::type_complexity)]
    fn snapshot_conflicts(&self) -> Result<Vec<(u64, Inode, Vec<StoredConflict>)>, PristineError> {
        Err(PristineError::Inconsistent {
            message: "complete CONFLICTS snapshots are unavailable for this transaction wrapper"
                .to_string(),
        })
    }

    /// Get a view by name.
    fn get_view(&self, name: &str) -> Result<Option<ViewState>, PristineError>;

    /// Return every VIEWS row in deterministic key order.
    ///
    /// The stored key and decoded state are both returned so repair can inspect
    /// key/value inconsistencies. Malformed values are returned as errors.
    fn snapshot_views(&self) -> Result<Vec<(String, ViewState)>, PristineError> {
        Err(PristineError::Inconsistent {
            message: "complete VIEWS snapshots are unavailable for this transaction wrapper"
                .to_string(),
        })
    }

    /// List all view names.
    ///
    /// Returns a vector of all view names in the repository.
    /// The order is not guaranteed.
    fn list_views(&self) -> Result<Vec<String>, PristineError>;

    /// Get the current Merkle state for a view.
    fn view_state(&self, view: &ViewState) -> Merkle {
        view.state
    }

    /// Get the sequence number for a change in a view.
    ///
    /// Returns `Some(seq)` if the change is in the view, `None` otherwise.
    fn get_change_seq(
        &self,
        view: &ViewState,
        change_id: NodeId,
    ) -> Result<Option<u64>, PristineError>;

    /// Get the change at a sequence number in a view.
    ///
    /// Returns `Some(id)` for the change at this sequence, `None` if out of range.
    fn get_change_at_seq(
        &self,
        view: &ViewState,
        seq: u64,
    ) -> Result<Option<NodeId>, PristineError>;

    /// Iterate over changes in a view.
    ///
    /// Returns an iterator over (sequence, change_id, merkle_state) tuples,
    /// starting from the given sequence number.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Get all changes after sequence 10
    /// for result in txn.iter_changes(&view, 10)? {
    ///     let (seq, change_id, state) = result?;
    ///     println!("Change #{}: {:?} (state: {})", seq, change_id, state);
    /// }
    /// ```
    #[allow(clippy::type_complexity)]
    fn iter_changes(
        &self,
        view: &ViewState,
        from_seq: u64,
    ) -> Result<
        Box<dyn Iterator<Item = Result<(u64, NodeId, Merkle), PristineError>> + '_>,
        PristineError,
    >;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::types::{EdgeFlags, GraphNode, Hash, Position, SerializedGraphEdge};

    use super::*;

    #[derive(Default)]
    struct MockTxn {
        external: HashMap<NodeId, Hash>,
        internal: HashMap<Hash, NodeId>,
        dependencies: HashMap<NodeId, Vec<Hash>>,
        indexed_counts: HashMap<NodeId, u64>,
        views: HashMap<u64, ViewState>,
        logs: HashMap<u64, Vec<NodeId>>,
    }

    impl MockTxn {
        fn register(&mut self, id: u64, hash: Hash, dependencies: Vec<Hash>) {
            let id = NodeId::new(id);
            self.external.insert(id, hash);
            self.internal.insert(hash, id);
            self.indexed_counts.insert(id, dependencies.len() as u64);
            self.dependencies.insert(id, dependencies);
        }

        fn add_view(&mut self, view: ViewState, changes: Vec<NodeId>) {
            self.logs.insert(view.id, changes);
            self.views.insert(view.id, view);
        }
    }

    impl GraphTxnT for MockTxn {
        type Adj = std::iter::Empty<Result<SerializedGraphEdge, PristineError>>;

        fn get_external(&self, id: NodeId) -> Result<Option<Hash>, PristineError> {
            Ok(self.external.get(&id).copied())
        }

        fn get_internal(&self, hash: &Hash) -> Result<Option<NodeId>, PristineError> {
            Ok(self.internal.get(hash).copied())
        }

        fn iter_adjacent(
            &self,
            _node: GraphNode<NodeId>,
            _min_flag: EdgeFlags,
            _max_flag: EdgeFlags,
        ) -> Result<Self::Adj, PristineError> {
            Ok(std::iter::empty())
        }

        fn find_block(&self, _pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
            Err(PristineError::BlockNotFound { change: 0, pos: 0 })
        }

        fn find_block_end(
            &self,
            _pos: Position<NodeId>,
        ) -> Result<GraphNode<NodeId>, PristineError> {
            Err(PristineError::BlockNotFound { change: 0, pos: 0 })
        }

        fn has_vertex(&self, _node: GraphNode<NodeId>) -> Result<bool, PristineError> {
            Ok(false)
        }

        fn get_node_type(&self, node_id: NodeId) -> Result<Option<u8>, PristineError> {
            Ok(self.external.contains_key(&node_id).then_some(0))
        }

        fn get_rev_deps(&self, _dep_id: NodeId) -> Result<Vec<NodeId>, PristineError> {
            Ok(Vec::new())
        }

        fn get_change_deps(&self, change_id: NodeId) -> Result<Vec<Hash>, PristineError> {
            Ok(self
                .dependencies
                .get(&change_id)
                .cloned()
                .unwrap_or_default())
        }

        fn change_deps_indexed_count(
            &self,
            change_id: NodeId,
        ) -> Result<Option<u64>, PristineError> {
            Ok(self.indexed_counts.get(&change_id).copied())
        }

        fn get_rev_change_deps(&self, _dep_hash: &Hash) -> Result<Vec<NodeId>, PristineError> {
            Ok(Vec::new())
        }

        fn has_change_in_graph(&self, _change_id: NodeId) -> Result<bool, PristineError> {
            Ok(false)
        }
    }

    impl ViewTxnT for MockTxn {
        fn get_view_by_id(&self, id: u64) -> Result<Option<ViewState>, PristineError> {
            Ok(self.views.get(&id).cloned())
        }

        fn get_conflicts(
            &self,
            _view_id: u64,
            _inode: u64,
        ) -> Result<Vec<StoredConflict>, PristineError> {
            Ok(Vec::new())
        }

        fn iter_conflicts(
            &self,
            _view_id: u64,
        ) -> Result<Vec<(u64, Vec<StoredConflict>)>, PristineError> {
            Ok(Vec::new())
        }

        fn get_view(&self, name: &str) -> Result<Option<ViewState>, PristineError> {
            Ok(self.views.values().find(|view| view.name == name).cloned())
        }

        fn list_views(&self) -> Result<Vec<String>, PristineError> {
            Ok(self.views.values().map(|view| view.name.clone()).collect())
        }

        fn get_change_seq(
            &self,
            view: &ViewState,
            change_id: NodeId,
        ) -> Result<Option<u64>, PristineError> {
            Ok(self.logs.get(&view.id).and_then(|changes| {
                changes
                    .iter()
                    .position(|candidate| *candidate == change_id)
                    .map(|sequence| sequence as u64)
            }))
        }

        fn get_change_at_seq(
            &self,
            view: &ViewState,
            seq: u64,
        ) -> Result<Option<NodeId>, PristineError> {
            Ok(self
                .logs
                .get(&view.id)
                .and_then(|changes| changes.get(seq as usize))
                .copied())
        }

        fn iter_changes(
            &self,
            view: &ViewState,
            from_seq: u64,
        ) -> Result<
            Box<dyn Iterator<Item = Result<(u64, NodeId, Merkle), PristineError>> + '_>,
            PristineError,
        > {
            let entries = self
                .logs
                .get(&view.id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .skip(from_seq as usize)
                .map(|(sequence, change_id)| Ok((sequence as u64, change_id, Merkle::ZERO)));
            Ok(Box::new(entries))
        }
    }

    fn hash(byte: u8) -> Hash {
        Hash::from_bytes([byte; 32])
    }

    fn chain_hash(id: u64) -> Hash {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&id.to_le_bytes());
        bytes[8..17].copy_from_slice(b"n13-chain");
        Hash::from_bytes(bytes)
    }

    #[test]
    fn membership_preserves_first_occurrence_and_without_order() {
        let membership = ViewMembershipSet::from_ordered([
            NodeId::new(1),
            NodeId::new(2),
            NodeId::new(1),
            NodeId::new(3),
        ]);

        assert_eq!(membership.len(), 3);
        assert!(membership.contains(NodeId::new(2)));
        assert_eq!(
            membership.iter().copied().collect::<Vec<_>>(),
            vec![NodeId::new(1), NodeId::new(2), NodeId::new(3)]
        );
        assert_eq!(
            membership
                .without(NodeId::new(2))
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![NodeId::new(1), NodeId::new(3)]
        );
    }

    #[test]
    fn closure_is_deterministic_dependency_first_and_clone_is_shared() {
        let mut txn = MockTxn::default();
        txn.register(1, hash(1), Vec::new());
        txn.register(2, hash(2), vec![hash(1)]);
        txn.register(3, hash(3), vec![hash(2), hash(1)]);

        let membership = ViewMembershipSet::from_ordered([NodeId::new(3)]);
        let closure = GraphVisibilityClosure::try_from_membership(&txn, &membership).unwrap();
        assert_eq!(
            closure.iter_dependency_first().copied().collect::<Vec<_>>(),
            vec![NodeId::new(1), NodeId::new(2), NodeId::new(3)]
        );
        assert!(closure.contains(NodeId::new(2)));

        let cloned = closure.clone();
        assert!(Arc::ptr_eq(&closure.data, &cloned.data));
    }

    #[test]
    fn closure_handles_long_linear_chain_without_call_stack_growth() {
        const CHAIN_LEN: u64 = 25_000;

        let mut txn = MockTxn::default();
        for id in 1..=CHAIN_LEN {
            let dependencies = if id == 1 {
                Vec::new()
            } else {
                vec![chain_hash(id - 1)]
            };
            txn.register(id, chain_hash(id), dependencies);
        }

        let membership = ViewMembershipSet::from_ordered([NodeId::new(CHAIN_LEN)]);
        let closure = GraphVisibilityClosure::try_from_membership(&txn, &membership).unwrap();

        assert_eq!(closure.len(), CHAIN_LEN as usize);
        assert!(closure
            .iter_dependency_first()
            .enumerate()
            .all(|(index, change_id)| change_id.get() == index as u64 + 1));
    }

    #[test]
    fn closure_rejects_unindexed_count_mismatch_missing_and_cycles() {
        let mut unindexed = MockTxn::default();
        unindexed.external.insert(NodeId::new(1), hash(1));
        unindexed.internal.insert(hash(1), NodeId::new(1));
        let membership = ViewMembershipSet::from_ordered([NodeId::new(1)]);
        assert!(matches!(
            GraphVisibilityClosure::try_from_membership(&unindexed, &membership),
            Err(PristineError::UnindexedChangeDependencies { change_id: 1 })
        ));

        let mut mismatched = MockTxn::default();
        mismatched.register(1, hash(1), vec![hash(2)]);
        mismatched.indexed_counts.insert(NodeId::new(1), 2);
        assert!(matches!(
            GraphVisibilityClosure::try_from_membership(&mismatched, &membership),
            Err(PristineError::ChangeDependencyCountMismatch {
                change_id: 1,
                expected: 2,
                actual: 1
            })
        ));

        let mut missing = MockTxn::default();
        missing.register(1, hash(1), vec![hash(2)]);
        assert!(matches!(
            GraphVisibilityClosure::try_from_membership(&missing, &membership),
            Err(PristineError::MissingRegisteredDependency { change_id: 1, .. })
        ));

        let mut cyclic = MockTxn::default();
        cyclic.register(1, hash(1), vec![hash(2)]);
        cyclic.register(2, hash(2), vec![hash(1)]);
        assert!(matches!(
            GraphVisibilityClosure::try_from_membership(&cyclic, &membership),
            Err(PristineError::DependencyCycle { .. })
        ));
    }

    #[test]
    fn full_chain_and_membership_include_every_scope_root_to_leaf() {
        let mut txn = MockTxn::default();
        let main = ViewState::with_scope(1, "main".into(), ViewScope::Shared, None);
        let dev = ViewState::with_scope(2, "dev".into(), ViewScope::Shared, Some(1));
        let feature = ViewState::with_scope(3, "feature".into(), ViewScope::Draft, Some(2));
        txn.add_view(main, vec![NodeId::new(10), NodeId::new(20)]);
        txn.add_view(dev, vec![NodeId::new(20), NodeId::new(30)]);
        txn.add_view(feature.clone(), vec![NodeId::new(40)]);

        let chain = txn.resolve_full_view_chain(&feature).unwrap();
        assert_eq!(
            chain.iter().map(|view| view.id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            txn.view_membership_set(&feature)
                .unwrap()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![
                NodeId::new(10),
                NodeId::new(20),
                NodeId::new(30),
                NodeId::new(40)
            ]
        );
    }

    #[test]
    fn full_chain_rejects_missing_parent_and_cycles() {
        let txn = MockTxn::default();
        let broken = ViewState::with_scope(3, "broken".into(), ViewScope::Draft, Some(99));
        assert!(matches!(
            txn.resolve_full_view_chain(&broken),
            Err(PristineError::BrokenViewParent {
                view_id: 3,
                parent_id: 99,
                ..
            })
        ));

        let mut txn = MockTxn::default();
        let a = ViewState::with_scope(1, "a".into(), ViewScope::Shared, Some(2));
        let b = ViewState::with_scope(2, "b".into(), ViewScope::Draft, Some(1));
        txn.add_view(a.clone(), Vec::new());
        txn.add_view(b, Vec::new());
        assert!(matches!(
            txn.resolve_full_view_chain(&a),
            Err(PristineError::ViewCycleDetected { .. })
        ));
    }
}
