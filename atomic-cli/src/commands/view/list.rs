//! The `view list` command for listing all views.
//!
//! This module implements the `atomic view list` command. Views are rendered
//! as an indented hierarchy following parent chains: a draft split from
//! another view is listed beneath its parent, so a chain like
//! `dev → baby-bird-123 → jumbo-tron-444` reads as a tree. Views with no
//! changes of their own are hidden by default to keep the listing focused
//! on active work:
//!
//! ```text
//! $ atomic view list
//! dev              [shared]  (3 changes)                state: 2AAAAAAAA...
//!   - baby-bird-123  [draft]  (2 changes, 3 inherited)   state: XYZABCDEF...  parent: dev
//!
//! 1 view not shown because contained no changes. -a to view them.
//! ```
//!
//! # Usage
//!
//! ```text
//! atomic view list [OPTIONS]
//!
//! Options:
//!   -s, --short   Show only view names (no metadata)
//!   -a, --all     Show all views, including those with no changes
//!   -h, --help    Print help information
//! ```
//!
//! # Examples
//!
//! Short listing (names only, hierarchy preserved):
//! ```text
//! $ atomic view list --short
//! dev
//!   - baby-bird-123
//! ```

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use clap::Parser;

use atomic_remote::{HttpRemote, HttpRemoteConfig, RemoteViewInfo};
use atomic_repository::repository::ViewInfo;
use atomic_repository::Repository;

use crate::commands::auth::attach_identity;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{hint, view as style_view};

/// Default request timeout (seconds) for the remote view listing.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[cfg(test)]
use std::path::PathBuf;

// List Command

/// List all views.
///
/// Shows views as an indented hierarchy following parent chains, with the
/// current view marked with an asterisk (*). Views with no changes of their
/// own are hidden by default (with a trailing summary count); pass `-a` to
/// list every view. By default, displays metadata (scope, change count,
/// state hash, parent). Use `--short` for names only.
#[derive(Parser, Debug, Default)]
#[command(name = "list")]
pub struct List {
    /// Show only view names (no metadata).
    #[arg(long, short = 's')]
    pub short: bool,

    /// Show all views, including those with no changes of their own.
    ///
    /// By default, views without own changes are hidden from the listing
    /// (counted in a trailing summary line) unless they anchor a hierarchy
    /// that does have changes or are the current view.
    #[arg(short = 'a', long)]
    pub all: bool,

    /// Show additional details (state hash, change count).
    ///
    /// This is now the default behavior. Kept for backward compatibility.
    #[arg(long, short = 'v', hide = true)]
    pub verbose: bool,

    /// List views on a remote instead of locally.
    ///
    /// Pass `--remote` alone to use the default remote, or
    /// `--remote <name|url>` to target a specific configured remote or URL.
    #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "REMOTE")]
    pub remote: Option<String>,

    /// Identity to use for authenticating to the remote.
    ///
    /// Only meaningful together with `--remote`. Overrides the identity
    /// inferred from the remote URL. Must match a locally stored identity.
    #[arg(long)]
    pub identity: Option<String>,

    /// Skip TLS certificate verification when querying a remote.
    ///
    /// Only meaningful together with `--remote`. Reduces security; use only
    /// for testing or self-signed certificates.
    #[arg(short = 'k', long)]
    pub insecure: bool,
}

impl List {
    /// Create a new List command with default settings.
    pub fn new() -> Self {
        Self {
            short: false,
            all: false,
            verbose: false,
            remote: None,
            identity: None,
            insecure: false,
        }
    }

    /// Builder: set the verbose flag.
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }
}

// View entry + hierarchy rendering
//
// The tree building, filtering, and line rendering below are pure functions
// over plain data so the layout can be unit-tested without a repository.

/// A view prepared for rendering, carrying the metadata the tree needs.
#[derive(Debug, Clone)]
struct ViewEntry {
    name: String,
    parent: Option<String>,
    /// Changes recorded into this view that are not visible through the
    /// parent chain. Zero means the view has nothing of its own to show.
    own_change_count: u64,
    change_count: u64,
    inherited_change_count: u64,
    kind: &'static str,
    state_short: String,
    is_current: bool,
    /// False when `get_view_info` failed; the line renders name-only.
    has_info: bool,
}

