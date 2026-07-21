#!/usr/bin/env bash
# 22_switch_conflict_markers.sh — View-switch conflict marker regression tests.
#
# Validates that switching views NEVER produces conflict markers in the
# working copy when the target view has a clean, unambiguous graph state.
#
# Bug report: switching from a draft view (with changes) to a shared view
# (DEV) was producing conflict markers instead of a clean materialization
# of the target view's content.
#
# Possible triggers under test:
#   1. Pure view switch (draft → shared) with divergent file edits
#   2. Insert from draft → shared, then switch
#   3. Git shadow + atomic view switch
#   4. Draft view showing conflict markers due to parent inheritance
#
# Key invariant:
#   After switching to a SHARED view, the working copy NEVER has conflict
#   markers — the shared view's change filter scopes to its own changes
#   only, producing a clean build from the graph.
#
#   Draft views may show conflict markers when the parent view has
#   conflicting changes (draft = live perspective, not snapshot).

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ── Conflict marker helpers ─────────────────────────────────────────────────

# Check for conflict markers in a file.
# Returns 0 if markers found, 1 if clean.
has_conflict_markers() {
    local path="$1"
    grep -qE '>>>>>>|<<<<<<|=======' "$path" 2>/dev/null
}

# Assert that a file has NO conflict markers.
assert_no_conflict_markers() {
    local desc="$1"
    local path="$2"
    if [[ ! -f "$path" ]]; then
        _fail "$desc" "file does not exist: $path"
        return
    fi
    if has_conflict_markers "$path"; then
        local content
        content="$(cat "$path")"
        _fail "$desc" "conflict markers found in $path. Content: $(echo "$content" | head -20)"
    else
        _pass "$desc"
    fi
}

# Assert that a file contains a specific substring.
assert_file_contains() {
    local desc="$1"
    local path="$2"
    local needle="$3"
    if [[ ! -f "$path" ]]; then
        _fail "$desc" "file does not exist: $path"
        return
    fi
    if grep -qF "$needle" "$path"; then
        _pass "$desc"
    else
        _fail "$desc" "'$needle' not found in $path. Content: $(cat "$path" | head -10)"
    fi
}

# Assert that a file does NOT contain a specific substring.
assert_file_not_contains() {
    local desc="$1"
    local path="$2"
    local needle="$3"
    if [[ ! -f "$path" ]]; then
        _pass "$desc"
        return
    fi
    if grep -qF "$needle" "$path"; then
        _fail "$desc" "'$needle' should not be in $path but was found"
    else
        _pass "$desc"
    fi
}

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 1: Simple view switch — single-line file, complete overwrite"
# ═══════════════════════════════════════════════════════════════════════════
#
# Baseline: simplest possible divergence.  Each view completely overwrites
# the file.  No partial edits, no shared content.

make_temp_repo "switch-conflict-1"
init_repo

# Create file on dev
create_file "config.txt" "version=1.0"
assert_success "add config.txt" atomic add config.txt
record_change "Add config.txt" >/dev/null 2>&1 || true

# Create draft from dev
new_view "feature" --draft --parent dev >/dev/null 2>&1 || true
switch_view "feature" >/dev/null 2>&1 || true

# Modify on feature
overwrite_file "config.txt" "version=2.0"
record_change "Update config to 2.0 on feature" >/dev/null 2>&1 || true

# Switch to dev — should show dev's version, no conflict markers
switch_view "dev" >/dev/null 2>&1 || true
assert_file_content "dev has original version" "config.txt" "version=1.0"
assert_no_conflict_markers "no conflict markers after switch to dev (case 1)" "config.txt"

# Switch back to feature — should show feature's version
switch_view "feature" >/dev/null 2>&1 || true
assert_file_content "feature has v2.0" "config.txt" "version=2.0"
assert_no_conflict_markers "no conflict markers after switch to feature" "config.txt"

# Round-trip
switch_view "dev" >/dev/null 2>&1 || true
assert_file_content "dev still has v1.0 (round 2)" "config.txt" "version=1.0"
assert_no_conflict_markers "no conflict markers (round 2)" "config.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 2: View switch — multi-line file, divergent line edits"
# ═══════════════════════════════════════════════════════════════════════════
#
# Multi-line file where the draft edits one line.  Switch to shared should
# show the original.

make_temp_repo "switch-conflict-2"
init_repo

