#!/usr/bin/env bash
# Native review-readiness -> exact original-change insertion -> Draft retirement.
# No Git import, fabricated trailer, surrogate squash, or EXPECT_INSERT skip.
# Readiness is a triage verdict checked by this workflow, not a claim that every
# low-level insertion API automatically enforces the triage policy.
HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
source "$HARNESS_DIR/native_helpers.sh"
begin_section "Native signed review, preview and original-change promotion"
make_temp_repo native-review-insert
run_native_checks native_review_insert_test.py
print_summary
