use super::*;
use crate::ai::tools::{RepoToolExecutor, ToolCall, ToolExecutor};
use crate::{ContentSearchOptions, CrossViewInsertOptions, RecordOptions};
use atomic_core::kg::{iri, ontology::Class};
use atomic_core::pristine::vault::KgNode;
use atomic_core::pristine::{KgMutTxnT, ViewTxnT};

#[test]
fn narrow_query_retrieves_only_selected_bytes_and_retains_only_display_limit() {
    let (temp, repo) = create_temp_repo();
    for (path, content) in [
        ("src/selected.rs", "needle\n".repeat(100)),
        ("src/other.rs", "needle\n".repeat(100)),
        ("src-other/other.rs", "needle\n".repeat(100)),
        ("src/notes.md", "needle\n".repeat(100)),
    ] {
        let full = temp.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
        repo.add(path, TrackingOptions::default()).unwrap();
    }
    record(&repo, "record selective query fixture");
    let (result, resources) = crate::query_diagnostics::measure(|| {
        repo.search_content_paths_on_view(
            "dev",
            "needle",
            ContentSearchOptions {
                max_results: Some(1),
                ..Default::default()
            },
            &["src/selected.rs".into()],
        )
        .unwrap()
    });
    assert_eq!(result.total_matches, 100);
    assert_eq!(result.matches.len(), 1);
    assert_eq!(resources.files_retrieved, 1, "{resources:?}");
    assert!(resources.displayed_records_high_water <= 1, "{resources:?}");

    let (scope, resources) = crate::query_diagnostics::measure(|| repo.query_view("dev").unwrap());
    assert_eq!(
        resources.files_retrieved, 0,
        "metadata projection must be lazy"
    );
    assert_eq!(scope.nodes().len(), 4);
    drop(scope);
    for (paths, glob, kind, exclude, expected) in [
        (vec!["src/"], None, None, None, 3),
        (
            vec!["src", "src/", "src/selected.rs"],
            Some("**/*.rs"),
            Some("rs"),
            None,
            2,
        ),
        (vec!["src"], None, None, Some("rs"), 1),
        (vec!["src/selected.rs"], Some("**/*.md"), None, None, 0),
        (vec!["src/selected"], None, None, None, 0),
        (vec!["missing"], None, None, None, 0),
        (vec!["."], None, None, None, 0),
        (vec!["/src"], None, None, None, 0),
        (vec!["../src"], None, None, None, 0),
    ] {
        let (result, resources) = crate::query_diagnostics::measure(|| {
            repo.search_content_paths_on_view(
                "dev",
                "needle",
                ContentSearchOptions {
                    path_filter: glob.map(str::to_owned),
                    file_type: kind.map(str::to_owned),
                    exclude_type: exclude.map(str::to_owned),
                    max_results: Some(0),
                    ..Default::default()
                },
                &paths
                    .iter()
                    .map(|path| (*path).to_owned())
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        });
        assert_eq!(result.total_matches, expected * 100, "paths={paths:?}");
        assert_eq!(resources.files_retrieved, expected, "paths={paths:?}");
        assert_eq!(resources.displayed_records_high_water, 0);
    }
    std::fs::write(temp.path().join(".atomicignore"), "src/selected.rs\n").unwrap();
    let (result, resources) = crate::query_diagnostics::measure(|| {
        repo.search_content_paths_on_view(
            "dev",
            "needle",
            ContentSearchOptions::default(),
            &["src/selected.rs".into()],
        )
        .unwrap()
    });
    assert_eq!(
        result.total_matches, 0,
        "existing ambient ignore behavior is intentional"
    );
    assert_eq!(resources.files_retrieved, 0);
}

#[test]
fn selected_content_errors_propagate_but_excluded_content_is_not_read() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("good.rs"), "needle\n").unwrap();
    repo.add("good.rs", TrackingOptions::default()).unwrap();
    record(&repo, "good source");
    std::fs::write(temp.path().join("bad.rs"), "other\n").unwrap();
    repo.add("bad.rs", TrackingOptions::default()).unwrap();
    let bad = record(&repo, "bad source");
    // Reopen to eliminate cached payloads before corrupting a disposable store.
    drop(repo);
    let repo = Repository::open(temp.path()).unwrap();
    std::fs::write(
        repo.change_store().change_path(&bad),
        b"invalid stored payload",
    )
    .unwrap();
    let selected = |path: &str, pattern: &str, limit| {
        repo.search_content_paths_on_view(
            "dev",
            pattern,
            ContentSearchOptions {
                max_results: Some(limit),
                ..Default::default()
            },
            &[path.into()],
        )
    };
    assert_eq!(selected("good.rs", "needle", 1).unwrap().total_matches, 1);
    assert!(selected("bad.rs", "other", 1).is_err());
    assert!(selected("bad.rs", "other", 0).is_err());
    assert!(selected("missing", "[", 0).is_err());
    assert!(repo
        .vault_kg_search_on_view("dev", "other", 10, None)
        .is_err());
}