# Create a multi-line file on dev
cat > app.py << 'EOF'
#!/usr/bin/env python3
"""Application module."""

def main():
    name = "World"
    greeting = "Hello"
    print(f"{greeting}, {name}!")

if __name__ == "__main__":
    main()
EOF
assert_success "add app.py" atomic add app.py
record_change "Add app.py" >/dev/null 2>&1 || true

ORIGINAL_CONTENT="$(cat app.py)"

# Create draft from dev
new_view "agent" --draft --parent dev >/dev/null 2>&1 || true
switch_view "agent" >/dev/null 2>&1 || true

# Modify line 5 on agent (change name)
sed -i.bak 's/name = "World"/name = "Agent"/' app.py && rm -f app.py.bak
record_change "Change name to Agent" >/dev/null 2>&1 || true

AGENT_CONTENT="$(cat app.py)"

# Switch to dev — should show ORIGINAL, not agent's version, no conflict
switch_view "dev" >/dev/null 2>&1 || true
assert_file_content "dev has original app.py" "app.py" "$ORIGINAL_CONTENT"
assert_no_conflict_markers "no conflict markers on dev after agent edits (case 2)" "app.py"
assert_file_contains "dev has World" "app.py" '"World"'
assert_file_not_contains "dev does NOT have Agent" "app.py" '"Agent"'

# Switch back to agent
switch_view "agent" >/dev/null 2>&1 || true
assert_file_content "agent has its version" "app.py" "$AGENT_CONTENT"
assert_no_conflict_markers "no conflict markers on agent" "app.py"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 3: Both views edit same file, different lines — shared is clean"
# ═══════════════════════════════════════════════════════════════════════════
#
# Dev AND the draft both edit the same multi-line file but at different
# lines.  The KEY invariant: switching to the SHARED view (dev) must
# produce a clean build — only dev's changes, no merge with draft.
#
# The DRAFT view inherits dev's changes (live perspective), so it sees
# both sets of changes merged together — this is by design.

make_temp_repo "switch-conflict-3"
init_repo

# Create multi-line file on dev
cat > server.conf << 'EOF'
[server]
host = localhost
port = 8080
workers = 4

[database]
url = postgres://localhost/mydb
pool_size = 10
timeout = 30
EOF
assert_success "add server.conf" atomic add server.conf
record_change "Add server.conf" >/dev/null 2>&1 || true

# Create draft
new_view "feature" --draft --parent dev >/dev/null 2>&1 || true
switch_view "feature" >/dev/null 2>&1 || true

# Feature edits: change port and pool_size
sed -i.bak 's/port = 8080/port = 9090/' server.conf && rm -f server.conf.bak
sed -i.bak 's/pool_size = 10/pool_size = 20/' server.conf && rm -f server.conf.bak
record_change "Feature: change port and pool_size" >/dev/null 2>&1 || true

# Switch to dev, edit workers and timeout
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers on dev before dev edits" "server.conf"

sed -i.bak 's/workers = 4/workers = 8/' server.conf && rm -f server.conf.bak
sed -i.bak 's/timeout = 30/timeout = 60/' server.conf && rm -f server.conf.bak
record_change "Dev: change workers and timeout" >/dev/null 2>&1 || true

DEV_CONF="$(cat server.conf)"

# Switch to feature — draft inherits dev's changes (live perspective)
# so it sees the merged state of both views' edits
switch_view "feature" >/dev/null 2>&1 || true
assert_no_conflict_markers "no conflict markers on feature (merged, different lines)" "server.conf"
# Feature should see BOTH its own edits AND dev's edits (inherited)
assert_file_contains "feature has port 9090 (own edit)" "server.conf" "port = 9090"
assert_file_contains "feature has pool_size 20 (own edit)" "server.conf" "pool_size = 20"
assert_file_contains "feature has workers 8 (inherited from dev)" "server.conf" "workers = 8"
assert_file_contains "feature has timeout 60 (inherited from dev)" "server.conf" "timeout = 60"

