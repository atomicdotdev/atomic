//! Content revise must preserve canonical history, scope and working bytes.
use atomic_core::{change::ChangeHeader, types::Hash};
use atomic_repository::{RecordOptions, Repository, ViewEntryKind};
use std::{collections::BTreeMap, fs};

fn record(repo: &Repository, message: &str) -> Hash {
    let wc = repo.require_working_copy_id().unwrap();
    for entry in repo.status(wc, Default::default()).unwrap().entries() {
        if entry.status() == atomic_repository::FileStatus::Untracked {
            repo.add(wc, entry.path(), Default::default()).unwrap();
        }
    }
    *repo
        .record(
            repo.require_working_copy_id().unwrap(),
            ChangeHeader::builder().message(message).build(),
            RecordOptions::default().include_untracked(true),
        )
        .unwrap()
        .hash()
}

fn write(repo: &Repository, path: &str, content: &str) {
    let file = repo.root().join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, content).unwrap();
}

fn recorded(repo: &Repository) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    repo.materialize_view_entries::<()>(repo.current_view(), |entry| {
        if entry.kind != ViewEntryKind::Directory {
            files.insert(entry.path, entry.content);
        }
        Ok(())
    })
    .unwrap()
    .unwrap();
    files
}

#[test]
fn revising_an_add_keeps_independent_changes_and_excludes_scratch_after_reopen() {
    for reverse in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(root.path()).unwrap();
        write(&repo, "src/first.txt", "one\n");
        let target = record(&repo, "first");
        let names = if reverse {
            ["third.txt", "second.txt"]
        } else {
            ["second.txt", "third.txt"]
        };
        let mut pending = Vec::new();
        for name in names {
            write(&repo, name, name);
            pending.push(record(&repo, name));
        }
        write(&repo, "src/first.txt", "one edited\n");
        write(&repo, "src/scratch.txt", "private scratch\n");
        write(&repo, "scratch.txt", "unrelated\n");
        let outcome = repo
            .revise_content(&target, "revised", None, vec![])
            .unwrap();
        assert_eq!(outcome.reinserted, pending);
        let mut expected = vec![outcome.new_hash];
        expected.extend(pending);
        assert_eq!(
            repo.get_view_changes(None)
                .unwrap()
                .into_iter()
                .map(|(_, hash)| hash)
                .collect::<Vec<_>>(),
            expected
        );
        let files = recorded(&repo);
        assert_eq!(files.len(), 3);
        assert_eq!(files["src/first.txt"], b"one edited\n");
        assert_eq!(files["second.txt"], b"second.txt");
        assert_eq!(
            fs::read(root.path().join("src/scratch.txt")).unwrap(),
            b"private scratch\n"
        );
        // An empty view removes src itself; its normal safety guard refuses a
        // nonempty untracked directory. Move this fixture file to the root
        // after verifying revise left it untouched, then test the round trip.
        fs::rename(
            root.path().join("src/scratch.txt"),
            root.path().join("kept-scratch.txt"),
        )
        .unwrap();
        let view = repo.current_view().to_string();
        repo.create_view_with_identity("empty", atomic_core::pristine::ViewScope::Shared, None)
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.switch_view(wc, "empty").unwrap();
        repo.switch_view(wc, &view).unwrap();
        assert_eq!(recorded(&repo), files);
        assert_eq!(
            fs::read(root.path().join("kept-scratch.txt")).unwrap(),
            b"private scratch\n"
        );
        drop(repo);
        let repo = Repository::open(root.path()).unwrap();
        assert_eq!(recorded(&repo), files);
        let state = repo
            .working_copy_record(repo.require_working_copy_id().unwrap())
            .unwrap();
        assert_eq!(
            state.desired_state,
            repo.get_view_info(repo.current_view()).unwrap().state
        );
    }
}

#[test]
fn revising_to_an_empty_change_preserves_independent_history_and_working_bytes() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "first.txt", "one\n");
    let target = record(&repo, "first");
    write(&repo, "other.txt", "independent\n");
    let pending = record(&repo, "later");
    fs::remove_file(root.path().join("first.txt")).unwrap();
    write(&repo, "scratch.txt", "keep\n");
    let result = repo
        .revise_content(&target, "removed effect", None, vec![])
        .unwrap();
    assert_eq!(result.reinserted, vec![pending]);
    assert!(repo
        .load_change(&result.new_hash)
        .unwrap()
        .hunks()
        .is_empty());
    assert_eq!(recorded(&repo).len(), 1);
    assert!(!root.path().join("first.txt").exists());
    assert_eq!(
        fs::read(root.path().join("scratch.txt")).unwrap(),
        b"keep\n"
    );
    drop(repo);
    let repo = Repository::open(root.path()).unwrap();
    assert_eq!(recorded(&repo).len(), 1);
}

