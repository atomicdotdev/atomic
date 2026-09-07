//! Resolving which delegation authorizes an agent right now.
//!
//! An agent identity is only half the story — on its own it proves possession
//! of a key and nothing else. The certificate signed by the human is what says
//! the key is theirs and what it may do. Everything that acts as an agent
//! (minting a token, pre-flighting a push, listing agents) needs the same
//! question answered: *for this identity, against this server, which
//! certificate applies, and is it still good?*
//!
//! This module is that one answer, so the CLI cannot end up with a permissive
//! check on one path and a strict one on another.
//!
//! # What "still good" means locally
//!
//! Locally we can check three of the four things that matter — the proof
//! verifies, it has not expired, and it is in scope — plus a locally recorded
//! revocation. We cannot check a revocation issued from another machine; the
//! server is the authority there and re-checks on every request. So a local
//! pass means "worth sending", never "the server will accept it".

use atomic_canonical::delegation as cert;
use atomic_identity::delegation::{
    Delegation, DelegationPermission, DelegationStatus, ResourceRef,
};
use atomic_identity::{Identity, IdentityStore};
use serde_json::Value;

use crate::error::{CliError, CliResult};

/// A stored certificate together with everything the CLI knows about it.
#[derive(Debug, Clone)]
pub struct ResolvedDelegation {
    /// The verified, typed certificate.
    pub delegation: Delegation,
    /// The document as stored — the exact bytes the proof covers.
    pub document: Value,
    /// Status from local facts: expiry plus any locally recorded revocation.
    pub status: DelegationStatus,
}

impl ResolvedDelegation {
    /// Base32 id, the form used for filenames and API paths.
    pub fn id(&self) -> String {
        self.delegation.id.to_base32()
    }

    /// Is this usable, as far as this machine can tell?
    pub fn is_usable(&self) -> bool {
        self.status == DelegationStatus::Active
    }
}

/// Every stored certificate naming `identity` as the delegate, verified.
///
/// A thin adapter over [`atomic_canonical::delegation::load_for_delegate`] so
/// the CLI, the agent recording path, and anything else asking this question
/// get the same answer from the same code.
pub fn load_for_delegate(
    store: &IdentityStore,
    identity: &Identity,
) -> CliResult<Vec<ResolvedDelegation>> {
    let stored = cert::load_for_delegate(store, identity)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to load delegations: {e}")))?;

    Ok(stored
        .into_iter()
        .map(|s| {
            let status = if s.revoked_locally {
                DelegationStatus::Revoked
            } else {
                s.delegation.status()
            };
            ResolvedDelegation {
                delegation: s.delegation,
                document: s.document,
                status,
            }
        })
        .collect())
}

/// The certificate to use for `identity` against `server`.
///
/// Picks the most recently issued certificate that is active, unrevoked, and
/// scoped to that server. Fails with a message naming the command that fixes
/// it, because every reason this can fail is something the human who issued the
/// delegation can put right.
pub fn active_for(
    store: &IdentityStore,
    identity: &Identity,
    server: Option<&str>,
) -> CliResult<ResolvedDelegation> {
    let all = load_for_delegate(store, identity)?;

    if all.is_empty() {
        return Err(CliError::DelegationError {
            message: format!(
                "'{}' is an agent identity with no delegation certificate on this machine.\n  \
                 Issue one with:  atomic identity delegate {} --can read,record,push",
                identity.name, identity.name
            ),
        });
    }

    let resource = server
        .map(|s| ResourceRef::new().server(s))
        .unwrap_or_default();

    let usable = all.iter().find(|d| {
        d.is_usable()
            && d.delegation
                .scope
                .allows(DelegationPermission::Read, &resource)
    });

    if let Some(found) = usable {
        return Ok(found.clone());
    }

    // Nothing usable: say precisely why, using the freshest certificate as the
    // subject. Reporting "no delegation" when one is merely expired sends the
    // human looking for the wrong problem.
    let newest = &all[0];
    let message = match newest.status {
        DelegationStatus::Revoked => format!(
            "The delegation for '{}' has been revoked.\n  \
             Issue a new one with:  atomic identity delegate {} --can read,record,push",
            identity.name, identity.name
        ),
        DelegationStatus::Expired => format!(
            "The delegation for '{}' expired {}.\n  \
             Renew it with:  atomic identity agent renew {}",
            identity.name,
            newest
                .delegation
                .expires
                .map(|e| e.format("on %Y-%m-%d").to_string())
                .unwrap_or_else(|| "some time ago".to_string()),
            identity.name
        ),
        DelegationStatus::Active => match server {
            Some(server) => format!(
                "The delegation for '{}' is not valid against {server}.\n  \
                 It is scoped to: {}\n  \
                 Issue one for this server with:  atomic identity delegate {} --server {server}",
                identity.name,
                if newest.delegation.scope.servers.is_empty() {
                    "(no server — this should not happen)".to_string()
                } else {
                    newest.delegation.scope.servers.join(", ")
                },
                identity.name
            ),
            None => format!(
                "No usable delegation for '{}'.\n  \
                 Issue one with:  atomic identity delegate {}",
                identity.name, identity.name
            ),
        },
    };

    Err(CliError::DelegationError { message })
}

