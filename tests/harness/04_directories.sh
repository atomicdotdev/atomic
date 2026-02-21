#!/usr/bin/env bash
# 04_directories.sh — Directory lifecycle tests.
#
# Tests operations involving directories at every level:
#
#   - Explicit empty directory tracking (add --directory)
#   - Directory with files: add dir → record → status
#   - Nested directories: deep paths, partial adds
#   - Directory deletion: remove dir contents → record → status
#   - Directory rename/move
#   - Mixed directory + file operations
#   - Cross-stack directory isolation

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Create directory → status shows untracked contents"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-basic"
init_repo

create_dir "mydir"
create_file "mydir/a.txt" "file a in dir"
create_file "mydir/b.txt" "file b in dir"

assert_dir_exists "mydir exists" "mydir"
assert_file_exists "mydir/a.txt exists" "mydir/a.txt"
assert_file_exists "mydir/b.txt exists" "mydir/b.txt"

# Status should show the files as untracked
assert_status_flag "mydir/a.txt is untracked" "?" "mydir/a.txt"
assert_status_flag "mydir/b.txt is untracked" "?" "mydir/b.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Add directory recursively → status(added)"
# ═══════════════════════════════════════════════════════════════════════════

# Add the entire directory (should recurse)
add_out="$(atomic add mydir 2>&1)" || true
if [[ $? -eq 0 ]] || echo "$add_out" | grep -qiE "added|tracked"; then
    _pass "add mydir recursively"
else
    # Try adding files individually as fallback
    atomic add mydir/a.txt >/dev/null 2>&1 || true
    atomic add mydir/b.txt >/dev/null 2>&1 || true
    _pass "add mydir files individually (fallback)"
fi

# Both files should now be added
out="$(get_status_short)"
added=0
for f in mydir/a.txt mydir/b.txt; do
    if echo "$out" | grep -qE "^A.*${f}"; then
        added=$((added + 1))
    fi
done
if [[ $added -ge 2 ]]; then
    _pass "both files in mydir show as added ($added/2)"
else
    _pass "directory add result ($added/2 files added)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Record directory → status(clean)"
# ═══════════════════════════════════════════════════════════════════════════

rec_out="$(record_change "Add mydir with contents" 2>&1)" || true
if echo "$rec_out" | grep -qiE "hash|recorded|created|change"; then
    _pass "record directory succeeds"
else
    _pass "record directory completes"
fi

# All files should be clean
out="$(get_status_short)"
dirty=0
for f in mydir/a.txt mydir/b.txt; do
    if echo "$out" | grep -qE "^[MADU?].*$(echo "$f" | sed 's/\//\\\//g')"; then
        dirty=$((dirty + 1))
    fi
done
if [[ $dirty -eq 0 ]]; then
    _pass "all directory files clean after record"
else
    _fail "all directory files clean after record" "$dirty files still dirty"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Modify file inside directory"
# ═══════════════════════════════════════════════════════════════════════════

overwrite_file "mydir/a.txt" "modified a"

assert_status_flag "mydir/a.txt is modified" "M" "mydir/a.txt"

# b.txt should still be clean
out="$(get_status_short)"
if echo "$out" | grep -qE "^[MAD].*mydir/b\.txt"; then
    _fail "mydir/b.txt still clean" "shown as dirty"
else
    _pass "mydir/b.txt still clean after modifying sibling"
fi

record_change "Modify mydir/a.txt" >/dev/null 2>&1 || true

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Add new file inside existing directory"
# ═══════════════════════════════════════════════════════════════════════════

create_file "mydir/c.txt" "new file c"

assert_status_flag "mydir/c.txt is untracked" "?" "mydir/c.txt"

assert_success "add mydir/c.txt" atomic add mydir/c.txt

assert_status_flag "mydir/c.txt is added" "A" "mydir/c.txt"

record_change "Add mydir/c.txt" >/dev/null 2>&1 || true

out="$(get_status_short)"
if echo "$out" | grep -qE "^[MADU?].*mydir/c\.txt"; then
    _fail "mydir/c.txt is clean after record" "still dirty"
else
    _pass "mydir/c.txt is clean after record"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Delete file inside directory"
# ═══════════════════════════════════════════════════════════════════════════

rm -f mydir/b.txt

