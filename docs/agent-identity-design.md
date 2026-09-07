# Agent Identity & Delegation

How a human issues a keyed identity to an agent, bounds what that agent may do,
proves to Atomic Storage that the agent is theirs, and revokes it — with every
change the agent makes attributable to both the agent and the human behind it.

Status: **implemented**. This document describes the design as built; where
implementation changed the design, the change and its reason are called out
inline.

---

## 1. The shape of the thing

Today an agent has no identity. `atomic-agent/src/identity.rs` derives an
`Author` from the human's default identity by plus-tagging the name —
`claude+60f5 <lee@atomic.dev>` — and signs with **the human's key**. It is a
labelling convention: legible in `log` and `blame`, worth nothing
cryptographically. The `atomic-identity` crate has a complete, unused
`Delegation` model; `AgentEnvelope.delegation_id` exists and is always `None`.

The target:

```
    ┌──────────────────────────┐
    │  aaron                   │  IdentityType::User
    │  did:atomic:B2XZ…        │  registered with atomic.storage → tenant
    │  Ed25519 keypair         │  holds org/workspace grants
    └────────────┬─────────────┘
                 │  signs a delegation certificate
                 │  (eddsa-jcs-2022 over the agent's DID + scope + expiry)
                 ▼
    ┌──────────────────────────┐
    │  aaron+claude            │  IdentityType::Delegated
    │  did:atomic:K7QF…        │  enrolled, never a tenant
    │  its own Ed25519 keypair │  holds NO grants of its own
    └──────────────────────────┘
                 │
                 │  effective permissions =
                 │      aaron's grants  ∩  delegation scope
                 ▼
         acme/api, acme/web · read, record, push · expires in 30d
```

Three invariants hold the design together:

1. **An agent can never exceed its human.** Permissions are an
   *intersection*, never a union. Revoking the human's access to a project
   revokes the agent's in the same instant, with no extra bookkeeping.
2. **Possession is not authority.** Holding the agent key proves you are the
   agent. It says nothing about what the agent may do — that comes from a
   certificate signed by the human, checked server-side on every request.
3. **One signature format.** The delegation certificate is a canonical node
   with an `eddsa-jcs-2022` Data Integrity proof, the same machinery
   `atomic-canonical` already uses for intents and memories. No second
   signing path to keep in sync.

---

## 2. The delegation certificate

A canonical JSON-LD node, JCS-canonicalized and signed by the delegator, using
`atomic-canonical`'s existing `proof` module. It is the whole trust story in
one file: portable, offline-verifiable, and identical on disk, on the wire, and
in the server's database.

```json
{
  "@context": "https://atomic.dev/ns/v1",
  "@type": "AgentDelegation",
  "@id": "urn:atomic:delegation:9HTVQ3M8…",

  "delegator": "did:atomic:B2XZ…",
  "delegatorKey": "did:key:z6MkqR…",
  "delegatorName": "aaron",

  "delegate": "did:atomic:K7QF…",
  "delegateKey": "did:key:z6MkfR…",
  "delegateName": "aaron+claude",
  "softwareAgent": "urn:atomic:agent:claude-code",

  "scope": {
    "permissions": ["read", "record", "push"],
    "servers":    ["https://atomic.storage"],
    "workspaces": ["acme"],
    "projects":   ["acme/api", "acme/web"],
    "views":      ["main", "feature/*"],
    "maxChanges": 500
  },

  "issued":  "2026-09-07T18:00:00Z",
  "expires": "2026-10-07T18:00:00Z",

  "contentHash": "…",
  "proof": {
    "type": "DataIntegrityProof",
    "cryptosuite": "eddsa-jcs-2022",
    "verificationMethod": "did:atomic:B2XZ…#key-1",
    "proofPurpose": "assertionMethod",
    "created": "2026-09-07T18:00:00Z",
    "proofValue": "z3FXQ…"
  }
}
```

Both parties carry two DIDs. `did:atomic` is `base32(blake3(pubkey))` — a
fingerprint, not reversible — so the `did:key` form is carried alongside it and
the public key *is* recoverable from the certificate.

**Changed during implementation:** the design originally carried only
`delegateKey`. That made the certificate unverifiable on a machine holding just
the agent's key — a CI runner, or anyone auditing a clone — because there was no
way to obtain the delegator's key to check the signature against, which would
have made the "offline verifiable" property in §6 untrue. `delegatorKey` fixes
it, and `atomic_canonical::delegation::verify_self_contained` is the entry point
that uses it. Note carefully what that proves: **integrity, not trust**. It
shows the document was signed by whoever holds the key it names and has not been
altered. That the key belongs to the person you think is settled by comparing
against a key you already trust — which is exactly what the server does, using
its registered copy.

