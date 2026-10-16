# PR #207: review and testing walkthrough

Companion to the [bridge operating guide](bridge-operating-guide.md).
Updated 2026-10-13 for source `27d06ffb664400f6788b6fae0c4bee91e9a92c61`
(`atomic 0.19.1`). Record the actual commit and `atomic --version` with results.
Use Linux or macOS for bridge/RPC tests. Windows bridge support has not been
validated.

**Known bug:** the new-Git-branch steps in section 9 fail after Atomic records,
even with the normal Git hooks installed. The bug is not fixed. It has not yet
been compared against `dev`.

Run sections 1–5 in order in one **fresh Bash shell**. They use temporary
repositories and keys and do not push anything. Sections 6–8 cover agent,
service, and database tests.

## 1. Build and isolate the test

Start in a checkout of the PR branch (`feat/atomic-sidecar-for-git`). Rust/Cargo,
Git, Python 3, and the repository's normal build prerequisites are required.
This builds the default configuration, without fault-injection features.

```sh
export ATOMIC_REVIEW_SOURCE="$PWD"
git rev-parse HEAD
cargo build --locked -p atomic-cli --bin atomic
export ATOMIC_REVIEW_BIN="$ATOMIC_REVIEW_SOURCE/target/debug/atomic"
export ATOMIC_REVIEW_ROOT="$(mktemp -d /tmp/atomic-bridge-review.XXXXXX)"
export ATOMIC_CONFIG_DIR="$ATOMIC_REVIEW_ROOT/config"
export ATOMIC_SERVICE=local
export ATOMIC_NONINTERACTIVE=1
export NO_COLOR=1
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_NOSYSTEM=1
unset ATOMIC_RPC ATOMIC_DAEMON_SOCKET
atomic() { "$ATOMIC_REVIEW_BIN" "$@"; }
atomic --version
```

These settings keep the test separate from personal Git hooks and signing
settings. `ATOMIC_SERVICE=local` runs libatomic handlers in the CLI process.
Keep the temporary repo and command output if a check fails.

## 2. Native Atomic: record, rename, and views

```sh
mkdir "$ATOMIC_REVIEW_ROOT/native"
cd "$ATOMIC_REVIEW_ROOT/native"
atomic init --no-vault --view main
printf 'base\n' > old.txt
atomic add old.txt
atomic record -am 'review: native baseline'
mv old.txt new.txt
atomic status --short
atomic record -am 'review: native rename'
test -f new.txt
test ! -e old.txt
test -z "$(atomic status --short)"

atomic view create child --draft --parent main --switch
printf 'child\n' > new.txt
atomic record -am 'review: child edit'
atomic view switch main
test "$(cat new.txt)" = base
atomic view switch child
test "$(cat new.txt)" = child
test -z "$(atomic status --short)"
atomic log --short
test ! -e .git
```

Expected: rename records without an inode error. Switching views shows `base`
on `main` and `child` on `child`. After recording, `status --short` is empty.
No `.git` directory is created.

## 3. Existing Git history → explicit bridge

```sh
mkdir "$ATOMIC_REVIEW_ROOT/bridge"
cd "$ATOMIC_REVIEW_ROOT/bridge"
git init -q -b main
git config user.name 'Bridge Test'
git config user.email 'test@example.invalid'
printf 'base\n' > tracked.txt
git add tracked.txt
git commit -qm 'review: Git baseline'

atomic git import --no-vault
python3 -c 'import os,secrets; p=os.path.join(os.environ["ATOMIC_REVIEW_ROOT"],"binding.key"); fd=os.open(p,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600); os.write(fd,secrets.token_hex(32).encode()); os.close(fd)'
atomic git bridge enable --no-refspecs \
  --binding-key-file "$ATOMIC_REVIEW_ROOT/binding.key"
atomic git bridge reconcile
atomic git bridge verify
atomic git hooks status
test -z "$(atomic status --short)"
test -z "$(git status --porcelain)"
```

Expected: Git history is imported, the hooks are installed, and verification
passes. The test key stays outside the repository. `--no-refspecs` skips remote
setup. A notice that automatic rollout is blocked is expected.

