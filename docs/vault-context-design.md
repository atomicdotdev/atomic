# Vault Context: Pre-Task Memory Injection

**Status:** Phase A (`atomic vault context`) implemented — PR #108. Phases B–E are
design; sequencing below.

## Why

The knowledge flywheel (Memory → informs → Intent → motivates → Change →
generates → Provenance → produces → Memory) has the nouns — vault memories,
intents, provenance — but not yet the verbs that make memory *operational*.
Studying the agent-flywheel methodology (Emanuel's CASS/CM stack) surfaced four
mechanics worth adopting:

1. **Pre-task retrieval ritual** — inject relevant lessons before every run
   (this doc / Phase A–D).
2. **Post-session distillation** — turn transcripts into typed memories
   (`explain --save` is the embryo; separate design).
3. **Reinforcement with decay** — score memories, let stale ones fade.
4. **Corpus mining** — promote repeated knowledge into skills.

Where we can beat that stack structurally: CM's scoring depends on humans
running `cm mark`, which nobody does. We can derive the signal from
provenance — a memory injected into a run whose change passes `cb check` and
gets inserted is auto-helpful; a reverted/unrecorded change is auto-harmful.
That requires recording *which memories went into which run*, which is why the
`--json` output below exists.

## What exists already (verified)

Retrieval primitives are mostly built:

- `vault_kg_search` — hybrid KG-FTS + content-index ranked search
  (atomic-repository `vault_triples.rs:148`); memories/intents/goals are
  already KG nodes (`vault_triples.rs:30-45`).
- `vault_kg_neighbors` (`vault_triples.rs:473`) — intent bodies with
  `[[wiki-links]]` and file-path mentions already produce `REFERENCES` edges.
- Vector search `vault_search` (`vault_embeddings.rs:159`) exists with **no CLI
  surface**; embeddings are hash placeholders without a provider key, so the
  design is keyword-first.
- Injection precedent: `learnings.rs` fenced-marker blocks in CLAUDE.md.
- The `hooks claude-code session-start` hook fires on every session but emits
  nothing injectable today.

What was missing: a retrieval command, any injection, any record of what was
injected, and memory bodies in the FTS (it indexes node ids/labels/summaries
only).

## Phase A — `atomic vault context` (this PR)

```
atomic vault context [QUERY]... [--intent <id>] [--files <path>]
                     [--limit 5] [--budget-chars 8000] [--format md|json]
```

- **Candidates:** KG keyword search (kind=memory) ∪ KG neighbors of the seed
  intent (wiki-link `REFERENCES`) ∪ neighbors of `--files` nodes (file
  `REFERENCES` edges) ∪ a CLI-side body term scan (until bodies are FTS-indexed).
- **Ranking:** search rank + neighbor bonus (0.75) + recency
  (90-day half-life, weight 0.25). `type: index` memories (MEMORY.md) excluded.
- **Output:** a fenced markdown block for direct prompt prepending —

  ```markdown
  <!-- atomic:memory-context:start -->
  ## Relevant memories

  ### auth-jwt [project · 2026-07-08]
  Auth module uses JWT with RS256, not HS256. ...
  <!-- atomic:memory-context:end -->
  ```

  or `--json` `[{path, name, kind, score, preview, updated_at}]` so callers can
  record injected ids. Empty result renders empty output / `[]`, so callers
  prepend unconditionally.
- With no seeds at all, returns the most recently updated memories.

## Phases B–E (follow-ups)

- **B — memory write upgrade:** `memory write --kind/--description/--about
  <file|intent:ID>`; extend the frontmatter→edge extraction so `about[]`
  becomes first-class `ABOUT` edges and `description` feeds the node summary.
  Index memory bodies (or descriptions) into KG-FTS, then drop the CLI-side
  body scan. Update the vault skill/template accordingly.
- **C — Sherpa/noname injection:** call `vault context --intent <id> --json`
  before `acp_ask` (cwd = project root, not the sandbox); prepend the md block
  where SKILL.md is already injected, keeping the attested-title invariant
  (`title = original prompt`); record `injectedMemoryIds` in the run sidecar
  and `RunRecord`; surface an "N memories" badge.
- **D — direct-mode injection (no Sherpa):** make `hooks claude-code
  session-start` emit `hookSpecificOutput.additionalContext` with a small
  `vault context` bundle seeded from in-progress intents; config-gated.
- **E — provenance hook:** accept `injected_memories[]` in the turn-end
  payload and store it in the session envelope/provenance metadata. This is
  the interface reinforcement scoring (mechanic 3) builds on.

## Verification (Phase A)

- 22 unit tests in `context.rs`; `cargo test -p atomic-cli` green; clippy clean.
- E2E on a scratch repo: `--intent` (wiki-link neighbor hit), `--files`
  (REFERENCES edge hit), free-text body match, budget truncation, empty-vault
  fallback, unknown-intent error.

## Known limits / notes

- **Cold start:** vaults have few memories until distillation (mechanic 2)
  lands — seed a handful of high-value memories manually for demos.
- `--intent` requires the full id (numeric shorthand like `intent show 1`
  is not resolved yet); `--files` must be repo-relative, forward-slash paths.
- Injection cost should stay visible to callers (budget + count), not hidden.
