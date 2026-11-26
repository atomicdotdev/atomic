# Structural and anti-slop review contract

## Local-only execution

`atomic review --change <hash> --html` is a local recorded-source review using
the existing `atomic-semantic` parser/relationship facts and `atomic-audit` checks
and metrics. It does not call a model, use credentials or start another coding
agent. HTML is self-contained and makes no network requests.

All defined audit and metric cards appear in the viewer's all-cards coverage
tables, including cards with no findings. Audit rows show completed/skipped/error
counts and finding totals. Metric rows show every observation's before/after
measured, not-applicable and unavailable counts. Checked is execution coverage,
not a passing correctness verdict; unsupported cases never become fabricated
passes or zero-valued measurements. Registered pattern/language coverage remains
explicit in each card and in `atomic-audit/AUDITS.md`.
Selecting a metric card also exposes every recorded observation, with its exact
change, path, subject, before/after values and ranges, reasons and delta; no
finding-only filter or representative sample hides the other observations.

No AI validation or second model stage is part of ordinary review. Explicitly
imported observations and the separate publication workflow are not silently run
by local HTML export.

The HTML landing page is a dedicated **Summary** tab. Report-wide observation
counts, before/after pins and file states, all-card/coverage tables, governance
context and risk estimates live there once. The three review tabs focus on the
selected file, region or criterion, without repeating those report-wide blocks.
Summary actions and card links open their review details; returning from Summary
preserves the selected region. Summary hides the source navigation, filter bar
and card inspector until a detail section is opened, with keyboard and mobile
tab navigation preserved. No JSON findings or evidence are removed or capped.

## Entire named view

```sh
atomic review --view dev
atomic review --view dev --json
atomic review --view dev --html --no-open --output dev-review.html
```

`--view NAME` audits each live file in the canonical recorded view snapshot once,
including its parent-chain/dependency visibility. This is not a history replay:
old revisions, deleted paths and unrelated views do not contribute measurements
or old findings. Dirty/staged/untracked files and working-copy attributes are not
used; symlinks are not followed; `.vault` remains excluded from code analysis.
No working-copy view switch or database mutation is performed. Names are exact,
unknown names fail, and empty views produce an honest no-code report with all
cards still available. The view selector is exclusive with turn/intent/change
selectors and with change-specific finding/risk-profile input.

Snapshot reports have `schema_version: 3`, `selection.kind: view`, and
`view_snapshot` containing `source_ref`, view/state/effective-membership pins,
live paths and `analysis`. Existing change reports retain their v2 shape.
`changes`/`selected_roots` are empty in snapshot mode. Membership is metadata-only
in `dependency_closure`/`dependency_references`; no historical patches or file/token
payloads are included. The references' direct flags are false because there is
no selected change whose direct dependencies are being reviewed.

`source_ref` is a `urn:atomic:view-snapshot:...` descriptor, not a fabricated change
hash. The existing source-reference fields (`finding.change`, check `change`,
metric-node `change`) carry that URI in this mode. View state and effective
membership are checked through analysis/export, so inherited-parent changes also
invalidate the pin even if the child's own Merkle is unchanged.

The same local grammars, audit rules and metric extractor run on current recorded
source. Existing issues are visible even when unchanged by the latest change.
Introduction remains unknown; there is no diff/before revision or computed delta.
The uniform metric contract retains an unavailable-before reason, not an absent
file or zero baseline; HTML and terminal present current snapshot values. Formal
conformance includes explicit view-assigned intents and their declared criteria,
not inferred change ownership or automatic acceptance approval. Change-regression
risk is unknown, not guessed from absolute snapshot metrics. Dependency/public-surface metric cards
remain available without claiming a before/after call-contract analysis.

Whole-view analysis does not inherit the per-change 128-file ceiling. It uses
explicit bounds of 1 MiB per source, 128 MiB total, 2 million tokens, 65,536 subjects
and 2 million clone-pair comparisons, plus the existing per-file extraction limits.
Unsupported/binary/over-budget cases remain skipped/unavailable with reasons,
never silently passed. HTML source-text export retains its separate existing
128-file / 8-MiB limits; all analyzed metric observations remain in JSON/HTML.

## Compare two recorded views

