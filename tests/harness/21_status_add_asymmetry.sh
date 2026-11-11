#!/usr/bin/env bash
# 21_status_add_asymmetry.sh — Status/Add view-filter asymmetry bug.
#
# Reproduces the bug report:
#   "I run atomic status and it shows me tons of things that aren't tracked,
#    then I run atomic add and it says there's nothing to add."
#
# A file recorded only on another view must disappear when switching away.
# If independently recreated on disk, it is untracked on this view and `add`
# must stage it; a global TREE row alone does not grant view ownership.
#
# Scenarios tested:
#   1. Switching away from a child or sibling removes that view's file.
#   2. An independently recreated path is untracked until added on this view.
#   3. `add` stages such paths, and recording preserves their bytes.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Status/Add Asymmetry: Child draft view leaks into parent"
# ═══════════════════════════════════════════════════════════════════════════
#
# Workflow (simulates the agent hook creating files on a draft view):
#   1. Init repo on dev (shared)
#   2. Record a base file on dev
#   3. Create a draft child view (like the agent would)
#   4. Switch to draft, create+add+record a new file
#   5. Switch back to dev
#   6. Verify the file is absent, then recreate it independently on dev.
#   7. Verify untracked → added without a contradictory "already tracked".

make_temp_repo "status-add-child-draft"
init_repo

# Step 1: Record a base file on dev so the view isn't empty
create_file "base.txt" "base content"
require_success "add base.txt on dev" atomic add base.txt
require_success "record base.txt on dev" record_change "Add base.txt on dev"
assert_clean "dev is clean after recording base.txt"

# Step 2: Create a draft child view (simulates agent session)
require_success "create draft view agent-session" atomic view create agent-session --draft --parent dev

# Step 3: Switch to the draft view and create+add+record a file
require_success "switch to agent-session" switch_view "agent-session"
assert_current_view "on agent-session" "agent-session"

create_file "agent_file.txt" "created by agent"
require_success "add agent_file.txt on agent-session" atomic add agent_file.txt
require_success "record agent_file.txt" record_change "Agent creates agent_file.txt"

# Verify it's clean on the draft view
assert_status_no_entry "agent_file.txt is clean on agent-session after record" "agent_file.txt"

# Step 4: Switch back to dev
require_success "switch to dev" switch_view "dev"
assert_current_view "back on dev" "dev"

# Step 5: The draft-only file must be absent on dev.
assert_file_not_exists "agent_file.txt is absent on dev" "agent_file.txt"
assert_status_no_entry "agent_file.txt absent from dev status" "agent_file.txt"

# Step 6: An independently recreated file must be addable on dev.
create_file "agent_file.txt" "independently recreated"
assert_status_flag "recreated agent_file.txt is untracked on dev" "?" "agent_file.txt"
require_success "add independently recreated agent_file.txt" atomic add agent_file.txt
assert_status_flag "recreated agent_file.txt is staged on dev" "A" "agent_file.txt"
assert_file_content "adding recreated file preserves bytes" "agent_file.txt" "independently recreated"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Status/Add Asymmetry: Sibling views don't leak"
# ═══════════════════════════════════════════════════════════════════════════
#
# Workflow:
#   1. On dev, record a base file
#   2. Create two sibling views: feature-a, feature-b (both children of dev)
#   3. Record a file on feature-a
#   4. Switch to feature-b
#   5. The feature-a file should not show as Untracked on feature-b
#   6. add on feature-b should not be contradictory

make_temp_repo "status-add-sibling"
init_repo

# Record base on dev
create_file "base.txt" "base"
require_success "add base.txt" atomic add base.txt
require_success "record base" record_change "Add base"

# Create two sibling draft views
require_success "create feature-a" atomic view create feature-a --draft --parent dev
require_success "create feature-b" atomic view create feature-b --draft --parent dev

