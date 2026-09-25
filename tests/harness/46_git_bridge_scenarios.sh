#!/usr/bin/env bash
# 46_git_bridge_scenarios.sh — Git bridge workflows and failed scenarios.
#
# One section per workflow in docs/testing/git-bridge-test-scenarios.md
# (W1–W13, W16). Assertions state the behaviour that RFC-ATOMIC-GIT-CAUSAL-BRIDGE
# and bridge-operating-guide expect, so a section tagged with an open failed
# scenario (F-number) fails until that scenario is fixed.
#
# Not covered here: W14/F8 (needs the pull save path), W15/F10 (unit tests in
# #213).
#
# Optional: ATOMIC_PREV_BIN=<previous release binary> enables W10 and W11.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 46_git_bridge_scenarios${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

# ── Helpers ─────────────────────────────────────────────────────────────────

# The current view is marked `*`, also when listed as a child (`  - * name`).
current_atomic_view() {
    atomic view list 2>/dev/null | awk '{ for (i = 1; i < NF; i++) if ($i == "*") { print $(i + 1); exit } }'
}

# The Git branch HEAD points at, or `detached`.
git_head_state() {
    git symbolic-ref --quiet --short HEAD 2>/dev/null || echo "detached"
}

assert_git_clean() {
    local label="$1" out
    out="$(git status --short)"
    if [[ -z "$out" ]]; then _pass "$label: git status is clean"; else _fail "$label: git status is clean" "$out"; fi
}

assert_atomic_clean() {
    local label="$1" out rc
    out="$(atomic status --short 2>&1)" && rc=0 || rc=$?
    if [[ $rc -eq 0 && -z "$out" ]]; then
        _pass "$label: atomic status is clean"
    else
        _fail "$label: atomic status is clean" "$(printf '%s' "$out" | head -3 | tr '\n' ' ')"
    fi
}

assert_both_clean() {
    assert_git_clean "$1"
    assert_atomic_clean "$1"
}

# A Git repository with one commit, created in a fresh temp directory.
new_git_repo() {
    make_temp_repo "$1"
    init_git_repo
    create_file "README.md" "hello"
    git add README.md
    git commit --quiet -m "initial"
}

# A Git repository the bridge has anchored. The `.atomicignore` written by
# `atomic init` is removed so these workflows don't inherit F2 (W1 covers it).
new_bridge_repo() {
    new_git_repo "$1"
    atomic init --no-vault >/dev/null 2>&1 || true
    rm -f .atomicignore
    assert_success "setup: bridge anchored" atomic git bridge reconcile
}

# Agent hooks write session state under $HOME; keep them out of the user's HOME.
AGENT_HOME=""
use_agent_home() {
    AGENT_HOME="$(mktemp -d "${TMPDIR:-/tmp}/atomic-agent-home-XXXXXX")"
    _HARNESS_TMPDIRS+=("$AGENT_HOME")
}

agent_hook() {
    local event="$1" payload="$2"
    printf '%s' "$payload" \
        | HOME="$AGENT_HOME" ATOMIC_HOME="$AGENT_HOME/.atomic" \
          atomic agent hooks claude-code "$event" --json 2>/dev/null | head -1 || true
}

assert_turn_recorded() {
    local label="$1" out="$2"
    if printf '%s' "$out" | grep -q '"recorded":true' && ! printf '%s' "$out" | grep -q '"incomplete"'; then
        _pass "$label"
    else
        _fail "$label" "$(printf '%s' "$out" | cut -c1-220)"
    fi
}

# ── W1 / F2. Onboard an existing Git project ────────────────────────────────

begin_section "W1 / F2. Onboard an existing Git project"
new_git_repo "bridge-onboarding"
assert_success "atomic init in an existing Git checkout" atomic init
assert_success "bridge reconcile anchors the workspace" atomic git bridge reconcile
assert_success "bridge verify passes" atomic git bridge verify
assert_file_exists "workspace checkpoint written" ".atomic/bridge/workspace.json"
assert_both_clean "after onboarding"

# ── W2. Git-first daily loop ────────────────────────────────────────────────

begin_section "W2. Git-first daily loop"
new_bridge_repo "bridge-git-first"
append_file "README.md" $'\ngit side edit'
git commit --quiet -am "git side edit"
assert_success "reconcile imports the Git commit" atomic git bridge reconcile
assert_output_contains "Atomic history contains the Git commit" "git side edit" atomic log --format short
assert_success "bridge verify after the Git-first edit" atomic git bridge verify
assert_both_clean "after the Git-first loop"

# ── W3. Atomic-first daily loop ─────────────────────────────────────────────

