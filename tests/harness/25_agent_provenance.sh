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
# Prerequisites: sqlite3, python3, zstd. Suite skips (exit 77) if absent.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

for tool in sqlite3 python3 zstd; do
    if ! command -v "$tool" &>/dev/null; then
        skip_suite "$tool not installed"
    fi
done

# Extract the last zstd frame of a change file (the UNHASHED section is
# written last, before the 32-byte trailer) and print its JSON.
last_section_json() {
    local change_file="$1"
    python3 - "$change_file" <<'EOF' >/tmp/_prov_last.zst
import sys
data = open(sys.argv[1], "rb").read()
i = data.rfind(b"\x28\xb5\x2f\xfd")
assert i >= 0, "no zstd frame in change file"
sys.stdout.buffer.write(data[i:len(data) - 32])
EOF
    zstd -d -f /tmp/_prov_last.zst -o /tmp/_prov_last.json >/dev/null 2>&1
    cat /tmp/_prov_last.json
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
OC_HOME="$REPO_DIR/oc-home"
mkdir -p "$OC_HOME"
NOW_MS=$(( $(date +%s) * 1000 ))
USER_MS=$NOW_MS
ASST_MS=$(( NOW_MS + 5000 ))

sqlite3 "$OC_HOME/opencode.db" <<EOF
CREATE TABLE session (
  id text PRIMARY KEY, project_id text, directory text NOT NULL,
  time_created integer, time_updated integer);
CREATE TABLE message (
  id text PRIMARY KEY, session_id text,
  time_created integer, time_updated integer, data text);
CREATE TABLE part (
  id text PRIMARY KEY, message_id text, session_id text,
  time_created integer, time_updated integer, data text);
INSERT INTO session VALUES ('ses_harness', 'prj_x', '$REPO_DIR', $USER_MS, $ASST_MS);
INSERT INTO message VALUES ('msg_u', 'ses_harness', $USER_MS, $USER_MS, '{"role":"user"}');
INSERT INTO message VALUES ('msg_a', 'ses_harness', $ASST_MS, $ASST_MS, '{"role":"assistant"}');
INSERT INTO part VALUES
 ('p1','msg_u','ses_harness',$USER_MS,$USER_MS,
  '{"type":"text","text":"harness: fix the widget"}'),
 ('p2','msg_a','ses_harness',$((ASST_MS+100)),$((ASST_MS+100)),
  '{"type":"reasoning","text":"widget reasoning","time":{"start":10,"end":60}}'),
 ('p3','msg_a','ses_harness',$((ASST_MS+200)),$((ASST_MS+200)),
  '{"type":"step-start"}'),
 ('p4','msg_a','ses_harness',$((ASST_MS+300)),$((ASST_MS+300)),
  '{"type":"step-finish","reason":"stop","cost":0.02,"tokens":{"input":40,"output":50,"reasoning":10,"cache":{"write":5,"read":20}}}'),
 ('p5','msg_a','ses_harness',$((ASST_MS+400)),$((ASST_MS+400)),
  '{"type":"tool","tool":"bash","state":{"title":"ran tests","status":"completed"}}'),
 ('p6','msg_a','ses_harness',$((ASST_MS+450)),$((ASST_MS+450)),
  '{"type":"tool","tool":"bash","callID":"call_h1","state":{"status":"completed","input":{"command":"cargo test"},"output":"test result: ok. 12 passed","title":"cargo test"}}'),
 ('p7','msg_a','ses_harness',$((ASST_MS+500)),$((ASST_MS+500)),
  '{"type":"text","text":"The widget is fixed."}');
EOF

# Drive the hook sequence with a THIN stop payload — what the old plugin
# sends: only session/model metadata.
echo '{"session_id":"ses_harness","source":"startup","cwd":"'"$REPO_DIR"'"}' \
    | OPENCODE_HOME="$OC_HOME" atomic agent hooks opencode session-start >/dev/null 2>&1
echo '{"session_id":"ses_harness","prompt":"harness: fix the widget","cwd":"'"$REPO_DIR"'"}' \
    | OPENCODE_HOME="$OC_HOME" atomic agent hooks opencode user-prompt >/dev/null 2>&1

create_file "widget.txt" "widget v1"
# A tool hook carrying the same call id as the store's tool part — the join
# key the enrichment uses to graft commands/outputs onto graph nodes.
echo '{"session_id":"ses_harness","tool_name":"bash","tool_call_id":"call_h1","status":"completed","cwd":"'"$REPO_DIR"'"}' \
    | OPENCODE_HOME="$OC_HOME" atomic agent hooks opencode after-tool >/dev/null 2>&1

echo '{"session_id":"ses_harness","turn_number":1,"model":"test/model","provider":"testprovider","cwd":"'"$REPO_DIR"'"}' \
    | OPENCODE_HOME="$OC_HOME" atomic agent hooks opencode stop >/dev/null 2>&1
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

# #1 + #4 (graph side) — llm_response and decision nodes present.
graph=".atomic/sessions/ses_harness/graph.json"
if python3 - "$graph" <<'EOF'
import json, sys
kinds = [n["kind"] for n in json.load(open(sys.argv[1]))["nodes"]]
assert "llm_response" in kinds, kinds
assert "decision" in kinds, kinds
EOF
then
    _pass "graph has llm_response and decision nodes"
else
    _fail "graph has llm_response and decision nodes" "$(cat "$graph" 2>/dev/null | head -3)"
fi
if python3 - "$graph" <<'EOF'
import json, sys
nodes = json.load(open(sys.argv[1]))["nodes"]
resp = next(n for n in nodes if n["kind"] == "llm_response")
assert resp["summary"] == "The widget is fixed.", resp["summary"]
EOF
then
    _pass "llm_response carries the store response text"
else
    _fail "llm_response carries the store response text"
fi

# #4 (change side) — reasoning_text, tokens, cost, steps on the provenance.
HASH="$(atomic log -f json 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['hash'])")"
prov_json="$(atomic change "$HASH" -f json 2>/dev/null)"
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

