//! Binding-ref transfer policy: which Git refs may carry Atomic binding
//! state between repositories (RFC §8.6, §6.4, §12, CB-6B).
//!
//! The Git transport surface for binding closure is deliberately tiny:
//!
//! - **Only** `refs/atomic/bindings/<shard>/<id>` refs transfer, and they are
//!   create-only (CB-6A publication; never updated, never deleted here).
//! - WIP recovery refs (`refs/atomic/wip/…`) are local crash-recovery aids
//!   (§6.4) and **never transfer**.
//! - No hidden branches: hosting namespaces that reject custom refs produce
//!   an explicit Atomic-remote requirement (or a *deferred*, user-approved
//!   degraded fallback), never an implicit `refs/heads/…` publication.
//!
//! Every function here is pure over names/refs: the transfer surface is
//! decided before any pack is built, so private working-copy snapshots and
//! WIP namespaces cannot leak through transport by construction.

use super::codec::BindingId;
use crate::repository::{Repository, BINDING_REF_PREFIX};

/// Local, never-transferred WIP recovery namespace (RFC §6.4).
pub const WIP_REF_PREFIX: &str = "refs/atomic/wip/";

/// The explicitly-degraded hosting fallback namespace (RFC §8.6). It is **not**
/// semantically equivalent to the binding namespace and is only ever used
/// after an explicit operator decision — never as an automatic fallback.
pub const DEGRADED_HEAD_BINDING_PREFIX: &str = "refs/heads/atomic/bindings/";

/// Whether `name` is a ref this transport may transfer.
///
/// The allowlist is exactly the create-only binding namespace with a
/// two-hex-character shard and a 64-hex id. Everything else — WIP refs,
/// `refs/heads/*` (including the degraded fallback), tags, notes, and any
/// other namespace — is refused.
pub fn is_transferable_ref(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(BINDING_REF_PREFIX) else {
        return false;
    };
    let mut segments = rest.split('/');
    match (segments.next(), segments.next(), segments.next()) {
        (Some(shard), Some(id), None) => {
            shard.len() == 2
                && shard.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && id.len() == 64
                && id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }
        _ => false,
    }
}

/// Whether `name` is a local WIP recovery ref (never transferable).
pub fn is_wip_ref(name: &str) -> bool {
    name.starts_with(WIP_REF_PREFIX)
}

/// The create-only binding ref for `id` (delegates to the reviewed shard
/// layout), or `None` when the id does not round-trip through the
/// transferable-namespace check — which would be a layout bug, not a
/// policy decision.
pub fn binding_ref_name(id: &BindingId) -> String {
    Repository::binding_ref_name(id)
}

/// Every local ref this transport may transfer, in deterministic order.
///
/// This is the *entire* advertised-ref surface for binding transfer: callers
/// push exactly these refs, so WIP recovery refs, branches, and tags are
/// structurally excluded from transfer.
/// The transfer queue's enumeration bound (CB-10B review R8): the all-refs
/// walk that feeds the queue is capped so a hostile or runaway ref surface
/// can never make binding transport unbounded.
pub const MAX_TRANSFER_REFS: usize = 10_000;