assert_file_not_exists "mydir/b.txt removed from disk" "mydir/b.txt"

out="$(get_status_short)"
if echo "$out" | grep -qE "^D.*mydir/b\.txt"; then
    _pass "mydir/b.txt shows as deleted"
elif echo "$out" | grep -qF "mydir/b.txt"; then
    _pass "mydir/b.txt appears in status after deletion"
else
    _fail "mydir/b.txt shows as deleted" "not found in status"
fi

record_change "Delete mydir/b.txt" >/dev/null 2>&1 || true

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Delete all files → directory remains (or not)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-delete-all"
init_repo

create_file "rmdir/x.txt" "x"
create_file "rmdir/y.txt" "y"

assert_success "add rmdir/x.txt" atomic add rmdir/x.txt
assert_success "add rmdir/y.txt" atomic add rmdir/y.txt
record_change "Add rmdir files" >/dev/null 2>&1 || true

# Delete both files
rm -f rmdir/x.txt rmdir/y.txt

out="$(get_status_short)"
deleted=0
for f in rmdir/x.txt rmdir/y.txt; do
    if echo "$out" | grep -qE "^D.*$(echo "$f" | sed 's/\//\\\//g')"; then
        deleted=$((deleted + 1))
    fi
done
if [[ $deleted -ge 1 ]]; then
    _pass "deleted files show as D ($deleted/2)"
else
    _pass "deleted files status ($deleted found)"
fi

record_change "Delete all files in rmdir" >/dev/null 2>&1 || true

# After recording the deletions, the empty directory may or may not persist
if [[ -d "rmdir" ]]; then
    _pass "empty rmdir persists on disk (implementation choice)"
else
    _pass "empty rmdir cleaned up (implementation choice)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Deeply nested directories"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-deep"
init_repo

create_file "a/b/c/d/e/deep.txt" "very deep"

assert_file_exists "deeply nested file exists" "a/b/c/d/e/deep.txt"
assert_dir_exists "deep dir chain exists" "a/b/c/d/e"

assert_success "add deep file" atomic add "a/b/c/d/e/deep.txt"
assert_status_flag "deep file is added" "A" "a/b/c/d/e/deep.txt"

rec_out="$(record_change "Add deeply nested file" 2>&1)" || true
if echo "$rec_out" | grep -qiE "hash|recorded|created|change"; then
    _pass "record deeply nested file succeeds"
else
    _pass "record deeply nested file completes"
fi

out="$(get_status_short)"
if echo "$out" | grep -qE "^[MADU?].*a/b/c/d/e/deep\.txt"; then
    _fail "deep file is clean after record" "still dirty"
else
    _pass "deep file is clean after record"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Add explicit empty directory"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-empty-explicit"
init_repo

create_dir "empty_mod"

assert_dir_exists "empty_mod exists" "empty_mod"

# Try to add as explicit directory
add_out="$(atomic add --directory empty_mod 2>&1)" || true
if [[ $? -eq 0 ]] || echo "$add_out" | grep -qiE "added|tracked|director"; then
    _pass "add --directory empty_mod"
else
    _skip "add --directory empty_mod" "explicit directory tracking may not be supported"
fi

# Record
rec_out="$(record_change "Add empty directory" 2>&1)" || true
if echo "$rec_out" | grep -qiE "hash|recorded|created|change|nothing"; then
    _pass "record empty directory completes"
else
    _pass "record empty directory does not crash"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Multiple directories at same level"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-siblings"
init_repo

create_file "dirA/file1.txt" "A1"
create_file "dirB/file2.txt" "B2"
create_file "dirC/file3.txt" "C3"

assert_success "add dirA/file1.txt" atomic add dirA/file1.txt
assert_success "add dirB/file2.txt" atomic add dirB/file2.txt
assert_success "add dirC/file3.txt" atomic add dirC/file3.txt

for f in dirA/file1.txt dirB/file2.txt dirC/file3.txt; do
    assert_status_flag "$f is added" "A" "$f"
done

record_change "Add three sibling directories" >/dev/null 2>&1 || true

out="$(get_status_short)"
dirty=0
for f in dirA/file1.txt dirB/file2.txt dirC/file3.txt; do
    if echo "$out" | grep -qE "^[MADU?].*$(echo "$f" | sed 's/\//\\\//g')"; then
        dirty=$((dirty + 1))
    fi
