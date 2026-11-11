#!/usr/bin/env bash
# Deterministic common-lock exclusion plus repeat-convergence smoke coverage.
HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
source "$HARNESS_DIR/shim_helpers.sh"
require_git
begin_section "Held cross-process operation lock excludes shim mutation"
shim_fixture shim-serialization
shim_refusal "common lock" 'lock|busy|contention|retry|another' shim_locked_command git shim reconcile
shim_clean "after lock release"

begin_section "Two reconcile callers converge without double publication"
HEAD_BEFORE="$(git_head_sha_full)"
COUNT_BEFORE="$(git_commit_count)"
evidence="$(mktemp -d "${TMPDIR:-/tmp}/atomic-shim-callers-XXXXXX")"
_HARNESS_TMPDIRS+=("$evidence")
atomic git shim reconcile > "$evidence/one" 2>&1 & first=$!
atomic git shim reconcile > "$evidence/two" 2>&1 & second=$!
for item in "$first:one" "$second:two"; do
    pid="${item%%:*}"; name="${item#*:}"
    if wait "$pid"; then
        _pass "$name: reconcile succeeded"
    elif grep -qiE 'lock|busy|contention|retry|another' "$evidence/$name"; then
        _pass "$name: typed retry under contention"
    else
        _fail "$name: succeeds or requests retry" "$(cat "$evidence/$name")"
    fi
done
require_success "reconcile converges after callers" atomic git shim reconcile
require_capture "read final commit count" git_commit_count
if [[ "$COMMAND_OUTPUT" == "$COUNT_BEFORE" && "$(git_head_sha_full)" == "$HEAD_BEFORE" ]]; then
    _pass "concurrent no-op callers create no commits"
else
    _fail "concurrent no-op callers create no commits" "Git history changed"
fi
shim_clean "converged projection"
print_summary
