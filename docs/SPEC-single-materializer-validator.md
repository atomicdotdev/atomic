# Architecture Spec: Single Shadow-Commit Pipeline + Validator for Git Shadow Sync

- **Status:** Proposed (governing architecture) — *revised, phased*
- **Severity:** Critical — addresses a bug *class*, not a single defect
- **Component:** `atomic-cli` git-shadow-sync (materialization, `atomic git push`,
  `atomic view switch`, `atomic git import`, turn-end hooks)
- **Version observed:** atomic 0.15.6
- **Subsumes:**
  - `SPEC-shadow-push-conflict-markers.md` → Validator **Rule V1** (already implemented)
  - `BUG-review-gate-original-hashes.md` → made non-load-bearing by V2 (parser already fixed)
- **Revision note:** This version reframes the original "single Materializer" as a
  **single shadow-commit pipeline** (the renderer is already centralized), tightens
  **V3** to an ancestor-based, false-positive-safe rule, records the **current code
  reality**, and lays out a **phased plan** where each phase ships independently.

---

## 1. Thesis

When git shadow sync is enabled, the path that turns a view into a git commit
MUST be governed by:

1. a **single shadow-commit pipeline** — the sole path permitted to stage and
   create a shadow commit (render-if-needed → validate → stage tracked-only →
   candidate → commit), and
2. a **secondary Validator** — an independent gate that must pass before the
   commit is finalized, or the operation aborts and mutates nothing.

The graph→disk **renderer already exists and is shared** (`Repository::materialize*`);
the defect is that `atomic git push` stages disk→git **independently** of it, with
no validation, and turn-end hooks race that path on their own schedule.

## 2. Motivating incident (evidence)

A single git-shadow session degenerated into hours of unrecoverable state. The
distinct, uncoordinated writers observed:

| Writer | What it did | File/behavior |
|---|---|---|
| Cross-view materialization | Wrote unresolved conflict markers to 53+ files | `atomic-core` merge/output emits `>>>>>>> N` / `======= N [HASH]` / `<<<<<<< N` |
| `atomic git push` (shadow hook) | `git add -A` + commit of whatever was on disk, no validation | `push.rs` stages/trees with no coherence check |
| `atomic view switch --force` | Materialized a drifted view over a clean tree | `switch.rs` force path |
| `atomic git import` | Applied a squash whose tree didn't match the view; later refused | `git/import.rs`, `parallel.rs` |
| Turn-end hook | Re-materialized/committed on its own schedule, recreating drift after each `git reset` | hook-driven `atomic git push --no-push` |

Consequences: broken git branches carrying conflict markers, a materialization
loop that re-dirtied `main` after every reset, an import that could never
reconcile, and finally deletion of `.atomic`/`.vault` — **losing all authored
intents, memories, attestations, and the change graph** (git-excluded, no
backup). git survived only because it was the one anchor no shadow writer was
allowed to rewrite remotely.

**Common cause:** many writers, zero validators, no single owner of "the correct
working-copy state right now," and no protection for the unbacked provenance store.

## 3. Current state (what the code actually does)

Grounded map, so this spec builds on reality rather than a strawman:

| Concern | Today | Gap |
|---|---|---|
| Graph→disk renderer | **Centralized** in `Repository::materialize*` (`atomic-repository/src/repository/materialize.rs:274+`); used by `switch_view` (`switch.rs:91`) and `import`. | None — keep it. |
| Shadow-commit staging | **Independent** in `atomic-cli/src/commands/git/push.rs` (`git add -A` → `write_tree` → commit) on whatever is on disk; no re-materialize, historically no validation. | The core hole. |
| Validator | **Rule V1 only** — `Repository::first_working_copy_conflict_marker` guards `push` before staging (shares `record`'s detector). | V2/V3/V4 missing. |
| Proto-V2 | `parallel.rs::squash_insert_matches_tree` does a **post-hoc** touched-path tree↔view check + rollback during import. | Promote to a **pre-commit** invariant. |
| Serialization lock | **Primitive exists**: `deferred_tree.rs:554 lock_deferred_tree_alignment` (advisory `fs2::FileExt::lock_exclusive` on a `.atomic/*.lock`). | Not applied to the commit pipeline. |
| Excluded paths | `ensure_git_shadow_excludes` (`import.rs:172`) git-excludes `.atomic/` etc. | Not *asserted* pre-commit (V4). |
| Provenance protection | **None.** | The catastrophic gap (§2, §10). |

## 4. Principles

1. **Single commit pipeline.** Exactly one path stages and creates a shadow
   commit. `git push` and hooks delegate to it; it renders via the existing
   `Repository::materialize*` when needed. It never independently `git add -A`.
2. **Validate before persist.** No shadow commit is created until the Validator
   affirms coherence. Failure aborts with no mutation.
3. **git is the durable anchor; provenance is precious and unbacked.** The
   working copy and shadow commits are reconstructable; `.vault` and `.atomic`
   are NOT. The architecture must never risk provenance to fix a tree.
4. **No silent success on failure.** A failed validate surfaces (non-zero
   interactively; `.atomic/hook-errors.log` with a distinct tag non-interactively)
   and leaves git + working copy + graph untouched.
5. **Serialized, not raced.** Only one shadow materialize/commit is in flight per
   repo; concurrent hooks/commands queue or no-op, never interleave.
6. **One authority; policy over mirroring.** Direction matters: **git shadows
   Atomic**, not the reverse. Atomic is the upstream **source of truth** for
   content and provenance; git is the **downstream shadow** — generated from
   Atomic (materialize/push) and serving as the durable published anchor. Poking
   the shadow (raw `git checkout`/`reset`/…) must never drive the source of
   truth; the next Atomic materialize regenerates the shadow. The Atomic view is
   the single authority for working-copy content. Rather than intercept every git command
   that moves HEAD or the tree — an unbounded surface (`checkout`, `reset`,
   `merge`, `rebase`, `cherry-pick`, `revert`, `stash pop`, `pull`, `am`,
   `switch`, `restore`…) — shadow-sync relies on (a) **policy/documentation**:
   prefer `atomic` commands over raw git, and (b) the **Validator as backstop**:
   raw git that desyncs the tree simply makes the next shadow push refuse
   (V1/V2/V3) with a reconcile hint. Safety comes from *refusing incoherent
   commits*, not from mirroring git. Full bidirectional HEAD coupling is a
   **non-goal** (see §5.4).

## 5. The shadow-commit pipeline (sole committer)

### 5.1 Responsibilities
- **Render-if-needed:** ensure the working copy reflects the target view via
  `Repository::materialize*` (it does not re-implement rendering).
- **Validate:** run all Validator rules on the *candidate* (staged tree +
  `Atomic-View`/`Atomic-State`/`Atomic-Changes` trailers).
- **Stage tracked-only:** stage precisely the view's tracked paths, honoring
  git-excluded shadow paths (`.atomic/`, `.vault/`, `.atomicignore`).
- **Commit** only after the Validator passes.

### 5.2 Required consolidations
- `atomic git push` MUST call the pipeline instead of its own
  `add_all`/`write_tree` block. That block is deleted.
- Turn-end hooks invoke only the pipeline; they never stage/commit directly.
- `atomic view switch` (incl. `--force`) keeps using `Repository::materialize*`
  for rendering, but MUST detect when git HEAD moved out from under Atomic (an
  external `git checkout`) and MUST NOT materialize a **conflicting** view over
  that tree without reconciliation — it never silently produces a mixed tree.
  Concretely: before materializing, if git HEAD's `Atomic-State` is not in the
  target view's lineage (the V3 predicate), refuse with a reconcile hint rather
  than overwriting. This is the root-cause guard for the git-checkout → switch
  collision (§6.3); V1/V2 remain the push-time backstop.
- `atomic git import`'s materialize step already uses the shared renderer; once
  V2 is a pre-commit invariant, its post-hoc "diverges from git tree" refusal is
  simplified to lean on V2.

### 5.3 Concurrency (lock ordering matters)
- A repo-scoped advisory lock (reuse the `deferred_tree.rs` pattern) guards the
  pipeline. It is acquired **outermost** — before any DB write txn or the
  deferred-tree alignment lock — to avoid deadlock.
- A second materialize request while one runs **queues or no-ops** with a logged
  reason; it never partially stages.

### 5.4 Policy over coupling (why we do **not** mirror git)

**Direction:** git shadows Atomic. Atomic is upstream (source of truth); git is
the downstream shadow, generated from Atomic. So there is no symmetric “both
drive each other” to build in the first place — work flows Atomic → git, and the
sole, *deliberate* git → Atomic bridge is `atomic git import` (onboarding
genuinely-new git-authored history), never automatic.

Full bidirectional binding — make `atomic view switch` move git HEAD *and* make
every `git checkout`/`reset`/`merge`/`rebase`/… move the Atomic view — was
considered and **rejected as a non-goal**. It also has the direction backwards:
letting a raw git command drive the Atomic view would make the shadow rewrite its
source of truth. Reacting to git correctly means
defending the entire git command surface (anything that moves HEAD or the tree),
plus loop-prevention and clobber-avoidance on each. That is a large, fragile
attack surface for a convenience.

Instead, shadow-sync takes the cheaper, safer path:

1. **Policy / documentation (primary).** In a shadow-sync repo, prefer `atomic`
   commands over raw git: `atomic view switch` (not `git checkout`), `atomic git
   import` to bring in external git commits, `atomic git push` to publish. This
   is already the rule for agents (`atomic-opencode/AGENTS.md`: “never use `git`
   for repository operations”); document it for humans in the shadow-sync guide.
2. **Validator backstop (guarantee).** Policy is advisory, so the *guarantee*
   lives in the Validator: if a user does use raw git and desyncs the working
   copy, the next shadow push **refuses** (V1 markers / V2 tree↔view / V3
   lineage) with a reconcile hint (`atomic git import`, or `git reset` to the
   last coherent shadow commit). Raw git can inconvenience the *local* working
   copy; it cannot corrupt shared history, because incoherent state is never
   committed. This is why documentation is *sufficient* rather than reckless.
3. **Optional advisory hook (nicety, not coupling).** At most, an installed git
   `post-checkout`/`post-merge` hook that **only warns** — e.g. “git HEAD is now
   `A` but the Atomic view is `B`; run `atomic view switch A` (or `atomic git
   import`) to resync”. Warn-only means no mutation, no loops, no data-loss risk,
   and a bounded surface. It never switches, imports, or commits on its own.

**Direction A is implemented (the correct, in-our-control half).** `atomic view
switch` moves the git shadow's HEAD to the mirror branch by a **ref move** —
`set_head` + index realign, never a `git checkout`/re-render, so it can't refuse
or revert Atomic's authoritative content. It is best-effort, gated to shadow-sync
repos (excludes present), idempotent (no-op when already on the branch), and
creates the mirror branch if absent. *(Done: `git/shadow.rs::sync_git_head_to_view`,
called from `view/switch.rs`; harness `37_shadow_view_branch_follow.sh`.)*
Note: running the real `git checkout` was rejected — when Atomic is ahead of the
git branch (the normal state before push) it would either refuse or, with `-f`,
revert to the stale git tree, discarding Atomic's un-pushed content (shadow
overwriting source). The ref move avoids both.

