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
#   - Cross-view directory isolation

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
require_success "add mydir recursively" atomic add mydir

# Both files should now be added
for f in mydir/a.txt mydir/b.txt; do
    assert_status_flag "$f is added" "A" "$f"
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Record directory → status(clean)"
# ═══════════════════════════════════════════════════════════════════════════

require_success "record directory succeeds" record_change "Add mydir with contents"

# All files should be clean
for f in mydir/a.txt mydir/b.txt; do
    assert_status_no_entry "$f is clean after record" "$f"
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Modify file inside directory"
# ═══════════════════════════════════════════════════════════════════════════

overwrite_file "mydir/a.txt" "modified a"

assert_status_flag "mydir/a.txt is modified" "M" "mydir/a.txt"

# b.txt should still be clean
assert_status_no_entry "mydir/b.txt still clean after modifying sibling" "mydir/b.txt"

require_success "record mydir/a.txt modification" record_change "Modify mydir/a.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Add new file inside existing directory"
# ═══════════════════════════════════════════════════════════════════════════

create_file "mydir/c.txt" "new file c"

assert_status_flag "mydir/c.txt is untracked" "?" "mydir/c.txt"

require_success "add mydir/c.txt" atomic add mydir/c.txt

assert_status_flag "mydir/c.txt is added" "A" "mydir/c.txt"

require_success "record mydir/c.txt" record_change "Add mydir/c.txt"

assert_status_no_entry "mydir/c.txt is clean after record" "mydir/c.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Delete file inside directory"
# ═══════════════════════════════════════════════════════════════════════════

rm -f mydir/b.txt

assert_file_not_exists "mydir/b.txt removed from disk" "mydir/b.txt"

assert_status_flag "mydir/b.txt shows as deleted" "D" "mydir/b.txt"

require_success "record mydir/b.txt deletion" record_change "Delete mydir/b.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Delete all files → directory remains (or not)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-delete-all"
init_repo

create_file "rmdir/x.txt" "x"
create_file "rmdir/y.txt" "y"

require_success "add rmdir/x.txt" atomic add rmdir/x.txt
require_success "add rmdir/y.txt" atomic add rmdir/y.txt
require_success "record rmdir files" record_change "Add rmdir files"

# Delete both files
rm -f rmdir/x.txt rmdir/y.txt

for f in rmdir/x.txt rmdir/y.txt; do
    assert_status_flag "$f shows as deleted" "D" "$f"
done

require_success "record rmdir deletions" record_change "Delete all files in rmdir"

# After recording the deletions, the empty directory may or may not persist
# Directory presence is not asserted here; file deletions are verified above.

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Deeply nested directories"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-deep"
init_repo

create_file "a/b/c/d/e/deep.txt" "very deep"

assert_file_exists "deeply nested file exists" "a/b/c/d/e/deep.txt"
assert_dir_exists "deep dir chain exists" "a/b/c/d/e"

require_success "add deep file" atomic add "a/b/c/d/e/deep.txt"
assert_status_flag "deep file is added" "A" "a/b/c/d/e/deep.txt"

require_success "record deeply nested file succeeds" record_change "Add deeply nested file"

assert_status_no_entry "deep file is clean after record" "a/b/c/d/e/deep.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Add explicit empty directory"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-empty-explicit"
init_repo

create_dir "empty_mod"

assert_dir_exists "empty_mod exists" "empty_mod"

require_success "add --directory empty_mod" atomic add --directory empty_mod

# Record
require_success "record empty directory" record_change "Add empty directory"

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

require_success "record three sibling directories" record_change "Add three sibling directories"

for f in dirA/file1.txt dirB/file2.txt dirC/file3.txt; do
    assert_status_no_entry "$f is clean after record" "$f"
done

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
require_success "add binary file in directory" atomic add mixed/image.png

require_success "record directory with mixed file types" record_change "Add mixed directory"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Unrecord directory change"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-unrecord"
init_repo

create_file "undodir/alpha.txt" "alpha"
create_file "undodir/beta.txt" "beta"

