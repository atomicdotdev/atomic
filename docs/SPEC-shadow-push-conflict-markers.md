# Spec: Shadow git-push must never commit unresolved conflict markers

- **Status:** Proposed
- **Severity:** Critical (silent working-tree corruption committed to git branches)
- **Component:** `atomic-cli` git-shadow-sync (`atomic git push` materialization) +
  cross-view materialization
- **Version observed:** atomic 0.15.6
- **Related:** `BUG-review-gate-original-hashes.md` (the parser bug is a
  downstream symptom; this is the root-cause class)

---

## 1. Problem statement

In a git-shadow-sync repository, the turn-end hook materializes the current
Atomic view into a git commit via `atomic git push --no-push`. When a
view's working copy contains **unresolved conflict markers** (produced by
cross-view materialization of conflicting changes), the shadow push commits
those markers verbatim into git. The result is git branches whose source files
contain lines like:

```
>>>>>>> 1
  onOpenRemoteUpdate: () => void;
======= 1 [C2YTBAHQ]
  remoteUpdateError: string | null;
======= 1 [UDILON2P]
};
<<<<<<< 1
```

These branches do not compile, are indistinguishable from real work in git
history, and silently diverge the git tree from any coherent Atomic state. In
the observed incident this corrupted **53+ source files** across an entire
application (`App.tsx`, `Header.tsx`, `ModelControls.tsx`, `useConfigStore.ts`,
etc.), produced broken draft branches, and — because the hook keeps
re-materializing — repeatedly re-committed the corruption onto `main` after
each `git reset --hard` (commit `6e7b4e92`-style re-drift).

## 2. Why this is the true root cause

Several confusing downstream symptoms all trace to this one behavior:

- `atomic git import` refusing with *"inserting its originals diverges from the
  git tree"* — the git tree contained committed conflict markers, so it could
  never match a clean Atomic insert.
- Draft branches that "conflict with main" — they carry committed markers.
- The re-materialization loop that re-dirtied `main` after every reset.

Fixing the ReviewGate trailer parser (companion report) does not address this;
it only makes provenance links complete once the tree is clean. **This spec is
the corruption source.**

## 3. Root cause in code

Two guards exist for `atomic record`, but the shadow push bypasses both.

### 3.1 `atomic record` already rejects conflict markers (the correct precedent)

- `atomic-repository/src/record/options.rs:93-99` — `allow_conflict_markers`
  option, **defaults to `false`**.
- `atomic-repository/src/record/mod.rs:148-154` — recording a file that still
  contains conflict markers is an error:
  `"{path} still contains conflict markers at line {line} (resolve the
  conflict, or pass --allow-conflict-markers to override)"`.
- `atomic-cli/src/error.rs:260` / `:601` — CLI surfaces this and instructs the
  user to remove `>>>>>>>` / `=======` / `<<<<<<<` lines.

So the record path treats markers as a hard stop unless explicitly overridden.

### 3.2 `atomic git push` (shadow materialization) has NO such guard

`atomic-cli/src/commands/git/push.rs:135-160` stages and trees the working copy
directly, with no conflict-marker inspection:

```rust
// Stage everything: git add -A
index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
index.update_all(["*"].iter(), None)?;
index.write()?;
let tree_oid = index.write_tree()?;   // <-- whatever is on disk, markers included
let tree = git_repo.find_tree(tree_oid)?;
```

The subsequent commit (push.rs, later in `run`) records this tree with the
`Atomic-View` / `Atomic-State` / `Atomic-Changes` trailers. **Nothing between
`add_all` and the commit checks whether any staged file contains conflict
markers.** The only skip condition is "tree identical to HEAD" (push.rs:169-188)
— which does not fire when the drifted, marker-laden tree genuinely differs
from HEAD.

### 3.3 Upstream: cross-view materialization emits the markers

Materialization is *allowed* to write conflict markers for human resolution
(by design):

- `atomic-core/src/apply/conflict.rs:15` — "Allowing output with conflict
  markers for human resolution".
- `atomic-core/src/merge/engine.rs:41`, `atomic-core/src/merge/resolved.rs:52`,
  `atomic-core/src/output/alive/order.rs:44` — SCC/cyclic conflicts are written
  with markers.
- `atomic-repository/src/archive.rs:64` — same marker convention.

The markers themselves are legitimate (they are how Atomic asks a human to
resolve a conflict). The defect is that an **automated background commit path**
(the shadow hook) treats a working copy that is *pending human conflict
resolution* as if it were a clean, commit-ready snapshot.

## 4. Marker format to detect

