# libatomic/proto — the Atomic protobuf service definitions

The one protobuf contract that `atomicd` serves: local daemon and hosted
server, UDS / named pipe / TLS / iroh. **There is no second protocol.**

This tree is the contract's single source of truth: it lives in the atomic
repo inside `libatomic` (the crate that owns the codegen AND the handlers),
compiled by `libatomic/build.rs` (protox — no system protoc) and gated by
`check_contract.py`. There is no vendored copy synced into any other repo.

## Layout

```
proto/
  README.md                     <- this file
  COVERAGE.md                   <- CLI command → RPC matrix (the alignment artifact)
  CONTRACT.md                   <- middleware/domain enforcement obligations
  check_contract.py             <- protobuf compilation + descriptor contract gate
  atomic/
    common/                     <- shared primitives
      options.proto             scope (51001), capabilities (51002), callers (51003), effects (51004)
      common.proto              repository/view/snapshot IDs, request/response metadata, hashes and bytes
      change.proto              shared change-domain types — source-verified shapes (ChangeInfo, ChangeLogEntry, ConflictInfo, Author)
      error.proto               stable error model (provenance-documented vocabulary)
      pagination.proto          PageCursor {seq, offset} — seeded from the frozen-page cursor
      event.proto               Reactor event envelope
    services/<service>/         <- one folder per service
      <svc>_service.proto       the service definition — RPCs only
      <svc>_messages.proto      that service's request/response messages
```

Services: daemon (Health, GetCapabilities, ResolveRepository, RegisterRepository, ListRepositories,
Shutdown) · provenance (the provenance tracking contract: journal, checkpoint
phases, turn lifecycle, session ledger, projection) · sandbox (grants + local
trees + remote data ops) · repository_query (Status, Log, GetChange, Diff
stream, conflicts) · repository_mutation (record/insert/revise/unrecord/
add/remove/move/restore/stash) · view · tag · vault (intents/memories/goals as
ONE entity model) · knowledge (11 query verbs → 2 RPCs) · attestation
(prepare/signed external signing) · sync (push/pull/remotes/project binding)
· git_interop · triage · maintenance · reactor (CONTRACT DRAFT — server lands
Phase 3).

94 RPCs across 15 services. **Contract version 2**: this hardening changes draft
behavior; removed field numbers are reserved. Older draft clients MUST fail
version negotiation, not send an ignored `dry_run`/`drop_after` field to a writer.
See [CONTRACT.md](CONTRACT.md) for normative middleware/domain requirements.

Compile and descriptor-contract check (temporary output only):

```bash
# TEMP_DIR is an existing temporary directory outside the repository
python3 -m venv "$TEMP_DIR/proto-venv"
"$TEMP_DIR/proto-venv/bin/pip" install grpcio-tools
"$TEMP_DIR/proto-venv/bin/python" proto/check_contract.py --temp-dir "$TEMP_DIR"
```

The import root is `proto/`; imports read `atomic/common/common.proto`.
The checker verifies all four annotations, read/write permissions, sandbox
allowlisting, metadata echo, retired fields, routing and snapshot/generation
fencing. It does not claim to test an RPC server that has not been implemented.

## Conventions

- **Add-only.** Field and extension numbers are never reused or renumbered.
  51001/51002 are the permanent method-option allocations.
- **Package `atomic`** (RFC examples' style; versioning happens at the crate
  boundary — revisit with Lee before the first tagged release if desired).
- **Capability tokens are dot-style** (`repository.read`). The RFC is
  internally inconsistent (dot in its method-option examples, colon in its
  authorization section); dot style is carried from the parked Slice 0 draft
  and flagged for ratification.
- **Every method declares** `execution_scope`, `allowed_caller`, `effect` and
  `required_capability`, with
  one documented carve-out: `Health`/`GetCapabilities` declare zero
  capabilities (the negotiation pair — gating the handshake on a capability
  would be circular).
- **Every repository request opens with** `RepositoryRef repository = 1`;
  daemon/bootstrap and Reactor consumer/event operations are the exceptions.
  One daemon multiplexes many repositories — the
  persistent `repository_id` resolves to exactly one lazily-opened handle
  (RFC D3), local clients discover theirs via `ResolveRepository` (path →
  ref + workspace context, lookup only) or the `.atomic` ID record. Explicit
  `RegisterRepository` handles registration/legacy ID assignment. Idempotency
  state is scoped by authority/repository/principal/grant/method. Every ordinary
  mutation carries `RequestMeta` (idempotency key + caller-supplied observed time)
  and returns `ResponseMeta` (field 100). View-dependent mutations require the
  matching snapshot; other preconditions are defined by the operation.
