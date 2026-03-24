# `atomic-cli` Compiler Warnings Cleanup

## Context

CI runs with `RUSTFLAGS=-Dwarnings`, which promotes all compiler warnings to
errors. During a recent sprint a large batch of scaffold/stub implementations
landed in `atomic-cli` — builder APIs, progress-tracking structs, and
output-formatting helpers that are not yet wired up to real usage. Rather than
block CI, the warnings were temporarily suppressed with an `#![allow(...)]`
block in `atomic-cli/src/main.rs` (lines 41–50):

```atomic/atomic-cli/src/main.rs#L41-50
#![allow(
    dead_code,
    unused_imports,
    unused_mut,
    unused_variables,
    unused_assignments
)]
```

This document tracks every suppressed warning so they can be resolved
file-by-file as the corresponding commands are fully implemented. Once every
item below is ticked off the `#![allow(...)]` block must be removed.

---

## Summary Table

| Lint category        | File count | Warning count (approx.) |
|----------------------|:----------:|:-----------------------:|
| `unused_imports`     |     11     |           ~20           |
| `dead_code`          |     30     |           ~90           |
| `unused_variables`   |      1     |            1            |
| `unused_mut`         |      1     |            1            |
| `unused_assignments` |      1     |            3            |
| **Total**            |   **~35**  |         **~115**        |

---

## How to Read This Document

- **`unused_imports`** — remove the offending identifier(s) from the `use`
  statement (or the entire statement if it becomes empty).
- **`dead_code` on builder methods** — either (a) wire the method up to actual
  call-site usage, or (b) delete it if the API will never be needed.
- **`dead_code` on structs / free functions** — either use them or delete them.
- **`unused_variables`** — prefix the binding with `_`, or restructure the
  code to actually use the value.
- **`unused_mut`** — remove the `mut` qualifier from the binding.
- **`unused_assignments`** — remove the assignment (or actually read the
  assigned value before it is overwritten / the scope ends).

---

## Files

### `atomic-cli/src/commands/diff/mod.rs` — `unused_imports`

Line 178.

- [ ] Remove `build_hunks_from_diff` from the `use` statement
- [ ] Remove `format_stat_graph` from the `use` statement

---

### `atomic-cli/src/commands/diff/output.rs` — `dead_code`

Line 34.

- [ ] `fn format_stat_graph` — wire up to diff stat output or delete

---

### `atomic-cli/src/commands/diff/command.rs` — `dead_code`

Line 159.

- [ ] `impl Diff` builder methods — wire up or delete:
  - [ ] `with_files`
  - [ ] `with_change`
  - [ ] `with_algorithm`
  - [ ] `with_context`
  - [ ] `with_stat`
  - [ ] `with_no_color`
  - [ ] `with_name_only`
  - [ ] `with_name_status`
  - [ ] `with_stack`
  - [ ] `with_word_diff`

---

### `atomic-cli/src/commands/diff/types.rs` — `dead_code`

Lines 150, 221, 306, 467, 535.

- [ ] `impl FileDiffStats` methods — wire up or delete:
  - [ ] `has_changes`
  - [ ] `is_added`
  - [ ] `is_deleted`
  - [ ] `is_modified`
- [ ] `impl DiffStats::total_changes` — wire up or delete
- [ ] `impl DiffOutputConfig` associated items — wire up or delete:
  - [ ] `new`
  - [ ] `with_context`
  - [ ] `with_color`
  - [ ] `with_format`
  - [ ] `with_stat_width`
  - [ ] `with_line_numbers`
  - [ ] `with_path_prefix`
  - [ ] `with_word_diff`
- [ ] `impl HunkLine` methods — wire up or delete:
  - [ ] `is_added`
  - [ ] `is_removed`
  - [ ] `is_deleted`
  - [ ] `is_context`
  - [ ] `is_modified`
- [ ] `impl FileDiff` methods — wire up or delete:
  - [ ] `has_changes`
  - [ ] `total_changes`

---

### `atomic-cli/src/commands/stash.rs` — `unused_imports` + `unused_mut`

Lines 80, 277.

- [ ] Remove `atomic_repository::record::RecordOptions` from the `use`
  statement (line 80)
- [ ] Remove `mut` from `with_dependencies(mut self, …)` — the parameter is
  never mutated (line 277)

---

### `atomic-cli/src/commands/clone/types.rs` — `unused_imports` + `dead_code`

Lines 25 + ~4 additional dead_code warnings.

- [ ] Remove `Hash` from `use atomic_core::types::{Hash, Merkle}` (line 25)
- [ ] Remaining `dead_code` items (~4 warnings) — wire up or delete the
  unreferenced struct fields / methods in this file

