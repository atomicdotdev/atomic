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
| Repository build metadata | Builds recorded at database creation, last completed table initialization, legacy-layout or path-claim migration, and last normal writable table-initialization open |
| Agent integrations (installed packages) | Install receipts in `~/.atomic/integrations/<agent>/receipt.json`, plus last recorded hook dispatch from the current repository's hook log |
| Skills | The cached `atomic-skills` package manifest and each installed skill's `metadata.version` and `metadata.updatedAt` frontmatter |
| Enabled agents | Whether supported settings contain recognizable active Atomic hooks, with the evidence and inspection limitations |
| Git shim | `[git.shim] enabled` in `.atomic/config.toml`, plus the state of each Atomic hook dispatcher (as `atomic git hooks status` reports them) |

**Installed is not the same as enabled.** An integration is *installed* when it
has a receipt. An agent is *enabled* when Atomic's hook commands are present in
its live configuration. Hooks can exist without a receipt (built-in hooks), and a
receipt survives hooks removed by hand. Each agent is reported as `yes`, `no`,
or `unknown`:

- `yes`: a recognizable command hook occurs in an inspected configuration,
  and no inspected scope is unreadable or conflicts with it.
- `no`: supported, inspectable settings contain no active Atomic hooks, or
  those settings are absent.
- `unknown`: settings or required locations cannot be inspected, a receipt is
  unreadable without independent positive evidence, scopes conflict, or the
  agent's active hook/plugin format cannot be verified.

These are observations of supported local configuration, not a guarantee that
an agent actually runs a hook. See the inspection limits below.

The last-fired time includes successful and failed dispatches recorded in the
current repository, including pending events for this working copy. Repositories
and their linked working copies share committed history; other repositories are
not searched. An absent observation does not prove the hook never ran.
If another process holds the repository database for writing, the timestamp is
reported as unknown rather than opening or modifying the database.

Repository build metadata is recorded only from the introduction of this feature;
older creation and migration builds remain unknown. The last writable initialization
open is not the last access: read-only commands and fast writable opens do not update
it. Diagnostics itself does not update these fields. The migration field covers the
additive table initialization, legacy split-database merge and path-claim backfill,
not redb's internal format upgrade.

## Privacy

The report contains versions, timestamps, paths, integration sources, and
names read from local metadata. URL credentials, query strings, and fragments
are removed from URL-shaped integration sources. Paths under a known home
directory are shown as `~/…`. Error reasons use content-free summaries rather
than raw parser or operating-system errors. Review the report before sharing:
paths, names, versions, and other metadata can themselves contain sensitive
information, and arbitrary non-URL source strings are not credential scrubbed.

## Limits

- **Linked Git worktrees:** if ordinary workspace discovery finds no Atomic
  workspace, diagnostics invokes local `git rev-parse` with optional locks
  disabled to inspect a linked worktree. If Git or its administrative paths
  cannot be inspected, workspace-dependent results are `unknown`.
- **Agent application versions** (for example, the Claude Code version) are not
  collected. Atomic has no record of them, and detecting them would mean running
  third-party programs. The report says so explicitly.
- **Skill metadata** comes from each installed skill file's YAML frontmatter:
  `metadata.version` and `metadata.updatedAt`. Missing fields are flagged as a
  possible older or unversioned copy; unreadable or invalid files are reported
  separately. A legacy top-level `version` remains visible but does not satisfy
  the nested metadata requirement. The skills package version comes from the local cache. That cache is
  refreshed on every `atomic agent enable`, so it shows the most recent sync.
  An install that used `--from-skills` leaves no cache.
- **Flat-layout skills** (`<name>.md`, as used for Cline and agy) are recognised
  only when the skills package manifest is cached.
- **Enabled** inspects recognized JSON command-hook entries for Claude Code,
  Gemini CLI, and Codex. It checks their home and workspace settings, plus
  Claude's workspace-local settings, and supported receipt settings at those
  locations. Other agents and unsupported receipt locations, keys, commands,
  or hook/plugin formats are `unknown` rather than inferred from file presence
  or incidental text. Unreadable or malformed settings are `unknown`; a
  conflicting or uninspectable scope can make an otherwise positive observation
  `unknown`. Other settings sources, managed policies, runtime conditions and
  future agent formats may affect actual execution and are not fully modeled.

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
