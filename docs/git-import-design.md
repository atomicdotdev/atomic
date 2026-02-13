# Git Import Design Document

## Overview

This document describes the design for importing Git repositories into Atomic VCS.
The goal is to provide a seamless migration path for existing Git projects while
preserving history, authorship, and semantic meaning of changes.

## Command Interface

### Primary Command

```bash
# Import from current directory (must be a Git repository)
atomic git import

# Import specific branch
atomic git import --branch main

# Import all branches as stacks
atomic git import --all-branches

# Import from specific Git directory
atomic git import --git-dir /path/to/repo/.git

# Import with options
atomic git import --branch main --since "2024-01-01" --limit 100

# Dry run (show what would be imported)
atomic git import --dry-run
```

### Options

| Option | Description |
|--------|-------------|
| `--branch <NAME>` | Branch to import (default: HEAD) |
| `--all-branches` | Import all branches as separate stacks |
| `--git-dir <PATH>` | Path to .git directory |
| `--since <DATE>` | Only import commits after this date |
| `--until <DATE>` | Only import commits before this date |
| `--limit <N>` | Maximum number of commits to import |
| `--authors-file <PATH>` | Map Git authors to Atomic identities |
| `--no-tags` | Don't import Git tags |
| `--dry-run` | Preview import without making changes |
| `--verbose` | Show detailed progress |

## Architecture

### Crate Structure

```
atomic-git/                    # New crate for Git integration
├── Cargo.toml
└── src/
    ├── lib.rs                 # Crate root, public API
    ├── error.rs               # Error types
    ├── repository.rs          # Git repository wrapper
    ├── commit.rs              # Git commit parsing
    ├── diff.rs                # Git diff extraction
    ├── convert.rs             # Git → Atomic conversion
    ├── import.rs              # Import orchestration
    └── author.rs              # Author mapping

atomic-cli/src/commands/git/   # CLI commands
├── mod.rs                     # Subcommand router
└── import.rs                  # Import command
```

### Dependencies

```toml
[dependencies]
git2 = "0.18"                  # libgit2 bindings
atomic-core = { path = "../atomic-core" }
atomic-repository = { path = "../atomic-repository" }
thiserror = "1.0"
chrono = "0.4"
```

## Data Model Mapping

### Git Commit → Atomic Change

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                     Git Commit to Atomic Change Mapping                      │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  Git Commit                           Atomic Change                         │
│  ┌─────────────────────────┐         ┌─────────────────────────┐           │
│  │ SHA-1 hash              │   →     │ Blake3 hash (computed)  │           │
│  │ Author name + email     │   →     │ ChangeHeader.authors    │           │
│  │ Committer (if different)│   →     │ ChangeHeader.authors[1] │           │
│  │ Commit message          │   →     │ ChangeHeader.message    │           │
│  │ Timestamp               │   →     │ ChangeHeader.timestamp  │           │
│  │ Parent commits          │   →     │ dependencies            │           │
│  │ Tree (file snapshot)    │   →     │ hunks[]                 │           │
│  └─────────────────────────┘         └─────────────────────────┘           │
│                                                                             │
│  Additional Metadata (stored in Change.unhashed or extra_known):            │
│  • Original Git SHA for reference                                           │
│  • Git committer if different from author                                   │
│  • GPG signature info (if signed)                                           │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Git Diff → Atomic Hunks

| Git Change | Atomic Hunk |
|------------|-------------|
| New file (A) | `Hunk::FileAdd` |
| Deleted file (D) | `Hunk::FileDel` |
| Modified file (M) | `Hunk::Edit` / `Hunk::Replacement` |
| Renamed file (R) | `Hunk::FileMove` + optional `Hunk::Edit` |
| Copied file (C) | `Hunk::FileAdd` (with content from source) |
| Type change (T) | `Hunk::FileDel` + `Hunk::FileAdd` |

