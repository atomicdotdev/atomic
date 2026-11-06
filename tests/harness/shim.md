# Current offline shim harness

The old shadow-sync and early shim suites overlapped and assumed different
publication models. The active suites now share signed, opt-in, local-only
fixtures in `shim_helpers.sh`. They never configure a remote or push one.

| Suite | Contract |
|---|---|
| `35_git_shim_serialization.sh` | Hold the common operation lock in a different process, prove a typed refusal, release it and verify convergence; two reconcile callers must not double-publish. |
| `39_git_shim_workflow.sh` | Enroll; native record projects automatically; adopt Git-authored branches without rewriting Git-rendered files; repeat reconciliation preserves identities; native Draft switching detaches HEAD; explicit local `shim publish` creates a branch; explicit shim switching restores exact files, empty files, deletions, executable modes and symlinks. Unenrolled native repos do not acquire mirror branches. |
| `40_git_shim_safety.sh` | Dirty/marker refusal, resolved retry, excluded metadata in the index, forged provenance, stale baseline, untracked file/parent/directory collisions, pre-materialize injection and successful retry. |
| `41_git_shim_hooks.sh` | Advisory event/receipt evidence, correctness without checkout hooks, owned dispatchers, idempotent installation/uninstallation, and byte/mode preservation of unmanaged hooks. |
| `42_git_shim_guard.sh` | Early stale-baseline refusal across command boundaries, typed drift and actionable remediation, raw database-byte/operation/receipt non-mutation, forensic bypasses and normal enrolled projection. |
| `43_git_shim_operation_recovery.sh` | Local signed/imported baseline, actual Git feature branch and explicit adoption, partial-removal rollback and ordinary retry, plus instrumented publication returned-error seams. Missing seams are not passes. |

```sh
ATOMIC_BIN=/absolute/path/to/atomic bash tests/harness/run_all.sh --list shim
ATOMIC_BIN=/absolute/path/to/atomic bash tests/harness/run_all.sh shim
bash tests/harness/shim_harness_selftest.sh
```

For the **full fault-injection test**, build the test feature explicitly:

```sh
cargo build --locked -p atomic-cli --features adoption-test-injection
ATOMIC_BIN="$PWD/target/debug/atomic" bash tests/harness/run_all.sh shim
```

Without that feature, suite 43 is intentionally partial: the two publication
seams are no-ops in a shipping build. These are injected **returned errors**,
not process kills or power-loss tests. Use a normal default-feature build for
ordinary demo/release use, not the fault-injection build.

`--list` does not build or invoke Atomic. Canonical suite discovery excludes the
`legacy-shadow/` directory. Old runner selectors are redirected and deduplicated:

| Historical selector | Active replacement |
|---|---|
| `19_git_shadow` | 39 workflow + 41 hooks |
| `33_shadow_push_conflict_markers` | 40 safety |
| `34_shadow_provenance_guard` | 40 safety |
| `35_shadow_concurrent_push` | 35 serialization |
| `36_shadow_coherence_v2` | 40 safety |
| `37_shadow_view_branch_follow` | 39 workflow |
| `38_shadow_drift_v3` | 40 safety (strict cross-command gate remains in 42) |
| `39_git_shim_mvp` | 39 workflow |
| `40_git_shim_materialize_safety` | 40 safety |

Numeric selectors retain their normal native-suite matches (notably `19_unrecord`).
Select canonical names if you do not want every suite sharing a numeric prefix.

## Deliberate contract changes

- Native Draft view switching does **not** promise to invent an attached Git
  mirror branch. Check detached projection; require explicit `shim publish`
  when a named branch is wanted. `shim switch` remains an explicit operation
  that attaches an already interpreted target branch.
- An enrolled native record already projects to Git. A second manual shadow
  push/reconcile is not required to publish that local record, nor should it
  manufacture another commit.
- Private-path policy is checked at its real boundary (index/projection), not
  by assuming `atomic add` must proceed past a stale baseline. `.atomic/` and
  `.atomicignore` must stay private; `.vault/` follows the configured policy,
  not the old blanket “all vault paths are always excluded” expectation.
