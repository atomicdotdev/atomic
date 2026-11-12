# Native workflow harnesses

Run the three native workflows without Git, shim enrollment or remotes:

```sh
ATOMIC_BIN=/absolute/path/to/atomic bash tests/harness/run_all.sh native-workflows
```

The selector runs **40 → 32 → 23**. Individual numeric selectors still work.
Python-driven fixtures use a disposable HOME/config/identity store and local
service calls, with remote service overrides removed. Every assertion is
reported to the shell harness; a driver crash or zero assertions cannot pass.

## 40 — switch transactionality

The native switch API attempts recovery synchronously when a switch returns
an error. This replaces the old test's expectation that a failed command must
leave `.atomic/deferred-tree-alignment.pending` and partial target files.

Both returned-error seams (`AFTER_PUBLISH` and `AFTER_MATERIALIZE`) must actually
fire. Before any writable retry the harness inspects the failed switch and its
verified inverse recovery head, parent link, leases and receipts. It checks
exact tracked/untracked bytes and modes, both views' ignored artifacts/shelves,
unchanged histories, clean tracked status, idempotent writable reopen, and a
successful ordinary retry and round trip. These are **returned-error rollback
tests, not process-kill or power-loss coverage**.

## 32 — signed review-readiness and insertion of originals

The active test is native-only. It creates two isolated fixture identities,
an attested work intent, two dependent code records, a negative self-review,
and an independently signed completed review. The triage report must change
from blocked to ready before the workflow inserts anything.

Preview must preserve both histories, working bytes and operation head. Explicit
cross-view insertion must retain the original change hashes, dependency and
immutable object bytes, with no surrogate/squash record. Repeated insertion is
membership-idempotent; the Shared view must render the exact result and retain
both originals and the verifiable review after the Draft is deleted.

This tests **the review-then-insert workflow**, not a claim that every low-level
insert API enforces triage automatically. The two fixture signing identities
exercise identity independence; this is not an independent review of Atomic's
implementation. The former Git squash/trailer proposal is preserved under
`legacy-shadow/32_review_gate_insert.sh`; it is not run or claimed passing here.
Native tests do not relax the rejection of fabricated Git provenance.

The fixture uncovered and regression-tests a real service-path bug:
`log --view <target> --all` dropped the explicit view and read the current Draft.
The service now honors the explicit view while retaining the selected log's
inherited-entry option. The local-service regression covers explicit Shared
and seeded Draft histories, with and without inherited entries, plus current
view selection.

## 23 — hierarchy and Shared-root boundaries

The historical filename remains for compatibility. The current native creation
contract makes Draft children; promotion makes a Shared **root**, not a Shared
child. There is no skipped assertion for an impossible CLI setup.

The harness checks multi-level Draft inheritance and isolation, promotion of
explicit baseline membership without changing original identities, isolation
from later changes on the former parent, live inheritance by a child of the
new Shared root, clean materialization and non-mutating missing-parent refusal.
It does **not** claim to exercise a Shared-child hierarchy from the older model.

## Validation

The native default-feature debug build passed the focused run with no skips:

| Suite | Passed | Failed | Skipped |
|---|---:|---:|---:|
| 40 native switch transactionality | 53 | 0 | 0 |
| 32 native review and original insertion | 28 | 0 | 0 |
| 23 native hierarchy | 89 | 0 | 0 |
| 17 cross-view merge (nonregression) | 208 | 0 | 0 |
| 22 switch/conflict markers (nonregression) | 179 | 0 | 0 |
| 28 merge rubric (nonregression) | 207 | 0 | 0 |

All 15 `libatomic` local-service integration tests, workspace `cargo check` and
workspace Clippy also passed. The native/shim selector selftest passed eight
checks; new Python and shell files passed syntax checks. No full-suite,
power-loss, server or network validation is claimed by this focused run.
