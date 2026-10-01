# Atomic Git Bridge — Operating Guide (CB-13C)

Status: **opt-in, advisory-evidence bridge; default rollout is blocked.** This
guide is written against the shipping `atomic` CLI (version 0.17.1) and every
command example below was executed in a disposable repository on Linux
(2026-09-14); outputs are quoted verbatim. It is the user, agent, migration,
and privacy/security entry point for the colocated Git bridge. The normative
contracts live in
[`RFC-ATOMIC-GIT-CAUSAL-BRIDGE.md`](RFC-ATOMIC-GIT-CAUSAL-BRIDGE.md) and the
work status in
[`RFC-ATOMIC-GIT-CAUSAL-BRIDGE-TODO.md`](RFC-ATOMIC-GIT-CAUSAL-BRIDGE-TODO.md).

> **Nothing in this guide is a claim of default rollout readiness.** The
> bridge is explicit per-repository opt-in; hooks and watchers are optional
> evidence and accelerators, never correctness boundaries; and an arbitrary
> `git push` to a server that does not itself enforce provenance is **outside
> the protection guarantee** (RFC §12.10).

---

## 1. Honest status: what the bridge does and does not do today

**Working, exercised in this guide:** advisory event evidence, reconcile /
verify / switch / publish commands, staged-status parity subset (CB-11A),
ref-mapping status (CB-10A), structured observability journal (CB-13C),
receiving-side verification (CB-12B), provenance publication gates (CB-12B),
local pre-push refusal.

**Not available (do not rely on it):**

- **CLI cutover / full migration is not exposed.** The CB-13B shadow→bridge
  cutover exists only as library code with a closed fix loop and open review
  findings: there is **no CLI command** that runs `execute_bridge_cutover`.
  Known open findings (source-confirmed, CB-13B review, all unresolved):
  - *Fence-before-lock race:* the legacy shadow writer checks the
    `git-bridge-cutover` fence **before** acquiring the common lock
    (`materialize.rs` `try_lock_shadow_commit`), so a cutover that completes
    between the check and the lock can interleave with a legacy writer.
  - *Empty-Git readiness:* cutover readiness accepts any existing `.git`
    path; the passing tests exercise an empty directory, not a real
    repository with hooks/remotes.
  - *Already-fenced proof:* a repository with the requirement present at a
    different version is treated as already fenced; no verified/incomplete
    head resolution precedes the no-op.
  - No full final-schema audit/backfill, no projection-verified legacy
    bindings, no CLI caller, no compatibility/failure matrix.
  Do not attempt migration from Shadow through the CLI; it is unsupported.
- **`--adopt-git` is explicitly unavailable** until Phase 9 foreign
  synthesis lands. The CLI refuses it:
  `atomic git bridge enable --adopt-git` → typed refusal before mutation.
- **Managed-session capture is evidence, not exact reassembly.** CB-12A
  findings (unresolved): commit capture is MAC-authenticated evidence, exact
  separable reassembly is unimplemented, and synthesized sessions stay
  incomplete.
- **Watchers/`atomic bridge watch` do not exist yet.** The reactive daemon is
  CB-13D. Candidate-path acceleration (fsmonitor/Watchman change sources)
  exists for status; hooks are advisory evidence.

## 2. Enable and disable

**Without the opt-in, a colocated repository is a native Atomic repository.**
A `.git` directory next to `.atomic` is not consent. Until the repository
enrolls — `[git.bridge] enabled = true` (recorded by `enable`) or a verified
checkpoint written by an explicit bridge command (`git bridge reconcile`,
`git import`, anchoring, clone bootstrap / `--adopt-git`) — ordinary commands
(`status`, `add`, `record`, `diff`, `stash`, `tag`, agent turn-end, …) behave
exactly as in a repository without Git: they never refuse for a missing Git
anchor, never read Git HEAD/index state, never write the Git index, and never
create Git refs. `git init && atomic init && atomic add f && atomic record`
works without touching Git.

Once enrolled, an unanchored workspace refuses ordinary commands and names the
exact commands that resolve it:

