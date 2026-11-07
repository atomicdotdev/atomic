//! Opt-in source preparation for the repository's real-history replay sample.
//! Uses the existing shim parser/preflight/writer, not another graph compiler.
//! Preparation includes per-commit publication: never hide it in speed claims.

use std::io::{BufRead, Write};

use super::*;

const PINNED_HYPERFINE: &str = "c5161524980522320c7d3425e714cc0965a869a5";

thread_local! {
    static DEFER_SOURCE_CONTENT_PROOF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn content_verification_deferred() -> bool {
    DEFER_SOURCE_CONTENT_PROOF.with(std::cell::Cell::get)
}

fn with_source_proof_scope<T>(defer: bool, work: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            DEFER_SOURCE_CONTENT_PROOF.with(|flag| flag.set(self.0));
        }
    }
    let _reset = Reset(DEFER_SOURCE_CONTENT_PROOF.with(|flag| flag.replace(defer)));
    work()
}

#[test]
fn source_proof_override_is_scoped_and_restored() {
    assert!(!content_verification_deferred());
    with_source_proof_scope(true, || {
        assert!(content_verification_deferred());
        with_source_proof_scope(false, || assert!(!content_verification_deferred()));
        assert!(content_verification_deferred());
    });
    assert!(!content_verification_deferred());
}

fn assert_no_checkout(root: &Path) {
    for entry in fs::read_dir(root).unwrap() {
        assert_eq!(
            entry.unwrap().file_name(),
            ".atomic",
            "source preparation materialized a working file"
        );
    }
}