/// Pre-flight a specific operation before paying for a network round trip.
///
/// The server is the authority and re-checks everything; this exists so an
/// out-of-scope push fails with "your agent may not push to acme/api" instead
/// of an opaque 403 after the upload.
pub fn check_permission(
    resolved: &ResolvedDelegation,
    permission: DelegationPermission,
    resource: &ResourceRef<'_>,
    identity_name: &str,
) -> CliResult<()> {
    if resolved.delegation.allows(permission, resource) {
        return Ok(());
    }

    let granted = resolved
        .delegation
        .scope
        .permissions
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let mut message = format!(
        "Agent '{identity_name}' is not authorized to {permission} here.\n  \
         Granted: {granted}"
    );
    if let Some(project) = resource.project {
        if !resolved.delegation.scope.allows_project(project) {
            message.push_str(&format!(
                "\n  Scoped to projects: {}\n  Requested: {project}",
                if resolved.delegation.scope.projects.is_empty() {
                    "(all)".to_string()
                } else {
                    resolved.delegation.scope.projects.join(", ")
                }
            ));
        }
    }
    message.push_str(&format!(
        "\n  Widen it with:  atomic identity delegate {identity_name} --can {permission}"
    ));

    Err(CliError::DelegationError { message })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_identity::delegation::DelegationScope;
    use atomic_identity::{IdentityType, KeyPair};
    use chrono::Duration;
    use tempfile::TempDir;

    struct Fixture {
        _dir: TempDir,
        store: IdentityStore,
        human: Identity,
        human_key: KeyPair,
        agent: Identity,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let store = IdentityStore::open(dir.path()).unwrap();

        let human_key = KeyPair::generate();
        let human = Identity::new("alice", &human_key);

        let agent_key = KeyPair::generate();
        let agent = Identity::builder("alice+claude")
            .identity_type(IdentityType::Agent)
            .public_key(agent_key.public.clone())
            .delegated_by(human.id)
            .build()
            .unwrap();

        Fixture {
            _dir: dir,
            store,
            human,
            human_key,
            agent,
        }
    }

    fn issue(f: &Fixture, scope: DelegationScope, expires_in: Duration) -> String {
        let terms = Delegation::new(&f.human, &f.agent, scope).expires_in(expires_in);
        let doc = cert::mint(&f.human, &f.human_key, &terms);
        let id = terms.id.to_base32();
        f.store
            .save_delegation(&id, &serde_json::to_string_pretty(&doc).unwrap())
            .unwrap();
        id
    }

    fn scope_for(server: &str) -> DelegationScope {
        DelegationScope::builder()
            .permission(DelegationPermission::Read)
            .permission(DelegationPermission::Push)
            .server(server)
            .project("acme/*")
            .build()
    }

    #[test]
    fn finds_the_certificate_for_this_agent() {
        let f = fixture();
        issue(&f, scope_for("https://atomic.storage"), Duration::days(30));

        let found = active_for(&f.store, &f.agent, Some("https://atomic.storage")).unwrap();
        assert_eq!(found.delegation.delegate_name, "alice+claude");
        assert!(found.is_usable());
    }

    #[test]
    fn ignores_certificates_belonging_to_another_agent() {
        let f = fixture();
        issue(&f, scope_for("https://atomic.storage"), Duration::days(30));

        let other = Identity::builder("alice+gemini")
            .identity_type(IdentityType::Agent)
            .delegated_by(f.human.id)
            .build()
            .unwrap();

        assert!(load_for_delegate(&f.store, &other).unwrap().is_empty());
    }

    #[test]
    fn no_certificate_says_how_to_issue_one() {
        let f = fixture();
        let err = active_for(&f.store, &f.agent, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no delegation certificate"), "{msg}");
        assert!(msg.contains("atomic identity delegate"), "{msg}");
    }

    #[test]
    fn an_expired_certificate_says_renew_not_issue() {
        let f = fixture();
        // expires_in with a negative duration puts expiry in the past.
        issue(&f, scope_for("https://atomic.storage"), Duration::days(-1));

        let err = active_for(&f.store, &f.agent, Some("https://atomic.storage")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("expired"), "{msg}");
        assert!(msg.contains("agent renew"), "{msg}");
    }

    #[test]
    fn a_locally_revoked_certificate_is_refused() {
        let f = fixture();
        let id = issue(&f, scope_for("https://atomic.storage"), Duration::days(30));
        f.store.save_revocation(&id, "{}").unwrap();

        let err = active_for(&f.store, &f.agent, Some("https://atomic.storage")).unwrap_err();
        assert!(err.to_string().contains("revoked"), "{err}");
    }

    #[test]
    fn a_certificate_for_another_server_is_refused_with_the_scope_shown() {
        let f = fixture();
        issue(&f, scope_for("https://atomic.storage"), Duration::days(30));

        let err = active_for(&f.store, &f.agent, Some("https://staging.example")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not valid against https://staging.example"),
            "{msg}"
        );
        assert!(msg.contains("https://atomic.storage"), "{msg}");
    }

    #[test]
    fn a_tampered_certificate_is_skipped_not_trusted() {
        let f = fixture();
        let id = issue(&f, scope_for("https://atomic.storage"), Duration::days(30));

        // Widen the scope on disk, exactly what a compromised agent would try.
        let mut doc: Value = serde_json::from_str(&f.store.load_delegation(&id).unwrap()).unwrap();
        doc["scope"]["permissions"] = serde_json::json!(["full"]);
        f.store
            .save_delegation(&id, &serde_json::to_string(&doc).unwrap())
            .unwrap();

        assert!(load_for_delegate(&f.store, &f.agent).unwrap().is_empty());
        assert!(active_for(&f.store, &f.agent, None).is_err());
    }

    #[test]
    fn permission_check_names_the_missing_permission() {
        let f = fixture();
        issue(&f, scope_for("https://atomic.storage"), Duration::days(30));
        let resolved = active_for(&f.store, &f.agent, Some("https://atomic.storage")).unwrap();

        // Push is granted...
        check_permission(
            &resolved,
            DelegationPermission::Push,
            &ResourceRef::new().project("acme/api"),
            "alice+claude",
        )
        .unwrap();

        // ...admin is not.
        let err = check_permission(
            &resolved,
            DelegationPermission::Admin,
            &ResourceRef::new().project("acme/api"),
            "alice+claude",
        )
        .unwrap_err();
        assert!(err.to_string().contains("not authorized to admin"), "{err}");
    }

    #[test]
    fn permission_check_names_the_out_of_scope_project() {
        let f = fixture();
        issue(&f, scope_for("https://atomic.storage"), Duration::days(30));
        let resolved = active_for(&f.store, &f.agent, Some("https://atomic.storage")).unwrap();

        let err = check_permission(
            &resolved,
            DelegationPermission::Push,
            &ResourceRef::new().project("other/api"),
            "alice+claude",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("other/api"), "{msg}");
        assert!(msg.contains("acme/*"), "{msg}");
    }

    #[test]
    fn the_freshest_certificate_wins() {
        let f = fixture();
        issue(&f, DelegationScope::read_only(), Duration::days(30));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        issue(&f, scope_for("https://atomic.storage"), Duration::days(30));

        let found = active_for(&f.store, &f.agent, Some("https://atomic.storage")).unwrap();
        assert!(found
            .delegation
            .scope
            .has_permission(DelegationPermission::Push));
    }
}
