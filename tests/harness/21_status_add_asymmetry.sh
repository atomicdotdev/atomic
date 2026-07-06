#!/usr/bin/env bash
# 21_status_add_asymmetry.sh — Status/Add view-filter asymmetry bug.
#
# Reproduces the bug report:
#   "I run atomic status and it shows me tons of things that aren't tracked,
#    then I run atomic add and it says there's nothing to add."
#
# Root cause: `status` uses the view's change filter to decide which TREE
# entries are "tracked" (view-aware), while `add` / `is_tracked` checks the
# global TREE table (view-unaware).  Files recorded on a child/sibling view
# are in TREE but their introducing change isn't visible on the current
# view.  Result:
#   - status: file in TREE but filtered out → filesystem walk finds it → "Untracked"
#   - add: file in TREE → is_tracked() = true → "already tracked" → skip
#
# Scenarios tested:
#   1. File recorded on child draft view, parent view shows it as untracked
#   2. File recorded on sibling view, other sibling shows it as untracked
#   3. Agent workflow: auto-created draft view records files, parent is confused
#   4. After fix: no file should be simultaneously "Untracked" AND "already tracked"

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
#   6. BUG: status shows the file as Untracked (it's on disk from the
#           draft view's record, but the change isn't in dev's filter)
#   7. BUG: atomic add says "already tracked" (it's in the global TREE)
#
# Expected after fix:
#   - The file should NOT appear as Untracked on dev
#   - atomic add should not produce a contradictory state

make_temp_repo "status-add-child-draft"
init_repo

# Step 1: Record a base file on dev so the view isn't empty
create_file "base.txt" "base content"
assert_success "add base.txt on dev" atomic add base.txt
record_change "Add base.txt on dev" >/dev/null 2>&1 || true
assert_clean "dev is clean after recording base.txt"

# Step 2: Create a draft child view (simulates agent session)
out="$(atomic view create agent-session --draft --parent dev 2>&1)" || true
if echo "$out" | grep -qiE "created|view"; then
    _pass "create draft view agent-session"
else
    _fail "create draft view agent-session" "output: $out"
fi

# Step 3: Switch to the draft view and create+add+record a file
switch_view "agent-session" >/dev/null 2>&1 || true
assert_current_view "on agent-session" "agent-session"

create_file "agent_file.txt" "created by agent"
assert_success "add agent_file.txt on agent-session" atomic add agent_file.txt
record_change "Agent creates agent_file.txt" >/dev/null 2>&1 || true

# Verify it's clean on the draft view
out="$(get_status_short)"
if echo "$out" | grep -qE "^[MADU?].*agent_file\.txt"; then
    _fail "agent_file.txt is clean on agent-session" "still dirty: $out"
else
    _pass "agent_file.txt is clean on agent-session after record"
fi

# Step 4: Switch back to dev
switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "back on dev" "dev"

# Step 5: THE BUG — status should NOT show agent_file.txt as Untracked
out="$(get_status_short)"
if echo "$out" | grep -qE "^\?.*agent_file\.txt"; then
    _fail "agent_file.txt NOT shown as Untracked on dev" \
        "BUG: status shows '?' for a file that is in TREE (from draft view). Status: $out"
else
    _pass "agent_file.txt NOT shown as Untracked on dev"
fi

# Step 6: THE BUG — add should not claim "already tracked" for an "Untracked" file
# If the file shows as Untracked, trying to add it should succeed.
# If the file doesn't show at all (correct behavior), add should say "already tracked".
# The BROKEN state is: status=Untracked + add=already-tracked (contradictory).
add_out="$(atomic add agent_file.txt 2>&1)" || true
status_out="$(get_status_short)"
has_untracked=$(echo "$status_out" | grep -cE "^\?.*agent_file\.txt" || true)
has_already_tracked=$(echo "$add_out" | grep -ci "already tracked" || true)

if [[ "$has_untracked" -gt 0 && "$has_already_tracked" -gt 0 ]]; then
    _fail "no contradictory status+add state" \
        "BUG: status says Untracked but add says 'already tracked'. Status: $status_out | Add: $add_out"
else
    _pass "no contradictory status+add state"
fi

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
assert_success "add base.txt" atomic add base.txt
record_change "Add base" >/dev/null 2>&1 || true

# Create two sibling draft views
atomic view create feature-a --draft --parent dev >/dev/null 2>&1 || true
atomic view create feature-b --draft --parent dev >/dev/null 2>&1 || true

# Record on feature-a
switch_view "feature-a" >/dev/null 2>&1 || true
create_file "only_on_a.txt" "feature-a content"
assert_success "add only_on_a.txt on feature-a" atomic add only_on_a.txt
record_change "Add only_on_a.txt on feature-a" >/dev/null 2>&1 || true

