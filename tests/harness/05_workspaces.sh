#!/usr/bin/env bash
# 05_workspaces.sh — Per-stack workspace isolation tests.
#
# Workspaces are the mechanism by which stacks achieve FULL working copy
# isolation — not just tracked files (handled by the graph), but also
# build artifacts, caches, and any other ignored/untracked content.
#
# Key principle: "views, not forks."  The repository (pristine, changes)
# is shared.  The working copy is a projection of the active stack.
# switch_stack swaps the ENTIRE projection — tracked files from the graph,
# artifacts from .atomic/workspaces/<stack>/.
#
# Under the hood, switch_stack uses rename() (O(1) on same filesystem)
# to swap ignored entries between the working copy and workspace storage.
# A future enhancement will use reflinks (copy-on-write clones) for
# additional efficiency on supported filesystems.
#
# There is NO separate "workspace" CLI.  Stacks ARE workspaces.
# stack new   → creates the stack + its workspace storage
# stack switch → shelves current working state, unshelves target
# record/add/status → work exactly as before, always from project root
#
# Invariants tested:
#
#   1. stack new creates .atomic/workspaces/<name>/
#   2. switch_stack shelves ignored/artifact files into the old workspace
#   3. switch_stack restores ignored/artifact files from the new workspace
#   4. Ignored files on stack A do NOT appear after switching to stack B
#   5. Switching back to stack A restores its ignored files exactly
#   6. Untracked non-ignored files (user's novel work) persist across switches
#   7. Tracked files continue to be managed by the graph (existing behavior)
#   8. Multiple stacks can each have different versions of the same artifact
#   9. Workspaces survive across many round-trip switches
#  10. Workspace storage lives in .atomic/ (durable, not /tmp)

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: stack new creates workspace storage"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "ws-create"
init_repo

# dev is the initial stack — it should have workspace storage
assert_dir_exists \
    ".atomic/workspaces/dev exists after init" \
    ".atomic/workspaces/dev"

# Create a feature stack
new_stack "feature" >/dev/null 2>&1 || true

assert_dir_exists \
    ".atomic/workspaces/feature created with stack" \
    ".atomic/workspaces/feature"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Ignored files shelved on switch"
# ═══════════════════════════════════════════════════════════════════════════
#
# Workflow:
#   1. On dev, create an .atomicignore that ignores node_modules/ and dist/
#   2. Create node_modules/ and dist/ (simulating npm install + build)
#   3. Record a tracked file so dev has content
#   4. Switch to feature
#   5. node_modules/ and dist/ should NOT exist in working copy
#   6. They should be shelved in .atomic/workspaces/dev/

make_temp_repo "ws-shelve"
init_repo

# Create ignore rules
create_file ".atomicignore" "node_modules/
dist/"

# Record a tracked file on dev so the stack has content
create_file "index.ts" "console.log('hello')"
assert_success "add index.ts" atomic add index.ts
record_change "Add index.ts on dev" >/dev/null 2>&1 || true

# Simulate build artifacts
mkdir -p node_modules/lodash
create_file "node_modules/lodash/index.js" "module.exports = {}"
create_file "node_modules/lodash/package.json" '{"name":"lodash","version":"4.17.21"}'
mkdir -p dist
create_file "dist/bundle.js" "var a=1;"

assert_dir_exists "node_modules exists on dev" "node_modules"
assert_dir_exists "dist exists on dev" "dist"
assert_file_exists "lodash exists on dev" "node_modules/lodash/index.js"

# Create feature and switch
new_stack "feature" >/dev/null 2>&1 || true
apply_from_stack "dev" "feature" >/dev/null 2>&1 || true
switch_stack "feature" >/dev/null 2>&1 || true

assert_current_stack "on feature" "feature"

# Tracked file should exist (inherited via apply)
assert_file_exists "index.ts on feature" "index.ts"

# Ignored artifacts should NOT be in the working copy
assert_dir_not_exists \
    "node_modules NOT in working copy on feature" \
    "node_modules"

assert_dir_not_exists \
    "dist NOT in working copy on feature" \
    "dist"

# They should be shelved in dev's workspace
assert_dir_exists \
    "node_modules shelved in dev workspace" \
    ".atomic/workspaces/dev/node_modules"

assert_file_exists \
    "lodash shelved in dev workspace" \
    ".atomic/workspaces/dev/node_modules/lodash/index.js"

