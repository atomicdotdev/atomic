//! Helper functions for the push command.
//!
//! # Manifest Sync Model
//!
//! Push syncs views by **identity**, not by flattening. Each view in the
//! local parent chain (root → leaf) is exported as a [`ViewManifest`] —
//! name, scope, parent, ordered change log, merkle state — and declared on
//! the remote:
//!
//! 1. Fetch the remote's manifest for the view (`?view-manifest=<name>`).
//! 2. Require the remote log to be a **prefix** of the local log
//!    (fast-forward rule); otherwise the histories have diverged.
//! 3. Store the suffix's change files (`?store=<hash>`, content-only,
//!    idempotent, no view application).
//! 4. Declare the local manifest (`POST ?view-manifest=<name>`); the server
//!    creates/fast-forwards the view and verifies the merkle state.
//!
//! The pure planning pieces live here so they can be unit tested without a
//! repository or network: chain construction ([`build_view_chain`]), suffix
//! computation and divergence detection ([`plan_view_sync`]), and the
//! old-server hard error ([`require_manifest_support`]).

use std::collections::HashSet;

use bytes::Bytes;

use atomic_core::types::{Base32, Hash};
use atomic_remote::RemoteError;
use atomic_repository::{Repository, ViewManifest};

use crate::error::{CliError, CliResult};
use crate::output::{info, view as style_view};

// View Chain Construction

/// Errors from walking a view's parent chain.
#[derive(Debug)]
pub enum ChainError<E> {
    /// The parent chain loops back on itself at this view.
    Cycle { view: String },

    /// Looking up a view's parent failed.
    Lookup(E),
}

/// Build the ancestor chain for a view, ordered root → leaf.
///
/// Walks `parent_of` from the leaf upward, guarding against cycles with a
/// visited set. A root view (no parent) yields a chain of length 1.
///
/// `parent_of` is injected so the traversal is pure and unit-testable; the
/// command wires it to `Repository::get_view_info(...).parent_name`.
///
/// # Example
///
/// ```rust,ignore
/// // draft `orange` parented on `dev` → ["dev", "orange"]
/// let chain = build_view_chain("orange", |name| {
///     repo.get_view_info(name).map(|info| info.parent_name)
/// })?;
/// ```
pub fn build_view_chain<E>(
    leaf: &str,
    mut parent_of: impl FnMut(&str) -> Result<Option<String>, E>,
) -> Result<Vec<String>, ChainError<E>> {
    let mut chain = vec![leaf.to_string()];
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(leaf.to_string());

    let mut current = leaf.to_string();
    while let Some(parent) = parent_of(&current).map_err(ChainError::Lookup)? {
        if !visited.insert(parent.clone()) {
            return Err(ChainError::Cycle { view: parent });
        }
        chain.push(parent.clone());
        current = parent;
    }

    chain.reverse();
    Ok(chain)
}

// Sync Planning

/// Why a view cannot be fast-forwarded on the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewSyncConflict {
    /// The remote log is not a prefix of the local log.
    Diverged {
        /// Index of the first differing entry, or `None` if the remote log
        /// is simply longer than the local log.
        first_mismatch: Option<usize>,
        local_len: usize,
        remote_len: usize,
    },

    /// The view exists on both sides with different identity (scope or
    /// parent). The server would reject the declare with a 409; catching it
    /// client-side gives a clearer message.
    IdentityMismatch {
        field: &'static str,
        local: String,
        remote: String,
    },
}

impl std::fmt::Display for ViewSyncConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Diverged {
                first_mismatch,
                local_len,
                remote_len,
            } => match first_mismatch {
                Some(i) => write!(
                    f,
                    "remote log ({} changes) is not a prefix of local log ({} changes): \
                     first mismatch at position {}",
                    remote_len, local_len, i
                ),
                None => write!(
                    f,
                    "remote log ({} changes) is longer than local log ({} changes)",
                    remote_len, local_len
                ),
            },
            Self::IdentityMismatch {
                field,
                local,
                remote,
            } => write!(
                f,
                "view {} differs: local '{}' vs remote '{}'",
                field, local, remote
            ),
        }
    }
}