# Switch to feature-b
switch_view "feature-b" >/dev/null 2>&1 || true
assert_current_view "on feature-b" "feature-b"

# File from feature-a should NOT appear as Untracked on feature-b
out="$(get_status_short)"
if echo "$out" | grep -qE "^\?.*only_on_a\.txt"; then
    _fail "only_on_a.txt NOT shown as Untracked on feature-b" \
        "BUG: sibling view file leaks as Untracked. Status: $out"
else
    _pass "only_on_a.txt NOT shown as Untracked on feature-b"
fi

# No contradictory state
add_out="$(atomic add only_on_a.txt 2>&1)" || true
status_out="$(get_status_short)"
has_untracked=$(echo "$status_out" | grep -cE "^\?.*only_on_a\.txt" || true)
has_already_tracked=$(echo "$add_out" | grep -ci "already tracked" || true)

if [[ "$has_untracked" -gt 0 && "$has_already_tracked" -gt 0 ]]; then
    _fail "no contradictory state on feature-b" \
        "BUG: status=Untracked + add='already tracked'. Status: $status_out | Add: $add_out"
else
    _pass "no contradictory state on feature-b"
fi

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
assert_success "add README.md" atomic add README.md
record_change "Initial commit" >/dev/null 2>&1 || true

# Agent creates a draft view and records multiple files
atomic view create agent-turn-1 --draft --parent dev >/dev/null 2>&1 || true
switch_view "agent-turn-1" >/dev/null 2>&1 || true

create_file "src/main.rs" "fn main() {}"
create_file "src/lib.rs" "pub mod util;"
create_file "src/util.rs" "pub fn helper() {}"
create_file "Cargo.toml" "[package]\nname = \"test\""

assert_success "add src/main.rs" atomic add src/main.rs
assert_success "add src/lib.rs" atomic add src/lib.rs
assert_success "add src/util.rs" atomic add src/util.rs
assert_success "add Cargo.toml" atomic add Cargo.toml
record_change "Agent adds project structure" >/dev/null 2>&1 || true

# Switch back to dev
switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "on dev after agent work" "dev"

# NO agent files should appear as Untracked on dev
out="$(get_status_short)"
agent_untracked=0
for f in "src/main.rs" "src/lib.rs" "src/util.rs" "Cargo.toml"; do
    if echo "$out" | grep -qE "^\?.*$(echo "$f" | sed 's/[\/\.]/\\&/g')"; then
        agent_untracked=$((agent_untracked + 1))
    fi
done

if [[ "$agent_untracked" -gt 0 ]]; then
    _fail "no agent files shown as Untracked on dev (found $agent_untracked)" \
        "BUG: $agent_untracked agent files leak as Untracked on parent. Status: $out"
else
    _pass "no agent files shown as Untracked on dev"
fi

# atomic add --all should not find contradictory files
add_all_out="$(atomic add --all 2>&1)" || true
if echo "$add_all_out" | grep -qiE "already tracked"; then
    # Check if any of those "already tracked" files were shown as Untracked
    status_out="$(get_status_short)"
    has_contradiction=false
    for f in "src/main.rs" "src/lib.rs" "src/util.rs" "Cargo.toml"; do
        escaped_f="$(echo "$f" | sed 's/[\/\.]/\\&/g')"
        is_untracked=$(echo "$status_out" | grep -cE "^\?.*$escaped_f" || true)
        if [[ "$is_untracked" -gt 0 ]]; then
            has_contradiction=true
            break
        fi
    done
    if $has_contradiction; then
        _fail "add --all not contradictory with status" \
            "BUG: files shown as Untracked but add says 'already tracked'"
    else
        _pass "add --all not contradictory with status"
    fi
else
    _pass "add --all not contradictory with status"
fi

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
assert_success "add base.txt" atomic add base.txt
record_change "Initial commit" >/dev/null 2>&1 || true
assert_clean "dev is clean"

# --- Simulate agent hook: create draft, add+record, switch back ---
# Step 1: Agent creates files on disk while user is on dev
create_file "agent_created.txt" "agent was here"
create_file "src/agent_module.rs" "pub fn agent_fn() {}"

# Step 2: Agent hook creates draft view
atomic view create agent-draft --draft --parent dev >/dev/null 2>&1 || true

# Step 3: Agent hook switches to draft, adds, records, switches back
switch_view "agent-draft" >/dev/null 2>&1 || true
assert_success "add agent_created.txt on draft" atomic add agent_created.txt
assert_success "add src/agent_module.rs on draft" atomic add "src/agent_module.rs"
record_change "Agent turn: add files" >/dev/null 2>&1 || true

