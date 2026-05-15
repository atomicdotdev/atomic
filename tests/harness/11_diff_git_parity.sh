#!/usr/bin/env bash
# shellcheck disable=SC2034,SC2206,SC2128
# 11_diff_git_parity.sh — Diff format parity tests: atomic diff vs git diff.
#
# Validates that `atomic diff -c <hash>` produces the same change lines
# as `git diff PARENT..COMMIT` for the same commits.
#
# Strategy:
#   1. Create a git repo with known file edits
#   2. Run `atomic git import` to convert git history to atomic changes
#   3. For each commit, compare the +/- lines from git diff against
#      the +/- lines from atomic diff -c
#
# We also run against the real hyperfine repo as an end-to-end test.
#
# What MUST match:
#   - Every `-` line (deletions) in the same order
#   - Every `+` line (insertions) in the same order

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# Pre-declare arrays at script scope so set -u doesn't trip on them.
ATOMIC_HASHES=()
GIT_SHAS=()
GIT_REPO=""

# ── Helpers ─────────────────────────────────────────────────────────────────

# Extract only +/- change lines from git unified diff output.
# Strips headers (diff, ---, +++, @@) and context lines.
extract_git_change_lines() {
    grep -E '^\+[^+]|^-[^-]' || true
}

# Extract only +/- change lines from atomic diff output.
# Atomic's --no-color format (with show_line_numbers=false) is:
#   -content
#   +content
# Same as git, so we use the same extractor.
# But we also need to skip the change header block (change hash, Date, message).
extract_atomic_change_lines() {
    grep -E '^\+[^+]|^-[^-]' || true
}

