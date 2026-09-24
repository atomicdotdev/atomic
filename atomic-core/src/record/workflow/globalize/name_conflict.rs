use super::*;

/// Exact graph identity of one losing path claim.
///
/// `source` is the parent-directory inode vertex, `name` is the non-empty name
/// vertex, and `claimant` is the stable inode reached by the name vertex. The
/// `introduced_by` change identifies the current state of the `source -> name`
/// edge: the claim change for a solve, or the resolution change for an unsolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameConflictClaim {
    claimant: Position<NodeId>,
    source: GraphNode<NodeId>,
    name: GraphNode<NodeId>,
    introduced_by: NodeId,
}

impl NameConflictClaim {
    /// Describe an exact path-claim edge and the inode it reaches.
    #[must_use]
    pub const fn new(
        claimant: Position<NodeId>,
        source: GraphNode<NodeId>,
        name: GraphNode<NodeId>,
        introduced_by: NodeId,
    ) -> Self {
        Self {
            claimant,
            source,
            name,
            introduced_by,
        }
    }

    /// Stable inode position of the losing claimant.
    #[must_use]
    pub const fn claimant(&self) -> Position<NodeId> {
        self.claimant
    }

    /// Exact parent-directory vertex from which the claim edge originates.
    #[must_use]
    pub const fn source(&self) -> GraphNode<NodeId> {
        self.source
    }

    /// Exact non-empty name vertex targeted by the claim edge.
    #[must_use]
    pub const fn name(&self) -> GraphNode<NodeId> {
        self.name
    }

    /// Change that introduced the current claim-edge state.
    #[must_use]
    pub const fn introduced_by(&self) -> NodeId {
        self.introduced_by
    }
}

/// Build a serialized name-conflict resolution from exact internal graph claims.
///
/// The returned operation stores `surviving_claimant` in `name.inode`, retains
/// `path`, and tombstones every supplied losing `source -> name` edge. Every
/// non-root hash needed by the operation, including losing claimant inode hashes
/// not otherwise present in the wire fields, is added to `ctx.dependencies()`.
///
/// # Errors
///
/// Fails if the path or claim list is empty, a claimant is ROOT, a losing claim
/// names the winner, any graph vertex/edge is missing, or an edge is not exactly
/// `FOLDER | BLOCK` and introduced by the supplied change.
pub fn globalize_solve_name_conflict<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    path: impl Into<String>,
    surviving_claimant: Position<NodeId>,
    losing_claims: impl IntoIterator<Item = NameConflictClaim>,
) -> GlobalizeResult<GraphOp<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    globalize_name_conflict(
        ctx,
        path.into(),
        surviving_claimant,
        losing_claims,
        NameConflictDirection::Solve,
    )
}

/// Build the exact inverse of a serialized name-conflict resolution.
///
/// Each claim's `introduced_by` must be the resolution change that introduced
/// the currently visible `FOLDER | BLOCK | DELETED` edge. The returned operation
/// restores each losing claim with an exact inverse transition and records all
/// claimant and edge dependencies.
///
/// # Errors
///
/// Fails closed under the same conditions as [`globalize_solve_name_conflict`],
/// or when an expected deleted resolution edge is absent.
pub fn globalize_unsolve_name_conflict<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    path: impl Into<String>,
    surviving_claimant: Position<NodeId>,
    losing_claims: impl IntoIterator<Item = NameConflictClaim>,
) -> GlobalizeResult<GraphOp<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    globalize_name_conflict(
        ctx,
        path.into(),
        surviving_claimant,
        losing_claims,
        NameConflictDirection::Unsolve,
    )
}

#[derive(Clone, Copy)]
enum NameConflictDirection {
    Solve,
    Unsolve,
}

impl NameConflictDirection {
    fn operation(self) -> &'static str {
        match self {
            Self::Solve => "SolveNameConflict",
            Self::Unsolve => "UnsolveNameConflict",
        }
    }

    fn current_flags(self) -> EdgeFlags {
        let alive = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
        match self {
            Self::Solve => alive,
            Self::Unsolve => alive | EdgeFlags::DELETED,
        }
    }
}