#[test]
fn overlapping_dependent_edits_report_a_real_conflict_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "first.txt", "one\n");
    let target = record(&repo, "first");
    write(&repo, "first.txt", "two\n");
    record(&repo, "dependent");
    write(&repo, "first.txt", "three\n");
    record(&repo, "transitive dependent");
    let history = repo.get_view_changes(None).unwrap();
    let files = recorded(&repo);
    write(&repo, "first.txt", "user edit\n");
    let error = repo
        .revise_content(&target, "revised", None, vec![])
        .unwrap_err();
    assert!(
        error.to_string().contains("revision content conflict"),
        "{error}"
    );
    assert_eq!(repo.get_view_changes(None).unwrap(), history);
    assert_eq!(recorded(&repo), files);
    assert_eq!(
        fs::read(root.path().join("first.txt")).unwrap(),
        b"user edit\n"
    );
}

#[test]
fn dependent_and_transitive_edits_replay_without_reviving_old_history() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    let base = "first\nseparator one\nsecond\nseparator two\nthird\nseparator three\nfourth\n";
    write(&repo, "file.txt", base);
    let target = record(&repo, "target");
    let second = base.replace("second", "SECOND");
    write(&repo, "file.txt", &second);
    let dependent = record(&repo, "dependent");
    let third = second.replace("third", "THIRD");
    write(&repo, "file.txt", &third);
    let transitive = record(&repo, "transitive");
    write(&repo, "other.txt", "independent\n");
    let independent = record(&repo, "independent");
    let view = repo.current_view().to_string();
    repo.create_view_with_identity("original", atomic_core::pristine::ViewScope::Shared, None)
        .unwrap();
    for (_, hash) in repo.get_view_changes(None).unwrap() {
        repo.insert_change(
            &hash,
            atomic_repository::InsertOptions::default().view("original"),
        )
        .unwrap();
    }
    let old_history = repo.get_view_changes(Some("original")).unwrap();
    let final_bytes = third.replace("first", "FIRST");
    write(&repo, "file.txt", &final_bytes);
    write(&repo, "other.txt", "unselected dirty bytes\n");
    write(&repo, "scratch.txt", "private\n");
    let result = repo
        .revise_content(&target, "revised", None, vec!["file.txt".into()])
        .unwrap();
    assert_ne!(result.new_hash, target);
    assert_ne!(result.reinserted[0], dependent);
    assert_ne!(result.reinserted[1], transitive);
    assert_eq!(result.reinserted[2], independent);
    let files = recorded(&repo);
    assert_eq!(files["file.txt"], final_bytes.as_bytes());
    assert_eq!(files["other.txt"], b"independent\n");
    assert_eq!(
        fs::read(root.path().join("other.txt")).unwrap(),
        b"unselected dirty bytes\n"
    );
    assert_eq!(
        fs::read(root.path().join("scratch.txt")).unwrap(),
        b"private\n"
    );
    assert_eq!(
        repo.get_view_changes(Some("original")).unwrap(),
        old_history
    );
    let mut closure = vec![result.new_hash];
    closure.extend(result.reinserted.iter().copied());
    let mut seen = std::collections::HashSet::new();
    while let Some(hash) = closure.pop() {
        assert!(![target, dependent, transitive].contains(&hash));
        if seen.insert(hash) {
            closure.extend(repo.load_change(&hash).unwrap().dependencies());
        }
    }
    for (hash, message) in result
        .reinserted
        .iter()
        .zip(["dependent", "transitive", "independent"])
    {
        assert_eq!(
            repo.load_change(hash).unwrap().hashed.header.message,
            message
        );
    }
    write(&repo, "other.txt", "independent\n");
    let wc = repo.require_working_copy_id().unwrap();
    repo.switch_view(wc, "original").unwrap();
    assert_eq!(
        fs::read(root.path().join("file.txt")).unwrap(),
        third.as_bytes()
    );
    repo.switch_view(wc, &view).unwrap();
    assert_eq!(recorded(&repo), files);
    drop(repo);
    let repo = Repository::open(root.path()).unwrap();
    assert_eq!(recorded(&repo), files);
    repo.repair_native_derived_indexes().unwrap();
    assert_eq!(recorded(&repo), files);
}

