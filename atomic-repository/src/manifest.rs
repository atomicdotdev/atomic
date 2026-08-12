//! View manifests: a view's identity as a self-contained, verifiable artifact.
//!
//! A view in the ambient-graph model is nothing more than an ordered
//! change-ref log plus `(scope, parent)`, whose merkle state is a fold over
//! that log (`state_n = H(state_{n-1} || change_hash_n)`, seeded at
//! [`Merkle::ZERO`]). A [`ViewManifest`] captures exactly that — no more, no
//! less — so a view can be transferred between repositories with byte-exact
//! round-trip fidelity and verified structurally on both ends.
//!
//! The manifest carries the view's log **exactly as stored** in
//! `VIEW_CHANGES`. For a draft view this includes the inherited prefix copied
//! from its parent at fork time, not a computed delta: the declared `state`
//! is the fold over the full sequence, which is what makes verification
//! possible without trusting the sender.
//!
//! # Wire format
//!
//! Text, line-based, tab-separated header — consistent with the rest of the
//! protocol (`?views`, changelists), and extensible via trailing fields:
//!
//! ```text
//! name\tscope\tparent\tstate
//! <change hash, base32>
//! <change hash, base32>
//! ...
//! ```
//!
//! * `scope` is `shared` or `draft`.
//! * `parent` is the parent view name, or `-` for a root view.
//! * `state` is the base32 merkle fold of the change sequence, or `-` for an
//!   empty view (fold seed [`Merkle::ZERO`]).
//!
//! Dependency truth is **not** part of the manifest: change files are
//! self-describing (they embed their dependency hashes), so a receiver
//! validates dependencies against the change files it holds, never against
//! sender claims.

use atomic_core::pristine::ViewScope;
use atomic_core::types::{Base32, Hash, Merkle};
use thiserror::Error;

/// Marker for "no parent" / "empty state" in the text format.
const NONE_FIELD: &str = "-";

/// Errors from parsing or verifying a view manifest.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// The manifest text has no header line.
    #[error("Empty manifest: missing header line")]
    Empty,

    /// The header line is missing required fields.
    #[error("Malformed manifest header: {0:?}")]
    MalformedHeader(String),

    /// The scope field is neither `shared` nor `draft`.
    #[error("Unknown view scope {scope:?} in manifest for {name:?}")]
    UnknownScope { name: String, scope: String },

    /// A change line is not a valid base32 hash.
    #[error("Invalid change hash on line {line}: {text:?}")]
    InvalidHash { line: usize, text: String },

    /// The declared state is not a valid base32 merkle.
    #[error("Invalid state {state:?} in manifest for {name:?}")]
    InvalidState { name: String, state: String },

    /// The fold of the change log does not equal the declared state.
    #[error(
        "Manifest state mismatch for view {name:?}: declared {declared}, log folds to {computed}"
    )]
    StateMismatch {
        name: String,
        declared: String,
        computed: String,
    },
}

/// A view's complete identity: name, scope, parent, ordered change log, and
/// the declared merkle state of that log.
///
/// See the [module docs](self) for the semantics and wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewManifest {
    /// The view name.
    pub name: String,

    /// The view scope (shared or draft).
    pub scope: ViewScope,

    /// Parent view name, or `None` for a root view.
    pub parent: Option<String>,

    /// The view's change log, exactly as stored in `VIEW_CHANGES`, in
    /// sequence order. For a draft this includes the inherited prefix.
    pub changes: Vec<Hash>,

    /// Declared merkle state: the fold of `changes` seeded at
    /// [`Merkle::ZERO`]. Receivers must verify this with [`Self::verify`].
    pub state: Merkle,
}

impl ViewManifest {
    /// Fold a change sequence into its merkle state.
    ///
    /// This mirrors `put_change`: `state_n = state_{n-1}.next(hash_n)`,
    /// seeded at [`Merkle::ZERO`].
    pub fn fold(changes: &[Hash]) -> Merkle {
        changes
            .iter()
            .fold(Merkle::ZERO, |state, hash| state.next(hash))
    }