begin_section "W3. Atomic-first daily loop"
new_bridge_repo "bridge-atomic-first"
HEAD_BEFORE="$(git rev-parse HEAD)"
create_file "atomic.txt" "atomic side edit"
assert_success "atomic add" atomic add atomic.txt
assert_success "atomic record" atomic record -m "atomic side edit"
assert_success "reconcile after the Atomic record" atomic git bridge reconcile
HEAD_AFTER="$(git rev-parse HEAD)"
if [[ "$HEAD_AFTER" != "$HEAD_BEFORE" ]]; then _pass "Git HEAD advanced to the projected commit"; else _fail "Git HEAD advanced to the projected commit" "HEAD unchanged"; fi
if git ls-tree --name-only HEAD | grep -qx "atomic.txt"; then _pass "Git tree contains the Atomic-recorded file"; else _fail "Git tree contains the Atomic-recorded file"; fi
assert_success "second reconcile succeeds" atomic git bridge reconcile
if [[ "$(git rev-parse HEAD)" == "$HEAD_AFTER" ]]; then _pass "second reconcile creates no new commit"; else _fail "second reconcile creates no new commit"; fi
assert_both_clean "after the Atomic-first loop"

# ── W4. Git branch round trip ───────────────────────────────────────────────

begin_section "W4. Git branch round trip"
new_bridge_repo "bridge-git-branch"
MAIN="$(git_current_branch)"
git switch --quiet -c feature
create_file "feature.txt" "feature"
git add feature.txt
git commit --quiet -m "feature work"
assert_success "reconcile adopts the new Git branch" atomic git bridge reconcile
if [[ "$(current_atomic_view)" == "feature" ]]; then _pass "Atomic follows to view 'feature'"; else _fail "Atomic follows to view 'feature'" "current view: $(current_atomic_view)"; fi
assert_both_clean "on feature"
if git switch --quiet "$MAIN" 2>/dev/null; then _pass "git switch back to $MAIN"; else _fail "git switch back to $MAIN" "git switch failed"; fi
assert_success "reconcile after switching back" atomic git bridge reconcile
if [[ "$(current_atomic_view)" == "$MAIN" ]]; then _pass "Atomic follows back to view '$MAIN'"; else _fail "Atomic follows back to view '$MAIN'" "current view: $(current_atomic_view)"; fi
assert_file_not_exists "feature-only file absent on $MAIN" "feature.txt"
assert_both_clean "back on $MAIN"

# ── W5. Atomic view round trip ──────────────────────────────────────────────

begin_section "W5. Atomic view round trip"
new_bridge_repo "bridge-view-switch"
MAIN="$(git_current_branch)"
MAIN_VIEW="$(current_atomic_view)"
assert_success "create Draft view 'topic'" atomic view create topic --parent "$MAIN_VIEW"
assert_success "switch to view 'topic'" atomic view switch topic
if [[ "$(current_atomic_view)" == "topic" ]]; then _pass "current view is 'topic'"; else _fail "current view is 'topic'" "current view: $(current_atomic_view)"; fi
create_file "topic.txt" "topic"
assert_success "add on 'topic'" atomic add topic.txt
assert_success "record on 'topic'" atomic record -m "topic work"
if git ls-tree --name-only HEAD | grep -qx "topic.txt"; then _pass "Git HEAD tree follows the topic view"; else _fail "Git HEAD tree follows the topic view"; fi
# Guide §4: a Draft view projects to a detached HEAD.
if [[ "$(git_head_state)" == "detached" ]]; then _pass "Git HEAD is detached on the Draft view"; else _fail "Git HEAD is detached on the Draft view" "HEAD on $(git_head_state)"; fi
assert_both_clean "on topic"
assert_success "switch back to '$MAIN_VIEW'" atomic view switch "$MAIN_VIEW"
assert_file_not_exists "topic-only file absent after switching back" "topic.txt"
if ! git ls-tree --name-only HEAD | grep -qx "topic.txt"; then _pass "Git HEAD tree follows back"; else _fail "Git HEAD tree follows back"; fi
if [[ "$(git_head_state)" == "$MAIN" ]]; then _pass "Git HEAD back on branch '$MAIN'"; else _fail "Git HEAD back on branch '$MAIN'" "HEAD on $(git_head_state)"; fi
assert_both_clean "back on $MAIN_VIEW"

# ── W6. Status parity ───────────────────────────────────────────────────────

