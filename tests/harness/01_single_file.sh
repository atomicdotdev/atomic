#!/usr/bin/env bash
# 01_single_file.sh — Single-file lifecycle tests.
#
# Tests the complete lifecycle of a single file through every status
# transition:
#
#   create → status(untracked) → add → status(added) → record → status(clean)
#   → modify → status(modified) → record → status(clean)
#   → unrecord → status(added/modified)
#   → delete from disk → status(deleted)
#
# Also covers edge-cases: empty files, binary files, files with spaces/unicode.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Init → Status (empty repo)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file"
init_repo

assert_current_view "initial view is dev" "dev"

assert_clean "empty repo status is clean"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Create → Status (untracked)"
# ═══════════════════════════════════════════════════════════════════════════

create_file "hello.txt" "Hello, World!"

assert_file_exists "hello.txt exists on disk" "hello.txt"
assert_file_content "hello.txt has correct content" "hello.txt" "Hello, World!"

# Status should show the file as untracked
assert_output_contains \
    "status shows hello.txt as untracked" \
    "hello.txt" \
    atomic status

# Short format: ? = untracked
assert_status_flag "short status shows ? for hello.txt" "?" "hello.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Add → Status (added)"
# ═══════════════════════════════════════════════════════════════════════════

require_success "add hello.txt succeeds" atomic add hello.txt

# File should now be tracked
assert_success "hello.txt is tracked after add" atomic status

# Short format: A = added
assert_status_flag "short status shows A for hello.txt" "A" "hello.txt"

assert_success "re-adding an already-tracked file succeeds" atomic add hello.txt

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Record → Status (clean)"
# ═══════════════════════════════════════════════════════════════════════════

require_success "record hello.txt succeeds" record_change "Add hello.txt"

# Status should be clean now
assert_status_no_entry "status is clean after record" "hello.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Modify → Status (modified)"
# ═══════════════════════════════════════════════════════════════════════════

overwrite_file "hello.txt" "Hello, Modified World!"

assert_file_content "hello.txt is modified on disk" "hello.txt" "Hello, Modified World!"

assert_status_flag "short status shows M for modified hello.txt" "M" "hello.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Record modification → Status (clean)"
# ═══════════════════════════════════════════════════════════════════════════

require_success "record modification succeeds" record_change "Modify hello.txt"

assert_status_no_entry "status is clean after recording modification" "hello.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Unrecord → Status"
# ═══════════════════════════════════════════════════════════════════════════

require_success "unrecord succeeds" unrecord_last

# After unrecording the modification, the file should show as modified
# (disk has the new content, graph has the old content)
assert_status_flag "hello.txt shows as modified after unrecord" "M" "hello.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Delete from disk → Status (deleted)"
# ═══════════════════════════════════════════════════════════════════════════

# Re-record so we have a clean state to delete from
require_success "re-record hello.txt" record_change "Re-record hello.txt"

rm -f hello.txt

assert_file_not_exists "hello.txt is gone from disk" "hello.txt"

# Status should show as deleted (D)
assert_status_flag "short status shows D for deleted hello.txt" "D" "hello.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Empty file"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-empty"
init_repo

create_file "empty.txt" ""

assert_file_exists "empty.txt exists" "empty.txt"
if [[ ! -s empty.txt ]]; then
    _pass "empty.txt contains zero bytes"
else
    _fail "empty.txt contains zero bytes" "file is not empty"
fi

# Add and record empty file
require_success "add empty.txt" atomic add empty.txt

assert_status_flag "empty.txt shows as added" "A" "empty.txt"

require_success "record empty file" record_change "Add empty file"
assert_status_no_entry "empty file is clean after record" "empty.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: File with spaces in name"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-spaces"
init_repo

create_file "my file.txt" "content with spaces in filename"

assert_file_exists "file with spaces exists" "my file.txt"

assert_success "add file with spaces in name" atomic add "my file.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: File with unicode name"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-unicode"
init_repo

create_file "héllo_wörld.txt" "unicode filename content"

assert_file_exists "unicode filename exists" "héllo_wörld.txt"

assert_success "add file with unicode name" atomic add "héllo_wörld.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Nested file (deep path)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-nested"
init_repo

create_file "src/pkg/deep/main.rs" "fn main() {}"

assert_file_exists "deeply nested file exists" "src/pkg/deep/main.rs"