impl ViewEntry {
    fn from_info(info: ViewInfo, current: &str) -> Self {
        let kind = match info.kind_label() {
            "draft" => "draft",
            _ => "shared",
        };
        let is_current = info.name == current;
        let state_short = info.state_short();
        ViewEntry {
            name: info.name,
            parent: info.parent_name,
            own_change_count: info.own_change_count,
            change_count: info.change_count,
            inherited_change_count: info.inherited_change_count,
            kind,
            state_short,
            is_current,
            has_info: true,
        }
    }

    /// A name-only entry used when view metadata is unavailable. Treated as
    /// having changes so it is never hidden from the listing.
    fn bare(name: &str, current: &str) -> Self {
        ViewEntry {
            name: name.to_string(),
            parent: None,
            own_change_count: 1,
            change_count: 0,
            inherited_change_count: 0,
            kind: "shared",
            state_short: "-".to_string(),
            is_current: name == current,
            has_info: false,
        }
    }
}

/// Decide which views are shown.
///
/// With `show_all`, every view is visible. Otherwise a view is visible when
/// it has changes of its own, or it is the current view, or it is an
/// ancestor of such a view (so the hierarchy stays connected). Returns the
/// visible name set and the hidden count.
fn compute_visibility(entries: &[ViewEntry], show_all: bool) -> (HashSet<String>, usize) {
    if show_all {
        let all = entries.iter().map(|e| e.name.clone()).collect();
        return (all, 0);
    }

    let by_name: HashMap<&str, &ViewEntry> = entries.iter().map(|e| (e.name.as_str(), e)).collect();

    let mut visible: HashSet<String> = HashSet::new();
    let mut stack: Vec<&str> = entries
        .iter()
        .filter(|e| e.own_change_count > 0 || e.is_current)
        .map(|e| e.name.as_str())
        .collect();

    while let Some(name) = stack.pop() {
        // Revisiting a name means a parent cycle — stop walking that chain.
        if !visible.insert(name.to_string()) {
            continue;
        }
        if let Some(entry) = by_name.get(name) {
            if let Some(parent) = &entry.parent {
                if by_name.contains_key(parent.as_str()) {
                    stack.push(parent.as_str());
                }
            }
        }
    }

    let hidden = entries.len() - visible.len();
    (visible, hidden)
}

/// Order the visible views depth-first along the parent chains.
///
/// Returns `(depth, entry)` pairs in render order: roots (views with no
/// parent, or with a parent missing from the listing) at depth 0, each
/// child level one step deeper. Roots and siblings sort alphabetically.
/// Cycles in the parent data are cut by a visited set.
fn tree_order<'a>(
    entries: &'a [ViewEntry],
    visible: &HashSet<String>,
) -> Vec<(usize, &'a ViewEntry)> {
    let by_name: HashMap<&str, &ViewEntry> = entries.iter().map(|e| (e.name.as_str(), e)).collect();

    let mut children: HashMap<&str, Vec<&ViewEntry>> = HashMap::new();
    let mut roots: Vec<&ViewEntry> = Vec::new();
    for entry in entries {
        match entry.parent.as_deref().and_then(|p| by_name.get(p)) {
            Some(parent) => children
                .entry(parent.name.as_str())
                .or_default()
                .push(entry),
            None => roots.push(entry),
        }
    }
    for kids in children.values_mut() {
        kids.sort_by(|a, b| a.name.cmp(&b.name));
    }
    roots.sort_by(|a, b| a.name.cmp(&b.name));

    fn visit<'a>(
        entry: &'a ViewEntry,
        depth: usize,
        children: &HashMap<&str, Vec<&'a ViewEntry>>,
        visited: &mut HashSet<&'a str>,
        visible: &HashSet<String>,
        ordered: &mut Vec<(usize, &'a ViewEntry)>,
    ) {
        if !visited.insert(entry.name.as_str()) {
            return; // cut parent cycles
        }
        if visible.contains(&entry.name) {
            ordered.push((depth, entry));
        }
        if let Some(kids) = children.get(entry.name.as_str()) {
            for kid in kids {
                visit(kid, depth + 1, children, visited, visible, ordered);
            }
        }
    }

    let mut ordered = Vec::new();
    let mut visited: HashSet<&str> = HashSet::new();
    for root in &roots {
        visit(root, 0, &children, &mut visited, visible, &mut ordered);
    }

    // Defensive: views caught in a parent cycle are never roots, so the
    // walk above never reaches them. Render any remainder in name order
    // from depth 0 so nothing silently vanishes from the listing.
    let mut leftovers: Vec<&ViewEntry> = entries
        .iter()
        .filter(|e| !visited.contains(e.name.as_str()))
        .collect();
    leftovers.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in &leftovers {
        visit(entry, 0, &children, &mut visited, visible, &mut ordered);
    }

    ordered
}

