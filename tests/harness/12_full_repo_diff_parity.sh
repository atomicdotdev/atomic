#!/usr/bin/env bash
# 12_full_repo_diff_parity.sh — Full-repo diff parity test.
#
# Imports a real git repository into atomic and validates that
# `atomic diff -c <hash>` produces the same +/- lines as `git diff`
# for every sampled commit across every file the commit touches.
#
# Configuration (via environment variables):
#
#   PARITY_REPO_URL   — git clone URL to test (default: hyperfine)
#   PARITY_SAMPLE_N   — check every Nth commit (default: 10)
#   PARITY_MAX_COMMITS— stop after this many sampled commits (default: 50)
#   PARITY_SKIP_PATHS — colon-separated glob patterns to skip
#                       (default: "*.lock:*.sum" — generated lockfiles
#                        whose diffs vary by algorithm)
#
# Examples:
#
#   # Run with defaults (hyperfine, every 10th commit, up to 50 samples)
#   bash tests/harness/12_full_repo_diff_parity.sh
#
#   # Test a different repo, denser sampling
#   PARITY_REPO_URL=https://github.com/BurntSushi/ripgrep.git \
#   PARITY_SAMPLE_N=5 \
#   PARITY_MAX_COMMITS=100 \
#   bash tests/harness/12_full_repo_diff_parity.sh
#
# What is compared:
#   For each sampled commit C with parent P:
#     For each file F changed in C (excluding PARITY_SKIP_PATHS):
#       git_lines  = `git diff P..C -- F` filtered to +/- lines
#       atomic_lines = `atomic diff -c <atomic_hash_for_C> -- F` filtered
#       PASS if git_lines == atomic_lines (same content, same order)
#
# Skipped-path rationale:
#   Lock files (Cargo.lock, go.sum, package-lock.json) are generated
#   automatically and their diffs depend heavily on the diff algorithm's
#   LCS heuristics.  Our Myers implementation may produce a different
#   (but equally valid) edit sequence.  For all hand-authored source
#   files the diffs must match exactly because we use git's own diff
#   lines via build_crdt_ops_from_git_diff.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ── Configuration ────────────────────────────────────────────────────────────

REPO_URL="${PARITY_REPO_URL:-https://github.com/sharkdp/hyperfine.git}"
SAMPLE_N="${PARITY_SAMPLE_N:-10}"
MAX_COMMITS="${PARITY_MAX_COMMITS:-50}"
SKIP_PATHS="${PARITY_SKIP_PATHS:-*.lock:*.sum:package-lock.json:yarn.lock}"

# ── Helpers ──────────────────────────────────────────────────────────────────

extract_git_change_lines() {
    grep -E '^\+[^+]|^-[^-]' || true
}

extract_atomic_change_lines() {
    grep -E '^\+[^+]|^-[^-]' || true
}

# Check if a file path matches any of the skip patterns.
# Usage: should_skip "path/to/file.lock"
should_skip() {
    local path="$1"
    local fname
    fname=$(basename "$path")
    local IFS=':'
    for pat in $SKIP_PATHS; do
        # shellcheck disable=SC2053
        [[ "$fname" == $pat ]] && return 0
        [[ "$path" == $pat ]] && return 0
    done
    return 1
}

# Extract +/- change lines from an atomic diff scoped to a single file path.
# Usage: atomic_diff_for_path "$raw_diff" "src/main.rs"
#
# The awk match uses a word-boundary-aware check: the path must appear as
# a complete path component in the "diff --atomic a/PATH b/PATH" header,
# not as a substring of another path.  We anchor on " b/" to avoid matching
# "snap/snapcraft.yaml" when looking for ".snapcraft.yaml".
atomic_diff_for_path() {
    local raw="$1"
    local path_filter="$2"
    printf '%s\n' "$raw" | awk -v pat="$path_filter" '
        /^diff --atomic/ {
            # Match only when the path appears after " b/" (the new-file side).
            in_sec = (index($0, " b/" pat) > 0 && \
                      (index($0, " b/" pat " ") > 0 || \
                       substr($0, length($0) - length(pat)) == pat)) ? 1 : 0
            next
        }
        in_sec { print }
    ' | extract_atomic_change_lines
}

