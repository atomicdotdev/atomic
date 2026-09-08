# Agent Identity & Delegation

How a human issues a keyed identity to an agent, bounds what that agent may do,
proves to Atomic Storage that the agent is theirs, and revokes it — with every
change the agent makes attributable to both the agent and the human behind it.

Status: **implemented**. This document describes the design as built; where
implementation changed the design, the change and its reason are called out
inline.

---

## 0. The one decision everything else follows from

**Grants are presented, not registered.**

A certificate is signed by a key the server already trusts — yours, from
registration — so it proves itself the moment you hand it over. The agent
carries it in a header; the server verifies it on the spot. This is the shape of
JOSE `x5c`, SPIFFE SVIDs, UCANs and macaroons, and it puts the round trip where
it belongs:

| Operation | Frequency | Server call? |
|---|---|---|
| Register an agent identity | **once** | yes |
| Issue / extend / widen a grant | **often** | **no** |
| Revoke a grant, or a whole epoch | rare | yes |

The consequence worth internalising: a four-hour grant scoped to one project
costs exactly as much to issue as a year-long grant scoped to everything. There
is no longer a lazy path, so short and narrow becomes the default rather than
the disciplined choice.

Withdrawal is the asymmetry. A credential the holder possesses cannot prove its
own revocation, so that is the one thing that must reach the server — and §6a
covers what happens when it cannot.

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

Two verbs, and the split between them is the whole model: **`agent`** is the
thing you register once, **`grant`** is the thing you issue often.

### 3.1 Register the agent — once

```console
$ atomic identity agent create claude --agent-type claude-code

Created agent identity  alice+claude
  DID           did:atomic:K7QF…
  Key           did:key:z6MkfR…
  Delegated by  alice  (did:atomic:B2XZ…)
  Expires       2026-10-07  (30 days)

Registered with https://atomic.storage
```

Generates a keypair, creates the identity, enrolls it with the server, and
issues a first grant to get you working. Enrollment is the only part that needs
the server, and it happens once per agent per machine.

### 3.2 Issue grants — often, and cheaply

```console
$ atomic identity grant new alice+claude \
    --can record,push --projects acme/api --expires 4h
```

No server call. You signed a document; the agent can use it immediately. Because
this is free, the right habit is a short grant scoped to the work at hand rather
than a standing one scoped to everything.

Three ways to get it to the agent, in rough order of how often you will want
them:

```console
# 1. straight into the environment — nothing to install, nothing to clean up
$ export ATOMIC_DELEGATION=$(atomic identity grant new alice+claude --expires 1h --export)

# 2. a file, for a machine you can drop one on
$ atomic identity grant new alice+claude --expires 8h -o grant.json
$ atomic identity grant load grant.json          # on the agent's machine

# 3. piped
$ atomic identity grant new alice+claude --export | ssh runner 'atomic identity grant load -'
```

`--export` writes the wire form and nothing else to stdout — the summary goes to
stderr — so command substitution captures exactly the grant.

### 3.3 Withdraw

```console
$ atomic identity agent revoke alice+claude --reason "laptop lost"
```

Bumps the agent's **epoch** (every grant issued before now is dead, including
ones nobody has a copy of) and deny-lists each grant this machine knows about,
each with a signed revocation. The epoch is the part that actually stops the
agent; the deny-list is the audit trail.

For a compromised *signing* key, where an attacker can mint grants you will
never see:

```console
$ atomic identity grant revoke --all-mine
```

### 3.4 Inspect and verify

```console
$ atomic identity agent list
$ atomic identity agent show alice+claude
$ atomic identity grant list
$ atomic identity grant verify <urn> --offline
$ atomic identity grant publish <urn>       # optional: for dashboards only
```

`verify --offline` matters most: given a clone and the grant, anyone can check
that a change's `delegation_id` was authorized, with no server. Revocation is
the only check that needs the network.

`publish` is genuinely optional. A grant works the moment you sign it; publishing
only makes the server able to *show* it.

### 3.5 Remote enrollment — a key you never hold

```console
# on the runner
$ atomic identity new ci-runner --type agent --request-delegation > request.json
# on your machine — verifies the self-signature, then countersigns
$ atomic identity delegate --request request.json --can record,push --expires 7d -o grant.json
# back on the runner
$ atomic identity grant load grant.json
```

