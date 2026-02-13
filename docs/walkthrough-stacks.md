# Atomic VCS: Stack Walkthrough

This document provides a step-by-step walkthrough of Atomic's stack functionality, demonstrating how stacks work as **views** of a shared graph rather than separate branches.

## Glossary: Core Terminology

Before diving into the walkthrough, let's establish the key terms used throughout Atomic:

### Storage Layer

| Term | Definition |
|------|------------|
| **Shared Graph** | The single directed acyclic graph (DAG) that stores ALL content across ALL stacks. Lives in `pristine.redb`. Changes from any stack are recorded here. |
| **Vertex** | A node in the graph representing a contiguous chunk of content (e.g., a line, a token, or file metadata). |
| **Edge** | A directed connection between vertices that defines ordering and relationships. Edges have flags (BLOCK, PARENT, DELETED, etc.). |
| **Change** | An immutable, content-addressed transformation that adds/removes vertices and edges. Identified by a Blake3 hash. |
| **Pristine** | The persistent storage layer (redb database) containing the graph, stack metadata, and indexes. |

### View Layer

| Term | Definition |
|------|------------|
| **Stack** | A **view** of the shared graph. Not a copy—just an ordered list of which changes are "visible" in this perspective. |
| **Changelog** | The ordered sequence of change hashes that belong to a stack. This is what makes each stack's view unique. |
| **Merkle State** | A rolling hash of the changelog: `state_n = Hash(state_{n-1} || change_hash_n)`. Uniquely identifies a stack's exact state. |
| **Fork** | Creating a new stack by copying another stack's changelog. No graph data is duplicated. |
| **Orphan Stack** | A stack with an empty changelog (no history). Used for stash and imports. |

### Semantic Layer (CRDT)

| Term | Definition |
|------|------------|
| **Trunk** | A file in the semantic model. Has a path, encoding, and contains branches. |
| **Branch** | A line within a file. Contains leaves (tokens). Can be alive or deleted. |
| **Leaf** | A token within a line (keyword, identifier, operator, whitespace, etc.). The finest granularity for diff/blame. |
| **Semantic Diff** | Human-readable diff at the line and token level, not byte offsets. Shows "line 42 changed" not "bytes 1024-1089". |

### Operations

| Term | Definition |
|------|------------|
| **Record** | Create a new change from working copy modifications and add it to the current stack's changelog. |
| **Apply** | Add a change to a stack's changelog (making it visible in that view). |
| **Stash** | Save uncommitted changes to a temporary orphan stack for later use. |

### Visual Model

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                            SHARED GRAPH (pristine.redb)                     │
│  ┌─────────┐    ┌─────────┐    ┌─────────┐    ┌─────────┐    ┌─────────┐   │
│  │Change 1 │    │Change 2 │    │Change 3 │    │Change A │    │Change B │   │
│  │(init)   │    │(update) │    │(feature)│    │(agent A)│    │(agent B)│   │
│  └─────────┘    └─────────┘    └─────────┘    └─────────┘    └─────────┘   │
│       All changes live here, regardless of which stack recorded them        │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
            ┌─────────────────────────┼─────────────────────────┐
            ▼                         ▼                         ▼
     ┌─────────────┐          ┌─────────────┐          ┌─────────────┐
     │    main     │          │  agent/A    │          │  agent/B    │
     │  Changelog: │          │  Changelog: │          │  Changelog: │
     │  [1, 2, 3]  │          │ [1, 2, 3, A]│          │ [1, 2, 3, B]│
     └─────────────┘          └─────────────┘          └─────────────┘
            │                         │                         │
            ▼                         ▼                         ▼
     Working Copy:             Working Copy:             Working Copy:
     sees 1+2+3                sees 1+2+3+A              sees 1+2+3+B
```

**Key Insight**: Agent A and Agent B both see the baseline (1, 2, 3) but are isolated from each other's changes (A vs B) until they explicitly `apply` from each other's stack.

---

## Key Concept: Stacks vs Branches

In Git, branches are pointers to commits that represent divergent histories. In Atomic, **stacks are views** of the same underlying graph. Think of them like database views or saved queries - they represent which changes have been applied and in what order, but they all share the same graph data.

| Aspect | Git Branches | Atomic Stacks |
|--------|--------------|---------------|
| Data Model | Pointer to commit | Ordered sequence of applied changes |
| Storage | Duplicates history | Shares underlying graph |
| "Merging" | 3-way merge | Apply missing changes |
| Forking | Creates new history | Copies change log (same graph) |

## Walkthrough

### 1. Initialize a Repository

```bash
$ atomic init
✓ Initialized empty Atomic repository in /tmp/atomic-walkthrough/.atomic
Created stack: dev

