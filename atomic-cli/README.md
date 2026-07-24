# atomic-cli

The command-line interface for **Atomic** — a mathematically sound distributed version control system built on patch theory.

## Installation

```bash
# Build from source
cargo build --release -p atomic-cli

# Install to ~/.cargo/bin
cargo install --path atomic-cli

# Verify
atomic --version
atomic --help
```

## Quick Start

```bash
# Initialize a new repository
atomic init

# Add files and record a change
atomic add src/
atomic record -m "Initial commit"

# View status and history
atomic status
atomic log

# Create a stack (Atomic's version of branches)
atomic stack new feature-auth --switch

# Push to a remote
atomic remote add origin https://api.example.com/repo
atomic push origin
```

## Global Options

| Flag | Description |
|------|-------------|
| `-v, --verbose` | Enable verbose output for debugging |
| `--no-color` | Disable colored terminal output |
| `-h, --help` | Print help information |
| `-V, --version` | Print version |

---

## Command Reference

### Repository Setup

#### `atomic init`

Initialize a new Atomic repository in the current directory.

```bash
# Initialize with defaults (creates "dev" stack)
atomic init

# Initialize with a custom default stack name
atomic init --stack main

# Initialize with language-specific .atomicignore
atomic init --kind rust
atomic init --kind node
atomic init --kind python
```

| Option | Description |
|--------|-------------|
| `--stack <NAME>` | Name of the initial stack (default: `dev`) |
| `--kind <LANG>` | Generate a language-specific `.atomicignore` file |

---

### Working Copy

#### `atomic status`

Show the status of the working copy — modified, added, deleted, and untracked files.

```bash
# Show status
atomic status

# Show short/compact output
atomic status --short
```

#### `atomic add <PATHS>`

Add files or directories to be tracked by Atomic.

```bash
# Add specific files
atomic add src/main.rs src/lib.rs

# Add an entire directory
atomic add src/

# Add everything
atomic add .
```

#### `atomic remove <PATHS>`

Remove files from tracking.

```bash
# Remove and delete from disk
atomic remove old_file.txt

# Stop tracking but keep file on disk
atomic remove --keep secrets.txt

# Remove a directory
atomic remove old_code/
```

**Alias:** `atomic rm`

#### `atomic move <SOURCE> <DESTINATION>`

Move or rename tracked files while preserving version history.

```bash
# Rename a file
atomic move old_name.rs new_name.rs

# Move into a directory
atomic move config.toml src/config.toml
```

**Alias:** `atomic mv`

#### `atomic restore`

Restore the working copy to the last recorded state, discarding uncommitted
changes (the working-copy counterpart of `git restore`). The legacy name
`atomic reset` is kept as an alias.

```bash
# Restore specific files (no --force needed)
atomic restore src/main.rs

# Restore everything (requires --force)
atomic restore --force

# Preview a single file's pristine content
atomic restore --dry-run src/main.rs
```

| Option | Description |
|--------|-------------|
| `--force` | Required only for a whole-tree restore |
| `--dry-run` | Preview without making changes |

To switch views, use `atomic view switch <view>`.

---

### Recording Changes

#### `atomic record`

Record changes from the working copy into the current stack.

```bash
# Record with a message
atomic record -m "Add authentication module"

# Record with message and description
atomic record -m "Fix login bug" -d "The session cookie was not being set correctly on redirect"

# Record specific files only
atomic record -m "Update config" src/config.rs

# Record with author override
atomic record -m "Pair programming" --author "Alice <alice@example.com>"

# Choose diff algorithm
atomic record -m "Refactor" --algorithm patience
```

| Option | Description |
|--------|-------------|
| `-m, --message <MSG>` | Change message (required) |
| `-d, --description <DESC>` | Longer description |
| `--author <AUTHOR>` | Override author (format: `Name <email>`) |
| `--algorithm <ALG>` | Diff algorithm: `myers` (default) or `patience` |

#### `atomic revise`