The request is self-signed by the runner's key, which is what stops you being
talked into granting to a key nobody holds.

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

Apex-scoped: an agent belongs to a *person*, not an org, so one agent works
across every org that person belongs to.

| Method | Path | Auth | Frequency |
|---|---|---|---|
| `POST` | `/identities/agents` | the human | **once per agent** |
| `GET` | `/identities/agents` | the human | on demand |
| `GET` | `/identities/agents/{id}` | the human | on demand |
| `DELETE` | `/identities/agents/{id}` | the human | retire |
| `POST` | `/identities/agents/{id}/epoch` | the human | **withdrawal** |
| `DELETE` | `/identities/agents/{id}/epoch` | the human | undo an epoch |
| `POST` | `/delegations/epoch` | the human | key compromise |
| `POST` | `/delegations/{id}/revoke` | the human | **withdrawal** |
| `GET` | `/delegations` | the human | listings |
| `POST` | `/delegations` | the human | *optional* publish |
| `GET` | `/delegations/{id}/status` | **none** | third-party audit |

Note what is *absent*: there is no endpoint you must call to issue, extend or
widen a grant. `POST /delegations` exists only to publish one for visibility,
and nothing depends on it having been called.

Every authenticated endpoint requires a **direct** call. An agent enrolling
agents, issuing itself grants, or clearing its own epoch would make a leaked key
self-perpetuating — exactly what short expiry exists to bound.

`GET /delegations/{id}/status` is unauthenticated because someone auditing a
change's `delegation_id` may have a clone and no account. It returns a single
boolean — `revoked` — and deliberately nothing else: not scope, not parties, not
expiry, and not whether the id was ever issued. An unknown id and a live one
answer identically, so it cannot be used to enumerate anything.

**Changed:** `POST /register` refuses a key already enrolled as an agent.
Registration mints a tenant named for the identity, so an agent getting one
would hand a delegated key its own top-level namespace that outlives any
withdrawal.

### 5.3 The authorization algebra

For an agent request against a resource:

```
allow  ⟺  certificate verifies against the delegator's REGISTERED key
      ∧  certificate.delegate == jwt.kid
      ∧  certificate.@id == jwt.dlg          (when the token names one)
      ∧  certificate.delegator == jwt.sub
      ∧  server_url ∈ certificate.scope.servers
      ∧  now < certificate.expires
      ∧  certificate.@id ∉ deny-list
      ∧  certificate.issued ≥ agent epoch, and ≥ delegator epoch
      ∧  parent_has(delegator, resource, action)     ← existing check
      ∧  scope_allows(certificate, resource, action) ← narrowing only
```

`scope_allows` matches the **server's** notion of the resource — the workspace
and project from the request path — never a client-asserted value. The existing
`parent_has` relation check is untouched: the agent path calls it with the
delegator's identity and then narrows.

A useful consequence: nothing needs to happen when a human leaves an org. Their
grants disappear, the intersection empties, and every agent they issued goes
inert on the next request.

### 5.4 Cost per request

One extra Ed25519 verify (~50µs) and one indexed lookup for the deny-list plus
both epochs, combined into a single query. Delegated tokens deliberately bypass
the resolver cache: it cannot see a revocation or a suspended delegator, and
both are precisely what an operator revoking an agent expects to take effect
*now* rather than when a token ages out.

The new attack surface is real and worth naming: the server now canonicalizes
and verifies caller-supplied JSON on every delegated request. It is bounded by a
hard 16KB cap checked **before** parsing — rejecting a large payload after
canonicalizing it is not a rejection — and the JCS path wants a fuzz target
before this carries production traffic.

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

**4 — The authorization is still live.** Three independent facts, checked
server-side on every request: the expiry inside the signed document, the
deny-list, and both epochs. Effective permission is then re-derived from the
human's *current* grants rather than frozen at issue time.

## 6a. Withdrawal, and why it is the only thing that must reach the server

A grant proves itself. A withdrawal cannot — you cannot prove a negative with a
document the holder is carrying. Every bearer-credential system has this
asymmetry, which is why X.509 has CRLs and OCSP.

