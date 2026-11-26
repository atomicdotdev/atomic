# Atomic change-review risk, v1

`atomic.change-risk.v1` is an explainable **review-priority estimate**, not a
defect probability, confirmed-bug count, outage prediction or approval. A change
can deserve High risk review while all observed call contracts remain compatible.

## Research and what it supports

The literature supports using change characteristics and dependencies to focus
review effort. It does **not** provide universal caller-count or size cutoffs:

1. Kamei et al., *A Large-Scale Empirical Study of Just-in-Time Quality Assurance*,
   IEEE TSE 39(6), 2013, DOI [10.1109/TSE.2012.70](https://doi.org/10.1109/TSE.2012.70).
   [Author institution abstract](https://kyushu-u.elsevierpure.com/en/publications/a-large-scale-empirical-study-of-just-in-time-quality-assurance).
   Change-level features can prioritize quality assurance. Atomic does not import
   that trained model, its accuracy, developer-experience judgments or coefficients.
2. Zimmermann and Nagappan, *Predicting Defects using Network Analysis on Dependency
   Graphs*, ICSE 2008, DOI [10.1145/1368088.1368161](https://doi.org/10.1145/1368088.1368161).
   [Microsoft Research publication](https://www.microsoft.com/en-us/research/publication/predicting-defects-using-network-analysis-on-dependency-graphs/).
   Dependency structure can reveal important program units. Simple observed caller
   counts are not equivalent to the paper's network model or validated predictions.
3. Google's [Small CLs](https://google.github.io/eng-practices/review/developer/small-cls.html)
   and [What to look for](https://google.github.io/eng-practices/review/reviewer/looking-for.html)
   explain why broad changes are harder to review/test and emphasize functionality,
   concurrency, complexity, integration and useful tests. Google explicitly rejects
   a universal line-count definition of a small change.
4. [NIST SP 800-30 Rev. 1](https://csrc.nist.gov/pubs/sp/800/30/r1/final)
   separates risk factors, impact, likelihood and uncertainty. Atomic borrows the
   principle of exposing uncertainty separately; this is not a NIST security-risk
   assessment, calibrated likelihood estimate or claim of standards conformance.
5. Shahini et al., [*On the calibration of Just-in-time Defect Prediction*](https://arxiv.org/abs/2504.12051),
   2025 preprint, reports calibration limitations even for learned models. Atomic
   therefore reports no invented percentage probability or numeric certainty.

**The following thresholds are Atomic-owned starter policy**, not scientifically
established constants. Teams should calibrate them against their changes and
observed outcomes. Changing the counting/role conventions requires a model-version
change; an explicit profile records numeric threshold choices.

## Default `balanced` rules

Rules take the maximum triggered band; correlated signals are not added into an
opaque score. Each result retains the full profile, measured facts, stable rule ID,
reason and bounded recorded references in JSON/graph.

| Rule | Band | Default trigger |
|---|---|---|
| `risk.broad-production-change` | High | At least 50 production functions, 5 changed source files, 2 component groups |
| `risk.cross-component-api-churn` | High | At least 20 production functions, 3 component groups, 10 declared public units added/removed |
| `risk.existing-consumer-exposure` | High | At least 25 observed production consumers in 5 files of modified/removed existing declarations |
| `risk.change-diffusion` | Medium | At least 10 production functions, 5 changed source files, or 2 component groups |
| `risk.public-contract-churn` | Medium | Declared public units added/removed, or an existing production signature changed |
| `risk.consumer-review` | Medium | At least 5 observed production consumers of existing declarations, or an observed consumer outside selected source files |
| `risk.dependency-growth` | Medium | Existing-file declared dependency fan-out increases alongside production-function changes |
| `risk.module-contract-change` | Medium | Production imports/types/module-level source changes beyond function-body checks |
| `risk.structural-alerts` | Medium | Introduced/uncertain automated syntax, placeholder or empty-catch alerts requiring judgment |
| `risk.localized-change` | Low | No elevated trigger and sufficient source-selection, reference and surface evidence for localized/test-support scope |

Unknown is reserved for insufficient facts to apply an elevated rule or support a
localized estimate. Missing evidence is not zero. **Partial coverage does not erase
positive High/Medium triggers.** Conversely, a large number of unresolved names is
not measured fan-out and never raises severity by itself.

Confirmed/reported risk findings impose a severity floor independently of the
estimate. Profiles or reviewer input cannot downgrade that floor. Rule triggers
do not create F-number findings or acceptance-criterion failures. A reviewer can
override an estimate with scoped evidence; the automatic estimate remains visible.

## Counting conventions and confidence

- Functions and consumers are deduplicated by semantic identity, not call-site
  count. New-helper consumers are distinguished from consumers of changed existing
  declarations. File scope includes selected paths even when their functions did
  not change, preventing a false “outside change” classification.
- Test/support classification uses conventional test/fixture/example/bench paths,
  test filenames and inline `tests::`/`test::` scopes. It is not proof of runtime
  deployment, executed tests, behavioral coverage or test adequacy. Such functions
  and consumers are reported separately rather than inflating production exposure.
- Component groups use conventional repository roots (`crates/<name>`,
  `packages/<name>`, `services/<name>`, `apps/<name>`; otherwise a top-level project
  directory, with a single `src`/`lib` root grouped together). They are not inferred
  compiler/runtime service boundaries.
- Public units and fan-out use the existing versioned metric convention. Additions
  of a new file are not numeric before=0 deltas. Unknown baselines/conventions stay
  unavailable. Public units are source declarations, not a compiled ABI; dependency
  roots are not necessarily new third-party packages or vulnerabilities.
  Aggregate growth counters tally only comparable observations and are lower
  bounds when measurements are incomplete; a zero tally is not a fabricated
  before-value or a claim that unmeasured API growth is zero. The separate
  completeness/unknown fields prevent such a tally from establishing Low risk.
- Confidence describes input sufficiency for the rule estimate, not a calibrated
  probability of defects. Unresolved references, missing metric facts or incomplete
  source selection reduce it. Static estimates retain Partial overall coverage;
  complete structural inputs do not establish runtime/security/test correctness.

For captured change `FQNWHCPNXXO5`, the measured summary contains 76 changed functions
(53 production, 23 test/support), 53 current bound consumers (31 production, 22
test/support), four affected component groups, +19 declared public units in existing
files and 24 in added files. The default broad-change/API-integration rules yield
**High review risk**, even with zero detected contract violations. Unresolved
expressions remain outside those caller counts and lower confidence.

## Profiles and reproduction

```sh
atomic review --change <hash> --risk-profile review-risk.json
```

```json
{
  "name": "team-calibration",
  "broad_functions": 80,
  "api_growth": 20,
  "propagation_callers": 40
}
```

Omitted settings use `balanced` defaults. Profiles reject unknown fields, empty
names, zero thresholds and contradictory high/moderate bands. Their expanded
values and model version are retained in `graph.risk.model`. They affect local
inspection only, not insertion/push, signed publication policy or authorization.
No online research, model training, build scripts, runtime tests or fixes run as
part of this deterministic assessment.