```sh
atomic review --from feature-shipping --to dev
atomic review --from feature-shipping --to dev --json
atomic review --from feature-shipping --to dev --html --no-open --output comparison.html
```

Both names are exact and required. From is the proposed source; To is the
destination/base, matching pull-request direction. Measurements are target-before
and source-after, not an ancestor range or a sequence of patches to replay. The selectors
conflict with `--view`, change/turn/intent selection and `--findings` /
`--risk-profile`. Self-comparisons, inherited views and empty views are valid.
Both effective memberships and view Merkles are checked through analysis and
HTML export. No working-copy switch, mutation, dirty/staged/untracked source,
model call, credential or runtime network request is involved.

Comparison reports use `schema_version: 5`, `selection.kind: view_comparison`,
`purpose: local_view_comparison_review` and `view_comparison` with `from`/`to`
pins, explicit `before` (target) / `after` (source) pins, diagnostics and `files`.
The v5 direction is not the earlier v4 before/after flag convention. The authored `changes` and `selected_roots` stay
empty; historical changes remain metadata references only. A domain-separated
comparison URI identifies the ordered pair of pins; each file has its own
`source_ref`, before/after path, inode and (when inspected) content hash, file and
content status, `analysis` and `finding_states`. Distinct units may share a
display path during rename-overwrite: resolve sources by reference, not filename.
Metric nodes/findings use the per-file URI; checks retain their side's snapshot
reference. These are report descriptors, not authored change identities.

File pairing reserves same-path/same-inode matches, then unambiguous inode
renames, then same-path replacements, then additions/removals. It never infers
identity from text or similar names. Canonical recorded name projection excludes
obsolete aliases even if recovery indexes still remember live inode content.
Recreating a path can be a recorded restore of its inode rather than a replacement.
Uninspected/unsupported content is `unavailable`, never falsely `unchanged`.

Every side is measured independently with the same whole-view bounds described
above. Whole-view duplicate-token counts include all current tokens against the
side-wide same-language corpus, not only tokens changed by the latest patch.
Per-file analyses retain those side-wide results. Named, qualified metric owners
are normalized across recorded file renames; anonymous/overloaded identities are
explicitly ambiguous and never matched numerically by display ordinal. Removed
or replaced file identities do not establish same-named function/class continuity;
file-level path-slot metrics can still compare a replacement. Removed
or added subjects are not numeric zeroes, and diagnostics, syntax recovery and
budgets cannot become absent-subject baselines. `delta` is present only when
both sides are measured under compatible conventions and subject identities.

The optional `graph.findings[*].comparison` records lifecycle status, before/after
locations and rationale. Exact rule/AST-byte fingerprints in the same recorded
file and qualified metric owner are reserved first as `existing`, regardless of
line shifts or renames. Named manageability owners support continuation through
body edits. Remaining same-rule/owner observations with changed bytes are
`changed`, not claimed equivalent. Unmatched after/before observations are
`new`/`resolved` only with completed counterpart rule coverage or proven file
absence; otherwise they are `unknown`. These are local observation lifecycles,
not defect verdicts or proof of behavioral equivalence. Authored introduction is
unknown. Conformance and blast radius are populated from their own recorded
evidence rather than guessed from audit/metric deltas. The HTML offers direction
labels, lifecycle filters, a file state table and pinned before/after source
controls alongside all twenty cards and all three review passes.

`view_comparison.conformance.changes` retains the effective membership difference
in both directions (`added_to_after` / `removed_from_after`), path references and
actual recorded governing-plan contexts. Common history is not swept into this
scope. Independently, `conformance.scope.view` identifies the explicitly reviewed
view (From/source for a comparison), and `conformance.intents` enumerates its explicitly
assigned intents by canonical ID, human key, title, status and source. An intent
does not need a recorded change-plan link to appear in this view-work catalogue.
Neither mere file visibility in an inherited view nor shared task paths assigns
an intent to the target. Actual missing/partial/ambiguous governing bindings stay
diagnostics, not a reason to hide valid view-assigned work or fabricate ownership.

The Conformance sidebar groups criteria beneath intent keys/titles, includes
selectable intents with no available criteria, and searches intent title/key,
criterion text/ID and related F-numbers. Any actual change-linked criteria outside
the source's explicit assignment are separately labelled change-linked context,
not silently presented as view-assigned work. Single-view reports carry the same
catalogue under `view_snapshot.conformance` without inventing a comparison.

