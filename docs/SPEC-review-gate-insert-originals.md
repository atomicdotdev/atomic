# Spec: Squash imports insert originals as records and represent the squash as a tag

**Status:** Draft for review
**Component:** `atomic-cli` git import (classifier, write pipeline, dry-run) +
`atomic-repository` insert/tags
**Related:** `BUG-review-gate-original-hashes.md` (prerequisite, already fixed)
**Version target:** atomic 0.15.x

---

## 1. Problem

In the git-shadow-sync workflow, developer work is recorded as granular
per-turn Atomic **change records** in a **draft** view, materialized to a git
shadow branch, and squash-merged onto `main` on GitHub. When we re-import that
squash with:

```
atomic git import --incremental --branch main
```

the importer today **writes a brand-new squashed change record** into `main`
and attaches a `ReviewGate` tag whose metadata lists the original change hashes.

Two problems follow:

1. **Provenance depends on keeping draft views.** The original per-turn records
   are only referenced by the (disposable) draft view. The ReviewGate metadata
   names them by hash, but nothing in a *synced* view references them, so:
   - A default clone of `main` downloads only main's manifest changes (the
     squashed record), not the originals — the ReviewGate hashes dangle
     (`clone/command.rs:679`, `change_union`).
   - Deleting the draft views severs the only live reference to the granular
     history.

2. **`main` holds a re-derived squash, not the real records.** The squashed
   record is a fresh re-derivation of content that already exists in the graph
   as the originals. Blame/diff/provenance on `main` sees one opaque squash
   instead of the actual per-turn records.

### What we want

- Deleting draft views must be safe — no provenance loss.
- `main` should reference the **original per-turn change records** so they are
  cloned with `main` and remain queryable in the graph.
- The squash is represented as an **atomic tag** (the ReviewGate) whose payload
  is the **union/aggregate** of those records — `[first … last]`. **A squash
  never becomes an atomic change record** (except the one non-atomic case in
  §5).
- No corruption of `atomic git import --incremental --branch main`.
- `main` materializes to exactly the git squash's tree.

---

## 2. Two entity kinds — the squash is a tag, not a change

Atomic has two distinct first-class entity kinds, both registered in the
identity layer (INTERNAL / EXTERNAL / NODE_TYPES):

- **Change record** (`NODE_TYPES` = change). Carries graph ops; participates in
  `VIEW_CHANGES` and therefore in materialization.
- **Tag** (`NODE_TYPES` = TAG; `TagRecord` in `TAG_RECORDS`). Carries a `state`
  Merkle, `sequence`, `change_hash`, `kind`, and extensible `metadata`. Travels
  over sync (`save_synced_tag`; pull replicates tags).

The **originals are change records.** The **squash is a tag.** The ReviewGate
tag's payload is the union of the inserted records.

```
originals (in draft) ──insert──▶ change records live in shared view
                                 (materializable, cloneable, blameable)

squash commit        ──────────▶ atomic TAG (ReviewGate) = union[first … last]
                                 (NOT a change record)
```

### Why insert is cheap and non-destructive (ambient graph model)

All edges from all views live in one canonical `GRAPH`. A view is a change-set
**filter**. The originals' edges are already in `GRAPH` (recorded in the draft),
so `insert_change_rec(hash, view=main)` (`insert.rs:1969`) only adds change refs
(+ their transitive dependency closure, in dependency order) to main's
`VIEW_CHANGES`. O(1) metadata per change, no edge copying, no content rewrite.

### Materialization

The **tag does not materialize anything.** `main` materializes from the change
records in its filter, so inserting the originals is what puts the files on
disk (`repo.materialize()`, `import.rs:611`). Because the originals are exactly
the changes that produced the shadow-branch tip, materialized `main` equals the
squash's git tree (in the shadow-sync model, where Atomic drives git). The tag
sits on top purely as the aggregate marker / git↔atomic link.

> Rejected alternative — *consolidating tag*: a Pijul-style tag that snapshots
> state so `main` need not carry the individual records. Not chosen: tags here
> carry a `state` but materialization still walks `VIEW_CHANGES`, and keeping
> the individual records is the whole point (blame/provenance). Revisit only if
> record-count in `main` becomes a scaling problem.

---

## 3. Current pipeline (what happens today)

