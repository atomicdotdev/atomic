#!/usr/bin/env bash
# Native switch errors recover synchronously through leased operations/receipts.
# These injected returned errors are not process-kill or crash-window coverage.
HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
source "$HARNESS_DIR/native_helpers.sh"
begin_section "Native switch failures: journaled rollback, shelves and retry"
make_temp_repo switch-transaction
run_native_checks native_switch_test.py
print_summary