So the burden inverts, which is the right way round: the frequent operation is
free, and the rare one costs a call. Three mechanisms, in increasing blast
radius:

| | Reaches | Use when |
|---|---|---|
| **Expiry** | that certificate | always — the backstop that needs nothing |
| **Deny-list** | one certificate, by id | you know which grant to kill |
| **Epoch** | every grant issued before an instant | you don't, or there is no list |

The epoch is not garnish. Because grants are never registered, **the server
cannot enumerate what is outstanding** — so "revoke everything for this agent"
has no list to walk. A timestamp says it instead. It is also the honest answer
when a laptop goes missing and nobody knows what it issued.

Two scopes:

- **Per agent** (`identities.delegations_valid_from` on the agent). The routine
  tool. Narrowing a scope means issuing a tighter grant *and* bumping this, so
  the old broader one dies immediately instead of lingering until its own
  expiry. `atomic identity agent revoke` does both.
- **Per delegator** (the same column on the human). The key-compromise button:
  if your signing key leaks, an attacker can mint grants nobody knows exist, and
  this is the only action that reaches them. `atomic identity grant
  revoke --all-mine`.

An epoch can be cleared, which brings unexpired grants back. That is safe
because each is still bounded by its own expiry, and it means bumping one in
error is recoverable rather than permanent.

### What this costs you

Revocation is now the operation with a hard network dependency. `agent revoke`
says so explicitly when it cannot reach the server, and distinguishes the two
outcomes: grants this machine knows about are refused locally straight away,
while grants it has never seen **remain valid** until the epoch bump lands.
That is stated in the output rather than left for someone to discover.

### Threat table

| Threat | What stops it |
|---|---|
| Agent secret key read off disk | Scope limits it to named projects and permissions; short expiry; one-command revoke. It cannot be widened without the human's key. Because issuing is free, the grant it holds should be hours old and narrow, not a standing year-long one. |
| Agent pushes to a project outside its scope | Server matches scope against the request path, not a client claim |
| Forged delegation certificate | Requires the parent's private key — the proof covers the JCS bytes including delegate, scope, and expiry |
| Stale certificate replayed after revocation | Deny-list and both epochs checked per request; `expires` caps the window regardless |
| **Human's signing key compromised** | The attacker can mint grants the server has never seen and there is no list to revoke. The delegator epoch is the answer, and the only one — it works on time rather than identifiers |
| **Oversized or malformed certificate in the header** | 16KB cap enforced before parsing; decode and verify are separate functions so a well-formed certificate is never mistaken for a trusted one |
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
| `atomic-canonical` | `delegation.rs` *(new)* | `AgentDelegation`, `AgentDelegationRequest`, `DelegationRevocation`: mint, verify, JCS + `eddsa-jcs-2022` via the existing `proof` module. Plus the wire encoding (`encode_for_transport`, the 16KB cap, the header name) shared by both ends. |
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
| **atomic-storage** | — | §5.2 endpoints, §5.1 JWT rule, §5.3 algebra, deny-list + epochs, per-request certificate verification, audit rows |

---

## 7a. What implementation changed

Six things moved from the design as first written. Each is called out where it
applies; collected here so a reader comparing the two does not have to hunt.

1. **Grants are presented, not registered** (§0). The first cut made the server
   the registry: a certificate had to be POSTed before an agent could use it.
   That put the round trip on the frequent operation and made short-lived
   narrow grants *more* work than a standing broad one — precisely backwards.
2. **The deny-list and epochs replaced per-grant rows as the authorization
   source** (§6a). Once grants are not registered, the server cannot enumerate
   them, so withdrawal needed a primitive that works on time rather than
   identifiers.
3. **`delegatorKey` added to the certificate** (§2). Without it a certificate
   cannot be verified by a machine holding only the agent's key — a CI runner,
   or anyone auditing a clone — which contradicted the offline-verification
   property the attribution story rests on. It also turned out to be what makes
   the *server's* lookup work without a registered row.
4. **Certificates live inside the identity store root** (§4), not beside it, so
   a store is one directory.