# Record on feature-a
require_success "switch to feature-a" switch_view "feature-a"
create_file "only_on_a.txt" "feature-a content"
require_success "add only_on_a.txt on feature-a" atomic add only_on_a.txt
require_success "record only_on_a.txt" record_change "Add only_on_a.txt on feature-a"

# Switch to feature-b
require_success "switch to feature-b" switch_view "feature-b"
assert_current_view "on feature-b" "feature-b"

# File from feature-a should NOT appear as Untracked on feature-b
assert_file_not_exists "only_on_a.txt absent on feature-b" "only_on_a.txt"
assert_status_no_entry "only_on_a.txt absent from feature-b status" "only_on_a.txt"

# Recreate the name independently and verify status/add agree.
create_file "only_on_a.txt" "independent sibling content"
assert_status_flag "recreated sibling path is untracked" "?" "only_on_a.txt"
require_success "add recreated sibling path" atomic add only_on_a.txt
assert_status_flag "recreated sibling path is staged" "A" "only_on_a.txt"
assert_file_content "sibling re-add preserves bytes" "only_on_a.txt" "independent sibling content"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Status/Add Asymmetry: Agent batch-add workflow"
# ═══════════════════════════════════════════════════════════════════════════
#
# Simulates the exact agent workflow that triggers the bug:
#   1. Agent hook creates a draft view
#   2. Agent creates multiple files, calls add_batch + record
#   3. Parent view should not see any of those files as Untracked
#   4. atomic add --all on parent should not find contradictory files

make_temp_repo "status-add-agent-batch"
init_repo

# Base content on dev
create_file "README.md" "# Project"
require_success "add README.md" atomic add README.md
require_success "record initial commit" record_change "Initial commit"

# Agent creates a draft view and records multiple files
require_success "create agent-turn-1" atomic view create agent-turn-1 --draft --parent dev
require_success "switch to agent-turn-1" switch_view "agent-turn-1"

create_file "src/main.rs" "fn main() {}"
create_file "src/lib.rs" "pub mod util;"
create_file "src/util.rs" "pub fn helper() {}"
create_file "Cargo.toml" $'[package]\nname = "test"'

require_success "add src/main.rs" atomic add src/main.rs
require_success "add src/lib.rs" atomic add src/lib.rs
require_success "add src/util.rs" atomic add src/util.rs
require_success "add Cargo.toml" atomic add Cargo.toml
require_success "record agent project structure" record_change "Agent adds project structure"

# Switch back to dev
require_success "switch to dev" switch_view "dev"
assert_current_view "on dev after agent work" "dev"

# NO agent files should appear as Untracked on dev
for f in "src/main.rs" "src/lib.rs" "src/util.rs" "Cargo.toml"; do
    assert_file_not_exists "$f absent from dev" "$f"
    assert_status_no_entry "$f absent from dev status" "$f"
done

# atomic add --all should not find contradictory files
require_success "add --all on clean dev" atomic add --all
assert_clean "dev stays clean after add --all"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Status/Add Asymmetry: No-switch agent workflow (THE BUG)"
# ═══════════════════════════════════════════════════════════════════════════
#
# This is the EXACT scenario from the customer report and the wordl project.
# The agent creates files on disk WITHOUT switching views. The agent hook
# then creates a draft view and records on it. The user never leaves dev.
#
# Workflow:
#   1. User is on dev, has a base file recorded
#   2. Agent creates files on disk (user's working copy)
#   3. Agent hook creates a draft view (child of dev)
#   4. Agent hook does: switch to draft → add files → record → switch back to dev
#   5. User runs `atomic status` on dev
#   6. BUG: agent's files show as Untracked (they're on disk, in TREE,
#           but the change is on the draft view, not dev)
#   7. User runs `atomic add` — says "already tracked"
#
# The critical difference from the switch tests above: the FILES REMAIN
# ON DISK because the agent created them in the working copy. The switch
# back to dev removes them via materialization, but then they're recreated
# by the fact that the working copy still has pending content.
#
# Actually, the most common trigger is simpler: the agent hook adds files
# to TREE and records on the draft view, but the user stays on dev the
# whole time (or the switch back leaves files on disk).