```console
$ atomic status
✗ workspace reconciliation required: workspace is not anchored to a verified Git baseline: MissingCheckpoint

The Git bridge is enabled for this repository, but this workspace has no verified Git baseline yet. Anchor it to the current Git HEAD (imports the Git history into the current view):
  atomic git bridge reconcile
To use Atomic without the Git bridge in a workspace that was never anchored:
  atomic git bridge disable
```

A branch with no commits yet (`UnbornHead`) has no Git baseline to anchor to:
create the first Git commit, then run `atomic git bridge reconcile`.

Enable is explicit and per-repository. Exercised:

```console
$ cd repo && atomic git bridge enable
ℹ Installed Atomic advisory post-checkout dispatcher at <repo>/.git/hooks/post-checkout
ℹ Installed Atomic advisory pre-commit capture evidence (managed sessions only, advisory) dispatcher at <repo>/.git/hooks/pre-commit
ℹ Installed Atomic advisory pre-push provenance verification (advisory to the guarantee; refuses locally) dispatcher at <repo>/.git/hooks/pre-push
ℹ Installed Atomic advisory post-rewrite evidence (operation linkage only) dispatcher at <repo>/.git/hooks/post-rewrite
ℹ Installed Atomic advisory reference-transaction evidence (ref movement only) dispatcher at <repo>/.git/hooks/reference-transaction
ℹ Recorded explicit bridge opt-in in the repository configuration ([git.bridge] enabled = true).
ℹ Default (automatic) enablement remains refused: rollout gate 'recovery' is unmeasured; default enablement is blocked until the RFC §21 matrix records measured evidence and owner approval
⚠ fetch refspecs not configured: Git error: cannot configure Atomic fetch refspecs: remote 'origin' does not exist (remote 'origin' does not exist; class=Config (7); code=NotFound (-3)); binding refs will not transfer with fetch until 'atomic git bridge enable --remote <name>' succeeds
⚠ bridge anchor not created: cannot anchor: no --binding-key-file was provided, so the Anchor cannot be signed; … pending work is preserved as-is and is not re-globalized until CB-7B — commit or record it first, then retry
```

What that says truthfully:

- Enable installs **advisory** hook dispatchers and (without a remote) leaves
  binding transfer unconfigured with a warning.
- Enable also records the explicit opt-in in the repository configuration
  (`[git.bridge] enabled = true`) — the flag the automatic telemetry sink
  consumes — and then reports the default-enablement gate honestly: default
  (automatic) enablement remains refused while rollout gates are unmeasured.
- The **anchor** (signed binding, RFC §14.4) is refused without an explicit
  `--binding-key-file <file>` holding a hex-encoded 32-byte Ed25519 secret
  key. Nothing is silently anchored.
- Anchoring requires clean Git and Atomic; otherwise use `--adopt-atomic`
  (explicit remediation) — never a silent merge.
- A custom `core.hooksPath`, existing hook binaries, or symlinks are never
  edited in place; Atomic prints integration instructions instead.

**Disable:** `atomic git bridge disable` records the rollback in the
repository configuration (`[git.bridge] enabled = false`); the consent-gated
automatic telemetry sink stops writing once consent is withdrawn. The
advisory dispatchers installed by `enable` remain on disk — they are
advisory evidence hooks, not the guarantee (RFC §10.3) — and
`atomic git hooks uninstall` removes the post-checkout dispatcher only; the
capture, pre-push, post-rewrite and reference-transaction dispatchers must
be removed manually from `.git/hooks/`. All of them are advisory:
correctness never depends on a hook having run (RFC §11).
Disabling a workspace that was never anchored returns it to native Atomic
behavior. A workspace that already holds a verified checkpoint keeps the
stale-baseline guard for that baseline.

## 3. Working copies

Atomic tracks physical directories as **working copies** (`.atomic/
working_copy_id`), so linked Git worktrees and agent sandboxes each get their
own identity, shelves and caches. Ordinary view-scoped graph APIs do not need
a working copy. A fresh clone of one repository directory is a *different*
working copy; state moves with the repository, not the directory. Operation
history is per-working-copy plus one shared repository head; linked worktrees
fail closed on foreign incomplete heads.

