# Atomic RPC contract — revision 2

This specifies obligations for the generated adapters, middleware and domain
handlers. The protobufs expose targets, fences and permitted effects;
`check_contract.py` checks those declarations. Payload validation, leases,
transaction choice, filtering and replay enforcement remain implementation work.

Grounding: `~/REACTOR_HANDOFF.md` §§2, 5, 9, 12 and the unified-service RFC D5–D8.
The legacy owner process/protocol is dismantled; its provenance/isolation
semantics survive. Atomic is a patch graph with view filters, not Git branches
or independent per-view databases.

## Four independent checks

| Declaration | Meaning |
|---|---|
| execution_scope | LOCAL daemon, REMOTE hosted server, or BOTH deployments |
| allowed_caller | Explicit eligibility of local, remote-identity, sandbox callers |
| required_capability | All listed permissions must be granted on this resource |
| effect | Maximum permitted storage, workspace, daemon and external effects |

Missing/UNSPECIFIED annotations fail closed. An endpoint may advertise fewer
methods but cannot expand caller eligibility or effects. The advertised matrix
comes from the descriptors. A client preflight does not replace server checks.

LOCAL does **not** authorize all callers reaching atomicd. The network listener
uses the same handlers, but local-only methods reject network principals even
if they possess similarly named capabilities. BOTH does not grant a sandbox
access to every repository operation. REMOTE is not used for sandbox data:
that would incorrectly exclude the serving local atomicd.

## Middleware and request metadata

- `GetCapabilities.protocol_version` is **2**. Every non-negotiation call must
  carry `x-atomic-contract-version: 2` in gRPC metadata. Missing or mismatched
  versions fail before dispatch. The old draft's removed flags must never be
  ignored and then applied as writes. This is a behavioral incompatibility,
  even though removed field numbers are reserved rather than reused.
- Local transport authenticates an OS principal; TCP authenticates over TLS;
  iroh authenticates its encrypted peer connection. Socket access alone grants
  no repository permission. Proxies preserve the originating principal, selected
  authority, version, request ID and correlation, with verified delegation.
- Sandbox credentials use `x-atomic-sandbox-token-bin` (binary metadata) on **all**
  sandbox calls, including Provenance/Vault/Knowledge/Attestation services. Other
  remote identities use `authorization: Bearer ...` with verified audience.
  Multiple/conflicting credentials are refused. A restricted token is never
  reclassified as an unrestricted remote identity. Body tokens are retired.
- Resolve credentials and check the method allowlist, resource grant and expiry
  before opening a repository or looking up replay results. A sandbox cannot
  cause arbitrary IDs to be opened. Revalidate grant/resource validity at
  publication, including expiry/revocation after time spent in a write queue.
- Observation timestamps remain caller supplied. Grant TTL, leases, deadlines,
  authorization and ingest time use the trusted server clock instead.
- Ordinary mutations require a nonempty UUID request_id. Responses carry
  ResponseMeta at field 100; refusals/errors echo in ResponseMeta/ErrorInfo.
  Replay keys bind authority, repository (or daemon/bootstrap namespace),
  principal/grant, method and request_id. Fingerprint canonical request contents;
  changed contents fail IDEMPOTENCY_MISMATCH. Replays require current authority.
  Reactor publish uses principal-bound event_id deduplication instead.
- Grant-admin replay is process-lifetime-only, matching ephemeral grants. The
  authorization registry stores token hashes; any cached issuance reply is held
  only in memory, never in a durable outcome/event log. Replacement, revocation,
  expiry or restart refuses stale issuance replay; local administration uses a
  new request ID to mint a fresh grant.
- Repository-write replay outcomes/outbox commit with the mutation. Filesystem-
  only/external operations use the daemon outcome ledger and explicit recovery;
  a request ID does not magically make filesystem or network effects ACID.
