//! View types and read-only view operations trait.
//!
//! Contains `ViewScope`, `ViewState` (view metadata), and `ViewTxnT`
//! (the read-only trait for querying views and their change logs).

use crate::types::{Merkle, NodeId};

use crate::pristine::error::PristineError;

use super::graph::GraphTxnT;

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

    /// Get a view by name.
    fn get_view(&self, name: &str) -> Result<Option<ViewState>, PristineError>;

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
