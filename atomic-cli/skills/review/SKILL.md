---
name: Review
description: Review recorded work through anti-slop, intent conformance and blast-radius/risk passes, returning a graph of numbered evidence-backed findings and criterion assessments; or publish an independent view review through Atomic.
---

# Review

One skill, two CLI-owned execution modes. Read installed command help if the
binary's options differ; do not rebuild the workflow in prompts or shell scripts.

## Default: local-only semantic audit and metrics

Ordinary review uses `atomic-semantic` and `atomic-audit` against recorded source.
It does not invoke a model, contact an AI provider, start another coding agent or
require credentials. Do not add an AI validation pass to this workflow. All audit
and metric cards are included with their actual local coverage and evidence;
unsupported, skipped, unavailable and error states are explicit, not passing checks.

```sh
atomic review --json
```

The CLI resolves explicit current-session intent context when available, otherwise
the latest recorded change. It never silently expands the default to a full view.
Explicit selectors are mutually exclusive:

```sh
atomic review --turn <session>:<turn> --json
atomic review --intent <intent> --json
atomic review --change <hash> --json
atomic review --change <hash> --html
atomic review --change <hash> --html --no-open --output review.html
atomic review --view <name> --html
atomic review --view <name> --json
atomic review --from <before-view> --to <after-view> --html
atomic review --from <before-view> --to <after-view> --json
```

`--view NAME` explicitly audits the current recorded snapshot of that named view,
including inherited/dependency-visible files, once per live path. It never
switches the working copy, reads dirty/staged/untracked files or replays every
historical patch. It is exclusive with change/turn/intent selection and with
change-specific `--findings`/`--risk-profile` inputs. Empty views are valid.
Snapshot reports use `view_snapshot.analysis`; `changes` and `selected_roots`
are empty rather than inventing an authored change. All existing cards remain
available. Source locations use the pinned view-snapshot reference; introduction
is unknown, before values/deltas are not inferred, and HTML shows current snapshot
values. Conformance lists intents explicitly assigned to the named view and their
criteria, independently of change-plan links; this scope context is not inferred
governing-change attribution or acceptance approval. Change-regression risk still
has no baseline in snapshot mode.

`--from BEFORE --to AFTER` compares two complete recorded view states without a
working-copy switch. Both options are required; they conflict with the other
selectors and change-specific finding/risk inputs. From is before; To is after.
Metrics are extracted independently over each whole supplied corpus and matched
by canonical file inode and qualified subject. The report shows actual numeric
deltas only when both measurements and identities are comparable. Missing data
is unavailable, not an inferred zero. Audit observations are new, existing,
resolved, changed or unknown; changed/unknown are deliberately uncertain rather
than guessed resolutions. Renames use recorded identity, not content similarity.
The v4 `view_comparison.files[*].analysis` units use domain-separated source URIs,
not authored change hashes. All three passes are populated: conformance includes
intents explicitly assigned to the **To** view even without a change-plan link,
plus separately identified membership-delta governing context, pinned recorded
criterion declarations and task/file evidence. Current-vault context is labelled
and source/scope-hashed; unrelated and inherited parent-view assignments are not
silently treated as target work. The left navigation groups acceptance criteria
under searchable intent keys/titles and keeps intents without criteria visible.
Author
claims are not independent acceptance judgments. Blast radius uses the shared
semantic selector over the actual named views for changed functions, callers,
direct-call contracts and the local risk model, not intermediate patch replay.
Compiler/type/runtime behavior remains unproven. Every detected contract finding
is numbered and available in JSON and HTML, with no 100-finding display cap.
See `references/review-contract.md` for matching and coverage limits.

The offline viewer embeds Atomic's official wordmark and website palette. It
loads no remote fonts, assets or analytics; asset provenance is in
`src/commands/review/assets/README.md`.

Default terminal output is a location-first three-pass report: anti-slop, intent
conformance, and blast radius / risk. `--json` includes the same `graph.nodes`
and `graph.edges`, numbered `graph.findings` (`F1`, `F2`, ...), criterion
assessments and exact scope. `--details` additionally shows the original worklist
and diffs. Numbers are deterministic within a report, not permanent issue IDs.
Static caller/importer relationships are potential risk context, not automatic
bug findings or proof that tests cover callers.

The terminal groups concerns by recorded file/region with audit/metric card IDs.
The HTML opens on Summary for report-wide counts, comparison direction/file
states, all-card/coverage tables, conformance context and risk estimates. The
Anti-slop, Conformance and Blast radius tabs start with selected evidence rather
than repeating that overview above every file. Summary links open the relevant
review/card details; returning to a pass retains its selected region. This is a
presentation separation, not a filter or finding limit.
`--html` opens an offline workbench with source, card inspection and separate
anti-slop/conformance/blast-radius tabs. It uses the same scope and finding numbers;
it does not run a different audit or grant approval. HTML artifacts contain source
and evidence, so keep sensitive reports private. `--no-open` supports headless export.