Modify a previously recorded change in-place. Unlike `git commit --amend`, this can target any change in the stack, not just the most recent.

```bash
# Revise the last change (edit files, then revise)
atomic revise

# Revise with a new message
atomic revise -m "Better commit message"

# Only change the message (no file changes)
atomic revise --reword

# Revise a previous change by reference
atomic revise @~1
atomic revise @~1 -m "Updated message for previous change"
```

The revise workflow:

```
Before:  [A] ← [B] ← [C] ← @
                ↑
           target (@~1)

Step 1: Unrecord C (saved as pending)
Step 2: Unrecord B (the target)
Step 3: User edits files, record as B'
Step 4: Re-apply C on top of B'

After:   [A] ← [B'] ← [C] ← @
```

| Option | Description |
|--------|-------------|
| `-m, --message <MSG>` | New change message |
| `--reword` | Only change the message, don't modify files |
| `--no-edit` | Keep the original message |
| `<REF>` | Change reference: `@` (last), `@~1` (previous), `@~N` |

---

### Viewing History

#### `atomic log`

Display the change history for the current stack.

```bash
# Show full log
atomic log

# Compact one-line format
atomic log --oneline

# Limit number of entries
atomic log -n 10

# Show log for a different stack
atomic log feature-auth
```

#### `atomic change [HASH]`

Show detailed information about a specific change.

```bash
# Show the most recent change
atomic change

# Show change by hash prefix
atomic change ABC12345

# Show change by sequence number
atomic change #42
```

#### `atomic diff`

Show differences between the working copy and the last recorded state.

```bash
# Show all changes
atomic diff

# Show changes for a specific file
atomic diff src/main.rs

# Show only statistics
atomic diff --stat

# Use patience diff algorithm
atomic diff --algorithm patience

# Word-level diff
atomic diff --word-diff
```

| Option | Description |
|--------|-------------|
| `--stat` | Show only insertions/deletions statistics |
| `--algorithm <ALG>` | `myers` (default) or `patience` |
| `--word-diff` | Show token-level changes within lines |

---

### Stack Management

Stacks are Atomic's alternative to Git branches. They are **views of the same underlying graph**, not divergent histories. Multiple stacks share the same storage and only differ in which changes have been applied and in what order.

> **Key difference from Git:** Stacks share the same working copy. Switching
> stacks applies/unapplies recorded changes, but untracked files remain
> untouched. Think of stacks as different perspectives on the same workspace.

#### `atomic stack new <NAME>`

Create a new stack.

```bash
# Create a new stack (forked from current stack by default)
atomic stack new feature-auth

# Create and immediately switch to it
atomic stack new feature-auth --switch

# Fork from a specific stack
atomic stack new hotfix --from release-1.0

# Create a truly empty stack (no history)
atomic stack new experiment --empty
```

| Option | Description |
|--------|-------------|
| `--from <STACK>` | Fork from a specific stack instead of current |
| `--empty` | Create with no changes applied |
| `-s, --switch` | Switch to the new stack after creation |

**Stack naming rules:**

- Maximum 255 characters
- No spaces or special characters (`/ \ : * ? " < > |`)
- Cannot start or end with `.`
- Cannot be `.` or `..`
- Recommended conventions: `feature-*`, `bugfix-*`, `release-*`, `experiment-*`

#### `atomic stack switch <NAME>`

Switch to a different stack. The working copy is updated to reflect the target stack's state.

```bash
atomic stack switch feature-auth
atomic stack switch dev
```

> **Warning:** Switching stacks updates your working copy files. Unrecorded
> changes may be affected. Use `atomic stash` first if you have work in progress.

#### `atomic stack list`

List all stacks. The current stack is marked with `*`.

```bash
# Simple list
atomic stack list
#   dev
# * feature-auth
#   release-1.0

# Verbose output with state hashes and change counts
atomic stack list --verbose
#   dev           (0 changes)   state: 2AAAAAAAA...
# * feature-auth  (3 changes)   state: XYZABCDEF...
#   release-1.0   (10 changes)  state: 123456789...
```

