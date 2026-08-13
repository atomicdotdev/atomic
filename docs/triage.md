# Code Triage

Triage is how Atomic reviews work before it is promoted between views. It is
**not** a pull request. In Git a PR reviews *diffs proposed against a branch
tip* before the code exists on the target. In Atomic the code already lives in
the ambient GRAPH; a Draft view already sees it. Review is therefore not "should
these diffs be applied" but **"which change references should be inserted into a
Shared view, and does that change-set trace to an intent that is genuinely
satisfied."**

Triage answers that question by projecting a report over structure the graph
already holds, validating it against the gate, and recording an independent
**review intent** (`kind: review`) that `reviews` the work. The review is *not* a
memory and *not* a new node type — it is the existing Intent node with a `kind`
discriminator, authored and attested under the reviewer's own identity (ideally a
different model than the author). The report itself is a reproducible projection,
discarded after display.

## The join already exists

The intent-graph projection makes a task's `TOUCHES` edge point at the **same**
`file:` node a change's `MODIFIES` edge points at (see
[intent-graph.md](./intent-graph.md)). That gives a free walk from a diff back
to the acceptance criterion and the "why" that motivated it:

```
change:HASH ──MODIFIES──▶ file:path ◀──TOUCHES── task ──SATISFIES──▶ ac ◀──HAS_AC── intent
```

Triage is fundamentally a **report over that join**, filtered to a candidate
change-set, decorated with blast radius (`CALLS`), provenance (the PROV
flywheel), verification outcomes, and validated by the gate.

## The candidate set

Promoting a Draft view `feature` into a Shared view `dev` reviews the changes
`feature` can see that `dev` cannot, **plus their transitive dependency
closure** (insert always pulls the closure — see
[AGENTS.md § Insert](../AGENTS.md)):

```
candidate = diff_views(feature, dev).only_in_feature  ∪  dependency_closure
```

Changes that land via the closure but were **not** authored under a covered
intent are flagged as **baggage** (`BAGGAGE_DEP`) — visible, so a reviewer knows
what rides along.

## Reviewing an intent

The intent gate already models the review's mechanics:

- `acStatus = met` **requires** `verifiedBy` + `evidence` — "a checked box needs
  proof."
- `status: done` is **granted, not written** — granted by passing the gate.

