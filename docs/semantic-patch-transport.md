# Semantic patch transport and commutation

## The defect is a violated patch law

A patch's meaning must not depend on the order in which independent patches
were inserted or the numeric IDs a particular repository allocated to them.
Atomic's graph operations name external objects by hash and resolve those names
locally. Ordinary non-span `FileOps` currently retain non-ROOT numeric owner IDs
without an equivalent portable binding. They can consequently attach a line to
an unrelated file after transfer or independent insertion reordering.

This is not a missing branch lineage or an instruction to reconstruct ancestry.
Adding dependencies on independent changes to reproduce a producer's allocation
order would mask the defect, constrain valid commutations, and still fail when
unrelated local registrations differ. The rejected historical metadata-overlay
experiment is not the production solution.

## Laws and state equivalence

Write `P ; Q` for applying P, then Q, in valid context. Logical state equivalence
includes file content, file/line/token identities and relationships, conflicts,
and stable agent attachments. It does not require equal repository-local numbers,
equal order-sensitive view Merkle chains, or byte-identical operation journals.

1. **Contextual composition.** `P ; Q` is defined only when Q's required context
   is available. Associativity must hold for defined compositions.
2. **Independent commutation.** For independent operations with compatible
   context, `P ; Q` and `Q ; P` yield equivalent logical states. More general
   contextual commutation may require transported operands; absence of a direct
   dependency alone is not proof of independence.
3. **Transport equivariance.** If two repositories use different injective maps
   from stable object names to local IDs, transporting a patch and state between
   those representations must commute with application. The names change; the
   operation and agent attachment do not.
4. **Partial inverse.** An inverse is defined against the appropriate observed
   context. A valid `P ; inverse(P)` restores equivalent projected state, not an
   erased audit trail. Removing a view reference is not by itself proof of an
   inverse-patch construction.
5. **Undefined operations refuse.** Missing, ambiguous or conflicting context
   must not be invented. A failed name transport must not leave a partly renamed
   patch for a caller to apply accidentally.

For identity transport `T`, the representation laws are:

```text
T_identity(P) = P
T_g(T_f(P)) = T_(g composed with f)(P)
T_inverse(f)(T_f(P)) = P             for an invertible name map f
T_f(T_g(P)) = T_g(T_f(P))            for disjoint, non-capturing renamings
```

These laws for names do not claim that arbitrary content edits commute. ROOT
placeholders retain their self-relative meaning and are not external owners.

## Native counterexample

The standalone native experiment in `atomic-semantic-inspect/src/bin/algebra.rs`
in the approved diagnostic directory uses four immutable non-span test patches:

- A creates `a.txt`.
- B creates `b.txt`.
- C creates `c.txt`.
- E appends a line to `a.txt` and requires only A.

The creations are independent. E can commute past B and C but cannot be moved
before A. The experiment enumerates all twelve dependency-valid permutations,
using normal repository insertion, not a custom semantic interpreter. The same
four patch objects, including E's explicitly synthetic session attachment, are
used in every repository. Synthetic attachment data is exercised in draft views;
publication policy is not disabled and no fixture is represented as trusted
original-session evidence.

All twelve orders have equal graph content, SetId and attachment bytes, and
idempotent insertion. Required-context removal refuses. Removing E restores the
three-file base projection, and reinsertion restores membership without changing
stored objects or attachments. Nevertheless, ten orders attach E's line to the
wrong file. Only `B C A E` and `C B A E` happen to allocate A the numeric ID that E
recorded in its producer. Thus byte-only merge properties can pass while semantic
commutation fails. This does not date the introducing commit or establish a
general inverse operator.

Detailed run: `atomic-semantic-algebra-laws-v2.log`; per-case results are in
`atomic-semantic-algebra-laws-v2/results.json` in the approved temporary directory.

## Existing operation machinery

`atomic-core/src/apply/edge.rs` applies graph transitions additively: the old edge
remains and a change introduces the new edge, with visibility controlled by the
normal view filter. `NewEdge::reverse` exchanges the exact prior/new flags and
accepts the appropriate introducing change. Name/order conflict operations also
have explicit solve/unsolve forms. These are useful ordinary patch primitives;
none of them supplies a missing semantic object name automatically.

`FileOps::remap_referenced_node_ids` is the existing name-transport primitive.
Its implementation now resolves each distinct external owner once before
mutating any operation. Resolver errors leave the original FileOps unchanged.
Tests cover identity, composed and inverse renaming, disjoint-renaming
commutation, ROOT preservation, consistent owner resolution and failure
atomicity across the existing trunk/branch/leaf variants. This is a limited
primitive fix, not a fix for missing portable bindings in ordinary change files.

## Remaining native fix

The production boundary to correct is the writer/codec/localizer boundary, not
the ordering of unrelated patches:

- Persist semantic names using stable change identity and an intra-change index,
  with self-relative names represented explicitly. The graph already uses the
  analogous external-hash/local-ID distinction.
- Localize those names consistently for every native consumer, not only span
  changes. A codec name dictionary is representation data, not a mutable repair
  database, an ancestry relation or a permission to change an old patch's meaning.
- Preserve the exact bytes, addresses and attachments of old published changes.
  For old payloads without portable bindings, use only uniquely established
  operation context or explicit patch operations to resolve a real relationship;
  reject insufficient context. Never use matching numbers or chronological order
  as an owner resolver.
- Version any wire/capability change explicitly so unsupported readers cannot
  silently interpret external names as local numbers. Do not overload session
  metadata or label a non-span payload as a span to select a different reader.
- Upgrade the native regression to require semantic ownership and subsequent
  recording to succeed in all legal orders, while dependent/conflicting cases
  still refuse appropriately and all original attachment bytes remain intact.

No native codec or old-history recovery implementation is claimed by this note.
The published atomic-skills recording failure remains open.

There is now a bounded, opt-in [native prototype](native-patch-algebra-prototype.md)
using existing inode and graph-endpoint context. Unlike the original standalone
models, it runs the real insertion and record paths and exercises an actual
inverse edit. It changes neither the stored format nor default behavior and is
not a general solution for the missing-context legacy cases.

The subsequent inode-context trial found a separate missing declared semantic
context in the real release. The user selected design before new native
operations or compatibility changes. See [the proposed context-completion
operation](semantic-context-completion.md) and its explicitly model-only tests;
it is not an implemented repair or an ancestry-based interpretation layer.

## Validation of the limited primitive change

- `cargo test -p atomic-core --lib --locked`: 3,756 tests pass, including four
  transport-law tests. Before the implementation change, the exactly-once owner
  resolution and failure-atomicity tests fail.
- `cargo test -p atomic-repository --test span_insert --test native_span_record
  --locked`: 40 integration tests pass (10 insertion, 30 recording).
- `cargo clippy -p atomic-core --lib --locked -- -D warnings`: passes.
- Strict Clippy including tests remains blocked by existing `type_complexity`
  and `useless_vec` findings in `pristine/span_patch_storage.rs` and
  `pristine/git_closure.rs`; those unrelated files were not changed.

The twelve-order diagnostic deliberately records the unresolved semantic law
failures. The green primitive tests must not be mistaken for an end-to-end fix.
