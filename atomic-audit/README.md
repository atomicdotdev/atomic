# atomic-audit

Tool-neutral engineering audit cards and manageability metrics for Atomic review.
This crate reports evidence and review signals. It never fixes code, edits an
acceptance criterion, opens a repository, launches tests/tools, or grants approval.

## Ownership

```text
atomic-audit
├── catalog      readable audit and metric cards
├── metrics      explicit measurements, profiles, advisory bands and deltas
├── extract      automatic multilingual source-syntax metric collection
├── impact       compatibility policy over shared semantic source facts
├── risk         versioned, explainable change-review risk rules
└── structural   executable AST audit adapter (substantive rules included)
        └── atomic-semantic: grammar registry and parsing

atomic-cli / atomic-repository: recorded scope, source retrieval and adapters
atomic-canonical: common findings, criterion assessments and review graph
```

The default build needs only Serde. Enable `structural` to run all ten audit
families using `atomic-semantic`'s existing grammar instances and syntax trees.
Ruleset 2 adds executable substantive checks; the original three rule identities
remain stable. See [implemented patterns, languages and limits](AUDITS.md).
`atomic review` runs these checks against recorded before/after content and
projects evidence-backed matches into numbered findings. No second parser is introduced.

## Readable cards

`catalog::AUDIT_CARDS` describes each audit's question, required evidence,
exceptions and actual automation coverage. The initial families cover syntax,
unfinished behavior, empty catches, failure masking, duplication, unnecessary
machinery, manageability, async/resource safety, weak verification and safety
bypasses. Each family has executable checks with documented language/pattern
coverage; completing a pattern check is not exhaustive behavioral proof.

`catalog::METRIC_CARDS` includes:

| Metric | Subject | Measurement |
| --- | --- | --- |
| Method/function length | Function | Code-containing body lines |
| Class length | Class | Code-containing class-body lines, including methods |
| Methods per class | Class | Directly declared methods |
| Parameter count | Function | Declared input slots |
| Nesting depth | Function | Maximum control-flow nesting |
| Cyclomatic complexity | Function | Decisions under a declared language convention |
| Aggregate class complexity | Class | Sum of directly declared method complexity |
| Dependency fan-out | Module | Distinct direct dependencies |
| Public surface | Module | Externally visible declarations |
| Duplicated tokens | Change | Unique changed token positions in clone matches |

Cards contain the precise counting boundaries, interpretation and exceptions.
Class metrics are not applicable to constructs without class semantics. Do not
turn every Rust struct or data record into an object-oriented design warning.

## Automatic extraction

With `structural` enabled, `extract::extract_changes` automatically collects all
ten metrics using the existing registered Rust, TypeScript/TSX, Python, Java, Go,
C/C++ and Swift parsers. Supply recorded before/after `FileChange` inputs; the
extractor does no filesystem I/O, builds, tests or external tool invocation.

```rust
# #[cfg(feature = "structural")]
# {
use atomic_audit::extract::{extract_changes, ExtractionConfig, FileChange, Source};
use atomic_semantic::ParserRegistry;

let inputs = vec![FileChange {
    path: "example.ts".into(),
    before: Source::Text("export function work(x: number) { return x; }".into()),
    after: Source::Text("export function work(x: number, y: number) { return x + y; }".into()),
}];
let result = extract_changes(&inputs, &mut ParserRegistry::new(), &ExtractionConfig::default()).unwrap();
assert!(result.metrics.iter().any(|record| record.observation.metric == atomic_audit::Metric::ParameterCount && record.delta == Some(1)));
# }
```

Results include convention `atomic.metrics.syntax.v1`, recorded-side line
locations, before/after values and safe deltas. Named subjects are paired within
their qualified scope. Ambiguous overloads/anonymous subjects do not acquire an
invented baseline. No applicable class/function is `not_applicable`; parse/read
failures, unresolved facts and budgets are `unavailable`, not zero.

See [counting conventions and coverage](CONVENTIONS.md). These are source-syntax
metrics, not compiler-resolved runtime facts or measurements of human cognition.
Public surface means declared visibility; fan-out means explicit dependency
units. Wildcard/dynamic export expansion and unresolved implementation bodies
are not guessed.

