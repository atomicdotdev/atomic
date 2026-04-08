use crate::pristine::traits::view::{ViewScope, ViewState};
use crate::types::Merkle;

#[test]
fn test_view_state_default() {
    let state = ViewState::default();
    assert_eq!(state.id, 0);
    assert_eq!(state.name, "");
    assert_eq!(state.state, Merkle::ZERO);
    assert_eq!(state.change_count, 0);
    assert_eq!(state.kind, ViewScope::Shared);
    assert_eq!(state.parent, None);
}

#[test]
fn test_view_state_new() {
    let state = ViewState::new(42, "test".to_string());
    assert_eq!(state.id, 42);
    assert_eq!(state.name, "test");
    assert_eq!(state.state, Merkle::ZERO);
    assert_eq!(state.change_count, 0);
    assert_eq!(state.kind, ViewScope::Shared);
    assert_eq!(state.parent, None);
}

#[test]
fn test_view_state_with_scope() {
    let state = ViewState::with_scope(3, "feature".to_string(), ViewScope::Draft, Some(2));
    assert_eq!(state.id, 3);
    assert_eq!(state.name, "feature");
    assert_eq!(state.kind, ViewScope::Draft);
    assert_eq!(state.parent, Some(2));
    assert!(state.is_empty());
    assert!(!state.is_root());
}

#[test]
fn test_view_state_is_empty() {
    let mut state = ViewState::new(1, "test".to_string());
    assert!(state.is_empty());
    state.change_count = 1;
    assert!(!state.is_empty());
}

#[test]
fn test_view_state_is_root() {
    let root = ViewState::new(1, "main".to_string());
    assert!(root.is_root());

    let child = ViewState::with_scope(2, "dev".to_string(), ViewScope::Shared, Some(1));
    assert!(!child.is_root());
}

#[test]
fn test_view_scope_from_u8() {
    assert_eq!(ViewScope::from_u8(0), Some(ViewScope::Draft));
    assert_eq!(ViewScope::from_u8(1), Some(ViewScope::Shared));
    assert_eq!(ViewScope::from_u8(2), None);
    assert_eq!(ViewScope::from_u8(255), None);
}

#[test]
fn test_view_scope_display() {
    assert_eq!(format!("{}", ViewScope::Draft), "draft");
    assert_eq!(format!("{}", ViewScope::Shared), "shared");
}

#[test]
fn test_view_scope_default() {
    assert_eq!(ViewScope::default(), ViewScope::Shared);
}

#[test]
fn test_view_scope_predicates() {
    assert!(ViewScope::Shared.is_shared());
    assert!(!ViewScope::Shared.is_draft());
    assert!(ViewScope::Draft.is_draft());
    assert!(!ViewScope::Draft.is_shared());
}