---

### `atomic-cli/src/commands/clone/helpers.rs` — `dead_code`

~3 warnings.

- [ ] Audit unreferenced helpers — wire up or delete (~3 items)

---

### `atomic-cli/src/commands/clone/command.rs` — `unused_assignments`

Lines 436, 523, 537.

- [ ] Line 523: `progress.phase = ClonePhase::ConfiguringRemote` — either
  read `progress.phase` after this point or remove the assignment
- [ ] Line 537: `progress.phase = ClonePhase::Complete` — either read the
  field after assignment or remove it
- [ ] Line 436: `progress.phase = ClonePhase::Complete` — same as above

---

### `atomic-cli/src/commands/clone/mod.rs` — `unused_imports`

Lines 144, 147, 150.

- [ ] Line 144 — remove unused imports:
  - [ ] `CloneOutcome`
  - [ ] `ClonePhase`
  - [ ] `CloneProgress`
  - [ ] `CloneStats`
- [ ] Line 147 — remove unused imports:
  - [ ] `CleanupGuard`
  - [ ] `format_count as helpers_format_count`
  - [ ] `infer_repo_name`
- [ ] Line 150 — remove unused imports:
  - [ ] `DEFAULT_STACK`
  - [ ] `DEFAULT_TIMEOUT_SECS`

---

### `atomic-cli/src/commands/pull/types.rs` — `dead_code`

~4 warnings.

- [ ] `PullOutcome` — never constructed; wire up or delete
- [ ] Associated methods on `PullOutcome` — wire up or delete (~3 items)

---

### `atomic-cli/src/commands/pull/mod.rs` — `unused_imports`

Lines 160, 161.

- [ ] Line 160 — remove unused imports:
  - [ ] `DEFAULT_REMOTE`
  - [ ] `DEFAULT_TIMEOUT_SECS`
- [ ] Line 161 — remove unused imports:
  - [ ] `PullChange`
  - [ ] `PullOutcome`
  - [ ] `PullStats`

---

### `atomic-cli/src/commands/pull/command.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/push/helpers.rs` — `unused_imports`

Line 30.

- [ ] Remove `Merkle` from the `use` statement
- [ ] Remove `NodeId` from the `use` statement

---

### `atomic-cli/src/commands/push/types.rs` — `dead_code`

~5 warnings.

- [ ] `PushStats` — never constructed; wire up or delete
- [ ] `PushOutcome` — never constructed; wire up or delete
- [ ] Associated methods on these types — wire up or delete (~3 items)

---

### `atomic-cli/src/commands/push/mod.rs` — `unused_imports`

Lines 125, 126.

- [ ] Line 125 — remove unused imports:
  - [ ] `DEFAULT_REMOTE`
  - [ ] `DEFAULT_TIMEOUT_SECS`
- [ ] Line 126 — remove unused imports:
  - [ ] `PushChange`
  - [ ] `PushOutcome`
  - [ ] `PushStats`

---

### `atomic-cli/src/commands/push/command.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/add.rs` — `dead_code`

Line 208.

- [ ] `impl Add` builder methods — wire up or delete:
  - [ ] `with_files`
  - [ ] `with_all`
  - [ ] `with_dry_run`
  - [ ] `with_force`
  - [ ] `with_recursive`
  - [ ] `with_directory`

---

### `atomic-cli/src/commands/change/command.rs` — `dead_code`

Line 97.

- [ ] `impl ChangeCmd` builder methods — wire up or delete:
  - [ ] `with_identifier`
  - [ ] `with_stack`
  - [ ] `with_format`
  - [ ] `with_show_deps`
  - [ ] `with_show_hunks`
  - [ ] `with_full_hash`
  - [ ] `with_show_provenance`

---

### `atomic-cli/src/commands/change/types.rs` — `dead_code` + `unused_variables`

Lines 105, 162.

- [ ] Line 105: `offset_str` in `if let Some(offset_str) = s.strip_prefix("@~")`
  — rename to `_offset_str` (or actually parse and use the value)
- [ ] `impl ChangeIdentifier` methods — wire up or delete:
  - [ ] `is_hash`
  - [ ] `is_sequence`
  - [ ] `is_latest`

---

### `atomic-cli/src/commands/init.rs` — `dead_code`

Line 321.

- [ ] `impl Init` associated items — wire up or delete:
  - [ ] `at_path`
  - [ ] `with_stack`
  - [ ] `with_kind`

---

### `atomic-cli/src/commands/log/command.rs` — `dead_code`

Lines 120, 552, 564, 593.

