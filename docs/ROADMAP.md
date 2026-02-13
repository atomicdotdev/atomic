# Atomic Development Roadmap

## Overview

This document outlines the phased development plan for Atomic VCS. The goal is to reach a functional MVP in **6-8 weeks**, with subsequent phases adding enterprise features.

---

## Phase 1: Foundation (Week 1-2)

### Goals
- Core data structures compiled and tested
- Basic storage layer operational
- Can create empty repository

### Deliverables

#### 1.1 Core Types (`atomic-core/src/types/`)
- [ ] `L64` - Little-endian u64 wrapper
- [ ] `NodeId` - Internal node identifier
- [ ] `ChangePosition` - Position within change content
- [ ] `Inode` - File system inode identifier
- [ ] `Hash` - Blake3 content hash (32 bytes)
- [ ] `Merkle` - Incremental state hash
- [ ] `Vertex<H>` - Graph vertex
- [ ] `Position<H>` - Byte position reference
- [ ] `EdgeFlags` - Edge type bitflags
- [ ] `Edge` / `SerializedEdge` - Graph edges
- [ ] Base32 encoding trait and implementations

#### 1.2 Change Structures (`atomic-core/src/change/`)
- [ ] `Change` - Complete change structure
- [ ] `HashedChange` - Hash-contributing portion
- [ ] `ChangeHeader` - Metadata (author, message, timestamp)
- [ ] `Author` - Author information
- [ ] `Hunk` enum - All hunk types
- [ ] `NewVertex` - Insert new content
- [ ] `EdgeMap` / `NewEdge` - Modify existing edges
- [ ] Serialization (bincode + zstd compression)
- [ ] Deserialization with validation

#### 1.3 Storage Layer (`atomic-core/src/pristine/`)
- [ ] `GraphTxnT` trait - Read-only graph operations
- [ ] `ChannelTxnT` trait - Channel operations
- [ ] `MutTxnT` trait - Mutable operations
- [ ] redb backend implementation
- [ ] Table definitions and key encoding

#### 1.4 Repository Structure (`atomic-repository/`)
- [ ] `Repository` struct
- [ ] `Repository::init()` - Create new repository
- [ ] `Repository::open()` - Open existing repository
- [ ] `.atomic/` directory layout
- [ ] Configuration file (`config.toml`)

### Milestone 1 Checkpoint
```bash
atomic init        # Creates .atomic/ directory
atomic status      # Shows "No changes recorded"
```

---

## Phase 2: Recording Changes (Week 2-3)

### Goals
- Can record changes to tracked files
- Diff algorithm produces correct hunks
- Changes serialize/deserialize correctly

### Deliverables

#### 2.1 Diff Engine (`atomic-core/src/diff/`)
- [ ] `Line` struct with content hash
- [ ] `DiffAlgorithm` enum (Myers, Patience)
- [ ] Myers diff implementation
- [ ] Patience diff implementation
- [ ] `DiffResult` / `DiffHunk` structures
- [ ] Line-to-graph mapping

#### 2.2 Recording (`atomic-core/src/record/`)
- [ ] `RecordBuilder` struct
- [ ] File change detection (mtime/size)
- [ ] Content-based change detection (hash)
- [ ] Diff to hunk conversion
- [ ] Context computation (up/down)
- [ ] Dependency tracking
- [ ] `record_file()` function
- [ ] `finish()` → `Change`

#### 2.3 Change Store (`atomic-core/src/changestore/`)
- [ ] Content-addressed storage layout
- [ ] `ChangeStore` trait
- [ ] File system implementation
- [ ] `save_change()` function
- [ ] `load_change()` function
- [ ] Hash verification on load

#### 2.4 CLI Commands (`atomic/src/commands/`)
- [ ] `add` - Track files
- [ ] `remove` - Untrack files
- [ ] `record` - Create a change
- [ ] `log` - Show change history
- [ ] `diff` - Show working copy changes

### Milestone 2 Checkpoint
```bash
atomic init
echo "Hello" > test.txt
atomic add test.txt
atomic record -m "Add test file"
atomic log              # Shows the recorded change
echo "World" >> test.txt
atomic diff             # Shows the modification
atomic record -m "Update test file"
```

---

## Phase 3: Applying & Output (Week 3-4)

### Goals
- Can apply changes to a channel
- Can output graph state to working copy
- Round-trip: record → apply → output produces original content

