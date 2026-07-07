---
description: How to use the project vault for goals, intents, and shared memory
name: Atomic Vault
entry_type: skill
content_hash: KFV3MJULZHJMKSTY3HNCDNZSFDALMYTAFMHEWGLR4JQNNMZ5WYYQ
created_at: 2026-06-16T22:21:17.843890+00:00
updated_at: 2026-06-16T22:21:17.843890+00:00
---
---
name: Atomic Vault
description: How to use the project vault for goals, intents, and shared memory
---

# Atomic Vault

This project has an Atomic vault — a shared knowledge store at `.vault/`.
Use the `atomic` tool to interact with it.

## Goals (Development Sessions)

Goals track your work sessions. Start one when you begin working, stop when done.

```
# Start a goal (generates a name like "swift-meadow-a3f2")
atomic vault goal start --developer "your name"

# Start with a linked intent
atomic vault goal start --intent PIMO-1

# Stop and promote (marks as completed for the team)
atomic vault goal stop --promote

# Stop and suspend (can resume later)
atomic vault goal stop

# Resume a previous goal
atomic vault goal resume swift-meadow-a3f2

# List goals
atomic vault goal list --status active
atomic vault goal list --status all
```

## Intents (JIRA-style Tasks)

Intents are units of work with auto-generated IDs (e.g., PIMO-1, PIMO-2).

**IMPORTANT: Create ONE intent per task. Check existing intents first.**

### Creating an Intent

1. **Check if the intent already exists:**
   ```
   atomic vault intent list
   ```

2. **Create exactly ONE intent** (do NOT retry if the CLI errors — check the list):
   ```
   atomic vault intent create --title "Fix authentication" --priority high
   ```
   This returns an ID like PROJ-1 and creates a scaffold file at `.vault/intents/proj-1/intent.md`.

3. **Write your plan into the intent file.** The scaffold has sections to fill in:
   ```
   edit .vault/intents/proj-1/intent.md using the .vault/templates/intent.md template
   ```
   Replace the HTML comment placeholders in each section:
   - **Description**: What's the problem? Link to the GitHub issue.
   - **Preconditions**: What must be true before starting?
   - **Acceptance Criteria**: Concrete checkboxes — what does "done" look like?
   - **Files to Modify**: Table of files, what changes, and why.
   - **Approach**: Step-by-step plan referencing specific functions from the KG.
   - **Test Strategy**: How will you verify each criterion?
   - **Notes**: Findings from code exploration, trade-offs, open questions.

   The intent file IS the deliverable — a reviewer reads it to understand the full plan.

4. **Update status as you work:**
   ```
   atomic vault intent update PROJ-1 --status in-progress
   atomic vault intent update PROJ-1 --status review
   atomic vault intent update PROJ-1 --status done
   ```

### Intent Workflow Example

```
# Check existing intents first
atomic vault intent list

# Create ONE intent
atomic vault intent create --title "Add max_worker_threads option" --priority high
# → returns TOKI-1

# Explore the code to build the plan
atomic vault query search "worker_threads Builder"
atomic vault query neighbors "entity:src/runtime/builder.rs:worker_threads:42"

# Write the plan into the intent file — fill in every section
edit .vault/intents/toki-1/intent.md
# Fill in: Description, Preconditions, Acceptance Criteria,
# Files to Modify (table), Approach (steps), Test Strategy, Notes

# Link a goal and start working
atomic vault goal start --intent TOKI-1
```

Intent statuses: backlog → planned → in-progress → review → done

## Memory (Shared Knowledge)

Memory files persist project knowledge across sessions and developers.

```
# List memory files
atomic vault memory list

# Read a memory file
atomic vault memory show architecture
```

To save new knowledge, write a markdown file to `.vault/memory/` and
then run `atomic vault sync` to persist it to the database.

## Version Control

The vault is tracked by the same Atomic VCS as the code:

```
# Check repo status
atomic status

# View change history
atomic log

# Show working copy diff
atomic diff
```

## Workflow

1. **Check existing intents**: Don't create duplicates.
   ```
   atomic vault intent list
   ```

2. **Create ONE intent** with a clear title. Then explore the code and write a plan into the intent file:
   ```
   atomic vault intent create --title "Fix issue #42: description"
   # Explore with grep + KG queries
   # Edit .vault/intents/proj-N/intent.md with your plan
   ```

3. **Start a goal** linked to the intent:
   ```
   atomic vault goal start --intent PROJ-1
   ```

4. **Create a draft view** for isolated work:
   ```
   atomic view create <goal-name> --draft
   atomic view switch <goal-name>
   ```

5. **Search and explore** using grep + knowledge graph together:
   ```
   grep "worker_threads" --type rs
   atomic vault query search "worker_threads Builder"
   atomic vault query neighbors "entity:src/builder.rs:worker_threads:42"
   ```

6. **Make changes** using read, edit, write tools

7. **Record** your changes:
   ```
   atomic record -m "Add max_worker_threads option to Builder"
   ```

8. **Stop the goal** when done:
   ```
   atomic vault goal stop --promote
   ```

9. **Review**: The draft view stays alive for review.
   ```
   atomic view switch <goal-name>
   atomic diff
   ```

10. **Merge** when approved:
    ```
    atomic insert from-view <goal-name> --to-view dev
    ```