# THE KEY TEST: Switch to dev — must show ONLY dev's state, no merge
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "no conflict markers on dev after round-trip" "server.conf"
assert_file_content "dev has its own clean version" "server.conf" "$DEV_CONF"
assert_file_contains "dev has workers 8" "server.conf" "workers = 8"
assert_file_contains "dev has timeout 60" "server.conf" "timeout = 60"
# Dev must NOT have feature's edits
assert_file_contains "dev has port 8080 (not feature's 9090)" "server.conf" "port = 8080"
assert_file_contains "dev has pool_size 10 (not feature's 20)" "server.conf" "pool_size = 10"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 4: Same-line conflict — shared is clean, draft may merge"
# ═══════════════════════════════════════════════════════════════════════════
#
# Both views edit the exact same line.  The SHARED view must show only
# its own version (clean build).  The draft may show a CRDT merge since
# it inherits the parent's changes.

make_temp_repo "switch-conflict-4"
init_repo

cat > version.txt << 'EOF'
app_name = MyApp
version = 1.0.0
author = original
description = A sample app
EOF
assert_success "add version.txt" atomic add version.txt
record_change "Add version.txt" >/dev/null 2>&1 || true

# Create draft
new_view "hotfix" --draft --parent dev >/dev/null 2>&1 || true
switch_view "hotfix" >/dev/null 2>&1 || true

# Hotfix changes version to 1.0.1
sed -i.bak 's/version = 1.0.0/version = 1.0.1/' version.txt && rm -f version.txt.bak
record_change "Hotfix: bump to 1.0.1" >/dev/null 2>&1 || true

# Switch to dev, change version to 2.0.0
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers before dev edit" "version.txt"

sed -i.bak 's/version = 1.0.0/version = 2.0.0/' version.txt && rm -f version.txt.bak
record_change "Dev: bump to 2.0.0" >/dev/null 2>&1 || true

# THE KEY TEST: dev must show ONLY its own version, no merge
assert_no_conflict_markers "no conflict markers on dev (case 4)" "version.txt"
assert_file_contains "dev has 2.0.0" "version.txt" "version = 2.0.0"

# Switch to hotfix — draft inherits dev's change, CRDT merges
# (the exact result depends on token-level merge — may be 2.0.1 or markers)
switch_view "hotfix" >/dev/null 2>&1 || true
# Don't assert exact content — just verify no crash and file exists
assert_file_exists "version.txt exists on hotfix" "version.txt"

# Switch back to dev — MUST be clean, the visit to hotfix must not pollute
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "no conflict markers on dev after visiting hotfix" "version.txt"
assert_file_contains "dev still has 2.0.0" "version.txt" "version = 2.0.0"
# Verify dev does NOT have hotfix's version mixed in
assert_file_not_contains "dev does NOT have 1.0.1" "version.txt" "1.0.1"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 5: Insert from draft to shared — clean materialization"
# ═══════════════════════════════════════════════════════════════════════════
#
# After inserting changes from a draft into the shared view:
#   1. The shared view should show the merged content with no markers
#   2. Switching to draft and back should preserve clean shared state

make_temp_repo "switch-conflict-5"
init_repo

cat > readme.md << 'EOF'
# My Project

A sample project for testing.

## Features

- Feature A
- Feature B

## Usage

Run the app with `./run.sh`.
EOF
assert_success "add readme.md" atomic add readme.md
record_change "Add readme.md" >/dev/null 2>&1 || true

# Create draft
new_view "docs" --draft --parent dev >/dev/null 2>&1 || true
switch_view "docs" >/dev/null 2>&1 || true

# Add a new section on docs view
cat >> readme.md << 'EOF'

## Contributing

Please read CONTRIBUTING.md before submitting PRs.
EOF
record_change "Docs: add Contributing section" >/dev/null 2>&1 || true

# Switch to dev — should NOT have contributing section
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers on dev before insert" "readme.md"
assert_file_not_contains "dev lacks contributing" "readme.md" "Contributing"

# Insert from docs to dev
insert_from_view "docs" "dev" >/dev/null 2>&1 || true
# Dev should now have the contributing section
assert_no_conflict_markers "no conflict markers after insert" "readme.md"
assert_file_contains "dev has contributing after insert" "readme.md" "Contributing"

# Switch to docs — should match docs version (identical after insert)
switch_view "docs" >/dev/null 2>&1 || true
assert_no_conflict_markers "no conflict markers on docs after insert" "readme.md"
assert_file_contains "docs still has contributing" "readme.md" "Contributing"

# Switch back to dev — must stay clean
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers on dev after round-trip post-insert" "readme.md"
assert_file_contains "dev still has contributing" "readme.md" "Contributing"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 6: Insert divergent edits into shared — then switch"
# ═══════════════════════════════════════════════════════════════════════════
#
# Two changes (draft edit + dev edit) are both on the shared view after
# insert.  The shared view should show the merged result cleanly.  Then
# switching to the draft (which inherits from shared) should also be clean.