### Deliverables

#### 3.1 Apply Engine (`atomic-core/src/apply/`)
- [ ] `apply_change()` function
- [ ] `apply_hunk()` for each hunk type
- [ ] `apply_new_vertex()` - Insert vertices
- [ ] `apply_edge_map()` - Modify edges
- [ ] Dependency verification
- [ ] Merkle state update
- [ ] Graph invariant validation

#### 3.2 Output Engine (`atomic-core/src/output/`)
- [ ] `retrieve_file_graph()` - Get file's subgraph
- [ ] `compute_alive_vertices()` - Filter deleted
- [ ] `topological_sort()` - Order vertices
- [ ] `output_file()` - Write to working copy
- [ ] Conflict marker generation
- [ ] `output_repository()` - Full working copy

#### 3.3 Graph Traversal (`atomic-core/src/alive/`)
- [ ] Alive/dead vertex classification
- [ ] Reachability from ROOT
- [ ] Zombie detection
- [ ] Efficient inode-scoped traversal

#### 3.4 Additional CLI Commands
- [ ] `checkout` - Switch channels
- [ ] `reset` - Discard working copy changes
- [ ] `cat` - Output file at specific state

### Milestone 3 Checkpoint
```bash
atomic init
echo "Line 1" > test.txt
atomic add test.txt
atomic record -m "Add line 1"
echo "Line 2" >> test.txt
atomic record -m "Add line 2"
atomic reset --hard HEAD~1  # Go back one change
cat test.txt                  # Shows only "Line 1"
```

---

## Phase 4: Channels & Merge (Week 4-5)

### Goals
- Can create and switch channels (branches)
- Can merge channels
- Conflict detection and representation

### Deliverables

#### 4.1 Channel Management
- [ ] `Channel` struct
- [ ] `create_channel()` function
- [ ] `switch_channel()` function
- [ ] `delete_channel()` function
- [ ] `list_channels()` function
- [ ] Channel state persistence

#### 4.2 Merge Engine (`atomic-core/src/merge/`)
- [ ] `find_common_ancestor()` via Merkle states
- [ ] `changes_since()` - List changes between states
- [ ] `merge_channels()` function
- [ ] Order conflict detection
- [ ] Name conflict detection
- [ ] Zombie conflict detection
- [ ] Conflict-as-data representation

#### 4.3 Conflict Resolution
- [ ] Conflict markers in output
- [ ] `SolveOrderConflict` hunk
- [ ] `SolveNameConflict` hunk
- [ ] Interactive resolution UI

#### 4.4 CLI Commands
- [ ] `channel new <name>` - Create channel
- [ ] `channel switch <name>` - Switch to channel
- [ ] `channel delete <name>` - Delete channel
- [ ] `channel list` - List all channels
- [ ] `merge <channel>` - Merge into current

### Milestone 4 Checkpoint
```bash
atomic init
echo "Base" > test.txt
atomic add test.txt
atomic record -m "Base"
atomic channel new feature
echo "Feature line" >> test.txt
atomic record -m "Add feature"
atomic channel switch main
echo "Main line" >> test.txt
atomic record -m "Add main line"
atomic merge feature        # Creates merge or shows conflict
```

---

## Phase 5: Remote Operations (Week 5-6)

### Goals
- Can push/pull changes via atomic-api (HTTP)
- Efficient sync via Merkle comparison
- Clone repositories from remote

### Note on Architecture

Remote operations use the **atomic-api** server (`atomic-enterprise/atomic-api`) which implements the Atomic protocol over HTTP. The CLI only needs an HTTP client to communicate with this API.

### Deliverables

#### 5.1 HTTP Client (`atomic/src/remote/`)
- [ ] HTTP client for atomic-api
- [ ] Request/response handling
- [ ] Authentication (Bearer token)
- [ ] Binary data handling for changes

#### 5.2 Sync Logic (`atomic/src/remote/sync.rs`)
- [ ] `push()` - Upload local changes to atomic-api
- [ ] `pull()` - Download remote changes from atomic-api
- [ ] `fetch()` - Download without applying
- [ ] Conflict detection on push
- [ ] Merkle state comparison

#### 5.3 CLI Commands
- [ ] `remote add <name> <url>` - Add remote
- [ ] `remote remove <name>` - Remove remote
- [ ] `remote list` - List remotes
- [ ] `push <remote>` - Push changes
- [ ] `pull <remote>` - Pull changes
- [ ] `clone <url>` - Clone repository

