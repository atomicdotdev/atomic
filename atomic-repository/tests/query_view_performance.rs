//! Opt-in diagnostic, not a machine-dependent timing gate.
#![cfg(feature = "query-test-instrumentation")]

use atomic_core::change::ChangeHeader;
use atomic_repository::{ContentSearchOptions, RecordOptions, Repository, TrackingOptions};
use std::time::{Duration, Instant};

#[test]
#[ignore = "release diagnostic: 2000 recorded sources and five measured runs"]
fn recorded_query_resources() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    for index in 0..2000 {
        let path = format!("src/file_{index:04}.rs");
        let content = format!("// needle {index}\n{}\n", " ".repeat(4080));
        std::fs::write(dir.path().join(&path), content).unwrap();
        repo.add(wc, &path, TrackingOptions::default()).unwrap();
    }
    for (path, content) in [
        ("src/dense.rs", "needle\n".repeat(100_000)),
        ("cutoff.txt", "x".repeat(10 * 1024 * 1024)),
        ("oversized.txt", "x".repeat(10 * 1024 * 1024 + 1)),
    ] {
        std::fs::write(dir.path().join(path), content).unwrap();
        repo.add(wc, path, TrackingOptions::default()).unwrap();
    }
    repo.record(
        wc,
        ChangeHeader::new("query benchmark fixture"),
        RecordOptions::new()
            .with_all(true)
            .with_max_file_size(11 * 1024 * 1024)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
    let root = repo.desired_view_name(wc).unwrap();
    let mut parent = root;
    for depth in 0..20 {
        let child = format!("query-bench-{depth}");
        repo.create_draft_view(&child, &parent).unwrap();
        parent = child;
    }
    println!("fixture=2000 sources (~4KiB), 100000-line dense, cutoff/cutoff+1, 20 ancestors; setup excluded; warm ChangeStore/OS caches; no cache flush");
    for (label, paths) in [
        ("exact", vec!["src/file_1999.rs".into()]),
        ("prefix", vec!["src/".into()]),
        ("all", vec![]),
    ] {
        for limit in [0, 1, 50] {
            let run = || {
                repo.search_content_paths_on_view(
                    &parent,
                    "needle",
                    ContentSearchOptions {
                        max_results: Some(limit),
                        ..Default::default()
                    },
                    &paths,
                )
                .unwrap()
            };
            run();
            let mut durations = Vec::<Duration>::new();
            let mut last = None;
            for _ in 0..5 {
                let start = Instant::now();
                let (result, resources) = atomic_repository::query_diagnostics::measure(run);
                durations.push(start.elapsed());
                last = Some((result.total_matches, resources));
            }
            durations.sort();
            let (total, resources) = last.unwrap();
            println!(
                "{label} limit={limit} total={total} min={:?} median={:?} max={:?} resources={}",
                durations[0],
                durations[2],
                durations[4],
                serde_json::to_string(&resources).unwrap()
            );
            // Set QUERY_BENCH_BASELINE=1 only for the pre-fix diagnostic run.
            if std::env::var_os("QUERY_BENCH_BASELINE").is_none() {
                assert_eq!(total, if label == "exact" { 1 } else { 102_000 });
                assert_eq!(
                    resources.files_retrieved,
                    match label {
                        "exact" => 1,
                        "prefix" => 2001,
                        _ => 2003,
                    }
                );
                assert!(resources.output_buffer_high_water <= 10 * 1024 * 1024 + 1);
                assert!(resources.displayed_records_high_water <= limit);
            }
        }
    }
    let result = repo
        .search_content_paths_on_view(
            &parent,
            "^x",
            ContentSearchOptions {
                max_results: Some(1),
                ..Default::default()
            },
            &["cutoff.txt".into(), "oversized.txt".into()],
        )
        .unwrap();
    assert_eq!(
        result.total_matches, 1,
        "exact cutoff is searchable, cutoff+1 is not"
    );
    assert_eq!(result.matches[0].path, "cutoff.txt");
    assert_eq!(result.matches[0].line_content.len(), 10 * 1024 * 1024);

    // Agent tool path: the same recorded search reached through
    // `RepoToolExecutor::code_search`, with the view resolved from the
    // working copy's registered desired view. The tool adds one view-pointer
    // resolution and JSON serialization on top of the measured direct path,
    // so it must retrieve the same files and retain no more display records.
    repo.set_current_view(wc, &parent).unwrap();
    let executor = atomic_repository::ai::tools::RepoToolExecutor::new(&repo);
    let call = atomic_repository::ai::tools::ToolCall {
        id: "bench".into(),
        name: "code_search".into(),
        arguments: serde_json::json!({"pattern": "needle", "max_results": 50}),
    };
    let run_tool = || {
        atomic_repository::query_diagnostics::measure(|| {
            atomic_repository::ai::tools::ToolExecutor::execute(&executor, &call).unwrap()
        })
    };
    let (_response, _resources) = run_tool();
    let mut tool_durations = Vec::<Duration>::new();
    let mut last = None;
    for _ in 0..5 {
        let start = Instant::now();
        let (response, resources) = run_tool();
        tool_durations.push(start.elapsed());
        let total = serde_json::from_str::<serde_json::Value>(&response).unwrap()["total_matches"]
            .as_u64()
            .unwrap();
        assert_eq!(
            total, 102_000,
            "the tool path reports the same exhaustive total"
        );
        assert_eq!(
            resources.files_retrieved, 2003,
            "the tool path retrieves the same files as the direct path"
        );
        assert!(resources.displayed_records_high_water <= 50);
        last = Some(resources);
    }
    tool_durations.sort();
    println!(
        "tool limit=50 min={:?} median={:?} max={:?} resources={}",
        tool_durations[0],
        tool_durations[2],
        tool_durations[4],
        serde_json::to_string(&last.unwrap()).unwrap()
    );
}
