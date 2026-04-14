//! Default vault content installed on `init_vault()`.
//!
//! Includes the atomic-vault skill that teaches the agent how to use
//! the vault system (goals, intents, memory).

use super::*;
use atomic_core::pristine::VaultEntryType;

/// The code intelligence skill — teaches the agent to combine grep with KG queries.
const CODE_INTELLIGENCE_SKILL: &str = r#"---
name: Code Intelligence
description: Use the knowledge graph to understand code structure, not just text matches
---

# Code Intelligence

When you find code via grep, **don't just read the file**. Use the knowledge graph
to understand the structural context: what function it's in, who wrote it, what
calls it, and what intent it serves.

## The Pattern: grep → KG → targeted read

### Step 1: Find with grep

```
grep "authenticate"
→ src/auth.rs:42:    pub fn authenticate(creds: &Credentials) -> Result<Token> {
→ src/auth.rs:78:    let result = authenticate(&creds);
→ src/token.rs:15:   fn validate_token(token: &str) -> bool {
```

### Step 2: Look up the entity in the KG

```
atomic vault query neighbors "entity:src/auth.rs:authenticate:42"
```

This returns:
- **The entity itself**: function signature, line range, exported status
- **DEFINES**: which file defines it
- **MODIFIES**: which changes touched it (who, when, what commit message)
- **CALLS**: what other functions it calls
- **LINKED_INTENT**: what intent/task it relates to

### Step 3: Read only what you need

