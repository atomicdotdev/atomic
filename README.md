# Atomic

A mathematically sound distributed version control system — built for the age of AI.

## Why Atomic?

Git was designed for humans writing code in text editors. It tracks lines in files. That worked for 20 years.

Now AI agents write most of the code. They work in sessions and turns, use multiple models, cost money per token, and produce changes that need to be audited, attributed, and explained. Git doesn't know about any of this — you're bolting on external tools to answer basic questions like "which model wrote this?" or "how much did this session cost?"

Atomic is version control that understands AI-assisted development from the ground up.

## Atomic vs Git for Agentic Workflows

| | Git | Atomic |
|---|---|---|
| **Change model** | Line-based diffs | Patch theory — composable operations on a DAG |
| **AI attribution** | None — commits say "Co-authored-by" in prose | Every change records model, provider, session, tokens, and cost |
| **Provenance** | Not tracked | Causal decision graph: goal → exploration → commitment → verification |
| **Attestation** | Not tracked | Session-level audit nodes with cost, token breakdown, and change coverage |
| **Turn recording** | Manual commits or wrapper scripts | Automatic — each agent turn is a change with full metadata |
| **Agent isolation** | Branches diverge and need merging | Stacks are views of the same graph — isolated agent work, zero-orphan cleanup |
| **Conflict granularity** | Whole lines | Token-level — two agents editing different tokens on the same line don't conflict |
| **Identity** | Name + email string | Ed25519 cryptographic identity with agent delegation scopes |
| **Merge correctness** | Heuristic 3-way merge | Mathematical — commutative when changes are independent, precise when they're not |
| **Rename tracking** | Heuristic similarity matching | Structural — inodes survive renames across the entire history |

## How It Works

### Automatic Turn Recording

Enable agent hooks once. Every turn is recorded automatically.

```bash
# Enable for your agent (auto-detects Claude Code, Gemini CLI, OpenCode)
atomic agent enable

# That's it. Every agent turn now produces an Atomic change with:
#   - Model and provider identity
#   - Session and turn number
#   - Token usage and cost
#   - Cryptographic signature
#   - Provenance graph (why the agent made each decision)
```

### Provenance Graphs

Every agent session builds a causal decision DAG. Not just *what* changed, but *why*:

```
Goal: "Fix the authentication bug"
  ├── Exploration: read src/auth.rs
  ├── Exploration: grep "verify_token"
  ├── Commitment: edit src/auth.rs (fix token validation)
  ├── Verification: bash "cargo test"
  └── PatchProposal: Change XMJZ3IPF (2 files)
```

These graphs are content-addressed, stored alongside changes, and pushed to remotes. Your team can review not just the code, but the agent's reasoning.

### Attestations

When a session ends, Atomic creates an attestation — a graph-level audit node covering the session:

```bash
$ atomic agent attest

  XMJZ3IPF OpenCode · claude-sonnet-4-5 · 12.4k tokens · 3m 42s · 2 changes
  R3KQP7YN Claude Code · claude-sonnet-4-5 · 8.1k tokens · 1m 15s · 1 change

──────────────────────────────────────────
Total: $0.12 · 3 changes covered · 20.5k tokens
```

### Stacks, Not Branches

Agents work on isolated stacks — lightweight views of the same underlying graph. No branch divergence, no merge commits, no orphaned history.

```bash
# Agent session creates an isolated stack automatically
# Stack: agent-ses_3781fc7a6ffet5c6r1ILy1BEbv (Local, parent: dev)

# When done, apply changes to the parent stack
atomic apply @~1 --to dev

# Delete the agent stack — cascade-deletes its edges, zero orphans
atomic stack delete agent-ses_3781fc7a6ffet5c6r1ILy1BEbv
```

### Token-Level Diff

Atomic's CRDT semantic layer (Trunk → Branch → Leaf) tracks changes at the token level. Two agents editing different words on the same line produce independent patches that merge cleanly — Git would flag this as a conflict.

## Quick Start

```bash
# Initialize a new repository
atomic init

# Add files and record a change
atomic add src/
atomic record -m "Initial commit"

# Enable AI agent hooks
atomic agent enable

# Create a stack
atomic stack new feature-x

# Push to a remote
atomic push origin
```

## Key Concepts

### Patch Theory

Changes are composable, commutative operations on a directed graph:

- Merging `A` then `B` gives the same result as merging `B` then `A` (when no conflicts)
- Every change has a well-defined inverse
- Conflicts are represented as data, not failures

### The Repository Graph

Files are directed acyclic graphs (DAGs) where vertices hold content chunks, edges define ordering, and changes are transformations that add or remove vertices and edges. This is not a commit graph — it's the actual file structure.

### Dual-Layer Architecture

Every change stores two parallel representations:

- **Graph layer** — Content-addressed nodes and edges for mathematically sound merging
- **Semantic layer** — Files (Trunk), Lines (Branch), Tokens (Leaf) for human-readable diffs and token-level blame

## CLI Reference

### Core Commands