/// The tree prefix for a line at the given depth.
///
/// Roots start at column 0 (`dev`, or `* dev` when current); children are
/// indented two spaces per level with a `- ` bullet (`  - baby-bird-123`).
fn tree_prefix(entry: &ViewEntry, depth: usize) -> String {
    match depth {
        0 if entry.is_current => "* ".to_string(),
        0 => String::new(),
        _ => format!(
            "{}- {}",
            "  ".repeat(depth),
            if entry.is_current { "* " } else { "" }
        ),
    }
}

/// Render one view line for the default (metadata) mode.
fn render_line(entry: &ViewEntry, depth: usize, width: usize) -> String {
    let prefix = tree_prefix(entry, depth);
    let name = style_view(&entry.name);

    if !entry.has_info {
        return format!("{}{}", prefix, name);
    }

    let kind_tag = if entry.kind == "draft" {
        "[draft]"
    } else {
        "[shared]"
    };
    // Show own changes for views with a parent; show total for root views.
    let change_display = if entry.parent.is_some() {
        if entry.own_change_count == 1 {
            format!(
                "({} change, {} inherited)",
                entry.own_change_count, entry.inherited_change_count
            )
        } else {
            format!(
                "({} changes, {} inherited)",
                entry.own_change_count, entry.inherited_change_count
            )
        }
    } else if entry.change_count == 1 {
        "(1 change)".to_string()
    } else {
        format!("({} changes)", entry.change_count)
    };
    let parent_info = match &entry.parent {
        Some(p) => format!("  parent: {}", style_view(p)),
        None => String::new(),
    };

    format!(
        "{}{:<width$}  {:<10}  {}  state: {}{}",
        prefix,
        name,
        kind_tag,
        change_display,
        entry.state_short,
        parent_info,
        width = width
    )
}

/// Render one view line for `--short` mode (names only).
fn render_short_line(entry: &ViewEntry, depth: usize) -> String {
    format!("{}{}", tree_prefix(entry, depth), style_view(&entry.name))
}

/// The trailing hint describing hidden views.
fn summary_line(hidden: usize) -> String {
    let noun = if hidden == 1 { "view" } else { "views" };
    format!(
        "{} {} not shown because contained no changes. -a to view them.",
        hidden, noun
    )
}

impl List {
    /// List views on a remote repository.
    ///
    /// `remote_arg` is the raw `--remote` value: empty means "use the default
    /// remote", otherwise it is a configured remote name or a URL.
    fn run_remote(&self, remote_arg: &str) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // Resolve the remote name and URL.
        let (remote_name, remote_url) = if remote_arg.is_empty() {
            repo.get_default_remote()
                .map(|(name, entry)| (name, entry.url))
                .map_err(CliError::Repository)?
        } else if remote_arg.contains("://") {
            (remote_arg.to_string(), remote_arg.to_string())
        } else {
            let entry = repo
                .get_remote(remote_arg)
                .map_err(|_| CliError::RemoteNotFound {
                    name: remote_arg.to_string(),
                })?;
            (remote_arg.to_string(), entry.url)
        };