# Create a git repo, apply a sequence of file versions, import into atomic.
#
# Usage:
#   setup_git_and_import "label" "file_path" version1 version2 ...
#
# After calling:
#   GIT_REPO   — path to the git repo (also the atomic repo after import)
#   GIT_SHAS   — array of git commit SHAs in order
#   ATOMIC_HASHES — array of atomic change hashes in order (parallel to GIT_SHAS)
setup_git_and_import() {
    local label="$1"
    local file_path="$2"
    shift 2
    # remaining args are version contents

    make_temp_repo "$label"
    GIT_REPO="$REPO_DIR"

    git -C "$GIT_REPO" init --quiet 2>/dev/null
    git -C "$GIT_REPO" config user.email "test@test.com" 2>/dev/null
    git -C "$GIT_REPO" config user.name "Test" 2>/dev/null

    GIT_SHAS=()
    local version_num=0
    for content in "$@"; do
        version_num=$((version_num + 1))
        mkdir -p "$GIT_REPO/$(dirname "$file_path")"
        # Bash command substitution $(...) strips trailing newlines.
        # To preserve the final \n that belongs in the file, we use printf
        # with the content variable but append a newline since the variable
        # already lost its trailing newline during assignment.
        printf '%s\n' "$content" > "$GIT_REPO/$file_path"
        git -C "$GIT_REPO" add -A >/dev/null 2>&1
        git -C "$GIT_REPO" commit -m "v$version_num" --quiet 2>/dev/null
        local sha
        sha=$(git -C "$GIT_REPO" rev-parse HEAD 2>/dev/null)
        GIT_SHAS+=("$sha")
    done

    # Run atomic git import
    (cd "$GIT_REPO" && atomic git import 2>/dev/null) || true

    # Collect atomic change hashes in order (oldest first).
    # `atomic log` lists newest first, so we reverse.
    ATOMIC_HASHES=()
    local log_output
    log_output=$( (cd "$GIT_REPO" && atomic log --format short --no-color --full-hash 2>/dev/null) || true )
    if [[ -n "${log_output:-}" ]]; then
        # Log lists newest first. Collect into temp, then reverse.
        local _tmp=()
        while IFS= read -r line; do
            local h
            h=$(echo "$line" | awk '{print $1}')
            if [[ -n "$h" ]]; then
                _tmp+=("$h")
            fi
        done <<< "$log_output"
        # Reverse: oldest first to match GIT_SHAS order
        local _i
        for (( _i=${#_tmp[@]}-1 ; _i>=0 ; _i-- )); do
            ATOMIC_HASHES+=("${_tmp[$_i]}")
        done
    fi
}

# Compare git diff (parent..commit) against atomic diff -c for a specific
# commit index (0-based).  The git diff is between GIT_SHAS[idx-1] and
# GIT_SHAS[idx].  The atomic diff is for ATOMIC_HASHES[idx].
# assert_commit_parity "desc" idx [path_filter]
#
# path_filter: optional path to pass to `git diff` and extract from
# `atomic diff`.  When provided, only lines touching that path are
# compared.  Useful for commits that also touch lock files or other
# generated files whose diffs differ by algorithm.
assert_commit_parity() {
    local desc="$1"
    local idx="$2"
    local path_filter="${3:-}"

    set +u
    local _n_atomic=${#ATOMIC_HASHES[@]}
    set -u
    if [[ "$_n_atomic" -eq 0 ]]; then
        _fail "$desc" "no atomic changes found (import may have failed)"
        return
    fi

    if [[ $idx -lt 1 ]]; then
        _skip "$desc" "first commit (no parent to diff against)"
        return
    fi

    local prev_sha="${GIT_SHAS[$((idx - 1))]}"
    local curr_sha="${GIT_SHAS[$idx]}"

    if [[ -z "$prev_sha" || -z "$curr_sha" ]]; then
        _fail "$desc" "missing git SHA at index $idx"
        return
    fi

    # Git diff (optionally scoped to a single path)
    local git_lines_file="$REPO_DIR/git_lines_$idx.txt"
    if [[ -n "$path_filter" ]]; then
        git -C "$GIT_REPO" --no-pager diff "$prev_sha" "$curr_sha" -- "$path_filter" 2>/dev/null \
            | extract_git_change_lines > "$git_lines_file"
    else
        git -C "$GIT_REPO" --no-pager diff "$prev_sha" "$curr_sha" 2>/dev/null \
            | extract_git_change_lines > "$git_lines_file"
    fi

    # Atomic diff
    set +u
    local _n_ah=${#ATOMIC_HASHES[@]}
    set -u
    local atomic_hash=""
    if [[ $idx -lt $_n_ah ]]; then
        atomic_hash="${ATOMIC_HASHES[$idx]}"
    fi
    local atomic_lines_file="$REPO_DIR/atomic_lines_$idx.txt"

    if [[ -z "$atomic_hash" ]]; then
        touch "$atomic_lines_file"
    else
        local raw
        raw=$( (cd "$GIT_REPO" && atomic diff -c "$atomic_hash" --no-color 2>/dev/null) || true )
        if [[ -n "$raw" ]]; then
            if [[ -n "$path_filter" ]]; then
                # Extract only the +/- lines from the section of the atomic diff
                # that belongs to path_filter.  awk handles both cases: when
                # path_filter is the last section (no following "diff --atomic")
                # and when it is not.
                printf '%s\n' "$raw" | awk -v pat="$path_filter" '
                    /^diff --atomic/ { in_sec = ($0 ~ pat) ? 1 : 0; next }
                    in_sec { print }
                ' | extract_atomic_change_lines > "$atomic_lines_file" || true
            else
                printf '%s\n' "$raw" | extract_atomic_change_lines > "$atomic_lines_file"
            fi
        else
            touch "$atomic_lines_file"
        fi
    fi

    local git_count atomic_count
    git_count=$(wc -l < "$git_lines_file" | tr -d ' ') || true
    atomic_count=$(wc -l < "$atomic_lines_file" | tr -d ' ') || true

    # Compare deletions
    local git_del="$REPO_DIR/git_del_$idx.txt"
    local atomic_del="$REPO_DIR/atomic_del_$idx.txt"
    grep '^-' "$git_lines_file" > "$git_del" 2>/dev/null || touch "$git_del"
    grep '^-' "$atomic_lines_file" > "$atomic_del" 2>/dev/null || touch "$atomic_del"

    if diff -u "$git_del" "$atomic_del" > /dev/null 2>&1; then
        local del_count
        del_count=$(wc -l < "$git_del" | tr -d ' ') || true
        _pass "$desc: deletions match ($del_count)"
    else
        local gdc adc
        gdc=$(wc -l < "$git_del" | tr -d ' ') || true
        adc=$(wc -l < "$atomic_del" | tr -d ' ') || true
        _fail "$desc: deletions" "git=$gdc atomic=$adc"
    fi

    # Compare insertions
    local git_ins="$REPO_DIR/git_ins_$idx.txt"
    local atomic_ins="$REPO_DIR/atomic_ins_$idx.txt"
    grep '^\+' "$git_lines_file" > "$git_ins" 2>/dev/null || touch "$git_ins"
    grep '^\+' "$atomic_lines_file" > "$atomic_ins" 2>/dev/null || touch "$atomic_ins"

    if diff -u "$git_ins" "$atomic_ins" > /dev/null 2>&1; then
        local ins_count
        ins_count=$(wc -l < "$git_ins" | tr -d ' ') || true
        _pass "$desc: insertions match ($ins_count)"
    else
        local gic aic
        gic=$(wc -l < "$git_ins" | tr -d ' ') || true
        aic=$(wc -l < "$atomic_ins" | tr -d ' ') || true
        _fail "$desc: insertions" "git=$gic atomic=$aic"
    fi

    # Total line count
    if [[ "$git_count" -eq "$atomic_count" ]]; then
        _pass "$desc: total lines match ($git_count)"
    else
        _fail "$desc: total lines" "git=$git_count atomic=$atomic_count"
    fi
}

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Single line modification"
# ═══════════════════════════════════════════════════════════════════════════

V1=$(printf 'line1\nline2\nline3\nline4\nline5\n')
V2=$(printf 'line1\nmodified-line2\nline3\nline4\nline5\n')

setup_git_and_import "parity-single-mod" "test.txt" "$V1" "$V2"
assert_commit_parity "single line mod" 1

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Pure insertion (middle)"
# ═══════════════════════════════════════════════════════════════════════════

V1=$(printf 'line1\nline2\nline3\n')
V2=$(printf 'line1\nline2\nnew-line\nline3\n')

setup_git_and_import "parity-insert-middle" "test.txt" "$V1" "$V2"
assert_commit_parity "insert middle" 1

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Pure insertion (beginning)"
# ═══════════════════════════════════════════════════════════════════════════

V1=$(printf 'line1\nline2\nline3\n')
V2=$(printf 'new-first-line\nline1\nline2\nline3\n')

setup_git_and_import "parity-insert-begin" "test.txt" "$V1" "$V2"
assert_commit_parity "insert beginning" 1

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Pure insertion (end)"
# ═══════════════════════════════════════════════════════════════════════════

V1=$(printf 'line1\nline2\nline3\n')
V2=$(printf 'line1\nline2\nline3\nnew-last-line\n')

setup_git_and_import "parity-insert-end" "test.txt" "$V1" "$V2"
assert_commit_parity "insert end" 1

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Pure deletion (middle)"
# ═══════════════════════════════════════════════════════════════════════════

V1=$(printf 'line1\nline2\nline3\nline4\nline5\n')
V2=$(printf 'line1\nline2\nline4\nline5\n')

setup_git_and_import "parity-delete-middle" "test.txt" "$V1" "$V2"
assert_commit_parity "delete middle" 1

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Pure deletion (multiple lines)"
# ═══════════════════════════════════════════════════════════════════════════

V1=$(printf 'line1\nline2\nline3\nline4\nline5\nline6\n')
V2=$(printf 'line1\nline4\nline6\n')

setup_git_and_import "parity-delete-multi" "test.txt" "$V1" "$V2"
assert_commit_parity "delete multiple" 1

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Replace block (delete + insert at same position)"
# ═══════════════════════════════════════════════════════════════════════════

V1=$(printf 'header\nold-code-1\nold-code-2\nold-code-3\nfooter\n')
V2=$(printf 'header\nnew-code-A\nnew-code-B\nfooter\n')

setup_git_and_import "parity-replace" "test.txt" "$V1" "$V2"
assert_commit_parity "replace block" 1

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Multiple hunks in one file"
# ═══════════════════════════════════════════════════════════════════════════

V1=$(printf 'a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\na11\na12\na13\na14\na15\na16\na17\na18\na19\na20\n')
V2=$(printf 'a1\nMOD-a2\na3\na4\na5\na6\na7\na8\na9\na10\na11\na12\na13\na14\na15\na16\na17\na18\na19\nMOD-a20\n')

setup_git_and_import "parity-multi-hunk" "test.txt" "$V1" "$V2"
assert_commit_parity "multi-hunk" 1

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Function extraction (add + delete + modify)"
# ═══════════════════════════════════════════════════════════════════════════

V1=$(printf 'fn main() {\n    let bar = Bar::new(10);\n    let style = Style::new()\n        .template("{spinner}");\n    bar.set_style(style);\n    bar.enable_steady_tick(80);\n    bar.set_message("Running");\n\n    for i in 0..10 {\n        bar.inc(1);\n        run(i);\n    }\n    bar.finish();\n}\n')

V2=$(printf 'fn get_bar(n: u64, msg: &str) -> Bar {\n    let style = Style::new()\n        .template("{spinner}");\n    let bar = Bar::new(n);\n    bar.set_style(style);\n    bar.enable_steady_tick(80);\n    bar.set_message(msg);\n    bar\n}\n\nfn main() {\n    let bar = get_bar(10, "Running");\n\n    for i in 0..10 {\n        bar.inc(1);\n        run(i);\n    }\n    bar.finish();\n}\n')

setup_git_and_import "parity-extract-fn" "src/main.rs" "$V1" "$V2"
assert_commit_parity "extract function" 1

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Delete entire file"
# ═══════════════════════════════════════════════════════════════════════════

V1=$(printf 'line1\nline2\nline3\n')

make_temp_repo "parity-delete-file"
GIT_REPO="$REPO_DIR"

git -C "$GIT_REPO" init --quiet 2>/dev/null
git -C "$GIT_REPO" config user.email "test@test.com" 2>/dev/null
git -C "$GIT_REPO" config user.name "Test" 2>/dev/null

printf '%s' "$V1" > "$GIT_REPO/victim.txt"
git -C "$GIT_REPO" add -A >/dev/null 2>&1
git -C "$GIT_REPO" commit -m "v1" --quiet 2>/dev/null
SHA1=$(git -C "$GIT_REPO" rev-parse HEAD 2>/dev/null)

rm "$GIT_REPO/victim.txt"
git -C "$GIT_REPO" add -A >/dev/null 2>&1
git -C "$GIT_REPO" commit -m "v2 delete" --quiet 2>/dev/null
SHA2=$(git -C "$GIT_REPO" rev-parse HEAD 2>/dev/null)

GIT_SHAS=("$SHA1" "$SHA2")

(cd "$GIT_REPO" && atomic git import 2>/dev/null) || true

ATOMIC_HASHES=()
log_output=$( (cd "$GIT_REPO" && atomic log --format short --no-color --full-hash 2>/dev/null) || true )
if [[ -n "${log_output:-}" ]]; then
    _tmp=()
    while IFS= read -r line; do
        _h=$(echo "$line" | awk '{print $1}')
        if [[ -n "$_h" ]]; then
            _tmp+=("$_h")
        fi
    done <<< "$log_output"
    for (( _i=${#_tmp[@]}-1 ; _i>=0 ; _i-- )); do
        ATOMIC_HASHES+=("${_tmp[$_i]}")
    done
fi

assert_commit_parity "delete file" 1

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Multiple files changed in one commit"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "parity-multi-file"
GIT_REPO="$REPO_DIR"

git -C "$GIT_REPO" init --quiet 2>/dev/null
git -C "$GIT_REPO" config user.email "test@test.com" 2>/dev/null
git -C "$GIT_REPO" config user.name "Test" 2>/dev/null

printf 'a-line1\na-line2\na-line3\n' > "$GIT_REPO/a.txt"
printf 'b-line1\nb-line2\nb-line3\n' > "$GIT_REPO/b.txt"
git -C "$GIT_REPO" add -A >/dev/null 2>&1
git -C "$GIT_REPO" commit -m "v1" --quiet 2>/dev/null
SHA1=$(git -C "$GIT_REPO" rev-parse HEAD 2>/dev/null)

printf 'a-line1\na-MODIFIED\na-line3\n' > "$GIT_REPO/a.txt"
printf 'b-line1\nb-line2\nb-line3\nb-NEW\n' > "$GIT_REPO/b.txt"
git -C "$GIT_REPO" add -A >/dev/null 2>&1
git -C "$GIT_REPO" commit -m "v2" --quiet 2>/dev/null
SHA2=$(git -C "$GIT_REPO" rev-parse HEAD 2>/dev/null)

GIT_SHAS=("$SHA1" "$SHA2")

(cd "$GIT_REPO" && atomic git import 2>/dev/null) || true

ATOMIC_HASHES=()
log_output=$( (cd "$GIT_REPO" && atomic log --format short --no-color --full-hash 2>/dev/null) || true )
if [[ -n "${log_output:-}" ]]; then
    _tmp=()
    while IFS= read -r line; do
        _h=$(echo "$line" | awk '{print $1}')
        if [[ -n "$_h" ]]; then
            _tmp+=("$_h")
        fi
    done <<< "$log_output"
    for (( _i=${#_tmp[@]}-1 ; _i>=0 ; _i-- )); do
        ATOMIC_HASHES+=("${_tmp[$_i]}")
    done
fi

assert_commit_parity "multi-file" 1

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Realistic code change (config struct)"
# ═══════════════════════════════════════════════════════════════════════════

V1=$(printf '/// App configuration.\nstruct Config {\n    host: String,\n    port: u16,\n}\n\nimpl Config {\n    fn new() -> Self {\n        // TODO: read from env\n        Config {\n            host: "localhost".into(),\n            port: 3000,\n        }\n    }\n}\n')

V2=$(printf '/// App configuration.\nstruct Config {\n    host: String,\n    port: u16,\n    db_url: String,\n    max_connections: u32,\n}\n\nimpl Config {\n    fn new() -> Self {\n        Config {\n            host: "localhost".into(),\n            port: 8080,\n            db_url: "postgres://localhost/app".into(),\n            max_connections: 10,\n        }\n    }\n}\n')

setup_git_and_import "parity-realistic" "src/config.rs" "$V1" "$V2"
assert_commit_parity "realistic code" 1

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Sequential edits (3 commits)"
# ═══════════════════════════════════════════════════════════════════════════

V1=$(printf 'fn main() {\n    println!("v1");\n}\n')
V2=$(printf 'fn main() {\n    println!("v2");\n    // added in v2\n}\n')
V3=$(printf 'fn main() {\n    println!("v3");\n}\n')

setup_git_and_import "parity-sequential" "main.rs" "$V1" "$V2" "$V3"
assert_commit_parity "sequential v1->v2" 1
assert_commit_parity "sequential v2->v3" 2

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Hyperfine first 4 commits (real repo)"
# ═══════════════════════════════════════════════════════════════════════════
#
# Clone the real hyperfine repo, import it, and compare diffs for each
# of the first 4 commits.

make_temp_repo "parity-hyperfine"

if ! git clone --quiet https://github.com/sharkdp/hyperfine.git "$REPO_DIR/hyperfine" 2>/dev/null; then
    _skip "hyperfine clone failed (no network?)"
else
    GIT_REPO="$REPO_DIR/hyperfine"

    # Import into atomic
    (cd "$GIT_REPO" && atomic git import 2>/dev/null) || true

    # The first 4 commits that touch src/main.rs
    COMMIT_SHAS=("a658ab8c" "d4ebdd7b" "197f9fb" "68fdc2c")

    # Resolve full SHAs
    GIT_SHAS=()
    for short in "${COMMIT_SHAS[@]}"; do
        full=$(git -C "$GIT_REPO" rev-parse "$short" 2>/dev/null) || true
        GIT_SHAS+=("$full")
    done

    # Build a map from git short SHA → atomic hash by scanning every
    # atomic change's metadata for the "Commit:" line.
    # Use parallel arrays for bash 3.2 compatibility (no associative arrays).
    MAP_GIT_SHAS=()
    MAP_ATOMIC_HASHES=()

    all_hashes_output=$( (cd "$GIT_REPO" && atomic log --format short --no-color --full-hash 2>/dev/null) || true )
    while IFS= read -r logline; do
        atomic_h=$(echo "$logline" | awk '{print $1}')
        [[ -z "$atomic_h" ]] && continue
        change_detail=$( (cd "$GIT_REPO" && atomic change "$atomic_h" 2>/dev/null) || true )
        git_sha=$(echo "$change_detail" | grep "Commit:" | awk '{print $2}')
        if [[ -n "$git_sha" ]]; then
            # Store the full git SHA so we can match short SHAs of any length
            MAP_GIT_SHAS+=("$git_sha")
            MAP_ATOMIC_HASHES+=("$atomic_h")
        fi
        # Stop once we have all 4 commits mapped
        _found=0
        for _s in "${COMMIT_SHAS[@]}"; do
            for _ms in "${MAP_GIT_SHAS[@]}"; do
                # Match if the full SHA starts with the short SHA
                [[ "$_ms" == "${_s}"* ]] && _found=$((_found+1)) && break
            done
        done
        [[ $_found -ge ${#COMMIT_SHAS[@]} ]] && break
    done <<< "$all_hashes_output"

    # Helper: look up atomic hash for a given git short SHA
    _lookup_atomic_hash() {
        local want="$1"
        local _i
        for (( _i=0; _i<${#MAP_GIT_SHAS[@]}; _i++ )); do
            # Match if the stored full SHA starts with the requested short SHA
            if [[ "${MAP_GIT_SHAS[$_i]}" == "${want}"* ]]; then
                echo "${MAP_ATOMIC_HASHES[$_i]}"
                return
            fi
        done
    }

    # For each transition, look up the atomic hash by git SHA and compare.
    # Scope to src/main.rs only: Cargo.lock diffs differ between our
    # Myers diff implementation and git's algorithm, which is expected.
    for i in 1 2 3; do
        SHORT="${COMMIT_SHAS[$i]}"
        PREV_SHORT="${COMMIT_SHAS[$((i-1))]}"
        ATOMIC_H=$(_lookup_atomic_hash "$SHORT")

        prev_full=$(git -C "$GIT_REPO" rev-parse "$PREV_SHORT" 2>/dev/null) || true
        curr_full=$(git -C "$GIT_REPO" rev-parse "$SHORT" 2>/dev/null) || true

        if [[ -z "$ATOMIC_H" ]]; then
            _fail "hyperfine ($SHORT)" "could not find atomic hash for git SHA $SHORT"
            continue
        fi

        # Git diff scoped to src/main.rs
        git_lf="$REPO_DIR/hf_git_$i.txt"
        git -C "$GIT_REPO" --no-pager diff "$prev_full" "$curr_full" -- src/main.rs 2>/dev/null \
            | extract_git_change_lines > "$git_lf"

        # Atomic diff scoped to src/main.rs
        atomic_lf="$REPO_DIR/hf_atomic_$i.txt"
        hf_raw=$( (cd "$GIT_REPO" && atomic diff -c "$ATOMIC_H" --no-color 2>/dev/null) || true )
        if [[ -n "$hf_raw" ]]; then
            printf '%s\n' "$hf_raw" | awk -v pat="src/main.rs" '
                /^diff --atomic/ { in_sec = ($0 ~ pat) ? 1 : 0; next }
                in_sec { print }
            ' | extract_atomic_change_lines > "$atomic_lf" || true
        else
            touch "$atomic_lf"
        fi

        gc=$(wc -l < "$git_lf" | tr -d ' ') || true
        ac=$(wc -l < "$atomic_lf" | tr -d ' ') || true

        # deletions
        gd="$REPO_DIR/hf_gdel_$i.txt"; ad="$REPO_DIR/hf_adel_$i.txt"
        grep '^-' "$git_lf" > "$gd" 2>/dev/null || touch "$gd"
        grep '^-' "$atomic_lf" > "$ad" 2>/dev/null || touch "$ad"
        if diff -u "$gd" "$ad" > /dev/null 2>&1; then
            _pass "hyperfine ($SHORT): deletions match ($(wc -l < "$gd" | tr -d ' '))"
        else
            _fail "hyperfine ($SHORT): deletions" "git=$(wc -l < "$gd" | tr -d ' ') atomic=$(wc -l < "$ad" | tr -d ' ')"
        fi

        # insertions
        gi="$REPO_DIR/hf_gins_$i.txt"; ai="$REPO_DIR/hf_ains_$i.txt"
        grep '^\+' "$git_lf" > "$gi" 2>/dev/null || touch "$gi"
        grep '^\+' "$atomic_lf" > "$ai" 2>/dev/null || touch "$ai"
        if diff -u "$gi" "$ai" > /dev/null 2>&1; then
            _pass "hyperfine ($SHORT): insertions match ($(wc -l < "$gi" | tr -d ' '))"
        else
            _fail "hyperfine ($SHORT): insertions" "git=$(wc -l < "$gi" | tr -d ' ') atomic=$(wc -l < "$ai" | tr -d ' ')"
        fi

        # total
        if [[ "$gc" -eq "$ac" ]]; then
            _pass "hyperfine ($SHORT): total lines match ($gc)"
        else
            _fail "hyperfine ($SHORT): total lines" "git=$gc atomic=$ac"
        fi
    done
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Pure rename (no content change)"
# ═══════════════════════════════════════════════════════════════════════════
#
# A pure rename produces ZERO +/- lines in both git diff and atomic diff.
# git diff --follow sees no content change; atomic treats the rename as a
# TrunkOp::Move which has no associated line operations.
# Both sides must produce an empty set of change lines.

make_temp_repo "parity-rename-pure"
GIT_REPO="$REPO_DIR"

git -C "$GIT_REPO" init --quiet 2>/dev/null
git -C "$GIT_REPO" config user.email "test@test.com" 2>/dev/null
git -C "$GIT_REPO" config user.name "Test" 2>/dev/null

# v1: create a file
printf 'line1\nline2\nline3\n' > "$GIT_REPO/old_name.txt"
git -C "$GIT_REPO" add -A >/dev/null 2>&1
git -C "$GIT_REPO" commit -m "v1" --quiet 2>/dev/null
SHA1=$(git -C "$GIT_REPO" rev-parse HEAD 2>/dev/null)

# v2: rename the file only (no content change)
git -C "$GIT_REPO" mv old_name.txt new_name.txt 2>/dev/null
git -C "$GIT_REPO" commit -m "v2: rename" --quiet 2>/dev/null
SHA2=$(git -C "$GIT_REPO" rev-parse HEAD 2>/dev/null)

GIT_SHAS=("$SHA1" "$SHA2")

(cd "$GIT_REPO" && atomic git import 2>/dev/null) || true

ATOMIC_HASHES=()
_log=$( (cd "$GIT_REPO" && atomic log --format short --no-color --full-hash 2>/dev/null) || true )
if [[ -n "${_log:-}" ]]; then
    _tmp=()
    while IFS= read -r line; do
        _h=$(echo "$line" | awk '{print $1}')
        [[ -n "$_h" ]] && _tmp+=("$_h")
    done <<< "$_log"
    for (( _i=${#_tmp[@]}-1 ; _i>=0 ; _i-- )); do
        ATOMIC_HASHES+=("${_tmp[$_i]}")
    done
fi

# For a pure rename git diff produces zero +/- lines (no content changed).
# atomic diff -c likewise produces zero +/- lines for a TrunkOp::Move with
# no associated BranchOp edits.  Compare both sides; both must be empty.
_rename_atomic_hash=""
if [[ ${#ATOMIC_HASHES[@]} -ge 2 ]]; then
    _rename_atomic_hash="${ATOMIC_HASHES[1]}"
fi

_git_lines_rename=$(git -C "$GIT_REPO" --no-pager diff "$SHA1" "$SHA2" \
    2>/dev/null | grep -E '^\+[^+]|^-[^-]' || true)

_atomic_lines_rename=""
if [[ -n "$_rename_atomic_hash" ]]; then
    _atomic_raw=$( (cd "$GIT_REPO" && atomic diff -c "$_rename_atomic_hash" --no-color 2>/dev/null) || true )
    _atomic_lines_rename=$(printf '%s\n' "$_atomic_raw" | grep -E '^\+[^+]|^-[^-]' || true)
fi

if [[ -z "$_git_lines_rename" && -z "$_atomic_lines_rename" ]]; then
    _pass "pure rename: both sides have zero change lines (correct)"
elif [[ "$_git_lines_rename" == "$_atomic_lines_rename" ]]; then
    _pass "pure rename: change lines match"
else
    _git_n=$(printf '%s\n' "$_git_lines_rename" | grep -c '.' 2>/dev/null || echo 0)
    _atomic_n=$(printf '%s\n' "$_atomic_lines_rename" | grep -c '.' 2>/dev/null || echo 0)
    _fail "pure rename: change lines" "git=$_git_n lines, atomic=$_atomic_n lines"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Rename + content change"
# ═══════════════════════════════════════════════════════════════════════════
#
# When a file is renamed AND its content changes in the same commit, atomic
# imports this as a TrunkOp::Move (the rename) plus BranchOp edits (the
# content change) — shown under the new filename as the delta only.
#
# Comparison strategy:
#   git side  : `git diff -M PARENT HEAD` with rename detection enabled,
#               then extract only the lines belonging to renamed.txt.
#               -M causes git to detect the rename and show only the content
#               delta (same as what atomic produces), rather than treating
#               the new file as freshly created.
#   atomic side: extract +/- lines from the renamed.txt section of
#               `atomic diff -c <hash>`

make_temp_repo "parity-rename-modify"
GIT_REPO="$REPO_DIR"

git -C "$GIT_REPO" init --quiet 2>/dev/null
git -C "$GIT_REPO" config user.email "test@test.com" 2>/dev/null
git -C "$GIT_REPO" config user.name "Test" 2>/dev/null

# v1: create original file
printf 'alpha\nbeta\ngamma\ndelta\n' > "$GIT_REPO/original.txt"
git -C "$GIT_REPO" add -A >/dev/null 2>&1
git -C "$GIT_REPO" commit -m "v1" --quiet 2>/dev/null
SHA1=$(git -C "$GIT_REPO" rev-parse HEAD 2>/dev/null)

# v2: rename AND modify content
git -C "$GIT_REPO" mv original.txt renamed.txt 2>/dev/null
printf 'alpha\nbeta\nGAMMA-MODIFIED\ndelta\nepsilon\n' > "$GIT_REPO/renamed.txt"
git -C "$GIT_REPO" add -A >/dev/null 2>&1
git -C "$GIT_REPO" commit -m "v2: rename+modify" --quiet 2>/dev/null
SHA2=$(git -C "$GIT_REPO" rev-parse HEAD 2>/dev/null)

GIT_SHAS=("$SHA1" "$SHA2")

(cd "$GIT_REPO" && atomic git import 2>/dev/null) || true

ATOMIC_HASHES=()
_log=$( (cd "$GIT_REPO" && atomic log --format short --no-color --full-hash 2>/dev/null) || true )
if [[ -n "${_log:-}" ]]; then
    _tmp=()
    while IFS= read -r line; do
        _h=$(echo "$line" | awk '{print $1}')
        [[ -n "$_h" ]] && _tmp+=("$_h")
    done <<< "$_log"
    for (( _i=${#_tmp[@]}-1 ; _i>=0 ; _i-- )); do
        ATOMIC_HASHES+=("${_tmp[$_i]}")
    done
fi

_rename_mod_hash=""
if [[ ${#ATOMIC_HASHES[@]} -ge 2 ]]; then
    _rename_mod_hash="${ATOMIC_HASHES[1]}"
fi

# Git: use -M10% (rename detection with a low similarity threshold) so git
# recognises the rename even for files with significant content changes.
# When git detects the rename the header becomes:
#   diff --git a/original.txt b/renamed.txt
#   rename from original.txt
#   rename to renamed.txt
# and the diff shows only the content delta.
# We extract only the +/- lines from that section.
_git_full_rmod=$(git -C "$GIT_REPO" --no-pager diff -M10% "$SHA1" "$SHA2" \
    2>/dev/null || true)
_git_lines_rmod=$(printf '%s\n' "$_git_full_rmod" | awk -v pat="renamed.txt" '
    /^diff --git/ {
        # A rename section header looks like: diff --git a/old b/new
        # Match on " b/<pat>" at end of line (1-based substr index).
        n = length($0)
        plen = length(pat)
        in_sec = (index($0, " b/" pat) > 0 && \
                  (index($0, " b/" pat " ") > 0 || \
                   substr($0, n - plen + 1) == pat)) ? 1 : 0
        next
    }
    in_sec { print }
' | grep -E '^\+[^+]|^-[^-]' || true)

# Atomic: extract +/- lines from the "renamed.txt" section.
_atomic_lines_rmod=""
if [[ -n "$_rename_mod_hash" ]]; then
    _raw=$( (cd "$GIT_REPO" && atomic diff -c "$_rename_mod_hash" --no-color 2>/dev/null) || true )
    _atomic_lines_rmod=$(printf '%s\n' "$_raw" | awk -v pat="renamed.txt" '
        /^diff --atomic/ {
            in_sec = (index($0, " b/" pat) > 0 && \
                      (index($0, " b/" pat " ") > 0 || \
                       substr($0, length($0) - length(pat)) == pat)) ? 1 : 0
            next
        }
        in_sec { print }
    ' | grep -E '^\+[^+]|^-[^-]' || true)
fi

# Sort both sides before comparing (hunk-ordering may differ).
_sorted_git=$(printf '%s\n' "$_git_lines_rmod" | sort)
_sorted_atomic=$(printf '%s\n' "$_atomic_lines_rmod" | sort)

if [[ "$_sorted_git" == "$_sorted_atomic" ]]; then
    _n=$(printf '%s\n' "$_git_lines_rmod" | grep -c '.' 2>/dev/null || echo 0)
    _pass "rename+modify: change lines match ($_n lines)"
else
    _gn=$(printf '%s\n' "$_git_lines_rmod" | grep -c '.' 2>/dev/null || echo 0)
    _an=$(printf '%s\n' "$_atomic_lines_rmod" | grep -c '.' 2>/dev/null || echo 0)
    _first=$(diff \
        <(printf '%s\n' "$_git_lines_rmod" | sort) \
        <(printf '%s\n' "$_atomic_lines_rmod" | sort) \
        2>/dev/null | head -4) || true
    _fail "rename+modify: change lines" \
        "git=$_gn lines, atomic=$_an lines. Diff: $_first"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Parity: Rename across directories"
# ═══════════════════════════════════════════════════════════════════════════
#
# Validates that moving a file from one subdirectory to another is handled
# correctly — the atomic diff should show zero +/- lines (no content change)
# and match git exactly.

make_temp_repo "parity-rename-dirs"
GIT_REPO="$REPO_DIR"

git -C "$GIT_REPO" init --quiet 2>/dev/null
git -C "$GIT_REPO" config user.email "test@test.com" 2>/dev/null
git -C "$GIT_REPO" config user.name "Test" 2>/dev/null

mkdir -p "$GIT_REPO/src" "$GIT_REPO/lib"
printf 'fn helper() {}\n' > "$GIT_REPO/src/helper.rs"
git -C "$GIT_REPO" add -A >/dev/null 2>&1
git -C "$GIT_REPO" commit -m "v1" --quiet 2>/dev/null
SHA1=$(git -C "$GIT_REPO" rev-parse HEAD 2>/dev/null)

# Move src/helper.rs → lib/helper.rs (no content change)
git -C "$GIT_REPO" mv src/helper.rs lib/helper.rs 2>/dev/null
git -C "$GIT_REPO" commit -m "v2: move to lib/" --quiet 2>/dev/null
SHA2=$(git -C "$GIT_REPO" rev-parse HEAD 2>/dev/null)

GIT_SHAS=("$SHA1" "$SHA2")

(cd "$GIT_REPO" && atomic git import 2>/dev/null) || true

ATOMIC_HASHES=()
_log=$( (cd "$GIT_REPO" && atomic log --format short --no-color --full-hash 2>/dev/null) || true )
if [[ -n "${_log:-}" ]]; then
    _tmp=()
    while IFS= read -r line; do
        _h=$(echo "$line" | awk '{print $1}')
        [[ -n "$_h" ]] && _tmp+=("$_h")
    done <<< "$_log"
    for (( _i=${#_tmp[@]}-1 ; _i>=0 ; _i-- )); do
        ATOMIC_HASHES+=("${_tmp[$_i]}")
    done
fi

_dir_rename_hash=""
if [[ ${#ATOMIC_HASHES[@]} -ge 2 ]]; then
    _dir_rename_hash="${ATOMIC_HASHES[1]}"
fi

_git_lines_dir=$(git -C "$GIT_REPO" --no-pager diff "$SHA1" "$SHA2" \
    2>/dev/null | grep -E '^\+[^+]|^-[^-]' || true)

_atomic_lines_dir=""
if [[ -n "$_dir_rename_hash" ]]; then
    _raw=$( (cd "$GIT_REPO" && atomic diff -c "$_dir_rename_hash" --no-color 2>/dev/null) || true )
    _atomic_lines_dir=$(printf '%s\n' "$_raw" | grep -E '^\+[^+]|^-[^-]' || true)
fi

if [[ -z "$_git_lines_dir" && -z "$_atomic_lines_dir" ]]; then
    _pass "cross-dir rename: both sides have zero change lines (correct)"
elif [[ "$_git_lines_dir" == "$_atomic_lines_dir" ]]; then
    _pass "cross-dir rename: change lines match"
else
    _gn=$(printf '%s\n' "$_git_lines_dir" | grep -c '.' 2>/dev/null || echo 0)
    _an=$(printf '%s\n' "$_atomic_lines_dir" | grep -c '.' 2>/dev/null || echo 0)
    _fail "cross-dir rename: change lines" "git=$_gn lines, atomic=$_an lines"
fi

# ═══════════════════════════════════════════════════════════════════════════
# Summary
# ═══════════════════════════════════════════════════════════════════════════

print_summary
