# Bug: ReviewGate `original_hashes` captures only the first `Atomic-Changes` trailer on squash imports

## Summary

When `atomic git import` classifies a GitHub squash-merge commit that
collapsed a **multi-commit** shadow branch, the resulting `ReviewGate` tag
records only **one** original change hash instead of all of them. The
granular provenance link from the shared view back to the original per-turn
changes is therefore incomplete.

Root cause: `parse_atomic_changes_trailer` returns on the **first**
`Atomic-Changes:` line it encounters, but a squashed commit body legitimately
contains **many** `Atomic-Changes:` lines (one per squashed materialization
commit).

- **Version:** atomic 0.15.6
- **Component:** `atomic-cli` git import classifier
- **Impact:** silent, partial provenance loss on the target view (no error,
  data on disk is fine — only the ReviewGate → originals link is truncated)

## Environment

- atomic 0.15.6
- Repo running git-shadow-sync (both `.git/` and `.atomic/`), squash-merge
  workflow on GitHub.

## Reproduction

1. Shadow branch accumulates several materialization commits, each carrying its
   own `Atomic-View` / `Atomic-State` / `Atomic-Changes` trailer block.
2. Squash-merge the branch on GitHub. GitHub concatenates every squashed
   commit's body, so the resulting merge commit contains **multiple**
   `Atomic-Changes:` lines, e.g.:

   ```
   Materialize session view for git shadow sync snapshots (#2)

   * chore(atomic): materialize session view for git shadow sync
   Atomic-View: old-forest-591e
   Atomic-State: NKEJLUQB6ZOP...
   Atomic-Changes: 3FYFZISLWHD5ENCMB34H32MX7V3RMIXNTH6MEMSVLKEBSKDSJAFA

   * chore(atomic): materialize session view for git shadow sync
   Atomic-View: calm-violet-8d7f
   Atomic-State: VXJOHMJMCNM6...
   Atomic-Changes: TDD7KBU7RWXAZ5MHBNJLP3LDQZSD65TDSSZ5DXNPTWBUWV4EXRBQ

   ... (several more Atomic-Changes lines) ...

   Atomic-Changes: KBDY3IYTUXPAQINT2K4FOW37FW4GLAXPCWOQIQ5NK4D5LWMS4KMQ, EQU5BW25HXOY3A5VCIMNSYKREXF4LP2MTIH32WL72TI75UQ2S4TQ
   ```

3. On the correct branch, run:

   ```
   atomic git import --incremental --branch main
   ```

4. Inspect the ReviewGate (from the `main` view):

   ```
   atomic view switch main
   atomic tag show pr-2
   ```

## Observed

```
Tag: pr-2
View: main
Message: Squash merge 97ccfe6d
Kind: review-gate
Metadata: {"changes":{"original_hashes":["3FYFZISLWHD5ENCMB34H32MX7V3RMIXNTH6MEMSVLKEBSKDSJAFA"]},
           "git":{"merge_strategy":"squash","pr_number":2,"sha":"97ccfe6d..."}}
```

`original_hashes` contains a single hash — the **first** `Atomic-Changes` entry
in the commit body (from `old-forest-591e`). All later `Atomic-Changes` lines,
including the tip work `KBDY3IYTUXPA...` / `EQU5BW25HXOY...`, are dropped.

## Expected

`original_hashes` should contain **every** hash across **all** `Atomic-Changes:`
lines in the squash body, so the ReviewGate links back to the full set of
original changes the squash represents.

## Root cause

`atomic-cli/src/commands/git/parallel.rs`, `parse_atomic_changes_trailer`
(around line 3631):

```rust
fn parse_atomic_changes_trailer(message: &str) -> Option<Vec<String>> {
    for line in message.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Atomic-Changes:") {
            let hashes: Vec<String> = rest
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !hashes.is_empty() {
                return Some(hashes);   // <-- returns on the FIRST trailer line
            }
        }
    }
    None
}
```

It returns as soon as one non-empty `Atomic-Changes:` line is parsed, so only
the first block's hashes survive. `classify_commit` (line ~3611) then stores
that partial list as the squash's `original_hashes`, and
`classify_and_tag_imports` (line ~3502) writes it into the ReviewGate metadata.

Note the sibling parser `parse_push_trailer` (line ~3895) already handles the
squash shape differently — it reads only the *last paragraph* to avoid embedded
trailers. The two parsers disagree about which `Atomic-Changes` block is
authoritative, which is worth reconciling as part of the fix.

## Suggested fix

Accumulate across all `Atomic-Changes:` lines instead of returning on the
first:

```rust
fn parse_atomic_changes_trailer(message: &str) -> Option<Vec<String>> {
    let mut all: Vec<String> = Vec::new();
    for line in message.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Atomic-Changes:") {
            all.extend(
                rest.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }
    }
    // Optionally de-duplicate while preserving order if the same hash can
    // appear across blocks.
    if all.is_empty() {
        None
    } else {
        Some(all)
    }
}
```

Decisions for the maintainer:
- Whether to de-duplicate hashes that repeat across blocks.
- Whether to preserve order (first-seen) or sort.
- Whether `parse_atomic_changes_trailer` and `parse_push_trailer` should share
  a single trailer-extraction routine so their notions of "which trailers
  count" stay consistent for squash commits.

## Suggested test

Add a unit test alongside the existing `test_parse_push_trailer_*` tests that
feeds a multi-block squash body and asserts all hashes are collected:

```rust
#[test]
fn test_parse_atomic_changes_trailer_collects_all_blocks() {
    let msg = "\
squash (#2)

* materialize
Atomic-View: a
Atomic-State: S1
Atomic-Changes: AAA, BBB

* materialize
Atomic-View: b
Atomic-State: S2
Atomic-Changes: CCC
";
    let hashes = parse_atomic_changes_trailer(msg).expect("should parse");
    assert_eq!(hashes, vec!["AAA", "BBB", "CCC"]);
}
```

## Impact / workaround

- No data loss: the original changes still exist in the graph and remain
  reachable by hash; only the ReviewGate → originals link is truncated.
- Workaround until fixed: do **not** delete the source draft views after a
  multi-commit squash import, so the full provenance stays reachable through
  the views. Re-running the import after the fix would need to rebuild the
  ReviewGate metadata (retroactive repair of existing tags is a separate
  question worth calling out).