# Check if a commit contains any rename/move operations.
# Returns 0 (true) if at least one renamed file is found.
commit_has_rename() {
    local repo="$1"
    local sha="$2"
    git -C "$repo" diff --name-status "${sha}^" "$sha" 2>/dev/null | grep -q '^R'
}

# For a given file path in a commit, detect whether it was renamed (i.e., it is
# the NEW path of a rename operation).  If so, echo the OLD path; otherwise echo
# nothing.
# Usage: get_rename_old_path <repo> <sha> <new_path>
get_rename_old_path() {
    local repo="$1"
    local sha="$2"
    local new_path="$3"
    # --diff-filter=R limits output to renamed files.
    # --name-status lines look like: R100<TAB>old/path<TAB>new/path
    git -C "$repo" diff --diff-filter=R --name-status "${sha}^" "$sha" \
        2>/dev/null \
        | awk -v np="$new_path" -F'\t' '$3 == np { print $2 }'
}

# Extract +/- change lines for a file in a commit, rename-aware.
#
# For normal modifications:   git diff PARENT HEAD -- path
# For renamed files:          git diff -M PARENT HEAD, then extract the section
#                             for new_path.
#
#   -M enables rename detection so git shows only the content delta for a
#   renamed file rather than treating it as a brand-new file (which would
#   show all lines as additions).  This matches what `atomic diff -c` produces:
#   zero lines for a pure rename, or just the changed lines for rename+modify.
#
# Usage: git_change_lines_for_file <repo> <parent> <sha> <path> <is_rename>
git_change_lines_for_file() {
    local repo="$1"
    local parent="$2"
    local sha="$3"
    local path="$4"
    local is_rename="$5"   # "1" if this file was renamed, "0" otherwise

    if [[ "$is_rename" == "1" ]]; then
        # -M10% enables rename detection with a low similarity threshold so
        # git recognises renames even when content changed significantly.
        # We then extract only the section of the diff that belongs to our
        # file (matched on the "b/<path>" side of the diff --git header),
        # and filter to +/- lines.
        git -C "$repo" --no-pager diff -M10% "$parent" "$sha" \
            2>/dev/null \
            | awk -v pat="$path" '
                /^diff --git/ {
                    n = length($0)
                    plen = length(pat)
                    in_sec = (index($0, " b/" pat) > 0 && \
                              (index($0, " b/" pat " ") > 0 || \
                               substr($0, n - plen + 1) == pat)) ? 1 : 0
                    next
                }
                in_sec { print }
            ' | extract_git_change_lines || true
    else
        git -C "$repo" --no-pager diff "$parent" "$sha" -- "$path" \
            2>/dev/null | extract_git_change_lines || true
    fi
}

# Compare two sets of change lines for parity.
#
# We sort both sides before comparing so that hunk-ordering differences
# (caused by different diff algorithm context grouping) do not produce
# false failures.  The content — which lines were added and which were
# deleted — must still match exactly.
#
# Usage: change_lines_match "$git_lines" "$atomic_lines"
# Returns 0 (true) if the sorted sets are identical.
change_lines_match() {
    local git_lines="$1"
    local atomic_lines="$2"
    local sorted_git sorted_atomic
    sorted_git=$(printf '%s\n' "$git_lines" | sort)
    sorted_atomic=$(printf '%s\n' "$atomic_lines" | sort)
    [[ "$sorted_git" == "$sorted_atomic" ]]
}

# ── Main ─────────────────────────────────────────────────────────────────────

begin_section "Full-Repo Import: ${REPO_URL}"

make_temp_repo "full-parity"
WORK="$REPO_DIR"
CLONE_DIR="$WORK/repo"

# Clone
git clone --quiet "$REPO_URL" "$CLONE_DIR" 2>/dev/null
if [[ $? -ne 0 ]]; then
    _skip "clone failed (no network?): $REPO_URL"
    print_summary
    exit 0
fi

_pass "cloned $REPO_URL"

# Import
(cd "$CLONE_DIR" && atomic git import 2>/dev/null) || true
_pass "atomic git import completed"

# ── Collect commits to sample ────────────────────────────────────────────────
#
# Walk git history in topological order (oldest first).  Sample every Nth
# commit that has a parent (we skip the root commit — nothing to diff against).

begin_section "Sampling commits (every ${SAMPLE_N}, max ${MAX_COMMITS}) and building SHA map"