```
phase 1  parse commits (parallel)
phase 2  write_commit(...)  — parallel.rs:2733
         └─ EVERY non-skipped commit → a NEW change record in target view
            (self-push commits skipped: should_skip_self_push, :2647)
phase 3  classify_and_tag_imports(...) — parallel.rs:3449
         ├─ Normal  → no tag
         ├─ Merge   → ReviewGate "merge-<sha>"
         └─ Squash  → ReviewGate "pr-<n>"/"squash-<sha>" with
                       metadata.changes.original_hashes
phase 3  phase3_finalize(...) — parallel.rs:3530
         └─ asserts commits_parsed == changes_written + empty + merge
                                        + self_push_skipped
final    repo.materialize() — import.rs:611
```

`classify_commit` (`:3604`) already separates squash-with-`Atomic-Changes`
(originals populated — now the *full* set after the prerequisite bug fix) from a
plain forge squash (originals empty). The corruption to avoid: inserting the
originals **and** writing the squash record → `main` holds both.

---

## 4. Proposed design

### 4.1 Squash → insert records + create aggregate tag

In phase 2, classify the commit before writing. When it is a **Squash whose
every `original_hash` exists locally** (`repo.has_change`):

1. **Do not** `write_commit` it (write no change record for the squash).
2. For each `original_hash`, in trailer order, `insert_change_rec(hash,
   InsertOptions::default().view(target_view))`. Deps pulled automatically in
   dependency order; re-inserting an already-present record is a no-op.
3. Record the git SHA as imported (git SHA index + incremental markers) so a
   re-run of `--incremental` skips it (`incremental_import_skips`, `:200`).
4. Flag the `ImportedCommitInfo` as `SquashInserted` so phase 3 creates the
   aggregate tag and counts it in a new `squash_inserted` bucket for
   `phase3_finalize`.

### 4.2 The ReviewGate tag as aggregate

The ordered `original_hashes` already encode `[first … last]`. Add explicit
convenience fields:

```json
{
  "git": { "sha": "...", "merge_strategy": "squash", "pr_number": 2 },
  "changes": {
    "original_hashes": ["AAA", "BBB", "CCC", "DDD", "EEE"],
    "from": "AAA",
    "to":   "EEE",
    "count": 5,
    "inserted": true
  }
}
```

`inserted: true` ⇒ the records are live in this view. (`inserted` is only ever
`false` for the non-atomic change-record case in §5; a squash tag is otherwise
always backed by inserted records.)

### 4.3 `atomic tag show` provenance UX (independent, low-risk)

Render a ReviewGate readably instead of raw JSON:

```
Tag: pr-2
View: main
Kind: review-gate
Git: squash 97ccfe6d (PR #2)
Aggregate: AAA … EEE  (5 records, inserted)
Records:
  ✓ AAA  present  (main)
  ✓ BBB  present  (main)
  ...
```

Needs a small repo helper `views_containing_change(hash) -> Vec<String>`
(read txn → `get_internal` → per view `get_change_seq`). Ships regardless of the
insert work and directly serves "let me query the graph for provenance."

### 4.4 Dry-run as router

`--dry-run` today only counts (`import.rs:492`: "Would import N commits…"). It
runs the real incremental skip rules but does no classification. Extend it to
classify each incoming commit and route the user:

```
Would import 1 commit from branch 'main':
  1 squash (PR #2) — atomic headers present, 5 records available
    → will insert 5 records into 'main' and tag pr-2 (aggregate AAA…EEE)
```

Three-way routing table:

| Dry-run detects | Recommendation | Result in `main` |
|---|---|---|
| Squash **with** atomic headers, records present | proceed with `--incremental` | insert records + aggregate **tag** |
| **No** atomic headers (git-authored) | `git pull && atomic git import` (normal path) | ordinary **change record(s)** — legitimate new non-atomic content |
| Atomic headers, records **missing locally** | `atomic pull` first, then re-import | insert once records are present |

The middle row is the **only** sanctioned path where a squash yields a change
record, and only because the content genuinely originated on the git side with
no atomic provenance to preserve.

---

## 5. Fallback = skip + advise (never fabricate a squash record)

Because a squash is a tag, the old "write the squash as a change record"
fallback is gone. The two non-happy paths:

1. **Atomic headers present, but ≥1 named record missing locally.** We cannot
   union records that don't exist, and we must not invent one. → **Skip the
   commit** (leave its git SHA unimported so a later run completes it) and warn,
   naming the missing hashes, recommending **`atomic pull`**. A `git pull` does
   *not* help here — git carries no atomic change records.

2. **No atomic headers at all (git-authored squash/commit).** Genuinely new
   non-atomic content with no records to union. → Handled by the **normal
   import path** (writes ordinary change records), surfaced by the dry-run
   router as `git pull && atomic git import`. This is the lone legitimate
   change-record outcome.

Partial presence (some records present, some not) is treated as case 1 — do not
insert a partial set.

