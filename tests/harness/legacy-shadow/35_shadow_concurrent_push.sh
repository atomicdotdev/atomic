#!/usr/bin/env bash
# HISTORICAL: coverage migrated to ../35_git_shim_serialization.sh.
#
# SPEC-single-materializer-validator.md §4.3 / Principle 5: only one shadow
# materialize/commit may be in flight per repo. Concurrent pushes must serialize
# via the repo-scoped lock — one commits, the other no-ops — and must never
# interleave into a corrupt tree or a double commit.
#
# The lock's exclusivity is proven deterministically by the Rust unit test
# `shadow_commit_lock_is_exclusive_and_non_blocking`. This suite is the
# end-to-end smoke test: two overlapping `atomic git push` invocations leave a
# valid repo, add at most one commit, and converge.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 35_shadow_concurrent_push${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

begin_section "Concurrent shadow pushes serialize (no corruption, no double commit)"

make_temp_repo "shadow-concurrent"
init_git_repo
git_commit "Initial" "README.md" "# Project"
require_success "git import" atomic git import --no-vault

# Baseline shadow commit.
create_file "src/a.ts" "const a = 1;"
require_success "add src/a.ts" atomic add "src/a.ts"
require_success "record src/a.ts" record_change "feat: a"
require_success "baseline local shadow commit" atomic git push --no-push -m "baseline"
require_capture "read baseline commit count" git rev-list --count HEAD
BASE_COUNT="$COMMAND_OUTPUT"

# A new recorded change, then two overlapping pushes of the same state.
create_file "src/b.ts" "const b = 2;"
require_success "add src/b.ts" atomic add "src/b.ts"
require_success "record src/b.ts" record_change "feat: b"

output_dir="$(mktemp -d "${TMPDIR:-/tmp}/atomic-concurrent-output-XXXXXX")"
_HARNESS_TMPDIRS+=("$output_dir")
atomic git push --no-push -m "concurrent-1" >"$output_dir/concurrent-1.out" 2>&1 &
P1=$!
atomic git push --no-push -m "concurrent-2" >"$output_dir/concurrent-2.out" 2>&1 &
P2=$!
RC1=0
RC2=0
wait "$P1" || RC1=$?
wait "$P2" || RC2=$?
# Launching two processes is a smoke test, not proof that their critical
# sections overlapped. The deterministic lock unit test covers exclusivity;
# this harness must not claim that it exercised actual contention.
_skip "confirmed overlapping shadow critical sections" "parallel process launch does not prove contention"
for index in 1 2; do
    rc_var="RC$index"
    rc="${!rc_var}"
    if [[ "$rc" -eq 0 ]]; then
        _pass "concurrent local shadow commit $index succeeds"
    else
        _fail "concurrent local shadow commit $index succeeds" "exit $rc: $(cat "$output_dir/concurrent-$index.out")"
    fi
done

# The repo must remain valid (no interleaved/corrupt tree).
assert_success "git repo still valid after concurrent pushes" git fsck --no-progress
assert_success "atomic status still works" atomic status

# The burst must add at most one commit — never a double commit.
require_capture "read concurrent commit count" git rev-list --count HEAD
BURST_COUNT="$COMMAND_OUTPUT"
if [ "$BURST_COUNT" -le "$((BASE_COUNT + 1))" ]; then
    _pass "concurrent burst added at most one commit ($BURST_COUNT)"
else
    _fail "concurrent burst added at most one commit" \
        "base=$BASE_COUNT after=$BURST_COUNT (double commit?)"
fi

# Convergence: a final push leaves the new change committed and the tree clean.
require_success "converge local shadow commit" atomic git push --no-push -m "converge"
require_capture "read committed src/b.ts" git show HEAD:src/b.ts
if [[ "$COMMAND_OUTPUT" == 'const b = 2;' ]]; then _pass "new change is committed with exact text"; else _fail "new change is committed with exact text" "got: $COMMAND_OUTPUT"; fi

require_capture "read converged commit count" git rev-list --count HEAD
FINAL_COUNT="$COMMAND_OUTPUT"
if [ "$FINAL_COUNT" -eq "$((BASE_COUNT + 1))" ]; then
    _pass "exactly one commit for the new change ($FINAL_COUNT)"
else
    _fail "exactly one commit for the new change" \
        "expected $((BASE_COUNT + 1)), got $FINAL_COUNT"
fi

print_summary
