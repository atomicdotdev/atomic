# atomic-teams

Team collaboration features for [Atomic VCS](https://atomic.dev).

This crate provides the domain logic for multi-user collaboration on an
[atomic-storage](https://github.com/atomicdotdev/atomic) server —
organization management, team membership, permission grants, and domain
aliases. It communicates with the server through the `StorageClient` type
from `atomic-remote`.

> **Feature-gated in the CLI.** The `atomic` binary includes `atomic-teams`
> by default. To build without it, pass `--no-default-features`. When the
> feature is disabled the `org` and `team` CLI commands disappear; workspace
> and project management remain available.

## Table of Contents

- [Quick Start](#quick-start)
- [Architecture](#architecture)
- [Authentication](#authentication)
- [CLI Commands](#cli-commands)
  - [Organizations](#organizations)
  - [Organization Members](#organization-members)
  - [Teams](#teams)
  - [Team Members](#team-members)
  - [Workspaces & Projects](#workspaces--projects)
- [Library Usage](#library-usage)
  - [StorageClient](#storageclient)
  - [Organizations API](#organizations-api)
  - [Members API](#members-api)
  - [Teams API](#teams-api)
  - [Team Members API](#team-members-api)
  - [Grants API](#grants-api)
  - [Domain Aliases API](#domain-aliases-api)
- [Local Development](#local-development)
  - [Prerequisites](#prerequisites)
  - [Start the Server](#start-the-server)
  - [Register an Identity](#register-an-identity)
  - [Full Localhost Walkthrough](#full-localhost-walkthrough)
- [Error Handling](#error-handling)
- [Types Reference](#types-reference)
- [Feature Flag](#feature-flag)
- [License](#license)

## Quick Start

```bash
# 1. Create an identity (if you haven't already)
atomic identity new alice --email alice@example.com --set-default

# 2. Register with a server (production or localhost)
atomic identity register https://atomic.storage

# 3. Create a workspace and project
atomic workspace create "My Apps"
atomic project create my-api --workspace my-apps --kind rust

# 4. Create a team org (requires teams feature)
atomic org create "Acme Corp" --email admin@acme.com

# 5. Invite collaborators
atomic org member add bob@acme.com --role admin

# 6. Create a team
atomic team create "Backend Engineering" --description "Backend services"
```

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         atomic (CLI)                            │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────────────┐    │
│  │  workspace/*  │  │  project/*   │  │  org/*   team/*    │    │
│  │  (always on)  │  │  (always on) │  │  (feature: teams)  │    │
│  └──────┬───────┘  └──────┬───────┘  └────────┬───────────┘    │
│         │                 │                    │                │
│         ▼                 ▼                    ▼                │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │                   commands/client.rs                      │   │
│  │           build_client(org_override) -> StorageClient     │   │
│  └──────────────────────────┬───────────────────────────────┘   │
└─────────────────────────────┼───────────────────────────────────┘
                              │
          ┌───────────────────┼───────────────────┐
          ▼                                       ▼
┌───────────────────┐               ┌──────────────────────┐
│   atomic-remote   │               │    atomic-teams      │
│  ┌─────────────┐  │               │  ┌────────────────┐  │
│  │StorageClient│◄─┼───────────────┼──│  org, member,  │  │
│  │  (HTTP)     │  │               │  │  team, grant,  │  │
│  └─────────────┘  │               │  │  domain        │  │
│  Authorization:   │               │  └────────────────┘  │
│  Bearer <pubkey>  │               └──────────────────────┘
└────────┬──────────┘
         │ HTTPS
         ▼
┌─────────────────────────────────────────────────────────────────┐
│                      atomic-storage                             │
│  Host: alice.atomic.storage                                     │
│  ┌──────────────┐  ┌──────────┐  ┌────────────────────────┐    │
│  │ TenantContext │  │ Caller   │  │  ReBAC Engine          │    │
│  │ (subdomain)   │  │ Identity │  │  (Zanzibar-style)      │    │
│  │               │  │ (Bearer) │  │                        │    │
│  └──────┬───────┘  └────┬─────┘  └────────────┬───────────┘    │
│         │               │                      │                │
│         ▼               ▼                      ▼                │
│  Organizations ── Identities ── Memberships ── Grants           │
│  Workspaces ──── Projects ──── Teams ───────── Domain Aliases   │
└─────────────────────────────────────────────────────────────────┘
```

## Authentication

Every request uses **Ed25519 public-key authentication**. There are no
passwords, no JWTs, no OAuth flows.

1. The CLI loads the default identity from `~/.atomic/identities/`.
2. It base32-encodes the 32-byte Ed25519 public key (52 characters, no padding).
3. It sends `Authorization: Bearer <base32_public_key>` on every request.
4. The server decodes the key, looks it up in the `identities` table, verifies
   org membership, and feeds the resolved identity into the ReBAC engine.

The server URL and default org are stored in `~/.atomic/config.toml`:

```toml
[server]
url = "https://atomic.storage"
default_org = "alice"
```

This is set automatically by `atomic identity register`.

## CLI Commands

### Organizations

```bash
# Show details for the current default org
atomic org show

# Show a specific org
atomic org show acme-corp

# Show as JSON
atomic org show acme-corp --format json

# Create a new team organization
atomic org create "Acme Corp" --email admin@acme.com

# Update org metadata
atomic org update acme-corp --name "Acme Corporation" --email new@acme.com

# Upgrade personal org → team org (enables multi-member features)
atomic org upgrade

# Switch your default org context
atomic org set acme-corp

# Delete an org (owner only, interactive confirmation)
atomic org delete acme-corp

# Delete without confirmation
atomic org delete acme-corp --force
```

### Organization Members

Identity references accept **email**, **name**, or **UUID**. The CLI resolves
emails and names to UUIDs automatically via `GET /identities/resolve`.

```bash
# List all members
atomic org member list
atomic org member list --org acme-corp

# Add a member by email (most common)
atomic org member add bob@acme.com --role admin

# Add by identity name
atomic org member add bob --role member

# UUIDs still work (backward compatible)
atomic org member add 550e8400-e29b-41d4-a716-446655440000 --role member

# Available roles: owner, admin, member (default: member)

# Update a member's role
atomic org member update bob@acme.com --role owner

# Remove a member (cannot remove the last owner)
atomic org member remove bob@acme.com
atomic org member remove bob --force
```

### Teams

```bash
# List teams in your org
atomic team list
atomic team list --org acme-corp --format json

# Create a team
atomic team create "Backend Engineering"
atomic team create "Secret Ops" --visibility secret --description "Classified projects"

# Show team details
atomic team show backend-engineering

# Update a team
atomic team update backend-engineering --name "Backend Eng" --visibility visible

# Delete a team (cascades memberships and grants)
atomic team delete old-team --force
```

### Team Members

Same flexible identity references — email, name, or UUID.

```bash
# List members of a team
atomic team member list backend-engineering

# Add a member by email (roles: maintainer, contributor, collaborator, consumer)
atomic team member add backend-engineering bob@acme.com
atomic team member add backend-engineering bob@acme.com --role collaborator

# Add by name
atomic team member add backend-engineering bob --role contributor

# Update role
atomic team member update backend-engineering bob@acme.com --role collaborator

# Remove
atomic team member remove backend-engineering bob
```

### Workspaces & Projects

These commands are always available — they don't require the `teams` feature.

```bash
# Workspaces
atomic workspace list
atomic workspace create "My Team" --visibility private
atomic workspace show my-team
atomic workspace update my-team --visibility public
atomic workspace delete my-team --force

# Projects
atomic project list --workspace my-team
atomic project create my-api --workspace my-team --kind rust
atomic project show my-team/my-api
atomic project update my-team/my-api --description "Main API service"
atomic project delete my-team/my-api --force

# Initialize: create project on server + configure local remote in one step
atomic project init my-api --workspace my-team --kind rust
```

All management commands accept `--org <slug>` to target a specific
organization (defaults to the configured `default_org`).

## Library Usage

### StorageClient

All functions in `atomic-teams` take a `&StorageClient` reference. The client
is constructed from a base URL, org slug, and bearer token:

```rust
use atomic_remote::StorageClient;

// Production
let client = StorageClient::new(
    "https://alice.atomic.storage",
    "alice",
    "JBSWY3DPEHPK3PXP...",  // base32 Ed25519 public key
)?;

// Local development
let client = StorageClient::new(
    "http://alice.localhost:8080",
    "alice",
    "JBSWY3DPEHPK3PXP...",
)?;
```

The CLI's `build_client()` helper resolves this automatically from
`~/.atomic/config.toml` and the default identity.

### Identity Resolution

The server provides `GET /identities/resolve` for looking up identities by
email or name. The `StorageClient` exposes this as two methods:

```rust
// Resolve by verified email address
let bob = client.resolve_identity_by_email("bob@acme.com").await?;
println!("Bob's UUID: {}", bob.id);   // use this for member/grant operations

// Resolve by display name
let carol = client.resolve_identity_by_name("carol").await?;
println!("Carol's UUID: {}", carol.id);
```

The CLI commands accept email, name, or UUID directly and resolve
automatically — you never need to look up UUIDs by hand.

### Organizations API

```rust
use atomic_teams::{org, OrgInfo, TeamsResult};
use atomic_remote::StorageClient;

async fn org_example(client: &StorageClient) -> TeamsResult<()> {
    // Create
    let info: OrgInfo = org::create_org(client, "Acme Corp", Some("admin@acme.com")).await?;
    println!("Created: {} (slug: {}, kind: {})", info.name, info.slug, info.kind);

    // Read
    let info = org::get_org(client, "acme-corp").await?;

    // Update (only non-None fields are sent)
    let info = org::update_org(client, "acme-corp", Some("Acme Inc"), None).await?;

    // Upgrade personal → team
    let info = org::upgrade_org(client, "acme-corp").await?;
    println!("Plan: {}", info.plan);

    // Delete
    org::delete_org(client, "acme-corp").await?;

    Ok(())
}
```

### Members API

```rust
use atomic_teams::{member, OrgMemberInfo, OrgRole, TeamsResult};
use atomic_remote::StorageClient;
use uuid::Uuid;

async fn member_example(client: &StorageClient) -> TeamsResult<()> {
    // The CLI resolves emails/names → UUIDs automatically.
    // At the library level, you work with UUIDs directly.
    // Use client.resolve_identity_by_email() or
    // client.resolve_identity_by_name() to look up the UUID first.

    let bob = client.resolve_identity_by_email("bob@acme.com").await
        .expect("Bob must be registered");

    // List all members
    let members: Vec<OrgMemberInfo> = member::list_members(client, "acme-corp").await?;
    for m in &members {
        println!("{}: {} (joined {})", m.identity_id, m.role, m.joined_at);
    }

    // Add a member
    let info = member::add_member(client, "acme-corp", bob.id, OrgRole::Admin).await?;

    // Get a specific member
    let info = member::get_member(client, "acme-corp", bob.id).await?;

    // Change role
    let info = member::update_member_role(client, "acme-corp", bob.id, OrgRole::Owner).await?;

    // Remove (errors if last owner)
    member::remove_member(client, "acme-corp", bob.id).await?;

    Ok(())
}
```

### Teams API

```rust
use atomic_teams::{team, TeamInfo, TeamVisibility, TeamsResult};
use atomic_remote::StorageClient;

async fn team_example(client: &StorageClient) -> TeamsResult<()> {
    // List
    let teams: Vec<TeamInfo> = team::list_teams(client, "acme-corp").await?;

    // Create
    let info: TeamInfo = team::create_team(
        client,
        "acme-corp",
        "Backend Engineering",
        Some("Backend services team"),
        Some(TeamVisibility::Visible),
    ).await?;
    println!("Created team: {} (slug: {})", info.name, info.slug);

    // Get
    let info = team::get_team(client, "acme-corp", "backend-engineering").await?;

    // Update (partial — None fields are omitted)
    let info = team::update_team(
        client,
        "acme-corp",
        "backend-engineering",
        None,                             // keep name
        Some("Updated description"),      // change description
        Some(TeamVisibility::Secret),     // change visibility
    ).await?;

    // Delete (cascades memberships and grants)
    team::delete_team(client, "acme-corp", "backend-engineering").await?;

    Ok(())
}
```

### Team Members API

```rust
use atomic_teams::{team_member, TeamMemberInfo, TeamRole, TeamsResult};
use atomic_remote::StorageClient;

async fn team_member_example(client: &StorageClient) -> TeamsResult<()> {
    // Resolve Bob's identity by email (or by name with resolve_identity_by_name)
    let bob = client.resolve_identity_by_email("bob@acme.com").await
        .expect("Bob must be registered");

    // List
    let members: Vec<TeamMemberInfo> =
        team_member::list_team_members(client, "acme-corp", "backend-eng").await?;

    // Add
    let info = team_member::add_team_member(
        client, "acme-corp", "backend-eng", bob.id, TeamRole::Contributor,
    ).await?;

    // Promote to maintainer
    let info = team_member::update_team_member_role(
        client, "acme-corp", "backend-eng", bob.id, TeamRole::Maintainer,
    ).await?;

    // Remove
    team_member::remove_team_member(
        client, "acme-corp", "backend-eng", bob.id,
    ).await?;

    Ok(())
}
```

### Grants API

Grants bind a subject (user or team) to a relation (read, write, admin,
owner) on a resource (org or workspace). The server uses a Zanzibar-style
ReBAC engine with structural inheritance — an org owner automatically has
admin access to all workspaces within the org.

```rust
use atomic_teams::{grant, GrantInfo, GrantRelation, GrantSubjectType, TeamsResult};
use atomic_remote::StorageClient;

async fn grant_example(client: &StorageClient) -> TeamsResult<()> {
    // Resolve identities by email/name first
    let bob = client.resolve_identity_by_email("bob@acme.com").await.unwrap();
    let backend_team = client.resolve_identity_by_name("backend-eng").await.unwrap();

    // --- Organization grants ---

    // List
    let grants: Vec<GrantInfo> = grant::list_org_grants(client, "acme-corp").await?;

    // Grant a user admin access to the org
    let info = grant::add_org_grant(
        client,
        "acme-corp",
        GrantSubjectType::User,
        Some(bob.id),
        GrantRelation::Admin,
    ).await?;

    // Grant a team write access to the org
    let info = grant::add_org_grant(
        client,
        "acme-corp",
        GrantSubjectType::Team,
        Some(backend_team.id),
        GrantRelation::Write,
    ).await?;

    // Revoke
    grant::revoke_org_grant(
        client, "acme-corp", GrantSubjectType::User, Some(bob.id),
    ).await?;

    // --- Workspace grants ---

    // Grant a team read access to a workspace
    let info = grant::add_workspace_grant(
        client,
        "my-workspace",
        GrantSubjectType::Team,
        Some(backend_team.id),
        GrantRelation::Read,
    ).await?;

    // List workspace grants
    let grants = grant::list_workspace_grants(client, "my-workspace").await?;

    // Revoke
    grant::revoke_workspace_grant(
        client, "my-workspace", GrantSubjectType::Team, Some(backend_team.id),
    ).await?;

    Ok(())
}
```

**Permission hierarchy** (higher implies lower):

| Resource | Owner | Admin | Write | Read |
|----------|-------|-------|-------|------|
| Organization | ✓ implies all | ✓ implies Write, Read, Member | ✓ implies Read | — |
| Workspace | — | ✓ implies Write, Read | ✓ implies Read | — |
| Project | — | ✓ implies Write, Read | ✓ implies Read | — |

Structural inheritance: org admin → workspace admin (for all workspaces in the org).

### Domain Aliases API

Domain aliases let organizations claim DNS domains for verified email
addresses and custom routing.

```rust
use atomic_teams::{domain, DomainAliasInfo, TeamsResult};
use atomic_remote::StorageClient;

async fn domain_example(client: &StorageClient) -> TeamsResult<()> {
    // List claimed domains
    let domains: Vec<DomainAliasInfo> = domain::list_domains(client, "acme-corp").await?;

    // Claim a new domain (starts as "pending")
    let info: DomainAliasInfo = domain::claim_domain(client, "acme-corp", "eng.acme.com").await?;
    println!("Add this DNS TXT record: {}", info.verification_token.unwrap_or_default());
    println!("Status: {}", info.status);  // "pending"

    // After configuring DNS, verify it
    let info = domain::verify_domain(client, "acme-corp", info.id).await?;
    println!("Status: {}", info.status);  // "verified"

    // Revoke
    domain::revoke_domain(client, "acme-corp", info.id).await?;

    Ok(())
}
```

## Local Development

### Prerequisites

1. **Rust toolchain** — `rustup` with stable 1.87+
2. **PostgreSQL** — the storage server uses Postgres for metadata
3. **atomic-storage server** — cloned and built locally

```bash
# Clone the repos side by side
git clone https://github.com/atomicdotdev/atomic.git
git clone https://github.com/atomicdotdev/atomic-storage.git
```

### Start the Server

```bash
cd atomic-storage

# Start Postgres (Docker is easiest)
docker run -d --name atomic-pg \
  -e POSTGRES_DB=atomic_storage \
  -e POSTGRES_USER=atomic \
  -e POSTGRES_PASSWORD=atomic \
  -p 5432:5432 \
  postgres:16

# Set environment variables
export DATABASE_URL="postgres://atomic:atomic@localhost:5432/atomic_storage"
export ATOMIC_BASE_DOMAIN="localhost:8080"
export ATOMIC_LICENSE_MODE="evaluation"

# Run migrations and start the server
cargo run --bin atomic-storage-server
# Server starts on :8080 (public) and :9090 (internal health/metrics)
```

The server should show:

```
INFO  Public API listening on 0.0.0.0:8080
INFO  Internal API listening on 0.0.0.0:9090
```

### Register an Identity

```bash
cd atomic

# Create a local identity
atomic identity new alice --email alice@dev.local --set-default

# Register with the local server
atomic identity register http://localhost:8080
```

You should see:

```
✓ Registered as alice
  Tenant ID: 550e8400-e29b-41d4-a716-446655440000
  Slug:      alice
  URL:       http://alice.localhost:8080
  Identity:  alice (ABCD1234)

✓ Server configured: http://localhost:8080
✓ Default org set: alice
```

Your `~/.atomic/config.toml` now contains:

```toml
[server]
url = "http://localhost:8080"
default_org = "alice"
```

### Full Localhost Walkthrough

This walkthrough demonstrates the complete team collaboration workflow
against a locally running `atomic-storage` server.

#### 1. Set up two identities (simulating two users)

```bash
# Alice (already registered above)
atomic identity whoami
# alice (ABCD1234)

# Create Bob's identity
atomic identity new bob --email bob@dev.local

# Show Bob's identity ID (you'll need this to add him as a member)
atomic identity show bob
# Name:       bob
# Email:      bob@dev.local
# ID:         EFGH5678...
# Public Key: MFRGGZDF...
```

#### 2. Register Bob with the server

```bash
# Register Bob (using his identity)
atomic identity register http://localhost:8080 --identity bob
# ✓ Registered as bob
#   URL: http://bob.localhost:8080
```

#### 3. Create a team organization (as Alice)

```bash
# Alice creates a team org
atomic org create "Dev Team" --email team@dev.local
# ✓ Created organization: dev-team
#   Slug:  dev-team
#   Kind:  team
#   Plan:  free

# Switch to the new org
atomic org set dev-team
```

#### 4. Add Bob to the organization

```bash
# Add Bob by email — the CLI resolves it to his server-side UUID automatically
atomic org member add bob@dev.local --role admin --org dev-team
# ✓ Added member to dev-team (role: admin)

# You can also add by identity name
atomic org member add bob --role admin --org dev-team
```

#### 5. Create a workspace and project

```bash
atomic workspace create "Backend Services" --org dev-team
# ✓ Created workspace: backend-services

atomic project create user-api --workspace backend-services --kind rust --org dev-team
# ✓ Created project: user-api
#   Workspace: backend-services
#   VCS URL:   http://dev-team.localhost:8080/workspaces/backend-services/projects/user-api/code
```

#### 6. Create a team and assign permissions

```bash
# Create a team
atomic team create "Backend Eng" --org dev-team
# ✓ Created team: backend-eng

# Add Bob to the team
atomic team member add backend-eng bob@dev.local --role maintainer --org dev-team
# ✓ Added member to team backend-eng (role: maintainer)
```

#### 7. Initialize a local repo and push

```bash
# In your project directory
cd ~/projects/user-api
atomic init --kind rust

# The project init command configures the remote automatically
atomic project init user-api --workspace backend-services --org dev-team
# ✓ Workspace 'backend-services' exists
# ✓ Created project 'user-api'
# ✓ Remote 'origin' configured

# Work normally
atomic add .
atomic record -m "Initial commit"
atomic push
```

#### 8. Bob clones and pushes (as Bob)

```bash
# Bob clones using the VCS URL
atomic clone http://bob@dev-team.localhost:8080/workspaces/backend-services/projects/user-api/code

cd user-api
echo "// Bob's change" >> src/main.rs
atomic add src/main.rs
atomic record -m "Bob's first change"
atomic push
```

#### Localhost URL patterns

| URL | Meaning |
|-----|---------|
| `http://localhost:8080` | Base server (no org context) |
| `http://alice.localhost:8080` | Alice's personal org |
| `http://dev-team.localhost:8080` | Team org "dev-team" |
| `http://bob@dev-team.localhost:8080/workspaces/ws/projects/proj/code` | Bob authenticating against dev-team |

> **Note:** `*.localhost` resolves to `127.0.0.1` on most modern operating
> systems. If yours doesn't, add entries to `/etc/hosts`:
> ```
> 127.0.0.1  alice.localhost
> 127.0.0.1  bob.localhost
> 127.0.0.1  dev-team.localhost
> ```

## Error Handling

All functions return `TeamsResult<T>`, which is `Result<T, TeamsError>`.
The error enum maps HTTP status codes to domain-specific variants:

| HTTP Status | TeamsError Variant | When |
|-------------|-------------------|------|
| 401 | `Remote(Unauthorized)` | Bearer token not recognized |
| 403 | `PermissionDenied` | Caller lacks the required ReBAC relation |
| 404 | `OrgNotFound` / `TeamNotFound` / `MemberNotFound` | Resource doesn't exist |
| 409 | `AlreadyExists` | Slug collision, duplicate member, etc. |
| 409 | `LastOwner` | Attempting to remove the last owner of an org |
| 5xx | `Remote(ServerError)` | Server-side failure |

```rust
use atomic_teams::{org, TeamsError};

async fn handle_errors(client: &atomic_remote::StorageClient) {
    match org::get_org(client, "nonexistent").await {
        Ok(info) => println!("Found: {}", info.name),
        Err(TeamsError::OrgNotFound(slug)) => {
            eprintln!("Org '{}' does not exist", slug);
        }
        Err(TeamsError::PermissionDenied(msg)) => {
            eprintln!("Access denied: {}", msg);
        }
        Err(TeamsError::Remote(e)) => {
            if e.is_retryable() {
                eprintln!("Transient error, retry: {}", e);
            } else {
                eprintln!("Fatal remote error: {}", e);
            }
        }
        Err(e) => eprintln!("Unexpected: {}", e),
    }
}
```

## Types Reference

### Enums

| Type | Variants | Used in |
|------|----------|---------|
| `OrgRole` | `Owner`, `Admin`, `Member` | Org membership |
| `TeamRole` | `Maintainer`, `Contributor`, `Collaborator`, `Consumer` | Team membership |
| `TeamVisibility` | `Visible`, `Secret` | Team creation/update |
| `GrantRelation` | `Read`, `Write`, `Admin`, `Owner` | Grant operations |
| `GrantSubjectType` | `User`, `Team` | Grant operations |

All enums implement `Display`, `FromStr`, `Serialize`, and `Deserialize`.
String representations are lowercase (`"owner"`, `"admin"`, `"maintainer"`, `"contributor"`, etc.).

### Structs

| Type | Key fields | Returned by |
|------|-----------|-------------|
| `OrgInfo` | `id`, `slug`, `name`, `email`, `kind`, `plan`, `created_at` | `org::*` |
| `OrgMemberInfo` | `org_id`, `identity_id`, `role`, `joined_at`, `invited_by` | `member::*` |
| `TeamInfo` | `id`, `org_id`, `slug`, `name`, `description`, `visibility`, `created_at` | `team::*` |
| `TeamMemberInfo` | `team_id`, `identity_id`, `role`, `added_at`, `added_by` | `team_member::*` |
| `GrantInfo` | `id`, `subject_type`, `subject_id`, `relation`, `granted_by`, `granted_at` | `grant::*` |
| `DomainAliasInfo` | `id`, `org_id`, `domain`, `status`, `verification_method`, `verification_token` | `domain::*` |

All structs use `#[serde(rename_all = "camelCase")]` to match the server's
JSON format.

## Feature Flag

In `atomic-cli/Cargo.toml`:

```toml
[features]
default = ["ast", "teams"]
teams = ["atomic-teams"]
```

The `teams` feature controls:

- **CLI**: `atomic org` and `atomic team` commands appear/disappear
- **Dependency**: `atomic-teams` crate is included/excluded
- **Always available**: `atomic workspace` and `atomic project` commands
  (these use `StorageClient` from `atomic-remote` directly)

To build without team features:

```bash
cargo build -p atomic-cli --no-default-features
```

To verify the feature gate works:

```bash
# With teams (default)
cargo run -p atomic-cli -- --help | grep -E "org|team"
#   org        Manage organizations and members
#   team       Manage teams within an organization

# Without teams
cargo run -p atomic-cli --no-default-features -- --help | grep -E "org|team"
# (no output)
```

## License

Dual-licensed under MIT and Apache 2.0, same as the Atomic project.