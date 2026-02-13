# Atomic

A mathematically sound distributed version control system.

## Overview

Atomic is a next-generation VCS built on **patch theory** — representing changes as composable, commutative operations on a directed graph. Unlike line-based systems (Git, Mercurial), Atomic tracks the semantic structure of changes, enabling:

- **Conflict-free merges** when changes are truly independent
- **Precise conflict detection** when real conflicts exist
- **Accurate rename tracking** across the entire history
- **Cherry-picking and reverting** that actually work
- **AI attribution & provenance** — every line traces back to an agent, model, and context

## Status

🚧 **Under Development** — Not yet ready for production use.

## Quick Start

```bash
# Initialize a new repository
atomic init

# Add files and record a change
atomic add src/
atomic record -m "Initial commit"

# Create a stack (view of graph)
atomic stack new feature-x

# Push to a remote
atomic push origin
```

## Key Concepts

### Patch Theory

Traditional VCS systems store snapshots or line-based diffs. Atomic stores **patches** — well-defined transformations on a graph structure. This mathematical foundation means:

- Merging `A` then `B` gives the same result as merging `B` then `A` (when no conflicts)
- Every change has a well-defined inverse
- Conflicts are represented as data, not failures

### The Repository Graph

Files are represented as directed acyclic graphs (DAGs) where:
- **Vertices** = chunks of content (typically lines)
- **Edges** = ordering relationships between chunks
- **Changes** = transformations that add/remove vertices and edges

### Stacks

Stacks are named views of the repository at a particular state. Multiple stacks can share the same underlying storage, differing only in which changes have been applied. Stacks are **not** branches — they are perspectives on the same graph.

### Dual-Layer Diff

Every change stores two parallel representations:

- **Graph layer** — Content-addressed nodes and edges for mathematically sound merging
- **Semantic layer** — Files (Trunk), Lines (Branch), Tokens (Leaf) for human-readable diffs and token-level blame

This dual-layer architecture means Atomic can merge at the token level — resolving "conflicts" that Git would flag when two developers edit the same line but different tokens.

### AI Agent Integration

Atomic natively supports AI coding agents through the `atomic agent` command. When enabled, every agent turn is automatically recorded as an Atomic change with full provenance:

- **Model & provider** — Which AI and which model generated the change
- **Token usage & cost** — Resource consumption per turn
- **Session tracking** — Turn numbers, timing, and conversation context
- **Cryptographic identity** — Every agent change is attributable via Ed25519 signatures

Supported agents: Claude Code, Gemini CLI, OpenCode.

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

Manage AI agent integration for turn-level recording.

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

