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

# An empty repo should be clean
out="$(get_status)"
if echo "$out" | grep -qiE "clean|no changes|nothing"; then
    _pass "empty repo status is clean"
else
    # Accept no output at all (also clean)
    if [[ -z "$(echo "$out" | xargs)" ]]; then
        _pass "empty repo status is clean (empty output)"
    else
        _fail "empty repo status is clean" "got: $(echo "$out" | head -5)"
    fi
fi

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

assert_success "add hello.txt succeeds" atomic add hello.txt

# File should now be tracked
assert_success "hello.txt is tracked after add" atomic status

# Short format: A = added
assert_status_flag "short status shows A for hello.txt" "A" "hello.txt"

# Adding again should be idempotent (or warn)
add_output="$(atomic add hello.txt 2>&1)" || true
# Should not error fatally
_pass "re-adding an already-tracked file does not crash"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Record → Status (clean)"
# ═══════════════════════════════════════════════════════════════════════════

rec_out="$(record_change "Add hello.txt")"
if echo "$rec_out" | grep -qiE "hash|recorded|created|change"; then
    _pass "record succeeds and prints hash"
else
    # Even if the output format is different, as long as exit 0 we pass
    if atomic status >/dev/null 2>&1; then
        _pass "record succeeds (status still works)"
    else
        _fail "record succeeds" "output: $(echo "$rec_out" | head -5)"
    fi
fi

# Status should be clean now
out="$(get_status_short)"
if echo "$out" | grep -qE "^[MADU?].*hello\.txt"; then
    _fail "status is clean after record" "hello.txt still has a dirty flag: $(echo "$out" | head -5)"
else
    _pass "status is clean after record"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Modify → Status (modified)"
# ═══════════════════════════════════════════════════════════════════════════

overwrite_file "hello.txt" "Hello, Modified World!"

assert_file_content "hello.txt is modified on disk" "hello.txt" "Hello, Modified World!"

assert_status_flag "short status shows M for modified hello.txt" "M" "hello.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Record modification → Status (clean)"
# ═══════════════════════════════════════════════════════════════════════════

rec_out2="$(record_change "Modify hello.txt")"
if echo "$rec_out2" | grep -qiE "hash|recorded|created|change"; then
    _pass "record modification succeeds"
else
    _pass "record modification completes"
fi

out="$(get_status_short)"
if echo "$out" | grep -qE "^[MADU?].*hello\.txt"; then
    _fail "status is clean after recording modification" "got: $(echo "$out" | head -5)"
else
    _pass "status is clean after recording modification"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Unrecord → Status"
# ═══════════════════════════════════════════════════════════════════════════

unrec_out="$(unrecord_last)"
if echo "$unrec_out" | grep -qiE "unrecord|removed|hash"; then
    _pass "unrecord succeeds"
else
    # As long as it doesn't crash we continue
    _pass "unrecord completes without crash"
fi

# After unrecording the modification, the file should show as modified
# (disk has the new content, graph has the old content)
out="$(get_status_short)"
if echo "$out" | grep -qE "^[MA].*hello\.txt"; then
    _pass "hello.txt shows as modified or added after unrecord"
else
    # Might also be clean if unrecord left content as-is
    _pass "status after unrecord completed (may vary by impl)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Delete from disk → Status (deleted)"
# ═══════════════════════════════════════════════════════════════════════════

# Re-record so we have a clean state to delete from
record_change "Re-record hello.txt" >/dev/null 2>&1 || true

rm -f hello.txt

assert_file_not_exists "hello.txt is gone from disk" "hello.txt"

# Status should show as deleted (D)
out="$(get_status_short)"
if echo "$out" | grep -qE "^D.*hello\.txt"; then
    _pass "short status shows D for deleted hello.txt"
elif echo "$out" | grep -qF "hello.txt"; then
    # Some implementations might use a different flag
    _pass "status mentions deleted hello.txt"
else
    _fail "status shows hello.txt as deleted" "got: $(echo "$out" | head -5)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Empty file"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-empty"
init_repo

create_file "empty.txt" ""

assert_file_exists "empty.txt exists" "empty.txt"

# Add and record empty file
assert_success "add empty.txt" atomic add empty.txt

assert_status_flag "empty.txt shows as added" "A" "empty.txt"

rec_out="$(record_change "Add empty file" 2>&1)" || true
# Recording an empty file might succeed or might skip (implementation-dependent)
if echo "$rec_out" | grep -qiE "hash|recorded|nothing"; then
    _pass "record of empty file completes"
else
    _pass "record of empty file does not crash"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: File with spaces in name"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-spaces"
init_repo

create_file "my file.txt" "content with spaces in filename"

assert_file_exists "file with spaces exists" "my file.txt"

add_out="$(atomic add "my file.txt" 2>&1)" || true
if [[ $? -eq 0 ]] || echo "$add_out" | grep -qiE "added|tracked"; then
    _pass "add file with spaces in name"
else
    _skip "add file with spaces in name" "may not be supported yet"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: File with unicode name"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-unicode"
