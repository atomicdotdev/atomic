#!/usr/bin/env bash
# 25_agent_provenance.sh — Agent provenance gap tests.
#
# End-to-end coverage for the four agent-provenance gaps (handoff doc:
# noname/docs/agent-provenance-gaps.md), driving real hook invocations
# against the CLI:
#
#   #1 llm_response nodes are emitted into the session graph
#   #2 change.unhashed["agent_turn"] is populated AND persisted
#   #3 opencode sessions get a transcript_path (synthesized from
#      OpenCode's SQLite store; seeded fake store here)
#   #4 reasoning_text lands on the change provenance (and tokens/cost/
#      finish_reason/step_count come along)
#
# Two scenarios:
#   A. opencode — thin stop payload (no reasoning/tokens/response),
#      everything recovered from the fake OpenCode store.
#   B. claude-code — session starts WITHOUT transcript_path; the Stop
#      hook carries it (the timing fix), and the agent's response is
#      derived from the transcript.
#
# Prerequisite: Python with sqlite3. Suite skips (exit 77) if absent.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

for tool in python3; do
    if ! command -v "$tool" &>/dev/null; then
        skip_suite "$tool not installed"
    fi
done

if ! python3 -c 'import sqlite3' >/dev/null 2>&1; then
    skip_suite "Python sqlite3 module unavailable"
fi

run_hook() {
    local payload="$1"; shift
    printf '%s\n' "$payload" | "$@"
}

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Agent provenance: opencode (thin payload + store recovery)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "agent-prov-oc"
init_repo
# macOS: mktemp lands under /var → sessions record the canonical
# /private/var path. Work with the physical path everywhere.
REPO_DIR="$(pwd -P)"

# Fake OpenCode data dir with a minimal schema — just what the reader
# queries: session.directory, message(id, data), part(message_id, data).
OC_HOME="$(mktemp -d "${TMPDIR:-/tmp}/atomic-opencode-fixture-XXXXXX")"
_HARNESS_TMPDIRS+=("$OC_HOME")
NOW_MS=$(( $(date +%s) * 1000 ))
USER_MS=$NOW_MS
ASST_MS=$(( NOW_MS + 5000 ))

require_success "create isolated OpenCode fixture" python3 -c '
import json, sqlite3, sys
db=sqlite3.connect(sys.argv[1]); root=sys.argv[2]; user=int(sys.argv[3]); assistant=int(sys.argv[4])
db.executescript("CREATE TABLE session (id text PRIMARY KEY, project_id text, directory text NOT NULL, time_created integer, time_updated integer); CREATE TABLE message (id text PRIMARY KEY, session_id text, time_created integer, time_updated integer, data text); CREATE TABLE part (id text PRIMARY KEY, message_id text, session_id text, time_created integer, time_updated integer, data text);")
db.execute("INSERT INTO session VALUES (?,?,?,?,?)", ("ses_harness", "prj_x", root, user, assistant))
for mid, at, role in [("msg_u",user,"user"),("msg_a",assistant,"assistant")]: db.execute("INSERT INTO message VALUES (?,?,?,?,?)", (mid,"ses_harness",at,at,json.dumps({"role":role})))
parts=[("p1","msg_u",user,{"type":"text","text":"harness: fix the widget"}),("p2","msg_a",assistant+100,{"type":"reasoning","text":"widget reasoning","time":{"start":10,"end":60}}),("p3","msg_a",assistant+200,{"type":"step-start"}),("p4","msg_a",assistant+300,{"type":"step-finish","reason":"stop","cost":0.02,"tokens":{"input":40,"output":50,"reasoning":10,"cache":{"write":5,"read":20}}}),("p5","msg_a",assistant+400,{"type":"tool","tool":"bash","state":{"title":"ran tests","status":"completed"}}),("p6","msg_a",assistant+450,{"type":"tool","tool":"bash","callID":"call_h1","state":{"status":"completed","input":{"command":"cargo test"},"output":"test result: ok. 12 passed","title":"cargo test"}}),("p7","msg_a",assistant+500,{"type":"text","text":"The widget is fixed."})]
for pid,mid,at,data in parts: db.execute("INSERT INTO part VALUES (?,?,?,?,?,?)",(pid,mid,"ses_harness",at,at,json.dumps(data)))
db.commit(); db.close()' "$OC_HOME/opencode.db" "$REPO_DIR" "$USER_MS" "$ASST_MS"

