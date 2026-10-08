# COVERAGE — atomic-protocol vs every existing surface

The alignment artifact for Lee/Aaron/Vince: proves (1) the two inventoried RPC
surfaces are fully covered, and (2) every CLI command that touches redb in
its path has a covering service method, with combine decisions explicit.

Redb verdicts are from source reading of `/Users/aaron/code/work/atomic`
(2026-10-01): `Repository::open*` opens `pristine.redb` (+ file change
store); `changes.redb` is opened ONLY by the legacy database-owner daemon
(`RedbChangeStore::open`, sole caller `owner.rs:900`) — a surface being
fully dismantled. "RW"/"RO" = write/read handle.

Legend — VERDICT: **direct-RW / direct-RO / creates** (opens redb itself) ·
**indirect** (via a shared helper or daemon RPC that opens) · **none** (no
redb access) · **bootstrap** (repository creation, exempt per RFC coverage
definition). COVERAGE: the RPC(s) that replace the command's redb path.
🔒 = hidden command or flag (clap `hide = true` — not shown in `--help`,
e.g. `agent database-owner`, `agent hooks`, `status --debug-ignore`,
`log --all`, `diff --cached`, `remove --keep`, `view list -v`,
`identity delegate`); 🔒🔒 = hidden subcommand within a hidden command
(`agent database-owner serve`). Hidden ≠ unimportant: hooks and the
completion engine are load-bearing surfaces.

---

## 1. Method inventory (94 RPCs / 15 services, contract revision 2)

| Service | RPCs | Scopes |
|---|---|---|
| DaemonService | Health, GetCapabilities, ResolveRepository, RegisterRepository, ListRepositories, Shutdown | BOTH/BOTH/LOCAL/LOCAL/LOCAL/LOCAL |
| ProvenanceService | ReserveTurn, AppendEnvelopes, PrepareCheckpoint, LoadFrozenEnvelopes, BindCheckpointHash, AcknowledgeCheckpoint, UpdateTurn, GetTurn, GetSession, ForkSession, RebuildSessionIndex, GetProvenance, ExportProvenance | journal 8×BOTH + session/projection 5×(BOTH/LOCAL) |
| SandboxService | OpenSandbox, RenewSandbox, CloseSandbox, CreateSandboxTree, StageSandboxImage, SealSandboxImage, Materialize (stream), GetFileStates, GetChanges, PublishProvenance, SubmitChange | admin/trees LOCAL; data ops BOTH |
| RepositoryQueryService | Status, Log, GetChange, Diff (stream), DiffWorkingCopy (stream), PreviewMutation, PreviewRestore, ListConflicts | workspace reads LOCAL; repository reads/previews BOTH |
| RepositoryMutationService | AddFiles, RemoveFiles, MoveFile, Restore, Record, Revise, InsertChanges, Unrecord, CreateStash, ApplyStash, ListStashes, DropStash | InsertChanges/Unrecord BOTH; workspace/stash LOCAL |
| ViewService | ListViews, CreateView, SwitchView, DeleteView, SetViewScope | BOTH, BOTH, LOCAL, BOTH, BOTH |
| TagService | CreateTag, DeleteTag, ListTags, GetTag | all BOTH |
| VaultService | InitVault, SyncVault, GetVaultEntry, ListVaultEntries, GetVaultContext, ExportVault, CreateVaultEntity, UpdateVaultEntity, DeleteVaultEntity, ValidateVaultEntity, LinkVaultEntities | view-scoped reads/writes BOTH, including sandboxes; SyncVault/ExportVault LOCAL |
| KnowledgeService | QueryGraph, MaintainKnowledgeGraph | BOTH, LOCAL |
| AttestationService | PrepareAttestation, RecordAttestation, VerifyAttestation, ListAttestations | all BOTH |
| SyncService | PushChanges, PullChanges, ListRemotes, ManageRemotes, BindRemoteProject | all LOCAL |
| GitInteropService | ImportFromGit, PreviewGitImport, PushToGit | LOCAL |
| TriageService | ListTriageCandidates, GenerateTriageReview | BOTH |
| MaintenanceService | Repair, CheckRepository, ReindexWorkspace, CompactDatabase | LOCAL, BOTH, LOCAL, LOCAL |
| ReactorService | Publish, PublishStream, Subscribe, Replay, CheckpointConsumer | all BOTH (contract draft) |