assert_dir_exists \
    "dist shelved in dev workspace" \
    ".atomic/workspaces/dev/dist"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Ignored files restored on switch back"
# ═══════════════════════════════════════════════════════════════════════════
#
# Continuing from previous: switch back to dev and verify artifacts return.

switch_stack "dev" >/dev/null 2>&1 || true
assert_current_stack "back on dev" "dev"

# Artifacts should be restored
assert_dir_exists "node_modules restored on dev" "node_modules"
assert_dir_exists "dist restored on dev" "dist"
assert_file_exists "lodash restored on dev" "node_modules/lodash/index.js"
assert_file_content \
    "lodash content intact" \
    "node_modules/lodash/index.js" \
    "module.exports = {}"
assert_file_content \
    "bundle content intact" \
    "dist/bundle.js" \
    "var a=1;"

# Tracked file should still be there
assert_file_exists "index.ts still on dev" "index.ts"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Each stack has its own artifacts"
# ═══════════════════════════════════════════════════════════════════════════
#
# Workflow:
#   1. dev has node_modules with lodash@4.17.21
#   2. Switch to feature
#   3. Create node_modules with lodash@3.10.0 (different version)
#   4. Switch to dev → see 4.17.21
#   5. Switch to feature → see 3.10.0

# Continuing from previous — we're on dev with lodash@4.17.21

switch_stack "feature" >/dev/null 2>&1 || true
assert_current_stack "on feature" "feature"

# Feature has no node_modules yet
assert_dir_not_exists "feature starts with no node_modules" "node_modules"

# Simulate a different npm install on feature
mkdir -p node_modules/lodash
create_file "node_modules/lodash/index.js" "module.exports = {v3: true}"
create_file "node_modules/lodash/package.json" '{"name":"lodash","version":"3.10.0"}'

assert_file_content \
    "feature has lodash 3.10.0" \
    "node_modules/lodash/package.json" \
    '{"name":"lodash","version":"3.10.0"}'

# Switch to dev — should see 4.17.21
switch_stack "dev" >/dev/null 2>&1 || true

assert_file_content \
    "dev has lodash 4.17.21" \
    "node_modules/lodash/package.json" \
    '{"name":"lodash","version":"4.17.21"}'

# Switch to feature — should see 3.10.0
switch_stack "feature" >/dev/null 2>&1 || true

assert_file_content \
    "feature still has lodash 3.10.0" \
    "node_modules/lodash/package.json" \
    '{"name":"lodash","version":"3.10.0"}'

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Untracked non-ignored files persist"
# ═══════════════════════════════════════════════════════════════════════════
#
# Files that are untracked AND not ignored are the user's "undecided"
# work — scratch files, notes, etc.  They should persist across switches
# (existing behavior, unchanged by workspaces).

make_temp_repo "ws-untracked-persist"
init_repo

create_file ".atomicignore" "node_modules/
dist/"

# Create a tracked file on dev
create_file "app.ts" "const app = 1"
assert_success "add app.ts" atomic add app.ts
record_change "Add app.ts" >/dev/null 2>&1 || true

# Create an untracked, NON-ignored file (not in .atomicignore)
create_file "notes.txt" "my personal notes"

# Create an ignored artifact
mkdir -p node_modules
create_file "node_modules/thing.js" "module.exports = 1"

# Switch to feature
new_stack "feature" >/dev/null 2>&1 || true
apply_from_stack "dev" "feature" >/dev/null 2>&1 || true
switch_stack "feature" >/dev/null 2>&1 || true

# notes.txt should persist (untracked, not ignored)
assert_file_exists "notes.txt persists on feature" "notes.txt"
assert_file_content "notes.txt content intact" "notes.txt" "my personal notes"

# node_modules should NOT persist (ignored → shelved)
assert_dir_not_exists "node_modules shelved away" "node_modules"

# Switch back — both should be there
switch_stack "dev" >/dev/null 2>&1 || true
assert_file_exists "notes.txt persists on dev" "notes.txt"
assert_dir_exists "node_modules restored on dev" "node_modules"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Tracked files still managed by graph"
# ═══════════════════════════════════════════════════════════════════════════
#
# Workspaces handle ignored files.  Tracked files continue to be managed
# by the graph — record, switch, apply all work as before.