        println!(
            "Views on {} ({})",
            style_view(&remote_name),
            hint(&remote_url)
        );

        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        let views = rt.block_on(async {
            let config = HttpRemoteConfig::new()
                .with_timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
                .danger_accept_invalid_certs(self.insecure);
            let config = attach_identity(config, &remote_url, self.identity.as_deref()).await;
            let remote = HttpRemote::with_config(&remote_url, config)
                .map_err(|e| CliError::remote_error(e.to_string(), Some(remote_url.clone())))?;
            // Bare object+ref model: the inventory is composed server-side from
            // the `.view` objects (GET /refs/views), independent of any
            // reconciled read-model.
            remote
                .list_view_refs()
                .await
                .map_err(|e| CliError::remote_error(e.to_string(), Some(remote_url.clone())))
        })?;

        self.print_remote_views(&views);
        Ok(())
    }

    /// Render the remote view listing.
    fn print_remote_views(&self, views: &[RemoteViewInfo]) {
        if views.is_empty() {
            println!("{}", hint("No views found on the remote."));
            return;
        }

        let mut sorted: Vec<&RemoteViewInfo> = views.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        if self.short {
            for view in sorted {
                println!("  {}", style_view(&view.name));
            }
            return;
        }

        let max_name_len = sorted.iter().map(|v| v.name.len()).max().unwrap_or(0);

        for view in sorted {
            let kind_tag = if view.is_draft() {
                "[draft]"
            } else {
                "[shared]"
            };
            let change_display = if view.change_count == 1 {
                "(1 change)".to_string()
            } else {
                format!("({} changes)", view.change_count)
            };
            let parent_info = match &view.parent {
                Some(p) => format!("  parent: {}", style_view(p)),
                None => String::new(),
            };
            let state_display = view
                .state
                .as_deref()
                .map(|s| &s[..12.min(s.len())])
                .unwrap_or("-");
            println!(
                "  {:<width$}  {:<10}  {}  state: {}{}",
                style_view(&view.name),
                kind_tag,
                change_display,
                state_display,
                parent_info,
                width = max_name_len
            );
        }
    }
}

