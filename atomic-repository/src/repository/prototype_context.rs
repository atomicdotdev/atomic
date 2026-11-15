//! Replayable, sandbox-only context operations. Authority is the immutable
//! GraphOp in selected changes; this projection is only a derived execution plan.
use super::*;
use crate::apply::{InsertOptions, InsertOutcome, InsertStats};
use crate::context_witness_prototype::{self as witness, Objects};
use atomic_core::change::prototype_context::{ContextEffectRef, PrototypeContextEffect as Effect};
use atomic_core::change::VerifiedChange;
use atomic_core::operation::{
    ActorRef, MetadataTarget, MetadataTransition, MetadataValue, OperationKind, OperationScope,
    RepoStateRef, ViewStateRef,
};
use atomic_core::pristine::{CapabilityMutTxnT, CapabilityTxnT, PROTOTYPE_CONTEXT_CAPABILITY};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

fn refused(message: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!("prototype context: {message}"),
    }
}

fn supported_line_order_reference(line: &atomic_core::change::LineOps, symbol: u64) -> bool {
    use atomic_core::crdt::BranchOp;
    match line.operation() {
        BranchOp::Reparent { .. } => true,
        // The new line is owned by the consumer; only its predecessor needs
        // the completed foreign identity. This is line-order context too, not
        // an update to foreign content. The witness separately rejects token
        // occurrences and validates every predecessor declaration.
        BranchOp::Insert { after, .. } => {
            line.branch_id().change_id().is_root()
                && after.is_some_and(|id| id.change_id().get() == symbol)
        }
        _ => false,
    }
}

#[cfg(test)]
mod line_order_tests {
    use super::*;
    use atomic_core::{
        change::LineOps,
        crdt::{BranchId, BranchOp},
    };

