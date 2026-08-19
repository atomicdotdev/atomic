#!/usr/bin/env bash
# 35_shadow_concurrent_push.sh — Shadow-commit serialization (Rule / Phase 3).
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
assert_success "git import" atomic git import --no-vault

# Baseline shadow commit.
create_file "src/a.ts" "const a = 1;"
add_files "src/a.ts"
record_change "feat: a" >/dev/null
assert_success "baseline push" atomic git push --no-push -m "baseline"
BASE_COUNT="$(git_commit_count)"

# A new recorded change, then two overlapping pushes of the same state.
create_file "src/b.ts" "const b = 2;"
add_files "src/b.ts"
record_change "feat: b" >/dev/null

atomic git push --no-push -m "concurrent-1" >/dev/null 2>&1 &
P1=$!
atomic git push --no-push -m "concurrent-2" >/dev/null 2>&1 &
P2=$!
wait "$P1" || true
wait "$P2" || true

# The repo must remain valid (no interleaved/corrupt tree).
assert_success "git repo still valid after concurrent pushes" git fsck --no-progress
assert_success "atomic status still works" atomic status

# The burst must add at most one commit — never a double commit.
BURST_COUNT="$(git_commit_count)"
if [ "$BURST_COUNT" -le "$((BASE_COUNT + 1))" ]; then
    _pass "concurrent burst added at most one commit ($BURST_COUNT)"
else
    _fail "concurrent burst added at most one commit" \
        "base=$BASE_COUNT after=$BURST_COUNT (double commit?)"
fi

# Convergence: a final push leaves the new change committed and the tree clean.
atomic git push --no-push -m "converge" >/dev/null 2>&1 || true
if git ls-tree -r --name-only HEAD 2>/dev/null | grep -qx "src/b.ts"; then
    _pass "new change is committed after convergence"
else
    _fail "new change is committed after convergence" "src/b.ts not in HEAD tree"
fi

FINAL_COUNT="$(git_commit_count)"
if [ "$FINAL_COUNT" -eq "$((BASE_COUNT + 1))" ]; then
    _pass "exactly one commit for the new change ($FINAL_COUNT)"
else
    _fail "exactly one commit for the new change" \
        "expected $((BASE_COUNT + 1)), got $FINAL_COUNT"
fi

print_summary