    /// Build a manifest from parts, computing the state from the log.
    pub fn new(
        name: impl Into<String>,
        scope: ViewScope,
        parent: Option<String>,
        changes: Vec<Hash>,
    ) -> Self {
        let state = Self::fold(&changes);
        Self {
            name: name.into(),
            scope,
            parent,
            changes,
            state,
        }
    }

    /// Verify that the declared state equals the fold of the change log.
    pub fn verify(&self) -> Result<(), ManifestError> {
        let computed = Self::fold(&self.changes);
        if computed != self.state {
            return Err(ManifestError::StateMismatch {
                name: self.name.clone(),
                declared: self.state.to_base32(),
                computed: computed.to_base32(),
            });
        }
        Ok(())
    }

    /// Serialize to the text wire format.
    pub fn to_text(&self) -> String {
        let scope = if self.scope.is_draft() {
            "draft"
        } else {
            "shared"
        };
        let parent = self.parent.as_deref().unwrap_or(NONE_FIELD);
        let state = if self.changes.is_empty() {
            NONE_FIELD.to_string()
        } else {
            self.state.to_base32()
        };

        let mut out = format!("{}\t{}\t{}\t{}\n", self.name, scope, parent, state);
        for hash in &self.changes {
            out.push_str(&hash.to_base32());
            out.push('\n');
        }
        out
    }

    /// Parse the text wire format.
    ///
    /// Parsing validates structure only; call [`Self::verify`] to check that
    /// the declared state matches the log. Unknown trailing header fields are
    /// ignored so the format can grow without breaking older readers.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let mut lines = text.lines();

        let header = loop {
            match lines.next() {
                Some(line) if line.trim().is_empty() => continue,
                Some(line) => break line,
                None => return Err(ManifestError::Empty),
            }
        };

