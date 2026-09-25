# Git bridge (#207): workflows tested and failed scenarios

The workflows I ran against #207 (Git Bridge / Git Shim) and the scenarios that failed, as input for strengthening the testing harness.

- **Build under test:** `feat/atomic-sidecar-for-git` @ `990612c` (0.18.3, includes dev `e5d1d3b`).
- **Compared with:** `dev` @ `bcbe2c5` (0.18.2); for F15 also 0.17.1. Where noted, also #212 (`d40cdcc`, bridge opt-in) and #213 (`2272daa`, filter path quoting).
- **Environment:** macOS, git 2.50, debug builds, throwaway repositories under `/tmp`.
- **How agents were tested:** Claude Code hook events were simulated with `atomic agent hooks claude-code <event> --json`: `session-start`, `user-prompt-submit`, file edits, `stop`. An isolated `HOME` was used.
- **References:** expectations cite `docs/RFC-ATOMIC-GIT-CAUSAL-BRIDGE.md` (RFC) and `docs/bridge-operating-guide.md` (guide).
- **Harness:** `tests/harness/46_git_bridge_scenarios.sh` has one section per workflow W1–W13, W16–W19 and W22–W24. W10 and W11 run only when `ATOMIC_PREV_BIN` points to a previous release binary. It asserts the expected behaviour, so the section for an open scenario fails until that scenario is fixed. On 990612c, W2–W6 pass and every failure falls in the section for F1–F7, F9, F11 or F13–F15. With #212, W12 passes too. W14 and W15 aren't in the harness: F8 needs the pull save path, and F10 has unit tests in #213. `tests/harness/47_git_bridge_history_corpus.sh` covers W20, W21, W25 and W26 (RFC §15 item 4): it runs each history shape through onboarding and through the daily reconcile loop, replays real repositories commit by commit, and after every step restores all tracked files from Atomic and compares them with the Git tree.

---

## 1. Workflows tested