#[test]
fn recorded_query_plan_keeps_one_snapshot_across_later_records() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("source.rs"), "before_marker\n").unwrap();
    repo.add("source.rs", TrackingOptions::default()).unwrap();
    record(&repo, "before snapshot");
    let scope = repo.query_view("dev").unwrap();
    std::fs::write(temp.path().join("source.rs"), "after_marker\n").unwrap();
    record(&repo, "after snapshot");
    let old = scope
        .search_content(
            "before_marker|after_marker",
            ContentSearchOptions::default(),
        )
        .unwrap();
    assert_eq!(old.matches[0].line_content, "before_marker");
    let new = repo
        .search_content_on_view(
            "dev",
            "before_marker|after_marker",
            ContentSearchOptions::default(),
        )
        .unwrap();
    assert_eq!(new.matches[0].line_content, "after_marker");
}

#[test]
fn bounded_retrieval_matches_full_rendering_for_linear_and_deleted_graphs() {
    use super::super::content::{retrieve_content_bounded, BoundedContent};
    let (temp, mut repo) = create_temp_repo();
    std::fs::write(temp.path().join("source.rs"), "first\nsecond\nthird\n").unwrap();
    repo.add("source.rs", TrackingOptions::default()).unwrap();
    record(&repo, "linear source");
    for revision in ["first\nsecond\nthird\n", "first\nthird\n"] {
        if revision == "first\nthird\n" {
            std::fs::write(temp.path().join("source.rs"), revision).unwrap();
            record(&repo, "delete middle line");
        }
        let txn = repo.pristine().read_txn().unwrap();
        let inode = txn.get_inode("source.rs").unwrap().unwrap();
        let position = txn.inode_position(inode).unwrap().unwrap();
        let view = txn.get_view("dev").unwrap().unwrap();
        let visibility = graph_visibility_closure(&txn, &view).unwrap();
        let options =
            atomic_core::output::alive::RetrieveOptions::new().with_graph_visibility(visibility);
        let (full, _, had_fork) = atomic_core::output::repo::output_file_to_buffer_with_options(
            &txn,
            repo.change_store(),
            position,
            Default::default(),
            options.clone(),
        )
        .unwrap();
        assert_eq!(full, revision.as_bytes());
        for cap in [0, 1, full.len() - 1, full.len(), full.len() + 1] {
            let bounded = atomic_core::output::repo::output_file_to_buffer_bounded(
                &txn,
                repo.change_store(),
                position,
                options.clone(),
                cap,
            )
            .unwrap();
            assert_eq!(bounded.had_fork_structure, had_fork);
            assert!(bounded.buffer_high_water <= cap);
            assert_eq!(
                bounded.content.as_deref(),
                (cap >= full.len()).then_some(full.as_slice())
            );
            match retrieve_content_bounded(
                &txn,
                repo.change_store(),
                inode,
                position,
                options.clone(),
                cap,
            )
            .unwrap()
            {
                BoundedContent::Complete(bytes) => assert_eq!(bytes, full),
                BoundedContent::Oversized { retained_bytes } => {
                    assert!(cap < full.len());
                    assert!(retained_bytes <= cap);
                }
            }
        }
    }
    repo.create_view_from("bounded-side", "dev").unwrap();
    repo.switch_view("bounded-side").unwrap();
    std::fs::write(temp.path().join("source.rs"), "first\nside\nthird\n").unwrap();
    record(&repo, "side insertion");
    repo.switch_view("dev").unwrap();
    std::fs::write(temp.path().join("source.rs"), "first\nmain\nthird\n").unwrap();
    record(&repo, "main insertion");
    repo.insert_from_view(CrossViewInsertOptions::new("bounded-side", "dev"))
        .unwrap();
    let txn = repo.pristine().read_txn().unwrap();
    let inode = txn.get_inode("source.rs").unwrap().unwrap();
    let position = txn.inode_position(inode).unwrap().unwrap();
    let view = txn.get_view("dev").unwrap().unwrap();
    let options = atomic_core::output::alive::RetrieveOptions::new()
        .with_graph_visibility(graph_visibility_closure(&txn, &view).unwrap());
    let (full, _, had_fork) = atomic_core::output::repo::output_file_to_buffer_with_options(
        &txn,
        repo.change_store(),
        position,
        Default::default(),
        options.clone(),
    )
    .unwrap();
    assert!(had_fork, "fixture must exercise semantic fork resolution");
    for cap in [0, full.len() - 1, full.len(), full.len() + 1] {
        let bounded = atomic_core::output::repo::output_file_to_buffer_bounded(
            &txn,
            repo.change_store(),
            position,
            options.clone(),
            cap,
        )
        .unwrap();
        assert_eq!(bounded.had_fork_structure, had_fork);
        assert_eq!(
            bounded.content.as_deref(),
            (cap >= full.len()).then_some(full.as_slice())
        );
        match retrieve_content_bounded(
            &txn,
            repo.change_store(),
            inode,
            position,
            options.clone(),
            cap,
        )
        .unwrap()
        {
            BoundedContent::Complete(bytes) => assert_eq!(bytes, full),
            BoundedContent::Oversized { retained_bytes } => {
                assert!(cap < full.len());
                assert!(retained_bytes <= cap);
            }
        }
    }
}