make_temp_repo "switch-conflict-6"
init_repo

cat > api.ts << 'EOF'
export function getUser(id: string) {
  return fetch(`/api/users/${id}`);
}

export function listUsers() {
  return fetch('/api/users');
}

export function deleteUser(id: string) {
  return fetch(`/api/users/${id}`, { method: 'DELETE' });
}
EOF
assert_success "add api.ts" atomic add api.ts
record_change "Add api.ts" >/dev/null 2>&1 || true

# Create draft
new_view "refactor" --draft --parent dev >/dev/null 2>&1 || true
switch_view "refactor" >/dev/null 2>&1 || true

# Refactor: change getUser to async
sed -i.bak 's/export function getUser/export async function getUser/' api.ts && rm -f api.ts.bak
sed -i.bak "s/return fetch(\`\/api\/users/return await fetch(\`\/api\/users/" api.ts && rm -f api.ts.bak
record_change "Make getUser async" >/dev/null 2>&1 || true

# Switch to dev, add a new function at the end
switch_view "dev" >/dev/null 2>&1 || true

cat >> api.ts << 'EOF'

export function createUser(data: object) {
  return fetch('/api/users', { method: 'POST', body: JSON.stringify(data) });
}
EOF
record_change "Add createUser function" >/dev/null 2>&1 || true

# Insert refactor changes into dev
insert_from_view "refactor" "dev" >/dev/null 2>&1 || true

# THE KEY TEST: dev should cleanly show both changes merged, no conflict markers
assert_no_conflict_markers "no conflict markers on dev after insert (case 6)" "api.ts"
assert_file_contains "dev has createUser" "api.ts" "createUser"

# Switch to refactor — draft inherits dev (which now includes its own changes)
switch_view "refactor" >/dev/null 2>&1 || true
assert_no_conflict_markers "no conflict markers on refactor after insert" "api.ts"
assert_file_contains "refactor has async getUser" "api.ts" "async function getUser"
# Refactor inherits dev's createUser since it's parented on dev
assert_file_contains "refactor also has createUser (inherited)" "api.ts" "createUser"

# Switch back to dev — must stay clean
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "no conflict markers on dev after round-trip (case 6)" "api.ts"
assert_file_contains "dev still has createUser" "api.ts" "createUser"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 7: Multiple draft views, rapid switching"
# ═══════════════════════════════════════════════════════════════════════════
#
# Three draft views from the same shared parent, each editing the same
# file.  Rapid-switch between all four views.  The shared view (dev)
# must always show its original content with no conflict markers.

make_temp_repo "switch-conflict-7"
init_repo

cat > index.html << 'EOF'
<!DOCTYPE html>
<html>
<head>
    <title>My App</title>
    <meta charset="utf-8">
</head>
<body>
    <h1>Welcome</h1>
    <p>Hello, world!</p>
    <footer>Copyright 2024</footer>
</body>
</html>
EOF
assert_success "add index.html" atomic add index.html
record_change "Add index.html" >/dev/null 2>&1 || true

ORIG_HTML="$(cat index.html)"

# Create 3 drafts, each editing different parts
for view_name in design content footer; do
    new_view "$view_name" --draft --parent dev >/dev/null 2>&1 || true
done

# Design: change title
switch_view "design" >/dev/null 2>&1 || true
sed -i.bak 's/<title>My App<\/title>/<title>Awesome App<\/title>/' index.html && rm -f index.html.bak
record_change "Design: update title" >/dev/null 2>&1 || true

# Content: change paragraph
switch_view "content" >/dev/null 2>&1 || true
sed -i.bak 's/Hello, world!/Welcome to our platform!/' index.html && rm -f index.html.bak
record_change "Content: update greeting" >/dev/null 2>&1 || true

# Footer: change copyright
switch_view "footer" >/dev/null 2>&1 || true
sed -i.bak 's/Copyright 2024/Copyright 2025 Acme Inc/' index.html && rm -f index.html.bak
record_change "Footer: update copyright" >/dev/null 2>&1 || true