    #[test]
    fn inserted_line_predecessors_are_order_context_not_foreign_content_updates() {
        let foreign = BranchId::new(NodeId::new(15), 2);
        let own = BranchId::new(NodeId::ROOT, 3);
        assert!(supported_line_order_reference(
            &LineOps::new(
                own,
                BranchOp::Insert {
                    after: Some(foreign),
                    content: vec![]
                }
            ),
            15
        ));
        assert!(!supported_line_order_reference(
            &LineOps::new(
                foreign,
                BranchOp::Insert {
                    after: Some(foreign),
                    content: vec![]
                }
            ),
            15
        ));
        assert!(!supported_line_order_reference(
            &LineOps::new(
                foreign,
                BranchOp::Delete {
                    branch: foreign,
                    content: vec![]
                }
            ),
            15
        ));
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Projection {
    pub(crate) effects: BTreeMap<ContextEffectRef, Effect>,
    claims: BTreeMap<(Hash, u64), BTreeMap<ContextEffectRef, Hash>>,
    cycle: bool,
    pub(crate) digest: Hash,
}

impl Projection {
    fn build(objects: &Objects) -> Result<Self, RepositoryError> {
        let mut effects = BTreeMap::new();
        for (hash, change) in objects {
            let found: Vec<_> = change
                .hunks()
                .iter()
                .enumerate()
                .filter_map(|(ordinal, op)| match op {
                    GraphOp::PrototypeContext { effect } => Some((ordinal, effect)),
                    _ => None,
                })
                .collect();
            if found.is_empty() {
                continue;
            }
            if found.len() != 1 {
                return Err(refused(
                    "one context effect per experimental patch is supported",
                ));
            }
            let (ordinal, effect) = found[0];
            if !matches!(effect, Effect::DeclareOwnNames { .. })
                && (change.hunks().len() != 1
                    || !change.file_ops().is_empty()
                    || !change.contents.is_empty())
            {
                return Err(refused("completion carriers require exactly one context effect and no content/semantic writes"));
            }
            match effect {
                Effect::DeclareOwnNames { owners } => {
                    use atomic_core::change::semantic_context::{SemanticNameFrame, SemanticOwner};
                    let frame = SemanticNameFrame::new(
                        owners
                            .iter()
                            .map(|(symbol, owner)| SemanticOwner {
                                authored_id: *symbol,
                                owner: *owner,
                            })
                            .collect(),
                    )
                    .map_err(|e| refused(format!("own-name declaration: {e}")))?;
                    frame
                        .validate_for(change.file_ops())
                        .map_err(|e| refused(format!("own-name declaration: {e}")))?;
                    if change.hashed.span.is_some()
                        || owners.is_empty()
                        || owners.iter().any(|(_, owner)| {
                            *owner == *hash
                                || !change.dependencies().contains(owner)
                                || !objects.contains_key(owner)
                        })
                    {
                        return Err(refused("own-name declaration must exactly cover the new patch's foreign names with distinct explicit prerequisites"));
                    }
                }
                Effect::Bind {
                    consumer,
                    symbol,
                    provider,
                } => {
                    if !change.dependencies().contains(consumer)
                        || !change.dependencies().contains(provider)
                    {
                        return Err(refused(
                            "binding must explicitly require its consumer and provider",
                        ));
                    }
                    let proof =
                        witness::proof(objects, *consumer, *symbol, *provider).map_err(refused)?;
                    witness::verify_positive_witness(objects, &proof).map_err(refused)?;
                    if proof["consumer_order_anomalies"]
                        .as_array()
                        .is_none_or(|v| !v.is_empty())
                    {
                        return Err(refused(
                            "completion proves context, not an invalid self-parent order write",
                        ));
                    }
                    // Complete line-order operands, including the predecessor
                    // of a consumer-owned inserted line.
                    // Undefined content/token updates cannot be deferred safely.
                    for ops in objects[consumer].file_ops() {
                        for line in ops.line_ops() {
                            let mut container = atomic_core::change::FileOps::edit(
                                atomic_core::crdt::TrunkId::new(NodeId::ROOT, 0),
                                String::new(),
                            );
                            container.add_line_op(line.clone());
                            if container
                                .referenced_node_ids()
                                .iter()
                                .any(|id| id.get() == *symbol)
                                && !supported_line_order_reference(line, *symbol)
                            {
                                return Err(refused(
                                    "native prototype supports only line-order context completion",
                                ));
                            }
                        }
                    }
                }
                Effect::Reverse { observed } => {
                    if observed.ordinal != 0 || !change.dependencies().contains(&observed.change) {
                        return Err(refused(
                            "inverse must require one exact observed context effect",
                        ));
                    }
                    let original = objects
                        .get(&observed.change)
                        .ok_or_else(|| refused("missing inverse context"))?;
                    if original.hunks().len() != 1
                        || !matches!(
                            original.hunks()[0],
                            GraphOp::PrototypeContext {
                                effect: Effect::Bind { .. } | Effect::Reverse { .. }
                            }
                        )
                    {
                        return Err(refused("inverse target is not a context operation"));
                    }
                }
            }
            effects.insert(
                ContextEffectRef {
                    change: *hash,
                    ordinal: ordinal as u32,
                },
                effect.clone(),
            );
        }
        fn active(
            id: ContextEffectRef,
            effects: &BTreeMap<ContextEffectRef, Effect>,
            visiting: &mut BTreeSet<ContextEffectRef>,
        ) -> Result<bool, RepositoryError> {
            if !visiting.insert(id) {
                return Err(refused("cyclic inverse effect context"));
            }
            for (candidate, effect) in effects {
                if matches!(effect, Effect::Reverse { observed } if *observed == id)
                    && active(*candidate, effects, visiting)?
                {
                    visiting.remove(&id);
                    return Ok(false);
                }
            }
            visiting.remove(&id);
            Ok(true)
        }
        let mut claims = BTreeMap::<_, BTreeMap<_, _>>::new();
        for (id, effect) in &effects {
            if let Effect::DeclareOwnNames { owners } = effect {
                for (symbol, provider) in owners {
                    claims
                        .entry((id.change, *symbol))
                        .or_default()
                        .insert(*id, *provider);
                }
            }
            if let Effect::Bind {
                consumer,
                symbol,
                provider,
            } = effect
            {
                let slot = claims.entry((*consumer, *symbol)).or_default();
                if active(*id, &effects, &mut BTreeSet::new())? {
                    slot.insert(*id, *provider);
                }
            }
        }
        let mut edges: BTreeMap<Hash, BTreeSet<Hash>> = objects
            .iter()
            .map(|(h, c)| (*h, c.dependencies().iter().copied().collect()))
            .collect();
        for ((consumer, _), values) in &claims {
            edges.entry(*consumer).or_default().extend(values.values());
        }
        fn cycle(
            node: Hash,
            edges: &BTreeMap<Hash, BTreeSet<Hash>>,
            pending: &mut BTreeSet<Hash>,
            done: &mut BTreeSet<Hash>,
        ) -> bool {
            if done.contains(&node) {
                return false;
            }
            if !pending.insert(node) {
                return true;
            }
            if edges
                .get(&node)
                .is_some_and(|next| next.iter().any(|h| cycle(*h, edges, pending, done)))
            {
                return true;
            }
            pending.remove(&node);
            done.insert(node);
            false
        }
        let mut done = BTreeSet::new();
        let cycle = edges
            .keys()
            .any(|h| cycle(*h, &edges, &mut BTreeSet::new(), &mut done));
        let rows:Vec<_>=claims.iter().map(|((consumer,symbol),values)|serde_json::json!({"consumer":consumer,"symbol":symbol,"claims":values.iter().collect::<Vec<_>>() })).collect();
        let digest = Hash::of(&serde_json::to_vec(&(cycle, rows)).map_err(refused)?);
        Ok(Self {
            effects,
            claims,
            cycle,
            digest,
        })
    }

    pub(crate) fn bindings(&self, consumer: Hash) -> Result<Vec<(u64, Hash)>, String> {
        if self.cycle {
            return Err("prototype context: cyclic required context".into());
        }
        let mut out = Vec::new();
        for ((patch, symbol), claims) in &self.claims {
            if *patch != consumer {
                continue;
            }
            let providers: BTreeSet<_> = claims.values().copied().collect();
            match providers.len() {
                0 => {
                    return Err(format!(
                        "prototype context: unresolved name {symbol} after exact reversal"
                    ))
                }
                1 => out.push((*symbol, *providers.first().unwrap())),
                _ => {
                    return Err(format!(
                        "prototype context: conflicting providers for name {symbol}"
                    ))
                }
            }
        }
        Ok(out)
    }

    pub(crate) fn require_complete(&self) -> Result<(), String> {
        for (consumer, _) in self.claims.keys() {
            self.bindings(*consumer)?;
        }
        Ok(())
    }

    fn order(&self, objects: &Objects) -> Result<Vec<Hash>, RepositoryError> {
        let mut order = Vec::new();
        let mut done = BTreeSet::new();
        let mut pending = BTreeSet::new();
        fn visit(
            h: Hash,
            p: &Projection,
            objects: &Objects,
            order: &mut Vec<Hash>,
            done: &mut BTreeSet<Hash>,
            pending: &mut BTreeSet<Hash>,
        ) -> Result<(), RepositoryError> {
            if done.contains(&h) {
                return Ok(());
            }
            if !pending.insert(h) {
                return Err(refused("cyclic composed patch prerequisites"));
            }
            let change = objects
                .get(&h)
                .ok_or_else(|| refused("missing composed object"))?;
            let mut deps: BTreeSet<_> = change.dependencies().iter().copied().collect();
            if !p.cycle {
                for ((consumer, _), claims) in &p.claims {
                    if *consumer == h {
                        let providers: BTreeSet<_> = claims.values().copied().collect();
                        if providers.len() == 1 {
                            deps.extend(providers);
                        }
                    }
                }
            }
            for dep in deps {
                visit(dep, p, objects, order, done, pending)?;
            }
            pending.remove(&h);
            done.insert(h);
            order.push(h);
            Ok(())
        }
        for hash in objects.keys() {
            visit(*hash, self, objects, &mut order, &mut done, &mut pending)?;
        }
        Ok(order)
    }

    pub(crate) fn report(&self) -> serde_json::Value {
        serde_json::json!({"experimental":true,"cycle":self.cycle,"digest":self.digest,
            "names":self.claims.iter().map(|((consumer,symbol),claims)| {
                let values:BTreeSet<_>=claims.values().copied().collect();
                serde_json::json!({"consumer":consumer,"symbol":symbol,"status":match values.len(){0=>"unbound",1=>"bound",_=>"conflict"},"providers":values,"effects":claims.keys().collect::<Vec<_>>()})
            }).collect::<Vec<_>>()})
    }

    pub(crate) fn required_effects_for(
        &self,
        repo: &Repository,
        change: &Change,
    ) -> Result<Vec<Hash>, RepositoryError> {
        fn anchors(change: &Change, paths: Option<&BTreeSet<&str>>) -> BTreeSet<(Hash, u64)> {
            let mut anchors = BTreeSet::new();
            for op in change.hunks() {
                if paths.is_some_and(|paths| op.path().is_none_or(|path| !paths.contains(path))) {
                    continue;
                }
                if let GraphOp::SetAttr { inode, .. } = op {
                    if let Some(hash) = inode.change {
                        anchors.insert((hash, inode.pos.get()));
                    }
                }
                for atom in op.atoms() {
                    let inode = match atom {
                        atomic_core::change::AtomRef::Insertion(i)
                        | atomic_core::change::AtomRef::Atom(
                            atomic_core::change::Atom::Insertion(i),
                        ) => i.inode,
                        atomic_core::change::AtomRef::EdgeUpdate(e)
                        | atomic_core::change::AtomRef::Atom(
                            atomic_core::change::Atom::EdgeUpdate(e),
                        ) => e.inode,
                    };
                    if let Some(hash) = inode.change {
                        anchors.insert((hash, inode.pos.get()));
                    }
                }
            }
            anchors
        }
        self.require_complete().map_err(refused)?;
        let touched = anchors(change, None);
        let mut required = BTreeSet::new();
        loop {
            let mut roots = change.dependencies().to_vec();
            roots.extend(&required);
            let scope = repo.prototype_load_scope(&roots)?;
            let before = required.len();
            for ((consumer_hash, symbol), claims) in &self.claims {
                let consumer = repo.load_change(consumer_hash)?;
                let paths = consumer
                    .file_ops()
                    .iter()
                    .filter(|ops| {
                        ops.referenced_node_ids()
                            .iter()
                            .any(|id| id.get() == *symbol)
                    })
                    .map(|ops| ops.path())
                    .collect::<BTreeSet<_>>();
                // Native replay executes the entire required patch, even when
                // its original author bundled several files. Retain the
                // completion context of every required consumer, transitively,
                // plus same-inode order context actually used in assembly.
                // Other consumers elsewhere in the view remain unrelated.
                if scope.contains_key(consumer_hash)
                    || !anchors(&consumer, Some(&paths)).is_disjoint(&touched)
                {
                    required.extend(claims.keys().map(|effect| effect.change));
                }
            }
            if required.len() == before {
                break;
            }
        }
        Ok(required.into_iter().collect())
    }
}

impl Repository {
    pub(crate) fn prototype_ordered_scope(
        &self,
        projection: &Projection,
        roots: &[Hash],
    ) -> Result<Vec<Hash>, RepositoryError> {
        projection.order(&self.prototype_load_scope(roots)?)
    }
    fn prototype_save_validated_context(&self, change: &Change) -> Result<Hash, RepositoryError> {
        let mut objects = self.prototype_load_scope(change.dependencies())?;
        let mut bytes = Vec::new();
        let hash = change.serialize(&mut bytes).map_err(refused)?;
        objects.insert(hash, change.clone());
        Projection::build(&objects)?;
        self.save_change_bytes(&hash, &bytes, change)
    }
    fn prototype_load_scope(&self, roots: &[Hash]) -> Result<Objects, RepositoryError> {
        let mut objects = Objects::new();
        let mut pending = roots.to_vec();
        while let Some(hash) = pending.pop() {
            if objects.contains_key(&hash) {
                continue;
            }
            if objects.len() > 4096 {
                return Err(refused("sandbox context budget exceeded"));
            }
            let bytes = std::fs::read(self.change_store.change_path(&hash))
                .map_err(|e| refused(format!("missing {hash}: {e}")))?;
            let verified = VerifiedChange::from_bytes(&bytes).map_err(refused)?;
            if verified.hash() != hash {
                return Err(refused("stored object hash mismatch"));
            }
            let change = verified.change().clone();
            pending.extend(change.dependencies());
            objects.insert(hash, change);
        }
        Ok(objects)
    }

    pub(crate) fn prototype_projection_for_hashes(
        &self,
        roots: &[Hash],
    ) -> Result<Arc<Projection>, RepositoryError> {
        Ok(Arc::new(Projection::build(
            &self.prototype_load_scope(roots)?,
        )?))
    }

    pub(crate) fn prototype_projection_for_txn<T: GraphTxnT + ViewTxnT>(
        &self,
        txn: &T,
        view: &str,
        additional: &[Hash],
    ) -> Result<Arc<Projection>, RepositoryError> {
        let state = txn
            .get_view(view)?
            .ok_or_else(|| refused("missing target view"))?;
        let visible = super::effective_projection_closure(txn, &state)?;
        let mut hashes = additional.to_vec();
        for id in visible.iter_dependency_first() {
            if !id.is_root() {
                hashes.push(
                    txn.get_external(*id)?
                        .ok_or_else(|| refused("missing visible object"))?,
                );
            }
        }
        self.prototype_projection_for_hashes(&hashes)
    }

    /// Create a new experimental completion patch; never edits its consumer.
    pub fn prototype_create_completion(
        &self,
        scope: &[Hash],
        consumer: Hash,
        symbol: u64,
    ) -> Result<Hash, RepositoryError> {
        self.prototype_require_draft()?;
        let objects = self.prototype_load_scope(scope)?;
        let proof = witness::reconstruct(&objects, consumer, symbol).map_err(refused)?;
        let provider = serde_json::from_value(proof["provider"].clone()).map_err(refused)?;
        let mut change = Change::empty(ChangeHeader::new(
            "EXPERIMENTAL SANDBOX: complete a patch context name",
        ));
        change.hashed.dependencies = vec![consumer, provider];
        change.hashed.dependencies.sort();
        change.hashed.dependencies.dedup();
        change.hashed.hunks.push(GraphOp::PrototypeContext {
            effect: Effect::Bind {
                consumer,
                symbol,
                provider,
            },
        });
        self.prototype_save_validated_context(&change)
    }

    /// Build the exact inverse of an observed experimental context effect.
    pub fn prototype_reverse_context(&self, observed: Hash) -> Result<Hash, RepositoryError> {
        self.prototype_require_draft()?;
        let mut change = Change::empty(ChangeHeader::new(
            "EXPERIMENTAL SANDBOX: reverse an exact context effect",
        ));
        change.hashed.dependencies = vec![observed];
        change.hashed.hunks.push(GraphOp::PrototypeContext {
            effect: Effect::Reverse {
                observed: ContextEffectRef {
                    change: observed,
                    ordinal: 0,
                },
            },
        });
        self.prototype_save_validated_context(&change)
    }

    fn prototype_require_draft(&self) -> Result<String, RepositoryError> {
        let name = self.authoritative_working_copy_view_name()?;
        if !self.get_view_info(&name)?.scope.is_draft() {
            return Err(refused(
                "sandbox completion operations require a draft working copy",
            ));
        }
        Ok(name)
    }

    /// Inspect the selected ordinary context-operation facts, including conflicts.
    pub fn prototype_context_report(&self) -> Result<serde_json::Value, RepositoryError> {
        let view = self.prototype_require_draft()?;
        let txn = self.pristine.read_txn()?;
        Ok(self
            .prototype_projection_for_txn(&txn, &view, &[])?
            .report())
    }

    /// Atomically compose a sandbox patch set with its explicit context effects.
    /// No membership is published under a merely prospective completion.
    pub fn prototype_insert_composed(
        &self,
        roots: &[Hash],
    ) -> Result<InsertOutcome, RepositoryError> {
        let view_name = self.prototype_require_draft()?;
        let wc = self.require_working_copy_id()?;
        let lock = self.try_lock_operation(wc)?;
        if let OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: OperationScope::WorkingCopy(wc).to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        let (objects, projection, missing, before, after, transitions) = {
            let txn = self.pristine.read_txn()?;
            let view = txn
                .get_view(&view_name)?
                .ok_or_else(|| refused("missing sandbox view"))?;
            let visible = super::effective_projection_closure(&txn, &view)?;
            let mut all = roots.to_vec();
            for id in visible.iter_dependency_first() {
                if !id.is_root() {
                    all.push(
                        txn.get_external(*id)?
                            .ok_or_else(|| refused("missing visible hash"))?,
                    );
                }
            }
            let objects = self.prototype_load_scope(&all)?;
            let projection = Arc::new(Projection::build(&objects)?);
            // Until conflict-state projection is implemented, refuse the whole
            // composition rather than retaining stale semantic table writes.
            projection.require_complete().map_err(refused)?;
            let old_projection = self.prototype_projection_for_txn(&txn, &view_name, &[])?;
            for (consumer, _) in projection.claims.keys() {
                if let Some(id) = txn.get_internal(consumer)? {
                    if txn.was_change_applied(id)?
                        && old_projection.bindings(*consumer).ok()
                            != projection.bindings(*consumer).ok()
                    {
                        return Err(refused("cannot reuse ambient semantic writes under a different completed context; use a fresh sandbox"));
                    }
                }
            }
            let order = projection.order(&objects)?;
            let mut missing = Vec::new();
            for hash in order {
                if txn
                    .get_internal(&hash)?
                    .is_none_or(|id| !visible.contains(id))
                {
                    missing.push(hash);
                }
            }
            let mut state = view.state;
            let mut transitions = Vec::new();
            for (i, hash) in missing.iter().enumerate() {
                state = state.next(hash);
                transitions.push(MetadataTransition {
                    target: MetadataTarget::ViewChange {
                        view: view_name.clone(),
                        change: *hash,
                    },
                    expected_old: MetadataValue::Absent,
                    expected_new: MetadataValue::Sequence(view.change_count + i as u64),
                });
            }
            if objects.values().any(|c| {
                c.hunks()
                    .iter()
                    .any(|h| matches!(h, GraphOp::PrototypeContext { .. }))
            }) && txn
                .required_capability_version(PROTOTYPE_CONTEXT_CAPABILITY.id())?
                .is_none()
            {
                transitions.push(MetadataTransition {
                    target: MetadataTarget::Capability {
                        id: PROTOTYPE_CONTEXT_CAPABILITY.id().into(),
                    },
                    expected_old: MetadataValue::Absent,
                    expected_new: MetadataValue::Sequence(1),
                });
            }
            let old = self.working_copy_record(wc)?;
            let mut new = old.clone();
            new.desired_state = state;
            let before = RepoStateRef {
                view: Some(ViewStateRef {
                    name: view_name.clone(),
                    state: view.state,
                    set_id: None,
                }),
                working_copy: Some(super::operation::working_copy_state_ref(old)),
                git: None,
            };
            let after = RepoStateRef {
                view: Some(ViewStateRef {
                    name: view_name.clone(),
                    state,
                    set_id: None,
                }),
                working_copy: Some(super::operation::working_copy_state_ref(new)),
                git: None,
            };
            (objects, projection, missing, before, after, transitions)
        };
        if missing.is_empty() {
            let v = self.get_view_info(&view_name)?;
            return Ok(InsertOutcome::new(
                v.state,
                v.change_count,
                false,
                InsertStats::default(),
            ));
        }
        let op = self.prepare_metadata_operation(
            &lock,
            OperationKind::Insert,
            None,
            before,
            after,
            transitions,
            missing.clone(),
            ActorRef::System {
                name: "prototype-composed-insert".into(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;
        let result = (|| {
            let mut txn = self.pristine.write_txn()?;
            let mut stats = InsertStats::default();
            let mut last = None;
            if !projection.effects.is_empty() {
                txn.put_required_capability(PROTOTYPE_CONTEXT_CAPABILITY)?;
            }
            let mut options = InsertOptions::default().view(&view_name);
            options.prototype_context = Some(projection);
            for hash in &missing {
                let change = &objects[hash];
                self.ensure_change_allowed_in_view(&txn, &view_name, change)?;
                let existing = txn.get_internal(hash)?;
                let applied = match existing {
                    Some(id) => txn.was_change_applied(id)?,
                    None => false,
                };
                let id = txn.register_change(hash)?;
                txn.put_change_deps(id, change.dependencies())?;
                let tree = if applied {
                    self.plan_tree_projection(
                        &mut txn,
                        id,
                        *hash,
                        &Change::empty(ChangeHeader::new("composed reference only")),
                        &[],
                        false,
                    )?
                } else {
                    self.plan_tree_projection(&mut txn, id, *hash, change, &[], false)?
                };
                tree.apply_prerequisites(&mut txn)?;
                let out = if change.hashed.span.is_some() && !applied {
                    super::insert::write_recorded_graph_and_spans(
                        self, &mut txn, &view_name, id, hash, change, &options,
                    )
                    .map_err(|e| refused(format!("while composing {hash}: {e}")))?
                } else {
                    crate::apply::write_change_to_graph(
                        &mut txn, &view_name, id, hash, change, &options, applied,
                    )
                    .map_err(|e| refused(format!("while composing {hash}: {e}")))?
                };
                self.apply_tree_projection(&mut txn, &tree, &view_name, false)?;
                stats.merge(out.stats.clone());
                last = Some(out);
            }
            if txn.kg_writes_enabled()? {
                self.project_view_kg(&mut txn)?;
            }
            txn.commit()?;
            let mut out = last.ok_or_else(|| refused("empty composition"))?;
            out.stats = stats;
            Ok(out)
        })();
        match result {
            Ok(out) => {
                self.apply_operation_metadata_locked(&lock, op.id())?;
                self.finalize_operation_verified(&lock, op.id())?;
                Ok(out)
            }
            Err(error) => {
                self.abort_prepared_metadata_operation(&lock, &op)?;
                Err(error)
            }
        }
    }
}