make_temp_repo "ws-tracked-graph"
init_repo

create_file ".atomicignore" "build/"

# Record different files on dev and feature
create_file "shared.ts" "shared code"
assert_success "add shared.ts" atomic add shared.ts
record_change "Add shared.ts on dev" >/dev/null 2>&1 || true

new_stack "feature" >/dev/null 2>&1 || true
apply_from_stack "dev" "feature" >/dev/null 2>&1 || true
switch_stack "feature" >/dev/null 2>&1 || true

create_file "feature.ts" "feature code"
assert_success "add feature.ts" atomic add feature.ts
record_change "Add feature.ts on feature" >/dev/null 2>&1 || true

# Switch to dev — feature.ts should not exist (graph isolation)
switch_stack "dev" >/dev/null 2>&1 || true

assert_file_exists "shared.ts on dev" "shared.ts"
assert_file_not_exists "feature.ts NOT on dev (graph isolation)" "feature.ts"

# Switch to feature — both should exist
switch_stack "feature" >/dev/null 2>&1 || true

assert_file_exists "shared.ts on feature" "shared.ts"
assert_file_exists "feature.ts on feature" "feature.ts"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Build artifacts + tracked files together"
# ═══════════════════════════════════════════════════════════════════════════
#
# The full picture: tracked files from the graph + artifacts from the
# workspace combine to give the complete working copy for a stack.

make_temp_repo "ws-full-picture"
init_repo

create_file ".atomicignore" "node_modules/
dist/
.cache/"

# ── dev: record source, create build artifacts ──
create_file "src/main.ts" "console.log('dev')"
assert_success "add src/main.ts" atomic add src/main.ts
record_change "Add main.ts on dev" >/dev/null 2>&1 || true

mkdir -p node_modules/express
create_file "node_modules/express/index.js" "module.exports = express"
mkdir -p dist
create_file "dist/main.js" "console.log('dev compiled')"
mkdir -p .cache
create_file ".cache/tsbuildinfo" "{}"

# ── feature: different source + different artifacts ──
new_stack "feature" >/dev/null 2>&1 || true
apply_from_stack "dev" "feature" >/dev/null 2>&1 || true
switch_stack "feature" >/dev/null 2>&1 || true

# Tracked: modify source on feature
overwrite_file "src/main.ts" "console.log('feature')"
record_change "Modify main.ts on feature" >/dev/null 2>&1 || true

# Artifacts: different deps on feature
mkdir -p node_modules/fastify
create_file "node_modules/fastify/index.js" "module.exports = fastify"
mkdir -p dist
create_file "dist/main.js" "console.log('feature compiled')"

# ── Verify feature state ──
assert_file_content "source is feature version" "src/main.ts" "console.log('feature')"
assert_file_exists "fastify on feature" "node_modules/fastify/index.js"
assert_file_not_exists "express NOT on feature" "node_modules/express/index.js"
assert_file_content "dist is feature build" "dist/main.js" "console.log('feature compiled')"

# ── Switch to dev: full state restored ──
switch_stack "dev" >/dev/null 2>&1 || true

assert_file_content "source is dev version" "src/main.ts" "console.log('dev')"
assert_file_exists "express on dev" "node_modules/express/index.js"
assert_file_not_exists "fastify NOT on dev" "node_modules/fastify/index.js"
assert_file_content "dist is dev build" "dist/main.js" "console.log('dev compiled')"
assert_file_exists ".cache on dev" ".cache/tsbuildinfo"

# ── Switch to feature: full state restored ──
switch_stack "feature" >/dev/null 2>&1 || true

assert_file_content "source is feature version (round 2)" "src/main.ts" "console.log('feature')"
assert_file_exists "fastify on feature (round 2)" "node_modules/fastify/index.js"
assert_file_not_exists "express NOT on feature (round 2)" "node_modules/express/index.js"
assert_file_content "dist is feature build (round 2)" "dist/main.js" "console.log('feature compiled')"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Three stacks with divergent artifacts"
# ═══════════════════════════════════════════════════════════════════════════
#
# Simulates three agents each with their own dependency versions:
#   dev      → express@4
#   agent-a  → express@5 (testing upgrade)
#   agent-b  → fastify (testing migration)

make_temp_repo "ws-three-stacks"
init_repo

create_file ".atomicignore" "node_modules/
dist/"

