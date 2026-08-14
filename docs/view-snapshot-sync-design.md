# Views as Objects: Bare Server & View-Snapshot Sync Design

> Status: **draft / proposal.** Captures the design agreed in discussion. Supersedes
> the bespoke `?view-manifest` + `ViewRecord` + reconcile set-convergence approach
> prototyped on `atomic-storage` `feat/geodist-view-sync` and `atomic`
> `feat/set-based-view-convergence` (whose *primitives* it reuses — see
> [Relationship to prior work](#relationship-to-prior-work)).

## Benefits

**Thesis: a patch-theoretic causal graph on the client; git-shaped object+ref
transport on the server — with divergence resolved by patch union, not 3-way
merge.**

The model deliberately splits the two things git couples together. Ancestry is
used only for *transport and head management*; the objects flowing through it are
still commutative patches, so *merging* stays patch-theoretic.

**Client (non-bare) — keeps full patch theory + causal graph, unchanged:**
- Commutative changes, the dependency/causal graph, the CRDT token layer.
- Conflict-free merges when changes are independent; patch-level conflict
  handling when they genuinely overlap — never a textual 3-way merge.
- `record` / `insert` / `diff` / `blame` / `materialize` exactly as today. This is
  where work happens, so it keeps the rich model.

**Server (bare) — gains git's wire + ancestry model:**
- Content-addressed object store + refs; push/pull = transfer the missing object
  closure and CAS a ref. No apply, no materialize on the write path.
- Fast-forward vs. divergence decided by `prev`-chain ancestry — deletes the
  `is_leaf` / prefix / shrink heuristics.
- Trivially convergent across pods (store immutable object + move a pointer), so
  geodist multi-pod durability falls out for free — no bespoke convergence.
- Graph-lazy, not graph-blind: the causal graph is built on demand, scoped to a
  read endpoint, and never for data nobody queries.

**The synthesis bonus neither system has on its own:**
- Git's dumb-fast-convergent *transport* with patch theory's principled *merge*.
  Two divergent view heads reconcile by **set-union of changes** (a new snapshot
  whose membership is the union and whose `prev` lists both heads — why `prev` is
  a `Vec`), not a 3-way snapshot merge. Git's merge pain never reaches the server.

**Honest costs (scoped below, neither on the convergent hot path):**
- The client must emit a canonical snapshot + move a ref on each mutation (D1a).
- The server must build the causal graph on demand for its read endpoints.

## Competition

The current wave of git-acceleration products (Pierre, Origin/Cursor, and
similar) make git faster largely by improving the **storage substrate**: a
high-IOPS-disk-backed, S3-like filesystem (e.g. a modified SeaweedFS on
provisioned AWS IOPS) under an otherwise-unmodified git server. That accelerates
the substrate beneath an inherently server-heavy model. This design removes the
server-heavy work from the **model** itself — same goal, a different (lower)
layer.

**Why git needs the fast-disk trick.** Git assumes a POSIX filesystem and stores
objects in **mutable packfiles**. Serving a fetch walks commit reachability to
compute what to send, delta-compresses, and periodically repacks/gcs — all
**random-IO + CPU heavy**, worsening with history depth and repo size. Fast
disks under an S3-like FS make that random IO tolerable *without changing git* —
i.e. paying (provisioned IOPS is expensive) to make an object store *pretend* to
be a fast POSIX disk, because git can't address an object store natively.

**Why the bare server doesn't.** Its access pattern is **object-store-native**,
not retrofitted:
- **Push** = content-addressed PUT (append, never rewrite) + one tiny **ref
  CAS**. No repack, no gc, no reachability walk, no server-side merge.
- **Clone/pull** = read the ref → read the `.view` snapshot → its membership *is*
  the list of keys to send → stream those immutable blobs. No negotiation walk
  over a commit graph to compute "what to send" — the snapshot is the manifest.
- **No mutable aggregates.** Immutable content-addressed blobs + one CAS pointer
  map cleanly onto an eventually-consistent object store; the only
  strong-consistency point is the tiny ref CAS.

**The distinction that matters.** Git's bottleneck is **random IOPS + CPU**
(packing, walking, repacking); ours is **sequential throughput** of immutable
blobs. Object stores are cheap and fast at the second and poor at the first —
exactly why git needs the fast-disk translation layer and we don't. We are fast
on *commodity* S3-class storage.

**It inverts the cost curve.** They add hardware cost (provisioned high-IOPS) to
reach parity with git's own model. We get the speed by *deleting the work* — and
get patch theory's conflict-free merge on top, which no amount of fast disk buys
them. That is a **cost moat and a capability moat**, not a speed/price tradeoff.
We could run on their fast-disk SeaweedFS — we'd just leave it massively
over-provisioned, because the bare server barely asks anything of the disk.

**Honest caveats.** Huge clones are still bandwidth-bound — but that's sequential
throughput (CDN-/cache-friendly), not the random IOPS git struggles with. Cold
graph-slice builds for content endpoints (materializing a hot file's deep
history) do touch storage — but they are demand-scoped, cacheable, and off the
write path, not a per-request tax.

## Overview

Today a view is a bespoke, server-special entity. Receiving a push means the
server **stores** change bytes, **applies** them into its redb graph, updates
`VIEW_CHANGES`, and validates a `?view-manifest`. That server-side apply is
stateful, order-sensitive, and racy across geodist pods — the source of the
split-brain / convergence complexity.

This design reframes views to ride the **same object machinery as changes, tags,
and provenance**, so the client and server hold byte-identical objects and the
wire protocol only ever does two things: **move objects** and **move refs** —
exactly git.

It rests on four observations:

1. **A view is a mutable entity over immutable state.** `record`, `insert`, and
   `view *` move a view's head. Everything a view *points at* is immutable; only
   the head is mutable — so a view needs exactly one **ref**, like a git branch.
2. **A view-snapshot is commit-shaped.** `{ scope, parent_view, prev_state,
   membership }` — a parent (lineage) pointer plus a snapshot of contents — is a
   commit. A view's history is its own little DAG.
3. **The snapshot's membership *is* the change-set structure of the view.** A
   snapshot stores the view's **own** contribution plus a **pointer to its
   parent view's snapshot** — the object-form of what redb already holds
   (`VIEW_CHANGES[this]` + `parent`). The effective union (own ∪ ancestors ∪
   dep-closure) is **composed on read** by a shallow walk up the parent chain,
   so membership/structure queries touch a handful of small objects, never the
   graph. Because only its own set is inlined, a **draft off a 100k-change
   `dev` stays tiny** — the big set lives only in the shared root it points at.
4. **The server can be bare.** If storing an immutable object and moving a ref
   are the only things a push does, both halves are trivially convergent. The
   redb graph becomes a *derived read-model*, built **on demand, scoped to the
   query** — never on the push path, never for data nobody reads.

## Core principle

**The object store + refs are the source of truth. The redb graph is a
disposable, demand-built read-model. Sync moves objects and CASes refs; it never
applies.**

```
  SOURCE OF TRUTH (write model)              DERIVED (read model)
  ═══════════════════════════════            ══════════════════════
                                             built on demand, scoped
  objects/  (content-addressed)              to the query, cached
    AB/CDEF…  .change   ── bytes             opportunistically:
    12/34..   .tag      ── bytes
    9F/A0..   .view     ── keys only  ─────▶   redb graph slice
    E1/..     .provenance                      (only for CONTENT reads:
                                                file bytes, diff, blame)
  refs/
    views/dev            → 9FA0…  (CAS)      membership/structure reads
    views/orange-night   → 4B2C…  (CAS)      need NO graph — just .view
```

## The three tiers

```
  refs            views/{name} → snapshot-key            (mutable, one per view)
    │
    ▼
  view-snapshots  { scope, parent, prev, membership }    (immutable, commit-shaped)
    │  membership = KEYS only
    ▼
  content         .change / .tag bytes                   (immutable, deduped leaves)
```

- A **ref** is the only mutable object. `views/dev → <snapshot-key>`.
- A **view-snapshot** is immutable and content-addressed by the Blake3 of its
  canonical bytes. It carries identity, lineage (`prev`), and membership.
- **Membership holds keys, not content.** A change in `dev`, `release`, and three
  drafts is stored **once** in `objects/`, referenced five times. (Git: a commit's
  tree references blobs by hash; it never inlines file bytes.)

## The view-snapshot object

```rust
/// Immutable, content-addressed snapshot of a view's state at one point in time.
/// Content address = Blake3 of the canonical serialization.
struct ViewSnapshot {
    /// Identity.
    scope: ViewScope,               // Shared | Draft
    parent_view: Option<String>,    // parent view NAME (the parent's ref, resolved
                                    // to its own snapshot chain at read time)

    /// Lineage — the predecessor state(s) this transition was built on. Makes the
    /// view's history a content-addressed DAG, exactly like change history. This
    /// is what makes fast-forward vs. divergence decidable (see below).
    prev: Vec<SnapshotKey>,         // usually 1; 0 for the genesis; >1 only if we
                                    // ever model a view merge.

    /// Membership = this view's OWN change/tag keys only (NOT the flattened
    /// union). The inherited part is reached via `parent_view` (resolved live to
    /// the parent's snapshot). Own history is encoded delta-vs-`prev` with
    /// periodic fulls (decision D3), so a shared root's growth stays cheap and a
    /// draft stores only its handful of own keys.
    own: Membership,

    /// Order-invariant identity of this view's OWN set. The EFFECTIVE set_id is
    /// composed on read: `own_set_id.combine(parent.effective_set_id)` — an
    /// O(1)-per-level hash compose, so set-equality / convergence needs no graph.
    own_set_id: SetId,

    /// Optional slot for provenance/attestation of the transition (who/why).
    /// Left empty in v1; wired later (decision D4).
    provenance: Option<ProvenanceKey>,
}

enum Membership {
    /// Full OWN key list — a checkpoint snapshot.
    Full { changes: Vec<ChangeKey>, tags: Vec<TagKey> },
    /// Delta vs `prev` — the common case (append one key on `record`/`insert`,
    /// remove keys on `view split`). Reconstruct this view's own set by walking
    /// back to the last Full.
    Delta { added: Vec<Key>, removed: Vec<Key> },
}
```

`own` is just this view's contribution; the **effective union is composed on
read** — own ∪ (parent snapshot's effective union), recursively up the parent
chain (depth 2–4 in practice), exactly the filter redb computes today. Two
independent sharing axes keep this small:

- **Hierarchical (parent pointer):** a draft never inlines its parent's set, so
  the large membership lives only in shared roots — the views that actually
  accumulate. This is the dominant win (see decision D3).
- **Temporal (delta + periodic full):** a shared root's own history is a chain of
  small deltas with occasional full checkpoints, so even `dev`'s growth is cheap
  to write and its current own-set is a short walk to reconstruct.

The `prev` chain gives a view's *own* set at any past state; joining it with the
parent's `prev` chain reconstructs a historical *effective* set when needed.

## Bare server

A bare server is git's `--bare`: object DB + refs, **no working tree and no
graph-apply on the write path.** On push it does only:

1. **Store** the objects it lacks (`.change`, `.tag`, `.view`, `.provenance`) —
   content-addressed, append-only. Convergent by construction.
2. **CAS the ref** `views/{name}` from the client's known-old to the new snapshot
   key, gated on the `prev` chain (fast-forward unless `--force`).

No `insert`, no CRDT apply, no `VIEW_CHANGES` mutation, no materialize.

Terminology (Atomic's, precisely):

| layer | what | on the bare push path? |
|---|---|---|
| **object** | `.change`/`.tag`/`.view`/`.provenance` files | **yes** — store |
| **ref** | `views/{name}` → snapshot key | **yes** — CAS |
| **apply** | write objects into the redb graph (GRAPH, `VIEW_CHANGES`, deps) | **no** — demand-built |
| **materialize** | graph → files on disk (working tree) | **no** — client-only |

The redb graph is a **read-model**, not truth. It is:

- **demand-built and query-scoped** — see next section,
- **rebuildable** — losing it loses nothing; rebuild from objects,
- **optionally checkpointed** — snapshots to S3 bound cold rebuild to
  "latest checkpoint + tail," never genesis (the existing geodist
  `write_snapshot`/`maybe_restore_snapshot` mechanism, demoted to an
  optimization).

## Demand-scoped reads: two query planes

The server builds **only the graph fragment a specific endpoint needs, when it's
called.** No derived endpoints exercised ⇒ no graph ever built. Crucially, most
queries split into a plane that needs no graph at all:

**Structure / membership — served purely from `.view` objects, no graph:**

- "what changes are in `dev`?" → read `views/dev` → snapshot → membership.
- "is change `C` in `dev`?" → membership lookup.
- "set-diff `dev` vs `release`" → two snapshots, compare (or compare `set_id`).
- "what was `dev` 6 months ago?" → walk `prev` to that snapshot.
- "are these two views the same set?" → O(1) `set_id` compare.

**Content — needs a graph slice, scoped to the request:**

- "file `X`'s bytes at `dev@head`" → snapshot's membership ∩ changes touching
  `X`'s inode, folded. O(history of `X`), not O(repo). Change headers name the
  paths they touch, so the relevant subset is locatable from cheap metadata.
- "diff of change `C`" → read that one `.change` object + touch its file's
  vertices via `INODE_GRAPH`. Sublinear.

Any index that accelerates "which changes touch path `X`" stays **derived**
(rebuildable), never authoritative — that is the invariant that keeps the server
honest-dumb. Caching (per-file materialization, per-view checkpoints) is an
**optimization, never a correctness requirement.**

## Wire protocol (git-shaped)

Push/pull negotiate refs and transfer the missing object closure. No server
apply.

```
  push dev:
    1. advertise    client GETs remote ref:  views/dev → R (or ∅)
    2. negotiate    client walks local prev-chain from its tip L back until it
                    reaches R (have/want); the frontier is the set of snapshot
                    keys the remote lacks — same dependency-closure walk `?insert`
                    already does, now over the snapshot DAG + the changes those
                    snapshots reference.
    3. transfer     PUT (?store) every missing object: .view snapshots, and the
                    .change/.tag bytes their membership references that the remote
                    lacks. Content-addressed ⇒ idempotent.
    4. move ref     CAS views/dev : R → L. Fast-forward iff R is an ancestor of L
                    in the prev-chain. Non-ancestor ⇒ reject unless --force.
```

- **clone/pull** are the same in reverse: pure object + ref transfer. The client
  is the **non-bare** side and builds its own graph locally, because that is where
  someone works.
- **Fast-forward / divergence is decided by ancestry**, not by guessing from log
  shapes. This deletes the `is_leaf` / prefix-rule / shrink-ambiguity heuristic:
  - remote tip is an ancestor of local → fast-forward.
  - local tip is an ancestor of remote → you're behind, pull.
  - neither → genuine divergence → `--force`.
  A `view split` (shrink) is just a normal new snapshot whose `prev` is the old
  tip — a fast-forward, not a special case.

## Membership representation (decision D3 — resolved)

Keys-only fixes **content dedup** (a change stored once across all views). It does
**not** by itself fix **membership-list size**: a flat union list is O(view size),
and rewriting it on every transition is quadratic (~100k × 32-byte keys ≈ 3–5 MB
*per* snapshot × every transition). Two axes of sharing fix it, and the important
one is hierarchical:

**Axis 1 — hierarchical (the big win).** A snapshot stores only the view's **own**
set + a **parent pointer**; the effective union is composed on read. So the large
membership exists **only in shared root views** (`dev`/`release`/`main`). A draft
off a 100k `dev` stores ~its own handful of keys, not 100k. This mirrors Atomic's
existing `VIEW_CHANGES[this] + parent` model exactly — no new semantics.

**Axis 2 — temporal (for the roots that *are* big).** A shared root's own history
is encoded **delta-vs-`prev` + a `Full` checkpoint every N transitions**.
Append-friendly (view logs are overwhelmingly append), tiny writes, reconstruct
the current own-set by walking back to the last `Full`. Reuses the checkpoint
idea already in geodist.

**Resolution:** own-set + parent-pointer (Axis 1) × delta+periodic-full (Axis 2).
Drafts are cheap by construction; only shared roots carry a big set, and even
theirs is a delta chain. If a single shared root's *own* set ever grows large
enough that even the periodic `Full` is a problem, the `Full` variant of
`Membership` can later be replaced by a **structurally-shared Merkle set object**
(git-tree-shaped, O(shared) storage, O(log) reads) without touching refs, the wire
protocol, or callers — the `own`/`own_set_id` shape is deliberately encoding-
agnostic to allow that drop-in.

## Relationship to prior work

The **primitives survive**; the **orchestration/representation is replaced.**

Reused as-is (become the local materializer / read-model builder):
- `Repository::create_draft_view`, `create_shared_view`, `delete_view`
- `Repository::retain_view_changes` (declarative "make the view's log equal the
  snapshot's membership" = remove-not-in ∪ insert-missing)
- `Repository::view_set_id`, `atomic_core::types::SetId`
- geodist `write_snapshot`/`maybe_restore_snapshot` checkpoints (demoted to an
  optional read-model accelerator)

Replaced / retired:
- `?view-manifest` declare endpoint and synchronous `apply_manifest`
- `ViewRecord` (`views/{name}.json`) as a name-keyed LWW blob → becomes a
  content-addressed `.view` object + a CAS ref
- reconcile-time imperative set-convergence (`converge_view_sets`,
  `ensure_views_from_records` as sync-path steps) → declarative materialize, off
  the write path
- client `plan_view_sync`'s `is_leaf` / prefix / shrink heuristics → ancestry-based
  fast-forward/divergence

## Open decisions

- **D1 — client source of truth. RESOLVED: (a) now, shaped for (b).** redb stays
  the client's truth; the `.view` snapshot is serialized from redb on demand (at
  push). The object format is kept complete + canonical so it can later be
  promoted to authoritative (b: snapshot chain is truth, redb a rebuildable
  cache — the fully git-shaped endgame) without changing the object or the wire
  protocol. The client keeps a small remote-tracking ref (last pushed snapshot
  key per remote) so CAS/ancestry works even under (a).
- **D2 — ref update semantics. RESOLVED: CAS-on-`prev`.** A ref move is a
  compare-and-swap from the client's known-old snapshot key to the new one,
  accepted only if old is an ancestor of new in the `prev` chain (fast-forward),
  else rejected unless `--force`. This is the payoff of the whole model and
  deletes the `is_leaf`/prefix/shrink heuristics.
- **D3 — membership representation. RESOLVED:** own-set + parent-pointer
  (hierarchical) × delta+periodic-full (temporal); drafts store only their own
  keys, big sets live only in shared roots, `Full` can later become a
  structurally-shared Merkle set object. See
  [Membership representation](#membership-representation-decision-d3--resolved).
- **D6 — parent reference. RESOLVED: live / by-name.** The snapshot names its
  parent view; inherited membership resolves against the parent's *current* head
  at read time — preserving Atomic's inheritance semantics (`record` onto `dev`
  ⇒ drafts off `dev` see it). The object stays immutable on what it owns; a
  historical effective set is reconstructable by joining both `prev` chains.
- **D4 — provenance on transitions. DEFAULT (revisit later):** leave a
  `provenance` slot in the object, don't wire it in v1. `record`/`insert`/`view
  split` emitting attestable view-state events is a later layer.
- **D5 — v1 derived endpoints. DEFAULT (revisit later):** smallest set that
  avoids reintroducing a full applier — raw `.change`/`.view`/`.tag` object fetch
  (no graph), single-file content at a ref, single-change diff. Web tree, blame,
  etc. layer on afterward.

## Phased plan

1. **Object + ref model (single node).** Define `ViewSnapshot`, its canonical
   serialization + Blake3 key, and the `views/{name}` ref. Client writes a snapshot
   + moves the ref on `record`/`insert`/`view *` (D1(a): derive from redb).
2. **Bare push/pull for one view.** Advertise ref → prev-chain negotiate →
   transfer missing objects → CAS ref. No server apply. Replaces `?view-manifest`.
3. **Local materializer.** "check out ref R" = ensure membership's change-closure
   present, then rebuild the redb view to equal the snapshot (via
   `retain_view_changes` + `insert`). Off the sync path.
4. **Demand-scoped read endpoints (D5).** Object fetch; single-file content;
   single-change diff — each building only its slice.
5. **Geodist = free.** Bare store over S3 + refs; NATS announces objects/ref moves;
   checkpoints accelerate the read-model. No bespoke view convergence.
```