#[test]
fn later_rename_and_edit_replay_using_the_current_name_and_stable_inode() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    let base = "first\nseparator one\nsecond\nseparator two\nthird\n";
    write(&repo, "old.txt", base);
    record(&repo, "base");
    let inode = repo.get_file_inode("old.txt").unwrap();
    let target_bytes = base.replace("second", "SECOND");
    write(&repo, "old.txt", &target_bytes);
    let target = record(&repo, "target edit");
    fs::rename(root.path().join("old.txt"), root.path().join("new.txt")).unwrap();
    repo.move_file(
        repo.require_working_copy_id().unwrap(),
        "old.txt",
        "new.txt",
    )
    .unwrap();
    let rename = record(&repo, "later rename");
    let later = target_bytes.replace("SECOND", "SECOND later");
    write(&repo, "new.txt", &later);
    let edit = record(&repo, "later edit");
    let desired = later.replace("first", "FIRST");
    write(&repo, "new.txt", &desired);
    let result = repo
        .revise_content(&target, "revised", None, vec!["new.txt".into()])
        .unwrap();
    assert_eq!(result.reinserted.len(), 2);
    assert_ne!(result.reinserted[1], edit);
    assert!(repo.has_change(&rename));
    assert_eq!(repo.get_file_inode("new.txt").unwrap(), inode);
    let files = recorded(&repo);
    assert!(!files.contains_key("old.txt"));
    assert_eq!(files["new.txt"], desired.as_bytes());
    assert!(repo
        .status(repo.require_working_copy_id().unwrap(), Default::default())
        .unwrap()
        .entries()
        .iter()
        .all(|e| !e.status().is_dirty()));
}

#[test]
fn explicit_new_file_does_not_capture_other_untracked_files() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "first.txt", "one\n");
    let target = record(&repo, "first");
    write(&repo, "include.txt", "include\n");
    write(&repo, "scratch.txt", "keep\n");
    repo.revise_content(&target, "revised", None, vec!["include.txt".into()])
        .unwrap();
    let files = recorded(&repo);
    assert_eq!(files.len(), 2);
    assert_eq!(files["first.txt"], b"one\n");
    assert_eq!(files["include.txt"], b"include\n");
    assert_eq!(
        fs::read(root.path().join("scratch.txt")).unwrap(),
        b"keep\n"
    );
}

#[test]
fn revising_a_recorded_rename_preserves_inode_and_old_path_absence() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "old.txt", "one\ntwo\nthree\nfour\n");
    record(&repo, "base");
    let wc = repo.require_working_copy_id().unwrap();
    let inode = repo.get_file_inode("old.txt").unwrap();
    fs::rename(root.path().join("old.txt"), root.path().join("new.txt")).unwrap();
    repo.move_file(wc, "old.txt", "new.txt").unwrap();
    let target = record(&repo, "rename");
    write(&repo, "other.txt", "independent\n");
    let pending = record(&repo, "later");
    write(&repo, "new.txt", "completely replaced content\n");
    let outcome = repo
        .revise_content(&target, "rename revised", None, vec![])
        .unwrap();
    assert_eq!(outcome.reinserted, vec![pending]);
    assert_eq!(repo.get_file_inode("new.txt").unwrap(), inode);
    let files = recorded(&repo);
    assert!(!files.contains_key("old.txt"));
    assert_eq!(files["new.txt"], b"completely replaced content\n");
}

#[test]
fn revising_a_deletion_keeps_the_file_deleted() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "deleted.txt", "one\n");
    record(&repo, "base");
    fs::remove_file(root.path().join("deleted.txt")).unwrap();
    let target = record(&repo, "delete");
    write(&repo, "other.txt", "independent\n");
    record(&repo, "later");
    repo.revise_content(&target, "delete revised", None, vec![])
        .unwrap();
    assert!(!recorded(&repo).contains_key("deleted.txt"));
    assert!(!root.path().join("deleted.txt").exists());
}

#[test]
fn revised_add_cannot_hide_conflict_markers_as_a_temporary_untracked_file() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "first.txt", "one\n");
    let target = record(&repo, "first");
    let history = repo.get_view_changes(None).unwrap();
    let conflict = ">>>>>>> 1 [ABCDEF12]\nours\n======= 1\ntheirs\n<<<<<<< 1\n";
    write(&repo, "first.txt", conflict);
    let error = repo
        .revise_content(&target, "revised", None, vec![])
        .unwrap_err();
    assert!(error.to_string().contains("conflict"), "{error}");
    assert_eq!(repo.get_view_changes(None).unwrap(), history);
    assert_eq!(
        fs::read(root.path().join("first.txt")).unwrap(),
        conflict.as_bytes()
    );
}