assert_success "add undodir/alpha.txt" atomic add undodir/alpha.txt
assert_success "add undodir/beta.txt" atomic add undodir/beta.txt
require_success "record undodir" record_change "Add undodir"

# Verify clean
assert_status_no_entry "undodir/alpha.txt clean before unrecord" "undodir/alpha.txt"
assert_status_no_entry "undodir/beta.txt clean before unrecord" "undodir/beta.txt"

# Unrecord
require_success "unrecord directory change succeeds" unrecord_last

# Files should still be on disk
assert_file_exists "undodir/alpha.txt still on disk" "undodir/alpha.txt"
assert_file_exists "undodir/beta.txt still on disk" "undodir/beta.txt"

# The original additions were unrecorded, so both files are untracked.
assert_status_flag "undodir/alpha.txt is untracked" "?" "undodir/alpha.txt"
assert_status_flag "undodir/beta.txt is untracked" "?" "undodir/beta.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Cross-view directory isolation"
# ═══════════════════════════════════════════════════════════════════════════
#
# Record a directory on feature, switch to dev, directory should not exist.

make_temp_repo "dir-cross-view"
init_repo

require_success "create feature view" new_view "feature"
require_success "switch to feature" switch_view "feature"

create_file "feature_dir/module.rs" "mod feature;"
create_file "feature_dir/types.rs" "struct Foo;"

assert_success "add feature_dir/module.rs" atomic add feature_dir/module.rs
assert_success "add feature_dir/types.rs" atomic add feature_dir/types.rs
require_success "record feature directory" record_change "Add feature directory"

# Verify on feature
assert_dir_exists "feature_dir on feature" "feature_dir"
assert_file_exists "feature_dir/module.rs on feature" "feature_dir/module.rs"
assert_file_exists "feature_dir/types.rs on feature" "feature_dir/types.rs"

# Switch to dev
require_success "switch to dev" switch_view "dev"

# Directory and contents should NOT exist
assert_file_not_exists "feature_dir/module.rs NOT on dev" "feature_dir/module.rs"
assert_file_not_exists "feature_dir/types.rs NOT on dev" "feature_dir/types.rs"
assert_dir_not_exists "feature_dir NOT on dev" "feature_dir"

# Switch back to feature — everything reappears
require_success "switch back to feature" switch_view "feature"
assert_dir_exists "feature_dir on feature (round 2)" "feature_dir"
assert_file_exists "feature_dir/module.rs on feature (round 2)" "feature_dir/module.rs"
assert_file_exists "feature_dir/types.rs on feature (round 2)" "feature_dir/types.rs"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Cross-view nested directory cleanup"
# ═══════════════════════════════════════════════════════════════════════════
#
# Deeply nested directories should be fully cleaned up (empty parents removed)
# when switching away from the view that owns them.

make_temp_repo "dir-cross-nested-cleanup"
init_repo

require_success "create deep-feature view" new_view "deep-feature"
require_success "switch to deep-feature" switch_view "deep-feature"

create_file "src/services/auth/handler.rs" "fn auth() {}"
create_file "src/services/auth/middleware.rs" "fn middleware() {}"

assert_success "add handler.rs" atomic add src/services/auth/handler.rs
assert_success "add middleware.rs" atomic add src/services/auth/middleware.rs
require_success "record auth service" record_change "Add auth service"

assert_dir_exists "src/services/auth on deep-feature" "src/services/auth"

# Switch to dev — entire src/services/auth tree should vanish
require_success "switch to dev" switch_view "dev"

assert_file_not_exists "handler.rs NOT on dev" "src/services/auth/handler.rs"
assert_file_not_exists "middleware.rs NOT on dev" "src/services/auth/middleware.rs"
assert_dir_not_exists "src/services/auth NOT on dev" "src/services/auth"
# Parent dirs should also be cleaned if empty
assert_dir_not_exists "src/services NOT on dev" "src/services"