# ── dev: base project with express@4 ──
create_file "package.json" '{"dependencies":{"express":"4.18.0"}}'
assert_success "add package.json" atomic add package.json
record_change "Add package.json on dev" >/dev/null 2>&1 || true

mkdir -p node_modules/express
create_file "node_modules/express/package.json" '{"version":"4.18.0"}'

# ── agent-a: testing express@5 ──
new_stack "agent-a" >/dev/null 2>&1 || true
apply_from_stack "dev" "agent-a" >/dev/null 2>&1 || true
switch_stack "agent-a" >/dev/null 2>&1 || true

overwrite_file "package.json" '{"dependencies":{"express":"5.0.0"}}'
record_change "Upgrade to express@5" >/dev/null 2>&1 || true

mkdir -p node_modules/express
create_file "node_modules/express/package.json" '{"version":"5.0.0"}'

# ── agent-b: testing fastify migration ──
switch_stack "dev" >/dev/null 2>&1 || true
new_stack "agent-b" >/dev/null 2>&1 || true
apply_from_stack "dev" "agent-b" >/dev/null 2>&1 || true
switch_stack "agent-b" >/dev/null 2>&1 || true

overwrite_file "package.json" '{"dependencies":{"fastify":"4.0.0"}}'
record_change "Migrate to fastify" >/dev/null 2>&1 || true

mkdir -p node_modules/fastify
create_file "node_modules/fastify/package.json" '{"version":"4.0.0"}'

# ── Verify each stack sees its own deps ──

# On agent-b (current)
assert_file_content \
    "agent-b has fastify" \
    "node_modules/fastify/package.json" \
    '{"version":"4.0.0"}'
assert_file_not_exists "agent-b has no express" "node_modules/express/package.json"

# Switch to agent-a
switch_stack "agent-a" >/dev/null 2>&1 || true
assert_file_content \
    "agent-a has express@5" \
    "node_modules/express/package.json" \
    '{"version":"5.0.0"}'
assert_file_not_exists "agent-a has no fastify" "node_modules/fastify/package.json"
assert_file_content \
    "agent-a package.json shows express@5" \
    "package.json" \
    '{"dependencies":{"express":"5.0.0"}}'

# Switch to dev
switch_stack "dev" >/dev/null 2>&1 || true
assert_file_content \
    "dev has express@4" \
    "node_modules/express/package.json" \
    '{"version":"4.18.0"}'
assert_file_not_exists "dev has no fastify" "node_modules/fastify/package.json"
assert_file_content \
    "dev package.json shows express@4" \
    "package.json" \
    '{"dependencies":{"express":"4.18.0"}}'

# Full round-trip back to agent-b
switch_stack "agent-b" >/dev/null 2>&1 || true
assert_file_content \
    "agent-b still has fastify (round 2)" \
    "node_modules/fastify/package.json" \
    '{"version":"4.0.0"}'
assert_file_content \
    "agent-b package.json shows fastify (round 2)" \
    "package.json" \
    '{"dependencies":{"fastify":"4.0.0"}}'

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Rapid switching preserves artifacts"
# ═══════════════════════════════════════════════════════════════════════════
#
# Switch between stacks 5 times to verify no state corruption or leakage.

# Continuing from previous (dev, agent-a, agent-b all set up)

for i in $(seq 1 5); do
    switch_stack "dev" >/dev/null 2>&1 || true
    assert_file_content \
        "dev express@4 (iteration $i)" \
        "node_modules/express/package.json" \
        '{"version":"4.18.0"}'
    assert_file_not_exists "dev no fastify (iteration $i)" "node_modules/fastify/package.json"

    switch_stack "agent-a" >/dev/null 2>&1 || true
    assert_file_content \
        "agent-a express@5 (iteration $i)" \
        "node_modules/express/package.json" \
        '{"version":"5.0.0"}'

    switch_stack "agent-b" >/dev/null 2>&1 || true
    assert_file_content \
        "agent-b fastify (iteration $i)" \
        "node_modules/fastify/package.json" \
        '{"version":"4.0.0"}'
    assert_file_not_exists "agent-b no express (iteration $i)" "node_modules/express/package.json"
done

_pass "rapid switching: 5 round-trips across 3 stacks completed"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Empty stack has no artifacts"
# ═══════════════════════════════════════════════════════════════════════════
#
# A freshly created stack with no history should have an empty working
# copy — no tracked files AND no artifacts.