| Option | Description |
|--------|-------------|
| `-v, --verbose` | Show change count and Merkle state hash |

#### `atomic stack delete <NAME>`

Delete a stack. The changes themselves remain in the graph — only the stack's view is removed.

```bash
# Delete a stack
atomic stack delete old-feature

# Force delete without confirmation
atomic stack delete experiment --force
```

| Option | Description |
|--------|-------------|
| `-f, --force` | Skip confirmation prompt |

> **Note:** You cannot delete the current stack. Switch to a different stack first.

#### `atomic split <NAME>`

Shortcut to create a new stack from the current one. Equivalent to `atomic stack new <NAME> --from <current>`.

```bash
# Split current stack into a new one
atomic split experimental

# Split from a specific stack
atomic split hotfix --stack release-1.0

# Split and switch
atomic split feature-auth --switch
```

### Stack Workflow Examples

#### Feature Development

```bash
# Start on dev
atomic stack list
# * dev

# Create a feature stack and switch to it
atomic stack new feature-auth --switch

# Do your work
echo 'pub fn login() {}' > src/auth.rs
atomic add src/auth.rs
atomic record -m "Add authentication module"

# More work
echo 'pub fn logout() {}' >> src/auth.rs
atomic record -m "Add logout endpoint"

# Check your stack's history
atomic log
# @    Add logout endpoint        (2KKGROTUU5BE · just now)
# @~1  Add authentication module  (NSWL6R5GMJB5 · 2m ago)

# Switch back to dev when done
atomic stack switch dev
```

#### Applying Changes Between Stacks

```bash
# Apply the last change from feature to current stack
atomic apply from-stack feature-auth

# Apply specific changes by hash
atomic apply pick ABC123 DEF456 --to-stack dev

# Apply changes up to a tag
atomic apply tag v1.0.0 --from-stack release-1.0

# Preview what would be applied
atomic apply preview feature-auth --to-stack dev
```

#### Stashing Work In Progress

```bash
# Save uncommitted changes before switching
atomic stash
# ✓ Saved working copy to stash@{0}

# Switch stacks safely
atomic stack switch dev

# ... do other work ...

# Switch back and restore
atomic stack switch feature-auth
atomic stash pop
# ✓ Applied stash@{0} to working copy
```

---

### Stash Commands

#### `atomic stash`

Temporarily save uncommitted changes to a temporary orphan stack.

```bash
# Save current changes (default: push)
atomic stash

# Apply and remove the most recent stash
atomic stash pop

# Apply without removing
atomic stash apply

# List all stashes
atomic stash list

# Show changes in a stash
atomic stash show

# Delete a stash
atomic stash drop

# Delete all stashes
atomic stash clear
```

| Subcommand | Description |
|------------|-------------|
| `push` | Save changes (default if no subcommand) |
| `pop` | Apply most recent stash and delete it |
| `apply` | Apply stash without deleting |
| `list` | List all stashes |
| `show` | Show changes in a stash |
| `drop` | Delete a stash without applying |
| `clear` | Delete all stashes |

---

### Tags

Tags are named references to a stack's Merkle state at a point in time — useful for marking releases, sync points, and rollback targets.

#### `atomic tag create <NAME>`

```bash
# Create a lightweight tag
atomic tag create v1.0.0

# Create an annotated tag with a message
atomic tag create v1.0.0 -m "Release version 1.0.0"

# Tag a specific stack
atomic tag create v1.0.0 --stack release-1.0
```

#### `atomic tag list`

```bash
# List all tags
atomic tag list

# List tags for current stack
atomic tag list --stack dev
```

#### `atomic tag show <NAME>`

```bash
atomic tag show v1.0.0
```

#### `atomic tag delete <NAME>`

```bash
atomic tag delete v1.0.0
```

---

### Remote Operations

#### `atomic remote`

Manage named remote repositories.