- [ ] `impl Log` builder methods — wire up or delete:
  - [ ] `with_count`
  - [ ] `with_stack`
  - [ ] `with_tags_only`
  - [ ] `with_path`
  - [ ] `with_format`
  - [ ] `with_reverse`
  - [ ] `with_from`
  - [ ] `with_full_hash`
- [ ] `fn format_author` (line 552) — wire up to log output or delete
- [ ] `struct LogOutputConfig` (line 564) — never constructed; wire up or delete
- [ ] `impl LogOutputConfig` associated items (line 593) — wire up or delete:
  - [ ] `new`
  - [ ] `format`
  - [ ] `full_hash`
  - [ ] `hash_length`
  - [ ] `count`
  - [ ] `reverse`
  - [ ] `tags_only`
  - [ ] `path`
  - [ ] `stack`
  - [ ] `from_sequence`

---

### `atomic-cli/src/commands/mv.rs` — `dead_code`

Line 112.

- [ ] `impl Move` associated items — wire up or delete:
  - [ ] `new`
  - [ ] `with_dry_run`
  - [ ] `with_force`

---

### `atomic-cli/src/commands/record/mod.rs` — `dead_code`

Line 263.

- [ ] `impl Record` builder methods — wire up or delete (all `with_*` methods
  on the `Record` struct)

---

### `atomic-cli/src/commands/mod.rs` — `dead_code`

Lines 491, 525, 549.

- [ ] `fn format_size` (line 491) — wire up to a command's output or delete
- [ ] `fn format_count` (line 525) — wire up or delete
- [ ] `fn format_count_auto` (line 549) — wire up or delete

---

### `atomic-cli/src/commands/status.rs` — `dead_code`

~6 warnings.

- [ ] `StatusOutputConfig` struct — never constructed; wire up or delete
- [ ] Builder methods on `StatusOutputConfig` — wire up or delete (~5 items)

---

### `atomic-cli/src/commands/split.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/revise.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/reset.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/remove.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/remote/mod.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/stack/new.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/stack/list.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/stack/switch.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/stack/delete.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/tag/create.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/tag/list.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/tag/show.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/tag/delete.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced item — wire up or delete

---

### `atomic-cli/src/commands/git/parallel.rs` — `dead_code`

~2 warnings.

- [ ] Audit unreferenced items — wire up or delete (~2 items)

---

### `atomic-cli/src/commands/hive/client.rs` — `dead_code` + `unused_imports`

~2 warnings.

- [ ] Audit unreferenced item — wire up or delete (~1 dead_code item)
- [ ] Remove unused import (~1 item)

---

### `atomic-cli/src/output/mod.rs` — `unused_imports`

Lines 61, 65.

- [ ] Line 61 — remove unused imports:
  - [ ] `conflict`
  - [ ] `renamed`
  - [ ] `ColorMode`
  - [ ] `StatusChar`
- [ ] Line 65 — remove unused imports:
  - [ ] `Alignment`
  - [ ] `Column`
  - [ ] `KeyValueTable`
  - [ ] `Row`
  - [ ] `Table`

---

### `atomic-cli/src/output/table.rs` — `dead_code`

~5 warnings.

- [ ] `KeyValueTable` struct — never constructed; wire up to an output site or
  delete
- [ ] Methods on `KeyValueTable` — wire up or delete (~4 items)

---

### `atomic-cli/src/output/progress.rs` — `dead_code`

~13 warnings.

- [ ] Progress-tracking structs — never constructed; wire up to command output
  or delete
- [ ] Methods on those structs — wire up or delete (~12 items)

---

### `atomic-cli/src/error.rs` — `dead_code`

~1 warning.

- [ ] Audit unreferenced variant or method — wire up or delete

---

## Resolution

### Completing a command

When a command module is fully implemented and all of its builder/helper APIs
are actually called from production code paths:

1. Run `cargo clippy -p atomic -- -D warnings` locally and confirm the
   warnings for that command are gone.
2. Tick off every checkbox in the corresponding section(s) above.
3. Delete the section from this document.

### Removing the `#![allow(...)]` block

Once **every** checkbox in this document has been ticked:

1. Delete `atomic/docs/cli-warnings-cleanup.md` (this file).
2. Remove lines 41–50 of `atomic-cli/src/main.rs`:

```atomic/atomic-cli/src/main.rs#L41-50
#![allow(
    dead_code,
    unused_imports,
    unused_mut,
    unused_variables,
    unused_assignments
)]
```

3. Run the full test suite and confirm CI is green:

```/dev/null/shell.sh#L1-3
RUSTFLAGS=-Dwarnings cargo build -p atomic
RUSTFLAGS=-Dwarnings cargo test -p atomic
```