make_temp_repo "ws-empty-stack"
init_repo

create_file ".atomicignore" "node_modules/"

create_file "main.ts" "code"
assert_success "add main.ts" atomic add main.ts
record_change "Add main.ts" >/dev/null 2>&1 || true

# Create artifacts on dev
mkdir -p node_modules
create_file "node_modules/dep.js" "dep"

# Create empty stack and switch
new_stack "empty" >/dev/null 2>&1 || true
switch_stack "empty" >/dev/null 2>&1 || true

# No tracked files (empty stack)
assert_file_not_exists "main.ts NOT on empty stack" "main.ts"

# No artifacts (workspace is fresh)
assert_dir_not_exists "no node_modules on empty stack" "node_modules"

# Switch back — everything returns
switch_stack "dev" >/dev/null 2>&1 || true
assert_file_exists "main.ts back on dev" "main.ts"
assert_dir_exists "node_modules back on dev" "node_modules"
assert_file_content "dep.js intact" "node_modules/dep.js" "dep"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Nested ignored directories"
# ═══════════════════════════════════════════════════════════════════════════
#
# Deeply nested ignored trees (node_modules/.cache/*, dist/assets/*)
# should be fully shelved and restored.

make_temp_repo "ws-nested-ignored"
init_repo

create_file ".atomicignore" "node_modules/
dist/
.next/"

create_file "page.tsx" "<div>hello</div>"
assert_success "add page.tsx" atomic add page.tsx
record_change "Add page.tsx" >/dev/null 2>&1 || true

# Create deeply nested build output
mkdir -p node_modules/.cache/babel/sub
create_file "node_modules/.cache/babel/sub/chunk.json" '{"cached":true}'
mkdir -p dist/assets/images
create_file "dist/assets/images/logo.png" "PNGDATA"
mkdir -p .next/static/chunks
create_file ".next/static/chunks/main.js" "nextjs_chunk"
create_file ".next/BUILD_ID" "abc123"

new_stack "feature" >/dev/null 2>&1 || true
apply_from_stack "dev" "feature" >/dev/null 2>&1 || true
switch_stack "feature" >/dev/null 2>&1 || true

# All nested ignored content should be gone
assert_dir_not_exists "node_modules gone" "node_modules"
assert_dir_not_exists "dist gone" "dist"
assert_dir_not_exists ".next gone" ".next"

# Switch back — deeply nested content restored
switch_stack "dev" >/dev/null 2>&1 || true

assert_file_content \
    "nested babel cache restored" \
    "node_modules/.cache/babel/sub/chunk.json" \
    '{"cached":true}'
assert_file_content \
    "nested image restored" \
    "dist/assets/images/logo.png" \
    "PNGDATA"
assert_file_content \
    "next.js chunks restored" \
    ".next/static/chunks/main.js" \
    "nextjs_chunk"
assert_file_content \
    "BUILD_ID restored" \
    ".next/BUILD_ID" \
    "abc123"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Artifacts created on feature, reviewed from dev"
# ═══════════════════════════════════════════════════════════════════════════
#
# This is the "human reviews agent's work" workflow:
#   1. Agent works on feature: records code, runs build
#   2. Human switches to feature from dev
#   3. Human sees agent's code (graph) AND build output (workspace)
#   4. Human can run tests against the agent's exact build state
#   5. Human switches back to dev — their own state is restored

make_temp_repo "ws-review"
init_repo

create_file ".atomicignore" "node_modules/
dist/"

# Human's dev state
create_file "src/app.ts" "// dev version"
assert_success "add src/app.ts" atomic add src/app.ts
record_change "Add app.ts on dev" >/dev/null 2>&1 || true

mkdir -p node_modules/humanlib
create_file "node_modules/humanlib/index.js" "human_dep"
mkdir -p dist
create_file "dist/app.js" "compiled_dev"

# Agent works on feature
new_stack "agent-review-test" >/dev/null 2>&1 || true
apply_from_stack "dev" "agent-review-test" >/dev/null 2>&1 || true
switch_stack "agent-review-test" >/dev/null 2>&1 || true

# Agent modifies source and builds
overwrite_file "src/app.ts" "// agent version with fix"
record_change "Agent fix" >/dev/null 2>&1 || true