/// Plan for syncing one view to the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewSyncPlan {
    /// Changes to store on the remote, in log order. For a fast-forward
    /// this is the local log beyond the remote prefix; for a forced push
    /// against a diverged remote it is every local-log hash the remote
    /// view lacks.
    pub suffix: Vec<Hash>,

    /// Whether the manifest needs to be declared (PUT). False only when
    /// local and remote logs are already identical.
    pub declare: bool,

    /// True when divergence was overridden with `--force`. The server is
    /// still authoritative and may reject the declare.
    pub forced: bool,
}

impl ViewSyncPlan {
    /// True when neither storing nor declaring is needed.
    pub fn is_noop(&self) -> bool {
        self.suffix.is_empty() && !self.declare
    }
}

/// Compute the sync plan for one view: what to store and whether to declare.
///
/// `remote` is the remote's parsed manifest, or `None` if the view does not
/// exist on the remote (the remote log is empty).
///
/// The fast-forward rule: the remote log must be a prefix of the local log.
/// The suffix (everything beyond the prefix) is what needs storing — its
/// dependencies are either earlier in the log or already on the remote by
/// induction over the root→leaf chain.
///
/// With `force`, a diverged remote does not fail the plan: the suffix
/// becomes the set difference (local-log hashes the remote view lacks) and
/// the declare is attempted anyway, leaving the server as the authority.
/// Identity mismatches (scope/parent) are never forced — they are
/// structural, and the server would reject them regardless.
pub fn plan_view_sync(
    local: &ViewManifest,
    remote: Option<&ViewManifest>,
    force: bool,
) -> Result<ViewSyncPlan, ViewSyncConflict> {
    let remote = match remote {
        None => {
            // View absent on remote: the whole log is the suffix.
            return Ok(ViewSyncPlan {
                suffix: local.changes.clone(),
                declare: true,
                forced: false,
            });
        }
        Some(r) => r,
    };

    // Identity must match: the manifest declares scope and parent, and the
    // server refuses to mutate an existing view's identity.
    if remote.scope != local.scope {
        return Err(ViewSyncConflict::IdentityMismatch {
            field: "scope",
            local: local.scope.to_string(),
            remote: remote.scope.to_string(),
        });
    }
    if remote.parent != local.parent {
        let show = |p: &Option<String>| p.clone().unwrap_or_else(|| "(none)".to_string());
        return Err(ViewSyncConflict::IdentityMismatch {
            field: "parent",
            local: show(&local.parent),
            remote: show(&remote.parent),
        });
    }

    // Prefix rule.
    let diverged = if remote.changes.len() > local.changes.len() {
        Some(ViewSyncConflict::Diverged {
            first_mismatch: None,
            local_len: local.changes.len(),
            remote_len: remote.changes.len(),
        })
    } else {
        remote
            .changes
            .iter()
            .zip(local.changes.iter())
            .position(|(r, l)| r != l)
            .map(|i| ViewSyncConflict::Diverged {
                first_mismatch: Some(i),
                local_len: local.changes.len(),
                remote_len: remote.changes.len(),
            })
    };

    if let Some(conflict) = diverged {
        if !force {
            return Err(conflict);
        }
        // Forced: store whatever the remote view lacks and attempt the
        // declare anyway. The server remains authoritative.
        let remote_set: HashSet<&Hash> = remote.changes.iter().collect();
        let suffix: Vec<Hash> = local
            .changes
            .iter()
            .filter(|h| !remote_set.contains(h))
            .copied()
            .collect();
        return Ok(ViewSyncPlan {
            suffix,
            declare: true,
            forced: true,
        });
    }

    // Remote is a (possibly complete) prefix of local.
    let suffix: Vec<Hash> = local.changes[remote.changes.len()..].to_vec();
    let declare = !suffix.is_empty();
    Ok(ViewSyncPlan {
        suffix,
        declare,
        forced: false,
    })
}

// Manifest Support Detection

/// Interpret the result of `get_view_manifest`, turning "server predates
/// manifest support" into a hard, actionable error.
///
/// Identity-preserving push has no fallback: this client no longer flattens
/// views, so a server without `?view-manifest` support cannot be pushed to.
pub fn require_manifest_support(
    result: Result<Option<String>, RemoteError>,
    url: &str,
) -> CliResult<Option<String>> {
    match result {
        Ok(text) => Ok(text),
        Err(RemoteError::ProtocolError { message }) => Err(CliError::RemoteError {
            message: format!(
                "The server does not support view manifests ({}). \
                 Identity-preserving push requires a server upgrade; \
                 this client no longer flattens views on push.",
                message
            ),
            url: Some(url.to_string()),
        }),
        Err(e) => Err(convert_remote_error(e, url)),
    }
}