## 2. Legacy agent journal protocol (13 ops, BEING DISMANTLED) → ProvenanceService

The legacy journal protocol and the `atomic agent database-owner` command
that served it are being **fully dismantled**; what carries forward is the
provenance tracking semantics. The mapping below proves nothing is lost in
the fold. NOT carried (dismantled with the protocol): the owner process
itself, its fs2 election, per-request re-opens, JSON framing, 3-attempt
reconnect, version-in-frame. Liveness and process lifecycle are daemon
concerns now (Health/Shutdown), not protocol ports.

| # | Legacy op | Target | Carried semantics |
|---|---|---|---|
| 1 | `Ping` → Pong{pid, flags} | DaemonService.Health (+GetCapabilities) | additive flags → feature tokens + method matrix; clients gate on the negotiated matrix (the additive capability-flag pattern, generalized) |
| 2 | ReserveProvenanceTurn | ProvenanceService.ReserveTurn | grants generation; first reserve claims session for view; committed flag |
| 3 | AppendProvenanceEnvelopes | AppendEnvelopes | expected_generation fencing; chunk under negotiated budget (4 MiB preserved); acks {event_id, sequence} |
| 4 | PrepareCheckpoint | PrepareCheckpoint | exact CheckpointSource fields; reuse_frozen_changes; freezes envelopes |
| 5 | LoadFrozenEnvelopes (unpaged) | — compat mode dropped | the unpaged form was compat with the dismantled protocol; frozen loads are paged-only (a complete journal may arrive as one page) |
| 6 | LoadFrozenEnvelopesPage | LoadFrozenEnvelopes (paged-only) | {seq,offset} cursor; fragments {cursor, bytes, complete}; 256-fragment budget; client reassembly + continuity validation; cursor absent = first page |
| 7 | BindCheckpointHash | BindCheckpointHash | binds manifest hash; session_turn; advances to attempt_generation |
| 8 | AcknowledgeCheckpoint | AcknowledgeCheckpoint | caller-supplied completed_at |
| 9-11 | Stop/Resume/AbandonTurn | UpdateTurn (action enum) | exact StopCause values; resumable; observed_at; expected_generation (verified in current source, missing in the old wire draft) |
| 12 | TurnStatus | GetTurn | read-side |
| 13 | Shutdown | DaemonService.Shutdown | daemon lifecycle admin (graceful drain) — not a carry of the legacy lock-release op |

**Kept as separate RPCs:** prepare / bind / acknowledge — the named
failpoints (before/after-envelope-commit, after-checkpoint-prepare,
before-frozen-page-continuation, frozen-page-unavailable, after-checkpoint-
bind) sit exactly on those seams.

**Dismantled deliberately (not carried):** the owner process and its fs2
election (daemon lifecycle owns storage, D3/D4), per-request re-opens
(persistent daemon), 3-attempt reconnect (`request_with_reconnect` → D4
client start-and-retry), JSON framing + version-in-frame (→ gRPC transport
+ GetCapabilities protocol_version + stable mismatch error), the unpaged
frozen-load compat mode (its callers are dismantled with the protocol).

**Exact types verified from source:** StoredProvenanceTurn (schema_version,
provenance_id u64, session_id, turn_number, state, generation, next_event_seq,
created/updated/completed_at, final_hash, checkpoint_attempt); StopState
(cause, observed_at, last_event_seq, resumable); CheckpointPhase (Prepared,
HashBound, Published); attempt (attempt_generation, frozen_event_count,
source, phase, provenance_hash, session_turn, manifest_hash, prepared_at,
updated_at); constants (PROTOCOL_VERSION 1, MAX_FRAME_BYTES 64 MiB,
FROZEN_PAGE_BYTES 1 MiB, MAX_FROZEN_PAGE_FRAGMENTS 256,
APPEND_CHUNK_BUDGET_BYTES 4 MiB).