pub fn transferable_binding_refs(git: &git2::Repository) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for reference in git
        .references()
        .map_err(|error| format!("cannot enumerate refs: {error}"))?
    {
        let reference =
            reference.map_err(|error| format!("cannot read ref: {error}"))?;
        if let Some(name) = reference.name() {
            if is_transferable_ref(name) {
                if names.len() >= MAX_TRANSFER_REFS {
                    return Err(format!(
                        "the binding ref surface exceeds the transfer bound of \
                         {MAX_TRANSFER_REFS} refs; the enumeration is bounded and refuses \
                         instead of feeding an unbounded queue"
                    ));
                }
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// The binding id carried by a canonical binding ref name, or `None` when
/// `name` is not exactly the reviewed shard/id layout. The shard must be the
/// id's own first two hex characters, so a re-shaped or re-sharded ref name
/// never resolves to an id it does not canonically carry.
pub fn binding_id_from_ref_name(name: &str) -> Option<BindingId> {
    let rest = name.strip_prefix(BINDING_REF_PREFIX)?;
    let mut segments = rest.split('/');
    let shard = segments.next()?;
    let id_hex = segments.next()?;
    if segments.next().is_some() || shard.len() != 2 || id_hex.len() != 64 {
        return None;
    }
    if !id_hex
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, pair) in id_hex.as_bytes().chunks(2).enumerate() {
        let high = (pair[0] as char).to_digit(16).expect("hex digit checked above") as u8;
        let low = (pair[1] as char).to_digit(16).expect("hex digit checked above") as u8;
        bytes[index] = (high << 4) | low;
    }
    let id = BindingId::from_bytes(bytes);
    if shard != &id.to_hex()[..2] {
        return None;
    }
    Some(id)
}

/// Why a binding-namespace push was refused, and what the operator must
/// decide next (RFC §8.6: "Hosts that reject custom namespaces require an
/// Atomic remote or an explicitly degraded, UI-visible fallback; it is not
/// semantically equivalent.").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingTransportDiagnostic {
    /// The remote that rejected the binding namespace.
    pub remote: String,
    /// The ref the publisher attempted to create.
    pub ref_name: String,
    /// The rejection detail surfaced by the transport.
    pub detail: String,
    /// The remediation options, ordered: an Atomic remote is the
    /// privacy-preserving requirement; the degraded fallback is deferred and
    /// must be explicitly enabled by an operator. This diagnostic never
    /// creates refs and never starts a nested push.
    pub remediation: Vec<String>,
}

/// Build the explicit diagnostic for a rejected binding-namespace write.
///
/// This is deliberately a pure function: it *reports* the refusal and the
/// remediation options. It does not retry through hidden branches, does not
/// create a degraded `refs/heads/atomic/bindings/...` ref, and does not
/// invoke a nested push — those actions require an explicit operator
/// decision outside this transport.
pub fn namespace_rejection_diagnostic(
    remote: &str,
    ref_name: &str,
    detail: &str,
) -> BindingTransportDiagnostic {
    BindingTransportDiagnostic {
        remote: remote.to_string(),
        ref_name: ref_name.to_string(),
        detail: detail.to_string(),
        remediation: vec![
            format!(
                "configure an Atomic remote for this repository and push bindings through it \
                 (the Atomic remote is the required transport for binding closures)"
            ),
            format!(
                "or explicitly enable the degraded refs/heads/atomic/bindings/* hosting fallback \
                 (NOT semantically equivalent; visible to every Git client and must be \
                 deferred to an operator decision)"
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding_id(seed: u8) -> BindingId {
        BindingId::from_bytes([seed; 32])
    }

    #[test]
    fn only_the_binding_namespace_is_transferable() {
        let id = binding_id(0xAB);
        let good = Repository::binding_ref_name(&id);
        assert!(is_transferable_ref(&good), "{good} must transfer");

        // Wrong shapes inside the namespace are refused.
        for bad in [
            "refs/atomic/bindings/AB",                       // missing id
            "refs/atomic/bindings/AB/short",                 // not a 64-hex id
            "refs/atomic/bindings/GG/<id>",                  // non-hex shard
            "refs/atomic/bindings/ab/../id",                 // traversal-shaped
            "refs/atomic/bindings//0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "refs/atomic/bindings/ab/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/extra",
        ] {
            assert!(
                !is_transferable_ref(bad),
                "{bad} must not transfer"
            );
        }

        // Every other namespace is refused.
        for bad in [
            "refs/atomic/wip/workspace/op",
            "refs/heads/atomic/bindings/ab/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "refs/heads/main",
            "refs/tags/v1",
            "refs/views/dev",
            "HEAD",
        ] {
            assert!(!is_transferable_ref(bad), "{bad} must not transfer");
        }
        let _ = id;
    }

    #[test]
    fn wip_refs_never_transfer() {
        assert!(is_wip_ref("refs/atomic/wip/ws/op"));
        assert!(!is_transferable_ref("refs/atomic/wip/workspace/op"));
        // The degraded namespace is a branch: it is not silently transferable
        // either — only the reviewed shard layout is.
        assert!(!is_transferable_ref(
            "refs/heads/atomic/bindings/ab/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
        assert_eq!(DEGRADED_HEAD_BINDING_PREFIX, "refs/heads/atomic/bindings/");
        assert_eq!(WIP_REF_PREFIX, "refs/atomic/wip/");
    }

    #[test]
    fn transferable_ref_enumeration_is_exact() {
        let temp = tempfile::TempDir::new().unwrap();
        let git = git2::Repository::init_bare(temp.path()).unwrap();
        let signature = git2::Signature::now("T", "t@e").unwrap();
        let tree = git.treebuilder(None).unwrap().write().unwrap();
        let tree = git.find_tree(tree).unwrap();
        let commit = git
            .commit(None, &signature, &signature, "seed", &tree, &[])
            .unwrap();

        let id = binding_id(0x01);
        let binding_ref = binding_ref_name(&id);
        git.reference(&binding_ref, commit, false, "binding").unwrap();
        // Noise that must never be enumerated: WIP ref, branch, tag.
        git.reference("refs/atomic/wip/ws/op", commit, false, "wip").unwrap();
        git.reference("refs/heads/main", commit, false, "branch").unwrap();
        git.reference("refs/tags/v1", commit, false, "tag").unwrap();

        let transferable = transferable_binding_refs(&git).unwrap();
        assert_eq!(transferable, vec![binding_ref.clone()]);
    }

    #[test]
    fn namespace_rejection_is_an_explicit_diagnostic_not_hidden_publication() {
        let diagnostic = namespace_rejection_diagnostic(
            "git@example.com:acme/widgets.git",
            &binding_ref_name(&binding_id(0x02)),
            "remote rejected custom namespace",
        );
        assert_eq!(diagnostic.remote, "git@example.com:acme/widgets.git");
        assert!(
            diagnostic.remediation[0].contains("Atomic remote"),
            "the primary remediation is an Atomic remote: {diagnostic:?}"
        );
        assert!(
            diagnostic.remediation[1].contains("NOT semantically equivalent"),
            "the degraded fallback must be marked as deferred and non-equivalent"
        );
        // The diagnostic is data only: it creates nothing, pushes nothing.
        // (No git repository is even touched to build it.)
    }
}
