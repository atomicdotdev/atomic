#!/usr/bin/env bash
# 34_shadow_provenance_guard.sh — Validator Rule V4 (provenance / excluded paths).
#
# SPEC-single-materializer-validator.md, Phase 1 / Rule V4: a shadow commit MUST
# never stage a git-excluded Atomic provenance path (`.atomic/`, `.vault/`,
# `.atomicignore`). `.vault` (intents/memories/attestations) and `.atomic` (the
# change graph) are git-excluded and unbacked; committing or reconciling them is
# how the motivating incident began (and ended in their deletion).
#
# This suite pins:
#   1. A normal shadow commit contains ZERO provenance paths (prevention works).
#   2. If a provenance path is already git-tracked (excludes can't save it), the
#      push aborts (V4): no commit, names the path, logs shadow-validate:V4, and
#      leaves git + the provenance dir byte-identical.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 34_shadow_provenance_guard${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

# Grep the committed tree for any Atomic provenance path.
tree_has_provenance() {
    git ls-tree -r --name-only HEAD 2>/dev/null \
        | grep -qE '^\.atomic/|^\.vault/|^\.atomicignore$'
}

# ════════════════════════════════════════════════════════════════════════
# A normal shadow commit never contains provenance paths
# ════════════════════════════════════════════════════════════════════════

begin_section "Normal shadow commit excludes provenance paths"

make_temp_repo "v4-clean"
init_git_repo
git_commit "Initial" "README.md" "# Project"
assert_success "git import seeds the view" atomic git import --no-vault

assert_dir_exists ".atomic exists on disk" ".atomic"

create_file "src/app.ts" "const x = 1;"
add_files "src/app.ts"
record_change "feat: app" >/dev/null
assert_success "clean shadow push commits" atomic git push --no-push -m "sync"

if tree_has_provenance; then
    _fail "committed tree has no provenance paths" \
        "found: $(git ls-tree -r --name-only HEAD | grep -E '^\.atomic/|^\.vault/|^\.atomicignore$' | head -3)"
else
    _pass "committed tree has no provenance paths"
fi
assert_dir_exists ".atomic untouched after push" ".atomic"

# ════════════════════════════════════════════════════════════════════════
# V4 fires when a provenance path is already git-tracked
# ════════════════════════════════════════════════════════════════════════

begin_section "V4 aborts on an already-tracked provenance path"

make_temp_repo "v4-tracked"
init_git_repo
git_commit "Initial" "README.md" "# Project"
assert_success "git import" atomic git import --no-vault

# Simulate the dangerous pre-condition: a `.vault/` path that got committed to
# git before excludes were in place, so it is TRACKED (exclude rules no longer
# protect it and `git add -A`/update_all will keep staging it).
mkdir -p .vault
printf 'intent: precious' > .vault/note.md
git add -f .vault/note.md
git commit -q -m "Accidentally tracked a provenance path"

VAULT_HASH_BEFORE="$(git hash-object .vault/note.md)"
COUNT_BEFORE="$(git_commit_count)"

# Record an ordinary change and attempt a shadow push.
create_file "src/app.ts" "const y = 2;"
add_files "src/app.ts"
record_change "feat: app2" >/dev/null

set +e
PUSH_OUT="$(atomic git push --no-push -m "sync tracked provenance" 2>&1)"
PUSH_RC=$?
set -e
COUNT_AFTER="$(git_commit_count)"

if [ "$PUSH_RC" -ne 0 ]; then
    _pass "shadow push aborts (V4) on a tracked provenance path"
else
    _fail "shadow push aborts (V4) on a tracked provenance path" "expected non-zero"
fi

if echo "$PUSH_OUT" | grep -qa ".vault/note.md"; then
    _pass "abort names the provenance path"
else
    _fail "abort names the provenance path" "output: $(echo "$PUSH_OUT" | head -5)"
fi

if [ "$COUNT_AFTER" -eq "$COUNT_BEFORE" ]; then
    _pass "no commit created ($COUNT_AFTER)"
else
    _fail "no commit created" "expected $COUNT_BEFORE, got $COUNT_AFTER"
fi

if [ -f .atomic/hook-errors.log ] && grep -qa "shadow-validate:V4" .atomic/hook-errors.log; then
    _pass "hook-errors.log records a shadow-validate:V4 entry"
else
    _fail "hook-errors.log records a shadow-validate:V4 entry" \
        "log: $(cat .atomic/hook-errors.log 2>/dev/null | head -3)"
fi

# Provenance byte-identical after the aborted push.
VAULT_HASH_AFTER="$(git hash-object .vault/note.md)"
if [ "$VAULT_HASH_AFTER" = "$VAULT_HASH_BEFORE" ]; then
    _pass "provenance file left byte-identical on abort"
else
    _fail "provenance file left byte-identical on abort" \
        "before=$VAULT_HASH_BEFORE after=$VAULT_HASH_AFTER"
fi

print_summary