There is no `atomic git shim` command. Use normal Git commands after installing
the hooks. Having both `.git` and `.atomic` alone does not enable the bridge;
the opt-in tests in section 7 cover that case.

## 4. Atomic changes projected to Git

Edit and rename a file, then record with Atomic:

```sh
printf 'atomic edit\n' > tracked.txt
atomic diff
atomic record -am 'review: Atomic edit'
test "$(git show HEAD:tracked.txt)" = 'atomic edit'
mv tracked.txt renamed.txt
atomic record -am 'review: bridge rename'
test "$(git show HEAD:renamed.txt)" = 'atomic edit'
test ! -e tracked.txt
atomic git bridge verify
test -z "$(atomic status --short)"
test -z "$(git status --porcelain)"
```

Expected: Git HEAD contains the edited and renamed file, and both statuses
are clean. The new-Git-branch case still fails; see section 9.

## 5. Optional watcher and ordinary disable

Enable the existing watcher setting in the test repo, then run one pass:

```sh
python3 - <<'PY'
from pathlib import Path
import re
p = Path('.atomic/config.toml')
s = p.read_text()
s, n = re.subn(r'(\[git\.bridge\.watch\]\s*\nenabled\s*=\s*)false',
               r'\g<1>true', s, count=1)
assert n == 1, 'inspect the existing watcher configuration before editing'
p.write_text(s)
PY
atomic git bridge watch --once
atomic git bridge verify
atomic git bridge disable
atomic git hooks status
```

Expected: the idle pass succeeds without changing tracked files or Git HEAD.
Disable leaves hooks and checkpoints in place. Previously anchored workspaces
still check for a stale baseline. `atomic git hooks uninstall` removes only
Atomic's post-checkout hook.

The watcher tests in section 7 cover Git changes and blocked operations. To
run the watcher continuously, re-enable the bridge and run
`atomic git bridge watch`. Stop it with Ctrl-C.

## 6. OpenCode: real agent work

This test needs OpenCode, model credentials, and the official Atomic plugin
and skills. Use a test profile because installation can change agent settings
outside the repository. Add the built binary's directory to that profile's
PATH and check `command -v atomic`. The shell function above is not enough for
the agent to find the binary.

In the bridge test repo, re-enable the bridge and install the integration:

```sh
atomic git bridge enable --no-refspecs \
  --binding-key-file "$ATOMIC_REVIEW_ROOT/binding.key"
atomic agent enable --agent opencode
atomic agent status --json
```

For local packages, add `--from /path/to/atomic-opencode --from-skills
/path/to/atomic-skills`. Note the package versions, OpenCode version, and model.

Ask OpenCode to edit `renamed.txt`, add a test, and run it. In a second turn,
ask it to rename the file and update the test. Let the plugin finish. Check
auto-record before making any manual record.

After each turn inspect:

```sh
atomic agent status --json
atomic log --short
atomic status --short
git status --porcelain
atomic git bridge verify
```

Expected: tests pass, the turn is recorded, and both statuses are clean after
auto-record. Unrecorded work must be retained. Repeat with standalone OpenCode
and with a server started in the same repo. Also check partial staging and a
sandbox: sandbox-only work must leave the host view, Git state, and files alone.

`bridge_agent_workflow_regression_test` checks callbacks. The steps above also
check the actual plugin and model workflow.

## 7. Focused regression suites

Run these from the Atomic source checkout. They create their own test repos.
Check failures and skipped tests in the output.

```sh
cd "$ATOMIC_REVIEW_SOURCE"
cargo test --locked -p atomic-cli \
  --test adaptation_regression_test \
  --test git_bridge_opt_in_test \
  --test git_bridge_workspace_test \
  --test bridge_agent_workflow_regression_test \
  --test git_shim_logging_test

cargo test --locked -p atomic-cli \
  --test working_copy_service_adaptation_test \
  --test compact_service_integration_test
cargo test --locked -p libatomic \
  --test record_service_test --test status_service_test --test sandbox_service_test

cargo test --locked -p atomic-repository \
  --test database_consolidation_bridge_test --test migration_recovery_test

cargo test --locked -p atomic-cli \
  --features adoption-test-injection \
  --test git_bridge_cb13d_test --test git_cutover_cb13b_test
```

