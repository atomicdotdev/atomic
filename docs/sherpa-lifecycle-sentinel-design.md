# Lifecycle-ownership sentinel — design summary

> Status: **design, pre-implementation.** Replaces the failed env-based suppression
> (`ATOMIC_HOOKS_SUPPRESSED`). Write this down, confirm no holes, THEN implement.

## Why env failed (the baseline)

The env approach (`ATOMIC_HOOKS_SUPPRESSED=1` injected into the codex-acp child)
does **not** survive to the inner hook process. Verified twice (sherpa-e2e binary
and the integration binary), real codex-acp, global hooks on:

```
turn-end JSON:  {"recorded":false, "change_hash":null, "files":[]}

codex   session  view=rough-bloom-30b6  turn=1  recorded=[AMOO37KB…]  ← inner stole the change
sherpa  session  view=shy-river-125e    turn=1  recorded=[]            ← outer empty
```

Root cause: codex CLI runs the hooks.json commands (`atomic agent hooks codex …`)
under its own `shell_environment_policy` (not `inherit=all` by default), which
strips our injected env. An env probe inserted into hooks.json never fired with
our var set; the codex transcript shows no `atomic`/`hooks` exec at all — the
integration is internal to codex CLI. So **env is the wrong signal carrier**; it
is filtered by the very process we need to reach. A filesystem signal is not
filtered by an env policy.

## Verified premises (checked against real code, not assumed)

1. **Hook process knows its own agent.** `Hooks { agent_name: String, verb: String }`
   (atomic-cli/src/commands/agent/hooks.rs:81-86). It is a CLI positional arg, always
   present: `atomic agent hooks codex pre-tool` → agent_name=`codex`; the outer
   `atomic agent hooks sherpa turn-end` → agent_name=`sherpa`. So owner-vs-other
   discrimination is feasible. ✅
2. **Outer Sherpa uses the SAME entry.** noname/bridge shells out to
   `atomic agent hooks sherpa <verb>` (bridge.rs). So a naïve "sentinel exists →
   no-op" would suppress the OUTER too (self-destruct). The owner_agent field is
   mandatory, not optional. ✅ (this is Codex's key warning, confirmed real)
3. **Same cwd.** bridge dispatches with `.current_dir(cwd)`; codex runs with
   cwd=project too. Both inner and outer hooks run in the project repo, both can
   read `.atomic/…`. So the sentinel must live in the repo and discriminate by
   owner_agent — there is no cwd separation to exploit. ✅
4. **Sentinel lifecycle hook points exist.** acp.rs already has finally-style
   session_start (line 210) / session_end (line 457) cleanup. Sentinel create goes
   right after a successful session_start switch; delete goes in the same cleanup
   path that always runs session_end. ✅

## Two additional constraints I found (not in Codex's note)

A. **`find_repository_root()` returns `Err` (not panic) outside a repo**
   (commands/mod.rs:237). The sentinel check must therefore treat "no repo root"
   as "no sentinel → proceed normally", NOT as suppress. A user running plain
   codex in a non-atomic dir must be unaffected. (hooks.json already guards with
   `test -d .atomic && … || true`, but the in-binary check must be safe too.)

B. **Order of operations in `run()` needs adjusting.** Today: env-gate (131) →
   read stdin (137) → … → find_repository_root (188). Codex wants the sentinel
   check before reading stdin but it needs repo root. So we must MOVE a
   repo-root lookup ahead of the stdin read for the sentinel check. Keep it
   tolerant: if repo root errs, skip the sentinel check and fall through to the
   existing flow (which reads stdin then does its own repo lookup).

## The sentinel

- **Path:** `.atomic/agent-hooks-owner.json` (repo-local, NOT /tmp, NOT env).
- **Schema:**
  ```json
  {
    "owner_agent": "sherpa",
    "session_id": "<uuid>",
    "created_at": <unix_secs>,
    "expires_at": <unix_secs>
  }
  ```
- **Stale protection:** `expires_at` guards against a noname crash leaving the
  repo permanently suppressed. If `now > expires_at`, the sentinel is ignored
  (treated as absent). Pick a generous TTL (e.g. 1 hour) — long enough for a real
  ACP turn, short enough that a crashed run self-heals.

## atomic hook check (engine, hooks.rs `run()`)

```
fn run():
    if env ATOMIC_HOOKS_SUPPRESSED set: return Ok(())          # keep as belt-and-suspenders
    repo = find_repository_root()                              # tolerant: Err → skip sentinel
    if repo ok:
        s = read .atomic/agent-hooks-owner.json                # absent/parse-err → skip
        if s present and now <= s.expires_at and s.owner_agent != self.agent_name:
            return Ok(())                                      # inner agent → no-op, exit 0, silent
        # owner_agent == self.agent_name (outer sherpa) → fall through, record normally
        # expired → fall through (and ideally ignore/cleanup)
    read stdin … normal dispatch (unchanged)
```

Key property: **owner (sherpa) falls through and records; non-owner (codex/claude/
opencode) no-ops.** Exit 0, silent — never make the agent think the hook failed.

## noname side (bridge + acp.rs)

- Before the ACP child runs (after session_start switches view), write the
  sentinel with `owner_agent="sherpa"`, the run's session_id, created_at=now,
  expires_at=now+TTL.
- In the same always-run cleanup that calls session_end, **delete** the sentinel.
- Keep the existing env injection too? Optional — harmless belt-and-suspenders,
  but the sentinel is the load-bearing mechanism now. (Leaning: keep env-gate in
  the engine as a second lever, drop the noname env injection to avoid implying it
  works. TBD.)

## Merge gate (unchanged — must pass before merge)

Same real E2E: global codex hooks ON, codex-acp really edits a file, outer sherpa
turn-end returns `recorded:true + change_hash + view + files`, and `view list`
shows NO nested codex view stealing the change. Compare against the failed
baseline above.

## Open question for review

- TTL value + whether an expired sentinel should be actively deleted by the next
  hook that sees it (self-cleaning) vs just ignored.
- Do we keep the noname→child env injection as a belt-and-suspenders, or remove it
  so the mechanism is unambiguously the sentinel?
