# ADR 0001 — Custom vault kinds: projection, SHACL, and view semantics

- Status: accepted for the `feat/kind-kg-conformance` branch
- Supersedes: PR #278 decisions 1, 2, 3, 9, 11, 12 where they conflict
- Related: `docs/knowledge-graph-closure-rfc.md`, `docs/knowledge-graph-closure-phase-plan.md`

## Context

PR #278 adds repository-defined vault kinds. The proposal defaults to a
hand-written validator, a single `atomic:references` predicate for every custom
relation, and a "last deflate wins" hub. That conflicts with the closure plan's
rule that the knowledge graph is the validation surface and that no custom
constraint code is compiled into the binary.

This ADR fixes the projection contract so custom kinds are first-class in the
KG and in SHACL, and records the view semantics that the implementation must
honour.

## Decision 1 — Extension namespace and projection

Custom kinds project into the **project-scoped extension namespace**, not into
the closed workflow ontology:

```
kind class      urn:atomic:project:<pid>:kind:<kind>
kind hub        urn:atomic:project:<pid>:concept:kind.<kind>
kind instance   urn:atomic:project:<pid>:concept:<kind>.<uid>
field predicate urn:atomic:project:<pid>:kind:<kind>#<field>
body predicate  urn:atomic:project:<pid>:kind:<kind>#body
uid predicate   urn:atomic:project:<pid>:kind:<kind>#uid
relation pred.  urn:atomic:project:<pid>:kind:<kind>#<edge>
```

`<pid>` is the repository's project identity. Extension IRIs are the only
non-`Class` IRIs the adapter accepts; every other asserted class must still be
a `Class` in `atomic-core::kg::ontology`.

A kind instance carries:

- `rdf:type urn:atomic:project:<pid>:kind:<kind>`
- `rdf:type atomic:Concept` (the closed-vocabulary anchor, unchanged)
- one triple per declared field value, using the field predicate
- one `#body` triple when the body is non-blank
- one `#uid` triple
- `atomic:references` to the hub (`concept:kind.<kind>`)
- one `atomic:references` edge per resolved ref (graph traversal), plus one
  `<kind>#<edge>` triple per resolved ref (RDF/SHACL)

The hub carries `rdf:type atomic:Concept` and `rdf:type
urn:atomic:project:<pid>:kind:<kind>`? No — the hub is a definition, not an
instance. It stays `atomic:Concept` only. The kind class is asserted on
instances.

## Decision 2 — SHACL is the normative validator

`kind validate` runs SHACL. The generated shape is derived from the recorded
`_kind.md` spec; nothing is hand-authored per kind.

- `atomic-shacl` gains `Profile::Kind`.
- `atomic-shacl::kind_shapes::shape_for(spec, project)` emits one Turtle
  `NodeShape` per kind.
- The gate builds a dataset from the selected view's kind rows (the same rows
  `KindFiles` reads), runs `validate_ntriples`, and reports non-conformance.
- `validate_instance` stays as the write-time pre-check for `kind new` and as
  the test oracle. It is not the authority for `kind validate`.
- A kind whose spec cannot be represented as a shape is rejected at definition
  time (fail closed), not silently downgraded.

### Shape contract

| Spec construct | SHACL |
|---|---|
| `uid` | `sh:minCount 1`, `sh:pattern` ULID, SPARQL suffix check |
| scalar field | `sh:maxCount 1`, `sh:datatype` |
| list field | no `sh:maxCount`, `sh:datatype` |
| `enum` | `sh:in` |
| `required` | `sh:minCount 1` |
| `ref` | `sh:nodeKind sh:IRI`, `sh:class` per target kind |
| `body.required` | `sh:minCount 1` on `#body` |
| conflict markers | SHACL-SPARQL `FILTER (CONTAINS(?body, "<<<<<<<"))` |
| unknown field | `sh:closed true`, `sh:ignoredProperties (rdf:type)` |
| `default` | not emitted; defaults are write-time only |

The five constructs PR #278 found RDF-invisible are closed as follows:
cardinality (`sh:maxCount`), `uid == filename` (SPARQL), ULID validity
(`sh:pattern`), conflict markers (SPARQL), and enum-on-non-string
(`sh:datatype xsd:string` + `sh:in`).

## Decision 3 — View semantics

- The **gate** validates the selected view's tracked kind rows. A ref resolves
  only when its target is visible in the same view's effective filter. This is
  the view-closure rule the intent graph already uses.
- The **ambient KG** is repository-wide (pre-existing behaviour for all vault
  rows). Kind instances from every view are present. Queries filter by view
  where the query API supports it.
- The **hub** is owned by the definition path. Its metadata is deterministic
  given the stored definition row. A definition that is not visible in the
  selected view does not make the hub disappear; the gate reports the
  definition as missing for that view.
- "Last deflate wins" is removed as a *semantic* claim: the hub is the
  definition row's projection. Rebuilding from the same stored rows yields the
  same hub. A definition change on a sibling view updates the shared row, and
  the gate reports the conflict for the view that lost the definition.

## Decision 4 — Refs and patch dependencies

A `ref` is a semantic edge, not a patch dependency. `insert` does not carry
ref targets. Instead:

- `kind validate --view` fails closed when a ref target is not in the view's
  filter, with `UNRESOLVED_REF`.
- The KG projects the dangling ref as a `KindViolation` Finding.
- `kind new --record` (the default for `kind new`) records the instance so the
  KG and the view see it immediately. `--no-record` keeps the current
  write-only behaviour for callers that want to stage a definition first.

M3's `extra_dependencies` frontier remains the long-term fix; it is out of
scope for this branch and tracked in the ADR's follow-ups.

## Decision 5 — Wire parity

Kind commands are CLI-local in this branch. The reason is scope: the daemon
service contract needs a new proto arm and a server handler. The branch adds
the `kind` verb to the read-only `query validate` path so a server-side caller
can validate kinds through the existing knowledge service. A dedicated `kind`
RPC is a follow-up.

## Decision 6 — Performance

Whole-vault re-projection stays as dev's behaviour. The branch does not add a
new projection pass. The measured cost is recorded as a follow-up; a targeted
per-path projection is the recommended fix and is not implemented here.

## Consequences

- Custom kinds are queryable and validatable in RDF without new workflow
  ontology terms.
- `kind validate` and `query validate --profile kind` share one validator.
- The hand-written validator cannot drift from SHACL silently: the mapping
  tests pin the five closed constructs and the gate runs SHACL.
- Extension IRIs are project-scoped; two repositories cannot collide.
- The ambient KG remains repository-wide for vault rows. This is a known
  limitation of dev's vault projection, not a kind-specific one.
