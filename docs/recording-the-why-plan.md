I'm on the correct branch and the workspace layout matches the findings. I have everything I need to produce the plan.

# Recording the Why — Implementation Plan

Branch: `feat/recording-the-why` (atomic core + CLI). Grounded in the design doc `docs/recording-the-why.md` and the six research findings.

---

## 1. Architecture summary

RtW is **~65% a re-typing + lifting exercise over substrate that already ships, and ~35% net-new cryptographic-canonical + semantic-web machinery.**

**Reused / extended (do not rebuild):**
- **Round-trip engine.** The path-keyed redb `VAULT_ENTRIES` store plus the `vault_materialize` (redb→disk) / `vault_scan_working_copy` + `vault_record_working_copy` (disk→redb) cycle in `atomic-repository/src/repository/vault.rs` *is* RtW's "markdown in, canonical stored, render out, edits reconciled back." The lift slots into the scan/record path; the render into `render_entry_to_markdown`.
- **Hash primitive.** `Hash = Merkle` over BLAKE3 (`atomic-core/src/types/hash.rs:51,113`) is exactly RtW's `blake3:` — only the *input* changes (JCS-canonical node bytes instead of raw body).
- **Closed vocabulary seed.** `atomic-core/src/pristine/ontology.rs` already holds PROV/SKOS/DC prefixes, PROV-O predicate strings, vault entity types, and `is_known_predicate`/`all_predicates`. RtW hardens this from advisory into the single source of truth.
- **Identity/signing.** `atomic-identity` (`keypair.rs`, `signing.rs`, `identity.rs`) gives Ed25519 + `IdentityId = blake3(pubkey)` — the exact primitive for `eddsa-jcs-2022`. We add only a canonicalization + multibase + `did:atomic` adapter layer on top.
- **Provenance capture.** `atomic-agent/src/provenance/*` + `atomic-core/src/change/provenance_graph/*` already capture a content-addressed, chained, per-change causal DAG with turn boundaries across 14 agent adapters. RtW re-projects this into W3C PROV JSON-LD rather than inventing capture.
- **Projection tier.** The `(KgNode, KgEdge)` property graph + `kg_neighbors` BFS + `query_plan.rs` executor are the ready-made flywheel-query surface.

