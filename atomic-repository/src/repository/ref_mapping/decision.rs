//! Pure three-way classification over a persisted mapping (RFC §8.5, CB-10A).

use atomic_core::pristine::{RefMapping, RefSyncStatus};

/// Observation of the live sides at reconcile time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreeWayObservation {
    /// The current local mapped-ref tip (hex), or `None` when the ref is
    /// absent (unborn branch, deleted mapping target, or no ref policy).
    pub current_git: Option<String>,
    /// The current Atomic view state (base32 Merkle).
    pub current_atomic: String,
}

/// What reconcile should do with the mapped pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreeWayAction {
    /// Neither side moved: no-op (re-observation/verification only).
    Noop,
    /// Git-only movement (or Git provably contains Atomic's export): import.
    Import,
    /// Atomic-only movement (or Atomic provably contains Git's closure):
    /// export under the expected-old lease.
    Export,
    /// Both moved incompatibly: persist `Diverged` and move neither side.
    Diverged,
    /// The mapping has no local ref (ephemeral `git/<oid>` view or deleted
    /// Shared view): there is no ref to reconcile.
    Unrepresentable,
}

/// Containment proof results, tri-state: `None` means "cannot prove".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Containment {
    /// Atomic's effective filter contains Git's bound closure.
    pub atomic_contains_git: Option<bool>,
    /// Git's history contains Atomic's exported projection.
    pub git_contains_atomic: Option<bool>,
}

/// The classified outcome for one mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefMappingDecision {
    pub action: ThreeWayAction,
    pub git_moved: bool,
    pub atomic_moved: bool,
}

/// Classify the three-way matrix for one mapping (RFC §8.5).
///
/// Neither Git nor Atomic ever moves on an unprovable both-moved case: the
/// classification fails closed to [`ThreeWayAction::Diverged`], and the caller
/// persists that status instead of overwriting either history.
pub fn classify_three_way(
    mapping: &RefMapping,
    observation: &ThreeWayObservation,
    containment: Containment,
) -> RefMappingDecision {
    let git_moved = observation.current_git != mapping.last_observed_local;
    let atomic_moved = Some(observation.current_atomic.clone()) != mapping.last_observed_atomic;
    if mapping.local_ref.is_none() {
        return RefMappingDecision {
            action: ThreeWayAction::Unrepresentable,
            git_moved,
            atomic_moved,
        };
    }
    let action = match (git_moved, atomic_moved) {
        (false, false) => ThreeWayAction::Noop,
        (true, false) => ThreeWayAction::Import,
        (false, true) => ThreeWayAction::Export,
        (true, true) => match (
            containment.atomic_contains_git,
            containment.git_contains_atomic,
        ) {
            // RFC §8.5 order: Atomic containing Git's bound closure exports.
            (Some(true), _) => ThreeWayAction::Export,
            (_, Some(true)) => ThreeWayAction::Import,
            // Anything unprovable fails closed: neither side moves.
            _ => ThreeWayAction::Diverged,
        },
    };
    RefMappingDecision {
        action,
        git_moved,
        atomic_moved,
    }
}

