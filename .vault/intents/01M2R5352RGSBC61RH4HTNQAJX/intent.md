---
author: aaron
created_by: Aaron <aaron.ogle@atomic.dev>
doneSubstanceHash: blake3:55c98a265671ec4083b49cecfe6438d294aa29a06ac4499c96bd8d3874a46df9
goals: []
id: ATOM::aaron::1
priority: medium
project: ATOM
seq: 1
status: done
title: Hierarchical view list with empty-view filtering
uid: 01M2R5352RGSBC61RH4HTNQAJX
view: dev
entry_type: intent
content_hash: AC5UFAYXJGF65JZ4264NOTVLQ7NQS7QNPKPOPW4XJ7PAYOMLTO4Q
created_at: 2026-09-17T17:24:35.115086088+00:00
updated_at: 2026-09-17T17:24:35.115086088+00:00
---
:::why
Repositories accumulate many short-lived draft views (created by `view new`
and `view split`), and the flat alphabetical `atomic view list` hides the
containment structure between them while drowning active work in empty
views. Developers need to see the parent/child hierarchy at a glance —
dev → baby-bird-123 → jumbo-tron-444 — and focus on views that actually
hold changes, with `-a` as the escape hatch to see everything.
:::

:::acceptance-criterion{#01M2R5352RGSBC61RH4HTNQAJX-ac-1 status=met verifiedBy="cargo test -p atomic-cli + end-to-end CLI run against a real repo" evidence="tests test_hierarchy_layout_three_deep and test_all_flag_lists_every_view assert the exact tree lines; e2e run rendered dev → main → baby-bird-123 → jumbo-tron-444 → wren-77 with 2-space-per-level `- ` bullets and the `*` current marker"}
`atomic view list` (default and `--short` modes) renders views as an
indented hierarchy following parent chains: a view with a parent appears as
`- name` indented two spaces per depth level beneath its parent line, root
views at column 0, all sorted alphabetically (depth-first). With the chain
dev → baby-bird-123 → jumbo-tron-444, the listing reproduces:

```
dev
  - baby-bird-123
    - jumbo-tron-444
```

The current view keeps its `*` marker on its line (e.g. `* dev` or
`  - * baby-bird-123`).
:::

:::acceptance-criterion{#01M2R5352RGSBC61RH4HTNQAJX-ac-2 status=met verifiedBy="cargo test -p atomic-cli + e2e CLI run" evidence="test_hierarchy_layout_three_deep (empty leaf jumbo-tron-444 hidden), test_empty_sibling_draft_hidden, test_current_empty_view_stays_visible; e2e: after recording 1 change in baby-bird-123, empty jumbo-tron-444/wren-77 stayed hidden while baby-bird-123 appeared"}
Views with zero own changes are hidden from the listing unless (a) they have
a shown descendant or (b) they are the current view. "Own changes" means
`ViewInfo::own_change_count` — changes recorded into the view that are not
visible through the parent chain (for root views this equals
`change_count`). So an empty leaf draft (e.g. jumbo-tron-444 with no
recorded changes) is omitted while its ancestors stay visible to anchor the
hierarchy.
:::

:::acceptance-criterion{#01M2R5352RGSBC61RH4HTNQAJX-ac-3 status=met verifiedBy="cargo test -p atomic-cli + e2e CLI run" evidence="test_summary_line_counts asserts both singular and plural wording; e2e printed '3 views not shown because contained no changes. -a to view them.' and no summary line when nothing was hidden"}
When at least one view is hidden, the listing ends with a summary line in
the user's requested wording with the exact hidden count and singular/plural
handling, e.g. `2 views not shown because contained no changes. -a to view
them.` and `1 view not shown because contained no changes. -a to view them.`
No summary line is printed when nothing was hidden.
:::

:::acceptance-criterion{#01M2R5352RGSBC61RH4HTNQAJX-ac-4 status=met verifiedBy="cargo test -p atomic-cli + e2e CLI run" evidence="test_all_flag_lists_every_view (visible.len == entries.len, hidden 0, full tree rendered); e2e `atomic view list -a` showed all 5 views including the 3 empty ones with no summary line"}
A new `-a` / `--all` flag on `atomic view list` disables the empty-view
filter: every view is shown (hierarchy layout still applies, in both
default and `--short` modes) and no summary line is printed.
:::

:::acceptance-criterion{#01M2R5352RGSBC61RH4HTNQAJX-ac-5 status=met verifiedBy="cargo test -p atomic-cli + e2e CLI run" evidence="test_default_mode_line_keeps_metadata and test_root_line_format; e2e default mode still printed [shared]/[draft] tags, (n changes, m inherited), state short hash and parent info; existing integration tests (test_list_default_view, test_list_shows_current_marker, test_list_after_switch) pass unchanged"}
Visible lines in default mode keep the existing metadata (scope tag
`[shared]`/`[draft]`, change counts, state short hash, parent info) after
the tree prefix; `--short` still prints names only. Existing behaviors
(current-view marker, empty-repo hint) are preserved.
:::

:::acceptance-criterion{#01M2R5352RGSBC61RH4HTNQAJX-ac-6 status=met verifiedBy="cargo test -p atomic-cli; cargo clippy -p atomic-cli; cargo fmt" evidence="1863 tests passed, 0 failed (13 new tests incl. test_parent_cycle_terminates and test_missing_parent_renders_as_root); clippy clean on the touched crate; fmt applied"}
New unit/integration tests in the CLI crate cover: hierarchy layout for a
three-deep chain, empty-view hiding + summary count, `-a` showing all,
current empty view staying visible, and short-mode filtering. `cargo test
-p atomic` passes and the touched file is fmt/clippy clean.
:::

:::task{#01M2R5352RGSBC61RH4HTNQAJX-1 status=done criteria=01M2R5352RGSBC61RH4HTNQAJX-ac-1,01M2R5352RGSBC61RH4HTNQAJX-ac-2,01M2R5352RGSBC61RH4HTNQAJX-ac-3,01M2R5352RGSBC61RH4HTNQAJX-ac-4}
Add an `-a`/`--all` flag to the `List` command and rework `List::run` to
build the view tree from `repo.list_views()` + `repo.get_view_info()`
(parent chains, own-change counts), apply the empty-view filter with
ancestor anchoring, and render the indented hierarchy in both default and
short modes with the trailing summary line.
::file-ref{path=atomic-cli/src/commands/view/list.rs}
:::

:::task{#01M2R5352RGSBC61RH4HTNQAJX-2 status=done criteria=01M2R5352RGSBC61RH4HTNQAJX-ac-1,01M2R5352RGSBC61RH4HTNQAJX-ac-2,01M2R5352RGSBC61RH4HTNQAJX-ac-3,01M2R5352RGSBC61RH4HTNQAJX-ac-4,01M2R5352RGSBC61RH4HTNQAJX-ac-5}
Factor the tree-building, filtering, and line rendering into pure helper
functions inside the same module so the layout is unit-testable without
spawning the command (plain data in, rendered lines out).
::file-ref{path=atomic-cli/src/commands/view/list.rs}
:::

:::task{#01M2R5352RGSBC61RH4HTNQAJX-3 status=done criteria=01M2R5352RGSBC61RH4HTNQAJX-ac-6}
Add tests: three-deep chain rendering, empty leaf hidden + counted, `-a`
lists all with no summary, current empty view always visible, short mode
honors the same filter/tree; then run `cargo test -p atomic` and
fmt/clippy on the touched file.
::file-ref{path=atomic-cli/src/commands/view/list.rs}
:::

:::scope-in
The local `atomic view list` command: hierarchical (parent-chain) layout in
default and `--short` modes, empty-view filtering with the "not shown"
summary line, and the new `-a`/`--all` override flag. Tree rendering lives
in the CLI module as pure, unit-testable helpers.
:::

:::scope-out
- The `--remote` listing path (`run_remote`/`print_remote_views`) stays
  flat: the remote inventory is composed server-side from `.view` objects
  with different change-count semantics. Consequence: remote views won't
  show a hierarchy until a follow-up intent.
- "Pending changes" does NOT consider unrecorded working-copy edits — a
  view whose changes are all unrecorded counts as empty and is hidden
  (unless current or anchoring a shown descendant).
- No repository-layer API changes (`list_views`/`get_view_info` are reused
  as-is), no changes to view create/switch/delete semantics, and the
  `parent:` metadata column stays (redundant with the tree, but removal is
  a separate decision).
:::

:::constraint
Tree construction must be cycle-safe: walk parent chains with a visited
set, and render a view whose parent name is missing from the listing as a
root rather than recursing forever.
:::

:::constraint
The current view is always shown even when it has zero own changes — the
user must never lose track of where they are.
:::

:::constraint
Filtering and the summary line apply to both `--short` and default modes;
`-a` overrides filtering in both while keeping the tree layout.
:::