---

## 6. Decisions

### D1. Divergence handling (the one open decision)

If a human hand-resolved a conflict in the GitHub PR UI, the squash's git tree
won't equal the sum of the originals, so inserting them would leave `main`'s
materialized files different from git's `main`.

- **(A) Verify + skip (safer).** After inserting, compare the materialized tree
  for the touched paths against the git commit's tree. On mismatch, roll back
  the insert (`del_view_changes` on the added refs) and **skip + advise** (as
  §5) rather than corrupt `main`. `main` can never silently desync from git.
- **(B) Trust the invariant (simpler).** Always insert when records are
  present; no tree comparison. Correct as long as nobody hand-resolves in the
  GitHub UI.

*Recommendation:* A. Note the fallback under A is skip+advise, **not** writing a
squash change record.

### D2. Dependency-closure scope

`insert_change_rec` pulls the transitive closure. A PR branch based on `dev`
(not `main`) drags dev-only deps into `main`. Consistent with git (a squash of a
dev-based branch onto main includes dev's diff), but confirm this is desired vs.
erroring when the closure exceeds the declared trailer set.

### D3. What the squash git SHA maps to

Options: the resulting view state (Merkle), the last inserted record, or the
ReviewGate tag entity. Affects `atomic change <git-sha>` lookups and re-import
dedup. *Recommendation:* map the SHA to the resulting state and let the
ReviewGate tag own the git provenance.

### D4. Retroactive repair

Pre-existing ReviewGate tags reference truncated/absent originals and point at a
squash *record*. Rebuilding them (insert records, convert to aggregate tag,
remove the squash record) is a separate `atomic git reclassify`-style tool.
*Recommendation:* defer; confirm.

### D5. Merge (non-squash) commits

Out of scope; keep current `merge-<sha>` ReviewGate behavior. Confirm.

---

## 7. Edge cases

- **Idempotent re-import:** git SHA recorded so `--incremental` skips it;
  re-inserting the same records is a no-op (insert dedupes against
  `VIEW_CHANGES`).
- **Ordering:** insert in dependency order (handled by `insert_change_rec`);
  preserve trailer order for the aggregate `from`/`to`.
- **Draft view deletion after insert:** safe — records stay alive because
  `main` references them; only orphaned edges are GC'd.
- **Empty squash** (no file changes): keep current no-op handling.
- **Tag naming collision** (`pr-<n>` already exists from a prior import):
  overwrite vs. error — decide (recommend overwrite with a warning, since a
  re-squash of the same PR is legitimate).

---

## 8. Testing

Unit (prerequisite — already landed):
- `parse_atomic_changes_trailer` collects all blocks, dedupes, preserves order.
- `classify_commit` surfaces the full `original_hashes`.

New integration tests:
- Record N per-turn records in a draft; materialize to a git shadow branch;
  squash-merge; `import --incremental --branch main`; assert:
  - `main`'s `VIEW_CHANGES` contains all N originals, and **no** squash record.
  - `repo.materialize()` of `main` == git squash tree.
  - ReviewGate `pr-<n>` has `inserted: true`, `from`/`to`/`count`, full list.
  - Deleting the draft leaves `main` materializable and originals present.
- **Missing records:** headers present, a record absent → commit skipped, SHA
  not marked imported, warning names the hash, recommends `atomic pull`.
- **No headers:** git-authored commit → normal change record written (unchanged
  path); dry-run router recommends `git pull && atomic git import`.
- **Divergence (decision A):** synthesize a squash tree differing from the
  records → insert rolled back, commit skipped, `main` unchanged.
- **Dry-run classification:** each row of the §4.4 table produces the right
  recommendation string.
- `phase3_finalize` accounting holds with the `squash_inserted` bucket.

---

## 9. Rollout / compatibility

- Local-only change to `atomic-cli` import + `atomic-repository`. No push/pull
  wire-format change, so no `atomic-storage` `X-Atomic-Min-Version` bump
  required (unless a future server-side reclassify is added).
- ReviewGate metadata gains additive fields (`from`/`to`/`count`/`inserted`);
  older readers ignore unknown keys.
- Behavior change is gated on "squash with all records present"; git-authored
  and non-atomic imports are unaffected.

---

## 10. Open items for reviewer

1. **D1** — verify+skip (A) or trust-and-insert (B)? (Primary; determines size
   and risk.)
2. D2 dependency-closure semantics, D3 SHA mapping, D4 retroactive repair, D5
   merges, and the §7 tag-naming-collision policy — each has a recommended
   default; confirm or override inline.