#[test]
fn kg_content_window_and_nonfile_path_scoring_are_preserved() {
    let (temp, repo) = create_temp_repo();
    for (path, lines) in [("src/a.rs", 1999), ("src/z.rs", 2), ("tests/a.rs", 10)] {
        let full = temp.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, "windowmarker\n".repeat(lines)).unwrap();
        repo.add(path, TrackingOptions::default()).unwrap();
    }
    record(&repo, "promotion window fixture");
    let project = repo.project_id().unwrap();
    let memory_id = iri::mint_memory(&project, "pathless");
    let path_id = iri::mint_goal(&project, "withpath");
    let mut txn = repo.pristine().write_txn().unwrap();
    txn.upsert_kg_node(&KgNode::new(
        &memory_id,
        Class::Memory,
        "windowmarker",
        "vault",
    ))
    .unwrap();
    txn.upsert_kg_node(
        &KgNode::new(&path_id, Class::Goal, "windowmarker", "vault")
            .with_type(Class::Memory)
            .with_metadata(serde_json::json!({"path": "src/z.rs"})),
    )
    .unwrap();
    txn.commit().unwrap();
    let nodes = repo
        .vault_kg_search_on_view("dev", "windowmarker", 20, None)
        .unwrap();
    assert_eq!(
        nodes
            .iter()
            .find(|node| node.id == iri::mint_file(&project, "src/a.rs"))
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()["content_matches"],
        1999
    );
    assert_eq!(
        nodes
            .iter()
            .find(|node| node.id == iri::mint_file(&project, "src/z.rs"))
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()["content_matches"],
        1
    );
    assert!(!nodes
        .iter()
        .any(|node| node.id == iri::mint_file(&project, "tests/a.rs")));
    let filtered = repo
        .vault_kg_search_on_view_with_kind(
            "dev",
            "windowmarker",
            20,
            None,
            Some(Class::Memory.iri()),
        )
        .unwrap();
    assert_eq!(filtered[0].id, path_id);
    assert_eq!(filtered[0].metadata.as_ref().unwrap()["content_matches"], 1);
    let (nodes, resources) = crate::query_diagnostics::measure(|| {
        repo.vault_kg_search_on_view_with_kind(
            "dev",
            "windowmarker",
            20,
            None,
            Some(Class::View.iri()),
        )
        .unwrap()
    });
    assert!(nodes.is_empty());
    assert_eq!(resources.files_retrieved, 0);
    // A genuinely vault-only match cannot receive a content score.
    let mut txn = repo.pristine().write_txn().unwrap();
    txn.upsert_kg_node(&KgNode::new(
        &memory_id,
        Class::Memory,
        "vaultonlymarker",
        "vault",
    ))
    .unwrap();
    txn.commit().unwrap();
    let (nodes, resources) = crate::query_diagnostics::measure(|| {
        repo.vault_kg_search_on_view_with_kind(
            "dev",
            "vaultonlymarker",
            1,
            Some(1),
            Some(Class::Memory.iri()),
        )
        .unwrap()
    });
    assert_eq!(nodes[0].id, memory_id);
    assert_eq!(resources.files_retrieved, 0);
}

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
fn query_search_filters_before_ranking_and_uses_recorded_sources() {
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
        .query_callers_on_view("dev", "src/lib.rs#target")
        .unwrap()
        .into_iter()
        .find(|caller| caller.name == "caller")
        .unwrap();
    assert_eq!(target.summary.as_deref(), Some("pub fn caller()"));
    assert!(!repo
        .vault_kg_search_on_view("dev", "VisibleBaseAuthor", 1, None)
        .unwrap()
        .is_empty());
    // A low result limit is applied only after filtering hidden candidates.
    assert!(!repo
        .vault_kg_search_on_view("dev", "inherited", 1, Some(1))
        .unwrap()
        .is_empty());
    let memory_id = iri::mint_memory(&repo.project_id().unwrap(), "shared");
    let mut txn = repo.pristine().write_txn().unwrap();
    txn.upsert_kg_node(&KgNode::new(
        &memory_id,
        Class::Memory,
        "universal_marker",
        "vault",
    ))
    .unwrap();
    txn.commit().unwrap();
    assert_eq!(
        repo.vault_kg_search_on_view("dev", "universal_marker", 1, None)
            .unwrap()[0]
            .id,
        memory_id
    );
}

