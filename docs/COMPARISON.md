# Comparison: Original Atomic vs Atomic-Core

This document compares the original `atomic` (libatomic) implementation with the clean-room `atomic-core` rewrite to help maintain clarity about architectural differences.

## Overview

| Aspect | Original Atomic (libatomic) | Atomic-Core |
|--------|----------------------------|----------------------|
| Database | Sanakirja (custom) | redb (pure Rust) |
| Hash Type | Separate Hash + Merkle | Unified (Hash = Merkle alias) |
| View Naming | "Channel" | "Stack" |
| Hash Algorithm | Ed25519 curve points | Blake3 |
| Code Origin | Evolved from Pijul | Clean-room rewrite |

## Directory Structure Comparison

### Original Atomic (`atomic/libatomic/src/`)

```
libatomic/src/
├── alive/                 # Graph traversal, liveness
│   └── output.rs
├── apply/                 # Change application
├── attribution/           # AI attribution tracking
├── bundle/                # Bundle format
├── change/                # Change representation
│   ├── text_changes.rs
│   └── text_changes_old.rs
├── changestore/           # Change file storage
├── diff/                  # Diff algorithms
├── output/                # Working copy output
├── pristine/              # Storage layer (Sanakirja)
│   ├── block.rs
│   ├── edge.rs
│   ├── hash.rs
│   ├── inode.rs
│   ├── inode_metadata.rs
│   ├── inode_vertex.rs
│   ├── merkle.rs
│   ├── mod.rs
│   ├── node_id.rs
│   ├── patch_id.rs
│   ├── path_id.rs
│   ├── sanakirja.rs       # Sanakirja backend
│   ├── tag.rs
│   └── vertex.rs
├── tag/                   # Tag management
├── tests/
├── unrecord/              # Unrecord operations
├── working_copy/          # Working copy management
├── apply.rs
├── change.rs
├── fs.rs
├── key.rs
├── lib.rs
├── missing_context.rs
├── path.rs
├── record.rs
├── small_string.rs
├── tag.rs
├── text_encoding.rs
├── vector2.rs
└── vertex_buffer.rs
```

### Atomic-Core (`atomic/atomic-core/src/`)

```
atomic-core/src/
├── types/                 # Core data types (separated)
│   ├── mod.rs
│   ├── node_id.rs         # L64, NodeId, ChangePosition, Inode
│   ├── hash.rs            # Unified Hash/Merkle
│   ├── vertex.rs          # Vertex<H>
│   ├── position.rs        # Position<H>
│   └── edge.rs            # EdgeFlags, Edge, SerializedEdge
├── pristine/              # Storage layer (redb)
│   ├── mod.rs             # Module docs & exports
│   ├── error.rs           # PristineError types
│   ├── tables.rs          # Table definitions
│   ├── traits.rs          # GraphTxnT, StackTxnT, etc.
│   └── txn/               # Transaction implementations
│       ├── mod.rs
│       ├── helpers.rs     # Serialization helpers
│       ├── pristine.rs    # Database handle
│       ├── read.rs        # ReadTxn
│       └── write.rs       # WriteTxn
└── lib.rs
```

## Key Terminology Changes

| Original Atomic | Atomic-Core | Rationale |
|-----------------|-------------|-----------|
| `Channel` | `Stack` | "Stack" better conveys views-not-forks concept |
| `ChannelRef` | `StackState` | Simplified, no Arc wrapper needed yet |
| `ChannelTxnT` | `StackTxnT` | Consistent naming |
| `ChannelMutTxnT` | (merged into `MutTxnT`) | Simplified trait hierarchy |

## Database Backend Comparison

### Original: Sanakirja

```rust
// Sanakirja-specific types and macros
sanakirja_table!(
    CHANNELS: (&str, SmallStr) => (Channel)
);

// Custom B-tree implementation
pub struct MutTxn<T> {
    txn: ::sanakirja::MutTxn<T, ...>,
    ...
}
```

**Characteristics:**
- Custom copy-on-write database
- Complex macro-generated code
- Tight coupling with Pijul history
- Difficult to understand/maintain

### Atomic-Core: redb

```rust
// Simple table definitions
pub const STACKS: TableDefinition<&str, &[u8]> = 
    TableDefinition::new("stacks");

// Standard redb transactions
pub struct WriteTxn<'a> {
    txn: WriteTransaction,
    ...
}
```

**Characteristics:**
- Pure Rust, well-documented
- Simple, explicit table definitions
- Standard key-value semantics
- Easy to understand/maintain

## Hash Type Comparison

### Original Atomic

```rust
// hash.rs - Hash as alias for Merkle (Ed25519 points)
pub type Hash = Merkle;

// merkle.rs - Ed25519 curve points
pub enum Merkle {
    Ed25519(EdwardsPoint),
}

// Using curve25519-dalek for cryptographic operations
let scalar = Scalar::from_bytes_mod_order(scalar_bytes);
Merkle::Ed25519(ED25519_BASEPOINT_POINT * scalar)
```

**Rationale:** Enables cryptographic proofs for AI attestations