begin_section "W6. Status parity with git status"
new_bridge_repo "bridge-status"
assert_parity() {
    local label="$1" git_out atomic_out
    git_out="$(git status --short)"
    atomic_out="$(atomic status --git 2>&1)" || true
    if [[ "$git_out" == "$atomic_out" ]]; then
        _pass "$label"
    else
        _fail "$label" "git: [$git_out] atomic --git: [$atomic_out]"
    fi
}
append_file "README.md" $'\nedit'
assert_parity "modified tracked file"
create_file "new.txt" "new"
assert_parity "new untracked file"
git add new.txt
assert_parity "file added to the Git index"
git reset --quiet || true
git checkout --quiet README.md || true
rm -f new.txt
assert_parity "clean again"

# ── W7 / F3. Agent session in a bridge repository ───────────────────────────

begin_section "W7 / F3. Agent session in a bridge repository"
new_bridge_repo "bridge-agent"
use_agent_home
agent_hook session-start '{"session_id":"bridge-agent"}' >/dev/null
for turn in 1 2 3; do
    agent_hook user-prompt-submit "{\"session_id\":\"bridge-agent\",\"prompt\":\"turn $turn\"}" >/dev/null
    create_file "agent-$turn.txt" "turn $turn"
    OUT="$(agent_hook stop '{"session_id":"bridge-agent"}')"
    assert_turn_recorded "agent turn $turn is recorded" "$OUT"
done
assert_success "atomic status works during the session" atomic status

# ── W8 / F4. Agent runs git commit inside a turn ────────────────────────────

begin_section "W8 / F4. Agent runs git commit inside a turn"
new_git_repo "agent-git-commit"
atomic init --no-vault >/dev/null 2>&1 || true
use_agent_home
agent_hook session-start '{"session_id":"agent-git"}' >/dev/null
agent_hook user-prompt-submit '{"session_id":"agent-git","prompt":"turn 1"}' >/dev/null
create_file "turn-1.txt" "turn 1"
git add turn-1.txt
git commit --quiet -m "agent commit in turn 1" || _fail "agent git commit in turn 1" "git commit failed"
# Turn 1 may end Incomplete: capturing agent Git commits isn't implemented
# (guide §1, §8). Later turns must still be recorded.
agent_hook stop '{"session_id":"agent-git"}' >/dev/null
for turn in 2 3; do
    agent_hook user-prompt-submit "{\"session_id\":\"agent-git\",\"prompt\":\"turn $turn\"}" >/dev/null
    create_file "turn-$turn.txt" "turn $turn"
    OUT="$(agent_hook stop '{"session_id":"agent-git"}')"
    assert_turn_recorded "turn $turn after the Git commit is recorded" "$OUT"
done

# ── W9 / F5. View switch with a user-created untracked file ─────────────────

begin_section "W9 / F5. View switch keeps a user-created untracked file"
make_temp_repo "switch-untracked"
atomic init --no-vault >/dev/null 2>&1 || true
create_file "base.txt" "base"
assert_success "setup: add base" atomic add base.txt
assert_success "setup: record base" atomic record -m "base"
ROOT_VIEW="$(current_atomic_view)"
assert_success "setup: create view 't'" atomic view create t --draft --parent "$ROOT_VIEW"
assert_success "setup: switch to 't'" atomic view switch t
create_file "notes.txt" "v1"
assert_success "setup: add notes.txt on 't'" atomic add notes.txt
assert_success "setup: record notes.txt on 't'" atomic record -m "add notes on t"
rm notes.txt
assert_success "setup: record the deletion on 't'" atomic record -m "delete notes on t"
assert_success "setup: switch back to '$ROOT_VIEW'" atomic view switch "$ROOT_VIEW"
create_file "notes.txt" "my unsaved work"
atomic view switch t >/dev/null 2>&1 || true
# RFC §20.1 items 9 and 13: the switch either keeps the file or refuses.
assert_file_content "untracked notes.txt survives the switch" "notes.txt" "my unsaved work"

# ── W10 / F6. Upgrade from the previous release ─────────────────────────────

begin_section "W10 / F6. Repository from the previous release keeps working"
if [[ -z "${ATOMIC_PREV_BIN:-}" || ! -x "${ATOMIC_PREV_BIN:-}" ]]; then
    _skip "upgrade from the previous release" "set ATOMIC_PREV_BIN to a previous release binary"
else
    make_temp_repo "upgrade"
    "$ATOMIC_PREV_BIN" init --no-vault >/dev/null 2>&1 || true
    create_file "old.txt" "from the previous release"
    "$ATOMIC_PREV_BIN" add old.txt >/dev/null 2>&1 || true
    "$ATOMIC_PREV_BIN" record -m "previous release change" >/dev/null 2>&1 || true
    assert_success "status after upgrade" atomic status
    assert_output_contains "log after upgrade shows old history" "previous release change" atomic log --format short
    create_file "new.txt" "after upgrade"
    assert_success "add after upgrade" atomic add new.txt
    assert_success "record after upgrade" atomic record -m "after upgrade"