Next steps:
  atomic add <files>      Add files to track
  atomic record -m "..."  Record your first change
  atomic status           See what's changed
```

This creates the `.atomic` directory with:
- `pristine.redb` - The graph database (using redb)
- `changes/` - Content-addressed change files
- `config.toml` - Repository configuration

```bash
$ cat .atomic/config.toml
# Atomic repository configuration

[stack]
default = "dev"
```

### 2. Check Initial Stack

```bash
$ atomic stack list
* dev
```

The asterisk (`*`) indicates the current stack. Initially, we have one empty stack called `dev`.

### 3. Create and Track a File

```bash
$ echo "Hello, World!" > hello.txt

$ atomic status
On stack dev
State: AAAAAAAAAAAA...

Untracked files:
  (use "atomic add <file>..." to include in what will be recorded)

	hello.txt

Use "atomic add <file>..." to track files
```

The `State: AAAAAAAAAAAA...` is the Merkle state - a hash representing the complete state of the stack. An empty stack has a zero state.

```bash
$ atomic add hello.txt
Adding: hello.txt
✓ Added 1 file

Use 'atomic record -m "..."' to record your changes
```

```bash
$ atomic status
On stack dev
State: AAAAAAAAAAAA...

Changes to be recorded:
  (use "atomic reset <file>..." to discard changes)

	new file:   hello.txt

Use "atomic record" to record your changes
```

### 4. Record the First Change

```bash
$ atomic record -m "Add hello.txt"
[dev 1/ETVBO2LR] Add hello.txt
 1 file changed, +3 vertices, ~0 edges, 14 bytes
 1 line (+1 -0 ~0)
 5 tokens (+5 -0 ~0)
 hello.txt
```

Notice the output shows:
- `+3 vertices` - Graph nodes created (name, inode, content)
- `~0 edges` - Edge modifications
- `14 bytes` - Content size
- Line and token statistics from the CRDT layer

```bash
$ atomic log
change ETVBO2LRXZS3...
Author: leefaus <lee@atomic.dev>
Date:   2026-02-03 22:25:04

    Add hello.txt
```

### 5. Modify the File and Record Again

```bash
$ echo "Hello, Atomic VCS!" > hello.txt

$ atomic status
On stack dev
State: JGN6YSXJLJRV...

Changes to be recorded:
  (use "atomic reset <file>..." to discard changes)

	modified:   hello.txt

Use "atomic record" to record your changes
```

Notice the state hash has changed - it now reflects the first change.

```bash
$ atomic diff
diff --atomic a/hello.txt b/hello.txt (+1 -1)
--- a/hello.txt
+++ b/hello.txt
@@ -1,1 +1,1 @@
   1      -Hello, World!
        1 +Hello, Atomic VCS!
```

```bash
$ atomic record -m "Update greeting"
[dev 1/XBR3ELSQ] Update greeting
 1 file changed, +1 vertices, ~1 edges, 19 bytes
 3 lines (+1 -1 ~1)
 7 tokens (+7 -0 ~0)
 hello.txt
```

```bash
$ atomic log
change XBR3ELSQOHKZ...
Author: leefaus <lee@atomic.dev>
Date:   2026-02-03 22:25:19

    Update greeting

change ETVBO2LRXZS3...
Author: leefaus <lee@atomic.dev>
Date:   2026-02-03 22:25:04

    Add hello.txt
```

### 6. Fork a New Stack

This is where Atomic differs significantly from Git. When you fork a stack, you're creating a new **view** of the same graph, not copying history.

```bash
$ atomic stack new feature --from dev
✓ Created stack: feature (forked from dev with 2 changes)
Use 'atomic stack switch feature' to switch to the new stack
```

What happened here:
1. Created a new stack called `feature`
2. **Copied the change log** from `dev` (which changes are applied, in what order)
3. Did **NOT** re-apply changes to the graph (they're already there!)

```bash
$ atomic stack list
* dev
  feature
```

### 7. Switch to the New Stack

```bash
$ atomic stack switch feature
✓ Switched to stack: feature
  1 files updated, 0 directories
```

```bash
$ atomic log
change XBR3ELSQOHKZ...
Author: leefaus <lee@atomic.dev>
Date:   2026-02-03 22:25:19

    Update greeting

change ETVBO2LRXZS3...
Author: leefaus <lee@atomic.dev>
Date:   2026-02-03 22:25:04

    Add hello.txt
