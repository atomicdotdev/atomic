//! Anti-rot guards for the agent-native help contract.
//!
//! Every constraint the contract depends on is a TEST, not a convention. The
//! two that matter most are structural: a registry key or a typed [`Ref`] that
//! stops resolving is a test failure, not a stale line of documentation that
//! quietly sends an agent at a command which no longer exists.
//!
//! All guards are O(tree) and run under `cargo test -p atomic-cli`.

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use crate::agent_doc::{self, Doc, Ref, DOCS, EXIT_CODES, MAX_ERROR_LINE};
    use crate::error::CliError;
    use crate::{apply_agent_help, Cli};

    /// The full tree with the agent layout applied, built so `find_subcommand`
    /// and `get_arg_conflicts_with` see the finished command.
    fn tree() -> clap::Command {
        let mut cmd = apply_agent_help("", Cli::command());
        cmd.build();
        cmd
    }

    fn resolve(root: &clap::Command, parts: &[&str]) -> bool {
        let mut node = root;
        for p in parts {
            match node.find_subcommand(*p) {
                Some(child) => node = child,
                None => return false,
            }
        }
        true
    }

    /// GUARD 1 — every registry KEY resolves to a real command. Renaming or
    /// retiring a command breaks the test suite instead of silently orphaning
    /// its row.
    #[test]
    fn every_key_resolves() {
        let root = tree();
        for (path, _) in DOCS {
            let parts: Vec<&str> = path.split(' ').collect();
            assert!(
                resolve(&root, &parts),
                "agent_doc key '{path}' does not resolve to a command"
            );
        }
    }

    /// GUARD 2 — every typed [`Ref`] resolves COMPLETELY, in `needs`, `then`,
    /// `instead` and every `fails.fix`.
    ///
    /// This is the guard that catches a pointer into a retired subtree, and it
    /// works only because `Ref` is a type: lexically scraping `atomic \w+` out
    /// of prose gives both false positives (a prefix match on a retired path)
    /// and false negatives (the word "atomic" inside a sentence).
    #[test]
    fn every_ref_resolves() {
        let root = tree();
        let check = |r: &Ref, owner: &str, slot: &str| {
            if r.cmd.is_empty() {
                return;
            }
            let parts = r.path();
            assert!(
                !parts.is_empty(),
                "row '{owner}' {slot} ref '{}' has no resolvable path",
                r.cmd
            );
            assert!(
                resolve(&root, &parts),
                "row '{owner}' {slot} references 'atomic {}', which does not exist",
                r.cmd
            );
        };
        for (path, doc) in DOCS {
            for r in doc.needs {
                check(r, path, "needs");
            }
            for r in doc.then {
                check(r, path, "then");
            }
            for r in doc.instead {
                check(r, path, "instead");
            }
            for f in doc.fails {
                check(&f.fix, path, "fails.fix");
            }
        }
    }

    /// GUARD 3 — `run` must invoke the row's own command. Stops copy-paste rot
    /// between sibling verbs.
    #[test]
    fn run_invokes_own_command() {
        for (path, doc) in DOCS {
            if doc.run.is_empty() {
                continue;
            }
            assert!(
                doc.run == *path || doc.run.starts_with(&format!("{path} ")),
                "row '{path}' run line '{}' does not invoke '{path}'",
                doc.run
            );
        }
    }

    /// GUARD 4 — every cited exit code is in the root taxonomy.
    #[test]
    fn exit_codes_in_taxonomy() {
        let known: Vec<i32> = EXIT_CODES.iter().map(|(c, _)| *c).collect();
        for (path, doc) in DOCS {
            for f in doc.fails {
                assert!(
                    known.contains(&f.exit),
                    "row '{path}' cites exit {} which is not in EXIT_CODES",
                    f.exit
                );
            }
        }
    }

    /// GUARD 5 — no failure line may exceed its budget.
    #[test]
    fn no_line_exceeds_budget() {
        for (path, doc) in DOCS {
            for l in doc.error_lines() {
                assert!(
                    l.chars().count() <= MAX_ERROR_LINE,
                    "row '{path}' error line is {} chars, budget is {MAX_ERROR_LINE}: {l}",
                    l.chars().count()
                );
            }
        }
    }

    /// GUARD 6 — a row with no `when:` is not a row. `when:` is the only line
    /// that answers "should I be running this at all".
    #[test]
    fn every_row_answers_when() {
        for (path, doc) in DOCS {
            assert!(!doc.when.is_empty(), "row '{path}' has no when:");
        }
    }

    /// GUARD 7 — graceful degradation. An undocumented command renders EXACTLY
    /// as it did before: none of the keys leak into its help.
    #[test]
    fn undocumented_command_unchanged() {
        let root = tree();
        let mut push = root.find_subcommand("push").unwrap().clone();
        let help = push.render_help().to_string();
        for key in ["when:", "run:", "then:", "needs:", "instead:", "fails:"] {
            assert!(
                !help.contains(key),
                "undocumented `push` leaked {key}:\n{help}"
            );
        }
        assert!(agent_doc::lookup("push").is_none());
    }

    /// GUARD 8 — `--help` must be byte-identical to the template alone.
    ///
    /// The template's own doc comment states the position: the terse one-line
    /// summary is all an agent needs, and prose belongs on the docs website.
    /// This module therefore injects NOTHING into `--help` — every row feeds
    /// only the parse-failure renderer.
    ///
    /// The concrete thing this pins: `Command::after_help` is a SETTER
    /// (clap_builder-4.6.0 `command.rs:2026`), and `atomic vault context` is
    /// the one command in the tree that already ships an `after_help` block.
    /// Any future call would silently delete it.
    #[test]
    fn help_is_untouched() {
        let root = tree();

        // The one pre-existing after_help survives untouched.
        let ctx = root
            .find_subcommand("vault")
            .unwrap()
            .find_subcommand("context")
            .unwrap();
        let after = ctx
            .get_after_help()
            .map(|s| s.to_string())
            .unwrap_or_default();
        assert!(
            after.contains("Examples:"),
            "vault context lost its Examples block:\n{after}"
        );

        // No documented command leaks a row key into its help.
        for (path, _) in DOCS {
            let mut node = root.clone();
            for part in path.split(' ') {
                node = node.find_subcommand(part).unwrap().clone();
            }
            let help = node.render_help().to_string();
            for key in ["when:", "run:", "needs:", "then:", "instead:", "fails:", "exits:"] {
                assert!(
                    !help.contains(key),
                    "'{path}' leaked {key} into --help; rows belong to the failure path only:\n{help}"
                );
            }
        }

        // Including the root.
        let help = tree().render_help().to_string();
        assert!(
            !help.contains("exits:"),
            "the root leaked the exit taxonomy into --help:\n{help}"
        );
    }

    /// GUARD 12 — every failure line stays one line.
    ///
    /// The failure block is written straight to stderr by [`crate::agent_error`]
    /// and never passes through clap's `output.wrap(self.term_w)`
    /// (`help_template.rs:364`), so it cannot be split and cannot orphan a
    /// continuation with no key. This pins that property against the day
    /// someone routes it back through clap: no row may contain an embedded
    /// newline, and none may exceed [`MAX_ERROR_LINE`].
    #[test]
    fn failure_lines_are_single_lines() {
        for (path, doc) in DOCS {
            for line in doc.error_lines() {
                assert!(
                    !line.contains('\n'),
                    "row '{path}' produced a multi-line failure entry: {line:?}"
                );
                assert!(
                    line.len() <= MAX_ERROR_LINE,
                    "row '{path}' failure line is {} chars, budget is {MAX_ERROR_LINE}: {line}",
                    line.len()
                );
            }
        }
    }

    /// GUARD 13 — the root taxonomy must match what `CliError` actually
    /// returns. A new error variant with a new code is a doc bug, not a runtime
    /// bug, and this is where it surfaces.
    #[test]
    fn root_taxonomy_matches_cli_error() {
        let observed: Vec<(i32, CliError)> = vec![
            (1, CliError::NothingToRecord),
            (
                2,
                CliError::InvalidArgument {
                    message: String::new(),
                },
            ),
            (
                3,
                CliError::FileNotFound {
                    path: std::path::PathBuf::new(),
                },
            ),
            (
                4,
                CliError::RemoteError {
                    message: String::new(),
                    url: None,
                },
            ),
            (128, CliError::Internal(anyhow::anyhow!("bug"))),
        ];
        let known: Vec<i32> = EXIT_CODES.iter().map(|(c, _)| *c).collect();
        for (expected, err) in &observed {
            assert_eq!(
                err.exit_code(),
                *expected,
                "EXIT_CODES claims {expected} for {err}"
            );
            assert!(known.contains(expected), "EXIT_CODES is missing {expected}");
        }
    }

    /// GUARD 14 — the failure path actually carries the row.
    ///
    /// The whole point of the design: an agent that mis-invokes gets the
    /// trigger, the prerequisites and the working invocation in the SAME
    /// output, with no second round trip.
    #[test]
    fn failure_render_carries_the_row() {
        let mut root = apply_agent_help("", Cli::command());
        let argv: Vec<String> = ["atomic", "memory", "new"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = root
            .try_get_matches_from_mut(&argv)
            .expect_err("`atomic memory new` must fail: --kind is required");
        let out = crate::agent_error::render(&err, &root, &argv);

        for expected in [
            "error: missing-required-arg\n",
            "cmd: atomic memory new\n",
            "when: ",
            "run: atomic memory new ",
            "needs: atomic ",
            "fails: ",
            "help: atomic memory new --help\n",
        ] {
            assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
        }
        // Every emitted line is `key: value`. No prose, no orphans.
        for line in out.lines() {
            assert!(
                line.split_once(": ").is_some(),
                "failure output is not key: value: {line:?}\n{out}"
            );
        }
    }

    /// GUARD 15 — an unknown subcommand names the alternatives instead of
    /// telling the agent to go read `--help`.
    ///
    /// `verbs:` is the load-bearing key and is derived from the live tree, so it
    /// is always right. `did-you-mean:` is clap's own near-miss ranking and is
    /// only emitted when clap actually has one — `nwe` scores below its
    /// similarity threshold, `valdiate` does not.
    #[test]
    fn unknown_subcommand_lists_verbs() {
        let render_for = |tail: &str| {
            let mut root = apply_agent_help("", Cli::command());
            let argv: Vec<String> = ["atomic", "memory", tail]
                .iter()
                .map(|s| s.to_string())
                .collect();
            let err = root.try_get_matches_from_mut(&argv).unwrap_err();
            crate::agent_error::render(&err, &root, &argv)
        };

        let out = render_for("nwe");
        assert!(out.contains("error: unknown-subcommand\n"), "{out}");
        assert!(out.contains("cmd: atomic memory\n"), "{out}");
        assert!(out.contains("got: nwe\n"), "{out}");
        assert!(
            out.contains("verbs: new show validate attest verify list kinds write\n"),
            "{out}"
        );
        assert!(out.contains("help: atomic memory --help\n"), "{out}");

        let out = render_for("valdiate");
        let suggested = out
            .lines()
            .find(|l| l.starts_with("did-you-mean: "))
            .unwrap_or_else(|| panic!("no did-you-mean line in:\n{out}"));
        assert!(
            suggested.split_whitespace().any(|w| w == "validate"),
            "{out}"
        );
    }

    /// GUARD 18 — a rejected flag BEFORE a real subcommand must not make the
    /// renderer name a command that never ran.
    ///
    /// `atomic --json memory new` is rejected by clap at the root; `memory` and
    /// `new` are real commands sitting later in argv. Resolving the scope from
    /// argv walked straight past the rejected token and reported `cmd: atomic
    /// memory new`, whose row then contributed `run: … --json` — handing the
    /// agent back the very flag it had just been told was unknown, alongside a
    /// `help:` page documenting that flag as valid. Every key must agree with
    /// the usage line clap itself scoped the error to.
    #[test]
    fn leading_flag_does_not_misattribute() {
        for tail in [
            vec!["--json", "memory", "new"],
            vec!["--repository", "status"],
            vec!["--", "status"],
        ] {
            let mut root = apply_agent_help("", Cli::command());
            let argv: Vec<String> = std::iter::once("atomic")
                .chain(tail.iter().copied())
                .map(|s| s.to_string())
                .collect();
            let err = root.try_get_matches_from_mut(&argv).unwrap_err();
            let out = crate::agent_error::render(&err, &root, &argv);

            assert!(
                out.contains("cmd: atomic\n"),
                "scope must stay at the root for {argv:?}, got:\n{out}"
            );
            assert!(
                out.contains("help: atomic --help\n"),
                "help must point at the root for {argv:?}, got:\n{out}"
            );
            // No row belongs to the root, so no guidance may leak in — most of
            // all a `run:` line replaying the rejected token.
            for leaked in ["when: ", "run: ", "needs: ", "then: ", "instead: "] {
                assert!(
                    !out.contains(leaked),
                    "{leaked:?} leaked from a command that never ran for {argv:?}:\n{out}"
                );
            }
        }
    }

    /// GUARD 16 — the removed-command shim speaks the same dialect as
    /// everything else, because it goes through the same renderer.
    #[test]
    fn removed_shim_uses_the_shared_dialect() {
        let out = crate::emit::emit(&crate::commands::vault::removed::removed_lines(
            "atomic vault memory",
            "atomic memory",
            "new write show list kinds validate attest verify",
            "atomic memory write <name>",
        ));
        assert_eq!(
            out,
            "removed: atomic vault memory\n\
             use: atomic memory\n\
             verbs: new write show list kinds validate attest verify\n\
             run: atomic memory write <name>\n\
             help: atomic memory --help\n"
        );
    }

    /// Sanity: the schema itself degrades to nothing.
    #[test]
    fn empty_doc_renders_nothing() {
        assert!(Doc::EMPTY.error_lines().is_empty());
    }
}
