# Git bridge (#207): workflows tested and failed scenarios

The workflows I ran against #207 (Git Bridge / Git Shim) and the scenarios that failed, as input for strengthening the testing harness.

- **Build under test:** `feat/atomic-sidecar-for-git` @ `990612c` (0.18.3, includes dev `e5d1d3b`).
- **Compared with:** `dev` @ `bcbe2c5` (0.18.2). Where noted, also #212 (`d40cdcc`, bridge opt-in) and #213 (`2272daa`, filter path quoting).
- **Environment:** macOS, git 2.50, debug builds, throwaway repositories under `/tmp`.
- **How agents were tested:** Claude Code hook events were simulated with `atomic agent hooks claude-code <event> --json`: `session-start`, `user-prompt-submit`, file edits, `stop`. An isolated `HOME` was used.
- **References:** expectations cite `docs/RFC-ATOMIC-GIT-CAUSAL-BRIDGE.md` (RFC) and `docs/bridge-operating-guide.md` (guide).
- **Harness:** `tests/harness/46_git_bridge_scenarios.sh` has one section per workflow W1–W13. W10 and W11 run only when `ATOMIC_PREV_BIN` points to a previous release binary. It asserts the expected behaviour, so the section for an open scenario fails until that scenario is fixed. On 990612c, W2–W6 pass and every failure falls in the section for F1–F7 or F9. With #212, W12 passes too. W14–W16 aren't in the harness: F8 needs the pull save path, F10 has unit tests in #213, and F11 is only an error message.

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
| `43_record_status_name_conflict` (#203) | 64/79 (79/79 reported in #203) | `resolved name conflict … matches 0 claimants` |
| `43_git_bridge_operation_recovery`, `45_git_bridge_watch_daemon` | 21 passed + 2 skipped, 16/16 | ✅ |

Some findings are security-sensitive. They were reported privately and aren't included here.