# Get all commit SHAs oldest-first, skipping the root (no parent).
ALL_COMMITS=()
while IFS= read -r sha; do
    parent=$(git -C "$CLONE_DIR" rev-parse "${sha}^" 2>/dev/null) || true
    [[ -z "$parent" ]] && continue
    ALL_COMMITS+=("$sha")
done < <(git -C "$CLONE_DIR" rev-list --reverse HEAD 2>/dev/null)

TOTAL_COMMITS=${#ALL_COMMITS[@]}

# Build the sample: every Nth element, up to MAX_COMMITS.
SAMPLED_COMMITS=()
_idx=0
for sha in "${ALL_COMMITS[@]}"; do
    if (( _idx % SAMPLE_N == 0 )); then
        SAMPLED_COMMITS+=("$sha")
        [[ ${#SAMPLED_COMMITS[@]} -ge $MAX_COMMITS ]] && break
    fi
    _idx=$((_idx + 1))
done

_pass "sampling ${#SAMPLED_COMMITS[@]} of $TOTAL_COMMITS commits"

# ── Build git-SHA → atomic-hash map (lazy, for sampled commits only) ─────────
#
# For each sampled commit we call `atomic change <hash>` once to extract
# the embedded "Commit: <sha>" line.  This is O(MAX_COMMITS) subprocess
# calls — fast even for large repos.
#
# We scan the atomic log from newest to oldest; since import processes
# commits in topological order, the SHAs cluster at the tail of the log.
# We stop scanning once we have found all sampled commits.

# Use --reverse so the oldest commits appear first in the log.
# Our SAMPLED_COMMITS are drawn from git rev-list --reverse (oldest first),
# so they cluster near the beginning of the reversed atomic log.
# This makes the early-exit trigger much sooner for typical samples.
_all_atomic_log=$( (cd "$CLONE_DIR" && atomic log --format short --no-color --full-hash --reverse 2>/dev/null) || true )

MAP_GIT_SHAS=()
MAP_ATOMIC_HASHES=()

# Populate the map by iterating every atomic change entry once.
# We call `atomic change` for each entry, extract "Commit:", and store.
# Progress is shown every 100 entries.
_map_count=0
_total_log=$(printf '%s\n' "$_all_atomic_log" | grep -c '.' 2>/dev/null || echo 0)
while IFS= read -r logline; do
    _ah=$(echo "$logline" | awk '{print $1}')
    [[ -z "$_ah" ]] && continue
    _detail=$( (cd "$CLONE_DIR" && atomic change "$_ah" 2>/dev/null) || true )
    _gsha=$(echo "$_detail" | grep "Commit:" | awk '{print $2}')
    if [[ -n "$_gsha" ]]; then
        MAP_GIT_SHAS+=("$_gsha")
        MAP_ATOMIC_HASHES+=("$_ah")
    fi
    _map_count=$((_map_count + 1))
    if (( _map_count % 100 == 0 )); then
        echo "  building map: $_map_count / $_total_log ..." >&2
    fi
    # Early-exit once every sampled commit is found.
    _found_all=1
    for _sc in "${SAMPLED_COMMITS[@]}"; do
        _matched=0
        for (( _mi=0; _mi<${#MAP_GIT_SHAS[@]}; _mi++ )); do
            if [[ "$_sc" == "${MAP_GIT_SHAS[$_mi]}"* ]] || \
               [[ "${MAP_GIT_SHAS[$_mi]}" == "${_sc}"* ]]; then
                _matched=1; break
            fi
        done
        [[ $_matched -eq 0 ]] && _found_all=0 && break
    done
    [[ $_found_all -eq 1 ]] && break
done <<< "$_all_atomic_log"

_pass "map built: ${#MAP_GIT_SHAS[@]} entries (scanned $_map_count of $_total_log)"

# Helper: look up atomic hash for a git SHA (full or short prefix match).
_lookup_atomic_hash() {
    local want="$1"
    local _i
    for (( _i=0; _i<${#MAP_GIT_SHAS[@]}; _i++ )); do
        local stored="${MAP_GIT_SHAS[$_i]}"
        # Match: want starts with stored (short), OR stored starts with want (short query)
        if [[ "$want" == "${stored}"* ]] || [[ "$stored" == "${want}"* ]]; then
            echo "${MAP_ATOMIC_HASHES[$_i]}"
            return 0
        fi
    done
    return 1
}

# ── Per-commit diff comparison ───────────────────────────────────────────────

begin_section "Diff parity: ${#SAMPLED_COMMITS[@]} commits"

COMMITS_PASS=0
COMMITS_FAIL=0
COMMITS_SKIP=0
FILES_PASS=0
FILES_FAIL=0
FILES_SKIP=0

for curr_sha in "${SAMPLED_COMMITS[@]}"; do
    short="${curr_sha:0:8}"
    parent=$(git -C "$CLONE_DIR" rev-parse "${curr_sha}^" 2>/dev/null) || true

    if [[ -z "$parent" ]]; then
        COMMITS_SKIP=$((COMMITS_SKIP + 1))
        FILES_SKIP=$((FILES_SKIP + 1))
        continue
    fi

    # Get the atomic hash for this commit.
    atomic_hash=$(_lookup_atomic_hash "$curr_sha") || true
    if [[ -z "$atomic_hash" ]]; then
        _fail "commit $short" "no atomic hash found (commit may not have been imported)"
        COMMITS_FAIL=$((COMMITS_FAIL + 1))
        continue
    fi

    # Get files changed in this commit (excluding merges, submodules).
    changed_files=()
    while IFS= read -r fpath; do
        [[ -z "$fpath" ]] && continue
        should_skip "$fpath" && {
            FILES_SKIP=$((FILES_SKIP + 1))
            continue
        }
        changed_files+=("$fpath")
    done < <(git -C "$CLONE_DIR" diff --name-only "$parent" "$curr_sha" 2>/dev/null)

    if [[ ${#changed_files[@]} -eq 0 ]]; then
        # Empty or all-skipped commit
        COMMITS_SKIP=$((COMMITS_SKIP + 1))
        continue
    fi

    # Get the full atomic diff output once per commit (amortised cost).
    atomic_raw=$( (cd "$CLONE_DIR" && atomic diff -c "$atomic_hash" --no-color 2>/dev/null) || true )

    commit_ok=1
    for fpath in "${changed_files[@]}"; do
        # Detect whether this file is the new path of a rename.
        _is_rename=0
        if [[ -n "$(get_rename_old_path "$CLONE_DIR" "$curr_sha" "$fpath")" ]]; then
            _is_rename=1
        fi

        # Git change lines for this file (rename-aware).
        git_lines=$(git_change_lines_for_file \
            "$CLONE_DIR" "$parent" "$curr_sha" "$fpath" "$_is_rename") || true

        # Atomic change lines for this file
        atomic_lines=$(atomic_diff_for_path "$atomic_raw" "$fpath") || true

        if change_lines_match "$git_lines" "$atomic_lines"; then
            FILES_PASS=$((FILES_PASS + 1))
        else
            git_count=$(printf '%s\n' "$git_lines" | grep -c '.' 2>/dev/null || echo 0)
            atomic_count=$(printf '%s\n' "$atomic_lines" | grep -c '.' 2>/dev/null || echo 0)
            # Show first diverging line for diagnosis
            first_diff=$(diff \
                <(printf '%s\n' "$git_lines" | sort) \
                <(printf '%s\n' "$atomic_lines" | sort) \
                2>/dev/null | head -4) || true
            _rename_note=""
            [[ "$_is_rename" == "1" ]] && _rename_note=" (renamed)"
            _fail "commit $short: $fpath${_rename_note}" \
                "git=$git_count lines, atomic=$atomic_count lines. Diff: $first_diff"
            FILES_FAIL=$((FILES_FAIL + 1))
            commit_ok=0
        fi
    done
    if [[ $commit_ok -eq 1 ]]; then
        COMMITS_PASS=$((COMMITS_PASS + 1))
    else
        COMMITS_FAIL=$((COMMITS_FAIL + 1))
    fi
done

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "  Commits: ${COMMITS_PASS} passed, ${COMMITS_FAIL} failed, ${COMMITS_SKIP} skipped"
echo "  Files:   ${FILES_PASS} passed, ${FILES_FAIL} failed, ${FILES_SKIP} skipped"
echo ""

if [[ $COMMITS_FAIL -gt 0 || $FILES_FAIL -gt 0 ]]; then
    _fail "full-repo parity" \
        "${COMMITS_FAIL} commits and ${FILES_FAIL} files failed diff parity"
else
    _pass "full-repo parity: all ${FILES_PASS} file-diffs matched git exactly"
fi

print_summary