impl Command for List {
    fn run(&self) -> CliResult<()> {
        // Remote listing takes over entirely when --remote is present.
        if let Some(remote_arg) = &self.remote {
            return self.run_remote(remote_arg);
        }

        // Find the repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // Get list of views
        let views = repo.list_views().map_err(CliError::Repository)?;
        let current = repo.current_view().to_string();

        if views.is_empty() {
            println!(
                "{}",
                hint("No views found. Use 'atomic view create <name>' to create one.")
            );
            return Ok(());
        }

        // Collect metadata for every view; fall back to name-only entries
        // when info is unavailable so the names still render.
        let entries: Vec<ViewEntry> = views
            .iter()
            .map(|name| {
                repo.get_view_info(name)
                    .map(|info| ViewEntry::from_info(info, &current))
                    .unwrap_or_else(|_| ViewEntry::bare(name, &current))
            })
            .collect();

        // Filter empty views (unless --all), then render the hierarchy.
        let (visible, hidden) = compute_visibility(&entries, self.all);
        let ordered = tree_order(&entries, &visible);

        let max_name_len = ordered
            .iter()
            .map(|(_, entry)| entry.name.len())
            .max()
            .unwrap_or(0);

        for (depth, entry) in &ordered {
            let line = if self.short {
                render_short_line(entry, *depth)
            } else {
                render_line(entry, *depth, max_name_len)
            };
            println!("{}", line);
        }

        if hidden > 0 {
            println!("{}", hint(&summary_line(hidden)));
        }

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // -------------------------------------------------------------------------
    // Directory Guard for Safe Current Dir Changes
    // -------------------------------------------------------------------------

    struct DirGuard {
        original: PathBuf,
    }

    impl DirGuard {
        fn new() -> Self {
            Self {
                original: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    // -------------------------------------------------------------------------
    // Command Builder Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_default() {
        let cmd = List::default();
        assert!(!cmd.short);
        assert!(!cmd.all);
        assert!(!cmd.verbose);
    }

    #[test]
    fn test_new() {
        let cmd = List::new();
        assert!(!cmd.short);
        assert!(!cmd.all);
    }

    #[test]
    fn test_with_verbose() {
        let cmd = List::new().with_verbose(true);
        assert!(cmd.verbose);
    }

    // -------------------------------------------------------------------------
    // Hierarchy rendering helpers (pure functions)
    // -------------------------------------------------------------------------

    /// Build a `ViewEntry` for tests. Roots count their own changes as
    /// `change_count` (matching `get_view_info` for parentless views).
    fn entry(name: &str, parent: Option<&str>, own: u64, current: &str) -> ViewEntry {
        ViewEntry {
            name: name.to_string(),
            parent: parent.map(|p| p.to_string()),
            own_change_count: own,
            change_count: own,
            inherited_change_count: if parent.is_some() { 3 } else { 0 },
            kind: if parent.is_some() { "draft" } else { "shared" },
            state_short: "AAAAAAAAAAAA".to_string(),
            is_current: name == current,
            has_info: true,
        }
    }

    #[test]
    fn test_hierarchy_layout_three_deep() {
        // dev → baby-bird-123 → jumbo-tron-444 (empty leaf)
        let entries = vec![
            entry("dev", None, 3, "dev"),
            entry("baby-bird-123", Some("dev"), 2, "dev"),
            entry("jumbo-tron-444", Some("baby-bird-123"), 0, "dev"),
        ];

        let (visible, hidden) = compute_visibility(&entries, false);
        assert_eq!(hidden, 1);
        assert!(visible.contains("dev"));
        assert!(visible.contains("baby-bird-123"));
        assert!(!visible.contains("jumbo-tron-444"));

        let ordered = tree_order(&entries, &visible);
        let lines: Vec<String> = ordered
            .iter()
            .map(|(depth, e)| render_short_line(e, *depth))
            .collect();
        assert_eq!(lines, vec!["* dev", "  - baby-bird-123"]);
    }

    #[test]
    fn test_all_flag_lists_every_view() {
        let entries = vec![
            entry("dev", None, 3, "dev"),
            entry("baby-bird-123", Some("dev"), 2, "dev"),
            entry("jumbo-tron-444", Some("baby-bird-123"), 0, "dev"),
        ];

        let (visible, hidden) = compute_visibility(&entries, true);
        assert_eq!(hidden, 0);
        assert_eq!(visible.len(), entries.len());
        assert!(visible.contains("jumbo-tron-444"));

        let ordered = tree_order(&entries, &visible);
        let lines: Vec<String> = ordered
            .iter()
            .map(|(depth, e)| render_short_line(e, *depth))
            .collect();
        assert_eq!(
            lines,
            vec!["* dev", "  - baby-bird-123", "    - jumbo-tron-444"]
        );
    }

    #[test]
    fn test_summary_line_counts() {
        assert_eq!(
            summary_line(1),
            "1 view not shown because contained no changes. -a to view them."
        );
        assert_eq!(
            summary_line(2),
            "2 views not shown because contained no changes. -a to view them."
        );
    }

    #[test]
    fn test_current_empty_view_stays_visible() {
        // The current view is empty; it must still be listed, and its
        // parent chain anchors the hierarchy.
        let entries = vec![
            entry("dev", None, 3, "wren"),
            entry("wren", Some("dev"), 0, "wren"),
        ];

        let (visible, hidden) = compute_visibility(&entries, false);
        assert_eq!(hidden, 0);
        assert!(visible.contains("wren"));
        assert!(visible.contains("dev"));

        let ordered = tree_order(&entries, &visible);
        let lines: Vec<String> = ordered
            .iter()
            .map(|(depth, e)| render_short_line(e, *depth))
            .collect();
        assert_eq!(lines, vec!["dev", "  - * wren"]);
    }

    #[test]
    fn test_empty_sibling_draft_hidden() {
        let entries = vec![
            entry("dev", None, 3, "dev"),
            entry("hawk", Some("dev"), 1, "dev"),
            entry("wren", Some("dev"), 0, "dev"), // empty, not current → hidden
        ];

        let (visible, hidden) = compute_visibility(&entries, false);
        assert_eq!(hidden, 1);
        assert!(!visible.contains("wren"));

        let ordered = tree_order(&entries, &visible);
        let names: Vec<&str> = ordered.iter().map(|(_, e)| e.name.as_str()).collect();
        assert_eq!(names, vec!["dev", "hawk"]);
    }

    #[test]
    fn test_default_mode_line_keeps_metadata() {
        let e = entry("baby-bird-123", Some("dev"), 2, "dev");
        let line = render_line(&e, 1, 13);
        assert!(line.starts_with("  - baby-bird-123"));
        assert!(line.contains("[draft]"));
        assert!(line.contains("(2 changes, 3 inherited)"));
        assert!(line.contains("state: AAAAAAAAAAAA"));
        assert!(line.contains("parent: dev"));
    }

    #[test]
    fn test_root_line_format() {
        let dev = entry("dev", None, 3, "dev");
        let line = render_line(&dev, 0, 3);
        assert!(line.starts_with("* dev"));

        let main = entry("main", None, 2, "dev");
        let line = render_line(&main, 0, 4);
        assert!(line.starts_with("main"));
    }

    #[test]
    fn test_missing_parent_renders_as_root() {
        let entries = vec![entry("orphan", Some("ghost"), 2, "orphan")];

        let (visible, _) = compute_visibility(&entries, false);
        assert!(visible.contains("orphan"));

        let ordered = tree_order(&entries, &visible);
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].0, 0); // rendered at depth 0
    }

    #[test]
    fn test_parent_cycle_terminates() {
        let a = entry("a", Some("b"), 1, "a");
        let b = entry("b", Some("a"), 1, "a");
        let entries = vec![a, b];

        let (visible, _) = compute_visibility(&entries, false);
        assert_eq!(visible.len(), 2);

        // Must terminate (visited set cuts the a→b→a cycle).
        let ordered = tree_order(&entries, &visible);
        assert_eq!(ordered.len(), 2);
    }

    #[test]
    fn test_children_sort_alphabetically() {
        let entries = vec![
            entry("dev", None, 3, "dev"),
            entry("zeta", Some("dev"), 1, "dev"),
            entry("alpha", Some("dev"), 1, "dev"),
        ];

        let (visible, _) = compute_visibility(&entries, false);
        let ordered = tree_order(&entries, &visible);
        let names: Vec<&str> = ordered.iter().map(|(_, e)| e.name.as_str()).collect();
        assert_eq!(names, vec!["dev", "alpha", "zeta"]);
    }

    // -------------------------------------------------------------------------
    // Integration Tests (require temp repository)
    // -------------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_list_default_view() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and drop to release lock
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List views (default = verbose)
        let cmd = List::new();
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_multiple_views() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create some views, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_view("feature-a").unwrap();
            repo.create_view("feature-b").unwrap();
            repo.create_view("release-1.0").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List views
        let cmd = List::new();
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_verbose() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a view, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_view("feature").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List views with verbose output (same as default now)
        let cmd = List::new().with_verbose(true);
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_shows_current_marker() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create views, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_view("other").unwrap();
            // Verify current view is dev
            assert_eq!(repo.current_view(), "dev");
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List views - should mark "dev" as current
        let cmd = List::new();
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_after_switch() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a view, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_view("feature").unwrap();
            repo.align_to_view("feature").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List views - should mark "feature" as current
        let cmd = List::new();
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify we're still on feature
        let repo = Repository::open(repo_path).unwrap();
        assert_eq!(repo.current_view(), "feature");
    }
}