/// Parse and verify a remote manifest, mapping failures to remote errors.
///
/// The declared state must equal the fold of the log — a remote that sends
/// an inconsistent manifest is broken, and we refuse to plan against it.
pub fn parse_remote_manifest(view: &str, text: &str, url: &str) -> CliResult<ViewManifest> {
    let manifest = ViewManifest::parse(text).map_err(|e| CliError::RemoteError {
        message: format!("Invalid manifest for view '{}' from remote: {}", view, e),
        url: Some(url.to_string()),
    })?;
    manifest.verify().map_err(|e| CliError::RemoteError {
        message: format!("Corrupt manifest for view '{}' from remote: {}", view, e),
        url: Some(url.to_string()),
    })?;
    Ok(manifest)
}

// Change Data Loading

/// Load and serialize change data from the repository.
///
/// Loads the change from the repository and serializes it to bytes
/// suitable for uploading to a remote.
///
/// # Errors
///
/// Returns `CliError::ChangeNotFound` if the change doesn't exist,
/// or `CliError::Internal` if serialization fails.
pub fn load_change_data(repo: &Repository, hash: &Hash) -> CliResult<Bytes> {
    // Read the raw V3 change file from disk instead of deserializing and
    // re-serializing.  This is faster, uses less memory, and — critically —
    // preserves the exact bytes that produced the content hash.  Re-serializing
    // can produce different bytes (field ordering, padding) which would break
    // hash verification on the server.
    let change_path = repo.change_store().change_path(hash);
    if change_path.exists() {
        let data = std::fs::read(&change_path).map_err(|e| {
            CliError::Internal(anyhow::anyhow!(
                "Failed to read change file {:?}: {}",
                change_path,
                e
            ))
        })?;
        return Ok(Bytes::from(data));
    }

    // Fallback: deserialize + re-serialize (legacy path for changes
    // whose on-disk file was cleaned up or doesn't exist).
    let change = repo
        .load_change(hash)
        .map_err(|_| CliError::ChangeNotFound {
            hash: hash.to_base32(),
        })?;

    let mut buffer = Vec::new();
    change
        .serialize(&mut buffer)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to serialize change: {}", e)))?;

    Ok(Bytes::from(buffer))
}

/// Load the message from a change, returning None if it fails.
pub fn load_change_message(repo: &Repository, hash: &Hash) -> Option<String> {
    repo.load_change(hash)
        .ok()
        .map(|c| c.hashed.header.message.clone())
}

// Error Conversion

/// Build the user-facing message for a "not found" (HTTP 404) response during
/// a push.
///
/// The server deliberately masks private projects: it returns 404 to callers
/// who are not authenticated/authorized rather than revealing that the project
/// exists. Because a push is a write that always requires auth, a 404 almost
/// always means the caller's credentials are missing/expired or they lack push
/// access — not that the project/view is genuinely absent. The message spells
/// out both possibilities and points at the remedy.
///
/// `target` describes what was not found, e.g. `"project/view 'dev'"` or
/// `"the project"`.
fn not_found_push_message(target: &str) -> String {
    format!(
        "Remote returned 'not found' for {target}. This means either it \
         doesn't exist, or you're not authenticated/authorized to push to it. \
         Make sure your identity is registered with the server \
         (`atomic identity register <server-url>`) and that you have push access."
    )
}

