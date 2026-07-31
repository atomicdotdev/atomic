//! Team collaboration features for Atomic VCS.
//!
//! This crate provides domain logic for organization management, team
//! membership, permission grants, and domain aliases. It communicates with
//! the Atomic Storage API through the [`StorageClient`](atomic_remote::storage::StorageClient)
//! type provided by `atomic-remote`.
//!
//! # Modules
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`error`] | Error types and result alias |
//! | [`types`] | API response / request types and enums |
//! | [`org`] | Organization CRUD |
//! | [`member`] | Organization member management |
//! | [`team`] | Team CRUD |
//! | [`team_member`] | Team member management |
//! | [`grant`] | Permission grants on orgs and workspaces |
//! | [`domain`] | Domain alias claiming and verification |
//!
//! # Example
//!
//! ```ignore
//! use atomic_remote::storage::StorageClient;
//! use atomic_teams::org;
//!
//! async fn demo(client: &StorageClient) -> atomic_teams::TeamsResult<()> {
//!     let org = org::create_org(client, "Acme Corp", Some("admin@acme.com")).await?;
//!     println!("Created org: {} ({})", org.name, org.slug);
//!     Ok(())
//! }
//! ```

pub mod domain;
pub mod error;
pub mod grant;
pub mod member;
pub mod org;
pub mod team;
pub mod team_member;
pub mod types;

// Re-export key types for convenience.

pub use error::{TeamsError, TeamsResult};
pub use types::{
    DomainAliasInfo, GrantInfo, GrantRelation, GrantSubjectType, MyOrgInfo, OrgInfo, OrgMemberInfo,
    OrgRole, TeamInfo, TeamMemberInfo, TeamRole, TeamVisibility,
};