For an evidenced criterion failure, add an explicit `acceptance_criteria` entry
to the common finding: `criterion` (exact ID), `assessment: unmet`, and
`rationale` connecting the evidence to that criterion. Use `unknown` when the
evidence cannot establish failure. File/task overlap is only a related-criterion
hint. Never infer that every criterion of a touched file is unmet.

To render substantive observations in the same graph, supply a JSON file via
`atomic review --change <same-hash> --findings <file> [--json]`. The envelope is
`schema: atomic.review-findings.v1`, the exact `scope` copied from `graph.scope`,
and `findings: [{pass, finding}]`. Pass values are `anti_slop`,
`intent_conformance`, and `blast_radius_risk`; `finding` uses the common contract
below. Only link criteria of the change's unambiguous governing intent. Scope,
intent-source hashes, recorded coordinates and evidence are validated; stale or
foreign judgments are refused. Input is limited to 1 MiB / 500 findings.

An unmet review assessment links back to its numbered findings but does not
rewrite the source intent's declaration, invalidate its signature, or become
signed independent approval. Intent conformance remains pending/partial until
substantive findings are supplied; author `met` labels are not review verdicts.

Use the returned exact roots, dependency context, diffs, criteria and pending-state
diagnostics as the local review result. Source inspection does not prove tests pass;
do not execute reviewed code or tests as part of HTML export.
Report real findings and verification under their actual authorship and observed
scope. Reuse existing work/review evidence; do not create coordination intents or
claim the author's in-session observations are independent approval. Unfinished
or unrecorded work remains pending, not a reviewed snapshot.

Include a structural and anti-slop pass. `changes[].analysis` contains the
versioned `atomic.review-analysis.v1` contract, check coverage, and candidate
findings from `atomic-audit` using existing `atomic-semantic` parsers. Audit cards,
metric definitions and advisory policies live in the `atomic-audit` crate; a
catalog entry is not proof that its metric was measured. Use
`changes[].analysis.metrics` for actual values,
recorded-side locations, conventions and deltas; inspect `metric_diagnostics`
and `metric_comparison_scope`. Unknown/not-applicable is not zero, and changed-file
clone counts are not repository-wide duplication scores. Metric observations do
not themselves prove slop or an unmet criterion. Use the same finding structure
for local review observations: see [the review contract](references/review-contract.md).
Paths with an exact `.vault` component are evidence, not application code: exclude
them from code audits, metrics, clone comparisons and static blast-radius parsing.
Retain their diffs, governing intent/criterion context and publication evidence
checks. A vault-only selection has no applicable code passes; do not substitute
an earlier code change or scan the project to fill an empty result.
Every finding must include `analyzer`, exact recorded `change`, `summary`,
`locations` (`path`, `side`: before/after, 1-based inclusive `start_line`/`end_line`),
`requirement_or_rule` (`id`, `explanation`), supporting `evidence` (`kind`,
`reference`, `observation`), `introduced_by_change` (`status`: yes/no/unknown,
`rationale`), `severity` (info/low/medium/high/critical), `confidence`
(low/medium/high), and `proposed_correction`. Evidence kinds are source, diff,
test, command, or reference. Report analyzer/rule versions and coverage gaps.
The linked reference is repository-shipped detail; these required fields remain
self-contained when only this skill file is installed.
An AST match needs contextual judgment; do not infer requirement failure,
authorship, or introduction solely from its presence. Skipped/error checks are
not clean results. No new parser dependency or credentials are required.
The registered local checks cover the audit cards' supported semantic patterns.
Keep their language/pattern limitations visible. Report observed constructs and
consequences, not speculation about AI authorship; do not manufacture findings.

## Local review workflow

1. Resolve the exact recorded `--change`, `--intent` or `--turn` selection. Use
   `--json` for the machine-readable report or `--html` for the offline viewer.
2. Inspect the audit and metric card coverage, measured values, source spans and
   recorded before/after evidence. The viewer's all-cards tables include cards
   with no findings or no applicable measurements; nothing is silently passed.
3. Preserve the CLI-assigned finding numbers and selected roots. Keep criterion
   declarations distinct from local evidence and explicitly supplied observations;
   missing criteria or unsupported semantic checks remain pending/blocked.
4. Do not invoke a model or another agent, add an AI assessment, mutate candidate
   source, run reviewed tests or expand dependency references into reviewed roots.

`--findings` remains an optional import of explicitly supplied observations with
local scope/coordinate validation, not an automatic AI review. The publication
command below is a separate, explicitly selected workflow, not part of HTML export.

## Review-only budget and stop rule

### Risk summary and caller investigation