The reverse (`git checkout` ⇒ Atomic follows) remains a **non-goal** — that is
the shadow driving the source, and reacting to it means the whole git surface.
The safety mechanism is still the Validator (§6) plus provenance protection
(Rule V4), never the coupling.

## 6. The Validator (secondary gate)

Runs on the candidate; any failure ⇒ abort, log, no mutation.

### 6.1 Rule V1 — No unresolved conflict markers  *(implemented)*
No staged file may contain `>>>>>>> N` / `======= N [HASH]` / `<<<<<<< N`.
Reuses the **same** detector as `atomic record`
(`first_conflict_marker_line`), so record and shadow-push never disagree.
Override: `--allow-conflict-markers` (never passed by hooks). *(Done: `push.rs`
+ harness `33_shadow_push_conflict_markers.sh`.)*

### 6.2 Rule V2 — Tree ↔ view coherence  *(incremental form)*
The staged tree MUST correspond to the target view's recorded change set, over
git-tracked paths only (exclude `.atomic/`, `.vault/`, `.atomicignore`).
**Cost-safe implementation:** do **not** re-materialize the whole view and diff
the tree (O(tree)). Instead, for each path that differs between the staged tree
and git HEAD, compare the staged bytes to the view's materialized content for
that path (`get_file_content_on_view`). A mismatch, an unaccounted-for path, or
a view-recorded change absent from the tree fails V2. This lifts the existing
`squash_insert_matches_tree` check to a general pre-commit invariant.

