#!/usr/bin/env bash
# 18_git_import_hot_file.sh — Synthetic hot-file git import regression.
#
# This harness creates a local Git repository with one long-lived file that is
# edited repeatedly across many commits, then times `atomic git import`.
# It is meant to reproduce the Terraform-style "ordinary late edit to a hot
# file" shape without cloning a huge upstream repository.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

HOT_FILE_COMMITS="${HOT_FILE_COMMITS:-60}"
HOT_FILE_LINES="${HOT_FILE_LINES:-300}"
HOT_FILE_IMPORT_MAX_SECONDS="${HOT_FILE_IMPORT_MAX_SECONDS:-30}"
HOT_FILE_MODE="${HOT_FILE_MODE:-linear}"
HOT_FILE_BRANCHES="${HOT_FILE_BRANCHES:-8}"
HOT_FILE_ALLOWED_SKIPS="${HOT_FILE_ALLOWED_SKIPS:-3}"

HOT_FILE_PATH="terraform/context_apply_test.go"

count_imported_git_changes() {
    local count=0
    local hash
    while IFS= read -r hash; do
        [[ -z "$hash" ]] && continue
        if atomic change "$hash" 2>/dev/null | grep -q 'Commit:'; then
            count=$((count + 1))
        fi
    done < <(atomic log --format short --no-color --full-hash 2>/dev/null | awk '/^[A-Z2-7]{20,}/ { print $1 }')
    echo "$count"
}

git_first_parent_commit_count() {
    git rev-list --first-parent --count HEAD 2>/dev/null || echo "0"
}

write_hot_file() {
    local commit_idx="$1"
    local line_idx

    mkdir -p "$(dirname "$HOT_FILE_PATH")"
    : > "$HOT_FILE_PATH"
    for ((line_idx = 1; line_idx <= HOT_FILE_LINES; line_idx++)); do
        if (( line_idx % 37 == commit_idx % 37 )); then
            printf 'func TestHotPath_%04d_%04d(t *testing.T) { t.Log("hot-%04d") }\n' \
                "$line_idx" "$commit_idx" "$commit_idx" >> "$HOT_FILE_PATH"
        else
            printf 'func TestHotPath_%04d(t *testing.T) { t.Log("stable-%04d") }\n' \
                "$line_idx" "$line_idx" >> "$HOT_FILE_PATH"
        fi
    done
}

edit_hot_line_in_place() {
    local line_no="$1"
    local marker="$2"

    awk -v line_no="$line_no" -v marker="$marker" '
        NR == line_no {
            printf "func TestHotPath_%04d_%s(t *testing.T) { t.Log(\"%s\") }\n", line_no, marker, marker
            next
        }
        { print }
    ' "$HOT_FILE_PATH" > "$HOT_FILE_PATH.tmp"
    mv "$HOT_FILE_PATH.tmp" "$HOT_FILE_PATH"
}

create_linear_history() {
    local i
    for ((i = 1; i <= HOT_FILE_COMMITS; i++)); do
        write_hot_file "$i"
        git add "$HOT_FILE_PATH"
        git commit --quiet -m "Hot file edit $i"
    done
}