Use `graph.risk` for rating, confidence, coverage and numbered findings, and
`graph.risk.impact` for recorded changed functions, before/after callers, outgoing
callees, availability/access/arity checks and gaps. Review reuses the existing
semantic parser registry and recorded syntext search: changed entities first,
candidate references second, shared AST context/binding checks third. It does not
parse unrelated files until the budget expires or join global simple names.
Qualified targets, lexical owners, receivers and imports remain available in
shared semantic facts. `statically_bound_direct_call` edges retain binding evidence;
`surviving_reference_to_removed_function` preserves a removed declaration's callers.
Concrete call-contract incompatibilities become automatic numbered risk findings.
Unresolved dynamic/type/cross-crate bindings are not guessed. Normal cfg/attributes
and macros retain source context; configuration-dependent evidence is labeled.
Do not equate scoped static compatibility with implementation correctness or low
overall risk. Inspect relevant unresolved consumers using recorded imports,
visibility, receiver/type context and contracts; discard common-name collisions.
`graph.risk.model` uses researched dimensions and explicit Atomic thresholds to
estimate review risk from breadth, production-consumer exposure and contract churn.
High estimated risk can coexist with zero confirmed defects. Inspect triggered rule
IDs and facts; do not turn rule triggers into F-number findings or criterion failures.
Missing bindings reduce confidence, not measured exposure; unresolved expressions
never count as callers. Test/support changes are not runtime test coverage.
A caller alone is not a bug: a finding
must explain the changed behavior/contract, affected consumer, consequence and
supporting source/diff/test evidence. Stop with an explicit gap when resolution
needs unbounded work.

The scope-pinned `--findings` envelope optionally accepts `risk_assessment` with
`rating` (unknown/low/medium/high), `confidence` (low/medium/high), `coverage`
(complete/partial/unavailable), `rationale`, common-contract `evidence`, and
`limitations` (a string array). Low requires evidenced, complete selected-scope
inspection, not an empty findings list. Partial/unavailable coverage needs explicit
limitations. An evidenced Medium/High estimate does not require an invented defect;
the assessment cannot downgrade known risk findings. The default versioned rubric
and optional `--risk-profile <JSON>` are documented in `atomic-audit/RISK.md`.
These remain report-local judgments, not compiler proof or signed approval.
Human terminal review animates “Thinking...” during recorded analysis; JSON and
pipes stay clean. This indicates actual bounded analysis, not a model invocation.

A review request authorizes diagnosis, not remediation. Identify what is missing,
sloppy or risky, present numbered evidence-backed findings, then stop. Proposed
corrections are recommendations only; do not implement them, rewrite criteria,
create remediation intents, refactor code or repair the environment without a
separate explicit request.

Keep investigation proportional: reuse the CLI scope/evidence, inspect changed
code and the most relevant requirements/callers, and run only focused checks
needed to substantiate a finding. Do not launch full-workspace suites, repeated
builds, exhaustive dependency scans or debugging loops to make failures pass.
A failing check is evidence to report, not an instruction to fix it. If further
certainty needs substantial work, report the risk, confidence and coverage gap
instead. Stop after the three bounded passes (or explicit blockers), with a
concise graph/findings summary and observed verification. No automatic fix phase.

## A view argument means independent publication

For `@review --view a` or `@review --view a,b`, invoke:

```sh
atomic review publish --view a
atomic review publish --view a,b
```

There is one `--view` option, not a separate `--views` spelling. The CLI freezes
the recorded scope, preflights issuer authority, asks once for bounded local
ephemeral authorization, runs an independent OpenCode publisher in a temporary
recorded-only working copy, dispatches genuine session_end after child completion,
verifies native completion evidence, and retains exact-scope signed approval.
It owns credentials, lifecycle and cleanup. Do not provision grants, launch a
second agent, call session-end, or build a VM/transfer pipeline from this skill.

Use `--yes` only when the user has already approved the displayed exact local
scope/authority. No production enrollment or shared identity-default mutation is
part of this command. `--identity` selects a trusted local issuer explicitly.

Other supported operations:

```sh
atomic review publish --session <session>
atomic review publish --view a,b --dry-run --json
atomic review publish --resume <job-id>
atomic review publish --cancel <job-id>
atomic review publish inspect
```

An unknown/crashed process outcome is not guessed complete or blindly relaunched:
preserve its job and follow the actual recovery diagnostic. Cancellation retires
recording credentials; it is not withdrawal of a previously published approval.
Collection publication checks combined compatible inputs, deduplicates history,
and rejects ambiguous overlapping deltas or unsupported symlink composition
rather than picking a winner. An explicit integration view is needed for those
cases; the review command never creates one for the user.

## Results and boundaries

Distinguish inspection, independent review, native session completion and retained
publication approval. Report the CLI's actual blocker and job ID when blocked.
Do not revoke a still-resumable job merely because one step failed, silently renew
its authorization, weaken policy, infer intent ownership from shared filenames,
or fabricate historical session capture. Native work-session capture and a
present-day retrospective approval remain different evidence.

Publication means signed review finalization, **not insertion or push**. Those
remain separately requested operations. Legacy triage and advanced external
signing commands remain compatible; they are not extra skill-level workflows.
Never run git, atomic add or atomic record for this workflow: normal agent hooks
record actual work and provenance. If the installed CLI lacks `atomic review`,
report the binary/version mismatch instead of simulating the missing command.