Instead of reading 800 lines of auth.rs, you now know:
- The exact function signature (from the entity node's summary)
- Who last modified it (from AUTHORED_BY on the change)
- What it calls (from CALLS edges)
- Why it was written (from LINKED_INTENT)

Only read the file if you need the implementation details.

## Common Queries

### "What functions are in this file?"
```
atomic vault query search "file:src/auth.rs" --json
```

### "Who last changed this function?"
```
atomic vault query neighbors "entity:src/auth.rs:authenticate:42" --json
```
Look for `change:` nodes connected via `MODIFIES` edges.

### "What calls this function?"
```
atomic vault query neighbors "entity:src/auth.rs:authenticate:42" --json
```
Look for other `entity:` nodes connected via `CALLS` edges.

### "What files were changed in the last commit?"
```
atomic vault query neighbors "change:abc123" --json
```
Look for `file:` nodes connected via `MODIFIES` edges.

### "What's the full picture for an intent?"
```
atomic vault query neighbors "intent:PIMO-1" -d 2 --json
```
Depth 2 shows: the intent → linked goals → modified files → entities.

### "Find everything related to authentication"
```
atomic vault query ask "what code handles authentication?"
```
This does FTS + vector search + optional LLM answer.

## Entity Node IDs

Entity IDs follow the pattern `entity:{file}:{name}:{line}`:
- `entity:src/auth.rs:authenticate:42` — function at line 42
- `entity:src/main.rs:Config:10` — struct at line 10
- `entity:src/lib.rs:AppModule:1` — module at line 1

## When to Use KG vs Read

| Situation | Use KG | Use Read |
|-----------|--------|----------|
| Understand function purpose | ✅ signature + intent | |
| See who wrote it | ✅ AUTHORED_BY edge | |
| Find callers/callees | ✅ CALLS edges | |
| Understand implementation | | ✅ read the function body |
| Fix a bug in the code | ✅ first, then | ✅ read the specific lines |
| Explore unfamiliar code | ✅ neighbors depth 2 | |

## Planning an Intent

When creating or working on an intent, use the KG to plan:

1. `atomic vault query search "authentication"` — find all related entities
2. `atomic vault query neighbors "entity:src/auth.rs:authenticate:42" -d 2` — see the call graph
3. Now you know which files and functions need changing
4. Create the intent with concrete acceptance criteria based on actual code structure
"#;

/// The default atomic-vault skill content.
const VAULT_SKILL: &str = r#"---
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

```
# Create an intent
atomic vault intent create --title "Fix authentication" --priority high

# List intents
atomic vault intent list
atomic vault intent list --status in-progress

# Show intent details
atomic vault intent show PIMO-1

# Update status
atomic vault intent update PIMO-1 --status in-progress --assignee "your name"

# Link a goal to an intent
atomic vault intent link PIMO-1 --goal swift-meadow-a3f2
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

1. **Start**: `atomic vault goal start` when you begin working
2. **Check intents**: `atomic vault intent list` to see what needs doing
3. **Work**: Use grep, read, edit tools to make changes
4. **Record**: Changes are recorded automatically by the VCS hooks
5. **Stop**: `atomic vault goal stop --promote` when done
"#;

/// Default memory index content.
const MEMORY_INDEX: &[u8] = b"# Project Memory\n\nThis file indexes shared project knowledge. Each entry links to a detailed file.\n\n<!-- Add entries as: - [Title](filename.md) -- description -->\n";

impl Repository {
    /// Install default vault content (skills, initial memory index).
    ///
    /// Called automatically by `init_vault()`. Idempotent — skips entries
    /// that already exist.
    pub fn vault_install_defaults(&self) -> Result<(), RepositoryError> {
        // Install the vault skill
        let skill_path = "skills/atomic-vault.md";
        if self.vault_retrieve(skill_path)?.is_none() {
            self.vault_store(
                skill_path,
                VaultEntryType::Skill,
                VAULT_SKILL.as_bytes().to_vec(),
                r#"{"name":"Atomic Vault","description":"How to use the project vault for goals, intents, and shared memory"}"#.to_string(),
            )?;
            self.vault_materialize(skill_path)?;
        }

        // Install the code intelligence skill
        let code_intel_path = "skills/code-intelligence.md";
        if self.vault_retrieve(code_intel_path)?.is_none() {
            self.vault_store(
                code_intel_path,
                VaultEntryType::Skill,
                CODE_INTELLIGENCE_SKILL.as_bytes().to_vec(),
                r#"{"name":"Code Intelligence","description":"Use the knowledge graph to understand code structure, not just text matches"}"#.to_string(),
            )?;
            self.vault_materialize(code_intel_path)?;
        }

        // Install a default MEMORY.md index
        let memory_index_path = "memory/MEMORY.md";
        if self.vault_retrieve(memory_index_path)?.is_none() {
            self.vault_store(
                memory_index_path,
                VaultEntryType::Memory,
                MEMORY_INDEX.to_vec(),
                r#"{"name":"MEMORY","type":"index"}"#.to_string(),
            )?;
            self.vault_materialize(memory_index_path)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::pristine::VaultEntryType;
    use tempfile::tempdir;

    #[test]
    fn test_vault_install_defaults() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_install_defaults().unwrap();

        // Vault skill should exist in redb
        let skill = repo.vault_retrieve("skills/atomic-vault.md").unwrap();
        assert!(skill.is_some());
        let skill = skill.unwrap();
        assert_eq!(skill.entry_type, VaultEntryType::Skill);
        let content = String::from_utf8_lossy(&skill.content_bytes);
        assert!(content.contains("# Atomic Vault"));

        // Vault skill should be on disk
        assert!(repo.vault_dir().join("skills/atomic-vault.md").exists());

        // Code intelligence skill should exist
        let code_skill = repo.vault_retrieve("skills/code-intelligence.md").unwrap();
        assert!(code_skill.is_some());
        let code_skill = code_skill.unwrap();
        assert_eq!(code_skill.entry_type, VaultEntryType::Skill);
        let code_content = String::from_utf8_lossy(&code_skill.content_bytes);
        assert!(code_content.contains("# Code Intelligence"));
        assert!(code_content.contains("grep"));
        assert!(code_content.contains("neighbors"));

        // Code intelligence skill should be on disk
        assert!(repo
            .vault_dir()
            .join("skills/code-intelligence.md")
            .exists());

        // Memory index should exist
        let memory = repo.vault_retrieve("memory/MEMORY.md").unwrap();
        assert!(memory.is_some());
        let memory = memory.unwrap();
        assert_eq!(memory.entry_type, VaultEntryType::Memory);
        assert!(repo.vault_dir().join("memory/MEMORY.md").exists());
    }

    #[test]
    fn test_vault_install_defaults_idempotent() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_install_defaults().unwrap();
        repo.vault_install_defaults().unwrap(); // Should not fail or duplicate

        let entries = repo.vault_list("skills/", None).unwrap();
        assert_eq!(entries.len(), 2); // vault + code-intelligence skills
    }

    #[test]
    fn test_vault_skill_content_is_valid_markdown() {
        // Verify the embedded skill content is well-formed
        assert!(VAULT_SKILL.contains("# Atomic Vault"));
        assert!(VAULT_SKILL.contains("## Goals"));
        assert!(VAULT_SKILL.contains("## Intents"));
        assert!(VAULT_SKILL.contains("## Memory"));
        assert!(VAULT_SKILL.contains("## Workflow"));
    }

    #[test]
    fn test_vault_memory_index_content() {
        let content = String::from_utf8_lossy(MEMORY_INDEX);
        assert!(content.contains("# Project Memory"));
    }
}
