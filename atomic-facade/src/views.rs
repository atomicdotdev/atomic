//! View listing as serializable summaries.

use atomic_core::pristine::ViewScope;
use atomic_repository::Repository;
use serde::{Deserialize, Serialize};

use crate::error::FacadeResult;

/// A view and its state, ready for JSON serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewSummary {
    /// View name.
    pub name: String,
    /// Merkle state (base32).
    pub state: String,
    /// Total changes visible through the view's filter chain.
    pub change_count: u64,
    /// Changes recorded on this view itself (excludes inherited ones).
    pub own_change_count: u64,
    /// "draft" or "shared".
    pub scope: String,
    /// Parent view name (absent for the root view).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Whether this is the repository's current view.
    pub is_current: bool,
}

/// All views in the repository, sorted by name.
pub fn list_views(repo: &Repository) -> FacadeResult<Vec<ViewSummary>> {
    let current = repo.current_view().to_string();
    let mut names = repo.list_views()?;
    names.sort();

    names
        .into_iter()
        .map(|name| view_summary_inner(repo, &name, &current))
        .collect()
}

/// A single view's summary.
pub fn view_summary(repo: &Repository, name: &str) -> FacadeResult<ViewSummary> {
    view_summary_inner(repo, name, repo.current_view())
}

fn view_summary_inner(repo: &Repository, name: &str, current: &str) -> FacadeResult<ViewSummary> {
    let info = repo.get_view_info(name)?;
    let state = info.state_base32();
    Ok(ViewSummary {
        name: info.name,
        state,
        change_count: info.change_count,
        own_change_count: info.own_change_count,
        scope: scope_label(info.scope).to_string(),
        parent: info.parent_name,
        is_current: name == current,
    })
}

fn scope_label(scope: ViewScope) -> &'static str {
    match scope {
        ViewScope::Draft => "draft",
        ViewScope::Shared => "shared",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_default_view_as_current() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        let views = list_views(&repo).unwrap();
        assert!(!views.is_empty());

        let current: Vec<_> = views.iter().filter(|v| v.is_current).collect();
        assert_eq!(current.len(), 1, "exactly one current view");
        assert_eq!(current[0].name, repo.current_view());
        assert_eq!(current[0].scope, "shared");
    }
}