The conformance `criteria` retain before/after declarations, raw recorded source
hashes, author status, verifier/evidence claims, verification records/required kinds
and linked task/file comparison references. Prefer intent definitions rendered
from each pinned recorded view. Parsing is bounded to 1 MiB per intent and 16 MiB
of intent source per side; omitted/unreadable definitions are explicit. When no
recorded definition is available, a source-hashed current vault DB definition may
be included under `current_context`, labelled `current_vault_context`, with no
invented historical before/after status. That context is checked for drift during
export. The current assignment catalogue is also source-hashed under
`scope.current_inventory_hash`; new/reassigned/removed scoped intents and changed
current definitions invalidate export rather than reusing stale context. Recorded
source-view intent definitions remain available even if their current vault entry
is absent, and are preferred over dirty current definitions. Neither declarations nor evidence-command strings are executed or treated
as independently authenticated approval; arbitrary semantic acceptance judgments
stay unknown. Engineering findings in task files are related navigation evidence,
never automatic unmet-criterion verdicts.

Blast radius uses the same `RecordedSemanticView` selector and
`atomic_audit::impact::analyze_semantic` policy as ordinary change review, but pins
the two named effective states instead of a current-view patch prefix. Changed
roots are selected before parsing caller context; before/after static callers,
callees, declaration/access/argument-count checks and the rule-based risk estimate
are populated. It does not replay intermediate changes: a break fixed before the
source-after state is not presented as a current contract break. Source/path/parser/search
budgets and unsupported/dynamic bindings remain explicit coverage gaps. Semantic
callable IDs remain module/path-addressed, so a file rename can change bindings.
Compiler-resolved types, runtime behavior and executed-test coverage are not
claimed. Self-comparisons and metadata-only state differences do not trigger a
code-risk scan. Existing/resolved audit observations do not become new structural
risk alerts.

Every detected contract issue is projected into the numbered finding set; there
is no 100-finding display truncation. JSON, the embedded HTML startup graph and
the downloadable original JSON contain the same complete finding set. The viewer
renders every finding via its file/region navigation, including beyond F100;
bounded source-text export does not remove findings or their evidence.

The viewer embeds Atomic's actual wordmark PNG and uses the verified violet/pink
website palette. No remote fonts, website code, analytics or asset requests are
included. See `src/commands/review/assets/README.md` for source URLs, hashes and
the proportional downsample. Local installed font names have system fallbacks.

## Attested native evaluation and accepted degradation

`review publish --from draft --to dev` resolves this same native report. It never
starts an agent/model, creates credentials, invokes a runner or requires a vendor's
provider configuration. A trusted existing local identity attests what it evaluated.

`--dry-run --json` emits full evidence, stable normalized report/scope digests,
all outstanding issues and an unfilled `evaluation_template`. Fill that template
or supply it alone through `--evaluation FILE`. Every pass must be explicitly
evaluated with a rationale. `ready` requires no outstanding evidence;
`accepted_degradation` must name every waivable issue exactly once with a nonempty
rationale, and may attach evidence/follow-up references. `not_ready` retains a
negative decision without authorizing insertion. `--yes` is confirmation only,
never an opaque acceptance or waiver.

The signed `atomic.review-decision.v1` retains the complete report, its digest,
source/target state and membership, exact insertion closure, current intent-context
pins, every issue and each pass evaluation. Evidence readiness remains `not_ready`
when shortcomings exist even if user insertion disposition is `accepted_degradation`.
No independent approval, test execution or clean-code claim is inferred. The
published HTML labels these dispositions and links acceptance rationales to the
actual finding/criterion; its JSON download contains the complete attested decision.

The existing shared-insertion evidence boundary validates applicable retained
decisions, signatures/trust, exact scope/selection, intent context and dispositions
before mutation. A decision hash is retained in insertion operation evidence and
the outcome reports accepted degradation. Stale scopes, bad signatures, untrusted
signers and original provenance/integrity blockers cannot be waived as code debt.
Known not-ready decisions refuse insertion. Current evaluations are replaced only
by explicit signed `--supersedes HASH`, not by rewriting history; legacy insertion
without a native decision retains its existing provenance policy.