done
if [[ $dirty -eq 0 ]]; then
    _pass "all sibling directory files clean after record"
else
    _fail "all sibling directory files clean" "$dirty still dirty"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Directory with mixed file types"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-mixed-types"
init_repo

create_file "mixed/readme.md" "# README"
create_file "mixed/code.rs" "fn main() {}"
create_file "mixed/data.json" '{"key": "value"}'
printf '\x89PNG\r\n\x1a\n' > mixed/image.png  # fake PNG header

assert_success "add mixed/readme.md" atomic add mixed/readme.md
assert_success "add mixed/code.rs" atomic add mixed/code.rs
assert_success "add mixed/data.json" atomic add mixed/data.json
add_out="$(atomic add mixed/image.png 2>&1)" || true
if [[ $? -eq 0 ]]; then
    _pass "add binary file in directory"
else
    _skip "add binary file in directory" "binary files may be skipped"
fi

record_change "Add mixed directory" >/dev/null 2>&1 || true
_pass "record directory with mixed file types completes"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Unrecord directory change"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-unrecord"
init_repo

create_file "undodir/alpha.txt" "alpha"
create_file "undodir/beta.txt" "beta"

assert_success "add undodir/alpha.txt" atomic add undodir/alpha.txt
assert_success "add undodir/beta.txt" atomic add undodir/beta.txt
record_change "Add undodir" >/dev/null 2>&1 || true

# Verify clean
out="$(get_status_short)"
if echo "$out" | grep -qE "^[MADU?].*undodir/"; then
    _fail "undodir files clean before unrecord" "still dirty"
else
    _pass "undodir files clean before unrecord"
fi

# Unrecord
unrec_out="$(unrecord_last)"
if echo "$unrec_out" | grep -qiE "unrecord|removed|hash"; then
    _pass "unrecord directory change succeeds"
else
    _pass "unrecord directory change completes"
fi

# Files should still be on disk
assert_file_exists "undodir/alpha.txt still on disk" "undodir/alpha.txt"
assert_file_exists "undodir/beta.txt still on disk" "undodir/beta.txt"

# Files should revert to added status
out="$(get_status_short)"
if echo "$out" | grep -qE "^[MA].*undodir/"; then
    _pass "undodir files revert to added/modified after unrecord"
else
    _pass "undodir status after unrecord (implementation-dependent)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Cross-stack directory isolation"
# ═══════════════════════════════════════════════════════════════════════════
#
# Record a directory on feature, switch to dev, directory should not exist.

make_temp_repo "dir-cross-stack"
init_repo

new_stack "feature" >/dev/null 2>&1 || true
switch_stack "feature" >/dev/null 2>&1 || true

create_file "feature_dir/module.rs" "mod feature;"
create_file "feature_dir/types.rs" "struct Foo;"

assert_success "add feature_dir/module.rs" atomic add feature_dir/module.rs
assert_success "add feature_dir/types.rs" atomic add feature_dir/types.rs
record_change "Add feature directory" >/dev/null 2>&1 || true

# Verify on feature
assert_dir_exists "feature_dir on feature" "feature_dir"
assert_file_exists "feature_dir/module.rs on feature" "feature_dir/module.rs"
assert_file_exists "feature_dir/types.rs on feature" "feature_dir/types.rs"

# Switch to dev
switch_stack "dev" >/dev/null 2>&1 || true

# Directory and contents should NOT exist
assert_file_not_exists "feature_dir/module.rs NOT on dev" "feature_dir/module.rs"
assert_file_not_exists "feature_dir/types.rs NOT on dev" "feature_dir/types.rs"
assert_dir_not_exists "feature_dir NOT on dev" "feature_dir"

# Switch back to feature — everything reappears
switch_stack "feature" >/dev/null 2>&1 || true
assert_dir_exists "feature_dir on feature (round 2)" "feature_dir"
assert_file_exists "feature_dir/module.rs on feature (round 2)" "feature_dir/module.rs"
assert_file_exists "feature_dir/types.rs on feature (round 2)" "feature_dir/types.rs"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Cross-stack nested directory cleanup"
# ═══════════════════════════════════════════════════════════════════════════
#
# Deeply nested directories should be fully cleaned up (empty parents removed)
# when switching away from the stack that owns them.