#[test]
fn reinsertion_gate_refusal_leaves_the_original_history_intact() {
    use atomic_core::change::{AITool, AIVendor, Provenance};
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "first.txt", "one\n");
    let target = record(&repo, "first");
    let wc = repo.require_working_copy_id().unwrap();
    write(&repo, "managed.txt", "two\n");
    repo.add(wc, "managed.txt", Default::default()).unwrap();
    let mut provenance =
        Provenance::new(AIVendor::OpenAI, "fixture", AITool::Other("fixture".into()));
    provenance.session_id = Some("unattested-session".into());
    repo.record(
        wc,
        ChangeHeader::builder().message("managed pending").build(),
        RecordOptions::default().add_provenance(provenance),
    )
    .unwrap();
    let history = repo.get_view_changes(None).unwrap();
    write(&repo, "first.txt", "user edit\n");
    let error = repo
        .revise_content(&target, "revised", None, vec![])
        .unwrap_err();
    assert!(
        matches!(
            error,
            atomic_repository::RepositoryError::PublicationGateRefused { .. }
        ),
        "{error}"
    );
    assert_eq!(repo.get_view_changes(None).unwrap(), history);
    assert_eq!(
        fs::read(root.path().join("first.txt")).unwrap(),
        b"user edit\n"
    );
}

#[test]
fn the_same_independent_imports_revise_identically_in_both_orders_and_after_repair() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "first.txt", "one\n");
    let target = record(&repo, "first");
    write(&repo, "second.txt", "two\n");
    let second = record(&repo, "second");
    write(&repo, "third.txt", "three\n");
    let third = record(&repo, "third");
    let wc = repo.require_working_copy_id().unwrap();
    let mut projections = Vec::new();
    for (view, pending) in [("order-a", [second, third]), ("order-b", [third, second])] {
        repo.create_view_with_identity(view, atomic_core::pristine::ViewScope::Shared, None)
            .unwrap();
        repo.insert_change(
            &target,
            atomic_repository::InsertOptions::default().view(view),
        )
        .unwrap();
        for hash in pending {
            repo.insert_change(
                &hash,
                atomic_repository::InsertOptions::default().view(view),
            )
            .unwrap();
        }
        repo.switch_view(wc, view).unwrap();
        write(&repo, "first.txt", "revised\n");
        let outcome = repo
            .revise_content(&target, "revised", None, vec![])
            .unwrap();
        assert_eq!(outcome.reinserted, pending);
        let files = recorded(&repo);
        repo.repair_native_derived_indexes().unwrap();
        assert_eq!(recorded(&repo), files);
        projections.push(files);
    }
    assert_eq!(projections[0], projections[1]);
}

#[test]
fn reword_replays_dependencies_and_ignores_dirty_working_bytes() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "file.txt", "one\n");
    let target = record(&repo, "target");
    write(&repo, "file.txt", "two\n");
    let pending = record(&repo, "pending");
    write(&repo, "file.txt", "unrecorded edit\n");
    let outcome = repo.reword_change(&target, "new message", None).unwrap();
    assert_ne!(outcome.reinserted[0], pending);
    assert_eq!(recorded(&repo)["file.txt"], b"two\n");
    assert_eq!(
        fs::read(root.path().join("file.txt")).unwrap(),
        b"unrecorded edit\n"
    );
    let revised = repo.load_change(&outcome.new_hash).unwrap();
    assert_eq!(revised.hashed.header.message, "new message");
    assert_eq!(revised.hunks(), repo.load_change(&target).unwrap().hunks());
    assert!(!repo
        .get_view_changes(None)
        .unwrap()
        .iter()
        .any(|(_, h)| *h == target || *h == pending));
}

#[test]
fn raw_rename_during_revision_is_recorded_without_capturing_scratch() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "old.txt", "unchanged rename content\n");
    record(&repo, "base");
    let inode = repo.get_file_inode("old.txt").unwrap();
    write(&repo, "other.txt", "target\n");
    let target = record(&repo, "target");
    fs::rename(root.path().join("old.txt"), root.path().join("new.txt")).unwrap();
    write(&repo, "scratch.txt", "do not include\n");
    repo.revise_content(&target, "revised", None, vec![])
        .unwrap();
    assert_eq!(repo.get_file_inode("new.txt").unwrap(), inode);
    let files = recorded(&repo);
    assert_eq!(files.len(), 2);
    assert!(!files.contains_key("old.txt"));
    assert_eq!(files["new.txt"], b"unchanged rename content\n");
}