#[test]
fn query_callers_uses_recorded_calls_not_stale_shared_edges_or_disk() {
    let (temp, repo, _) = fixture();
    let target = "src/lib.rs#target";
    let callers = |view: &str| {
        repo.query_callers_on_view(view, target)
            .unwrap()
            .into_iter()
            .map(|caller| format!("{}#{}:{}", caller.path, caller.name, caller.line))
            .collect::<Vec<_>>()
    };
    assert_eq!(callers("dev"), ["src/lib.rs#caller:2"]);
    assert_eq!(callers("feature"), ["src/sibling.rs#sibling_only:1"]);
    std::fs::write(temp.path().join("src/lib.rs"), "pub fn disk_only() {}\n").unwrap();
    assert_eq!(callers("dev"), ["src/lib.rs#caller:2"]);
    assert!(repo
        .query_callers_on_view("dev", "src/sibling.rs#sibling_only")
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

fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: name.into(),
        arguments,
    }
}

#[test]
fn agent_code_search_tool_reads_recorded_view_without_a_content_index() {
    let (temp, repo) = create_temp_repo();
    assert!(
        !temp.path().join(".atomic/content-index").exists(),
        "the fixture must not ship a content index"
    );
    std::fs::write(temp.path().join("needle.rs"), "needle here\nneedle again\n").unwrap();
    repo.add("needle.rs", TrackingOptions::default()).unwrap();
    record(&repo, "needle source");
    let executor = RepoToolExecutor::new(&repo);
    let response = executor
        .execute(&tool_call(
            "code_search",
            serde_json::json!({"pattern": "needle"}),
        ))
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(parsed["total_matches"], 2);
    assert_eq!(parsed["showing"], 2);
    assert_eq!(parsed["matches"][0]["path"], "needle.rs");
    assert_eq!(parsed["matches"][0]["line_number"], 1);
    // Ambient ignore rules still suppress recorded paths through the tool.
    std::fs::write(temp.path().join(".atomicignore"), "needle.rs\n").unwrap();
    let response = executor
        .execute(&tool_call(
            "code_search",
            serde_json::json!({"pattern": "needle"}),
        ))
        .unwrap();
    assert_eq!(response, "No matches found.");
}

