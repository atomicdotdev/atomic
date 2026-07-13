#!/usr/bin/env bash
# 13_import_fidelity.sh — Regression tests for git import fidelity
#
# Exercises three historically-buggy areas:
#
#   1. File deletions — files removed in git must disappear from the
#      atomic tree (not linger as ghosts).
#
#   2. Binary file modifications — binary content updates must be
#      recorded and applied so the graph matches the working copy.
#
#   3. Rewrite misclassification — a heavy content rewrite + new file
#      in the same commit must NOT be misclassified as a rename,
#      which would steal the inode and leave the original untracked.
#
# All tests are self-contained (no network required).

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

assert_import_clean_with_bootstrap() {
    local desc="$1"
    local out
    out="$(get_status_short)"

    local unexpected=""
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue

        # Ignore the repository bootstrap artifacts materialized by
        # `atomic init`/`atomic git import` (vault scaffold + .atomicignore).
        # These can show up as untracked ("??") or already-added ("A ")
        # depending on whether the import step auto-adds them, so match on
        # the path regardless of the leading status flag(s).
        local path
        path="$(echo "$line" | sed -E 's/^[^[:space:]]+[[:space:]]+//')"
        case "$path" in
            .atomicignore) continue ;;
            .vault/*) continue ;;
        esac

        unexpected+="${line}"$'\n'
    done <<< "$out"

    if [[ -z "$unexpected" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "unexpected status entries: $(printf '%s' "$unexpected" | head -10)"
    fi
}

# Assert that `atomic diff` (working copy) shows no changes other than the
# repository bootstrap artifacts (vault scaffold + .atomicignore), which are
# added but not recorded by `atomic git import` and so appear as new-file
# diffs on the working copy.
assert_no_diff_except_bootstrap() {
    local desc="$1"
    local diff_out
    diff_out="$(atomic diff 2>/dev/null || true)"

    local unexpected=""
    while IFS= read -r line; do
        if [[ "$line" =~ ^diff\ --atomic\ a/.*\ b/(.*)\ \( ]]; then
            local path="${BASH_REMATCH[1]}"
            case "$path" in
                .atomicignore | .vault/*) continue ;;
                *) unexpected+="${path}"$'\n' ;;
            esac
        fi
    done <<< "$diff_out"

    if [[ -z "$unexpected" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "atomic diff shows unexpected changes for: $(printf '%s' "$unexpected" | head -10)"
    fi
}

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 13_import_fidelity${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

# ════════════════════════════════════════════════════════════════════════════
# Section 1: File Deletions
# ════════════════════════════════════════════════════════════════════════════

begin_section "Import Fidelity: File Deletions"

make_temp_repo "fidelity-deletions"
init_git_repo

# Build a history: add several files, then delete some
git_commit "Add alpha" "alpha.txt" "alpha content"
git_commit "Add beta"  "beta.txt"  "beta content"
git_commit "Add gamma" "sub/gamma.txt" "gamma content"
git_commit "Modify alpha" "alpha.txt" "alpha v2"

# Delete alpha and gamma
git rm --quiet alpha.txt
git commit --quiet -m "Delete alpha"

git rm --quiet sub/gamma.txt
git commit --quiet -m "Delete gamma"

# beta should survive
git_commit "Modify beta" "beta.txt" "beta v2"

atomic init >/dev/null 2>&1
assert_success "import succeeds" atomic git import

# Verify deleted files are gone from atomic
assert_import_clean_with_bootstrap "working tree clean after import"

# Double-check: atomic diff should show nothing
assert_no_diff_except_bootstrap "no diff after import"

# Verify beta still exists and has correct content
assert_file_exists "beta.txt survives deletion of siblings" "beta.txt"
assert_file_content "beta.txt has final content" "beta.txt" "beta v2"

# ────────────────────────────────────────────────────────────────────────────
# Multi-file deletion in a single commit

begin_section "Import Fidelity: Bulk Deletion in Single Commit"

make_temp_repo "fidelity-bulk-delete"
init_git_repo

# Create 5 files
for i in 1 2 3 4 5; do
    git_commit "Add file$i" "dir/file$i.txt" "content $i"
done

# Delete 3 files in one commit (like the CI migration in hyperfine)
git rm --quiet dir/file1.txt dir/file3.txt dir/file5.txt
git commit --quiet -m "Remove odd files"

atomic init >/dev/null 2>&1
assert_success "import bulk deletion succeeds" atomic git import
assert_import_clean_with_bootstrap "clean after bulk deletion import"

assert_file_not_exists "file1 deleted" "dir/file1.txt"
assert_file_exists     "file2 survives" "dir/file2.txt"
assert_file_not_exists "file3 deleted" "dir/file3.txt"
assert_file_exists     "file4 survives" "dir/file4.txt"
assert_file_not_exists "file5 deleted" "dir/file5.txt"

# ════════════════════════════════════════════════════════════════════════════
# Section 2: Binary File Modifications
# ════════════════════════════════════════════════════════════════════════════

begin_section "Import Fidelity: Binary File Add"

make_temp_repo "fidelity-binary-add"
init_git_repo

# Create a binary file (PNG-like header + random bytes)
printf '\x89PNG\r\n\x1a\n' > image.png
dd if=/dev/urandom bs=1 count=256 >> image.png 2>/dev/null
git add image.png
git commit --quiet -m "Add binary image"

atomic init >/dev/null 2>&1
assert_success "import with binary add succeeds" atomic git import
assert_import_clean_with_bootstrap "clean after binary add import"

# ────────────────────────────────────────────────────────────────────────────

begin_section "Import Fidelity: Binary File Modification"

make_temp_repo "fidelity-binary-mod"
init_git_repo

# Create initial binary file
printf '\x89PNG\r\n\x1a\n' > image.png
dd if=/dev/urandom bs=1 count=512 >> image.png 2>/dev/null
original_size=$(wc -c < image.png | tr -d ' ')
git add image.png
git commit --quiet -m "Add binary image"

# Modify the binary file (completely different content)
printf '\x89PNG\r\n\x1a\n' > image.png
dd if=/dev/urandom bs=1 count=128 >> image.png 2>/dev/null
modified_size=$(wc -c < image.png | tr -d ' ')
git add image.png
git commit --quiet -m "Update binary image"

atomic init >/dev/null 2>&1
assert_success "import with binary modification succeeds" atomic git import
assert_import_clean_with_bootstrap "clean after binary modification import"

# Also verify diff shows nothing
assert_no_diff_except_bootstrap "no diff after binary modification import"

# ────────────────────────────────────────────────────────────────────────────

begin_section "Import Fidelity: Multiple Binary Modifications"

make_temp_repo "fidelity-binary-multi"
init_git_repo

# Create binary file, then modify it several times (like execution-order.png)
printf '\x89PNG\r\n\x1a\n' > icon.png
dd if=/dev/urandom bs=1 count=1024 >> icon.png 2>/dev/null
git add icon.png
git commit --quiet -m "Add icon"

for i in 1 2 3 4; do
    printf '\x89PNG\r\n\x1a\n' > icon.png
    dd if=/dev/urandom bs=1 count=$((256 * i)) >> icon.png 2>/dev/null
    git add icon.png
    git commit --quiet -m "Update icon v$i"
done

final_size=$(wc -c < icon.png | tr -d ' ')

atomic init >/dev/null 2>&1
assert_success "import with multiple binary mods succeeds" atomic git import
assert_import_clean_with_bootstrap "clean after multiple binary modifications"

# ────────────────────────────────────────────────────────────────────────────

begin_section "Import Fidelity: Binary File Deletion"

make_temp_repo "fidelity-binary-delete"
init_git_repo

printf '\x89PNG\r\n\x1a\n' > logo.png
dd if=/dev/urandom bs=1 count=256 >> logo.png 2>/dev/null
git add logo.png
git commit --quiet -m "Add logo"

git rm --quiet logo.png
git commit --quiet -m "Remove logo"

atomic init >/dev/null 2>&1
assert_success "import with binary deletion succeeds" atomic git import
assert_import_clean_with_bootstrap "clean after binary deletion"
assert_file_not_exists "logo.png deleted" "logo.png"

# ════════════════════════════════════════════════════════════════════════════
# Section 3: Rewrite Misclassification (Spurious Renames)
# ════════════════════════════════════════════════════════════════════════════

begin_section "Import Fidelity: Rewrite + New File (No Spurious Rename)"

make_temp_repo "fidelity-rewrite"
init_git_repo

# Create a file with substantial content
cat > tests.rs << 'EOF'
fn test_addition() {
    assert_eq!(2 + 2, 4);
}

fn test_subtraction() {
    assert_eq!(5 - 3, 2);
}

fn test_multiplication() {
    assert_eq!(3 * 4, 12);
}

fn test_division() {
    assert_eq!(10 / 2, 5);
}

fn test_modulo() {
    assert_eq!(10 % 3, 1);
}
EOF
git add tests.rs
git commit --quiet -m "Add tests.rs"

# In a single commit: heavily rewrite tests.rs AND add a new file
# The new file shares some keywords with the old file (the trigger for
# renames_from_rewrites), but this should NOT be classified as a rename.
cat > tests.rs << 'EOF'
use insta::assert_snapshot;

#[test]
fn snapshot_basic() {
    assert_snapshot!("hello", @"hello");
}

#[test]
fn snapshot_math() {
    let result = 42;
    assert_snapshot!(result.to_string(), @"42");
}
EOF

cat > helpers.rs << 'EOF'
pub fn setup() {
    // Test setup helper
    println!("setting up test environment");
}

pub fn teardown() {
    println!("tearing down");
}

pub fn assert_close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-6);
}
EOF

git add tests.rs helpers.rs
git commit --quiet -m "Rewrite tests, add helpers"

# More commits on top to exercise continued tracking
git_commit "Update helpers" "helpers.rs" "$(cat helpers.rs)
pub fn extra() {}"

atomic init >/dev/null 2>&1
assert_success "import rewrite+add succeeds" atomic git import
assert_import_clean_with_bootstrap "clean after rewrite+add import"

# Both files must be tracked
assert_file_exists "tests.rs exists" "tests.rs"
assert_file_exists "helpers.rs exists" "helpers.rs"

# Verify status doesn't show either as untracked
status_out="$(atomic status 2>/dev/null || true)"
if echo "$status_out" | grep -qF "tests.rs"; then
    _fail "tests.rs not listed in status" "tests.rs appears in status output"
else
    _pass "tests.rs not listed in status"
fi
if echo "$status_out" | grep -qF "helpers.rs"; then
    _fail "helpers.rs not listed in status" "helpers.rs appears in status output"
else
    _pass "helpers.rs not listed in status"
fi

# ────────────────────────────────────────────────────────────────────────────

begin_section "Import Fidelity: Rename + Modify vs Rewrite + Add"

make_temp_repo "fidelity-rename-vs-rewrite"
init_git_repo

# True rename: git mv + small content change (should be tracked as rename)
cat > original.txt << 'EOF'
This is the original file with some content.
It has multiple lines to give rename detection
enough material to work with for similarity.
Line four.
Line five.
EOF
git add original.txt
git commit --quiet -m "Add original.txt"

git mv original.txt renamed.txt
# Small modification (rename + edit)
echo "Added a new line." >> renamed.txt
git add renamed.txt
git commit --quiet -m "Rename original to renamed"

# Now: delete + add with very different content (should NOT be a rename)
cat > brand_new.txt << 'EOF'
Completely different content here.
Nothing like what was in original.txt.
This is a fresh file with its own purpose.
EOF
git add brand_new.txt
git commit --quiet -m "Add brand_new.txt"

atomic init >/dev/null 2>&1
assert_success "import rename-vs-rewrite succeeds" atomic git import
assert_import_clean_with_bootstrap "clean after rename-vs-rewrite import"

assert_file_not_exists "original.txt removed" "original.txt"
assert_file_exists     "renamed.txt exists" "renamed.txt"
assert_file_exists     "brand_new.txt exists" "brand_new.txt"

# ════════════════════════════════════════════════════════════════════════════
# Section 4: Combined Stress Test
# ════════════════════════════════════════════════════════════════════════════

begin_section "Import Fidelity: Combined Stress Test"

make_temp_repo "fidelity-stress"
init_git_repo

# Phase 1: Create a mix of text and binary files
git_commit "Add readme"   "README.md"      "# Project\nInitial readme"
git_commit "Add config"   "config.yml"     "key: value\nport: 8080"
git_commit "Add source"   "src/main.rs"    "fn main() { println!(\"hello\"); }"
git_commit "Add module"   "src/lib.rs"     "pub mod utils;"
git_commit "Add utils"    "src/utils.rs"   "pub fn helper() -> i32 { 42 }"

printf '\x89PNG\r\n\x1a\n' > logo.png
dd if=/dev/urandom bs=1 count=500 >> logo.png 2>/dev/null
git add logo.png
git commit --quiet -m "Add logo"

printf '\x89PNG\r\n\x1a\n' > banner.png
dd if=/dev/urandom bs=1 count=300 >> banner.png 2>/dev/null
git add banner.png
git commit --quiet -m "Add banner"

# Phase 2: Modify some, delete others, rename one
git_commit "Update readme" "README.md" "# Project\nUpdated readme with more info"

# Modify binary
printf '\x89PNG\r\n\x1a\n' > logo.png
dd if=/dev/urandom bs=1 count=200 >> logo.png 2>/dev/null
git add logo.png
git commit --quiet -m "Resize logo"

# Delete config and banner
git rm --quiet config.yml banner.png
git commit --quiet -m "Remove config and banner"

# Rename utils
git mv src/utils.rs src/helpers.rs
git commit --quiet -m "Rename utils to helpers"

# Phase 3: More churn
git_commit "Update lib"  "src/lib.rs"     "pub mod helpers;"
git_commit "Add tests"   "tests/test.rs"  "#[test] fn it_works() { assert!(true); }"

# Heavily rewrite main.rs AND add a new file in the same commit
cat > src/main.rs << 'EOF'
use std::io;
fn main() -> io::Result<()> {
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    println!("Got: {}", input.trim());
    Ok(())
}
EOF
cat > src/cli.rs << 'EOF'
pub fn parse_args() -> Vec<String> {
    std::env::args().collect()
}
EOF
git add src/main.rs src/cli.rs
git commit --quiet -m "Rewrite main, add cli module"

# Final deletion
git rm --quiet src/lib.rs
git commit --quiet -m "Remove lib.rs"

# Record expected final state
expected_files=(
    "README.md"
    "logo.png"
    "src/main.rs"
    "src/helpers.rs"
    "src/cli.rs"
    "tests/test.rs"
)
deleted_files=(
    "config.yml"
    "banner.png"
    "src/utils.rs"
    "src/lib.rs"
)

# Import
atomic init >/dev/null 2>&1
assert_success "stress test import succeeds" atomic git import
assert_import_clean_with_bootstrap "clean after stress test import"

# Verify expected files exist
for f in "${expected_files[@]}"; do
    assert_file_exists "$f exists" "$f"
done

# Verify deleted files are gone
for f in "${deleted_files[@]}"; do
    assert_file_not_exists "$f deleted" "$f"
done

# Final consistency check: atomic diff should be empty
assert_no_diff_except_bootstrap "no diff in stress test"

# ════════════════════════════════════════════════════════════════════════════
# Summary
# ════════════════════════════════════════════════════════════════════════════

print_summary