#[test]
fn crlf_filter_and_nested_new_file_survive_dependent_revision() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, ".gitattributes", "*.txt text eol=crlf\n");
    record(&repo, "filter");
    let base = "first\r\nseparator\r\nlast\r\n";
    write(&repo, "file.txt", base);
    let target = record(&repo, "target");
    write(&repo, "file.txt", &base.replace("last", "LAST"));
    record(&repo, "dependent");
    let expected = base.replace("first", "FIRST").replace("last", "LAST");
    write(&repo, "file.txt", &expected);
    write(&repo, "nested/new.txt", "nested\r\n");
    repo.revise_content(
        &target,
        "revised",
        None,
        vec!["file.txt".into(), "nested/new.txt".into()],
    )
    .unwrap();
    let files = recorded(&repo);
    assert_eq!(files["file.txt"], expected.as_bytes());
    assert_eq!(files["nested/new.txt"], b"nested\r\n");
    assert_eq!(
        fs::read(root.path().join("file.txt")).unwrap(),
        expected.as_bytes()
    );
}

#[test]
fn dependent_replay_preserves_authors_description_metadata_and_provenance() {
    use atomic_core::change::{AITool, AIVendor, Author, Provenance};
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "file.txt", "first\nseparator\nlast\n");
    let target = record(&repo, "target");
    write(&repo, "file.txt", "first\nseparator\nLAST\n");
    let header = ChangeHeader::builder()
        .message("dependent")
        .description("keep description")
        .author(Author::new("one", None::<String>))
        .author(Author::new("two", None::<String>))
        .build();
    let provenance = Provenance::new(AIVendor::OpenAI, "fixture", AITool::Other("fixture".into()));
    let pending = *repo
        .record(
            repo.require_working_copy_id().unwrap(),
            header.clone(),
            RecordOptions::default()
                .metadata_bytes(b"application metadata".to_vec())
                .add_provenance(provenance),
        )
        .unwrap()
        .hash();
    write(&repo, "file.txt", "FIRST\nseparator\nLAST\n");
    let outcome = repo
        .revise_content(&target, "revised", None, vec![])
        .unwrap();
    let original = repo.load_change(&pending).unwrap();
    let revised = repo.load_change(&outcome.reinserted[0]).unwrap();
    assert_eq!(revised.hashed.header, header);
    assert_eq!(revised.hashed.metadata, original.hashed.metadata);
    assert_eq!(revised.hashed.provenance, original.hashed.provenance);
}