### Git Branch → Atomic Stack

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        Branch to Stack Mapping                               │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  Git Repository                        Atomic Repository                    │
│                                                                             │
│  main ─────●─────●─────●               Stack "main"                        │
│             \                          [A] → [B] → [C]                      │
│              \                                                              │
│               ●─────●─────●            Stack "feature"                     │
│              feature                   [A] → [B] → [D] → [E] → [F]         │
│                                                                             │
│  Note: Stacks share common ancestors (changes A, B)                        │
│  The graph is shared, stacks are just different views                      │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Import Algorithm

### Phase 1: Analysis

```rust
pub struct ImportAnalysis {
    /// Total commits to import
    pub commit_count: usize,
    /// Branches/tags found
    pub branches: Vec<BranchInfo>,
    pub tags: Vec<TagInfo>,
    /// Estimated size
    pub total_size_bytes: u64,
    /// Authors found
    pub authors: HashSet<GitAuthor>,
    /// Merge commits (need special handling)
    pub merge_commits: usize,
    /// Binary files detected
    pub binary_files: Vec<String>,
}

fn analyze_repository(git_repo: &git2::Repository, options: &ImportOptions) 
    -> Result<ImportAnalysis, ImportError>;
```

### Phase 2: Topological Sort

Git commits form a DAG. We need to import in topological order so dependencies
exist before dependents:

```rust
/// Returns commits in topological order (parents before children)
fn topological_sort(
    repo: &git2::Repository,
    head: git2::Oid,
) -> Result<Vec<git2::Oid>, ImportError>;
```

### Phase 3: Commit Conversion

For each commit in topological order:

```rust
pub struct ConvertedCommit {
    /// The Atomic change
    pub change: Change,
    /// Mapping from Git SHA to Atomic hash
    pub git_sha: git2::Oid,
    pub atomic_hash: Hash,
    /// Files affected
    pub files: Vec<String>,
}

fn convert_commit(
    git_repo: &git2::Repository,
    commit: &git2::Commit,
    parent_map: &HashMap<git2::Oid, Hash>,  // Git SHA → Atomic Hash
    options: &ConvertOptions,
) -> Result<ConvertedCommit, ImportError>;
```

### Phase 4: Diff Extraction

```rust
/// Extract file changes between parent and commit
fn extract_changes(
    repo: &git2::Repository,
    parent: Option<&git2::Tree>,
    commit_tree: &git2::Tree,
) -> Result<Vec<FileChange>, ImportError>;

pub enum FileChange {
    Add { path: String, content: Vec<u8>, mode: FileMode },
    Delete { path: String },
    Modify { path: String, old: Vec<u8>, new: Vec<u8> },
    Rename { old_path: String, new_path: String, content_changed: bool },
}
```

### Phase 5: Hunk Generation

```rust
/// Convert a file change to Atomic hunks
fn file_change_to_hunks(
    change: &FileChange,
    inode_map: &mut InodeMap,
    content_buffer: &mut Vec<u8>,
) -> Result<Vec<Hunk<Option<Hash>>>, ImportError>;
```

### Phase 6: Change Assembly

```rust
/// Assemble a complete Atomic change from converted commit data
fn assemble_change(
    header: ChangeHeader,
    hunks: Vec<Hunk<Option<Hash>>>,
    contents: Vec<u8>,
    dependencies: Vec<Hash>,
) -> Change;
```

## Special Cases

### Merge Commits

Git merge commits have multiple parents. Options for handling:

#### Option A: Linearize (Recommended for MVP)

```
Git:                           Atomic:
    A───B───C                  A → B → C → D → E → F
         \   \
          D───E───F
```

Pro: Simple, predictable
Con: Loses branch structure

#### Option B: Preserve Structure

Create multiple stacks, import merge as applying changes from one stack to another:

```
Git:                           Atomic:
    A───B───C───M              Stack "main": A → B → C → (apply D,E from feature) → M
         \     /
          D───E                Stack "feature": A → B → D → E
```

Pro: Preserves history
Con: More complex, merge commit needs special handling

#### Option C: Record Merge as Metadata

Import merge commit as a regular change with metadata indicating it was a merge:

```rust
// In Change.unhashed
{
    "git_merge": {
        "parents": ["sha1", "sha2"],
        "strategy": "recursive"
    }
}
```