assert_success "add deeply nested file" atomic add "src/pkg/deep/main.rs"

assert_status_flag "nested file shows as added" "A" "src/pkg/deep/main.rs"

require_success "record nested file succeeds" record_change "Add nested file"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Binary file"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-binary"
init_repo

# Create a file with null bytes (binary content)
printf '\x00\x01\x02\xff\xfe' > binary.dat

assert_file_exists "binary file exists" "binary.dat"

assert_success "add binary file" atomic add binary.dat

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Large file"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-large"
init_repo

# Create a 1 MB file
dd if=/dev/urandom of=large.bin bs=1024 count=1024 2>/dev/null

assert_file_exists "large file exists" "large.bin"

assert_success "add large file" atomic add large.bin

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Add file then remove tracking"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-remove"
init_repo

create_file "removeme.txt" "I will be untracked"

assert_success "add removeme.txt" atomic add removeme.txt
assert_status_flag "removeme.txt shows as added" "A" "removeme.txt"

# Remove from tracking (file stays on disk)
require_success "remove from tracking succeeds" atomic remove removeme.txt

# File should still exist on disk
assert_file_exists "file still on disk after remove tracking" "removeme.txt"

# File should now be untracked
assert_status_flag "removeme.txt is untracked after remove" "?" "removeme.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Re-add after unrecord"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-readd"
init_repo

create_file "cycle.txt" "v1"
assert_success "add cycle.txt" atomic add cycle.txt
require_success "record cycle.txt v1" record_change "Add cycle.txt v1"

# Unrecord
require_success "unrecord cycle.txt v1" unrecord_last
assert_status_flag "unrecorded cycle.txt is untracked" "?" "cycle.txt"

# Modify
overwrite_file "cycle.txt" "v2"
require_success "re-add cycle.txt after unrecord" atomic add cycle.txt
assert_status_flag "re-added cycle.txt is staged" "A" "cycle.txt"

# Re-record
require_success "re-record after unrecord succeeds" record_change "Re-add cycle.txt v2"
assert_status_no_entry "cycle.txt is clean after re-record" "cycle.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Multiple modifications, single record"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-multi-mod"
init_repo

create_file "evolving.txt" "version 1"
assert_success "add evolving.txt" atomic add evolving.txt
require_success "record initial evolving.txt" record_change "Initial evolving.txt"

# Make multiple modifications without recording
overwrite_file "evolving.txt" "version 2"
overwrite_file "evolving.txt" "version 3"
overwrite_file "evolving.txt" "version 4 final"

# Only the final state should be recorded
assert_status_flag "evolving.txt is modified" "M" "evolving.txt"

require_success "record captures final state after multiple edits" record_change "Final version of evolving.txt"

assert_file_content "file has final content after record" "evolving.txt" "version 4 final"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Dotfile"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-dotfile"
init_repo

create_file ".config" "dotfile content"

assert_file_exists ".config exists" ".config"

# Dotfiles may or may not be auto-hidden from status
assert_success "add dotfile" atomic add ".config"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Large Record: Many files, performance baseline"
# ═══════════════════════════════════════════════════════════════════════════
#
# Generate a realistic project with many files and record it.
# This tests that record performance is acceptable for agentic
# development where changesets tend to be large.
#
# Thresholds:
#   - Initial record of 50 files: < 15s
#   - Subsequent record modifying 10 files: < 10s
#   - View switch after record: < 5s

make_temp_repo "large-record-perf"
init_repo

# Generate 50 source files with realistic content (30-200 lines each)
for i in $(seq 1 20); do
    dir="src/module_${i}"
    mkdir -p "$dir"
    # Main module file (~100 lines)
    {
        echo "// Module $i"
        echo "pub struct Module${i} {"
        for f in $(seq 1 10); do
            echo "    field_${f}: String,"
        done
        echo "}"
        echo ""
        echo "impl Module${i} {"
        echo "    pub fn new() -> Self {"
        echo "        Self {"
        for f in $(seq 1 10); do
            echo "            field_${f}: String::new(),"
        done
        echo "        }"
        echo "    }"
        for m in $(seq 1 5); do
            echo ""
            echo "    pub fn method_${m}(&self) -> &str {"
            echo "        // Implementation for method $m"
            echo "        &self.field_1"
            echo "    }"
        done
        echo "}"
    } > "$dir/mod.rs"

    # Test file (~60 lines)
    {
        echo "#[cfg(test)]"
        echo "mod tests {"
        echo "    use super::*;"
        for t in $(seq 1 5); do
            echo ""
            echo "    #[test]"
            echo "    fn test_method_${t}() {"
            echo "        let m = Module${i}::new();"
            echo "        assert_eq!(m.method_${t}(), \"\");"
            echo "    }"
        done
        echo "}"
    } > "$dir/tests.rs"

    # Config file (~30 lines)
    {
        echo "[module_${i}]"
        echo "name = \"module_${i}\""
        echo "version = \"0.1.0\""
        for k in $(seq 1 10); do
            echo "setting_${k} = \"value_${k}\""
        done
    } > "$dir/config.toml"