| # | Workflow | Commands | Result |
|---|---|---|---|
| W1 | Onboard an existing Git project | `git init`, commit, `atomic init`, then either `atomic git import` + `atomic git bridge enable --binding-key-file <key>` (RFC §14.4) or `atomic git bridge reconcile` | Anchors, checkpoint written, `bridge verify` passes; **`atomic status` never clean → F2** |
| W2 | Git-first daily loop | edit, `git commit`, `atomic git bridge reconcile` | ✅ Atomic history gains the commit, `bridge verify` passes, both statuses clean |
| W3 | Atomic-first daily loop | `atomic add` + `atomic record`, `atomic git bridge reconcile` twice | ✅ Git HEAD advances to a commit containing the file; the second reconcile creates no commit; both clean |
| W4 | Git branch round trip | `git switch -c feature`, commit, reconcile; `git switch main`, reconcile | ✅ Atomic follows to `feature` and back; feature-only file gone on `main`; both clean. (Fails after the default `atomic init`, see F2.) |
| W5 | Atomic view round trip | `atomic view create topic --parent main`, `view switch topic`, record; `view switch main` | ✅ Git HEAD tree follows both ways; both clean. Git is detached on the Draft view, which guide §4 documents as expected. |
| W6 | Status parity | modify a tracked file; add an untracked file; `git add` it | ✅ `atomic status --git` equals `git status --short` on every step (RFC §9.2) |
| W7 | Agent session in a bridge repository | hooks: session-start → prompt → edit → stop, three turns, no Git commands | **❌ F3** |
| W8 | Agent runs `git commit` inside a turn | same session; turn 1 edits and `git commit`s, turns 2–3 only edit | **❌ F4** |
| W9 | View switch with a user-created untracked file | file created at a path deleted in the target view's history, then `atomic view switch` | **❌ F5** (plain and bridge repositories) |
| W10 | Upgrade from the previous release | repository created by 0.18.2 / 0.17.1, then `status`, `log`, `record` with #207 | **❌ F6** |
| W11 | Older binary used in an upgraded repository | 0.18.2 `atomic add` / `record` in a #207 repository | **❌ F7** |
| W12 | Colocated repository that never enables the bridge | `git init`, `atomic init`, `add`, `record`, `status` | **❌ F1** (fixed by #212) |
| W13 | Concurrent same-path changes into the current view | two repositories each create `same.txt`; `atomic insert` both into one repository | **❌ F9** |
| W14 | Pulled/cloned legacy history | a v1 change through the pull/clone save path | **❌ F8** |
| W15 | Filter drivers with ordinary file names | `.gitattributes` filter driver using `%f`; file names with spaces | **❌ F10** (fixed by #213) |
| W16 | `atomic git import` before the first Git commit | `git init`, `atomic init`, `atomic git import` | **❌ F11** |
| W17 | Onboard a history that edits a file, with `reconcile` | Git history where a later commit edits an earlier file, `atomic init`, `atomic git bridge reconcile` | **❌ F13** |
| W18 | Git branch round trip after importing that history | `atomic git import` + `bridge enable --binding-key-file`; `git switch -c feature`, commit, reconcile; switch back, reconcile | **❌ F14** |
| W19 | Delete one of several lines added together | one file: `c` → `a b c` → `b c`, by `atomic record` and by `atomic git import` | **❌ F15** (found importing a real project; also in 0.17.1 and 0.18.2) |
| W20 | History shapes through the bridge (harness 47) | merge, octopus, rename, delete and re-add, CRLF, binary, symlink, executable bit, path names with spaces or non-ASCII characters, empty commit, submodule; each onboarded and replayed with `reconcile` | merges, renames, CRLF, binary, empty commits and submodules ✅; **❌ F16, F17, F18** |
| W21 | Import public repositories (harness 47, harness 10) | `hashicorp/go-uuid`, `holman/spark` | go-uuid ✅; **❌ F19** (spark) |
| W22 | Edit a lockfile | `Cargo.lock` `a b` → `b` and `a b` → `x b`; hyperfine's real `Cargo.lock` v1 → v2; `git import` of `sharkdp/hyperfine` | **❌ F20** (partly also in 0.18.2) |
| W23 | Publish a Draft view and switch with the bridge | Draft view, record, `atomic git bridge publish feature --branch feature`, `atomic git bridge switch main`, `atomic git bridge switch feature` | ✅ As guide §4 says: publish refuses a view with no projection commit, HEAD stays detached after publish, and `bridge switch` refuses a Draft view without a branch |
| W24 | Share Atomic changes with a teammate through Git | Alice: enable, record, reconcile, `bridge binding publish --with-changes-pack`, push the branch and `refs/atomic/*`. Bob: `atomic clone <git-url>`, or `git clone` + `atomic git import` | **❌ F21, F22** |
| W25 | Git history rewrites and pulls (harness 47) | `commit --amend`, `reset --hard HEAD~1`, `rebase`, `cherry-pick`, `git pull` fast-forward and merge; each onboarded and replayed with `reconcile` | amend, rebase, cherry-pick and both pulls ✅; **❌ F23** (reset) |
| W26 | Real repositories replayed commit by commit (harness 47) | `hashicorp/go-uuid` (30 first-parent commits) and `holman/spark` (68): anchor at the first commit, fast-forward one commit at a time, reconcile, restore and compare after each | go-uuid ✅; **❌ F18** (spark, at its first commit) |

W2–W6 start from a repository anchored with `atomic init --no-vault`, with `.atomicignore` removed, so they don't inherit F2.

---

## 2. Failed scenarios

### F1. A colocated repository that never enabled the bridge refuses every command
- **Steps:** `git init && atomic init && echo x > f.txt && atomic add f.txt` (same with existing Git history).
- **Expected:** the bridge applies only to explicitly configured colocated workspaces (RFC §2). The guide: "The bridge is explicit per-repository opt-in". `[git.bridge] enabled` defaults to `false`.
- **Actual:** `add`, `record`, `status`, `status --json`, `diff`, `stash`, `tag` and agent turn-end fail with `workspace is not anchored to a verified Git baseline: UnbornHead` (or `MissingCheckpoint`).
- **Verified:** run. **Status:** fixed by #212.

### F2. Onboarding never reaches a clean `atomic status`
- **Steps:** W1, by either anchoring path.
- **Expected:** after anchoring, Git and Atomic agree and both report clean (RFC §2.2, §21).
- **Actual:** `git status` is clean, but `atomic status` lists `?? .atomicignore`, plus `?? .vault/...` with the default `atomic init`. Committing `.atomicignore` to Git doesn't help: `reconcile` then fails with `GitIndex/ExclusionPolicy .atomicignore: expected excluded, actual present in index` and writes no checkpoint. With the default `atomic init`, W4 also failed by hand: `reconcile` reported `view 'feature' not found`, then `status` refused with `HeadSymrefChanged`.
- **Verified:** run. **Status:** open. Needs a decision on how `.atomicignore` and `.vault/` are treated in bridge mode.

### F3. An agent session in a bridge repository is refused at every turn end
- **Steps:** W7 in an anchored repository. No Git commands are run.
- **Expected:** each managed agent turn produces an Atomic record (RFC §1.5 outcome 6, §10). The guide (§8) marks a session Incomplete only when a turn ends with unexplained work.
- **Actual:** `session-start` creates the session's draft view and moves the working copy to it. The bridge checkpoint still names `main`, so every boundary returns `AtomicCheckpointDrift`, although `checkpoint_state == desired_state` and only the view name differs. All three turns end `recorded:false`, the session is Incomplete and a WIP ref is written. `atomic status` is refused for the whole session.
- **Verified:** run (0 of 3 turns recorded). **Status:** open.

### F4. After one agent `git commit`, no later turn is recorded
- **Steps:** W8 in a colocated repository that never enabled the bridge. Run with the #212 build so F1 doesn't interfere.
- **Expected:** the turn with the uncaptured commit becomes Incomplete by design (RFC §10.3, §10.5; guide §1 and §8: "synthesized sessions stay incomplete"). Neither the RFC nor the guide says later turns stop being recorded; outcome 6 expects every managed turn to produce a record. The repository also never enabled the bridge (RFC §2).
- **Actual:** turns 2 and 3 (plain edits) are also `recorded:false`, with the same reason as turn 1. Their files stay untracked. Only `atomic agent repair` recovers. Control session without the commit: 3/3 turns recorded.
- **Verified:** run. **Status:** open.

### F5. `view switch` deletes an untracked file the user created
- **Steps:** on view `t`, record `notes.txt` and then its deletion. Back on `main`, create a new `notes.txt` and never add it. `atomic view switch t`.
- **Expected:** the switch preserves private untracked data (RFC §20.1 item 9). Collisions refuse before materialization and preserve every observable state (§20.1 item 13, Learning M15). dev 0.18.2 keeps the file.
- **Actual:** `✓ Switched to view: t (0 files updated, 0 directories)` and `notes.txt` is gone. The only copy is under `.atomic/working-copies/<id>/operation-recovery/...`. Reproduced in a plain Atomic repository and in an anchored bridge repository; in the latter, `git status` also listed the file as `??` before the switch.
- **Cause:** switch removes every path the target view's history deleted (its `absent` projection), whatever is on disk (`switch.rs` ~696–717). That cleans stale bytes (Resolution M18) but doesn't check that Atomic wrote them. Learning M2 ties stale bytes to their file-index row.
- **Verified:** run. **Status:** open.

### F6. Repositories from the previous release fail after upgrading
- **Steps:** W10.
- **Expected:** the new build migrates existing repositories (RFC §14.3), and ordinary commands keep working.
- **Actual:** `status`, `log` and `record` all fail with `PATH_CLAIMS migration required … reopen the repository writable and run the path-claim backfill`. `atomic doctor repair-path-claims` writes the missing rows but doesn't complete the migration, so `status` still fails. Only commands that open through `Repository::open` (e.g. `atomic view list`) migrate the repository; after that, everything works.
- **Verified:** run with 0.18.2 and 0.17.1 repositories. **Status:** open.

### F7. An older binary writes into an upgraded repository; the new build then refuses everything
- **Steps:** W11.
- **Expected:** old clients encountering a bridged repository fail closed via the repository capability fence (RFC §14.3).
- **Actual:** 0.18.2 predates the fence and isn't stopped. It can't read v2 changes ("unsupported format version: expected 1, got 2") but still records a v1 change. The #207 build then refuses every command: `working-copy <ULID> expects view 't' at <hash>, but the view is at <hash>`. `atomic doctor reconcile-working-copy --working-copy <ULID> --target-view <view>` recovers the repository, but the error doesn't mention it. A likely trigger is agent hooks installed globally with an older `atomic`.
- **Verified:** run. **Status:** open.

### F8. Pulled/cloned legacy changes are stored under a different hash
- **Steps:** a v1 change written by 0.18.2 goes through the save sequence of `pull`/`clone` (`save_downloaded_change`: deserialize, verify the hash, `repo.save_change(&change)`) and is then inserted by its advertised hash.
- **Expected:** legacy change bytes stay hash-authoritative and are never re-serialized (RFC §14.3, §16).
- **Actual:** `save_change` re-serializes the change as v2 and stores it under a new hash. The following insert fails with `ChangeNotFound`. Saving the original bytes (`save_change_bytes`) works.
- **Verified:** run through the same save sequence with public APIs; no server was used. **Status:** open.

### F9. A concurrent same-path change inserted into the current view is refused
- **Steps:** W13. The change files were copied between repositories to stand in for a pull.
- **Expected:** concurrent claims on one path are kept as live claims and surfaced as a name conflict (RFC §3.12 N12, §4 invariant 8).
- **Actual:** the second insert fails with `cannot add 'same.txt': path is already bound to graph inode`. The refused insert also leaves its operation open: `atomic log` then fails with `repository operation is still completing; retry with a writable repository open` until a writable command such as `atomic status` completes it. Inserting the same two changes into a *non-current* view correctly produces a name conflict, in either order.
- **Verified:** run. **Status:** open.

### F10. Filter drivers received paths split or interpreted by the shell
- `%f` in `filter.<name>.clean` / `smudge` was substituted unquoted, so `with space.txt` reached the driver as `withspace.txt`.
- **Verified:** run. **Status:** fixed by #213, which quotes the path exactly as Git does.

### F11. `atomic git import` before the first Git commit
- **Expected:** importing an empty repository either succeeds with nothing to import or says the repository has no commits yet.
- **Actual:** `Git error: Could not determine default branch`, with a hint about permissions. The message doesn't say that the repository needs a first commit.
- **Verified:** run. **Status:** open (message only).

### F12. Existing suites on 990612c
| Suite | Result | Note |
|---|---|---|
| `39_git_bridge_mvp` | 120/146 | First failure expects Git on branch `feature` after switching to a Draft view. Guide §4 says a detached HEAD is expected, so this expectation is outdated. The rest is not triaged. |
| `40_git_bridge_materialize_safety` | 70/82 | not triaged |
| `41_git_bridge_hooks` | 13/15 | not triaged |
| `42_git_bridge_guard` | 47/61 | not triaged |
| `11_diff_git_parity` | 44/48 | not triaged |
| `13_import_fidelity` | 45/46 | not triaged |
| `10_git_import` | stops in the hyperfine section (0.18.2: 43/43) | Three failures before that. go-uuid imports 60 changes where the suite expects its 30 first-parent commits; importing every parent is what RFC §5.3 item 4 asks for, so this expectation is outdated. The other two are F19 (spark). Then hyperfine's import fails at its second commit (F20), and because that `atomic git import` isn't guarded, the suite exits and the later sections don't run. |
| `43_record_status_name_conflict` (#203) | 64/79 (79/79 reported in #203) | `resolved name conflict … matches 0 claimants` |
| `43_git_bridge_operation_recovery`, `45_git_bridge_watch_daemon` | 21 passed + 2 skipped, 16/16 | ✅ |

### F13. `reconcile` can't onboard a history that edits a file
- **Steps:** W17. Two Git commits, the second edits `README` from the first. `atomic init`, `atomic git bridge reconcile`.
- **Expected:** reconcile anchors any Git history. Foreign history imports verified against every commit tree (RFC §5.3, §21 item 4).
- **Actual:** `partial import failed after 1 landed commit(s): … staged projection has 1 unresolved name conflict(s) at [README]; refusing to publish`. No checkpoint is written, so later commands are refused with `MissingCheckpoint`. A second commit that adds a different file works. `atomic git import` + `bridge enable` anchors the same history. Seen on `octocat/Hello-World` and on a two-commit local repository.
- **Verified:** run. **Status:** open.

### F14. After importing a history, Atomic doesn't follow a Git branch
- **Steps:** W18. Three commits editing `README.md`; `atomic git import`, `atomic git bridge enable --binding-key-file <key>`; `git switch -c feature`, edit, commit, `atomic git bridge reconcile`; `git switch main`, reconcile.
- **Expected:** Atomic follows to `feature` and back, and both statuses are clean (RFC §1.2, §21 item 1).
- **Actual:** reconcile on `feature` warns `Resurrection <commit>: could not persist its interpretation closure (… already has a different persisted interpretation closure; Git ancestry is immutable, so this is corruption or a conflicting reimport)`, and Atomic stays on `main`. Back on `main`, `atomic status` lists `A README.md` while `git status` is clean. Seen on `octocat/Hello-World` and locally.
- **Verified:** run. **Status:** open.

### F15. Deleting one of several lines added together deletes the others too
- **Steps:** W19. One file goes `c` → `a b c` (two lines added in one change) → `b c`. Recorded with `atomic record` in a plain Atomic repository (no Git), or imported with `atomic git import`.
- **Expected:** the recorded state equals the file, `b c`.
- **Actual:** `atomic record` succeeds, but the recorded state is `c`: `atomic status` shows `M f.txt`, `atomic diff` shows `+b`, and `atomic restore f.txt` writes `c`. `atomic git import` refuses: `staged path 'f.txt' renders 2 byte(s) but the commit tree expects 4 byte(s)`.
- **Not new in #207:** 0.17.1 and 0.18.2 record wrong changes too. In the same repository `atomic restore f.txt` writes `c` (after a further edit to `b c d`, it writes `b b c d`), and replaying the recorded changes into a fresh repository with `atomic insert` gives an order conflict between `b` and `c` that the history never had. Their `status` reports clean, and their `git import` succeeds with the wrong content. #207 is the first build that notices: its status compares against the recorded state, and import verifies every commit against its Git tree (RFC §5.3 item 10). The fix belongs in core record, not in the bridge.
- **Found:** importing a real project (197 linear commits, 879 files) stopped after 50 commits with `renders 5063 byte(s) but the commit tree expects 5001 byte(s)`. That file's history was reduced to the three versions above.
- **Verified:** run. **Status:** open.

### F16. Paths with spaces or non-ASCII characters come back percent-encoded
- **Steps:** W20 `path-names`. Commit `dir with space/ñame é.txt`; onboard, or reconcile after the commit. Then restore the tracked files from Atomic (`atomic restore --force`).
- **Expected:** paths are raw bytes; percent or quoted encoding is display-only and reversible (RFC §5.3 item 9). 0.18.2 restores the path correctly.
- **Actual:** `atomic status` and `bridge verify` fail with `bridge adoption snapshot capture failed: … new attribute path 'dir with space/ñame é.txt' has no FileAdd inode`. The restore writes `dir%20with%20space/%C3%B1ame%20%C3%A9.txt` instead of the real name. The same happens with plain `atomic git import` without the bridge.
- **Verified:** run (harness 47). **Status:** open.

### F17. Symlinks break status and reconcile, and come back as regular files
- **Steps:** W20 `symlink`. Commit a symlink `link -> target.txt`, onboard; later retarget it and reconcile.
- **Expected:** symlinks are carried as attributes and restored as symlinks (RFC §3.5, §5.3 item 9).
- **Actual:** after onboarding, `atomic status` fails with `bridge adoption snapshot capture failed: … attribute path 'link' has no semantic trunk`. After retargeting, `reconcile` fails and later commands are refused with `HeadChanged`. Restoring from Atomic writes a regular file (`git status`: `T link`). 0.18.2 also restores a regular file; the status and reconcile failures are new in #207.
- **Verified:** run (harness 47). **Status:** open.

### F18. Executable bits are lost
- **Steps:** W20 `exec-bit`: onboard a history whose `run.sh` is executable, or `chmod +x run.sh`, commit and `atomic git bridge reconcile`. Then restore from Atomic. W26: the first commit of `holman/spark` has an executable `spark` script.
- **Expected:** the mode is carried as an attribute (RFC §3.5).
- **Actual:** the restored file isn't executable (`git status`: `M run.sh`, mode 100755 → 100644), after onboarding and after reconcile. The spark replay fails at its first commit for this reason. 0.18.2 also loses the bit.
- **Verified:** run (harness 47). **Status:** open.

### F19. Importing `holman/spark` fails in its merge history
- **Steps:** W21. `git clone https://github.com/holman/spark.git` (104 commits, 29 merges), `atomic init`, `atomic git import`.
- **Expected:** every commit imports and verifies against its tree, including merge parents (RFC §5.3 item 4, §21 item 4).
- **Actual:** `partial import failed after 41 landed commit(s): … staged semantic closure for 'spark': a branch is marked Deleted but no applied change deleted it (the global graph proves the line alive and no writer recorded the delete): an unattributed tombstone is corruption`. It fails where three branches leave the same commit. 0.18.2 imports the 68 first-parent commits, and its restored files match Git except for executable bits (F18). The cause isn't isolated yet; #207's new all-parents merge import is the likely area.
- **Verified:** run (harness 47 and `10_git_import`). **Status:** open.

### F20. Recording an edit to a lockfile loses content
- **Steps:** W22. Record a `Cargo.lock`, edit it, record again, and read back what Atomic recorded: restore the file, or replay the recorded changes into a fresh repository with `atomic insert`. Or `atomic git import` hyperfine.
- **Expected:** the recorded state equals the file.
- **Actual:** (replayed into a fresh repository)

  | Edit | 0.17.1 / 0.18.2 | #207 |
  |---|---|---|
  | `a b` → `b` (delete a line) | ✅ | ❌ empty file |
  | `a b` → `x b` (change a line) | ❌ `x` only | ✅ |
  | hyperfine's real `Cargo.lock` v1 → v2 (7293 → 16301 bytes) | ❌ 12 bytes (`[[package]]`) | ❌ 9273 bytes |

  0.18.2's `status` reports clean throughout; #207's reports `M Cargo.lock`. `git import` of hyperfine fails at its second commit, which deletes a checksum line: `staged path 'Cargo.lock' renders 9273 byte(s) but the commit tree expects 16301 byte(s)`. The same content under a name without `.lock` (`x.toml`, `page.txt`) records correctly.
- **Cause:** record treats machine-generated files (`*.lock`, `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, minified output) as opaque and replaces them whole instead of diffing lines (`should_use_opaque_generated_vertices` in `globalize/hunk.rs`, and the fast path in `record/mod.rs`). This was added on dev in `d39dd25` "speed up records". #207 (`2b4a4f9`) routes a modification with existing line state through the normal per-line record. That fixes changing a line (the "12 of 67 bytes rendered" case in its comment) but breaks deleting a line, and the real `Cargo.lock` update is still wrong.
- **Verified:** run. **Status:** open.

### F21. A binding can't carry its changes through Git
- **Steps:** W24. Record a change, then `atomic git bridge binding publish --key-file <key> --with-changes-pack`.
- **Expected:** without an Atomic remote, the binding's `changes.pack` carries the change files (RFC §8.6). The command's help says private sidecars and unhashed bodies never enter the pack.
- **Actual:** `cannot assemble the changes.pack: binding pack object … carries private material that never enters Git (the change carries an unhashed metadata section); private evidence travels only via an Atomic remote (RFC 12.13)`. Every change made with `atomic record` has that section, so a team without an Atomic remote can't publish a pack. Publishing without the pack works.
- **Verified:** run (harness 46). **Status:** open.

### F22. A teammate can't bring a bridge-pushed history into Atomic
- **Steps:** W24. Alice pushes her branch and `refs/atomic/*`. Bob runs `atomic clone <git-url>`; or `git clone`, fetches `refs/atomic/*`, `atomic init`, `atomic git import`.
- **Expected:** `atomic clone <git-url>` bootstraps through the binding (RFC §8.6) and restores Alice's exact change (§21 item 3). When a binding can't be resurrected, import falls back to synthesis (§5.2); trailers are hints (§12 item 8).
- **Actual:** `atomic clone` refuses with `binding … closure is incomplete (2 missing); refusing resurrection: MissingObjects; NoFallbackAvailable`, and leaves a clone where every command is refused with `MissingCheckpoint`. `atomic git import` refuses the pushed commit: `commit … carries an unverifiable Atomic change closure: repository projection failed: view 'main' not found`. Once one person pushes through the bridge, nobody else can import that history.
- **Verified:** run (harness 46). **Status:** open.

### F23. After `git reset` to an earlier commit, reconcile refuses
- **Steps:** W25 `reset`, daily loop. Anchor, commit twice with a reconcile after each, then `git reset --hard HEAD~1` and `atomic git bridge reconcile`.
- **Expected:** Git moved to a commit the bridge already imported, so Atomic adopts that state and both statuses are clean (RFC §1.2, §21 item 1).
- **Actual:** `✓ Imported 0 changes` followed by `✗ Git error: Atomic working copy is not clean`. Atomic still holds the reset commit's change and reads the missing line as unrecorded work. Later commands, `atomic restore` included, are refused with `HeadChanged`. Onboarding the same history after the reset works.
- **Verified:** run (harness 47). **Status:** open.

Some findings are security-sensitive. They were reported privately and aren't included here.