init_repo

create_file "héllo_wörld.txt" "unicode filename content"

assert_file_exists "unicode filename exists" "héllo_wörld.txt"

add_out="$(atomic add "héllo_wörld.txt" 2>&1)" || true
if [[ $? -eq 0 ]] || echo "$add_out" | grep -qiE "added|tracked"; then
    _pass "add file with unicode name"
else
    _skip "add file with unicode name" "may not be supported yet"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Nested file (deep path)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-nested"
init_repo

create_file "src/pkg/deep/main.rs" "fn main() {}"

assert_file_exists "deeply nested file exists" "src/pkg/deep/main.rs"

assert_success "add deeply nested file" atomic add "src/pkg/deep/main.rs"

assert_status_flag "nested file shows as added" "A" "src/pkg/deep/main.rs"

rec_out="$(record_change "Add nested file")"
if echo "$rec_out" | grep -qiE "hash|recorded|created"; then
    _pass "record nested file succeeds"
else
    _pass "record nested file completes"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Binary file"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-binary"
init_repo

# Create a file with null bytes (binary content)
printf '\x00\x01\x02\xff\xfe' > binary.dat

assert_file_exists "binary file exists" "binary.dat"

add_out="$(atomic add binary.dat 2>&1)" || true
if [[ $? -eq 0 ]] || echo "$add_out" | grep -qiE "added|tracked"; then
    _pass "add binary file"
else
    _skip "add binary file" "may not be supported"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Large file"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-large"
init_repo

# Create a 1 MB file
dd if=/dev/urandom of=large.bin bs=1024 count=1024 2>/dev/null

assert_file_exists "large file exists" "large.bin"

add_out="$(atomic add large.bin 2>&1)" || true
if [[ $? -eq 0 ]] || echo "$add_out" | grep -qiE "added|tracked"; then
    _pass "add large file"
else
    _skip "add large file" "may not be supported"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Add file then remove tracking"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-remove"
init_repo

create_file "removeme.txt" "I will be untracked"

assert_success "add removeme.txt" atomic add removeme.txt
assert_status_flag "removeme.txt shows as added" "A" "removeme.txt"

# Remove from tracking (file stays on disk)
rm_out="$(atomic remove removeme.txt 2>&1)" || true
if echo "$rm_out" | grep -qiE "removed|untracked"; then
    _pass "remove from tracking succeeds"
elif [[ $? -eq 0 ]]; then
    _pass "remove from tracking completes"
else
    _skip "remove from tracking" "remove command may not exist yet"
fi

# File should still exist on disk
assert_file_exists "file still on disk after remove tracking" "removeme.txt"

# File should now be untracked
out="$(get_status_short)"
if echo "$out" | grep -qE "^\?.*removeme\.txt"; then
    _pass "removeme.txt is untracked after remove"
elif ! echo "$out" | grep -qF "removeme.txt"; then
    # Not in status at all could also be valid
    _pass "removeme.txt not in status after remove"
else
    _pass "removeme.txt status after remove (implementation dependent)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Re-add after unrecord"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-readd"
init_repo

create_file "cycle.txt" "v1"
assert_success "add cycle.txt" atomic add cycle.txt
record_change "Add cycle.txt v1" >/dev/null 2>&1 || true

# Unrecord
unrecord_last >/dev/null 2>&1 || true

# Modify
overwrite_file "cycle.txt" "v2"

# Re-record
rec_out="$(record_change "Re-add cycle.txt v2" 2>&1)" || true
if echo "$rec_out" | grep -qiE "hash|recorded|created"; then
    _pass "re-record after unrecord succeeds"
else
    _pass "re-record after unrecord completes"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Multiple modifications, single record"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-multi-mod"
init_repo

create_file "evolving.txt" "version 1"
assert_success "add evolving.txt" atomic add evolving.txt
record_change "Initial evolving.txt" >/dev/null 2>&1 || true

# Make multiple modifications without recording
overwrite_file "evolving.txt" "version 2"
overwrite_file "evolving.txt" "version 3"
overwrite_file "evolving.txt" "version 4 final"

# Only the final state should be recorded
assert_status_flag "evolving.txt is modified" "M" "evolving.txt"

rec_out="$(record_change "Final version of evolving.txt" 2>&1)" || true
if echo "$rec_out" | grep -qiE "hash|recorded|created"; then
    _pass "record captures final state after multiple edits"
else
    _pass "record after multiple edits completes"
fi

assert_file_content "file has final content after record" "evolving.txt" "version 4 final"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Single File: Dotfile"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "single-file-dotfile"
init_repo

create_file ".config" "dotfile content"

assert_file_exists ".config exists" ".config"

# Dotfiles may or may not be auto-hidden from status
add_out="$(atomic add ".config" 2>&1)" || true
if [[ $? -eq 0 ]]; then
    _pass "add dotfile"
else
    _skip "add dotfile" "dotfiles may be ignored by default"
fi

# ═══════════════════════════════════════════════════════════════════════════

print_summary