Two fields are new relative to today's `DelegationScope`: **`servers`** (a
delegation must be bounded to the server it is valid against — a certificate
minted for a staging deployment must not authenticate against production) and
**`projects`**, which replaces the more ambiguous `repository_patterns`.
`view_patterns` becomes `views`. `max_changes` survives as a client-enforced
budget with a server-side soft counter (see §8).

### Revocation is also a signed document

```json
{
  "@context": "https://atomic.dev/ns/v1",
  "@type": "DelegationRevocation",
  "delegation": "urn:atomic:delegation:9HTVQ3M8…",
  "delegator": "did:atomic:B2XZ…",
  "revokedAt": "2026-09-20T09:14:00Z",
  "reason": "laptop lost",
  "proof": { … }
}
```

Revocation as a signed artifact rather than a bare API call means it
replicates, audits, and verifies like everything else — and a revocation
recorded offline is still provable when it reaches a server later.

---

## 3. CLI surface

Porcelain for the common case, plumbing underneath it. Everything lives under
`atomic identity`, which already owns identity lifecycle; `atomic agent`
remains about hooks and provenance capture.

### 3.1 The 90% case — one command

```console
$ atomic identity agent create claude \
    --agent-type claude-code \
    --projects acme/api,acme/web \
    --can read,record,push \
    --expires 30d

Created agent identity  aaron+claude
  DID           did:atomic:K7QF…   (did:key:z6MkfR…)
  Delegated by  aaron  (did:atomic:B2XZ…)
  Can           read, record, push
  On            acme/api, acme/web
  Expires       2026-10-07  (30 days)

Registered with https://atomic.storage
  Delegation    urn:atomic:delegation:9HTVQ3M8…
  Bound         ~/.atomic/config.toml → [servers.storage] agent_identity

Hooks will now record as aaron+claude. Run `atomic identity agent show
aaron+claude` to inspect, `atomic identity agent revoke aaron+claude` to stop it.
```

That single command does six things: generates an Ed25519 keypair, creates a
`Delegated` identity named `<parent>+<agent>`, mints and signs the certificate
with the parent's key, enrolls the agent key with the bound server, stores the
certificate locally and remotely, and writes the config binding so hooks pick
it up without flags.

Name and email follow the plus-tag convention already in
`atomic-agent/src/identity.rs`: identity `aaron+claude`, email
`aaron+claude@atomic.dev`. Mail still routes to the human; `log` and `blame`
still read as an agent.

### 3.2 Inspect, renew, revoke

```console
$ atomic identity agent list
NAME           AGENT         CAN                  ON             EXPIRES   STATUS
aaron+claude   claude-code   read,record,push     acme/api,+1    in 30d    active
aaron+ci       agent         record,push          acme/*         in 6d     active
aaron+gemini   gemini-cli    read                 acme/api       -3d       expired

$ atomic identity agent show aaron+claude
$ atomic identity agent show aaron+claude --json          # for scripting
$ atomic identity agent renew aaron+claude --expires 30d  # new cert, same key
$ atomic identity agent revoke aaron+claude --reason "laptop lost"
$ atomic identity agent retire aaron+claude               # revoke + delete key
```

`revoke` keeps the identity and its history (past changes stay attributable and
verifiable); `retire` additionally deletes the local secret key and asks the
server to retire the enrollment.

### 3.3 Verification, offline

```console
$ atomic identity delegation verify urn:atomic:delegation:9HTVQ3M8…
✓ Proof valid            signed by did:atomic:B2XZ… (aaron)
✓ Delegate key matches   did:atomic:K7QF… (aaron+claude)
✓ Not expired            18 days remaining
✓ Not revoked            (checked https://atomic.storage, 2s ago)
  Scope                  read, record, push on acme/api, acme/web

$ atomic identity delegation verify --offline <file>   # proof + expiry only
$ atomic change <hash> -a                              # attestation shows the chain
```

`--offline` is the important mode: given a clone and the parent's public key,
anyone can verify that a change claiming `delegation_id` was made by a key the
human actually authorized, with no server involved. Revocation is the only
check that needs the network.

### 3.4 Plumbing

Each porcelain step is separately addressable:

```console
atomic identity new aaron+claude --type agent --delegated-by aaron
atomic identity delegate aaron+claude \
    --can read,record,push --projects acme/api --expires 30d \
    --output cert.json
atomic identity delegation install cert.json
atomic identity delegation push --server https://atomic.storage
atomic identity delegation list [--agent aaron+claude] [--include-expired]
atomic identity delegation revoke urn:atomic:delegation:… [--reason …]
```

Note `atomic identity new --delegated-by` — the builder already flips
`IdentityType` to `Delegated` when a delegator is set
(`atomic-identity/src/identity.rs:450`), but the CLI has no flag to reach it.
Today `--type delegated` produces an orphan with `delegated_by: None`; that
combination should become an error pointing at `--delegated-by`.

### 3.5 Remote enrollment — when the human doesn't hold the key

CI runners and hosted agents must generate their own key; the human's laptop
never sees the secret. Two-step, with proof of possession:

```console
# on the runner — self-signed request, proves it holds the key
$ atomic identity new ci-runner --type agent --request-delegation > request.json

# on the human's machine — inspect, then countersign
$ atomic identity delegate --request request.json \
    --can record,push --projects "acme/*" --expires 7d --output cert.json

# back on the runner
$ atomic identity delegation install cert.json
$ atomic identity delegation push --server https://atomic.storage
```

The request is an `AgentDelegationRequest` node self-signed by the agent key.
`delegate --request` verifies that self-signature before countersigning, so the
human cannot be tricked into delegating to a key nobody holds.

### 3.6 Unattended key access

Agent keys are unattended by definition, so a passphrase prompt is not
available. Resolution order for the agent secret:

1. `--key-file <path>`
2. `ATOMIC_AGENT_KEY` (base64 secret key — for CI secret stores)
3. `~/.atomic/identities/<id>/secret.key`, mode `0600`

Worth knowing before relying on this: `IdentityStore::save_secret_key` writes
`encryption = "none"` on **both** branches — password protection is a `TODO`
(`atomic-identity/src/store.rs:458`). Every secret key on disk today is
base64 plaintext at `0600`. The design's answer is not to pretend otherwise but
to make agent keys *cheap to rotate*: short default expiry (30 days
interactive, 7 days CI), one-command renew, one-command revoke, and a scope
that bounds the blast radius to named projects and permissions. Real key
encryption for *human* parent keys is a separate, still-needed fix.

---

## 4. Local storage

```
~/.atomic/
├── config.toml                       default_identity, [servers.*] bindings
└── identities/                       the identity store root
    ├── config.toml                   default identity, per-usage defaults
    ├── <parent-id>/                  identity.toml, secret.key
    ├── <agent-id>/                   identity.toml (delegated_by set), secret.key
    └── delegations/
        ├── 9HTVQ3M8….json            the signed certificate
        └── 9HTVQ3M8….revocation.json present once revoked
```

**Changed during implementation:** certificates live *inside* the identity store
root rather than beside it, so a store is one self-contained directory to back
up, copy, or point a test at. There is no collision risk with an identity
directory: those are `BASE32_NOPAD` renderings of a 32-byte id, which is
uppercase-only and 52 characters.

The store deals in **documents**, not parsed delegations: `save_delegation`
takes the exact bytes the proof covers. Re-serializing a parsed struct could
produce different bytes than the signature was made over, so the bytes are the
artifact and parsing is the caller's business.

`IdentityStore` gains `save_delegation` / `load_delegation` / `list_delegations`
/ `delete_delegation` — it currently has none; grep for "delegation" in
`store.rs` returns nothing.

Config gains a per-server agent binding, alongside the existing `identity`
binding that #178 taught push to honor:

```toml
[servers.storage]
url            = "https://atomic.storage"
identity       = "aaron"
agent_identity = "aaron+claude"     # new: what hooks sign as
```

---

## 5. Talking to Atomic Storage

Today's mechanism (worth restating, because the design leans on all of it):
the CLI mints a **short-lived self-signed EdDSA JWT** per request, `kid` = the
caller's base32 Ed25519 public key, `sub` = the same value, 5-minute TTL, no
server login endpoint — the server resolves the registered identity by the
`kid` public key and verifies the signature against the key on record
(`atomic-cli/src/commands/token.rs`). Registration is a separate signed payload
that creates a **tenant** (`atomic identity register`).

### 5.1 The token grows an actor claim

RFC 8693 already has the vocabulary for "A acting on behalf of B" — the `act`
claim. The subject becomes the human; the actor becomes the agent; the signer
(and therefore `kid`) is the agent, because the agent is the one holding a key
at request time:

```json
{
  "alg": "EdDSA", "typ": "JWT",
  "kid": "<agent pubkey b32>"
}
{
  "sub": "<parent pubkey b32>",
  "act": { "sub": "<agent pubkey b32>" },
  "dlg": "urn:atomic:delegation:9HTVQ3M8…",
  "iat": …, "exp": …, "jti": "…"
}
```

Note the inversion against today's rule. The server's check becomes:

- `act` absent → `kid == sub` (today's behavior, unchanged for humans)
- `act` present → `kid == act.sub`, and `sub` must be the delegator recorded on
  the delegation named by `dlg`

`dlg` pins *which* delegation authorized the call when an agent holds several,
so the audit row is unambiguous and the server never has to guess.

### 5.2 Endpoints

**New:**

| Method | Path | Auth | Purpose |
|---|---|---|---|
| `POST` | `/identities/agents` | parent | Enroll an agent key + its first delegation |
| `GET` | `/identities/agents` | parent | List my agents |
| `DELETE` | `/identities/agents/{id}` | parent | Retire; cascades revoke |
| `POST` | `/delegations` | parent | Issue or renew a certificate |
| `GET` | `/delegations` | parent | List, filterable by agent/status |
| `GET` | `/delegations/{id}` | parent | Fetch one |
| `POST` | `/delegations/{id}/revoke` | parent | Body is the signed revocation |
| `GET` | `/delegations/{id}/status` | none | `{active, expired, revoked, revokedAt}` for third-party verification |

`POST /identities/agents` verification, in order — all five must hold:

1. The caller's JWT verifies (`kid` = parent's registered key).
2. The certificate's `proof` verifies against the parent's **registered**
   public key — not one supplied in the request.
3. `cert.delegator` is the caller's DID.
4. `cert.delegate` and `cert.delegateKey` agree, and `cert.delegate` matches
   the `public_key` field in the body.
5. `cert.scope.servers` includes this server's canonical URL.

That canonical URL comes from `SERVER_APEX_URL` (defaulting to
`https://{SERVER_BASE_DOMAIN}`), injected as an axum extension — **never** from
the request's `Host` header. A client that could choose the value it is compared
against could enroll a certificate scoped to somewhere else entirely.

On success the server writes an identity row with `kind = 'agent'` and
`parent_identity_id` set — **no tenant, no subdomain, no `/register`**.

**Changed:**

| What | Change | Why |
|---|---|---|
| `POST /register` | Reject when the identity is `agent` or `delegated`; return an error naming `atomic identity agent create` | Today *any* identity that registers mints a tenant. An agent key must never own one. |
| JWT verifier | Accept and enforce `act` / `dlg` per §5.1 | The delegation path |
| Resolver cache | Delegated tokens bypass the verified-token cache entirely | The cache cannot see a revocation or a suspended delegator, and both are re-checked per request. Revocation taking effect *now* is worth one indexed lookup. |
| Authorization | Add the intersection step in §5.3 | The whole point |
| `GET /orgs/{slug}/members` | `OrgMemberInfo` gains `kind` and `parent_identity_id`; agents render nested under their human | So "who is in this org" answers honestly. Enrichment fields (`name`, `public_key`, `status`, `email`) already exist from #149. |
| Push audit | Record `acting_identity_id`, `on_behalf_of_identity_id`, `delegation_id` | Attribution has to survive on the server, not just in the change header |

**Deliberately unchanged:** `GrantSubjectType` stays `{User, Team, Everyone}`.
Agents are not grant subjects in v1. Adding `Agent` there would let someone
grant an agent access its human lacks, which breaks invariant 1 and doubles the
revocation surface. Narrowing is what the scope is for.

### 5.3 The authorization algebra

For an agent request against a resource:

```
allow  ⟺  delegation.status == active
      ∧  now < delegation.expires
      ∧  delegation.delegate == jwt.kid
      ∧  delegation.delegator == jwt.sub
      ∧  server_url ∈ delegation.scope.servers
      ∧  parent_has(delegation.delegator, resource, action)   ← existing check
      ∧  scope_allows(delegation.scope, resource, action)     ← new
```

`scope_allows` matches the **server's** notion of the resource — the workspace
and project from the request path — never a client-asserted value. The existing
`parent_has` relation check is untouched: the agent path calls it with the
delegator's identity and then narrows.

A useful consequence: nothing needs to happen when a human leaves an org. Their
grants disappear, the intersection empties, and every agent they issued goes
inert on the next request.

