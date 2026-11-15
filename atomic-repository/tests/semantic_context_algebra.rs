//! Executable specification of the proposed context-relation algebra.
//!
//! This is NOT a native opcode, validator, migration or attachment-lookup test.
//! Fixture declarations stand in for graph endpoints independently verified by
//! a future native validator. See docs/semantic-context-completion.md.
use atomic_core::types::Hash;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Name {
    consumer: Hash,
    symbol: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Effect {
    Bind { name: Name, provider: Hash },
    Reverse { observed: Hash },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Model {
    required: BTreeMap<Name, (Hash, BTreeSet<u32>)>,
    provided: BTreeMap<Hash, (Hash, BTreeSet<u32>)>,
    effects: BTreeMap<Hash, Effect>,
    attachments: BTreeMap<Hash, Vec<u8>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Projection {
    active_claims: BTreeMap<Name, BTreeMap<Hash, Hash>>,
    cycle: bool,
}

fn h(s: &str) -> Hash {
    Hash::of(s.as_bytes())
}

fn name(consumer: &str) -> Name {
    Name {
        consumer: h(consumer),
        symbol: 15,
    }
}

fn fixture() -> Model {
    let mut model = Model::default();
    for patch in ["P", "X", "Q", "Y"] {
        model
            .required
            .insert(name(patch), (h("inode"), BTreeSet::from([2, 3])));
        model
            .provided
            .insert(h(patch), (h("inode"), BTreeSet::from([2, 3, 4])));
        model.attachments.insert(
            h(patch),
            format!("unchanged synthetic attachment for {patch}").into_bytes(),
        );
    }
    model.provided.insert(
        h("wrong-inode"),
        (h("other inode"), BTreeSet::from([2, 3, 4])),
    );
    model
        .provided
        .insert(h("missing-item"), (h("inode"), BTreeSet::from([2])));
    model
}

impl Model {
    fn apply(&mut self, id: Hash, effect: Effect) -> Result<(), &'static str> {
        if let Some(existing) = self.effects.get(&id) {
            return if existing == &effect {
                Ok(())
            } else {
                Err("effect identity mismatch")
            };
        }
        match &effect {
            Effect::Bind { name, provider } => {
                if name.symbol == 0 {
                    return Err("ROOT is not a foreign context symbol");
                }
                let (inode, items) = self
                    .required
                    .get(name)
                    .ok_or("undeclared consumer symbol")?;
                let (provider_inode, provided) = self
                    .provided
                    .get(provider)
                    .ok_or("provider is outside explicit context")?;
                if inode != provider_inode {
                    return Err("inode mismatch");
                }
                if !items.is_subset(provided) {
                    return Err("provider does not declare every referenced item");
                }
            }
            Effect::Reverse { observed } => {
                if !self.effects.contains_key(observed) {
                    return Err("inverse lacks its observed operation context");
                }
            }
        }
        self.effects.insert(id, effect);
        Ok(())
    }

    fn active(&self, id: Hash) -> bool {
        // A reverse can reference only an already declared immutable effect,
        // so this effect-reference graph cannot contain a cycle.
        !self.effects.iter().any(|(candidate, effect)| {
            matches!(effect, Effect::Reverse { observed } if *observed == id)
                && self.active(*candidate)
        })
    }

    fn project(&self) -> Projection {
        let mut projection = Projection::default();
        for (id, effect) in &self.effects {
            if self.active(*id) {
                if let Effect::Bind { name, provider } = effect {
                    projection
                        .active_claims
                        .entry(name.clone())
                        .or_default()
                        .insert(*id, *provider);
                }
            }
        }
        let mut edges: BTreeMap<Hash, BTreeSet<Hash>> = BTreeMap::new();
        for (name, claims) in &projection.active_claims {
            edges
                .entry(name.consumer)
                .or_default()
                .extend(claims.values());
        }
        fn cycle(
            node: Hash,
            edges: &BTreeMap<Hash, BTreeSet<Hash>>,
            active: &mut BTreeSet<Hash>,
            done: &mut BTreeSet<Hash>,
        ) -> bool {
            if done.contains(&node) {
                return false;
            }
            if !active.insert(node) {
                return true;
            }
            if edges.get(&node).is_some_and(|children| {
                children
                    .iter()
                    .any(|next| cycle(*next, edges, active, done))
            }) {
                return true;
            }
            active.remove(&node);
            done.insert(node);
            false
        }
        let mut done = BTreeSet::new();
        projection.cycle = edges
            .keys()
            .copied()
            .any(|node| cycle(node, &edges, &mut BTreeSet::new(), &mut done));
        projection
    }
}

fn bind(consumer: &str, provider: &str) -> Effect {
    Effect::Bind {
        name: name(consumer),
        provider: h(provider),
    }
}

fn reverse(observed: &str) -> Effect {
    Effect::Reverse {
        observed: h(observed),
    }
}

fn apply_all(model: &Model, effects: &[(&str, Effect)]) -> Model {
    let mut model = model.clone();
    for (id, effect) in effects {
        model.apply(h(id), effect.clone()).unwrap();
    }
    model
}

#[test]
fn proposed_independent_context_relations_compose_and_commute() {
    let base = fixture();
    let a = ("R", bind("P", "Q"));
    let b = ("S", bind("X", "Y"));
    let c = ("T", bind("Y", "P"));
    let ab = apply_all(&base, &[a.clone(), b.clone()]);
    assert_eq!(
        ab.project(),
        apply_all(&base, &[b.clone(), a.clone()]).project()
    );
    assert_eq!(
        apply_all(&ab, std::slice::from_ref(&c)),
        apply_all(&base, &[a.clone(), b.clone(), c.clone()])
    );
    assert_eq!(
        apply_all(&apply_all(&base, &[a]), &[b, c]),
        apply_all(&ab, &[("T", bind("Y", "P"))])
    );
    assert_ne!(name("P"), name("X"));
    assert_eq!(
        ab.project().active_claims.len(),
        2,
        "encoded symbol 15 is scoped by its exact consumer patch"
    );
    assert_eq!(
        base.project(),
        Projection::default(),
        "unselected completing effects do not bind names"
    );
    assert_eq!(ab.attachments, base.attachments);
}

#[test]
fn proposed_exact_inverse_restores_relation_identity_and_preserves_other_claims() {
    let base = fixture();
    let effect = ("R", bind("P", "Q"));
    let applied = apply_all(&base, std::slice::from_ref(&effect));
    assert_eq!(apply_all(&applied, &[effect]), applied);
    let undone = apply_all(&applied, &[("undo-R", reverse("R"))]);
    assert_eq!(undone.project(), base.project());
    let restored = apply_all(&undone, &[("undo-undo-R", reverse("undo-R"))]);
    assert_eq!(restored.project(), applied.project());
    assert!(restored.project().active_claims[&name("P")].contains_key(&h("R")));

    let equal = apply_all(
        &applied,
        &[("equal", bind("P", "Q")), ("undo-R", reverse("R"))],
    );
    let claims = &equal.project().active_claims[&name("P")];
    assert_eq!(
        claims.values().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([h("Q")])
    );
    assert!(claims.contains_key(&h("equal")));
    assert!(!claims.contains_key(&h("R")));
    for model in [&applied, &undone, &restored, &equal] {
        assert_eq!(model.attachments, base.attachments);
    }
}

#[test]
fn proposed_competing_and_cyclic_relations_are_order_independent_conflicts() {
    let base = fixture();
    let first = ("R", bind("P", "Q"));
    let competing = ("competing", bind("P", "Y"));
    let conflict = apply_all(&base, &[first.clone(), competing.clone()]);
    assert_eq!(
        conflict.project(),
        apply_all(&base, &[competing, first.clone()]).project()
    );
    assert_eq!(
        conflict.project().active_claims[&name("P")]
            .values()
            .copied()
            .collect::<BTreeSet<_>>()
            .len(),
        2
    );
    let resolved = apply_all(&conflict, &[("remove competing", reverse("competing"))]);
    assert_eq!(resolved.project(), apply_all(&base, &[first]).project());

    // Disjoint names alone do not establish independence: these two relations
    // form a cycle together. Retain both facts, never choose the first arrival.
    let a = ("cycle-a", bind("P", "Q"));
    let b = ("cycle-b", bind("Q", "P"));
    let cyclic = apply_all(&base, &[a.clone(), b.clone()]);
    assert_eq!(cyclic.project(), apply_all(&base, &[b, a]).project());
    assert!(cyclic.project().cycle);
    let repaired = apply_all(&cyclic, &[("undo cycle", reverse("cycle-b"))]);
    assert!(!repaired.project().cycle);
    for model in [&conflict, &resolved, &cyclic, &repaired] {
        assert_eq!(model.attachments, base.attachments);
    }
}

#[test]
fn proposed_undefined_context_operations_refuse_without_mutation() {
    let base = fixture();
    for (label, effect) in [
        ("missing inverse context", reverse("not-applied")),
        ("wrong inode", bind("P", "wrong-inode")),
        ("missing item", bind("P", "missing-item")),
        ("unknown provider", bind("P", "unknown")),
        ("undeclared consumer", bind("unknown", "Q")),
        (
            "ROOT symbol",
            Effect::Bind {
                name: Name {
                    consumer: h("P"),
                    symbol: 0,
                },
                provider: h("Q"),
            },
        ),
    ] {
        let mut model = base.clone();
        assert!(model.apply(h(label), effect).is_err());
        assert_eq!(model, base);
    }
    let mut model = apply_all(&base, &[("R", bind("P", "Q"))]);
    let before = model.clone();
    assert!(model.apply(h("R"), bind("P", "Y")).is_err());
    assert_eq!(
        model, before,
        "one immutable operation ID cannot change payload"
    );
}