# Rapid switch cycle: dev must always show original
for round in 1 2; do
    switch_view "dev" >/dev/null 2>&1 || true
    assert_no_conflict_markers "dev clean (round $round)" "index.html"
    assert_file_content "dev has original (round $round)" "index.html" "$ORIG_HTML"

    switch_view "design" >/dev/null 2>&1 || true
    assert_no_conflict_markers "design clean (round $round)" "index.html"
    assert_file_contains "design has Awesome App (round $round)" "index.html" "Awesome App"

    switch_view "content" >/dev/null 2>&1 || true
    assert_no_conflict_markers "content clean (round $round)" "index.html"
    assert_file_contains "content has platform (round $round)" "index.html" "Welcome to our platform"

    switch_view "footer" >/dev/null 2>&1 || true
    assert_no_conflict_markers "footer clean (round $round)" "index.html"
    assert_file_contains "footer has 2025 (round $round)" "index.html" "Copyright 2025"
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 8: Git shadow — import, create draft, edit, switch"
# ═══════════════════════════════════════════════════════════════════════════
#
# Simulate the git+atomic coexistence scenario.  Create a git repo, import
# into atomic, make edits on a draft view, then switch back to the shared
# view that tracks the git branch.

require_git

make_temp_repo "switch-conflict-8"

# Set up a git repo with several commits
init_git_repo

cat > main.py << 'PYEOF'
"""Main application."""

def greet(name: str) -> str:
    return f"Hello, {name}!"

def farewell(name: str) -> str:
    return f"Goodbye, {name}!"

if __name__ == "__main__":
    print(greet("World"))
    print(farewell("World"))
PYEOF
git add main.py
git commit --quiet -m "Initial: add main.py"

# Second commit: add a utility function
cat > utils.py << 'PYEOF'
"""Utility functions."""

def format_name(first: str, last: str) -> str:
    return f"{first} {last}"

def validate_email(email: str) -> bool:
    return "@" in email and "." in email
PYEOF
git add utils.py
git commit --quiet -m "Add utils.py"

# Detect the git default branch name (master vs main)
GIT_BRANCH="$(git branch --show-current 2>/dev/null || git rev-parse --abbrev-ref HEAD)"

# Import into atomic
assert_success "atomic git import" atomic git import --branch "$GIT_BRANCH"
assert_success "atomic status clean after import" atomic status

# Save main.py content as the shared view's expected state
SHARED_MAIN_PY="$(cat main.py)"

# Create a draft view
new_view "feature" --draft --parent "$GIT_BRANCH" >/dev/null 2>&1 || true
switch_view "feature" >/dev/null 2>&1 || true

# Edit main.py on feature
sed -i.bak 's/Hello, {name}/Hi there, {name}/' main.py && rm -f main.py.bak
assert_success "add modified main.py" atomic add main.py
record_change "Feature: change greeting" >/dev/null 2>&1 || true

# THE KEY TEST: Switch back to shared view — must show git's version
switch_view "$GIT_BRANCH" >/dev/null 2>&1 || true
assert_no_conflict_markers "no conflict markers on shared after feature edits (git)" "main.py"
assert_file_contains "shared has Hello" "main.py" "Hello, {name}"
assert_file_not_contains "shared does NOT have Hi there" "main.py" "Hi there"
# utils.py should be untouched
assert_file_exists "utils.py exists on shared" "utils.py"
assert_no_conflict_markers "no conflict markers in utils.py" "utils.py"

# Switch back to feature
switch_view "feature" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers on feature (round-trip)" "main.py"
assert_file_contains "feature has Hi there" "main.py" "Hi there"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 9: Git shadow — import, git commit, incremental import, switch"
# ═══════════════════════════════════════════════════════════════════════════
#
# After initial import: make a git commit on the shared branch, run
# incremental import, then switch between draft and updated shared view.
# The shared view should show the new git content, not merged content.

require_git

make_temp_repo "switch-conflict-9"

# Set up git + initial import
init_git_repo

cat > config.yml << 'YEOF'
app:
  name: TestApp
  version: "1.0"
  debug: false

database:
  host: localhost
  port: 5432
YEOF
git add config.yml
git commit --quiet -m "Add config.yml"

GIT_BRANCH="$(git branch --show-current 2>/dev/null || git rev-parse --abbrev-ref HEAD)"

assert_success "import" atomic git import --branch "$GIT_BRANCH"

# Create draft, edit config
new_view "tweak" --draft --parent "$GIT_BRANCH" >/dev/null 2>&1 || true
switch_view "tweak" >/dev/null 2>&1 || true