5. **`DelegationScope` gained `workspaces`**. The server's hierarchy is
   org → workspace → project and grants attach at workspace level; a scope that
   could not name one would force enumerating every project under it.
6. **Permission mapping fails closed** (§5.3). Every server `Permission` with
   no obvious delegated meaning — deletes, tenant administration, identity
   management — maps to `Admin`, which nothing but an explicit `--can admin`
   grants. The trap avoided is `--can push` quietly also meaning "may delete
   this project".

Two things the design specified and the implementation kept:

- **Agents are not grant subjects.** `GrantSubjectType` is untouched.
- **`sub` inverts on delegated tokens.** The human is the effective subject and
  the agent — the signer, and therefore the `kid` — is the actor. The one
  definition of "who signed" lives in `TokenClaims::signing_subject`, so no call
  site can verify a delegated token against the human's key by accident.

## 8. Deferred

- **A fuzz target for the JCS path.** The server now canonicalizes
  caller-supplied JSON on every delegated request. The 16KB pre-parse cap bounds
  it, but this is the one genuinely new attack surface and it should be fuzzed
  before production traffic.
- **Integration tests for the route handlers.** They need live Postgres and
  there is no harness for authenticated end-to-end requests. This matters more
  under the presented-grant model than it did under registration: the
  authorization path went from a row lookup to a seven-step verification.
- **Caching verified certificates by content hash.** `transport_fingerprint`
  exists for it and the encoding is deterministic to make it possible, but
  nothing caches yet. One Ed25519 verify per request is cheap; measure before
  adding a cache with its own invalidation questions.
- **`maxChanges`** is in the certificate and enforced client-side. Server-side
  it needs a counter per grant, which is a write on a hot path. Ship it as a
  soft limit reported in `agent show`, harden later if it earns its keep.
- **Agents as grant subjects.** Explicitly excluded (§5.3). If a use case
  appears for an agent that should reach something its human cannot, it needs
  its own design — it is not a small extension of this one.
- **Nested delegation** (agent delegating to a sub-agent). The certificate
  shape allows it; the intersection rule makes it safe in principle. No use case
  yet, and the revocation semantics get considerably harder — an epoch on an
  intermediate would need to cascade.

---

## 9. Migration

Nothing breaks. An identity with no grant behaves exactly as today: a JWT with
no `act` claim, no `Atomic-Delegation` header, a plus-tagged author on the
human's key, `delegation_id` null. A human's token is byte-identical to the
pre-agent format, with a test pinning that, so every deployed CLI is unaffected.

`atomic identity agent create` is opt-in per agent per machine. A server that
has not shipped §5.2 rejects `POST /identities/agents` with 404, which the CLI
reports as "this server does not support agent identities yet" — the same
degradation pattern used for the pre-v1.4.0 tenancy-mode field in `register.rs`.

The two repositories must land in order: the server compiles against the
client's `atomic-canonical`, so the client change has to reach `release` before
the server's CI can pass. Any future change to the shared certificate contract
will have the same red window on the server side.

---

## 10. Decisions taken, and one still open

1. **Default expiry: 30 days — but that is a ceiling, not a target.**
   `agent::DEFAULT_EXPIRY_DAYS` applies when you say nothing. Since issuing
   costs no server call, the intended habit is far shorter: hours, scoped to the
   work at hand. The default exists so `agent create` produces something usable,
   not as a recommendation.
2. **Verification cadence: every request.** The signature is re-checked against
   the delegator's registered key on each delegated call. An earlier revision
   verified once at registration and trusted the stored row thereafter, which
   stopped making sense the moment grants stopped being registered — there is no
   row to trust. The cost is one Ed25519 verify (~50µs), and it buys the
   property the whole model rests on: a grant is only as good as the signature
   presented with it.
3. **`jti` replay cache: still open.** Whether atomic-storage caches `jti` for
   the token TTL was not determined, and this work did not add one. If it does
   not, that is a pre-existing gap for humans as much as agents — a 5-minute
   window on a self-signed token — and worth its own issue rather than being
   folded in here.
4. **Naming: `aaron+claude`.** Mirrors the plus-tag convention
   `atomic-agent/src/identity.rs` already used. `aaron/claude` reads better but
   collides with slug parsing in paths and URLs.