**Net-new (nothing exists — grep-confirmed zero hits):** JSON-LD typed-node type + `@context`; JCS (RFC 8785) canonicalizer; content-hash-over-canonical-node (today's hash is BODY-only, `vault.rs:99` — the exact bug the doc warns of); Data Integrity `proof` object + `did:atomic` method/resolver; container/leaf/inline **directive parser**; the **lift** (surface→typed node); a **SHACL gate** (closed-world) + validation-report-as-graph; typed **collection sub-nodes** (AC/task/scope/constraint/dep); append-only **Memory** revisions with stable id ≠ path; the render **projection function**; and the new CLI verbs (`intent new/validate`, `memory new`, `provenance trace`).

**The single biggest structural change:** `content_hash` moves from `blake3(body-bytes)` to `blake3(JCS(canonical JSON-LD node))`. This is a format break requiring migration of every existing entry, and it is the load-bearing decision the rest of the plan hangs off.

---

## 2. The thin vertical slice (M0) — one node type, end to end

**Goal:** `atomic intent new WORD-5 --template feature` → author frontmatter + two container directives → lift → canonical JSON-LD Intent + JCS/BLAKE3 hash + Ed25519 Data Integrity proof → minimal `IntentShape` gate → `atomic intent show WORD-5` renders text back. No Memory, no PROV, no web render, no full directive grammar — just enough to prove the three-layer round trip on Intent.

**New crate: `atomic-canonical`** (new workspace member; keeps JSON-LD/JCS/proof logic out of `atomic-core`'s hot path and out of `atomic-repository`). Depends on `atomic-identity`, `blake3`, `serde_json`.

Files to **add**:
- `atomic-canonical/src/vocab.rs` — the closed registry *stub* for the slice: `NodeType::{Intent, AcceptanceCriterion, Task}`, `Status`, directive names `acceptance-criterion`/`task`. (Grows into the full registry in Phase 3a.)
- `atomic-canonical/src/node.rs` — `CanonicalNode` typed struct + serde to JSON-LD (`@context`, `@type`, `@id: urn:atomic:intent:<uuid>`, spine fields, `hasAcceptanceCriterion[]`, `why`, `contentHash`, `attributedTo`, `createdAt`, `proof`).
- `atomic-canonical/src/jcs.rs` — RFC 8785 canonicalizer (lexicographic key sort, canonical number/string encoding). Prefer wrapping an existing `serde_jcs` crate; if none acceptable, implement over `serde_json::Value`.
- `atomic-canonical/src/hash.rs` — `content_hash(node) = "blake3:" + hex(blake3(jcs(node_without_proof_or_hash)))`. Reuses `atomic-core` `Hash::of` semantics but over JCS bytes.
- `atomic-canonical/src/proof.rs` — `DataIntegrityProof` (`type=DataIntegrityProof`, `cryptosuite=eddsa-jcs-2022`, `verificationMethod=did:atomic:...#key-1`, `proofPurpose=assertionMethod`, `proofValue=z...`); sign/verify wrapping `atomic-identity::Signer`/`PublicKey::verify` over JCS bytes; multibase `z` (base58btc) encoding of the 64-byte sig.
- `atomic-canonical/src/did.rs` — minimal `did:atomic:<base32-of-IdentityId>` construction + resolve-to-pubkey (local `IdentityStore` lookup for the slice).
- `atomic-canonical/src/lift.rs` — slice lift: frontmatter map → spine; `:::acceptance-criterion{...}` and `:::task{...}` containers → sub-nodes. Reuses/replaces the flat parser.
- `atomic-canonical/src/gate.rs` — hand-coded `IntentShape` for the slice: `status ∈ {backlog,todo,in_progress,done}` (min1/max1); `attributedTo` present + resolves to an agent; `proof` present; `hasAcceptanceCriterion` items satisfy AC rule (`acStatus=met ⇒ verifiedBy ∧ evidence`). Returns a structured `ValidationReport` (Rust struct, JSON-LD-serializable). **Hand-coded now; a real SHACL engine arrives in Phase 3f.**
- `atomic-canonical/src/render.rs` — projection `render(node, target=Cli) -> String`; regenerates the body status header from the spine (kills the dual-authoring bug).

Files to **modify**:
- `atomic-repository/src/repository/vault_intent.rs` — `vault_intent_create` calls the directive-stub template (Phase 3h wiring, minimal here); on write, run lift → gate → compute canonical `content_hash` → sign → store. Delete the `**Status:** backlog` body-header authoring site (line ~3 of template).
- `atomic-repository/vault/templates/intent.md` — replace positional headings with `:::acceptance-criterion` / `:::task` directive stubs; remove the body status line.
- `atomic-core/src/pristine/vault.rs` — add optional `proof: Option<String>` and `canonical_json: Option<String>` to `VaultEntry` (postcard-compatible additive fields); keep `content_hash` but redefine what it hashes. Add a `format_version` discriminator for migration.
- `atomic-repository/src/repository/vault.rs` — `render_entry_to_markdown` delegates to `atomic-canonical::render` for intents; `vault_store` computes canonical hash + attaches proof.
- `atomic-cli/src/commands/vault/intent.rs` — add `Show` rendering via `atomic-canonical::render`; add `Validate` verb printing the `ValidationReport`.
- `Cargo.toml` (workspace) + `atomic-repository/Cargo.toml`, `atomic-cli/Cargo.toml` — add `atomic-canonical` dependency.

**Slice exit criteria:** create → lift → hash → sign → gate-pass → `show` renders with a spine-derived status header; tampering the body does not change `content_hash` in a way that hides a spine change; a `status=done` intent with an unmet AC (met-without-evidence) is *rejected* by the gate.

---

## 3. Phased plan for the rest

### Phase 3a — Closed-vocabulary registry as the single source (P0, blocks everything)
The doc's primary integration risk: five consumers (lift, directive parser, SHACL shapes, templates, CLI scaffolds) must not diverge. Make `atomic-canonical/src/vocab.rs` the *one* registry and derive the rest.
- **Add:** `atomic-canonical/src/vocab.rs` — enums/const tables for node types, edge types (`motivatedBy, informedBy, supersedes, previousRevision, about, hasAcceptanceCriterion, hasTask, satisfies, touchesFile, criteria, depends, blockedBy, verifiedBy, evidence, attributedTo` + PROV `used/generated/associatedWith/actedOnBehalfOf/turnParent`), status value sets, memory kinds, directive names. `unknown ⇒ error`.
- **Add:** `atomic-canonical/ns/ctx.jsonld` (checked-in `@context` for `https://atomic.dev/ns/ctx.jsonld`) generated from vocab; `atomic-canonical/shapes/*.ttl` generated from vocab (or generated at build time via `build.rs`).
- **Modify:** `atomic-core/src/pristine/ontology.rs` — re-export / align its predicate registry with `atomic-canonical::vocab` so KG extraction and the gate share one vocabulary (resolves the "advisory, largely unused" gap). Ontology becomes a thin view over the canonical registry.
- **Deliverable:** a single test that asserts every SHACL shape target, every directive name, every template stub, and every lift rule references a registry member.

### Phase 3b — Full directive grammar + lift (P0)
- **Add:** `atomic-canonical/src/directive.rs` — parser for container `:::name{attrs}...:::`, leaf `::name{attrs}`, inline `:name[label]{attrs}`; attribute grammar (`#id`, `key=value`, edge `to=`/`edge=`); nesting (`::file-ref` inside `:::task`).
- **Expand:** `atomic-canonical/src/lift.rs` — full typed extractor: frontmatter→spine, directive-name→`@type`, attrs→typed props/edges, fence prose→text slot, unmatched prose→unlifted body. Enforce the **single-authoring-site rule** (lift one site, ignore duplicates).
- **Replace:** the hand-rolled `yaml_frontmatter_to_json`/`parse_markdown_frontmatter` in `atomic-repository/src/repository/vault.rs:800,847` with a proper YAML parser feeding the lift (the flat parser can't hold nested AC/task arrays or reference lists — findings gap #4/#5). Keep the disk-first re-parse hook in `vault_intent.rs:432-443,573-584`.

### Phase 3c — Collection sub-nodes (P0 for Intent completeness)
- **Modify:** `atomic-canonical/src/node.rs` — nested `AcceptanceCriterion`, `Task`, `ScopeIn/ScopeOut`, `Constraint`, `Ref` sub-node types with `@id` (`urn:atomic:ac|task:...`).
- **Stable sub-node identity on edit (the critical risk):** the lift must reconcile sub-node `@id`s per AC/task on every write rather than treat the body as an opaque blob (findings intent-model RISK). Add `atomic-canonical/src/reconcile.rs` — match existing sub-node ids by `#id` attribute, allocate new ones only for new directives, so per-AC/task attestations survive edits.
- **Modify:** `atomic-core/src/pristine/vault.rs` `IntentSummary` — carry/reference sub-node collections (or a count + hash) so `intent list` can filter by AC/task status.
- **Add:** `TaskShape` (doc leaves it unspecified — see open questions) incl. `depends` cycle detection over `atomic-canonical`'s edge model.

### Phase 3d — Memory node type (P1; highest-risk data-model change)
- **Add:** `atomic-repository/src/repository/vault_memory.rs` (mirror `vault_goal.rs`/`vault_intent.rs` structure — none exists today; memory is generic-entry-only).
- **Stable id ≠ path (P0 within this phase):** introduce `urn:atomic:memory:<ULID>` as identity; keep the path as a materialized view only. This breaks the current path-as-key overwrite model and `entry_subject` (`vault_triples.rs:539-546`) — a genuine migration, not additive.
- **Append-only + supersede:** new revision writes a new entry referencing `previousRevision`/`supersedes`; old stays queryable; status `active|superseded|retracted`. Requires manifest keying by id (findings memory gap #2/#6).
- **Typed fields:** `memoryKind ∈ {constraint,preference,lesson,context}` (gate-enforced; replaces free-text `--type`); `about[]` edges to modules/domains.
- **Directionality rule:** `informedBy` lives on the **intent** side; memory never lists consumers. Do **not** copy the symmetric `blocked_by/blocks` pattern from `vault_triples.rs:648-663` (the exact anti-pattern the doc forbids).
- **Modify:** `MemorySummary` (`vault.rs:143-149`) to carry kind/status/id/about.

### Phase 3e — W3C PROV provenance + `provenance trace` (P1)
- **Add:** `atomic-canonical/src/prov.rs` — project the existing accumulator DAG (`atomic-agent/src/provenance/*`) and `atomic-core/src/change/provenance_graph/*` into a named JSON-LD `@graph` keyed `urn:atomic:provgraph:<changeid>` with `prov:Activity/SoftwareAgent/Person`, `used/generated/actedOnBehalfOf/turnParent`.
- **Modify:** `atomic-agent/src/provenance/accumulator/` — **capture `used` edges** (which intent/memory URNs the turn consumed). This is the flywheel's load-bearing edge and it is entirely missing today (findings prov gap #4). Turn boundaries (`event.rs:116`, `HookType::TurnStart/TurnEnd`) already fire where an Activity starts/ends.
- **Modify:** `atomic-agent/src/turn/orchestrator/provenance.rs:395` + `atomic-repository/src/repository/changes.rs:636,704-792` — the content-address/register/dep-link plumbing is reused; swap postcard for JCS-canonicalized JSON-LD as the addressed form, sign the graph.
- **Add:** `atomic-cli/src/commands/provenance/` (new top-level) — `provenance trace <urn>` walking `load_provenance_graph` + dep lookup: change → activity → intent → memories → agent + person.

### Phase 3f — SHACL gate hardening + agent validation loop (P1)
- **Replace** the hand-coded `gate.rs` with a real closed-world SHACL evaluator in `atomic-canonical/src/shacl.rs` consuming the generated `shapes/*.ttl`: `sh:in`, `sh:minCount/maxCount`, `sh:class`, `sh:node`, `sh:equals`, and a **rationed** `sh:sparql` (AC `met ⇒ verifiedBy ∧ evidence`; task `depends` cycle). Prefer declarative rules; sparql only where conditional (doc NON-GOAL: don't grow a query engine).
- **Validation-report-as-graph:** define its shape/vocab in `vocab.rs` (doc leaves it unspecified) — failing node, source shape, offending value, message.
- **Gate semantics:** `status:done` is *granted* by the gate when shapes pass, never auto-fixing a load-bearing/attested fact; may only regenerate a projected field (doc lines 452-454, 541). Wire the gate to run automatically at the `→ done` transition in `vault_intent.rs`, and `intent validate` for manual authoring.

### Phase 3g — Render projection: CLI / editor / web (P2)
- **Expand** `atomic-canonical/src/render.rs` into one projection function with three targets (CLI text, editor-panel JSON, web HTML), **zero files on disk**, lazy per-read (doc line 531).
- **Reference-not-transclusion:** resolve `:::ref`/`informedBy`/`about` edges *inline at render time*; store only the edge (doc lines 61, 189-193). Add resolve-at-render machinery (absent today — findings semantic gap #9).

### Phase 3h — CLI verbs + templates (P1, incremental)
- **Modify:** `atomic-cli/src/commands/vault/intent.rs` — `new WORD-5 --template feature|bugfix` (add `--template`, absent today); a **template registry** keyed by node-type/use-case replaces the single `INTENT_TEMPLATE` (`vault_intent.rs:16`). Templates scaffold spine + directive blocks only, never body prose, and can only emit registry types.
- **Modify:** `atomic-cli/src/commands/vault/memory.rs` — `memory new --kind --about` (none exist today; `--type` vocab is wrong).
- **Decision (see §6):** whether these live at top-level `atomic intent/memory/provenance` (doc) or stay under `atomic vault` (codebase). Prior art for aliasing exists (`vault query` re-exports top-level `Query`, `mod.rs:68`).

### Phase 3i — Flywheel edges/queries (P2)
- **Modify:** `atomic-repository/src/repository/vault_triples.rs` — extend the proven Intent frontmatter→edge extractor (`:627-691`) to emit `informedBy` (intent→memory), `about` (memory→module), `motivatedBy` (intent→decision), and the PROV edges — as a *pure projection* recomputed from canonical node ids (see §4).
- **Reuse:** `vault_kg_neighbors` + `query_plan.rs` for flywheel queries ("what must I know before changing this module" = `about` traversal; "AC met but no evidence" = graph query).

---

## 4. Reconciliation with existing atomic architecture

**Do the doc and `vault_triples.rs` agree? Partially — with one required reframing.**

- **Agreement:** The doc says "patch graph stays canonical, the triple view is a projection we compute and discard, no triplestore" (lines 533). Atomic already treats the KG as a *derived* index built by `vault_auto_index`/`kg_enrich`, not a source of truth, and does **not** run SPARQL storage. The `(KgNode,KgEdge)` redb tables are analogized to "regional redb materializations vs the S3 log" — exactly the doc's framing. So the *spirit* aligns.
- **Conflict to fix:** Today the KG is a **separately materialized, manually (re)enriched, persisted** store that can **drift** from vault entries and is keyed by **path-derived** ids (`intent:PIMO-1`, `entry_subject` on filename). The doc wants a **deterministic pure function** of the canonical records, keyed by **stable `@id` URNs** (`urn:atomic:intent:<uuid>`). **Reframe** (Phase 3i): rebuild KG extraction as a pure recompute over canonical node ids, not paths, so it honors "the patch graph stays canonical" and drift becomes impossible-by-construction. Reconcile `humanKey` (WORD-5) as a display alias over the URN `@id`.

**How the SHACL/JSON-LD layer coexists with patch-theory storage:**
- **Canonical source of truth stays the patch/change graph + redb `VAULT_ENTRIES`.** JSON-LD is a **compile target** (like a TS AST), never authored or stored as the primary artifact. RtW stores the postcard `VaultEntry` as today, plus (decision §6) either the canonical JSON-LD bytes alongside it or recompute-on-demand.
- **The gate sits between lift and `put_vault_entry`** (a chokepoint that accepts any bytes today — findings storage gap #3). No node enters the store unless it passes SHACL; `status:done` is granted there.
- **Signing anchors to identity, not to changes' own provenance section.** Vault entries gain a `proof` field (absent today); the per-change PROV graph is signed separately and hangs off the existing content-addressed provenance sidecar (`node_type::PROVENANCE=3`).
- **No new storage tier.** SHACL shapes are just Turtle files; JSON-LD is transient; the projection is discarded. This directly satisfies the doc's NON-GOALS (no triplestore, no pre-rendered HTML files, no OWL).

---

## 5. Sequenced milestones

| Milestone | Content | Priority | Depends on |
|---|---|---|---|
| **M0** | Thin vertical slice (§2): Intent create→lift→JCS/BLAKE3→proof→minimal IntentShape→CLI render. `atomic-canonical` crate born. | **P0** | — |
| **M1** | Registry as single source (3a) + full directive grammar & lift (3b). Migration of existing intents to canonical hashing. | **P0** | M0 |
| **M2** | Collection sub-nodes + stable-id reconciliation + TaskShape (3c). Intent is now feature-complete per doc. | **P0** | M1 |
| **M3** | SHACL gate hardening + validation-report-as-graph + auto-gate at `→done` (3f). | **P1** | M1, M2 |
| **M4** | Memory node type: stable id≠path, append-only/supersede, kinds, `about`, directionality (3d). Data-model migration. | **P1** | M1, M3 |
| **M5** | CLI verbs + template registry (`intent new --template`, `memory new --kind --about`) (3h). | **P1** | M2, M4 |
| **M6** | W3C PROV projection + `used`-edge capture + `provenance trace` (3e). | **P1** | M4 (memory URNs), M0 (proof/JCS) |
| **M7** | Render projection three targets + reference-at-render resolution (3g). | **P2** | M2, M4 |
| **M8** | Flywheel edges as pure projection + KG reframe to canonical ids + queries (3i). | **P2** | M4, M6 |

**P0 (must land first, load-bearing):** M0, M1, M2 — the Intent round trip and the canonical hash-format break. **P1:** M3–M6 — gate, memory, CLI, provenance. **P2:** M7–M8 — render targets and full flywheel query.

**Critical-path note:** the `content_hash` semantics change (M0/M1) gates a migration of every existing vault entry; do it early and behind a `format_version` so old entries verify under old rules until re-signed.

---

## 6. Open questions needing a human decision

1. **DID method: `did:atomic` vs `did:key`.** `IdentityId = blake3(pubkey)` (base32, no multicodec context) will *not* match standard `did:key` ids (which need multibase base58btc + multicodec `0xed01`). Adopt a custom `did:atomic:<base32(IdentityId)>` method (simpler, in-tree) or conform to `did:key` (interoperable with regulator tooling)? The doc uses `did:atomic:*` but PROV/Data-Integrity ecosystems assume `did:key`-style multibase. **This choice ripples through `proof.rs`, `did.rs`, and every `attributedTo`.**

2. **Store canonical JSON-LD bytes, or recompute on demand?** (findings storage gap #9.) Storing alongside the postcard `VaultEntry` gives stable proof re-verification but grows storage and duplicates state; recomputing risks JCS/serialization drift silently breaking a stored proof. Recommend storing canonical bytes for signed nodes; needs sign-off.

3. **CLI surface: top-level `atomic intent/memory/provenance` vs nested `atomic vault *`.** The doc uses top-level; the codebase nests under `atomic vault` (with aliasing prior art at `mod.rs:68`). Decision affects `main.rs` command tree and every example in the doc.

4. **Undefined node/shape types the doc references but never specifies:** `TaskShape` (only referenced, line 380), and the `Change` / `Decision` node types (used as URN/edge targets `urn:atomic:decision|change`, `motivatedBy→decision`, `generated/evidence→change`) have no record type, shape, or authoring surface. Are these authored records or system-minted (Change ≈ existing change hash, Decision ≈ ?)? Also unspecified: `ScopeIn/ScopeOut/Constraint/Ref` have directives but **no `@type` names or SHACL shapes** in any example.

5. **Secret-key-at-rest security.** `IdentityStore` writes signing keys **unencrypted** (`store.rs:459-463`; argon2id+chacha20poly1305 is a TODO). RtW makes these keys load-bearing for every attested node. Fix before shipping signing, or accept plaintext keys for the initial milestones?

6. **The `status:done` granting tension.** The gate "grants" `status:done` when shapes pass (line 342), yet "never auto-fixes a load-bearing fact" (line 454). Concrete rule needed for the boundary between *granting a transition* (writing status) and *mutating an attested field*. Proposal: gate writes status only on the spine, only forward through the state machine, only when all shapes pass, and never rewrites any other attested field — needs confirmation.

7. **`@context` + `atom:` namespace authorship.** `https://atomic.dev/ns/ctx.jsonld` and `https://atomic.dev/ns#` must be authored and hosted (or embedded/checked-in). Who owns publishing the context doc, and is it served or vendored?

8. **Memory identity migration scope.** Moving memory from path-as-identity to `urn:atomic:memory:<ULID>` (M4) touches the storage key model, manifest keying, `entry_subject`, and every existing memory on disk. Is a one-shot migration acceptable, or must old path-keyed memories keep resolving indefinitely via an alias table?

---

**Key files referenced (all under `/Users/bradleyhilton/Documents/workspace/atomic-projects/atomic`):** `atomic-core/src/pristine/vault.rs` (VaultEntry/summaries), `.../ontology.rs` (vocab seed), `.../tables.rs:550-595` (redb tables), `.../types/hash.rs` (BLAKE3), `atomic-repository/src/repository/vault.rs` (materialize/scan/record/render/parse), `.../vault_intent.rs` (intent CRUD + template wiring + status bug), `.../vault_triples.rs` (KG lift + edge patterns), `.../vault_kg_enrich.rs`, `.../changes.rs:636,704-792` (provenance registration), `atomic-agent/src/provenance/*` + `.../turn/orchestrator/provenance.rs:395` + `.../event.rs` (turn DAG/hooks), `atomic-core/src/change/provenance_graph/*`, `atomic-identity/src/{keypair,signing,identity,store}.rs`, `atomic-cli/src/commands/vault/{mod,intent,memory,goal}.rs` + `.../main.rs`. New crate to add: **`atomic-canonical`** (`vocab, node, jcs, hash, proof, did, directive, lift, reconcile, gate, shacl, prov, render` + `ns/ctx.jsonld` + `shapes/*.ttl`).

---

# Adversarial Critique & Required Corrections

> Produced by an independent reviewer against the doc + the atomic substrate. The plan is sound; these are the corrections that MUST be folded into M0/M1 before it truly round-trips.

## Thin-slice assessment
The M0 slice is well-chosen (Intent over Memory) and covers the right conceptual span (create -> lift -> JCS/BLAKE3 -> proof -> gate -> render), but as specified it does NOT work end-to-end and does not fully prove the concept. Two blockers: (1) redefining content_hash breaks vault_scan_working_copy's drift classification (vault.rs:483-488 compares Hash::of(body) to stored hash) and the manifest merkle (vault.rs:625-628) — the round-trip the slice claims to prove is silently severed, and the slice's own exit criterion about body-tampering-not-hiding-spine-changes becomes untestable because scan can no longer classify changes correctly. (2) The slice claims render 'regenerates the body status header from the spine,' but no body-projection engine exists (render_entry_to_markdown appends content_bytes verbatim), so the single-authoring-site proof — the doc's whole thesis in miniature (lines 436-450) — is not actually demonstrated unless that projection is built in M0. It is also under-scoped on reference-at-render (deferred to M7) and validation-report-as-graph (deferred to 3f), both of which are part of the doc's minimal render/gate contract. Recommendation: M0 must additionally (a) re-plumb scan/merkle to operate on canonical-node hashes, and (b) build minimal spine->body projection for the status header. With those two additions the slice becomes genuinely end-to-end and demoable. Without them it is a compile-and-store demo, not a round-trip proof.

## Confirmed solid
- Correctly diagnoses the load-bearing decision: content_hash moving from blake3(body) to blake3(JCS(canonical node)). Verified at atomic-core/src/pristine/vault.rs:99 (VaultEntry::new hashes content only) and the doc's own status-lesson (lines 440-442) names this exact failure. This is the right center of gravity.
- The three-layer separation (surface/canonical/render) maps cleanly and honestly onto the existing substrate: VAULT_ENTRIES path-keyed redb + vault_materialize (render) + vault_scan/record_working_copy (lift) genuinely is a round-trip engine. Reuse framing is accurate, not hand-waving.
- Honors reference-not-embed: Phase 3g explicitly stores only the edge and resolves at render time (doc lines 61, 189-193), and the plan never proposes transclusion into the canonical node.
- Honors no-triplestore / patch-graph-canonical: JSON-LD is treated as a compile target (like a TS AST), SHACL shapes as Turtle files, projection discarded. Section 4 correctly reconciles this with the doc's line-533 'regional redb materialization vs S3 log' analogy.
- Honors closed-vocabulary: Phase 3a makes vocab.rs the single source and adds a test asserting every SHACL target/directive/template/lift rule references a registry member — directly attacking the doc's stated primary integration risk (five diverging consumers).
- Honors no-auto-fix: Phase 3f and open-question 6 keep 'status:done is granted, never written' and forbid mutating attested fields, only regenerating projected fields (doc lines 452-454, 541).
- Honors presence-not-content: the plan never schematizes the 'why' prose; gate checks presence of proof/attributedTo/verifiedBy+evidence, not reason quality (doc line 57).
- Honors single-authoring-site: M0 exit criteria explicitly deletes the body '**Status:** backlog' authoring site (verified live at atomic-repository/vault/templates/intent.md line 3) and regenerates it from spine on render.
- Correctly identifies the 8 genuinely undefined items in the doc (TaskShape, Change/Decision node types, Scope/Constraint/Ref @types, @context contents, DID resolution, validation-report shape) as open questions rather than silently inventing them.
- The thin-slice choice of Intent (not Memory) is right: Intent already ships as entry_type and avoids the hardest data-model migration (memory id != path) in M0.

## Risks
- CRITICAL / UNADDRESSED: the hash-semantics change breaks drift detection. vault_scan_working_copy computes Hash::of(body_bytes) and compares it against the stored content_hash to classify New/Modified/Unchanged (atomic-repository/src/repository/vault.rs:483-488). If content_hash becomes blake3(JCS(node)), a body-hash will NEVER equal a canonical-node-hash, so every entry classifies as Modified on every scan and the disk->redb sync loop is corrupted. The plan changes 'what content_hash hashes' but is silent on this comparison site — the M0 slice as written will not round-trip.
- CRITICAL / UNADDRESSED: the manifest merkle chain is derived from content_hash (update_manifest_for_store, vault.rs:625-628, prev.next(&content_hash)). Redefining content_hash silently redefines the vault-wide merkle used for sync/selective-fetch across peers. The plan's 'format_version so old entries verify under old rules' does not cover the merkle, which is a single incremental root, not per-entry — mixed old/new entries produce a merkle that is neither. This is a distributed-sync break, not just a local migration.
- M0 claims render 'regenerates the body status header from the spine' but render_entry_to_markdown currently appends entry.content_bytes verbatim (vault.rs:706-711) — there is NO body-templating engine. Regenerating a spine-derived body is net-new projection work that the plan buries as a one-line render.rs deliverable; it is actually the mechanism the whole single-authoring-site principle depends on and deserves M0 first-class scope.
- Sub-node stable-id reconciliation (Phase 3c reconcile.rs) is scheduled AFTER the M0/M1 lift, but intent update already does whole-body replace (vault_intent.rs:503, new_content = body). If the lift runs on every write before reconcile.rs exists (M1/M2), per-AC/task @ids and their attestations are reallocated and destroyed on the first edit. The dependency ordering understates that reconciliation is a correctness prerequisite for ANY multi-write Intent, not a Phase-3c nicety.
- eddsa-jcs-2022 requires signing over JCS-canonicalized bytes, but atomic-identity signs arbitrary caller bytes with no canonicalization (signing.rs). If proof.rs and hash.rs canonicalize independently (two JCS call sites), any serialization drift between them silently invalidates proofs. The plan splits jcs.rs/hash.rs/proof.rs without mandating a single canonicalization entry point.
- Open-question 5 (unencrypted secret keys at rest, store.rs:459-463) is correctly flagged but the plan still schedules signing in M0 as non-blocking. Making Ed25519 keys load-bearing for every attested node while they sit in plaintext is a ship-blocker that M0 normalizes rather than gates.
- Storing canonical JSON-LD bytes vs recompute-on-demand (open question 2) is deferred, but M0 both signs a proof AND needs re-verification. Without deciding storage first, M0 will implicitly pick recompute, and any future serde/serde_json ordering change breaks every stored proof — the exact drift risk the doc warns about with transclusion, reintroduced at the serialization layer.
- The plan asserts ~65% reuse / ~35% net-new as a confidence signal, but the 35% (JCS, JSON-LD, SHACL closed-world engine, Data Integrity proofs, DID method, directive parser, validation-report-as-graph) is the entire hard core and every piece is greenfield with no in-tree precedent. The percentage framing understates schedule risk on the parts that actually gate the concept.

## Missing
- Migration of the manifest merkle is omitted. The plan migrates per-entry content_hash behind format_version but never says how the single incremental merkle root (VaultManifest.merkle) is recomputed or how peers with old-merkle vaults reconcile. This is a NON-additive change to a sync primitive the doc implicitly relies on.
- Drift-detection re-plumbing is missing entirely. To keep round-trip working, scan must compare a canonical-node-hash to a canonical-node-hash (re-lift disk file -> canonical -> hash), not Hash::of(body). No task covers rewriting vault.rs:483-488. Without it M0's exit criterion ('tampering the body does not change content_hash in a way that hides a spine change') is untestable because the scan can't even detect changes correctly.
- The doc's 'render resolves reference edges inline at read time' (lines 61, 191-193) is deferred to Phase 3g/M7 (P2), but the M0/M1 Intent already carries informedBy/motivatedBy edges. An intent show that cannot inline a referenced memory is not the doc's render; M0's 'show renders text back' quietly omits the reference-resolution half of the render contract.
- No task addresses the doc's requirement that attributedTo must resolve to a prov:Agent via sh:class (doc lines 366, 427). M0's gate.rs checks 'attributedTo present + resolves to an agent' but there is no prov:Agent node type, no agent record, and no resolver in the M0 scope — the DID resolves to a pubkey, not a typed prov:Agent. The gate cannot enforce sh:class prov:Agent as specified.
- The validation-report-as-graph (doc step 4, lines 508-519) is the doc's stated agent-facing contract, but the plan only produces a Rust ValidationReport struct in M0 and defers the graph form to 3f. If the report is not itself a graph from the start, the agent validation loop the doc centers on is not demonstrated by the P0 milestones.
- Provenance 'used' edge capture (Phase 3e/M6) is correctly called the flywheel's load-bearing edge, but it is P1 and depends on M4. The doc's entire thesis (line 43: 'the flywheel only works if every link is a real edge') means without used-capture there is no closed flywheel. Scheduling the one edge that closes the loop as a late P1 risks shipping intents+memories+provenance that never actually connect.
- No non-goal is violated, but the plan does not explicitly guard against the 'schema on the reason' non-goal (doc line 539) in the lift design — the lift must leave 'why' as an unconstrained text slot. Worth an explicit test that the gate never inspects why content.

## Conflicts with existing atomic architecture
- content_hash redefinition conflicts with vault_scan_working_copy drift classification (vault.rs:483-488), which is the disk->redb half of the round-trip. The two cannot coexist without re-plumbing scan to hash the canonical node.
- content_hash redefinition conflicts with the incremental manifest merkle (vault.rs:625-628) and thus with cross-peer vault sync/selective-fetch, which chains off content_hash. This touches distributed state, not just local entries.
- Memory id != path (Phase 3d) conflicts with the fundamental storage key model: VAULT_ENTRIES is keyed by vault-relative path (tables.rs:550) and vault_store overwrites by path (last-writer-wins). entry_subject derives KG identity from filename (vault_triples.rs:539-546). Append-only revisions with stable ULID ids conflict with path-as-key at the storage, manifest, and KG layers simultaneously — the plan acknowledges this but schedules it as one P1 milestone (M4) when it is arguably a storage-primitive change.
- The plan proposes ontology.rs becomes 'a thin view over atomic-canonical::vocab' (Phase 3a). But ontology.rs lives in atomic-core and atomic-canonical depends on atomic-identity, not the reverse; atomic-core cannot depend on atomic-canonical without a dependency cycle (atomic-repository/atomic-core are lower in the stack). The single-source-of-truth direction likely has to be atomic-core owning the registry and atomic-canonical consuming it, inverting the plan's proposed ownership.
- Symmetric BLOCKED_BY/BLOCKS edge emission (vault_triples.rs:648-663) conflicts with the doc's directionality rule (line 81, references-inputs-never-uses). The plan correctly forbids copying this for memory but does not schedule fixing the existing symmetric intent edges, which will remain in the KG as a live anti-pattern alongside the new directional model.
- No conflict with patch-theory storage itself: the plan correctly keeps the change/patch graph canonical and JSON-LD as a discarded projection, consistent with doc line 533. This is aligned, not a conflict.

## Recommendations
- Pull two tasks into M0 that the plan omits or defers: (a) re-plumb vault_scan_working_copy (vault.rs:483-488) to re-lift the disk file to a canonical node and hash THAT for drift classification, and (b) build minimal spine->body projection so the status header is rendered, not stored. These are the actual mechanisms that make the round-trip and single-authoring-site claims true; without them M0 does not prove the concept.
- Add an explicit manifest-merkle migration/versioning task alongside the content_hash change. Decide whether the merkle is recomputed wholesale at migration or namespaced by format_version, and how peers reconcile. Treat this as a distributed-sync change, not a local-entry migration.
- Mandate a single JCS canonicalization entry point used by BOTH hash.rs and proof.rs (one function, one call site each). Add a round-trip test: sign -> serialize -> deserialize -> reverify, and a test that reordering serde map keys does not change the canonical bytes. This closes the silent proof-drift risk.
- Resolve open-question 2 (store canonical bytes vs recompute) BEFORE M0 ships signing. Recommend storing canonical JSON-LD bytes for any signed node so proofs re-verify independent of serializer changes; the doc's transclusion-staleness argument applies equally to recompute drift.
- Invert the registry ownership: put the closed vocabulary in atomic-core (where ontology.rs already lives, low in the dependency stack) and have atomic-canonical consume it. The plan's 'ontology becomes a thin view over atomic-canonical' creates a dependency cycle since atomic-core cannot depend on atomic-canonical.
- Gate signing on the identity-key-at-rest fix (open question 5, store.rs:459-463). Do not make Ed25519 keys load-bearing for every attested node while they persist in plaintext; either implement argon2id+chacha20poly1305 first or explicitly scope M0 to a throwaway/dev identity with a documented non-production caveat.
- Move minimal 'used'-edge capture earlier or at least de-risk it: the flywheel does not close without it (doc line 43). Consider a P1-early stub that records intent/memory URNs consumed per turn even before the full PROV projection, so the chain is traversable when memory lands.
- Pin down the prov:Agent resolution story for M0's gate. Either add a minimal prov:SoftwareAgent/prov:Person node type in M0 so attributedTo can satisfy sh:class prov:Agent as the doc requires (lines 366, 427), or explicitly downgrade M0's gate to 'attributedTo resolves to a DID' and note the sh:class check is deferred — do not claim IntentShape parity when it is not enforced.
- Add an explicit test/guardrail that the lift never schematizes 'why' (doc non-goal, line 539) and that render inlines referenced memories (doc lines 61, 191-193) before declaring intent show complete. Reference-at-render is half the render contract and should not be a P2 afterthought.
- Schedule removal/reframing of the existing symmetric BLOCKED_BY/BLOCKS edges (vault_triples.rs:648-663) as part of the directionality work, not just avoidance for memory — otherwise the KG carries both the old bidirectional anti-pattern and the new directional model simultaneously.