### 6.3 Rule V3 — git ↔ Atomic state agreement  *(implemented; no bypass; advance = reconcile)*
Purpose: stop a **drifted** view from being shadow-committed over an unrelated
git HEAD (the "drifted `main`" failure) and — critically — stop the *external*
materializer, `git checkout`, from silently poisoning a shadow commit.

**The external-materializer hazard.** `git checkout <branch>` is a working-copy
writer Atomic does not control. After a checkout, the working copy and git HEAD
reflect branch A; a subsequent `atomic view switch B` materializes view B over
that tree, leaving a **mixed** working copy (view-B tracked files + branch-A-only
files, or per-path drift where the two disagree). This is the two-materializer
collision this whole architecture exists to prevent.

**What already catches it at push time (defense in depth, already shipped):**
- Any branch-A file not in view B is staged but absent from the view ⇒ **V2**
  fails ("not recorded by the view").
- Any conflict markers left by materialization ⇒ **V1** fails.
So the *default* push path already refuses the mixed tree. V3 adds the one signal
V1/V2 can't see: the git HEAD **lineage** itself.

**V3 definition (cheap lineage-membership, no tree diff):**
- Let `HEAD_state` = the `Atomic-State` trailer of the most recent shadow commit
  on the target branch whose `Atomic-View` matches the target view
  (`find_last_pushed_state`).
- **Pass** iff `HEAD_state` is absent (first publish) OR is an ancestor of the
  view's current `Atomic-State` (a normal fast-forward advance).
- **Fail** when `HEAD_state` exists but is not in the view's lineage — genuine
  drift, including the git-checkout case (HEAD carries a foreign view's state, or
  none this view can reach).

**No blanket bypass — this is the key correction.** There is deliberately *no*
`--force`/`--advance` flag that commits a state failing V3: that is exactly the
escape hatch that reintroduced the incident (force-materialize over a clean tree
→ loop → delete provenance to escape). To *intentionally* re-anchor a branch to a
different view, the user reconciles **first**, producing a coherent state that V3
then passes on its own merits. Because git shadows Atomic, the remedies are
ordered by direction:
- **Primary — re-shadow:** reset git to the view's last coherent published state
  (`git reset --hard` to that shadow commit). git is the downstream shadow, so
  discarding a poked-shadow state and regenerating from Atomic is the normal fix.
- **Exception — onboard:** only when git HEAD carries genuinely-new *authored*
  content that belongs in Atomic, import it (`atomic git import`) so the view's
  lineage includes `HEAD_state`. This is the sole deliberate git → Atomic bridge.
The `--allow-conflict-markers` override stays narrow (V1 only; V2 and V3 always
apply). Reconcile-to-advance, never bypass-to-commit.

### 6.4 Rule V4 — Excluded-path & provenance integrity  *(highest priority)*
- The candidate MUST contain **no** paths under `.atomic/`, `.vault/`, or the
  `.atomicignore` file. If staging would include any, fail V4.
- The pipeline MUST NOT delete or modify `.vault/` or `.atomic/` as a side effect
  of any tree reconciliation. (Directly serves Principle 3 and the §10 loss.)

### 6.5 Failure semantics
- **Interactive:** non-zero exit; message names the failing rule + first
  offending path/line + remediation.
- **Non-interactive (hook):** append `shadow-validate:<rule>` to
  `.atomic/hook-errors.log`; create no commit; mutate neither git nor graph.
- **Never partial.** The candidate is discarded atomically.

## 7. Interaction with existing specs

- **Conflict markers:** `SPEC-shadow-push-conflict-markers.md` = Rule V1 (done).
- **ReviewGate trailer parser:** already fixed; once V2 owns tree↔view coherence
  pre-commit, the parser stops being load-bearing but remains correct. No rework.
- **Squash-insert (from `SPEC-review-gate-insert-originals.md`):** its post-hoc
  `squash_insert_matches_tree` is the seed of V2; V2 generalizes it.

## 8. Phased plan (each phase independently shippable, harness-first)