**Two bars, by two parties.** The *author* self-attests the work intent: each
acceptance criterion is flipped to `met` with `verifiedBy` + `evidence`
(verification records), and when all criteria hold and scope/constraints conform,
the gate grants **`done`** — the author's claim of completion. Independent
*review* is a second, separate bar: a reviewer authors a `kind: review` intent
that `reviews` the work, carrying the review's own findings, and only when that
review is `done`, attested, and by a **non-author** identity is the work
**`ready`** to promote into a shared view. `done` says "the author thinks it's
finished"; `ready` says "someone else confirmed it." See
[Review intents](#review-intents).

### `done` is derived, granted, and view-relative

`done` is not a stored boolean. It is **derived** from the intent's criteria and
**granted** per view:

```
done@view  ⇔  every AC is met@view  ∧  scope/constraints conform  ∧  no blocking finding@view
```

A view is a promotion stage. `done@feature` (what a Draft can verify locally) is
a different bar from `done@dev` (which may add environment-bound checks). This
mirrors the change filter being view-relative.

## Review intents

A review is its own intent, classified by a `kind`:

| `kind` | meaning |
|--------|---------|
| `feature` (default) / `bug` / `chore` | ordinary work intents |
| `review` | reviews another intent — carries a `reviews` edge |
| `remediation` | forward-fix of a promoted defect — carries a `remediates` edge |

`kind` is authored via the frontmatter `kind:` key, validated against the closed
`INTENT_KIND` set, and mirrored into the manifest `IntentSummary` so `intent list`
/ `intent show` present it without lifting. It defaults to `feature` and is
omitted from the canonical node when default, so existing intents' content and
substance hashes are unchanged.

A **review intent** declares its target with `:::ref{to=<work-intent> edge=reviews}`
(the same ref surface as `blockedBy`/`remediates`), projected as a KG edge
`intent:<review> --REVIEWS--> intent:<work>` (reverse-queryable). The gate's
`ReviewShape` couples the two: `kind == review` ⟺ the node carries a `reviews`
edge — a review must declare what it reviews, and only a review may carry the
edge. The review's *own* acceptance criteria are the review checklist ("no
security flaws", "tests cover X", "AC-2 genuinely met"), so a flaw the reviewer
finds is an `unmet` criterion on the review, which keeps it from reaching `done`.

### Independent review is the promotion gate

`done` is self-attestation; **`ready` requires an independent review.** The work
intent is `ready` to promote into a shared view iff a review intent `REVIEWS` it,
is `done`, and is attributed to a **different identity than the work's author**
(`review.attributedTo ≠ work.attributedTo`). Because each agent/model holds its
own delegated identity, this is naturally satisfied by a *different model*
reviewing the author's work — cross-model review, cryptographically distinct and
unforgeable. `UNREVIEWED_CHANGE` blocks promotion into a shared view when no such
review exists (advisory `warn` for draft→draft); the report surfaces the
reviewer's identity as `reviewed_by`.

## Verification records

`evidence` is not a one-shot boolean. It is a set of **typed, refutable,
merkle-pinned verification records**:

| Field | Values | Purpose |
|-------|--------|---------|
| `kind` | `unit`, `integration`, `manual`, `e2e`, `runtime` | *what* was verified |
| `outcome` | `pass`, `fail` | the result |
| `scope` | `ac`, `view` | criterion-specific vs whole-view baseline |
| `observedAtMerkle` | view Merkle | the materialized state it is a fact about |
| `ref` / `observation` | change hash, test id, or note | the anchor |

`acStatus = met` is **derived**: met iff every *required* kind has a passing,
non-refuted record. Because each record pins the Merkle it was observed against,
supersession is automatic — a passing run at a newer Merkle supersedes an older
failing one:

```
draft merkle A:  npm run serve → FAIL   (observed @A)
   ↓ record fix change
draft merkle B:  npm run serve → PASS   (observed @B, supersedes the @A failure)
```

### View-scoped vs AC-scoped

- **AC-scoped** — does this criterion's specific behavior hold? Evidence on the
  AC.
- **View-scoped** — does the materialized view build / serve / pass baseline? A
  failure here blocks the **entire** triage regardless of which intent is being
  promoted. `npm run serve` failing lands here by default; if diagnosably
  attributable to a change, it also attaches to that change's AC.

## The three invalidation axes

A granted `done` is a **standing grant conditioned on freshness**. Three
independent things can invalidate it:

| Axis | Detects | Signal |
|------|---------|--------|
| **intent drift** | the intent's meaning changed | `intentSubstanceHash` moved |
| **code drift** | a new change touched the intent's files | file-scoped candidate-set delta |
| **evidence refutation** | reality contradicted the evidence | a failing verification record |

Whether invalidation *lapses* the old `done` or *spawns new work* depends on the
view scope (below).

### `intentSubstanceHash` — hash the substance, not the node

If a triage pinned the hash of the whole intent, recording the review would
change that hash and instantly stale its own triage. So the pin is over the
**reviewable substance only**, excluding the review-state fields:

```
intentSubstanceHash = JCS+BLAKE3 over { why, scope-in/out, constraints,
                                        AC definitions (text + requiredKinds), tasks }
                      excluding { proof, contentHash, attributedTo, status,
                                  per-AC acStatus, verifiedBy, evidence, verifications }
```

The rule is: strip review **state** and authorship/proof **metadata**, keep the
reviewable **definition** — so an AC's `requiredKinds` (the verification bar) is
part of the substance, while its verification records are not. `attributedTo` is
excluded because attestation injects it *after* the pin is stamped; leaving it in
made every attested-then-done intent read as spuriously `STALE_TRIAGE`.

- Edit an AC definition / add a task / change scope / change `requiredKinds` → substance moves → stale.
- Mark an AC met / attach evidence or a verification record / grant done → substance stable → still fresh.

Implemented as `substance_view` + `intent_substance_hash` in `atomic-canonical`
(built on the existing `hashing_view`, adding the nested per-AC key stripping).

(Same class of bug as the BODY-only hash `recording-the-why-plan.md` warns of —
hash the right thing.)

### Draft doneness lapses; Shared doneness is immutable

The invalidation axes act on **unpromoted (Draft) doneness**, where done is
genuinely derived-and-refutable because nothing external depends on it yet. Once
`done@shared` is attested and the change is inserted, that grant is **immutable**
— it was legitimately green at its Merkle with the evidence available then. A bug
discovered later is *new information*, not a retroactive lie.

```
bug found in Draft, pre-insert   → failing verification record on the existing AC; fix in draft; no new node
bug found in Shared, post-insert → new intent, remediates-linked to the original (below)
```

The boundary falls exactly on the `Draft | Shared` scope split.

## Remediation: post-insert bugs

A bug found in a Shared view (the change is already in collaborative history,
where deletion is restricted) is remediated **forward** as a new intent linked to
the original:

```
intent:B  --remediates-->  intent:A          # new closed-vocab verb (distinct from PROV wasDerivedFrom / motivatedBy)
intent:B  --about-->       change:H7K2        # the flawed change it fixes
intent:B  --foundIn-->     view:dev @merkle   # where/when it manifested
```

Rules:

1. **Surfaced, not blocking.** B travels alongside A up the promotion chain and
   appears in the `dev → release` triage as a non-blocking `OPEN_REMEDIATION`
   finding on A. The flaw is already in shared history; blocking A's promotion
   would punish forward progress for a defect already present. B promotes on its
   own cadence.
2. **`remediates` is additive.** A new intent→intent verb, reverse-queryable
   ("what remediations exist for A?"). It does not replace `wasDerivedFrom`
   (PROV entity derivation) or `motivatedBy` (→ decision), which keep their
   meanings. Shipped: authored via the same `:::ref{to=<intent> edge=remediates}`
   surface `blockedBy` uses, projected as a `intent:B --REMEDIATES--> intent:A`
   KG edge (reverse-queryable from A). The triage report emits `OPEN_REMEDIATION`
   (info, non-blocking) on a reached intent A whenever a remediation B targets it
   and B is not `done`.
3. **B gets a fresh AC — the ratchet.** The defect escaped *because* the AC was
   only covered by a local unit test. B's acceptance criterion therefore
   requires the **manual test, now automated** (a stronger `kind` — `e2e` /
   `integration` / `runtime`, not `unit`). B's AC is met only when that
   automated test passes.

### The verification ratchet

Step 3 makes triage self-improving. Once B lands, its automated test is part of
the repo, so it now runs in the **view-scoped verification bar** for *every
future triage*. The manual observation that caught this escape becomes a standing
automated guard for all subsequent promotions:

> An escaped defect converts the manual observation that caught it into a
> permanent automated verification on the remediation's AC, which then joins the
> view-scoped baseline. The verification bar is monotonically non-decreasing
> across the project's history.

## Triage references

A triage report is a **pure function** of its inputs, so it is content-addressed
and reproduced on demand rather than stored:

```
report = f(view Merkle, candidate change hashes, intentSubstanceHash per intent)
urn:atomic:triage:<blake3>  =  JCS + BLAKE3 over the canonical report
```

An `evidence` edge on a met AC points at `urn:atomic:triage:<hash>`; the grant of
`done` cites the same reference. The report bytes are recomputed and verified
against the hash (same JCS/BLAKE3 machinery as every canonical node). This is a
**minted, derived** reference like `change:<hash>` — not an authored node.

**Pin the Merkle.** The report only reproduces if the exact graph state is
reconstructable, so the reference pins the view Merkle (the `STATES`-table
change-sequence hash). Without the pin, evidence drifts the moment anyone records
after review.

**Portable export escape hatch.** If a durable, non-reproducible artifact is
needed (compliance, sending outside the repo), `atomic triage attest` signs the
canonical bytes and keeps them. That is the only case where frozen bytes (e.g. a
downstream PDF) are legitimate — never the working surface.

## Freshness shapes (SHACL-style)

The gate only *reports*; it never mutates. Because `done` is granted-not-written,
a freshness violation means the grant **lapses** — a small reconciliation at the
write chokepoint applies the demotion (Draft only). Editing an intent already
re-projects and re-hashes it, so the check runs exactly when substance changes;
no background sweeper is needed.

```
FreshnessShape        (targets: Draft intents where status = done)
  done ⇒ ∀ met AC: AC.evidence.triage.intentSubstanceHash == currentIntentSubstanceHash

CodeFreshnessShape    (targets: Draft intents where status = done)
  done ⇒ no change touching this intent's TOUCHES files is visible beyond the pinned candidate set

EvidenceShape         (targets: any AC where status = met)
  met ⇒ every required verification kind has a passing, non-refuted record @ current merkle
```

Lapsed intents demote `done → in_progress` with the violation recorded as the
reason (queryable: "why did this leave done?"). Shared/promoted done is exempt —
its remediation is a `remediates`-linked intent, not a lapse.

### Implementation status (first cut)

Shipped: the granted-at pin is stored in the intent's **frontmatter** as
`doneSubstanceHash` (ignored by `lift_intent`, so it moves neither `contentHash`
nor `intentSubstanceHash`), mirrored into the manifest `IntentSummary`. It is
stamped when the intent transitions to `done` (via `vault_intent_update`
`--status done`, after the rollup gate) and cleared on lapse. On any later edit
that is not itself a done-grant, if the stored status is `done`, the current
`intent_substance_hash` differs from the pin, and the intent's authoring view is
Draft-scoped, the status demotes to `in_progress` and `doneLapsedReason` records
the drift — done inline before the single write, so there is no re-entrant write
loop. The triage report reads the same pin to emit `STALE_TRIAGE` (warn) and set
the `stale` verdict (`Blocked > Stale > Ready`).

Known first-cut limits (tracked as `TODO(T5b-follow-up)`): record-time raw disk
edits reach `vault_store` directly and bypass the lapse until the next
`vault_intent_update`; scope-gating uses the frozen authoring `view`, so a
Draft-authored intent whose changes already reached a Shared view is not yet
distinguished (precise "Shared is immutable" needs the change-visibility walk).
Also note `done` is today a written frontmatter status, not yet a fully derived
grant, and `intent attest` does not itself set `done`.

## Output: one model, four skins

"Human + agent readable" is not two formats. It is **one canonical report model
where every item carries both a prose face and a machine face**, projected to
skins (the same never-dual-author rule as `render.rs`). Every criterion and
finding carries a stable node id + closed-vocab `code` + `severity` +
`suggested_query` (machine) *and* a `message` + `remedy` (human).

The report plays three roles at once: a **verdict** (`ready | blocked | stale`),
a **worklist** (which ACs still need a content judgment, with the query to run),
and an **evidence bundle** (the pins that reproduce it).

### Canonical structure

```
TriageReport
  ref            urn:atomic:triage:<blake3>
  verdict        ready | blocked | stale
  inputs         { feature_view, target_view, view_merkle,
                   intent_substance_hashes{}, candidate_changes[], closure_changes[] }
  summary        { changes, files, criteria_met, criteria_unmet,
                   findings{block,warn,info}, blast_entities }
  intents[]      (= "testsuites")
    id, why, scope_in[], scope_out[], constraints[]
    conforms, gate_violations[]
    criteria[]   (= "testcases")
      id, status(met|unmet), verifiedBy, evidence[], satisfied_by[], judgment_required
    tasks[]      { status, touches[] }
  changes[]
    id, authored_by, diff_ref, files[], blast_radius[], closure_baggage, provenance{turn,session,memories}
  findings[]     (= "failures", also mirrored inline, severity-sorted)
    code, severity, focus, message, suggested_query, remedy
  walkthrough[]  (= the guided reading order, foundations first)
    id, title, rationale, files[], tasks[], criteria[], changes[], depends_on[]
```

### Skins and scale

The CLI must never become a dump; size lives behind drill-down so a 400-change
review yields the same three-line verdict as a 4-change one.

| Skin | Job | Scales by | Consumer |
|------|-----|-----------|----------|
| **CLI** (default) | verdict + summary + blocking findings, then drill-down flags | bounded output | human triaging |
| **HTML** (self-contained) | the full navigable review as a **graph** (reuse `emit_html` / DOT) | zoom / filter / collapse, not scroll | human deep-review |
| **JSON** | streamable, paginated worklist | agent pages through it | the review skill |
| **attested export** | signed frozen bytes (opt-in) | — | portability / compliance |

Drill-down: `atomic triage feature --into dev [--finding block | --intent <ref> | --change <hash> | --walkthrough | --html]`.

Shipped: `--json` (the agent worklist), the bounded CLI dashboard (default),
`--walkthrough` (the guided reading order: the report's `walkthrough` section
— candidate modifications clustered into semantic layers by module, ordered
foundations-first by import/include direction — as bounded text, one numbered
entry per layer with rationale, files, and inspect commands, never a diff
dump), `--html` (a self-contained, dependency-free document — inline CSS/JS,
no CDN, verdict banner, severity-filterable finding cards, collapsible
per-intent `<details>`, an ordered walkthrough chapter tour — reading-order
nav, numbered collapsible chapters with `depends_on` anchors and layer-scoped
diffs — and the full report embedded as a JSON island), and `--attest`
(the signed frozen export: the report Value plus an `eddsa-jcs-2022` Data
Integrity `proof`, alongside the content-addressed `reference`). The
walkthrough is a pure, deterministic projection built from facts already in
the report (KG `PART_OF` modules with a parent-directory fallback,
`IMPORTS`/`INCLUDES` ordering, task/criterion joins) — never LLM-authored, so
the attested export stays reproducible. A richer severity-colored node-link
graph view (reusing the `query graph` machinery, once de-CDN'd) remains a
future enhancement over the current structured document.

## Finding taxonomy (closed set)

Both the shapes and the skill branch on `code`, so it is a closed vocabulary.

| Code | Severity | Focus | Meaning |
|------|----------|-------|---------|
| `VIEW_VERIFY_FAIL` | block | view | materialized view fails baseline (build/serve/tests) |
| `GATE_VIOLATION` | block | intent/ac | intent does not conform to the gate |
| `SCOPE_OUT_BREACH` | block | change | change modifies a file the intent marks scope-out (via the `SCOPE_OUT_FILE` edge from a `:::scope-out` `::file-ref`) |
| `ORPHAN_CHANGE` | block | change | candidate change no task/intent links |
| `MET_AC_NO_EVIDENCE` | block | ac | `acStatus=met` without `verifiedBy` + evidence |
| `UNMET_AC_WITH_CANDIDATE` | warn | ac | a candidate claims to satisfy an unmet AC — judge it |
| `BAGGAGE_DEP` | warn | change | landed via closure, not under a covered intent |
| `BLAST_UNREVIEWED` | warn | entity | a caller outside the change-set may be affected |
| `STALE_TRIAGE` | warn | intent | pinned inputs no longer match current state |
| `OPEN_REMEDIATION` | info | intent | promoted code has a `remediates`-linked intent in flight |
| `UNREVIEWED_CHANGE` | block →shared / warn draft | intent | no `done`, independent (non-author) review covers this intent's changes |

## The review skill

`triage-review` drives the content judgment the gate deliberately leaves honest
("presence is enforced; content is left honest"):

1. `atomic triage <feature> --into <dev> --json` → the worklist.
2. For each `block` finding and every `judgment_required` criterion, investigate
   with the code-intelligence tools (`callers`, `entities`, `code`) and **read the
   actual diffs** (`atomic change <hash>`, `atomic diff -c <hash> --word-diff`) —
   the code-quality judgment the gate cannot make.
3. Create your review: `atomic intent new --review <work-intent>` (a `kind: review`
   intent that `reviews` the work). Its acceptance criteria are your checklist; a
   flaw is an `unmet` review criterion.
4. Run the checks, record them as verification records on the review's criteria,
   and `atomic intent attest <review>` **under your own identity** (ideally a
   different model than the author — that is what makes it independent).
5. When the review is `done`, attested, by a non-author identity, and no blocking
   findings remain, the work is `ready`; recommend `insert from-view <feature>
   --to-view <dev>`. `UNREVIEWED_CHANGE` blocks shared promotion until then.

The reviewer signs their *own* review intent, so "who reviewed this, with what
findings" is a first-class, attested, independent record — not an edit to the
author's intent.

## Staged implementation plan

Each stage is independently shippable and testable. Dependencies are on
already-shipped substrate (the join, `diff_views`, `callers`, `provenance
trace`, JCS/BLAKE3, the gate) or on earlier stages.

### S0 — Candidate-set primitive (P0, blocks everything)
Expose `diff_views` change hashes + dependency closure through the CLI/JSON
(`atomic diff --view F --against T --changes --json`, or a dedicated query verb).
Every other stage consumes this list. **Depends on:** existing `diff_views`,
dependency-closure computation.
**Exit:** given `F` and `T`, emit `only_in_feature`, `closure_additions`, and
which additions are baggage (no covered intent).

### S1 — Verification records + derived `acStatus` (P0)
Add `kind` / `outcome` / `scope` / `observedAtMerkle` to evidence records; make
`acStatus = met` a derivation over required kinds; add the closed
`verificationKind` / `outcome` / `verificationScope` vocab. **Depends on:** the
existing `evidence` field, `ontology.rs` / canonical `vocab.rs`.
**Exit:** an AC with only a `unit` record does not satisfy a requirement of
`{unit, e2e}`; a newer passing record supersedes an older failing one by Merkle.

### S2 — Triage reference + `intentSubstanceHash` (P0)
Define `intentSubstanceHash` (substance-only canonicalization) and the
`urn:atomic:triage:<blake3>` scheme with its pinned inputs; reproduce-and-verify
on demand. **Depends on:** S0, S1, existing JCS/BLAKE3.
**Exit:** a report reproduces byte-identically from its pins; editing an AC
definition changes the substance hash while marking it met does not.

### S3 — Triage projection verb + finding taxonomy (P0)
`atomic triage F --into T` walks the join (candidate → files → tasks → ACs →
intents), attaches blast radius (`callers`), provenance (`provenance trace`), and
diffs, and emits the canonical report model with the closed finding `code` set.
**Depends on:** S0, S1, S2, existing `neighbors` / `callers` / `provenance
trace`.
**Exit:** the model contains every finding code, each with focus node +
message + suggested_query; verdict derives correctly from findings + AC state.

### S4 — Output skins (P1)
CLI dashboard + drill-down; JSON worklist; HTML graph surface (reuse `emit_html`
/ DOT); attested frozen export. **Depends on:** S3.
**Exit:** a 400-change review yields the same bounded CLI verdict as a 4-change
one; the JSON is the skill's input; the HTML renders the severity-colored join.

### S5 — Freshness shapes + grant-lapse (P1)
`FreshnessShape`, `CodeFreshnessShape`, `EvidenceShape`; reconciliation at the
intent-write chokepoint that lapses **Draft** `done → in_progress` with the
recorded reason; Shared done exempt. **Depends on:** S1, S2.
**Exit:** editing a done Draft intent's substance demotes it; an unrelated record
does not; a manual-test refutation flips the AC and lapses done.

### S6 — Remediation flow (P1)
The `remediates` verb; post-insert bug → new intent with a fresh AC requiring the
automated-from-manual kind; `OPEN_REMEDIATION` surfaced-not-blocking; the
ratchet joins the automated test to the view-scoped baseline. **Depends on:** S1
(kinds), S3 (findings), S5 (Draft vs Shared doneness).
**Exit:** a post-insert bug produces a `remediates`-linked intent whose AC needs
the automated test; the original's promotion is not blocked but is surfaced.

### S7 — Review skill (P2)
The `triage-review` skill consuming `--json`, judging content, writing back
verification records, attesting, recommending insert. **Depends on:** S1, S3,
S4 (JSON).
**Exit:** the skill closes the loop end-to-end on a sample repo: report →
investigate findings → write evidence → attest → insert.

### Sequenced milestones

| Milestone | Content | Priority | Depends on |
|-----------|---------|----------|------------|
| **T0** | Candidate-set primitive (S0) | P0 | — |
| **T1** | Verification records + derived `acStatus` (S1) | P0 | — |
| **T2** | Triage reference + `intentSubstanceHash` (S2) | P0 | T0, T1 |
| **T3** | Projection verb + finding taxonomy (S3) | P0 | T0, T1, T2 |
| **T4** | Output skins — CLI / JSON / HTML / export (S4) | P1 | T3 |
| **T5** | Freshness shapes + grant-lapse (S5) | P1 | T1, T2 |
| **T6** | Remediation flow + ratchet (S6) | P1 | T1, T3, T5 |
| **T7** | Review skill (S7) | P2 | T3, T4 |

All milestones T0–T7 are implemented (T5 as T5a EvidenceShape + T5b
grant-lapse). The review skill ships in the `atomic-opencode` package as
`skills/triage-review`. Follow-ups since completed: scope-out `file-ref`s +
the `SCOPE_OUT_FILE` edge make `SCOPE_OUT_BREACH` fire; `VIEW_VERIFY_FAIL` and
`BLAST_UNREVIEWED` are populated — so **all ten finding codes now emit**; and
the grant-lapse also runs on the record-time write path (raw disk edit +
`atomic record`), via a shared helper. Consciously deferred (low value / high
parser risk): the `:::verification` container form (the `::verification` leaf
form works) and a de-CDN'd node-link graph view (the self-contained structured
HTML document already covers the human skin).

**Post-T7: the review-intent model.** Reviews are now first-class review intents
(`kind: review`) rather than edits to the reviewed intent: an `INTENT_KIND`
taxonomy (`feature`/`review`/`bug`/`chore`/`remediation`) in frontmatter (mirrored
to `IntentSummary`, shown in `intent list`/`show`), a `reviews` ref → `REVIEWS` KG
edge, a `ReviewShape` gate rule, `atomic intent new --review`, and the
`UNREVIEWED_CHANGE` promotion gate requiring a `done`, non-author review before a
→shared insert (the 11th finding code, `attributedTo`-based independence).

## Governing principles

- **No new node *type*; the review is a review *intent*.** Triage adds no memory
  and no bespoke review node — a review is the existing Intent node with
  `kind: review`, authored and attested under the reviewer's own identity. The
  report is a reproducible projection.
- **`done` is self-attestation; `ready` needs independent review.** The author
  grants `done`; promoting into a shared view requires a `done` review intent by a
  non-author identity (`UNREVIEWED_CHANGE` blocks otherwise) — naturally a
  different model reviewing the author's work.
- **Presence enforced, content honest.** The gate enforces that evidence of the
  required kind exists and is not contradicted; the skill (or human) judges
  whether the content genuinely satisfies the criterion.
- **`done` is granted, derived, and view-relative.** Never a stored boolean.
- **Draft doneness lapses; Shared doneness is immutable.** Pre-insert bugs
  refute the same intent; post-insert bugs remediate forward.
- **The verification bar only ratchets up.** Every escaped defect leaves behind a
  permanent automated guard.

## Relevant code

- `atomic-repository/src/repository/content.rs` — `diff_views` (candidate set),
  `get_file_content_via_filter`.
- `atomic-cli/src/commands/query/mod.rs` — `neighbors`, `callers` (blast radius),
  `entities`, `graph` / `emit_html` (the HTML surface), `plan`.
- `atomic-cli/src/commands/provenance/command.rs` — `provenance trace` (the PROV
  flywheel behind each change).
- `atomic-canonical/src/gate.rs` — `ValidationReport`, `Violation`, the shape
  rules; add the freshness shapes here.
- `atomic-canonical/src/vocab.rs` — closed value sets; add `verificationKind`,
  `outcome`, `verificationScope`, the `remediates` edge, and the finding `code`
  set.
- `atomic-core/src/pristine/ontology.rs` — `HAS_ACCEPTANCE_CRITERION`,
  `SATISFIES`, `TOUCHES`, `MODIFIES` (the join); `remediates` lands alongside.
- `atomic-core/src/types/hash.rs` — BLAKE3; `intentSubstanceHash` and
  `urn:atomic:triage:<hash>` reuse the JCS/BLAKE3 path.

See also [intent-graph.md](./intent-graph.md) (the join this projects over),
[recording-the-why-plan.md](./recording-the-why-plan.md) (the canonical-node /
gate machinery), and [intent-identity.md](./intent-identity.md) (`intent:<ULID>`
and reference forms).