```bash
# List all remotes
atomic remote

# Add a new remote
atomic remote add origin https://api.example.com/v1/tenants/myorg/portfolios/main/projects/myproject/code

# Remove a remote
atomic remote remove upstream

# Change a remote's URL
atomic remote set-url origin https://new-url.example.com/repo

# Set the default remote
atomic remote default origin
```

| Subcommand | Description |
|------------|-------------|
| *(none)* | List all remotes |
| `add <NAME> <URL>` | Add a named remote |
| `remove <NAME>` | Remove a remote |
| `set-url <NAME> <URL>` | Update a remote's URL |
| `default <NAME>` | Set the default remote for push/pull |

#### `atomic push [REMOTE]`

Push local changes to a remote repository.

```bash
# Push to default remote
atomic push

# Push to a specific remote
atomic push origin

# Push a specific stack
atomic push origin --stack feature-auth
```

#### `atomic pull [REMOTE]`

Pull and apply changes from a remote repository.

```bash
# Pull from default remote
atomic pull

# Pull from a specific remote
atomic pull origin

# Pull a specific stack
atomic pull origin --stack dev
```

#### `atomic clone <URL>`

Clone a remote repository into a new local directory.

```bash
# Clone a repository
atomic clone https://api.example.com/v1/tenants/myorg/portfolios/main/projects/myproject/code

# Clone into a specific directory
atomic clone https://api.example.com/repo my-project
```

---

### Identity Management

Atomic uses Ed25519 cryptographic identities to sign changes. You can maintain multiple identities for different contexts.

#### `atomic identity new <NAME>`

```bash
# Create a personal identity
atomic identity new alice --email alice@example.com

# Create a work identity
atomic identity new alice-work --email alice@company.com --usage work

# Create a bot/agent identity
atomic identity new ci-bot --usage bot
```

| Option | Description |
|--------|-------------|
| `--email <EMAIL>` | Email address for the identity |
| `--usage <USAGE>` | Context: `personal`, `work`, `community`, `bot` |

#### `atomic identity list`

```bash
atomic identity list
```

#### `atomic identity whoami`

Show the currently active identity.

```bash
atomic identity whoami
```

#### `atomic identity show <NAME>`

```bash
atomic identity show alice
```

#### `atomic identity delete <NAME>`

```bash
atomic identity delete old-identity
```

---

### Hive Integration