# Tool nodes recorded from the thin payload get their command and output back
# from the store, matched on the tool call id.
if python3 - "$graph" <<'EOF'
import json, sys
nodes = json.load(open(sys.argv[1]))["nodes"]
node = next(n for n in nodes if n.get("tool_call_id") == "call_h1")
assert "cargo test" in node["summary"], node["summary"]
assert node["detail"]["command"] == "cargo test", node["detail"]
assert "12 passed" in node["detail"]["output_summary"], node["detail"]
EOF
then
    _pass "tool node enriched from the store (command + output)"
else
    _fail "tool node enriched from the store (command + output)"
fi

# #2 — agent_turn persisted inside the change file (unhashed section).
change_file="$(find .atomic/changes -name "$HASH.change" | head -1)"
if last_section_json "$change_file" | python3 -c "
import json, sys
t = json.load(sys.stdin).get('agent_turn')
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

TRANSCRIPT="$REPO_DIR/cc-transcript.jsonl"
cat > "$TRANSCRIPT" <<'EOF'
{"type":"user","uuid":"u1","message":{"role":"user","content":[{"type":"text","text":"patch the parser"}]}}
{"type":"assistant","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"Patching now."},{"type":"tool_use","name":"Edit","input":{"file_path":"parser.rs"}}]}}
{"type":"assistant","uuid":"a2","message":{"role":"assistant","content":[{"type":"text","text":"The parser is patched and verified."}]}}
EOF

# Deliberately NO transcript_path at session start / prompt — the timing
# gap. The Stop hook carries it.
echo '{"session_id":"cc-harness","cwd":"'"$REPO_DIR"'"}' \
    | atomic agent hooks claude-code session-start >/dev/null 2>&1
echo '{"session_id":"cc-harness","prompt":"patch the parser","cwd":"'"$REPO_DIR"'"}' \
    | atomic agent hooks claude-code user-prompt-submit >/dev/null 2>&1

before="$(python3 -c "import json; print(json.load(open('.atomic/sessions/cc-harness.json')).get('transcript_path'))" 2>/dev/null || true)"
if [[ "$before" == "None" || -z "$before" ]]; then
    _pass "session has no transcript_path before Stop (the gap scenario)"
else
    _fail "session has no transcript_path before Stop (the gap scenario)" "got: $before"
fi

create_file "parser.rs" "fn main() {}"

echo '{"session_id":"cc-harness","transcript_path":"'"$TRANSCRIPT"'","cwd":"'"$REPO_DIR"'"}' \
    | atomic agent hooks claude-code stop >/dev/null 2>&1

# #2 (timing fix) — transcript_path applied from the Stop event.
after="$(python3 -c "import json; print(json.load(open('.atomic/sessions/cc-harness.json')).get('transcript_path') or '')")"
if [[ "$after" == "$TRANSCRIPT" ]]; then
    _pass "transcript_path applied from the Stop event"
else
    _fail "transcript_path applied from the Stop event" "got: $after"
fi

# #1 (transcript fallback) — response derived from the last assistant entry.
cc_graph=".atomic/sessions/cc-harness/graph.json"
if python3 - "$cc_graph" <<'EOF'
import json, sys
nodes = json.load(open(sys.argv[1]))["nodes"]
resp = next(n for n in nodes if n["kind"] == "llm_response")
assert resp["summary"] == "The parser is patched and verified.", resp["summary"]
EOF
then
    _pass "llm_response derived from the transcript fallback"
else
    _fail "llm_response derived from the transcript fallback"
fi

# #2 — agent_turn persisted for the claude-code change too.
CC_HASH="$(atomic log -f json 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['hash'])")"
cc_file="$(find .atomic/changes -name "$CC_HASH.change" | head -1)"
if last_section_json "$cc_file" | python3 -c "
import json, sys
t = json.load(sys.stdin).get('agent_turn')
assert t, 'no agent_turn key'
texts = [e.get('content') for e in t['condensed_transcript'] if e.get('content')]
assert any('patched and verified' in x for x in texts), texts
"; then
    _pass "claude-code agent_turn persisted on the change"
else
    _fail "claude-code agent_turn persisted on the change"
fi

print_summary
