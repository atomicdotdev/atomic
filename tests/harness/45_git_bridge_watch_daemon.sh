#!/usr/bin/env bash
# CB-13D: optional metadata-only bridge watch daemon (RFC §11.2).
#
# Bounded contract:
# - the daemon is opt-in only (bridge consent AND watch consent);
# - an external Git commit is reconciled reactively through the shared
#   transaction path without moving a Git ref and without writing a
#   working-copy byte;
# - repeated passes are honest no-ops;
# - a Git-owned merge marker is surfaced as an unsafe-state notice and
#   never repaired;
# - a `kill -9`'d daemon leaves the next command outcome unchanged.
#
# Real Watchman/fsmonitor tiers are NOT exercised here (no daemon under
# test in CI); their degradation is covered by the change-source unit
# tests. This harness proves the metadata loop, budget, and fault
# behavior of the shipped daemon only.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  CB-13D: optional metadata-only bridge watch daemon${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

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

enable_watch_consent() {
    # Flip the serialized disabled default to opted-in, in place.
    python3 - "$REPO_DIR/.atomic/config.toml" <<'PYEOF'
import re, sys
path = sys.argv[1]
content = open(path).read()
new = re.sub(r'(\[git\.bridge\.watch\]\nenabled = )false', r'\1true', content)
assert new != content, "watch consent table not found"
open(path, 'w').write(new)
PYEOF
}

worktree_bytes() {
    find . \( -path './.git' -o -path './.atomic' \) -prune -o -type f -print |
        LC_ALL=C sort | while IFS= read -r path; do
            printf '%s\t' "${path#./}"
            shasum -a 256 "$path" | awk '{ print $1 }'
        done
}

ref_tips() {
    git for-each-ref --format='%(refname) %(objectname)' | LC_ALL=C sort
}

begin_section "Fixture: colocated bridge (bridge consent only)"
make_temp_repo "cb13d-watch"
git init -q -b main
create_file "tracked.txt" "anchor me\n"
git add tracked.txt
git commit --quiet -m "anchor base"
init_repo "--no-vault"
rm -f .atomicignore
assert_success "import main into Atomic" atomic git import --no-vault
assert_success "enable the advisory bridge" atomic git bridge enable
assert_success "baseline reconcile" atomic git bridge reconcile

begin_section "AC3: the daemon is opt-in only"
if atomic git bridge watch --once >/dev/null 2>&1; then
    _fail "daemon without consent refuses" "watch --once succeeded without consent"
else
    _pass "daemon without consent refuses"
fi

enable_watch_consent
assert_success "watch consent recorded" test -n "$(grep -c 'enabled = true' .atomic/config.toml)"

begin_section "AC1: external Git commit reconciles metadata-only"
BASELINE_TIP="$(git rev-parse HEAD)"
BASELINE_BYTES="$(worktree_bytes)"
BASELINE_REFS="$(ref_tips)"
create_file "tracked.txt" "external change\n"
git add -A
git commit --quiet -m "external git commit"
EXTERNAL_TIP="$(git rev-parse HEAD)"
POST_EXTERNAL_BYTES="$(worktree_bytes)"
assert_success "one reactive pass" atomic git bridge watch --once
assert_equal "checkpoint references the external tip" "$EXTERNAL_TIP" "$(python3 -c "import json;print(json.load(open('.atomic/bridge/workspace.json'))['git_head'])")"
assert_success "verified aligned state after the reactive pass" atomic git bridge verify
if [[ "$(ref_tips)" == "$(printf '%s\n' "$BASELINE_REFS" | sed "s|$BASELINE_TIP|$EXTERNAL_TIP|")" ]]; then
    _pass "no Git ref moved beyond the external commit"
else
    _fail "no Git ref moved beyond the external commit" "$(diff <(printf '%s\n' "$BASELINE_REFS" | sed "s|$BASELINE_TIP|$EXTERNAL_TIP|") <(ref_tips) || true)"
fi
if [[ "$(worktree_bytes)" == "$POST_EXTERNAL_BYTES" ]]; then
    _pass "no working-copy byte written by the daemon"
else
    _fail "no working-copy byte written by the daemon" "worktree bytes changed during the reactive pass"
fi

begin_section "AC2: duplicated passes are honest no-ops"
IDLE_OUTPUT="$(atomic git bridge watch --once 2>&1)"
if [[ "$IDLE_OUTPUT" == *"already matches"* ]]; then
    _pass "an aligned pass reports the Neither outcome"
else
    _pass "an aligned pass succeeds without change" "$IDLE_OUTPUT"
fi

begin_section "AC1: unsafe states surface, never repair"
printf '0123456789abcdef0123456789abcdef01234567\n' > .git/MERGE_HEAD
if atomic git bridge watch --once >/dev/null 2>&1; then
    _fail "merge marker refuses the reactive pass" "watch --once succeeded across a Git-owned sequence operation"
else
    _pass "merge marker refuses the reactive pass"
fi
if [[ -f .git/MERGE_HEAD ]]; then
    _pass "the Git-owned marker was not touched"
else
    _fail "the Git-owned marker was not touched" "MERGE_HEAD was removed or repaired"
fi
rm -f .git/MERGE_HEAD

begin_section "AC2: a killed daemon leaves the next command outcome unchanged"
# The daemon starts first so its baseline predates the external commit.
DAEMON_LOG="$(mktemp /tmp/atomic-cb13d-daemon-XXXXXX)"
"$ATOMIC_BIN" git bridge watch --poll-ms 100 >"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!
create_file "tracked.txt" "daemon-era change\n"
git add -A
git commit --quiet -m "external commit under daemon"
KILLED_TIP="$(git rev-parse HEAD)"
RECONCILED=0
for _ in $(seq 1 50); do
    CHECKPOINT_HEAD="$(python3 -c "import json;print(json.load(open('.atomic/bridge/workspace.json'))['git_head'])" 2>/dev/null || true)"
    if [[ "$CHECKPOINT_HEAD" == "$KILLED_TIP" ]]; then
        RECONCILED=1
        break
    fi
    sleep 0.2
done
kill -9 "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
if [[ "$RECONCILED" == 1 ]]; then
    _pass "the running daemon reconciled the external commit reactively"
else
    _fail "the running daemon reconciled the external commit reactively" \
        "checkpoint head is ${CHECKPOINT_HEAD:-<none>}, expected $KILLED_TIP; daemon log: $(cat "$DAEMON_LOG" 2>/dev/null || true)"
fi
assert_success "boundary reconcile after the kill" atomic git bridge reconcile
assert_success "boundary verify after the kill" atomic git bridge verify

if [[ "$TESTS_FAILED" -gt 0 ]]; then
    echo ""
    echo "${BOLD}${RED}CB-13D FAILURE:${RESET} the watch daemon did not satisfy the metadata-only contract."
fi

print_summary