Atomic's conflict markers are numbered and change-hash-tagged (not classic git
3-way markers):

```
>>>>>>> <N>
=======  <N> [<CHANGE_HASH_BASE32>]
<<<<<<< <N>
```

Detection must match `>>>>>>>`, `<<<<<<<`, and `======= N [hash]` at
line-start. The record-path detector (`record/mod.rs`) already locates these
"at line {line}"; the shadow push must reuse the **same** detector so the two
paths cannot disagree.

## 5. Required behavior

### 5.1 Invariant

> The shadow materialization path (`atomic git push`, including `--no-push`)
> MUST NOT create a git commit from a working copy that contains unresolved
> conflict markers, unless conflict-marker commits are explicitly enabled.

### 5.2 Default behavior on markers detected

1. **Do not stage, tree, or commit.** Abort the push before `write_tree`.
2. Exit non-zero with a clear, actionable message naming the first conflicted
   file and line, and instructing the user to resolve markers then re-run.
3. When invoked from the turn-end hook (non-interactive), **log to
   `.atomic/hook-errors.log`** with a distinct tag (e.g. `shadow-conflict`) and
   leave both git and the working copy untouched. The hook must surface a
   visible warning, not fail silently.
4. Leave `git status` and `atomic status` exactly as they were (no partial
   stage, no partial commit).

### 5.3 Explicit override

Provide an opt-in flag mirroring record's `--allow-conflict-markers` (e.g.
`atomic git push --allow-conflict-markers`) for the rare case where markers are
legitimate file content. Default is `false`. The hook MUST NOT pass this flag.

### 5.4 Consistency requirement

`atomic git push` and `atomic record` MUST share one conflict-marker detector
and one default policy. It is a defect for `record` to reject markers while
`git push` commits them.

## 6. Acceptance criteria

- [ ] With a view whose working copy contains conflict markers,
      `atomic git push --no-push` creates **no** commit, exits non-zero, and
      names the conflicted file + line.
- [ ] The turn-end hook, on the same state, records a `shadow-conflict` entry in
      `.atomic/hook-errors.log`, creates no commit, and mutates neither git nor
      the working copy.
- [ ] `atomic git push --allow-conflict-markers` still commits (explicit
      override only).
- [ ] A clean working copy (no markers) pushes exactly as before — no
      regression to the normal shadow loop.
- [ ] `atomic record` and `atomic git push` produce identical marker detection
      (same file/line reported) for the same working copy.
- [ ] Reproduction from the incident: a cross-view materialization that writes
      markers to 50+ files does not result in any of those files being committed
      by the shadow hook.

## 7. Test plan

Unit:
- Marker detector: numbered/hash-tagged markers (`>>>>>>> 1`,
  `======= 1 [ABC123]`, `<<<<<<< 1`) at line start are detected; legitimate
  content containing `=======` mid-line is not misdetected.
- `push::run` aborts before `write_tree` when detector returns a hit.

Integration:
- Materialize a synthetic conflict (two views editing the same span, cross-view
  insert) so the working copy gains markers; assert `atomic git push --no-push`
  makes no commit and the tree/HEAD are unchanged.
- Assert `--allow-conflict-markers` path does commit.
- Assert hook path writes `shadow-conflict` to the error log and is idempotent
  (re-running does not accumulate commits).

## 8. Risks / non-goals

- **Non-goal:** changing whether cross-view materialization *emits* markers.
  Markers-for-human-resolution is intended; this spec only stops the automated
  commit of them.
- **Risk:** a stricter push could block a user mid-flow if a view legitimately
  contains `=======`-like content; the shared detector + `--allow-conflict-markers`
  override mitigates this, matching existing `record` behavior.
- **Migration:** existing branches already polluted with markers are out of
  scope; they are cleaned up by discarding the affected draft branches/views and
  resetting git to the last clean published state.

## 9. Incident evidence (for the fix author)

- Corrupted working-tree sample (Header.tsx) showing `>>>>>>> 1` /
  `======= 1 [C2YTBAHQ]` / `<<<<<<< 1` with scrambled field order.
- 53+ source files carried markers on the draft-view chain
  (`git grep -lE '^(>>>>>>> [0-9]|<<<<<<< [0-9]|======= [0-9] \[)'`).
- Shadow materialization commits (`chore(atomic): materialize session view for
  git shadow sync`, e.g. `6e7b4e92`, `76f295ca`) committed these files with
  full `Atomic-View`/`Atomic-State`/`Atomic-Changes` trailers.
- `atomic git import` later refused with *"inserting its originals diverges from
  the git tree ... skipping"* — a direct consequence of the committed markers.
