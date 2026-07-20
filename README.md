# Atomic

A Semantic Change Graph (SCG) to track memory, intent, provenance, and change as a single unit of work.  This is the evolution of source code management in the era of agents and agentic coding.

## Why Atomic?

Git was designed for humans writing code in text editors. It tracks lines in files. That worked for 20 years.

Now AI agents write most of the code. They work in sessions and turns, use multiple models, cost money per token, and produce changes that need to be audited, attributed, and explained. Git doesn't know about any of this — you're bolting on external tools to answer basic questions like "which model wrote this?" or "how much did this session cost?"

## Atomic vs Git for Agentic Workflows

| | Git | Atomic |
|---|---|---|
| **Change model** | Line-based diffs | Patch theory — composable operations on a DAG |
| **AI attribution** | None — commits say "Co-authored-by" in prose | Every change records model, provider, session, tokens, and cost |
| **Provenance** | Not tracked | Causal decision graph: goal → exploration → commitment → verification |
| **Attestation** | Not tracked | Session-level audit nodes with cost, token breakdown, and change coverage |
| **Turn recording** | Manual commits or wrapper scripts | Automatic — each agent turn is a change with full metadata |
| **Agent isolation** | Branches diverge and need merging | Views are filtered perspectives of the same graph — isolated agent work, zero-orphan cleanup |
| **Conflict granularity** | Whole lines | Token-level — two agents editing different tokens on the same line don't conflict |
| **Identity** | Name + email string | Ed25519 cryptographic identity with agent delegation scopes |
| **Merge correctness** | Heuristic 3-way merge | Mathematical — commutative when changes are independent, precise when they're not |
| **Rename tracking** | Heuristic similarity matching | Structural — inodes survive renames across the entire history |

## How It Works

### Automatic Turn Recording

Enable agent hooks once. Every turn is recorded automatically.

```bash
# Enable for your agent (auto-detects Claude Code, Antigravity CLI, Gemini CLI, OpenCode)
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

### Views, Not Branches

Agents work on isolated views of the same underlying graph. No branch divergence, no merge commits, no orphaned history.

```bash
# Agent session creates an isolated view automatically
# View: agent-ses_3781fc7a6ffet5c6r1ILy1BEbv (Draft, parent: dev)

# When done, insert changes into the parent view
atomic insert @~1 --to dev

# Delete the agent view — cascade-deletes its edges, zero orphans
atomic view delete agent-ses_3781fc7a6ffet5c6r1ILy1BEbv
```

### Workspace Shelving

When you switch views, Atomic automatically isolates build artifacts per view — each view gets its own `node_modules/`, `target/`, etc. But tool configs like `.opencode/`, `.vscode/`, and `.idea/` are project-wide and should persist across all views.

Atomic handles this with `[workspace] expose`:

```toml
# .atomic/config.toml
[workspace]
expose = [".opencode", ".vscode", ".idea", ".claude", ".gemini", ".agents"]
```

- **`.atomicignore`** = "don't track" (same as `.gitignore`). By default, all ignored files are **shelved per-view** on switch.
- **`[workspace] expose`** = "persist across views". These paths are never shelved — they stay on disk through every view switch.

| File | `.atomicignore` | `expose` | On view switch |
|------|:-:|:-:|---|
| `.opencode/`, `.vscode/` | ✓ | ✓ | **Left alone** — persists across views |
| `node_modules/`, `target/` | ✓ | ✗ | **Shelved** per view (O(1) rename) |
| `src/main.rs` | ✗ | — | **Tracked** — materialized from the graph |

`atomic init` seeds a default `expose` list with common tool configs. Build artifacts in `.atomicignore` are shelved automatically — no per-language configuration needed.

You can also set `expose` globally in `~/.atomic/config.toml` so it applies to every repository on your machine:

```toml
# ~/.atomic/config.toml
[workspace]
expose = [".opencode", ".vscode", ".idea", ".claude", ".gemini", ".agents"]
```

Global and repo-local patterns are merged — a repo can add project-specific entries without repeating the global ones.

### Token-Level Diff

Atomic's CRDT semantic layer (Trunk → Branch → Leaf) tracks changes at the token level. Two agents editing different words on the same line produce independent patches that merge cleanly — Git would flag this as a conflict.

### Vault — Shared Project Brain

Every repository has a **vault** — a versioned knowledge store that persists what your team and your agents learn across sessions. Goals, decisions, architecture context, and working memory all live in `.vault/` as structured markdown with cryptographic provenance.

```bash
# Initialize the vault in your repo
atomic vault init