`atomic review` collects metrics from exact selected recorded before/after
source under `changes[].analysis.metrics`, with `metric_diagnostics` and
`metric_comparison_scope`. Graph metric nodes link cards and source anchors.
The terminal shows every family, comparable numeric changes and selected actual
values/locations; --details and JSON retain every collected observation.
Metrics are observations, not automatic F-number findings or unmet criteria.
The separate manageability audit requires joint source/control-flow evidence and
recorded new/worsened burden under [explicit Atomic review policy](AUDITS.md).

Clone comparison is exact-token, same-language and restricted to supplied paths
(changed files by default in review). It counts only changed token positions and
unions overlapping matches. It is **not** a repository-wide duplication score.
Callers may include unchanged comparison files with identical before/after text.

## Direct-call impact and contract evidence

With `structural` enabled, `impact::analyze_semantic` consumes already-parsed
`atomic-semantic` source sets. The existing parser registry owns entity extraction,
qualified calls, receivers, scopes and relationships; this crate owns compatibility
policy, not another AST parser. `impact::analyze` remains a convenience adapter for
in-memory source pairs, delegating to that same registry. Review uses recorded
syntext-backed candidate selection before parsing callers, not a whole-project
AST pass. It measures incoming/outgoing supported relationships and checks
declaration availability, access and declared fixed/optional/rest argument counts.

```rust
# #[cfg(feature = "structural")]
# {
use atomic_audit::impact::analyze;
use atomic_semantic::ParserRegistry;
let before = vec![("lib.rs".into(), "fn work(x:i32) {} fn consumer() { work(1); }".into())];
let after = vec![("lib.rs".into(), "fn work(x:i32,y:i32) {} fn consumer() { work(1); }".into())];
let report = analyze(&before, &after, &["lib.rs".into()], &mut ParserRegistry::new());
assert_eq!(report.callers_after.len(), 1);
assert_eq!(report.issues.len(), 1);
assert!(report.issues[0].introduced);
# }
```

The CLI turns concrete contract issues into numbered findings with source evidence.
Separately, [change-review risk rules](RISK.md) use measured breadth, consumer
exposure and contract churn to prioritize review, even with zero confirmed defects.
These are Atomic policy bands, not calibrated defect probabilities. Incomplete
evidence limits confidence without erasing measured exposure. Neither a risk
estimate nor metric growth creates a defect finding or criterion failure.
See [impact coverage and safety boundaries](IMPACT.md).

## Advisory policy

`Measurement` distinguishes a measured value (including real zero), unavailable
with a reason, and not applicable with a reason. Missing baselines have no delta.
There are no universal limits or default pass/fail gates. A project selects a
named `MetricProfile` with explicit inclusive review/high-review thresholds.

```rust
use std::collections::BTreeMap;
use atomic_audit::{evaluate, Measurement, Metric, MetricObservation,
                   MetricProfile, ReviewBand, Threshold};

let observation = MetricObservation {
    metric: Metric::MethodLength,
    subject: "src/report.ts#ReportBuilder.build".into(),
    before: Measurement::Measured { value: 34 },
    after: Measurement::Measured { value: 112 },
};
// Illustrative project policy, not a universal good-code standard.
let profile = MetricProfile {
    name: "example-typescript-service".into(),
    limits: BTreeMap::from([(
        Metric::MethodLength,
        Threshold { review_at: 50, high_review_at: Some(100) },
    )]),
};
let assessment = evaluate(&observation, &profile).unwrap();
assert_eq!(assessment.delta, Some(78));
assert_eq!(assessment.band, ReviewBand::HighReview);
```

The next question is whether that growth obscures responsibilities or failure
paths. A high band does not prove slop, a regression, missing tests, or an unmet
acceptance criterion. Adapters/reviewers make an evidenced finding when justified;
the existing review layer assigns finding numbers and validates criterion links.

## Focused validation

```sh
cargo test -p atomic-audit
cargo test -p atomic-audit --features structural
cargo test -p atomic-cli commands::review --bin atomic
```
