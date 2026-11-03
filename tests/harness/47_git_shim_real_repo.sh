#!/usr/bin/env bash
# 47_git_shim_real_repo.sh — The full Git shim lifecycle on a REAL repository.
#
# Every bug the founding-team rollout found came from running the shim on
# real public repositories, not on the fixture suites (39–45). This suite
# clones one real repository and drives the two phases that found them:
#
#   Phase 1 — day one: import, status/verify, enable, reconcile, hooks, a
#     Git commit through the hooks, native records (edit, rename), a feature
#     branch, fast-forward and no-ff merges, shim switches, disable.
#   Phase 2 — day two: amend, revert, cherry-pick, rebase, stash, tag,
#     pull --rebase from a bare remote (fast-forward and diverged), a
#     directory move, a binary file, a native line deletion, a linked
#     worktree. Every mutation is followed by reconcile, verify and both
#     status checks.
#
# Environment:
#   GIT_SHIM_REAL_REPO_URL     repository to clone (default: antirez/sds, 60
#                              commits, imports in seconds)
#   GIT_SHIM_REAL_REPO_REF     ref to check out after cloning (default: HEAD)
#   GIT_SHIM_REAL_REPO_EDIT    tracked text file the flow edits (README.md)
#   GIT_SHIM_REAL_REPO_RENAME  tracked file the flow renames natively (LICENSE;
#                              falls back to the first tracked top-level file)
#   GIT_SHIM_REAL_REPO_DAY2    0 skips phase 2 (default: 1)
#   GIT_SHIM_REAL_REPO_SKIP    1 skips the suite (it needs the network and a
#                              few minutes on the default repository; larger
#                              repositories take hours)
#
# The suite isolates HOME and the Atomic config directory, uses the binary
# helpers.sh resolved ($ATOMIC_BIN), and never touches the machine's Git
# configuration.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 47_git_shim_real_repo${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

REPO_URL="${GIT_SHIM_REAL_REPO_URL:-https://github.com/antirez/sds.git}"
REPO_REF="${GIT_SHIM_REAL_REPO_REF:-HEAD}"
EDIT="${GIT_SHIM_REAL_REPO_EDIT:-README.md}"
RENAME="${GIT_SHIM_REAL_REPO_RENAME:-LICENSE}"
RUN_DAY2="${GIT_SHIM_REAL_REPO_DAY2:-1}"

begin_section "Prerequisites"
if [[ "${GIT_SHIM_REAL_REPO_SKIP:-0}" == "1" ]]; then
    skip_suite "GIT_SHIM_REAL_REPO_SKIP=1"
fi
require_git
_pass "git is installed"
require_network
_pass "network is available"

# ── Isolation ───────────────────────────────────────────────────────────────

ISOLATED_HOME="$(mktemp -d "${TMPDIR:-/tmp}/atomic-shim-real-home-XXXXXX")"
_HARNESS_TMPDIRS+=("$ISOLATED_HOME")
export HOME="$ISOLATED_HOME"
export ATOMIC_CONFIG_DIR="$ISOLATED_HOME/config"
mkdir -p "$ATOMIC_CONFIG_DIR"
export ATOMIC_SERVICE=local ATOMIC_NONINTERACTIVE=1 NO_COLOR=1
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
unset ATOMIC_RPC ATOMIC_DAEMON_SOCKET
BINDING_KEY="$ISOLATED_HOME/binding.key"
printf '%s' "$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')" > "$BINDING_KEY"
STEP_LOG="$ISOLATED_HOME/steps.log"
: > "$STEP_LOG"

# ── Local helpers ───────────────────────────────────────────────────────────

# Run a command; pass when it exits 0, fail with its first error lines.
run_step() {
    local desc="$1"; shift
    local out rc=0
    out="$("$@" 2>&1)" || rc=$?
    printf '=== %s (rc=%s)\n%s\n' "$desc" "$rc" "$out" >> "$STEP_LOG"
    if [[ $rc -eq 0 ]]; then
        _pass "$desc"
    else
        _fail "$desc" "$(echo "$out" | grep -E '✗|rror' | head -2 | cut -c1-200 | tr '\n' ' ')"
    fi
    return 0
}

expect_no_output() {
    local desc="$1"; shift
    local out
    out="$("$@" 2>&1 || true)"
    if [[ -z "$out" ]]; then
        _pass "$desc"
    else
        printf '=== %s (dirty)\n%s\n' "$desc" "$out" >> "$STEP_LOG"
        _fail "$desc" "$(echo "$out" | head -3 | tr '\n' ' ')"
    fi
}

