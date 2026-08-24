//! The view-snapshot object — the content-addressed representation of a view's
//! identity, lineage, and membership at one point in time.
//!
//! A [`ViewSnapshot`] is immutable and content-addressed by the Blake3 of its
//! canonical JSON. A `views/{name}` ref points at the current snapshot key and
//! is CAS-updated (fast-forward gated on `prev`) on each view-state change.
//!
//! # Membership is own-set + parent pointer
//!
//! A snapshot inlines only the view's **own** change set; the effective union is
//! composed on read by walking `parent_view` up the chain (see
//! [`ViewSnapshot::effective_changes`]). So a draft off a 100k-change `dev`
//! stays tiny — the big set lives only in the shared root it points at. This
//! mirrors Atomic's `VIEW_CHANGES[this] + parent` model exactly.

use serde::{Deserialize, Serialize};

/// The two view scopes, as they appear in the object's `scope` string. Kept as
/// a light label here (the authoritative `ViewScope` enum lives in
/// `atomic-core`) so this crate stays engine-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewScopeLabel {
    /// A collaborative root/shared view. Serialized as `"shared"`.
    Shared,
    /// A personal draft view parented on another view. Serialized as `"draft"`.
    Draft,
}

impl ViewScopeLabel {
    /// The canonical string form stored in the object.
    pub fn as_str(self) -> &'static str {
        match self {
            ViewScopeLabel::Shared => "shared",
            ViewScopeLabel::Draft => "draft",
        }
    }

    /// Parse from the object's `scope` string. Unknown values are treated as
    /// `Shared` — the safe default (a root that inherits nothing).
    pub fn parse(s: &str) -> Self {
        if s == "draft" {
            ViewScopeLabel::Draft
        } else {
            ViewScopeLabel::Shared
        }
    }
}

/// An immutable, content-addressed snapshot of a view's state.
///
/// Serialized as canonical JSON; content address = Blake3 of the canonical
/// bytes (see [`ViewSnapshot::content_key`]). Field order is fixed by the
/// struct definition and every field is deterministic, so two repositories
/// holding the same view state mint byte-identical objects with identical keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSnapshot {
    /// View scope: `"shared"` or `"draft"`.
    pub scope: String,
    /// Parent view **name**, or `None` for a root view. Inherited membership is
    /// resolved against the parent's *current* head at read time (live by-name,
    /// decision D6), preserving Atomic's inheritance semantics.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_view: Option<String>,
    /// Predecessor snapshot key(s) (hex Blake3). Usually one; empty for genesis;
    /// multiple only for a view merge. Makes the view's history a content-addressed
    /// DAG — fast-forward vs divergence is decided by walking `prev`, not by
    /// log-shape heuristics.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub prev: Vec<String>,
    /// The view's **own** change hashes (base32), in log order. For a draft
    /// this is just the draft's own changes; for a shared root it is the full
    /// change log. The effective union is composed on read via `parent_view`.
    pub own_changes: Vec<String>,
    /// Order-invariant `SetId` of `own_changes` (base32), computed by the
    /// producer (which has the engine). Carried opaquely here so a reader can
    /// do an O(1) "same own-set?" check by string equality without the engine.
    pub own_set_id: String,
    /// The view's merkle state (base32) — the fold of the change log. `None`
    /// for an empty view.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub merkle_state: Option<String>,
}

impl ViewSnapshot {
    /// Construct a snapshot from its parts. The producer computes `own_set_id`
    /// (order-invariant over `own_changes`) and `merkle_state` from its engine;
    /// this crate carries them opaquely.
    pub fn new(
        scope: ViewScopeLabel,
        parent_view: Option<String>,
        prev: Vec<String>,
        own_changes: Vec<String>,
        own_set_id: String,
        merkle_state: Option<String>,
    ) -> Self {
        ViewSnapshot {
            scope: scope.as_str().to_string(),
            parent_view,
            prev,
            own_changes,
            own_set_id,
            merkle_state,
        }
    }

