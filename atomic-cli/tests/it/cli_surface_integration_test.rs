//! End-to-end smoke tests for every public Atomic CLI command.
//!
//! The test discovers subcommands from the binary's own Clap help output, then
//! recursively invokes `--help` for every discovered command path. This keeps
//! the test in sync as commands are added or removed and catches parser
//! conflicts that command-level unit tests do not exercise.

use std::collections::{HashSet, VecDeque};
use std::process::{Command, Output};

fn run_help(path: &[String]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(path)
        .arg("--no-color")
        .arg("--help")
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "failed to execute `atomic{} --help`: {error}",
                display_path(path)
            )
        })
}

fn display_path(path: &[String]) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!(" {}", path.join(" "))
    }
}

/// Extract the first column from Clap's `Commands:` section.
///
/// Command rows have exactly two leading spaces. Wrapped descriptions are
/// indented further, so they are ignored. Rows marked `[REMOVED` are skipped:
/// those are deprecation shims that deliberately print a redirect and exit
/// non-zero instead of rendering help, so they are not public commands.
fn visible_subcommands(help: &str) -> Vec<String> {
    let mut in_commands = false;
    let mut commands = Vec::new();

    for raw_line in help.lines() {
        let line = raw_line.trim_end_matches('\r');

        if line == "Commands:" {
            in_commands = true;
            continue;
        }

        if !in_commands {
            continue;
        }

        if line.is_empty() {
            if !commands.is_empty() {
                break;
            }
            continue;
        }

        let Some(row) = line.strip_prefix("  ") else {
            break;
        };

        if row.starts_with(char::is_whitespace) {
            continue;
        }

        let Some(name) = row.split_whitespace().next() else {
            continue;
        };

        let description = row[name.len()..].trim_start();

        // Clap adds this standard dispatcher automatically. Its child paths
        // duplicate the commands already traversed from their canonical path.
        // Removed commands are redirect shims, not live command families.
        if name == "help" || description.starts_with("[REMOVED") {
            continue;
        }

        // Retired command families keep a shim that redirects to the new path
        // and exits non-zero; it has no help to render.
        if row.contains("[REMOVED") {
            continue;
        }

        commands.push(name.to_string());
    }

    commands
}

#[test]
fn every_public_command_renders_help() {
    let mut pending = VecDeque::from([Vec::<String>::new()]);
    let mut visited = HashSet::new();
    let mut leaf_count = 0usize;

    while let Some(path) = pending.pop_front() {
        assert!(
            visited.insert(path.clone()),
            "CLI command tree contains a duplicate path: atomic{}",
            display_path(&path)
        );

        let output = run_help(&path);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            output.status.success(),
            "`atomic{} --help` failed with {}\nstdout:\n{}\nstderr:\n{}",
            display_path(&path),
            output.status,
            stdout,
            stderr
        );
        // The agent-native help template renders a lowercase `usage:` line in
        // place of clap's default `Usage:` heading. Anchor to the line start so
        // prose in a command's description cannot satisfy this on its own.
        let renders_usage = stdout
            .lines()
            .any(|line| line.trim_end_matches('\r').starts_with("usage: "));
        assert!(
            renders_usage,
            "`atomic{} --help` did not render a usage section\nstdout:\n{}\nstderr:\n{}",
            display_path(&path),
            stdout,
            stderr
        );

        let children = visible_subcommands(&stdout);
        if children.is_empty() {
            leaf_count += 1;
        }

        for child in children {
            let mut child_path = path.clone();
            child_path.push(child);
            pending.push_back(child_path);
        }
    }

    // These sentinels prove discovery descended through one-, two-, and
    // three-level command families rather than only checking root help.
    for expected in [
        ["agent", "enable"].as_slice(),
        ["project", "delete"].as_slice(),
        ["intent", "new"].as_slice(),
        ["vault", "goal", "start"].as_slice(),
    ] {
        let expected = expected
            .iter()
            .map(|part| (*part).to_string())
            .collect::<Vec<_>>();
        assert!(
            visited.contains(&expected),
            "CLI help discovery missed `atomic{}`",
            display_path(&expected)
        );
    }

    assert!(leaf_count > 0, "CLI help discovery found no leaf commands");
    eprintln!(
        "validated help for {} public command paths ({} leaves)",
        visited.len(),
        leaf_count
    );
}

#[test]
fn command_section_parser_ignores_wrapped_descriptions_help_dispatcher_and_removed() {
    let help = "\
Commands:
  alpha  First command
         with a wrapped description
  beta   Second command
  gamma  [REMOVED — use `atomic gamma`] Agent-facing redirect shim
  help   Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
";

    assert_eq!(visible_subcommands(help), ["alpha", "beta"]);
}