Native publication is not an automatic insert or push. `publish inspect` exposes
retained reports and current applicability. Existing retrospective
`prepare/sign/record` tools retain their stronger review-intent/authority contract;
a native risk evaluation is not fabricated original session capture. Old agent
jobs may only be explicitly cleaned up with `--cancel UUID`; no native resume,
provider, runner, or agent-execution path is offered.

## Three-pass graph and numbered findings

`atomic review` renders a location-first report with anti-slop, intent-conformance and
blast-radius/risk passes. `--json` exposes `graph` (`atomic.review-graph.v1`)
with the same nodes/edges plus exact scope, pass coverage, criterion assessments
and numbered findings. `--details` retains the verbose worklist, raw diffs and
every metric observation without a row cap.

The default anti-slop report groups numbered concerns by recorded file and line
range, with audit/rule IDs and concise metric-card callouts. It does not expand
the evidence graph or dump every measurement. `--details` preserves full evidence,
audit coverage, all ten metric families and every observation without truncation.
Metrics are not a substitute for executable audits; see `atomic-audit/AUDITS.md`
for supported patterns/languages and exclusions.

## Offline HTML review workbench

```sh
atomic review --change <hash> --html
atomic review --intent <id> --html
atomic review --change <hash> --html --no-open --output review.html
atomic review --change <hash> --html --no-open > review.html
```

`--html` writes a unique self-contained artifact and opens it using the existing
HTML-export convention. `--no-open` emits HTML to stdout, or to `--output` when
provided; it never opens a browser. HTML conflicts with `--json`/`--details`, and
`--output`/`--no-open` require HTML. The canonical graph schema is unchanged;
the enclosing inspection report is now `schema_version: 2`.

JSON and HTML use the same export boundary: `changes` contains selected roots
only, with their paths, review results and recorded evidence. `dependency_closure`
includes roots in dependency-first order; `dependency_only` lists non-roots.
`dependency_references` contains only non-root hashes, direct-dependency flags,
current-view membership, header summaries (first line, at most 240 characters),
and dependency hashes. Ancestor paths, patches, file/token payloads, diffs,
analyses and governing intent documents are not collected for normal export.
Dependencies are causal references, not additional reviewed roots.

The viewer uses the same numbered findings, scope, criteria and risk assessment:

- **Anti-slop:** file/region navigation, recorded source before/after, exact finding
  evidence/corrections, and audit/metric card inspection. Findings open first;
  changed/all metric-region filters expose every observation, not just a sample.
- **Conformance:** reviewer assessment is separate from the author's declared
  status. Mapped findings navigate to their recorded source. Missing governing
  criteria remain blocked; current intent definitions are not authenticated history.
- **Blast radius:** explainable risk rules/profile, production/test-support facts,
  actual bound reference evidence and source locations. Caller context is labeled
  as context, not an additional reviewed root; unresolved names are not consumers.

All twenty audit/metric definitions remain available, including their evidence,
counting conventions and exceptions. Enclosing-function metrics are identified as
context rather than measurements of an individual branch. Opposite-side source
highlights require a genuine matched metric/function location; otherwise the UI
states that no reliable region mapping exists. A change diff is explicitly the
whole selected change, not a fabricated per-region diff.

Additional source snapshots use recorded content APIs only for selected paths,
prioritized by concerns. Caller context retains bounded recorded references and
span-based evidence, never whole caller files outside selected paths. Export is
bounded to 128 eligible file snapshots, 1 MiB per source side and 8 MiB total
source text; omissions are explicit
unavailable states, not silent truncation or dirty-byte fallbacks. `.vault` code
source stays excluded while governing intent/diff evidence remains in the report.

There are no external scripts, fonts, CDNs, analytics or server requirements.
Embedded JSON escapes markup delimiters; DOM rendering uses text nodes and a
nonce-based restrictive CSP. UI startup uses only the report fields it displays;
the same bounded JSON report (including dependency references and graph edges) is
retained as a gzip archive and restored only when downloading JSON. Native browser
decompression needs no library/network request; older browsers can download JSON.gz.
Downloads preserve original serialized text, including integers outside JavaScript's
precise-number range. The artifact includes
source/evidence: keep it private if that content is sensitive. Neither viewing nor
exporting it performs fixes, changes criterion judgments or grants approval.