| Command | Description |
|---------|-------------|
| `atomic init` | Initialize a new repository |
| `atomic add <paths>` | Add files to tracking |
| `atomic remove <paths>` | Remove files from tracking |
| `atomic move <src> <dst>` | Move or rename tracked files |
| `atomic status` | Show working copy status |
| `atomic diff` | Show differences in working copy |
| `atomic record -m "msg"` | Record changes to the repository |
| `atomic revise` | Modify a change in-place |
| `atomic log` | Show change history |
| `atomic change [hash]` | Show details for a specific change |

### Stack Commands

| Command | Description |
|---------|-------------|
| `atomic stack new <name>` | Create a new stack |
| `atomic stack switch <name>` | Switch to a stack |
| `atomic stack list` | List all stacks |
| `atomic stack delete <name>` | Delete a stack |
| `atomic split <name>` | Create a new stack from the current one |
| `atomic stash` | Temporarily save uncommitted changes |

### Remote Commands

| Command | Description |
|---------|-------------|
| `atomic push [remote]` | Push changes to a remote |
| `atomic pull [remote]` | Pull changes from a remote |
| `atomic clone <url>` | Clone a remote repository |
| `atomic remote add <name> <url>` | Add a named remote |
| `atomic remote remove <name>` | Remove a named remote |
| `atomic remote set-url <name> <url>` | Change a remote's URL |
| `atomic remote default <name>` | Set the default remote |

### Tag Commands

| Command | Description |
|---------|-------------|
| `atomic tag create <name>` | Create a named state snapshot |
| `atomic tag list` | List all tags |
| `atomic tag show <name>` | Show tag details |
| `atomic tag delete <name>` | Delete a tag |

### Identity Commands

| Command | Description |
|---------|-------------|
| `atomic identity new <name>` | Create a new Ed25519 identity |
| `atomic identity list` | List all identities |
| `atomic identity whoami` | Show the current default identity |
| `atomic identity show <name>` | Show identity details |
| `atomic identity delete <name>` | Delete an identity |

### Agent Commands

| Command | Description |
|---------|-------------|
| `atomic agent enable` | Install agent hooks (auto-detect or `--agent claude-code`) |
| `atomic agent disable` | Remove agent hooks |
| `atomic agent status` | Show active sessions and hook status |
| `atomic agent explain <id>` | Generate AI reasoning summary for a session |
| `atomic agent attest` | List and inspect attestations |

#### Supported Agents

| Agent | Config | Hook System |
|-------|--------|-------------|
| Claude Code | `.claude/settings.json` | Native hooks |
| Gemini CLI | `.gemini/settings.json` | Native hooks |
| OpenCode | `.opencode/plugins/atomic/` | Plugin-based |

## Documentation

Full documentation at **[docs.atomic.dev](https://docs.atomic.dev/)**.

- [Getting Started](https://docs.atomic.dev/getting-started/installation) — Installation and first repository
- [Concepts](https://docs.atomic.dev/concepts/the-lego-story) — How Atomic works under the hood
- [Agent Integration](https://docs.atomic.dev/agents/overview) — Setting up AI agent hooks
- [Command Reference](https://docs.atomic.dev/commands/overview) — Complete CLI reference
- [AGENTS.md](AGENTS.md) — Development guide for contributors and AI agents

## Building

```bash
# Build all crates
cargo build --release

# Run tests
cargo test

# Install CLI
cargo install --path atomic-cli

# Verify installation
atomic --version
```

## Project Structure

```
atomic/
├── atomic-cli/               # CLI application
├── atomic-core/              # Core VCS engine (types, pristine, change, diff, CRDT)
├── atomic-agent/             # AI agent integration (hooks, turns, provenance)
├── atomic-config/            # Configuration management
├── atomic-identity/          # User identity & Ed25519 signing
└── atomic-repository/        # High-level repository operations
```

### Related Projects

| Project | Location | Description |
|---------|----------|-------------|
| **atomic-api** | `atomic-enterprise/atomic-api` | HTTP API for remote operations and multi-tenant hosting |
| **atomic-remote-client** | `atomic-enterprise/atomic-remote` | HTTP client library for push/pull/clone |
| **atomic-docs** | `atomic-docs/` | Documentation site ([docs.atomic.dev](https://docs.atomic.dev/)) |

## Design Principles

1. **Mathematical Soundness** — Operations are well-defined transformations with provable properties
2. **AI-Native** — Provenance, attestation, and agent identity are core concepts, not afterthoughts
3. **Correctness Over Speed** — Validate invariants aggressively, prefer clarity over micro-optimizations
4. **Efficiency** — O(n) file operations via inode indexing, not O(N) repository-wide scans
5. **Cryptographic Identity** — Every change is attributable via Ed25519 signatures

## Why "Atomic"?

Because version control should have **no doubt** about what changed, when, why, and who — human or machine. Every change is a well-defined, composable transformation. Atomic in the mathematical sense.

## License

Copyright 2025-2026 Atomic Software, Co.

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for the full license text.

## Links

- **Website**: [atomic.dev](https://atomic.dev)
- **Documentation**: [docs.atomic.dev](https://docs.atomic.dev)
- **GitHub**: [github.com/atomicdotdev/atomic](https://github.com/atomicdotdev/atomic)

## Acknowledgments

Atomic builds on decades of research in patch theory, drawing inspiration from academic work on categorical semantics of version control and practical implementations that explored these ideas.