```

The `feature` stack has the same changes as `dev` because it was forked from it.

```bash
$ cat hello.txt
Hello, Atomic VCS!
```

### 8. Make Changes on the Feature Stack

```bash
$ echo "Hello, Atomic VCS!
This is a feature branch." > hello.txt

$ atomic diff
diff --atomic a/hello.txt b/hello.txt (+1)
--- a/hello.txt
+++ b/hello.txt
@@ -1,1 +1,2 @@
   1    1  Hello, Atomic VCS!
        2 +This is a feature branch.
```

```bash
$ atomic record -m "Add feature description"
[dev 1/FO5GKKKC] Add feature description
 1 file changed, +1 vertices, ~0 edges, 45 bytes
 1 line (+1 -0 ~0)
 10 tokens (+10 -0 ~0)
 hello.txt
```

```bash
$ atomic log
change FO5GKKKCEOLW...
Author: leefaus <lee@atomic.dev>
Date:   2026-02-03 22:25:49

    Add feature description

change XBR3ELSQOHKZ...
Author: leefaus <lee@atomic.dev>
Date:   2026-02-03 22:25:19

    Update greeting

change ETVBO2LRXZS3...
Author: leefaus <lee@atomic.dev>
Date:   2026-02-03 22:25:04

    Add hello.txt
```

### 9. Switch Back to Dev

```bash
$ atomic stack switch dev
✓ Switched to stack: dev
  1 files updated, 0 directories
```

```bash
$ cat hello.txt
Hello, Atomic VCS!
```

The `dev` stack still has the original content! The new change only exists in `feature`'s change log, so when we switch to `dev`, the working copy is updated to reflect only the changes in `dev`'s log.

```bash
$ atomic log
change XBR3ELSQOHKZ...
Author: leefaus <lee@atomic.dev>
Date:   2026-02-03 22:25:19

    Update greeting

change ETVBO2LRXZS3...
Author: leefaus <lee@atomic.dev>
Date:   2026-02-03 22:25:04

    Add hello.txt
```

### 10. Using the Split Command

The `split` command is a convenience wrapper for creating a forked stack:

```bash
$ atomic split hotfix --switch
✓ Created stack: hotfix (split from dev with 2 changes)
✓ Switched to stack: hotfix
```

```bash
$ atomic stack list
  dev
  feature
* hotfix
```

## Applying Changes Between Stacks

Unlike forking (which copies the change log at creation time), **applying** moves changes from one stack to another after they've diverged. This is how you "merge" in Atomic.

### 11. Apply Changes from One Stack to Another

Let's say `feature` has a change that `dev` doesn't have. First, let's see the state of each stack:

```bash
# dev has 2 changes
$ atomic stack switch dev
✓ Switched to stack: dev

$ atomic log
change XBR3ELSQOHKZ...  Update greeting
change ETVBO2LRXZS3...  Add hello.txt

# feature has 3 changes (one extra)
$ atomic stack switch feature
✓ Switched to stack: feature

$ atomic log
change FO5GKKKCEOLW...  Add feature description
change XBR3ELSQOHKZ...  Update greeting
change ETVBO2LRXZS3...  Add hello.txt
```

Now let's apply the missing change from `feature` to `dev`:

```bash
$ atomic stack switch dev
✓ Switched to stack: dev

# Preview what would be applied (dry run)
$ atomic apply from-stack feature --dry-run
ℹ Applying changes from 'feature' to 'dev'...

ℹ Dry run: 1 change(s) would be applied

Changes:
  FO5GKKKCEOLWF2KMG37BDFPBZXLGOTRBWDSFEOGOGLQSORPA45TA
```

The dry run shows that only **1 change** would be applied - the one that's missing from `dev`. Now let's apply it:

```bash
$ atomic apply from-stack feature
ℹ Applying changes from 'feature' to 'dev'...

✓ Applied 1 change(s)
  New state: 6AYQHA6TJLBVBQ2G6PQ7KVENC5N232XIQHZL2LMTMIC47BH7REJQ
  Skipped:   2 (already applied)
```

Notice:
- **Applied 1**: The missing change was added to dev's change log
- **Skipped 2**: The other 2 changes were already in dev's log

```bash
$ atomic log
change FO5GKKKCEOLW...  Add feature description
change XBR3ELSQOHKZ...  Update greeting
change ETVBO2LRXZS3...  Add hello.txt
```

### 12. Working Copy Updates Automatically

When applying to the **current stack**, Atomic automatically updates the working copy (like Git merge). Notice in the output above:

```bash
✓ Applied 1 change(s)
  New state: 6AYQHA6TJLBVBQ2G6PQ7KVENC5N232XIQHZL2LMTMIC47BH7REJQ
  Skipped:   2 (already applied)
