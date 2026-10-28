# Support Diagnostics

`atomic diagnostics` prints a report to paste into a support conversation. It
collects what support otherwise asks for one follow-up at a time.

```sh
atomic diagnostics          # paste-ready text
atomic diagnostics --json   # the same facts as one JSON document
```

The command is read-only. It changes no configuration, attempts no repair, and
uses no network. Outside an Atomic workspace it still reports everything that
does not depend on a workspace. The exit status is 0 even when values are
unknown. The only file it touches is the command log that every `atomic`
invocation appends to.

## What it reports

| Section | Source |
|---|---|
| Atomic version, install source, platform | The running binary (`atomic --version`); install source uses the same detection as `atomic update` |
| Workspace | The same upward search other commands use |
| Agent integrations (installed packages) | Install receipts in `~/.atomic/integrations/<agent>/receipt.json`: package version, the Atomic version that installed it, install time, source, and whether each recorded file is unchanged, modified, or missing |
| Skills | The `atomic-skills` package manifest cached by the last `atomic agent enable`, plus each skill file listed in an integration receipt with its own `version:` frontmatter |
| Enabled agents | Whether Atomic hooks are live in each agent's configuration, with the evidence |
| Git shim | `[git.shim] enabled` in `.atomic/config.toml`, plus the state of each Atomic hook dispatcher (as `atomic git hooks status` reports them) |

**Installed is not the same as enabled.** An integration is *installed* when it
has a receipt. An agent is *enabled* when Atomic's hook commands are present in
its live configuration. Hooks can exist without a receipt (built-in hooks), and a
receipt survives hooks removed by hand. Each agent is reported as `yes`, `no`,
or `unknown`:

- `yes`: the agent's adapter finds Atomic hooks (the check `atomic agent
  status` uses), or a settings file named in the receipt still contains the
  integration's hook command.
- `no`: none of that evidence was found.
- `unknown`: the receipt or a settings file cannot be read, or the package
  installs its hooks somewhere Atomic cannot inspect.

Every unavailable value is reported as unknown or not found, with a reason.
Values are never inferred.

## Privacy

The report contains booleans, versions, timestamps, and paths. URL credentials,
query strings, and fragments are removed from integration sources. Paths under
the home directory are shown as `~/…`. The report never includes file contents,
tokens, transcripts, or other configuration values.

## Limits

- **Agent application versions** (for example, the Claude Code version) are not
  collected. Atomic has no record of them, and detecting them would mean running
  third-party programs. The report says so explicitly.
- **Skill versions** come from a skill file's `version:` frontmatter when it has
  one. The skills package version comes from the local cache. That cache is
  refreshed on every `atomic agent enable`, so it shows the most recent sync.
  An install that used `--from-skills` leaves no cache.
- **Flat-layout skills** (`<name>.md`, as used for Cline and agy) are recognised
  only when the skills package manifest is cached.
- **Enabled** follows each adapter's own definition of "installed". For a few
  adapters (for example Hermes and Sherpa), that check finds the agent's
  configuration rather than specific hook entries.

## Was a record made by an agent?

That is a property of one change, so it is not part of this report. Atomic does
record it:

- Changes recorded by agent hooks carry a hashed `SessionEnvelope` (agent name,
  session ID, turn number) and hashed AI `Provenance` (vendor, model, tool,
  session ID, and `agent_name` / `turn_number` metadata).
- A manual `atomic record` carries AI provenance only when `--ai-assisted` or
  the `ATOMIC_AI_*` environment variables are set. It never carries a session
  envelope.
- A change with neither shows only that no agent provenance was recorded. An
  agent that ran `atomic record` through a shell without those flags looks the
  same as a manual record.

To check a specific record, `atomic change <hash> --format json` shows
`has_provenance` and `provenance`. That output also includes the unhashed turn
transcript, so do not paste it into a support conversation as-is.
