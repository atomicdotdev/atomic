---
author: bradley
created_by: bradley <bradley.hilton@atomic.dev>
goals: []
id: ATOM::bradley::1
priority: medium
project: ATOM
seq: 1
status: needs-review
title: Replace hook health snapshots with session-scoped operational logs
uid: 01M4R8PZCD07ADTNQW8R5GBCM3
view: dev
entry_type: intent
content_hash: Q3KZAMGKJF42CBMLQGG6BJKU6KG5T5DCYWSF7K77BQXPNOJKB76A
created_at: 2026-10-12T15:36:35.548955+00:00
updated_at: 2026-10-12T15:36:35.548955+00:00
---
:::why
Concurrent parent and child agents can lose hook-health updates in the shared JSON snapshot. Persist session-scoped operational events and derive health from those events so failures remain attributable and concurrent dispatches cannot overwrite one another.
:::

:::acceptance-criterion{#01M4R8PZCD07ADTNQW8R5GBCM3-ac-1 status=met verifiedBy="OpenCode (openrouter/openai/gpt-6.1-sol-pro)" evidence="Hook tracing/health tests, transactional concurrent session-log tests, local and TCP gRPC dispatch tests, and CLI multi-process/outage regressions pass. Workspace Clippy, formatting, documentation, and protobuf contract checks pass. Workspace tests outside atomic-cli and CLI tests pass except the existing provenance_rpc_sink fixture requiring atomicd; the sibling Reactor now builds atomic-reactor instead."}
Native hooks, typed receive, and shared service dispatch persist tracing events through the existing database/service infrastructure. Concurrent parent/child events survive, malformed input remains diagnosable, and status reports session-scoped health without rewriting hook-health.json. Relevant tests, formatting, protocol checks, and workspace Clippy pass.
:::

:::task{#01M4R8PZCD07ADTNQW8R5GBCM3-1 status=done criteria=01M4R8PZCD07ADTNQW8R5GBCM3-ac-1}
Implement transactional event storage, service append/query methods, tracing emission, session correlation, health projection, and concurrency regressions.
::file-ref{path=atomic-agent/src/hook_health.rs}
::file-ref{path=atomic-repository/src/redb_change_store}
::file-ref{path=libatomic/src/daemon/services_agent.rs}
::file-ref{path=atomic-cli/src/commands/agent}
:::

:::scope-in
Hook diagnostics, session operational logs, service access, and related tests.
:::

:::scope-out
Release/version automation and unrelated vault edits.
:::

:::constraint
Use the existing database ownership and local/reactor service model. Operational diagnostics must preserve the original hook result and remain independent of turn-constrained provenance replay. Preserve unrelated user edits.
:::