- Cancellation before publication performs no mutation. After a durable commit,
  a lost response is recovered by replay, not by repeating effects. Bounded
  stream/message/queue limits apply per principal; slow streaming cannot keep a
  writer open. Negotiate limits before allocating opaque payloads.

## Repository and workspace routing

RepositoryRef.authority selects an explicit configured authority; it is not an
arbitrary forwarding URL. repository_id resolves one authorized physical store.
Aliases are canonicalized; duplicate IDs at different stores are refused.
Normal handlers use the persistent owner and transactions, never per-request
Repository::open or an embedded-storage fallback.

ResolveRepository is a local **lookup**, returning RepositoryRef and WorkspaceInfo
(root, relative cwd, current ViewRef). It does not register, initialize, migrate,
repair, or align a tree. RegisterRepository is an explicit local administrative
mutation that verifies ownership and maps the repository/workspace; legacy IDs
are persisted to the registry/.atomic record. Schema migration is separate.

The CLI converts paths from its source subdirectory to workspace-root-relative
paths. Workspace IDs bind repository, root and current view; cross-repository
IDs are refused. No operation uses atomicd's cwd. Bootstrap Git import requires
an explicit destination when no RepositoryRef exists. Physical paths occur only
in locally authorized operations; content search paths are logical index filters.

## Sandbox grants and visibility

The issuing local admin supplies a stable ViewRef and an explicit capability
subset. The grant binds authority/repository/view ID, optional acting identity,
capabilities, and server-clock expiry. The only delegable tokens are:

```
sandbox.read  sandbox.submit
provenance.read  provenance.write  agent.checkpoint
vault.read  vault.write
knowledge.read
attestation.read  attestation.write
```

Never delegate repository.*, working-copy.*, view lifecycle, daemon, maintenance,
sync, Reactor, Git interop, or sandbox.admin rights to a sandbox token. If the
issuer cannot delegate a requested capability on that view, reject the grant.
An empty permission set is invalid; clients choose their desired subset.
SandboxOpened.repository has no workspace_id; sandbox requests containing one
are refused rather than interpreted as host workspace selection.

One live token per (repository, stable view ID); mint replaces, renew does not
change permissions/identity, close revokes. The grant registry is hash-only and ephemeral:
daemon restart invalidates them. Deleting a view invalidates its grants and
associated ownership; recreating the same name cannot recover that authority.
Reissuing a token does not claim previously local/foreign sessions. Session and
turn ownership checks use repository + stable view ID, never just a name.

For SandboxService, omitted target derives from the grant; other principals must
name one. An explicit target must equal the grant. For Vault/Knowledge/Attestation
requests the required view must match. Session/turn operations verify ownership,
even when the request has only a provenance/session ID. PrepareCheckpoint checks
explained changes, previous provenance and plan links; BindCheckpointHash must
refer to the prepared turn/attempt, never replace another SessionTurn.

Every requested inode/change/node is checked against the authorized **effective
filter**, including inherited dependencies, before returning content or metadata.
An inode is a domain handle, not a capability; knowing one or a hash grants no
access. QueryGraph filters nodes/edges/snippets and each plan step before external
inference. Provider calls may use authorized context only; read plans cannot
invoke maintenance, arbitrary paths or arbitrary network tools.

## Sandbox and vault writes

Sandbox SubmitChange parses and verifies schema, hash, dependencies, referenced
graph nodes, all path-bearing operations and reserved bookkeeping names. Validate
the effective snapshot, permissions and graph references again inside the short
publication transaction. Refusal leaves graph, view, vault and outbox unchanged.
Dependency closure is mandatory; a client flag cannot bypass patch correctness.

Publication may append global graph data and target-view membership: that is
Atomic's ambient graph model. It cannot promote into another view, overwrite
existing content-addressed objects, switch current_view, or materialize the host
working tree. Workspace refresh is a separate local authorization/lease.

