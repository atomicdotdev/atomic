# Real-repository Git shim integration

## Harness 47 completed after approved fork reset (2026-10-27)

The user explicitly approved preserving the personal fork's current tip,
resetting only its `release` ref to upstream, and rerunning harness 47.
Before resetting, the existing tip
`2108be37f5c1221cac39e1914a83f56f6bb6b400` was saved and verified on
`leefaus/train-tracks:shim-backup-before-47-20261027T000004Z-2108be37f5c1`.
The personal `release` and upstream tip were rechecked for drift before the
reset; no upstream write or deletion was performed.

**Harness 47 passed, exit 0 after all 51 steps**, with
`TRAIN_TRACKS_PATH_CASE=all` and `PUSH_TRAIN_TRACKS_SHIM=1`, using this clone's
explicit local debug executable. It covered:

- Import, signed shim enablement and installed hooks pointing at the local build.
- Native records, content rewrite and rename without a trailing newline.
- New Git branch, branch return and fast-forward merge with reconciliation.
- Real push through the pre-push hook to the unique personal-fork branch
  `shim-test-20261027T000040Z-6349`.
- Literal percent plus Unicode/space filenames, empty lockfile regeneration,
  ignored/untracked noise, detached checkout and return.
- Disable removing all five owned hooks, clean final Git/Atomic statuses and
  passing final verification.

The run still emitted advisory dropped-unverifiable-`GIT_SHA_INDEX` cache-row
warnings during branch returns. Final verification passed; no cache-repair or
new implementation work was attempted to suppress these warnings.

The run's binary SHA-256 was
`1391f00ee691eb98410b3dd41c7c66b9a784e1c69286b89f7dd61c111721b3aa`.
Evidence under the approved temp directory:

- `atomic-train-tracks-OTgemU/`: retained clone, binary identity, settings,
  command logs and `result.txt` (`exit=0`, `last_step=51`).
- `shim-2-harness-47-reset-z87u19ve/`: approved reset plan, backup creation
  response, reset response, full runner log and `final-verification.json`.

Final GitHub API checks verified that the backup still points at `2108be37...`
and both personal/upstream `release` refs point at
`dddbe3e1434e8fbad4c8aba4b875b1726707f3d5`. The prior harness-48 PR #3/#4
history remains reachable from the backup and retained topic branches. No PR,
branch, fork or fixture was deleted. **Harnesses 47 and both modes of 48 have
now passed in atomic-shim-2**, with their different input baselines and exact
binary identities recorded separately.

## Fresh remote validation — atomic-shim-2 (2026-10-26)

The user authorized harnesses 47/48 after the intent-5 percent-path correction.
The authenticated account was `leefaus`; all test writes targeted the verified
personal fork `leefaus/train-tracks`. No source or harness code was changed
during these runs. Each suite built and pinned this clone's explicit debug
binary, reporting `atomic 0.20.0 (dev e8378462c0ed-dirty)`, SHA-256
`310480898f531687cc72edd5e19806612af44480c52c8fd5782ffae6859957c1`.

