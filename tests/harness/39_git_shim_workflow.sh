#!/usr/bin/env bash
# Current opt-in shim: Git branch adoption, record projection, draft detach,
# explicit shim switching, idempotence and unenrolled-repository isolation.
HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
source "$HARNESS_DIR/shim_helpers.sh"
require_git

begin_section "Enrollment, exact native projection and repeat reconciliation"
shim_fixture shim-workflow
MAIN_HEAD="$(git_head_sha_full)"
printf 'native\n' > tracked.txt
require_success "record projects through the enrolled shim" atomic record -am native
require_capture "read projected HEAD" git rev-parse HEAD
if [[ "$COMMAND_OUTPUT" != "$MAIN_HEAD" ]]; then _pass "record advances Git projection"; else _fail "record advances Git projection" "HEAD unchanged"; fi
require_success "native bytes match Git" shim_assert_blob "native blob" tracked.txt $'native\n'
shim_clean "native record"
REPEAT_HEAD="$(git_head_sha_full)"
require_capture "history before repeated reconcile" atomic log --all --format json
HISTORY="$COMMAND_OUTPUT"
require_success "repeated reconcile" atomic git shim reconcile
assert_output_contains "repeat retains HEAD" "$REPEAT_HEAD" git rev-parse HEAD
require_capture "history after repeated reconcile" atomic log --all --format json
if [[ "$COMMAND_OUTPUT" == "$HISTORY" ]]; then _pass "repeat retains Atomic identities"; else _fail "repeat retains Atomic identities" "history changed"; fi

begin_section "Git-authored topic branch is explicitly adopted"
require_success "create Git topic" git switch -qc topic
printf 'foreign\n' > tracked.txt
printf 'topic\n' > topic.txt
: > topic-empty.txt
mkdir -p 'nested space'
printf '#!/bin/sh\necho topic\n' > 'nested space/run.sh'
chmod +x 'nested space/run.sh'
ln -s tracked.txt topic-link
require_success "delete topic baseline path" git rm -q base-only.txt
require_success "stage topic" git add tracked.txt topic.txt topic-empty.txt 'nested space/run.sh' topic-link
require_success "commit topic" git commit -qm topic
TOPIC_HEAD="$(git_head_sha_full)"
require_success "adopt Git topic" atomic git shim reconcile
assert_current_view "topic view adopted" topic
assert_output_contains "adoption preserves foreign commit" "$TOPIC_HEAD" git rev-parse HEAD
require_success "topic blob exact" shim_assert_blob "topic bytes" tracked.txt $'foreign\n'
shim_clean "adopted topic"
require_success "return through Git" git switch -q "$SHIM_MAIN"
metadata="$(mktemp -d "${TMPDIR:-/tmp}/atomic-shim-adoption-XXXXXX")"
_HARNESS_TMPDIRS+=("$metadata")
require_success "snapshot Git-rendered files before adoption" shim_tracked_file_metadata "$metadata/before"
require_success "adopt branch return" atomic git shim reconcile
require_success "snapshot Git-rendered files after adoption" shim_tracked_file_metadata "$metadata/after"
assert_success "adoption does not rewrite Git-rendered files (bytes, modes, inodes, mtimes)" cmp -s "$metadata/before" "$metadata/after"
assert_current_view "main view restored" "$SHIM_MAIN"
require_success "main blob exact" shim_assert_blob "main bytes" tracked.txt $'native\n'
assert_file_not_exists "topic-only path absent on main" topic.txt
assert_file_not_exists "topic-local empty file absent on main" topic-empty.txt
assert_file_exists "topic-deleted path restored on main" base-only.txt
printf 'private sentinel\n' > .atomic/bridge/sentinel.txt

begin_section "Native draft projection is detached, not an invented mirror branch"
MAIN_REF="$(git rev-parse "refs/heads/$SHIM_MAIN")"
require_success "create native draft" atomic view create agent --draft --parent "$SHIM_MAIN"
require_success "switch native draft" atomic view switch agent --force
assert_current_view "agent view current" agent
assert_failure "draft projection detaches HEAD" git symbolic-ref -q HEAD
assert_failure "native switch does not invent branch agent" git show-ref --verify --quiet refs/heads/agent
assert_output_contains "main ref retained" "$MAIN_REF" git rev-parse "refs/heads/$SHIM_MAIN"
printf 'agent\n' > tracked.txt
require_success "record on detached draft" atomic record -am agent
require_success "detached draft blob exact" shim_assert_blob "draft bytes" tracked.txt $'agent\n'
shim_clean "detached draft"
require_success "publish draft branch explicitly (local only)" atomic git shim publish agent --branch agent
assert_success "explicit publication creates branch agent" git show-ref --verify --quiet refs/heads/agent
require_capture "read explicit publication blob" git show refs/heads/agent:tracked.txt
if [[ "$COMMAND_OUTPUT" == agent ]]; then _pass "published branch contains the draft bytes"; else _fail "published branch contains the draft bytes" "$COMMAND_OUTPUT"; fi
require_success "native return to main" atomic view switch "$SHIM_MAIN" --force
require_success "main bytes retained" shim_assert_blob "main after draft" tracked.txt $'native\n'
require_success "explicit shim switch to known topic" atomic git shim switch topic
assert_current_view "explicit switch selects topic" topic
assert_output_contains "explicit shim switch attaches topic" refs/heads/topic git symbolic-ref HEAD
require_success "topic materialized exactly" shim_assert_blob "topic after switch" tracked.txt $'foreign\n'
assert_file_exists "topic-only path restored" topic.txt
assert_file_not_exists "topic deletion survives round-trip" base-only.txt
assert_file_exists "topic-local zero-byte file restored" topic-empty.txt
assert_success "topic-local file stays empty" test ! -s topic-empty.txt
assert_success "topic executable mode restored" test -x 'nested space/run.sh'
assert_success "topic symlink kind restored" test -L topic-link
assert_output_contains "topic symlink target exact" tracked.txt readlink topic-link
assert_success "private sentinel survives switches byte-exactly" shim_file_bytes .atomic/bridge/sentinel.txt $'private sentinel\n'
assert_file_exists "zero-byte file survives switching" empty.txt
assert_success "zero-byte file remains empty" test ! -s empty.txt
shim_clean "explicit switch"
require_success "repeat explicit switch to main" atomic git shim switch "$SHIM_MAIN"
require_success "repeat explicit switch to topic" atomic git shim switch topic
require_success "topic bytes survive second round-trip" shim_assert_blob "second topic projection" tracked.txt $'foreign\n'
assert_success "private sentinel survives second round-trip byte-exactly" shim_file_bytes .atomic/bridge/sentinel.txt $'private sentinel\n'
shim_clean "second round-trip"

begin_section "Unenrolled native repositories do not acquire shim branch behavior"
make_temp_repo shim-unenrolled
init_git_repo
git_commit initial tracked.txt base
BEFORE="$(git_head_sha_full)"
require_success "native init without Git import" atomic init --no-vault
require_success "create unenrolled draft" atomic view create other
require_success "switch unenrolled draft" atomic view switch other --force
assert_output_contains "unenrolled switch preserves Git HEAD" "$BEFORE" git rev-parse HEAD
assert_failure "unenrolled switch creates no mirror branch" git show-ref --verify --quiet refs/heads/other
print_summary