**Vault writes from a sandbox are supported.** Direct VaultService CRUD/status/
link operations and submitted `.vault` edits both create target-view entity
revisions and update that view's index. Inherited entities are edited through
target-local revisions, deleted with target-local tombstones and linked only to
visible targets. Neither path replaces ancestor/sibling entities or indexes.
Represent visibility using Atomic's graph/revision model, not Git-like independent
vault copies. Writes require vault.write; SubmitChange requires it additionally
when a bundle changes `.vault`. Promotion is a separately authorized operation.

Direct RPC writes return the new ViewSnapshot in ResponseMeta. Reads/cursors are
view-scoped. InitVault creates view storage; it does not scaffold host files.
SyncVault imports host workspace files only for a local principal. ExportVault
reads the view and writes local workspace files only. Remote VM materialization
and file edits are client work; no VM path is interpreted as a host destination.

Attestation preparation returns public signing bytes under a read capability.
Recording verifies the supplied signature against the current target, expected
snapshot and signer authorization. Vault attestations are view-local. A sandbox
signer must match its acting identity; unbound grants cannot use daemon default
human keys. ExportProvenance returns unsigned data for caller-side signing.

PublishProvenance accepts a SessionTurn and expected_generation, not a caller-
supplied StoredProvenanceTurn. Derive state, counters and checkpoint attempts
server-side. Lifecycle stop/resume/abandon also require expected_generation.
Keep prepare/bind/acknowledge separate for crash-injection parity.

## Snapshot and effect semantics

ViewSnapshot is a server-derived canonical digest covering repository/stable target ID,
scope, contributing ancestor IDs/states, visible dependency closure and effective
vault revision. Own Merkle log state is diagnostic, not enough to fence a draft.
View-dependent mutations require the matching snapshot; rejecting absence or a
different target is a domain precondition, despite proto3 message optionality.

Materialize entries/summary/skeleton come from one coherent perspective. Slices
and continuation handles bind to it. On SubmitChange staleness, return the live
skeleton with VIEW_STALE. Rendering a non-current view is a read-only projection,
not an aborted redb write transaction. Client materialization retains lexical,
reserved-name, symlink-parent and final-write path safety checks.

Repository reads never acquire a write permit or persist repairs/stat caches.
Repository writes queue per repository and commit the outbox together. Workspace
mutations lease the workspace first; filesystem-only writes and mutable tracking/
index state changes also lease it. Lock
order remains workspace lease → repository permit → redb write transaction.
External providers and daemon bookkeeping are separately declared effects.

Status does not reindex. ReindexWorkspace is explicit. PreviewMutation (insert/
unrecord), PreviewRestore and PreviewGitImport cannot apply or create a repository.
Diff is repository-only; DiffWorkingCopy is local. CreateView does not switch;
ApplyStash does not drop. CLI composition preserves commands without hiding extra
authority in bool flags. InsertChanges/Unrecord/PullChanges do not refresh host
files; explicit Restore/SwitchView is separately authorized.

## Reactor principal boundaries

Inbound authenticated_principal and daemon-assigned sequence/time assertions are
refused. Derive them from middleware. source/delegation and every repository in
PublishStream are authorized independently; outer/inner repository IDs must agree.
Deduplication never returns another principal's private outcome on an ID collision.

Consumers are owned by authenticated principal; read filters and replay never
expand that ownership. CheckpointConsumer requires reactor.checkpoint, a request
ID and monotonic progress bounded by delivered state. Merely subscribing grants
no checkpoint-write authority. Sandbox tokens cannot invoke Reactor operations.

## Implementation verification still required

Descriptor checks are an early contract gate. Each server slice must additionally
prove caller/resource filtering, cross-repository/workspace refusal, malformed
payload refusal, view-local vault writes, effective-ancestor staleness, expiry
while queued, restart/revocation, replay fingerprint isolation, read/write queue
behavior, filesystem recovery and checkpoint failpoint parity. They are runtime
tests, not claims that annotations alone implement those behaviors.
