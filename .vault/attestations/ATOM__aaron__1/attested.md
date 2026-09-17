---
intentId: ATOM::aaron::1
sourceContentHash: blake3:1cc1fe2caa54f852afdf7a29c67c9c077f9fd5228d706d4fc3aa0a7ac91c8498
vaultPath: intents/01M2R5352RGSBC61RH4HTNQAJX/intent.md
entry_type: attestation
content_hash: N7SNDIWSSGTGGQ2PK2CG2AEACHW5AXKAFJGBEXMQNZASU72HBR7Q
created_at: 2026-09-17T17:24:39.447914375+00:00
updated_at: 2026-09-17T17:24:39.447914375+00:00
---
{
  "@context": "https://atomic.dev/ns/ctx.jsonld",
  "@id": "urn:atomic:intent:01M2R5352RGSBC61RH4HTNQAJX",
  "@type": "Intent",
  "attributedTo": "did:atomic:YKOT5JBCBRJ6RRUXTH6HYS4LO3DOGLJ76LBUGM4RL74D2GHI2XAQ",
  "contentHash": "blake3:715a42d276472a0cb898c877ef840a1ded71e48f43a53b5b9a40c2f29f64f325",
  "createdAt": "",
  "hasAcceptanceCriterion": [
    {
      "@id": "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-1",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "tests test_hierarchy_layout_three_deep and test_all_flag_lists_every_view assert the exact tree lines; e2e run rendered dev → main → baby-bird-123 → jumbo-tron-444 → wren-77 with 2-space-per-level `- ` bullets and the `*` current marker",
      "text": "`atomic view list` (default and `--short` modes) renders views as an\nindented hierarchy following parent chains: a view with a parent appears as\n`- name` indented two spaces per depth level beneath its parent line, root\nviews at column 0, all sorted alphabetically (depth-first). With the chain\ndev → baby-bird-123 → jumbo-tron-444, the listing reproduces:\n\n```\ndev\n  - baby-bird-123\n    - jumbo-tron-444\n```\n\nThe current view keeps its `*` marker on its line (e.g. `* dev` or\n`  - * baby-bird-123`).",
      "verifiedBy": "cargo test -p atomic-cli + end-to-end CLI run against a real repo"
    },
    {
      "@id": "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-2",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "test_hierarchy_layout_three_deep (empty leaf jumbo-tron-444 hidden), test_empty_sibling_draft_hidden, test_current_empty_view_stays_visible; e2e: after recording 1 change in baby-bird-123, empty jumbo-tron-444/wren-77 stayed hidden while baby-bird-123 appeared",
      "text": "Views with zero own changes are hidden from the listing unless (a) they have\na shown descendant or (b) they are the current view. \"Own changes\" means\n`ViewInfo::own_change_count` — changes recorded into the view that are not\nvisible through the parent chain (for root views this equals\n`change_count`). So an empty leaf draft (e.g. jumbo-tron-444 with no\nrecorded changes) is omitted while its ancestors stay visible to anchor the\nhierarchy.",
      "verifiedBy": "cargo test -p atomic-cli + e2e CLI run"
    },
    {
      "@id": "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-3",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "test_summary_line_counts asserts both singular and plural wording; e2e printed '3 views not shown because contained no changes. -a to view them.' and no summary line when nothing was hidden",
      "text": "When at least one view is hidden, the listing ends with a summary line in\nthe user's requested wording with the exact hidden count and singular/plural\nhandling, e.g. `2 views not shown because contained no changes. -a to view\nthem.` and `1 view not shown because contained no changes. -a to view them.`\nNo summary line is printed when nothing was hidden.",
      "verifiedBy": "cargo test -p atomic-cli + e2e CLI run"
    },
    {
      "@id": "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-4",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "test_all_flag_lists_every_view (visible.len == entries.len, hidden 0, full tree rendered); e2e `atomic view list -a` showed all 5 views including the 3 empty ones with no summary line",
      "text": "A new `-a` / `--all` flag on `atomic view list` disables the empty-view\nfilter: every view is shown (hierarchy layout still applies, in both\ndefault and `--short` modes) and no summary line is printed.",
      "verifiedBy": "cargo test -p atomic-cli + e2e CLI run"
    },
    {
      "@id": "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-5",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "test_default_mode_line_keeps_metadata and test_root_line_format; e2e default mode still printed [shared]/[draft] tags, (n changes, m inherited), state short hash and parent info; existing integration tests (test_list_default_view, test_list_shows_current_marker, test_list_after_switch) pass unchanged",
      "text": "Visible lines in default mode keep the existing metadata (scope tag\n`[shared]`/`[draft]`, change counts, state short hash, parent info) after\nthe tree prefix; `--short` still prints names only. Existing behaviors\n(current-view marker, empty-repo hint) are preserved.",
      "verifiedBy": "cargo test -p atomic-cli + e2e CLI run"
    },
    {
      "@id": "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-6",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "1863 tests passed, 0 failed (13 new tests incl. test_parent_cycle_terminates and test_missing_parent_renders_as_root); clippy clean on the touched crate; fmt applied",
      "text": "New unit/integration tests in the CLI crate cover: hierarchy layout for a\nthree-deep chain, empty-view hiding + summary count, `-a` showing all,\ncurrent empty view staying visible, and short-mode filtering. `cargo test\n-p atomic` passes and the touched file is fmt/clippy clean.",
      "verifiedBy": "cargo test -p atomic-cli; cargo clippy -p atomic-cli; cargo fmt"
    }
  ],
  "hasConstraint": [
    {
      "@id": "urn:atomic:constraint:01m2r5352rgsbc61rh4htnqajx-constraint-1",
      "@type": "Constraint",
      "text": "Tree construction must be cycle-safe: walk parent chains with a visited\nset, and render a view whose parent name is missing from the listing as a\nroot rather than recursing forever."
    },
    {
      "@id": "urn:atomic:constraint:01m2r5352rgsbc61rh4htnqajx-constraint-2",
      "@type": "Constraint",
      "text": "The current view is always shown even when it has zero own changes — the\nuser must never lose track of where they are."
    },
    {
      "@id": "urn:atomic:constraint:01m2r5352rgsbc61rh4htnqajx-constraint-3",
      "@type": "Constraint",
      "text": "Filtering and the summary line apply to both `--short` and default modes;\n`-a` overrides filtering in both while keeping the tree layout."
    }
  ],
  "hasScopeIn": [
    {
      "@id": "urn:atomic:scope:01m2r5352rgsbc61rh4htnqajx-scope-in-1",
      "@type": "ScopeItem",
      "text": "The local `atomic view list` command: hierarchical (parent-chain) layout in\ndefault and `--short` modes, empty-view filtering with the \"not shown\"\nsummary line, and the new `-a`/`--all` override flag. Tree rendering lives\nin the CLI module as pure, unit-testable helpers."
    }
  ],
  "hasScopeOut": [
    {
      "@id": "urn:atomic:scope:01m2r5352rgsbc61rh4htnqajx-scope-out-1",
      "@type": "ScopeItem",
      "text": "- The `--remote` listing path (`run_remote`/`print_remote_views`) stays\n  flat: the remote inventory is composed server-side from `.view` objects\n  with different change-count semantics. Consequence: remote views won't\n  show a hierarchy until a follow-up intent.\n- \"Pending changes\" does NOT consider unrecorded working-copy edits — a\n  view whose changes are all unrecorded counts as empty and is hidden\n  (unless current or anchoring a shown descendant).\n- No repository-layer API changes (`list_views`/`get_view_info` are reused\n  as-is), no changes to view create/switch/delete semantics, and the\n  `parent:` metadata column stays (redundant with the tree, but removal is\n  a separate decision)."
    }
  ],
  "hasTask": [
    {
      "@id": "urn:atomic:task:01M2R5352RGSBC61RH4HTNQAJX-1",
      "@type": "Task",
      "satisfies": [
        "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-1",
        "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-2",
        "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-3",
        "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-4"
      ],
      "taskStatus": "done",
      "text": "Add an `-a`/`--all` flag to the `List` command and rework `List::run` to\nbuild the view tree from `repo.list_views()` + `repo.get_view_info()`\n(parent chains, own-change counts), apply the empty-view filter with\nancestor anchoring, and render the indented hierarchy in both default and\nshort modes with the trailing summary line.",
      "touchesFile": [
        "atomic-cli/src/commands/view/list.rs"
      ]
    },
    {
      "@id": "urn:atomic:task:01M2R5352RGSBC61RH4HTNQAJX-2",
      "@type": "Task",
      "satisfies": [
        "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-1",
        "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-2",
        "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-3",
        "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-4",
        "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-5"
      ],
      "taskStatus": "done",
      "text": "Factor the tree-building, filtering, and line rendering into pure helper\nfunctions inside the same module so the layout is unit-testable without\nspawning the command (plain data in, rendered lines out).",
      "touchesFile": [
        "atomic-cli/src/commands/view/list.rs"
      ]
    },
    {
      "@id": "urn:atomic:task:01M2R5352RGSBC61RH4HTNQAJX-3",
      "@type": "Task",
      "satisfies": [
        "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-6"
      ],
      "taskStatus": "done",
      "text": "Add tests: three-deep chain rendering, empty leaf hidden + counted, `-a`\nlists all with no summary, current empty view always visible, short mode\nhonors the same filter/tree; then run `cargo test -p atomic` and\nfmt/clippy on the touched file.",
      "touchesFile": [
        "atomic-cli/src/commands/view/list.rs"
      ]
    }
  ],
  "humanKey": "ATOM::aaron::1",
  "priority": "medium",
  "proof": {
    "@type": "DataIntegrityProof",
    "cryptosuite": "eddsa-jcs-2022",
    "proofPurpose": "assertionMethod",
    "proofValue": "z64APQ9npDZKqvDQKXBxpdUscP9dRitPG2LCFLs2ZhkCHYPrL7yE6to3N5krQc9BuZG3vqkCD7wLcJZT6PDHq8WDc",
    "verificationMethod": "did:atomic:YKOT5JBCBRJ6RRUXTH6HYS4LO3DOGLJ76LBUGM4RL74D2GHI2XAQ#key-1"
  },
  "status": "done",
  "title": "Hierarchical view list with empty-view filtering",
  "view": "dev",
  "why": "Repositories accumulate many short-lived draft views (created by `view new`\nand `view split`), and the flat alphabetical `atomic view list` hides the\ncontainment structure between them while drowning active work in empty\nviews. Developers need to see the parent/child hierarchy at a glance —\ndev → baby-bird-123 → jumbo-tron-444 — and focus on views that actually\nhold changes, with `-a` as the escape hatch to see everything."
}
