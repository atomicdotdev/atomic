# Atomic Documentation

Welcome to the Atomic VCS documentation. This directory contains comprehensive documentation for understanding, developing, and contributing to the project.

## Documentation Overview

| Document | Description | Audience |
|----------|-------------|----------|
| [CLI Reference](../atomic-cli/README.md) | Complete command reference for the `atomic` CLI | Users, Everyone |
| [Stack Walkthrough](./walkthrough-stacks.md) | Hands-on tutorial for stacks, apply, and stash | Users, Contributors |
| [Attestation Design](./attestation-design.md) | Graph-level audit nodes for AI cost, tokens, and compliance | Developers, Contributors |
| [Intent Identity](./intent-identity.md) | ULID + `PROJECT::author::seq` intent identity and reference resolution | Users, Contributors |
| [Intent Knowledge Graph](./intent-graph.md) | How intents become a queryable graph and the `atomic vault query` commands | Users, Contributors |
| [ARCHITECTURE.md](./ARCHITECTURE.md) | System architecture, data model, and design decisions | Developers, Contributors |
| [THEORY.md](./THEORY.md) | Mathematical foundations of patch theory | Researchers, Core developers |
| [IMPLEMENTATION.md](./IMPLEMENTATION.md) | Implementation details and code organization | Contributors |
| [COMPARISON.md](./COMPARISON.md) | Comparison with other VCS systems (Git, etc.) | Everyone |
| [ROADMAP.md](./ROADMAP.md) | Development phases and future plans | Contributors, Users |

## Quick Links

- **New to Atomic?** Start with [COMPARISON.md](./COMPARISON.md) to understand what makes Atomic different
- **Using the CLI?** See the [CLI Reference](../atomic-cli/README.md) for every command and option
- **Learning stacks?** Walk through the [Stack Walkthrough](./walkthrough-stacks.md) step by step
- **AI audit trail?** Read the [Attestation Design](./attestation-design.md) for graph-level cost and compliance tracking
- **Working with intents?** See [Intent Identity](./intent-identity.md) for how they're named and referenced, and [Intent Knowledge Graph](./intent-graph.md) for querying them
- **Want to contribute?** Read [ARCHITECTURE.md](./ARCHITECTURE.md) then [IMPLEMENTATION.md](./IMPLEMENTATION.md)
- **Curious about the math?** Dive into [THEORY.md](./THEORY.md)
- **Wondering what's next?** Check [ROADMAP.md](./ROADMAP.md)

## Project Root Documentation

Additional documentation in the project root:

| File | Purpose |
|------|---------|
| [../AGENTS.md](../AGENTS.md) | AI development guide and best practices |
| [../README.md](../README.md) | Project overview and quick start |
| [../atomic-cli/README.md](../atomic-cli/README.md) | CLI command reference and usage examples |

## Documentation Conventions

### Code Examples

Code examples use Rust syntax highlighting and are tested where possible:

```rust
use atomic_core::types::Hash;

let hash = Hash::of(b"example content");
println!("Hash: {}", hash);
```

### Diagrams

ASCII diagrams are used for compatibility:

```
┌─────────────┐     ┌─────────────┐
│   Change    │────▶│    Graph    │
└─────────────┘     └─────────────┘
```

### Cross-References

Internal links use relative paths: `[THEORY.md](./THEORY.md)`

## Contributing to Documentation

Documentation improvements are welcome! Please:

1. Keep explanations clear and concise
2. Include code examples where helpful
3. Update diagrams to reflect code changes
4. Maintain consistency with existing style

## Building Documentation

Generate API documentation with:

```bash
cargo doc --open
```

This creates HTML documentation from doc comments in the source code.

---

*This documentation is part of the Atomic VCS clean-room implementation.*