#[test]
#[ignore = "manual pinned real-Git sample; requires ATOMIC_REAL_SAMPLE_GIT and ATOMIC_REAL_SAMPLE_ARTIFACT"]
fn prepare_hyperfine_publication_sample() {
    let git_path = PathBuf::from(
        std::env::var_os("ATOMIC_REAL_SAMPLE_GIT").expect("set read-only Hyperfine source"),
    );
    let artifact = PathBuf::from(
        std::env::var_os("ATOMIC_REAL_SAMPLE_ARTIFACT")
            .expect("set a fresh disposable artifact directory"),
    );
    let count: usize = std::env::var("ATOMIC_REAL_SAMPLE_COMMITS")
        .unwrap_or_else(|_| "100".into())
        .parse()
        .unwrap();
    let budget = Duration::from_secs(
        std::env::var("ATOMIC_REAL_SAMPLE_BUDGET_SECONDS")
            .unwrap_or_else(|_| "120".into())
            .parse()
            .unwrap(),
    );
    assert!((1..=1097).contains(&count));
    let defer_source = match std::env::var("ATOMIC_REAL_SAMPLE_SOURCE_VERIFICATION").as_deref() {
        Ok("defer-final") => true,
        Ok("each-commit") | Err(_) => false,
        Ok(other) => panic!("unknown source proof scope {other}"),
    };
    fs::create_dir_all(&artifact).unwrap();
    let resume = std::env::var("ATOMIC_REAL_SAMPLE_RESUME").as_deref() == Ok("1");
    let previous: Option<serde_json::Value> = if resume {
        let bytes = fs::read(artifact.join("preparation.json")).unwrap();
        let previous: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(previous["pinned_tip"], PINNED_HYPERFINE);
        assert_eq!(previous["requested"].as_u64(), Some(count as u64));
        assert_eq!(previous["status"], "incomplete");
        assert_eq!(
            previous["full_per_commit_verification"].as_bool().unwrap(),
            !defer_source
        );
        assert!(
            previous["error"]
                .as_str()
                .unwrap()
                .contains("budget exhausted"),
            "only resume a clean budget stop, not an arbitrary failed mutation"
        );
        let attempt = previous["attempts"].as_u64().unwrap_or(1);
        let snapshot = artifact.join(format!("preparation-attempt-{attempt}.json"));
        if !snapshot.exists() {
            fs::write(snapshot, bytes).unwrap();
        }
        Some(previous)
    } else {
        assert!(
            !artifact.join("source").exists(),
            "never overwrite an earlier experiment"
        );
        None
    };
    let git = GitRepository::open(&git_path).unwrap();
    let pinned = git
        .find_commit(Oid::from_str(PINNED_HYPERFINE).unwrap())
        .unwrap();
    let mut repo = if resume {
        Repository::open(artifact.join("source"))
    } else {
        Repository::init(artifact.join("source"))
    }
    .unwrap();
    let view = repo.current_view().to_owned();
    assert_ne!(
        git.head()
            .ok()
            .and_then(|head| head.shorthand().ok().map(str::to_owned))
            .as_deref(),
        Some(view.as_str()),
        "this ODB-only sample must not claim to import the source's checked-out branch"
    );
    let mut importer = ParallelImporter::new(
        &git,
        ParallelImportOptions {
            target_view: view.clone(),
            repo_name: "hyperfine-real-sample".into(),
            mainline_only: true,
            graph_only: false,
            validate_equivalence: true,
            verify_touched_paths: false,
            ..Default::default()
        },
    );
    let all = importer
        .collect_commit_oids_from_tip(&git, pinned.id())
        .unwrap();
    assert_eq!(
        all.len(),
        1097,
        "the pinned source's first-parent history changed"
    );
    let selected_tip = all[count - 1];
    let prior_completed = previous.as_ref().map_or(0, |previous| {
        previous["completed"].as_u64().unwrap() as usize
    });
    if resume {
        assert_eq!(
            repo.effective_history(Some(&view)).unwrap().len(),
            prior_completed
        );
        let rows: Vec<_> =
            std::io::BufReader::new(fs::File::open(artifact.join("commits.jsonl")).unwrap())
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(&line.unwrap()).unwrap())
                .collect();
        assert_eq!(rows.len(), prior_completed);
        importer.options.incremental = true;
        for (index, row) in rows.iter().enumerate() {
            let sha = row["sha"].as_str().unwrap();
            assert_eq!(all[index].to_string(), sha);
            let hash = repo.checked_git_sha(sha).unwrap().unwrap();
            assert_eq!(hash.to_base32(), row["hash"].as_str().unwrap());
            importer.options.imported_shas.insert(sha.into());
        }
        assert_no_checkout(repo.root());
    }
    let mut manifest = fs::OpenOptions::new()
        .write(true)
        .append(resume)
        .create_new(!resume)
        .open(artifact.join("commits.jsonl"))
        .unwrap();
    let started = Instant::now();
    let mut completed = prior_completed;
    let mut merges = previous.as_ref().map_or(0, |previous| {
        previous["mainline_merges"].as_u64().unwrap() as usize
    });
    let mut preflight_ms = 0.0;
    let result: CliResult<()> = (|| {
        let plan = importer.validate_branch_prospectively_for_tip(
            &view,
            Some(&repo),
            Some(selected_tip),
        )?;
        preflight_ms = started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(plan.commits.len(), count - prior_completed);
        let boundaries = plan.boundaries.clone();
        let mut ledger = ClosureLedger::default();
        ledger.target_closure = Some(ledger.ancestry_of(&git, &selected_tip.to_string())?);
        let mut line_index = ImportLineIndex::default();
        for candidate in plan.commits {
            if started.elapsed() > budget {
                return Err(git_error(format!(
                    "source preparation budget exhausted after {completed}/{count} commits"
                )));
            }
            let parsed = &candidate.parsed;
            let info = with_source_proof_scope(defer_source, || {
                importer.write_commit(
                    &mut repo,
                    &git,
                    &mut ledger,
                    parsed,
                    &mut line_index,
                    &candidate.verified,
                    &candidate.prospective,
                    &boundaries,
                    false,
                )
            })?;
            let change = repo
                .load_change(&info.atomic_hash)
                .map_err(CliError::from)?;
            let semantic_paths: Vec<_> = change
                .file_ops()
                .iter()
                .map(|ops| ops.path().to_string())
                .collect();
            let expectation =
                staged_expectation_for(parsed, &candidate.prospective, &semantic_paths, None);
            let tree = expectation.tree.unwrap();
            let entries: Vec<_> = tree
                .entries
                .iter()
                .filter(|_| !defer_source || completed + 1 == count)
                .map(|(path, entry)| {
                    serde_json::json!({
                        "path": path, "bytes": entry.bytes, "mode": entry.mode, "kind": entry.kind,
                    })
                })
                .collect();
            let deleted_paths: Vec<_> = change
                .hunks()
                .iter()
                .filter_map(|op| match op {
                    GraphOp::FileDel { path, .. } => Some(path.clone()),
                    _ => None,
                })
                .collect();
            serde_json::to_writer(
                &mut manifest,
                &serde_json::json!({
                    "sha": parsed.git_sha, "hash": info.atomic_hash.to_base32(),
                    "entries": entries, "semantic_paths": tree.semantic_paths,
                    "deleted_paths": deleted_paths,
                }),
            )
            .map_err(|error| CliError::Internal(error.into()))?;
            writeln!(&mut manifest).map_err(|error| CliError::Internal(error.into()))?;
            manifest
                .flush()
                .map_err(|error| CliError::Internal(error.into()))?;
            completed += 1;
            merges += usize::from(parsed.is_merge);
            assert_no_checkout(repo.root());
            if completed % 10 == 0 || completed == count {
                println!(
                    "HYPERFINE prepare commits={completed}/{count} elapsed_ms={:.3}",
                    started.elapsed().as_secs_f64() * 1000.0
                );
            }
            if manifest.metadata().unwrap().len() > 512 * 1024 * 1024 {
                return Err(git_error(
                    "sample expectation spool exceeds 512 MiB; no streaming-memory claim is made",
                ));
            }
        }
        Ok(())
    })();
    let summary = serde_json::json!({
        "status": if result.is_ok() { "complete" } else { "incomplete" },
        "error": result.as_ref().err().map(ToString::to_string),
        "pinned_tip": PINNED_HYPERFINE, "selected_tip": selected_tip.to_string(),
        "available_first_parent_commits": all.len(), "requested": count, "completed": completed,
        "mainline_merges": merges, "view": view,
        "preflight_ms": preflight_ms + previous.as_ref().map_or(0.0, |previous| previous["preflight_ms"].as_f64().unwrap()),
        "source_preparation_ms": started.elapsed().as_secs_f64() * 1000.0 + previous.as_ref().map_or(0.0, |previous| previous["source_preparation_ms"].as_f64().unwrap()),
        "attempt_ms": started.elapsed().as_secs_f64() * 1000.0,
        "attempts": previous.as_ref().map_or(1, |previous| previous["attempts"].as_u64().unwrap_or(1) + 1),
        "preparation_includes_per_commit_publication": true,
        "full_per_commit_verification": !defer_source, "materialized": false,
        "source_content_proof": if defer_source { "deferred-to-private-final-promotion" } else { "each-commit" },
        "expectation_scope": if defer_source { "final-tip-only" } else { "each-commit" },
        "build_label": std::env::var("ATOMIC_REAL_SAMPLE_BUILD_LABEL").unwrap_or_else(|_| "unspecified test build".into()),
        "manifest_bytes": manifest.metadata().unwrap().len(),
    });
    fs::write(
        artifact.join("preparation.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    let snapshot = artifact.join(format!(
        "preparation-attempt-{}.json",
        summary["attempts"].as_u64().unwrap()
    ));
    assert!(!snapshot.exists());
    fs::write(snapshot, serde_json::to_vec_pretty(&summary).unwrap()).unwrap();
    println!("HYPERFINE preparation {summary}");
    assert_no_checkout(repo.root());
    result.unwrap();
}