### Binary Files

Binary files can't be diffed meaningfully. Options:

1. **Store as FileAdd/FileReplace**: Treat each version as a complete replacement
2. **Skip with warning**: Don't import binary file changes
3. **Configurable**: Let user choose per-file or by pattern

```rust
pub enum BinaryHandling {
    /// Store complete file on each change
    FullContent,
    /// Skip binary files
    Skip,
    /// Store only in specific commits (e.g., first appearance)
    FirstOnly,
}
```

### Renames and Copies

Git detects renames heuristically (by content similarity). We should:

1. Use git2's rename detection
2. Convert to `Hunk::FileMove`
3. If content also changed, add `Hunk::Edit` hunks

```rust
let mut diff_opts = git2::DiffOptions::new();
diff_opts.find_renames(true);
diff_opts.rename_threshold(50);  // 50% similarity threshold
```

### Symlinks

Git supports symlinks. Options:

1. **Import as regular files**: Store symlink target as content
2. **Import with metadata**: Store as file with symlink flag
3. **Skip**: Warn and skip symlinks

### File Modes/Permissions

Git tracks executable bit. We should preserve this:

```rust
pub struct FileMode {
    pub executable: bool,
    pub is_symlink: bool,
}
```

### Empty Commits

Git allows empty commits (no file changes). Options:

1. **Skip**: Don't create Atomic change
2. **Create empty change**: Preserve for message/timestamp
3. **Configurable**: User chooses

### Shallow Clones

Shallow clones don't have full history. Detection and handling:

```rust
if repo.is_shallow() {
    return Err(ImportError::ShallowRepository {
        suggestion: "Run 'git fetch --unshallow' first".into()
    });
}
```

## Author Mapping

### Automatic Mapping

Try to match Git authors to existing Atomic identities:

```rust
fn find_matching_identity(
    git_author: &GitAuthor,
    identities: &IdentityStore,
) -> Option<Identity> {
    // Match by email first
    if let Some(id) = identities.find_by_email(&git_author.email) {
        return Some(id);
    }
    // Fall back to name match
    identities.find_by_name(&git_author.name)
}
```

### Manual Mapping File

```toml
# authors-mapping.toml

[authors]
# Git author = Atomic identity name
"John Doe <john@old-email.com>" = "john-personal"
"John Doe <john@work.com>" = "john-work"

[default]
# Create new identities for unmapped authors
create_identities = true
# Or use a default identity
# default_identity = "imported"
```

## Progress Reporting

```rust
pub trait ImportProgress {
    fn on_analysis_start(&self);
    fn on_analysis_complete(&self, analysis: &ImportAnalysis);
    fn on_commit_start(&self, index: usize, total: usize, sha: &str);
    fn on_commit_complete(&self, index: usize, total: usize, hash: &Hash);
    fn on_file_processed(&self, path: &str, change_type: &str);
    fn on_warning(&self, message: &str);
    fn on_error(&self, error: &ImportError);
    fn on_complete(&self, summary: &ImportSummary);
}
```

## Error Handling

```rust
#[derive(Debug, Error)]
pub enum ImportError {
    #[error("Not a Git repository: {path}")]
    NotGitRepository { path: PathBuf },
    
    #[error("Shallow repository - run 'git fetch --unshallow' first")]
    ShallowRepository,
    
    #[error("Branch not found: {name}")]
    BranchNotFound { name: String },
    
    #[error("Failed to parse commit {sha}: {reason}")]
    CommitParseError { sha: String, reason: String },
    
    #[error("Binary file too large: {path} ({size} bytes)")]
    BinaryTooLarge { path: String, size: u64 },
    
    #[error("Atomic repository error: {0}")]
    Repository(#[from] RepositoryError),
    
    #[error("Git error: {0}")]
    Git(#[from] git2::Error),
    
    #[error("Import cancelled by user")]
    Cancelled,
}
```

## Output and Summary