        let mut fields = header.split('\t');
        let name = fields
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ManifestError::MalformedHeader(header.to_string()))?
            .to_string();
        let scope_text = fields
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ManifestError::MalformedHeader(header.to_string()))?;
        let parent_text = fields
            .next()
            .map(str::trim)
            .ok_or_else(|| ManifestError::MalformedHeader(header.to_string()))?;
        let state_text = fields
            .next()
            .map(str::trim)
            .ok_or_else(|| ManifestError::MalformedHeader(header.to_string()))?;

        let scope = match scope_text.to_ascii_lowercase().as_str() {
            "shared" => ViewScope::Shared,
            "draft" => ViewScope::Draft,
            other => {
                return Err(ManifestError::UnknownScope {
                    name,
                    scope: other.to_string(),
                })
            }
        };

        let parent = match parent_text {
            NONE_FIELD | "" => None,
            p => Some(p.to_string()),
        };

        let mut changes = Vec::new();
        for (i, line) in lines.enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let hash = Hash::from_base32(line.as_bytes()).ok_or(ManifestError::InvalidHash {
                // +2: 1-based, plus the header line.
                line: i + 2,
                text: line.to_string(),
            })?;
            changes.push(hash);
        }

        let state = match state_text {
            NONE_FIELD | "" => Merkle::ZERO,
            s => Merkle::from_base32(s.as_bytes()).ok_or_else(|| ManifestError::InvalidState {
                name: name.clone(),
                state: s.to_string(),
            })?,
        };

        Ok(Self {
            name,
            scope,
            parent,
            changes,
            state,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> Hash {
        Hash::of(&[n])
    }

    /// A shared root view round-trips through text with an identical fold.
    #[test]
    fn round_trip_shared_root() {
        let m = ViewManifest::new("dev", ViewScope::Shared, None, vec![h(1), h(2)]);
        m.verify().unwrap();

        let parsed = ViewManifest::parse(&m.to_text()).unwrap();
        assert_eq!(parsed, m);
        parsed.verify().unwrap();
        assert_eq!(parsed.parent, None);
        assert!(parsed.scope.is_shared());
    }

    /// A draft with a parent keeps scope, parent, and full log (inherited
    /// prefix included) through the round-trip.
    #[test]
    fn round_trip_draft_with_parent() {
        // Log = inherited prefix (1, 2) + own suffix (3, 4, 5): the manifest
        // carries the stored log, never a delta.
        let m = ViewManifest::new(
            "orange-night-44fb",
            ViewScope::Draft,
            Some("dev".to_string()),
            vec![h(1), h(2), h(3), h(4), h(5)],
        );
        m.verify().unwrap();

        let parsed = ViewManifest::parse(&m.to_text()).unwrap();
        assert_eq!(parsed, m);
        assert!(parsed.scope.is_draft());
        assert_eq!(parsed.parent.as_deref(), Some("dev"));
        assert_eq!(parsed.changes.len(), 5);
    }

    /// An empty view serializes its state as `-` and folds to ZERO.
    #[test]
    fn round_trip_empty_view() {
        let m = ViewManifest::new("fresh", ViewScope::Shared, None, vec![]);
        assert_eq!(m.state, Merkle::ZERO);

        let text = m.to_text();
        assert!(text.starts_with("fresh\tshared\t-\t-"));

        let parsed = ViewManifest::parse(&text).unwrap();
        assert_eq!(parsed, m);
        parsed.verify().unwrap();
    }

    /// A declared state that doesn't match the log fails verification.
    #[test]
    fn verify_rejects_state_mismatch() {
        let mut m = ViewManifest::new("dev", ViewScope::Shared, None, vec![h(1), h(2)]);
        m.state = Merkle::of(b"tampered");
        let err = m.verify().unwrap_err();
        assert!(matches!(err, ManifestError::StateMismatch { .. }));
    }

    /// The fold matches put_change's incremental construction.
    #[test]
    fn fold_matches_incremental_next() {
        let changes = vec![h(9), h(8), h(7)];
        let mut state = Merkle::ZERO;
        for c in &changes {
            state = state.next(c);
        }
        assert_eq!(ViewManifest::fold(&changes), state);
        // Order matters: the fold is a chain, not a set hash.
        let reordered = vec![h(7), h(8), h(9)];
        assert_ne!(ViewManifest::fold(&reordered), state);
    }

    /// Structural parse errors are reported distinctly.
    #[test]
    fn parse_rejects_malformed_input() {
        assert!(matches!(ViewManifest::parse(""), Err(ManifestError::Empty)));
        assert!(matches!(
            ViewManifest::parse("dev\tshared"),
            Err(ManifestError::MalformedHeader(_))
        ));
        assert!(matches!(
            ViewManifest::parse("dev\tcosmic\t-\t-"),
            Err(ManifestError::UnknownScope { .. })
        ));
        let bad_hash = "dev\tshared\t-\t-\nnot-a-hash\n";
        assert!(matches!(
            ViewManifest::parse(bad_hash),
            Err(ManifestError::InvalidHash { line: 2, .. })
        ));
        let bad_state = "dev\tshared\t-\tzzz!\n";
        assert!(matches!(
            ViewManifest::parse(bad_state),
            Err(ManifestError::InvalidState { .. })
        ));
    }

    /// Trailing header fields (future extensions) don't break parsing.
    #[test]
    fn parse_ignores_trailing_header_fields() {
        let m = ViewManifest::new("dev", ViewScope::Shared, None, vec![h(1)]);
        let mut text = m.to_text();
        // Simulate a newer sender appending a field to the header line.
        let nl = text.find('\n').unwrap();
        text.insert_str(nl, "\tfuture-field");
        let parsed = ViewManifest::parse(&text).unwrap();
        assert_eq!(parsed, m);
    }
}