## 3. Inventory B — remote-sandbox extension (8 ops) → SandboxService

| Op | Target | Preserved mechanics |
|---|---|---|
| OpenSandbox | OpenSandbox (LOCAL, sandbox.admin) | one live token/view; TTL 1s..365d checked; default 2h; blake3-only storage; ephemeral registry (restart invalidates — documented contract behavior) |
| RenewSandbox | RenewSandbox (LOCAL) | TTL re-arming |
| CloseSandbox | CloseSandbox (LOCAL) | revocation |
| Materialize | Materialize → **server-streaming** | entries stream, summary carries {snapshot, entries, skeleton}; path safety rules documented on TreeEntry |
| FileStates | GetFileStates (BOTH, sandbox.read) | inodes = **opaque server-issued handles**, visibility-checked and bound to effective snapshot; slice {rows, content spans} |
| Changes | GetChanges | versioned-opaque ChangeBundle; refusal as ErrorInfo oneof |
| PublishProvenance | PublishProvenance | provenance.write; graph as VersionedBytes; owned SessionTurn + generation, not caller-supplied stored state |
| SubmitChange | SubmitChange | sandbox.submit; effective ViewSnapshot fence; .vault edits additionally require vault.write; **VIEW_STALE carries live skeleton**; per-repo queue; no host materialization |

Grant model → sandbox_messages.proto. Authorization matrix (token's stable view only; session
ownership on first reserve; provenance ownership; on-view checkpoint
hash visibility; admin ops local-only) → documented in provenance.proto +
sandbox message headers and CONTRACT.md, enforced server-side. iroh transport kind stays
(persistent node key, ALPN, one bi-stream per request — §8 of the handoff).

## 4. CLI command matrix (every command, verdict → coverage)

**Verdict caveat (see §9):** today's "direct-RW" verdicts reflect the
CLI's write-capable OPEN PATH (`Repository::open` always begins a write
transaction for table init + deferred-alignment recovery), not necessarily
command semantics. Roughly 18 commands are READ-ONLY-SEMANTICS despite the
RW handle — §9 reclassifies them with source evidence; their covering RPCs
are read-capped and they will run as concurrent read transactions under the
daemon, never the write queue.

### `agent` family
| Command | Verdict | Coverage | Notes |
|---|---|---|---|
| `agent enable` / `disable` | none | — | writes agent config files; exempt (local files) |
| `agent status` | none | — | SessionStore JSON under `.atomic/sessions/`; exempt |
| `agent explain [--save]` | direct-RW | ProvenanceService.GetSession / GetTurn (read) | **FLAG (§7)**: the `--save` write target (stored reasoning) needs confirmation from Aaron → candidate session-annotation op |
| `agent attest` | direct-RW — **read-only semantics** (§9) | AttestationService.ListAttestations (graph_stats filter) | list/inspect only: provenance_summary, get_view_info, iter_attestations — all reads |
| `agent lifecycle begin/renew/stop/resume/abandon/end/status` | indirect | ProvenanceService.UpdateTurn / GetTurn; declarations stay local JSON files | orchestrator IPC becomes daemon client logic; turn ops via ProvenanceService |
| 🔒 `agent database-owner start/ping/reserve/shutdown` | being REMOVED | no coverage required — the command and its daemon are fully dismantled; liveness/process lifecycle are daemon concerns (Health, Shutdown) and journal ops are ProvenanceService | hidden client verbs |
| 🔒🔒 `agent database-owner serve` | being REMOVED | dismantled; `changes.redb` ownership moves into the unified database + atomicd (Phases 1/3) | today the sole `RedbChangeStore::open` caller |
| `agent identity set/unset/show` | none | — | global config + files; exempt |
| 🔒 `agent hooks <agent> <verb>` | indirect | aggregate client: session_start/record/scope/provenance/attestation/session_end map onto ProvenanceService + RepositoryMutationService.Record + AttestationService + KnowledgeService | TurnOrchestrator's `open_existing_wait` re-opens vanish under the daemon; journal writes go through the legacy journal sink today and become direct ProvenanceService calls under the contract |