---

## 6. How we validate that an identity belongs to who

Five links; the chain is only as good as its weakest, so each is worth stating
plainly along with what it does *not* prove.

**1 — The human is who they claim.** Established at `atomic identity register`:
a signature over `atomic-storage:register\n{username}\n{pubkey}\n{timestamp}`
binds the username to the key. *This is trust-on-first-use* — the first key to
claim a username owns it. That is fine for a personal tenant and thin for an
org. Strengthening it is out of scope here but the hooks exist:
`DomainAliasInfo` already carries `verification_method` and
`verification_token`, so an org can prove it controls `acme.com` and thereby
that `aaron@acme.com` is theirs.

**2 — The agent holds its key.** The agent signs its own JWT; `kid` is its
public key. Only the holder of the secret can mint a token. This proves
*possession* and nothing else.

**3 — The agent belongs to that human.** The certificate: the parent's Ed25519
signature over a JCS-canonical document naming the agent's DID, its scope, and
its expiry. This is the link that answers the question. It is verifiable by
anyone holding the parent's public key, with no server and no network — which
is what makes attribution in a clone meaningful rather than a claim the server
makes on your behalf. The server verifies it once at enrollment against the key
it already has on record, and trusts its own stored row thereafter.

**4 — The authorization is still live.** Expiry is in the signed document;
revocation is checked server-side per request. Effective permission is the
intersection from §5.3, so authority is re-derived from the human's *current*
grants on every call rather than frozen at issue time.

**5 — The work stays attributable.** Every change the agent records carries
`attributedTo` = agent DID, `actedOnBehalfOf` = human DID, and
`delegation_id` in the envelope; the attestation is signed by the agent's key.
`atomic change <hash> -a` and `atomic identity delegation verify --offline`
re-walk links 2–4 from a clone months later.

### Threat table

| Threat | What stops it |
|---|---|
| Agent secret key read off disk | Scope limits it to named projects and permissions; short expiry; one-command revoke. It cannot be widened without the human's key. |
| Agent pushes to a project outside its scope | Server matches scope against the request path, not a client claim |
| Forged delegation certificate | Requires the parent's private key — the proof covers the JCS bytes including delegate, scope, and expiry |
| Stale certificate replayed after revocation | Revocation checked per request; `expires` caps the window even against a server that missed the revocation |
| Agent escalates its own permissions | No grants are ever written for agent subjects; effective = parent ∩ scope |
| Agent mints itself a tenant | `/register` rejects agent and delegated identity types |
| Agent work silently attributed to the human | Distinct DIDs in the change header and the server audit row; `blame` shows `claude+60f5` |
| Human leaves the org, agent keeps pushing | Intersection empties on the next request; no separate cleanup |
| Token replay inside the 5-minute TTL | `jti` + short TTL. **Open:** whether the server keeps a `jti` cache is unverified from this repo — see §10. |

---

## 7. Changes by crate

| Crate | File | Change |
|---|---|---|
| `atomic-identity` | `delegation.rs` | Add `servers`, rename `repository_patterns`→`projects`, `view_patterns`→`views`. Remove `signing_data()` — signing moves to `atomic-canonical` so there is one format. Keep the scope/permission types and `allows()`. |
| | `store.rs` | Delegation persistence: save/load/list/delete. Agent-key resolution order (§3.6). |
| | `identity.rs` | `software_agent` label in `IdentityMetadata`; a `parent()` accessor. |
| `atomic-canonical` | `delegation.rs` *(new)* | `AgentDelegation`, `AgentDelegationRequest`, `DelegationRevocation`: mint, verify, JCS + `eddsa-jcs-2022` via the existing `proof` module. |
| | `prov.rs` | A keyed agent's `@id` becomes a real `did:key` instead of `urn:atomic:agent:<slug>` — the module comment at line 27 already anticipates exactly this. `actedOnBehalfOf` keeps pointing at the person. |
| `atomic-agent` | `identity.rs` | Prefer a delegated agent identity when one is bound; sign with *its* key. Keep the plus-tag author name and fall back to today's behavior when no agent identity exists. |
| | `envelope.rs` | Populate `delegation_id` — the field and its builder exist and are never called. |
| `atomic-cli` | `commands/token.rs` | Emit `act` and `dlg` for delegated identities |
| | `commands/auth.rs` | Resolve the agent identity (flag → env → config binding); actionable errors for expired/revoked/out-of-scope |
| | `commands/identity/agent.rs` *(new)* | `create`, `list`, `show`, `renew`, `revoke`, `retire` |
| | `commands/identity/delegate.rs` *(new)* | Mint a certificate; `--request` countersigning |
| | `commands/identity/delegation.rs` *(new)* | `install`, `push`, `list`, `show`, `verify`, `revoke` |
| | `commands/identity/register.rs` | Refuse agent/delegated identities with a pointer to the agent flow |
| | `commands/push/` | Pre-flight the delegation locally so scope and expiry failures are actionable before the network call — the pattern #93 established for credentials |
| `atomic-config` | `lib.rs` | `agent_identity` on server profiles |
| **atomic-storage** | — | §5.2 endpoints, §5.1 JWT rule, §5.3 intersection, agent-aware member listing, audit columns |