`working_copy_service_adaptation_test` starts a real RPC server using this
workspace's libatomic handlers. It checks rename identity, file attributes,
and parent-view content.

For a full test run, use both configurations:

```sh
cargo test --locked --workspace --no-fail-fast
cargo test --locked --workspace \
  --features atomic-cli/adoption-test-injection --no-fail-fast
```

The injection feature enables simulated failures. It also excludes the
default-only `shipping_build_cannot_execute_adoption_injection_variables` test,
so run both configurations. Build the normal CLI without the injection feature.

## 8. Database migration

Stop all writers, then copy an old-format repository. With the old client,
save its file contents, unrecorded work, views, full change hashes, and session
information. Open the copy with this build and compare those results. After
migration, the database is `.atomic/atomic.redb` and old databases are kept
under `.atomic/legacy/`.

Then rename and record a file, switch views and back, reopen, and run
`atomic compact`. Reopen again and check that history, provenance, and files
are preserved. Section 7 includes tests for database locks and interrupted
migration.

Database migration is separate from `git bridge cutover`. See
[operating guide section 15](bridge-operating-guide.md#15-bridge-cutover-and-database-migration)
for cutover limits.

## 9. Known failure: new Git branch after Atomic records

Run this in a **fresh test repo immediately after sections 1–4**, before
disabling the bridge or running the agent test:

```sh
git switch -c foreign-edit
printf 'Git edit\n' > renamed.txt
git add renamed.txt
git commit -qm 'review: foreign Git edit'
atomic git bridge reconcile
```

Expected: reconcile imports the commit and verify passes. Switching between
`main` and `foreign-edit`, then reconciling, should show `atomic edit` and
`Git edit` respectively.

**Observed on `27d06ff`, default build, macOS, local service mode:**

```text
Git error: commit <oid> carries an unverifiable Atomic change closure:
repository projection failed: view 'foreign-edit' not found
```

`reconcile` exits 4. This fails both with Atomic hooks installed and with a
custom hook path that prevents installation. Earlier steps pass. The existing
`git_bridge_workspace_test` suite passes but does not catch this sequence.

Keep the failed repo for debugging. The cause and comparison against `dev`
have not been confirmed. This bug needs a fix before these basic bridge
workflows can be marked ready to merge.

## 10. Evidence and reporting

Validation performed for this documentation refresh (macOS, `27d06ff` plus
documentation-only edits; the binary reports a `-dirty` suffix):

| Check | Result |
| --- | --- |
| Default-build commands extracted from sections 1–5, in fresh repos | PASS |
| `git_bridge_workspace_test` | 4 passed |
| `working_copy_service_adaptation_test` (real RPC socket) | 1 passed |
| `git_bridge_cb13d_test` (injection build) | 21 passed |
| `git_cutover_cb13b_test` (injection build) | 4 passed |
| Database consolidation + migration recovery suites listed in §7 | 4 passed |
| Section 9 combined new-branch reproduction | FAIL, exit 4, with and without installed hooks |
| OpenCode/model workflow | Not rerun in this documentation refresh |

The passing checks do not resolve the section 9 failure.

Before this documentation refresh, [CI run 38344469173](https://github.com/atomicdotdev/atomic/actions/runs/38344469173)
passed all eight jobs on `27d06ff`, including Linux/macOS/Windows native test
jobs. Earlier local tests also covered OpenCode, RPC transport, and old-format
redb migration. Those results do not cover every combination.

For each review result report:

- Atomic commit/version, OS, service mode, and plugin versions if used.
- Scenario and exact commands, expected result, actual result and exit status.
- PASS, FAIL, or NOT RUN; include the reason for every skipped integration.
- Relevant stdout/stderr, both statuses, session/turn IDs, and reproducible
  fixture details. Share no signing keys, credentials, or private transcripts.

Keep failed test repos. Check file contents and history as well as status
and bridge verification.
