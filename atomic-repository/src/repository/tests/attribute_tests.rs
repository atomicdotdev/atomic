use super::*;
use crate::record::RecordOptions;
use crate::status::FileStatus;
use atomic_core::change::{GraphOp, InodeAttr, InodeKind};

fn record_all(repo: &TestRepository, message: &str) -> crate::record::RecordOutcome {
    repo.record(
        ChangeHeader::new(message),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap()
}

#[cfg(unix)]
#[test]
fn chmod_records_graph_attribute_and_materializes_mode() {
    use std::os::unix::fs::PermissionsExt;

    let (temp, repo) = create_temp_repo();
    let path = temp.path().join("script.sh");
    std::fs::write(&path, b"echo ok\n").unwrap();
    repo.add("script.sh", TrackingOptions::default()).unwrap();
    record_all(&repo, "add script");

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let status = repo.status(StatusOptions::default()).unwrap();
    assert_eq!(status.entries()[0].status(), FileStatus::PermissionsChanged);

    let outcome = record_all(&repo, "chmod script");
    assert!(outcome.change().hunks().iter().any(|operation| {
        matches!(
            operation,
            GraphOp::SetAttr {
                path,
                value: InodeAttr::Mode(0o755),
                ..
            } if path == "script.sh"
        )
    }));
    {
        use atomic_core::pristine::{InodeAttrTxnT, TreeTxnT, ViewTxnT};
        let txn = repo.pristine.read_txn().unwrap();
        let inode = txn.get_inode("script.sh").unwrap().unwrap();
        let position = txn.inode_position(inode).unwrap().unwrap();
        let view = txn.get_view("dev").unwrap().unwrap();
        let visibility = graph_visibility_closure(&txn, &view).unwrap();
        let visible = visibility.iter_dependency_first().copied().collect();
        assert_eq!(
            txn.resolve_inode_attr(position, atomic_core::change::InodeAttrName::Mode, &visible,)
                .unwrap()
                .value(),
            Some(InodeAttr::Mode(0o755))
        );
    }

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    repo.materialize_paths(std::collections::HashSet::from(["script.sh".to_string()]))
        .unwrap();
    assert_eq!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[cfg(unix)]
#[test]
fn concurrent_attribute_values_surface_as_status_conflict() {
    use atomic_core::pristine::{InodeAttrEvent, InodeAttrMutTxnT, MutTxnT, TreeTxnT, ViewTxnT};

    let (temp, repo) = create_temp_repo();
    let path = temp.path().join("conflicted");
    std::fs::write(&path, b"content").unwrap();
    repo.add("conflicted", TrackingOptions::default()).unwrap();
    record_all(&repo, "add conflicted file");

    let mut txn = repo.pristine.write_txn().unwrap();
    let inode = txn.get_inode("conflicted").unwrap().unwrap();
    let position = txn.inode_position(inode).unwrap().unwrap();
    let first_hash = atomic_core::Hash::of(b"concurrent mode one");
    let second_hash = atomic_core::Hash::of(b"concurrent mode two");
    let first = txn.register_change(&first_hash).unwrap();
    let second = txn.register_change(&second_hash).unwrap();
    txn.put_change_deps(first, &[]).unwrap();
    txn.put_change_deps(second, &[]).unwrap();
    txn.put_inode_attr_event(
        inode,
        position,
        InodeAttrEvent::new(first, InodeAttr::Mode(0o700)).unwrap(),
    )
    .unwrap();
    txn.put_inode_attr_event(
        inode,
        position,
        InodeAttrEvent::new(second, InodeAttr::Mode(0o755)).unwrap(),
    )
    .unwrap();
    let mut view = txn.get_view("dev").unwrap().unwrap();
    txn.put_change(&mut view, first, &first_hash).unwrap();
    txn.put_change(&mut view, second, &second_hash).unwrap();
    txn.update_view(&view).unwrap();
    txn.commit().unwrap();

    let status = repo.status(StatusOptions::default()).unwrap();
    let entry = status
        .entries()
        .iter()
        .find(|entry| entry.path() == std::path::Path::new("conflicted"))
        .unwrap();
    assert_eq!(entry.status(), FileStatus::Conflicted);
    assert_eq!(entry.details(), Some("inode attribute conflict"));
}

#[cfg(unix)]
#[test]
fn gitlink_lifecycle_switches_exact_payload_without_filter_corruption() {
    let (temp, mut repo) = create_temp_repo();
    let path = temp.path().join("dependency");
    let regular = b"regular baseline\n";
    let object_id = b"0123456789abcdef0123456789abcdef01234567";

    std::fs::write(&path, regular).unwrap();
    repo.add("dependency", TrackingOptions::default()).unwrap();
    record_all(&repo, "add regular dependency path");
    repo.create_view_from("feature", "dev").unwrap();

    repo.switch_view("feature").unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join(".git"), object_id).unwrap();
    let status = repo.status(StatusOptions::default()).unwrap();
    assert_eq!(status.entries()[0].status(), FileStatus::TypeChanged);

    let outcome = record_all(&repo, "convert dependency to gitlink");
    assert!(outcome.change().hunks().iter().any(|operation| {
        matches!(
            operation,
            GraphOp::SetAttr {
                path,
                value: InodeAttr::Kind(InodeKind::Gitlink),
                ..
            } if path == "dependency"
        )
    }));
    assert_eq!(
        repo.get_file_content_on_view("dependency", "feature")
            .unwrap(),
        Some(object_id.to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("dependency", "dev").unwrap(),
        Some(regular.to_vec())
    );

    repo.switch_view("dev").unwrap();
    assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
    assert_eq!(std::fs::read(&path).unwrap(), regular);
    assert_eq!(
        repo.get_file_content_on_view("dependency", "feature")
            .unwrap(),
        Some(object_id.to_vec())
    );

    repo.switch_view("feature").unwrap();
    assert!(std::fs::symlink_metadata(&path).unwrap().is_dir());
    assert_eq!(std::fs::read(path.join(".git")).unwrap(), object_id);
    let status = repo.status(StatusOptions::default()).unwrap();
    assert!(
        status.is_clean(),
        "unexpected status: {:?}",
        status.entries()
    );

    repo.switch_view("dev").unwrap();
    assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
    assert_eq!(std::fs::read(&path).unwrap(), regular);
}

#[cfg(unix)]
#[test]
fn regular_to_dangling_symlink_records_type_and_materializes_target() {
    let (temp, repo) = create_temp_repo();
    let path = temp.path().join("link");
    std::fs::write(&path, b"regular").unwrap();
    repo.add("link", TrackingOptions::default()).unwrap();
    record_all(&repo, "add regular");

    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("missing-target", &path).unwrap();
    let status = repo.status(StatusOptions::default()).unwrap();
    assert_eq!(status.entries()[0].status(), FileStatus::TypeChanged);

    let outcome = record_all(&repo, "make symlink");
    assert!(outcome.change().hunks().iter().any(|operation| {
        matches!(
            operation,
            GraphOp::SetAttr {
                path,
                value: InodeAttr::Kind(InodeKind::Symlink),
                ..
            } if path == "link"
        )
    }));

    std::fs::remove_file(&path).unwrap();
    repo.materialize_paths(std::collections::HashSet::from(["link".to_string()]))
        .unwrap();
    assert!(std::fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_link(&path).unwrap(),
        std::path::Path::new("missing-target")
    );

    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"regular again").unwrap();
    let status = repo.status(StatusOptions::default()).unwrap();
    assert_eq!(status.entries()[0].status(), FileStatus::TypeChanged);
    let outcome = record_all(&repo, "make regular");
    assert!(outcome.change().hunks().iter().any(|operation| {
        matches!(
            operation,
            GraphOp::SetAttr {
                path,
                value: InodeAttr::Kind(InodeKind::Regular),
                ..
            } if path == "link"
        )
    }));
    std::fs::remove_file(&path).unwrap();
    repo.materialize_paths(std::collections::HashSet::from(["link".to_string()]))
        .unwrap();
    assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
    assert_eq!(std::fs::read(&path).unwrap(), b"regular again");
}

/// Register the invisible sibling writers the review R1 probe plants: two
/// independent changes writing one value each to the same register, visible
/// from no view in this fixture. Returns their NodeIds.
fn seed_invisible_siblings(
    repo: &TestRepository,
    inode: atomic_core::types::Inode,
    position: atomic_core::types::Position<atomic_core::types::NodeId>,
    values: [InodeAttr; 2],
) -> Vec<atomic_core::types::NodeId> {
    use atomic_core::pristine::{InodeAttrEvent, InodeAttrMutTxnT, MutTxnT};
    let mut txn = repo.pristine.write_txn().unwrap();
    let mut ids = Vec::new();
    for (index, value) in values.into_iter().enumerate() {
        let hash = atomic_core::Hash::of(format!("sibling {value:?} {index}").as_bytes());
        let id = txn.register_change(&hash).unwrap();
        txn.put_change_deps(id, &[]).unwrap();
        txn.put_inode_attr_event(inode, position, InodeAttrEvent::new(id, value).unwrap())
            .unwrap();
        ids.push(id);
    }
    txn.commit().unwrap();
    ids
}

/// Assemble one attribute-only change on the base-only `dev` view through
/// the production entry the import path uses.
fn assemble_attribute_write(
    repo: &TestRepository,
    path: &str,
    value: InodeAttr,
) -> atomic_core::change::Change {
    use atomic_core::record::workflow::{DetectionKind, RecordedFile};
    let (inode, position) = {
        use atomic_core::pristine::TreeTxnT;
        let txn = repo.pristine.read_txn().unwrap();
        let inode = txn.get_inode(path).unwrap().unwrap();
        let position = txn.inode_position(inode).unwrap().unwrap();
        (inode, position)
    };
    let mut recorded = RecordedFile::new(path);
    recorded.set_kind(DetectionKind::Modified);
    recorded.set_inode(inode);
    recorded.set_position(position);
    recorded.set_attr(value);
    let (change, _) = repo
        .assemble_and_hash(
            "dev",
            ChangeHeader::new("assembled attribute write on base-only view"),
            &[recorded],
        )
        .unwrap();
    change
}

/// Review CB-9C R1 (mode register): a chmod-only change assembled against a
/// base-only view depends on that view's visible register writer only. Two
/// invisible sibling chmod writers — registered in either order — never
/// become causal dependencies, and the dependency set is exactly the
/// observed visible frontier.
#[cfg(unix)]
#[test]
fn sibling_mode_writers_never_become_foreign_assembly_dependencies() {
    use std::os::unix::fs::PermissionsExt;

    for sibling_values in [
        [InodeAttr::Mode(0o700), InodeAttr::Mode(0o711)],
        [InodeAttr::Mode(0o711), InodeAttr::Mode(0o700)],
    ] {
        let (temp, repo) = create_temp_repo();
        let path = temp.path().join("register.txt");
        std::fs::write(&path, b"content\n").unwrap();
        repo.add("register.txt", TrackingOptions::default())
            .unwrap();
        let base = record_all(&repo, "add register file")
            .change()
            .hash()
            .unwrap();

        // The visible writer: a real chmod recorded on the base-only view.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let visible_writer = record_all(&repo, "visible chmod").change().hash().unwrap();

        let _siblings = {
            let txn = repo.pristine.read_txn().unwrap();
            let inode = txn.get_inode("register.txt").unwrap().unwrap();
            let position = txn.inode_position(inode).unwrap().unwrap();
            seed_invisible_siblings(&repo, inode, position, sibling_values)
        };

        let change = assemble_attribute_write(&repo, "register.txt", InodeAttr::Mode(0o755));
        let deps = change.dependencies();
        let expected: std::collections::HashSet<atomic_core::Hash> =
            [base, visible_writer].into_iter().collect();
        let actual: std::collections::HashSet<atomic_core::Hash> = deps.iter().copied().collect();
        assert_eq!(
            actual,
            expected,
            "exact assembly visibility: the attribute write depends on the target \
             position's base change and the register's visible frontier writer — \
             never an invisible sibling (deps: {:?})",
            deps.iter().map(|h| h.to_base32()).collect::<Vec<_>>()
        );
    }
}

/// Review CB-9C R1 (kind register): the kind-register analogue — the visible
/// file→symlink conversion is the only dependency; invisible sibling kind
/// writers in either registration order never leak into the assembly.
#[cfg(unix)]
#[test]
fn sibling_kind_writers_never_become_foreign_assembly_dependencies() {
    for sibling_values in [
        [
            InodeAttr::Kind(InodeKind::Symlink),
            InodeAttr::Kind(InodeKind::Gitlink),
        ],
        [
            InodeAttr::Kind(InodeKind::Gitlink),
            InodeAttr::Kind(InodeKind::Symlink),
        ],
    ] {
        let (temp, repo) = create_temp_repo();
        let path = temp.path().join("register.txt");
        std::fs::write(&path, b"content\n").unwrap();
        repo.add("register.txt", TrackingOptions::default())
            .unwrap();
        let base = record_all(&repo, "add register file")
            .change()
            .hash()
            .unwrap();

        // The visible writer: a real file→symlink conversion recorded on
        // the base-only view.
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("target", &path).unwrap();
        let visible_writer = record_all(&repo, "visible conversion")
            .change()
            .hash()
            .unwrap();

        let _siblings = {
            let inode = inode_of(&repo, "register.txt");
            let position = position_of(&repo, "register.txt");
            seed_invisible_siblings(&repo, inode, position, sibling_values)
        };

        let change =
            assemble_attribute_write(&repo, "register.txt", InodeAttr::Kind(InodeKind::Gitlink));
        let deps = change.dependencies();
        let expected: std::collections::HashSet<atomic_core::Hash> =
            [base, visible_writer].into_iter().collect();
        let actual: std::collections::HashSet<atomic_core::Hash> = deps.iter().copied().collect();
        assert_eq!(
            actual,
            expected,
            "exact assembly visibility for the kind register: the base change and \
             the visible kind writer only — never an invisible sibling (deps: {:?})",
            deps.iter().map(|h| h.to_base32()).collect::<Vec<_>>()
        );
    }
}

fn inode_of(repo: &TestRepository, path: &str) -> atomic_core::types::Inode {
    use atomic_core::pristine::TreeTxnT;
    let txn = repo.pristine.read_txn().unwrap();
    txn.get_inode(path).unwrap().unwrap()
}

fn position_of(
    repo: &TestRepository,
    path: &str,
) -> atomic_core::types::Position<atomic_core::types::NodeId> {
    use atomic_core::pristine::TreeTxnT;
    let txn = repo.pristine.read_txn().unwrap();
    let inode = txn.get_inode(path).unwrap().unwrap();
    txn.inode_position(inode).unwrap().unwrap()
}
