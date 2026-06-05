# PRD: First-Class Tags & Incremental Git Sync

## Status: Draft
## Authors: Lee Faus, Atomic Core Team
## Date: 2026-06-04

---

## Executive Summary

Tags in Atomic are dead code — JSON files on disk that nobody uses, with no
transactional guarantees, no content addressing, and no sync support. Meanwhile
the identity system already has a `TAG=1` entity type in `NODE_TYPES` with
nowhere to store actual tag data.

This PRD does two things:

1. **Add `TAG_RECORDS`** — a redb sub-table that gives tags a real home next to
   the entity identity layer that already exists (INTERNAL/EXTERNAL/NODE_TYPES).
   Delete the file-based tag code.

2. **Extend `atomic git import --incremental`** with squash/merge classification
   and `ReviewGate` tags, solving incremental Git sync without inventing new
   commands.

The key insight: a Git squash merge is semantically equivalent to an Atomic
tag. Both say "this set of changes, at this point in time, is a blessed state."

---

## The Entity Model (What Already Exists)

The entity system is already in place, just fragmented:

```
INTERNAL    hash → node_id          identity lookup by hash
EXTERNAL    node_id → hash          identity lookup by id
NODE_TYPES  node_id → u8            type discriminator
```

`register_change()`, `register_tag()`, `register_attestation()`,
`register_provenance()` are the same function with a different `u8` constant.

**Changes** have sub-tables: `GRAPH`, `INODE_GRAPH`, `DEPS`, `VIEW_CHANGES`,
`CHANGE_META`, `CHANGE_GRAPH`, etc.

**Tags** have `NODE_TYPES` entry `TAG=1` and... nothing else. No sub-table.
The actual tag data lives as JSON files on disk at `.atomic/tags/{view}/{name}.tag`
that nobody uses because the feature isn't functional.

The fix: add `TAG_RECORDS` as the tag sub-table. Same pattern as every other
entity type.

---

## Part 1: First-Class Tags

### 1.1 `TAG_RECORDS` Table

```rust
/// Tag-specific data, keyed by entity_id from INTERNAL/EXTERNAL.
///
/// Same pattern as CHANGE_META for changes. The entity identity layer
/// (INTERNAL, EXTERNAL, NODE_TYPES) handles hash ↔ id mapping and type
/// discrimination. This table stores the tag-specific payload.
pub const TAG_RECORDS: TableDefinition<u64, &[u8]> =
    TableDefinition::new("tag_records");
```

Keyed by `entity_id` (the `NodeId` returned by `register_tag()`).
Value is postcard-serialized `TagRecord`.

### 1.2 `TagRecord`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagRecord {
    // === Identity ===
    pub name: String,
    pub view: String,

    // === Position in the view's change sequence ===
    pub sequence: u64,
    pub state: Merkle,
    pub change_hash: Hash,

    // === Metadata ===
    pub timestamp: DateTime<Utc>,
    pub author: Option<Author>,
    pub message: Option<String>,

    // === Kind ===
    pub kind: TagKind,

    // === Extensible provenance (not included in content hash) ===
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TagKind {
    /// Named release bookmark (e.g., "v1.2.3").
    Release,
    /// Review attestation — marks changes as reviewed and approved.
    /// Created by incremental git import when a merge/squash is detected.
    ReviewGate,
    /// Custom/user-defined.
    Custom,
}
```

### 1.3 Content Hash

Tags are content-addressed for sync and integrity. The `metadata` field
is excluded from the hash (same pattern as `Change.unhashed`):

```rust
impl TagRecord {
    pub fn content_hash(&self) -> Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.name.as_bytes());
        hasher.update(self.view.as_bytes());
        hasher.update(&self.sequence.to_le_bytes());
        hasher.update(self.state.as_bytes());
        hasher.update(self.change_hash.as_bytes());
        hasher.update(&self.timestamp.timestamp().to_le_bytes());
        if let Some(ref author) = self.author {
            hasher.update(author.name.as_bytes());
        }
        if let Some(ref message) = self.message {
            hasher.update(message.as_bytes());
        }
        hasher.update(&[self.kind as u8]);
        Hash::from_bytes(*hasher.finalize().as_bytes())
    }
}
```

### 1.4 Tag Name Index

Tags are looked up by name, not just by entity_id. Add a name index:

```rust
/// Tag name index: "{view}\0{tag_name}" → entity_id
///
/// Enables get_tag_by_name(view, name) without scanning TAG_RECORDS.
pub const TAG_NAME_INDEX: TableDefinition<&str, u64> =
    TableDefinition::new("tag_name_index");