### core commands
| Command | Verdict | Coverage |
|---|---|---|
| `init` | creates | **bootstrap** — exempt (note: consider DaemonService.CreateRepository later) |
| `status [--reindex]` | direct-RO (reindex → RW) | Status; --reindex explicitly composes MaintenanceService.ReindexWorkspace then Status |
| `conflicts` | direct-RO | RepositoryQueryService.ListConflicts |
| `add` | direct-RW | RepositoryMutationService.AddFiles |
| `remove/rm [--keep]` | direct-RW | RemoveFiles |
| `move/mv` | direct-RW | MoveFile |
| `restore` (alias `reset`) | direct-RW | Restore; dry-run → RepositoryQueryService.PreviewRestore (the listing report AND the single-file pristine-bytes arm) |
| `record -m` | direct-RW | Record |
| `revise [--reword]` | direct-RW | Revise |
| `log [--all]` | direct-RO | Log (paged) |
| `change <HASH\|#N>` | direct-RO | GetChange (includes flags) |
| `diff [--stat\|--cached]` | direct-RO | DiffWorkingCopy (LOCAL); repository ref-pair Diff (BOTH) |
| `insert` (all modes + bare) | direct-RW | InsertChanges (explicit target + mandatory deps); preview → PreviewMutation; the bare promotion resolves source/parent through the promote arm server-side; multi-pick references resolve handler-side; the request's dry_run previews every arm's plan; host refresh is explicit |
| `unrecord` | direct-RW | Unrecord (explicit view); preview → PreviewMutation |
| `split [--switch]` | direct-RW | CreateView (explicit base); --switch composes SwitchView |
| `stash push/pop/apply/list/show/drop/clear` | direct-RW | CreateStash / ApplyStash / ListStashes / DropStash; pop composes apply then drop |
| `doctor repair-dependency-index/materialize-crdt/check` | direct-RW (check: read semantics) | Repair + CheckRepository (maintenance.read, BOTH) |
| `compact [--repository PATH] [--json]` | shared service (local or Reactor) | CompactDatabase (maintenance.admin, LOCAL); repeatable physical maintenance |
| `session show/fork/rebuild` | direct-RW | ProvenanceService.GetSession / ForkSession / RebuildSessionIndex |
| `sandbox create/stage/seal` | direct-RW | SandboxService.CreateSandboxTree / StageSandboxImage / SealSandboxImage (SHAPE-CONFIRM) |
| `tag create/delete/list/show` | direct-RW | TagService (create/delete/list/get) |
| `view promote` | direct-RW | ViewService.SetViewScope — **distinct from bare `insert`** (scope change vs landing changes; verified against insert.rs + promote.rs) |
| `view create` / `view split` | direct-RW | ViewService.CreateView |
| `view switch` | direct-RW | ViewService.SwitchView |
| `view delete` | direct-RW | ViewService.DeleteView |
| `view list [-v] [--remote]` | direct-RW | ViewService.ListViews — `--remote` = same RPC with hosted authority (D7 routing) |
| `triage candidates/review` | direct-RW | TriageService.ListTriageCandidates / GenerateTriageReview |
| `provenance show/trace` | direct-RW handle, read semantics | ProvenanceService.GetProvenance / ExportProvenance |
| `query search/neighbors/callers/entities/graph/embed/enrich/reindex/plan/ask` | direct-RW | KnowledgeService.QueryGraph (oneof kind; plan + ask included) / MaintainKnowledgeGraph (embed/enrich/reindex) |
| `query code` / `query index` | none (file index) | QueryGraph content_search reads a view-scoped index; local file-index construction remains client-side, never inside a read RPC |
| `vault init/show` | direct-RW/RO | VaultService.InitVault / GetVaultEntry |
| `vault list/materialize/sync/summaries` | direct-RW | ListVaultEntries (summaries mode) / ExportVault / SyncVault |
| `vault goal start/stop/resume/list/show` | direct-RW | UpdateVaultEntity (kind=GOAL, status transitions) / ListVaultEntries / GetVaultEntry |
| `vault context` | direct-RO | GetVaultContext |
| `vault query ...` | direct-RW | (alias of `query`) KnowledgeService |
| `vault intent` / `vault memory` shims | none | redirect text only; exempt |
| `intent new/attest/update/delete/link` | direct-RW | CreateVaultEntity / AttestationService / UpdateVaultEntity / DeleteVaultEntity / LinkVaultEntities |
| `intent validate` | forks: path-arg none; ID-arg RW | ValidateVaultEntity (both forms; db-validated vs bytes-only via oneof) |
| `intent show/list` | direct-RO | GetVaultEntry / ListVaultEntries |
| `intent verify` | direct-RW | AttestationService.VerifyAttestation |
| `memory new/show/write/update?` | direct-RW | CreateVaultEntity / GetVaultEntry / UpdateVaultEntity |
| `memory validate/verify` | direct-RW | ValidateVaultEntity / AttestationService.VerifyAttestation |
| `memory attest/list` | direct-RW | AttestationService.RecordAttestation / ListVaultEntries |
| `memory kinds` | none | static output; exempt |
| `completions <shell>` | none | static script; exempt |
| `update [--check]` | none | install-source detection; exempt |
| `COMPLETE=<shell> atomic` (dynamic completion) | direct-RW | covered by ViewService.ListViews + RepositoryQueryService.Log (client composes) — no new RPC; optional CompletionHints later if round-trips hurt |

