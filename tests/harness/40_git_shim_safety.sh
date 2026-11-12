#!/usr/bin/env bash
# Current shim safety: exact-content refusal, private paths, stale claims,
# collisions and pre-materialize failure. No legacy shadow log/branch contract.
HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
source "$HARNESS_DIR/shim_helpers.sh"
require_git

begin_section "Dirty bytes and conflict markers are never published implicitly"
shim_fixture shim-safety
printf 'unrecorded\n' > tracked.txt
shim_refusal "dirty reconcile" 'tracked.txt|dirty|uncommitted|working|staged or unstaged' --allow-wip-capture atomic git shim reconcile
printf 'base\n' > tracked.txt
marker_gt='>>>>>>>'; marker_eq='======='; marker_lt='<<<<<<<'
printf 'base\n%s 1\nleft\n%s 1 [C2YTBAHQ]\nright\n%s 1\n' "$marker_gt" "$marker_eq" "$marker_lt" > tracked.txt
shim_refusal "marker record" 'marker|conflict|tracked.txt' atomic record -am markers
shim_refusal "marker reconcile" 'marker|conflict|tracked.txt|dirty|working' atomic git shim reconcile
# Legacy --allow-conflict-markers is not a promise to bypass the enrolled
# shim's snapshot/publication policy. The ordinary refusal is tested above;
# the native-only explicit override remains covered in suite 30.
printf 'resolved\n' > tracked.txt
require_success "record resolved content" atomic record -am resolved
require_success "resolved projection exact" shim_assert_blob "resolved blob" tracked.txt $'resolved\n'
shim_clean "after resolution"

begin_section "Private metadata never enters an ordinary projection"
require_capture "read projected paths" git ls-tree -r --name-only HEAD
if printf '%s\n' "$COMMAND_OUTPUT" | grep -qE '^\.atomic(/|$)|^\.atomicignore$'; then
    _fail "repository database and ignore policy excluded" "$COMMAND_OUTPUT"
else
    _pass "repository database and ignore policy excluded"
fi
require_success "stage excluded ignore policy deliberately" git add -f .atomicignore
shim_refusal "tracked private-path verification" 'atomicignore|exclu|private|index' atomic git shim verify
require_success "unstage fixture private policy without deleting bytes" git restore --staged .atomicignore
shim_clean "private policy retained locally"
mkdir -p .vault
printf 'private intent evidence\n' > .vault/private-note.md
require_success "stage private vault note deliberately" git add -f .vault/private-note.md
shim_refusal "private vault index verification" 'vault|exclu|private|index' atomic git shim verify
require_success "unstage private vault note without deleting it" git restore --staged .vault/private-note.md
assert_success "private vault bytes survive refusal" shim_file_bytes .vault/private-note.md $'private intent evidence\n'
rm .vault/private-note.md
rmdir .vault

begin_section "Forged provenance and stale Git baseline refuse without publication"
require_capture "read coherent tip" git rev-parse HEAD
COHERENT="$COMMAND_OUTPUT"
require_success "create untrusted claim fixture" git -c core.hooksPath=/dev/null commit --allow-empty -qm $'Untrusted fixture\n\nAtomic-Changes: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'
DRIFT="$(git_head_sha_full)"
shim_refusal "stale-baseline record" 'reconcil|baseline|HeadChanged|unanchored' atomic record -am refused
shim_refusal "forged-claim reconcile" 'unverif|forg|bare Atomic|provenance' atomic git shim reconcile
require_success "restore only fixture ref using expected-old lease" git update-ref "refs/heads/$SHIM_MAIN" "$COHERENT" "$DRIFT"
require_success "reconcile restored fixture" atomic git shim reconcile
shim_clean "after forged claim refusal"

begin_section "Switch collisions preserve exact path types and private bytes"
require_success "create Git feature branch" git switch -qc feature
printf 'target\n' > collision.txt
mkdir nested
printf 'child\n' > nested/child.txt
printf 'regular target\n' > directory-target
require_success "stage feature paths" git add collision.txt nested/child.txt directory-target
require_success "commit feature" git commit -qm feature
require_success "adopt feature" atomic git shim reconcile
require_success "switch to main" atomic git shim switch "$SHIM_MAIN"
printf 'collision.txt\nnested\ndirectory-target/\n' >> .git/info/exclude
printf 'private user bytes\n' > collision.txt
shim_refusal "untracked regular file" 'collision|untracked|collision.txt' --allow-wip-capture atomic git shim switch feature
rm collision.txt
printf 'private parent\n' > nested
shim_refusal "untracked parent file" 'collision|untracked|nested|parent' --allow-wip-capture atomic git shim switch feature
rm nested
mkdir directory-target
printf 'private child\n' > directory-target/private.txt
shim_refusal "directory versus file" 'collision|untracked|directory-target|directory' --allow-wip-capture atomic git shim switch feature
rm directory-target/private.txt
rmdir directory-target
shim_refusal "before-materialize failpoint" 'before.materializ|ATOMIC_TEST_SHIM_FAIL_BEFORE_MATERIALIZE' --allow-wip-capture env ATOMIC_TEST_SHIM_FAIL_BEFORE_MATERIALIZE=1 "$ATOMIC_BIN" git shim switch feature
require_success "switch retries successfully" atomic git shim switch feature
assert_success "target collision file exact" shim_file_bytes collision.txt $'target\n'
assert_success "target nested file exact" shim_file_bytes nested/child.txt $'child\n'
assert_success "target regular file exact" shim_file_bytes directory-target $'regular target\n'
shim_clean "after successful retry"
require_success "return to main" atomic git shim switch "$SHIM_MAIN"
assert_file_not_exists "feature file removed on return" collision.txt
assert_file_not_exists "feature nested file removed on return" nested/child.txt
shim_clean "after collision round-trip"
print_summary