```

Null byte separator enables prefix scan for "all tags in view X".

### 1.5 Trait Methods

On `ViewTxnT` (read):

```rust
fn get_tag(&self, view: &str, name: &str) -> Result<Option<TagRecord>, PristineError>;
fn list_tags(&self, view: &str) -> Result<Vec<TagRecord>, PristineError>;
fn find_tag_by_hash(&self, hash: &Hash) -> Result<Option<TagRecord>, PristineError>;
```

On `MutTxnT` (write):

```rust
fn put_tag(&mut self, tag: &TagRecord) -> Result<NodeId, PristineError>;
fn del_tag(&mut self, view: &str, name: &str) -> Result<bool, PristineError>;
```

`put_tag` does:
1. Compute content hash
2. `register_tag(&hash)` → get entity_id (existing function, writes INTERNAL/EXTERNAL/NODE_TYPES)
3. Serialize `TagRecord`, write to `TAG_RECORDS[entity_id]`
4. Write to `TAG_NAME_INDEX["{view}\0{name}"]` → entity_id

### 1.6 `del_view` Cleanup

Add `TAG_RECORDS` + `TAG_NAME_INDEX` cleanup to `del_view()`. Prefix scan
`TAG_NAME_INDEX` for `"{view}\0"`, delete matching entries from both tables.

### 1.7 Rename Rust Constant

```rust
// Developer sees a clear name. redb file unchanged. Zero migration.
pub const MERKLE_CHAIN: TableDefinition<&[u8; 16], &[u8; 32]> =
    TableDefinition::new("tags");
```

### 1.8 Collapse `register_*` Functions

```rust
fn register_entity(&mut self, hash: &Hash, entity_type: u8) -> Result<NodeId, PristineError> {
    // Check INTERNAL for existing
    // Allocate node_id
    // Write EXTERNAL, INTERNAL, NODE_TYPES
}

fn register_change(&mut self, hash: &Hash) -> Result<NodeId, _> {
    self.register_entity(hash, node_type::CHANGE)
}
fn register_tag(&mut self, hash: &Hash) -> Result<NodeId, _> {
    self.register_entity(hash, node_type::TAG)
}
// etc.
```

### 1.9 Delete File-Based Tag Code

Delete:
- `atomic-repository/src/tags/mod.rs` — `save_tag`, `load_tag`, `delete_tag`, file I/O
- `atomic-repository/src/tags/queries.rs` — `list_tags`, `list_all_tags`, filesystem walks
- `atomic-repository/src/tags/types.rs` — old `Tag` struct, `TagOptions`, `TagFilter`

The filesystem directory `.atomic/tags/` is no longer created or read.

### 1.10 Wire Into Push/Pull

The plumbing already exists and is never called:

| What | Status |
|---|---|
| `HttpRemote::upload_tag(state, view, data)` | ✅ Exists, never called |
| `HttpRemote::download_tag(state)` | ✅ Exists, never called |
| `PushStats.tags_uploaded` | ✅ Exists, always zero |
| `PullStats.tags_downloaded` | ✅ Exists, always zero |
| `PushChange.tagged` / `PullChange.tagged` | ✅ Carried in protocol |

Wire it up: after pushing/pulling changes, push/pull `TAG_RECORDS` entries
for the view. Compare content hashes for incremental sync.

### Deliverables — Part 1

| # | Task | Estimate |
|---|------|----------|
| 1.1 | `TAG_RECORDS` + `TAG_NAME_INDEX` table definitions | half day |
| 1.2 | `TagRecord`, `TagKind` types + content hash | half day |
| 1.3 | `ViewTxnT` reads: `get_tag`, `list_tags`, `find_tag_by_hash` | half day |
| 1.4 | `MutTxnT` writes: `put_tag`, `del_tag` | half day |
| 1.5 | `del_view` cleanup | trivial |
| 1.6 | Rename `TAGS` constant → `MERKLE_CHAIN` | trivial |
| 1.7 | Collapse four `register_*` → one `register_entity` | half day |
| 1.8 | Delete file-based tag code | half day |
| 1.9 | Update `Repository` tag methods + CLI commands | 1 day |
| 1.10 | Wire tag sync into push/pull | 1 day |
| 1.11 | Tests | 1 day |

**Total Part 1**: ~6 days

---

## Part 2: Incremental Git Sync via Tags

### Problem

Teams adopting Atomic want to keep GitHub as a backup and review platform.
The workflow:

```
Agent work in Atomic draft views
    → insert to DEV view
    → materialize → git add/commit → push to GitHub
    → PR review on GitHub (DEV → RELEASE)
    → GitHub merge (squash/rebase/merge commit)
    → sync back to Atomic ???
