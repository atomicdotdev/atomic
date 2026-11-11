#!/usr/bin/env bash
# 41_git_shim_hooks.sh — CB-0D advisory post-checkout shim contract.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

printf '\n%s\n' "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
printf '%s\n' "${BOLD}  Suite: 41_git_shim_hooks${RESET}"
printf '%s\n' "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

atomic_current_view() {
    local output
    output="$(atomic view list --all --json 2>&1)" || return 1
    printf '%s\n' "$output" | python3 -c 'import json,sys; print(json.load(sys.stdin)["current_view"])'
}

wait_for_receipt() {
    local journal="$1"
    local deadline=100
    local checkout_count receipt_count
    while [[ "$deadline" -gt 0 ]]; do
        checkout_count=0
        receipt_count=0
        if [[ -f "$journal" ]]; then
            checkout_count="$(grep -c '"record_type":"post-checkout"' "$journal" || true)"
            receipt_count="$(grep -c '"record_type":"deferred-observation"' "$journal" || true)"
        fi
        if [[ "$checkout_count" -ge 1 && "$receipt_count" -ge 1 ]] && \
           [[ -d .atomic/bridge/deferred-observations ]] && \
           [[ -z "$(find .atomic/bridge/deferred-observations -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
            return 0
        fi
        sleep 0.1
        deadline=$((deadline - 1))
    done
    return 1
}

begin_section "Owned common-dir dispatcher and advisory evidence"
make_temp_repo "git-shim-hooks"
init_git_repo
create_file "tracked.txt" "main\n"
git add tracked.txt
git commit --quiet -m "main"
MAIN="$(git_current_branch)"
MAIN_HEAD="$(git_head_sha_full)"
git switch --quiet -c feature
overwrite_file "tracked.txt" "feature\n"
git add tracked.txt
git commit --quiet -m "feature"
FEATURE_HEAD="$(git_head_sha_full)"
git switch --quiet "$MAIN"

require_success "initialize Atomic for shim hooks" atomic init --view "$MAIN"
require_success "enable advisory shim" atomic git shim enable
require_capture "locate post-checkout hook" git rev-parse --git-path hooks/post-checkout
HOOK_PATH="$COMMAND_OUTPUT"
if [[ -x "$HOOK_PATH" ]] && grep -q '^# atomic:git-bridge-dispatcher:v1$' "$HOOK_PATH"; then
    _pass "shim enable installs an owned executable dispatcher"
else
    _fail "shim enable installs an owned executable dispatcher" "$HOOK_PATH missing or unowned"
fi
if grep -qF -- "${HARNESS_REAL_ATOMIC:-$ATOMIC_BIN}" "$HOOK_PATH"; then
    _pass "dispatcher embeds the absolute Atomic binary"
else
    _fail "dispatcher embeds the absolute Atomic binary" "$(cat "$HOOK_PATH")"
fi
if grep -q 'atomic git import\|shim reconcile' "$HOOK_PATH"; then
    _fail "dispatcher contains no synchronous reconciliation" "$(cat "$HOOK_PATH")"
else
    _pass "dispatcher contains no synchronous reconciliation"
fi

ORIGINAL_DISPATCHER="$(cat "$HOOK_PATH")"
assert_success "refresh owned dispatcher" atomic git shim enable
if [[ "$(cat "$HOOK_PATH")" == "$ORIGINAL_DISPATCHER" ]]; then
    _pass "shim enable refresh is idempotent"
else
    _fail "shim enable refresh is idempotent" "dispatcher content changed"
fi

assert_success "raw Git switch fires advisory hook" git switch feature
JOURNAL=".atomic/bridge/git-events.jsonl"
if wait_for_receipt "$JOURNAL"; then
    _pass "post-checkout appends event and deferred observation receipt"
else
    _fail "post-checkout appends event and deferred observation receipt" "$(cat "$JOURNAL" 2>/dev/null || true)"
fi
if grep -q "\"old_head\":\"$MAIN_HEAD\"" "$JOURNAL" && \
   grep -q "\"new_head\":\"$FEATURE_HEAD\"" "$JOURNAL" && \
   grep -q '"checkout_kind":"branch"' "$JOURNAL"; then
    _pass "checkout evidence records immutable transition inputs"
else
    _fail "checkout evidence records immutable transition inputs" "$(cat "$JOURNAL")"
fi
if [[ "$(atomic_current_view)" == "$MAIN" ]] && [[ ! -e .atomic/bridge/workspace.json ]]; then
    _pass "advisory hook does not switch or checkpoint Atomic"
else
    _fail "advisory hook does not switch or checkpoint Atomic" \
        "view=$(atomic_current_view), workspace checkpoint=$(test -e .atomic/bridge/workspace.json && echo present || echo absent)"
fi

begin_section "Correctness remains independent of hook execution"
rm -f "$HOOK_PATH" "$JOURNAL"
git switch --quiet "$MAIN"
git switch --quiet feature
# Reference-transaction dispatchers remain installed and may legitimately
# recreate the journal; only post-checkout evidence must be absent.
if [[ -f "$JOURNAL" ]] && grep -q '"record_type":"post-checkout"' "$JOURNAL"; then
    _fail "disabled checkout hook produces no checkout evidence" "$(cat "$JOURNAL")"
else
    _pass "disabled checkout hook produces no checkout evidence"
fi
set +e
FORENSIC_OUT="$(atomic status --no-reconcile 2>&1)"
FORENSIC_RC=$?
set -e
if [[ "$FORENSIC_RC" -eq 0 ]] && echo "$FORENSIC_OUT" | grep -q 'AtomicViewMismatch'; then
    _pass "direct read-only observer detects checkout without hooks"
else
    _fail "direct read-only observer detects checkout without hooks" "exit $FORENSIC_RC: $FORENSIC_OUT"
fi

begin_section "Custom hook systems remain untouched"
git config core.hooksPath .githooks
mkdir -p .githooks
printf '#!/bin/sh\necho custom-hook\n' > .githooks/post-checkout
CUSTOM_BEFORE="$(shasum -a 256 .githooks/post-checkout | awk '{ print $1 }')"
set +e
CUSTOM_OUT="$(atomic git shim enable 2>&1)"
CUSTOM_RC=$?
set -e
CUSTOM_AFTER="$(shasum -a 256 .githooks/post-checkout | awk '{ print $1 }')"
if [[ "$CUSTOM_RC" -eq 0 ]] && [[ "$CUSTOM_BEFORE" == "$CUSTOM_AFTER" ]]; then
    _pass "custom core.hooksPath hook is byte-for-byte preserved"
else
    _fail "custom core.hooksPath hook is byte-for-byte preserved" "exit $CUSTOM_RC: $CUSTOM_OUT"
fi
if echo "$CUSTOM_OUT" | grep -q 'core.hooksPath' && echo "$CUSTOM_OUT" | grep -q 'hook-post-checkout'; then
    _pass "custom hook owner receives integration instructions"
else
    _fail "custom hook owner receives integration instructions" "$CUSTOM_OUT"
fi

begin_section "Owned dispatchers install and uninstall without touching user hooks"
make_temp_repo git-shim-hook-ownership
init_git_repo
git_commit initial tracked.txt baseline
require_success "import hook-ownership fixture" atomic git import --no-vault
printf '#!/bin/sh\necho user-owned\n' > .git/hooks/post-checkout
chmod +x .git/hooks/post-checkout
CUSTOM_BYTES="$(shasum -a 256 .git/hooks/post-checkout)"
require_success "install preserves unmanaged post-checkout" atomic git hooks install
assert_output_not_contains "unmanaged hook has no Atomic dispatcher" atomic:git-bridge-dispatcher:v1 cat .git/hooks/post-checkout
for hook in pre-commit pre-push post-rewrite reference-transaction; do
    assert_success "$hook is an executable owned dispatcher" test -x ".git/hooks/$hook"
    assert_output_contains "$hook carries ownership marker" atomic:git-bridge-dispatcher:v1 cat ".git/hooks/$hook"
done
assert_file_not_exists "legacy post-commit importer absent" .git/hooks/post-commit
assert_file_not_exists "legacy post-merge importer absent" .git/hooks/post-merge
require_success "second install succeeds" atomic git hooks install
require_success "uninstall owned dispatchers" atomic git hooks uninstall
for hook in pre-commit pre-push post-rewrite reference-transaction; do
    assert_file_not_exists "$hook removed by uninstall" ".git/hooks/$hook"
done
require_capture "hash unmanaged hook after install/uninstall" shasum -a 256 .git/hooks/post-checkout
if [[ "$COMMAND_OUTPUT" == "$CUSTOM_BYTES" ]]; then
    _pass "unmanaged hook bytes retained exactly"
else
    _fail "unmanaged hook bytes retained exactly" "$COMMAND_OUTPUT"
fi
assert_success "unmanaged executable mode retained" test -x .git/hooks/post-checkout
print_summary