Findings are deterministically numbered F1, F2, ... within that report. An
identical observation shared by several passes has one number. Numbers are
navigation labels, not permanent cross-revision identities. Nodes and local IDs
are a read-only report projection, not newly asserted persistent KG facts.
File resources retain their existing canonical Atomic IDs. Source locations
include exact change, side and line range; enclosing functions/types and static
caller/importer context are connected when available.

Direct impact uses recorded view-prefix source immediately before/after each
selected change. Earlier unrelated files are caller context, not extra reviewed
roots. No later or dirty source is used. Unavailable/off-view snapshots are gaps.
Changed source is parsed first through the existing `atomic-semantic` registry.
Recorded syntext path/content search selects relevant references before AST
budgets; owner/private scope constrains candidates. Matching source and explicit
import/module context use the same semantic facts, not a review-specific parser.
The adapter bounds selected changes, changed/matching files and context separately;
see `atomic-audit/IMPACT.md`. Search counts and parsed-file counts are distinct.

`graph.risk.impact` and `direct_impact_analysis` nodes retain changed functions,
before/after callers, outgoing direct callees, binding evidence, compatibility
checks/issues and gaps. Function roots use token-level before/after comparison,
not every function in a changed file. `statically_bound_direct_call` edges require
supported lexical/import/module binding; `surviving_reference_to_removed_function`
preserves a before-bound symbol absent after the change; `inaccessible_direct_reference`
retains a reference rejected by supported export/access rules. These rejected
references are not counted as legal current callers. Review no longer generates
global-name caller candidates. Legacy unverified candidate-count fields remain
zero for compatibility, not measured caller totals.

Shared source facts preserve qualified targets, receivers, owners, methods and
imports from registered grammars. Ordinary attributes/macros are not reasons to
discard whole files. Supported lexical, declared-receiver and import/module
bindings are checked; ambiguous scopes, unresolved dynamic/type/cross-crate
context and actual truncation remain scoped gaps. Configuration-dependent source
facts are labeled rather than presented as active compiler configuration.
See `atomic-audit/IMPACT.md`. Counts describe scoped static bindings, not
runtime reachability or whole-program proof. All passes retain honest partial/
pending/blocked states; there is no fabricated semantic-conformance verdict.

## Risk assessments, confidence and coverage

`graph.risk` and the `risk_summary` graph node expose a report-local assessment,
its basis (reviewer/findings/rule_model/no_code), numbered risk findings and measured
direct impact. `graph.risk.model` retains `atomic.change-risk.v1`, the expanded
profile, role-separated facts, triggered rule IDs/reasons and recorded references.
See `atomic-audit/RISK.md` for research sources and Atomic-owned thresholds. This
is a review-priority estimate, not defect probability, signed approval or a bug
count. `--risk-profile <JSON>` explicitly overrides documented thresholds.

Default review checks availability/access/arity on affected direct references. Concrete
incompatibilities become `compatibility.resolved-direct-call-contract` findings
without supplied observations, with source/binding evidence and honest introduction
attribution. The direct-call compatibility verdict is separate from overall risk.
Broad production changes, cross-component API churn and existing-consumer exposure
can establish elevated estimated risk with no confirmed issue. Partial coverage
limits confidence, not the measured positive triggers. Unresolved names never count
as callers or raise severity. Tests/support are separated; changing tests does not
prove execution/coverage. Known risk findings remain a severity floor. Rule triggers
are `risk_rule` nodes, not fabricated findings or criterion failures. Metadata-only
code risk remains not applicable; intent/publication checks stay separate.

The existing exact-scope findings envelope accepts an optional assessment:

```json
"risk_assessment": {
  "rating": "unknown",
  "confidence": "low",
  "coverage": "partial",
  "rationale": "The recorded implementation was inspected but consumer binding remains unresolved.",
  "evidence": [
    {"kind": "source", "reference": "<recorded change/path:lines>", "observation": "<observed contract and inspection result>"}
  ],
  "limitations": ["<specific unresolved consumer or verification gap>"]
}
```