Full documentation is available at [docs.atomic.dev](https://docs.atomic.dev/).

- [Getting Started](https://docs.atomic.dev/getting-started/installation) — Installation and first repository
- [Concepts](https://docs.atomic.dev/concepts/the-lego-story) — How Atomic works under the hood
- [Command Reference](https://docs.atomic.dev/commands/overview) — Complete CLI reference
- [AGENTS.md](AGENTS.md) — Comprehensive development guide for AI agents

## Project Structure

```
atomic/
├── atomic-cli/               # CLI application
│   └── src/
│       ├── commands/
│       │   ├── init.rs        # Repository initialization
│       │   ├── status.rs      # Working copy status
│       │   ├── add.rs         # File tracking
│       │   ├── record.rs      # Change recording
│       │   ├── revise.rs      # In-place change modification
│       │   ├── diff.rs        # Working copy differences
│       │   ├── log.rs         # Change history
│       │   ├── stack/         # Stack management
│       │   ├── stash.rs       # Temporary change storage
│       │   ├── identity/      # Identity management
│       │   ├── agent/         # AI agent integration
│       │   ├── push/          # Remote push
│       │   ├── pull/          # Remote pull
│       │   ├── clone/         # Remote clone
│       │   ├── remote/        # Remote management
│       │   └── tag/           # Tag management
│       ├── error.rs           # CLI error types
│       └── output/            # Terminal output formatting
├── atomic-core/              # Core VCS engine
│   ├── change/                # Change representation & provenance
│   ├── diff/                  # Diff algorithms (Myers, Patience, word-level)
│   ├── crdt/                  # Hierarchical CRDT (Trunk → Branch → Leaf)
│   ├── pristine/              # Storage layer (redb)
│   ├── record/                # Change recording workflow
│   ├── apply/                 # Change application
│   └── output/                # Working copy output
├── atomic-agent/             # AI agent integration
│   ├── hooks/                 # Agent adapters (Claude Code, Gemini CLI, OpenCode)
│   ├── turn/                  # Turn orchestrator & state machine
│   ├── watcher/               # File change detection
│   ├── record.rs              # Turn recording workflow
│   ├── envelope.rs            # Session metadata encoding
│   ├── identity.rs            # Agent identity resolution
│   ├── transcript.rs          # Transcript parsing & reasoning
│   └── learnings.rs           # Knowledge flywheel (CLAUDE.md, GEMINI.md, etc.)
├── atomic-config/            # Configuration management
├── atomic-identity/          # User identity & Ed25519 signing
│   ├── identity.rs            # Identity types & builder
│   ├── keypair.rs             # Ed25519 key generation & signing
│   ├── signing.rs             # Signature creation & verification
│   ├── delegation.rs          # Agent delegation support
│   ├── usage.rs               # Usage contexts (personal, work, bot)
│   └── store.rs               # Identity persistence
└── atomic-repository/        # High-level repository operations
    ├── repository.rs          # Main Repository struct
    ├── status.rs              # Working copy status
    ├── tracking.rs            # File tracking
    ├── history.rs             # Change history
    ├── apply.rs               # Change application
    ├── tags.rs                # Named state snapshots
    ├── unrecord.rs            # Change removal
    ├── archive.rs             # Repository export
    └── ignore.rs              # .atomicignore pattern matching
```

### Related Projects

| Project | Location | Description |
|---------|----------|-------------|
| **atomic-api** | `atomic-enterprise/atomic-api` | Rust/Axum HTTP API for remote push/pull/clone, multi-tenant repository hosting |
| **atomic-remote-client** | `atomic-enterprise/atomic-remote` | HTTP client library for remote operations |
| **atomic-docs** | `atomic-docs/` | Documentation site at [docs.atomic.dev](https://docs.atomic.dev/) |

## Design Principles

1. **Mathematical Soundness** — Operations are well-defined transformations with provable properties
2. **Efficiency** — O(n) file operations via smart indexing, not O(N) repository-wide scans
3. **Correctness Over Speed** — Validate invariants aggressively, prefer clarity over micro-optimizations
4. **Clean Separation** — Core engine has minimal dependencies, storage is abstracted
5. **Cryptographic Identity** — Every change is attributable via Ed25519 signatures

## Building

```bash
# Build all crates
cargo build --release

# Run tests
cargo test

# Run just CLI tests
cargo test -p atomic-cli

# Install CLI
cargo install --path atomic-cli

# Verify installation
atomic --version
atomic --help
```

## Test Coverage

| Crate | Tests | Description |
|-------|-------|-------------|
| atomic-core | 2,789 | Types, pristine, change, diff, record, apply, output, CRDT |
| atomic-agent | 691 | Hooks, events, turn orchestrator, recording, identity, learnings |
| atomic-repository | 497 | Repository, status, tracking, history, tags, apply, archive |
| atomic-identity | 79 | Identity, keypair, signing, delegation, store |
| **Total** | **4,600+** | All passing |

## Why "Atomic"?

Because version control should have **no doubt** about what changed, when, and why. Every change is a well-defined, composable transformation — atomic in the mathematical sense.

## License

Copyright 2025-2026 Atomic Software, Co.

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for the full license text.

## Links

- **Website**: [atomic.dev](https://atomic.dev)
- **Documentation**: [docs.atomic.dev](https://docs.atomic.dev)
- **GitHub**: [github.com/atomicdotdev/atomic](https://github.com/atomicdotdev/atomic)

## Acknowledgments

Atomic builds on decades of research in patch theory, drawing inspiration from academic work on categorical semantics of version control and practical implementations that explored these ideas.