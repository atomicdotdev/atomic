//! Process-level coverage for the CLI log file.
//!
//! Every command writes `info` and above to a daily file under the global
//! config directory, one line per event, each naming the process and the
//! spans it ran in. The terminal keeps showing warnings only, as it did
//! before, and `--verbose` still shows the `log` macros' debug lines.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

/// Run `atomic` in `dir` with an isolated config dir (`home/.atomic`) and a
/// clean logging environment, plus `envs`. `ATOMIC_CONFIG_DIR` rather than
/// `HOME` alone: on Windows the home directory ignores `HOME`.
fn atomic(dir: &Path, home: &Path, envs: &[(&str, &OsStr)], args: &[&str]) -> Output {
    let mut command = Command::new(ATOMIC_BIN);
    command
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("ATOMIC_CONFIG_DIR", home.join(".atomic"))
        .env_remove("ATOMIC_LOG")
        .env_remove("ATOMIC_LOG_DIR")
        .env_remove("RUST_LOG");
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("run atomic")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Logging Tests")
        .env("GIT_AUTHOR_EMAIL", "logging@example.com")
        .env("GIT_COMMITTER_NAME", "Logging Tests")
        .env("GIT_COMMITTER_EMAIL", "logging@example.com")
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn default_log_dir(home: &Path) -> PathBuf {
    home.join(".atomic").join("logs")
}

/// The daily log files in `dir`: `atomic.YYYY-MM-DD.log`.
fn log_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with("atomic.") && name.ends_with(".log")
        })
        .collect();
    files.sort();
    files
}

fn log_text(dir: &Path) -> String {
    log_files(dir)
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn info_goes_to_the_daily_log_file_and_not_to_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let output = atomic(dir.path(), home.path(), &[], &["init"]);
    assert!(output.status.success(), "{output:?}");

    let files = log_files(&default_log_dir(home.path()));
    assert_eq!(files.len(), 1, "one daily file: {files:?}");
    let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(name.len(), "atomic.2026-09-28.log".len(), "{name}");

    let text = log_text(&default_log_dir(home.path()));
    assert!(
        text.lines().all(|line| line.contains(" pid=")),
        "every line names its process:\n{text}"
    );
    assert!(text.contains(" INFO "), "{text}");
    assert!(text.contains("atomic{cmd=init}"), "{text}");
    assert!(text.contains("command started"), "{text}");
    assert!(text.contains("command finished"), "{text}");
    assert!(text.contains("outcome=ok"), "{text}");

    let stderr = stderr(&output);
    assert!(!stderr.contains("INFO"), "{stderr}");
    assert!(!stderr.contains("command started"), "{stderr}");
}

#[test]
fn a_failing_command_logs_its_error_and_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let output = atomic(dir.path(), home.path(), &[], &["status"]);
    assert!(!output.status.success(), "{output:?}");
    let code = output.status.code().unwrap();

    let text = log_text(&default_log_dir(home.path()));
    assert!(text.contains("atomic{cmd=status}"), "{text}");
    assert!(text.contains("outcome=error"), "{text}");
    assert!(text.contains(&format!("exit_code={code}")), "{text}");
}

#[test]
fn atomic_log_off_writes_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let output = atomic(
        dir.path(),
        home.path(),
        &[("ATOMIC_LOG", OsStr::new("off"))],
        &["init"],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(!default_log_dir(home.path()).exists());
}

#[test]
fn atomic_log_dir_moves_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let custom = tempfile::tempdir().unwrap();
    let custom_logs = custom.path().join("logs");

    let output = atomic(
        dir.path(),
        home.path(),
        &[("ATOMIC_LOG_DIR", custom_logs.as_os_str())],
        &["init"],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(log_text(&custom_logs).contains("atomic{cmd=init}"));
    assert!(log_files(&default_log_dir(home.path())).is_empty());
}

