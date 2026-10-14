#!/usr/bin/env bash
# 42_git_bridge_guard.sh — CB-0C shared stale-baseline guard evidence.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

printf '\n%s\n' "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
printf '%s\n' "${BOLD}  Suite: 42_git_bridge_guard${RESET}"
printf '%s\n' "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git
case "$ATOMIC_BIN" in
    /*)
        if [[ -x "$ATOMIC_BIN" ]]; then
            _pass "harness uses an absolute executable Atomic binary"
        else
            _fail "harness uses an absolute executable Atomic binary" "$ATOMIC_BIN is not executable"
        fi
        ;;
    *)
        _fail "harness uses an absolute executable Atomic binary" "got relative path: $ATOMIC_BIN"
        ;;
esac

export GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_DATE="2001-01-01T00:00:00Z"
export GIT_COMMITTER_DATE="2001-01-01T00:00:00Z"
export ATOMIC_NONINTERACTIVE=1
export NO_COLOR=1
export CLICOLOR=0
export TERM=dumb

use_isolated_home() {
    local home
    home="$(mktemp -d "${TMPDIR:-/tmp}/atomic-cb0c-home-XXXXXX")"
    _HARNESS_TMPDIRS+=("$home")
    export HOME="$home"
    export ATOMIC_HOME="$home/.atomic"
}

start_colocated_repo() {
    local label="$1"
    make_temp_repo "$label"
    use_isolated_home
    init_git_repo
    git symbolic-ref HEAD refs/heads/main
    git config core.autocrlf false
    git config commit.gpgsign false
    create_file tracked.txt "checkpoint\n"
    git add tracked.txt
    git commit --no-gpg-sign --quiet -m "checkpoint"
    atomic git import --no-vault >/dev/null
    atomic git bridge reconcile >/dev/null
    if [[ -s .atomic/bridge/workspace.json ]]; then
        _pass "$label: valid bridge checkpoint established"
    else
        _fail "$label: valid bridge checkpoint established" ".atomic/bridge/workspace.json missing"
    fi
}

raw_git_drift() {
    local branch="$1"
    git switch --quiet -c "$branch"
    overwrite_file tracked.txt "raw Git checkout\n"
    git add tracked.txt
    git commit --no-gpg-sign --quiet -m "raw Git checkout"
}

snapshot_guard_state() {
    local destination="$1"
    {
        printf '%s\n' "atomic-files"
        find .atomic -type f -print | LC_ALL=C sort | while IFS= read -r path; do
            shasum -a 256 "$path"
        done
        printf '%s\n' "git-files"
        find .git -type f -print | LC_ALL=C sort | while IFS= read -r path; do
            shasum -a 256 "$path"
        done
        printf '%s\n' "worktree-files"
        find . \
            \( -path './.atomic' -o -path './.git' -o -path './.vault' \) -prune -o \
            -type f -print | LC_ALL=C sort | while IFS= read -r path; do
                shasum -a 256 "$path"
            done
        printf '%s\n' "atomic-log"
        atomic log -f json --full-hash
    } > "$destination"
}

assert_guard_refusal() {
    local label="$1"
    local operation="$2"
    shift 2
    local before after output rc lines
    before="$(mktemp "${TMPDIR:-/tmp}/atomic-cb0c-before-XXXXXX")"
    after="$(mktemp "${TMPDIR:-/tmp}/atomic-cb0c-after-XXXXXX")"
    _HARNESS_TMPDIRS+=("$before" "$after")
    snapshot_guard_state "$before"

    set +e
    output="$("$@" 2>&1)"
    rc=$?
    set -e

    if [[ "$rc" -ne 0 ]]; then
        _pass "$label: exits nonzero"
    else
        _fail "$label: exits nonzero" "command unexpectedly succeeded"
    fi

    local evidence_ok=1
    for expected in \
        "Unsafe operation: $operation" \
        "Refusal: Git moved away from the bridge checkpoint" \
        "Old checkpoint Atomic view: main" \
        "Old checkpoint Atomic state:" \
        "Old checkpoint Atomic manifest root: atomic-visible-regular-files-blake3-v1:" \
        "Old checkpoint Git HEAD: attached refs/heads/main at" \
        "Old checkpoint Git HEAD tree:" \
        "Old checkpoint Git index tree:" \
        "Old checkpoint Git index digest:" \
        "Old checkpoint Git refs digest:" \
        "Old checkpoint Git refs: digest-only checkpoint evidence" \
        "Old checkpoint Git manifest root: git-tree-oid:" \
        "Current Atomic view: main" \
        "Current Atomic state:" \
        "Current Atomic manifest root: atomic-visible-regular-files-blake3-v1:" \
        "Current Git HEAD: attached refs/heads/raw-drift at" \
        "Current Git HEAD tree:" \
        "Current Git index tree:" \
        "Current Git index digest:" \
        "Current Git refs digest:" \
        "Current Git refs:" \
        "refs/heads/main ->" \
        "refs/heads/raw-drift ->" \
        "Current Git manifest root: git-tree-oid:" \
        "Remediation: Reconcile the observed Git baseline before retrying the refused operation." \
        "  atomic status --no-reconcile" \
        "  atomic git bridge reconcile"
    do
        if ! printf '%s\n' "$output" | grep -qF -- "$expected"; then
            evidence_ok=0
            break
        fi
    done
    if [[ "$evidence_ok" -eq 1 ]]; then
        _pass "$label: reports old/current Atomic+Git, refs/index/roots, operation, and exact remediation"
    else
        _fail "$label: reports old/current Atomic+Git, refs/index/roots, operation, and exact remediation" "$output"
    fi

    lines="$(printf '%s\n' "$output" | wc -l | tr -d '[:space:]')"
    if [[ "$lines" -lt 100 ]] && ! printf '%s\n' "$output" | grep -qF 'mass-output-000.txt'; then
        _pass "$label: refuses before mass working-copy output"
    else
        _fail "$label: refuses before mass working-copy output" "$lines lines; $output"
    fi

    snapshot_guard_state "$after"
    if cmp -s "$before" "$after"; then
        _pass "$label: refusal is non-mutating"
    else
        _fail "$label: refusal is non-mutating" "$(diff -u "$before" "$after" || true)"
    fi
}

begin_section "Every working-copy boundary shares one early refusal"
start_colocated_repo "git-bridge-guard-matrix"
atomic view create feature --draft --parent main >/dev/null
CHANGE="$(atomic log -f oneline --full-hash | awk 'NR == 1 { print $1 }')"
HISTORY_BEFORE="$(atomic diff --change "$CHANGE" --name-only --no-color)"

atomic git bridge enable >/dev/null
HOOK_PATH="$(git rev-parse --git-path hooks/post-checkout)"
rm -f "$HOOK_PATH"
raw_git_drift raw-drift
if [[ ! -e .atomic/bridge/git-events.jsonl ]]; then
    _pass "removing the advisory hook leaves no checkout journal"
else
    _fail "removing the advisory hook leaves no checkout journal" "$(cat .atomic/bridge/git-events.jsonl)"
fi
for index in $(seq 0 63); do
    printf 'must not be scanned\n' > "$(printf 'mass-output-%03d.txt' "$index")"
done
create_file candidate.txt "must remain untracked\n"

assert_guard_refusal "status" "status" atomic status --short
assert_guard_refusal "diff" "diff" atomic diff --name-only --no-color
assert_guard_refusal "record" "record" atomic record --all -m "must refuse"
assert_guard_refusal "add" "add" atomic add candidate.txt
assert_guard_refusal "view switch" "view switch" atomic view switch feature --force
assert_guard_refusal "restore materializer" "materialize" atomic restore tracked.txt

set +e
FORENSIC_OUT="$(atomic status --no-reconcile 2>&1)"
FORENSIC_RC=$?
set -e
if [[ "$FORENSIC_RC" -eq 0 ]] && \
   printf '%s\n' "$FORENSIC_OUT" | grep -qF 'Forensic status (read-only; reconciliation disabled)' && \
   printf '%s\n' "$FORENSIC_OUT" | grep -qF 'refs/heads/raw-drift'; then
    _pass "status --no-reconcile bypasses the guard for forensic evidence"
else
    _fail "status --no-reconcile bypasses the guard for forensic evidence" "exit $FORENSIC_RC: $FORENSIC_OUT"
fi

set +e
HISTORY_AFTER="$(atomic diff --change "$CHANGE" --name-only --no-color 2>&1)"
HISTORY_RC=$?
set -e
if [[ "$HISTORY_RC" -eq 0 && "$HISTORY_AFTER" == "$HISTORY_BEFORE" ]]; then
    _pass "diff --change bypasses the guard and remains history-only"
else
    _fail "diff --change bypasses the guard and remains history-only" "exit $HISTORY_RC: $HISTORY_AFTER"
fi

begin_section "Ordinary edits and Atomic-only advancement remain allowed"
start_colocated_repo "git-bridge-guard-atomic-advance"
GIT_HEAD_BEFORE="$(git rev-parse HEAD)"
overwrite_file tracked.txt "ordinary edit\n"
if STATUS_OUT="$(atomic status --short 2>&1)" && printf '%s\n' "$STATUS_OUT" | grep -qF tracked.txt; then
    _pass "ordinary edit passes status"
else
    _fail "ordinary edit passes status" "$STATUS_OUT"
fi
if DIFF_OUT="$(atomic diff --name-only --no-color 2>&1)" && printf '%s\n' "$DIFF_OUT" | grep -qF tracked.txt; then
    _pass "ordinary edit passes diff"
else
    _fail "ordinary edit passes diff" "$DIFF_OUT"
fi
assert_success "ordinary edit passes record" atomic record -m "Atomic-only advancement"
if [[ "$(git rev-parse HEAD)" == "$GIT_HEAD_BEFORE" ]]; then
    _pass "Atomic record does not move Git HEAD"
else
    _fail "Atomic record does not move Git HEAD" "Git HEAD changed"
fi
assert_success "status passes after Atomic-only advancement" atomic status --short
create_file atomic-only.txt "further Atomic work\n"
assert_success "add passes after Atomic-only advancement" atomic add atomic-only.txt
assert_success "record passes after further Atomic-only advancement" atomic record -m "Further Atomic-only advancement"

begin_section "No-Git repositories retain normal behavior"
make_temp_repo "git-bridge-guard-no-git"
use_isolated_home
assert_success "initialize no-Git Atomic repository" atomic init --view main
create_file tracked.txt "initial\n"
assert_success "no-Git add succeeds" atomic add tracked.txt
assert_success "no-Git record succeeds" atomic record -m "Initial no-Git change"
overwrite_file tracked.txt "edited\n"
if STATUS_OUT="$(atomic status --short 2>&1)" && printf '%s\n' "$STATUS_OUT" | grep -qF tracked.txt; then
    _pass "no-Git status still reports edits"
else
    _fail "no-Git status still reports edits" "$STATUS_OUT"
fi
if DIFF_OUT="$(atomic diff --name-only --no-color 2>&1)" && printf '%s\n' "$DIFF_OUT" | grep -qF tracked.txt; then
    _pass "no-Git diff still reports edits"
else
    _fail "no-Git diff still reports edits" "$DIFF_OUT"
fi
assert_success "no-Git restore materializes recorded bytes" atomic restore tracked.txt
assert_file_content "no-Git restore preserves prior behavior" tracked.txt "initial\n"
assert_success "create no-Git feature view" atomic view create feature --draft --parent main
assert_success "no-Git view switch succeeds" atomic view switch feature --force
assert_current_view "no-Git view switch changes the current view" feature

assert_agent_incomplete() {
    local label="$1"
    local session_id="$2"
    local first_verb="$3"
    local retry_verb="$4"
    local expected_operation="$5"
    local before_log first_out first_rc first_json recovery_ref retry_out retry_rc retry_json after_log

    before_log="$(atomic log -f json --full-hash)"
    set +e
    first_out="$(printf '{\"session_id\":\"%s\"}\n' "$session_id" | \
        atomic agent hooks claude-code "$first_verb" --json 2>&1)"
    first_rc=$?
    set -e
    first_json="$(printf '%s\n' "$first_out" | sed -n '/^{/p' | sed -n '1p')"
    recovery_ref="$(printf '%s\n' "$first_json" | sed -n 's/.*"recovery_ref":"\([^"]*\)".*/\1/p')"

    if [[ "$first_rc" -ne 0 ]] && \
       printf '%s\n' "$first_json" | grep -qF '"recorded":false' && \
       ! printf '%s\n' "$first_json" | grep -qF '"change_hash"' && \
       printf '%s\n' "$first_json" | grep -qF '"files":[]' && \
       printf '%s\n' "$first_json" | grep -qF '"origin":"unknown_post_checkout"' && \
       printf '%s\n' "$first_json" | grep -qF '"paths":["tracked.txt"]' && \
       printf '%s\n' "$first_json" | grep -qF "Unsafe operation: $expected_operation" && \
       [[ "$recovery_ref" == refs/atomic/wip/* ]]; then
        _pass "$label: first boundary exits nonzero with incomplete and no false change result"
    else
        _fail "$label: first boundary exits nonzero with incomplete and no false change result" "exit $first_rc: $first_out"
    fi

    if git show-ref --verify --quiet "$recovery_ref" && \
       [[ "$(git show "$recovery_ref:tracked.txt")" == "unattributed agent bytes" ]]; then
        _pass "$label: WIP ref durably preserves tracked bytes"
    else
        _fail "$label: WIP ref durably preserves tracked bytes" "ref=$recovery_ref"
    fi
    if grep -q '"incomplete"' ".atomic/sessions/$session_id.json" && \
       grep -q '"recorded_change_hashes": \[\]' ".atomic/sessions/$session_id.json" && \
       SESSION_OUT="$(atomic session show "$session_id" 2>&1)" && \
       printf '%s\n' "$SESSION_OUT" | grep -qF 'Status: incomplete' && \
       printf '%s\n' "$SESSION_OUT" | grep -qF "$recovery_ref"; then
        _pass "$label: incomplete outcome is durable in JSON and the Atomic session index"
    else
        _fail "$label: incomplete outcome is durable in JSON and the Atomic session index" "$(cat ".atomic/sessions/$session_id.json" 2>/dev/null || true)"
    fi

    set +e
    retry_out="$(printf '{\"session_id\":\"%s\"}\n' "$session_id" | \
        atomic agent hooks claude-code "$retry_verb" --json 2>&1)"
    retry_rc=$?
    set -e
    retry_json="$(printf '%s\n' "$retry_out" | sed -n '/^{/p' | sed -n '1p')"
    if [[ "$retry_rc" -ne 0 ]] && printf '%s\n' "$retry_json" | grep -qF "\"recovery_ref\":\"$recovery_ref\""; then
        _pass "$label: turn/session retry reuses the first immutable WIP ref"
    else
        _fail "$label: turn/session retry reuses the first immutable WIP ref" "exit $retry_rc: $retry_out"
    fi

    after_log="$(atomic log -f json --full-hash)"
    if [[ "$after_log" == "$before_log" ]]; then
        _pass "$label: refusal records no false Atomic change"
    else
        _fail "$label: refusal records no false Atomic change" "before=$before_log after=$after_log"
    fi

    AGENT_RECOVERY_REF="$recovery_ref"
}

begin_section "Unmanaged turn/session endings are durable and unattributed"
start_colocated_repo "git-bridge-guard-unmanaged-agent"
raw_git_drift unmanaged-drift
printf 'unattributed agent bytes\n' > tracked.txt
assert_agent_incomplete "unmanaged agent" "unmanaged-session" stop session-end "agent turn-end"

begin_section "Managed session/turn endings are durable and unattributed"
start_colocated_repo "git-bridge-guard-managed-agent"
CANONICAL_ROOT="$(pwd -P)"
set +e
LIFECYCLE_JSON="$(atomic agent lifecycle begin \
    --owner claude-code \
    --session managed-owner \
    --executor claude-code \
    --view main \
    --workdir "$CANONICAL_ROOT" \
    --json 2>&1)"
LIFECYCLE_RC=$?
set -e
RUN_ID="$(printf '%s\n' "$LIFECYCLE_JSON" | sed -n 's/.*"run_id":"\([^"]*\)".*/\1/p')"
if [[ "$LIFECYCLE_RC" -eq 0 && -n "$RUN_ID" ]]; then
    _pass "managed lifecycle begins"
else
    _fail "managed lifecycle begins" "exit $LIFECYCLE_RC: $LIFECYCLE_JSON"
fi
raw_git_drift managed-drift
printf 'unattributed agent bytes\n' > tracked.txt
assert_agent_incomplete "managed agent" "managed-session" session-end stop "agent session-end"
if [[ -s ".atomic/agent-lifecycle/$RUN_ID.refusal" ]] && \
   grep -qF "$AGENT_RECOVERY_REF" ".atomic/agent-lifecycle/$RUN_ID.refusal" && \
   LIFECYCLE_STATUS="$(atomic agent lifecycle status --json 2>&1)" && \
   printf '%s\n' "$LIFECYCLE_STATUS" | grep -qF "$AGENT_RECOVERY_REF"; then
    _pass "managed lifecycle durably persists its first incomplete WIP outcome"
else
    _fail "managed lifecycle durably persists its first incomplete WIP outcome" "run=$RUN_ID ref=$AGENT_RECOVERY_REF"
fi

print_summary