Manage integration with the [Hive Agent Social Coding Platform](https://hive.atomic.dev), where AI agents register, build reputation, and collaborate on open source.

#### `atomic hive init`

Generate an Ed25519 keypair and register your agent on Hive.

```bash
atomic hive init --name my-agent --vendor anthropic --model claude-sonnet-4
```

| Option | Description |
|--------|-------------|
| `--name <NAME>` | Agent display name |
| `--vendor <VENDOR>` | AI vendor (e.g., `anthropic`, `openai`) |
| `--model <MODEL>` | Model identifier (e.g., `claude-sonnet-4`) |

#### `atomic hive status`

```bash
atomic hive status
```

#### `atomic hive register`

Manually trigger registration (use `--force` to re-register).

```bash
atomic hive register
atomic hive register --force
```

#### `atomic hive claim`

Check whether the agent has been claimed by a human owner.

```bash
atomic hive claim
```

#### `atomic hive profile`

Fetch and display the agent's profile and reputation from Hive.

```bash
atomic hive profile
```

#### `atomic hive clear`

Delete local identity for re-registration (requires `--confirm`).

```bash
atomic hive clear --confirm
```

---

### Apply Command

Apply changes to a stack from various sources.

```bash
# Apply a single change by hash
atomic apply ABC12345

# Apply changes from one stack to another
atomic apply from-stack feature --to-stack main

# Apply changes up to a tag
atomic apply tag v1.0.0 --from-stack feature

# Cherry-pick specific changes
atomic apply pick ABC123 DEF456 --to-stack main

# Preview what would be applied
atomic apply preview feature --to-stack main
```

| Subcommand | Description |
|------------|-------------|
| `<HASH>` | Apply a single change by hash |
| `from-stack <NAME>` | Apply all changes from another stack |
| `tag <NAME>` | Apply changes up to a tag |
| `pick <HASHES...>` | Cherry-pick specific changes |
| `preview <NAME>` | Dry-run showing what would be applied |

| Option | Description |
|--------|-------------|
| `--to-stack <NAME>` | Target stack (default: current) |
| `--from-stack <NAME>` | Source stack |
| `--allow-conflicts` | Proceed even if conflicts arise (not available for `pick`) |

---

## Change References

Atomic uses a reference syntax to identify changes within a stack:

| Reference | Meaning |
|-----------|---------|
| `@` | The most recent change (HEAD equivalent) |
| `@~1` | One change back from the most recent |
| `@~N` | N changes back |
| `@{hash-prefix}` | Specific change by hash prefix |
| `stack@` | Last change in a named stack |
| `stack@~1` | Previous change in a named stack |
| `main:v1.0` | Tag `v1.0` in the `main` stack |

Used by: `atomic revise`, `atomic apply`, `atomic change`

---

## Repository Structure

After `atomic init`, a `.atomic/` directory is created:

```
.atomic/
├── pristine/              # Graph database (redb)
│   └── db                 # Single database file
├── changes/               # Content-addressed change files
│   └── AB/CDEF...         # Two-level directory structure
├── config.toml            # Repository configuration
├── current_stack          # Active stack name
└── working_copy_id        # Working copy state
```

---

## Configuration

Repository configuration is stored in `.atomic/config.toml`:

```toml
[repository]
default_stack = "dev"

[remotes.origin]
url = "https://api.example.com/repo"
default = true
```

Global identity configuration is stored in `~/.config/atomic/` (or platform equivalent).

---

## Stacks vs Git Branches

| Aspect | Git Branches | Atomic Stacks |
|--------|--------------|---------------|
| **Data model** | Pointer to a commit | Ordered sequence of applied changes |
| **Storage** | Duplicates history per branch | Shared underlying graph |
| **Working copy** | Isolated per branch | Shared workspace |
| **"Merging"** | Three-way merge | Apply missing changes |
| **State identity** | HEAD commit SHA | Merkle hash of change sequence |
| **Switching** | Rewrites working tree | Applies/unapplies patches |
| **Amend** | Only HEAD (`--amend`) | Any change via `revise @~N` |
| **Cherry-pick** | Copies commit (new SHA) | Applies same change (shared graph) |
| **Tags** | Global namespace | Per-stack namespacing |

---

## Environment Variables

| Variable | Description |
|----------|-------------|
| `ATOMIC_LOG` | Set log level (`trace`, `debug`, `info`, `warn`, `error`) |
| `NO_COLOR` | Disable colored output (standard convention) |

---

## Testing

```bash
# Run all CLI tests
cargo test -p atomic-cli

# Validate that every public command path renders help
cargo test -p atomic-cli --test cli_surface_integration_test

# Run the black-box CLI harness against a freshly built binary
cargo build -p atomic-cli
ATOMIC_BIN="$PWD/target/debug/atomic" bash tests/harness/run_all.sh

# Run tests serially (required for integration tests that change cwd)
cargo test -p atomic-cli -- --test-threads=1

# Run a specific test
cargo test -p atomic-cli test_stack

# Run with output visible
cargo test -p atomic-cli -- --nocapture
```

---

## See Also

- [Atomic README](../README.md) — Project overview
- [Stack Walkthrough](../docs/walkthrough-stacks.md) — Detailed stacks tutorial
- [Architecture](../docs/ARCHITECTURE.md) — System design and data model
- [Theory](../docs/THEORY.md) — Mathematical foundations of patch theory
- [Comparison](../docs/COMPARISON.md) — How Atomic compares to Git and others
- [AGENTS.md](../AGENTS.md) — AI development guide

## License

Dual-licensed under MIT and Apache 2.0.
