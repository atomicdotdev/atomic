#!/usr/bin/env bash
# 47_git_bridge_history_corpus.sh — Git history shapes through the bridge.
#
# RFC-ATOMIC-GIT-CAUSAL-BRIDGE §15 item 4 asks for corpus imports (merges,
# octopus, renames, CRLF, LFS, submodules) with per-commit tree verification.
# Each history is checked two ways:
#   onboarding — build the whole history, then `git import` + `bridge enable`;
#   daily loop — anchor after the first commit, then `bridge reconcile` after
#                every Git step.
# After each check point: `bridge verify` passes, both statuses are clean, and
# restoring every tracked file from Atomic reproduces the Git tree exactly
# (bytes, symlinks, executable bits).
#
# Optional: BRIDGE_CORPUS_URLS="<git url> ..." replaces the real repositories
# (default: go-uuid and spark, skipped without network).

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 47_git_bridge_history_corpus${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git
if ! command -v openssl >/dev/null 2>&1; then
    skip_suite "openssl is needed to create a bridge binding key"
fi

# ── Helpers ─────────────────────────────────────────────────────────────────

KEY_DIR="$(mktemp -d "${TMPDIR:-/tmp}/atomic-binding-key-XXXXXX")"
_HARNESS_TMPDIRS+=("$KEY_DIR")
BINDING_KEY="$KEY_DIR/binding.key.hex"
openssl rand -hex 32 > "$BINDING_KEY"

new_corpus_repo() {
    make_temp_repo "$1"
    init_git_repo
    git config core.autocrlf false
}

# Anchor the current Git history with the RFC §14.4 path.
anchor_bridge() {
    local label="$1"
    atomic init --no-vault >/dev/null 2>&1 || true
    rm -f .atomicignore
    assert_success "$label: git import" atomic git import
    assert_success "$label: bridge enable" atomic git bridge enable --binding-key-file "$BINDING_KEY"
}

# Every tracked path restored from Atomic must equal the Git tree.
assert_restore_matches_git() {
    local label="$1" out
    git ls-files -z | xargs -0 rm -f 2>/dev/null || true
    atomic restore --force >/dev/null 2>&1 || true
    out="$(git status --porcelain 2>&1)"
    if [[ -z "$out" ]]; then
        _pass "$label: restoring from Atomic reproduces the Git tree"
    else
        _fail "$label: restoring from Atomic reproduces the Git tree" "$(printf '%s' "$out" | head -4 | tr '\n' ' ')"
    fi
    # Put Git's bytes back so a failure doesn't cascade into later steps.
    git reset --quiet --hard HEAD 2>/dev/null || true
}

assert_bridge_state() {
    local label="$1" git_out atomic_out rc
    assert_success "$label: bridge verify" atomic git bridge verify
    git_out="$(git status --porcelain)"
    atomic_out="$(atomic status --short 2>&1)" && rc=0 || rc=$?
    if [[ -z "$git_out" && $rc -eq 0 && -z "$atomic_out" ]]; then
        _pass "$label: git and atomic status are clean"
    else
        _fail "$label: git and atomic status are clean" "git: [$(printf '%s' "$git_out" | head -2 | tr '\n' ' ')] atomic: [$(printf '%s' "$atomic_out" | head -2 | tr '\n' ' ')]"
    fi
    assert_restore_matches_git "$label"
}

# Git steps run outside errexit so one failing Git command reports a failure
# instead of aborting the suite.
run_step() {
    "$1" || _fail "Git step ${1#step_}" "a Git command in the step failed"
}

# run_corpus <name> <step functions...>
# Onboarding: all steps, then anchor. Daily loop: first step, anchor, then
# reconcile after each remaining step.
run_corpus() {
    local name="$1"; shift
    local step

    begin_section "$name: onboarding"
    new_corpus_repo "corpus-$name-onboard"
    for step in "$@"; do run_step "$step"; done
    anchor_bridge "onboard"
    assert_bridge_state "onboarded"

    begin_section "$name: daily loop"
    new_corpus_repo "corpus-$name-daily"
    run_step "$1"
    anchor_bridge "anchor"
    assert_bridge_state "after ${1#step_}"
    shift
    for step in "$@"; do
        run_step "$step"
        assert_success "reconcile after ${step#step_}" atomic git bridge reconcile
        assert_bridge_state "after ${step#step_}"
    done
}

commit_all() {
    git add -A
    git commit --quiet -m "$1"
}

# ── Merge commits ───────────────────────────────────────────────────────────

step_merge_base() { printf 'base\n' > a.txt; commit_all "base"; }
step_merge_side() {
    git switch --quiet -c side
    printf 'side\n' > b.txt
    commit_all "side adds b.txt"
    git switch --quiet -
}
step_merge_main() { printf 'main\n' >> a.txt; commit_all "main edits a.txt"; }
step_merge_noff() { git merge --quiet --no-ff side -m "merge side"; git branch --quiet -D side; }
run_corpus "merge" step_merge_base step_merge_side step_merge_main step_merge_noff

# ── Octopus merge ───────────────────────────────────────────────────────────

step_octo_base() { printf 'base\n' > base.txt; commit_all "base"; }
step_octo_branches() {
    local b
    for b in one two; do
        git switch --quiet -c "$b"
        printf '%s\n' "$b" > "$b.txt"
        commit_all "$b"
        git switch --quiet -
    done
}
step_octo_merge() { git merge --quiet one two -m "octopus"; git branch --quiet -D one two; }
run_corpus "octopus" step_octo_base step_octo_branches step_octo_merge

# ── Renames ─────────────────────────────────────────────────────────────────

step_rename_add() { printf 'hello\n' > old.txt; commit_all "add old.txt"; }
step_rename_move() { mkdir -p dir; git mv old.txt dir/new.txt; commit_all "rename"; }
step_rename_edit() { printf 'more\n' >> dir/new.txt; commit_all "edit renamed file"; }
run_corpus "rename" step_rename_add step_rename_move step_rename_edit

# ── Delete and re-add a path ────────────────────────────────────────────────

step_readd_add() { printf 'first\n' > f.txt; commit_all "add f.txt"; }
step_readd_delete() { git rm --quiet f.txt; commit_all "delete f.txt"; }
step_readd_again() { printf 'second\n' > f.txt; commit_all "re-add f.txt"; }
run_corpus "delete-readd" step_readd_add step_readd_delete step_readd_again

# ── CRLF bytes ──────────────────────────────────────────────────────────────

step_crlf_add() { printf 'one\r\ntwo\r\n' > crlf.txt; commit_all "add CRLF file"; }
step_crlf_edit() { printf 'one\r\ntwo\r\nthree\r\n' > crlf.txt; commit_all "append CRLF line"; }
run_corpus "crlf" step_crlf_add step_crlf_edit

# ── Binary files ────────────────────────────────────────────────────────────

step_bin_add() { printf 'bin\0ary\1\2\3' > data.bin; commit_all "add binary"; }
step_bin_edit() { printf 'bin\0ARY\4\5\6\7' > data.bin; commit_all "edit binary"; }
run_corpus "binary" step_bin_add step_bin_edit

# ── Symlinks ────────────────────────────────────────────────────────────────

step_link_add() { printf 'target\n' > target.txt; ln -s target.txt link; commit_all "add symlink"; }
step_link_retarget() { printf 'other\n' > other.txt; ln -sfn other.txt link; commit_all "retarget symlink"; }
run_corpus "symlink" step_link_add step_link_retarget

# ── Executable bit ──────────────────────────────────────────────────────────

step_exec_add() { printf '#!/bin/sh\necho hi\n' > run.sh; commit_all "add script"; }
step_exec_on() { chmod +x run.sh; commit_all "make executable"; }
step_exec_off() { chmod -x run.sh; commit_all "drop executable"; }
run_corpus "exec-bit" step_exec_add step_exec_on step_exec_off

# ── Unusual path names ──────────────────────────────────────────────────────

step_names_add() { mkdir -p "dir with space"; printf 'x\n' > "dir with space/ñame é.txt"; commit_all "unicode path"; }
step_names_edit() { printf 'y\n' >> "dir with space/ñame é.txt"; commit_all "edit unicode path"; }
run_corpus "path-names" step_names_add step_names_edit

# ── Empty commit ────────────────────────────────────────────────────────────

step_empty_add() { printf 'x\n' > f.txt; commit_all "add f.txt"; }
step_empty_commit() { git commit --quiet --allow-empty -m "empty"; }
run_corpus "empty-commit" step_empty_add step_empty_commit

# ── Submodule ───────────────────────────────────────────────────────────────

SUB_SOURCE=""
make_submodule_source() {
    SUB_SOURCE="$(mktemp -d "${TMPDIR:-/tmp}/atomic-submodule-XXXXXX")"
    _HARNESS_TMPDIRS+=("$SUB_SOURCE")
    git -C "$SUB_SOURCE" init --quiet
    git -C "$SUB_SOURCE" config user.email "test@atomic.dev"
    git -C "$SUB_SOURCE" config user.name "Test User"
    printf 'lib\n' > "$SUB_SOURCE/lib.txt"
    git -C "$SUB_SOURCE" add lib.txt
    git -C "$SUB_SOURCE" commit --quiet -m "lib"
}
make_submodule_source
step_sub_base() { printf 'app\n' > app.txt; commit_all "app"; }
step_sub_add() {
    git -c protocol.file.allow=always submodule --quiet add "$SUB_SOURCE" vendor/lib >/dev/null 2>&1
    commit_all "add submodule"
}
run_corpus "submodule" step_sub_base step_sub_add

# ── Git LFS ─────────────────────────────────────────────────────────────────

if git lfs version >/dev/null 2>&1; then
    step_lfs_setup() { git lfs install --local >/dev/null 2>&1; git lfs track "*.big" >/dev/null 2>&1; commit_all "track *.big"; }
    step_lfs_add() { printf 'large payload\n' > asset.big; commit_all "add LFS file"; }
    run_corpus "lfs" step_lfs_setup step_lfs_add
else
    begin_section "lfs"
    _skip "Git LFS histories" "git-lfs is not installed"
fi

# ── Real repositories ───────────────────────────────────────────────────────

for url in ${BRIDGE_CORPUS_URLS:-https://github.com/hashicorp/go-uuid.git https://github.com/holman/spark.git}; do
    begin_section "real repository: $url"
    if ! git ls-remote --heads "$url" >/dev/null 2>&1; then
        _skip "real repository $url" "not reachable"
        continue
    fi
    make_temp_repo "corpus-real"
    git clone --quiet "$url" repo 2>/dev/null
    cd repo
    git remote set-url --push origin DISABLED
    START=$SECONDS
    anchor_bridge "$(basename "$url" .git)"
    echo "    ${YELLOW}ℹ import + enable: $((SECONDS - START))s for $(git rev-list --count HEAD) commits, $(git ls-files | wc -l | tr -d ' ') files${RESET}"
    assert_bridge_state "$(basename "$url" .git)"
done

print_summary