# Drive the hook sequence with a THIN stop payload — what the old plugin
# sends: only session/model metadata.
require_success "start OpenCode session" run_hook '{"session_id":"ses_harness","source":"startup","cwd":"'"$REPO_DIR"'"}' env OPENCODE_HOME="$OC_HOME" "$ATOMIC_BIN" agent hooks opencode session-start
require_success "submit OpenCode prompt" run_hook '{"session_id":"ses_harness","prompt":"harness: fix the widget","cwd":"'"$REPO_DIR"'"}' env OPENCODE_HOME="$OC_HOME" "$ATOMIC_BIN" agent hooks opencode user-prompt

create_file "widget.txt" "widget v1"
# A tool hook carrying the same call id as the store's tool part — the join
# key the enrichment uses to graft commands/outputs onto graph nodes.
require_success "record OpenCode tool event" run_hook '{"session_id":"ses_harness","tool_name":"bash","tool_call_id":"call_h1","status":"completed","cwd":"'"$REPO_DIR"'"}' env OPENCODE_HOME="$OC_HOME" "$ATOMIC_BIN" agent hooks opencode after-tool

require_success "finalize OpenCode turn" run_hook '{"session_id":"ses_harness","turn_number":1,"model":"test/model","provider":"testprovider","cwd":"'"$REPO_DIR"'"}' env OPENCODE_HOME="$OC_HOME" "$ATOMIC_BIN" agent hooks opencode stop
if [[ -f ".atomic/sessions/ses_harness.json" ]]; then
    _pass "opencode turn recorded a session"
else
    _fail "opencode turn recorded a session" "no session file"
fi

# #3 — transcript synthesized from the store, path set on the session.
assert_file_exists "synthesized opencode transcript exists" \
    ".atomic/sessions/ses_harness/opencode-transcript.jsonl"
tp="$(python3 -c "import json; print(json.load(open('.atomic/sessions/ses_harness.json')).get('transcript_path') or '')")"
if [[ "$tp" == "$REPO_DIR/.atomic/sessions/ses_harness/opencode-transcript.jsonl" ]]; then
    _pass "session transcript_path points at the synthesized transcript"
else
    _fail "session transcript_path points at the synthesized transcript" "got: $tp"
fi

# Finalized history remains readable through the immutable session ledger.
require_capture "read OpenCode change log" atomic log -f json
HASH="$(printf '%s\n' "$COMMAND_OUTPUT" | python3 -c 'import json,sys; entries=json.load(sys.stdin); assert entries; print(entries[0]["hash"])')"
require_capture "read OpenCode session ledger" atomic session show ses_harness --json
ledger_json="$COMMAND_OUTPUT"
if echo "$ledger_json" | python3 -c "import json,sys; d=json.load(sys.stdin); assert len(d[1]) == 1; assert d[1][0]['provenance_hash']"; then
    _pass "finalized provenance is readable from the session ledger"
else
    _fail "finalized provenance is readable from the session ledger"
fi
assert_file_not_exists "graph.json removed after owner-journal cutover" ".atomic/sessions/ses_harness/graph.json"
assert_file_not_exists "graph.lock removed after owner-journal cutover" ".atomic/sessions/ses_harness/graph.lock"

# #4 (change side) — reasoning_text, tokens, cost, steps on the provenance.
require_capture "read recorded OpenCode change" atomic change "$HASH" -f json
prov_json="$COMMAND_OUTPUT"
if echo "$prov_json" | python3 -c "
import json, sys
p = json.load(sys.stdin)['provenance']
assert p.get('reasoning_text') == 'widget reasoning', p.get('reasoning_text')

assert p.get('finish_reason') == 'stop'
assert p.get('step_count') == 1
assert (p.get('tokens') or {}).get('input') == 40
assert (p.get('cost') or {}).get('amount_micros') == 20000
"; then
    _pass "change provenance carries reasoning_text/tokens/cost/steps"
else
    _fail "change provenance carries reasoning_text/tokens/cost/steps" \
        "$(echo "$prov_json" | head -3)"
fi


# #2 — agent_turn persisted inside the change file (unhashed section).
change_file=".atomic/changes/${HASH:0:2}/${HASH}.change"
assert_file_exists "OpenCode change object exists" "$change_file"
if echo "$prov_json" | python3 -c "
import json, sys
t = (json.load(sys.stdin).get('unhashed') or {}).get('agent_turn')
assert t, 'no agent_turn key'
types = [e['entry_type'] for e in t['condensed_transcript']]
assert 'user' in types and 'assistant' in types and 'tool' in types, types
"; then
    _pass "agent_turn with condensed transcript persisted on the change"