sed -i.bak 's/debug: false/debug: true/' config.yml && rm -f config.yml.bak
assert_success "add config" atomic add config.yml
record_change "Enable debug mode" >/dev/null 2>&1 || true

# Now make a git commit directly on the shared branch (simulating upstream change)
switch_view "$GIT_BRANCH" >/dev/null 2>&1 || true
sed -i.bak 's/port: 5432/port: 5433/' config.yml && rm -f config.yml.bak
git add config.yml
git commit --quiet -m "Change DB port to 5433"

# Incremental import picks up the new git commit
assert_success "incremental import" atomic git import --incremental --branch "$GIT_BRANCH"

# THE KEY TEST: Shared view must show ONLY its own content (with new port)
assert_no_conflict_markers "no markers on shared after import" "config.yml"
assert_file_contains "shared has port 5433" "config.yml" "port: 5433"
assert_file_contains "shared has debug false" "config.yml" "debug: false"

# Switch to tweak — draft inherits the new port from shared
switch_view "tweak" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers on tweak" "config.yml"
assert_file_contains "tweak has debug true (own edit)" "config.yml" "debug: true"
# Draft inherits updated port from shared
assert_file_contains "tweak has port 5433 (inherited)" "config.yml" "port: 5433"

# Switch back to shared — must be clean, no merge pollution
switch_view "$GIT_BRANCH" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers on shared after round-trip" "config.yml"
assert_file_contains "shared still has port 5433" "config.yml" "port: 5433"
assert_file_contains "shared still has debug false" "config.yml" "debug: false"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 10: Large file with many lines — partial edits"
# ═══════════════════════════════════════════════════════════════════════════
#
# A larger file (50+ lines) where the draft edits a few lines in the
# middle.  Switch to shared must show all original values.

make_temp_repo "switch-conflict-10"
init_repo

# Generate a 60-line file
{
    echo "# Configuration file"
    echo ""
    for i in $(seq 1 20); do
        echo "setting_${i} = value_${i}"
    done
    echo ""
    echo "# Section 2"
    echo ""
    for i in $(seq 21 40); do
        echo "param_${i} = default_${i}"
    done
    echo ""
    echo "# Section 3"
    echo ""
    for i in $(seq 41 50); do
        echo "flag_${i} = false"
    done
} > big_config.txt

assert_success "add big_config.txt" atomic add big_config.txt
record_change "Add big_config.txt" >/dev/null 2>&1 || true

ORIG_BIG="$(cat big_config.txt)"

# Create draft
new_view "tuning" --draft --parent dev >/dev/null 2>&1 || true
switch_view "tuning" >/dev/null 2>&1 || true

# Edit lines 10, 25, and 45 (spread across sections)
sed -i.bak 's/setting_10 = value_10/setting_10 = TUNED_10/' big_config.txt && rm -f big_config.txt.bak
sed -i.bak 's/param_25 = default_25/param_25 = TUNED_25/' big_config.txt && rm -f big_config.txt.bak
sed -i.bak 's/flag_45 = false/flag_45 = true/' big_config.txt && rm -f big_config.txt.bak
record_change "Tune settings" >/dev/null 2>&1 || true

# Switch to dev — should show ALL original values
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers on dev (large file)" "big_config.txt"
assert_file_content "dev has original big_config" "big_config.txt" "$ORIG_BIG"
assert_file_contains "dev has value_10" "big_config.txt" "setting_10 = value_10"
assert_file_contains "dev has default_25" "big_config.txt" "param_25 = default_25"
assert_file_contains "dev has flag_45 false" "big_config.txt" "flag_45 = false"

# Switch to tuning — should show tuned values
switch_view "tuning" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers on tuning (large file)" "big_config.txt"
assert_file_contains "tuning has TUNED_10" "big_config.txt" "setting_10 = TUNED_10"
assert_file_contains "tuning has TUNED_25" "big_config.txt" "param_25 = TUNED_25"
assert_file_contains "tuning has flag_45 true" "big_config.txt" "flag_45 = true"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 11: Status clean after switch — no false Modified"
# ═══════════════════════════════════════════════════════════════════════════
#
# After switching views, `atomic status` should report a clean working
# copy — not show the switched-to files as Modified.

make_temp_repo "switch-conflict-11"
init_repo

