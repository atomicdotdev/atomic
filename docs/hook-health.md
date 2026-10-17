# Hook Health and Session Logs

Use hook health to check whether agent callbacks are reaching Atomic and to
locate failures that would otherwise be hidden by the hook's shell guard.

```bash
atomic agent status
atomic agent status --json
```

## Reading the report

The human report shows each agent's most recent successful dispatch, its age
relative to the current time, and outstanding failing verbs. Session rows
attribute failures to a session and its parent. Existing plugin and git-shadow
errors are grouped from a bounded tail of `.atomic/hook-errors.log`.

Health is tracked per **agent, session, and verb**. A successful child cannot
clear a parent's failure. A later successful dispatch of the same verb in the
same session clears that session's current failure while retaining the earlier
observations in the log. A successful dispatch may legitimately produce no new
change when the working copy is unchanged.

### JSON fields

| Field | Meaning |
|-------|---------|
| `hook_health` | Per-agent summary: `last_fired`, `last_ok`, `last_error`, `verbs_recorded`, and `failing_verbs` |
| `hook_sessions` | Session-level rows with `agent`, `session_id`, `parent_session_id`, and `health.verbs` |
| `hook_health_error` | Why the operational log could not be loaded; pending observations may still be reported |
| `hook_errors` | Grouped errors and recent entries from the bounded legacy error-log tail |
| `silent_agents` | Installed agents with no successful dispatch in the available operational log |

Each entry in `health.verbs` contains `last_fired`, `outcome` (`ok` or `error`),
an optional failure `detail`, and an optional `last_ok`. Failure summaries flatten
whitespace and truncate long details. Missing session IDs appear as `null`.

The hook-health fields are omitted when there are no observations. An empty
log gives no evidence that a freshly installed hook has failed. When the service
is unavailable and only pending observations are accessible, `silent_agents`
is omitted because the report is incomplete.

With `jq` installed, inspect one session's current outcomes:

```bash
atomic agent status --json \
  | jq '.hook_sessions[]? | select(.session_id == "ses_child")'
```

## Parent/child correlation

Native adapters retain the session ID and agent context from the payload.
Integration bridges can supply `parent_session_id` in `raw_json`, or explicitly
set `ATOMIC_SESSION_ID` and `ATOMIC_PARENT_SESSION_ID` for the hook process.
Typed `agent receive` also accepts a top-level `parent_session_id` and forwards
the resolved parent on its service request. Correlation is resolved in the
caller so it survives process boundaries.

If malformed input has no usable session ID, Atomic retains it as an unassigned
diagnostic. It remains visible without being attributed to a different session.

## Storage and recovery

Native hooks, typed receive, and the shared dispatch handler emit structured
Rust `tracing` observations. The existing local/reactor service layer appends
them transactionally to `.atomic/atomic.redb`, with event-ID deduplication and a
session index for paginated reads. Status projects health from these immutable
events. It no longer rewrites a shared `hook-health.json` snapshot.

Operational observations are local diagnostic data, independent of the
running-turn provenance journal. Parsing failures and failures after a turn
closes can therefore be logged without requiring a running turn.

During a service outage, each observation is spooled to its own immutable file
under `.atomic/hook-log-pending/`. Status can inspect these pending observations.
A successful append replays pending events into the database and removes their
spool files after commit. Retried events retain their original IDs and
observation times, so replay neither duplicates them nor makes an older failure
look newer than a later recovery.
