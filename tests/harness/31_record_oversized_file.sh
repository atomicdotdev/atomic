#!/usr/bin/env bash
# chmod +x tests/harness/31_record_oversized_file.sh
#
# 31_record_oversized_file.sh — build output stays out of `record`, and an
# oversized file that *is* tracked is reported as a USER error.
#
# The reported symptom was a first `atomic record` in a Next.js project:
#
#   ✗ Internal error: File too large: .next/dev/cache/turbopack/…/00000012.sst
#     (11290117 bytes, limit: 10485760)
#   Hint: This appears to be a bug. Please report it at <issues URL>
#
# Two separate defects, both pinned here:
#
#   1. `.next/` was walked at all. IgnoreRules::load read only `.atomicignore`,
#      never `.gitignore` — so every Node/Rust/Python project handed `record`
#      its build artifacts, and one of them happened to exceed the limit.
#
#   2. When an oversized file does reach `record`, the failure was misreported.
#      `RecordError::FileTooLarge` had no arm in the record command's error
#      mapping and fell through to `other => CliError::Internal`, which renders
#      "Internal error:", appends the bug-report hint, and exits 128 — the code
#      error.rs reserves for actual bugs. This is the same catch-all that
#      swallowed ConflictMarkersPresent (see 30_record_conflict_refusal.sh);
#      that fix added an arm, this one removes the catch-all.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo "${BOLD}record ignores build output and reports oversized files as user errors${RESET}"

# 11290117 bytes — the size from the original report, just over the 10 MiB limit.
OVERSIZED_BYTES=11290117

make_oversized_file() {
    local path="$1"
    mkdir -p "$(dirname "$path")"
    # Sparse allocation: instant, and no multi-megabyte fixture in the repo.
    dd if=/dev/zero of="$path" bs=1 count=0 seek="$OVERSIZED_BYTES" 2>/dev/null
}

# ── 1 · .gitignore keeps build output out of record ─────────────────────────

begin_section ".gitignore is honored, so build output never reaches record"

make_temp_repo record-oversized
init_repo

printf '/.next/\n/node_modules\n' > .gitignore
create_file src/app/page.tsx 'export default function Page() { return null }'
create_file package.json '{"name":"next-app"}'
make_oversized_file .next/dev/cache/turbopack/v16.2.10/00000012.sst

assert_status_no_entry "the build cache is not offered as untracked" ".next"
assert_status_contains "source files still are"                      "src/app/page.tsx"

add_files . >/dev/null
assert_success "record succeeds despite the oversized build artifact" \
    atomic record -m "First change with Atomic"

# The whole point: the artifact must not be in history.
assert_output_contains     "source files were recorded"   "page.tsx" atomic change
assert_output_not_contains "the build cache was not recorded" ".next" atomic change

# ── 2 · .atomicignore still wins over .gitignore ────────────────────────────
#
# .atomicignore is loaded last so a negation there can re-include something the
# project's .gitignore excludes. Without this there would be no way to version
# a generated file that Git skips.

begin_section ".atomicignore overrides .gitignore"

make_temp_repo record-oversized-override
init_repo

printf 'dist/\n'  >  .gitignore
printf '!dist/\n' >> .atomicignore
create_file dist/bundle.js 'built'

assert_status_contains "'!dist/' in .atomicignore re-includes the path" "dist/bundle.js"

# ── 3 · a tracked oversized file is a USER error, not an internal one ───────
#
# These are the assertions that would have caught the misclassification.

begin_section "an oversized file is classified as a user error"

make_temp_repo record-oversized-tracked
init_repo

create_file README.md 'hello'
make_oversized_file dataset.bin
add_files . >/dev/null

assert_failure "record fails on the oversized file" \
    atomic record -m "should be refused"

assert_output_not_contains "not reported as an internal error" \
    "Internal error" atomic record -m "should be refused"
assert_output_not_contains "does not tell the user to file a bug" \
    "Please report it" atomic record -m "should be refused"

# Sizes must be human-readable; a raw byte count says nothing about how far
# over the limit the file is.
assert_output_contains "names the file"        "dataset.bin" atomic record -m "should be refused"
assert_output_contains "size in human units"   "10.8 MiB"    atomic record -m "should be refused"
assert_output_contains "limit in human units"  "10.0 MiB"    atomic record -m "should be refused"
assert_output_not_contains "no raw byte counts" \
    "$OVERSIZED_BYTES" atomic record -m "should be refused"

# Exit code: must be a user/data error, never 128.
# NB: the harness runs under `set -e`, so capture the status without letting
# the expected failure abort the suite.
RECORD_EXIT=0
atomic record -m "should be refused" >/dev/null 2>&1 || RECORD_EXIT=$?
if [[ "$RECORD_EXIT" -ne 0 && "$RECORD_EXIT" -ne 128 ]]; then
    _pass "exit code is a user error ($RECORD_EXIT), not 128"
else
    _fail "exit code is a user error, not 128" \
        "expected non-zero and != 128, got $RECORD_EXIT"
fi

# ── 4 · the hint names escape hatches that actually work ────────────────────
#
# NB: leading '--' is dropped from the needles on purpose — assert_output_contains
# passes them straight to `grep -F`, which would parse '--skip-binary' as an option.

begin_section "the suggested escape hatches work"

assert_output_contains "hint mentions .atomicignore" \
    ".atomicignore" atomic record -m "should be refused"
assert_output_contains "hint mentions skip-binary" \
    "skip-binary"   atomic record -m "should be refused"
assert_output_contains "hint mentions max-size" \
    "max-size"      atomic record -m "should be refused"

assert_success "--skip-binary records everything else" \
    atomic record -m "skip the big one" --skip-binary

# README.md went in; dataset.bin did not.
assert_output_contains "the small file was recorded" \
    "README.md" atomic change
assert_output_not_contains "the oversized file was skipped" \
    "dataset.bin" atomic change

# ── 5 · --max-size moves the ceiling in both directions ─────────────────────
#
# Deliberately exercised against a *small* file. Proving the flag is wired only
# needs the ceiling to move relative to the file; using a genuinely oversized
# one would mean recording 10+ MiB through the tokenizer, which dominated this
# suite's runtime for no extra coverage. Section 3 already pins the real
# 11 MB case against the default limit.

begin_section "--max-size moves the ceiling in both directions"

make_temp_repo record-oversized-maxsize
init_repo

create_file notes.txt 'a few hundred bytes would be plenty here'
add_files . >/dev/null

assert_failure "a lowered --max-size refuses a small file" \
    atomic record -m "should be refused" --max-size 8
assert_output_contains "and reports it the same way" \
    "notes.txt" atomic record -m "should be refused" --max-size 8
assert_success "a raised --max-size accepts it" \
    atomic record -m "raised ceiling" --max-size 100000

print_summary