Rating is unknown/low/medium/high; confidence is low/medium/high; coverage is
complete/partial/unavailable. Complete means reviewer-declared inspection of the
selected scope, not an exhaustive project certification. Rationale and referenced
evidence are mandatory. Partial/unavailable coverage requires limitations; low
requires complete scoped coverage for an explicit reviewer judgment. An evidenced
Medium/High estimate need not claim a defect; assessments cannot downgrade scoped
medium/high risk findings. Automatic estimates remain retained beside overrides.
The CLI validates scope/contract consistency, not the truth of a reviewer's claims.
Ratings are neither authenticated approval nor automatic acceptance verdicts.

Substantive reviewers must validate relevant candidate relationships against
recorded imports, visibility, receiver/type context and contracts. A shared name
is insufficient, and a real caller alone is not a finding. Report concrete
implementation/compatibility consequences with locations, evidence and corrections;
keep investigation bounded and state unresolved coverage rather than guessing.

## Criterion failures are explicit review judgments

The common finding can include:

```json
"acceptance_criteria": [
  {
    "criterion": "<exact criterion ID from graph.criteria>",
    "assessment": "unmet",
    "rationale": "<why the cited evidence contradicts this criterion>"
  }
]
```

Only explicit, evidenced judgments create `assesses_unmet` graph edges and roll
the criterion's `review_assessment` to `unmet`, linked to its finding numbers.
Missing evidence or inconclusive linkage is `unknown`, never an invented pass
or fail. The criterion's separate `declared_status` is preserved. Default review
does not change the signed source intent. Shared file paths produce only
`related_criterion_not_a_verdict` edges within actual governing intent context;
they never establish ownership or failure.

Use `atomic review --change <hash> --json` to obtain the exact `graph.scope`.
An inspector can supply a file to `atomic review --change <same-hash>
--findings <file>` with this envelope:

```text
{
  "schema": "atomic.review-findings.v1",
  "scope": <copy the complete graph.scope object unchanged>,
  "findings": [
    {"pass": "intent_conformance", "finding": <complete common finding>}
  ]
}
```

This is a template, not literal runnable JSON. `pass` may also be `anti_slop` or
`blast_radius_risk`. A copied structural finding can be enriched with criterion
judgments without creating a second finding number. Input is capped at 1 MiB /
500 findings. Every finding needs evidence and valid recorded coordinates; each
criterion must belong to the finding change's unambiguous governing intent.
The selected roots, source view/state/inherited membership and current intent
source hashes must still match. Reject scope drift, fabricated criteria and
out-of-range coordinates rather than silently widening the review. Supplied
observations are not authenticated independent approval, even when well formed.

Perform both correctness and anti-slop inspection against the exact recorded
scope. For each selected recorded root, `atomic review --change <hash> --json`
returns `changes[].analysis`. Dependency-only changes appear only in
`dependency_references` unless independently selected; do not claim they were inspected.

Use the analyzer-neutral `atomic.review-analysis.v1` envelope and the typed
`atomic_canonical::review_finding` contract for findings from every analyzer or
review skill. Include analyzer identity/version, checks executed and skipped or
errored, and findings. No findings plus skipped checks does not mean a clean pass.

Each finding carries:

- `analyzer`, exact recorded `change`, and a `summary` of the issue/change.
- `locations`: recorded `path`, `side` (`before` or `after`), `start_line`,
  `end_line` (1-based, inclusive). The initial structural analyzer emits after-side
  locations; substantive review can cite before-side evidence for deletions.
- `requirement_or_rule`: `id` and an explanation of the affected requirement or
  engineering rule. Cite an actual requirement when available, otherwise name
  the engineering rule; never invent governing intent ownership.
- `evidence`: entries with `kind` (`source`, `diff`, `test`, `command`,
  `reference`), `reference` and observed supporting facts in `observation`.
- `introduced_by_change`: `status` (`yes`, `no`, `unknown`) and a `rationale`
  based on recorded before/after comparison. Unknown is not yes.
- `severity`: `info`, `low`, `medium`, `high`, or `critical`.
- `confidence`: `low`, `medium`, or `high`.
- `proposed_correction`: an actionable correction, not automatic edits.