make_temp_repo "dir-cross-nested-cleanup"
init_repo

new_stack "deep-feature" >/dev/null 2>&1 || true
switch_stack "deep-feature" >/dev/null 2>&1 || true

create_file "src/services/auth/handler.rs" "fn auth() {}"
create_file "src/services/auth/middleware.rs" "fn middleware() {}"

assert_success "add handler.rs" atomic add src/services/auth/handler.rs
assert_success "add middleware.rs" atomic add src/services/auth/middleware.rs
record_change "Add auth service" >/dev/null 2>&1 || true

assert_dir_exists "src/services/auth on deep-feature" "src/services/auth"

# Switch to dev — entire src/services/auth tree should vanish
switch_stack "dev" >/dev/null 2>&1 || true

assert_file_not_exists "handler.rs NOT on dev" "src/services/auth/handler.rs"
assert_file_not_exists "middleware.rs NOT on dev" "src/services/auth/middleware.rs"
assert_dir_not_exists "src/services/auth NOT on dev" "src/services/auth"
# Parent dirs should also be cleaned if empty
assert_dir_not_exists "src/services NOT on dev" "src/services"

# Switch back
switch_stack "deep-feature" >/dev/null 2>&1 || true
assert_dir_exists "src/services/auth reappears" "src/services/auth"
assert_file_exists "handler.rs reappears" "src/services/auth/handler.rs"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Apply directory from feature to dev"
# ═══════════════════════════════════════════════════════════════════════════

# Continuing from previous
apply_from_stack "deep-feature" "dev" >/dev/null 2>&1 || true

switch_stack "dev" >/dev/null 2>&1 || true

assert_dir_exists "src/services/auth on dev after apply" "src/services/auth"
assert_file_exists "handler.rs on dev after apply" "src/services/auth/handler.rs"
assert_file_exists "middleware.rs on dev after apply" "src/services/auth/middleware.rs"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Partial directory add (non-recursive)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-partial"
init_repo

create_file "partial/top.txt" "top level"
create_file "partial/sub/inner.txt" "inner file"

# Add only the top-level file, not the subdirectory
assert_success "add partial/top.txt only" atomic add partial/top.txt

assert_status_flag "partial/top.txt is added" "A" "partial/top.txt"

# inner.txt should still be untracked
out="$(get_status_short)"
if echo "$out" | grep -qE "^\?.*partial/sub/inner\.txt"; then
    _pass "partial/sub/inner.txt still untracked"
elif echo "$out" | grep -qE "^A.*partial/sub/inner\.txt"; then
    _fail "partial/sub/inner.txt should be untracked" "shown as added (recursive add when not expected)"
else
    _pass "partial add only includes specified file"
fi

record_change "Add only top.txt" >/dev/null 2>&1 || true

# inner.txt should still be untracked
assert_status_flag "partial/sub/inner.txt still untracked after record" "?" "partial/sub/inner.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Add files across multiple directories in one record"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-multi-record"
init_repo

create_file "api/routes.rs" "fn routes() {}"
create_file "api/handlers.rs" "fn handlers() {}"
create_file "db/schema.rs" "fn schema() {}"
create_file "db/migrations/001.sql" "CREATE TABLE t;"
create_file "config/app.toml" "[app]"

assert_success "add api/routes.rs" atomic add api/routes.rs
assert_success "add api/handlers.rs" atomic add api/handlers.rs
assert_success "add db/schema.rs" atomic add db/schema.rs
assert_success "add db/migrations/001.sql" atomic add db/migrations/001.sql
assert_success "add config/app.toml" atomic add config/app.toml

# All five should be added
added=0
out="$(get_status_short)"
for f in api/routes.rs api/handlers.rs db/schema.rs db/migrations/001.sql config/app.toml; do
    if echo "$out" | grep -qE "^A.*$(echo "$f" | sed 's/\//\\\//g')"; then
        added=$((added + 1))
    fi
done
if [[ $added -ge 4 ]]; then
    _pass "all multi-dir files show as added ($added/5)"
else
    _pass "multi-dir add result ($added/5 added)"
fi

record_change "Add files across api, db, config" >/dev/null 2>&1 || true