### Milestone 5 Checkpoint
```bash
# Machine A
atomic init
echo "Hello" > test.txt
atomic add test.txt
atomic record -m "Initial"
atomic remote add origin https://api.example.com/tenant/portfolio/project
atomic push origin

# Machine B
atomic clone https://api.example.com/tenant/portfolio/project
cat test.txt               # Shows "Hello"
echo "World" >> test.txt
atomic record -m "Update"
atomic push origin

# Machine A
atomic pull origin
cat test.txt               # Shows "Hello\nWorld"
```

---

## Phase 6: Polish & Performance (Week 6-8)

### Goals
- Production-ready performance
- Comprehensive test coverage
- Documentation complete

### Deliverables

#### 6.1 Performance Optimization
- [ ] Inode graph index (O(n) file traversal)
- [ ] Parallel diff computation
- [ ] Lazy change loading
- [ ] Connection pooling for remotes
- [ ] Progress indicators for long operations

#### 6.2 Testing
- [ ] Unit tests for all modules
- [ ] Integration tests for CLI
- [ ] Property-based tests (quickcheck)
- [ ] Fuzzing for parser/deserializer
- [ ] Benchmark suite

#### 6.3 User Experience
- [ ] Colored terminal output
- [ ] Progress bars for long operations
- [ ] Interactive prompts (record, merge)
- [ ] Pager integration for long output
- [ ] Shell completions (bash, zsh, fish)

#### 6.4 Documentation
- [ ] Man pages for all commands
- [ ] User guide
- [ ] API documentation
- [ ] Migration guide from Git

#### 6.5 Packaging
- [ ] Release binaries (Linux, macOS, Windows)
- [ ] Homebrew formula
- [ ] Cargo publish
- [ ] Docker image

---

## Future Phases

### Phase 7: Identity & Signing
- [ ] Ed25519 key generation
- [ ] Change signing
- [ ] Signature verification
- [ ] Key management (keyring integration)
- [ ] Web of trust

### Phase 8: Enterprise Features
- [ ] Large file support (LFS-like)
- [ ] Partial clone / sparse checkout
- [ ] Server-side hooks
- [ ] Access control integration
- [ ] Audit logging

### Phase 9: Advanced Features
- [ ] Patch dependencies visualization
- [ ] Semantic diff (tree-sitter integration)
- [ ] Blame / annotation
- [ ] Bisect
- [ ] Stash equivalent
- [ ] Worktrees

---

## Success Metrics

### MVP (End of Phase 5)
- [ ] Can init, record, push, pull, merge
- [ ] Handles 10,000+ changes without degradation
- [ ] Sub-second operations on typical files
- [ ] Zero data loss in all tested scenarios

### Production Ready (End of Phase 6)
- [ ] 90%+ test coverage
- [ ] Benchmarks show competitive performance with Git
- [ ] Documentation for all features
- [ ] Successfully used for Atomic's own development

---

## Risk Mitigation

| Risk | Mitigation |
|------|------------|
| Diff algorithm bugs | Property-based testing, comparison with known-good implementations |
| Storage corruption | Checksums on all data, write-ahead logging |
| Performance regression | Automated benchmarks in CI |
| Protocol incompatibility | Version negotiation, backward compatibility tests |
| Merge incorrectness | Extensive merge test suite with edge cases |

---

## Weekly Checkpoints

| Week | Focus | Deliverable |
|------|-------|-------------|
| 1 | Core types, storage | `atomic init` works |
| 2 | Diff, record | `atomic record` works |
| 3 | Apply, output | Round-trip works |
| 4 | Channels, merge | Basic merge works |
| 5 | Remote protocol | Push/pull works |
| 6 | HTTP/SSH transports | Clone works |
| 7 | Performance, testing | Benchmarks pass |
| 8 | Polish, docs | Release candidate |

---

## Getting Started

1. Read [ARCHITECTURE.md](./ARCHITECTURE.md) for system design
2. Read [THEORY.md](./THEORY.md) for mathematical foundations
3. Read [IMPLEMENTATION.md](./IMPLEMENTATION.md) for data structures
4. Start with Phase 1.1 (Core Types)
5. Write tests as you go
6. Commit frequently to track progress