else
    _fail "agent_turn with condensed transcript persisted on the change"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Agent provenance: claude-code (transcript_path timing + fallback)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "agent-prov-cc"
init_repo
REPO_DIR="$(pwd -P)"

CC_FIXTURE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/atomic-claude-fixture-XXXXXX")"
_HARNESS_TMPDIRS+=("$CC_FIXTURE_DIR")
TRANSCRIPT="$CC_FIXTURE_DIR/cc-transcript.jsonl"
printf '%s\n' \
    '{"type":"user","uuid":"u1","message":{"role":"user","content":[{"type":"text","text":"patch the parser"}]}}' \
    '{"type":"assistant","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"Patching now."},{"type":"tool_use","name":"Edit","input":{"file_path":"parser.rs"}}]}}' \
    '{"type":"assistant","uuid":"a2","message":{"role":"assistant","content":[{"type":"text","text":"The parser is patched and verified."}]}}' > "$TRANSCRIPT"

# Deliberately NO transcript_path at session start / prompt — the timing
# gap. The Stop hook carries it.
require_success "start Claude session" run_hook '{"session_id":"cc-harness","cwd":"'"$REPO_DIR"'"}' "$ATOMIC_BIN" agent hooks claude-code session-start
require_success "submit Claude prompt" run_hook '{"session_id":"cc-harness","prompt":"patch the parser","cwd":"'"$REPO_DIR"'"}' "$ATOMIC_BIN" agent hooks claude-code user-prompt-submit

require_capture "read Claude session before stop" python3 -c "import json; print(json.load(open('.atomic/sessions/cc-harness.json')).get('transcript_path'))"
before="$COMMAND_OUTPUT"
if [[ "$before" == "None" || -z "$before" ]]; then
    _pass "session has no transcript_path before Stop (the gap scenario)"
else
    _fail "session has no transcript_path before Stop (the gap scenario)" "got: $before"
fi

create_file "parser.rs" "fn main() {}"

require_success "finalize Claude turn" run_hook '{"session_id":"cc-harness","transcript_path":"'"$TRANSCRIPT"'","cwd":"'"$REPO_DIR"'"}' "$ATOMIC_BIN" agent hooks claude-code stop

# #2 (timing fix) — transcript_path applied from the Stop event.
after="$(python3 -c "import json; print(json.load(open('.atomic/sessions/cc-harness.json')).get('transcript_path') or '')")"
if [[ "$after" == "$TRANSCRIPT" ]]; then
    _pass "transcript_path applied from the Stop event"
else
    _fail "transcript_path applied from the Stop event" "got: $after"
fi

# #1 (transcript fallback) — immutable turn exists; agent_turn below checks text.
require_capture "read Claude change log" atomic log -f json
CC_HASH="$(printf '%s\n' "$COMMAND_OUTPUT" | python3 -c 'import json,sys; entries=json.load(sys.stdin); assert entries; print(entries[0]["hash"])')"
require_capture "read Claude session ledger" atomic session show cc-harness --json
cc_ledger="$COMMAND_OUTPUT"
if echo "$cc_ledger" | python3 -c "import json,sys; d=json.load(sys.stdin); assert len(d[1]) == 1; assert d[1][0]['provenance_hash']"; then
    _pass "claude finalized provenance is readable from the session ledger"
else
    _fail "claude finalized provenance is readable from the session ledger"
fi
assert_file_not_exists "claude graph.json removed after cutover" ".atomic/sessions/cc-harness/graph.json"

# #2 — agent_turn persisted for the claude-code change too.
cc_file=".atomic/changes/${CC_HASH:0:2}/${CC_HASH}.change"
assert_file_exists "Claude change object exists" "$cc_file"
require_capture "read recorded Claude change" atomic change "$CC_HASH" -f json
cc_change="$COMMAND_OUTPUT"
if echo "$cc_change" | python3 -c "
import json, sys
t = (json.load(sys.stdin).get('unhashed') or {}).get('agent_turn')
assert t, 'no agent_turn key'
texts = [e.get('content') for e in t['condensed_transcript'] if e.get('content')]
assert any('patched and verified' in x for x in texts), texts
"; then
    _pass "claude-code agent_turn persisted on the change"
else
    _fail "claude-code agent_turn persisted on the change"
fi

print_summary
