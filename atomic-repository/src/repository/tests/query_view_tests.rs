use super::*;
use crate::{ContentSearchOptions, RecordOptions};
use atomic_core::pristine::vault::KgNode;
use atomic_core::pristine::{KgMutTxnT, ViewTxnT};

fn record(repo: &TestRepository, message: &str) -> Hash {
    let author = if message == "sibling_history_marker" {
        "HiddenSiblingAuthor"
    } else {
        "VisibleBaseAuthor"
    };
    *repo
        .record(
            ChangeHeader::builder()
                .message(message)
                .author(Author::new(author, None::<String>))
                .build(),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap()
        .hash()
}

fn fixture() -> (TempDir, TestRepository, Hash) {
    let (temp, mut repo) = create_temp_repo();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn target() {}\npub fn caller() { target(); }\n// inherited_marker\n",
    )
    .unwrap();
    repo.add("src/lib.rs", TrackingOptions::default()).unwrap();
    record(&repo, "inherited history");
    repo.kg_enrich_from_vcs(repo.working_copy()).unwrap();
    repo.create_draft_view("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    std::fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn target(secret: SiblingSecret) {}\npub fn caller() {}\n// inherited_marker\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("src/sibling.rs"),
        "pub fn sibling_only() { target(); }\n// sibling_marker\n",
    )
    .unwrap();
    repo.add("src/sibling.rs", TrackingOptions::default())
        .unwrap();
    let feature = record(&repo, "sibling_history_marker");
    repo.kg_enrich_from_vcs(repo.working_copy()).unwrap();
    crate::build_content_index(temp.path()).unwrap();
    (temp, repo, feature)
}

#[test]
fn query_search_filters_before_ranking_and_reprojects_same_id_entities() {
    let (_temp, repo, _) = fixture();
    for query in [
        "sibling",
        "SiblingSecret",
        "sibling_history_marker",
        "HiddenSiblingAuthor",
    ] {
        let nodes = repo
            .vault_kg_search_on_view("dev", query, 1, Some(1))
            .unwrap();
        assert!(nodes.is_empty(), "{query}: {nodes:?}");
        assert!(!repo
            .vault_kg_search_on_view("feature", query, 20, Some(50))
            .unwrap()
            .is_empty());
    }
    let target = repo
        .vault_kg_search_on_view("dev", "target", 50, Some(100))
        .unwrap()
        .into_iter()
        .find(|node| node.id == "entity:src/lib.rs:target:1")
        .unwrap();
    assert_eq!(target.summary.as_deref(), Some("pub fn target()"));
    assert!(!repo
        .vault_kg_search_on_view("dev", "VisibleBaseAuthor", 1, None)
        .unwrap()
        .is_empty());
    // A low result limit is applied only after filtering hidden candidates.
    assert!(!repo
        .vault_kg_search_on_view("dev", "inherited", 1, Some(1))
        .unwrap()
        .is_empty());
    let mut txn = repo.pristine().write_txn().unwrap();
    txn.upsert_kg_node(&KgNode::new(
        "memory:shared",
        "memory",
        "universal_marker",
        "vault",
    ))
    .unwrap();
    txn.commit().unwrap();
    assert_eq!(
        repo.vault_kg_search_on_view("dev", "universal_marker", 1, None)
            .unwrap()[0]
            .id,
        "memory:shared"
    );
}

#[test]
fn query_callers_uses_recorded_calls_not_stale_shared_edges_or_disk() {
    let (temp, repo, _) = fixture();
    let target = "entity:src/lib.rs:target:1";
    let callers = |view: &str| {
        repo.vault_kg_callers_on_view(view, target)
            .unwrap()
            .edges
            .into_iter()
            .filter(|edge| edge.to_id == target)
            .map(|edge| edge.from_id)
            .collect::<Vec<_>>()
    };
    assert_eq!(callers("dev"), ["entity:src/lib.rs:caller:2"]);
    assert_eq!(callers("feature"), ["entity:src/sibling.rs:sibling_only:1"]);
    std::fs::write(temp.path().join("src/lib.rs"), "pub fn disk_only() {}\n").unwrap();
    assert_eq!(callers("dev"), ["entity:src/lib.rs:caller:2"]);
    assert!(repo
        .vault_kg_callers_on_view("dev", "entity:src/sibling.rs:sibling_only:1")
        .unwrap()
        .is_empty());
}

#[test]
fn query_code_ignores_stale_indexes_and_filters_counts_facets_and_limits() {
    let (temp, repo, _) = fixture();
    let opts = ContentSearchOptions {
        max_results: Some(1),
        file_type: Some("rs".into()),
        path_filter: Some("src/".into()),
        case_insensitive: true,
        ..Default::default()
    };
    let result = repo
        .search_content_on_view("dev", "TARGET|INHERITED_MARKER", opts.clone())
        .unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.total_matches, 3);
    assert_eq!(result.dir_facets, [("src/lib.rs".into(), 3)]);
    assert_eq!(result.matches[0].path, "src/lib.rs");
    assert_eq!(result.matches[0].line_number, 1);
    assert_eq!(result.matches[0].byte_offset, 7);
    assert_eq!(result.matches[0].submatch_start, 7);
    assert_eq!(result.matches[0].submatch_end, 13);
    for query in ["SiblingSecret", "sibling_marker", "disk_only"] {
        std::fs::write(temp.path().join("src/lib.rs"), "pub fn disk_only() {}\n").unwrap();
        let result = repo
            .search_content_on_view("dev", query, opts.clone())
            .unwrap();
        assert_eq!(result.total_matches, 0, "{query}");
        assert!(result.matches.is_empty());
        assert!(result.dir_facets.is_empty());
    }
    let result = repo
        .search_content_on_view("feature", "sibling_marker", opts)
        .unwrap();
    assert_eq!(result.matches[0].path, "src/sibling.rs");
    assert!(repo
        .search_content_on_view("dev", "[", ContentSearchOptions::default())
        .is_err());
}