# Step 4: Switch back to dev (this is what the hook does after recording)
switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "back on dev after agent hook" "dev"

# Step 5: User manually re-creates files (or they leaked from switch)
# In the real scenario, the agent might still be writing to disk,
# or the files persist because the agent didn't clean up.
# Simulate this by re-creating the files:
create_file "agent_created.txt" "agent was here"
create_file "src/agent_module.rs" "pub fn agent_fn() {}"

# THE BUG: These files are in TREE (agent added them), the change is on
# the draft view. On dev:
#   - status: change not in dev's filter → skip from tracked → Untracked
#   - add: is_tracked() checks global TREE → already tracked → skip

out="$(get_status_short)"
echo "  DEBUG status output: $out" >&2

# Test 1: Files should NOT be shown as Untracked
if echo "$out" | grep -qE "^\?.*agent_created\.txt"; then
    _fail "agent_created.txt NOT Untracked on dev" \
        "BUG: file in TREE from draft view shows as Untracked. Status: $out"
else
    _pass "agent_created.txt NOT Untracked on dev"
fi

if echo "$out" | grep -qE "^\?.*agent_module\.rs"; then
    _fail "agent_module.rs NOT Untracked on dev" \
        "BUG: file in TREE from draft view shows as Untracked. Status: $out"
else
    _pass "agent_module.rs NOT Untracked on dev"
fi

# Test 2: The contradictory state must not exist
add_out="$(atomic add agent_created.txt 2>&1)" || true
status_out="$(get_status_short)"
has_untracked=$(echo "$status_out" | grep -cE "^\?.*agent_created\.txt" || true)
has_already_tracked=$(echo "$add_out" | grep -ci "already tracked" || true)

if [[ "$has_untracked" -gt 0 && "$has_already_tracked" -gt 0 ]]; then
    _fail "no status+add contradiction for agent_created.txt" \
        "BUG: status=Untracked + add='already tracked'. Status: $status_out | Add: $add_out"
else
    _pass "no status+add contradiction for agent_created.txt"
fi

# Test 3: If on disk + in TREE, the correct status should be Added
# (tracked but not recorded on THIS view)
if echo "$out" | grep -qE "^A.*agent_created\.txt"; then
    _pass "agent_created.txt shown as Added (correct: tracked, needs record on dev)"
else
    # Also acceptable: not shown at all (file shouldn't be on disk)
    if echo "$out" | grep -q "agent_created\.txt"; then
        _fail "agent_created.txt shown as Added" \
            "File shown with wrong status. Status: $out"
    else
        _pass "agent_created.txt shown as Added (or correctly hidden)"
    fi
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Status/Add Asymmetry: File on disk from other view shows as Added"
# ═══════════════════════════════════════════════════════════════════════════
#
# If a file IS on disk and IS in TREE (from another view), the correct
# status should be "Added" (tracked, needs recording on this view) — NOT
# "Untracked" (which implies it's unknown to the system).
#
# This test verifies that the file shows as Added when it's on disk but
# its graph content belongs to another view.

make_temp_repo "status-add-shows-added"
init_repo

# Record base on dev
create_file "base.txt" "base"
assert_success "add base.txt" atomic add base.txt
record_change "Base commit" >/dev/null 2>&1 || true

# Create draft, record a file on it
atomic view create feature --draft --parent dev >/dev/null 2>&1 || true
switch_view "feature" >/dev/null 2>&1 || true
create_file "feature.txt" "feature content"
assert_success "add feature.txt" atomic add feature.txt
record_change "Feature file" >/dev/null 2>&1 || true

# Switch to dev — file should be removed from disk by materialization
switch_view "dev" >/dev/null 2>&1 || true

# If the file was properly removed by switch_view, it should not appear at all
# If it still exists on disk (materialization bug), it should show as Added, NOT Untracked
if [[ -f "feature.txt" ]]; then
    # File leaked onto disk — at minimum it should be Added, not Untracked
    out="$(get_status_short)"
    if echo "$out" | grep -qE "^\?.*feature\.txt"; then
        _fail "leaked file shows as Added not Untracked" \
            "BUG: file on disk from another view shows as '?' instead of 'A'. Status: $out"
    elif echo "$out" | grep -qE "^A.*feature\.txt"; then
        _pass "leaked file shows as Added not Untracked"
    else
        _pass "leaked file handled correctly (not shown or shown with other status)"
    fi
else
    _pass "leaked file shows as Added not Untracked (file correctly removed by switch)"
fi

# ═══════════════════════════════════════════════════════════════════════════
print_summary