# Start a goal (a focused work session — human or agent)
atomic vault goal start --intent PIMO-3 --title "Fix token validation"

# Agents and humans accumulate tool results, decisions, and context
# When done, stop the goal — optionally promote it for team review
atomic vault goal stop --promote

# Add persistent project knowledge (conventions, architecture, etc.)
atomic vault memory add "Auth tokens use Ed25519 signatures, never HMAC"

# Track planned work items
atomic vault intent add --title "Migrate to async runtime" --priority high
```

The vault stores five kinds of content:

| Entry Type | What It Captures |
|------------|------------------|
| **Goals** | Work sessions — each tracks a developer/agent, linked intent, model, status, and tool results |
| **Intents** | Planned work items with priority, status, assignee, and labels |
| **Memory** | Persistent knowledge — architecture decisions, conventions, reference material |
| **Skills** | Reusable capability definitions for agents |
| **Scratch** | Temporary notes and working state |

Everything is content-addressed and version-controlled. When an agent starts a session, it reads the vault for context. When it finishes, its learnings go back into the vault for the next session — human or machine.

### Knowledge Graph — Code Intelligence at Scale

Atomic builds a **knowledge graph** from your entire codebase — source code content, file structure, tree-sitter entities (functions, classes, types), module hierarchy, change history, and their relationships. Search it, traverse it, visualize it, or let an LLM explore it with tools.

```bash
# Search the knowledge graph — unified structural + content search
atomic query search "replication"

# Search source code content (powered by syntext trigram index)
atomic query code "replication" -t cpp -g src/mongo/db/repl/

# List tree-sitter entities in a file (functions, classes, types)
atomic query entities src/mongo/db/repl/replication_coordinator_impl.cpp

# Explore relationships around a node (1-2 hops)
atomic query neighbors "module:src/mongo/db/repl"

# Visualize the graph — opens interactive D3 force-directed layout
atomic query graph "replication" -k 20 --depth 2
```

The KG contains seven node types connected by typed edges:

```
module:src/mongo/db/repl          ← directory-level grouping
  ├─ PART_OF ← file:replication_coordinator.h    ← tracked files
  │              ├─ DEFINES → entity:ReplicationCoordinator (class, L42-890)
  │              └─ INCLUDES → file:replication_process.h
  ├─ PART_OF ← file:bgsync.cpp
  │              └─ MODIFIES ← change:R4YQUAS2 ("fix replication lag")
  │                              ├─ AUTHORED_BY → identity:alice
  │                              └─ DEPENDS_ON → change:XMJZ3IPF
  └─ PART_OF → module:src/mongo/db  (parent module)
```

Search is powered by two indexes that work together:
- **KG FTS** — searches node labels, summaries, and metadata (structural search)
- **syntext** — trigram-indexed content search across all source files, ~20x faster than grep

Results are ranked: `src/` files above tests, modules above flat files, content match counts as a signal. A bounded min-heap ensures the top N results surface from thousands of matches without sorting the full set.

For agents (Claude Code, Gemini, etc.), the CLI commands ARE the tools. An agent calls `atomic query search`, `atomic query neighbors`, `atomic query entities`, and its own `read_file` — the same path a human follows. No grep, no find, no LLM-calling-LLM overhead.

For humans on the terminal, `ask` provides an agentic loop with tool use:

```bash
# LLM explores the KG with tools and answers your question
atomic query ask "who fixed the auth bug?"
```

The `ask` command gives the LLM six tools (`kg_search`, `kg_neighbors`, `read_file`, `list_entities`, `code_search`, `vault_read`) and lets it decide what to look up. Provider-agnostic — works with Anthropic or OpenAI. Set `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` to enable.

No API key required for `search`, `code`, `entities`, `neighbors`, `graph`, or `enrich`.

## Quick Start

```bash
# Initialize a new repository
atomic init

# Add files and record a change
atomic add src/
atomic record -m "Initial commit"

# Enable AI agent hooks
atomic agent enable

# Create a view
atomic view create feature-x

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

### Query Commands

| Command | Description |
|---------|-------------|
| `atomic query search <query>` | Search the knowledge graph (structural + content) |
| `atomic query code <pattern>` | Search source code content (trigram-indexed, `-t` for file type, `-g` for path filter) |
| `atomic query entities <path>` | List tree-sitter entities in a file (functions, classes, types with line ranges) |
| `atomic query neighbors <id>` | Explore the graph around a node (DEFINES, MODIFIES, PART_OF, INCLUDES edges) |
| `atomic query graph <query>` | Visualize the knowledge graph as interactive HTML (D3 force-directed, `-k` seeds, `--depth`, `--kinds`) |
| `atomic query ask <question>` | Natural-language Q&A with agentic tool loop (requires API key) |
| `atomic query enrich` | Build/rebuild KG from VCS data + content index (modules, entities, includes) |
| `atomic query index` | Build/update the content search index (`--rebuild` for full, `--stats` for info) |
| `atomic query reindex` | Rebuild the KG index from vault entries |