- **Phase 0 — V1 markers.** ✅ *Done.* `push` guard + harness 33.
- **Phase 1 — V4 provenance/excluded-path guard.** ✅ *Done.* `push` now (a)
  ensures `.git/info/exclude` carries the shadow patterns before staging
  (prevention) and (b) asserts no `.atomic/`/`.vault/`/`.atomicignore` path is
  staged (V4 guard); on a hit it restores the index from HEAD, logs
  `shadow-validate:V4`, and aborts with no commit. Harness
  `34_shadow_provenance_guard.sh`. The failure log format was unified to
  `shadow-validate:<rule>` (V1 now logs `shadow-validate:V1`).
- **Phase 2 — single shadow-commit pipeline.** ✅ *Done.* New module
  `atomic-cli/src/commands/git/shadow.rs` owns `stage_and_validate_tree`
  (ensure-excludes → stage → V1 → V4 → `write_tree`), returning the candidate
  tree. `push` (and therefore the turn-end hook, which shells `atomic git push`)
  delegates to it; no independent `add_all`/`write_tree` remains in `push`'s run
  path. V2/V3 slot into this one function ahead of `write_tree`.
- **Phase 3 — serialization lock.** ✅ *Done.* `Repository::try_lock_shadow_commit`
  is a non-blocking repo-scoped advisory lock (`.atomic/shadow-commit.lock`,
  `fs2`), acquired **outermost** in `push` via `shadow::acquire_shadow_lock`;
  contention is a logged no-op (`shadow-lock:contended`), never a block or an
  interleave. Unit test `shadow_commit_lock_is_exclusive_and_non_blocking`;
  harness `35_shadow_concurrent_push.sh`.
- **Phase 4 — V2 pre-commit coherence.** ✅ *Done.* `stage_and_validate_tree`
  now runs `first_incoherent_path` before returning the tree: it diffs the
  candidate tree against git HEAD and, for each changed non-provenance path,
  requires the staged bytes to equal `get_file_content_on_view` (the view's
  recorded content). Divergence (un-recorded drift, or a tree that omits a
  change the view records) aborts with `shadow-validate:V2`, restoring the
  index. Always-on (not bypassed by `--allow-conflict-markers`, which is
  content-marker-specific): to commit marker content you `record
  --allow-conflict-markers` first, making disk match the view. Harness
  `36_shadow_coherence_v2.sh`. (Import's `squash_insert_matches_tree` remains
  the analogous check on the import path; the two coexist by design.)
- **Phase 5 — V3 drift rule.** ✅ *Done.* The push pipeline (`push.rs`, at the
  `find_last_pushed_state` lineage check) refuses when the branch's last
  published `Atomic-State` exists but is not in the current view's history —
  genuine drift — logging `shadow-validate:V3`, **with no bypass flag**
  (reconcile-then-push). Replaces the old silent `unwrap_or(0)` re-push. Harness
  `38_shadow_drift_v3.sh`. The `view switch` pre-switch guard was **deferred**
  (§5.2): it false-positives on legitimate inter-view switches, and Direction A
  + the push-time V2/V3 backstops already prevent committing the collision.
- **Direction A (view switch → git HEAD ref move).** ✅ *Done.* `atomic view
  switch` repoints the git shadow's HEAD to the mirror branch (ref move, not a
  checkout), gated to shadow-sync repos, idempotent, creating the branch if
  absent. `git/shadow.rs::sync_git_head_to_view`; harness
  `37_shadow_view_branch_follow.sh`; `19_git_shadow` §9 updated to the new model.
- **Phase 6 — Policy + advisory (NOT coupling the other way).** ✅ *Done.* Per
  §5.4, full HEAD *mirroring* (git → Atomic) is a non-goal. Shipped: (a) a
  **shadow-sync documentation** update ("Prefer Atomic over raw Git" — the
  golden rule, the validator backstop table, and reconcile-then-push) in
  `atomic-docs/.../git-shadow-sync.md`; and (b) a **warn-only** `post-checkout`
  hook (`atomic git hooks install`) that prints a view↔branch mismatch + resync
  hint, mutating nothing. The *guarantee* remains the Validator (V1–V4), not the
  hook.

## 9. Acceptance criteria (mapped to phases)