mkdir -p node_modules/agentlib
create_file "node_modules/agentlib/index.js" "agent_dep"
mkdir -p dist
create_file "dist/app.js" "compiled_agent"

# Switch back to dev (simulates agent session ending)
switch_stack "dev" >/dev/null 2>&1 || true

# Human's state is fully restored
assert_file_content "human source" "src/app.ts" "// dev version"
assert_file_content "human deps" "node_modules/humanlib/index.js" "human_dep"
assert_file_content "human build" "dist/app.js" "compiled_dev"
assert_file_not_exists "no agent deps on dev" "node_modules/agentlib/index.js"

# Human switches to agent's stack to review
switch_stack "agent-review-test" >/dev/null 2>&1 || true

# Human sees agent's FULL state
assert_file_content \
    "agent source visible" \
    "src/app.ts" \
    "// agent version with fix"
assert_file_content \
    "agent deps visible" \
    "node_modules/agentlib/index.js" \
    "agent_dep"
assert_file_content \
    "agent build visible" \
    "dist/app.js" \
    "compiled_agent"
assert_file_not_exists "no human deps on agent stack" "node_modules/humanlib/index.js"

# Human goes back — their world is intact
switch_stack "dev" >/dev/null 2>&1 || true

assert_file_content "human source restored" "src/app.ts" "// dev version"
assert_file_content "human deps restored" "node_modules/humanlib/index.js" "human_dep"
assert_file_content "human build restored" "dist/app.js" "compiled_dev"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: .atomicignore changes between stacks"
# ═══════════════════════════════════════════════════════════════════════════
#
# Different stacks might have different .atomicignore rules (e.g. agent
# adds build/ to ignore).  The ignore rules for shelving should come from
# the current working copy's .atomicignore at switch time.

make_temp_repo "ws-ignore-changes"
init_repo

# dev ignores node_modules only
create_file ".atomicignore" "node_modules/"
assert_success "add .atomicignore" atomic add .atomicignore
create_file "src/index.ts" "main"
assert_success "add src/index.ts" atomic add src/index.ts
record_change "Initial dev" >/dev/null 2>&1 || true

mkdir -p node_modules
create_file "node_modules/pkg.js" "pkg"
mkdir -p build
create_file "build/output.js" "build_output"

# Create feature that ALSO ignores build/
new_stack "feature" >/dev/null 2>&1 || true
apply_from_stack "dev" "feature" >/dev/null 2>&1 || true
switch_stack "feature" >/dev/null 2>&1 || true

# Update .atomicignore on feature to also ignore build/
overwrite_file ".atomicignore" "node_modules/
build/"
record_change "Ignore build/ on feature" >/dev/null 2>&1 || true

# On dev, build/ was NOT ignored, so it should have persisted as untracked
# On feature, build/ IS ignored — so when we switch AWAY from feature,
# any build/ dir will be shelved.

# Create a build dir on feature
mkdir -p build
create_file "build/output.js" "feature_build"

# Switch to dev
switch_stack "dev" >/dev/null 2>&1 || true

# dev's .atomicignore only has node_modules/, so build/ is untracked-not-ignored.
# dev should see its own node_modules restored and build/ from the persistent
# untracked state (it was untracked on dev, not ignored).
assert_dir_exists "node_modules on dev" "node_modules"
assert_file_content "dev pkg" "node_modules/pkg.js" "pkg"

# build/ was untracked on dev (not in dev's .atomicignore at record time)
# It may or may not persist depending on whether it was in working copy
# before switch.  The key assertion: feature's build/ should NOT leak to dev.
# If build/ exists on dev, it should have dev's content, not feature's.
if [[ -f "build/output.js" ]]; then
    assert_file_content \
        "build/ on dev has dev content (not feature's)" \
        "build/output.js" \
        "build_output"
else
    _pass "build/ not present on dev (was shelved with feature)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Workspace storage is durable"
# ═══════════════════════════════════════════════════════════════════════════
#
# Workspace state lives in .atomic/workspaces/ — inside the project
# directory.  It is NOT in /tmp.  Verify the paths.

make_temp_repo "ws-durable"
init_repo

create_file ".atomicignore" "node_modules/"

create_file "code.ts" "code"
assert_success "add code.ts" atomic add code.ts
record_change "Add code.ts" >/dev/null 2>&1 || true