```

The "sync back" step is hard because GitHub merge operations create new Git
commits that don't correspond to any existing Atomic change.

### Key Insight: Squash Merge ≡ Tag

A squash commit is not new work — it's a **review attestation**. It says
"changes A through D were reviewed, approved, and constitute a blessed state."
That's exactly what a `ReviewGate` tag says.

```
┌─────────────────────────────────────────────────────────────────────┐
│                  Squash Merge ≡ ReviewGate Tag                      │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  Git world:                                                         │
│    DEV branch:     A → B → C → D                                    │
│    RELEASE branch: ... → S (squash of A+B+C+D, SHA: abc123)        │
│                                                                     │
│  Atomic world:                                                      │
│    DEV view:       [A, B, C, D]                                     │
│    RELEASE view:   [A, B, C, D]  ← same changes, inserted          │
│                         │                                           │
│                    🏷️ tag: "pr-42"                                   │
│                         kind: ReviewGate                            │
│                         metadata.git.sha: "abc123..."               │
│                         metadata.git.pr: 42                         │
│                         metadata.git.merge_strategy: "squash"       │
│                                                                     │
│  No synthetic change. No content duplication.                       │
│  Individual change provenance preserved.                            │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### What Already Exists

`atomic git import` already handles the hard work:

| Capability | Status | Code |
|---|---|---|
| Walk git history, create Atomic changes | ✅ | `parallel.rs` three-phase pipeline |
| Track git SHA → Atomic change | ✅ | `change.unhashed.git.sha` |
| Incremental import (skip known SHAs) | ✅ | `--incremental` + `get_imported_shas()` |
| Merge commit handling | ✅ | First-parent diff + phase 3 reconciliation |
| Branch → View mapping | ✅ | `--all` creates shared view per branch |
| File ops: add, modify, delete, rename | ✅ | Full pipeline |

What's missing is small and specific:

| Gap | Fix |
|---|---|
| O(n) SHA lookup | `GIT_SHA_INDEX` replaces `get_imported_shas()` scan |
| Squash → tag mapping | Post-import classification creates ReviewGate tags |
| Atomic → Git direction | `atomic git push` (materialize + commit + trailers) |

### Architecture

#### 2.1 `GIT_SHA_INDEX` — Replace the O(n) Scan

`get_imported_shas()` loads every change on the view, parses each one's
`unhashed.git.sha`, and builds a `HashSet<String>`. Replace with a redb
secondary index:

```rust
/// Git SHA → Atomic entity_id.
///
/// Points to the entity_id of the Atomic change (or tag) that
/// corresponds to this Git commit. Populated during git import.
pub const GIT_SHA_INDEX: TableDefinition<&str, u64> =
    TableDefinition::new("git_sha_index");
```

Key: full 40-char hex SHA. Value: entity_id (look up EXTERNAL/NODE_TYPES
to get the hash and type).

**Write path**: `parallel.rs` phase 2 already has the git SHA and the
change hash at write time. One `table.insert()` after
`write_import_graph_change` / `write_import_recorded`.

**Read path**: `--incremental` replaces the `HashSet` build with a direct
`GIT_SHA_INDEX.get(oid)` per commit during `collect_commit_oids`.

**Backfill**: First `--incremental` run on an existing repo does one scan
of `change.unhashed.git.sha` to populate the index. Subsequent runs are O(1)
per commit.

**Prefix lookup** for `atomic change --git-sha abc123`:
```rust
fn find_by_sha_prefix(&self, prefix: &str) -> Result<Option<(NodeId, Hash)>> {
    // Range scan: prefix.."prefix+g" (one past hex range)
}
```

#### 2.2 Post-Import Squash/Merge Classification

After `atomic git import --incremental` imports new commits, a post-import
pass classifies each newly imported change. This runs inside the existing
import pipeline — not a new command.