# Switch back
require_success "switch back to deep-feature" switch_view "deep-feature"
assert_dir_exists "src/services/auth reappears" "src/services/auth"
assert_file_exists "handler.rs reappears" "src/services/auth/handler.rs"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Insert directory from feature to dev"
# ═══════════════════════════════════════════════════════════════════════════

# Continuing from previous
require_success "insert deep-feature into dev" insert_from_view "deep-feature" "dev"

require_success "switch to dev after insert" switch_view "dev"

assert_dir_exists "src/services/auth on dev after insert" "src/services/auth"
assert_file_exists "handler.rs on dev after insert" "src/services/auth/handler.rs"
assert_file_exists "middleware.rs on dev after insert" "src/services/auth/middleware.rs"

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
assert_status_flag "partial/sub/inner.txt still untracked" "?" "partial/sub/inner.txt"

require_success "record only top.txt" record_change "Add only top.txt"

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
for f in api/routes.rs api/handlers.rs db/schema.rs db/migrations/001.sql config/app.toml; do
    assert_status_flag "$f is added" "A" "$f"
done

require_success "record files across api, db, config" record_change "Add files across api, db, config"

for f in api/routes.rs api/handlers.rs db/schema.rs db/migrations/001.sql config/app.toml; do
    assert_status_no_entry "$f is clean after record" "$f"
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Remove directory from tracking"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-remove-tracking"
init_repo

create_file "removable/one.txt" "one"
create_file "removable/two.txt" "two"

assert_success "add removable/one.txt" atomic add removable/one.txt
assert_success "add removable/two.txt" atomic add removable/two.txt

# Staged (never recorded): remove untracks the name and keeps the bytes.
assert_success "remove staged file keeps bytes" atomic remove removable/one.txt
assert_file_exists "staged removable/one.txt still on disk" "removable/one.txt"
assert_status_flag "staged removable/one.txt reads untracked" "??" "removable/one.txt"

# Recorded: remove mirrors git rm — the bytes leave the working copy and the
# deletion stays pending until a record finalizes the untrack.
assert_success "record removable/two.txt" atomic record -m "track two" removable/two.txt
assert_success "remove recorded file" atomic remove removable/two.txt
assert_file_not_exists "recorded removable/two.txt gone from disk" "removable/two.txt"
assert_status_flag "recorded removable/two.txt pending deletion" "D" "removable/two.txt"
assert_success "record the removal" atomic record -m "stop tracking two"
assert_status_no_entry "removable/two.txt has no pending state after record" "removable/two.txt"

# Git parity: bytes recreated at the recorded-deleted path are untracked,
# never an implicit re-add candidate.
create_file "removable/two.txt" "reborn"
assert_status_flag "recreated removable/two.txt reads untracked" "??" "removable/two.txt"

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

require_success "record parent with children" record_change "Add parent with children"

# Verify all clean
for f in parent/child1/file1.txt parent/child2/file2.txt parent/child3/file3.txt; do
    assert_status_no_entry "$f is clean" "$f"
done

# Delete one child directory's contents
rm -f parent/child2/file2.txt

assert_status_flag "deleted child2/file2.txt shows as deleted" "D" "parent/child2/file2.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Hidden directory (.hidden)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-hidden"
init_repo

create_file ".hidden/secret.txt" "hidden file"

assert_dir_exists ".hidden exists" ".hidden"
assert_file_exists ".hidden/secret.txt exists" ".hidden/secret.txt"

# Try to add — hidden dirs may be excluded by default
assert_success "add file in hidden directory" atomic add .hidden/secret.txt

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Cross-view with shared and unique directories"
# ═══════════════════════════════════════════════════════════════════════════
#
# Dev has src/core/, feature adds src/feature/.
# On dev: src/core/ exists, src/feature/ does not.
# On feature: both exist.

make_temp_repo "dir-cross-mixed"
init_repo

# Record src/core on dev
create_file "src/core/engine.rs" "fn engine() {}"
require_success "add src/core/engine.rs" atomic add src/core/engine.rs
require_success "record core engine" record_change "Add core engine"

# Create feature from dev
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