### git interop

**CLI-boundary helpers (the seam the daemon replaces):** today every command
runs `find_repository_root` then `open_repository`/`require_repository`/
`open_readonly_repository` to open redb in-process; under the contract the
flow becomes explicit RegisterRepository when needed, then ResolveRepository
(cwd path → persistent RepositoryRef + WorkspaceInfo, lookup only) → covering RPC.
CLI normalizes subdirectory-relative paths; daemon cwd is irrelevant. The daemon's
repository registry maps the persistent ID to its lazily-opened handle —
one daemon, many redb files (RFC D3).

| Command | Verdict | Coverage |
|---|---|---|
| `git import [--all/--incremental/--dry-run/--with-crdt]` | direct-RW (or creates) | ImportFromGit (bootstrap requires destination); --dry-run → PreviewGitImport |
| `git push [-m/--no-push/--branch]` | direct-RW | GitInteropService.PushToGit |
| `git hooks install/uninstall/status` | none | `.git/hooks` file shims; exempt |

### remote / storage
| Command | Verdict | Coverage |
|---|---|---|
| `push [--remote]` | direct-RW + HTTP | SyncService.PushChanges (hosted server = receiving side of same contract) |
| `pull [--remote]` | direct-RW + HTTP | SyncService.PullChanges (sidecar import via ProvenanceService/AttestationService on apply) |
| `clone <url>` | creates + HTTP | **bootstrap** — exempt; fetch/apply/sidecar phases covered by PullChanges |
| `remote add/remove/rm/set-url/rename/default` (+ bare list) | direct-RW (list: read-only, §9) | ManageRemotes (mutations) + ListRemotes (list, repository.read) | remotes persist in the repository DB |
| `server add/set/show/remove/set-identity` | none | `~/.atomic/config.toml`; exempt (client config) |
| `identity` ×13 (new/list/show/default/delete/whoami/register/sign/verify/agent/grant/delegate/lookup-key) | none | IdentityStore is file-based — no redb → exempt. **FLAG (§7)**: hosted identity/grant management (currently HTTP) is a candidate for the future HostedService family — Lee decision |
| `workspace` ×8, `project` ×4, `org` ×8, `team` ×7 | none (HTTP StorageClient) | no local redb → out of the redb rule; **FLAG (§7)**: hosted-management RPC family (workspaces/projects/orgs/teams) is an open Lee decision for the unified hosted surface |
| `project init` | direct-RW | SyncService.BindRemoteProject |

## 5. Hidden surfaces audit (§10.9 support)