### View Commands

| Command | Description |
|---------|-------------|
| `atomic view create <name>` | Create a new view (`--draft` for isolated, `--parent` to set parent) |
| `atomic view switch <name>` | Switch to a view |
| `atomic view list` | List all views (`--verbose` for scope and parent) |
| `atomic view delete <name>` | Delete a view |
| `atomic split <name>` | Create a new view from the current one |
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

### Vault Commands

| Command | Description |
|---------|-------------|
| `atomic vault init` | Initialize the vault in an existing repository |
| `atomic vault goal start` | Start a new goal (work session) |
| `atomic vault goal stop` | End the current goal (`--promote` for team review) |
| `atomic vault goal list` | List goals |
| `atomic vault intent add` | Create a planned work item |
| `atomic vault intent list` | List intents |
| `atomic vault memory add` | Add persistent project knowledge |
| `atomic vault memory list` | List memory entries |
| `atomic vault list` | List all vault entries (filter with `--type`) |
| `atomic vault show <path>` | Print a vault entry's content |
| `atomic vault sync` | Sync vault entries between disk and database |

#### Supported Agents

| Agent | Config | Hook System |
|-------|--------|-------------|
| Claude Code | `.claude/settings.json` | Native hooks |
| Antigravity CLI (`agy`) | `~/.gemini/config/plugins/atomic/` | Plugin hooks |
| Gemini CLI (deprecated) | `.gemini/settings.json` | Native hooks |
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

## Atomic + smolvm: Concurrent Agent Sandboxes

When several agents work a repository at once, each needs an isolated build
surface (no `node_modules` / `target` collisions) while still sharing one
history. Atomic does this with **copy-on-write working trees over a single
canonical graph** — no per-agent forks, no virtualized I/O.

- **The working tree is cloned; the graph is not.** A sandbox is a reflink
  (copy-on-write) clone of the working tree — APFS `clonefile`, Btrfs/XFS
  `FICLONE`, ReFS block-clone — done in-process via the `reflink-copy` crate.
  Five agents off a 200 MB base cost a few MB and run at bare-metal filesystem
  speed. There is exactly **one** pristine; a sandbox carries a small
  `.atomic-sandbox` pointer, so `status`, `record`, `diff`, and `vault query`
  all work inside it and land in the canonical graph.
- **No FUSE, no virtio, no VM in the hot path.** Filesystem-protocol
  virtualization (virtio-fs) is fatal for the metadata-heavy workloads agents
  run (git, cargo, npm). Copy-on-write on the real filesystem needs none of it.

```bash
# Give each agent a private, copy-on-write working tree
atomic sandbox create agent-1 --from dev      # forks a per-agent draft view
atomic sandbox create agent-2 --from dev      # isolated artifacts, shared graph
```

### Shipping a sandbox: `stage` and `seal`

A sandbox can be packaged as a **standard OCI image** (consumable by smolvm,
podman, containerd — built in-process, no shelling out) for two purposes:

| Command | Shape | Purpose |
|---------|-------|---------|
| `atomic sandbox stage` | layered: shared base + thin delta | inner-loop CI / circuit-breaker runs — base is pulled once and cached, only the delta ships each iteration |
| `atomic sandbox seal` | flattened, single self-contained layer | a deployable runtime — "run this exact version" anywhere |

```bash
# Inner-loop CI artifact: base layer (dev) + delta layer (only what changed)
atomic sandbox stage agent-1 --base dev -o ./ci-image

# Deployable runtime image of the full merged state
atomic sandbox seal dev -o ./runtime --entrypoint /app/run --env PORT=8080
```

Both embed provenance (the view names and Merkle states) in the image
annotations, so a shipped image traces back to exact graph state. The runtime
is [smolvm](https://github.com/binsquare/smolvm) — lightweight microVMs that
boot OCI images in under 200 ms — making these images the unit of work for
shared-infrastructure CI and circuit-breaker workflows.

See [docs/CONCURRENT-AGENT-SANDBOXES.md](docs/CONCURRENT-AGENT-SANDBOXES.md) for
the full design.

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

Atomic builds on decades of research in patch theory, drawing inspiration from academic work on categorical semantics of version control and practical implementations that explored these ideas.  Thanks to the team at https://pijul.org for originating that work.