#[cfg(unix)]
#[test]
fn executable_and_symlink_survive_reword_and_content_replay() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "script.sh", "first\nseparator\nlast\n");
    fs::set_permissions(
        root.path().join("script.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("script.sh", root.path().join("link")).unwrap();
    let target = record(&repo, "target");
    write(&repo, "script.sh", "first\nseparator\nLAST\n");
    record(&repo, "dependent");
    let result = repo.reword_change(&target, "reworded", None).unwrap();
    write(&repo, "script.sh", "FIRST\nseparator\nLAST\n");
    repo.revise_content(&result.new_hash, "revised", None, vec![])
        .unwrap();
    let mut entries = BTreeMap::new();
    repo.materialize_view_entries::<()>(repo.current_view(), |entry| {
        entries.insert(entry.path.clone(), entry);
        Ok(())
    })
    .unwrap()
    .unwrap();
    assert_eq!(entries["script.sh"].mode, 0o755);
    assert_eq!(entries["link"].kind, ViewEntryKind::Symlink);
    assert_eq!(entries["link"].content, b"script.sh");
}

#[test]
fn verified_independent_session_survives_but_rewriting_requires_new_evidence() {
    use atomic_core::change::{
        envelope::SessionEnvelope, AITool, AIVendor, AttestAgent, Attestation, Provenance,
        ProvenanceGraphBuilder,
    };
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    write(&repo, "target.txt", "target\n");
    let target = record(&repo, "target");
    write(&repo, "managed.txt", "managed\n");
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "managed.txt",
        Default::default(),
    )
    .unwrap();
    let mut provenance =
        Provenance::new(AIVendor::OpenAI, "fixture", AITool::Other("fixture".into()));
    provenance.session_id = Some("revise-session".into());
    let envelope = SessionEnvelope::builder("revise-session", "fixture")
        .build()
        .encode()
        .unwrap();
    let managed = *repo
        .record(
            repo.require_working_copy_id().unwrap(),
            ChangeHeader::builder().message("managed").build(),
            RecordOptions::default()
                .metadata_bytes(envelope)
                .add_provenance(provenance),
        )
        .unwrap()
        .hash();
    let graph = ProvenanceGraphBuilder::new("revise-session", "fixture")
        .changes_explained(vec![managed])
        .build();
    repo.save_provenance_graph(&graph).unwrap();
    let key = "test-only-revise-session-mac";
    let mut attestation = Attestation::builder(
        "revise-session",
        AttestAgent::new("fixture", "Fixture", "test"),
    )
    .changes_covered(vec![managed])
    .build();
    attestation.sign_with_mac(key);
    repo.save_attestation(&attestation).unwrap();
    fs::create_dir_all(repo.dot_dir().join("sessions")).unwrap();
    fs::write(
        repo.dot_dir().join("sessions/revise-session.json"),
        serde_json::to_vec(&serde_json::json!({"mac_key": key})).unwrap(),
    )
    .unwrap();
    let config_path = repo.dot_dir().join("config.toml");
    let mut config = atomic_config::RepoConfig::load(&config_path).unwrap();
    config.git.trust.signers = vec!["session-mac:revise-session".into()];
    config.save(&config_path).unwrap();
    write(&repo, "target.txt", "revised\n");
    let outcome = repo
        .revise_content(&target, "revised", None, vec![])
        .unwrap();
    assert_eq!(outcome.reinserted, vec![managed]);
    let history = repo.get_view_changes(None).unwrap();
    // The old attestation cannot authorize a new immutable managed hash.
    let error = repo
        .reword_change(&managed, "rewritten managed", None)
        .unwrap_err();
    assert!(
        matches!(
            error,
            atomic_repository::RepositoryError::PublicationGateRefused { .. }
        ),
        "{error}"
    );
    assert_eq!(repo.get_view_changes(None).unwrap(), history);
    assert_eq!(recorded(&repo)["managed.txt"], b"managed\n");
}

#[test]
fn revising_an_add_rewrites_the_later_rename_and_its_content_dependency() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    let base = "first\nseparator\nlast\n";
    write(&repo, "old.txt", base);
    let target = record(&repo, "add");
    fs::rename(root.path().join("old.txt"), root.path().join("new.txt")).unwrap();
    repo.move_file(
        repo.require_working_copy_id().unwrap(),
        "old.txt",
        "new.txt",
    )
    .unwrap();
    let rename = record(&repo, "rename");
    let tip = base.replace("last", "LAST");
    write(&repo, "new.txt", &tip);
    let edit = record(&repo, "edit renamed file");
    let desired = tip.replace("first", "FIRST");
    write(&repo, "new.txt", &desired);
    let outcome = repo
        .revise_content(&target, "revised add", None, vec![])
        .unwrap();
    assert_ne!(outcome.reinserted[0], rename);
    assert_ne!(outcome.reinserted[1], edit);
    let replacement = repo.load_change(&outcome.reinserted[0]).unwrap();
    assert!(replacement
        .hunks()
        .iter()
        .any(|op| matches!(op, atomic_core::change::GraphOp::FileMove { .. })));
    assert!(!replacement.hunks().iter().any(|op| matches!(
        op,
        atomic_core::change::GraphOp::FileAdd { .. } | atomic_core::change::GraphOp::FileDel { .. }
    )));
    let files = recorded(&repo);
    assert_eq!(files.len(), 1);
    assert_eq!(files["new.txt"], desired.as_bytes());
    let view = repo.current_view().to_string();
    repo.create_view_with_identity(
        "replacement-add",
        atomic_core::pristine::ViewScope::Shared,
        None,
    )
    .unwrap();
    repo.insert_change(
        &outcome.new_hash,
        atomic_repository::InsertOptions::default().view("replacement-add"),
    )
    .unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let renamed_inode = repo.get_file_inode("new.txt").unwrap();
    repo.switch_view(wc, "replacement-add").unwrap();
    assert_eq!(repo.get_file_inode("old.txt").unwrap(), renamed_inode);
    repo.switch_view(wc, &view).unwrap();
    assert_eq!(recorded(&repo), files);
}