Surfaces receiving external input beyond the two inventories: `agent hooks`
stdin receiver (aggregate client — §4 above); the completion engine (covered
by ListViews+Log); `agent lifecycle` JSON IPC (covered via UpdateTurn);
git hook shims (call back into `git import --incremental` — covered);
`/code` HTTP sync endpoints (the hosted server side — the SyncService
receiving contract, server not in this workspace); storage-management HTTP
(FLAGGED §7). The legacy database-owner daemon (being fully dismantled with
its `agent database-owner` command) served the same single-writer purpose
atomicd now owns. **Finding: no third RPC surface exists beyond the two
inventories in this workspace** — supports Aaron's expected confirmation.

## 6. Combine decisions (the "don't build a hundred same RPCs" ledger)

| Today | One RPC | Rationale |
|---|---|---|
| Ping + Pong flags | Health + GetCapabilities | flags → negotiated feature tokens + method matrix |
| LoadFrozenEnvelopes + Page | LoadFrozenEnvelopes (paged-only) | paging is the contract from day one; cursor absent = first page; the unpaged compat mode is dropped with the dismantled protocol |
| Stop/Resume/AbandonTurn | UpdateTurn (action enum) | no failpoints on those seams; identical request shape |
| turn status | GetTurn | read side |
| insert: bare/single/from-view/tag/multi-pick | InsertChanges (source oneof) | the promote arm resolves the bare promotion server-side; raw single/multi references resolve handler-side; dependency closure mandatory |
| insert/unrecord previews | PreviewMutation (operation oneof) | read permission/effects; no apply flag or idempotency writes |
| `split` + `view create` | CreateView (base oneof) | alias mapping; optional switch is explicit SwitchView |
| stash ×7 | 4 RPCs | pop composes apply then drop; show=list-detail; clear=drop-all |
| query ×11 | QueryGraph + MaintainKnowledgeGraph | 9 read kinds oneof; 3 rebuild actions enum |
| intent/memory/goal verbs | VaultService entity model (kind enum) | one entity store, one lifecycle; attest→AttestationService targets |
| goal ×5 | folded into Update/List/Get (GOAL kind) | status transitions, not separate ops |
| remote ×6 | ManageRemotes (mutations) + ListRemotes (read) | registry CRUD; the list action is split out so a read never gates on repository.write or enters the write queue (§9) |
| doctor ×3 | Repair (rebuild/materialize) + CheckRepository (check) | maintenance batch; check is doc'd read-only in source — split so it executes as a read transaction (§9) |
| view list `--remote` | ListViews with hosted authority | D7 routing, not a second method |
| GetChange sections | includes flags | per-section verbs avoided |

Deliberately NOT combined: Record/Insert/Revise/Unrecord (distinct domain
semantics per RFC D5); prepare/bind/acknowledge (failpoints); Open/Renew/
Close grant admin (distinct result payloads — mint vs extend vs revoke);
Status vs Log vs Diff (different reads), repository vs working-copy Diff, and
preview vs mutation (different authorization/effects).

## 7. Flags for Lee / Aaron / Vince (from this pass)

1. **`agent explain --save`** write target (stored reasoning) — needs Aaron's
   input; candidate: session-annotation op in ProvenanceService.
2. **Hosted management family** — workspace/project/org/team are HTTP-only
   today (no local redb) — do they join a HostedService proto family for the
   unified hosted surface, or stay HTTP? (RFC lists "hosted collaboration".)
3. **IdentityService** — file-based identity store means no redb RPC needed
   (exempt); hosted identity/grant management is the open question (see 2).
4. **Package naming** — `atomic` (RFC style, inherited) vs `atomic.v1`
   (versioned convention); flip before first tagged release if desired.
5. **Capability vocabulary** — dot-style carried; RFC's authorization section
   is colon-style; ratify one.
6. **SHAPE-CONFIRM markers** (sandbox trees, vault entry shapes, triage
   layers, session ledger, publication records) finalize in their slices
   against source; flagged inline in the protos.
7. **Zero-capability negotiation pair** — Health/GetCapabilities declare
   zero required capabilities (RFC says "one or more"); carve-out flagged.