/// Convert a remote error to a CLI error.
///
/// Maps the various remote error types to appropriate CLI error types
/// with user-friendly messages and suggestions.
///
/// # Arguments
///
/// * `err` - The remote error to convert
/// * `url` - The remote URL (for context in error messages)
///
/// # Returns
///
/// A `CliError` that can be displayed to the user.
pub fn convert_remote_error(err: RemoteError, url: &str) -> CliError {
    match err {
        RemoteError::ConnectionFailed { .. } => CliError::RemoteError {
            message: format!("Failed to connect: {}", err),
            url: Some(url.to_string()),
        },
        RemoteError::AuthenticationFailed { .. } => CliError::AuthenticationFailed {
            remote: url.to_string(),
        },
        // A 404 on a push is ambiguous. Private projects are deliberately
        // masked: the server returns "not found" to anyone who isn't
        // authenticated/authorized, so a missing project and a private one we
        // can't see are indistinguishable on the wire. A push is a *write* that
        // always requires auth, so the most likely cause is missing/expired
        // credentials — not a genuinely-absent project/view. Say so, and point
        // at the remedy, instead of the misleading "view not found".
        RemoteError::RepositoryNotFound { .. } => CliError::RemoteError {
            message: not_found_push_message("the project"),
            url: Some(url.to_string()),
        },
        RemoteError::ViewNotFound { view } => CliError::RemoteError {
            message: not_found_push_message(&format!("project/view '{}'", view)),
            url: Some(url.to_string()),
        },
        RemoteError::ChangeNotFound { hash } => CliError::ChangeNotFound { hash },
        RemoteError::MissingDependencies {
            count,
            missing_hashes,
        } => CliError::MissingDependency {
            change: "uploaded change".to_string(),
            dependency: if missing_hashes.is_empty() {
                format!("{} dependencies", count)
            } else {
                missing_hashes.join(", ")
            },
        },
        RemoteError::StateMismatch {
            remote_state,
            requested_state,
        } => CliError::Conflict {
            description: format!(
                "State mismatch: remote is at {}, requested {}",
                remote_state, requested_state
            ),
        },
        RemoteError::Timeout { seconds } => CliError::RemoteError {
            message: format!("Request timed out after {} seconds", seconds),
            url: Some(url.to_string()),
        },
        RemoteError::HttpError { status: 413, .. } => CliError::RemoteError {
            message: "Change too large for server (413 Payload Too Large). \
                 The server's body size limit is too small. \
                 Ask the server admin to increase MAX_BODY_SIZE_MB."
                .to_string(),
            url: Some(url.to_string()),
        },
        _ => CliError::RemoteError {
            message: err.to_string(),
            url: Some(url.to_string()),
        },
    }
}

// Display Helpers

/// Display a local vs remote manifest state comparison for one view.
///
/// Used when a view has diverged, so the user can see where each side is.
pub fn display_manifest_divergence(local_view: &str, local: &ViewManifest, remote: &ViewManifest) {
    println!(
        "  Local:  {} at {}",
        style_view(local_view),
        info(&format_manifest_state(local))
    );
    println!(
        "  Remote: {} at {}",
        style_view(&remote.name),
        info(&format_manifest_state(remote))
    );
}

/// Format a manifest's state for display.
fn format_manifest_state(manifest: &ViewManifest) -> String {
    if manifest.changes.is_empty() {
        "(empty)".to_string()
    } else {
        let state = manifest.state.to_base32();
        format!(
            "{} ({} changes)",
            &state[..12.min(state.len())],
            manifest.changes.len()
        )
    }
}

