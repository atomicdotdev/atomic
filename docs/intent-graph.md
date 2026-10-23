# Intent semantic change graphs

```sh
atomic intent graph ATOM::continuouslee::191
atomic intent graph ATOM::continuouslee::191 --json
atomic intent graph ATOM::continuouslee::191 --dot -o intent.dot
atomic intent graph ATOM::continuouslee::191 --html
atomic intent graph ATOM::continuouslee::191 --html -o intent.html
atomic intent graph ATOM::continuouslee::191 --html --no-open -o intent.html
```

Human keys, intent UIDs, and canonical project-qualified intent IRIs are accepted.
Text is the default. `--html` saves the graph and opens it in the default browser:
without `--output`/`-o`, it uses a unique temporary `.html` file retained after the
command exits; with `-o`, it opens that output file. If browser launch fails, the
saved graph remains available and a warning gives its file URL.

Use `--html --no-open` for scripts and headless environments: HTML writes to stdout
unless `-o` is supplied, in which case it only writes the file. HTML uses the
existing D3 renderer and its CDN dependency.

### Inspecting nodes

Known semantic node types have distinct, stable colors in both the graph and its
legend; newly encountered external types use a deterministic color derived from
their full type IRI. Colors stay consistent between exports, and text labels
remain available so identification does not depend on color alone.

Click a node to pin its details in the right-hand pane. The selected node and
its adjacent edges are highlighted; hovering still shows the lightweight tooltip.
The pane displays the full label, canonical ID, all types, source, untruncated
summary, structured metadata, incoming/outgoing relationships and raw exported
node data. Click a related node in the pane to navigate to it.

The node picker also supports keyboard selection. Use **Clear**, **Escape**, or
click empty graph space to reset selection. Dragging a node does not change the
selection. On narrow screens the inspector moves below the graph.

Inspector data is embedded in the HTML and needs no running Atomic service.
It reflects only the exported snapshot: file contents and historical patch bodies
are not fetched. The shared query-graph HTML renderer includes the same inspector.

### AI usage, costs and attribution

Selecting a **change, intent or view** shows a dedicated AI usage section with
input, output, reasoning, cache-read, cache-write and recorded-total tokens,
precise USD costs, and coverage. Changes and intents also show their contributing
models, tools and vendors; mixed attribution is listed separately rather than
reduced to a single model. The same summary is exported as `metadata.ai_usage` in
graph JSON.
The summaries are also included by
`atomic query neighbors <canonical-node-IRI> --json`, so the same usage data can be
queried without opening HTML.

- **Change:** its immutable recorded AI provenance.
- **Intent:** the union of explicitly implementing changes and changes recording
  its own intent artifact, with each change counted once. Dependency-only history
  is excluded. Recording evidence remains distinct from implementation attribution.
- **View:** authoritative repository membership, including ancestor-view membership
  and validated transitive change dependencies, deduplicated by change identity.
  This does not depend on derived KG membership edges being up to date. An
  incomplete/unresolvable view filter is reported as incomplete, not a complete
  zero-cost view.

Summaries use the complete repository snapshot **before** display limits/filtering;
a view's total does not shrink just because only one of its changes is visible in
an intent graph. Do not add overlapping intent/view totals to calculate a repository
total. Shared, identifiable provider requests or session-turn usage records are
counted once; conflicting copies are excluded and reported. Unavailable changes and
changes without AI provenance are counted in coverage, not assumed to be free.

These are recorded AI costs, not development-cost estimates. Token categories may
overlap according to the provider, so the recorded total is preserved rather than
reconstructed by adding the rows. Older captures may store zero for fields that
were not reported; the inspector discloses that limitation. All-zero token records
and unmarked zero costs are treated as unreported, rather than assumed free.
Prompts and reasoning
text are not included in usage metadata.

Older KG projections can read usage from verified immutable change headers without
writing a backfill or rebuilding the repository. Newly projected changes store the
normalized usage facts directly. Regenerate HTML exports to include these fields;
already saved HTML files do not update themselves.

Browser interaction regressions can be run explicitly with Chrome/Chromium:

```sh
ATOMIC_TEST_CHROME=/path/to/chrome \
  cargo test -p atomic-cli --test intent_graph_test -- --ignored
```

Set `ATOMIC_TEST_D3=/path/to/d3.min.js` to use a local D3 v7 bundle for the canvas
test instead of its CDN URL. Normal tests do not launch a browser.

The graph selects **asserted semantic lineage**, not a search neighborhood:

- the intent, its tasks, acceptance criteria, scope, constraints, and references;
- turns governed by the intent and directly implementing changes;
- changes recording the selected intent's exact vault artifact, separately
  classified as recording evidence rather than implementation attribution;
- transitive **direct change-dependency** closure, separately classified from implementation;
- recorded affected files, authors, direct view memberships and parent-view links;
- generating turns/sessions, provenance, covering attestations, signer and verification links;
- related review/memory/goal references and gap findings.

Shared sessions, files, authors and attestations do not pull unrelated changes into
the graph. View ancestry does not invent inherited direct membership. Related
intents are boundary nodes: their implementations are not silently counted as this
intent's implementation.