8. **Clone/init** remain bootstrap-exempt; consider CreateRepository +
   hosted-init RPCs later.
9. **Repository identity bootstrap** — persistent IDs are assigned at init
    or explicit `RegisterRepository`; the working copy records its ID
   under `.atomic`. Confirm with the Phase 1 colleague: who writes the ID
   record (init vs daemon), its filename, and whether pre-existing repos
   migrate IDs during Phase 1 consolidation.

**Counts:** 94 RPCs cover the 21 inventoried ops of the two legacy surfaces
(semantics carried, machinery not) + every redb-touching CLI path (~85
direct/indirect paths); ~40 commands are file/config/presentation-only and
stay client-side (exempt with reason).

## 9. RW-handle audit — read-semantics classification (a list never does a write)

Aaron's principle (2026-10-02): an op whose semantics are read-only must
(a) be gated by a read capability, (b) execute as a concurrent read
transaction under the daemon — never the per-repo write queue — and
(c) never require a write capability to read. Source sweep of
/Users/aaron/code/work/atomic confirmed: today's RW verdicts for the
commands below come from the write-capable OPEN PATH (`Repository::open`
begins a write txn for table init + deferred-alignment recovery,
pristine.rs:128, repository/mod.rs:390), NOT from command semantics — their
bodies only call read methods. The daemon must serve them read-only.