## 4. Draft views, detached HEAD, and explicit publish

A Draft view's Git projection is a **detached HEAD** at its projection
commit — `git status` reports "Not currently on any branch" while `atomic
status` stays coherent. This is expected, not drift. Exercised:

```console
$ atomic view switch feature
✓ Switched to view: feature (0 files updated, 4 directories)
$ atomic git bridge reconcile
✓ Git HEAD already matches the current Atomic view
$ git status | head -2
Not currently on any branch.
nothing to commit, working tree clean
```

To publish a Draft view to a real branch, use the explicit one-liner:

```console
$ atomic git bridge publish feature --branch feature
✓ Published draft view 'feature' on branch 'feature' at d3784964ea973c877c9290f0705d2eb83837585e; the view remains a Draft in Atomic
```

Notes from the exercise: publishing requires a projection commit first
(`✗ … has no projection commit to publish; run 'atomic git bridge reconcile'
or record first`, exit 4), and `atomic git bridge switch <view>` refuses a
missing target branch instead of creating one. HEAD stays detached after
publish; switch explicitly to the branch when you want it attached.

## 5. Staging and status parity limits

`atomic status --git` is the versioned, Git-inspired **staging/status parity
subset** (RFC §9.2 golden rows, CB-11A). It is observation-only and reports a
subset of Git semantics. Exercised (clean aligned state, JSON form):

```json
{
  "bridge": { "state": "aligned", "view": "main", "origin": "checkpoint", … },
  "clean": true,
  "format": "atomic-status-git",
  "layers": { "baseline_tree": "c7791dc…", "index_tree": "c7791dc…", … }
}
```

Limits to know: it does not claim full Git porcelain parity; on a fully clean
state the human-format output is intentionally empty; and conflicts surface
as conflict **snapshot** commits on Draft refs (RFC §8.3) — ordinary clean
Git commits, never index-conflict markers baked into shared history. A
refused Shared export without the explicit flag keeps conflict packs local.

## 6. Trust, untrusted provenance, and signed boundaries

- Trust is **deny-by-default per repository** (`[git.trust]` in
  `.atomic/config.toml`): the repository identity plus explicitly listed
  `signers` DIDs are trusted; `revoked` always wins; everyone else is
  `Unknown` — their content is still recomputed and usable, but their
  provenance claims never satisfy a publication gate.
- **Content correctness is independent of trust** (RFC §5.2): signatures gate
  provenance trust only.
- **Signed boundaries:** the Anchor binding requires an explicitly supplied
  Ed25519 key file (`--binding-key-file`); receiving-side enforcement is
  `atomic git bridge verify-receive` on a trusted boundary; publishing runs
  the managed-provenance gate over the full reachable closure and refuses
  incomplete evidence:

```text
pre-push (local): refusing THIS machine's push of view 'feature'. A local
refusal is advisory to the guarantee: it does not protect the remote, and a
remote without its own enforcement can still receive arbitrary pushes (for
example with 'git push --no-verify' or from any other client).
```

- Known trust limitation (recorded, unresolved): managed attestations are
  session-MAC authenticated, not agent-DID signed; boundary/outcome/binding/
  provenance roots are not yet covered by the signature (CB-12A review F3).

## 7. Safe recovery

Every operation is journaled with expected-old/new leases and immutable
receipts (`.atomic` `OPERATIONS`/`EFFECT_RECEIPTS` tables). A writable open
performs idempotent recovery before accepting new work; interrupted
filesystem effects replay from retained backups; `atomic op log` / `atomic op
show` / `atomic op undo` / `atomic op restore` inspect and invert. Recovery
outcomes are also recorded in the observability journal (below). `.atomic`,
`.vault`, shelves and Git objects are **never** deleted as a repair strategy;
GC respects retention roots (incomplete sessions and their snapshots/WIP
never expire by age).

## 8. WIP retention and incomplete sessions

