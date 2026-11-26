# Executable anti-slop audits — ruleset 2

All ten audit cards have executable checks. The seven substantive families are
no longer guidance-only: matches flow through the same recorded before/after
adapter into numbered findings, with source locations, concrete observations,
introduction attribution and proposed corrections. Nothing here runs tests,
fixes code, rewrites acceptance criteria or grants approval.

These checks inspect specific source constructs. Completing a check means its
documented patterns were inspected, not that the entire behavioral concern was
proved absent. Unsupported languages, syntax recovery and budget exhaustion
remain explicit. The parser registry and its trees are reused; there is no new
parser, whole-project scan, global-name caller join or compiler/type resolver.

## Implemented checks

| Audit | Executable evidence | Languages | Important exclusions |
|---|---|---|---|
| Syntax | Error/missing syntax nodes | All nine registered syntax modes | Parser evidence, not compiler proof |
| Unfinished behavior | Actual `todo!`/`unimplemented!` invocations | Rust | Comments/strings are not invocations; macro shadowing and deliberate scaffolds need judgment |
| Undocumented empty catch | Empty catch AST body without comments | TS/TSX | An explanatory comment or executable handler is not an empty-catch match |
| Failure masking | `Err` match/if-let branch constructs `Ok`; catch ends with `return true` | Rust, TS/TSX | Error propagation, ordinary successful branches and explicit `std::io::ErrorKind::NotFound`-guarded `Ok(None)` absence contracts are excluded; other intentional fallbacks may be justified by their contract |
| Duplicated behavior | Nonempty if/else bodies have identical syntax tokens, with literal content preserved | Rust, TS/TSX | Comments/formatting do not change comparison; Rust let/let-chain name bindings are excluded; condition effects must be preserved |
| Unnecessary machinery | At least two same-module private forwarding layers, unchanged arguments and identical first-layer parameter/return contracts | Rust | Excludes single naming wrappers, public/attributed/generic/receiver boundaries, transformed arguments and direct cycles; architectural naming intent still needs judgment |
| Manageability | Joint long-body/control-flow concentration, new or worsened relative to the recorded baseline | All nine modes | Unique named functions only; standalone large length/parameter/complexity values do not produce a finding |
| Async/resource safety | Known synchronous std/parking_lot guard remains live across same-block await; async executor passed to unshadowed standard Promise | Rust, TS/TSX | Explicit drop/completed inner scopes and Tokio mutexes are excluded. Rust binding recognition is limited to qualified constructors/simple parameter types; indirect aliases, complex paths, possible ownership transfers and type inference are not guessed. Shadowing does not incorrectly count as releasing the old guard |
| Weak verification | Empty test without should_panic; test asserts literal true or compares an identical simple expression with itself | Rust | Does not classify every assertion-free test as weak. Result-returning tests and framework behavior can verify real contracts; side-effecting self-calls are excluded |
| Unsafe shortcuts | Selected qualified unsafe memory operation without local SAFETY argument; blanket warnings/clippy::all suppression without local reason | Rust | Narrow lint exceptions and locally explained operations are excluded; absence of a safety argument is not proof of undefined behavior |

Unnecessary-machinery, duplication and manageability matches are Low-severity
review concerns, not declarations that an abstraction is forbidden or a bug
exists. Other source-pattern matches are Medium review findings. Every result
states the observed construct and its potential contract consequence so a
reviewer can accept/reject the concern with relevant context. No finding invents
a governing criterion or marks an acceptance criterion unmet.

The Rust constructors/macros in these patterns are source-syntax observations;
unusual local shadowing or generated code can require contextual rejection of a
match. In particular, this is not proof of a compiled `Result` type, runtime
reachability, external API compatibility, executed tests or complete cleanup.

## Joint manageability policy

`atomic.manageability.v1` is explicit Atomic review policy, not a universal
scientific cutoff or a metric-quality pass/fail gate. It requires:

- at least **100 code-bearing function-body lines**, **and**
- cyclomatic complexity at least **15** or control-flow nesting at least **4**.

Both use the existing `atomic.metrics.syntax.v1` counting implementation, with
comments and nested-function bodies excluded according to that convention.
Long straight-line tables alone and small branch-heavy dispatchers alone do not
trigger it. For an already-triggering named function, a new finding requires at
least **three additional decision points or a new nesting level**, not merely
one more line, formatting, straight-line growth or an unchanged count. This is
also Atomic policy, not a calibrated defect predictor. New/worsened joint burden
reports the actual after factors and, when present, before factors. An unavailable
baseline yields Unknown introduction, never an invented zero baseline.

An isolated metric value, metric count or risk-rule trigger still does not create
a defect finding. This audit's finding is the specific combination of long body
and branching/nesting in an identified recorded function, qualified as increased
review/maintenance burden rather than proof of missing tests or runtime failure.

## Scope, attribution and bounds

The adapter inspects selected recorded content and its actual before snapshot.
Later/dirty source is not used. Ordinary matches use exact rule/source-text
multisets: line shifts alone are preexisting; additional identical occurrences
can be new; an edited existing same-rule match has Unknown introduction rather
than a fabricated regression. Manageability additionally compares the named
subject's joint evidence as described above.

The existing 1 MiB source limit and 100-issue limit remain. AST inventory is capped
at 300,000 nodes / nesting depth 256 and substantive subtree work is capped at 32 MiB of inspected
source spans per file. Budget exhaustion produces an explicit incomplete check,
not a truncated clean result. A syntax-recovered tree runs syntax diagnostics
only, never substantive checks on unreliable nodes.

Tests include positive evidence for every substantive family, nearby negative
cases, attribution of new/preexisting/unknown concerns, scope-pinned recorded
inspection and a real CLI invocation producing all seven families without a
supplied findings file. Those fixtures validate detector execution; they do not
claim exhaustive detection of every problem in each audit category.