create_file "src/lib.rs" 'pub fn hello() -> &'"'"'static str { "hello" }'
assert_success "add src/lib.rs" atomic add src/lib.rs
record_change "Add lib.rs" >/dev/null 2>&1 || true

new_view "experiment" --draft --parent dev >/dev/null 2>&1 || true
switch_view "experiment" >/dev/null 2>&1 || true

overwrite_file "src/lib.rs" 'pub fn hello() -> &'"'"'static str { "hi there" }'
record_change "Experiment: change greeting" >/dev/null 2>&1 || true

# Switch to dev
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers" "src/lib.rs"
assert_clean "dev is clean after switch"

# Switch to experiment
switch_view "experiment" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers on experiment" "src/lib.rs"
assert_clean "experiment is clean after switch"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 12: Multiple files — some shared, some view-specific"
# ═══════════════════════════════════════════════════════════════════════════
#
# A mix of files: some only on dev, some only on draft, some on both.
# After switching, no file should have conflict markers.

make_temp_repo "switch-conflict-12"
init_repo

create_file "shared.txt" "Shared content"
create_file "dev-only.txt" "Dev-only content"
assert_success "add shared and dev-only" atomic add shared.txt dev-only.txt
record_change "Add shared and dev-only files" >/dev/null 2>&1 || true

new_view "draft" --draft --parent dev >/dev/null 2>&1 || true
switch_view "draft" >/dev/null 2>&1 || true

# Create a draft-only file
create_file "draft-only.txt" "Draft-only content"
assert_success "add draft-only" atomic add draft-only.txt
# Modify the shared file on draft
overwrite_file "shared.txt" "Shared content (modified by draft)"
record_change "Draft: add file and modify shared" >/dev/null 2>&1 || true

# Switch to dev — check all files
switch_view "dev" >/dev/null 2>&1 || true
assert_file_exists "shared.txt exists on dev" "shared.txt"
assert_file_exists "dev-only.txt exists on dev" "dev-only.txt"
assert_file_not_exists "draft-only.txt NOT on dev" "draft-only.txt"
assert_no_conflict_markers "no markers in shared.txt" "shared.txt"
assert_no_conflict_markers "no markers in dev-only.txt" "dev-only.txt"
assert_file_content "shared.txt has original" "shared.txt" "Shared content"

# Switch to draft — check all files
switch_view "draft" >/dev/null 2>&1 || true
assert_file_exists "shared.txt exists on draft" "shared.txt"
assert_file_exists "draft-only.txt exists on draft" "draft-only.txt"
assert_no_conflict_markers "no markers in shared.txt on draft" "shared.txt"
assert_no_conflict_markers "no markers in draft-only.txt" "draft-only.txt"
assert_file_content "shared.txt has draft version" "shared.txt" "Shared content (modified by draft)"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 13: Insert conflicting changes from two drafts into shared"
# ═══════════════════════════════════════════════════════════════════════════
#
# Two separate draft views each modify the same line of the same file.
# Insert both into the shared view.  After insert, the shared view should
# handle the conflict via CRDT resolution (auto-merge or markers) — but
# the key test is that switching BACK from a draft does not produce
# ADDITIONAL conflict markers or duplicate content.

make_temp_repo "switch-conflict-13"
init_repo

cat > data.json << 'EOF'
{
  "name": "TestProject",
  "version": "1.0.0",
  "description": "A test project"
}
EOF
assert_success "add data.json" atomic add data.json
record_change "Add data.json" >/dev/null 2>&1 || true

# Create two drafts
new_view "alpha" --draft --parent dev >/dev/null 2>&1 || true
new_view "beta" --draft --parent dev >/dev/null 2>&1 || true

# Alpha edits name
switch_view "alpha" >/dev/null 2>&1 || true
sed -i.bak 's/"TestProject"/"AlphaProject"/' data.json && rm -f data.json.bak
record_change "Alpha: rename project" >/dev/null 2>&1 || true

# Beta edits description (different line, clean merge expected)
switch_view "beta" >/dev/null 2>&1 || true
sed -i.bak 's/"A test project"/"Beta description"/' data.json && rm -f data.json.bak
record_change "Beta: update description" >/dev/null 2>&1 || true

# Switch to dev — clean before any inserts
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "dev clean before inserts" "data.json"

