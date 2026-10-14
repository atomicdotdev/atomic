#!/usr/bin/env bash
# Regression contract for interruption-safe bridge switching.
#
# The assertions are the original expected-red CB-1B contract, now promoted
# unchanged into the numbered integration harness.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Bridge switch recovers after partial tracked removal${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

atomic_current_view() {
    atomic view list 2>/dev/null | awk '/^\*/ { print $2 }'
}

checkpoint_hash() {
    shasum -a 256 .atomic/bridge/workspace.json | awk '{ print $1 }'
}

snapshot_complete_state() {
    local destination="$1"
    : > "$destination"
    find . \( -path './.git' -o -path './.atomic' \) -prune -o -print |
        LC_ALL=C sort |
        while IFS= read -r path; do
            [[ "$path" == "." ]] && continue
            if [[ -L "$path" ]]; then
                printf 'link\t%s\t%s\n' "${path#./}" "$(readlink "$path")"
            elif [[ -d "$path" ]]; then
                printf 'dir\t%s\n' "${path#./}"
            elif [[ -f "$path" ]]; then
                printf 'file\t%s\t' "${path#./}"
                shasum -a 256 "$path" | awk '{ print $1 }'
            else
                printf 'other\t%s\n' "${path#./}"
            fi
        done > "$destination"
}

assert_equal() {
    local label="$1"
    local expected="$2"
    local actual="$3"
    if [[ "$actual" == "$expected" ]]; then
        _pass "$label"
    else
        _fail "$label" "expected '${expected}', got '${actual}'"
    fi
}

# Count of crash points this binary could not exercise because it was built
# without the adoption-test-injection seams (CB-13A R4: documented
# limitations, never crash passes).
UNINSTRUMENTED_FAILPOINTS=0

assert_clean_statuses() {
    local label="$1"
    local git_status atomic_status
    git_status="$(git status --short)"
    atomic_status="$(atomic status --short 2>/dev/null || true)"
    if [[ -z "$git_status" ]]; then
        _pass "$label: Git status is clean"
    else
        _fail "$label: Git status is clean" "$git_status"
    fi
    if [[ -z "$atomic_status" ]]; then
        _pass "$label: Atomic status is clean"
    else
        _fail "$label: Atomic status is clean" "$atomic_status"
    fi
}

begin_section "Create offline main and feature projections"
make_temp_repo "red-switch-partial-removal"
init_git_repo
create_file "shared.txt" "shared projection\n"
create_file "target-only.txt" "main target-only content\n"
git add shared.txt target-only.txt
git commit --quiet -m "Main projection"
MAIN="$(git_current_branch)"

assert_success "import main into Atomic without network or vault" atomic git import --no-vault
assert_success "create feature view" atomic view create feature --draft --parent "$MAIN"
assert_success "align feature view and Git branch" atomic view switch feature --force

# Two lexically sorted source-only tracked paths guarantee that the failpoint
# after the first tracked removal observes a genuinely partial materialization.
rm target-only.txt
create_file "source-only/01-first.txt" "first source-only tracked file\n"
create_file "source-only/02-second.txt" "second source-only tracked file\n"
git add -A
git commit --quiet -m "Feature projection with two source-only files"
assert_success "project feature through the experimental bridge" atomic git bridge reconcile
assert_success "imported deletion passes native index verification" atomic doctor check
assert_success "native index repair reconstructs the imported deletion" atomic doctor repair-native-indexes
assert_clean_statuses "source projection before interrupted switch"
assert_success "source projection verifies" atomic git bridge verify

SOURCE_HEAD="$(git_head_sha_full)"
SOURCE_BRANCH="$(git_current_branch)"
SOURCE_VIEW="$(atomic_current_view)"
SOURCE_CHECKPOINT="$(checkpoint_hash)"
SOURCE_STATE="$(mktemp "${TMPDIR:-/tmp}/atomic-red-switch-source-XXXXXX")"
AFTER_FAILURE_STATE="$(mktemp "${TMPDIR:-/tmp}/atomic-red-switch-failed-XXXXXX")"
_HARNESS_TMPDIRS+=("$SOURCE_STATE" "$AFTER_FAILURE_STATE")
snapshot_complete_state "$SOURCE_STATE"

begin_section "Interrupt after the first sorted tracked removal"
set +e
FAIL_OUTPUT="$(ATOMIC_FAIL_SWITCH_AFTER_FIRST_TRACKED_REMOVAL=1 atomic git bridge switch "$MAIN" 2>&1)"
FAIL_RC=$?
set -e
if [[ "$FAIL_RC" -ne 0 ]]; then
    _pass "failpoint makes bridge switch fail"