When an agent turn ends with unexplained work, the edits are preserved as a
WIP ref and the session is marked durably `Incomplete` — never recorded as
empty, never silently dropped. Incomplete sessions and their snapshot/WIP
objects are retention roots: age alone never deletes the only known copy of
unbound work. `atomic status --no-reconcile` prints forensic drift evidence
without mutating anything.

## 9. Namespace fallback

Binding and view refs live under `refs/atomic/bindings/*` and
`refs/atomic/views/*`. Fetch refspecs for those namespaces are configured at
enable time for a chosen remote (`--remote <name>`); a missing remote is a
warning, not an error, and binding refs then simply do not transfer with
fetch until it is configured. The Atomic remote remains the primary object
source (RFC §16).

Those refspecs only make the *custom namespace* transfer; they cannot bypass
a host that rejects custom ref namespaces at push time. The actual fallback
for such hosts is the explicit, never-automatic
`atomic git bridge binding queue --degraded-head-fallback`: it publishes
bindings into the **`refs/heads/atomic/bindings/*`** namespace instead.
This mapping is explicitly **not semantically equivalent** (RFC §8.6) —
head-namespace refs look like ordinary branches to every Git client, can be
fetched or pruned by ordinary branch tooling, and do not carry the custom
namespace's create-only protection — so it is opt-in per invocation, and
stale/external refs are still never overwritten. Verify whatever path you
publish with `git ls-remote`.

## 10. Hooks and watchers are optional — never the guarantee

> Bridge mode requires no additional software. For repositories over roughly
> 50k files, Git's builtin filesystem monitor or Watchman may accelerate
> candidate-path discovery when the installed Git/Watchman integration passes
> Atomic's capability probe. Atomic always revalidates candidates and falls
> back to scanning. Leave monitoring off in CI and containers. (RFC §11.2)

Configured with `[git] watch = "off" | "auto" | "fsmonitor" | "watchman"`
in `.atomic/config.toml` (default `auto`; `off` is fully correct). Every
watcher tier is an accelerator: Atomic revalidates candidates against the
canonical index, and degradation is observable (below). The reactive daemon
(CB-13D) does not exist in this release and must not be assumed.

## 11. Arbitrary Git push outside the protection guarantee

A `git push` from any client to a server that does not enforce provenance
(Atomic-controlled remote, `verify-receive` boundary, or required CI check)
can publish anything; local hooks are advisory and bypassable with
`--no-verify`. The guarantee holds only at Atomic-controlled or
server-enforced publication gates (RFC §12.10). Detect-and-warn is the local
pre-push behavior quoted in §6.

## 12. Privacy and security rules

- **Transcripts, prompts, and decision graphs never enter Git objects or
  telemetry.** The projection excludes `.atomic/` and `.vault/` entirely
  (enforced by the bridge path exclusion set and pinned by tests). Content
  that *you* track in files is ordinary tracked content — as in Git.
- **Structured telemetry is validated by construction:** the event journal's
  schema is a closed enum of typed fields — closed classification-code
  enums and validated fixed-format identifiers (ULIDs, 52-character base32
  operation IDs, 64-character lowercase-hex binding IDs) plus counts. A
  value outside those closed sets cannot be constructed into an event, so
  there is no free-form payload field that could carry transcripts,
  prompts, decision graphs, secrets, or environment values; trust signers
  and keys are never logged. The journal is best-effort advisory telemetry:
  it is lossy, unsynced (no fsync), drops records at the retention cap and
  under concurrent contention, and repairs crash-interrupted tails. It is
  **not** audit-retention or a recovery authority — the durable
  operation/receipt journal is.
- Advisory Git event evidence (`.atomic/bridge/git-events.jsonl`) records ref
  transactions and hook events only; it is never rewrite identity (RFC §5.4)
  and never authoritative.
- Session capture evidence is MAC-keyed and stored beside the evidence —
  evidence quality, not a content-correctness claim (CB-12A).

## 13. Observability: the structured event journal