# Add feature-specific directory
create_file "src/feature/widget.rs" "fn widget() {}"
require_success "add src/feature/widget.rs" atomic add src/feature/widget.rs
require_success "record feature widget" record_change "Add feature widget"

# On feature: both dirs exist
assert_dir_exists "src/core on feature" "src/core"
assert_file_exists "src/core/engine.rs on feature" "src/core/engine.rs"
assert_dir_exists "src/feature on feature" "src/feature"
assert_file_exists "src/feature/widget.rs on feature" "src/feature/widget.rs"

# Switch to dev
require_success "switch to dev" switch_view "dev"

# src/core should exist, src/feature should NOT
assert_dir_exists "src/core on dev" "src/core"
assert_file_exists "src/core/engine.rs on dev" "src/core/engine.rs"
assert_file_not_exists "src/feature/widget.rs NOT on dev" "src/feature/widget.rs"
assert_dir_not_exists "src/feature NOT on dev" "src/feature"

# The src/ parent should still exist (it has src/core/)
assert_dir_exists "src/ still exists on dev (has core/)" "src"

# Switch back
require_success "switch back to feature" switch_view "feature"
assert_dir_exists "src/feature reappears on feature" "src/feature"
assert_file_exists "src/feature/widget.rs reappears" "src/feature/widget.rs"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Unrecord directory change on feature, then switch"
# ═══════════════════════════════════════════════════════════════════════════

# Continuing from previous
require_success "unrecord feature directory change succeeds" unrecord_last

# src/feature/widget.rs should still be on disk (reverted to added)
assert_file_exists "widget.rs still on disk after unrecord" "src/feature/widget.rs"

# Switch to dev
require_success "switch to dev after unrecord" switch_view "dev"

# src/core should still be here
assert_file_exists "src/core/engine.rs still on dev" "src/core/engine.rs"

# widget.rs: since it was unrecorded (no INODES position), it persists as
# an untracked file across the switch
assert_file_content "unrecorded widget.rs persists with its bytes" "src/feature/widget.rs" "fn widget() {}"
assert_status_flag "unrecorded widget.rs remains untracked" "?" "src/feature/widget.rs"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Stress — many directories"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-stress"
init_repo

# Create 10 directories with 2 files each
for i in $(seq 1 10); do
    create_file "dir${i}/file_a.txt" "dir${i} a"
    create_file "dir${i}/file_b.txt" "dir${i} b"
    require_success "add dir${i}/file_a.txt" atomic add "dir${i}/file_a.txt"
    require_success "add dir${i}/file_b.txt" atomic add "dir${i}/file_b.txt"
done

for i in $(seq 1 10); do
    assert_status_flag "dir${i}/file_a.txt is added" "A" "dir${i}/file_a.txt"
    assert_status_flag "dir${i}/file_b.txt is added" "A" "dir${i}/file_b.txt"
done

require_success "record 10 directories" record_change "Bulk add 10 directories"

assert_clean "all 10 directories clean after record"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Directory: Directory with .atomicignore patterns"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "dir-ignore"
init_repo

# Create an ignore file
create_file ".atomicignore" $'*.log\nbuild/\n'

create_file "src/main.rs" "fn main() {}"
create_file "debug.log" "log output"
create_file "build/output.bin" "binary output"

# main.rs should be addable, debug.log and build/ should be ignored
assert_success "add src/main.rs" atomic add src/main.rs

if ! add_log="$(atomic add debug.log 2>&1)"; then
    if ! printf '%s\n' "$add_log" | grep -qi 'ignor'; then
        _fail "add debug.log reports an ignore refusal" "$add_log"
    fi
fi
assert_status_no_entry "debug.log is ignored" "debug.log"

if ! add_build="$(atomic add build/output.bin 2>&1)"; then
    if ! printf '%s\n' "$add_build" | grep -qi 'ignor'; then
        _fail "add build/output.bin reports an ignore refusal" "$add_build"
    fi
fi
assert_status_no_entry "build/output.bin is ignored" "build/output.bin"

# ═══════════════════════════════════════════════════════════════════════════

print_summary