**When it triggers**: The import pipeline already tracks which changes are
new (vs skipped by `--incremental`). After phase 2 writes complete, iterate
newly imported changes and classify:

```rust
enum ImportedCommitKind {
    /// Normal commit. Already a full Atomic change. Nothing extra.
    Normal,

    /// Merge commit. Import already handled it. Create ReviewGate tag.
    Merge {
        parent_shas: Vec<String>,
    },

    /// Squash merge. The imported change IS the squash. Create a
    /// ReviewGate tag linking back to the original individual changes.
    Squash {
        original_changes: Vec<Hash>,
        source_branch: Option<String>,
    },
}
```

**Classification algorithm**:

```
for each newly_imported_change:
    sha = change.unhashed.git.sha
    commit = git_repo.find_commit(sha)

    // 1. Check for Atomic-Changes trailer in commit message
    //    (written by `atomic git push`, see §2.4)
    if commit.message contains "Atomic-Changes: HASH1, HASH2, ..."
        → Squash { original_changes: parsed_hashes }

    // 2. Multi-parent = merge commit
    if commit.parents.len() > 1
        → Merge { parent_shas }

    // 3. Single-parent on a protected/shared view, matches squash patterns
    if commit is single-parent AND target view is Shared:
        // GitHub squash format: "title (#42)\n\n* commit msg 1\n* commit msg 2"
        if message matches squash pattern
            extract PR number, look up original changes in DEV view via
            GIT_SHA_INDEX on the constituent commit SHAs
            → Squash { original_changes, source_branch }

    // 4. Normal commit
    → Normal
```

**What happens for each kind**:

| Kind | Action |
|---|---|
| Normal | Nothing — the imported change is the complete record |
| Merge | Create `ReviewGate` tag on the view at this sequence point |
| Squash | Insert original changes into the view (if not already present via import), then create `ReviewGate` tag linking to them |

**ReviewGate tag metadata**:

```json
{
    "git": {
        "sha": "abc123def456...",
        "merge_strategy": "squash",
        "source_branch": "dev",
        "pr_number": 42
    },
    "changes": {
        "original_hashes": ["HASH_A", "HASH_B", "HASH_C", "HASH_D"]
    }
}
```

#### 2.3 The Insert Path (Squash Handler Detail)

When a squash merge is detected on RELEASE and the original changes are
identified:

```
1. For each original_change hash:
   a. Check if it's already in the RELEASE view (via REV_VIEW_CHANGES)
   b. If not: insert_change(hash, "release", allow_conflicts=true)
      - Edges already in GRAPH (came from DEV view) → already_in_graph=true
      - Only writes VIEW_CHANGES entry → O(1)

2. Create ReviewGate tag:
   put_tag(TagRecord {
       name: format!("pr-{}", pr_number),  // or "merge-{short_sha}"
       view: "release",
       sequence: view.change_count,
       state: view.state,
       change_hash: last_inserted_hash,
       kind: TagKind::ReviewGate,
       metadata: { git provenance },
   })

3. The squash commit's own imported change is also in the view.
   It coexists with the individual changes — the graph handles both.
```

#### 2.4 `atomic git push` — The Atomic → Git Direction

One new command. Thin wrapper around materialize + git commit:

```bash
atomic git push [--view <view>] [--message <msg>]
```

**What it does**:

```
1. Working copy is already materialized (from current view)
2. git add -A
3. Synthesize commit message:
   - From --message flag, OR
   - From change messages since last git push
4. git commit with trailers:
     Atomic-Changes: HASH_A, HASH_B, HASH_C, HASH_D
     Atomic-State: <merkle>
5. Update GIT_SHA_INDEX: new git SHA → each Atomic change entity_id
6. git push origin <branch>
```

The `Atomic-Changes` trailer is what makes squash classification reliable
on the pull side. When GitHub squash-merges a PR, the squash commit's
message includes the original commit messages (and their trailers). The
classifier parses these to find the original Atomic change hashes.

#### 2.5 The DEV Update Problem (Solved)

After a squash merge to RELEASE, DEV needs to be updated. In Git, this
is infamously painful — permanent history divergence, conflicts even when
content is identical.

**In Atomic, this problem doesn't exist.**

After `atomic git import --incremental` on RELEASE:
- RELEASE view: original changes [A, B, C, D] inserted + ReviewGate tag
- DEV view: [A, B, C, D] (already has the same changes)
- **No divergence.** Both views share the same change objects.

