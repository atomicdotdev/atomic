//! Repository-dependent commands must reject configuration-only directories
//! before accessing indexes or writing repository data.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn atomic(dir: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atomic"))
        .arg("--no-color")
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        // Repository initialization must use local embeddings even when the
        // developer running this test has provider credentials configured.
        .env_remove("VOYAGE_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .expect("run atomic")
}

fn assert_not_repository(output: &Output, dir: &Path) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(stderr.contains("Not in an Atomic repository"), "{stderr}");
    assert!(stderr.contains(&dir.display().to_string()), "{stderr}");
    assert!(stderr.contains("Vault searches"), "{stderr}");
    assert!(stderr.contains("atomic init"), "{stderr}");
    assert!(!stderr.contains("Internal error"), "{stderr}");
    assert!(!stderr.contains("Please report"), "{stderr}");
}

#[test]
fn commands_reject_global_config_and_git_only_directories() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let config = home.join(".atomic");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join("config.toml"), "# global configuration\n").unwrap();
    let project = home.join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir(project.join(".vault")).unwrap();

    for dir in [&home, &project] {
        for args in [
            &[
                "vault",
                "query",
                "code",
                "repository_root",
                "-g",
                "atomic-cli/src/commands/view/list.rs",
            ][..],
            &["query", "code", "repository_root", "--json"][..],
            &["vault", "query", "search", "repository_root"][..],
            &["vault", "query", "entities", "src/main.rs"][..],
            &["vault", "query", "index"][..],
            &["vault", "query", "enrich"][..],
            &["vault", "query", "plan"][..],
            &["vault", "list"][..],
            &["status"][..],
            &["view", "list"][..],
            &["log"][..],
        ] {
            let output = atomic(dir, &home, args);
            assert_not_repository(&output, dir);
        }
    }

    assert!(!config.join("content-index").exists());
    assert!(!config.join("pristine.redb").exists());
    assert!(!project.join(".atomic").exists());
}

#[test]
fn standalone_config_directory_is_not_a_repository() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let project = home.join("project");
    std::fs::create_dir_all(project.join(".atomic")).unwrap();

    let output = atomic(&project, &home, &["query", "code", "repository_root"]);
    assert_not_repository(&output, &project);
    assert!(!project.join(".atomic/content-index").exists());
}

#[test]
fn init_and_queries_from_repository_subdirectories_still_work() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().canonicalize().unwrap();
    std::fs::create_dir(home.join(".atomic")).unwrap();
    let project = home.join("project");
    std::fs::create_dir(&project).unwrap();

    let output = atomic(&project, &home, &["init"]);
    assert!(output.status.success(), "{output:?}");
    assert!(project.join(".atomic/pristine.redb").is_file());

    let nested = project.join("src/deep");
    std::fs::create_dir_all(nested.join(".atomic")).unwrap();
    for args in [
        &["query", "search", "repository_root", "--json"][..],
        &["vault", "query", "search", "repository_root", "--json"][..],
        &["status", "--short"][..],
    ] {
        let output = atomic(&nested, &home, args);
        assert!(output.status.success(), "{args:?}: {output:?}");
    }
    assert!(!nested.join(".atomic/pristine.redb").exists());
}

#[test]
fn help_and_version_work_outside_a_repository() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().canonicalize().unwrap();
    for args in [
        &["--help"][..],
        &["--version"][..],
        &["vault", "query", "code", "--help"][..],
    ] {
        let output = atomic(&home, &home, args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
    }
}
