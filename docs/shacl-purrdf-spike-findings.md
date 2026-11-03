# PurRDF SHACL engine spike — findings (`purrdf-shapes` 3.0.1)

> Status: **initial spike passed; provisional Phase 1 engine choice.**
> Reproducer: [`tools/shacl-purrdf-spike`](../tools/shacl-purrdf-spike/README.md).
> No engine has been added to Atomic's workspace or publication path.

## Question

Can an existing Rust SHACL engine enforce enterprise-authored Turtle shapes,
including nested constraints and local SPARQL conditions, without an Atomic-owned
constraint evaluator?

## Result

**Yes for the tested cases.** The published crate compiled without modification on
Rust 1.98.0 (aarch64 macOS). All eleven tests passed, with positive and negative
fixtures for the operational constraints. Initial debug build took about 26 seconds
(after dependency downloads); the final test runner reported about 0.04 seconds for
the eleven tests. These are smoke-test timings, not performance benchmarks.

| Probe | Observation |
|---|---|
| `sh:minCount` / `sh:maxCount` | Missing plan fails; exactly one passes; multiple plans fail |
| Nested `sh:node` + `sh:in` | Missing/invalid criterion status fails; valid status passes |
| `sh:sparql` conditional requirement | Open criterion passes; met criterion fails until both verifier and evidence exist |
| Inverse + sequence `sh:path` | Change → generating turn → intent path enforced; complex path structure retained in report |
| `sh:class`, `sh:datatype`, `sh:or` | Wrong type/datatype and unmet alternatives fail; corresponding positive cases pass |
| Violation report | Focus, path, source shape, source component, severity, and message available; standard RDF report serializes |
| Multiple missing properties | Two separate violations retain two distinct property paths |
| Malformed `sh:minCount "one"` | Shape loading returns an error |
| Unsupported SHACL-JS | Shape loading returns an error, including validation against an empty data graph |
| Malformed SPARQL query | Shape loading and validation return an error, even against an empty data graph |
| Valid shape with no applicable focus nodes | Empty graph conforms with no results; confirms the need for separate target-coverage reporting |
| Project-scoped subjects | Same local turn ID in two project namespaces stays distinct; violation identifies the correct project |

The nested constraint, SPARQL conditional, and result-path probes directly cover
categories that failed in the older OxiRS 0.3.1 spike. This is not a rerun of the
entire old differential corpus and makes no claim about newer OxiRS versions.

## Dependency and toolchain cost

- Direct dependency: `purrdf-shapes = "=3.0.1"`; the lockfile resolved its PurRDF
  family to 3.0.1 as well.
- **73 dependency packages** in the resolved metadata, excluding the spike itself.
  This includes target-specific dependencies; not all were compiled on macOS.
- Native Rust library API; this run needed neither a Python runtime nor a JVM.
- **Rust 1.98 required.** Atomic declares 1.90 and the default compiler on this
  machine was 1.97.1. We installed a separate 1.98.0 toolchain for the spike only.
- Before embedding PurRDF, approve an Atomic MSRV upgrade or keep validation in a
  separately built runner. Do not silently raise the main workspace requirement.

## Integration decision

Use PurRDF as the **provisional existing SHACL engine**, not as a model for writing
our own validator. Atomic owns RDF snapshot adaptation, shape bundle loading,
validation invocation, report presentation, and workflow-boundary integration.
PurRDF owns constraint parsing and evaluation, including SHACL-SPARQL.

`oxttl` / `oxrdf` can still be useful parser/model utilities elsewhere; they are
not SHACL engines and are not substitutes for PurRDF.

This changes the direction recorded in
[`shacl-oxirs-spike-findings.md`](./shacl-oxirs-spike-findings.md): SHACL is now the
primary workflow-policy mechanism under evaluation. The historical rejection of
OxiRS 0.3.1 remains a historical finding, not a ban on every embedded engine.

## Still required before production gates

1. Run relevant W3C fixtures and the full existing intent/memory gate regression
   corpus; compare results with an independent established SHACL implementation.
2. Exercise the actual Atomic RDF adapter, custom shape bundles, and CLI, not only
   synthetic N-Triples data. Preserve typed literals, paths, full IDs, and types.
3. Confirm all shapes in each supported profile load and evaluate, including
   shortcut agreement, coverage, and recorded verifier-result checks.
4. Define profile scope and target coverage. Valid shapes with zero applicable
   focus nodes can conform vacuously; that must not imply complete audit evidence.
5. Test malformed queries, unresolved imports, unsupported terms, and runtime
   failures: errors must never be reported as conformance. The spike proves three
   specific rejection cases, not universal fail-closed behavior.
6. Bound validation time/memory and shape import access. Test severity handling,
   deterministic report comparison, and repeated validation.
7. Resolve the MSRV/deployment choice. If validation runs externally, unavailable
   or failed validation must block any boundary configured to require it.

SHACL validates **recorded cryptographic verification results**; signature and hash
verification remain separate operations. This spike contains no signature verifier,
OWL inference, cross-project SPARQL endpoint, redb adapter, or publication hook.