expect_eq() {
    local desc="$1" got="$2" want="$3"
    if [[ "$got" == "$want" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "got '$got', want '$want'"
    fi
}

# reconcile + verify + both statuses: the invariant after every mutation.
checkpoint() {
    local tag="$1"
    run_step "reconcile ($tag)" atomic git shim reconcile
    run_step "verify ($tag)" atomic git shim verify
    expect_no_output "atomic status clean ($tag)" atomic status --short
    expect_no_output "git status clean ($tag)" git status --porcelain
}

atomic_log_mentions() {
    atomic log --short 2>/dev/null | grep -c "$1" || true
}

# ── Clone ───────────────────────────────────────────────────────────────────

begin_section "Clone $REPO_URL"
clone_git_repo "$REPO_URL" "$REPO_REF"
cd "$GIT_REPO_DIR"
git config user.name "Shim Real Repo"
git config user.email "shim-real@example.invalid"
MAIN="$(git_current_branch)"
if [[ ! -f "$EDIT" ]]; then
    skip_suite "edit file '$EDIT' is not in the repository (set GIT_SHIM_REAL_REPO_EDIT)"
fi
if [[ ! -f "$RENAME" ]]; then
    RENAME="$(git ls-files | grep -v '/' | grep -vx "$EDIT" | head -1 || true)"
fi
_pass "cloned: $(git_commit_count) commits, $(git ls-files | wc -l | tr -d ' ') files, branch $MAIN, edit $EDIT, rename $RENAME"

# ── Phase 1: day one ────────────────────────────────────────────────────────

begin_section "Phase 1: import"
run_step "atomic git import" atomic git import --no-vault
expect_no_output "atomic status clean after import" atomic status --short
expect_no_output "git status clean after import" git status --porcelain
run_step "shim verify after import" atomic git shim verify

begin_section "Phase 1: enable"
run_step "shim enable" atomic git shim enable --no-refspecs --binding-key-file "$BINDING_KEY"
run_step "shim reconcile (idle)" atomic git shim reconcile
run_step "shim verify" atomic git shim verify
run_step "hooks status" atomic git hooks status
expect_no_output "atomic status clean after enable" atomic status --short
expect_no_output "git status clean after enable" git status --porcelain

begin_section "Phase 1: git commit through the hooks"
printf '\n// shim real-repo test: git edit\n' >> "$EDIT"
run_step "git commit" git commit -qam 'shim test: git edit'
run_step "reconcile after git commit" atomic git shim reconcile
run_step "verify after git commit" atomic git shim verify
expect_no_output "atomic status clean after git commit" atomic status --short
expect_eq "atomic log carries the git commit" "$(atomic_log_mentions 'shim test: git edit')" "1"

begin_section "Phase 1: native records"
printf '\n// shim real-repo test: atomic edit\n' >> "$EDIT"
run_step "atomic record (edit)" atomic record -am 'shim test: atomic edit'
expect_eq "git HEAD carries the atomic edit" "$(git show "HEAD:$EDIT" | tail -1)" "// shim real-repo test: atomic edit"
NEWNAME="${RENAME}.renamed"
mv "$RENAME" "$NEWNAME"
run_step "atomic record (rename)" atomic record -am 'shim test: atomic rename'
expect_eq "git HEAD has the renamed file" "$(git ls-tree --name-only HEAD "$NEWNAME")" "$NEWNAME"
expect_eq "git HEAD dropped the old name" "$(git ls-tree --name-only HEAD "$RENAME")" ""
run_step "verify after records" atomic git shim verify
expect_no_output "atomic status clean after records" atomic status --short
expect_no_output "git status clean after records" git status --porcelain

begin_section "Phase 1: feature branch and fast-forward merge"
run_step "git switch -c feature/real" git switch -qc feature/real
printf '\n// shim real-repo test: branch edit\n' >> "$EDIT"
run_step "git commit on the branch" git commit -qam 'shim test: branch edit'
run_step "reconcile on the branch" atomic git shim reconcile
run_step "verify on the branch" atomic git shim verify
expect_no_output "atomic status clean on the branch" atomic status --short
run_step "git switch $MAIN" git switch -q "$MAIN"
run_step "reconcile on $MAIN" atomic git shim reconcile
expect_eq "$MAIN lacks the branch edit (git)" "$(git show "HEAD:$EDIT" | grep -c 'branch edit' || true)" "0"
expect_eq "$MAIN lacks the branch edit (worktree)" "$(grep -c 'branch edit' "$EDIT" || true)" "0"
run_step "git merge --ff-only feature/real" git merge -q --ff-only feature/real
run_step "reconcile after the ff merge" atomic git shim reconcile
run_step "verify after the ff merge" atomic git shim verify
expect_eq "$MAIN has the branch edit after the ff merge" "$(grep -c 'branch edit' "$EDIT" || true)" "1"
expect_no_output "atomic status clean after the ff merge" atomic status --short

begin_section "Phase 1: no-ff merge"
run_step "git switch -c feature/noff" git switch -qc feature/noff
printf '\n// shim real-repo test: noff edit\n' >> "$EDIT"
run_step "git commit on feature/noff" git commit -qam 'shim test: noff edit'
run_step "reconcile on feature/noff" atomic git shim reconcile
run_step "git switch $MAIN (2)" git switch -q "$MAIN"
run_step "reconcile on $MAIN (2)" atomic git shim reconcile
run_step "git merge --no-ff feature/noff" git merge -q --no-ff -m 'shim test: merge noff' feature/noff
run_step "reconcile after the no-ff merge" atomic git shim reconcile
run_step "verify after the no-ff merge" atomic git shim verify
expect_no_output "atomic status clean after the no-ff merge" atomic status --short
expect_no_output "git status clean after the no-ff merge" git status --porcelain

begin_section "Phase 1: shim switches"
run_step "shim switch feature/real" atomic git shim switch feature/real
expect_eq "git branch after the shim switch" "$(git branch --show-current)" "feature/real"
run_step "shim switch $MAIN" atomic git shim switch "$MAIN"
expect_eq "git branch after switching back" "$(git branch --show-current)" "$MAIN"
expect_no_output "atomic status clean after the switches" atomic status --short
run_step "verify final" atomic git shim verify

if [[ "$RUN_DAY2" != "1" ]]; then
    run_step "shim disable" atomic git shim disable
    print_summary
    exit $(( TESTS_FAILED > 0 ))
fi

# ── Phase 2: day two ────────────────────────────────────────────────────────

begin_section "Phase 2: amend"
printf '\n// d2: amend base\n' >> "$EDIT"
git commit -qam 'd2: amend base'
checkpoint "after the commit to amend"
printf '// d2: amended\n' >> "$EDIT"
git commit -q --amend -am 'd2: amended'
checkpoint "after amend"
expect_eq "atomic log has the amended commit" "$(atomic_log_mentions 'd2: amended')" "1"
expect_eq "atomic log dropped the amended-away commit" "$(atomic_log_mentions 'd2: amend base')" "0"

begin_section "Phase 2: revert"
run_step "git revert HEAD" git revert --no-edit HEAD
checkpoint "after revert"
expect_eq "revert removed the amended line" "$(grep -c 'd2: amended' "$EDIT" || true)" "0"

begin_section "Phase 2: cherry-pick and branch deletion"
git switch -qc d2/cp
printf 'cherry\n' > d2-cherry.txt
git add -f d2-cherry.txt
git commit -qm 'd2: cherry source'
checkpoint "on the cherry branch"
git switch -q "$MAIN"
run_step "git cherry-pick d2/cp" git cherry-pick d2/cp
checkpoint "after cherry-pick"
expect_eq "cherry-picked file present" "$(cat d2-cherry.txt 2>/dev/null)" "cherry"
git branch -qD d2/cp
checkpoint "after deleting the cherry branch"

begin_section "Phase 2: rebase onto an advanced $MAIN"
printf 'rebase base\n' > d2-rb.txt
git add -f d2-rb.txt
git commit -qm 'd2: rb base'
checkpoint "rb base on $MAIN"
git switch -qc d2/rb HEAD~1
printf 'side\n' > d2-side.txt
git add -f d2-side.txt
git commit -qm 'd2: side on rb'
checkpoint "side branch before rebase"
run_step "git rebase $MAIN" git rebase -q "$MAIN"
checkpoint "after rebase"
expect_eq "rebased branch has $MAIN's file" "$(cat d2-rb.txt 2>/dev/null)" "rebase base"
git switch -q "$MAIN"
git merge -q --ff-only d2/rb
checkpoint "after the ff merge of the rebased branch"

begin_section "Phase 2: stash"
printf '// d2: stashed\n' >> "$EDIT"
git stash -q
checkpoint "after stash"
git stash pop -q
expect_eq "atomic status sees the popped edit" "$(atomic status --short 2>/dev/null | grep -c "$EDIT" || true)" "1"
git commit -qam 'd2: stash pop'
checkpoint "after the stash-pop commit"

begin_section "Phase 2: tag"
git tag v-d2-test
checkpoint "after tag"

begin_section "Phase 2: pull --rebase from a bare remote"
REMOTE="$(mktemp -d "${TMPDIR:-/tmp}/atomic-shim-real-remote-XXXXXX")"
CLONE="$(mktemp -d "${TMPDIR:-/tmp}/atomic-shim-real-clone-XXXXXX")"
_HARNESS_TMPDIRS+=("$REMOTE" "$CLONE")
git init -q --bare -b "$MAIN" "$REMOTE"
git remote add d2 "$REMOTE"
run_step "git push to the bare remote" git push -q d2 "$MAIN"
rm -rf "$CLONE"
run_step "clone the bare remote" git clone -q -b "$MAIN" "$REMOTE" "$CLONE"
if ( cd "$CLONE" && git config user.name t && git config user.email t@example.invalid \
        && printf 'remote\n' > d2-remote.txt && git add -f d2-remote.txt \
        && git commit -qm 'd2: remote edit' && git push -q origin "$MAIN" ) >/dev/null 2>&1; then
    _pass "remote clone commits and pushes"
else
    _fail "remote clone commits and pushes"
fi
run_step "git pull --rebase (fast-forward)" git pull -q --rebase d2 "$MAIN"
checkpoint "after pull --rebase (fast-forward)"
expect_eq "remote file present" "$(cat d2-remote.txt 2>/dev/null)" "remote"
printf 'local\n' > d2-local.txt
git add -f d2-local.txt
git commit -qm 'd2: local edit'
checkpoint "local commit before the diverging pull"
if ( cd "$CLONE" && printf 'remote2\n' >> d2-remote.txt && git commit -qam 'd2: remote edit 2' && git push -q origin "$MAIN" ) >/dev/null 2>&1; then
    _pass "remote clone pushes a second edit"
else
    _fail "remote clone pushes a second edit"
fi
run_step "git pull --rebase (diverged)" git pull -q --rebase d2 "$MAIN"
checkpoint "after pull --rebase (diverged)"
expect_eq "both edits present" "$(cat d2-local.txt d2-remote.txt 2>/dev/null | tr '\n' ' ')" "local remote remote2 "

begin_section "Phase 2: directory move"
MOVE_DIR="$(git ls-files | awk -F/ 'NF>1{print $1}' | sort | uniq -c | awk '$1>=2 && $1<=25 {print $2}' | head -1)"
if [[ -n "$MOVE_DIR" ]]; then
    run_step "git mv $MOVE_DIR $MOVE_DIR-moved" git mv -k "$MOVE_DIR" "$MOVE_DIR-moved"
    git commit -qm "d2: move $MOVE_DIR"
    checkpoint "after the directory move"
    expect_eq "moved directory present" "$([[ -d "$MOVE_DIR-moved" ]] && echo yes)" "yes"
else
    _skip "directory move" "no tracked directory with 2–25 files"
fi

begin_section "Phase 2: binary file"
head -c 4096 /dev/urandom > d2-bin.dat
git add -f d2-bin.dat
git commit -qm 'd2: binary add'
checkpoint "after the binary add"
head -c 5000 /dev/urandom > d2-bin.dat
git commit -qam 'd2: binary change'
checkpoint "after the binary change"

begin_section "Phase 2: native line deletion, then a git commit"
tail -n +2 "$EDIT" > "$EDIT.tmp" && mv "$EDIT.tmp" "$EDIT"
run_step "atomic record (native line deletion)" atomic record -am 'd2: native delete'
checkpoint "after the native deletion record"
printf 'after\n' > d2-after.txt
git add -f d2-after.txt
git commit -qm 'd2: after native delete'
checkpoint "git commit after the native deletion"

begin_section "Phase 2: linked worktree"
WT="$(mktemp -d "${TMPDIR:-/tmp}/atomic-shim-real-wt-XXXXXX")"
_HARNESS_TMPDIRS+=("$WT")
rm -rf "$WT"
run_step "git worktree add" git worktree add -q -b d2/wt "$WT"
if ( cd "$WT" && printf 'wt\n' > d2-wt.txt && git add -f d2-wt.txt \
        && git commit -qm 'd2: worktree commit' && git push -q d2 d2/wt ) >/dev/null 2>&1; then
    _pass "commit and push from the linked worktree pass through the hooks"
else
    _fail "commit and push from the linked worktree pass through the hooks"
fi
if [[ -e "$WT/.atomic" ]]; then
    _fail "the linked worktree gained no stray .atomic"
else
    _pass "the linked worktree gained no stray .atomic"
fi
git worktree remove --force "$WT" >/dev/null 2>&1 || true
git branch -qD d2/wt >/dev/null 2>&1 || true
checkpoint "after the worktree removal"

begin_section "Disable"
run_step "shim disable" atomic git shim disable

echo ""
echo "  step log: $STEP_LOG"
print_summary
exit $(( TESTS_FAILED > 0 ))