JSON includes full canonical IRIs, asserted types/predicates, snapshot identity,
ontology version, implementing/dependency-only change sets, missing resources and
diagnostics. `--max-nodes` defaults to 5000, with a hard maximum of 10000. A cap
always reports `truncated: true`; it never claims complete dependency closure.
No linked changes means “none linked in this snapshot,” not “no work happened.”

## Recorded change evidence without an implementation assertion

A full change record may already be in the KG even when there is no explicit
`implements` or governing-plan relationship. The default graph now also joins
the intent's **stored `vault_path` metadata** to its exact canonical repository
File node (`.vault/<vault_path>`) and follows existing incoming `modifiesFile`
edges. It shows those changes, their recorded affected files, dependencies and
available generating-turn/provenance relationships. It does not read working
files, rebuild the KG, or require an agent session binding to expose this evidence.

JSON reports the matched `intent_artifact` and its `recording_changes` separately
from `implementing_changes`, `captured_changes` and `declared_changes`. Text, DOT
and HTML label them “records intent artifact; implementation attribution not
asserted.” A change can belong to both the recording and implementation sets
when both kinds of evidence exist. Dependency-only changes exclude either kind
of primary selected change.

This is a join over existing snapshot facts, **not a new semantic edge or a
retrospective declaration**. Recording an intent can be implementation work,
documentation, or administration; it does not by itself prove implementation or
governing-turn provenance. Shared task/source files, authors, sessions and
attestations never seed additional changes. Historical renames whose paths are
not represented by the current snapshot are not reconstructed.

When recording evidence exists but implementation attribution does not, the
diagnostic says so explicitly instead of presenting only an empty-implementation
warning. An absent `plan_id` remains visible as a provenance gap; it does not
hide the full recorded changes that the snapshot actually contains.

This command reads through the shared local/reactor services and **does not rebuild**
the KG. If an intent is not projected, use `atomic query enrich` explicitly. The
scope is repository-wide and is not available as a remote sandbox query.

“Full” means the predicate-scoped lineage present in the asserted snapshot. It
does not mean inferred source behavior, token-level code graphs, a complete
historical diff, or proof that all historical work has been ingested. For exact
recorded operations use the `patch_evidence` query-plan step. A change's
availability/projection fields remain separate from provenance/attestation coverage.

## Capture governance when work starts

Recording an intent file alongside code does **not** bind that intent to the
generating turn. Declare the association while the hook turn is active:

```sh
atomic intent update ATOM::continuouslee::194 --status in_progress \
  --session-id <active-session-id> --turn <1-indexed-hook-turn>
```

The CLI resolves the human key to the stored intent UID and persists a
turn-scoped declaration before the status update returns. At checkpoint time,
that UID becomes `plan_id` in both the provenance graph and `SessionTurn`, so the
semantic graph can project the captured association. This works for ordinary,
unmanaged sessions. Managed runs without an explicit declaration retain their
existing work-item capture.

Integrations can inject `ATOMIC_SESSION_ID` into each session's shell environment
instead of passing `--session-id`. `--turn` is optional but recommended to reject
delayed commands. The integration must complete `session-start` and `user-prompt`
hooks **before** executing the work-start command, and must not use a process-wide
session variable shared by concurrent sessions. Without a flag or environment
context, an update remains a human/status-only operation; Atomic never chooses a
session by recency or by scanning active sessions. The separately installed
`atomic-opencode` integration needs to supply this context; changing the CLI does
not update installed plugin code.

Declarations are stored at
`.atomic/sessions/<session-id>/intent-<hook-turn>.json`, independently of mutable
session JSON. They share the recorder's `turn-end.lock`, survive hook process
restarts, are idempotent for the same UID, and reject replacement, stale/completed
turns, recording contention, and conflicting managed work items. Governance is
frozen before the first recording attempt, even if that attempt fails before
checkpoint publication; retry cannot introduce a late declaration. They do not
carry into the next turn. Re-declare governance explicitly on later turns that
continue the same intent. This is a prospective work-start declaration, not a
repair tool for an already recorded turn.

## Repairing an implementation association without inventing provenance

A failed recording hook can leave implementation in a mixed parent change with no
captured intent ID. Do not change that historical provenance or guess a generating
turn from shared files. An author can instead make an explicit declaration in the
intent body, citing the **full canonical change IRI** and nonempty evidence:

```text
::ref{to=urn:atomic:change:blake3-256:<FULL_HASH> edge=declaresImplementation declarationEvidence="Explain the explicit attribution, its evidence, and the historical gap."}
```

Then run `atomic vault sync` and `atomic intent attest <intent-id>`; verify the
fresh attestation with `atomic intent verify <intent-id>`. This is a new,
retrospective author claim, not a claim captured at the original turn. The
declaration and evidence participate in the canonical signature. Unsigned
authoring data can still be projected; verify the signed intent before trusting it.

The graph emits `atomic:declaresImplementation` and lists the change under
`declared_changes`. `implementing_changes` is the union of declared and captured
implementation; `captured_changes` preserves the captured subset. Declarations
never synthesize `atomic:implements`, `hasPlan`, or `generatedChange` edges, satisfy
missing-provenance publication gates, or turn into dependency edges. Missing change
objects produce unresolved-reference findings rather than fabricated targets. Old
signed snapshots do not resurrect declarations removed from the current source.

Automatic hook retry/intent capture is separate from this explicit repair path.