done

# Add a large data file (~5000 lines of JSON)
{
    echo "["
    for j in $(seq 1 5000); do
        if [ $j -eq 5000 ]; then
            echo "  {\"id\": $j, \"name\": \"item_$j\", \"value\": $((j * 17))}"
        else
            echo "  {\"id\": $j, \"name\": \"item_$j\", \"value\": $((j * 17))},"
        fi
    done
    echo "]"
} > data.json

# Count what we generated
total_files=$(find . -not -path './.atomic/*' -type f | wc -l | tr -d ' ')
total_lines=$(find . -not -path './.atomic/*' -type f -exec cat {} + | wc -l | tr -d ' ')
echo "  Generated $total_files files, ~$total_lines lines"

# Add all files
assert_success "add all files" atomic add .

# Time the initial record
record_start=$SECONDS
require_success "initial large record completes" record_change "Initial large record"
record_elapsed=$((SECONDS - record_start))

if [ $record_elapsed -le 15 ]; then
    _pass "initial record within 15s threshold (${record_elapsed}s)"
else
    _fail "initial record within 15s threshold" "took ${record_elapsed}s"
fi

assert_clean "clean after initial record"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Large Record: Modify subset, re-record"
# ═══════════════════════════════════════════════════════════════════════════
#
# Modify 10 files and re-record. This exercises the per-file
# INODE_GRAPH fast path during globalization.

# Modify 10 module files
for i in $(seq 1 10); do
    echo "" >> "src/module_${i}/mod.rs"
    echo "    pub fn added_method(&self) -> bool { true }" >> "src/module_${i}/mod.rs"
done

# Also modify the large data file (append 100 lines)
for j in $(seq 5001 5100); do
    echo "  {\"id\": $j, \"name\": \"item_$j\", \"value\": $((j * 17))}," >> data.json
done

modify_start=$SECONDS
require_success "modify record completes" record_change "Modify 10 modules + data"
modify_elapsed=$((SECONDS - modify_start))

if [ $modify_elapsed -le 10 ]; then
    _pass "modify record within 10s threshold (${modify_elapsed}s)"
else
    _fail "modify record within 10s threshold" "took ${modify_elapsed}s"
fi

assert_clean "clean after modify record"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Large Record: View switch performance after large record"
# ═══════════════════════════════════════════════════════════════════════════
#
# Create a feature view, switch to it and back.
# Validates that the parallel materialize + INODE_GRAPH preload
# keeps view switching fast even with many files.

require_success "create feature view" atomic view create feature

switch_start=$SECONDS
require_success "switch to feature" switch_view "feature"
switch_elapsed=$((SECONDS - switch_start))

assert_current_view "on feature" "feature"

if [ $switch_elapsed -le 5 ]; then
    _pass "switch within 5s threshold (${switch_elapsed}s)"
else
    _fail "switch within 5s threshold" "took ${switch_elapsed}s"
fi

# Switch back
switch_start=$SECONDS
require_success "switch back to dev" switch_view "dev"
switch_elapsed=$((SECONDS - switch_start))

assert_current_view "back on dev" "dev"
assert_clean "dev still clean after round-trip"

# Verify file count is preserved
final_files=$(find . -not -path './.atomic/*' -type f | wc -l | tr -d ' ')
if [ "$final_files" -eq "$total_files" ]; then
    _pass "all files preserved after view round-trip ($final_files files)"
else
    _fail "all files preserved after view round-trip" "expected $total_files files, found $final_files"
fi

# ═══════════════════════════════════════════════════════════════════════════

print_summary