- **Remote clients never select server filesystem paths.** Working-copy
  operations require a registered workspace ID and root-relative paths and are
  local-caller-only. Logical graph paths/index filters are not server paths.
- **LOCAL is placement, not caller trust.** It means local-daemon deployment;
  REMOTE means hosted-only; BOTH includes an atomicd accepting iroh requests.
  Sandbox data stays BOTH. Grant administration and host filesystem operations
  separately declare local callers only. Repository-only view lifecycle,
  insertion/unrecord, vault mutations, session fork and triage are BOTH.
- **Sandboxes can write the vault.** Vault RPCs name a stable view. Grants can
  include vault.read/write, knowledge.read and attestation permissions as well
  as sandbox/provenance permissions. Edits/deletes/links affect only the granted
  view's entity revisions; inherited entries are overlaid, never overwritten.
  SubmitChange also accepts `.vault` changes with vault.write. Materialization
  and ExportVault on the VM are client work, not host filesystem permissions.
- **Effective-view fencing.** ViewSnapshot covers inherited graph state and the
  view-scoped vault revision. Own-log Merkle state alone is not a draft fence.
- **ErrorInfo** rides transport failures; domain refusals that must carry
  recovery payloads (e.g. stale-view skeleton) travel as `oneof` outcomes in
  the response.
- **Atomic vocabulary, not git.** Atomic is patch theory and a semantic
  change graph: changes have NO commit-parents (their position is a view-log
  sequence + post-application Merkle state; their graph edges are dependency
  hashes), authors are name/email records, and file statuses are
  Modified/Deleted/Untracked/Added/Conflicted. Check the atomic source before
  naming a field — see COVERAGE.md §8 for the audit that fixed the first
  draft's git-shaped fields.
- **Reads are reads.** An op whose semantics are read-only rides a read
  capability and executes as a concurrent read transaction — never the
  per-repository write queue — and no read action hides inside a
  write-gated RPC. Status has no reindex flag; previews have no apply flag;
  prepare/check operations use read permissions. Declared read effects prohibit
  lazy repair, cache/index writes and aborted write-transaction projections.
  ExportVault may write workspace files and daemon replay metadata, but not the
  repository DB. See COVERAGE §10 for the hardening of the earlier §9 audit.
- **SHAPE-CONFIRM comments** mark message fields drafted from the handoff or
  CLI shape but not yet verified against source — each names the slice that
  finalizes it.

## Provenance tracking semantics (what this contract must never lose)

The legacy agent journal protocol and the `atomic agent database-owner`
command that served it are being **fully dismantled** — atomicd becomes the
only storage owner. What carries forward is the provenance tracking core
(types verified against the provenance store source,
`atomic-repository/src/redb_change_store/provenance.rs`): generation fencing
on every mutation; request-ID echo + replay detection; caller-supplied
`now`; frozen-envelope paging with fragment reassembly and strict
continuity checks (paged-only — a complete journal may arrive as one page);
the checkpoint seams (prepare/bind/acknowledge stay SEPARATE RPCs — the
crash-injection story lives on those seams); the additive capability-flag
pattern, generalized into `GetCapabilities`' method matrix.

From the remote-sandbox extension (HANDOFF §5, Aaron's unmerged branch):
view-scoped identity-bound time-boxed grants; ephemeral fail-closed tokens;
the authorization matrix (session/provenance ownership, on-view checkpoint
checks, local-only admin); stale-view refusals carrying a live skeleton;
materialize path safety (plain relative components, reserved names,
symlink-parent escape refused).

## §10 recommendations carried (on record from REAC::aaron::2, pending Lee)

1. inodes → opaque server-issued handles (skeleton-issued, like cursors)
2. sandbox tokens stay ephemeral — restart invalidates; contract behavior
3. V3 change bytes → versioned-opaque (`ChangeBundle.schema`, like the event type URL)
4. compat window → decide before a second release; N-1 default shape
5. database filename → RFC `repository.redb` wins; align with Phase 1 now
6. harness → grpcurl + server reflection (`harness_reflection` feature token)
7. the contract's source of truth lives in libatomic (this crate) — the
   reactor's draft tree was absorbed when libatomic was created
8. slice order → HANDOFF §11 (Slice 0 done; Status next; then the provenance port)
9. hidden surface → Aaron confirms no RPC surface outside the two inventories
   (this audit's finding: none found beyond them — see COVERAGE.md §5)

## Next steps

1. Lee/Aaron/Vince review: COVERAGE.md gaps-and-combines section (§6),
   capability vocabulary, package naming.
2. Slice 1 (`RepositoryQueryService.Status`) becomes the first CLI-vs-RPC
   parity target.
3. Slice 2 = the ProvenanceService port with failpoint parity.