| Command | Classification | Covering read-capped RPC |
|---|---|---|
| `agent attest` (all flags) | read-only | AttestationService.ListAttestations (attestation.read) |
| `session show` | read-only (fork/rebuild stay mutations) | ProvenanceService.GetSession (provenance.read) |
| `tag list` / `tag show` | read-only | ListTags / GetTag (repository.read) |
| `view list` (local + --remote redb path) | read-only | ListViews (repository.read) |
| `vault list` / `summaries` | read-only | ListVaultEntries (vault.read) |
| `vault materialize` | repository-read; writes working-copy FILES + daemon recovery ledger | ExportVault (vault.read + working-copy.write, never repository writer) |
| `query search/neighbors/callers/entities/graph/plan/ask` | read-only | KnowledgeService.QueryGraph (knowledge.read) |
| `intent verify` | read-only (id-arg; path-arg needs no redb) | VerifyAttestation (attestation.read) |
| `memory show/list/validate/verify` | read-only | vault.read / VerifyAttestation |
| `triage candidates` / `triage review` (all output modes) | read-only (review doc'd "pure and read-only", triage/project.rs:1-7) | TriageService (triage.read) |
| `doctor check` | read-only ("Read-only… Mutates nothing", doctor.rs:36-46) | MaintenanceService.CheckRepository (read execution) |
| dynamic completion (`COMPLETE=`) | read-only | ListViews + Log |

Not "pure" reads despite the read-redb classification (documented, not
reclassified): `vault materialize`, `query graph --output`, and
`triage review --html` write FILES to the working copy;
`query ask`/`plan(vector_search)` call external LLM/embedding APIs.

Earlier contract fixes driven by this audit: **ListRemotes** split out of
ManageRemotes (repository.read; enum value 6 reserved) and
**CheckRepository** split out of Repair (read execution; enum value 3
reserved). The follow-up §10 found remaining exceptions (status reindex,
registration, previews, preparation/check permissions) and closes them.
PrepareAttestation now uses attestation.read (not merely an execution comment).
Read-verb repository
methods verified pure read_txn: vault_kg_*, vault_list/retrieve/manifest,
list_views/get_view_info, get_tag*/list_*_tags, log,
get/list_session_ledger(s), find_attestations_for_view,
triage_candidate_set, status.

## 10. Contract hardening after the read/write and sandbox audit

The authoritative behavior is [CONTRACT.md](CONTRACT.md), revision 2. Old draft
behavior must fail version negotiation; reserved protobuf fields alone do not
prevent an old dry-run caller's flag being ignored by a new apply handler.

| Boundary | Contract change |
|---|---|
| Placement vs caller trust | Every method declares allowed_caller (51003) independently of execution_scope; LOCAL methods explicitly deny network/sandbox callers |
| Storage vs filesystem effects | Every method declares effect (51004); reads cannot acquire even an aborted writer; workspace effects stay local |
| Remote sandbox | Data BOTH, not hosted-only REMOTE; local grant admin preserved; token lives in shared middleware metadata across services |
| Sandbox vault writes | View-scoped Init/CRUD/link/read/validate BOTH and sandbox-allowed; .vault submissions update only target-view revisions/index |
| Broad repository rights | SubmitChange uses sandbox.submit, not repository.write; sandbox.read no longer authorizes provenance publication |
| Stable targets/fencing | ViewRef + effective ViewSnapshot covers ancestor visibility and vault revisions; lifecycle/publication generation; publication accepts SessionTurn only |
| Read/write split | Status/ReindexWorkspace; Resolve/RegisterRepository; PreviewMutation/PreviewRestore/PreviewGitImport; prepare and check use read caps |
| Workspace authority | Separate DiffWorkingCopy; remove switch_after/drop_after; no host materialization from sandbox submit or repository insert/pull |
| Remote repository operations | Create/Delete/SetViewScope, InsertChanges/Unrecord, vault DB mutations, ForkSession, triage and CheckRepository now BOTH |
| Multi-repo routing | Resolve returns WorkspaceInfo; root-relative paths; Git bootstrap requires destination; duplicate physical stores cannot silently share IDs |
| Replay | ResponseMeta on mutations; fingerprint/scope by repository/principal/grant/method; auth before replay |
| Signing/events | Public prepare + caller-held key; no daemon signer under provenance.read; consumer checkpoint has its own capability/ownership |

`python proto/check_contract.py` compiles 36 protobufs and checks all 94 method
descriptors, including the exact 32-method sandbox allowlist. It checks contract
declarations, not an RPC implementation. Content validation, view-index isolation,
leases, snapshot calculation, replay storage and failpoint parity remain slice-
level implementation gates. This pass does not migrate Atomic storage.

## 8. Git-pattern audit — Atomic is patch theory + a semantic change graph, NOT git

Rule going forward: check the atomic model in source before naming a field;
do not import git vocabulary by reflex. Findings from the 2026-10-02 sweep
(all fixed; shapes verified against /Users/aaron/code/work/atomic and
compiled through the descriptor):

| Git-shaped pattern (was) | Atomic's model (source) | Fix |
|---|---|---|
| change `parents` (commit-DAG thinking) | a change has NO commit-parents: HistoryEntry = sequence + hash + the Merkle state AFTER applying it, ordered by the per-view merkle chain (atomic-repository/src/history/types.rs:79); graph edges are dependency hashes from StoredChangeMeta's hash table — index 0 = self, 1+ = deps (redb_change_store/mod.rs:181) | ChangeLogEntry/ChangeInfo carry sequence/state/dependencies; descriptor sweep confirms zero `parents` fields remain |
| DID-shaped `author` | ChangeHeader carries message/description/timestamp/authors; authors are name/email records (atomic-core/src/change/header.rs:197) | `Author {name, email}` replaces Identity in change-domain messages |
| invented file-status states | FileStatus = Modified/Deleted/Untracked/Added/Conflicted (status.rs:136); FileStatusEntry = path/status/inode/recorded_hash/current_hash/details (status.rs:263); RepositoryStatus carries stale_index_count (status.rs:390) — the `--reindex` driver | StatusResponse, FileStatus, FileStatusEntry reshaped to source; stale_index_count added; ChangeRef.ordinal → sequence (the CLI's #N) |

**Deliberately kept** (atomic's own verbs, source-verified — not git
imports): record/insert/revise/unrecord (the change lifecycle), stash
(implemented as orphan-VIEW parking of uncommitted work — patch-theory
native, commands/stash.rs), restore, tag (named Merkle-state snapshots),
push/pull/remote/clone (atomic's sync model), view promote/insert.
Node-level "parents"/reparenting in atomic-core is CRDT vertex-edge
machinery — internal to graph sections, which stay versioned-opaque on the
wire. git_interop is the explicit, contained bridge.