#[test]
fn atomic_config_dir_moves_the_default_location() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();

    let output = atomic(
        dir.path(),
        home.path(),
        &[("ATOMIC_CONFIG_DIR", config.path().as_os_str())],
        &["init"],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(log_text(&config.path().join("logs")).contains("atomic{cmd=init}"));
}

#[test]
fn an_unusable_log_dir_warns_and_the_command_still_runs() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let blocker = home.path().join("not-a-dir");
    fs::write(&blocker, b"file").unwrap();
    let unusable = blocker.join("logs");

    let output = atomic(
        dir.path(),
        home.path(),
        &[("ATOMIC_LOG_DIR", unusable.as_os_str())],
        &["init"],
    );
    assert!(output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("file logging is off"), "{stderr}");
    assert!(stderr.contains(&unusable.display().to_string()), "{stderr}");
}

#[test]
fn a_relative_log_dir_warns_and_writes_nothing_into_the_working_tree() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let output = atomic(
        dir.path(),
        home.path(),
        &[("ATOMIC_LOG_DIR", OsStr::new("logs"))],
        &["init"],
    );
    assert!(output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("not an absolute path"), "{stderr}");
    assert!(!dir.path().join("logs").exists());
}

#[test]
fn verbose_still_shows_the_log_macros_debug_lines_on_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let init = atomic(dir.path(), home.path(), &[], &["init"]);
    assert!(init.status.success(), "{init:?}");

    let verbose = atomic(dir.path(), home.path(), &[], &["-v", "status"]);
    assert!(verbose.status.success(), "{verbose:?}");
    let stderr = stderr(&verbose);
    let line = stderr
        .lines()
        .find(|line| line.contains(" DEBUG atomic_repository::"))
        .unwrap_or_else(|| panic!("no debug line from the `log` macros:\n{stderr}"));
    assert!(line.starts_with('['), "env_logger's shape: {line}");
    assert!(line.contains("] "), "env_logger's shape: {line}");
    assert!(
        !stderr.contains("command started"),
        "file-only lines stay off the terminal under -v:\n{stderr}"
    );

    let rust_log = atomic(
        dir.path(),
        home.path(),
        &[("RUST_LOG", OsStr::new("off"))],
        &["-v", "status"],
    );
    assert!(rust_log.status.success(), "{rust_log:?}");
    assert!(
        !stderr_has_debug(&rust_log),
        "RUST_LOG wins over -v:\n{}",
        String::from_utf8_lossy(&rust_log.stderr)
    );
}

fn stderr_has_debug(output: &Output) -> bool {
    String::from_utf8_lossy(&output.stderr).contains(" DEBUG ")
}

#[test]
fn git_import_logs_each_commit_inside_its_span() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let dir = root.path();
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("README.md"), b"readme\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "first"]);
    let first = git(dir, &["rev-parse", "HEAD"]);
    fs::write(dir.join("README.md"), b"readme\nmore\n").unwrap();
    git(dir, &["commit", "-q", "-am", "second"]);
    let second = git(dir, &["rev-parse", "HEAD"]);

    let output = atomic(
        dir,
        home.path(),
        &[],
        &["git", "import", "--all", "--no-vault"],
    );
    assert!(output.status.success(), "{output:?}");

    let text = log_text(&default_log_dir(home.path()));
    assert!(text.contains("atomic{cmd=git import}"), "{text}");
    for (n, sha) in [(1, &first), (2, &second)] {
        let span = format!("commit{{n={n} of=2 sha={}}}", &sha[..8]);
        let in_span = |needle: &str| {
            text.lines()
                .find(|line| line.contains(&span) && line.contains(needle))
                .unwrap_or_else(|| panic!("no {needle:?} line in {span}:\n{text}"))
                .to_string()
        };
        assert!(in_span("write ").contains(" INFO "));
        // Each commit reports how long it took when its span closes.
        in_span("close time.busy");
    }
    assert!(
        text.lines()
            .any(|line| line.contains("import{") && line.contains("close time.busy")),
        "{text}"
    );
    assert!(!stderr(&output).contains("INFO"), "{}", stderr(&output));
}