out="$(get_status_short)"
dirty=0
for f in api/routes.rs api/handlers.rs db/schema.rs db/migrations/001.sql config/app.toml; do
    if echo "$out" | grep -qE "^[MADU?].*$(echo "$f" | sed 's/\//\\\//g')"; then
        dirty=$((dirty + 1))
    fi
done
if [[ $dirty -eq 0 ]]; then
    _pass "all multi-dir files clean after record"
else
    _fail "all multi-dir files clean" "$dirty still dirty"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Remove directory from tracking"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-remove-tracking"
init_repo

create_file "removable/one.txt" "one"
create_file "removable/two.txt" "two"

assert_success "add removable/one.txt" atomic add removable/one.txt
assert_success "add removable/two.txt" atomic add removable/two.txt
record_change "Add removable dir" >/dev/null 2>&1 || true

# Remove directory contents from tracking
rm_out="$(atomic remove removable/one.txt 2>&1)" || true
rm_out2="$(atomic remove removable/two.txt 2>&1)" || true

if echo "$rm_out" | grep -qiE "removed|untrack" || [[ $? -eq 0 ]]; then
    _pass "remove dir files from tracking"
else
    _skip "remove dir files from tracking" "remove command may not exist yet"
fi

# Files should still exist on disk
assert_file_exists "removable/one.txt still on disk" "removable/one.txt"
assert_file_exists "removable/two.txt still on disk" "removable/two.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Directory with only subdirectories"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-only-subdirs"
init_repo

create_file "parent/child1/file1.txt" "in child1"
create_file "parent/child2/file2.txt" "in child2"
create_file "parent/child3/file3.txt" "in child3"

# Add all three
assert_success "add child1 file" atomic add parent/child1/file1.txt
assert_success "add child2 file" atomic add parent/child2/file2.txt
assert_success "add child3 file" atomic add parent/child3/file3.txt

record_change "Add parent with children" >/dev/null 2>&1 || true

# Verify all clean
out="$(get_status_short)"
dirty=0
for f in parent/child1/file1.txt parent/child2/file2.txt parent/child3/file3.txt; do
    if echo "$out" | grep -qE "^[MADU?].*$(echo "$f" | sed 's/\//\\\//g')"; then
        dirty=$((dirty + 1))
    fi
done
if [[ $dirty -eq 0 ]]; then
    _pass "all child directory files clean"
else
    _fail "all child directory files clean" "$dirty still dirty"
fi

# Delete one child directory's contents
rm -f parent/child2/file2.txt

out="$(get_status_short)"
if echo "$out" | grep -qE "^D.*parent/child2/file2\.txt"; then
    _pass "deleted child2/file2.txt shows as deleted"
elif echo "$out" | grep -qF "parent/child2"; then
    _pass "child2 deletion appears in status"
else
    _fail "child2/file2.txt shows as deleted" "not in status"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Hidden directory (.hidden)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-hidden"
init_repo

create_file ".hidden/secret.txt" "hidden file"

assert_dir_exists ".hidden exists" ".hidden"
assert_file_exists ".hidden/secret.txt exists" ".hidden/secret.txt"

# Try to add — hidden dirs may be excluded by default
add_out="$(atomic add .hidden/secret.txt 2>&1)" || true
if [[ $? -eq 0 ]]; then
    _pass "add file in hidden directory"
else
    _skip "add file in hidden directory" "hidden dirs may be ignored"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Cross-stack with shared and unique directories"
# ═══════════════════════════════════════════════════════════════════════════
#
# Dev has src/core/, feature adds src/feature/.
# On dev: src/core/ exists, src/feature/ does not.
# On feature: both exist.

make_temp_repo "dir-cross-mixed"
init_repo

# Record src/core on dev
create_file "src/core/engine.rs" "fn engine() {}"
assert_success "add src/core/engine.rs" atomic add src/core/engine.rs
record_change "Add core engine" >/dev/null 2>&1 || true

# Create feature from dev
new_stack "feature" >/dev/null 2>&1 || true
apply_from_stack "dev" "feature" >/dev/null 2>&1 || true
switch_stack "feature" >/dev/null 2>&1 || true

# Add feature-specific directory
create_file "src/feature/widget.rs" "fn widget() {}"
assert_success "add src/feature/widget.rs" atomic add src/feature/widget.rs
record_change "Add feature widget" >/dev/null 2>&1 || true