/// The status the mapping carries when `action` has been executed (or
/// refused). Diverged/Unrepresentable persist their status; successful
/// reconciliation returns both sides to `Synchronized`.
pub fn status_for(action: ThreeWayAction) -> RefSyncStatus {
    match action {
        ThreeWayAction::Noop | ThreeWayAction::Import | ThreeWayAction::Export => {
            RefSyncStatus::Synchronized
        }
        ThreeWayAction::Diverged => RefSyncStatus::Diverged,
        ThreeWayAction::Unrepresentable => RefSyncStatus::Unrepresentable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::ref_mapping::tests_support::sample_mapping;

    fn observe(git: Option<&str>, atomic: &str) -> ThreeWayObservation {
        ThreeWayObservation {
            current_git: git.map(str::to_string),
            current_atomic: atomic.to_string(),
        }
    }

    fn proven(atomic: bool, git: bool) -> Containment {
        Containment {
            atomic_contains_git: Some(atomic),
            git_contains_atomic: Some(git),
        }
    }

    #[test]
    fn neither_moved_is_a_noop() {
        let mapping = sample_mapping("main", Some("refs/heads/main"), Some("oid-1"), Some("st-1"));
        let decision = classify_three_way(&mapping, &observe(Some("oid-1"), "st-1"), contained());
        assert_eq!(decision.action, ThreeWayAction::Noop);
        assert!(!decision.git_moved && !decision.atomic_moved);
    }

    #[test]
    fn git_only_movement_imports() {
        let mapping = sample_mapping("main", Some("refs/heads/main"), Some("oid-1"), Some("st-1"));
        let decision = classify_three_way(&mapping, &observe(Some("oid-2"), "st-1"), contained());
        assert_eq!(decision.action, ThreeWayAction::Import);
        assert!(decision.git_moved && !decision.atomic_moved);
    }

    #[test]
    fn atomic_only_movement_exports() {
        let mapping = sample_mapping("main", Some("refs/heads/main"), Some("oid-1"), Some("st-1"));
        let decision = classify_three_way(&mapping, &observe(Some("oid-1"), "st-2"), contained());
        assert_eq!(decision.action, ThreeWayAction::Export);
        assert!(!decision.git_moved && decision.atomic_moved);
    }

    #[test]
    fn both_moved_exports_when_atomic_contains_git_closure() {
        let mapping = sample_mapping("main", Some("refs/heads/main"), Some("oid-1"), Some("st-1"));
        let decision = classify_three_way(
            &mapping,
            &observe(Some("oid-2"), "st-2"),
            proven(true, false),
        );
        assert_eq!(decision.action, ThreeWayAction::Export);
    }

    #[test]
    fn both_moved_imports_when_git_contains_atomic_export() {
        let mapping = sample_mapping("main", Some("refs/heads/main"), Some("oid-1"), Some("st-1"));
        let decision = classify_three_way(
            &mapping,
            &observe(Some("oid-2"), "st-2"),
            proven(false, true),
        );
        assert_eq!(decision.action, ThreeWayAction::Import);
    }

    #[test]
    fn both_moved_incompatible_persists_diverged() {
        let mapping = sample_mapping("main", Some("refs/heads/main"), Some("oid-1"), Some("st-1"));
        let decision = classify_three_way(
            &mapping,
            &observe(Some("oid-2"), "st-2"),
            proven(false, false),
        );
        assert_eq!(decision.action, ThreeWayAction::Diverged);
    }

    #[test]
    fn unprovable_containment_fails_closed_to_diverged() {
        let mapping = sample_mapping("main", Some("refs/heads/main"), Some("oid-1"), Some("st-1"));
        let decision = classify_three_way(
            &mapping,
            &observe(Some("oid-2"), "st-2"),
            Containment::default(),
        );
        assert_eq!(decision.action, ThreeWayAction::Diverged);
    }

    #[test]
    fn missing_local_ref_is_unrepresentable_even_when_sides_move() {
        let mut mapping = sample_mapping("git/abc1234", None, Some("oid-1"), Some("st-1"));
        mapping.status = RefSyncStatus::Unrepresentable;
        let decision = classify_three_way(
            &mapping,
            &observe(Some("oid-9"), "st-9"),
            proven(true, true),
        );
        assert_eq!(decision.action, ThreeWayAction::Unrepresentable);
    }

    #[test]
    fn status_follows_outcome() {
        assert_eq!(
            status_for(ThreeWayAction::Noop),
            RefSyncStatus::Synchronized
        );
        assert_eq!(
            status_for(ThreeWayAction::Import),
            RefSyncStatus::Synchronized
        );
        assert_eq!(
            status_for(ThreeWayAction::Export),
            RefSyncStatus::Synchronized
        );
        assert_eq!(
            status_for(ThreeWayAction::Diverged),
            RefSyncStatus::Diverged
        );
        assert_eq!(
            status_for(ThreeWayAction::Unrepresentable),
            RefSyncStatus::Unrepresentable
        );
    }

    fn contained() -> Containment {
        Containment::default()
    }
}