✓ 1 files updated, 0 directories   # <-- Working copy updated automatically!
```

After the apply, the working copy reflects the new state:

```bash
$ cat hello.txt
Hello, Atomic VCS!
This is a feature branch.

$ atomic status
nothing to record, working tree clean
```

**Note**: If you apply to a *different* stack (not your current stack), the working copy is NOT updated since you're not on that stack.

### 13. Cherry-Pick Specific Changes

You can also cherry-pick specific changes by hash prefix:

```bash
# On feature, create two new changes
$ atomic stack switch feature
$ echo "# README" > README.md
$ atomic add README.md && atomic record -m "Add README"
$ echo "Another line" >> hello.txt
$ atomic record -m "Add another line"

$ atomic log
change 5CRHRK5MHCSO...  Add another line
change VSOAHFL6YIRK...  Add README
change FO5GKKKCEOLW...  Add feature description
...
```

Now cherry-pick just the README change to `hotfix`:

```bash
$ atomic stack switch hotfix
✓ Switched to stack: hotfix

$ atomic log
change XBR3ELSQOHKZ...  Update greeting
change ETVBO2LRXZS3...  Add hello.txt

# Cherry-pick by hash prefix
$ atomic apply pick VSOAHFL6
ℹ Cherry-picking 1 change(s) to 'hotfix'...

✓ Applied 1 change(s)
  New state: LYHHGRHH6KIRHMVISP6WZVFH5OD7XBSFP5HJV74QLTGSY6SQ742Q

$ atomic log
change VSOAHFL6YIRK...  Add README
change XBR3ELSQOHKZ...  Update greeting
change ETVBO2LRXZS3...  Add hello.txt

$ cat README.md
# README
This is the feature readme.
```

The README change was cherry-picked to `hotfix` without the "Add another line" change. The working copy was automatically updated because we applied to the current stack.

### Apply vs Fork

| Operation | What It Does | When to Use |
|-----------|--------------|-------------|
| `stack new --from` | Copy change log at creation | Starting a new line of work |
| `apply from-stack` | Add missing changes | "Merging" diverged stacks |
| `apply pick` | Cherry-pick specific changes | Selective integration |

## How Stacks Work Internally

### The Shared Graph

All stacks share the same graph in `pristine.redb`. When you record a change:

1. **Vertices** are added to the graph (content chunks)
2. **Edges** connect vertices (ordering relationships)
3. The change is added to the **current stack's changelog**

The graph is append-only and content-addressed. Once a change is recorded, it exists in the graph forever—stacks just choose whether to include it in their view.

### Stack Changelogs

Each stack maintains:
- **Changelog**: Ordered list of change hashes (which changes are visible, in what order)
- **Merkle state**: A rolling hash uniquely identifying the changelog state

When you fork a stack:
- The changelog entries are **copied** (not the graph data)
- The new stack starts with the same Merkle state
- No graph data is duplicated—it's already in the shared graph

### Working Copy Output

When you switch stacks:
1. Atomic reads the target stack's changelog
2. It outputs the working copy by traversing the graph
3. Only vertices/edges introduced by changes **in that stack's changelog** are included
4. This is why `dev` and `feature` can have different file contents

### Semantic Layer (Trunk → Branch → Leaf)

The graph stores raw bytes efficiently. The **semantic layer** interprets the graph for humans:

```
Trunk (File)
└── Branch (Line 1)
│   ├── Leaf: "fn"
│   ├── Leaf: " "
│   ├── Leaf: "main"
│   ├── Leaf: "()"
│   └── Leaf: " {"
└── Branch (Line 2)
    ├── Leaf: "    "
    ├── Leaf: "println!"
    └── ...