#[test]
fn agent_code_search_tool_respects_the_desired_view() {
    let (temp, mut repo, _) = fixture();
    let matches = |repo: &TestRepository, pattern: &str| -> u64 {
        let executor = RepoToolExecutor::new(repo);
        let response = executor
            .execute(&tool_call(
                "code_search",
                serde_json::json!({"pattern": pattern}),
            ))
            .unwrap();
        // The tool reports an empty result set as a plain sentence.
        if response == "No matches found." {
            return 0;
        }
        serde_json::from_str::<serde_json::Value>(&response).unwrap()["total_matches"]
            .as_u64()
            .unwrap()
    };
    // The fixture's working copy desires `feature`, whose bytes are on disk.
    assert_eq!(matches(&repo, "sibling_marker"), 1);
    // Repoint the desired view at `dev` without materializing: feature-only
    // content must leave the results even though its bytes remain on disk,
    // and inherited content must stay.
    repo.set_current_view("dev").unwrap();
    assert_eq!(matches(&repo, "sibling_marker"), 0);
    assert_eq!(matches(&repo, "inherited_marker"), 1);
    // The disk still holds the feature bytes, proving the answer came from
    // the graph rather than the working copy.
    assert!(std::fs::read_to_string(temp.path().join("src/sibling.rs"))
        .unwrap()
        .contains("sibling_marker"));
}

#[test]
fn agent_kg_search_tool_respects_the_desired_view() {
    let (_temp, mut repo, _) = fixture();
    let search = |repo: &TestRepository, query: &str| -> Vec<serde_json::Value> {
        let executor = RepoToolExecutor::new(repo);
        let response = executor
            .execute(&tool_call(
                "kg_search",
                serde_json::json!({"query": query, "limit": 10}),
            ))
            .unwrap();
        serde_json::from_str(&response).unwrap()
    };
    // The feature change's provenance nodes are reachable through `feature`.
    assert!(!search(&repo, "sibling_history_marker").is_empty());
    // Through `dev` the same nodes fall outside the view's closure.
    repo.set_current_view("dev").unwrap();
    assert!(search(&repo, "sibling_history_marker").is_empty());
}

#[test]
fn agent_code_search_matches_indexed_search_on_a_synchronized_copy() {
    let (temp, repo, _) = fixture();
    // The fixture leaves the working copy synchronized with `feature` and the
    // content index built from those exact bytes.
    let opts = ContentSearchOptions::default();
    let graph = repo
        .search_content_on_view("feature", "target|marker", opts.clone())
        .unwrap();
    let indexed =
        crate::content_search::search_content(temp.path(), "target|marker", opts).unwrap();
    assert_eq!(
        serde_json::to_value(&graph).unwrap(),
        serde_json::to_value(&indexed).unwrap(),
        "graph-backed and index-backed search must agree on a synchronized copy"
    );
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
        .query_callers_on_view("empty", "src/lib.rs#target")
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
            .query_callers_on_view(view, "src/lib.rs#target")
            .unwrap()
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
        .any(|node| node.id == iri::mint_file(&repo.project_id().unwrap(), "renamed.rs")));
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
        repo.search_content_paths_on_view(
            "feature",
            "sibling",
            ContentSearchOptions {
                max_results: Some(0),
                ..Default::default()
            },
            &["missing/file.rs".into()],
        )
        .unwrap_err(),
        repo.query_callers_on_view("feature", "src/lib.rs#target")
            .unwrap_err(),
        repo.vault_kg_search_on_view_with_kind(
            "feature",
            "sibling",
            0,
            Some(0),
            Some("urn:unknown:class"),
        )
        .unwrap_err(),
    ];
    for error in errors {
        assert!(error.to_string().contains(&missing.to_base32()), "{error}");
    }
}