| Run | Actual result |
| --- | --- |
| Harness 47, initial attempt before approved reset | BLOCKED: exit 1 after build/version and fork checks, before cloning or running shim scenarios. Personal `release` was already `bfc818716850b1c3ec9aeeabfbd1f69282488458`, while upstream was `dddbe3e1434e8fbad4c8aba4b875b1726707f3d5`. The safety refusal was not bypassed; the later successful run is recorded above. |
| Harness 48, Git-authored | PASS, exit 0: push, [PR #3](https://github.com/leefaus/train-tracks/pull/3), real merge, existing reader pull, incremental import, checkpoint/content/clean-status verification and unchanged-history repeat. Includes percent, escape-looking and Unicode/space filenames. |
| Harness 48, Atomic-authored | PASS, exit 0: native record projected to Git, push, [PR #4](https://github.com/leefaus/train-tracks/pull/4), real merge, existing reader pull, incremental import, checkpoint/content/clean-status verification and unchanged-history repeat. |

The merge SHAs were `abc9b8a0994cbdbaee6c07697a913d93db5a46ee` (PR #3)
and `2108be37f5c1221cac39e1914a83f56f6bb6b400` (PR #4). At completion of these
lifecycle runs, personal `release` had intentionally advanced to the latter.
Both lifecycle runs compared
all upstream branch-ref observations before/after and found no changes; a
subsequent API check confirmed upstream `release` still at `dddbe3e1434...`.
During these lifecycle runs, no fork was reset, deleted or recreated and no
test branch/PR evidence was deleted. The initial harness-47 blocker was later
resolved by the separately approved backup/reset and successful run above.

The Atomic-authored identity audit again records
`original_change_in_reader_view: false`: this is a successful content/state
round trip through plain Git, not restoration of the author's original Atomic
change identity or provenance. No new identity-restoration implementation was
attempted.

Evidence under the approved temp directory:

- `atomic-train-tracks-9AbxQM/`: harness-47 refusal, build identity and exit 1.
- `atomic-train-tracks-pr-47mMEv/`: Git-authored lifecycle, exit 0, command logs,
  PR/merge responses, reader checkpoints and history.
- `atomic-train-tracks-pr-RixRMa/`: Atomic-authored lifecycle, exit 0, the same
  checks plus `identity-audit.json`.
- `shim-2-remote-validation-nfxgk7wa/`: Atomic-mode driver log and
  `verification-summary.json` comparing successful exits and upstream refs.

An initial lifecycle launch used a mistyped `TMPDIR` and stopped at `mktemp`
before fixture creation or remote mutation; the corrected invocation produced
the Git-authored pass above. Private binding keys in retained fixtures must
not be published.

## Intent 5 — percent-path correction (2026-10-26)

The clean clone's missing percent-path behavior is now corrected in the three
existing path-boundary consumers: repository status, the Git-index status
overlay and staged semantic path lookup. No `atomic-core`, importer,
batch-writer, storage or merge-policy code was changed.

Three new regressions were run against the unfixed implementation and failed:
initial import/status of nested percent/Unicode/symlink paths, single-percent
reconciliation, and escape-lookalike reconciliation. All three now pass after
the correction. They check physical path filtering, real edits/deletions,
distinct lookalike bytes, Git edit/rename/delete, current checkpoint and clean
statuses, no escaped physical sibling, and idempotent repeated reconciliation.

Fresh validation completed with the explicit
`/Users/leefaus/Projects/atomic-shim-2/target/debug/atomic` build:

| Check | Result |
| --- | --- |
| `cargo build --locked -p atomic-cli --bin atomic` | PASS |
| `git_import_path_audit_test` | 15 passed |
| `git_shim_branch_reconcile_regression` | 4 passed |
| `git_shim_workspace_test` | 4 passed |
| `git_shim_opt_in_test` | 9 passed |
| `adaptation_regression_test` | 6 passed |
| Repository `status::change_source_convergence_tests` | 4 passed |
| Offline harness 49 | 17 passed |
| Formatting of all five allowed Rust files; diff whitespace check | PASS |
| Rust allowlist and transferred-script integrity | Only the five allowed Rust files changed; all five harness/helper scripts unchanged |

That is **42 focused Rust tests and 17 offline configuration tests**. The
repository tests include both native/non-enrolled Git percent-name behavior,
decode-once and invalid decoded path checks, and the existing candidate-source
convergence and no-telemetry-write regressions.

A separate fresh single-percent smoke fixture verifies every shim layer,
both clean statuses, exact `100%.txt` bytes, no `100%25.txt` physical sibling,
and identical Atomic history JSON and Git HEAD after a second reconcile.
No remote is configured or contacted in this fixture. The tested binary's
SHA-256 is
`0053a3d2f6826cd3cc8208f4a1168ae4ca06ec9ddde36e2fcdba5cc0a8937206`.

The remote harnesses were not included in this local-validation batch; their
subsequent actual results are recorded above. The full workspace test suite
was not run. No performance optimization or benchmark was attempted.

Fresh intent-5 evidence is retained at
`/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/shim-2-intent-5-gqbgmu6z/`:
`pre-fix-regressions.log`, `post-fix-percent-regressions.log`,
`validation-results.json`, `scope-check.json`, `harness-integrity.json`,
`idempotence-results.json` and `binary.sha256`. Original failed fixtures in
`shim-2-baseline-fjnsgtnm/` remain intact; their failures below describe the
pre-fix clean baseline, not the corrected code.

## Clean-clone restoration — atomic-shim-2, 2026-10-26

Only the seven reviewed non-Rust harness/helper/documentation files from the
original intents 1–4 were transferred. No Rust code, old `.atomic` database,
old vault/memories or build artifacts were transferred. Four fresh intents
track this restoration in the new clone.

The clean source is `e8378462c0ed12630fd1b963f7957287c927e21a`. A new
`cargo build --locked -p atomic-cli --bin atomic` completed in 61 seconds.
At that pre-fix restoration checkpoint, all 1,019 tracked Rust files retained
their starting hashes. That locally built binary reported
`atomic 0.20.0 (dev e8378462c0ed-dirty)`; its dirty tree included non-Rust
restoration and fresh vault metadata, not Rust modifications.

Initial, pre-fix local validation:

| Check | Result |
| --- | --- |
| Bash syntax: harnesses 47–49 and shared helper | PASS |
| Harness 49, offline configuration/safety tests | 17 passed |
| Existing `git_import_path_audit_test` | 14 passed |
| Existing `git_shim_branch_reconcile_regression` | 2 passed |
| Harnesses 47/48 without network opt-in | Correctly exit 77; not integration passes |
| Single `100%.txt`, locally imported/enabled shim | FAIL: import lands, reconcile exits 4 with `Atomic working copy is not clean`; ordinary status refuses the stale checkpoint |
| Distinct `100%.txt`, `a%41.txt`, `aA.txt`, `a%2541.txt` in a separate fixture | FAIL: staged semantic closure rejects mismatched trunk identity, reconcile exits 128 |
| Real GitHub push/PR/merge/pull in this clone | NOT RUN; requires explicit personal-fork mutation authorization |

At this stage the original intent-2 percent-path Rust fix was **not established
in the clean clone**. Transfer and compilation did not recover that fix, and no
Rust correction was made during the non-Rust-only restoration. Fresh intent 5
subsequently corrected it as documented above.

Evidence is retained at
`/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/shim-2-baseline-fjnsgtnm/`:
`transfer.json`, `rust-before.json`, `percent-results.json`,
`percent-single-results.json`, per-command fixture logs and both local fixtures.
Binding keys are private and must not be published. The binary used for the
single-percent check has SHA-256
`a2666ab39fdcb73bb25ac69d76b4c51fc55fdc58abc1e3b3128187e7ad6f9348`.

**The earlier measured results below belong to the original workspace and its
explicitly identified binaries. They are historical evidence, not fresh-clone
validation.**

`47_git_shim_train_tracks.sh` follows `docs/shim-operating-guide.md` using a
fresh clone of **your-personal-account/train-tracks**, forked from
**atomicdotdev/train-tracks**.
It never pushes to upstream and never deletes a fork or existing local test state.
Ordinary harness runs skip this suite (exit 77).

## Run

Prerequisites: macOS/Linux, Cargo, Git, Python 3, and `gh` authenticated with
**your own personal GitHub account**, with permission to create forks and push.

Each opted-in GitHub suite asks:

```text
Personal GitHub username or profile URL [your-authenticated-login]:
```

Enter your username or `https://github.com/your-username`, or press Enter to
confirm the authenticated account shown. The suite creates or validates
`<your-username>/train-tracks` and uses only that fork for clones, pushes, PRs,
and merges. The selected personal account must match `gh api user`; use
`gh auth switch` or `gh auth login` first if the wrong account is active.
Organization/upstream namespaces, repository URLs, unrelated repos, and forks
without push permission are refused. No access to the historical tester's
personal GitHub account is needed.

For automation or a noninteractive shell, set `TRAIN_TRACKS_GITHUB_OWNER`
explicitly; without it, a noninteractive run stops with instructions instead
of silently choosing an account. It accepts the same username/profile URL:

```sh
# Set TMPDIR to your preferred test-artifact directory if needed.
# Interactive: asks for your personal GitHub account.
RUN_TRAIN_TRACKS_SHIM=1 bash tests/harness/47_git_shim_train_tracks.sh

# Noninteractive: substitute YOUR authenticated username (or profile URL).
TRAIN_TRACKS_GITHUB_OWNER=your-github-username RUN_TRAIN_TRACKS_SHIM=1 \
  bash tests/harness/47_git_shim_train_tracks.sh

# Also exercise a real GitHub push, including the installed pre-push hook:
RUN_TRAIN_TRACKS_SHIM=1 PUSH_TRAIN_TRACKS_SHIM=1 \
  bash tests/harness/47_git_shim_train_tracks.sh

# Narrow filename coverage in a fresh run when investigating a failure:
RUN_TRAIN_TRACKS_SHIM=1 TRAIN_TRACKS_PATH_CASE=unicode \
  bash tests/harness/47_git_shim_train_tracks.sh
```

`TRAIN_TRACKS_PATH_CASE` accepts `all` (default), `unicode` (spaces plus Unicode),
`percent`, or `none`. This is an explicit coverage selection, not an expected-fail
override: every selected check must pass and failures always return nonzero.

The suite always builds `target/debug/atomic` with `cargo build --locked -p
atomic-cli --bin atomic`, invokes that executable by absolute path, and checks
that installed hooks embed it. It intentionally ignores `ATOMIC_BIN` overrides
and does not put this binary on PATH. The installed PATH `atomic` remains the
tool for tracking the Atomic source project, not the implementation under test.

Both GitHub suites use the shared `train_tracks_github.sh` helper. The GitHub
API creates the selected personal fork only if it is absent. An existing repo must be
the correct fork and its default branch must still match upstream. The suite
does not automatically reset a stale fork. Each run uses a fresh local clone;
there is usually no need to delete the fork between runs because tests only
publish an optional unique `shim-test-*` branch, never the default branch.

## Checks and evidence

- Full Git import; explicit shim enablement with a private external binding key;
  remote fetch rules, reconciliation, verification, and all five owned hooks.
- Native Atomic records, rewriting content, and rename without a final newline;
  Git projection bytes and both clean statuses.
- New Git branch after Atomic records, checkout back, and fast-forward merge
  into the original branch, with reconcile/verify at each boundary.
- Empty lockfile regeneration and filenames containing spaces, Unicode, and `%`.
- Untracked/ignored noise; detached ancestor checkout and return.
- Optional push of a non-current reconciled branch to the personal fork.
- Disable removes all owned hooks; upstream default branch SHA is unchanged.

The suite stops at the first failed command or assertion. It prints a private
artifact directory containing the clone, per-command output logs, exit status,
source commit, binary digest, platform, and upstream ref observations. **Both
successful and failed runs are retained.** Do not publish `binding.key` or
unfiltered private state. A failure is not automatically repaired by deleting
`.atomic`, `.vault`, or Git objects.

The operating guide takes precedence over stale text in the older testing
walkthrough (notably its old new-branch failure and disable-hook descriptions).
This integration is not an exhaustive run of every regression, agent workflow,
watcher, migration, or remote receiving-side enforcement test.

## Clean restart

Run the suite again for a fresh local clone. To recreate the remote fork too,
first inspect your selected `<your-username>/train-tracks` and save any evidence
or work worth keeping.
Only then explicitly delete **the personal fork**, for example through its
GitHub settings after checking the owner. Never delete the upstream repository.
The next opted-in GitHub suite run will recreate
the fork via the API. Failed local clones remain available for investigation.

## GitHub PR → merge → pull → incremental import

`48_git_shim_github_lifecycle.sh` tests a second, already-imported clone pulling
a real server-side PR merge. **It intentionally advances the personal fork's
default branch**, unlike suite 47. This repository calls that branch `release`,
not `main`; no upstream ref or PR is touched.

This suite asks for the same personal account as suite 47, or uses your explicit
`TRAIN_TRACKS_GITHUB_OWNER` value. It can create your fork if you run it first.

```sh
# Git-authored commit (includes percent/Unicode filename coverage):
RUN_TRAIN_TRACKS_LIFECYCLE=1 bash tests/harness/48_git_shim_github_lifecycle.sh

# Atomic-native record projected to Git:
RUN_TRAIN_TRACKS_LIFECYCLE=1 TRAIN_TRACKS_AUTHOR_MODE=atomic \
  bash tests/harness/48_git_shim_github_lifecycle.sh

# Automation: no personal account prompt.
TRAIN_TRACKS_GITHUB_OWNER=your-github-username RUN_TRAIN_TRACKS_LIFECYCLE=1 \
  bash tests/harness/48_git_shim_github_lifecycle.sh
```

Run these **serially**, since both target the fork's default branch. Each run
creates new isolated author/reader clones and separate external binding keys,
builds and pins the local debug binary, verifies hooks reference it, and uses
the GitHub API with the explicitly selected `<your-username>/train-tracks` repository for PR
creation and merge. It retains PR URLs/responses, all command output, source
and binary identities, the imported reader's original history/checkpoint, and
failure diagnostics.

The reader imports before the topic exists. After merge, the suite checks a
real `git pull --ff-only origin release`, exact merge parents and tree, explicit
`atomic git import --incremental --no-vault --branch release`, clean statuses,
checkpoint alignment, retention of pre-existing change hashes, and a second
import that must not change history. The merge method is a merge commit, not a
local fast-forward simulation, squash, or rebase.

This opt-in can leave an open PR or a merged default branch even if a later
check fails; it never resets those effects or deletes evidence automatically.
After a merged run, suite 47 deliberately refuses the fork's changed baseline.
Inspect/save the PR evidence before explicitly recreating the disposable fork
if you want suite 47's original pristine upstream baseline again. The lifecycle
suite itself can run against its previously merged baseline.

Shared account selection and safety guards have offline regression coverage:

```sh
bash tests/harness/49_train_tracks_github_config.sh
# Or run_all.sh 49; no GitHub authentication or network access is needed.
```

These tests simulate different users, a terminal prompt and its confirmed
default, noninteractive configuration, account mismatch/authentication failure,
fork creation versus permission errors, invalid owners, and PR head/base guards.
Portability validation passed all **17 offline tests**, Bash syntax checks, and
runner classification (47/48 skipped without opt-in, 49 passed). No real GitHub
mutations were rerun for this configuration change.
The results below are historical evidence from the original tester's personal
fork; those usernames and PR URLs are not configuration defaults.

### Remote lifecycle results — 2026-10-26

Tested with the explicit local debug executable reporting
`atomic 0.20.0 (dev e9d57bdb06f1-dirty)`, source base
`e9d57bdb06f10f724d71486305efe757f8dcfdcf` plus the percent-path fix, Darwin
25.4.0 arm64, `ATOMIC_SERVICE=local`. Binary SHA-256:
`2c133b62131184d5428a83c43f2c245d0df16a180b90e17473073f716442bb6d`.

| Operation | Git-authored, PR #1 | Atomic-projected, PR #2 |
| --- | --- | --- |
| Push feature branch through the local debug pre-push hook | PASS | PASS |
| Create and merge a real GitHub PR into personal `release` | PASS | PASS |
| Pull into a clone imported/enabled before PR creation | PASS | PASS |
| Exact merge parents and tree match | PASS | PASS |
| Explicit `atomic git import --incremental --no-vault --branch release` | PASS, 1 added change | PASS, 1 added change |
| Keep existing reader history, advance checkpoint, clean both statuses, verify | PASS | PASS |
| Repeat incremental import without changing history | PASS, 0 changes | PASS, 0 changes |
| Upstream branch refs unchanged | PASS | PASS |
| Restore the author's original Atomic change identity | Not applicable to this Git-authored input | **Not retained in reader view** with plain Git push |

- [PR #1](https://github.com/leefaus/train-tracks/pull/1): Git-authored content
  including `%`, escape-looking, and Unicode/space filenames. Merge commit
  `203cd7533e3a328c276bab9b1aa0d56f859c2a39`. Evidence:
  `/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/atomic-train-tracks-pr-pn9o3u`.
  Logs `21.log`–`31.log` cover push through repeat import/checkpoint validation.
- [PR #2](https://github.com/leefaus/train-tracks/pull/2): a native Atomic record
  projected to Git. Merge commit `bfc818716850b1c3ec9aeeabfbd1f69282488458`.
  Evidence:
  `/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/atomic-train-tracks-pr-8jEJ3w`.
  Logs `18.log`–`30.log` cover record through repeat import/checkpoint validation.
  The identity audit was also run directly against the retained author/reader
  histories and saved as `author-tip-log.json` and `identity-audit.json`.

Both lifecycle runs exited 0. The reader's original history was retained and one
new synthesized merge change was added in each run. On the second run, the
author's native change `MTKV3LOVK7KJSVJLDZ2AROBTH3VYHTMITGDDLJDXA2SBW4RL5ZPA`
was **not** among the reader view's change hashes; the new merge interpretation
was `ORULG4SYWUBQC4ZCUGT73FO35YKYM76VW2UDHKLTHQZ66C2LS3EA` instead.
The harness now records this identity audit independently of its content/state
pass result. Binding publication/resurrection was not exercised; ordinary Git
branch pushes in these runs did not establish original-history/provenance
restoration. Do not interpret the successful content round trip as that stronger
guarantee.

The personal fork's `release` now points at `bfc818716850b1c3ec9aeeabfbd1f69282488458`;
upstream remains at `dddbe3e1434e8fbad4c8aba4b875b1726707f3d5`. No fork or PR
was deleted. Squash/rebase merges, protected-branch reviews/required checks,
server-side provenance enforcement, agent-managed provenance, and conflict
resolution were not tested. The initial test-harness assertion wrongly required
the optional Anchor `git_index_tree` cache to be populated; it was corrected
before either PR was created. The actual verifier still checks index equivalence.

## Latest result — literal-percent fix validated (2026-10-26)

The full suite now **passes with default `all` filename coverage**, including
`100%.txt`; no filename case is excluded. Tested on Darwin 25.4.0 arm64 with
`ATOMIC_SERVICE=local`, the local debug binary reporting
`atomic 0.20.0 (dev e9d57bdb06f1-dirty)` (the base source plus this fix).
Binary SHA-256:
`7d0b334d85f33ad8b360c53e8c2fb6aec3727f48dacafd9aa4c9bd33e6946cda`.

```sh
RUN_TRAIN_TRACKS_SHIM=1 PUSH_TRAIN_TRACKS_SHIM=1 \
  bash tests/harness/47_git_shim_train_tracks.sh
```

Exit 0; evidence root:
`/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/atomic-train-tracks-cBA065`.
Logs `34.log` and `35.log` show the formerly failing import/reconcile and
verification; ordinary Atomic and Git statuses are both clean. Subsequent
lockfile regeneration, noise, detached checkout/return, and hook removal pass.
The real pre-push check passes and the optional test branch
`shim-test-20261026T094209Z-90054` exists only on the personal fork. Upstream and
the fork's default branch remain at `dddbe3e1434e8fbad4c8aba4b875b1726707f3d5`.

Focused regression commands (all passed using this workspace's debug build):

```sh
ATOMIC_SERVICE=local ATOMIC_NONINTERACTIVE=1 \
  GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
  ATOMIC_AUDIT_BINARY="$PWD/target/debug/atomic" \
  cargo test --locked -p atomic-cli \
    --test git_import_path_audit_test --test git_shim_branch_reconcile_regression
cargo test --locked -p atomic-repository --lib \
  repository::status::change_source_convergence_tests
ATOMIC_SERVICE=local ATOMIC_NONINTERACTIVE=1 \
  GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
  cargo test --locked -p atomic-cli --test adaptation_regression_test \
    --test git_shim_opt_in_test --test git_shim_workspace_test
```

Counts: 15 + 3 + 4 + 6 + 9 + 4 = **41 passed**. New checks cover an initially
clean import, dirty/deleted percent paths (not hidden), physical path filters,
nested percent names, symlinks, escape lookalikes (`a%41.txt`, `aA.txt`,
`a%2541.txt`), repeat reconciliation, and Git edit/rename/delete. Native raw
names remain unchanged both without Git and with unenrolled colocated Git.
Formatting checks passed. Full workspace, agent, watcher, migration and remote
receiving-side enforcement suites were not run for this targeted fix.

## Initial measured results — 2026-10-26 (before the percent-path fix)

Tested the local debug build, `atomic 0.20.0 (dev e9d57bdb06f1)`, from source
commit `e9d57bdb06f10f724d71486305efe757f8dcfdcf`, on Darwin 25.4.0 arm64,
with `ATOMIC_SERVICE=local`. Binary SHA-256:
`f26a83e2f65a45e6aed2adcae4e08c3c63339bf17c7a05ae554bdfd2ccd75ee3`.
The imported default branch is `release`, containing one upstream commit,
`dddbe3e1434e8fbad4c8aba4b875b1726707f3d5`; this is not a large-history test.

| Check | Actual result |
| --- | --- |
| Fork creation through GitHub API; parent/owner validation | PASS |
| Import, signed Anchor, remote fetch rules, five hooks using debug binary | PASS |
| Native record, rewrite, rename without trailing newline; exact Git bytes | PASS |
| New Git branch after Atomic records, reconcile, return, fast-forward merge | PASS |
| Real push of non-current branch, local pre-push verification, remote SHA | PASS |
| Unicode/space filename and empty lockfile regeneration | PASS |
| Untracked scratch plus explicitly gitignored dependency file | PASS |
| Detached imported ancestor, reconcile, return with unchanged branch tip | PASS |
| Disable removes all five owned dispatchers; both statuses clean | PASS |
| Literal `%` filename | FAIL: reconcile exits 4 after import |
| Shell syntax; default skip and runner skip classification | PASS |
| Agent/plugin, watcher, migration, server-side enforcement, full Rust suites | NOT RUN: outside this fork-backed smoke test |

Successful final run (exit 0):

```sh
RUN_TRAIN_TRACKS_SHIM=1 PUSH_TRAIN_TRACKS_SHIM=1 TRAIN_TRACKS_PATH_CASE=unicode \
  bash tests/harness/47_git_shim_train_tracks.sh
```

Evidence root:
`/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/atomic-train-tracks-rhpIlQ`.
Logs `31.log`, `40.log`, and `50.log` contain the successful push, ignored-file
check, and hook removal respectively. The remote test branch is
`leefaus/train-tracks:shim-test-20261026T092727Z-82222`. An earlier successful
run also published `shim-test-20261026T092613Z-40482`; both remain for inspection.
No pull requests were created and neither repository's `release` ref changed.

### Original literal-percent failure (retained evidence)

The original failure was reproduced in a fresh clone with:

```sh
RUN_TRAIN_TRACKS_SHIM=1 TRAIN_TRACKS_PATH_CASE=percent \
  bash tests/harness/47_git_shim_train_tracks.sh
```

This committed `shim-integration/100%.txt` alongside an empty lockfile. Unlike
the passing Unicode/space case with the same empty lockfile, reconciliation
imported the commit and then exited 4 with `Atomic working copy is not clean`.
Git porcelain was empty, and a subsequent explicit `atomic git shim verify`
passed every layer, but forensic status showed the checkpoint still at the
previous Git HEAD/Atomic state. Ordinary status then refused the stale baseline.
Do not treat `verify` alone as proof that reconciliation completed.

Retained evidence root:
`/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/atomic-train-tracks-LRziWZ`.
See `33.log`, `failure-git-status.txt`, `failure-forensic-status.txt`, and
`failure-verify.txt`. These fixtures remain untouched. The root cause was later
confirmed: status used escaped TREE names as filesystem paths, while the CLI
Git-index overlay compared raw index names with those escaped identities.
Extended lookalike coverage also found semantic replay double-escaping stored
names and looking up the decoded sibling's trunk. The implementation now fixes
those boundaries without relaxing cleanliness or verification checks; the
default `all` coverage remains enabled rather than skipping the original case.

During branch returns, the importer also warned about dropped unverifiable
`GIT_SHA_INDEX` cache rows and skipped divergent squash interpretations. The
tested final bytes, statuses, and verification passed despite those warnings;
the warnings themselves have not been diagnosed by this smoke test.
