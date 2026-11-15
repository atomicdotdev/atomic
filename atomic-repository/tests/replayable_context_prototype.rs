//! Experimental immutable context operations, never a published repair.
#![cfg(feature = "prototype-context-completion")]
use atomic_core::change::prototype_context::{ContextEffectRef, PrototypeContextEffect as Effect};
use atomic_core::change::{Change, ChangeHeader, GraphOp, LineOps, VerifiedChange};
use atomic_core::crdt::{BranchId, BranchOp};
use atomic_core::pristine::{CapabilityTxnT, PROTOTYPE_CONTEXT_CAPABILITY};
use atomic_core::types::{Hash, NodeId};
use atomic_repository::{
    record::RecordOptions, InsertOptions, Repository, TrackingOptions, UnrecordOptions,
};

const VIEW: &str = "context-sandbox";
fn sandbox(path: &std::path::Path) -> Repository {
    let mut r = Repository::init(path).unwrap();
    r.create_draft_view(VIEW, "dev").unwrap();
    r.switch_view(r.require_working_copy_id().unwrap(), VIEW)
        .unwrap();
    r
}
fn record(r: &Repository, text: &[u8]) -> Hash {
    std::fs::write(r.root().join("f"), text).unwrap();
    *r.record(
        r.require_working_copy_id().unwrap(),
        ChangeHeader::new("synthetic context fixture"),
        RecordOptions::new()
            .paths(vec!["f"])
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap()
    .hash()
}
fn fixture(r: &Repository) -> (Vec<Hash>, Hash, Hash) {
    std::fs::write(r.root().join("f"), b"alpha\nbeta\ntheta\n").unwrap();
    r.add(
        r.require_working_copy_id().unwrap(),
        "f",
        TrackingOptions::default(),
    )
    .unwrap();
    let base = record(r, b"alpha\nbeta\ntheta\n");
    let provider = record(r, b"alpha\nbeta\ntheta\ngamma\n");
    r.unrecord(&provider, UnrecordOptions::default()).unwrap();
    let original = record(r, b"ALPHA\nbeta\ntheta\n");
    let mut consumer = r.load_change(&original).unwrap();
    let foreign = BranchId::new(NodeId::new(15), 0);
    consumer.hashed.file_ops[0].add_line_op(LineOps::new(
        foreign,
        BranchOp::Reparent {
            branch: foreign,
            new_after: Some(BranchId::new(NodeId::new(1), 2)),
        },
    ));
    consumer.hashed.header = ChangeHeader::new("synthetic consumer missing an order operand owner");
    consumer.hashed.metadata = b"immutable synthetic agent attachment".to_vec();
    let consumer = r.save_change(&consumer).unwrap();
    (vec![base, provider, consumer], consumer, provider)
}
fn transfer(source: &Repository, target: &Repository, hashes: &[Hash]) {
    for hash in hashes {
        let bytes = std::fs::read(source.change_store().change_path(hash)).unwrap();
        assert_eq!(VerifiedChange::from_bytes(&bytes).unwrap().hash(), *hash);
        target.import_change_bytes(hash, &bytes).unwrap();
        assert_eq!(
            std::fs::read(target.change_store().change_path(hash)).unwrap(),
            bytes
        );
    }
}
fn effect(r: &Repository, message: &str, effect: Effect, dependencies: Vec<Hash>) -> Hash {
    let mut c = Change::empty(ChangeHeader::new(message));
    c.hashed.hunks.push(GraphOp::PrototypeContext { effect });
    c.hashed.dependencies = dependencies;
    c.hashed.dependencies.sort();
    c.hashed.dependencies.dedup();
    r.save_change(&c).unwrap()
}

#[test]
fn saved_completion_replays_records_and_keeps_originals() {
    let src_dir = tempfile::tempdir().unwrap();
    let src = sandbox(src_dir.path());
    let (mut scope, consumer, _) = fixture(&src);
    let out_dir = tempfile::tempdir().unwrap();
    let out = sandbox(out_dir.path());
    transfer(&src, &out, &scope);
    let before = out.view_set_id(VIEW).unwrap();
    assert!(out.prototype_insert_composed(&scope).is_err());
    assert_eq!(out.view_set_id(VIEW).unwrap(), before);
    let completion = out
        .prototype_create_completion(&scope, consumer, 15)
        .unwrap();
    // An ordinary per-patch insertion must not execute half a composition.
    assert!(out
        .insert_change_rec(&completion, InsertOptions::default().view(VIEW))
        .is_err());
    assert_eq!(out.view_set_id(VIEW).unwrap(), before);
    scope.push(completion);
    out.prototype_insert_composed(&scope).unwrap();
    assert_eq!(
        out.pristine()
            .read_txn()
            .unwrap()
            .required_capability_version(PROTOTYPE_CONTEXT_CAPABILITY.id())
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        out.prototype_context_report().unwrap()["names"][0]["status"],
        "bound"
    );
    let set = out.view_set_id(VIEW).unwrap();
    out.prototype_insert_composed(&scope).unwrap();
    assert_eq!(out.view_set_id(VIEW).unwrap(), set);
    out.materialize_view_to(VIEW, out.root()).unwrap();
    let mut text = std::fs::read(out.root().join("f")).unwrap();
    text.extend_from_slice(b"delta\n");
    let candidate = record(&out, &text);
    let authored = out.load_change(&candidate).unwrap();
    let patch = authored
        .hashed
        .span
        .as_ref()
        .expect("ordinary recorder uses spans");
    assert_eq!(
        patch.version,
        atomic_core::change::span_patch::LEGACY_SOURCE_VERSION
    );
    assert!(authored.file_ops().iter().any(|ops| matches!(
        ops.trunk_op(),
        Some(atomic_core::crdt::TrunkOp::ProjectedSpanEdit { .. })
    )));
    assert!(out
        .load_change(&candidate)
        .unwrap()
        .dependencies()
        .contains(&completion));
    scope.push(candidate);
    let dst_dir = tempfile::tempdir().unwrap();
    let dst = sandbox(dst_dir.path());
    use atomic_core::pristine::{GraphTxnT, MutTxnT};
    for i in 0..7 {
        let hash = dst
            .save_change(&Change::empty(ChangeHeader::new(format!(
                "unselected allocation padding {i}"
            ))))
            .unwrap();
        let mut txn = dst.pristine().write_txn().unwrap();
        txn.register_change(&hash).unwrap();
        txn.commit().unwrap();
    }
    scope.reverse();
    transfer(&out, &dst, &scope);
    dst.prototype_insert_composed(&[candidate]).unwrap();
    dst.materialize_view_to(VIEW, dst.root()).unwrap();
    assert_ne!(
        out.pristine()
            .read_txn()
            .unwrap()
            .get_internal(&candidate)
            .unwrap(),
        dst.pristine()
            .read_txn()
            .unwrap()
            .get_internal(&candidate)
            .unwrap()
    );
    assert_eq!(std::fs::read(dst.root().join("f")).unwrap(), text);
    assert_eq!(
        dst.load_change(&consumer).unwrap().hashed.metadata,
        b"immutable synthetic agent attachment"
    );
    transfer(
        &src,
        &dst,
        &scope
            .iter()
            .copied()
            .filter(|h| *h != completion && *h != candidate)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn incomplete_own_name_declarations_refuse_before_membership() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = sandbox(source_dir.path());
    let (mut scope, consumer, _) = fixture(&source);
    let dir = tempfile::tempdir().unwrap();
    let r = sandbox(dir.path());
    transfer(&source, &r, &scope);
    scope.push(r.prototype_create_completion(&scope, consumer, 15).unwrap());
    r.prototype_insert_composed(&scope).unwrap();
    r.materialize_view_to(VIEW, r.root()).unwrap();
    let mut bytes = std::fs::read(r.root().join("f")).unwrap();
    bytes.extend_from_slice(b"delta\n");
    let good = record(&r, &bytes);
    let mut bad = r.load_change(&good).unwrap();
    for op in &mut bad.hashed.hunks {
        if let GraphOp::PrototypeContext {
            effect: Effect::DeclareOwnNames { owners },
        } = op
        {
            owners.pop();
        }
    }
    let bad = r.save_change(&bad).unwrap();
    let before = r.view_set_id(VIEW).unwrap();
    assert!(r
        .prototype_insert_composed(&[bad])
        .unwrap_err()
        .to_string()
        .contains("own-name declaration"));
    assert_eq!(r.view_set_id(VIEW).unwrap(), before);
}

#[test]
fn exact_reversals_and_equal_claims_never_erase_another_claim() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = sandbox(source_dir.path());
    let (mut scope, consumer, provider) = fixture(&source);
    let dir = tempfile::tempdir().unwrap();
    let r = sandbox(dir.path());
    transfer(&source, &r, &scope);
    let a = r.prototype_create_completion(&scope, consumer, 15).unwrap();
    let b = effect(
        &r,
        "separate equal binding",
        Effect::Bind {
            consumer,
            symbol: 15,
            provider,
        },
        vec![consumer, provider],
    );
    let reverse_a = r.prototype_reverse_context(a).unwrap();
    scope.extend([a, b, reverse_a]);
    r.prototype_insert_composed(&scope).unwrap();
    let report = r.prototype_context_report().unwrap();
    assert_eq!(report["names"][0]["status"], "bound");
    assert_eq!(report["names"][0]["effects"].as_array().unwrap().len(), 1);
    let reverse_b = r.prototype_reverse_context(b).unwrap();
    let before = r.view_set_id(VIEW).unwrap();
    assert!(r
        .prototype_insert_composed(&[reverse_b])
        .unwrap_err()
        .to_string()
        .contains("unresolved"));
    assert_eq!(r.view_set_id(VIEW).unwrap(), before);
    // Reverse-of-reverse reactivates exactly B, not A. Compose both at once.
    let restore_b = r.prototype_reverse_context(reverse_b).unwrap();
    r.prototype_insert_composed(&[restore_b]).unwrap();
    assert_eq!(
        r.prototype_context_report().unwrap()["names"][0]["effects"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn invalid_endpoints_and_reverse_ordinals_abort_without_membership_or_capability() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = sandbox(source_dir.path());
    let (scope, consumer, _) = fixture(&source);
    let dir = tempfile::tempdir().unwrap();
    let r = sandbox(dir.path());
    transfer(&source, &r, &scope);
    let before = r.view_set_id(VIEW).unwrap();
    for hash in [
        effect(
            &r,
            "wrong provider",
            Effect::Bind {
                consumer,
                symbol: 15,
                provider: scope[0],
            },
            vec![consumer, scope[0]],
        ),
        effect(
            &r,
            "invalid effect ordinal",
            Effect::Reverse {
                observed: ContextEffectRef {
                    change: consumer,
                    ordinal: 9,
                },
            },
            vec![consumer],
        ),
    ] {
        assert!(r.prototype_insert_composed(&[hash]).is_err());
        assert_eq!(r.view_set_id(VIEW).unwrap(), before);
        assert_eq!(
            r.pristine()
                .read_txn()
                .unwrap()
                .required_capability_version(PROTOTYPE_CONTEXT_CAPABILITY.id())
                .unwrap(),
            None
        );
    }
}

#[test]
fn competing_valid_providers_are_a_conflict_not_an_arrival_order_choice() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = sandbox(source_dir.path());
    let (scope, consumer, provider) = fixture(&source);
    let dir = tempfile::tempdir().unwrap();
    let r = sandbox(dir.path());
    transfer(&source, &r, &scope);
    let a = r.prototype_create_completion(&scope, consumer, 15).unwrap();
    let mut duplicate = r.load_change(&provider).unwrap();
    duplicate.hashed.header = ChangeHeader::new("synthetic second equally valid provider");
    let other = r.save_change(&duplicate).unwrap();
    let b = effect(
        &r,
        "conflicting binding",
        Effect::Bind {
            consumer,
            symbol: 15,
            provider: other,
        },
        vec![consumer, other],
    );
    let before = r.view_set_id(VIEW).unwrap();
    for roots in [[a, b], [b, a]] {
        assert!(r
            .prototype_insert_composed(&roots)
            .unwrap_err()
            .to_string()
            .contains("conflicting providers"));
        assert_eq!(r.view_set_id(VIEW).unwrap(), before);
    }
}