#[test]
fn query_kind_filter_is_pool_independent_and_matches_every_asserted_type() {
    let (temp, repo) = create_temp_repo();
    let marker = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    repo.vault_store(
        &format!("memory/{marker}.md"),
        atomic_core::pristine::VaultEntryType::Memory,
        b":::memory\nPool regression.\n:::\n".to_vec(),
        serde_json::json!({"id": marker, "kind": "context"}).to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    for index in 0..8 {
        // Both filename and content match, so FTS/projected/content candidates
        // must be deduplicated and must not displace an eligible Memory.
        let path = format!("src/{marker}_{index}.rs");
        std::fs::write(temp.path().join(&path), format!("// {marker}\n")).unwrap();
        repo.add(&path, TrackingOptions::default()).unwrap();
    }
    record(&repo, "record pool files");
    let project = repo.project_id().unwrap();
    let memory_id = iri::mint_memory(&project, marker);
    let mut txn = repo.pristine().write_txn().unwrap();
    for index in 0..4 {
        txn.upsert_kg_node(
            &KgNode::new(
                iri::mint_goal(&project, &format!("pool-{index}")),
                Class::Goal,
                marker,
                "vault",
            )
            .with_type(Class::Memory),
        )
        .unwrap();
    }
    txn.commit().unwrap();
    let exhaustive = repo
        .vault_kg_search_on_view("dev", marker, usize::MAX, None)
        .unwrap();
    let expected_memories: Vec<_> = exhaustive
        .iter()
        .filter(|node| node.has_type(Class::Memory))
        .cloned()
        .collect();
    assert_eq!(expected_memories.len(), 5);
    assert!(expected_memories.iter().any(|node| node.id == memory_id));
    assert!(expected_memories
        .iter()
        .any(|node| node.primary_type() != Class::Memory.iri()));

    for limit in [0, 1, 3, 20] {
        for pool in [None, Some(0), Some(1), Some(3), Some(100)] {
            for kind in [
                None,
                Some(""),
                Some(Class::Memory.iri()),
                Some(Class::File.iri()),
                Some("Memory"),
                Some("https://atomic.dev/ns/workflow#memory"),
                Some("urn:unknown:class"),
            ] {
                let actual = repo
                    .vault_kg_search_on_view_with_kind("dev", marker, limit, pool, kind)
                    .unwrap();
                let expected: Vec<_> = if let Some(kind) = kind.filter(|kind| !kind.is_empty()) {
                    exhaustive
                        .iter()
                        .filter(|node| node.types.iter().any(|ty| ty == kind))
                        .take(limit)
                        .cloned()
                        .collect()
                } else {
                    repo.vault_kg_search_on_view("dev", marker, limit, pool)
                        .unwrap()
                };
                assert_eq!(
                    actual, expected,
                    "limit={limit}, pool={pool:?}, kind={kind:?}"
                );
                let ids: std::collections::HashSet<_> =
                    actual.iter().map(|node| &node.id).collect();
                assert_eq!(ids.len(), actual.len(), "duplicate results: {actual:?}");
            }
        }
    }
    for (view, query) in [("dev", "no_matching_token"), ("missing", marker)] {
        for limit in [0, 3] {
            for kind in [Class::Memory.iri(), "urn:unknown:class"] {
                let result =
                    repo.vault_kg_search_on_view_with_kind(view, query, limit, Some(0), Some(kind));
                if view == "missing" {
                    assert!(result.is_err(), "unknown view must not short-circuit");
                } else {
                    assert!(result.unwrap().is_empty());
                }
            }
        }
    }
}

#[test]
fn query_kind_filter_preserves_view_projection_before_type_matching() {
    let (temp, mut repo, feature_hash) = fixture();
    repo.create_draft_view("child-filtered", "feature").unwrap();
    repo.create_view_with_identity("tip-filtered", ViewScope::Shared, None)
        .unwrap();
    let mut txn = repo.pristine().write_txn().unwrap();
    let mut view = txn.get_view("tip-filtered").unwrap().unwrap();
    let id = txn.get_internal(&feature_hash).unwrap().unwrap();
    txn.put_change(&mut view, id, &feature_hash).unwrap();
    txn.update_view(&view).unwrap();
    txn.commit().unwrap();
    let before = std::fs::read(temp.path().join("src/lib.rs")).unwrap();
    for view in ["dev", "feature", "child-filtered", "tip-filtered"] {
        for query in [
            "sibling",
            "SiblingSecret",
            "HiddenSiblingAuthor",
            "inherited",
        ] {
            let exhaustive = repo
                .vault_kg_search_on_view(view, query, 100, None)
                .unwrap();
            for class in [Class::File, Class::Change, Class::Identity, Class::View] {
                let expected: Vec<_> = exhaustive
                    .iter()
                    .filter(|node| node.has_type(class))
                    .take(1)
                    .cloned()
                    .collect();
                let actual = repo
                    .vault_kg_search_on_view_with_kind(view, query, 1, Some(1), Some(class.iri()))
                    .unwrap();
                assert_eq!(
                    actual, expected,
                    "view={view}, query={query}, class={class:?}"
                );
            }
        }
    }
    assert_eq!(
        std::fs::read(temp.path().join("src/lib.rs")).unwrap(),
        before
    );
    assert_eq!(
        repo.desired_view_name(repo.working_copy()).unwrap(),
        "feature"
    );
}

#[test]
fn query_no_kind_ranking_and_diversity_retain_their_golden_order() {
    let (_temp, repo) = create_temp_repo();
    repo.init_kg().unwrap();
    let project = repo.project_id().unwrap();
    let mut goals = Vec::new();
    let mut memories = Vec::new();
    let mut txn = repo.pristine().write_txn().unwrap();
    for index in 0..4 {
        let id = iri::mint_goal(&project, &format!("golden-{index}"));
        txn.upsert_kg_node(&KgNode::new(&id, Class::Goal, "goldenmarker", "vault"))
            .unwrap();
        goals.push(id);
    }
    for index in 0..2 {
        let id = iri::mint_memory(&project, &format!("golden-{index}"));
        txn.upsert_kg_node(&KgNode::new(&id, Class::Memory, "goldenmarker", "vault"))
            .unwrap();
        memories.push(id);
    }
    txn.commit().unwrap();
    for (pool, expected) in [
        (None, goals.clone()),
        (Some(0), goals.clone()),
        (Some(4), goals.clone()),
        (Some(100), [goals[..2].to_vec(), memories].concat()),
    ] {
        let plain = repo
            .vault_kg_search_on_view("dev", "goldenmarker", 4, pool)
            .unwrap();
        assert_eq!(
            plain.iter().map(|node| node.id.clone()).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            repo.vault_kg_search("goldenmarker", 4, pool).unwrap(),
            plain
        );
        for kind in [None, Some("")] {
            assert_eq!(
                repo.vault_kg_search_on_view_with_kind("dev", "goldenmarker", 4, pool, kind)
                    .unwrap(),
                plain,
            );
        }
    }
    assert_eq!(
        repo.vault_kg_search_by_kind("goldenmarker", 4, Class::Goal)
            .unwrap()
            .iter()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>(),
        goals,
    );
}