- Foreground refusal can append WIP-capture evidence. The opt-in capture
  comparator permits only verified system capture/rebase records and a
  verified switch **back to the original view**, with a capture present. It
  checks receipts for each new operation. It still requires unchanged user
  history/current view, filesystem bytes/kinds/modes, refs, Git HEAD/index,
  checkpoint, every existing change object, and all existing operation-log
  entries (apart from their derived head flags). This comparator does not
  byte-snapshot the redb database or every prior receipt body.
  Strict refusal cases permit no operation/object additions at all. Suite 42
  retains the stronger raw-database non-mutation gate separately. Ordinary
  command preflight now rejects an already observed unbound checkout before
  opening the unified database writable. Verified bound-HEAD adoption and
  pending recovery still enter the locked transaction and revalidate there;
  preflight is not itself a lease or a race-free Git lock.
- Legacy `shadow-validate:V1/V2/V3/V4` strings and `hook-errors.log` are not the
  current authority. Check actual refusal, causal state, and journal receipts.
  Marker override in a native-only repo remains in suite 30; it is not a bypass
  of the enrolled shim's publication policy.
- The old concurrency suite explicitly skipped proof of contention. The new
  suite holds the actual common lock before launching the other process;
  the subsequent two-caller burst is labeled only as convergence smoke coverage.

## Coverage boundaries, not silent omissions

The original scripts are preserved verbatim apart from their archival headers
under `legacy-shadow/`; they are reference material, not executable active
contracts. Initial/imported Git history fidelity stays in suites 10/13; squash
classification and review-evidence scenarios from the old 32 are retained in
`legacy-shadow/32_review_gate_insert.sh` as the historical Git-squash proposal;
its bare trailers are not accepted proof. Active suite 32 is now the native
signed-review and original-record insertion workflow described in `native.md`.
The old 19 dirty-background-import fixture manually mixed divergent Git index
and draft bytes, and its recovery fixture edited `.atomic/current_view` plus a
legacy pending marker. Those are not valid setup for today's working-copy
authority. Current safety tests require refusal without external data loss;
operation recovery is owned by 43, not counted as passed by a note in suite 40.
The 45 watcher, 47 real-repository and 48 GitHub suites remain separate and
retain their opt-in/network requirements; they are not run by this consolidation.

## Consolidation validation

Validated offline against the pinned default-feature debug binary used for the
17/22/28 fixes (`atomic-harness17` in the disposable harness evidence directory):

| Suite | Observed result |
|---|---|
| 35 serialization | 30 passed, 0 failed, 0 skipped |
| 39 workflow | 99 passed, 0 failed, 0 skipped |
| 40 safety | 115 passed, 0 failed, 0 skipped |
| 41 hooks | 38 passed, 0 failed, 0 skipped |
| 42 strict guard (unchanged) | 104 passed, 12 failed, 0 skipped |
| 43 operation recovery (unchanged) | Not rerun in this consolidation |

Those are historical consolidation results. Subsequent shim repair preserved
all raw-state and journal comparisons while fixing writable-open-before-refusal.
It replaced the legacy shadow diagnostic prose expectation with typed drift
and exact remediation commands, and the obsolete “record leaves HEAD alone”
expectation with exact enrolled-record projection plus verification.

| Suite | Fresh instrumented result |
|---|---|
| 35 serialization | 30 passed, 0 failed, 0 skipped |
| 39 workflow | 99 passed, 0 failed, 0 skipped |
| 40 safety | 115 passed, 0 failed, 0 skipped |
| 41 hooks | 38 passed, 0 failed, 0 skipped |
| 42 strict guard | 117 passed, 0 failed, 0 skipped |
| 43 returned-error recovery | 39 passed, 0 failed, 0 skipped |

Default-feature suite 43 separately reported **29 passed, 0 failed, 2 skipped**.
The six CLI workspace integration tests passed, including new byte-for-byte
database non-mutation checks for status/diff/add/record/switch/restore under an
unbound checkout and existing bound/adoption regressions. Native suites 23/32/40,
all 15 local-service tests, workspace check and workspace Clippy also passed.
This is not full network, watcher, process-kill or power-loss certification.

### Separate maintenance edge found during setup

An alternate fixture that adds two native paths (`shared.txt`, `target-only.txt`)
on an enrolled imported baseline, records/projects them, then creates a Git
feature branch which deletes one path and adds two source-only paths, reconciles
and verifies its visible projection. However, `doctor check` reports four missing
`CONFLICTS` cache rows in the private `wc/<id>` snapshot view. Repair populates
them, but that cache-completeness issue is **not fixed** here. The reproduction
was retained in disposable evidence rather than weakening doctor checks. Suite
43 retains its original imported-baseline scope and still runs doctor checks
for its foreign deletion before recovery testing.
