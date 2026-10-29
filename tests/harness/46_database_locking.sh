#!/usr/bin/env bash
# Issue #264: run the real storage regressions against this checkout.
# Unlike the CLI suites, this builds Rust tests and does not use ATOMIC_BIN.
# All database fixtures are disposable; no existing repository is opened.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: bash tests/harness/46_database_locking.sh [--full]

Default: snapshot lifecycle, same/cross-process exclusion, bounded lock waits,
         and provenance checkpoint tests.
--full:  all pristine unit tests, storage integration regressions, and a
         libatomic compile check (the issue #264 validation set).

Logs and summary.tsv are retained in a unique directory beneath
target/locking-harness. Override the parent directory with
ATOMIC_LOCK_HARNESS_LOG_DIR. Cargo uses --locked and never updates dependencies.
Exit status is nonzero if any check fails.
EOF
}

full=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --full) full=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
    shift
done

HARNESS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HARNESS_DIR/../.." && pwd)"
cd "$REPO_ROOT"
command -v cargo >/dev/null || { echo "cargo is required" >&2; exit 1; }
command -v rustc >/dev/null || { echo "rustc is required" >&2; exit 1; }

log_parent="${ATOMIC_LOCK_HARNESS_LOG_DIR:-$REPO_ROOT/target/locking-harness}"
mkdir -p "$log_parent"
log_dir="$(mktemp -d "$log_parent/run.XXXXXX")"
summary="$log_dir/summary.tsv"
printf 'check\tresult\texit_code\tlog\n' > "$summary"
{
    printf 'checkout: %s\n' "$REPO_ROOT"
    printf 'full: %s\n' "$full"
    rustc --version
    cargo --version
    uname -sm
    cksum Cargo.lock
} > "$log_dir/environment.log"

passed=0
failed=0
run_check() {
    local name="$1"
    shift
    local log="$log_dir/$name.log"
    local code
    printf '\nRUN  %s\n' "$name"
    # Save the command alongside its complete output for reproducibility.
    printf '%q ' "$@" > "$log"
    printf '\n' >> "$log"
    if "$@" >> "$log" 2>&1; then
        passed=$((passed + 1))
        printf 'PASS %s\n' "$name"
        printf '%s\tPASS\t0\t%s\n' "$name" "$log" >> "$summary"
    else
        code=$?
        failed=$((failed + 1))
        printf 'FAIL %s (exit %s)\n' "$name" "$code" >&2
        tail -n 25 "$log" >&2
        printf '%s\tFAIL\t%s\t%s\n' "$name" "$code" "$log" >> "$summary"
    fi
}

printf 'Atomic database locking harness (#264)\nLogs: %s\n' "$log_dir"
run_check dependencies cargo tree --locked -p atomic-core --depth 1

if [[ "$full" -eq 1 ]]; then
    run_check pristine cargo test --locked -p atomic-core --lib pristine:: -- --test-threads=4
    run_check storage-core cargo test --locked -p atomic-core --test merge_test
    run_check storage cargo test --locked -p atomic-repository --test it -- \
        database_lock_exclusivity_test:: database_open_wait_test:: \
        database_consolidation_shim_test:: legacy_database_merge_test:: \
        schema_guard_test:: provenance_checkpoint_test::
    run_check libatomic cargo check --locked -p libatomic
else
    run_check snapshots cargo test --locked -p atomic-core --lib \
        pristine::txn::pristine::snapshot::tests
    run_check exclusion cargo test --locked -p atomic-repository \
        --test it -- database_lock_exclusivity_test::
    run_check lock-waits cargo test --locked -p atomic-repository \
        --test it -- database_open_wait_test::
    run_check provenance cargo test --locked -p atomic-repository \
        --test it -- provenance_checkpoint_test::
fi

printf '\nChecks: %s passed, %s failed\nSummary: %s\n' "$passed" "$failed" "$summary"
[[ "$failed" -eq 0 ]]
