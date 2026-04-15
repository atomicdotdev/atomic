---
name: Code Intelligence
description: Use the knowledge graph to understand code structure before reading files
---

# Code Intelligence

The project has a knowledge graph (KG) with every function, struct, trait, enum,
and module indexed by tree-sitter. **Search the KG first, not grep.**

## The Pattern: KG search → neighbors → targeted read

### Step 1: Search the KG with SIMPLE terms

Use one or two keywords, not long phrases. Simpler queries return better results:

✅ Good: `atomic vault query search "worker_threads"`
✅ Good: `atomic vault query search "Builder"`
❌ Bad:  `atomic vault query search "Builder multithread worker_threads tokio::main"`

Run multiple simple searches in parallel instead of one complex query.

### Step 2: Read the `id` field from the results — this is the ONLY source of truth

```
CRITICAL RULE: You MUST copy the "id" field VERBATIM from search results.
NEVER construct, guess, or modify an entity ID. If the search didn't
return it, the ID does not exist.
```

Example: search returns this result:
```json
{"id": "entity:tokio/src/runtime/builder.rs:worker_threads:509", ...}
```

✅ CORRECT — copy the id exactly as returned:
```
atomic vault query neighbors entity:tokio/src/runtime/builder.rs:worker_threads:509
```

❌ WRONG — you guessed the line number:
```
atomic vault query neighbors entity:tokio/src/runtime/builder.rs:worker_threads:356
```

❌ WRONG — you guessed the path:
```
atomic vault query neighbors entity:src/runtime/builder.rs:worker_threads
```

❌ WRONG — the search didn't return this ID:
```
atomic vault query neighbors entity:tokio/src/runtime/builder.rs:Builder:44
```

If you need an entity that didn't appear in search results, do another search
with different terms — do NOT guess the ID.

### Step 3: Use neighbors on IDs you found

The neighbors query returns:
- **DEFINES edges**: which file defines this entity
- **MODIFIES edges**: which changes touched it (who, when, commit message)
- **Connected entities**: other functions in the same file

### Step 4: Explore the file's entities

To see ALL functions/structs in a file, query neighbors on the file node:

```
atomic vault query neighbors file:tokio/src/runtime/builder.rs
```

This returns every entity the file defines — all functions, structs, traits,
impls, constants. Much faster than reading the file.

### Step 5: Read only what you must

After KG search + neighbors narrow the scope, read only the specific function
body you need to understand or edit. Don't read entire files.

## When to use KG vs grep vs read

| I need to... | Use |
|---|---|
| Find functions/types by name | `atomic vault query search "name"` |
| See a function's signature and location | KG search (summary field has the signature) |
| See what file defines a function | `atomic vault query neighbors entity:...` → DEFINES edge |
| See what changes modified a function | `atomic vault query neighbors entity:...` → MODIFIES edge |
| See all functions in a file | `atomic vault query neighbors file:path` |
| See what a change touched | `atomic vault query neighbors change:HASH` |
| Find a literal string or error message | `grep "exact string"` |
| Find regex patterns | `grep "pattern"` |
| Read function implementation details | `read file` (only after KG narrows the scope) |

## Searching tips

- **Search broadly first**: `atomic vault query search "worker_threads"` finds entities, files, AND changes
- **Use file: prefix to explore a file**: `atomic vault query neighbors file:src/runtime/builder.rs`
- **Chain searches**: search → get IDs → neighbors on those IDs → find connected IDs → neighbors again
- **The search returns at most 10 results** — use specific terms to narrow down

## Entity ID Format

Entity IDs are structured as `entity:{file_path}:{name}:{line_number}`:

```
entity:tokio/src/runtime/builder.rs:worker_threads:509    — function at line 509
entity:tokio/src/runtime/builder.rs:Builder:50             — struct at line 50
entity:tokio-macros/src/entry.rs:set_worker_threads:126    — method at line 126
```

The line number comes from the AST extraction and is stable for the current
version of the code. **Always get IDs from search results, never construct them.**

## Planning an Intent with KG

When creating an intent, use the KG to build a concrete plan:

1. Search for the relevant concept:
   ```
   atomic vault query search "worker_threads Builder multithread"
   ```

2. For each relevant entity in the results, explore its neighbors:
   ```
   atomic vault query neighbors entity:tokio/src/runtime/builder.rs:worker_threads:509
   atomic vault query neighbors file:tokio/src/runtime/builder.rs
   atomic vault query neighbors file:tokio-macros/src/entry.rs
   ```

3. Now you know the exact files, functions, and signatures involved.
   Write this into the intent's **Files to Modify** table and **Approach** section.

4. Only `read` the function bodies you need to understand the implementation
   details for your plan.