fi

# ── W11 / F7. Older binary used in an upgraded repository ───────────────────

begin_section "W11 / F7. Older binary used in an upgraded repository"
if [[ -z "${ATOMIC_PREV_BIN:-}" || ! -x "${ATOMIC_PREV_BIN:-}" ]]; then
    _skip "older binary in an upgraded repository" "set ATOMIC_PREV_BIN to a previous release binary"
else
    new_bridge_repo "older-binary"
    create_file "a.txt" "new build"
    assert_success "setup: add with this build" atomic add a.txt
    assert_success "setup: record with this build" atomic record -m "new build change"
    create_file "b.txt" "older build"
    "$ATOMIC_PREV_BIN" add b.txt >/dev/null 2>&1 || true
    "$ATOMIC_PREV_BIN" record -m "older build change" >/dev/null 2>&1 || true
    # RFC §14.3: old clients fail closed; this build keeps working.
    assert_success "status still works after the older binary ran" atomic status
    create_file "c.txt" "after the older binary"
    assert_success "add still works after the older binary ran" atomic add c.txt
    assert_success "record still works after the older binary ran" atomic record -m "after the older binary"
fi

# ── W12 / F1. Colocated repository that never enables the bridge ────────────

begin_section "W12 / F1. Colocated repository without the bridge"
make_temp_repo "colocated-native"
init_git_repo
atomic init --no-vault >/dev/null 2>&1 || true
create_file "notes.txt" "native"
assert_success "atomic add without the bridge" atomic add notes.txt
assert_success "atomic record without the bridge" atomic record -m "native work"
assert_success "atomic status without the bridge" atomic status
if [[ -z "$(git ls-files --stage)" ]]; then _pass "Git index untouched"; else _fail "Git index untouched" "$(git ls-files --stage)"; fi
HOOKS="$(ls .git/hooks | grep -v '\.sample$' || true)"
if [[ -z "$HOOKS" ]]; then _pass "no Git hooks installed"; else _fail "no Git hooks installed" "$(printf '%s' "$HOOKS" | tr '\n' ' ')"; fi

# ── W13 / F9. Concurrent same-path changes into the current view ────────────

begin_section "W13 / F9. Concurrent same-path changes into the current view"
# Two independent repositories each add `same.txt`.
make_same_path_source() {
    make_temp_repo "same-path-$1"
    atomic init --no-vault >/dev/null 2>&1 || true
    create_file "same.txt" "$1"
    atomic add same.txt >/dev/null 2>&1 || true
    atomic record -m "same.txt $1" >/dev/null 2>&1 || true
    SOURCE_DIR="$REPO_DIR"
    SOURCE_HASH="$(atomic log --format short 2>/dev/null | awk -v m="$1" '$NF == m { print $1; exit }')"
}
make_same_path_source alpha
SRC_A="$SOURCE_DIR" HASH_A="$SOURCE_HASH"
make_same_path_source beta
SRC_B="$SOURCE_DIR" HASH_B="$SOURCE_HASH"
make_temp_repo "same-path-target"
atomic init --no-vault >/dev/null 2>&1 || true
# Stand-in for pull: copy both change files into the target's change store.
cp -R "$SRC_A/.atomic/changes/." .atomic/changes/
cp -R "$SRC_B/.atomic/changes/." .atomic/changes/
assert_success "insert A into the current view" atomic insert "$HASH_A"
# RFC §3.12 N12, §4 invariant 8: both claims stay live as a name conflict.
assert_success "insert B into the current view is accepted" atomic insert "$HASH_B"
assert_success "read-only log works after the inserts" atomic log --format short
assert_output_contains "history contains both changes" "same.txt beta" atomic log --format short

# ── W16 / F11. `atomic git import` before the first Git commit ──────────────

begin_section "W16 / F11. git import before the first Git commit"
make_temp_repo "import-unborn"
init_git_repo
atomic init --no-vault >/dev/null 2>&1 || true
OUT="$(atomic git import 2>&1)" && RC=0 || RC=$?
# Importing nothing may succeed; if it fails, the error must say why.
if [[ $RC -eq 0 ]] || printf '%s' "$OUT" | grep -Eiq 'no commits|any commits|first commit|empty repository|unborn'; then
    _pass "import before the first commit succeeds or says the repository has no commits"
else
    _fail "import before the first commit succeeds or says the repository has no commits" "$(printf '%s' "$OUT" | head -1)"
fi

print_summary