    /// Serialize to canonical JSON bytes. Deterministic: struct field order is
    /// fixed and every field is deterministic, so the byte output — and thus the
    /// content key — is stable across client and server.
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("ViewSnapshot serializes")
    }

    /// The content address (hex Blake3) of the canonical bytes — this object's
    /// key in the object store and the value a `views/{name}` ref points at.
    pub fn content_key(&self) -> crate::ObjectKey {
        crate::content_key(&self.to_canonical_bytes())
    }

    /// Deserialize from canonical bytes. Returns `None` on malformed input.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }

    /// The parsed scope label.
    pub fn scope_label(&self) -> ViewScopeLabel {
        ViewScopeLabel::parse(&self.scope)
    }

    /// Whether this snapshot represents a draft view.
    pub fn is_draft(&self) -> bool {
        self.scope_label() == ViewScopeLabel::Draft
    }

    /// The number of changes this view owns (does not include inherited).
    pub fn own_change_count(&self) -> usize {
        self.own_changes.len()
    }

    /// Compose the **effective** change set (own ∪ ancestors) from an ordered
    /// chain of snapshots: `chain[0]` is this view, each subsequent element its
    /// parent, up to the root. Order-preserving and deduplicated (root-first,
    /// then descendants), so callers get a stable membership list without
    /// touching the graph.
    ///
    /// The caller resolves the chain by walking `parent_view` (each name → its
    /// ref → its snapshot); this function is the pure composition step.
    pub fn effective_changes(chain: &[&ViewSnapshot]) -> Vec<String> {
        use std::collections::HashSet;
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        // Root-first so inherited history precedes a draft's own additions,
        // matching the order redb's filter chain yields.
        for snap in chain.iter().rev() {
            for h in &snap.own_changes {
                if seen.insert(h.as_str()) {
                    out.push(h.clone());
                }
            }
        }
        out
    }

    /// The size of the effective change set for a resolved ancestor chain
    /// (`chain[0]` = this view). Deduplicates across the chain.
    pub fn effective_change_count(chain: &[&ViewSnapshot]) -> usize {
        use std::collections::HashSet;
        let mut seen: HashSet<&str> = HashSet::new();
        for snap in chain {
            for h in &snap.own_changes {
                seen.insert(h.as_str());
            }
        }
        seen.len()
    }

    /// Render the legacy tab-separated manifest text this snapshot represents,
    /// for the transitional `?view-manifest` compatibility path. New code
    /// should read the object directly.
    pub fn to_manifest_text(&self, name: &str) -> String {
        let parent = self.parent_view.as_deref().unwrap_or("-");
        let state = self.merkle_state.as_deref().unwrap_or("-");
        let mut out = format!("{}\t{}\t{}\t{}\n", name, self.scope, parent, state);
        for hash in &self.own_changes {
            out.push_str(hash);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(scope: ViewScopeLabel, parent: Option<&str>, own: &[&str]) -> ViewSnapshot {
        ViewSnapshot::new(
            scope,
            parent.map(|s| s.to_string()),
            vec![],
            own.iter().map(|s| s.to_string()).collect(),
            "SETID".to_string(),
            own.last().map(|_| "MERKLE".to_string()),
        )
    }

    #[test]
    fn content_key_is_deterministic_and_bytes_roundtrip() {
        let a = snap(ViewScopeLabel::Draft, Some("dev"), &["C1", "C2"]);
        let b = a.clone();
        assert_eq!(a.content_key(), b.content_key());

        let bytes = a.to_canonical_bytes();
        assert!(crate::verify_key(&a.content_key(), &bytes));
        let decoded = ViewSnapshot::from_bytes(&bytes).expect("roundtrip");
        assert_eq!(decoded, a);
    }

    #[test]
    fn different_membership_changes_the_key() {
        let a = snap(ViewScopeLabel::Shared, None, &["C1"]);
        let b = snap(ViewScopeLabel::Shared, None, &["C2"]);
        assert_ne!(a.content_key(), b.content_key());
    }

    #[test]
    fn scope_and_parent_survive_roundtrip() {
        let s = snap(ViewScopeLabel::Draft, Some("dev"), &["C3"]);
        let decoded = ViewSnapshot::from_bytes(&s.to_canonical_bytes()).unwrap();
        assert!(decoded.is_draft());
        assert_eq!(decoded.parent_view.as_deref(), Some("dev"));
    }

    #[test]
    fn effective_set_composes_root_first_and_dedups() {
        // dev (root) owns C1,C2 ; draft owns C2 (dup) + C3.
        let dev = snap(ViewScopeLabel::Shared, None, &["C1", "C2"]);
        let draft = snap(ViewScopeLabel::Draft, Some("dev"), &["C2", "C3"]);

        // chain[0] = this view (draft), chain[1] = parent (dev).
        let chain = [&draft, &dev];
        assert_eq!(
            ViewSnapshot::effective_changes(&chain),
            vec!["C1".to_string(), "C2".to_string(), "C3".to_string()],
            "root-first, deduped"
        );
        assert_eq!(ViewSnapshot::effective_change_count(&chain), 3);
        assert_eq!(draft.own_change_count(), 2);
    }

    #[test]
    fn genesis_has_empty_prev_and_omits_optionals_in_json() {
        let s = snap(ViewScopeLabel::Shared, None, &[]);
        let text = String::from_utf8(s.to_canonical_bytes()).unwrap();
        // Empty prev and None parent/merkle are skipped in the canonical form.
        assert!(!text.contains("prev"));
        assert!(!text.contains("parent_view"));
        assert!(!text.contains("merkle_state"));
    }

    #[test]
    fn manifest_text_has_tab_header() {
        let s = snap(ViewScopeLabel::Draft, Some("dev"), &["C1", "C2"]);
        let text = s.to_manifest_text("feature");
        assert!(text.starts_with("feature\tdraft\tdev\tMERKLE\n"));
        assert!(text.contains("C1\n"));
        assert!(text.contains("C2\n"));
    }
}