```rust
pub struct ImportSummary {
    /// Total commits imported
    pub commits_imported: usize,
    /// Commits skipped (empty, etc.)
    pub commits_skipped: usize,
    /// Files processed
    pub files_processed: usize,
    /// Binary files handled
    pub binary_files: usize,
    /// Merge commits
    pub merge_commits: usize,
    /// Total content size
    pub content_bytes: u64,
    /// Time taken
    pub duration: Duration,
    /// Warnings generated
    pub warnings: Vec<String>,
    /// SHA → Hash mapping for reference
    pub commit_map: HashMap<String, Hash>,
}
```

Example output:

```
$ atomic git import --branch main

Analyzing Git repository...
  Found 1,247 commits on branch 'main'
  Found 23 merge commits
  Found 15 binary files
  Found 47 unique authors

Importing commits...
  [====================================] 1247/1247 (100%)

Import complete!
  ✓ Imported 1,247 commits as Atomic changes
  ✓ Created stack 'main' with 1,247 changes
  ✓ Processed 3,891 file changes
  ⚠ 15 binary files stored as full content
  ⚠ 23 merge commits linearized

Time: 45.3s
Size: 127.4 MB

Commit mapping saved to .atomic/git-import-map.json
```

## Incremental Import

For repositories that continue using Git alongside Atomic:

```bash
# Initial import
atomic git import --branch main

# Later, import new commits
atomic git import --branch main --incremental
```

Implementation:

```rust
pub struct IncrementalState {
    /// Last imported Git SHA
    pub last_sha: git2::Oid,
    /// Corresponding Atomic hash
    pub last_hash: Hash,
    /// Branch name
    pub branch: String,
    /// Import timestamp
    pub imported_at: DateTime<Utc>,
}

// Stored in .atomic/git-import-state.json
```

## Testing Strategy

### Unit Tests

- Commit parsing
- Diff extraction
- Hunk conversion
- Author mapping
- Topological sorting

### Integration Tests

- Import small test repository
- Verify change content matches
- Verify dependencies are correct
- Verify author/message preservation

### Fixtures

Create test Git repositories with:
- Linear history
- Branches and merges
- Renames and copies
- Binary files
- Empty commits
- Signed commits
- Various encodings

## Implementation Phases

### Phase 1: MVP (Linear Import)

- [ ] Basic Git repository reading with git2
- [ ] Linear commit traversal (single branch, no merges)
- [ ] File add/delete/modify conversion
- [ ] Author/message/timestamp preservation
- [ ] Basic CLI command

### Phase 2: Full History

- [ ] Merge commit handling (linearization)
- [ ] Multiple branch import
- [ ] Git tag import
- [ ] Rename detection

### Phase 3: Advanced Features

- [ ] Incremental import
- [ ] Author mapping file
- [ ] Binary file strategies
- [ ] Progress reporting
- [ ] Import state persistence

### Phase 4: Polish

- [ ] Performance optimization
- [ ] Better error messages
- [ ] Documentation
- [ ] Edge case handling

## Open Questions

1. **Should we preserve Git SHAs?** 
   - Pro: Allows cross-referencing
   - Con: Adds complexity, storage overhead
   - Recommendation: Store in `Change.unhashed` as optional metadata

2. **How to handle submodules?**
   - Option A: Skip with warning
   - Option B: Import as nested repositories
   - Option C: Inline the submodule content
   - Recommendation: Skip for MVP, add later

3. **Should import be reversible?**
   - Could we export back to Git?
   - Useful for migration testing
   - Recommendation: Out of scope for initial implementation

4. **How to handle Git LFS?**
   - Git LFS files are pointers, not content
   - Need to fetch actual content from LFS server
   - Recommendation: Skip for MVP, warn user

5. **Concurrent Git/Atomic usage?**
   - Some teams might want to use both during migration
   - Need clear guidance on workflow
   - Recommendation: Document as one-way migration for now

## References

- [git2-rs documentation](https://docs.rs/git2)
- [libgit2 documentation](https://libgit2.org/)
- [Git internals](https://git-scm.com/book/en/v2/Git-Internals-Plumbing-and-Porcelain)
- [Atomic change format](./change-format.md)
- [Atomic hunk types](../atomic-core/src/change/hunk.rs)