#[test]
fn queries_include_ancestors_and_transitive_dependencies_without_materializing() {
    let (temp, mut repo, feature_hash) = fixture();
    repo.create_draft_view("child", "feature").unwrap();
    repo.create_draft_view("grandchild", "child").unwrap();
    repo.create_view_with_identity("empty", ViewScope::Shared, None)
        .unwrap();
    assert!(repo
        .vault_kg_search_on_view("empty", "target", 10, None)
        .unwrap()
        .is_empty());
    assert_eq!(
        repo.search_content_on_view("empty", "target", ContentSearchOptions::default())
            .unwrap()
            .total_matches,
        0
    );
    assert!(repo
        .vault_kg_callers_on_view("empty", "entity:src/lib.rs:target:1")
        .unwrap()
        .is_empty());
    repo.create_view_with_identity("tip-only", ViewScope::Shared, None)
        .unwrap();
    // Reference only the tip: its required base must be included by the
    // canonical dependency closure, not by direct membership or parent names.
    let mut txn = repo.pristine().write_txn().unwrap();
    let mut view = txn.get_view("tip-only").unwrap().unwrap();
    let id = txn.get_internal(&feature_hash).unwrap().unwrap();
    txn.put_change(&mut view, id, &feature_hash).unwrap();
    txn.update_view(&view).unwrap();
    txn.commit().unwrap();
    let before = std::fs::read(temp.path().join("src/lib.rs")).unwrap();
    for view in ["child", "grandchild", "tip-only"] {
        assert!(!repo
            .vault_kg_search_on_view(view, "inherited", 10, None)
            .unwrap()
            .is_empty());
        assert_eq!(
            repo.search_content_on_view(view, "inherited_marker", ContentSearchOptions::default())
                .unwrap()
                .total_matches,
            1
        );
        assert!(!repo
            .vault_kg_callers_on_view(view, "entity:src/lib.rs:target:1")
            .unwrap()
            .edges
            .is_empty());
    }
    assert_eq!(
        std::fs::read(temp.path().join("src/lib.rs")).unwrap(),
        before
    );
    assert_eq!(
        repo.desired_view_name(repo.working_copy()).unwrap(),
        "feature"
    );
    assert!(repo
        .vault_kg_search_on_view("missing", "target", 10, None)
        .is_err());
}

#[test]
fn queries_exclude_deleted_and_renamed_paths_without_an_index_rebuild() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("old.rs"), "pub fn renamed_marker() {}\n").unwrap();
    std::fs::write(
        temp.path().join("deleted.rs"),
        "pub fn deleted_marker() {}\n",
    )
    .unwrap();
    repo.add("old.rs", TrackingOptions::default()).unwrap();
    repo.add("deleted.rs", TrackingOptions::default()).unwrap();
    record(&repo, "before lifecycle changes");
    repo.kg_enrich_from_vcs(repo.working_copy()).unwrap();
    crate::build_content_index(temp.path()).unwrap();
    std::fs::rename(temp.path().join("old.rs"), temp.path().join("renamed.rs")).unwrap();
    std::fs::remove_file(temp.path().join("deleted.rs")).unwrap();
    record(&repo, "after lifecycle changes");
    let matches = repo
        .search_content_on_view("dev", "marker", ContentSearchOptions::default())
        .unwrap();
    assert_eq!(matches.total_matches, 1);
    assert_eq!(matches.matches[0].path, "renamed.rs");
    let nodes = repo
        .vault_kg_search_on_view("dev", "marker", 50, None)
        .unwrap();
    assert!(nodes
        .iter()
        .all(|node| !node.id.contains("old.rs") && !node.id.contains("deleted.rs")));
    assert!(repo
        .vault_kg_search_on_view("dev", "renamed.rs", 20, None)
        .unwrap()
        .iter()
        .any(|node| node.id == "file:renamed.rs"));
}

#[test]
fn invalid_dependency_closure_fails_closed_for_every_query() {
    let (_temp, repo, hash) = fixture();
    let missing = Hash::of(b"unregistered query dependency");
    let mut txn = repo.pristine().write_txn().unwrap();
    let id = txn.get_internal(&hash).unwrap().unwrap();
    txn.put_change_deps(id, &[missing]).unwrap();
    txn.commit().unwrap();
    let errors = [
        repo.vault_kg_search_on_view("feature", "sibling", 10, None)
            .unwrap_err(),
        repo.search_content_on_view("feature", "sibling", ContentSearchOptions::default())
            .unwrap_err(),
        repo.vault_kg_callers_on_view("feature", "entity:src/lib.rs:target:1")
            .unwrap_err(),
    ];
    for error in errors {
        assert!(error.to_string().contains(&missing.to_base32()), "{error}");
    }
}