fn globalize_name_conflict<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    path: String,
    surviving_claimant: Position<NodeId>,
    losing_claims: impl IntoIterator<Item = NameConflictClaim>,
    direction: NameConflictDirection,
) -> GlobalizeResult<GraphOp<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    if path.is_empty() {
        return invalid_name_conflict(&path, "surviving path is empty");
    }
    validate_inode_vertex(ctx.txn(), &path, "surviving claimant", surviving_claimant)?;

    let claims: Vec<_> = losing_claims.into_iter().collect();
    if claims.is_empty() {
        return invalid_name_conflict(&path, "no losing claims were supplied");
    }

    for (index, claim) in claims.iter().enumerate() {
        if claims[..index].contains(claim) {
            return invalid_name_conflict(&path, format!("losing claim {index} is duplicated"));
        }
        validate_claim(
            ctx.txn(),
            &path,
            index,
            surviving_claimant,
            claim,
            direction,
        )?;
    }

    let mut dependencies = HashSet::new();
    let surviving_claimant =
        externalize_position(ctx.txn(), &path, surviving_claimant, &mut dependencies)?;
    let mut edges = Vec::with_capacity(claims.len());
    for claim in &claims {
        // The losing inode is a dependency even when a move introduced its name
        // vertex in a different change, so dependency closure remains complete.
        externalize_change(
            ctx.txn(),
            &path,
            claim.claimant.change,
            "losing claimant",
            &mut dependencies,
        )?;
        let from =
            externalize_position(ctx.txn(), &path, claim.source.end_pos(), &mut dependencies)?;
        let to = GraphNode {
            change: externalize_change(
                ctx.txn(),
                &path,
                claim.name.change,
                "losing name",
                &mut dependencies,
            )?,
            start: claim.name.start,
            end: claim.name.end,
        };
        let introduced_by = externalize_change(
            ctx.txn(),
            &path,
            claim.introduced_by,
            "claim edge introducer",
            &mut dependencies,
        )?;
        edges.push((from, to, introduced_by));
    }

    let result = match direction {
        NameConflictDirection::Solve => {
            GraphOp::solve_name_conflict(path.clone(), surviving_claimant, edges)
        }
        NameConflictDirection::Unsolve => {
            GraphOp::unsolve_name_conflict(path.clone(), surviving_claimant, edges)
        }
    };
    let op = result.map_err(|reason| GlobalizeError::InvalidNameConflict {
        path: path.clone(),
        reason,
    })?;
    for dependency in dependencies {
        ctx.add_dependency(dependency);
    }
    Ok(op)
}

fn validate_claim<T: GraphTxnT>(
    txn: &T,
    path: &str,
    index: usize,
    surviving_claimant: Position<NodeId>,
    claim: &NameConflictClaim,
    direction: NameConflictDirection,
) -> GlobalizeResult<()> {
    if claim.claimant.change.is_root() {
        return invalid_name_conflict(path, format!("losing claim {index} uses ROOT as its inode"));
    }
    if claim.claimant == surviving_claimant {
        return invalid_name_conflict(
            path,
            format!("losing claim {index} is the surviving claimant"),
        );
    }
    if !claim.source.is_empty() {
        return invalid_name_conflict(
            path,
            format!("losing claim {index} source is not a directory inode vertex"),
        );
    }
    if claim.name.is_empty() {
        return invalid_name_conflict(
            path,
            format!("losing claim {index} targets an empty name vertex"),
        );
    }
    if claim.name.change.is_root() || claim.introduced_by.is_root() {
        return invalid_name_conflict(
            path,
            format!("losing claim {index} contains a ROOT name or introducer"),
        );
    }
    validate_inode_vertex(txn, path, "losing claimant", claim.claimant)?;

    let expected = direction.current_flags();
    let mut found_claim_edge = false;
    for edge in txn.iter_adjacent(claim.source, EdgeFlags::empty(), EdgeFlags::all())? {
        let edge = edge?;
        if edge.flag() == expected
            && edge.dest() == claim.name.start_pos()
            && edge.introduced_by() == claim.introduced_by
        {
            found_claim_edge = true;
            break;
        }
    }
    if !found_claim_edge {
        return invalid_name_conflict(
            path,
            format!(
                "{} losing claim {index} has no exact {expected:?} source-to-name edge",
                direction.operation()
            ),
        );
    }

    let alive = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
    let mut reaches_claimant = false;
    for edge in txn.iter_adjacent(claim.name, EdgeFlags::empty(), EdgeFlags::all())? {
        let edge = edge?;
        if edge.flag() == alive && edge.dest() == claim.claimant {
            reaches_claimant = true;
            break;
        }
    }
    if !reaches_claimant {
        return invalid_name_conflict(
            path,
            format!("losing claim {index} name vertex has no exact {alive:?} edge to its claimant"),
        );
    }
    Ok(())
}

fn validate_inode_vertex<T: GraphTxnT>(
    txn: &T,
    path: &str,
    role: &str,
    position: Position<NodeId>,
) -> GlobalizeResult<()> {
    if position.change.is_root() {
        return invalid_name_conflict(path, format!("{role} cannot be ROOT"));
    }
    let inode = GraphNode::new(position.change, position.pos, position.pos);
    if !txn.has_vertex(inode)? {
        return invalid_name_conflict(path, format!("{role} inode vertex is missing"));
    }
    Ok(())
}

fn externalize_position<T: GraphTxnT>(
    txn: &T,
    path: &str,
    position: Position<NodeId>,
    dependencies: &mut HashSet<Hash>,
) -> GlobalizeResult<Position<Option<Hash>>> {
    Ok(Position {
        change: externalize_change(txn, path, position.change, "position", dependencies)?,
        pos: position.pos,
    })
}

fn externalize_change<T: GraphTxnT>(
    txn: &T,
    path: &str,
    node_id: NodeId,
    role: &str,
    dependencies: &mut HashSet<Hash>,
) -> GlobalizeResult<Option<Hash>> {
    if node_id.is_root() {
        return Ok(Some(Hash::NONE));
    }
    let hash = txn
        .get_external(node_id)?
        .ok_or_else(|| GlobalizeError::InvalidNameConflict {
            path: path.to_string(),
            reason: format!("{role} change {node_id} has no external hash"),
        })?;
    dependencies.insert(hash);
    Ok(Some(hash))
}

fn invalid_name_conflict<T>(path: &str, reason: impl Into<String>) -> GlobalizeResult<T> {
    Err(GlobalizeError::InvalidNameConflict {
        path: path.to_string(),
        reason: reason.into(),
    })
}