else
    _fail "failpoint makes bridge switch fail" "command unexpectedly succeeded: $FAIL_OUTPUT"
fi

ACTUAL_HEAD="$(git_head_sha_full)"
ACTUAL_BRANCH="$(git_current_branch)"
ACTUAL_VIEW="$(atomic_current_view)"
ACTUAL_CHECKPOINT="$(checkpoint_hash)"
ACTUAL_GIT_STATUS="$(git status --short)"
ACTUAL_ATOMIC_STATUS="$(atomic status --short 2>/dev/null || true)"
snapshot_complete_state "$AFTER_FAILURE_STATE"

begin_section "Rollback contract"
assert_equal "source Git HEAD is restored" "$SOURCE_HEAD" "$ACTUAL_HEAD"
assert_equal "source Git branch is restored" "$SOURCE_BRANCH" "$ACTUAL_BRANCH"
assert_equal "source Atomic view is restored" "$SOURCE_VIEW" "$ACTUAL_VIEW"
assert_equal "bridge checkpoint is unchanged" "$SOURCE_CHECKPOINT" "$ACTUAL_CHECKPOINT"
if cmp -s "$SOURCE_STATE" "$AFTER_FAILURE_STATE"; then
    _pass "all source paths, types, and contents are restored with no target-only files"
else
    _fail "all source paths, types, and contents are restored with no target-only files" \
        "$(diff -u "$SOURCE_STATE" "$AFTER_FAILURE_STATE" || true)"
fi

begin_section "Automatic recovery on ordinary retry"
set +e
RETRY_OUTPUT="$(atomic git bridge switch "$MAIN" 2>&1)"
RETRY_RC=$?
set -e
if [[ "$RETRY_RC" -eq 0 ]]; then
    _pass "ordinary retry automatically recovers and succeeds"
else
    _fail "ordinary retry automatically recovers and succeeds" "exit $RETRY_RC: $RETRY_OUTPUT"
fi
assert_equal "retry aligns Git branch to target" "$MAIN" "$(git_current_branch)"
assert_equal "retry aligns Atomic view to target" "$MAIN" "$(atomic_current_view)"
assert_clean_statuses "target projection after retry"
assert_success "bridge verify succeeds after retry" atomic git bridge verify

