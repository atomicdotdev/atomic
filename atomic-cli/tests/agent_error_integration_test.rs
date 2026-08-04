//! Process-level coverage for the CLI parse-error boundary.

use std::process::{Command, Output};

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .output()
        .expect("run atomic")
}

#[test]
fn help_and_version_keep_claps_success_stream_and_exit_code() {
    for args in [
        &["--help"][..],
        &["--version"][..],
        &["memory", "--help"][..],
    ] {
        let output = atomic(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(!output.stdout.is_empty(), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
    }
}

#[test]
fn missing_root_command_keeps_claps_failure_stream_and_exit_code() {
    let output = atomic(&[]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("usage: atomic"),
        "{output:?}"
    );
}

#[test]
fn parse_failures_are_structured_on_stderr() {
    let output = atomic(&["memory", "new"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error: missing-required-arg\n"), "{stderr}");
    assert!(stderr.contains("cmd: atomic memory new\n"), "{stderr}");
    assert!(
        stderr.contains("help: atomic memory new --help\n"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_exit_two_without_panicking() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let output = Command::new(ATOMIC_BIN)
        .args(["log", "--count"])
        .arg(OsString::from_vec(vec![0xff]))
        .output()
        .expect("run atomic with non-UTF-8 argv");

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("error: invalid-utf8\n"),
        "{output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("got-raw: unix-hex:ff\n"),
        "{output:?}"
    );
}
