# Intent Identity: ULIDs and Human Keys

How Atomic identifies vault intents so a distributed team never collides — even
working offline — while still getting short, human-friendly references.

## The problem

An intent id wants four properties, and you can only have three at once:

1. **Human-friendly** sequential keys (`PIMO::lee::3`)
2. **Offline** allocation (no server round-trip to create an intent)
3. **Globally unique** across the whole team
4. **No coordination**

Sequential + offline + globally-unique is impossible without coordination — two
teammates working offline both mint "number 3" and collide on push. Atomic
resolves this by splitting identity into two layers:

- a **stable machine identity** that is globally unique with zero coordination, and
- a **human display key** that is friendly and collision-free *per author*.

## Primary identity: a ULID

Every intent's real identity is a [ULID](https://github.com/ulid/spec) — a
26-character, time-sortable, Crockford-base32 identifier minted locally at
creation. It never collides regardless of team size or offline work, and it is
used everywhere identity matters:

| Surface | Form |
|---------|------|
| On-disk path | `intents/<ULID>/intent.md` |
| Canonical URN | `urn:atomic:intent:<ulid>` |
| Knowledge-graph node | `intent:<ULID>` |
| Frontmatter | `uid: <ULID>` |

Because the ULID alone guarantees a unique path, intents are stored **flat** —
there is no view/session/turn nesting in the path. (Earlier builds used
`intents/<view>/<session>/<turn>/intent.md`; that nesting is gone.)

```
intents/
├── 01J8ZE7G2WABCDEFGHJKMNPQRS/
│   └── intent.md
├── 01J8ZE9K4TQ7XM2N6P0VWY3ZBD/
│   └── intent.md
└── ...
```

## Human display key: `PROJECT::author::seq`

The friendly reference is composed from three parts:

```
PIMO::lee-faus::3
────  ────────  ─
  │       │     └── per-author sequence (local counter)
  │       └──────── author handle (slug of the default identity)
  └──────────────── project code (uppercased)
```

- **PROJECT** — a short code stored in the vault manifest, derived from the
  project directory name on first use (e.g. `hello-demo` → `HELL`).
- **author** — a slug of Atomic's default identity name (`Lee Faus` → `lee-faus`).
  Interior hyphens are preserved.
- **seq** — a **per-author** counter. Each author increments only their own
  counter, so two teammates never produce the same key even offline.

### Why `::` and not `-`

The field separator is `::`, not `-`, precisely because an author handle can
contain a hyphen (`lee-faus`). With `::`, `PIMO::lee-faus::3` splits
unambiguously into `[PIMO, lee-faus, 3]`. The key is also stored as structured
components (`project`, `author`, `seq`) in the manifest and frontmatter, so the
separator is cosmetic — identity never depends on parsing the rendered string.

### Collision safety

The human key is collision-safe for the team case because it is per-author:
`PIMO::lee::3` and `PIMO::alice::3` are distinct even though both are "the third
intent." The rare same-author-two-clones case is disambiguated by the ULID (the
files live at different paths; only a manifest display entry could be
last-write-wins).

## Referring to intents

You never have to type the project — it is always the current project — and you
rarely type the author. Resolution fills in the current project and identity:

| You type | Resolves to |
|----------|-------------|
| `3` | `PROJECT::<current-author>::3` |
| `alice::3` | `PROJECT::alice::3` (a teammate's intent) |
| `PIMO::lee-faus::3` | exact (project case-normalized) |
| `01J8ZE…` | the intent whose ULID matches (a **unique prefix** works too) |

```bash
atomic intent show 3                      # my third intent, this project
atomic intent show alice::3               # alice's third intent
atomic intent show PIMO::lee-faus::3      # fully qualified
atomic intent show 01J8ZE7G2W             # by ULID prefix
```

If a prefix matches more than one intent, Atomic asks for a longer prefix
instead of choosing one arbitrarily.

All of these flow through a single resolver
(`Repository::resolve_intent_key`), shared by the CLI and the attestation
bridge, so every command resolves references identically.

## Session and turn are provenance, not identity

When an intent is created inside an agent session, the session id and turn
number are recorded as **frontmatter metadata** (and on the manifest summary),
not baked into the path:

```yaml
uid: 01J8ZE7G2WABCDEFGHJKMNPQRS
id: PIMO::lee-faus::3
project: PIMO
author: lee-faus
seq: 3
session: ses_0739e30edffecd2BVayyUHjQpY
turn: 4
title: Interactive readline prompt
status: backlog
view: fresh-ridge-394a
```

`atomic session show` reads these fields to link a session to the intents it
produced (mapping each turn to the intent created during it). The stored `turn`
is `turn_count + 1` at creation, so it maps to the 0-indexed session-ledger turn
by subtracting one.

## Child ids (acceptance criteria, tasks)

The directive body's child ids are namespaced under the intent's ULID, so they
are globally unique and never carry the human key's `::`/`-`. `atomic intent new`
scaffolds them from the ULID:

```
:::acceptance-criterion{#01j8ze7g2w…-ac-1 status=unmet}
:::task{#01j8ze7g2w…-1 criteria=01j8ze7g2w…-ac-1}
```

## Migration

There is none by design. New intents use the ULID + `PROJECT::author::seq`
scheme going forward; intents already stored under the old
`intents/<view>/<session>/<turn>/` layout remain readable, and legacy `PIMO-1`
references still resolve case-insensitively against manifest keys.

## Relevant code

- `atomic-core/src/pristine/vault.rs` — `VaultManifest` (`intent_seq`,
  `project_code`, `allocate_author_seq`, `compose_human_key`), `IntentSummary`
  (`uid`/`human_key`/`project`/`author`/`seq`/`session`/`turn`),
  `parse_intent_reference` / `IntentRef`, `HUMAN_KEY_SEP`.
- `atomic-repository/src/repository/vault_intent.rs` — `vault_intent_create`
  (ULID mint, path, frontmatter), `resolve_intent_key` / `normalize_intent_id`,
  `slug_author`.
- `atomic-cli/src/commands/intent/new.rs` — directive scaffold (child-id
  namespacing under the ULID).
- `atomic-cli/src/commands/session.rs` — session→intent mapping from manifest
  provenance.

See also [Intent Knowledge Graph](./intent-graph.md) for how these intents
become a queryable semantic graph.