```

This enables:
- **Line-level diffs**: "Line 42 was modified" (not "bytes 1024-1089")
- **Token-level diffs**: `--word-diff` shows individual token changes
- **Fine-grained blame**: Which change introduced each token
- **Semantic conflicts**: Conflicts at meaningful boundaries

### Multi-Agent Workflow Example

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  Scenario: Multiple AI agents working on the same codebase                  │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  1. main has baseline (100 changes)                                         │
│                                                                             │
│  2. Agent A forks: atomic stack new agent/A-1234                            │
│     → agent/A's changelog: [1..100]                                         │
│                                                                             │
│  3. Agent B forks: atomic stack new agent/B-5678                            │
│     → agent/B's changelog: [1..100]                                         │
│                                                                             │
│  4. Agent A records change: "Add auth module"                               │
│     → Graph now has change A                                                │
│     → agent/A's changelog: [1..100, A]                                      │
│     → agent/B's changelog: [1..100] (unchanged - doesn't see A)             │
│                                                                             │
│  5. Agent B records change: "Add payment module"                            │
│     → Graph now has changes A and B                                         │
│     → agent/A's changelog: [1..100, A] (doesn't see B)                      │
│     → agent/B's changelog: [1..100, B] (doesn't see A)                      │
│                                                                             │
│  6. Agent A wants to test with B's changes:                                 │
│     atomic apply from-stack agent/B-5678                                    │
│     → agent/A's changelog: [1..100, A, B]                                   │
│     → Now agent A's CI sees both changes!                                   │
│                                                                             │
│  7. Circuit breaker: Each agent's CI runs against THEIR view only           │
│     → Agent B's broken code can't break Agent A's build                     │
│     → Until Agent A explicitly applies it                                   │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Stashing Uncommitted Changes

Sometimes you have uncommitted changes on the wrong stack. Instead of losing them, you can **stash** them temporarily and apply them to the correct stack later.

### 14. Stash Changes to Move Them

Let's say you're on `dev` but realize your changes should be on `feature`:

```bash
$ atomic status
On stack dev
Changes to be recorded:
    modified: src/auth.rs

# Oops, these changes belong on feature/auth!
$ atomic stash -m "WIP auth changes"
✓ Saved working copy to stash@{0}
✓ Working copy restored to clean state
```

The stash command:
1. Creates a temporary **orphan stack** (no history)
2. Records your uncommitted changes to it
3. Restores your working copy to a clean state

### 15. Apply Stashed Changes to Another Stack

Now switch to the correct stack and apply the stash:

```bash
$ atomic stack switch feature/auth
✓ Switched to stack: feature/auth

$ atomic stash pop
✓ Applied stash@{0} to working copy
✓ Dropped stash@{0}
```

Your changes are now uncommitted on `feature/auth`, ready to be recorded there:

```bash
$ atomic record -m "Add OAuth support"
[feature/auth 3/abc123] Add OAuth support
```

### 16. Stash Management Commands

```bash
# List all stashes
$ atomic stash list
stash@{0}: On dev: WIP auth changes (2 hours ago)
stash@{1}: On feature: Debugging output (1 day ago)

# Show details of a stash
$ atomic stash show stash@{0}

# Apply without deleting
$ atomic stash apply

# Delete a stash
$ atomic stash drop stash@{1}

# Delete all stashes
$ atomic stash clear
```

### Why Orphan Stacks for Stash?

Stashes use **orphan stacks** (created with `--empty`) because:
- **Lightweight**: No changelog to copy
- **Stack-agnostic**: Changes can apply to any stack
- **Temporary**: Easy to clean up after use

This is different from forking a stack, which copies the source's history.

## Summary

| Command | What It Does |
|---------|--------------|
| `atomic stack list` | List all stacks, show current with `*` |
| `atomic stack new NAME` | Fork from current stack (copies change log) |
| `atomic stack new NAME --from SOURCE` | Fork from a specific stack |
| `atomic stack new NAME --empty` | Create an orphan stack with no history (rare) |
| `atomic stack switch NAME` | Switch to a stack, update working copy |
| `atomic split NAME` | Fork current stack (convenience command) |
| `atomic apply from-stack SOURCE` | Apply missing changes from another stack |
| `atomic apply pick HASH...` | Cherry-pick specific changes |
| `atomic stash` | Save uncommitted changes to a temporary orphan stack |
| `atomic stash pop` | Apply and delete most recent stash |
| `atomic stash list` | List all stashes |
| `atomic reset --force` | Manually sync working copy with graph |

Key takeaways:
- **Shared Graph**: All changes from all stacks live in one graph—no duplication
- **Stacks are Views**: A stack is just a changelog (list of change hashes), not a copy of data
- **Changelog = Visibility**: A change is only visible in stacks whose changelog includes it
- **New stacks fork by default**: Ensures shared history and easy integration later
- **Semantic Layer**: Trunk (file) → Branch (line) → Leaf (token) for human-readable diffs
- **Isolation + Visibility**: Agents can work in isolation but selectively see each other's work
- **Apply = Make Visible**: `apply from-stack` adds changes to your changelog (like "merging")
- **Stash = Orphan Stack**: Temporary holding area with no history, can apply anywhere
- **CI/Circuit Breaker**: Each stack's build sees only its changelog—isolated by default