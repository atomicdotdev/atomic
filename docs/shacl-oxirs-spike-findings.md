# SHACL engine spike — findings (oxirs-shacl 0.3.1)

> **STATUS (2026-07-06): the `atomic-shacl` spike crate has been removed.**
> Decision (Lee, owner): SHACL validation runs as a **Circuit Breaker (CB)
> workflow**, not built into `atomic`. So no SHACL engine ships in the atomic
> binary. This document is retained as the record of *why* oxirs (and, more
> broadly, an in-atomic engine) was rejected: the ~432-crate footprint, the
> silently-dropped `sh:node`/`sh:sparql`, and the pre-1.0 maturity. atomic keeps
> the fast hand-coded tier-1 gate, emits canonical JSON-LD, and ships the shape
> `.ttl` as data for CB to validate against.

**Question:** can a real SHACL engine (`oxirs-shacl`) over real Turtle shapes
reproduce the hand-coded gate (`atomic-canonical/src/gate.rs`), so intent/memory
policy becomes team-editable data?

**Verdict: No — not with oxirs-shacl 0.3.1.** It silently fails to enforce 3 of
the 12 rules (a signing-path false-negative class), loses property-path detail on
the rest, and costs a ~430-crate dependency tree. The shapes and the JSON-LD→RDF
shim are correct standard SHACL and are retained; only the *engine* is unfit.

Everything below is reproduced by executable tests:
`cargo test -p atomic-shacl --features oxirs-engine` (13 tests, all green).

## What works
`sh:minCount` (presence) and `sh:in` (closed value sets) fire correctly. oxirs
matches the gate's `conforms` verdict on every Core-rule fixture, **including**:
- the **trim asymmetry** — whitespace-only `why`/`text` rejected, whitespace-only
  `attributedTo`/`verifiedBy`/`evidence` accepted (reproduced by the shim, not the
  shapes; `src/shim.rs::should_emit`);
- present-empty vs omitted string fields (both project to nothing);
- the load-bearing **directionality ABSENCE** (a `superseded` memory with no
  forward edge conforms — no forward-edge rule was authored).

## Confirmed blockers (silent false-negatives)
oxirs 0.3.1 reports `conforms: true` where the gate correctly rejects:

| Rule | SHACL construct | Observed |
|------|-----------------|----------|
| AC closed-set (rule 6) | `sh:node` (nested AcceptanceCriterionShape) | **not enforced** — unknown `acStatus` passes |
| INTENT-5 (scope-in ⇒ scope-out) | `sh:sparql` | **not enforced** — passes |
| INTENT-7 (met AC ⇒ verifiedBy+evidence) | `sh:sparql` | **not enforced** — passes |

`strict_mode(true)` did **not** error on these unsupported constructs — they are
dropped silently (fail-open, the worst posture for a signing gate).

## Fidelity gap (even on working rules)
`ValidationViolation.result_path` is always `None`, so a violation cannot be
mapped to the property that failed, and multiple distinct violations on one node
collapse (a node missing both `attributedTo` and `proof` yields two gate
violations but one SHACL finding).

## Cost / maturity
- `--features oxirs-engine` pulls **~432 crates**, including `scirs2-*` /
  `ndarray` / `oxiblas-ndarray` (BLAS), the full `icu_*` suite, and
  `reqwest`/`hyper-rustls`/`rustls` (HTTP + TLS).
- oxirs-shacl's own README: "API is still under development and subject to
  change"; core validation example commented out.

## The blocker tests are tripwires
`tests/differential.rs` asserts these defects **as passing tests today**. If a
future oxirs release fixes `sh:node`/`sh:sparql`/`result_path`, the corresponding
assertion flips and the test FAILS — the signal to re-evaluate promotion.

## Isolation (verified)
- `atomic-canonical` never depends on this crate; the oxirs stack does not reach
  it (`cargo tree -p atomic-canonical` is clean).
- oxirs is behind the **off-by-default** `oxirs-engine` feature: routine
  workspace builds/CI do not compile it.
- Nothing here is on the signing path. The hand-coded gate remains the sole
  authority. Removing this crate is a no-op for `atomic`'s behavior.

## Options from here
1. **Pivot to an in-house evaluator over these same shapes** (parse with `oxttl`,
   ~20 mature crates; own the `sh:node`/`sh:sparql`/conditional evaluation). Gets
   policy-as-Turtle-data at ~5% of the dependency cost, with correctness we own
   and prove against the gate via this same differential corpus.
2. **Shelve oxirs**: keep the gate authoritative, keep this crate as a documented
   negative result + tripwires, revisit if oxirs matures.
3. Keep digging on oxirs (unlikely to pay off given the maturity signal).