# Insert alpha's changes into dev
insert_from_view "alpha" "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers after alpha insert" "data.json"
assert_file_contains "dev has AlphaProject" "data.json" "AlphaProject"

# Insert beta's changes into dev
insert_from_view "beta" "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers after beta insert (different lines)" "data.json"
assert_file_contains "dev has AlphaProject (still)" "data.json" "AlphaProject"
assert_file_contains "dev has Beta description" "data.json" "Beta description"

# Save dev's merged state
DEV_AFTER_INSERTS="$(cat data.json)"

# Now switch to alpha, then back to dev — dev must stay clean
switch_view "alpha" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "dev clean after alpha round-trip" "data.json"
assert_file_content "dev state preserved after round-trip" "data.json" "$DEV_AFTER_INSERTS"

# Switch to beta, then back to dev — dev must stay clean
switch_view "beta" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "dev clean after beta round-trip" "data.json"
assert_file_content "dev state preserved (round 2)" "data.json" "$DEV_AFTER_INSERTS"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 14: Dev edits AFTER draft creation, switch to dev"
# ═══════════════════════════════════════════════════════════════════════════
#
# This is the exact scenario from the bug report:
#   1. Create draft from dev
#   2. Make changes on draft
#   3. Switch to dev (which hasn't changed since draft was created)
#   4. Make changes on dev
#   5. Switch FROM dev TO draft (draft inherits dev's new changes)
#   6. Switch BACK to dev — dev must be clean
#
# Step 6 is where the bug was reported: after visiting the draft (which
# has a merged/conflicted state), switching to dev should produce a
# clean build, not carry over the merged state.

make_temp_repo "switch-conflict-14"
init_repo

cat > module.ts << 'EOF'
interface Config {
  host: string;
  port: number;
  timeout: number;
  retries: number;
}

export function createConfig(): Config {
  return {
    host: "localhost",
    port: 3000,
    timeout: 5000,
    retries: 3,
  };
}

export function validateConfig(config: Config): boolean {
  return config.port > 0 && config.timeout > 0;
}
EOF
assert_success "add module.ts" atomic add module.ts
record_change "Add module.ts" >/dev/null 2>&1 || true

# Step 1: Create draft
new_view "agent-session" --draft --parent dev >/dev/null 2>&1 || true
switch_view "agent-session" >/dev/null 2>&1 || true

# Step 2: Draft changes port
sed -i.bak 's/port: 3000/port: 8080/' module.ts && rm -f module.ts.bak
record_change "Agent: change port to 8080" >/dev/null 2>&1 || true

# Step 3: Switch to dev (unchanged since draft creation)
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "dev clean (step 3)" "module.ts"
assert_file_contains "dev has port 3000" "module.ts" "port: 3000"

# Step 4: Dev changes timeout
sed -i.bak 's/timeout: 5000/timeout: 10000/' module.ts && rm -f module.ts.bak
record_change "Dev: change timeout to 10000" >/dev/null 2>&1 || true

# Step 5: Switch to draft — draft inherits dev's timeout change
switch_view "agent-session" >/dev/null 2>&1 || true
# Draft sees merged state (its own port + dev's timeout) — different lines, clean
assert_no_conflict_markers "draft merged cleanly (step 5)" "module.ts"
assert_file_contains "draft has port 8080 (own edit)" "module.ts" "port: 8080"
assert_file_contains "draft has timeout 10000 (inherited)" "module.ts" "timeout: 10000"

# Step 6: THE BUG TEST — switch back to dev, must be CLEAN
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "dev clean after visiting draft (step 6)" "module.ts"
assert_file_contains "dev has port 3000 (own, not draft's 8080)" "module.ts" "port: 3000"
assert_file_contains "dev has timeout 10000 (own edit)" "module.ts" "timeout: 10000"
assert_file_not_contains "dev does NOT have port 8080" "module.ts" "port: 8080"

# Extra: multiple round-trips to stress the cache
for i in 1 2 3; do
    switch_view "agent-session" >/dev/null 2>&1 || true
    switch_view "dev" >/dev/null 2>&1 || true
    assert_no_conflict_markers "dev clean after round-trip $i" "module.ts"
    assert_file_contains "dev has port 3000 (round $i)" "module.ts" "port: 3000"
    assert_file_not_contains "dev no 8080 (round $i)" "module.ts" "port: 8080"
done

# ═══════════════════════════════════════════════════════════════════════════

print_summary
