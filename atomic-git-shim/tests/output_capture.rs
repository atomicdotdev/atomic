//! Keep the process-wide reporter isolated from other command tests.
use atomic_git_shim::{
    commands::Import,
    output::{self, live, Notice},
};
use std::sync::atomic::{AtomicUsize, Ordering};

static REPORTED: AtomicUsize = AtomicUsize::new(0);

#[test]
fn import_notices_share_capture_with_and_without_a_reporter() {
    let root = tempfile::tempdir().unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    let git = git2::Repository::init_opts(root.path(), &options).unwrap();
    let tree = git.treebuilder(None).unwrap().write().unwrap();
    let tree = git.find_tree(tree).unwrap();
    let signature = git2::Signature::now("Capture test", "capture@example.invalid").unwrap();
    git.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
        .unwrap();

    struct RestoreDirectory(std::path::PathBuf);
    impl Drop for RestoreDirectory {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).unwrap();
        }
    }
    let _directory = RestoreDirectory(std::env::current_dir().unwrap());
    std::env::set_current_dir(root.path()).unwrap();
    let mut import = Import::default();
    import.dry_run = true;
    import.branch = Some("main".into());

    let scope = live::LiveOutput::start().unwrap();
    assert!(live::LiveOutput::start().is_none());
    import.run().unwrap();
    assert_eq!(live::latest(), "Would import 1 commit from branch 'main'");
    drop(scope);
    assert!(!live::is_active());
    assert!(live::latest().is_empty());

    assert!(output::set_reporter(|kind, message| {
        // The embedding logger must see notices even while a visual is active.
        REPORTED.fetch_add(1, Ordering::SeqCst);
        let kind = match kind {
            Notice::Info | Notice::Hint => live::Kind::Info,
            Notice::Success => live::Kind::Success,
            Notice::Warning => live::Kind::Warning,
        };
        assert!(live::capture(kind, message));
    }));
    let scope = live::LiveOutput::start().unwrap();
    import.run().unwrap();
    assert_eq!(REPORTED.load(Ordering::SeqCst), 2);
    assert_eq!(live::latest(), "Would import 1 commit from branch 'main'");
    drop(scope);
    assert!(!live::is_active());
    assert!(!root.path().join(".atomic").exists());
}