RFC §13 Phase 13 task 5 outcomes are recorded as one JSON line per event in
**`.atomic/bridge/events.jsonl`**. The journal is bounded at 4 MiB: a record
is appended only when the complete record (JSON plus newline) fits under
the bound — otherwise the event is dropped, never truncated history. Each
record is appended as one atomic write under a nonblocking file lock;
contended writers drop their event, a crashed writer's partial tail line is
repaired before the next append, and the file lives under `.atomic/` so the
Git projection excludes it. Writing is consent-gated: observational paths
(`atomic status`) never write telemetry, automatic paths (recovery,
binding fetch, import synthesis) write only for repositories that opted in
(`[git.bridge] enabled = true`, recorded by `atomic git bridge enable` and
rolled back by `atomic git bridge disable`), and explicit bridge commands
are their own consent. Recorded events from the fix pass's disposable
repository exercise:

```json
{"timestamp_ms":1789365010649,"working_copy":"01M2F7AY8HTE7JM7J01CMFA95K","event":"import_synthesis","commits_found":1,"commits_parsed":1,"written":1,"empty":0,"merges":0,"self_push_skipped":0,"squash_inserted":0}
{"timestamp_ms":1789365011164,"working_copy":"01M2F7AY8HTE7JM7J01CMFA95K","event":"change_source_fallback","source":"scan","fallback_source":"custom","reason":"source_error"}
{"timestamp_ms":1789365011395,"event":"reconcile","direction":"git_to_atomic","outcome":"applied"}
```
(The three lines above are verbatim from the fix pass's disposable repository
exercise; earlier sessions recorded the same schema with different values.)

Stable event classes: `reconcile` (direction `neither|git_to_atomic|
atomic_to_git|diverged|unknown`, outcome `applied|refused|failed|no_change`,
refusal class — `failed` marks a terminal failure whose effects are
unverified and may have partially applied), `workspace_refusal` (mode +
remediation code; recorded only by the mutating remediation boundary —
guarded commands never write telemetry on refusal, per the RFC Phase 0
no-mutation contract), `publication_refusal` (boundary
`verify_receive|pre_push`, refused/checked counts, and the unit counted:
`refs` at the receive boundary, `changes` at the pre-push provenance
gate), `change_source_fallback` (tier + degradation reason, recorded only
by explicit bridge command boundaries), `recovery` and `recovery_failure`
(original/recovery operation IDs, whether a new Recover operation was
created, and the stable failure reason for lease rejections or
replay/finalize failures), `binding_fetch` (readiness + loss flag — the
loss observable for truncated shallow history, missing closure objects,
refused packs or unavailable sources), `binding_fetch_refused` (cryptography,
content, closure-budget or closure-validation refusals), and
`import_synthesis` (per-import counters: commits found/parsed, changes
written, exact resurrections from verified bindings, empty/merge commits,
self-push skips, squash insertions, the per-import correlation ID, and — on
a partial failure — the landed-commit count, so a failed import is never
bypassed by the aggregate; synthesis is derivable as written minus
resurrections — one event per imported branch from the real import
statistics).

Known observability limits (recorded, not hidden): the *loss* observable is
per binding fetch (the `lossy` flag). CORRECTION (CB-13C F4, ::23): an
import CAN fetch bindings — the resurrection path
(`parallel.rs` -> `resurrection.rs` -> `fetch_binding_closure`) fetches and
re-validates binding closures for commits with verified bindings, so the
earlier claim that "an import never fetches bindings" was false and has
been corrected here and in the signed memory it produced. Per-import loss
correlation therefore exists through the resurrection/fetch chain: the
fetch's `lossy` flag is observable per fetch and the import's
`correlation_id` ties its events together; consumers compute rates from
the event counts. The steady-state watcher tier is reported per status
transaction via `ChangeSourceMetrics` rather than per event; status itself
never journals. Refused guarded commands are deliberately not journaled
(RFC Phase 0 no-mutation contract), and the journal makes no fsync: it is
advisory telemetry, not the durable operation/receipt journal.

## 14. Migration and cutover (unsupported)

See §1: the transactional Shadow→bridge cutover has no CLI caller and open
fence/readiness/proof findings. Old clients encountering a bridged
repository fail closed via the repository capability fence (that part is
tested); the positive cutover path is **not** certified. Keep backups; do
not attempt CLI migration.