---

## 7a. What implementation changed

Four things moved from the design as written. Each is called out where it
applies above; collected here so a reader comparing the two does not have to
hunt:

1. **`delegatorKey` added to the certificate** (§2). Without it a certificate
   cannot be verified by a machine that holds only the agent's key, which
   contradicted the offline-verification property the whole attribution story
   rests on.
2. **Certificates live inside the identity store root** (§4), not beside it, so
   a store is one directory.
3. **`DelegationScope` gained `workspaces`** alongside `projects`. The server's
   object hierarchy is org → workspace → project, and grants attach at workspace
   level; a scope that could not name a workspace would force enumerating every
   project under it.
4. **Permission mapping fails closed** (§5.3). Every server `Permission` with no
   obvious delegated meaning — deletes, tenant administration, identity
   management — maps to `Admin`, which nothing but an explicit `--can admin`
   grants. The trap avoided is `--can push` quietly also meaning "may delete
   this project".

Two things the design specified and the implementation deliberately kept:

- **Agents are not grant subjects.** `GrantSubjectType` is untouched.
- **`sub` inverts on delegated tokens.** The human is the effective subject and
  the agent — the signer, and therefore the `kid` — is the actor. The one
  definition of "who signed" lives in `TokenClaims::signing_subject`, so no call
  site can verify a delegated token against the human's key by accident.

## 8. Deferred

- **`maxChanges`** is in the certificate and enforced client-side. Server-side
  it needs a counter per delegation, which is a write on a hot path. Ship it as
  a soft limit reported in `agent show`, harden later if it earns its keep.
- **Agents as grant subjects.** Explicitly excluded (§5.2). If a use case
  appears for an agent that should reach something its human cannot, it needs
  its own design — it is not a small extension of this one.
- **Nested delegation** (agent delegating to a sub-agent). The certificate
  shape allows it; the intersection rule makes it safe in principle. No use case
  yet, and the revocation semantics get considerably harder.

---

## 9. Migration

Nothing breaks. An identity with no delegation behaves exactly as today: JWT
with no `act` claim, plus-tagged author on the human key, `delegation_id` null.
`atomic identity agent create` is opt-in per agent per machine. Servers that
have not shipped §5.2 reject `POST /identities/agents` with 404, which the CLI
reports as "this server does not support agent identities yet" — the same
degradation pattern used for the pre-v1.4.0 tenancy-mode field in
`register.rs`.

---

## 10. Decisions taken, and one still open

1. **Default expiry: 30 days.** `agent::DEFAULT_EXPIRY_DAYS`, applied by both
   `agent create` and `agent renew`. It is the compromise the design leans on:
   agent secret keys sit unencrypted at `0600`, so the defence against a leaked
   one is that it stops working soon and costs one command to replace.
2. **Certificate verification cadence: verify at write, trust the row at read.**
   The proof is checked at enrollment and renewal against the registered key;
   per-request authorization re-parses the stored certificate for its scope but
   does not re-verify the signature. Re-verifying every request would buy
   defence against a compromised database at the cost of an Ed25519 verify per
   call — and an attacker who can write that table can also write the
   `identities` row the signature would be checked against, so it buys less than
   it looks like.
3. **`jti` replay cache: still open.** Whether atomic-storage caches `jti` for
   the token TTL was not determined, and this work did not add one. If it does
   not, that is a pre-existing gap for humans as much as agents — a 5-minute
   window on a self-signed token — and worth its own issue rather than being
   folded in here.
4. **Naming: `aaron+claude`.** Mirrors the plus-tag convention
   `atomic-agent/src/identity.rs` already used. `aaron/claude` reads better but
   collides with slug parsing in paths and URLs.