create_branchy_history() {
    local main_branch branch line_no b round marker

    main_branch="$(git symbolic-ref --short HEAD 2>/dev/null || git rev-parse --abbrev-ref HEAD)"

    # Create independent branch edits from the same base line. Importing with
    # --all replays all branch changes into the Atomic graph, producing a
    # branchy file graph before the final mainline edit.
    for ((b = 1; b <= HOT_FILE_BRANCHES; b++)); do
        git checkout --quiet -b "hot-branch-$b" "$main_branch"
        line_no=$(( (b % HOT_FILE_LINES) + 1 ))
        for ((round = 1; round <= HOT_FILE_COMMITS; round++)); do
            marker="branch_${b}_${round}"
            edit_hot_line_in_place "$line_no" "$marker"
            git add "$HOT_FILE_PATH"
            git commit --quiet -m "Branch $b hot edit $round"
        done
    done

    git checkout --quiet "$main_branch"
    line_no=$(( (HOT_FILE_BRANCHES % HOT_FILE_LINES) + 2 ))
    edit_hot_line_in_place "$line_no" "final_main_${HOT_FILE_COMMITS}"
    git add "$HOT_FILE_PATH"
    git commit --quiet -m "Final main hot edit"
}

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 18_git_import_hot_file${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Git Import: Synthetic Hot File"

make_temp_repo "git-import-hot-file"
init_git_repo

write_hot_file 0
git add "$HOT_FILE_PATH"
git commit --quiet -m "Initial hot file"

case "$HOT_FILE_MODE" in
    linear)
        create_linear_history
        ;;
    branchy)
        create_branchy_history
        ;;
    *)
        _fail "valid HOT_FILE_MODE" "expected 'linear' or 'branchy', got '$HOT_FILE_MODE'"
        print_summary
        exit 1
        ;;
esac

expected_commits="$(git_first_parent_commit_count)"
if [[ "$HOT_FILE_MODE" == "branchy" ]]; then
    expected_commits="$(git rev-list --all --count 2>/dev/null || echo "$expected_commits")"
fi
echo "  Synthetic history: mode=${HOT_FILE_MODE}, commits=${expected_commits}, lines=${HOT_FILE_LINES}, path=${HOT_FILE_PATH}"

atomic init >/dev/null 2>&1

start_time=$(date +%s)
import_args=(git import)
if [[ "$HOT_FILE_MODE" == "branchy" ]]; then
    import_args+=(--all)
fi

import_out="$(atomic "${import_args[@]}" 2>&1)" || {
    _fail "hot-file import succeeds" "$import_out"
    print_summary
    exit 1
}
end_time=$(date +%s)
duration=$((end_time - start_time))

_pass "hot-file import succeeds"
echo "  Import took ${duration}s"

actual="$(count_imported_git_changes)"
if [[ "$HOT_FILE_MODE" == "branchy" ]]; then
    if [[ "$actual" -ge 2 ]]; then
        _pass "branchy import produced git changes ($actual imported)"
    else
        _fail "branchy import produced git changes" "expected at least 2 imported git changes, got $actual"
    fi
elif [[ "$actual" -eq "$expected_commits" ]]; then
    _pass "imported git change count matches ($actual vs $expected_commits)"
elif [[ "$actual" -ge $((expected_commits - HOT_FILE_ALLOWED_SKIPS)) ]]; then
    _pass "imported git change count within skip tolerance ($actual vs $expected_commits)"
else
    _fail "imported git change count matches" \
        "expected $expected_commits imported git changes, got $actual (allowed skips: $HOT_FILE_ALLOWED_SKIPS)"
fi

if [[ "$duration" -le "$HOT_FILE_IMPORT_MAX_SECONDS" ]]; then
    _pass "hot-file import within ${HOT_FILE_IMPORT_MAX_SECONDS}s budget"
else
    _fail "hot-file import within ${HOT_FILE_IMPORT_MAX_SECONDS}s budget" \
        "took ${duration}s; likely hot-file assembly traversal regression"
fi

status_out="$(atomic status --short 2>/dev/null || true)"
hot_file_status="$(echo "$status_out" | grep -F "$HOT_FILE_PATH" || true)"
if [[ -z "$hot_file_status" ]]; then
    _pass "hot file clean after import"
else
    _fail "hot file clean after import" "$hot_file_status"
fi

expected_marker="hot-$(printf '%04d' "$HOT_FILE_COMMITS")"
if [[ "$HOT_FILE_MODE" == "branchy" ]]; then
    expected_marker="final_main_${HOT_FILE_COMMITS}"
fi

if grep -q "$expected_marker" "$HOT_FILE_PATH"; then
    _pass "final hot-file content materialized"
else
    _fail "final hot-file content materialized" "missing final edit marker '$expected_marker'"
fi

print_summary