The shared presence validator rejects missing rules, evidence, correction or
valid locations. Presence validation does not authenticate evidence or prove a
finding is true. Skills should report complete fields, not anonymous complaints.

## Initial deterministic coverage

Default terminal output lists concerns by file/region rather than expanding graph
edges or coverage inventories. The HTML viewer and --details retain every metric
observation, including real zero values and explicit unavailable/inapplicable states.
Unavailable reasons/paths remain grouped in detailed coverage and attached to their
individual measurements. Counts are observations, not files or clean-code verdicts.

Exact `.vault` path components are excluded before code-source reads, structural
checks, metric placeholders and clone/static relationship input. JSON retains
`excluded_metadata_paths`; original selected hashes, paths, diffs and governing
intent/criterion evidence remain intact. Vault-only selections report code passes
as `not_applicable` and skip the project relationship scan, not as a clean audit.
This does not suppress intent conformance or independent publication checks.

The `atomic-audit` crate owns readable audit/metric cards, advisory metric
evaluation and the optional structural checks. The existing
`atomic-semantic::ParserRegistry` supplies tree-sitter trees; parsing does not
depend on audit policy. No ast-grep dependency is added. Metric definitions,
including method/class length, have automatic source-syntax extraction under
`changes[].analysis.metrics`, with before/after locations, values, deltas and
convention `atomic.metrics.syntax.v1`. Check `metric_diagnostics` and the exact
`metric_comparison_scope`; changed-file duplication is not repository-wide.
Unsupported, unresolved and budget-limited measurements remain unavailable;
inapplicable constructs are not zero. Metrics link cards/source nodes in the
graph without manufacturing findings or criterion failures. Ruleset version 2
also executes seven substantive source audits; it is not merely a measurement
inventory. The original checks remain:

- `structural.syntax`: syntax error/missing nodes in every registered language.
  This is parser evidence, not a compiler verdict; grammar limitations can matter.
- `anti-slop.rust-placeholder`: Rust `todo!` / `unimplemented!` macro invocations,
  including qualified calls. Strings/comments are not matches. Shadowed macros,
  intentionally unfinished paths and test scaffolding need contextual review.
- `anti-slop.empty-catch`: TypeScript/TSX catch bodies with no named children.
  A comment or executable handler is left to substantive review rather than
  automatically labeled undocumented suppression.

Additional executable audit families are failure masking, duplicated behavior,
unnecessary machinery, joint manageability, async/resource safety, weak verification
and unsafe shortcuts. Their exact patterns are exposed in check details and
documented in `atomic-audit/AUDITS.md`. Supported positive matches become common
numbered findings with source evidence, correction and before/after attribution,
not just metric counters. Joint manageability uses existing counting code plus
the explicit `atomic.manageability.v1` policy, suppressing unchanged/improving
before-side burden. Isolated metric values remain observations, not bug verdicts.

Unsupported languages/rules, absent after-content, binary/non-UTF-8 content,
parser failure and files exceeding 1 MiB are explicitly skipped or errored.
No general anti-slop coverage is claimed for other languages. Syntax recovery
prevents anti-slop matching for that file. More than 100 matches in a file
requires manual inspection rather than silently truncating a clean result.

Introduction compares exact rule/node-text occurrences as a multiset. Line
shifts alone are not new findings; extra identical occurrences are new syntactic
candidates, not proven behavioral regressions. Missing/unparseable baseline
means unknown, unless an explicit file-creation operation establishes absence.
If the same rule matched different node text before, a changed match is unknown:
editing an existing placeholder does not prove this change introduced it.
The same observed syntax error is pre-existing; different observed errors in two
already-broken revisions have unknown attribution rather than an invented answer.

## Local semantic audit pass

Inspect requirement fit, needless abstractions/dependencies, duplicated behavior,
swallowed failures, unexplained fallback success, unsupported documentation
claims, and tests that do not check observable behavior. Report concrete evidence
and consequences, not judgments that code “looks AI-generated.” Do not fabricate
findings when a minimal implementation is appropriate. Record checks and any
coverage gaps even when there are no findings.

These are inspection candidates, not a new publication gate or signed approval.
Existing publication trust, isolation, lifecycle and verification rules remain
unchanged. Do not repair the implementation in the independent review copy.