The Git side still needs reconciliation:

```bash
git checkout dev
git reset --hard origin/release    # or git merge release
```

The Atomic side is clean. The Git branch topology is a cosmetic concern.

#### 2.6 Full Workflow

```
Day 1: Initial Setup
──────────────────────
$ atomic git import                    # .git + .atomic, creates views from branches

Day 2: Agent Development
─────────────────────────
$ atomic view create feature --draft --parent dev
# ... agent works, records changes A, B, C, D ...
$ atomic insert from-view feature --to-view dev

Day 2: Sync to GitHub
──────────────────────
$ atomic view switch dev
$ atomic git push                      # materialize → commit (with trailers) → push
# Git DEV branch now has commit with:
#   Atomic-Changes: HASH_A, HASH_B, HASH_C, HASH_D

Day 3: PR Review
─────────────────
# Reviewer approves PR #42: dev → release
# GitHub squash-merges → commit S on release branch

Day 3: Sync Back
─────────────────
$ git checkout release && git pull origin release
$ atomic view switch release
$ atomic git import --incremental
#   1. Finds commit S (not in GIT_SHA_INDEX)
#   2. Imports S as a new Atomic change (standard import pipeline)
#   3. Post-import: classifies S as Squash (finds Atomic-Changes trailer)
#   4. Inserts original changes A, B, C, D into RELEASE view (O(1) each)
#   5. Creates ReviewGate tag "pr-42" on RELEASE
#   6. Updates GIT_SHA_INDEX: S → entity_id

Day 3: Reconcile DEV (Git side only)
──────────────────────────────────────
$ git checkout dev && git reset --hard origin/release
# Atomic DEV already has A, B, C, D — no Atomic-side work needed

Day 4: Next Cycle
──────────────────
$ atomic view create feature-2 --draft --parent dev
# Clean state, both systems in sync
```

#### 2.7 Edge Cases

**Conflict resolution during GitHub merge**:
The squash commit contains content not in any Atomic change. The import
pipeline handles this normally — it imports the squash commit as a regular
change (the diff captures the conflict resolution). The ReviewGate tag
links to both the original changes and notes the content divergence in
its metadata.

**Review fixup commits on GitHub**:
Same — the squash includes them. The imported change captures everything.
The tag links back to the original changes that were identifiable.

**Multiple PRs merged between syncs**:
`--incremental` processes all new commits. Each squash/merge gets its own
ReviewGate tag.

**Force-push on a branch**:
Git history was rewritten. `--incremental` detects that `last_synced_sha`
is no longer reachable. Falls back to full SHA comparison against
`GIT_SHA_INDEX`. Existing Atomic changes are not removed (immutable).

**No trailer present (someone used raw git)**:
Falls back to GitHub squash format parsing in the commit message, then
to classifying as Normal. The change is imported as a regular Atomic
change. No ReviewGate tag. This is fine — the provenance link is a bonus,
not a requirement.

### Deliverables — Part 2

| # | Task | Depends on | Estimate |
|---|------|-----------|----------|
| 2.1 | `GIT_SHA_INDEX` table + read/write methods | — | 1 day |
| 2.2 | Populate `GIT_SHA_INDEX` during import (phase 2 write path) | 2.1 | half day |
| 2.3 | Backfill `GIT_SHA_INDEX` on first `--incremental` run | 2.1 | half day |
| 2.4 | Replace `get_imported_shas()` with index lookup | 2.1 | half day |
| 2.5 | Post-import commit classification | 2.1, Part 1 | 1.5 days |
| 2.6 | Squash handler (insert originals + ReviewGate tag) | 2.5 | 1.5 days |
| 2.7 | Merge handler (ReviewGate tag) | 2.5 | half day |
| 2.8 | Trailer parsing (`Atomic-Changes`, `Atomic-State`) | — | half day |
| 2.9 | `atomic git push` command | 2.1, 2.8 | 1.5 days |
| 2.10 | Tests | all above | 2 days |
| 2.11 | Workflow documentation | all above | half day |

**Total Part 2**: ~10 days

---

## Part 3: Shadow Mode (Future)

Once Parts 1 and 2 are in place, the shadow mode design from
`git-shadow-tasks.md` simplifies dramatically:

- **`post-commit` hook** → calls `atomic git import --incremental`
- **`post-merge` hook** → calls `atomic git import --incremental`
  (squash classification creates ReviewGate tags automatically)
- **`post-rewrite` hook** → `--incremental` with `GIT_SHA_INDEX` detects
  that rebased commits are new SHAs for equivalent content
- **Provenance edges** (`Rewrote`, `Absorbed`) → link to ReviewGate tag
  metadata instead of a parallel data model

Shadow mode becomes a hook installer + the existing import pipeline. No
new data model, no new commands, no new tables beyond what Parts 1 and 2
already provide.

---

## Open Questions

### Q1: Should `atomic git push` create one git commit per sync or one per change?

Default: one commit per push (batches all new changes). The
`Atomic-Changes` trailer preserves the individual mapping. Add
`--per-change` later if needed.

### Q2: What if the squash commit content diverges from the original changes?

The squash commit is imported as its own Atomic change regardless. The
ReviewGate tag links it to the originals. Both exist in the view. The
graph handles overlapping content via its CRDT merge semantics.

### Q3: Should ReviewGate tags auto-create, or require opt-in?

Default: auto-create during `--incremental` import when a squash/merge
is detected. Add `--no-tags` flag to suppress if someone doesn't want them.

---

## Success Criteria

### Part 1: First-Class Tags
- [ ] `TAG_RECORDS` table in redb, keyed by entity_id
- [ ] `TAG_NAME_INDEX` for name-based lookup
- [ ] Tags content-addressed (Blake3)
- [ ] Tags sync via push/pull (wire existing plumbing)
- [ ] `TagKind::ReviewGate` supported
- [ ] `TagRecord.metadata` carries arbitrary JSON
- [ ] File-based tag code deleted
- [ ] `TAGS` Rust constant renamed to `MERKLE_CHAIN`
- [ ] Four `register_*` functions collapsed to one `register_entity`
- [ ] `del_view` cleans up tag records

### Part 2: Incremental Git Sync
- [ ] `GIT_SHA_INDEX` populated during import, O(1) lookup
- [ ] `get_imported_shas()` replaced with index lookup
- [ ] Post-import classification detects squash and merge commits
- [ ] Squash merges create ReviewGate tags (not synthetic changes)
- [ ] `atomic git push` materializes + commits with trailers
- [ ] Round-trip: push → squash on GitHub → incremental import → verify state
- [ ] DEV view doesn't diverge from RELEASE after squash merge

---

## Appendix: Table Summary

### Existing (unchanged)

| Table | Key | Value | Purpose |
|-------|-----|-------|---------|
| `INTERNAL` | hash [u8;32] | node_id u64 | Entity lookup by hash |
| `EXTERNAL` | node_id u64 | hash [u8;32] | Entity lookup by id |
| `NODE_TYPES` | node_id u64 | type u8 | Entity type discriminator |
| `"tags"` (const `MERKLE_CHAIN`) | (view_id, seq) [u8;16] | merkle [u8;32] | Per-sequence Merkle state |
| `STATES` | (view_id, merkle) [u8;40] | seq u64 | Reverse Merkle lookup |

### New

| Table | Key | Value | Purpose |
|-------|-----|-------|---------|
| `TAG_RECORDS` | entity_id u64 | TagRecord [u8] | Tag-specific data (sub-table) |
| `TAG_NAME_INDEX` | "{view}\0{name}" str | entity_id u64 | Name-based tag lookup |
| `GIT_SHA_INDEX` | git_sha str | entity_id u64 | Git SHA → Atomic entity |

### Deleted

| What | Was |
|------|-----|
| `.atomic/tags/{view}/{name}.tag` | JSON files on disk |
| `atomic-repository/src/tags/mod.rs` | File I/O for tags |
| `atomic-repository/src/tags/queries.rs` | Filesystem walks |
| `atomic-repository/src/tags/types.rs` | Old `Tag` struct |

## Appendix: CLI Commands

```bash
# Tags (Part 1 — redb backend)
atomic tag create v1.2.3 -m "Release 1.2.3"
atomic tag list [--view release]
atomic tag show v1.2.3
atomic tag delete v1.2.3

# Git sync (Part 2 — extends existing commands)
atomic git import                          # existing: full import
atomic git import --incremental            # existing: now with GIT_SHA_INDEX + tags
atomic git push [--view dev] [-m "msg"]    # new: materialize → commit → push
```