### Atomic-Core

```rust
// hash.rs - Unified type using Blake3
pub struct Merkle(pub [u8; 32]);
pub type Hash = Merkle;

// Simple Blake3 hashing
pub fn of(data: &[u8]) -> Self {
    Merkle(blake3::hash(data).into())
}
```

**Rationale:** Simpler, faster, can upgrade to Ed25519 later if needed

## Trait Hierarchy Comparison

### Original Atomic

```rust
// Complex trait hierarchy with many associated types
pub trait ChannelTxnT: GraphTxnT {
    type Channel: Borrow<Channel> + Clone;
    fn name<'a>(&self, channel: &'a Self::Channel) -> &'a str;
    fn graph<'a>(&self, channel: &'a Self::Channel) -> &'a Self::Graph;
    ...
}

pub trait ChannelMutTxnT: ChannelTxnT + GraphMutTxnT + TreeMutTxnT {
    fn apply_change(...) -> ...;
    ...
}
```

### Atomic-Core

```rust
// Simplified trait hierarchy
pub trait GraphTxnT {
    type Adj: Iterator<Item = Result<SerializedEdge, PristineError>>;
    fn get_external(&self, id: NodeId) -> Result<Option<Hash>, PristineError>;
    ...
}

pub trait StackTxnT: GraphTxnT { ... }
pub trait TreeTxnT: GraphTxnT { ... }
pub trait MutTxnT: StackTxnT + TreeTxnT { ... }
```

## Table Naming Comparison

| Original Atomic | Atomic-Core | Notes |
|-----------------|-------------|-------|
| `CHANNELS` | `STACKS` | Renamed concept |
| `CHANNEL_CHANGES` | `STACK_CHANGES` | Renamed concept |
| `REVCHANNELCHANGES` | `REV_STACK_CHANGES` | Cleaner naming |
| `graph` | `GRAPH` | Consistent SCREAMING_CASE |
| `inodes` | `INODES` | Consistent SCREAMING_CASE |
| `tree` | `TREE` | Consistent SCREAMING_CASE |
| `internal` | `INTERNAL` | Consistent SCREAMING_CASE |
| `external` | `EXTERNAL` | Consistent SCREAMING_CASE |
| (various) | `INODE_GRAPH` | Secondary index for O(n) file traversal |

## Code Organization Philosophy

### Original Atomic
- Types scattered across `pristine/` subdirectory
- Database code mixed with type definitions
- Macro-heavy for table generation
- Complex lifetimes and associated types

### Atomic-Core
- Types in dedicated `types/` module
- Storage in dedicated `pristine/` module
- Explicit code over macros
- Simplified lifetimes (collect iterators to avoid issues)

## Implementation Status

### What Atomic-Core Has (Phase 1-2 Complete)

| Feature | Status | Tests |
|---------|--------|-------|
| Core types | ✅ | 87 tests |
| Hash/Merkle unified | ✅ | Included |
| Pristine storage | ✅ | 92 tests |
| Transaction traits | ✅ | Included |
| redb integration | ✅ | Included |
| Inode graph index | ✅ | 18 tests |

### What Atomic-Core Needs (Phase 3+)

| Feature | Original Location | Priority |
|---------|------------------|----------|
| Change representation | `change/` | Phase 3 |
| Diff algorithms | `diff/` | Phase 4 |
| Record changes | `record.rs` | Phase 5 |
| Apply changes | `apply/`, `apply.rs` | Phase 5 |
| Working copy output | `output/`, `working_copy/` | Phase 6 |
| Tags | `tag/` | Phase 7 |
| Bundles | `bundle/` | Phase 8 |
| Unrecord | `unrecord/` | Phase 8 |

## Migration Path

When porting features from original Atomic to Atomic-Core:

1. **Read the original code** to understand the algorithm
2. **Ignore Sanakirja specifics** - translate to redb patterns
3. **Use Stack instead of Channel** - rename consistently
4. **Simplify traits** - avoid unnecessary associated types
5. **Use Blake3** - not Ed25519 curve points (for now)
6. **Write tests first** - TDD approach
7. **Document thoroughly** - better than original

## Key Files to Reference

When implementing new features, these original files are most relevant:

| Feature | Original File(s) | Notes |
|---------|-----------------|-------|
| Change format | `change.rs`, `change/*.rs` | Core patch structure |
| Diff | `diff/mod.rs` | Myers/Patience algorithms |
| Record | `record.rs` | Creating changes from diffs |
| Apply | `apply.rs`, `apply/*.rs` | Applying changes to graph |
| Output | `output/*.rs` | Reconstructing files |
| Graph traversal | `alive/*.rs` | Alive/dead classification |
| File system | `fs.rs`, `working_copy/*.rs` | Working copy management |

## Why Clean-Room?

1. **Licensing clarity** - No Pijul GPL code
2. **Simpler codebase** - Remove historical complexity
3. **Better documentation** - Document as we build
4. **Modern patterns** - Use redb, avoid custom DB
5. **Maintainability** - Easier to understand and modify
6. **Test-driven** - Comprehensive test coverage from start