- [x] (P2) Exactly one code path stages/commits the shadow working copy; no
      independent `add_all`/`write_tree` remains in `push.rs`.
- [x] (P0) A working copy with conflict markers never produces a shadow commit
      (V1); record and push report identical detections.
- [x] (P1) `.vault/`/`.atomic/`/`.atomicignore` are never staged into a shadow
      commit and never modified/deleted as a side effect (V4).
- [x] (P4) A materialize whose tree omits/adds changes vs the view's recorded set
      is rejected before commit (V2), not refused later by import.
- [x] (P5) A drifted view (state not in the branch's published lineage) is
      rejected (V3) with **no bypass** — reconcile-then-push; fast-forward
      advances are unaffected. (Pre-switch guard deferred as false-positive-prone;
      Direction A + push-time V2/V3 cover the collision.)
- [x] (P3) Concurrent materialize requests serialize; no partial staging is ever
      observable.
- [x] (P6) Shadow-sync docs state the “prefer Atomic over raw Git” policy +
      reconcile recipe; the `post-checkout` hook is warn-only (no mutation, no
      loop). Raw-git desync is caught by V1/V2/V3 at push, not by mirroring git.
- [x] (all) Every abort leaves git, working copy, and graph byte-identical to
      their pre-operation state, and is logged.

## 10. Test plan

- **Single-writer (P2):** assert `git push` and the hook invoke the shared
  pipeline; grep-assert no `add_all`/`write_tree` outside it.
- **V1 (P0):** synthetic markers ⇒ no commit; parity with `record`. *(harness 33)*
- **V4 (P1):** attempt to stage `.vault/`/`.atomic/` ⇒ V4 failure; abort leaves
  provenance dirs byte-identical (hash before/after).
- **V2 (P4):** construct a view/tree mismatch (e.g. an out-of-band disk edit) ⇒
  pre-commit rejection naming the path.
- **V3 (P5):** publish view A to a branch, drift the branch's last state so it is
  not in A's lineage ⇒ rejection; confirm a normal fast-forward advance passes.
- **Concurrency (P3):** two simultaneous pipeline requests ⇒ one runs, one
  queues/no-ops; no interleaved partial state.
- **Policy + advisory (P6):** raw `git checkout` to a divergent branch ⇒ the next
  `atomic git push` refuses with V2/V3 and a reconcile hint (the guarantee). Any
  advisory hook is warn-only: it prints the mismatch + resync command and creates
  no commit, switches no view, imports nothing.
- **Idempotency:** repeated hook fires on an unchanged coherent state produce at
  most one commit and never drift.

## 11. Risks / non-goals

- **Non-goal:** changing whether cross-view materialization *emits* markers for
  human resolution. The Validator refuses to *commit* that state, nothing more.
- **Non-goal:** automatic conflict resolution. Conflicts are made visible and
  blocking, never silently committed.
- **Risk (V3 false positives):** mitigated by the lineage-membership definition
  (§6.3), which cannot fire on fast-forwards. There is intentionally **no bypass
  flag** — the tension with "stricter gating can block a user" is resolved by
  reconcile-then-push (reset git, or `atomic git import`), never by committing an
  incoherent state. A bypass here is what let the original incident happen.
- **Risk (external `git checkout`):** git is a working-copy materializer Atomic
  does not control. Mitigated in depth — `view switch` refuses to render a
  conflicting view over a checkout-moved HEAD (§5.2), and V1/V2/V3 refuse to
  commit the mixed result at push time (§6.3).
- **Risk (deadlock):** mitigated by fixed lock ordering (pipeline lock outermost).
- **Migration:** repos already polluted (committed markers, drifted views) are
  out of scope; recovery is reset git to the last clean published state and
  re-derive the shadow — while protecting `.vault`/`.atomic` from deletion.

## 12. Postmortem note (why this matters)

The incident's final, irreversible loss was authored provenance — intents,
memories, attestations in `.vault` — deleted to escape a corruption loop the
tooling created and could not cleanly exit. A single-pipeline + validator design
would have prevented incoherent state from ever being committed, so the escape
hatch of deleting the provenance store would never have been reached. **Phase 1
(V4) is sequenced first for exactly this reason:** protecting the provenance
layer is the highest-order requirement, and it is independent of the rest.