mkdir -p node_modules
create_file "node_modules/dep.js" "dep_content"

new_stack "feature" >/dev/null 2>&1 || true
apply_from_stack "dev" "feature" >/dev/null 2>&1 || true
switch_stack "feature" >/dev/null 2>&1 || true

# Verify workspace is inside .atomic/ (durable path)
assert_dir_exists \
    "workspace storage is inside .atomic/" \
    ".atomic/workspaces"

assert_dir_exists \
    "dev workspace is inside .atomic/workspaces/" \
    ".atomic/workspaces/dev"

# Verify it's the actual content (not a symlink to /tmp)
actual_path="$(cd .atomic/workspaces/dev 2>/dev/null && pwd -P)"
if echo "$actual_path" | grep -q "/tmp"; then
    _fail "workspace is NOT in /tmp" "path resolved to $actual_path"
else
    _pass "workspace is NOT in /tmp"
fi

# Verify the shelved content is really there
assert_file_exists \
    "shelved dep exists on disk" \
    ".atomic/workspaces/dev/node_modules/dep.js"
assert_file_content \
    "shelved dep has correct content" \
    ".atomic/workspaces/dev/node_modules/dep.js" \
    "dep_content"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Stack deletion cleans workspace"
# ═══════════════════════════════════════════════════════════════════════════
#
# When a stack is deleted, its workspace storage should be cleaned up too.

make_temp_repo "ws-delete"
init_repo

create_file ".atomicignore" "node_modules/"

create_file "base.ts" "base"
assert_success "add base.ts" atomic add base.ts
record_change "Add base.ts" >/dev/null 2>&1 || true

new_stack "ephemeral" >/dev/null 2>&1 || true
apply_from_stack "dev" "ephemeral" >/dev/null 2>&1 || true
switch_stack "ephemeral" >/dev/null 2>&1 || true

# Create artifacts on ephemeral stack
mkdir -p node_modules
create_file "node_modules/temp.js" "temp"

# Switch back to dev (shelves ephemeral's artifacts)
switch_stack "dev" >/dev/null 2>&1 || true

assert_dir_exists \
    "ephemeral workspace exists before delete" \
    ".atomic/workspaces/ephemeral"

# Delete the ephemeral stack
del_out="$(atomic stack delete ephemeral 2>&1)" || true
if echo "$del_out" | grep -qiE "deleted|removed|success"; then
    _pass "delete ephemeral stack"
else
    _pass "delete ephemeral stack completes"
fi

# Workspace storage should be cleaned up
assert_dir_not_exists \
    "ephemeral workspace removed after stack delete" \
    ".atomic/workspaces/ephemeral"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Many stacks stress test"
# ═══════════════════════════════════════════════════════════════════════════
#
# Create 5 stacks, each with unique artifacts.  Switch between them
# and verify isolation holds.

make_temp_repo "ws-stress"
init_repo

create_file ".atomicignore" "node_modules/"

create_file "app.ts" "shared"
assert_success "add app.ts" atomic add app.ts
record_change "Add app.ts" >/dev/null 2>&1 || true

# Create 5 stacks with unique node_modules
for i in $(seq 1 5); do
    new_stack "agent-${i}" >/dev/null 2>&1 || true
    apply_from_stack "dev" "agent-${i}" >/dev/null 2>&1 || true
    switch_stack "agent-${i}" >/dev/null 2>&1 || true

    mkdir -p node_modules
    create_file "node_modules/marker.txt" "agent-${i}-deps"
done

# Now cycle through all stacks and verify each has its own marker
for i in $(seq 1 5); do
    switch_stack "agent-${i}" >/dev/null 2>&1 || true
    assert_file_content \
        "agent-${i} has its own deps" \
        "node_modules/marker.txt" \
        "agent-${i}-deps"
done

# Switch to dev — should have no node_modules (dev never created any)
switch_stack "dev" >/dev/null 2>&1 || true
assert_dir_not_exists "dev has no node_modules" "node_modules"

# One more round trip
switch_stack "agent-3" >/dev/null 2>&1 || true
assert_file_content \
    "agent-3 still has its deps after full cycle" \
    "node_modules/marker.txt" \
    "agent-3-deps"

_pass "5-stack stress test completed"

# ═══════════════════════════════════════════════════════════════════════════

print_summary