/// Format a count with singular/plural suffix.
///
/// # Example
///
/// ```rust,ignore
/// assert_eq!(format_count(1, "change"), "1 change");
/// assert_eq!(format_count(5, "change"), "5 changes");
/// ```
pub fn format_count(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("{} {}", count, singular)
    } else {
        format!("{} {}s", count, singular)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::pristine::ViewScope;
    use std::collections::HashMap;

    /// Deterministic test hash.
    fn h(n: u8) -> Hash {
        Hash::of(&[n])
    }

    /// Build a manifest with a computed (consistent) state.
    fn manifest(
        name: &str,
        scope: ViewScope,
        parent: Option<&str>,
        changes: Vec<Hash>,
    ) -> ViewManifest {
        ViewManifest::new(name, scope, parent.map(str::to_string), changes)
    }

    /// Parent lookup backed by a map, for pure chain tests.
    fn parents(
        pairs: &[(&str, Option<&str>)],
    ) -> impl FnMut(&str) -> Result<Option<String>, String> {
        let map: HashMap<String, Option<String>> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
            .collect();
        move |name: &str| {
            map.get(name)
                .cloned()
                .ok_or_else(|| format!("view '{}' not found", name))
        }
    }

    // Chain Construction Tests

    #[test]
    fn test_chain_root_view_is_length_one() {
        let chain = build_view_chain("main", parents(&[("main", None)])).unwrap();
        assert_eq!(chain, vec!["main"]);
    }

    #[test]
    fn test_chain_orders_root_to_leaf() {
        // orange → dev → main should come out as [main, dev, orange]
        let chain = build_view_chain(
            "orange",
            parents(&[
                ("orange", Some("dev")),
                ("dev", Some("main")),
                ("main", None),
            ]),
        )
        .unwrap();
        assert_eq!(chain, vec!["main", "dev", "orange"]);
    }

    #[test]
    fn test_chain_detects_cycle() {
        // a → b → a is a corrupt parent chain, not an infinite loop.
        let err =
            build_view_chain("a", parents(&[("a", Some("b")), ("b", Some("a"))])).unwrap_err();
        match err {
            ChainError::Cycle { view } => assert_eq!(view, "a"),
            other => panic!("Expected Cycle, got {:?}", other),
        }
    }

    #[test]
    fn test_chain_detects_self_cycle() {
        let err = build_view_chain("a", parents(&[("a", Some("a"))])).unwrap_err();
        assert!(matches!(err, ChainError::Cycle { view } if view == "a"));
    }

    #[test]
    fn test_chain_propagates_lookup_error() {
        // Parent references a view that cannot be resolved.
        let err = build_view_chain("a", parents(&[("a", Some("ghost"))])).unwrap_err();
        assert!(matches!(err, ChainError::Lookup(msg) if msg.contains("ghost")));
    }

    // Sync Planning Tests

    #[test]
    fn test_plan_absent_remote_uploads_full_log() {
        let local = manifest("dev", ViewScope::Shared, None, vec![h(1), h(2), h(3)]);
        let plan = plan_view_sync(&local, None, false).unwrap();

        assert_eq!(plan.suffix, vec![h(1), h(2), h(3)]);
        assert!(plan.declare);
        assert!(!plan.forced);
    }

    #[test]
    fn test_plan_remote_prefix_uploads_suffix_only() {
        let local = manifest("dev", ViewScope::Shared, None, vec![h(1), h(2), h(3), h(4)]);
        let remote = manifest("dev", ViewScope::Shared, None, vec![h(1), h(2)]);
        let plan = plan_view_sync(&local, Some(&remote), false).unwrap();

        assert_eq!(plan.suffix, vec![h(3), h(4)]);
        assert!(plan.declare);
        assert!(!plan.forced);
    }

    #[test]
    fn test_plan_identical_logs_is_noop() {
        let local = manifest("dev", ViewScope::Shared, None, vec![h(1), h(2)]);
        let remote = manifest("dev", ViewScope::Shared, None, vec![h(1), h(2)]);
        let plan = plan_view_sync(&local, Some(&remote), false).unwrap();

        assert!(plan.suffix.is_empty());
        assert!(!plan.declare);
        assert!(plan.is_noop());
    }

    #[test]
    fn test_plan_empty_remote_view_uploads_full_log() {
        // View exists on the remote but its log is empty (freshly declared).
        let local = manifest("dev", ViewScope::Shared, None, vec![h(1)]);
        let remote = manifest("dev", ViewScope::Shared, None, vec![]);
        let plan = plan_view_sync(&local, Some(&remote), false).unwrap();

        assert_eq!(plan.suffix, vec![h(1)]);
        assert!(plan.declare);
    }

    #[test]
    fn test_plan_diverged_hash_mismatch() {
        let local = manifest("dev", ViewScope::Shared, None, vec![h(1), h(2), h(3)]);
        let remote = manifest("dev", ViewScope::Shared, None, vec![h(1), h(9)]);
        let err = plan_view_sync(&local, Some(&remote), false).unwrap_err();

        assert_eq!(
            err,
            ViewSyncConflict::Diverged {
                first_mismatch: Some(1),
                local_len: 3,
                remote_len: 2,
            }
        );
    }

    #[test]
    fn test_plan_diverged_remote_longer() {
        // Remote is ahead of local: not a prefix, so pull first.
        let local = manifest("dev", ViewScope::Shared, None, vec![h(1)]);
        let remote = manifest("dev", ViewScope::Shared, None, vec![h(1), h(2)]);
        let err = plan_view_sync(&local, Some(&remote), false).unwrap_err();

        assert_eq!(
            err,
            ViewSyncConflict::Diverged {
                first_mismatch: None,
                local_len: 1,
                remote_len: 2,
            }
        );
    }

    #[test]
    fn test_plan_forced_diverged_stores_set_difference() {
        let local = manifest("dev", ViewScope::Shared, None, vec![h(1), h(2), h(3)]);
        let remote = manifest("dev", ViewScope::Shared, None, vec![h(1), h(9)]);
        let plan = plan_view_sync(&local, Some(&remote), true).unwrap();

        // h(1) is already on the remote view; h(2), h(3) are not.
        assert_eq!(plan.suffix, vec![h(2), h(3)]);
        assert!(plan.declare);
        assert!(plan.forced);
    }

    #[test]
    fn test_plan_scope_mismatch_is_conflict_even_forced() {
        let local = manifest("x", ViewScope::Draft, Some("dev"), vec![h(1)]);
        let remote = manifest("x", ViewScope::Shared, Some("dev"), vec![h(1)]);

        let err = plan_view_sync(&local, Some(&remote), false).unwrap_err();
        assert!(matches!(
            err,
            ViewSyncConflict::IdentityMismatch { field: "scope", .. }
        ));

        // Identity mismatches are structural — force does not bypass them.
        let err = plan_view_sync(&local, Some(&remote), true).unwrap_err();
        assert!(matches!(
            err,
            ViewSyncConflict::IdentityMismatch { field: "scope", .. }
        ));
    }

    #[test]
    fn test_plan_parent_mismatch_is_conflict() {
        let local = manifest("x", ViewScope::Draft, Some("dev"), vec![h(1)]);
        let remote = manifest("x", ViewScope::Draft, Some("release"), vec![h(1)]);
        let err = plan_view_sync(&local, Some(&remote), false).unwrap_err();

        match err {
            ViewSyncConflict::IdentityMismatch {
                field,
                local,
                remote,
            } => {
                assert_eq!(field, "parent");
                assert_eq!(local, "dev");
                assert_eq!(remote, "release");
            }
            other => panic!("Expected IdentityMismatch, got {:?}", other),
        }
    }

    #[test]
    fn test_conflict_display_is_actionable() {
        let diverged = ViewSyncConflict::Diverged {
            first_mismatch: Some(2),
            local_len: 5,
            remote_len: 4,
        };
        let msg = diverged.to_string();
        assert!(msg.contains("not a prefix"));
        assert!(msg.contains("position 2"));

        let longer = ViewSyncConflict::Diverged {
            first_mismatch: None,
            local_len: 1,
            remote_len: 3,
        };
        assert!(longer.to_string().contains("longer than local"));
    }

    // Old-Server Hard Error Tests

    #[test]
    fn test_require_manifest_support_passes_through_manifest() {
        let text = "dev\tshared\t-\t-\n".to_string();
        let result = require_manifest_support(Ok(Some(text.clone())), "http://example.com");
        assert_eq!(result.unwrap(), Some(text));
    }

    #[test]
    fn test_require_manifest_support_passes_through_absent_view() {
        let result = require_manifest_support(Ok(None), "http://example.com");
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_require_manifest_support_hard_errors_on_old_server() {
        // A protocol error from get_view_manifest means the server predates
        // manifest support — there is no flatten fallback anymore.
        let err = require_manifest_support(
            Err(RemoteError::protocol(
                "server does not support view manifests (?view-manifest)",
            )),
            "http://example.com",
        )
        .unwrap_err();

        match err {
            CliError::RemoteError { message, url } => {
                assert!(message.contains("does not support view manifests"));
                assert!(message.contains("server upgrade"));
                assert!(message.contains("no longer flattens"));
                assert_eq!(url.as_deref(), Some("http://example.com"));
            }
            other => panic!("Expected RemoteError, got {:?}", other),
        }
    }

    #[test]
    fn test_require_manifest_support_converts_other_errors() {
        let err = require_manifest_support(
            Err(RemoteError::auth_failed("http://example.com", "nope")),
            "http://example.com",
        )
        .unwrap_err();
        assert!(matches!(err, CliError::AuthenticationFailed { .. }));
    }

    // Remote Manifest Parsing Tests

    #[test]
    fn test_parse_remote_manifest_round_trip() {
        let m = manifest("dev", ViewScope::Shared, None, vec![h(1), h(2)]);
        let parsed = parse_remote_manifest("dev", &m.to_text(), "http://example.com").unwrap();
        assert_eq!(parsed, m);
    }

    #[test]
    fn test_parse_remote_manifest_rejects_garbage() {
        let err = parse_remote_manifest("dev", "", "http://example.com").unwrap_err();
        assert!(matches!(err, CliError::RemoteError { .. }));
    }

    #[test]
    fn test_parse_remote_manifest_rejects_state_mismatch() {
        // Header declares a state that does not fold from the log.
        let mut m = manifest("dev", ViewScope::Shared, None, vec![h(1)]);
        m.state = atomic_core::types::Merkle::of(b"tampered");
        let err = parse_remote_manifest("dev", &m.to_text(), "http://example.com").unwrap_err();

        match err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("Corrupt manifest"));
            }
            other => panic!("Expected RemoteError, got {:?}", other),
        }
    }

    // Error Conversion Tests

    #[test]
    fn test_convert_connection_failed() {
        let err = RemoteError::connection_failed(
            "http://example.com",
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        );
        let cli_err = convert_remote_error(err, "http://example.com");

        assert!(matches!(cli_err, CliError::RemoteError { .. }));
    }

    #[test]
    fn test_convert_auth_failed() {
        let err = RemoteError::auth_failed("http://example.com", "invalid token");
        let cli_err = convert_remote_error(err, "http://example.com");

        assert!(matches!(cli_err, CliError::AuthenticationFailed { .. }));
    }

    #[test]
    fn test_convert_repo_not_found() {
        // A 404 for the repository/project on a push is ambiguous (the server
        // masks private projects), so the message must point at auth rather
        // than implying the project is simply absent.
        let err = RemoteError::repo_not_found("http://example.com/repo");
        let cli_err = convert_remote_error(err, "http://example.com/repo");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                let lower = message.to_lowercase();
                assert!(message.contains("not found"));
                // Mentions the authentication/authorization possibility.
                assert!(lower.contains("authenticated") || lower.contains("authorized"));
                // Points at the remedy.
                assert!(message.contains("atomic identity register"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    #[test]
    fn test_convert_view_not_found() {
        // The server returns 404 both for a genuinely-missing view and for a
        // private one the caller can't see. On a push (a write that needs
        // auth) the message must name the view *and* explain that this may be
        // an auth/authorization problem, with the remedy to run identity
        // register — not the misleading "view not found".
        let err = RemoteError::view_not_found("main");
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                let lower = message.to_lowercase();
                assert!(message.contains("main"));
                assert!(message.contains("not found"));
                assert!(lower.contains("authenticated") || lower.contains("authorized"));
                assert!(message.contains("atomic identity register"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    #[test]
    fn test_convert_missing_deps() {
        let err = RemoteError::missing_deps(vec!["ABC".to_string(), "DEF".to_string()]);
        let cli_err = convert_remote_error(err, "http://example.com");

        assert!(matches!(cli_err, CliError::MissingDependency { .. }));
    }

    #[test]
    fn test_convert_timeout() {
        let err = RemoteError::timeout(30);
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("timed out"));
                assert!(message.contains("30"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    // Format Count Tests

    #[test]
    fn test_format_count_zero() {
        assert_eq!(format_count(0, "change"), "0 changes");
    }

    #[test]
    fn test_format_count_one() {
        assert_eq!(format_count(1, "change"), "1 change");
    }

    #[test]
    fn test_format_count_many() {
        assert_eq!(format_count(5, "change"), "5 changes");
        assert_eq!(format_count(100, "file"), "100 files");
    }

    #[test]
    fn test_format_count_different_words() {
        assert_eq!(format_count(1, "tag"), "1 tag");
        assert_eq!(format_count(2, "tag"), "2 tags");
        assert_eq!(format_count(1, "warning"), "1 warning");
        assert_eq!(format_count(3, "warning"), "3 warnings");
    }

    // Display Helper Tests

    #[test]
    fn test_format_manifest_state_empty() {
        let m = manifest("dev", ViewScope::Shared, None, vec![]);
        assert_eq!(format_manifest_state(&m), "(empty)");
    }

    #[test]
    fn test_format_manifest_state_with_changes() {
        let m = manifest("dev", ViewScope::Shared, None, vec![h(1), h(2)]);
        let text = format_manifest_state(&m);
        assert!(text.contains("2 changes"));
        assert!(text.starts_with(&m.state.to_base32()[..12]));
    }
}