make_temp_repo "status-add-no-switch"
init_repo

# Base content on dev
create_file "base.txt" "base"
require_success "add base.txt" atomic add base.txt
require_success "record base" record_change "Initial commit"
assert_clean "dev is clean"

# --- Simulate agent hook: create draft, add+record, switch back ---
# Step 1: Agent creates files on disk while user is on dev
create_file "agent_created.txt" "agent was here"
create_file "src/agent_module.rs" "pub fn agent_fn() {}"

# Step 2: Agent hook creates draft view
require_success "create agent-draft" atomic view create agent-draft --draft --parent dev

# Step 3: Agent hook switches to draft, adds, records, switches back
require_success "switch to agent-draft" switch_view "agent-draft"
require_success "add agent_created.txt on draft" atomic add agent_created.txt
require_success "add src/agent_module.rs on draft" atomic add "src/agent_module.rs"
require_success "record agent files" record_change "Agent turn: add files"

# Step 4: Switch back to dev (this is what the hook does after recording)
require_success "switch to dev" switch_view "dev"
assert_current_view "back on dev after agent hook" "dev"

# Step 5: User manually re-creates files (or they leaked from switch)
# In the real scenario, the agent might still be writing to disk,
# or the files persist because the agent didn't clean up.
# Simulate this by re-creating the files:
create_file "agent_created.txt" "agent was here"
create_file "src/agent_module.rs" "pub fn agent_fn() {}"

# The draft's recorded change is not visible on dev. The independently
# recreated files must be untracked until explicitly added on dev.

assert_status_flag "recreated agent_created.txt is untracked" "?" "agent_created.txt"
assert_status_flag "recreated agent_module.rs is untracked" "?" "src/agent_module.rs"

# Test 2: Adding both untracked paths must stage them.
require_success "add recreated agent files" atomic add agent_created.txt src/agent_module.rs
assert_status_flag "agent_created.txt is staged on dev" "A" "agent_created.txt"
assert_status_flag "agent_module.rs is staged on dev" "A" "src/agent_module.rs"

# Test 3: Record the independently added files and preserve their bytes.
require_success "record recreated agent files on dev" record_change "Record recreated agent files"
assert_clean "dev is clean after recording recreated files"
assert_file_content "agent_created.txt bytes preserved" "agent_created.txt" "agent was here"
assert_file_content "agent module bytes preserved" "src/agent_module.rs" "pub fn agent_fn() {}"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Status/Add Asymmetry: Recreated path stages on the current view"
# ═══════════════════════════════════════════════════════════════════════════
#
# A file independently recreated after switching away is untracked until
# added on this view, regardless of another view's global TREE row.

make_temp_repo "status-add-shows-added"
init_repo

# Record base on dev
create_file "base.txt" "base"
require_success "add base.txt" atomic add base.txt
require_success "record base" record_change "Base commit"

# Create draft, record a file on it
require_success "create feature" atomic view create feature --draft --parent dev
require_success "switch to feature" switch_view "feature"
create_file "feature.txt" "feature content"
require_success "add feature.txt" atomic add feature.txt
require_success "record feature file" record_change "Feature file"

# Switch to dev — file should be removed from disk by materialization
require_success "switch to dev" switch_view "dev"

# Switching removes the recorded draft-only file; re-creation is explicit.
assert_file_not_exists "feature.txt removed on switch to dev" "feature.txt"
assert_status_no_entry "removed feature.txt absent from dev status" "feature.txt"
create_file "feature.txt" "recreated on dev"
assert_status_flag "recreated feature.txt is untracked" "?" "feature.txt"
require_success "add recreated feature.txt" atomic add feature.txt
assert_status_flag "recreated feature.txt is added" "A" "feature.txt"
assert_file_content "recreated feature.txt bytes are preserved" "feature.txt" "recreated on dev"

# ═══════════════════════════════════════════════════════════════════════════
print_summary
