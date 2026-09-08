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

/// Environment variable carrying an encoded grant.
///
/// The delivery mechanism for anywhere there is no interactive step and no
/// config to write: CI runners, containers, a sandbox handed a fresh short-lived
/// grant each session. Set it and the agent presents that certificate, with
/// nothing else to install.
///
/// It takes precedence over the store, because a caller that set it meant it —
/// and because the common shape is a runner given a grant minted seconds ago
/// while the store may hold something older.
pub const DELEGATION_ENV: &str = "ATOMIC_DELEGATION";

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
    // A grant handed over out of band wins. It is the mechanism for machines
    // with no store to populate, and the freshest thing the caller has.
    if let Some(resolved) = from_environment(identity)? {
        return Ok(resolved);
    }

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

/// A grant supplied through [`DELEGATION_ENV`], verified and checked to belong
/// to this agent.
///
/// Returns `Err` rather than `Ok(None)` when the variable is set but unusable.
/// Falling back to the store there would be worse than failing: the operator
/// asked for a specific grant, and silently using a different one is how you
/// get an agent acting under a scope nobody intended.
fn from_environment(identity: &Identity) -> CliResult<Option<ResolvedDelegation>> {
    let Ok(encoded) = std::env::var(DELEGATION_ENV) else {
        return Ok(None);
    };
    let encoded = encoded.trim();
    if encoded.is_empty() {
        return Ok(None);
    }

    let document = cert::decode_from_transport(encoded).map_err(|e| CliError::DelegationError {
        message: format!("{DELEGATION_ENV} is not a usable grant: {e}"),
    })?;

    let delegation =
        cert::verify_self_contained(&document).map_err(|e| CliError::DelegationError {
            message: format!("The grant in {DELEGATION_ENV} does not verify: {e}"),
        })?;

    if delegation.delegate != identity.id.to_did() {
        // Compare DIDs, and say so. Display names are not unique — two agents
        // called `alice+claude` on different machines are different keys, and
        // an error reading "issued to 'alice+claude', not 'alice+claude'" tells
        // the reader nothing.
        return Err(CliError::DelegationError {
            message: format!(
                "The grant in {DELEGATION_ENV} was issued to a different key.\n  \
                 Grant is for  {} ({})\n  \
                 Running as    {} ({})",
                delegation.delegate_name,
                delegation.delegate,
                identity.name,
                identity.id.to_did()
            ),
        });
    }

    if delegation.is_expired() {
        return Err(CliError::DelegationError {
            message: format!(
                "The grant in {DELEGATION_ENV} expired {}. Ask for a fresh one.",
                delegation
                    .expires
                    .map(|e| e.format("on %Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "some time ago".to_string())
            ),
        });
    }

    Ok(Some(ResolvedDelegation {
        delegation,
        document,
        status: DelegationStatus::Active,
    }))
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

    /// The automation path: a runner is handed a grant in the environment and
    /// needs nothing installed.
    #[test]
    #[serial_test::serial]
    fn a_grant_in_the_environment_is_used() {
        let f = fixture();
        let terms = Delegation::new(&f.human, &f.agent, scope_for("https://atomic.storage"))
            .expires_in(Duration::days(1));
        let doc = cert::mint(&f.human, &f.human_key, &terms);

        std::env::set_var(DELEGATION_ENV, cert::encode_for_transport(&doc));
        let found = active_for(&f.store, &f.agent, Some("https://atomic.storage"));
        std::env::remove_var(DELEGATION_ENV);

        let found = found.unwrap();
        assert_eq!(found.delegation.id, terms.id);
        // Nothing was ever written to the store.
        assert!(f.store.list_delegations().unwrap().is_empty());
    }

    /// A grant issued to someone else must not be usable just because it is in
    /// the environment.
    #[test]
    #[serial_test::serial]
    fn a_grant_for_another_agent_is_refused() {
        let f = fixture();
        let other = Identity::builder("alice+gemini")
            .identity_type(IdentityType::Agent)
            .delegated_by(f.human.id)
            .build()
            .unwrap();
        let terms = Delegation::new(&f.human, &other, DelegationScope::full());
        let doc = cert::mint(&f.human, &f.human_key, &terms);

        std::env::set_var(DELEGATION_ENV, cert::encode_for_transport(&doc));
        let result = active_for(&f.store, &f.agent, None);
        std::env::remove_var(DELEGATION_ENV);

        let err = result.unwrap_err();
        assert!(err.to_string().contains("was issued to"), "{err}");
    }

    /// A set-but-broken variable must fail loudly rather than falling back to
    /// the store — silently using a different grant than the one asked for is
    /// how an agent ends up with a scope nobody intended.
    #[test]
    #[serial_test::serial]
    fn a_broken_environment_grant_does_not_fall_back() {
        let f = fixture();
        issue(&f, scope_for("https://atomic.storage"), Duration::days(30));

        std::env::set_var(DELEGATION_ENV, "not-a-grant");
        let result = active_for(&f.store, &f.agent, Some("https://atomic.storage"));
        std::env::remove_var(DELEGATION_ENV);

        assert!(
            result.is_err(),
            "should not have silently used the stored grant"
        );
    }

    /// An empty variable is treated as unset, so `ATOMIC_DELEGATION=` in a
    /// shell profile does not break an otherwise working setup.
    #[test]
    #[serial_test::serial]
    fn an_empty_environment_variable_is_ignored() {
        let f = fixture();
        issue(&f, scope_for("https://atomic.storage"), Duration::days(30));

        std::env::set_var(DELEGATION_ENV, "");
        let result = active_for(&f.store, &f.agent, Some("https://atomic.storage"));
        std::env::remove_var(DELEGATION_ENV);

        assert!(result.is_ok());
    }

    #[test]
    #[serial_test::serial]
    fn finds_the_certificate_for_this_agent() {
        let f = fixture();
        issue(&f, scope_for("https://atomic.storage"), Duration::days(30));

        let found = active_for(&f.store, &f.agent, Some("https://atomic.storage")).unwrap();
        assert_eq!(found.delegation.delegate_name, "alice+claude");
        assert!(found.is_usable());
    }

    #[test]
    #[serial_test::serial]
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
    #[serial_test::serial]
    fn no_certificate_says_how_to_issue_one() {
        let f = fixture();
        let err = active_for(&f.store, &f.agent, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no delegation certificate"), "{msg}");
        assert!(msg.contains("atomic identity delegate"), "{msg}");
    }

    #[test]
    #[serial_test::serial]
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
    #[serial_test::serial]
    fn a_locally_revoked_certificate_is_refused() {
        let f = fixture();
        let id = issue(&f, scope_for("https://atomic.storage"), Duration::days(30));
        f.store.save_revocation(&id, "{}").unwrap();

        let err = active_for(&f.store, &f.agent, Some("https://atomic.storage")).unwrap_err();
        assert!(err.to_string().contains("revoked"), "{err}");
    }

    #[test]
    #[serial_test::serial]
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
    #[serial_test::serial]
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
    #[serial_test::serial]
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
    #[serial_test::serial]
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
    #[serial_test::serial]
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