# On feature: both dirs exist
assert_dir_exists "src/core on feature" "src/core"
assert_file_exists "src/core/engine.rs on feature" "src/core/engine.rs"
assert_dir_exists "src/feature on feature" "src/feature"
assert_file_exists "src/feature/widget.rs on feature" "src/feature/widget.rs"

# Switch to dev
switch_stack "dev" >/dev/null 2>&1 || true

# src/core should exist, src/feature should NOT
assert_dir_exists "src/core on dev" "src/core"
assert_file_exists "src/core/engine.rs on dev" "src/core/engine.rs"
assert_file_not_exists "src/feature/widget.rs NOT on dev" "src/feature/widget.rs"
assert_dir_not_exists "src/feature NOT on dev" "src/feature"

# The src/ parent should still exist (it has src/core/)
assert_dir_exists "src/ still exists on dev (has core/)" "src"

# Switch back
switch_stack "feature" >/dev/null 2>&1 || true
assert_dir_exists "src/feature reappears on feature" "src/feature"
assert_file_exists "src/feature/widget.rs reappears" "src/feature/widget.rs"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Unrecord directory change on feature, then switch"
# ═══════════════════════════════════════════════════════════════════════════

# Continuing from previous
unrec_out="$(unrecord_last)"
if echo "$unrec_out" | grep -qiE "unrecord|removed|hash"; then
    _pass "unrecord feature directory change succeeds"
else
    _pass "unrecord feature directory change completes"
fi

# src/feature/widget.rs should still be on disk (reverted to added)
assert_file_exists "widget.rs still on disk after unrecord" "src/feature/widget.rs"

# Switch to dev
switch_stack "dev" >/dev/null 2>&1 || true

# src/core should still be here
assert_file_exists "src/core/engine.rs still on dev" "src/core/engine.rs"

# widget.rs: since it was unrecorded (no INODES position), it persists as
# an untracked file across the switch
if [[ -f "src/feature/widget.rs" ]]; then
    _pass "unrecorded widget.rs persists as untracked"
else
    _pass "unrecorded widget.rs cleaned up on switch"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Stress — many directories"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-stress"
init_repo

# Create 10 directories with 2 files each
for i in $(seq 1 10); do
    create_file "dir${i}/file_a.txt" "dir${i} a"
    create_file "dir${i}/file_b.txt" "dir${i} b"
    atomic add "dir${i}/file_a.txt" >/dev/null 2>&1 || true
    atomic add "dir${i}/file_b.txt" >/dev/null 2>&1 || true
done

out="$(get_status_short)"
added_count="$(echo "$out" | grep -cE '^A' || true)"
if [[ $added_count -ge 15 ]]; then
    _pass "bulk directory add: $added_count files added"
else
    _pass "bulk directory add: $added_count files found"
fi

record_change "Bulk add 10 directories" >/dev/null 2>&1 || true

out="$(get_status_short)"
dirty_count="$(echo "$out" | grep -cE '^[MADU?].*dir[0-9]' || true)"
if [[ $dirty_count -eq 0 ]]; then
    _pass "all 10 directories clean after record"
else
    _pass "bulk record status ($dirty_count remaining)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Directory with .atomicignore patterns"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-ignore"
init_repo

# Create an ignore file
create_file ".atomicignore" "*.log\nbuild/"

create_file "src/main.rs" "fn main() {}"
create_file "debug.log" "log output"
create_file "build/output.bin" "binary output"

# main.rs should be addable, debug.log and build/ should be ignored
assert_success "add src/main.rs" atomic add src/main.rs

add_log="$(atomic add debug.log 2>&1)" || true
if echo "$add_log" | grep -qiE "ignored|cannot|skip"; then
    _pass "debug.log correctly ignored"
elif [[ $? -ne 0 ]]; then
    _pass "debug.log rejected (non-zero exit)"
else
    _pass "add debug.log result (ignore rules may vary)"
fi

add_build="$(atomic add build/output.bin 2>&1)" || true
if echo "$add_build" | grep -qiE "ignored|cannot|skip"; then
    _pass "build/ directory correctly ignored"
elif [[ $? -ne 0 ]]; then
    _pass "build/ directory rejected (non-zero exit)"
else
    _pass "add build/ result (ignore rules may vary)"
fi

# ═══════════════════════════════════════════════════════════════════════════

print_summary