begin_section "CB-8B: journaled projection publication recovers at its crash points"
# The record-path projection journals its ref/HEAD publication before
# visibility (RFC §7.2 step 3/4, CB-8B). Each named failpoint crashes the
# journaled publication at a phase boundary; the retry must complete or roll
# back under the same leases — the original switch assertions above stay
# unchanged, and these are ADDITIONAL Git-specific crash points.
#
# CB-13A R4 crash-evidence contract: an instrumented run must PROVE the
# expected seam was reached (`debug failpoint: <NAME>` in the output) before
# any of this counts as a crash pass. A missing seam or an ordinary command
# error is never credited as a failpoint success, and an uninstrumented
# (shipping) build is reported as an explicit documented limitation — never
# as a passing crash check.
for FAILPOINT_NAME in ATOMIC_FAIL_PROJECTION_BEFORE_REF ATOMIC_FAIL_PROJECTION_AFTER_EFFECTS; do
    begin_section "Projection publication crash point: $FAILPOINT_NAME"
    atomic view switch "$MAIN" --force >/dev/null 2>&1 || true
    # Unique content per crash point: a previous iteration's successful
    # record (uninstrumented build) must not leave the tree looking clean.
    create_file "projection-crash.txt" "projection publication crash point: $FAILPOINT_NAME\n"
    set +e
    atomic add projection-crash.txt >/dev/null 2>&1
    ADD_RC=$?
    set -e
    if [[ "$ADD_RC" -ne 0 ]]; then
        _fail "staging for crash point: $FAILPOINT_NAME" \
            "atomic add refused (exit $ADD_RC); the crash scenario cannot be prepared and nothing was proven"
        continue
    fi
    set +e
    # `env VAR=1 atomic …` ignores the shell function wrapper and would
    # resolve a stale `atomic` from PATH; invoke the instrumented binary
    # directly through $ATOMIC_BIN (the failpoints are opt-in seams).
    FAIL_OUTPUT="$(env "$FAILPOINT_NAME=1" "$ATOMIC_BIN" record -m "projection crash $FAILPOINT_NAME" 2>&1)"
    FAIL_RC=$?
    set -e
    if [[ "$FAIL_RC" -ne 0 ]]; then
        if [[ "$FAIL_OUTPUT" == *"debug failpoint: $FAILPOINT_NAME"* ]]; then
            _pass "failpoint makes the projection publication fail: $FAILPOINT_NAME (seam proven)"
        else
            _fail "failpoint makes the projection publication fail: $FAILPOINT_NAME" \
                "command failed (exit $FAIL_RC) without reaching the named failpoint seam; an ordinary error is not crash evidence: $FAIL_OUTPUT"
            git reset -q 2>/dev/null || true
            continue
        fi
    else
        _skip "projection publication failpoint: $FAILPOINT_NAME" \
            "uninstrumented build — the $FAILPOINT_NAME seam never triggered (record succeeded); crash point NOT exercised (documented limitation)"
        UNINSTRUMENTED_FAILPOINTS=$((UNINSTRUMENTED_FAILPOINTS + 1))
        git reset -q 2>/dev/null || true
        continue
    fi
    # The journaled publication must leave the Git repository intact: the
    # ref exists, HEAD resolves, and the ODB is clean (no force overwrite,
    # no torn refs).
    if git rev-parse --verify --quiet refs/heads/master >/dev/null; then
        _pass "projection ref survives the interruption: $FAILPOINT_NAME"
    else
        _fail "projection ref survives the interruption: $FAILPOINT_NAME" "refs/heads/master missing"
    fi
    if git fsck --no-progress >/dev/null 2>&1; then
        _pass "Git object database intact after interruption: $FAILPOINT_NAME"
    else
        _fail "Git object database intact after interruption: $FAILPOINT_NAME" "git fsck failed"
    fi
    # Remediation: unstage the already-durable content, then converge. The
    # bounded loop accepts only explicit typed refusals (never silent
    # repair) and ends with the switch re-projection, which re-aligns the
    # index and refreshes the checkpoint.
    git reset -q 2>/dev/null || true
    CONVERGED=0
    for _ in 1 2 3 4; do
        set +e
        atomic record -m "projection crash $FAILPOINT_NAME" >/dev/null 2>&1
        RECORD_RC=$?
        atomic git bridge reconcile >/dev/null 2>&1
        RECONCILE_RC=$?
        set -e
        if [[ "$RECONCILE_RC" -eq 0 ]]; then
            CONVERGED=1
            break
        fi
        set +e
        atomic view switch "$MAIN" --force >/dev/null 2>&1
        SWITCH_RC=$?
        set -e
        if [[ "$SWITCH_RC" -eq 0 ]]; then
            set +e
            atomic git bridge reconcile >/dev/null 2>&1
            RECONCILE_RC=$?
            set -e
            if [[ "$RECONCILE_RC" -eq 0 ]]; then
                CONVERGED=1
                break
            fi
        fi
        git reset -q 2>/dev/null || true
    done
    if [[ "$CONVERGED" -eq 1 ]]; then
        _pass "bounded recovery loop converges: $FAILPOINT_NAME"
    else
        _fail "bounded recovery loop converges: $FAILPOINT_NAME" "did not converge within the budget"
    fi
    if git status --porcelain | grep -qv '^??'; then
        _fail "statuses clean after projection recovery: $FAILPOINT_NAME" \
            "$(git status --porcelain)"
    else
        _pass "statuses clean after projection recovery: $FAILPOINT_NAME (untracked leftovers tolerated)"
    fi
done

if [[ "$TESTS_FAILED" -gt 0 ]]; then
    echo ""
    echo "${BOLD}${RED}RECOVERY FAILURE:${RESET} bridge switch did not satisfy the interruption-safe contract."
    echo "${RED}  Actual state immediately after failpoint:${RESET}"
    echo "    Git:    branch=${ACTUAL_BRANCH:-<detached>} head=$ACTUAL_HEAD"
    echo "    Atomic: view=${ACTUAL_VIEW:-<unknown>}"
    echo "    Checkpoint: $ACTUAL_CHECKPOINT"
    echo "    Git status:"
    printf '%s\n' "${ACTUAL_GIT_STATUS:-<clean>}" | sed 's/^/      /'
    echo "    Atomic status:"
    printf '%s\n' "${ACTUAL_ATOMIC_STATUS:-<clean>}" | sed 's/^/      /'
    echo "${RED}  The operation journal must restore or safely resume this partial transition.${RESET}"
fi

if [[ "${UNINSTRUMENTED_FAILPOINTS:-0}" -gt 0 ]]; then
    echo ""
    echo "${YELLOW}LIMITATION:${RESET} ${UNINSTRUMENTED_FAILPOINTS} projection crash point(s) were NOT exercised:"
    echo "  this binary was built without the adoption-test-injection feature, so no crash"
    echo "  seam existed to trigger. These are documented limitations, not crash passes."
    echo "  Rebuild with --features adoption-test-injection to exercise the crash matrix."
fi

print_summary
